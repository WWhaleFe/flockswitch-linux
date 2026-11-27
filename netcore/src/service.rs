use crate::{
    discovery::{DiscoveredPeer, Discovery},
    identity::Identity,
    pairing::Pairing,
    keymap::KeyMapper,
    proto::{Msg, PROTO_VERSION},
    tls, *,
};
use anyhow::{Context, Result};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime},
};

mod files;
mod queue;

const IDLE_POLL: Duration = Duration::from_millis(100);
const OFF_POLL: Duration = Duration::from_millis(250);
const LINK_QUEUE: usize = 256;
const MAX_ACCEPTS_PER_IP: usize = 8;

const POLL: Duration = Duration::from_millis(8);
const CAPTURE_RETRY_MIN: Duration = Duration::from_secs(2);
const CAPTURE_RETRY_MAX: Duration = Duration::from_secs(30);
const HEARTBEAT: Duration = Duration::from_millis(500);
const DEAD: Duration = Duration::from_secs(2);
/// How long a peer paired by the service is kept against an older config snapshot from the app.
const RECENT_PAIRED: Duration = Duration::from_secs(15);
/// "Unpaired message" goodbyes must be this far apart to make us forget a peer.
const UNPAIRED_CONFIRM: Duration = Duration::from_secs(10);
/// A wall-clock jump this big between two ticks means the computer slept or was locked.
const WAKE_JUMP: Duration = Duration::from_secs(5);
/// After a wake-up, links silent for this long are closed.
const WAKE_STALE: Duration = Duration::from_secs(4);
/// An incoming clipboard transfer that got nothing for this long is given up.
const INCOMING_STALL: Duration = Duration::from_secs(30);
/// How long the status says "reconnecting" at most.
const RECONNECT_SHOWN: Duration = Duration::from_secs(10);
/// Seconds to wait before dial attempt number `attempt` (0 is the first retry), at most 8.
pub(crate) fn backoff_secs(attempt: usize) -> u64 {
    [1, 2, 4, 8][attempt.min(3)]
}
static NEXT_LINK: AtomicU64 = AtomicU64::new(1);

pub struct Service {
    tx: Sender<Command>,
    status: Arc<Mutex<Status>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}
enum Command {
    Config(NetConfig),
    Request(Request),
    Cross(Side, f64),
    Reconnect,
    KeyRules(std::collections::BTreeMap<String, DirRules>),
    ClockJump(Duration),
    /// Something arrived that the idle loop must look at now.
    Wake,
    Stop,
}
impl Service {
    /// Start with an explicit config_dir. Port zero asks the OS for an available port.
    pub fn start(mut config: NetConfig, platform: Arc<dyn Platform>, hooks: Hooks) -> Result<Self> {
        config.on &= config.notice_accepted;
        anyhow::ensure!(
            !config.config_dir.as_os_str().is_empty(),
            "config_dir must be supplied"
        );
        let identity = Identity::load_or_create(&config.config_dir)?;
        let server = tls::server_config(&identity)?;
        let client = tls::client_config(&identity)?;
        let name = computer_name(&config);
        let listener = if config.on {
            Some(bind(config.port)?)
        } else {
            None
        };
        let port = listener
            .as_ref()
            .map(|l| l.local_addr().unwrap().port())
            .unwrap_or(config.port);
        let status = Arc::new(Mutex::new(Status {
            running: config.on,
            me: MeStatus {
                id: identity.id.clone(),
                name,
                port,
                error: None,
                can_send: true,
            },
            discovered: vec![],
            peers: vec![],
            state: StateStatus {
                mode: "local".into(),
                peer: None,
            },
            pairing: None,
            transfer: None,
            following: None,
            layout: vec![],
            reconnecting: false,
            keys: config.all_rules(),
        }));
        let (tx, rx) = mpsc::channel();
        let (events, event_rx) = mpsc::channel();
        let (capture, captured) = mpsc::channel();
        let listener = listener.map(|l| Acceptor::start(l, events.clone(), tx.clone())).transpose()?;
        let mut control = Control {
            config,
            platform,
            hooks,
            identity,
            server,
            client,
            listener,
            status: status.clone(),
            events,
            event_rx,
            rx,
            wake: tx.clone(),
            capture,
            captured,
            links: HashMap::new(),
            workers: vec![],
            inflight: HashSet::new(),
            retry: HashMap::new(),
            state: State::Local,
            pairing: None,
            pair_status: None,
            master: None,
            told: HashMap::new(),
            peer_features: HashMap::new(),
            keys: HashSet::new(),
            clip_sigs: HashMap::new(),
            pending_clip: HashMap::new(),
            deferred_clipboard: HashSet::new(),
            recent_paired: HashMap::new(),
            unpaired_seen: HashMap::new(),
            clip_origin: None,
            trusted: vec![],
            group: None,
            reconnecting: None,
            skew: Duration::ZERO,
            last_wall: SystemTime::now(),
            outgoing: None,
            waiting_sends: VecDeque::new(),
            file_jobs: files::Jobs::default(),
            transfer_revision: 0,
            incoming: None,
            incoming_at: Instant::now(),
            next_transfer: 1,
            buttons: HashSet::new(),
            captured_keys: HashSet::new(),
            mapper: KeyMapper::default(),
            discovery: None,
            discovered: vec![],
            edge_wait: None,
            last_cursor: None,
            armed: Vec::new(),
            edge_hit: None,
            capture_block: None,
            capture_backoff: Duration::ZERO,
            outward: false,
            started: Instant::now(),
            last_write: Instant::now() - Duration::from_secs(1),
            written: None,
            written_permissions: None,
            readiness_error: None,
        };
        control.setup_discovery();
        let handle = thread::Builder::new()
            .name("flock-net-control".into())
            .spawn(move || control.run())?;
        Ok(Self {
            tx,
            status,
            thread: Mutex::new(Some(handle)),
        })
    }
    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
    /// Just the input mode, without copying the roster, layout and key rules.
    pub fn state(&self) -> StateStatus {
        self.status.lock().unwrap().state.clone()
    }
    pub fn local_addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.status().me.port))
    }
    pub fn request(&self, request: Request) -> Result<()> {
        self.tx
            .send(Command::Request(request))
            .context("service stopped")
    }
    /// Move control to the peer placed on `side` now, entering it at `pos` (0..1 along
    /// that edge). For apps that watch the edges themselves (`app_drives_edges`): they
    /// show their countdown and call this when it ends. Ignored unless this computer is
    /// in control and a connected, paired peer sits on that side.
    pub fn cross(&self, side: Side, pos: f64) -> Result<()> {
        self.tx
            .send(Command::Cross(side, pos))
            .context("service stopped")
    }
    /// Close links that are not fully up, reset the dial backoff, redial every peer now
    /// and browse for computers again. Apps call this on wake and unlock.
    pub fn reconnect(&self) -> Result<()> {
        self.tx.send(Command::Reconnect).context("service stopped")
    }
    /// Send edited key rules: a master saves and hands them on, a sub sends them to its master.
    /// A sub's message is a partial update: the master merges only the pairs in it into its
    /// own rules, so apps on a sub should send only the "src>dst" pairs the user changed,
    /// never the whole map (a whole map would overwrite changes made elsewhere meanwhile).
    pub fn send_key_rules(&self, keys: std::collections::BTreeMap<String, DirRules>) -> Result<()> {
        self.tx.send(Command::KeyRules(keys)).context("service stopped")
    }
    /// Test hook: acts as if the wall clock jumped forward, like after sleep.
    #[doc(hidden)]
    pub fn simulate_clock_jump(&self, by: Duration) -> Result<()> {
        self.tx.send(Command::ClockJump(by)).context("service stopped")
    }
    /// Queue a live update. The original config_dir remains fixed.
    pub fn update_config(&self, config: NetConfig) -> Result<()> {
        self.tx
            .send(Command::Config(config))
            .context("service stopped")
    }
    /// Release input and join all service threads. Safe to call repeatedly.
    pub fn stop(&self) {
        let mut handle = self.thread.lock().unwrap();
        if let Some(handle) = handle.take() {
            let _ = self.tx.send(Command::Stop);
            let _ = handle.join();
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.stop();
    }
}
fn computer_name(config: &NetConfig) -> String {
    static HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    if !config.name.is_empty() {
        return config.name.clone();
    }
    HOST.get_or_init(|| {
        std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .ok()
            .filter(|name| !name.is_empty())
            .or_else(|| {
                #[cfg(unix)]
                {
                    std::process::Command::new("/bin/hostname")
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .and_then(|o| String::from_utf8(o.stdout).ok())
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                }
                #[cfg(not(unix))]
                {
                    None
                }
            })
            .unwrap_or_else(|| "Flockswitch".into())
    })
    .clone()
}
fn bind(port: u16) -> Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let dual_stack = (|| -> std::io::Result<TcpListener> {
        let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_only_v6(false)?;
        // As std does for IPv4: a restart may bind the port again at once.
        #[cfg(unix)]
        socket.set_reuse_address(true)?;
        socket.bind(&SocketAddr::from(([0u16; 8], port)).into())?;
        socket.listen(64)?;
        Ok(socket.into())
    })();
    let listener = dual_stack.or_else(|_| TcpListener::bind(("0.0.0.0", port)))?;
    Ok(listener)
}

/// Accepts connections on its own thread, so the service loop can sleep while idle: each
/// connection is handed over as an event and wakes the loop.
struct Acceptor {
    stop: Arc<AtomicBool>,
    addr: SocketAddr,
    handle: Option<JoinHandle<()>>,
}

/// Bound unauthenticated accepts from one IP before they can fill worker slots.
#[derive(Default)]
struct AcceptPace(HashMap<std::net::IpAddr, (Instant, usize)>);
impl AcceptPace {
    fn allow(&mut self, ip: std::net::IpAddr, now: Instant) -> bool {
        let ip = ip.to_canonical();
        if self.0.len() >= 1024 {
            self.0.retain(|_, (since, _)| now.duration_since(*since) < DEAD);
            if self.0.len() >= 1024 && !self.0.contains_key(&ip) { return false; }
        }
        let (since, count) = self.0.entry(ip).or_insert((now, 0));
        if now.duration_since(*since) >= DEAD { *since = now; *count = 0; }
        if *count >= MAX_ACCEPTS_PER_IP { return false; }
        *count += 1;
        true
    }
}
impl Acceptor {
    fn start(listener: TcpListener, events: Sender<Event>, wake: Sender<Command>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let addr = listener.local_addr()?;
        let handle = thread::Builder::new().name("flock-accept".into()).spawn(move || {
            let mut pace = AcceptPace::default();
            let _ = listener.set_nonblocking(true);
            loop {
                // Wait for a connection at most ACCEPT_WAIT so the stop flag is looked at
                // even when the poke from Drop never arrived.
                if !wait_readable(&listener, ACCEPT_WAIT) {
                    if stopped.load(Ordering::Acquire) { break; }
                    continue;
                }
                let accepted = listener.accept();
                if stopped.load(Ordering::Acquire) { break; }
                if let Err(e) = &accepted {
                    if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::Interrupted { continue; }
                }
                // Some systems hand on the listener's non-blocking mode to what it accepts.
                if let Ok((socket, _)) = &accepted { let _ = socket.set_nonblocking(false); }
                match accepted {
                    Ok((socket, addr)) => {
                        if pace.allow(addr.ip(), Instant::now())
                            && (events.send(Event::Accepted(socket, addr)).is_err()
                                || wake.send(Command::Wake).is_err()) { break; }
                    }
                    Err(e) => {
                        log::warn!("listener: {e}");
                        thread::sleep(IDLE_POLL);
                    }
                }
            }
        })?;
        Ok(Self { stop, addr, handle: Some(handle) })
    }
    fn port(&self) -> u16 { self.addr.port() }
}
impl Drop for Acceptor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Fast path: a connection to ourselves wakes the accept loop at once. Without it
        // the loop notices the flag within ACCEPT_WAIT.
        let port = self.addr.port();
        let targets = [SocketAddr::from(([127, 0, 0, 1], port)), SocketAddr::from(([0u16, 0, 0, 0, 0, 0, 0, 1], port))];
        let poked = targets.iter().any(|a| TcpStream::connect_timeout(a, Duration::from_millis(50)).is_ok());
        if let Some(handle) = self.handle.take() {
            if poked {
                let _ = handle.join();
            } else {
                // Never hold the service loop for the timeout: let go of the thread in the background.
                let _ = thread::Builder::new().name("flock-accept-join".into()).spawn(move || { let _ = handle.join(); });
            }
        }
    }
}
/// How long the accept thread waits between looks at its stop flag.
const ACCEPT_WAIT: Duration = Duration::from_secs(2);
/// Waits until `listener` has a connection to accept (true) or `timeout` passed (false).
#[cfg(unix)]
fn wait_readable(listener: &TcpListener, timeout: Duration) -> bool {
    use std::os::fd::AsRawFd;
    #[repr(C)]
    struct PollFd { fd: i32, events: i16, revents: i16 }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    type NFds = std::os::raw::c_ulong;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    type NFds = u32;
    extern "C" {
        fn poll(fds: *mut PollFd, n: NFds, timeout: i32) -> i32;
    }
    const POLLIN: i16 = 1;
    let mut fd = PollFd { fd: listener.as_raw_fd(), events: POLLIN, revents: 0 };
    // SAFETY: one valid pollfd that lives through the call.
    let ready = unsafe { poll(&mut fd, 1, timeout.as_millis() as i32) };
    // On an error (not a timeout) fall through to accept, which reports it.
    ready != 0
}
#[cfg(windows)]
fn wait_readable(listener: &TcpListener, timeout: Duration) -> bool {
    use std::os::windows::io::AsRawSocket;
    #[repr(C)]
    struct WsaPollFd { socket: usize, events: i16, revents: i16 }
    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAPoll(fds: *mut WsaPollFd, n: u32, timeout: i32) -> i32;
    }
    const POLLRDNORM: i16 = 0x0100;
    let mut fd = WsaPollFd { socket: listener.as_raw_socket() as usize, events: POLLRDNORM, revents: 0 };
    // SAFETY: one valid WSAPOLLFD that lives through the call; the listener keeps Winsock started.
    let ready = unsafe { WSAPoll(&mut fd, 1, timeout.as_millis() as i32) };
    // On an error (not a timeout) fall through to accept, which reports it.
    ready != 0
}
#[cfg(not(any(unix, windows)))]
fn wait_readable(_: &TcpListener, timeout: Duration) -> bool {
    thread::sleep(timeout.min(Duration::from_millis(50)));
    true
}

struct Link {
    token: u64,
    tx: queue::Sender,
    cancel: Arc<AtomicBool>,
    name: String,
    os: Os,
    role: Role,
    features: Vec<String>,
    fp: String,
    addr: SocketAddr,
    listen_port: Option<u16>,
    outgoing: bool,
    paired: bool,
    last_rx: Instant,
    last_ping: Instant,
    ping: Option<(u64, Instant)>,
    rtt: Option<f64>,
    /// Transfer pieces handed to this link but not yet taken by its writer.
    bulk: Arc<AtomicUsize>,
}
impl Link {
    fn send(&self, msg: Msg) -> bool {
        if let Err(msg) = self.tx.try_send(msg) {
            // Lost input transitions or group settings require a fresh connection.
            // Motion and other replaceable updates may still be dropped.
            if matches!(msg, Msg::Input(InputEvent::Key { .. } | InputEvent::Button { .. })
                | Msg::Enter { .. } | Msg::Leave { .. } | Msg::ReleaseAll
                | Msg::Layout { .. } | Msg::MasterSettings(_) | Msg::Group { .. }) {
                self.cancel.store(true, Ordering::Release);
            }
            return false;
        }
        true
    }
}
impl Drop for Link {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
enum Event {
    Accepted(TcpStream, SocketAddr),
    Connected(String, Link, bool),
    Message(String, u64, Msg),
    Closed(String, u64),
    DialDone(SocketAddr),
    /// ClipDone has left rustls's output buffer on this link.
    ClipSent(String, u64, u32),
}
#[derive(Clone)]
enum State {
    Local,
    Controlling { peer: String, side: Side, pos: f64 },
    Controlled { peer: String, entered: Side },
}
impl State {
    fn peer(&self) -> Option<&str> {
        match self {
            Self::Local => None,
            Self::Controlling { peer, .. } | Self::Controlled { peer, .. } => Some(peer),
        }
    }
}
struct Retry {
    attempt: usize,
    next: Instant,
}
struct Control {
    config: NetConfig,
    platform: Arc<dyn Platform>,
    hooks: Hooks,
    identity: Identity,
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
    listener: Option<Acceptor>,
    status: Arc<Mutex<Status>>,
    events: Sender<Event>,
    event_rx: Receiver<Event>,
    rx: Receiver<Command>,
    wake: Sender<Command>,
    capture: Sender<InputEvent>,
    captured: Receiver<InputEvent>,
    links: HashMap<String, Link>,
    workers: Vec<(Arc<AtomicBool>, JoinHandle<()>)>,
    inflight: HashSet<SocketAddr>,
    retry: HashMap<String, Retry>,
    state: State,
    pairing: Option<Pairing>,
    pair_status: Option<PairingStatus>,
    /// On a sub: the settings its master handed over, kept over the sub's own.
    master: Option<MasterSettings>,
    /// On a master: what each sub was last told (its side and the settings).
    told: HashMap<String, (Option<Side>, MasterSettings, (Vec<Cell>, Vec<Member>))>,
    /// Features each peer advertised the last time it connected (kept across blips).
    peer_features: HashMap<String, Vec<String>>,
    keys: HashSet<u16>,
    /// Per peer: fingerprint of the clipboard content last sent to or received from it,
    /// so unchanged content is not sent again.
    clip_sigs: HashMap<String, Vec<u8>>,
    /// Per peer: transfer id and hash queued but not yet flushed to its socket.
    pending_clip: HashMap<String, (u32, Vec<u8>)>,
    /// Targets requested while a completed receive is still being finalized.
    deferred_clipboard: HashSet<String>,
    /// Peers this service paired itself lately: an app config snapshot taken before that must not drop them.
    recent_paired: HashMap<String, (PeerConfig, Instant)>,
    /// When a peer first said "unpaired message" since a session last worked.
    unpaired_seen: HashMap<String, Instant>,
    /// Fingerprint of the content that last arrived, and the peer it came from.
    clip_origin: Option<(Vec<u8>, String, bool)>,
    /// On a sub: computers its master's roster vouches for (runtime only, `via_master`).
    trusted: Vec<PeerConfig>,
    /// On a sub: the layout and roster the master last sent.
    group: Option<(Vec<Cell>, Vec<Member>)>,
    reconnecting: Option<Instant>,
    /// Added to the wall clock (test hook for a simulated sleep).
    skew: Duration,
    last_wall: SystemTime,
    /// A clipboard transfer on its way out, to this peer.
    outgoing: Option<(String, crate::transfer::Outgoing)>,
    /// Prepared transfers for other targets, in request order, one per peer.
    waiting_sends: VecDeque<(String, String, crate::transfer::Outgoing)>,
    file_jobs: files::Jobs,
    transfer_revision: u64,
    /// A clipboard transfer on its way in, from this peer.
    incoming: Option<(String, crate::transfer::Incoming)>,
    /// When the incoming transfer last made progress.
    incoming_at: Instant,
    next_transfer: u32,
    buttons: HashSet<MouseButton>,
    captured_keys: HashSet<u16>,
    mapper: KeyMapper,
    discovery: Option<Discovery>,
    discovered: Vec<DiscoveredPeer>,
    edge_wait: Option<(Side, Instant)>,
    last_cursor: Option<(f64, f64)>,
    armed: Vec<Side>,
    edge_hit: Option<(Side, f64, Instant)>,
    /// No crossing is attempted before this instant after a failed `start_capture`.
    capture_block: Option<Instant>,
    /// Last cooldown length: 2 s, doubling to 30 s while captures keep failing.
    capture_backoff: Duration,
    outward: bool,
    started: Instant,
    last_write: Instant,
    written: Option<Status>,
    written_permissions: Option<crate::Permissions>,
    readiness_error: Option<String>,
}
impl Control {
    fn setup_discovery(&mut self) {
        self.discovery = None;
        self.discovered.clear();
        if self.config.on && self.config.discovery && self.listener.is_some() {
            let port = self.status.lock().unwrap().me.port;
            match Discovery::start(
                &self.identity,
                &computer_name(&self.config),
                self.platform.os(),
                port,
            ) {
                Ok(d) => self.discovery = Some(d),
                Err(e) => log::warn!("mDNS unavailable: {e:#}"),
            }
        }
    }
    fn spawn_link(
        &mut self,
        socket: Option<TcpStream>,
        addr: SocketAddr,
        pair: bool,
        expected: Option<String>,
    ) {
        if self.workers.len() >= 64 {
            return;
        }
        let addr = canonical(addr);
        if !crate::is_local_network(addr.ip()) {
            log::warn!("refused {addr}: not on the local network");
            self.status.lock().unwrap().me.error =
                Some(format!("{addr} is not on the local network; only local network addresses are allowed"));
            return;
        }
        let outgoing = socket.is_none();
        if outgoing && !self.inflight.insert(addr) {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let cancelled = cancel.clone();
        let args = Worker {
            identity: self.identity.clone(),
            name: computer_name(&self.config),
            os: self.platform.os(),
            role: self.config.role,
            listen_port: self.status.lock().unwrap().me.port,
            client: self.client.clone(),
            server: self.server.clone(),
            events: self.events.clone(),
            wake: self.wake.clone(),
            cancel: cancelled,
            addr,
            outgoing,
            pair,
            expected,
        };
        let handle = thread::spawn(move || args.run(socket));
        self.workers.push((cancel, handle));
    }
    fn run(&mut self) {
        let mut stop = false;
        let mut pending_command = None;
        while !stop {
            let tick = Instant::now();
            for _ in 0..128 {
                match pending_command.take().map(Ok).unwrap_or_else(|| self.rx.try_recv()) {
                    Ok(Command::Stop) => {
                        stop = true;
                        break;
                    }
                    Ok(Command::Request(r)) => self.request(r),
                    Ok(Command::Config(c)) => self.update(c),
                    Ok(Command::Cross(side, pos)) => self.cross_on_request(side, pos),
                    Ok(Command::Reconnect) => self.reconnect(None),
                    Ok(Command::KeyRules(keys)) => self.key_rules_edited(keys),
                    Ok(Command::ClockJump(by)) => self.skew += by,
                    Ok(Command::Wake) => {}
                    Err(_) => break,
                }
            }
            if stop {
                break;
            }
            let wall = SystemTime::now() + self.skew;
            if wall.duration_since(self.last_wall).is_ok_and(|d| d >= WAKE_JUMP) {
                log::info!("clock jumped: reconnecting");
                self.reconnect(Some((WAKE_STALE, wall.duration_since(self.last_wall).unwrap_or_default())));
            }
            self.last_wall = wall;
            for _ in 0..512 {
                match self.event_rx.try_recv() {
                    Ok(event) => self.event(event),
                    Err(_) => break,
                }
            }
            for _ in 0..512 {
                match self.captured.try_recv() {
                    Ok(event) => self.capture_event(event),
                    Err(_) => break,
                }
            }
            if let Some(d) = &mut self.discovery {
                self.discovered = d.poll();
                self.discovered.retain(|p| p.id != self.identity.id);
            }
            match take_request(&self.config.config_dir) {
                Ok(Some(r)) => self.request(r),
                Ok(None) => {}
                Err(e) => log::warn!("network request: {e:#}"),
            }
            if self.config.on {
                self.maintenance();
                self.poll_cursor();
            }
            self.enforce_send_permission();
            if let State::Controlling { peer, .. } = &self.state {
                let events = self.mapper.tick(Instant::now());
                if let Some(l) = self.links.get(peer) {
                    for event in events {
                        l.send(Msg::Input(event));
                    }
                }
            }
            self.poll_file_jobs();
            self.flush_transfer();
            self.publish(false);
            let mut i = 0;
            while i < self.workers.len() {
                if self.workers[i].1.is_finished() {
                    let (_, h) = self.workers.swap_remove(i);
                    let _ = h.join();
                } else {
                    i += 1;
                }
            }
            let wait = self.poll_interval().saturating_sub(tick.elapsed());
            if let Ok(command) = self.rx.recv_timeout(wait) {
                pending_command = Some(command);
            }
        }
        self.file_jobs.cancel_direction(0);
        self.file_jobs.cancel_direction(1);
        if let Some((_, incoming)) = self.incoming.take() { incoming.abandon(); }
        self.outgoing = None;
        self.waiting_sends.clear();
        self.pending_clip.clear();
        self.local(true);
        self.platform.arm_edges(&[]);
        for link in self.links.values() {
            link.send(Msg::ReleaseAll);
            link.send(Msg::Bye {
                reason: "shutdown".into(),
            });
        }
        self.links.clear();
        self.discovery = None;
        self.listener = None;
        for (cancel, _) in &self.workers {
            cancel.store(true, Ordering::Release);
        }
        self.file_jobs.stop();
        for (_, handle) in self.workers.drain(..) {
            let _ = handle.join();
        }
        self.config.on = false;
        // Persist the final snapshot within the same write rate limit.
        thread::sleep(Duration::from_millis(300).saturating_sub(self.last_write.elapsed()));
        self.publish(true);
    }
    fn poll_interval(&self) -> Duration {
        if self.file_jobs.busy() || (!self.links.is_empty() && !self.config.app_drives_edges)
            || self.edge_wait.is_some() || self.edge_hit.is_some()
            || !matches!(self.state, State::Local) || self.outgoing.is_some() || !self.waiting_sends.is_empty() || self.incoming.is_some()
        {
            POLL
        } else if self.config.on {
            IDLE_POLL
        } else {
            OFF_POLL
        }
    }
    fn update(&mut self, mut config: NetConfig) {
        config.on &= config.notice_accepted;
        if config.role == Role::Sub {
            if let Some(m) = &self.master {
                m.apply_to(&mut config);
            }
        } else {
            self.master = None;
            self.status.lock().unwrap().following = None;
        }
        if config.swap_cmd_ctrl != self.config.swap_cmd_ctrl
            || config.keys != self.config.keys
            || config.lang1_on_mac != self.config.lang1_on_mac
            || config.mac_capslock_to_hangul != self.config.mac_capslock_to_hangul
            || config.mac_shift_space_to_hangul != self.config.mac_shift_space_to_hangul
        {
            self.local(true);
        }
        config.config_dir = self.config.config_dir.clone();
        config.peers.retain(|p| !p.via_master);
        // A peer paired here after the app took its snapshot is not in it yet: keep it.
        self.recent_paired.retain(|_, (_, at)| at.elapsed() < RECENT_PAIRED);
        // Once a snapshot lists the peer the app has seen it, and a later removal is its own.
        self.recent_paired.retain(|id, _| !config.peers.iter().any(|p| p.id == *id));
        for (id, (peer, _)) in &self.recent_paired {
            if self.config.peers.iter().any(|p| p.id == *id && p.fingerprint == peer.fingerprint) {
                config.peers.push(self.config.peers.iter().find(|p| p.id == *id).cloned().unwrap_or_else(|| peer.clone()));
            }
        }
        let master_id = |c: &NetConfig| c.peers.iter().find(|p| p.role == Role::Master).map(|p| p.id.clone());
        if config.role != Role::Sub || master_id(&config).is_none() || master_id(&config) != master_id(&self.config) {
            self.clear_group();
        }
        let rebind = config.on != self.config.on || config.port != self.config.port;
        let rediscover =
            rebind || config.discovery != self.config.discovery || config.name != self.config.name;
        let remove: Vec<_> = self
            .links
            .iter()
            .filter(|(id, l)| {
                !config.on
                    || !config
                        .peers
                        .iter()
                        .chain(&self.trusted)
                        .any(|p| p.id == **id && p.fingerprint == l.fp)
                        && l.paired
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in remove {
            self.disconnect(&id, "config");
        }
        if !config.on {
            self.local(true);
            self.platform.arm_edges(&[]);
            self.armed.clear();
            self.pairing = None;
        }
        if config.on && config.share_clipboard && !config.receive_clipboard {
            self.file_jobs.cancel_direction(1);
            if let Some((_, incoming)) = self.incoming.take() { incoming.abandon(); }
        }
        if !config.on || !config.share_clipboard {
            if self.file_jobs.busy() || self.outgoing.is_some() || !self.waiting_sends.is_empty() || self.incoming.is_some() {
                self.fail_running_transfer();
            }
            self.file_jobs.cancel_direction(0);
            self.file_jobs.cancel_direction(1);
            self.outgoing = None;
            self.waiting_sends.clear();
            self.pending_clip.clear();
            self.deferred_clipboard.clear();
            if let Some((_, incoming)) = self.incoming.take() { incoming.abandon(); }
        }
        self.config = config;
        self.prune_peer_state();
        self.enforce_send_permission();
        self.sync_sides();
        if rebind {
            self.listener = None;
            if self.config.on {
                // The old listener may need a moment to let go of the port.
                let mut started = None;
                for n in 0..60 {
                    if n > 0 { thread::sleep(Duration::from_millis(50)); }
                    started = Some(bind(self.config.port)
                        .and_then(|l| Acceptor::start(l, self.events.clone(), self.wake.clone())));
                    if started.as_ref().is_some_and(|r| r.is_ok()) || self.config.port == 0 { break; }
                }
                match started.unwrap() {
                    Ok(l) => {
                        self.status.lock().unwrap().me.port = l.port();
                        self.listener = Some(l)
                    }
                    Err(e) => self.status.lock().unwrap().me.error = Some(e.to_string()),
                }
            }
        }
        if rediscover {
            self.setup_discovery();
        }
    }
    fn request(&mut self, request: Request) {
        if !self.config.on {
            return;
        }
        match request {
            Request::PairAddr { pair_addr } => match pair_addr.parse() {
                Ok(addr) => self.spawn_link(None, addr, true, None),
                Err(e) => log::warn!("invalid pair address: {e}"),
            },
            Request::Pair { pair } => {
                if let Some(l) = self.links.get(&pair) {
                    l.send(Msg::PairRequest);
                    self.begin_pair(&pair);
                } else if let Some(p) = self.discovered.iter().find(|p| p.id == pair) {
                    if let Some(a) = p.addresses.first() {
                        self.spawn_link(None, *a, true, Some(pair));
                    }
                }
            }
            Request::Accept { accept } => {
                let Some(p) = self.pairing.as_mut() else {
                    return;
                };
                let id = p.peer.clone();
                if accept && !p.expired() {
                    p.local_accepted = true;
                    if let Some(l) = self.links.get(&id) {
                        l.send(Msg::PairAccept);
                    }
                    self.finish_pair();
                } else {
                    self.fail_pair();
                }
            }
            Request::Unpair { unpair } => {
                let was_master = self.config.peers.iter().any(|p| p.id == unpair && p.role == Role::Master);
                self.recent_paired.remove(&unpair);
                self.config.peers.retain(|p| p.id != unpair);
                self.config.layout.retain(|c| c.id != unpair);
                if was_master {
                    self.clear_group();
                }
                self.disconnect(&unpair, "unpaired");
                self.prune_peer_state();
                (self.hooks.on_unpaired)(unpair);
            }
            Request::Action { action } => {
                if action == "reconnect" {
                    self.reconnect(None);
                }
            }
        }
    }
    fn begin_pair(&mut self, id: &str) {
        let Some(l) = self.links.get(id) else { return };
        if l.paired {
            return;
        }
        let mine = self.config.role;
        let subs = self.config.peers.iter().filter(|p| p.role == Role::Sub && p.id != id).count();
        let has_master = self.config.peers.iter().any(|p| p.role == Role::Master && p.id != id);
        let refusal = match (mine, l.role) {
            (Role::Master, Role::Master) => Some("both_master"),
            (Role::Sub, Role::Sub) => Some("both_sub"),
            (Role::Master, Role::Sub) if subs >= MAX_SUBS => Some("full"),
            (Role::Sub, Role::Master) if has_master => Some("has_master"),
            _ => None,
        };
        if let Some(reason) = refusal {
            l.send(Msg::PairReject);
            self.pair_status = Some(PairingStatus {
                peer_name: l.name.clone(),
                code: String::new(),
                stage: "failed".into(),
                reason: Some(reason.into()),
            });
            return;
        }
        if let Some(p) = &self.pairing {
            if p.peer == id {
                return;
            }
            l.send(Msg::PairReject);
            return;
        }
        self.pairing = Some(Pairing::new(id.into(), &self.identity.fingerprint, &l.fp));
        let p = self.pairing.as_ref().unwrap();
        self.pair_status = Some(PairingStatus {
            peer_name: l.name.clone(),
            code: p.code.clone(),
            stage: "waiting".into(),
            reason: None,
        });
    }
    fn finish_pair(&mut self) {
        let Some(p) = &self.pairing else { return };
        if p.peer_accepted {
            if let Some(s) = &mut self.pair_status {
                s.stage = "peer_accepted".into();
            }
        }
        if !p.complete() {
            return;
        }
        let id = p.peer.clone();
        if let Some(l) = self.links.get_mut(&id) {
            if !l.outgoing && l.listen_port.is_none() {
                if let Some(addr) = self
                    .discovered
                    .iter()
                    .find(|p| p.id == id && p.fingerprint == l.fp)
                    .and_then(|p| p.addresses.first())
                {
                    l.addr.set_port(addr.port());
                }
            }
            l.paired = true;
            let peer = PeerConfig {
                id: id.clone(),
                name: l.name.clone(),
                os: l.os,
                fingerprint: l.fp.clone(),
                last_addr: l.addr.to_string(),
                role: l.role,
                ..PeerConfig::default()
            };
            self.config.peers.retain(|p| p.id != id);
            self.config.peers.push(peer.clone());
            self.recent_paired.insert(id.clone(), (peer.clone(), Instant::now()));
            (self.hooks.on_paired)(peer);
            if let Some(s) = &mut self.pair_status {
                s.stage = "done".into();
            }
        }
        self.pairing = None;
    }
    fn fail_pair(&mut self) {
        if let Some(p) = self.pairing.take() {
            if let Some(l) = self.links.get(&p.peer) {
                l.send(Msg::PairReject);
            }
            self.disconnect(&p.peer, "pairing cancelled");
        }
        if let Some(s) = &mut self.pair_status {
            s.stage = "failed".into();
        }
    }
    fn event(&mut self, event: Event) {
        match event {
            Event::ClipSent(id, token, transfer) => {
                if self.links.get(&id).is_some_and(|l| l.token == token)
                    && self.pending_clip.get(&id).is_some_and(|(tid, _)| *tid == transfer)
                {
                    if let Some((_, sig)) = self.pending_clip.remove(&id) {
                        self.clip_sigs.insert(id, sig);
                    }
                }
            }
            Event::DialDone(addr) => {
                self.inflight.remove(&addr);
            }
            Event::Connected(id, mut link, pair) => {
                if !self.config.on || id == self.identity.id {
                    return;
                }
                if let Some(p) = self.config.peers.iter().chain(&self.trusted).find(|p| p.id == id) {
                    // A roster entry vouches for one role; a pairing is pinned by fingerprint.
                    let roster_only = p.via_master && !self.config.peers.iter().any(|q| q.id == id);
                    if p.fingerprint != link.fp || (roster_only && p.role != link.role) {
                        log::warn!("fingerprint or role mismatch for {id}");
                        return;
                    }
                    link.paired = true;
                    if let Ok(addr) = p.last_addr.parse::<SocketAddr>() {
                        if !link.outgoing && link.listen_port.is_none() {
                            // The inbound socket supplies the current IP, the saved
                            // address only supplies a legacy peer's listening port.
                            link.addr.set_port(addr.port());
                        }
                    }
                }
                if let Some(old) = self.links.get(&id) {
                    let prefer_outgoing = self.identity.id < id;
                    if old.outgoing == prefer_outgoing || link.outgoing != prefer_outgoing {
                        if pair {
                            // The pairing was asked for on the link that lost the tie-break:
                            // carry it out on the one that stays.
                            old.send(Msg::PairRequest);
                            self.begin_pair(&id);
                        }
                        return;
                    }
                    self.disconnect(&id, "duplicate");
                }
                if link.paired {
                    if let Some(p) = self.config.peers.iter_mut().find(|p| p.id == id) {
                        let addr = link.addr.to_string();
                        if p.last_addr != addr {
                            p.last_addr = addr;
                            (self.hooks.on_peer_changed)(p.clone());
                        }
                    }
                }
                self.peer_features.insert(id.clone(), link.features.clone());
                self.links.insert(id.clone(), link);
                if pair {
                    if let Some(l) = self.links.get(&id) {
                        l.send(Msg::PairRequest);
                    }
                    self.begin_pair(&id);
                }
            }
            Event::Closed(id, token) => {
                if self.links.get(&id).is_some_and(|l| l.token == token) {
                    self.disconnect(&id, "link lost");
                }
            }
            Event::Accepted(socket, addr) => {
                if self.config.on && self.listener.is_some() {
                    self.spawn_link(Some(socket), addr, false, None);
                }
            }
            Event::Message(id, token, msg) => {
                let can_send = self.peer_can_send(&id);
                let from_master = self.config.role == Role::Sub
                    && self.config.peers.iter().any(|p| p.id == id && p.role == Role::Master && !p.via_master);
                let Some(l) = self.links.get_mut(&id) else {
                    return;
                };
                if l.token != token {
                    return;
                }
                l.last_rx = Instant::now();
                if !l.paired
                    && !matches!(
                        msg,
                        Msg::PairRequest | Msg::PairAccept | Msg::PairReject | Msg::Bye { .. }
                    )
                {
                    self.disconnect(&id, "unpaired message");
                    return;
                }
                // Anything but a goodbye or hello on a paired link means the peer accepts us.
                if !self.unpaired_seen.is_empty() && !matches!(msg, Msg::Bye { .. } | Msg::Hello { .. }) {
                    self.unpaired_seen.remove(&id);
                }
                match msg {
                    Msg::Hello { .. } => self.disconnect(&id, "duplicate hello"),
                    Msg::PairRequest => self.begin_pair(&id),
                    Msg::PairAccept => {
                        if let Some(p) = &mut self.pairing {
                            if p.peer == id {
                                p.peer_accepted = true;
                                self.finish_pair();
                            }
                        }
                    }
                    Msg::PairReject => {
                        if self.pairing.as_ref().is_some_and(|p| p.peer == id) {
                            self.fail_pair();
                        }
                    }
                    Msg::Ping { t } => { l.send(Msg::Pong { t }); }
                    Msg::Layout { side } => {
                        if from_master {
                            let mine = side.map(Side::opposite);
                            if let Some(p) = self.config.peers.iter_mut().find(|p| p.id == id) {
                                if p.side != mine {
                                    p.side = mine;
                                    (self.hooks.on_peer_changed)(p.clone());
                                }
                            }
                        }
                    }
                    Msg::Group { cells, roster } => {
                        if from_master {
                            self.apply_group(&id, cells, roster);
                        }
                    }
                    Msg::KeyRules(keys) => {
                        if self.config.role == Role::Master && l.role == Role::Sub {
                            // Partial update: only the pairs in the message change.
                            let mut merged = self.config.keys.clone();
                            merged.extend(keys.into_iter().filter(|(k, _)| keymap::parse_pair(k).is_some()));
                            self.key_rules_edited(merged);
                            // Echo even an unchanged edit, so a lost acknowledgement
                            // can be recovered by retransmitting the durable request.
                            self.told.remove(&id);
                        }
                    }
                    Msg::MasterSettings(settings) => {
                        if from_master {
                            let mut next = self.config.clone();
                            settings.apply_to(&mut next);
                            if next.all_rules() != self.config.all_rules() || next.lang1_on_mac != self.config.lang1_on_mac {
                                self.local(true);
                            }
                            settings.apply_to(&mut self.config);
                            self.status.lock().unwrap().following = Some(settings.master_name.clone());
                            (self.hooks.on_master_settings)(settings.clone());
                            self.master = Some(settings);
                        }
                    }
                    Msg::Pong { t } => {
                        if let Some((sent, at)) = l.ping {
                            if sent == t {
                                l.rtt = Some(at.elapsed().as_secs_f64() * 1000.0);
                                l.ping = None;
                                self.retry.remove(&id);
                            }
                        }
                    }
                    Msg::Enter { side, pos } => {
                        if !can_send {
                            l.send(Msg::ReleaseAll);
                        } else if matches!(self.state, State::Local)
                            && pos.is_finite()
                            && self.platform.ready().is_ok()
                        {
                            if let Some((x, y)) = geometry::edge_position(
                                &self.platform.displays(),
                                side.opposite(),
                                pos,
                            ) {
                                if self.edge_hit.take().is_some() {
                                    self.platform.stop_capture();
                                }
                                self.edge_wait = None;
                                self.platform.warp(x, y);
                                self.last_cursor = Some((x, y));
                                self.outward = false;
                                self.state = State::Controlled {
                                    peer: id,
                                    entered: side.opposite(),
                                };
                            } else {
                                l.send(Msg::ReleaseAll);
                            }
                        } else {
                            l.send(Msg::ReleaseAll);
                        }
                    }
                    Msg::Leave { pos } => {
                        if matches!(&self.state,State::Controlling{peer,..} if peer==&id)
                            && pos.is_finite()
                        {
                            if let State::Controlling { pos: ref mut p, .. } = &mut self.state {
                                *p = pos;
                            }
                            self.local(false);
                        }
                    }
                    Msg::Clipboard { text } => {
                        if self.config.receives_clipboard() {
                            if self.file_jobs.cancel_direction(1) { self.fail_running_transfer(); }
                            self.drop_stale_sends();
                            if let Some((_, old)) = self.incoming.take() { old.abandon(); }
                            let sig = clip_sig(b"text", &[text.as_bytes()]);
                            self.platform.set_clipboard_text(&text);
                            self.clip_received(&id, sig);
                        }
                    }
                    Msg::ClipStart { id: tid, kind, files } => self.clip_start(&id, tid, kind, files),
                    Msg::ClipChunk { id: tid, file, data } => self.clip_chunk(&id, tid, file, &data),
                    Msg::ClipDone { id: tid } => self.clip_done(&id, tid),
                    Msg::Input(event) => {
                        // A 2.1 sender applied the key rules itself; only a 2.0 one leaves
                        // Lang1 for this Mac to turn into Caps Lock.
                        let legacy = !l.features.iter().any(|f| f == "keyrules");
                        if !can_send {
                            l.send(Msg::ReleaseAll);
                            if self.state.peer() == Some(&id) {
                                self.local(false);
                            }
                        } else if matches!(&self.state,State::Controlled{peer,..} if peer==&id)
                            && event.valid()
                        {
                            self.inject(event, legacy);
                        }
                    }
                    Msg::ReleaseAll => {
                        if self.state.peer() == Some(&id) {
                            self.local(false);
                        }
                    }
                    Msg::Bye { reason } => {
                        // The peer no longer knows this computer (it was unpaired while this
                        // one was away): forget it here too, so it can be paired again.
                        // "unpaired" is the user's explicit unpair and counts at once. "unpaired
                        // message" only says the peer does not know us right now: it counts
                        // when heard twice at least UNPAIRED_CONFIRM apart with no working
                        // session in between.
                        let explicit = reason == "unpaired";
                        let known = self.config.peers.iter().any(|p| p.id == id && !p.via_master);
                        let forgotten = known
                            && (explicit || (reason == "unpaired message" && self.unpaired_confirmed(&id)));
                        if forgotten {
                            self.recent_paired.remove(&id);
                            self.unpaired_seen.remove(&id);
                            let was_master = self.config.peers.iter().any(|p| p.id == id && p.role == Role::Master);
                            self.config.peers.retain(|p| p.id != id);
                            self.config.layout.retain(|c| c.id != id);
                            if was_master {
                                self.clear_group();
                            }
                            self.disconnect(&id, "bye");
                            self.prune_peer_state();
                            (self.hooks.on_unpaired)(id);
                        } else {
                            self.disconnect(&id, "bye");
                        }
                    }
                }
            }
        }
    }
    /// Records an "unpaired message" goodbye from `id`; true once an earlier one is old enough.
    fn unpaired_confirmed(&mut self, id: &str) -> bool {
        let now = Instant::now();
        match self.unpaired_seen.get(id) {
            Some(first) if now.duration_since(*first) >= UNPAIRED_CONFIRM => true,
            Some(_) => false,
            None => {
                self.unpaired_seen.insert(id.into(), now);
                false
            }
        }
    }
    fn disconnect(&mut self, id: &str, reason: &str) {
        if self.file_jobs.cancel_peer(id) { self.fail_running_transfer(); }
        if self.state.peer() == Some(id) {
            self.local(false);
        }
        self.told.remove(id);
        self.deferred_clipboard.remove(id);
        if let Some(l) = self.links.remove(id) {
            l.send(Msg::ReleaseAll);
            l.send(Msg::Bye {
                reason: reason.into(),
            });
        }
        if self.outgoing.as_ref().is_some_and(|(peer, _)| peer == id) {
            self.outgoing = None;
            self.transfer_note("send", "files", "failed");
        }
        self.waiting_sends.retain(|(peer, _, _)| peer != id);
        if self.incoming.as_ref().is_some_and(|(peer, _)| peer == id) {
            if let Some((_, incoming)) = self.incoming.take() {
                incoming.abandon();
            }
            self.transfer_note("receive", "files", "failed");
        }
        self.clip_sigs.remove(id);
        self.pending_clip.remove(id);
        if self.pairing.as_ref().is_some_and(|p| p.peer == id) {
            self.pairing = None;
            if let Some(s) = &mut self.pair_status {
                s.stage = "failed".into();
            }
        }
        self.schedule_retry(id);
    }
    fn schedule_retry(&mut self, id: &str) {
        let retry = self.retry.entry(id.into()).or_insert(Retry {
            attempt: 0,
            next: Instant::now(),
        });
        let seconds = backoff_secs(retry.attempt);
        retry.attempt += 1;
        retry.next = Instant::now() + Duration::from_secs(seconds);
    }
    /// On a master: tells each connected sub where it sits, who else is in the group and
    /// which settings to use, whenever that changed (or the sub just connected).
    fn tell_subs(&mut self) {
        if self.config.role != Role::Master {
            self.told.clear();
            return;
        }
        self.sync_sides();
        let settings = MasterSettings::of(&self.config, &computer_name(&self.config));
        let group = (self.my_cells(), self.roster());
        self.told.retain(|id, _| self.links.get(id).is_some_and(|l| l.paired));
        for p in &self.config.peers {
            let Some(l) = self.links.get(&p.id).filter(|l|
                l.paired && l.role == Role::Sub && !l.cancel.load(Ordering::Acquire)) else {
                continue;
            };
            let now = (p.side, settings.clone(), group.clone());
            if self.told.get(&p.id) != Some(&now) {
                if l.send(Msg::Layout { side: p.side })
                    && l.send(Msg::MasterSettings(settings.clone()))
                    && (!l.features.iter().any(|f| f == "group")
                        || l.send(Msg::Group { cells: group.0.clone(), roster: group.1.clone() }))
                {
                    self.told.insert(p.id.clone(), now);
                }
            }
        }
    }
    /// The master and its subs, as handed to every sub.
    fn roster(&self) -> Vec<Member> {
        let mut roster = vec![Member {
            id: self.identity.id.clone(),
            name: computer_name(&self.config),
            os: self.platform.os(),
            fingerprint: self.identity.fingerprint.clone(),
            addr: String::new(),
            can_send: true,
            features: proto::FEATURES.iter().map(|s| s.to_string()).collect(),
        }];
        for p in self.config.peers.iter().filter(|p| p.role == Role::Sub) {
            // Only subs that said "group" in their Hello can take part; a 2.0 one would
            // drop every link the others open to it.
            let Some(features) = self.peer_features.get(&p.id).filter(|f| f.iter().any(|f| f == "group")) else {
                continue;
            };
            let addr = self.links.get(&p.id).map_or_else(|| p.last_addr.clone(), |l| l.addr.to_string());
            roster.push(Member {
                id: p.id.clone(),
                name: p.name.clone(),
                os: p.os,
                fingerprint: p.fingerprint.clone(),
                addr,
                can_send: p.can_send,
                features: features.clone(),
            });
        }
        roster
    }
    /// The grid in effect: the master's layout, the one the master sent, or what the
    /// peers' 2.0 sides make of it.
    fn my_cells(&self) -> Vec<Cell> {
        let me = &self.identity.id;
        match &self.group {
            Some((cells, _)) if self.config.role == Role::Sub => cells.clone(),
            _ if self.config.role == Role::Master && !self.config.layout.is_empty() => {
                // A cell whose computer is no longer paired is an orphan: never shown or handed on.
                let live: Vec<Cell> = self.config.layout.iter()
                    .filter(|c| c.id == *me || self.config.peers.iter().any(|p| p.id == c.id))
                    .cloned().collect();
                layout::with_master(&live, me)
            }
            _ => layout::cells_from_sides(me, &self.config.peers),
        }
    }
    /// On a master with a layout: the 2.0 side of each sub follows the grid.
    fn sync_sides(&mut self) {
        if self.config.role != Role::Master || self.config.layout.is_empty() {
            return;
        }
        let cells = self.my_cells();
        let me = self.identity.id.clone();
        for p in &mut self.config.peers {
            let side = layout::side_of(&cells, &me, &p.id);
            if p.side != side {
                p.side = side;
                (self.hooks.on_peer_changed)(p.clone());
            }
        }
    }
    /// A peer a link may be open to: paired, or vouched for by the master's roster.
    fn known_peer(&self, id: &str) -> Option<&PeerConfig> {
        self.config.peers.iter().chain(&self.trusted).find(|p| p.id == id)
    }
    fn open(&self, id: &str) -> bool {
        self.links.get(id).is_some_and(|l| l.paired)
    }
    /// Whether this computer may use its mouse and keyboard on the others.
    fn me_can_send(&self) -> bool {
        match &self.group {
            Some((_, roster)) if self.config.role == Role::Sub => {
                roster.iter().find(|m| m.id == self.identity.id).is_none_or(|m| m.can_send)
            }
            _ => true,
        }
    }
    /// Whether `id` may take control of this computer: a master looks at its config,
    /// a sub at the roster.
    fn peer_can_send(&self, id: &str) -> bool {
        if let Some((_, roster)) = &self.group {
            if self.config.role == Role::Sub {
                if let Some(m) = roster.iter().find(|m| m.id == id) {
                    return m.can_send;
                }
            }
        }
        self.known_peer(id).is_none_or(|p| p.can_send)
    }
    /// Revoke an active session as soon as a config or roster removes its permission.
    fn enforce_send_permission(&mut self) {
        let allowed = match &self.state {
            State::Controlling { .. } => self.me_can_send(),
            State::Controlled { peer, .. } => self.peer_can_send(peer),
            State::Local => true,
        };
        if !allowed {
            self.local(true);
        }
        if !self.me_can_send() {
            if self.edge_hit.is_some() {
                self.platform.stop_capture();
            }
            self.edge_hit = None;
            self.edge_wait = None;
            self.platform.arm_edges(&[]);
            self.armed.clear();
        }
    }
    /// The paired master's authority is gone, including every derived live link.
    fn clear_group(&mut self) {
        let dropped: Vec<String> = self.links.iter()
            .filter(|(id, l)| l.paired && !self.config.peers.iter().any(|p| p.id == **id))
            .map(|(id, _)| id.clone()).collect();
        self.trusted.clear();
        self.group = None;
        self.master = None;
        self.status.lock().unwrap().following = None;
        for id in dropped {
            self.disconnect(&id, "master removed");
            self.retry.remove(&id);
        }
        self.prune_peer_state();
    }
    fn prune_peer_state(&mut self) {
        let known: HashSet<&str> = self.config.peers.iter().chain(&self.trusted).map(|p| p.id.as_str()).collect();
        self.retry.retain(|id, _| known.contains(id.as_str()));
        self.peer_features.retain(|id, _| known.contains(id.as_str()));
        self.clip_sigs.retain(|id, _| known.contains(id.as_str()));
        self.pending_clip.retain(|id, _| known.contains(id.as_str()));
        self.told.retain(|id, _| known.contains(id.as_str()));
    }
    /// The open link on the other side of the neighbouring cell on `side`.
    fn neighbour_peer(&self, side: Side) -> Option<String> {
        if !self.me_can_send() {
            return None;
        }
        let cells = self.my_cells();
        layout::neighbour(&cells, &self.identity.id, side)
            .filter(|id| self.open(id))
            .map(String::from)
    }
    /// On a sub: the master sent the grid and roster. Listed computers are trusted for
    /// sub-to-sub links; computers no longer listed are dropped.
    fn apply_group(&mut self, master: &str, cells: Vec<Cell>, roster: Vec<Member>) {
        let me = self.identity.id.clone();
        let trusted: Vec<PeerConfig> = roster
            .iter()
            .filter(|m| m.id != me && m.id != master && !self.config.peers.iter().any(|p| p.id == m.id))
            .filter(|m| m.fingerprint.len() == 64 && m.fingerprint.starts_with(&m.id))
            .map(|m| PeerConfig {
                id: m.id.clone(),
                name: m.name.clone(),
                os: m.os,
                fingerprint: m.fingerprint.clone(),
                last_addr: m.addr.clone(),
                role: Role::Sub,
                can_send: m.can_send,
                via_master: true,
                ..PeerConfig::default()
            })
            .collect();
        let dropped: Vec<String> = self
            .links
            .iter()
            .filter(|(id, l)| {
                l.paired
                    && !self.config.peers.iter().any(|p| p.id == **id)
                    && !trusted.iter().any(|t| t.id == **id && t.fingerprint == l.fp && t.role == l.role)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dropped {
            self.disconnect(id, "left the group");
            self.retry.remove(id);
        }
        for (id, l) in self.links.iter_mut().filter(|(_, l)| !l.paired) {
            if let Some(t) = trusted.iter().find(|t| t.id == *id && t.fingerprint == l.fp && t.role == l.role) {
                l.paired = true;
                if !l.outgoing {
                    if let Ok(addr) = t.last_addr.parse() {
                        l.addr = addr;
                    }
                }
            }
        }
        self.trusted = trusted;
        self.prune_peer_state();
        self.group = Some((layout::valid_cells(&cells), roster));
        self.enforce_send_permission();
    }
    /// Key rules were edited (here, or by a sub): a master saves and hands them on, a sub
    /// passes them to its master.
    fn key_rules_edited(&mut self, keys: std::collections::BTreeMap<String, DirRules>) {
        let keys: std::collections::BTreeMap<_, _> =
            keys.into_iter().filter(|(k, _)| keymap::parse_pair(k).is_some()).collect();
        if self.config.role == Role::Master {
            if self.config.keys != keys {
                if let Err(e) = (self.hooks.persist_key_rules)(&keys) {
                    log::warn!("key rules not saved, leaving edit unacknowledged: {e:#}");
                    return;
                }
                self.config.keys = keys.clone();
                self.local(true);
                (self.hooks.on_key_rules)(keys);
            }
        } else if let Some(l) = self
            .config
            .peers
            .iter()
            .find(|p| p.role == Role::Master)
            .and_then(|p| self.links.get(&p.id))
            .filter(|l| l.paired && l.features.iter().any(|f| f == "keyrules"))
        {
            l.send(Msg::KeyRules(keys));
        }
    }
    /// Closes links that are not fully up (and, after a wake-up, silent ones), resets the
    /// dial backoff, redials everyone now and browses again.
    fn reconnect(&mut self, stale: Option<(Duration, Duration)>) {
        if !self.config.on {
            return;
        }
        let close: Vec<String> = self
            .links
            .iter()
            .filter(|(id, l)| {
                let pairing = self.pairing.as_ref().is_some_and(|p| &p.peer == *id);
                (!l.paired && !pairing) || stale.is_some_and(|(s, slept)| l.last_rx.elapsed() + slept >= s)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in close {
            self.disconnect(&id, "reconnect");
        }
        self.retry.clear();
        self.reconnecting = Some(Instant::now());
        if let Some(d) = &mut self.discovery {
            d.rebrowse();
        }
        self.dial_peers();
    }
    /// A sender that moved on or went quiet leaves a half transfer behind: remove it.
    fn drop_stalled_incoming(&mut self, now: Instant) {
        if self.incoming.is_some() && now.saturating_duration_since(self.incoming_at) >= INCOMING_STALL {
            if let Some((_, incoming)) = self.incoming.take() {
                incoming.abandon();
            }
            self.transfer_note("receive", "files", "failed");
        }
    }
    fn maintenance(&mut self) {
        self.tell_subs();
        self.drop_stalled_incoming(Instant::now());
        if self.pairing.as_ref().is_some_and(|p| p.expired()) {
            self.fail_pair();
        }
        let dead: Vec<_> = self
            .links
            .iter()
            .filter(|(_, l)| {
                l.last_rx.elapsed()
                    >= if l.paired {
                        DEAD
                    } else {
                        crate::pairing::PAIR_TIMEOUT
                    }
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            self.disconnect(&id, "timeout");
        }
        for l in self.links.values_mut().filter(|l| l.paired) {
            if l.last_ping.elapsed() >= HEARTBEAT {
                let t = self.started.elapsed().as_micros() as u64;
                l.send(Msg::Ping { t });
                l.last_ping = Instant::now();
                if l.ping.is_none() {
                    l.ping = Some((t, Instant::now()));
                }
            }
        }
        self.dial_peers();
    }
    /// Dials every peer that has no link, is not waiting out a backoff and is ours to dial
    /// (the one with the smaller id dials). Roster-vouched subs only if they are neighbours.
    fn dial_peers(&mut self) {
        let cells = self.my_cells();
        let near: Vec<&str> = [Side::Left, Side::Right, Side::Top, Side::Bottom]
            .into_iter()
            .filter_map(|s| layout::neighbour(&cells, &self.identity.id, s))
            .collect();
        let peers: Vec<PeerConfig> = self
            .config
            .peers
            .iter()
            .chain(self.trusted.iter().filter(|t| near.contains(&t.id.as_str())))
            .cloned()
            .collect();
        for p in peers {
            if self.identity.id >= p.id
                || self.links.contains_key(&p.id)
                || self
                    .retry
                    .get(&p.id)
                    .is_some_and(|r| r.next > Instant::now())
            {
                continue;
            }
            if let Some(addr) = self.dial_address(&p) {
                if !self.inflight.contains(&addr) {
                    self.spawn_link(None, addr, false, Some(p.id.clone()));
                    self.schedule_retry(&p.id);
                }
            }
        }
    }
    fn dial_address(&self, peer: &PeerConfig) -> Option<SocketAddr> {
        let mut candidates = Vec::new();
        for addr in self.discovered.iter()
            .filter(|d| d.id == peer.id && d.fingerprint == peer.fingerprint)
            .flat_map(|d| d.addresses.iter().copied())
            .chain(peer.last_addr.parse::<SocketAddr>().ok())
            .map(canonical)
            .filter(|a| crate::is_local_network(a.ip()))
        {
            if !candidates.contains(&addr) {
                candidates.push(addr);
            }
        }
        if candidates.is_empty() {
            return None;
        }
        let attempt = self.retry.get(&peer.id).map_or(0, |r| r.attempt);
        Some(candidates[attempt % candidates.len()])
    }
    fn local(&mut self, notify: bool) {
        self.deferred_clipboard.clear();
        let old = std::mem::replace(&mut self.state, State::Local);
        if notify {
            if let Some(id) = old.peer() {
                if let Some(l) = self.links.get(id) {
                    l.send(Msg::ReleaseAll);
                }
            }
        }
        if let State::Controlling { side, pos, .. } = old {
            self.platform.stop_capture();
            self.platform.place_on_return(side, pos);
        }
        if let State::Controlled { entered, .. } = old {
            if let Some((x, y)) = self.platform.cursor() {
                let displays = self.platform.displays();
                let pos = geometry::edge_ratio(&displays, entered, x, y);
                if let Some((x, y)) = geometry::edge_position(&displays, entered, pos) {
                    self.platform.warp(x, y);
                }
            }
        }
        self.release();
        self.captured_keys.clear();
        self.mapper = KeyMapper::default();
        self.edge_wait = None;
        self.edge_hit = None;
        self.outward = false;
    }
    fn release(&mut self) {
        for usage in self.keys.drain() {
            self.platform
                .inject(&InputEvent::Key { usage, down: false });
        }
        for button in self.buttons.drain() {
            self.platform.inject(&InputEvent::Button {
                button,
                down: false,
            });
        }
    }
    fn inject(&mut self, mut event: InputEvent, legacy: bool) {
        match &mut event {
            InputEvent::Key { usage, down } => {
                if legacy && self.platform.os() == Os::Mac {
                    let Some(u) = keymap::mac_received_usage(*usage, &self.config.lang1_on_mac)
                    else {
                        return;
                    };
                    *usage = u;
                }
                if *down {
                    self.keys.insert(*usage);
                } else {
                    self.keys.remove(usage);
                }
            }
            InputEvent::Button { button, down } => {
                if *down {
                    self.buttons.insert(*button);
                } else {
                    self.buttons.remove(button);
                }
            }
            InputEvent::Motion { dx, dy } => {
                if let State::Controlled { entered, .. } = self.state {
                    self.outward = match entered {
                        Side::Left => *dx < 0.0,
                        Side::Right => *dx > 0.0,
                        Side::Top => *dy < 0.0,
                        Side::Bottom => *dy > 0.0,
                    };
                }
            }
            _ => {}
        }
        self.platform.inject(&event);
    }
    fn capture_event(&mut self, mut event: InputEvent) {
        let now = self.platform.captured_at(&event).unwrap_or_else(Instant::now);
        let caps_hold = if matches!(event, InputEvent::Key { usage: 0x39, down: false }) {
            self.platform.caps_hold_ms()
        } else {
            None
        };
        self.enforce_send_permission();
        let State::Controlling { peer, .. } = &self.state else {
            return;
        };
        let peer = peer.clone();
        if !event.valid() {
            return;
        }
        if let InputEvent::Key { usage, down } = &mut event {
            if *down {
                self.captured_keys.insert(*usage);
            } else {
                self.captured_keys.remove(usage);
            }
            let has = |a, b| self.captured_keys.contains(&a) || self.captured_keys.contains(&b);
            if *usage == 0x14 && *down && has(0xe0, 0xe4) && has(0xe1, 0xe5) && has(0xe2, 0xe6) {
                self.local(true);
                return;
            }
        }
        if let Some(l) = self.links.get(&peer) {
            match self.config.rules_for(self.platform.os(), l.os) {
                Some(rules) => {
                    if let Some(ms) = caps_hold {
                        self.mapper.caps_hold_ms(ms);
                    }
                    for event in self.mapper.map(event, self.platform.os(), l.os, &rules, now) {
                        l.send(Msg::Input(event));
                    }
                }
                None => { l.send(Msg::Input(event)); }
            }
        }
    }

    fn switch_channel(&self, id: &str) {
        if let Some(p) = self.config.peers.iter().find(|p| p.id == id) {
            if (1..=3).contains(&p.logitech_channel) {
                (self.hooks.switch_logitech)(p.logitech_channel);
            }
        }
    }
    /// Takes control of `peer` across `side`.
    fn cross_to(&mut self, peer: String, side: Side, pos: f64) {
        if self.capture_block.is_some_and(|t| Instant::now() < t) {
            self.edge_hit = None;
            self.edge_wait = None;
            return;
        }
        // Discard events from an earlier capture before installing the next sink.
        while self.captured.try_recv().is_ok() {}
        if let Err(e) = self.platform.start_capture(self.capture.clone()) {
            self.edge_hit = None;
            self.edge_wait = None;
            self.platform.stop_capture();
            self.capture_backoff = if self.capture_backoff.is_zero() {
                CAPTURE_RETRY_MIN
            } else {
                (self.capture_backoff * 2).min(CAPTURE_RETRY_MAX)
            };
            self.capture_block = Some(Instant::now() + self.capture_backoff);
            self.status.lock().unwrap().me.error = Some(format!(
                "Cannot capture input to cross to the next screen ({e}). Retrying in {} s.",
                self.capture_backoff.as_secs()
            ));
            return;
        }
        if self.capture_block.take().is_some() {
            self.status.lock().unwrap().me.error = None;
        }
        self.capture_backoff = Duration::ZERO;
        self.edge_hit = None;
        self.edge_wait = None;
        self.send_clipboard(&peer);
        if let Some(l) = self.links.get(&peer) {
            l.send(Msg::Enter { side, pos });
        }
        self.state = State::Controlling {
            peer: peer.clone(),
            side,
            pos,
        };
        self.captured_keys.clear();
        self.mapper = KeyMapper::default();
        // Modifiers already held when the capture began were pressed before the crossing;
        // releasing one alone must not open the target's Start menu or Activities view.
        while let Ok(event) = self.captured.try_recv() {
            let carried = match event {
                InputEvent::Key { usage, down: true } if (0xe0..=0xe7).contains(&usage) => Some(usage),
                _ => None,
            };
            self.capture_event(event);
            if let Some(usage) = carried {
                self.mapper.carry(usage);
            }
        }
        self.switch_channel(&peer);
    }
    fn cross_on_request(&mut self, side: Side, pos: f64) {
        if !self.config.on || !matches!(self.state, State::Local) || !pos.is_finite() {
            return;
        }
        if self.platform.ready().is_err() {
            return;
        }
        let Some(peer) = self.neighbour_peer(side) else {
            return;
        };
        self.cross_to(peer, side, pos.clamp(0.0, 1.0));
    }
    fn poll_cursor(&mut self) {
        if !matches!(self.state, State::Local) && self.platform.ready().is_err() {
            self.local(true);
        }
        let mut sides = Vec::new();
        for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            if self.neighbour_peer(side).is_some() {
                sides.push(side);
            }
        }
        if self
            .edge_hit
            .is_some_and(|(side, _, _)| !sides.contains(&side))
        {
            self.edge_hit = None;
            self.edge_wait = None;
            self.platform.stop_capture();
        }
        if sides != self.armed {
            self.platform.arm_edges(&sides);
            self.armed = sides;
        }
        let displays = self.platform.displays();
        let cursor = self.platform.cursor();
        if let Some((side, pos)) = self.platform.take_edge_hit() {
            if pos.is_finite() && self.armed.contains(&side) && matches!(self.state, State::Local) {
                self.edge_hit = Some((side, pos.clamp(0.0, 1.0), Instant::now()));
            } else if cursor.is_none() {
                self.edge_hit = None;
                self.platform.stop_capture();
            }
        }
        let (edge, pos, x, y) = if let Some((x, y)) = cursor {
            let edge = geometry::edge_at(&displays, x, y, self.config.corner_margin);
            (
                edge,
                edge.map(|side| geometry::edge_ratio(&displays, side, x, y)),
                x,
                y,
            )
        } else if let Some((side, pos, since)) = self.edge_hit {
            self.edge_wait = Some((side, since));
            (Some(side), Some(pos), 0.0, 0.0)
        } else {
            self.edge_wait = None;
            return;
        };
        match self.state.clone() {
            State::Local if self.config.app_drives_edges && cursor.is_some() => {
                self.edge_wait = None;
            }
            State::Local => {
                let Some(side) = edge else {
                    self.edge_wait = None;
                    if self.capture_block.is_some_and(|t| Instant::now() >= t) {
                        self.capture_backoff = Duration::ZERO;
                    }
                    return;
                };
                let Some(peer) = self.neighbour_peer(side) else {
                    self.edge_wait = None;
                    return;
                };
                if self.platform.ready().is_err()
                    || (self.config.require_control && !self.platform.control_down())
                {
                    if self.edge_hit.take().is_some() {
                        self.platform.stop_capture();
                    }
                    self.edge_wait = None;
                    return;
                }
                let since = match self.edge_wait {
                    Some((s, t)) if s == side => t,
                    _ => {
                        let t = Instant::now();
                        self.edge_wait = Some((side, t));
                        t
                    }
                };
                if since.elapsed() < Duration::from_millis(self.config.cross_delay_ms) {
                    return;
                }
                self.cross_to(peer, side, pos.unwrap_or(0.5));
            }
            State::Controlled { peer, entered } => {
                let native_out = self.last_cursor.is_some_and(|(px, py)| match entered {
                    Side::Left => x < px,
                    Side::Right => x > px,
                    Side::Top => y < py,
                    Side::Bottom => y > py,
                });
                let control_ok = !self.config.require_control || self.control_held(&peer);
                if edge == Some(entered) && (self.outward || native_out) && control_ok {
                    let pos = geometry::edge_ratio(&displays, entered, x, y);
                    self.send_clipboard(&peer);
                    if let Some(l) = self.links.get(&peer) {
                        l.send(Msg::Leave { pos });
                    }
                    self.local(false);
                    let cells = self.my_cells();
                    if layout::neighbour(&cells, &self.identity.id, entered) == Some(peer.as_str()) {
                        self.switch_channel(&peer);
                    }
                }
            }
            State::Controlling { .. } => {}
        }
        self.last_cursor = cursor;
    }
    /// While controlled: Control held on the controlling computer's keyboard (as injected
    /// here) or on this computer's own. Its Control key arrives here as whatever the key
    /// rules between the two systems turn it into (Command/Windows when swapped).
    fn control_held(&self, peer: &str) -> bool {
        let mine = self.platform.os();
        let control: Vec<u16> = match self.known_peer(peer).and_then(|p| self.config.rules_for(p.os, mine).map(|r| (p.os, r))) {
            Some((os, rules)) => [0xE0, 0xE4].map(|u| keymap::map_modifier(os, mine, &rules, u)).to_vec(),
            None => vec![0xE0, 0xE4],
        };
        self.keys.iter().any(|k| control.contains(k)) || self.platform.control_down()
    }
    /// Sends what this computer's clipboard holds to `peer` (files, else an image, else
    /// text) if sharing is on and it changed since it was last sent or received. Files
    /// and images over the size limit stay here; the status says so.
    fn send_clipboard(&mut self, peer: &str) {
        use crate::transfer::{ClipKind, Outgoing, INLINE_TEXT, MAX_TEXT};
        if !self.config.share_clipboard || !self.links.contains_key(peer) {
            return;
        }
        if self.file_jobs.receiving() {
            self.deferred_clipboard.insert(peer.into());
            return;
        }
        let limit = u64::from(crate::transfer::LIMIT_MB) * 1024 * 1024;
        if limit > 0 {
            let mut files = self.platform.clipboard_files();
            files.truncate(crate::transfer::MAX_FILES);
            files.shrink_to_fit();
            // Android cannot use files: text and images still go.
            let android = self.links[peer].os == Os::Android;
            let had_files = !files.is_empty();
            if android { files.clear(); }
            if !files.is_empty() {
                let id = self.take_transfer_id();
                self.file_jobs.submit(files::Job {
                    peer: peer.into(), token: self.links[peer].token, revision: self.transfer_revision,
                    work: files::Work::Send { id, paths: files, limit },
                });
                return;
            }
            // Files on the clipboard bring an icon image along: for Android that is not content.
            let png = if android && had_files { None } else { self.platform.clipboard_image_png() };
            if let Some(png) = png {
                let sig = clip_sig(b"image", &[&png]);
                if !self.clip_wanted(peer, &sig) {
                    return;
                }
                if png.len() as u64 > limit {
                    self.transfer_note("send", "image", "too_large");
                    return;
                }
                let id = self.take_transfer_id();
                self.begin_send(peer, "image", Outgoing::bytes(id, ClipKind::Image, "clipboard.png", png), sig);
                return;
            }
        }
        let Some(text) = self.platform.clipboard_text().filter(|t| !t.is_empty()) else {
            return;
        };
        let sig = clip_sig(b"text", &[text.as_bytes()]);
        if !self.clip_wanted(peer, &sig) {
            return;
        }
        if text.len() <= INLINE_TEXT {
            if let Some(l) = self.links.get(peer) {
                if l.tx.try_send(Msg::Clipboard { text }).is_ok() {
                    self.clip_sigs.insert(peer.into(), sig);
                }
            }
        } else if text.len() as u64 <= MAX_TEXT {
            let id = self.take_transfer_id();
            self.begin_send(peer, "text", Outgoing::bytes(id, ClipKind::Text, "clipboard.txt", text.into_bytes()), sig);
        } else {
            self.transfer_note("send", "text", "too_large");
        }
    }
    /// Content just arrived from `peer`: it has it, and this is where it came from.
    fn clip_received(&mut self, peer: &str, sig: Vec<u8>) {
        self.clip_sigs.insert(peer.into(), sig.clone());
        let from_sub = self.known_peer(peer).is_some_and(|p| p.role == Role::Sub);
        self.clip_origin = Some((sig, peer.into(), from_sub));
    }
    /// Whether clipboard content `sig` should go to `peer`: it does not have it yet, and
    /// with relay off it did not come from another sub (when `peer` is a sub too).
    fn clip_wanted(&self, peer: &str, sig: &[u8]) -> bool {
        if self.clip_sigs.get(peer).map(Vec::as_slice) == Some(sig) {
            return false;
        }
        if self.pending_clip.get(peer).is_some_and(|(_, pending)| pending.as_slice() == sig) {
            return false;
        }
        if !self.config.clipboard_relay {
            let is_sub = |id: &str| self.known_peer(id).is_some_and(|p| p.role == Role::Sub);
            if let Some((origin, from, from_sub)) = &self.clip_origin {
                if origin.as_slice() == sig && from != peer && *from_sub && is_sub(peer) {
                    return false;
                }
            }
        }
        true
    }
    fn take_transfer_id(&mut self) -> u32 {
        self.next_transfer = self.next_transfer.wrapping_add(1).max(1);
        self.next_transfer
    }
    fn begin_send(&mut self, peer: &str, kind: &str, out: crate::transfer::Outgoing, sig: Vec<u8>) {
        // A different target must not interrupt the same clipboard's active send.
        // New content still supersedes every target waiting for the old clipboard.
        let changed = self.outgoing.iter().map(|(peer, _)| peer)
            .chain(self.waiting_sends.iter().map(|(peer, _, _)| peer))
            .any(|peer| self.pending_clip.get(peer).is_some_and(|(_, old)| old != &sig));
        if changed { self.drop_stale_sends(); }
        if !self.clip_wanted(peer, &sig) { return; }
        self.pending_clip.insert(peer.into(), (out.id(), sig));
        self.waiting_sends.push_back((peer.into(), kind.into(), out));
        self.start_next_send();
    }
    fn start_next_send(&mut self) {
        if self.outgoing.is_some() { return; }
        let Some((peer, kind, out)) = self.waiting_sends.pop_front() else { return };
        self.transfer_revision = self.transfer_revision.wrapping_add(1);
        self.status.lock().unwrap().transfer = Some(TransferStatus {
            direction: "send".into(),
            kind,
            done_bytes: 0,
            total_bytes: out.total,
            stage: "running".into(),
            saved_in: None,
        });
        self.outgoing = Some((peer, out));
    }
    fn transfer_note(&mut self, direction: &str, kind: &str, stage: &str) {
        self.status.lock().unwrap().transfer = Some(TransferStatus {
            direction: direction.into(),
            kind: kind.into(),
            done_bytes: 0,
            total_bytes: 0,
            stage: stage.into(),
            saved_in: None,
        });
    }
    /// Hands the next pieces of a transfer to its link, a few at a time, so input sent
    /// meanwhile is never stuck behind a long queue.
    fn flush_transfer(&mut self) {
        self.start_next_send();
        let Some((peer, out)) = &mut self.outgoing else { return };
        let Some(link) = self.links.get(peer.as_str()) else {
            self.pending_clip.remove(peer.as_str());
            self.outgoing = None;
            self.transfer_note("send", "files", "failed");
            return;
        };
        while link.bulk.load(Ordering::Acquire) < 2 {
            match out.next() {
                Ok(Some(msg)) => {
                    let done = matches!(msg, Msg::ClipDone { .. });
                    let chunk = matches!(msg, Msg::ClipChunk { .. });
                    if chunk {
                        link.bulk.fetch_add(1, Ordering::AcqRel);
                    }
                    if let Err(msg) = link.tx.try_send(msg) {
                        if chunk { link.bulk.fetch_sub(1, Ordering::AcqRel); }
                        out.retry(msg);
                        return;
                    }
                    if done {
                        if let Some(sig) = out.sent_signature() {
                            self.pending_clip.insert(peer.clone(), (out.id(), sig.to_vec()));
                        }
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log::warn!("clipboard transfer: {e:#}");
                    self.pending_clip.remove(peer.as_str());
                    self.outgoing = None;
                    self.transfer_note("send", "files", "failed");
                    return;
                }
            }
        }
        let (sent, total) = (out.sent, out.total);
        let finished = out.finished();
        let mut status = self.status.lock().unwrap();
        if let Some(t) = &mut status.transfer {
            t.done_bytes = sent;
            if finished {
                t.stage = "done".into();
                t.done_bytes = total;
            }
        }
        drop(status);
        if finished {
            self.outgoing = None;
        }
    }
    /// The local clipboard is about to be replaced by what a peer sent: a send of the old
    /// content, hashing or already streaming, would overwrite that peer's newer clipboard.
    fn drop_stale_sends(&mut self) {
        let hashing = self.file_jobs.cancel_direction(0);
        let outgoing = self.outgoing.take();
        if let Some((peer, _)) = &outgoing { self.pending_clip.remove(peer); }
        let waiting = !self.waiting_sends.is_empty();
        for (peer, _, _) in self.waiting_sends.drain(..) { self.pending_clip.remove(&peer); }
        if outgoing.is_some() || waiting || hashing {
            self.fail_running_transfer();
        }
    }
    fn clip_start(&mut self, peer: &str, id: u32, kind: crate::transfer::ClipKind, files: Vec<crate::transfer::FileMeta>) {
        use crate::transfer::{ClipKind, Incoming};
        self.file_jobs.cancel_direction(1);
        self.transfer_revision = self.transfer_revision.wrapping_add(1);
        if let Some((_, old)) = self.incoming.take() {
            old.abandon();
        }
        let kind_name = match kind {
            ClipKind::Files => "files",
            ClipKind::Image => "image",
            ClipKind::Text => "text",
        };
        if !self.config.receives_clipboard() {
            return;
        }
        // Android has no place for files: refuse them without downloading.
        if kind == ClipKind::Files && self.platform.os() == Os::Android {
            return;
        }
        if self.config.share_clipboard {
            self.drop_stale_sends();
        }
        let limit = u64::from(crate::transfer::LIMIT_MB) * 1024 * 1024;
        let work = self.config.config_dir.join("flockswitch");
        match Incoming::start(id, kind, files, limit, &work) {
            Ok(incoming) => {
                self.status.lock().unwrap().transfer = Some(TransferStatus {
                    direction: "receive".into(),
                    kind: kind_name.into(),
                    done_bytes: 0,
                    total_bytes: incoming.total,
                    stage: "running".into(),
                    saved_in: None,
                });
                self.incoming = Some((peer.into(), incoming));
                self.incoming_at = Instant::now();
            }
            Err(e) => {
                log::info!("clipboard transfer refused: {e:#}");
                let large = e.to_string().starts_with("too large");
                self.transfer_note("receive", kind_name, if large { "too_large" } else { "failed" });
            }
        }
    }
    fn clip_chunk(&mut self, peer: &str, id: u32, file: u32, data: &str) {
        let Some((from, incoming)) = &mut self.incoming else { return };
        if from != peer || incoming.id != id {
            return;
        }
        self.incoming_at = Instant::now();
        if let Err(e) = incoming.chunk(file, data) {
            log::warn!("clipboard transfer: {e:#}");
            if let Some((_, i)) = self.incoming.take() {
                i.abandon();
            }
            self.transfer_note("receive", "files", "failed");
            return;
        }
        let done = incoming.received;
        if let Some(t) = &mut self.status.lock().unwrap().transfer {
            t.done_bytes = done;
        }
    }
    fn clip_done(&mut self, peer: &str, id: u32) {
        let Some((from, incoming)) = self.incoming.take() else { return };
        if from != peer || incoming.id != id {
            self.incoming = Some((from, incoming));
            return;
        }
        let dest = self
            .platform
            .received_dir()
            .unwrap_or_else(|| self.config.config_dir.join("flockswitch").join("received"));
        self.file_jobs.submit(files::Job {
            peer: peer.into(), token: self.links.get(peer).map_or(0, |l| l.token), revision: self.transfer_revision,
            work: files::Work::Receive { incoming, dest },
        });
    }
    fn poll_file_jobs(&mut self) {
        if let Some(done) = self.file_jobs.poll() { self.apply_file_job(done); }
        if !self.file_jobs.receiving() {
            for peer in std::mem::take(&mut self.deferred_clipboard) {
                // Only while still in a session with that very peer.
                if self.state.peer() == Some(peer.as_str()) { self.send_clipboard(&peer); }
            }
        }
    }
    fn apply_file_job(&mut self, done: files::Completion) {
        use crate::transfer::Received;
        if !self.config.on || !self.config.share_clipboard
            || !self.links.get(&done.peer).is_some_and(|l| l.token == done.token)
        { return; }
        match done.result {
            Ok(files::Content::Send(out, sig)) => {
                if self.clip_wanted(&done.peer, &sig) { self.begin_send(&done.peer, "files", out, sig); }
            }
            Ok(files::Content::Receive(mut receipt)) => {
                let current = done.revision == self.transfer_revision;
                let sig = receipt.sig.take();
                match receipt.received.take().unwrap() {
                    Received::Files(paths) => {
                        // Completion and relay fingerprints precede clipboard files.
                        if let Some(sig) = sig { self.clip_received(&done.peer, sig); }
                        if current { self.transfer_finished(Some(receipt.dest.display().to_string())); }
                        self.platform.set_clipboard_files(&paths);
                    }
                    other => {
                        match other {
                            Received::Image(png) => self.platform.set_clipboard_image_png(&png),
                            Received::Text(text) => self.platform.set_clipboard_text(&text),
                            Received::Files(_) => unreachable!(),
                        }
                        if let Some(sig) = sig { self.clip_received(&done.peer, sig); }
                        if current { self.transfer_finished(None); }
                    }
                }
            }
            Err(e) => {
                log::info!("clipboard files: {e:#}");
                if done.revision == self.transfer_revision {
                    let large = e.to_string().starts_with("too large");
                    self.transfer_note(if done.direction == 0 { "send" } else { "receive" }, "files", if large { "too_large" } else { "failed" });
                }
            }
        }
    }
    fn transfer_finished(&mut self, saved_in: Option<String>) {
        if let Some(t) = &mut self.status.lock().unwrap().transfer {
            t.stage = "done".into();
            t.done_bytes = t.total_bytes;
            t.saved_in = saved_in;
        }
    }
    fn fail_running_transfer(&self) {
        if let Some(t) = &mut self.status.lock().unwrap().transfer {
            if t.stage == "running" { t.stage = "failed".into(); }
        }
    }
    fn publish(&mut self, force: bool) {
        let mut status = self.status.lock().unwrap();
        status.running = self.config.on && self.listener.is_some();
        if status.running {
            let error = self.platform.ready().err();
            if error != self.readiness_error {
                if error.is_some() || status.me.error == self.readiness_error {
                    status.me.error = error.clone();
                }
                self.readiness_error = error;
            }
        }
        status.me.name = computer_name(&self.config);
        status.me.can_send = self.me_can_send();
        status.state = match &self.state {
            State::Local => StateStatus {
                mode: "local".into(),
                peer: None,
            },
            State::Controlling { peer, .. } => StateStatus {
                mode: "controlling".into(),
                peer: Some(peer.clone()),
            },
            State::Controlled { peer, .. } => StateStatus {
                mode: "controlled".into(),
                peer: Some(peer.clone()),
            },
        };
        status.peers = self
            .config
            .peers
            .iter()
            .chain(&self.trusted)
            .map(|p| PeerStatus {
                id: p.id.clone(),
                name: p.name.clone(),
                os: Some(p.os),
                connected: self.links.get(&p.id).is_some_and(|l| l.paired),
                rtt_ms: self.links.get(&p.id).and_then(|l| l.rtt),
                can_send: self.peer_can_send(&p.id),
                via_master: p.via_master,
                keyrules: self.links.get(&p.id).is_some_and(|l| l.paired && l.features.iter().any(|f| f == "keyrules")),
            })
            .collect();
        status.layout = self.my_cells();
        status.keys = self.config.all_rules();
        if let Some(since) = self.reconnecting {
            let down = self.config.peers.iter().any(|p| !self.open(&p.id));
            if since.elapsed() >= RECONNECT_SHOWN || (!down && since.elapsed() >= Duration::from_millis(500)) {
                self.reconnecting = None;
            }
        }
        status.reconnecting = self.reconnecting.is_some();
        status.discovered = self
            .discovered
            .iter()
            .map(|p| DiscoveredStatus {
                id: p.id.clone(),
                name: p.name.clone(),
                os: p.os,
                addr: p
                    .addresses
                    .first()
                    .map(ToString::to_string)
                    .unwrap_or_default(),
                paired: self
                    .config
                    .peers
                    .iter()
                    .any(|c| c.id == p.id && c.fingerprint == p.fingerprint),
            })
            .collect();
        status.discovered.sort_by(|a, b| a.id.cmp(&b.id));
        status.pairing = self.pair_status.clone();
        let permissions = self.platform.permissions();
        if (force || self.last_write.elapsed() >= Duration::from_millis(300))
            && (self.written.as_ref() != Some(&*status) || self.written_permissions != permissions)
        {
            match crate::write_status_with_permissions(&self.config.config_dir, &status, permissions) {
                Ok(()) => {
                    self.written = Some(status.clone());
                    self.written_permissions = permissions;
                    self.last_write = Instant::now();
                }
                Err(e) => {
                    log::warn!("status file: {e:#}");
                    self.last_write = Instant::now();
                }
            }
        }
    }
}

struct Worker {
    identity: Identity,
    name: String,
    os: Os,
    role: Role,
    listen_port: u16,
    client: Arc<rustls::ClientConfig>,
    server: Arc<rustls::ServerConfig>,
    events: Sender<Event>,
    wake: Sender<Command>,
    cancel: Arc<AtomicBool>,
    addr: SocketAddr,
    outgoing: bool,
    pair: bool,
    expected: Option<String>,
}
impl Worker {
    fn emit(&self, event: Event) -> Result<()> {
        self.events.send(event)?;
        let _ = self.wake.send(Command::Wake);
        Ok(())
    }
    fn run(self, socket: Option<TcpStream>) {
        let token = NEXT_LINK.fetch_add(1, Ordering::Relaxed);
        let mut remote = None;
        if let Err(e) = self.transport(socket, token, &mut remote) {
            log::debug!("network link {}: {e:#}", self.addr);
        }
        if let Some(id) = remote {
            let _ = self.emit(Event::Closed(id, token));
        }
        if self.outgoing {
            let _ = self.emit(Event::DialDone(self.addr));
        }
    }
    fn transport(
        &self,
        socket: Option<TcpStream>,
        token: u64,
        remote: &mut Option<String>,
    ) -> Result<()> {
        let mut socket = match socket {
            Some(s) => s,
            None => TcpStream::connect_timeout(&self.addr, Duration::from_millis(700))?,
        };
        socket.set_nodelay(true)?;
        socket.set_read_timeout(Some(Duration::from_millis(100)))?;
        socket.set_write_timeout(Some(Duration::from_millis(100)))?;
        let mut conn = if self.outgoing {
            rustls::Connection::Client(rustls::ClientConnection::new(
                self.client.clone(),
                "flockswitch.local".try_into()?,
            )?)
        } else {
            rustls::Connection::Server(rustls::ServerConnection::new(self.server.clone())?)
        };
        let deadline = Instant::now() + DEAD;
        while conn.is_handshaking() {
            anyhow::ensure!(
                !self.cancel.load(Ordering::Acquire) && Instant::now() < deadline,
                "handshake timeout"
            );
            match conn.complete_io(&mut socket) {
                Ok(_) => {}
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        let fp = tls::peer_fingerprint(&conn)?;
        socket.set_nonblocking(true)?;
        proto::write_frame(
            conn.writer(),
            &Msg::Hello {
                proto: PROTO_VERSION,
                id: self.identity.id.clone(),
                name: self.name.clone(),
                os: self.os,
                app_version: env!("CARGO_PKG_VERSION").into(),
                role: self.role,
                features: proto::FEATURES.iter().map(|f| f.to_string()).collect(),
                listen_port: Some(self.listen_port),
            },
        )?;
        let (tx, rx) = queue::channel();
        let mut tx = Some(tx);
        let bulk = Arc::new(AtomicUsize::new(0));
        let mut pending_clip = None;
        let mut queued = None;
        let mut bytes = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            let cancelled = self.cancel.load(Ordering::Acquire);
            if cancelled {
                // The link is going away: still let ReleaseAll and Bye reach the peer,
                // or its keys and buttons could stay held.
                return flush_goodbye(&mut conn, &mut socket, &rx, &mut queued);
            }
            write_link_batch(&mut conn, &mut socket, &rx, &mut queued, &bulk, &mut pending_clip)?;
            while conn.wants_write() {
                match conn.write_tls(&mut socket) {
                    Ok(0) => anyhow::bail!("socket closed"),
                    Ok(_) => {}
                    Err(e) if transient(&e) => break,
                    Err(e) => return Err(e.into()),
                }
            }
            if !conn.wants_write() {
                if let (Some(id), Some(peer)) = (pending_clip.take(), remote.as_ref()) {
                    self.emit(Event::ClipSent(peer.clone(), token, id))?;
                }
            }
            match conn.read_tls(&mut socket) {
                Ok(0) => anyhow::bail!("socket closed"),
                Ok(_) => {
                    conn.process_new_packets()?;
                }
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e.into()),
            }
            loop {
                match conn.reader().read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                    Err(e) if transient(&e) => break,
                    Err(e) => return Err(e.into()),
                }
            }
            while let Some(msg) = proto::take_frame(&mut bytes)? {
                if remote.is_none() {
                    let Msg::Hello {
                        proto,
                        id,
                        name,
                        os,
                        role,
                        features,
                        listen_port,
                        ..
                    } = msg
                    else {
                        anyhow::bail!("Hello required")
                    };
                    if proto != PROTO_VERSION {
                        proto::write_frame(
                            conn.writer(),
                            &Msg::Bye {
                                reason: "version".into(),
                            },
                        )?;
                        let _ = conn.write_tls(&mut socket);
                        anyhow::bail!("protocol version mismatch");
                    }
                    anyhow::ensure!(id == fp[..16], "identity does not match certificate");
                    anyhow::ensure!(
                        self.expected
                            .as_ref()
                            .is_none_or(|expected| expected == &id),
                        "unexpected device"
                    );
                    *remote = Some(id.clone());
                    let listen_port = listen_port.filter(|&port| port != 0);
                    // Keep the socket's real IP and the advertised listening port.
                    let mut addr = canonical(self.addr);
                    if !self.outgoing { addr.set_port(listen_port.unwrap_or(47160)); }
                    self.emit(Event::Connected(
                        id,
                        Link {
                            token,
                            tx: tx.take().unwrap(),
                            cancel: self.cancel.clone(),
                            name,
                            os,
                            role,
                            features,
                            fp: fp.clone(),
                            addr,
                            listen_port,
                            outgoing: self.outgoing,
                            paired: false,
                            last_rx: Instant::now(),
                            last_ping: Instant::now(),
                            ping: None,
                            rtt: None,
                            bulk: bulk.clone(),
                        },
                        self.pair,
                    ))?;
                } else {
                    self.emit(Event::Message(
                        remote.as_ref().unwrap().clone(),
                        token,
                        msg,
                    ))?;
                }
            }
            anyhow::ensure!(
                remote.is_some() || Instant::now() < deadline,
                "Hello timeout"
            );
            if queued.is_none() { queued = rx.recv_timeout(Duration::from_millis(2)).ok(); }
            else { thread::sleep(Duration::from_millis(2)); }
        }
    }
}
/// How long a closing link may take to hand ReleaseAll and Bye to a slow socket.
const GOODBYE: Duration = Duration::from_millis(250);
/// Final flush of a cancelled link: only ReleaseAll and Bye are still sent, everything
/// else queued is dropped, and a socket that stays full gives up after `GOODBYE`.
fn flush_goodbye(
    conn: &mut rustls::Connection,
    socket: &mut impl std::io::Write,
    rx: &queue::Receiver,
    queued: &mut Option<Msg>,
) -> Result<()> {
    let deadline = Instant::now() + GOODBYE;
    while let Some(msg) = queued.take().map(Ok).unwrap_or_else(|| rx.try_recv()).ok() {
        if matches!(msg, Msg::ReleaseAll | Msg::Bye { .. }) {
            proto::write_frame(conn.writer(), &msg)?;
        }
    }
    conn.send_close_notify();
    loop {
        while conn.wants_write() {
            match conn.write_tls(&mut *socket) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(e) if transient(&e) => break,
                Err(_) => return Ok(()),
            }
        }
        if !conn.wants_write() || Instant::now() >= deadline {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(2));
    }
}
/// Flush small frames in one wake, while bounding clipboard work and socket backlog.
fn write_link_batch(
    conn: &mut rustls::Connection,
    socket: &mut impl std::io::Write,
    rx: &queue::Receiver,
    queued: &mut Option<Msg>,
    bulk: &AtomicUsize,
    pending_clip: &mut Option<u32>,
) -> Result<()> {
    // Drain small frames while the socket accepts them. A large clipboard
    // chunk yields to reading, keeping input responsive in both directions.
    for _ in 0..128 {
        while conn.wants_write() {
            match conn.write_tls(&mut *socket) {
                Ok(0) => anyhow::bail!("socket closed"),
                Ok(_) => {}
                Err(e) if transient(&e) => break,
                Err(e) => return Err(e.into()),
            }
        }
        if conn.wants_write() {
            break;
        }
        match queued.take().map(Ok).unwrap_or_else(|| rx.try_recv()) {
            Ok(msg) => {
                proto::write_frame(conn.writer(), &msg)?;
                if let Msg::ClipDone { id } = msg {
                    *pending_clip = Some(id);
                }
                if matches!(msg, Msg::ClipChunk { .. }) {
                    bulk.fetch_sub(1, Ordering::AcqRel);
                    break;
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}
/// A dual-stack listener reports an IPv4 peer as `::ffff:a.b.c.d`; keep the plain form,
/// which every OS can dial.
fn canonical(addr: SocketAddr) -> SocketAddr {
    SocketAddr::new(addr.ip().to_canonical(), addr.port())
}
fn transient(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    )
}

/// A fingerprint of clipboard content: what it is plus its bytes.
fn clip_sig(kind: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(kind);
    for p in parts {
        h.update((p.len() as u64).to_le_bytes());
        h.update(p);
    }
    h.finalize().to_vec()
}
/// Ordered file contents, independent of local paths, collision suffixes and mtimes.
#[cfg(test)]
fn files_sig(files: &[std::path::PathBuf]) -> Result<Vec<u8>> {
    files_sig_cancellable(files, &AtomicBool::new(false))
}
fn files_sig_cancellable(files: &[std::path::PathBuf], cancel: &AtomicBool) -> Result<Vec<u8>> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"files");
    h.update((files.len() as u64).to_le_bytes());
    let mut buffer = [0; 64 * 1024];
    for path in files {
        anyhow::ensure!(std::fs::metadata(path)?.is_file(), "not a regular clipboard file");
        let mut file = std::fs::File::open(path)?;
        let size = file.metadata()?.len();
        h.update(size.to_le_bytes());
        let mut read = 0u64;
        loop {
            anyhow::ensure!(!cancel.load(Ordering::Acquire), "cancelled");
            let n = match file.read(&mut buffer) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if n == 0 { break; }
            read += n as u64;
            h.update(&buffer[..n]);
        }
        anyhow::ensure!(read == size, "clipboard file changed while reading");
    }
    Ok(h.finalize().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dial_backoff_is_1_2_4_8_and_stays_at_8() {
        let waits: Vec<u64> = (0..8).map(backoff_secs).collect();
        assert_eq!(waits, [1, 2, 4, 8, 8, 8, 8, 8]);
        assert_eq!(backoff_secs(usize::MAX), 8);
    }
    #[derive(Default)]
    struct TestPlatform {
        injected: Mutex<Vec<InputEvent>>,
        text: Mutex<Option<String>>,
        files: Mutex<Vec<std::path::PathBuf>>,
        image: Mutex<Option<Vec<u8>>>,
        stopped: AtomicUsize,
        capture_fail: AtomicUsize,
        capture_calls: AtomicUsize,
        mac: AtomicBool,
        android: AtomicBool,
        on_files: Mutex<Option<Box<dyn Fn() + Send>>>,
    }
    impl Platform for TestPlatform {
        fn os(&self) -> Os {
            if self.android.load(Ordering::Relaxed) { Os::Android }
            else if self.mac.load(Ordering::Relaxed) { Os::Mac } else { Os::Windows }
        }
        fn displays(&self) -> Vec<Rect> { vec![Rect::new(0.0, 0.0, 100.0, 100.0)] }
        fn cursor(&self) -> Option<(f64, f64)> { Some((50.0, 50.0)) }
        fn warp(&self, _: f64, _: f64) {}
        fn start_capture(&self, _: Sender<InputEvent>) -> Result<()> {
            self.capture_calls.fetch_add(1, Ordering::Relaxed);
            let mut n = self.capture_fail.load(Ordering::Relaxed);
            while n > 0 {
                match self.capture_fail.compare_exchange(n, n - 1, Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => { anyhow::bail!("tap denied"); }
                    Err(now) => n = now,
                }
            }
            Ok(())
        }
        fn stop_capture(&self) { self.stopped.fetch_add(1, Ordering::Relaxed); }
        fn inject(&self, event: &InputEvent) { self.injected.lock().unwrap().push(event.clone()); }
        fn ready(&self) -> Result<(), String> { Ok(()) }
        fn clipboard_text(&self) -> Option<String> { self.text.lock().unwrap().clone() }
        fn set_clipboard_text(&self, text: &str) { *self.text.lock().unwrap() = Some(text.into()); }
        fn clipboard_files(&self) -> Vec<std::path::PathBuf> { self.files.lock().unwrap().clone() }
        fn clipboard_image_png(&self) -> Option<Vec<u8>> { self.image.lock().unwrap().clone() }
        fn set_clipboard_files(&self, paths: &[std::path::PathBuf]) {
            if let Some(check) = &*self.on_files.lock().unwrap() { check(); }
            *self.files.lock().unwrap() = paths.to_vec();
        }
    }
    struct Rig { c: Control, p: Arc<TestPlatform>, dir: std::path::PathBuf }
    impl Drop for Rig {
        fn drop(&mut self) {
            self.c.file_jobs.stop();
            for (cancel, _) in &self.c.workers { cancel.store(true, Ordering::Release); }
            for (_, worker) in self.c.workers.drain(..) { worker.join().unwrap(); }
            if let Some((_, incoming)) = self.c.incoming.take() { incoming.abandon(); }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    fn rig(role: Role) -> Rig {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("flock-control-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        let config = NetConfig { on: true, notice_accepted: true, discovery: false, role, config_dir: dir.clone(), ..NetConfig::default() };
        let p = Arc::new(TestPlatform::default());
        let platform: Arc<dyn Platform> = p.clone();
        let hooks = Hooks::default();
        let identity = Identity::load_or_create(&dir).unwrap();
        let server = tls::server_config(&identity).unwrap();
        let client = tls::client_config(&identity).unwrap();
        let listener = None;
        let name = "test".into();
        let port = 0;
        let status = Arc::new(Mutex::new(Status {
            running: config.on,
            me: MeStatus {
                id: identity.id.clone(),
                name,
                port,
                error: None,
                can_send: true,
            },
            discovered: vec![],
            peers: vec![],
            state: StateStatus {
                mode: "local".into(),
                peer: None,
            },
            pairing: None,
            transfer: None,
            following: None,
            layout: vec![],
            reconnecting: false,
            keys: config.all_rules(),
        }));
        let (wake, rx) = mpsc::channel();
        let (events, event_rx) = mpsc::channel();
        let (capture, captured) = mpsc::channel();
        let control = Control {
            config,
            platform,
            hooks,
            identity,
            server,
            client,
            listener,
            status: status.clone(),
            events,
            event_rx,
            rx,
            wake,
            capture,
            captured,
            links: HashMap::new(),
            workers: vec![],
            inflight: HashSet::new(),
            retry: HashMap::new(),
            state: State::Local,
            pairing: None,
            pair_status: None,
            master: None,
            told: HashMap::new(),
            peer_features: HashMap::new(),
            keys: HashSet::new(),
            clip_sigs: HashMap::new(),
            pending_clip: HashMap::new(),
            deferred_clipboard: HashSet::new(),
            recent_paired: HashMap::new(),
            unpaired_seen: HashMap::new(),
            clip_origin: None,
            trusted: vec![],
            group: None,
            reconnecting: None,
            skew: Duration::ZERO,
            last_wall: SystemTime::now(),
            outgoing: None,
            waiting_sends: VecDeque::new(),
            file_jobs: files::Jobs::default(),
            transfer_revision: 0,
            incoming: None,
            incoming_at: Instant::now(),
            next_transfer: 1,
            buttons: HashSet::new(),
            captured_keys: HashSet::new(),
            mapper: KeyMapper::default(),
            discovery: None,
            discovered: vec![],
            edge_wait: None,
            last_cursor: None,
            armed: Vec::new(),
            edge_hit: None,
            capture_block: None,
            capture_backoff: Duration::ZERO,
            outward: false,
            started: Instant::now(),
            last_write: Instant::now() - Duration::from_secs(1),
            written: None,
            written_permissions: None,
            readiness_error: None,
        };
        Rig { c: control, p, dir }
    }
    const MASTER: &str = "aaaaaaaaaaaaaaaa";
    const SUB: &str = "bbbbbbbbbbbbbbbb";
    const OTHER: &str = "cccccccccccccccc";
    fn peer(id: &str, role: Role) -> PeerConfig {
        PeerConfig { id: id.into(), fingerprint: id.repeat(4), role, os: Os::Mac, ..PeerConfig::default() }
    }
    fn link(c: &mut Control, id: &str, role: Role) -> queue::Receiver {
        let (tx, rx) = queue::channel();
        c.event(Event::Connected(id.into(), Link {
            token: 1, tx, cancel: Arc::new(AtomicBool::new(false)), name: id.into(), os: Os::Mac,
            role, features: proto::FEATURES.iter().map(|s| s.to_string()).collect(), fp: id.repeat(4),
            addr: "127.0.0.1:47160".parse().unwrap(), listen_port: None, outgoing: true, paired: false,
            last_rx: Instant::now(), last_ping: Instant::now(), ping: None, rtt: None,
            bulk: Arc::new(AtomicUsize::new(0)),
        }, false));
        rx
    }
    fn finish_file_jobs(c: &mut Control) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while c.file_jobs.busy() {
            c.poll_file_jobs();
            assert!(Instant::now() < deadline, "clipboard worker timed out");
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn message(c: &mut Control, id: &str, msg: Msg) { c.event(Event::Message(id.into(), 1, msg)); }
    fn controlling(c: &mut Control, id: &str) { c.state = State::Controlling { peer: id.into(), side: Side::Right, pos: 0.5 }; }
    fn controlled(c: &mut Control, id: &str) { c.state = State::Controlled { peer: id.into(), entered: Side::Left }; }
    fn roster(c: &Control, sender_allowed: bool) -> Vec<Member> {
        [(MASTER, true), (SUB, sender_allowed), (c.identity.id.as_str(), sender_allowed)].into_iter()
            .map(|(id, can_send)| Member { id: id.into(), name: id.into(), os: Os::Windows, fingerprint: id.repeat(4), addr: String::new(), can_send, features: proto::FEATURES.iter().map(|s| s.to_string()).collect() }).collect()
    }
    fn key(usage: u16, down: bool) -> InputEvent { InputEvent::Key { usage, down } }

    #[test]
    fn accept_pacing_is_per_ip_bounded_and_recovers() {
        let mut pace = AcceptPace::default();
        let now = Instant::now();
        let a = "192.168.1.2".parse().unwrap();
        let b = "192.168.1.3".parse().unwrap();
        for _ in 0..MAX_ACCEPTS_PER_IP { assert!(pace.allow(a, now)); }
        assert!(!pace.allow(a, now));
        assert!(!pace.allow("::ffff:192.168.1.2".parse().unwrap(), now));
        assert!(pace.allow(b, now));
        assert!(pace.allow(a, now + DEAD));
        for n in 0..1100u32 {
            pace.allow(std::net::Ipv4Addr::from(0x0a000001 + n).into(), now + DEAD);
        }
        assert_eq!(pace.0.len(), 1024);
        assert!(pace.allow(b, now + DEAD * 2));
    }

    #[test]
    fn failed_capture_backs_off_and_recovers_for_every_neighbour() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        r.c.config.peers.push(peer(OTHER, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        r.p.capture_fail.store(3, Ordering::Relaxed);
        let calls = || r.p.capture_calls.load(Ordering::Relaxed);
        let error = |c: &Control| c.status.lock().unwrap().me.error.clone();
        // A storm of attempts inside the cooldown reaches the platform once.
        for _ in 0..100 { r.c.cross_to(SUB.into(), Side::Right, 0.5); }
        assert_eq!(calls(), 1);
        assert!(error(&r.c).unwrap().contains("tap denied"));
        assert!(matches!(r.c.state, State::Local));
        // A different neighbour obeys the same cooldown.
        r.c.cross_to(OTHER.into(), Side::Left, 0.5);
        assert_eq!(calls(), 1);
        // The cooldown doubles while it keeps failing, capped at 30 s.
        let mut expect = [4, 8, 16, 30, 30].into_iter();
        r.c.capture_block = Some(Instant::now());
        r.c.cross_to(SUB.into(), Side::Right, 0.5);
        assert_eq!(calls(), 2);
        assert_eq!(r.c.capture_backoff, Duration::from_secs(expect.next().unwrap()));
        r.c.capture_block = Some(Instant::now());
        r.c.cross_to(OTHER.into(), Side::Left, 0.5);
        assert_eq!(calls(), 3);
        assert_eq!(r.c.capture_backoff, Duration::from_secs(expect.next().unwrap()));
        assert!(error(&r.c).is_some());
        // The next attempt succeeds: the error clears and the backoff resets.
        r.c.capture_block = Some(Instant::now());
        r.c.cross_to(SUB.into(), Side::Right, 0.5);
        assert_eq!(calls(), 4);
        assert!(matches!(r.c.state, State::Controlling { .. }));
        assert_eq!(error(&r.c), None);
        assert_eq!(r.c.capture_backoff, Duration::ZERO);
        assert!(r.c.capture_block.is_none());
    }

    #[test]
    fn capture_backoff_is_capped_and_resets_when_the_cursor_leaves_the_edge() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        r.p.capture_fail.store(usize::MAX, Ordering::Relaxed);
        for _ in 0..8 {
            r.c.capture_block = Some(Instant::now());
            r.c.cross_to(SUB.into(), Side::Right, 0.5);
        }
        assert_eq!(r.c.capture_backoff, Duration::from_secs(30));
        // Cursor away from any edge after the cooldown: the next failure starts at 2 s again.
        r.c.capture_block = Some(Instant::now());
        r.c.poll_cursor();
        assert_eq!(r.c.capture_backoff, Duration::ZERO);
        r.c.cross_to(SUB.into(), Side::Right, 0.5);
        assert_eq!(r.c.capture_backoff, Duration::from_secs(2));
        assert!(r.c.status.lock().unwrap().me.error.is_some());
    }

    #[test]
    fn app_driven_local_edges_idle_until_network_or_control_work_arrives() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        r.c.config.app_drives_edges = true;
        assert_eq!(r.c.poll_interval(), IDLE_POLL);
        controlling(&mut r.c, SUB);
        assert_eq!(r.c.poll_interval(), POLL);
        r.c.state = State::Local;
        r.c.edge_wait = Some((Side::Right, Instant::now()));
        assert_eq!(r.c.poll_interval(), POLL);
    }

    #[test]
    fn coalescing_preserves_button_order_and_full_queue_motion_does_not_cancel() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link(&mut r.c, SUB, Role::Sub);
        rx.try_iter().for_each(drop);
        let link = &r.c.links[SUB];
        let motion = |dx| Msg::Input(InputEvent::Motion { dx, dy: 0.0 });
        link.send(motion(1.0));
        link.send(motion(2.0));
        link.send(Msg::Input(InputEvent::Button { button: MouseButton::Left, down: true }));
        link.send(motion(4.0));
        assert_eq!(rx.try_iter().collect::<Vec<_>>(), vec![motion(3.0),
            Msg::Input(InputEvent::Button { button: MouseButton::Left, down: true }), motion(4.0)]);
        for _ in 0..LINK_QUEUE { link.send(Msg::Input(key(4, true))); }
        link.send(motion(10.0));
        link.send(Msg::Ping { t: 1 });
        assert!(!link.cancel.load(Ordering::Acquire));
        assert_eq!(rx.try_iter().count(), LINK_QUEUE);
    }

    #[test]
    fn rejected_sub_sync_closes_the_link_and_resyncs_after_reconnect() {
        // Reject Layout, MasterSettings and Group in turn, including partial admission.
        for available in 0..3 {
            let mut r = rig(Role::Master);
            r.c.config.peers.push(peer(SUB, Role::Sub));
            let rx = link(&mut r.c, SUB, Role::Sub);
            r.c.tell_subs();
            rx.try_iter().for_each(drop);
            let before = r.c.told[SUB].clone();
            r.c.config.clipboard_relay = !r.c.config.clipboard_relay;
            for _ in 0..LINK_QUEUE - available {
                r.c.links[SUB].send(Msg::Ping { t: 1 });
            }
            r.c.tell_subs();
            assert_eq!(r.c.told.get(SUB), Some(&before));
            assert!(r.c.links[SUB].cancel.load(Ordering::Acquire));
            rx.try_iter().for_each(drop);
            r.c.tell_subs();
            assert_eq!(r.c.told.get(SUB), Some(&before));
            assert!(rx.try_recv().is_err());
            r.c.event(Event::Closed(SUB.into(), 1));
            assert!(!r.c.told.contains_key(SUB));
            let rx = link(&mut r.c, SUB, Role::Sub);
            r.c.tell_subs();
            let frames: Vec<_> = rx.try_iter().collect();
            assert!(matches!(frames.as_slice(),
                [Msg::Layout { .. }, Msg::MasterSettings(_), Msg::Group { .. }]));
            assert!(r.c.told.contains_key(SUB));
        }
    }

    #[test]
    fn prepared_files_wait_for_the_previous_peer_to_finish() {
        let mut r = rig(Role::Master);
        r.c.config.peers.extend([peer(SUB, Role::Sub), peer(OTHER, Role::Sub)]);
        let b = link(&mut r.c, SUB, Role::Sub);
        let c = link(&mut r.c, OTHER, Role::Sub);
        let path = r.dir.join("large-file");
        let bytes = vec![42; crate::transfer::CHUNK * 5];
        std::fs::write(&path, &bytes).unwrap();
        *r.p.files.lock().unwrap() = vec![path.clone()];
        r.c.send_clipboard(SUB);
        finish_file_jobs(&mut r.c);
        r.c.flush_transfer();
        assert_eq!(r.c.links[SUB].bulk.load(Ordering::Acquire), 2);
        r.c.send_clipboard(OTHER);
        finish_file_jobs(&mut r.c);
        assert_eq!(r.c.outgoing.as_ref().map(|(peer, _)| peer.as_str()), Some(SUB));
        assert!(c.try_recv().is_err());
        let mut b_done = false;
        for (peer, rx) in [(SUB, b), (OTHER, c)] {
            let mut incoming = None;
            let mut done_id = None;
            for _ in 0..20 {
                r.c.flush_transfer();
                for msg in rx.try_iter() {
                    match msg {
                        Msg::ClipStart { id, kind, files } => {
                            assert!(peer == SUB || b_done);
                            incoming = Some(crate::transfer::Incoming::start(id, kind, files,
                                bytes.len() as u64, &r.dir.join(peer)).unwrap());
                        }
                        Msg::ClipChunk { file, data, .. } => {
                            r.c.links[peer].bulk.fetch_sub(1, Ordering::AcqRel);
                            incoming.as_mut().unwrap().chunk(file, &data).unwrap();
                        }
                        Msg::ClipDone { id } => { done_id = Some(id); }
                        _ => panic!("unexpected transfer frame"),
                    }
                }
                if done_id.is_some() { break; }
            }
            let id = done_id.expect("every target must receive ClipDone");
            let received = incoming.unwrap().finish(&r.dir.join(peer).join("received")).unwrap();
            let crate::transfer::Received::Files(paths) = received else { panic!("expected files") };
            assert_eq!(std::fs::read(&paths[0]).unwrap(), bytes);
            r.c.event(Event::ClipSent(peer.into(), 1, id));
            assert_eq!(r.c.clip_sigs[peer], files_sig(std::slice::from_ref(&path)).unwrap());
            b_done = true;
        }
        assert!(r.c.outgoing.is_none());
        assert!(r.c.waiting_sends.is_empty());
    }

    #[test]
    fn queued_sends_follow_clipboard_cancellation_and_peer_disconnects() {
        use crate::transfer::{ClipKind, FileMeta, Outgoing};
        for action in 0..6 {
            let mut r = rig(Role::Master);
            r.c.config.peers.extend([peer(SUB, Role::Sub), peer(OTHER, Role::Sub)]);
            let b = link(&mut r.c, SUB, Role::Sub);
            let c = link(&mut r.c, OTHER, Role::Sub);
            for (id, peer) in [(10, SUB), (11, OTHER)] {
                r.c.begin_send(peer, "image", Outgoing::bytes(id, ClipKind::Image,
                    "image", vec![1; crate::transfer::CHUNK * 5]), vec![1]);
            }
            assert_eq!(r.c.outgoing.as_ref().unwrap().0, SUB);
            assert_eq!(r.c.waiting_sends.len(), 1);
            // A repeated request for the same peer does not grow the queue.
            r.c.begin_send(OTHER, "image", Outgoing::bytes(12, ClipKind::Image,
                "image", vec![1; crate::transfer::CHUNK * 5]), vec![1]);
            assert_eq!(r.c.waiting_sends.len(), 1);
            match action {
                0 => message(&mut r.c, SUB, Msg::Clipboard { text: "new".into() }),
                1 => r.c.clip_start(SUB, 70, ClipKind::Text,
                    vec![FileMeta { name: "text".into(), size: 2 }]),
                2 => r.c.begin_send(OTHER, "image", Outgoing::bytes(13, ClipKind::Image,
                    "new-image", vec![2]), vec![2]),
                3 => r.c.update(NetConfig { share_clipboard: false, ..r.c.config.clone() }),
                4 => r.c.disconnect(OTHER, "waiting peer lost"),
                5 => r.c.disconnect(SUB, "active peer lost"),
                _ => unreachable!(),
            }
            assert_eq!(r.c.pending_clip.contains_key(SUB), action == 4);
            assert_eq!(r.c.pending_clip.contains_key(OTHER), action == 2 || action == 5);
            assert!(r.c.waiting_sends.iter().all(|(peer, _, _)| peer != SUB));
            match action {
                0 | 1 | 3 => {
                    assert!(r.c.outgoing.is_none() && r.c.waiting_sends.is_empty());
                    r.c.flush_transfer();
                    assert!(b.try_recv().is_err() && c.try_recv().is_err());
                }
                2 => {
                    assert!(r.c.waiting_sends.is_empty());
                    assert_eq!(r.c.outgoing.as_ref().unwrap().1.id(), 13);
                }
                4 => {
                    assert!(r.c.waiting_sends.is_empty());
                    assert_eq!(r.c.outgoing.as_ref().unwrap().0, SUB);
                }
                5 => {
                    assert!(r.c.outgoing.is_none());
                    r.c.flush_transfer();
                    assert_eq!(r.c.outgoing.as_ref().unwrap().0, OTHER);
                    assert!(c.try_iter().any(|msg| matches!(msg, Msg::ClipStart { id: 11, .. })));
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn service_backs_off_only_without_live_work() {
        let mut r = rig(Role::Master);
        assert_eq!(r.c.poll_interval(), IDLE_POLL);
        r.c.config.on = false;
        assert_eq!(r.c.poll_interval(), OFF_POLL);
        r.c.config.on = true;
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        assert_eq!(r.c.poll_interval(), POLL);
        r.c.disconnect(SUB, "test");
        assert_eq!(r.c.poll_interval(), IDLE_POLL);
        let release = r.c.file_jobs.hold_worker(SUB);
        assert_eq!(r.c.poll_interval(), POLL);
        release.send(()).unwrap();
    }

    #[test]
    fn blocked_disk_worker_does_not_block_input_and_disconnect_discards_results() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        controlled(&mut r.c, SUB);
        let release = r.c.file_jobs.hold_worker(SUB);
        r.c.poll_file_jobs();
        message(&mut r.c, SUB, Msg::Input(key(4, true)));
        assert_eq!(*r.p.injected.lock().unwrap(), [key(4, true)]);
        r.c.disconnect(SUB, "test");
        release.send(()).unwrap();
        finish_file_jobs(&mut r.c);
        assert!(r.c.outgoing.is_none());
        assert!(r.c.clip_sigs.is_empty());
    }

    #[test]
    fn received_files_finish_before_clipboard_and_later_text_cancels_old_files() {
        use base64::{engine::general_purpose::STANDARD, Engine};
        use crate::transfer::{ClipKind, FileMeta};
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        let status = r.c.status.clone();
        *r.p.on_files.lock().unwrap() = Some(Box::new(move || {
            let status = status.lock().unwrap();
            let transfer = status.transfer.as_ref().unwrap();
            assert_eq!(transfer.stage, "done");
            assert_eq!(transfer.done_bytes, transfer.total_bytes);
            assert!(transfer.saved_in.is_some());
        }));
        let content = vec![31; 64 * 1024 + 7];
        r.c.clip_start(SUB, 51, ClipKind::Files, vec![FileMeta { name: "file.bin".into(), size: content.len() as u64 }]);
        for chunk in content.chunks(crate::transfer::CHUNK) { r.c.clip_chunk(SUB, 51, 0, &STANDARD.encode(chunk)); }
        r.c.clip_done(SUB, 51);
        assert!(r.p.files.lock().unwrap().is_empty());
        finish_file_jobs(&mut r.c);
        let paths = r.p.files.lock().unwrap().clone();
        assert_eq!(std::fs::read(&paths[0]).unwrap(), content);
        assert_eq!(r.c.clip_sigs[SUB], files_sig(&paths).unwrap());
        r.p.files.lock().unwrap().clear();
        r.c.clip_start(SUB, 52, ClipKind::Files, vec![FileMeta { name: "old.bin".into(), size: 0 }]);
        r.c.clip_done(SUB, 52);
        message(&mut r.c, SUB, Msg::Clipboard { text: "new clipboard".into() });
        finish_file_jobs(&mut r.c);
        assert!(r.p.files.lock().unwrap().is_empty());
        assert_eq!(r.p.text.lock().unwrap().as_deref(), Some("new clipboard"));
        assert_eq!(r.c.status.lock().unwrap().transfer.as_ref().unwrap().stage, "failed");
    }

    #[test]
    fn sharing_off_cancels_queued_receive_and_leaves_no_running_transfer() {
        use crate::transfer::{ClipKind, FileMeta};
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        r.c.clip_start(SUB, 53, ClipKind::Files, vec![FileMeta { name: "empty".into(), size: 0 }]);
        r.c.clip_done(SUB, 53);
        let mut config = r.c.config.clone();
        config.share_clipboard = false;
        r.c.update(config);
        finish_file_jobs(&mut r.c);
        assert!(r.p.files.lock().unwrap().is_empty());
        assert!(!r.c.file_jobs.busy());
        assert_eq!(r.c.status.lock().unwrap().transfer.as_ref().unwrap().stage, "failed");
        assert!(!std::fs::read_dir(r.dir.join("flockswitch")).unwrap()
            .any(|entry| entry.unwrap().file_name().to_string_lossy().starts_with("incoming-")));
    }

    #[test]
    fn unchanged_status_is_not_written_even_when_publication_is_forced() {
        let mut r = rig(Role::Master);
        r.c.publish(true);
        let path = crate::status_path(&r.dir);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        thread::sleep(Duration::from_millis(5));
        r.c.publish(true);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[test]
    fn five_thousand_motions_preserve_distance_without_cancelling() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link(&mut r.c, SUB, Role::Sub);
        rx.try_iter().for_each(drop);
        let link = &r.c.links[SUB];
        for _ in 0..5000 {
            link.send(Msg::Input(InputEvent::Motion { dx: 1.0, dy: -2.0 }));
        }
        assert!(!link.cancel.load(Ordering::Acquire));
        let (mut x, mut y) = (0.0, 0.0);
        for msg in rx.try_iter() {
            if let Msg::Input(InputEvent::Motion { dx, dy }) = msg { x += dx; y += dy; }
        }
        assert_eq!((x, y), (5000.0, -10000.0));
    }

    #[test]
    fn rejected_chunk_does_not_leak_bulk_count() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link(&mut r.c, SUB, Role::Sub);
        rx.try_iter().for_each(drop);
        for _ in 0..LINK_QUEUE - 1 { r.c.links[SUB].send(Msg::Input(key(4, true))); }
        r.c.begin_send(SUB, "image", crate::transfer::Outgoing::bytes(7,
            crate::transfer::ClipKind::Image, "image", vec![1; 20]), vec![1]);
        r.c.flush_transfer();
        assert_eq!(r.c.links[SUB].bulk.load(Ordering::Acquire), 0);
        r.c.flush_transfer();
        assert_eq!(r.c.links[SUB].bulk.load(Ordering::Acquire), 0);
        assert!(!r.c.links[SUB].cancel.load(Ordering::Acquire));
        assert!(r.c.outgoing.is_some());
        rx.try_iter().for_each(drop);
        r.c.flush_transfer();
        let frames: Vec<_> = rx.try_iter().collect();
        assert_eq!(frames.iter().filter(|m| matches!(m, Msg::ClipChunk { .. })).count(), 1);
        assert!(matches!(frames.last(), Some(Msg::ClipDone { .. })));
        assert!(r.c.outgoing.is_none());
    }

    #[test]
    fn already_synced_peer_does_not_cancel_another_peers_file_preparation() {
        let mut r = rig(Role::Master);
        r.c.config.peers.extend([peer(SUB, Role::Sub), peer(OTHER, Role::Sub)]);
        let _b = link(&mut r.c, SUB, Role::Sub);
        let _c = link(&mut r.c, OTHER, Role::Sub);
        let path = r.dir.join("file");
        std::fs::write(&path, b"clipboard").unwrap();
        *r.p.files.lock().unwrap() = vec![path.clone()];
        r.c.clip_sigs.insert(OTHER.into(), files_sig(&[path]).unwrap());
        let release = r.c.file_jobs.hold_worker("disk");
        r.c.send_clipboard(SUB);
        r.c.send_clipboard(OTHER);
        release.send(()).unwrap();
        finish_file_jobs(&mut r.c);
        assert_eq!(r.c.outgoing.as_ref().map(|(peer, _)| peer.as_str()), Some(SUB));
    }

    #[test]
    fn files_are_not_sent_to_android_but_text_is() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link(&mut r.c, SUB, Role::Sub);
        r.c.links.get_mut(SUB).unwrap().os = Os::Android;
        let path = r.dir.join("file");
        std::fs::write(&path, b"clipboard").unwrap();
        *r.p.files.lock().unwrap() = vec![path];
        *r.p.text.lock().unwrap() = Some("hello".into());
        rx.try_iter().for_each(drop);
        r.c.send_clipboard(SUB);
        finish_file_jobs(&mut r.c);
        assert!(!r.c.file_jobs.busy() && r.c.outgoing.is_none());
        let frames: Vec<_> = rx.try_iter().collect();
        assert!(!frames.iter().any(|m| matches!(m, Msg::ClipStart { .. })));
        assert!(frames.iter().any(|m| matches!(m, Msg::Clipboard { text } if text == "hello")));
    }

    #[test]
    fn android_receiver_refuses_files_but_takes_text_and_images() {
        use crate::transfer::{ClipKind, FileMeta};
        let mut r = rig(Role::Master);
        r.p.android.store(true, Ordering::Relaxed);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        r.c.clip_start(SUB, 60, ClipKind::Files, vec![FileMeta { name: "a".into(), size: 3 }]);
        assert!(r.c.incoming.is_none() && !r.c.file_jobs.busy());
        r.c.clip_chunk(SUB, 60, 0, "AAAA");
        r.c.clip_done(SUB, 60);
        assert!(!r.c.file_jobs.busy() && r.p.files.lock().unwrap().is_empty());
        r.c.clip_start(SUB, 61, ClipKind::Text, vec![FileMeta { name: "clipboard.txt".into(), size: 2 }]);
        assert!(r.c.incoming.is_some());
    }

    #[test]
    fn local_receive_switch_survives_master_settings() {
        let mut r = rig(Role::Sub);
        r.c.config.peers.push(peer(MASTER, Role::Master));
        let _rx = link(&mut r.c, MASTER, Role::Master);
        r.c.config.receive_clipboard = false;
        let settings = MasterSettings::of(&NetConfig::default(), "master");
        assert!(settings.share_clipboard);
        message(&mut r.c, MASTER, Msg::MasterSettings(settings));
        assert!(r.c.config.share_clipboard && !r.c.config.receives_clipboard());
        message(&mut r.c, MASTER, Msg::Clipboard { text: "x".into() });
        assert!(r.p.text.lock().unwrap().is_none());
        r.c.update(r.c.config.clone());
        assert!(!r.c.config.receives_clipboard());
        let config = NetConfig { receive_clipboard: true, ..r.c.config.clone() };
        r.c.update(config);
        message(&mut r.c, MASTER, Msg::Clipboard { text: "y".into() });
        assert_eq!(r.p.text.lock().unwrap().as_deref(), Some("y"));
    }

    fn queue_empty_receive(c: &mut Control) {
        c.clip_start(SUB, 9, crate::transfer::ClipKind::Files,
            vec![crate::transfer::FileMeta { name: "received".into(), size: 0 }]);
        c.clip_done(SUB, 9);
    }

    #[test]
    fn crossing_during_receive_relays_the_completed_files() {
        let mut r = rig(Role::Master);
        r.c.config.peers.extend([peer(SUB, Role::Sub), peer(OTHER, Role::Sub)]);
        let _b = link(&mut r.c, SUB, Role::Sub);
        let _c = link(&mut r.c, OTHER, Role::Sub);
        let release = r.c.file_jobs.hold_worker("disk");
        queue_empty_receive(&mut r.c);
        controlling(&mut r.c, OTHER);
        r.c.send_clipboard(OTHER);
        release.send(()).unwrap();
        finish_file_jobs(&mut r.c);
        assert_eq!(r.c.outgoing.as_ref().map(|(peer, _)| peer.as_str()), Some(OTHER));
        assert_eq!(r.c.pending_clip[OTHER].1, r.c.clip_sigs[SUB]);
    }

    #[test]
    fn older_receive_does_not_finish_a_newer_send_status() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        let release = r.c.file_jobs.hold_worker("disk");
        queue_empty_receive(&mut r.c);
        r.c.begin_send(SUB, "image", crate::transfer::Outgoing::bytes(11,
            crate::transfer::ClipKind::Image, "image", vec![1; 20]), vec![1]);
        release.send(()).unwrap();
        finish_file_jobs(&mut r.c);
        let status = r.c.status.lock().unwrap();
        let transfer = status.transfer.as_ref().unwrap();
        assert_eq!(transfer.stage, "running");
        assert_eq!(transfer.direction, "send");
        assert_eq!(transfer.done_bytes, 0);
        assert!(transfer.saved_in.is_none());
        assert_eq!(r.p.files.lock().unwrap().len(), 1);
    }

    #[test]
    fn file_signature_matches_the_bytes_actually_sent_after_a_change() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        let path = r.dir.join("file");
        std::fs::write(&path, b"before").unwrap();
        *r.p.files.lock().unwrap() = vec![path.clone()];
        r.c.send_clipboard(SUB);
        finish_file_jobs(&mut r.c);
        std::fs::write(&path, b"after!").unwrap();
        r.c.flush_transfer();
        assert!(r.c.outgoing.is_none());
        assert_eq!(r.c.pending_clip[SUB].1, files_sig(&[path]).unwrap());
    }

    #[test]
    fn full_link_queue_is_bounded_and_never_blocks_service_input() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link(&mut r.c, SUB, Role::Sub);
        rx.try_iter().for_each(drop);
        let link = &r.c.links[SUB];
        for _ in 0..LINK_QUEUE { link.send(Msg::Input(key(4, true))); }
        assert!(!link.cancel.load(Ordering::Acquire));
        link.send(Msg::Input(key(4, false)));
        assert!(link.cancel.load(Ordering::Acquire));
        assert_eq!(rx.try_iter().count(), LINK_QUEUE);
    }

    #[test]
    fn removing_roster_members_releases_cached_peer_state() {
        let mut r = rig(Role::Sub);
        r.c.config.peers.push(peer(MASTER, Role::Master));
        r.c.apply_group(MASTER, vec![], roster(&r.c, true));
        r.c.retry.insert(SUB.into(), Retry { attempt: 1, next: Instant::now() });
        r.c.peer_features.insert(SUB.into(), vec!["group".into()]);
        r.c.clip_sigs.insert(SUB.into(), vec![1]);
        r.c.apply_group(MASTER, vec![], vec![]);
        assert!(!r.c.retry.contains_key(SUB));
        assert!(!r.c.peer_features.contains_key(SUB));
        assert!(!r.c.clip_sigs.contains_key(SUB));
    }

    #[test]
    fn file_hash_streams_multiple_chunks_and_matches_the_wire_fingerprint() {
        use sha2::{Digest, Sha256};
        let r = rig(Role::Master);
        let bytes: Vec<u8> = (0..64 * 1024 * 3 + 7).map(|i| (i % 251) as u8).collect();
        let path = r.dir.join("chunks.bin");
        std::fs::write(&path, &bytes).unwrap();
        let mut hash = Sha256::new();
        hash.update(b"files");
        hash.update(1u64.to_le_bytes());
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
        assert_eq!(files_sig(std::slice::from_ref(&path)).unwrap(), hash.finalize().to_vec());
        assert!(files_sig_cancellable(&[path], &AtomicBool::new(true)).is_err());
    }

    #[test]
    fn roster_sub_cannot_impersonate_master_or_inject_settings() {
        let mut r = rig(Role::Sub);
        r.c.config.peers.push(peer(MASTER, Role::Master));
        let members = roster(&r.c, true);
        r.c.apply_group(MASTER, vec![], members);
        let _rx = link(&mut r.c, SUB, Role::Master);
        assert!(!r.c.open(SUB), "roster roles must be checked against Hello");
        // Even an already present link with a forged role cannot supply settings.
        r.c.trusted.clear();
        let _rx = link(&mut r.c, OTHER, Role::Master);
        r.c.links.get_mut(OTHER).unwrap().paired = true;
        let mut settings = MasterSettings::of(&r.c.config, "attacker");
        settings.clipboard_relay = false;
        message(&mut r.c, OTHER, Msg::MasterSettings(settings));
        assert!(r.c.master.is_none());
        assert!(r.c.config.clipboard_relay);
    }

    #[test]
    fn unpair_master_revokes_roster_links_and_transfers() {
        let mut r = rig(Role::Sub);
        r.c.config.peers.push(peer(MASTER, Role::Master));
        let members = roster(&r.c, true);
        r.c.apply_group(MASTER, vec![], members);
        let rx = link(&mut r.c, SUB, Role::Sub);
        controlled(&mut r.c, SUB);
        r.c.inject(key(0xe3, true), false);
        r.c.incoming = Some((SUB.into(), crate::transfer::Incoming::start(7, crate::transfer::ClipKind::Text,
            vec![crate::transfer::FileMeta { name: "text".into(), size: 3 }], 100, &r.dir).unwrap()));
        r.c.outgoing = Some((SUB.into(), crate::transfer::Outgoing::bytes(8, crate::transfer::ClipKind::Text, "text", vec![1; 3])));
        r.c.master = Some(MasterSettings::of(&r.c.config, "old master"));
        r.c.request(Request::Unpair { unpair: MASTER.into() });
        assert!(!r.c.links.contains_key(SUB));
        assert!(matches!(r.c.state, State::Local));
        assert!(r.c.incoming.is_none() && r.c.outgoing.is_none());
        assert!(r.c.master.is_none());
        assert!(!r.dir.join("incoming-7").exists());
        assert!(r.p.injected.lock().unwrap().contains(&key(0xe3, false)));
        assert!(rx.try_iter().any(|m| matches!(m, Msg::ReleaseAll)));
        message(&mut r.c, SUB, Msg::Clipboard { text: "forbidden".into() });
        assert!(r.p.text.lock().unwrap().is_none());
    }

    #[test]
    fn live_can_send_revocation_ends_sender_and_receiver_sessions() {
        // Master receiver: changing saved permission releases keys already held.
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link(&mut r.c, SUB, Role::Sub);
        controlled(&mut r.c, SUB);
        r.c.inject(key(0xe0, true), false);
        let mut config = r.c.config.clone();
        config.peers[0].can_send = false;
        r.c.update(config);
        assert!(matches!(r.c.state, State::Local));
        assert!(r.p.injected.lock().unwrap().contains(&key(0xe0, false)));
        assert!(rx.try_iter().any(|m| matches!(m, Msg::ReleaseAll)));
        // Sub sender: the new roster disables the active capture.
        let mut r = rig(Role::Sub);
        r.c.config.peers.push(peer(MASTER, Role::Master));
        let members = roster(&r.c, true);
        r.c.apply_group(MASTER, vec![], members);
        let rx = link(&mut r.c, SUB, Role::Sub);
        controlling(&mut r.c, SUB);
        let members = roster(&r.c, false);
        r.c.apply_group(MASTER, vec![], members);
        assert!(matches!(r.c.state, State::Local));
        assert!(r.p.stopped.load(Ordering::Relaxed) > 0);
        assert!(rx.try_iter().any(|m| matches!(m, Msg::ReleaseAll)));
        // Sub receiver: the same update revokes the remote sender.
        let members = roster(&r.c, true);
        r.c.apply_group(MASTER, vec![], members);
        controlled(&mut r.c, SUB);
        r.c.inject(key(0xe1, true), false);
        let members = roster(&r.c, false);
        r.c.apply_group(MASTER, vec![], members);
        assert!(matches!(r.c.state, State::Local));
        assert!(r.p.injected.lock().unwrap().contains(&key(0xe1, false)));
    }

    #[test]
    fn mac_receiver_keeps_lang1_from_2_1_senders_and_converts_it_for_2_0() {
        let mut r = rig(Role::Master);
        r.p.mac.store(true, Ordering::Relaxed);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        controlled(&mut r.c, SUB);
        message(&mut r.c, SUB, Msg::Input(key(keymap::LANG1, true)));
        assert_eq!(r.p.injected.lock().unwrap().last(), Some(&key(keymap::LANG1, true)), "2.1 senders map once, on their side");
        // With the old switch off a 2.1 sender's Lang1 is still not dropped.
        r.c.config.lang1_on_mac = "off".into();
        message(&mut r.c, SUB, Msg::Input(key(keymap::LANG1, false)));
        assert_eq!(r.p.injected.lock().unwrap().last(), Some(&key(keymap::LANG1, false)));
        // A 2.0 sender knows nothing of rules: the old receiver-side switch applies.
        r.c.links.get_mut(SUB).unwrap().features.clear();
        message(&mut r.c, SUB, Msg::Input(key(keymap::LANG1, true)));
        assert_eq!(r.p.injected.lock().unwrap().last(), Some(&key(keymap::LANG1, false)));
        r.c.config.lang1_on_mac = "capslock".into();
        message(&mut r.c, SUB, Msg::Input(key(keymap::LANG1, true)));
        assert_eq!(r.p.injected.lock().unwrap().last(), Some(&key(0x39, true)));
    }

    #[test]
    fn input_paths_recheck_send_permission() {
        let mut r = rig(Role::Master);
        let mut p = peer(SUB, Role::Sub);
        p.can_send = false;
        r.c.config.peers.push(p);
        let rx = link(&mut r.c, SUB, Role::Sub);
        controlled(&mut r.c, SUB);
        message(&mut r.c, SUB, Msg::Input(key(4, true)));
        assert!(r.p.injected.lock().unwrap().is_empty());
        assert!(matches!(r.c.state, State::Local));
        assert!(rx.try_iter().any(|m| matches!(m, Msg::ReleaseAll)));
        r.c.config.role = Role::Sub;
        r.c.group = Some((vec![], roster(&r.c, false)));
        controlling(&mut r.c, SUB);
        r.c.capture_event(key(4, true));
        assert!(matches!(r.c.state, State::Local));
        assert!(!rx.try_iter().any(|m| matches!(m, Msg::Input(_))));
    }

    #[test]
    fn master_settings_release_old_mapping_before_changing_rules() {
        let mut r = rig(Role::Sub);
        r.c.config.peers.push(peer(MASTER, Role::Master));
        let master_rx = link(&mut r.c, MASTER, Role::Master);
        r.c.trusted.push(peer(SUB, Role::Sub));
        let rx = link(&mut r.c, SUB, Role::Sub);
        controlling(&mut r.c, SUB);
        r.c.capture_event(key(0xe0, true));
        assert!(rx.try_iter().any(|m| matches!(m, Msg::Input(InputEvent::Key { usage: 0xe3, down: true }))));
        let mut settings = MasterSettings::of(&r.c.config, "master");
        settings.keys.get_mut("windows>mac").unwrap().mod_mode = keymap::ModMode::Off;
        message(&mut r.c, MASTER, Msg::MasterSettings(settings));
        assert!(matches!(r.c.state, State::Local));
        assert!(rx.try_iter().any(|m| matches!(m, Msg::ReleaseAll)));
        r.c.capture_event(key(0xe0, false));
        assert!(!rx.try_iter().any(|m| matches!(m, Msg::Input(_))));
        assert!(master_rx.try_iter().next().is_none());
        // Explicit 2.1 rule changes also end the session, same settings do not.
        controlling(&mut r.c, SUB);
        let same = MasterSettings::of(&r.c.config, "master");
        message(&mut r.c, MASTER, Msg::MasterSettings(same.clone()));
        assert!(matches!(r.c.state, State::Controlling { .. }));
        let mut changed = same;
        changed.keys.get_mut("windows>mac").unwrap().hangul_src.clear();
        message(&mut r.c, MASTER, Msg::MasterSettings(changed));
        assert!(matches!(r.c.state, State::Local));
    }

    #[test]
    fn relay_off_preserves_sub_origin_after_unpair() {
        let mut r = rig(Role::Master);
        r.c.config.clipboard_relay = false;
        r.c.config.peers = vec![peer(SUB, Role::Sub), peer(OTHER, Role::Sub)];
        let _a = link(&mut r.c, SUB, Role::Sub);
        let b = link(&mut r.c, OTHER, Role::Sub);
        message(&mut r.c, SUB, Msg::Clipboard { text: "private".into() });
        r.c.request(Request::Unpair { unpair: SUB.into() });
        r.c.send_clipboard(OTHER);
        assert!(b.try_iter().next().is_none());
        *r.p.text.lock().unwrap() = Some("local edit".into());
        r.c.send_clipboard(OTHER);
        assert!(b.try_iter().any(|m| matches!(m, Msg::Clipboard { text } if text == "local edit")));
    }

    #[test]
    fn key_rules_are_echoed_after_persistence_and_duplicate_requests_get_an_echo() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link(&mut r.c, SUB, Role::Sub);
        r.c.tell_subs();
        rx.try_iter().for_each(drop);
        let mut keys = r.c.config.all_rules();
        keys.get_mut("mac>windows").unwrap().hanja = true;
        r.c.hooks.persist_key_rules = Box::new(|_| anyhow::bail!("disk full"));
        message(&mut r.c, SUB, Msg::KeyRules(keys.clone()));
        r.c.tell_subs();
        assert_ne!(r.c.config.keys, keys);
        assert!(rx.try_iter().filter_map(|msg| if let Msg::MasterSettings(s) = msg { Some(s) } else { None }).all(|s| s.keys != keys));
        r.c.hooks.persist_key_rules = Box::new(|_| Ok(()));
        for _ in 0..2 {
            message(&mut r.c, SUB, Msg::KeyRules(keys.clone()));
            r.c.tell_subs();
            assert!(rx.try_iter().any(|msg| matches!(msg, Msg::MasterSettings(s) if s.keys == keys)));
        }
    }

    #[test]
    fn received_file_signature_prevents_the_next_round_of_a_relay_cycle() {
        let mut a = rig(Role::Master);
        let b = rig(Role::Sub);
        let c = rig(Role::Sub);
        a.c.config.peers.push(peer(SUB, Role::Sub));
        let original = a.dir.join("source.txt");
        let forwarded = b.dir.join("source (1).txt");
        let returned = c.dir.join("source (2).txt");
        for file in [&original, &forwarded, &returned] { std::fs::write(file, b"cycle content").unwrap(); }
        let sent = files_sig(&[original]).unwrap();
        a.c.clip_sigs.insert(SUB.into(), sent.clone());
        assert_eq!(files_sig(&[forwarded]).unwrap(), sent);
        a.c.clip_received(OTHER, files_sig(&[returned]).unwrap());
        assert!(!a.c.clip_wanted(SUB, &sent));
    }

    #[test]
    fn file_signature_covers_only_the_files_selected_for_transfer() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        let file = r.dir.join("valid.txt");
        std::fs::write(&file, b"valid").unwrap();
        *r.p.files.lock().unwrap() = vec![r.dir.clone(), file.clone(), r.dir.join("missing")];
        r.c.send_clipboard(SUB);
        assert!(r.c.outgoing.is_none());
        finish_file_jobs(&mut r.c);
        assert!(r.c.outgoing.is_some());
        assert_eq!(r.c.pending_clip[SUB].1, files_sig(&[file]).unwrap());
    }

    #[test]
    fn file_signature_survives_receive_rename_and_checks_content_and_order() {
        let r = rig(Role::Master);
        let a = r.dir.join("original.txt");
        let b = r.dir.join("original (1).txt");
        let c = r.dir.join("other.txt");
        std::fs::write(&a, b"same bytes").unwrap();
        std::fs::write(&b, b"same bytes").unwrap();
        std::fs::write(&c, b"different!").unwrap();
        assert_eq!(files_sig(&[a.clone()]).unwrap(), files_sig(&[b.clone()]).unwrap());
        assert_ne!(files_sig(&[a.clone(), c.clone()]).unwrap(), files_sig(&[c.clone(), a.clone()]).unwrap());
        std::fs::write(&b, b"new bytes!").unwrap();
        assert_ne!(files_sig(&[a]).unwrap(), files_sig(&[b]).unwrap());
    }

    #[test]
    fn inbound_connection_keeps_live_ip_and_persists_combined_address() {
        let mut r = rig(Role::Master);
        let mut p = peer(SUB, Role::Sub);
        p.last_addr = "10.0.0.1:49000".into();
        r.c.config.peers.push(p);
        let _rx = link(&mut r.c, SUB, Role::Sub);
        let mut l = r.c.links.remove(SUB).unwrap();
        l.outgoing = false;
        l.addr = "10.0.0.2:47160".parse().unwrap();
        r.c.config.peers[0].last_addr = "10.0.0.1:49000".into();
        r.c.event(Event::Connected(SUB.into(), l, false));
        assert_eq!(r.c.links[SUB].addr.to_string(), "10.0.0.2:49000");
        assert_eq!(r.c.config.peers[0].last_addr, "10.0.0.2:49000");
        assert_eq!(r.c.roster().iter().find(|m| m.id == SUB).unwrap().addr, "10.0.0.2:49000");
    }

    #[test]
    fn pairing_uses_discovery_only_for_the_port_and_keeps_the_inbound_ip() {
        for advertised in [None, Some(48000)] {
            let mut r = rig(Role::Master);
            let _rx = link(&mut r.c, SUB, Role::Sub);
            let l = r.c.links.get_mut(SUB).unwrap();
            l.outgoing = false;
            l.listen_port = advertised;
            l.addr = format!("10.0.0.2:{}", advertised.unwrap_or(47160)).parse().unwrap();
            r.c.discovered.push(DiscoveredPeer { id: SUB.into(), name: "sub".into(), os: Os::Mac,
                addresses: vec!["10.0.0.1:49000".parse().unwrap()], port: 49000,
                fingerprint: SUB.repeat(4), seen: Instant::now() });
            let mut pairing = Pairing::new(SUB.into(), "master", "sub");
            pairing.local_accepted = true;
            pairing.peer_accepted = true;
            r.c.pairing = Some(pairing);
            r.c.finish_pair();
            assert_eq!(r.c.config.peers[0].last_addr, format!("10.0.0.2:{}", advertised.unwrap_or(49000)));
        }
    }

    #[test]
    fn inbound_advertised_port_wins_over_saved_port() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        let mut l = r.c.links.remove(SUB).unwrap();
        l.outgoing = false;
        l.listen_port = Some(48000);
        l.addr = "10.0.0.2:48000".parse().unwrap();
        r.c.config.peers[0].last_addr = "10.0.0.1:49000".into();
        r.c.event(Event::Connected(SUB.into(), l, false));
        assert_eq!(r.c.config.peers[0].last_addr, "10.0.0.2:48000");
    }

    #[test]
    fn reconnect_backoff_only_resets_after_matching_pong() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        for attempt in 1..=5 {
            let _rx = link(&mut r.c, SUB, Role::Sub);
            r.c.disconnect(SUB, "immediate close");
            assert_eq!(r.c.retry[SUB].attempt, attempt);
        }
        let _rx = link(&mut r.c, SUB, Role::Sub);
        r.c.links.get_mut(SUB).unwrap().ping = Some((42, Instant::now()));
        message(&mut r.c, SUB, Msg::Pong { t: 41 });
        assert_eq!(r.c.retry[SUB].attempt, 5);
        message(&mut r.c, SUB, Msg::Pong { t: 42 });
        assert!(!r.c.retry.contains_key(SUB));
        r.c.disconnect(SUB, "healthy close");
        assert_eq!(r.c.retry[SUB].attempt, 1);
    }

    #[test]
    fn failed_clipboard_creation_and_disconnect_allow_retry() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        let missing = r.dir.join("missing.txt");
        *r.p.files.lock().unwrap() = vec![missing.clone()];
        r.c.send_clipboard(SUB);
        finish_file_jobs(&mut r.c);
        assert!(r.c.outgoing.is_none());
        assert!(files_sig(std::slice::from_ref(&missing)).is_err());
        // A transfer that was queued but lost with its link must be retried too.
        r.p.files.lock().unwrap().clear();
        let text = "x".repeat(crate::transfer::INLINE_TEXT + 1);
        *r.p.text.lock().unwrap() = Some(text.clone());
        r.c.send_clipboard(SUB);
        assert!(r.c.outgoing.is_some());
        r.c.disconnect(SUB, "lost");
        assert!(r.c.outgoing.is_none());
        assert!(r.c.clip_wanted(SUB, &clip_sig(b"text", &[text.as_bytes()])));
    }

    #[test]
    fn roster_lists_only_group_capable_members_with_the_live_address() {
        let mut r = rig(Role::Master);
        let mut a = peer(SUB, Role::Sub);
        a.last_addr = "10.9.9.9:47161".into();
        let mut b = peer(OTHER, Role::Sub);
        b.last_addr = "10.9.9.8:47162".into();
        r.c.config.peers = vec![a, b];
        let _a = link(&mut r.c, SUB, Role::Sub);
        let _b = link(&mut r.c, OTHER, Role::Sub);
        // A 2.0 sub says nothing in its Hello: it must not be vouched for.
        let mut l = r.c.links.remove(OTHER).unwrap();
        l.features.clear();
        l.token = 2;
        r.c.event(Event::Connected(OTHER.into(), l, false));
        r.c.links.get_mut(SUB).unwrap().outgoing = false;
        r.c.links.get_mut(SUB).unwrap().addr.set_port(47161);
        let roster = r.c.roster();
        assert!(roster.iter().any(|m| m.id == SUB) && !roster.iter().any(|m| m.id == OTHER));
        assert!(roster[0].features.iter().any(|f| f == "group"));
        // Inbound link: its ip is real, its port is the fixed default, so the stored port stays.
        let ours = roster.iter().find(|m| m.id == SUB).unwrap();
        assert_eq!(ours.addr, "127.0.0.1:47161");
        // An outgoing link knows the real address; no stored address is needed either.
        r.c.links.get_mut(SUB).unwrap().outgoing = true;
        r.c.links.get_mut(SUB).unwrap().addr = "192.168.1.20:47199".parse().unwrap();
        assert_eq!(r.c.roster().iter().find(|m| m.id == SUB).unwrap().addr, "192.168.1.20:47199");
        r.c.config.peers[0].last_addr.clear();
        r.c.links.get_mut(SUB).unwrap().outgoing = false;
        assert_eq!(r.c.roster().iter().find(|m| m.id == SUB).unwrap().addr, "192.168.1.20:47199");
        // Without a link the stored address is all there is, and it survives a blip.
        r.c.config.peers[0].last_addr = "10.9.9.9:47161".into();
        r.c.disconnect(SUB, "blip");
        let roster = r.c.roster();
        assert_eq!(roster.iter().find(|m| m.id == SUB).map(|m| m.addr.as_str()), Some("10.9.9.9:47161"));
    }

    #[test]
    fn clipboard_is_sent_again_after_the_peer_reconnects() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link(&mut r.c, SUB, Role::Sub);
        *r.p.text.lock().unwrap() = Some("same".into());
        r.c.send_clipboard(SUB);
        r.c.send_clipboard(SUB);
        assert_eq!(rx.try_iter().filter(|m| matches!(m, Msg::Clipboard { .. })).count(), 1);
        // The peer restarted and lost it: an unchanged clipboard must go out again.
        r.c.disconnect(SUB, "lost");
        assert!(r.c.clip_sigs.is_empty());
        let rx = link(&mut r.c, SUB, Role::Sub);
        r.c.send_clipboard(SUB);
        assert_eq!(rx.try_iter().filter(|m| matches!(m, Msg::Clipboard { .. })).count(), 1);
    }

    #[test]
    fn wake_closes_links_that_were_silent_across_the_sleep() {
        // Instant stops during sleep, so the jump size must count towards the silence.
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        r.c.links.get_mut(SUB).unwrap().paired = true;
        r.c.reconnect(Some((WAKE_STALE, Duration::from_millis(5))));
        assert!(r.c.links.contains_key(SUB), "a link heard from just now stays");
        r.c.reconnect(Some((WAKE_STALE, Duration::from_secs(60))));
        assert!(!r.c.links.contains_key(SUB));
    }

    #[test]
    fn stalled_incoming_transfer_is_dropped_with_its_files() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        r.c.links.get_mut(SUB).unwrap().paired = true;
        let files = vec![crate::transfer::FileMeta { name: "a.bin".into(), size: 10 }];
        message(&mut r.c, SUB, Msg::ClipStart { id: 5, kind: crate::transfer::ClipKind::Files, files });
        assert!(r.c.incoming.is_some());
        r.c.drop_stalled_incoming(Instant::now() + INCOMING_STALL / 2);
        assert!(r.c.incoming.is_some());
        r.c.drop_stalled_incoming(Instant::now() + INCOMING_STALL + Duration::from_secs(1));
        assert!(r.c.incoming.is_none());
        assert!(!r.dir.join("flockswitch").join("incoming-5").exists());
        assert_eq!(r.c.status.lock().unwrap().transfer.as_ref().map(|t| t.stage.as_str()), Some("failed"));
    }

    #[test]
    fn reconnect_cycles_all_discovered_and_saved_addresses() {
        let mut r = rig(Role::Master);
        let mut p = peer(SUB, Role::Sub);
        p.last_addr = "127.0.0.1:47163".into();
        let a: SocketAddr = "127.0.0.1:47161".parse().unwrap();
        let b: SocketAddr = "[::1]:47162".parse().unwrap();
        let saved = p.last_addr.parse().unwrap();
        r.c.discovered.push(DiscoveredPeer { id: SUB.into(), name: SUB.into(), os: Os::Mac,
            fingerprint: p.fingerprint.clone(), addresses: vec![a, a, b], port: 47161, seen: Instant::now() });
        for expected in [a, b, saved, a, b, saved] {
            assert_eq!(r.c.dial_address(&p), Some(expected));
            r.c.schedule_retry(SUB);
        }
        r.c.discovered[0].fingerprint = "bad".into();
        assert_eq!(r.c.dial_address(&p), Some(saved));
        r.c.retry.clear();
        r.c.discovered[0].fingerprint = p.fingerprint.clone();
        assert_eq!(r.c.dial_address(&p), Some(a));
    }

    #[test]
    fn closing_link_flushes_release_all_and_bye_past_a_full_socket() {
        struct Slow { writes: usize, out: Vec<u8> }
        impl std::io::Write for Slow {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.writes += 1;
                if self.writes % 3 != 0 { return Err(std::io::ErrorKind::WouldBlock.into()); }
                self.out.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let identity = crate::identity::Identity::load_or_create(&std::env::temp_dir().join(format!("flock-goodbye-{}", std::process::id()))).unwrap();
        let mut conn = rustls::Connection::Server(rustls::ServerConnection::new(crate::tls::server_config(&identity).unwrap()).unwrap());
        let (tx, rx) = queue::channel();
        tx.try_send(Msg::Input(InputEvent::Motion { dx: 1.0, dy: 1.0 })).unwrap();
        tx.try_send(Msg::ReleaseAll).unwrap();
        tx.try_send(Msg::Bye { reason: "test".into() }).unwrap();
        let mut queued = None;
        let mut socket = Slow { writes: 0, out: vec![] };
        flush_goodbye(&mut conn, &mut socket, &rx, &mut queued).unwrap();
        assert!(rx.try_recv().is_err() && queued.is_none());
    }

    #[test]
    fn canonical_drops_the_ipv4_mapped_form() {
        let mapped: SocketAddr = "[::ffff:192.168.1.7]:47160".parse().unwrap();
        assert_eq!(canonical(mapped), "192.168.1.7:47160".parse().unwrap());
        let v6: SocketAddr = "[fe80::1]:1".parse().unwrap();
        assert_eq!(canonical(v6), v6);
    }

    #[test]
    fn disconnect_forgets_what_the_sub_was_told() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        r.c.told.insert(SUB.into(), (None, MasterSettings::of(&r.c.config, "m"), (vec![], vec![])));
        r.c.disconnect(SUB, "test");
        assert!(!r.c.told.contains_key(SUB));
    }

    #[test]
    fn unpair_removes_the_orphan_layout_cell() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let me = r.c.identity.id.clone();
        r.c.config.layout = vec![Cell { id: me.clone(), col: 2, row: 1 }, Cell { id: SUB.into(), col: 3, row: 1 }];
        r.c.request(Request::Unpair { unpair: SUB.into() });
        assert!(r.c.config.layout.iter().all(|c| c.id != SUB));
        // Even if the app still hands over the old grid, the orphan is not shown.
        r.c.config.layout.push(Cell { id: SUB.into(), col: 3, row: 1 });
        assert!(r.c.my_cells().iter().all(|c| c.id != SUB));
    }

    #[test]
    fn a_subs_key_rules_edit_merges_into_the_masters_rules() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        let mut all = r.c.config.all_rules();
        all.get_mut("linux>mac").unwrap().hanja = true;
        r.c.key_rules_edited(all);
        // The sub never heard of that change and edits another pair only.
        let mut edit = std::collections::BTreeMap::new();
        let mut rule = r.c.config.all_rules()["mac>windows"].clone();
        rule.hanja = true;
        edit.insert("mac>windows".to_string(), rule);
        message(&mut r.c, SUB, Msg::KeyRules(edit));
        let keys = r.c.config.all_rules();
        assert!(keys["linux>mac"].hanja && keys["mac>windows"].hanja);
    }

    #[test]
    fn a_new_sub_without_master_settings_sends_one_pair_and_resets_no_other() {
        let mut r = rig(Role::Sub);
        r.c.config.peers.push(peer(MASTER, Role::Master));
        let rx = link(&mut r.c, MASTER, Role::Master);
        assert!(r.c.master.is_none());
        let mut edit = std::collections::BTreeMap::new();
        edit.insert("mac>windows".to_string(), r.c.config.all_rules()["mac>windows"].clone());
        r.c.key_rules_edited(edit.clone());
        let sent: Vec<_> = rx.try_iter().filter_map(|m| if let Msg::KeyRules(k) = m { Some(k) } else { None }).collect();
        assert_eq!(sent, vec![edit]);
        // And the master keeps what it had for every other pair.
        let mut m = rig(Role::Master);
        m.c.config.peers.push(peer(SUB, Role::Sub));
        let _l = link(&mut m.c, SUB, Role::Sub);
        let mut all = m.c.config.all_rules();
        all.get_mut("linux>mac").unwrap().hanja = true;
        m.c.key_rules_edited(all);
        message(&mut m.c, SUB, Msg::KeyRules(sent[0].clone()));
        assert!(m.c.config.all_rules()["linux>mac"].hanja);
    }

    #[test]
    fn stale_sends_survive_when_nothing_is_received() {
        use crate::transfer::{ClipKind, FileMeta, Outgoing};
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        r.c.begin_send(SUB, "image", Outgoing::bytes(7, ClipKind::Image, "image", vec![1; 20]), vec![1]);
        r.c.config.receive_clipboard = false;
        r.c.clip_start(SUB, 70, ClipKind::Text, vec![FileMeta { name: "clipboard.txt".into(), size: 2 }]);
        assert!(r.c.outgoing.is_some());
        r.c.config.receive_clipboard = true;
        r.p.android.store(true, Ordering::Relaxed);
        r.c.clip_start(SUB, 71, ClipKind::Files, vec![FileMeta { name: "a".into(), size: 3 }]);
        assert!(r.c.outgoing.is_some());
        r.p.android.store(false, Ordering::Relaxed);
        r.c.clip_start(SUB, 72, ClipKind::Text, vec![FileMeta { name: "clipboard.txt".into(), size: 2 }]);
        assert!(r.c.outgoing.is_none());
    }

    #[test]
    fn unpaired_message_goodbye_needs_two_sightings_ten_seconds_apart() {
        let mut r = rig(Role::Sub);
        r.c.config.peers.push(peer(MASTER, Role::Master));
        let _rx = link(&mut r.c, MASTER, Role::Master);
        message(&mut r.c, MASTER, Msg::Bye { reason: "unpaired message".into() });
        assert_eq!(r.c.config.peers.len(), 1);
        // A working session in between resets the count.
        let _rx = link(&mut r.c, MASTER, Role::Master);
        message(&mut r.c, MASTER, Msg::Pong { t: 1 });
        assert!(r.c.unpaired_seen.is_empty());
        message(&mut r.c, MASTER, Msg::Bye { reason: "unpaired message".into() });
        let _rx = link(&mut r.c, MASTER, Role::Master);
        message(&mut r.c, MASTER, Msg::Bye { reason: "unpaired message".into() });
        assert_eq!(r.c.config.peers.len(), 1);
        *r.c.unpaired_seen.get_mut(MASTER).unwrap() -= Duration::from_secs(11);
        let _rx = link(&mut r.c, MASTER, Role::Master);
        message(&mut r.c, MASTER, Msg::Bye { reason: "unpaired message".into() });
        assert!(r.c.config.peers.is_empty());
    }

    #[test]
    fn config_snapshot_from_the_app_does_not_drop_a_freshly_paired_peer() {
        let mut r = rig(Role::Master);
        let stale = r.c.config.clone();
        r.c.config.peers.push(peer(SUB, Role::Sub));
        r.c.recent_paired.insert(SUB.into(), (peer(SUB, Role::Sub), Instant::now()));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        r.c.links.get_mut(SUB).unwrap().paired = true;
        r.c.update(stale);
        assert!(r.c.config.peers.iter().any(|p| p.id == SUB));
        assert!(r.c.links.contains_key(SUB));
    }

    #[test]
    fn deferred_clipboard_goes_only_to_the_peer_still_in_control() {
        let mut r = rig(Role::Master);
        r.c.config.peers.extend([peer(SUB, Role::Sub), peer(OTHER, Role::Sub)]);
        let rx = link(&mut r.c, SUB, Role::Sub);
        let _o = link(&mut r.c, OTHER, Role::Sub);
        *r.p.text.lock().unwrap() = Some("hi".into());
        rx.try_iter().for_each(drop);
        // Switched to another peer meanwhile: nothing goes to SUB.
        controlling(&mut r.c, OTHER);
        r.c.deferred_clipboard.insert(SUB.into());
        r.c.poll_file_jobs();
        assert!(!rx.try_iter().any(|m| matches!(m, Msg::Clipboard { .. })));
        // Back to local or a lost link forgets what waited.
        controlling(&mut r.c, SUB);
        r.c.deferred_clipboard.insert(SUB.into());
        r.c.local(false);
        assert!(r.c.deferred_clipboard.is_empty());
        controlling(&mut r.c, SUB);
        r.c.deferred_clipboard.insert(SUB.into());
        r.c.disconnect(SUB, "test");
        assert!(r.c.deferred_clipboard.is_empty());
        // Still controlling the same peer: it is sent.
        let rx = link(&mut r.c, SUB, Role::Sub);
        rx.try_iter().for_each(drop);
        controlling(&mut r.c, SUB);
        r.c.deferred_clipboard.insert(SUB.into());
        r.c.poll_file_jobs();
        assert!(rx.try_iter().any(|m| matches!(m, Msg::Clipboard { text } if text == "hi")));
    }

    #[test]
    fn android_gets_text_not_the_icon_image_that_comes_with_files() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link(&mut r.c, SUB, Role::Sub);
        r.c.links.get_mut(SUB).unwrap().os = Os::Android;
        let path = r.dir.join("file");
        std::fs::write(&path, b"x").unwrap();
        *r.p.files.lock().unwrap() = vec![path];
        *r.p.image.lock().unwrap() = Some(vec![1, 2, 3]);
        rx.try_iter().for_each(drop);
        r.c.send_clipboard(SUB);
        assert!(r.c.outgoing.is_none());
        assert!(rx.try_iter().next().is_none());
        *r.p.text.lock().unwrap() = Some("t".into());
        r.c.send_clipboard(SUB);
        assert!(rx.try_iter().any(|m| matches!(m, Msg::Clipboard { text } if text == "t")));
    }

    #[test]
    fn accept_thread_stops_by_itself_without_the_poke() {
        let (events, _er) = mpsc::channel();
        let (wake, _wr) = mpsc::channel();
        let mut acceptor = Acceptor::start(bind(0).unwrap(), events, wake).unwrap();
        acceptor.stop.store(true, Ordering::Release);
        let handle = acceptor.handle.take().unwrap();
        let begun = Instant::now();
        handle.join().unwrap();
        assert!(begun.elapsed() < Duration::from_millis(2600));
    }

    #[test]
    fn peer_that_forgot_us_makes_us_forget_it_too() {
        let mut r = rig(Role::Sub);
        r.c.config.peers.push(peer(MASTER, Role::Master));
        let _rx = link(&mut r.c, MASTER, Role::Master);
        assert!(r.c.links[MASTER].paired);
        message(&mut r.c, MASTER, Msg::Bye { reason: "unpaired".into() });
        assert!(r.c.config.peers.is_empty());
        assert!(!r.c.links.contains_key(MASTER));
    }

    #[test]
    fn pair_request_on_a_losing_duplicate_link_still_reaches_the_kept_one() {
        let mut r = rig(Role::Master);
        // Our id is larger than SUB's, so the inbound link is the one kept.
        r.c.identity.id = "ffffffffffffffff".into();
        let rx = link_with(&mut r.c, SUB, Role::Sub, false);
        let (tx, _rx2) = queue::channel();
        r.c.event(Event::Connected(SUB.into(), Link {
            token: 2, tx, cancel: Arc::new(AtomicBool::new(false)), name: SUB.into(), os: Os::Mac,
            role: Role::Sub, features: proto::FEATURES.iter().map(|s| s.to_string()).collect(), fp: SUB.repeat(4),
            addr: "127.0.0.1:47160".parse().unwrap(), listen_port: None, outgoing: true, paired: false,
            last_rx: Instant::now(), last_ping: Instant::now(), ping: None, rtt: None,
            bulk: Arc::new(AtomicUsize::new(0)),
        }, true));
        assert!(rx.try_iter().any(|m| matches!(m, Msg::PairRequest)));
        assert!(r.c.pairing.is_some());
    }
    fn link_with(c: &mut Control, id: &str, role: Role, outgoing: bool) -> queue::Receiver {
        let (tx, rx) = queue::channel();
        c.event(Event::Connected(id.into(), Link {
            token: 1, tx, cancel: Arc::new(AtomicBool::new(false)), name: id.into(), os: Os::Mac,
            role, features: proto::FEATURES.iter().map(|s| s.to_string()).collect(), fp: id.repeat(4),
            addr: "127.0.0.1:47160".parse().unwrap(), listen_port: None, outgoing, paired: false,
            last_rx: Instant::now(), last_ping: Instant::now(), ping: None, rtt: None,
            bulk: Arc::new(AtomicUsize::new(0)),
        }, false));
        rx
    }

    #[test]
    fn incoming_clipboard_cancels_a_send_of_the_old_content() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        r.c.outgoing = Some((SUB.into(), crate::transfer::Outgoing::bytes(8, crate::transfer::ClipKind::Text, "text", vec![1; 3])));
        message(&mut r.c, SUB, Msg::Clipboard { text: "new".into() });
        assert!(r.c.outgoing.is_none());
    }
}
