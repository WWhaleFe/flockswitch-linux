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
mod keycheck;

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
/// How often the monitor list is looked at.
/// Most monitors taken from a peer's list.
const MAX_DISPLAYS: usize = 16;
const DISPLAYS_POLL: Duration = Duration::from_secs(2);
/// A computer that stayed unlinked this long is also dialled by the side with the larger
/// id (normally only the smaller one dials), in case the smaller one cannot reach it.
/// A key check that sees no key this long lets the keyboard go.
const PROBE_IDLE: Duration = Duration::from_secs(30);
const FALLBACK_DIAL: Duration = Duration::from_secs(6);
/// How long a peer that closed the link with Bye "paused" is left alone (unless it dials us
/// or shows up with other addresses).
const PEER_PAUSE_QUIET: Duration = Duration::from_secs(60);
/// Seconds to wait before dial attempt number `attempt` (0 is the first retry). While the
/// peer advertises itself on the network it is 1, 2, then 4 at most: it is there, so the
/// next try must not be far off. A peer only known by its saved address backs off to 10.
pub(crate) fn backoff_secs(attempt: usize, discovered: bool) -> u64 {
    if discovered {
        [1, 2, 4][attempt.min(2)]
    } else {
        [1, 2, 4, 8, 10][attempt.min(4)]
    }
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
                displays: vec![],
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
            key_check: None,
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
            key_check: None,
            probe: None,
            discovery: None,
            discovered: vec![],
            edge_wait: None,
            last_cursor: None,
            armed: Vec::new(),
            edge_hit: None,
            capture_block: None,
            capture_backoff: Duration::ZERO,
            outward: false,
            motion: (0.0, 0.0),
            pass_side: None,
            started: Instant::now(),
            last_write: Instant::now() - Duration::from_secs(1),
            written: None,
            written_permissions: None,
            readiness_error: None,
            my_displays: vec![],
            displays_checked: None,
            displays_rev: 0,
            displays_told: HashMap::new(),
            peer_displays: HashMap::new(),
            down_since: HashMap::new(),
            seen_addrs: HashMap::new(),
            peer_paused_until: HashMap::new(),
            retired: HashMap::new(),
            pair_dial: None,
            bind_failed: false,
            last_bind_try: None,
        };
        control.refresh_displays();
        control.seed_peer_displays();
        control.setup_discovery();
        control.dial_all_now();
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
    /// The accept thread still runs.
    fn alive(&self) -> bool { self.handle.as_ref().is_some_and(|h| !h.is_finished()) }
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
    up_since: Instant,
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
    /// `pos` is where this computer was left along its `side` edge: on monitor `display`
    /// when that is known, else along the whole exposed edge.
    Controlling { peer: String, side: Side, pos: f64, display: Option<u8> },
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
/// Where the cursor leaves this computer: its monitor (`display`), the monitor it enters on
/// the other computer (`target`, None when that has no per-monitor cell to name), the position along this monitor's edge (`pos`) and along
/// the whole exposed edge (`whole`, what a pre-2.2 peer understands).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Exit {
    display: u8,
    target: Option<u8>,
    pos: f64,
    whole: f64,
}
impl Exit {
    fn at(self, side: Side) -> (Self, Side) {
        (self, side)
    }
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
    /// On a master: the key check window is open, with its log.
    key_check: Option<keycheck::KeyCheck>,
    /// This computer's keyboard is held for a key check: since its last key.
    probe: Option<Instant>,
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
    /// The last motion injected while controlled (to tell which edge it pushes against).
    motion: (f64, f64),
    /// While controlled: the edge over which control was passed on (Pass), and when.
    pass_side: Option<(Side, Instant)>,
    started: Instant,
    last_write: Instant,
    written: Option<Status>,
    written_permissions: Option<crate::Permissions>,
    readiness_error: Option<String>,
    /// This computer's monitors (sorted), looked at every `DISPLAYS_POLL`; empty until first seen.
    my_displays: Vec<Rect>,
    displays_checked: Option<Instant>,
    /// Counts changes of `my_displays`.
    displays_rev: u64,
    /// Per peer: the link token and `displays_rev` it was last told.
    displays_told: HashMap<String, (u64, u64)>,
    /// The monitors each peer told us (kept across blips, like its features).
    peer_displays: HashMap<String, Vec<Rect>>,
    /// Since when each peer has had no link (for the fallback dial).
    down_since: HashMap<String, Instant>,
    /// The addresses discovery listed per peer the last time, to notice a change.
    seen_addrs: HashMap<String, Vec<SocketAddr>>,
    /// Peers that ended their link with Bye "paused": not dialled until then.
    peer_paused_until: HashMap<String, Instant>,
    /// Per peer: tokens of its last few verified links that are gone (closed or the loser of
    /// a duplicate). An explicit "unpaired" goodbye that arrives on one of them still counts.
    retired: HashMap<String, VecDeque<u64>>,
    /// The address this computer last dialled to pair, and when: dialled again while that
    /// pairing waits without a link.
    pair_dial: Option<(SocketAddr, Instant)>,
    /// The listener could not be (re)bound: try again from `maintenance`.
    bind_failed: bool,
    last_bind_try: Option<Instant>,
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
        // One automatic dial per address at a time. A pairing the user asked for goes out
        // even while an earlier link to that address is still closing.
        if outgoing && !self.inflight.insert(addr) && !pair {
            return;
        }
        if outgoing && pair {
            self.pair_dial = Some((addr, Instant::now()));
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
            self.refresh_displays();
            if let Some(d) = &mut self.discovery {
                self.discovered = d.poll();
                self.discovered.retain(|p| p.id != self.identity.id);
                // A computer that just showed up (or came back with other addresses) is
                // dialled at once, not after the rest of a back-off.
                for id in track_discovered(&mut self.seen_addrs, &self.discovered) {
                    self.peer_paused_until.remove(&id);
                    if !self.links.contains_key(&id) && self.retry.remove(&id).is_some() {
                        log::info!("{id} was discovered again: dialling without waiting");
                    }
                }
            }
            match take_request(&self.config.config_dir) {
                Ok(Some(r)) => self.request(r),
                Ok(None) => {}
                Err(e) => log::warn!("network request: {e:#}"),
            }
            if let Some(at) = self.probe {
                if !matches!(self.state, State::Local) || at.elapsed() >= PROBE_IDLE {
                    self.stop_probe();
                }
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
        // Paused now: close the link. Resumed: dial again at once.
        for p in &config.peers {
            let was = self.config.peers.iter().find(|q| q.id == p.id).is_some_and(|q| q.paused);
            if p.paused && self.links.contains_key(&p.id) {
                self.disconnect(&p.id, "paused");
            } else if was && !p.paused {
                self.retry.remove(&p.id);
                self.dial_now(&p.id);
            }
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
        self.seed_peer_displays();
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
                        self.listener = Some(l);
                        self.bind_failed = false;
                    }
                    Err(e) => {
                        self.status.lock().unwrap().me.error = Some(e.to_string());
                        self.bind_failed = true;
                    }
                }
            }
        }
        if rediscover {
            self.setup_discovery();
        }
        if rebind && self.config.on {
            // Switched on again: a backoff grown before or while it was off does not apply.
            self.retry.clear();
            self.dial_all_now();
        }
    }
    /// The side that was just switched on or resumed dials at once, whichever id is smaller:
    /// the other side may be waiting out a pause quiet or a long backoff. A link that comes
    /// up from both sides at once is settled in `Event::Connected`.
    fn dial_now(&mut self, id: &str) {
        match Instant::now().checked_sub(FALLBACK_DIAL) {
            Some(t) => { self.down_since.insert(id.into(), t); }
            None => { self.down_since.remove(id); }
        }
    }
    /// Monitors a peer told us before (saved in its PeerConfig) stand in until it tells again.
    fn seed_peer_displays(&mut self) {
        for p in &self.config.peers {
            if !p.displays.is_empty() && !self.peer_displays.contains_key(&p.id) {
                let rects: Vec<Rect> = geometry::sorted_displays(&p.displays).into_iter().take(MAX_DISPLAYS).collect();
                self.peer_displays.insert(p.id.clone(), rects);
            }
        }
    }
    fn dial_all_now(&mut self) {
        let ids: Vec<String> = self.config.peers.iter().map(|p| p.id.clone()).collect();
        for id in ids {
            self.dial_now(&id);
        }
    }
    fn request(&mut self, request: Request) {
        if let Request::Pause { pause, paused } = &request {
            self.set_paused(pause, *paused);
            return;
        }
        if !self.config.on {
            return;
        }
        match request {
            Request::Pause { .. } => {}
            Request::PairAddr { pair_addr } => match pair_addr.parse() {
                Ok(addr) => self.spawn_link(None, addr, true, None),
                Err(e) => log::warn!("invalid pair address: {e}"),
            },
            Request::Pair { pair } => {
                if let Some(l) = self.links.get(&pair) {
                    l.send(Msg::PairRequest);
                    self.begin_pair(&pair);
                } else if let Some(p) = self.discovered.iter().find(|p| p.id == pair) {
                    if let Some(a) = dial_order(&p.addresses, None).first() {
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
                // Say "unpaired" before `clear_group` closes the master's link with a reason
                // that would leave the master still paired to this computer.
                self.disconnect(&unpair, "unpaired");
                if was_master {
                    self.clear_group();
                }
                self.prune_peer_state();
                (self.hooks.on_unpaired)(unpair);
            }
            Request::Action { action } => match action.as_str() {
                "reconnect" => self.reconnect(None),
                // The keys tab's "check key mapping": a master opens its window; a sub asks
                // its master to and holds its own keyboard for the check.
                "keycheck_open" => {
                    if self.config.role == Role::Master {
                        self.open_key_check();
                    } else if let Some(m) = self.master_link_with("keycheck") {
                        if let Some(l) = self.links.get(&m) {
                            l.send(Msg::KeyCheckOpen);
                        }
                        self.start_probe();
                    }
                }
                "keycheck_probe" => self.start_probe(),
                "keycheck_stop" => self.stop_probe(),
                "keycheck_close" => self.close_key_check(),
                _ => {}
            },
        }
    }
    /// Pause or resume the connection to a paired computer. A paused one is disconnected
    /// (Bye "paused"), not dialled and never a crossing target; resuming dials at once.
    fn set_paused(&mut self, id: &str, paused: bool) {
        let Some(p) = self.config.peers.iter_mut().find(|p| p.id == id && !p.via_master) else {
            return;
        };
        if p.paused == paused {
            return;
        }
        p.paused = paused;
        let peer = p.clone();
        log::info!("{} {}", if paused { "pausing" } else { "resuming" }, id);
        (self.hooks.on_peer_changed)(peer);
        if paused {
            self.disconnect(id, "paused");
        } else {
            self.retry.remove(id);
            self.dial_now(id);
            self.peer_paused_until.remove(id);
        }
    }
    fn is_paused(&self, id: &str) -> bool {
        self.config.peers.iter().any(|p| p.id == id && p.paused)
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
                if let Some(port) = self
                    .discovered
                    .iter()
                    .find(|p| p.id == id && p.fingerprint == l.fp)
                    .and_then(|p| p.addresses.first().map(|a| a.port()))
                {
                    l.addr.set_port(port);
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
                // A peer that dials us is not paused any more.
                self.peer_paused_until.remove(&id);
                if let Some(p) = self.config.peers.iter().chain(&self.trusted).find(|p| p.id == id) {
                    // A roster entry vouches for one role; a pairing is pinned by fingerprint.
                    let roster_only = p.via_master && !self.config.peers.iter().any(|q| q.id == id);
                    if p.fingerprint != link.fp || (roster_only && p.role != link.role) {
                        log::warn!("fingerprint or role mismatch for {id}");
                        return;
                    }
                    if p.paused {
                        log::info!("link from {id} refused: paused");
                        link.send(Msg::Bye { reason: "paused".into() });
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
                        log::info!("second link to {id} dropped (kept the one {} dialled)", if prefer_outgoing { "we" } else { "they" });
                        if pair {
                            // The pairing was asked for on the link that lost the tie-break:
                            // carry it out on the one that stays.
                            old.send(Msg::PairRequest);
                            self.begin_pair(&id);
                        }
                        if link.paired {
                            self.retire(&id, link.token);
                        }
                        return;
                    }
                    self.disconnect(&id, "duplicate");
                }
                if link.paired {
                    if let Some(p) = self.config.peers.iter_mut().find(|p| p.id == id) {
                        let addr = link.addr.to_string();
                        if p.last_addr != addr && prefer_saved_addr(&p.last_addr, link.addr) {
                            p.last_addr = addr;
                            (self.hooks.on_peer_changed)(p.clone());
                        }
                    }
                }
                self.peer_features.insert(id.clone(), link.features.clone());
                let resume_pair = !pair && !link.paired && self.pairing.as_ref().is_some_and(|p| p.peer == id && !p.expired());
                self.links.insert(id.clone(), link);
                if resume_pair {
                    // The link a pairing was going on closed (it lost a duplicate race, say)
                    // and the peer came back: carry on with the pairing on this one.
                    if let (Some(l), Some(p)) = (self.links.get(&id), &self.pairing) {
                        log::info!("pairing with {id} goes on over a new link");
                        l.send(Msg::PairRequest);
                        if p.local_accepted {
                            l.send(Msg::PairAccept);
                        }
                    }
                }
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
                let current = self.links.get(&id).is_some_and(|l| l.token == token);
                if !current {
                    // A verified link that is gone may still bring the user's explicit unpair.
                    let retired = self.retired.get(&id).is_some_and(|t| t.contains(&token));
                    if retired && matches!(&msg, Msg::Bye { reason } if reason == "unpaired") {
                        self.forget_peer(&id);
                    }
                    return;
                }
                let Some(l) = self.links.get_mut(&id) else {
                    return;
                };
                l.last_rx = Instant::now();
                if !l.paired
                    && !matches!(
                        msg,
                        Msg::PairRequest | Msg::PairAccept | Msg::PairReject | Msg::Bye { .. }
                    )
                {
                    // While we pair with it, a peer that still holds us as paired may send
                    // session traffic until it reads our PairRequest: drop that, keep the link.
                    if self.pairing.as_ref().is_some_and(|p| p.peer == id) {
                        return;
                    }
                    self.disconnect(&id, "unpaired message");
                    return;
                }
                // Anything but a goodbye or hello on a paired link means the peer accepts us.
                // ReleaseAll does not: every closing link sends it just before its Bye.
                if !self.unpaired_seen.is_empty() && !matches!(msg, Msg::Bye { .. } | Msg::Hello { .. } | Msg::ReleaseAll) {
                    self.unpaired_seen.remove(&id);
                }
                match msg {
                    Msg::Hello { .. } => self.disconnect(&id, "duplicate hello"),
                    Msg::PairRequest => {
                        // The peer asks to pair on a link we hold as paired: it has forgotten
                        // this computer (unpaired there while the goodbye was lost). Pair anew,
                        // with the code shown on both sides as usual.
                        if l.paired && self.pairing.is_none() {
                            log::info!("{id} asks to pair again: it no longer knows this computer");
                            l.paired = false;
                        }
                        self.begin_pair(&id)
                    }
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
                    Msg::Displays { rects } => {
                        let rects: Vec<Rect> = geometry::sorted_displays(&rects).into_iter().take(MAX_DISPLAYS).collect();
                        if rects.is_empty() {
                            self.peer_displays.remove(&id);
                        } else {
                            if self.peer_displays.get(&id) != Some(&rects) {
                                log::info!("{id} has {} monitor(s)", rects.len());
                                self.peer_displays.insert(id.clone(), rects.clone());
                            }
                            // A master keeps them, so the peer's tiles stay arranged while it is away.
                            if self.config.role == Role::Master {
                                if let Some(p) = self.config.peers.iter_mut().find(|p| p.id == id && !p.via_master) {
                                    if p.displays != rects {
                                        p.displays = rects;
                                        (self.hooks.on_peer_changed)(p.clone());
                                    }
                                }
                            }
                        }
                    }
                    Msg::Enter { side, pos, display } => {
                        if !can_send {
                            l.send(Msg::ReleaseAll);
                        } else if matches!(self.state, State::Local)
                            && pos.is_finite()
                            && self.platform.ready().is_ok()
                        {
                            let displays = self.platform.displays();
                            // With a monitor number the position runs along that monitor's
                            // edge; an unknown monitor or an old peer means the whole edge.
                            let landing = display
                                .and_then(|d| geometry::display_edge_position(&displays, d, side.opposite(), pos))
                                .or_else(|| geometry::edge_position(&displays, side.opposite(), pos));
                            if let Some((x, y)) = landing {
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
                    Msg::Leave { pos, display, side } => {
                        if matches!(&self.state,State::Controlling{peer,..} if peer==&id)
                            && pos.is_finite()
                        {
                            if let State::Controlling { pos: ref mut p, display: ref mut d, side: ref mut s, .. } = &mut self.state {
                                *p = pos;
                                *d = display;
                                if let Some(side) = side {
                                    *s = side;
                                }
                            }
                            self.local(false);
                        }
                    }
                    Msg::KeyCheckOpen => {
                        if self.config.role == Role::Master && self.is_direct_peer(&id) {
                            self.open_key_check();
                        }
                    }
                    Msg::KeyProbe { usage, down } => {
                        if self.config.role == Role::Master && self.is_direct_peer(&id) {
                            self.check_key(&id, usage, down, None);
                        }
                    }
                    Msg::KeyCheckEnd => {
                        if from_master {
                            self.stop_probe();
                        }
                    }
                    Msg::Pass { to, side, pos, whole, display } => {
                        if matches!(&self.state, State::Controlling { peer, .. } if peer == &id)
                            && pos.is_finite()
                            && whole.is_finite()
                        {
                            self.pass_control(&id, to, side, pos.clamp(0.0, 1.0), whole.clamp(0.0, 1.0), display);
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
                            self.forget_peer(&id);
                        } else {
                            // The peer dropped this link for a duplicate one: like a lost link,
                            // that leaves a pairing on it waiting for the next link.
                            self.disconnect(&id, if reason == "duplicate" { "duplicate" } else { "bye" });
                            if reason == "paused" {
                                // The peer paused its side: dialling it again only makes it refuse.
                                self.peer_paused_until.insert(id, Instant::now() + PEER_PAUSE_QUIET);
                            }
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
    /// The peer unpaired this computer: forget it here too, so the two can pair again.
    fn forget_peer(&mut self, id: &str) {
        if !self.config.peers.iter().any(|p| p.id == id && !p.via_master) {
            return;
        }
        self.recent_paired.remove(id);
        self.unpaired_seen.remove(id);
        let was_master = self.config.peers.iter().any(|p| p.id == id && p.role == Role::Master);
        self.config.peers.retain(|p| p.id != id);
        self.config.layout.retain(|c| c.id != id);
        self.disconnect(id, "bye");
        if was_master {
            self.clear_group();
        }
        self.prune_peer_state();
        self.retired.remove(id);
        (self.hooks.on_unpaired)(id.to_string());
    }
    fn retire(&mut self, id: &str, token: u64) {
        let tokens = self.retired.entry(id.to_string()).or_default();
        tokens.push_back(token);
        while tokens.len() > 4 {
            tokens.pop_front();
        }
    }
    fn disconnect(&mut self, id: &str, reason: &str) {
        if self.file_jobs.cancel_peer(id) { self.fail_running_transfer(); }
        if self.state.peer() == Some(id) {
            self.local(false);
        }
        self.told.remove(id);
        self.displays_told.remove(id);
        self.deferred_clipboard.remove(id);
        if let Some(l) = self.links.remove(id) {
            if l.paired {
                self.retire(id, l.token);
            }
            log::info!(
                "link to {id} ({}) closed: {reason}; {} one, up for {:.1} s",
                l.addr,
                if l.outgoing { "our" } else { "their" },
                l.up_since.elapsed().as_secs_f64()
            );
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
        // A pairing whose link was merely lost waits (until it expires) for the next link.
        let lost = matches!(reason, "link lost" | "duplicate");
        if self.pairing.as_ref().is_some_and(|p| p.peer == id && !(lost && !p.expired())) {
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
        let discovered = self.discovered.iter().any(|d| d.id == id);
        let seconds = backoff_secs(retry.attempt, discovered);
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
            _ if self.config.role == Role::Master => {
                let cells = if self.config.layout.is_empty() {
                    layout::cells_from_sides(me, &self.config.peers)
                } else {
                    // A cell whose computer is no longer paired is an orphan: never shown or handed on.
                    // Cells of monitors that are gone stay in the saved layout but do not count.
                    let known: Vec<Cell> = self.config.layout.iter()
                        .filter(|c| c.id == *me || self.config.peers.iter().any(|p| p.id == c.id))
                        .cloned().collect();
                    layout::valid_cells(&layout::live_cells(&known, |id| self.display_count(id)))
                };
                // Every monitor of this computer and of each peer that told us its monitors
                // has a cell, placed as the monitors are arranged.
                let mut cells = layout::with_master_displays(&cells, me, &self.my_displays);
                for p in &self.config.peers {
                    if let Some(rects) = self.peer_displays.get(&p.id) {
                        cells = layout::auto_place(&cells, &p.id, rects);
                    }
                }
                cells
            }
            _ => layout::cells_from_sides(me, &self.config.peers),
        }
    }
    /// How many monitors computer `id` has, if known: this computer's own list, what a peer
    /// told us, or one for a peer that cannot tell (a pre-2.2 version that is connected).
    fn display_count(&self, id: &str) -> Option<usize> {
        if id == self.identity.id {
            return (!self.my_displays.is_empty()).then_some(self.my_displays.len());
        }
        if let Some(rects) = self.peer_displays.get(id) {
            return Some(rects.len());
        }
        self.peer_features
            .get(id)
            .filter(|f| !f.iter().any(|f| f == "displays"))
            .map(|_| 1)
    }
    /// Looks at this computer's monitors (at most every `DISPLAYS_POLL`).
    fn refresh_displays(&mut self) {
        if self.displays_checked.is_some_and(|t| t.elapsed() < DISPLAYS_POLL) {
            return;
        }
        self.displays_checked = Some(Instant::now());
        let now = geometry::sorted_displays(&self.platform.displays());
        if now != self.my_displays {
            log::info!("monitors: {} ({:?})", now.len(), now);
            self.my_displays = now;
            self.displays_rev += 1;
        }
    }
    /// Tells every paired peer that understands it (feature "displays") our monitors,
    /// again when they changed or the link is a new one.
    fn tell_displays(&mut self) {
        if self.my_displays.is_empty() {
            return;
        }
        let rev = self.displays_rev;
        self.displays_told.retain(|id, (token, _)| self.links.get(id).is_some_and(|l| l.token == *token));
        for (id, l) in &self.links {
            if !l.paired || !l.features.iter().any(|f| f == "displays") {
                continue;
            }
            if self.displays_told.get(id) == Some(&(l.token, rev)) {
                continue;
            }
            if l.send(Msg::Displays { rects: self.my_displays.clone() }) {
                self.displays_told.insert(id.clone(), (l.token, rev));
            }
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
        self.peer_displays.retain(|id, _| known.contains(id.as_str()));
        self.displays_told.retain(|id, _| known.contains(id.as_str()));
        self.down_since.retain(|id, _| known.contains(id.as_str()));
        self.peer_paused_until.retain(|id, _| known.contains(id.as_str()));
        self.retired.retain(|id, _| known.contains(id.as_str()));
    }
    /// The open link on the other side of the neighbouring cell on `side`.
    /// The open link of the computer next to monitor `display` of this one on `side`, and
    /// the monitor of it that lies there. A paused computer is skipped.
    /// The monitor is None unless the layout has a real cell for it and every monitor of that
    /// computer is placed (see `target_display`).
    fn neighbour_peer(&self, cells: &[Cell], display: u8, side: Side) -> Option<(String, Option<u8>)> {
        if !self.me_can_send() {
            return None;
        }
        layout::neighbour_of(cells, &self.identity.id, display, side)
            .filter(|(id, _)| self.open(id) && !self.is_paused(id))
            .map(|(id, d)| (id.to_string(), self.target_display(cells, id, d)))
    }
    /// The monitor `d` of computer `id` to name in Enter or Leave: only when all its monitors
    /// have a cell, otherwise the cell may stand for the whole computer (a 2.1 layout, or a
    /// monitor whose spot was taken) and the whole edge is used.
    fn target_display(&self, cells: &[Cell], id: &str, d: u8) -> Option<u8> {
        let placed = cells.iter().filter(|c| c.id == id).count();
        (placed >= self.display_count(id).unwrap_or(1).max(1)).then_some(d)
    }
    /// The sides that have a connected computer next to any of this computer's monitors.
    fn armed_sides(&self, cells: &[Cell]) -> Vec<Side> {
        [Side::Left, Side::Right, Side::Top, Side::Bottom]
            .into_iter()
            .filter(|&side| {
                cells.iter().filter(|c| c.id == self.identity.id)
                    .any(|c| self.neighbour_peer(cells, c.display, side).is_some())
            })
            .collect()
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
        log::info!("reconnect (after a wake-up: {}): closing {} link(s)", stale.is_some(), close.len());
        for id in close {
            self.disconnect(&id, "reconnect");
        }
        self.retry.clear();
        self.down_since.clear();
        self.dial_all_now();
        self.reconnecting = Some(Instant::now());
        // A wake-up or network change can leave the listener and the mDNS advertisement
        // stale: check the listener and, after a wake-up, start discovery afresh.
        self.ensure_listener(true);
        if stale.is_some() && self.discovery.is_some() {
            self.setup_discovery();
        } else if let Some(d) = &mut self.discovery {
            d.rebrowse();
        }
        self.dial_peers();
    }
    /// Binds the listener again when its thread died or an earlier bind failed. `now` skips
    /// the wait between tries.
    fn ensure_listener(&mut self, now: bool) {
        if !self.config.on {
            return;
        }
        if self.listener.as_ref().is_some_and(|l| !l.alive()) {
            log::warn!("listener thread ended: binding again");
            self.listener = None;
            self.bind_failed = true;
        }
        if !self.bind_failed || self.listener.is_some() {
            return;
        }
        if !now && self.last_bind_try.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
            return;
        }
        self.last_bind_try = Some(Instant::now());
        match bind(self.config.port).and_then(|l| Acceptor::start(l, self.events.clone(), self.wake.clone())) {
            Ok(l) => {
                log::info!("listening on port {}", l.port());
                let changed = self.status.lock().unwrap().me.port != l.port();
                self.status.lock().unwrap().me.port = l.port();
                self.listener = Some(l);
                self.bind_failed = false;
                if changed || self.discovery.is_none() {
                    self.setup_discovery();
                }
            }
            Err(e) => {
                log::warn!("cannot listen on port {}: {e:#}", self.config.port);
                self.status.lock().unwrap().me.error = Some(e.to_string());
            }
        }
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
        self.ensure_listener(false);
        // Links to a computer paused meanwhile (by the app's config or a request) end now.
        let paused: Vec<String> = self.links.keys().filter(|id| self.is_paused(id)).cloned().collect();
        for id in paused {
            self.disconnect(&id, "paused");
        }
        self.tell_displays();
        self.tell_subs();
        self.drop_stalled_incoming(Instant::now());
        if self.pairing.as_ref().is_some_and(|p| p.expired()) {
            self.fail_pair();
        }
        // A pairing this computer dialled for lost its link (and the peer may have stopped
        // dialling us, having forgotten us): dial it again.
        if let (Some(p), Some((addr, at))) = (&self.pairing, self.pair_dial) {
            if !self.links.contains_key(&p.peer) && at.elapsed() >= Duration::from_secs(1) {
                let peer = p.peer.clone();
                self.spawn_link(None, addr, true, Some(peer));
            }
        }
        if self.pairing.is_none() {
            self.pair_dial = None;
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
        let near = layout::neighbours(&cells, &self.identity.id);
        let peers: Vec<PeerConfig> = self
            .config
            .peers
            .iter()
            .chain(self.trusted.iter().filter(|t| near.contains(&t.id.as_str())))
            .cloned()
            .collect();
        for p in peers {
            if p.paused || self.peer_paused_until.get(&p.id).is_some_and(|t| *t > Instant::now()) {
                continue;
            }
            if self.links.contains_key(&p.id) {
                self.down_since.remove(&p.id);
                continue;
            }
            let down = *self.down_since.entry(p.id.clone()).or_insert_with(Instant::now);
            // The smaller id dials. The other side only helps after a while without a link,
            // for the case that the smaller one cannot find or reach it; a link that comes up
            // on both sides at once is settled in `Event::Connected`.
            if (self.identity.id >= p.id && down.elapsed() < FALLBACK_DIAL)
                || self
                    .retry
                    .get(&p.id)
                    .is_some_and(|r| r.next > Instant::now())
            {
                continue;
            }
            match self.dial_address(&p) {
                Some(addr) => {
                    if !self.inflight.contains(&addr) {
                        log::debug!("dialling {} at {addr}", p.id);
                        self.spawn_link(None, addr, false, Some(p.id.clone()));
                        self.schedule_retry(&p.id);
                    }
                }
                None => {
                    log::debug!("no address to dial {}", p.id);
                    self.schedule_retry(&p.id);
                }
            }
        }
    }
    /// The address to dial now: every address discovery lists and the saved one, best first
    /// (see `dial_order`), one per attempt in turn.
    fn dial_address(&self, peer: &PeerConfig) -> Option<SocketAddr> {
        let found: Vec<SocketAddr> = self.discovered.iter()
            .filter(|d| d.id == peer.id && d.fingerprint == peer.fingerprint)
            .flat_map(|d| d.addresses.iter().copied())
            .collect();
        let candidates = dial_order(&found, peer.last_addr.parse::<SocketAddr>().ok());
        let attempt = self.retry.get(&peer.id).map_or(0, |r| r.attempt);
        pick_candidate(&candidates, attempt)
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
        if let State::Controlling { side, pos, display, .. } = old {
            self.platform.stop_capture();
            let monitors = geometry::sorted_displays(&self.platform.displays()).len();
            match display {
                // With one monitor both positions are the same point; keep the platform's own way.
                // A monitor that is gone since the crossing falls back to the whole edge.
                Some(d) if monitors > 1 && usize::from(d) < monitors => {
                    self.platform.place_on_return_display(side, d, pos)
                }
                _ => self.platform.place_on_return(side, pos),
            }
        }
        if let State::Controlled { entered, .. } = old {
            // Passed on over another edge: the cursor stays at that edge.
            let entered = match self.pass_side.take() {
                Some((side, at)) if at.elapsed() < Duration::from_secs(2) => side,
                _ => entered,
            };
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
                self.motion = (*dx, *dy);
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
        if self.probe.is_some() {
            self.probe_event(event);
            return;
        }
        let State::Controlling { peer, .. } = &self.state else {
            return;
        };
        let peer = peer.clone();
        if !event.valid() {
            return;
        }
        if let (Some(_), InputEvent::Key { usage, down }) = (&self.key_check, &event) {
            let (usage, down, me) = (*usage, *down, self.identity.id.clone());
            self.check_key(&me, usage, down, Some(&peer));
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
                    self.mapper.set_lang1_toggle(l.features.iter().any(|f| f == "lang1-toggle"));
                    for event in self.mapper.map(event, self.platform.os(), l.os, &rules, now) {
                        l.send(Msg::Input(event));
                    }
                }
                None => { l.send(Msg::Input(event)); }
            }
        }
    }

    /// A paired computer of this one's own (not one known only through the master).
    fn is_direct_peer(&self, id: &str) -> bool {
        self.config.peers.iter().any(|p| p.id == id && !p.via_master)
    }
    /// On a sub: the linked master, when it understands `feature`.
    fn master_link_with(&self, feature: &str) -> Option<String> {
        let m = self.config.peers.iter().find(|p| p.role == Role::Master && !p.via_master)?;
        let l = self.links.get(&m.id).filter(|l| l.paired)?;
        l.features.iter().any(|f| f == feature).then(|| m.id.clone())
    }
    fn open_key_check(&mut self) {
        if self.key_check.is_none() {
            self.key_check = Some(keycheck::KeyCheck::default());
        }
        (self.hooks.open_key_check)();
        self.publish(true);
    }
    /// The key check window was closed: the subs stop sending keys, this computer lets go.
    fn close_key_check(&mut self) {
        if self.key_check.take().is_none() {
            return;
        }
        for l in self.links.values().filter(|l| l.paired && l.features.iter().any(|f| f == "keycheck")) {
            l.send(Msg::KeyCheckEnd);
        }
        self.stop_probe();
        self.publish(true);
    }
    /// Holds this computer's keyboard (and pointer) for a key check: only while it is on
    /// its own, never across a crossing. Escape, `PROBE_IDLE` without a key, or the window
    /// closing ends it.
    fn start_probe(&mut self) {
        if self.probe.is_some() || !matches!(self.state, State::Local) || self.platform.ready().is_err() {
            return;
        }
        if self.edge_hit.take().is_some() {
            self.platform.stop_capture();
        }
        self.edge_wait = None;
        while self.captured.try_recv().is_ok() {}
        if let Err(e) = self.platform.start_capture(self.capture.clone()) {
            log::warn!("key check: cannot hold the keyboard: {e}");
            self.platform.stop_capture();
            return;
        }
        log::info!("key check: this keyboard is held until Escape");
        self.probe = Some(Instant::now());
        self.captured_keys.clear();
        self.publish(true);
    }
    fn stop_probe(&mut self) {
        if self.probe.take().is_none() {
            return;
        }
        self.platform.stop_capture();
        // Keys still down are let go in the log too, so the next check starts clean.
        let held: Vec<u16> = self.captured_keys.drain().collect();
        for usage in held {
            self.send_probe(usage, false);
        }
        self.publish(true);
    }
    /// A captured event while this keyboard is held for a key check.
    fn probe_event(&mut self, event: InputEvent) {
        let InputEvent::Key { usage, down } = event else { return };
        self.probe = Some(Instant::now());
        if usage == 0x29 {
            if down {
                self.stop_probe();
            }
            return;
        }
        if down {
            self.captured_keys.insert(usage);
        } else if !self.captured_keys.remove(&usage) {
            return;
        }
        self.send_probe(usage, down);
    }
    fn send_probe(&mut self, usage: u16, down: bool) {
        if self.config.role == Role::Master {
            let me = self.identity.id.clone();
            self.check_key(&me, usage, down, None);
        } else if let Some(m) = self.master_link_with("keycheck") {
            if let Some(l) = self.links.get(&m) {
                l.send(Msg::KeyProbe { usage, down });
            }
        }
    }
    /// On a master with the window open: logs a key edge from computer `from` (this one or
    /// a sub) as each other computer of the group gets it.
    fn check_key(&mut self, from: &str, usage: u16, down: bool, live: Option<&str>) {
        let Some(check) = &mut self.key_check else { return };
        let mut group = vec![keycheck::Computer {
            id: self.identity.id.clone(),
            name: computer_name(&self.config),
            os: self.platform.os(),
        }];
        group.extend(self.config.peers.iter().filter(|p| !p.via_master).map(|p| keycheck::Computer {
            id: p.id.clone(),
            name: p.name.clone(),
            os: p.os,
        }));
        let Some(source) = group.iter().find(|c| c.id == from).cloned() else { return };
        let config = &self.config;
        if check.record(&source, usage, down, &group, live, |s, d| config.rules_for(s, d), Instant::now()) {
            self.publish(true);
        }
    }
    fn switch_channel(&self, id: &str) {
        if let Some(p) = self.config.peers.iter().find(|p| p.id == id) {
            if (1..=3).contains(&p.logitech_channel) {
                (self.hooks.switch_logitech)(p.logitech_channel);
            }
        }
    }
    /// Takes control of `peer` across `side`: out of this computer's monitor `exit.display`,
    /// into the peer's monitor `exit.target`.
    fn cross_to(&mut self, peer: String, side: Side, exit: Exit) {
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
            // Only a peer that knows monitors gets a monitor number; an older one gets the
            // position along the whole edge, as before.
            l.send(match exit.target {
                Some(target) if l.features.iter().any(|f| f == "displays") => {
                    Msg::Enter { side, pos: exit.pos, display: Some(target) }
                }
                _ => Msg::Enter { side, pos: exit.whole, display: None },
            });
        }
        self.state = State::Controlling {
            peer: peer.clone(),
            side,
            pos: exit.pos,
            display: Some(exit.display),
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
        // The app measured `pos` along the whole edge: find the monitor it is on.
        let pos = pos.clamp(0.0, 1.0);
        let displays = self.platform.displays();
        let (display, along) = geometry::display_for_edge_pos(&displays, side, pos).unwrap_or((0, pos));
        let cells = self.my_cells();
        let Some((peer, target)) = self.neighbour_peer(&cells, display, side) else {
            return;
        };
        self.cross_to(peer, side, Exit { display, target, pos: along, whole: pos });
    }
    fn poll_cursor(&mut self) {
        // The keyboard is held for a key check: no crossing until it ends.
        if self.probe.is_some() {
            return;
        }
        if !matches!(self.state, State::Local) && self.platform.ready().is_err() {
            self.local(true);
        }
        let cells = self.my_cells();
        let sides = self.armed_sides(&cells);
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
        // Where the cursor is on an outer edge: monitor, side, the position along that
        // monitor's edge and along the whole edge.
        let (edge, x, y) = if let Some((x, y)) = cursor {
            let edge = geometry::edge_at_display(&displays, x, y, self.config.corner_margin).map(|(display, side)| {
                let rect = geometry::sorted_displays(&displays)[usize::from(display)];
                Exit {
                    display,
                    target: None,
                    pos: geometry::display_edge_ratio(rect, side, x, y),
                    whole: geometry::edge_ratio(&displays, side, x, y),
                }
                .at(side)
            });
            (edge, x, y)
        } else if let Some((side, pos, since)) = self.edge_hit {
            self.edge_wait = Some((side, since));
            // A platform that only reports a position along the whole edge.
            let (display, along) = geometry::display_for_edge_pos(&displays, side, pos).unwrap_or((0, pos));
            (Some(Exit { display, target: None, pos: along, whole: pos }.at(side)), 0.0, 0.0)
        } else {
            self.edge_wait = None;
            return;
        };
        match self.state.clone() {
            State::Local if self.config.app_drives_edges && cursor.is_some() => {
                self.edge_wait = None;
            }
            State::Local => {
                let Some((mut exit, side)) = edge else {
                    self.edge_wait = None;
                    if self.capture_block.is_some_and(|t| Instant::now() >= t) {
                        self.capture_backoff = Duration::ZERO;
                    }
                    return;
                };
                let Some((peer, target)) = self.neighbour_peer(&cells, exit.display, side) else {
                    // An edge of a monitor with nothing next to it (or a computer that is gone
                    // or paused): a platform edge hit is handed back at once.
                    if self.edge_hit.take().is_some() {
                        self.platform.stop_capture();
                    }
                    self.edge_wait = None;
                    return;
                };
                exit.target = target;
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
                self.cross_to(peer, side, exit);
            }
            State::Controlled { peer, entered } => {
                let control_ok = !self.config.require_control || self.control_held(&peer);
                if let Some((exit, side)) = edge.filter(|_| control_ok) {
                    let toward = |dx: f64, dy: f64| match side {
                        Side::Left => dx < 0.0,
                        Side::Right => dx > 0.0,
                        Side::Top => dy < 0.0,
                        Side::Bottom => dy > 0.0,
                    };
                    let pushing = if side == entered { self.outward } else { toward(self.motion.0, self.motion.1) };
                    let native_out = self.last_cursor.is_some_and(|(px, py)| toward(x - px, y - py));
                    if pushing || native_out {
                        self.leave_controlled(&cells, &peer, entered, exit, side);
                    }
                }
            }
            State::Controlling { .. } => {}
        }
        self.last_cursor = cursor;
    }
    /// While controlled by `peer`, the cursor pushes out over `side` of monitor `exit.display`.
    /// Whatever the layout puts next to that edge gets the cursor: the controlling computer
    /// (on the monitor that is there, so a computer wrapped around by its monitors can leave
    /// over any edge that touches one), or another computer, to which the controlling one
    /// passes control (sub to sub). The edge it came in by keeps the rules from before.
    fn leave_controlled(&mut self, cells: &[Cell], peer: &str, entered: Side, exit: Exit, side: Side) {
        let me = self.identity.id.clone();
        let next = layout::neighbour_of(cells, &me, exit.display, side).map(|(id, d)| (id.to_string(), d));
        if let Some((to, d)) = next.as_ref().filter(|(to, _)| to != peer) {
            if self.ask_to_pass(peer, to, d.to_owned(), exit, side) {
                return;
            }
            if side != entered {
                return;
            }
        }
        let back = match next {
            Some((to, d)) if to == peer => Some(self.target_display(cells, peer, d)),
            _ if side == entered => self.way_back(cells, peer, exit.display, entered),
            _ => None,
        };
        let Some(target) = back else { return };
        // A peer that knows monitors gets the position along the monitor's edge and the
        // monitor to land on; an older one the whole edge. Over another edge than the one it
        // came in by, the peer is told its own side to come back at.
        let shows = self.links.get(peer).is_some_and(|l| l.features.iter().any(|f| f == "displays"));
        let back_side = (side != entered).then(|| side.opposite());
        let msg = match target {
            Some(d) if shows => Msg::Leave { pos: exit.pos, display: Some(d), side: back_side },
            _ => Msg::Leave { pos: exit.whole, display: None, side: back_side },
        };
        self.send_clipboard(peer);
        if let Some(l) = self.links.get(peer) {
            l.send(msg);
        }
        if side != entered {
            self.pass_side = Some((side, Instant::now()));
        }
        self.local(false);
        if layout::neighbour_of(cells, &me, exit.display, side).is_some_and(|(id, _)| id == peer) {
            self.switch_channel(peer);
        }
    }
    /// Asks the controlling computer `peer` to move control on to `to` (Pass). False when it
    /// cannot (a peer before 2.4). The cursor stays here until `peer` answers with
    /// ReleaseAll; while the edge is pushed the request is repeated now and then.
    fn ask_to_pass(&mut self, peer: &str, to: &str, display: u8, exit: Exit, side: Side) -> bool {
        let Some(l) = self.links.get(peer) else { return false };
        if !l.features.iter().any(|f| f == "pass") {
            return false;
        }
        if self.pass_side.is_some_and(|(s, at)| s == side && at.elapsed() < Duration::from_millis(300)) {
            return true;
        }
        l.send(Msg::Pass { to: to.to_string(), side, pos: exit.pos, whole: exit.whole, display: Some(display) });
        self.pass_side = Some((side, Instant::now()));
        true
    }
    /// The computer `from` that this one controls asked to pass control on to `to`.
    fn pass_control(&mut self, from: &str, to: String, side: Side, pos: f64, whole: f64, display: Option<u8>) {
        if to == self.identity.id || !self.open(&to) || self.is_paused(&to) || !self.me_can_send() {
            log::info!("{from} asked to pass control to {to}: not possible here");
            return;
        }
        let cells = self.my_cells();
        let target = display.and_then(|d| self.target_display(&cells, &to, d));
        log::info!("control passes from {from} to {to}");
        if let Some(l) = self.links.get(from) {
            l.send(Msg::ReleaseAll);
        }
        self.send_clipboard(&to);
        if let Some(l) = self.links.get(&to) {
            l.send(match target {
                Some(d) if l.features.iter().any(|f| f == "displays") => Msg::Enter { side, pos, display: Some(d) },
                _ => Msg::Enter { side, pos: whole, display: None },
            });
        }
        if let State::Controlling { peer, .. } = &mut self.state {
            *peer = to.clone();
        }
        self.captured_keys.clear();
        self.mapper = KeyMapper::default();
        self.switch_channel(&to);
    }
    /// Whether the cursor may go back to `peer` over the `entered` edge of monitor `display`:
    /// None when not; Some(Some(m)) when the layout puts the peer's monitor m there; Some(None)
    /// when it may go back without knowing a monitor. With one monitor (or a layout that does
    /// not list this computer) any edge it came in over leads back, as before 2.2; with several
    /// only the monitor that has the peer next to it does.
    fn way_back(&self, cells: &[Cell], peer: &str, display: u8, entered: Side) -> Option<Option<u8>> {
        let me = &self.identity.id;
        match layout::neighbour_of(cells, me, display, entered) {
            Some((id, d)) if id == peer => Some(self.target_display(cells, id, d)),
            _ => {
                let mine = cells.iter().filter(|c| c.id == *me).count();
                (mine <= 1 || geometry::sorted_displays(&self.platform.displays()).len() <= 1).then_some(None)
            }
        }
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
        status.me.displays = self.my_displays.clone();
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
                displays: self.peer_displays.get(&p.id).cloned().unwrap_or_default(),
                paused: p.paused,
            })
            .collect();
        status.layout = self.my_cells();
        status.keys = self.config.all_rules();
        status.key_check = (self.key_check.is_some() || self.probe.is_some()).then(|| KeyCheckStatus {
            open: self.key_check.is_some(),
            probing: self.probe.is_some(),
            entries: self.key_check.as_ref().map(|k| k.entries.iter().cloned().collect()).unwrap_or_default(),
        });
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
                addr: dial_order(&p.addresses, None)
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
        match (self.transport(socket, token, &mut remote), &remote) {
            (Err(e), Some(id)) => log::info!("link {token} to {id} ({}) ended: {e:#}", self.addr),
            (Ok(()), Some(id)) => log::info!("link {token} to {id} ({}) ended: closed here", self.addr),
            (Err(e), None) if self.outgoing => {
                // One line per failed attempt, but a computer that stays away is not logged every few seconds.
                if dial_failure_loggable(&mut dial_failures().lock().unwrap(), self.addr, Instant::now()) {
                    log::info!("dial {} failed: {e:#}", self.addr);
                }
            }
            (Err(e), None) => log::debug!("incoming {} dropped before Hello: {e:#}", self.addr),
            (Ok(()), None) => {}
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
                    if self.outgoing {
                        dial_failures().lock().unwrap().remove(&self.addr);
                        log::info!("dial {} connected to {id}", self.addr);
                    } else {
                        log::info!("accepted {} from {id}", self.addr);
                    }
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
                            last_rx: Instant::now(), up_since: Instant::now(),
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
/// The scope of a link-local IPv6 address (the interface) is kept: without it the address
/// cannot be dialled.
fn canonical(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            None => SocketAddr::V6(std::net::SocketAddrV6::new(*v6.ip(), v6.port(), 0, v6.scope_id())),
        },
        v4 => v4,
    }
}
fn link_local_v6(ip: &std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}
/// How good an address is to dial: 0 ordinary IPv4, 1 self-assigned IPv4, 2 IPv6 (global or
/// unique-local), 3 IPv6 link-local (needs the right interface, and often is an AirDrop one).
fn addr_rank(addr: &SocketAddr) -> u8 {
    match addr.ip() {
        std::net::IpAddr::V4(v4) => u8::from(v4.is_link_local()),
        std::net::IpAddr::V6(v6) => if link_local_v6(&v6) { 3 } else { 2 },
    }
}
/// The addresses worth dialling, best first: local-network ones only, link-local IPv6 only
/// with its interface, IPv4 before IPv6, what discovery just heard before the saved address
/// of the same quality, no duplicates.
fn dial_order(found: &[SocketAddr], saved: Option<SocketAddr>) -> Vec<SocketAddr> {
    let mut list: Vec<(SocketAddr, u8)> = Vec::new();
    for (addr, from_saved) in found.iter().map(|a| (*a, 0u8)).chain(saved.map(|a| (a, 1u8))) {
        let addr = canonical(addr);
        let unusable = !crate::is_local_network(addr.ip())
            || matches!(addr, SocketAddr::V6(v6) if link_local_v6(v6.ip()) && v6.scope_id() == 0);
        if !unusable && !list.iter().any(|(a, _)| *a == addr) {
            list.push((addr, from_saved));
        }
    }
    list.sort_by_key(|(a, from_saved)| (addr_rank(a), *from_saved));
    list.into_iter().map(|(a, _)| a).collect()
}
/// Candidate number `attempt` (counting from the best, wrapping round).
fn pick_candidate(candidates: &[SocketAddr], attempt: usize) -> Option<SocketAddr> {
    (!candidates.is_empty()).then(|| candidates[attempt % candidates.len()])
}
/// Whether the address a link came in on should replace the saved one: not when it would
/// trade a plain IPv4 address for a worse kind (IPv6 link-local from a dual-stack accept).
fn prefer_saved_addr(saved: &str, new: SocketAddr) -> bool {
    match saved.parse::<SocketAddr>() {
        Ok(old) => {
            let old = canonical(old);
            !crate::is_local_network(old.ip()) || addr_rank(&canonical(new)) <= addr_rank(&old)
        }
        Err(_) => true,
    }
}
/// Notes the addresses discovery lists per computer and returns the ids that are new or
/// whose addresses changed since the last call.
fn track_discovered(seen: &mut HashMap<String, Vec<SocketAddr>>, now: &[DiscoveredPeer]) -> Vec<String> {
    let mut changed = Vec::new();
    let mut current: HashMap<String, Vec<SocketAddr>> = HashMap::new();
    for p in now {
        let mut addrs: Vec<SocketAddr> = p.addresses.iter().copied().map(canonical).collect();
        addrs.sort();
        addrs.dedup();
        current.insert(p.id.clone(), addrs);
    }
    for (id, addrs) in &current {
        if seen.get(id) != Some(addrs) {
            changed.push(id.clone());
        }
    }
    *seen = current;
    changed.sort();
    changed
}
/// Failed dials per address: how many in a row and when one was last logged.
fn dial_failures() -> &'static Mutex<HashMap<SocketAddr, (u32, Instant)>> {
    static FAILURES: std::sync::OnceLock<Mutex<HashMap<SocketAddr, (u32, Instant)>>> = std::sync::OnceLock::new();
    FAILURES.get_or_init(Default::default)
}
const DIAL_LOG_STREAK: u32 = 3;
const DIAL_LOG_EVERY: Duration = Duration::from_secs(60);
/// The first few failures to an address are logged, then one a minute.
fn dial_failure_loggable(failures: &mut HashMap<SocketAddr, (u32, Instant)>, addr: SocketAddr, now: Instant) -> bool {
    if failures.len() > 256 {
        failures.clear();
    }
    let entry = failures.entry(addr).or_insert((0, now));
    entry.0 += 1;
    if entry.0 <= DIAL_LOG_STREAK || now.duration_since(entry.1) >= DIAL_LOG_EVERY {
        entry.1 = now;
        true
    } else {
        false
    }
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
    fn dial_backoff_is_capped_at_4_for_discovered_and_10_for_saved_only() {
        let waits: Vec<u64> = (0..8).map(|n| backoff_secs(n, false)).collect();
        assert_eq!(waits, [1, 2, 4, 8, 10, 10, 10, 10]);
        assert_eq!(backoff_secs(usize::MAX, false), 10);
        // A computer that is on the network is never waited for longer than 4 s.
        let waits: Vec<u64> = (0..8).map(|n| backoff_secs(n, true)).collect();
        assert_eq!(waits, [1, 2, 4, 4, 4, 4, 4, 4]);
        assert_eq!(backoff_secs(usize::MAX, true), 4);
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
        /// Monitors to report (empty: one 100x100 monitor).
        rects: Mutex<Vec<Rect>>,
        /// Where the cursor is (None: the middle of the default monitor).
        cursor_at: Mutex<Option<(f64, f64)>>,
        /// The platform cannot read the cursor (Wayland): only edge hits tell.
        blind: AtomicBool,
        hit: Mutex<Option<(Side, f64)>>,
        warps: Mutex<Vec<(f64, f64)>>,
        armed: Mutex<Vec<Side>>,
    }
    impl Platform for TestPlatform {
        fn os(&self) -> Os {
            if self.android.load(Ordering::Relaxed) { Os::Android }
            else if self.mac.load(Ordering::Relaxed) { Os::Mac } else { Os::Windows }
        }
        fn displays(&self) -> Vec<Rect> {
            let rects = self.rects.lock().unwrap().clone();
            if rects.is_empty() { vec![Rect::new(0.0, 0.0, 100.0, 100.0)] } else { rects }
        }
        fn cursor(&self) -> Option<(f64, f64)> {
            if self.blind.load(Ordering::Relaxed) { return None; }
            Some(self.cursor_at.lock().unwrap().unwrap_or((50.0, 50.0)))
        }
        fn warp(&self, x: f64, y: f64) { self.warps.lock().unwrap().push((x, y)); }
        fn arm_edges(&self, sides: &[Side]) { *self.armed.lock().unwrap() = sides.to_vec(); }
        fn take_edge_hit(&self) -> Option<(Side, f64)> { self.hit.lock().unwrap().take() }
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
                displays: vec![],
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
            key_check: None,
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
            key_check: None,
            probe: None,
            discovery: None,
            discovered: vec![],
            edge_wait: None,
            last_cursor: None,
            armed: Vec::new(),
            edge_hit: None,
            capture_block: None,
            capture_backoff: Duration::ZERO,
            outward: false,
            motion: (0.0, 0.0),
            pass_side: None,
            started: Instant::now(),
            last_write: Instant::now() - Duration::from_secs(1),
            written: None,
            written_permissions: None,
            readiness_error: None,
            my_displays: vec![],
            displays_checked: None,
            displays_rev: 0,
            displays_told: HashMap::new(),
            peer_displays: HashMap::new(),
            down_since: HashMap::new(),
            seen_addrs: HashMap::new(),
            peer_paused_until: HashMap::new(),
            retired: HashMap::new(),
            pair_dial: None,
            bind_failed: false,
            last_bind_try: None,
        };
        Rig { c: control, p, dir }
    }
    const MASTER: &str = "aaaaaaaaaaaaaaaa";
    const SUB: &str = "bbbbbbbbbbbbbbbb";
    const OTHER: &str = "cccccccccccccccc";
    const THIRD: &str = "dddddddddddddddd";
    fn peer(id: &str, role: Role) -> PeerConfig {
        PeerConfig { id: id.into(), fingerprint: id.repeat(4), role, os: Os::Mac, ..PeerConfig::default() }
    }
    fn link(c: &mut Control, id: &str, role: Role) -> queue::Receiver {
        let (tx, rx) = queue::channel();
        c.event(Event::Connected(id.into(), Link {
            token: 1, tx, cancel: Arc::new(AtomicBool::new(false)), name: id.into(), os: Os::Mac,
            role, features: proto::FEATURES.iter().map(|s| s.to_string()).collect(), fp: id.repeat(4),
            addr: "127.0.0.1:47160".parse().unwrap(), listen_port: None, outgoing: true, paired: false,
            last_rx: Instant::now(), up_since: Instant::now(), last_ping: Instant::now(), ping: None, rtt: None,
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
    fn controlling(c: &mut Control, id: &str) { c.state = State::Controlling { peer: id.into(), side: Side::Right, pos: 0.5, display: None }; }
    fn exit_at(pos: f64) -> Exit { Exit { display: 0, target: Some(0), pos, whole: pos } }
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
        for _ in 0..100 { r.c.cross_to(SUB.into(), Side::Right, exit_at(0.5)); }
        assert_eq!(calls(), 1);
        assert!(error(&r.c).unwrap().contains("tap denied"));
        assert!(matches!(r.c.state, State::Local));
        // A different neighbour obeys the same cooldown.
        r.c.cross_to(OTHER.into(), Side::Left, exit_at(0.5));
        assert_eq!(calls(), 1);
        // The cooldown doubles while it keeps failing, capped at 30 s.
        let mut expect = [4, 8, 16, 30, 30].into_iter();
        r.c.capture_block = Some(Instant::now());
        r.c.cross_to(SUB.into(), Side::Right, exit_at(0.5));
        assert_eq!(calls(), 2);
        assert_eq!(r.c.capture_backoff, Duration::from_secs(expect.next().unwrap()));
        r.c.capture_block = Some(Instant::now());
        r.c.cross_to(OTHER.into(), Side::Left, exit_at(0.5));
        assert_eq!(calls(), 3);
        assert_eq!(r.c.capture_backoff, Duration::from_secs(expect.next().unwrap()));
        assert!(error(&r.c).is_some());
        // The next attempt succeeds: the error clears and the backoff resets.
        r.c.capture_block = Some(Instant::now());
        r.c.cross_to(SUB.into(), Side::Right, exit_at(0.5));
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
            r.c.cross_to(SUB.into(), Side::Right, exit_at(0.5));
        }
        assert_eq!(r.c.capture_backoff, Duration::from_secs(30));
        // Cursor away from any edge after the cooldown: the next failure starts at 2 s again.
        r.c.capture_block = Some(Instant::now());
        r.c.poll_cursor();
        assert_eq!(r.c.capture_backoff, Duration::ZERO);
        r.c.cross_to(SUB.into(), Side::Right, exit_at(0.5));
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
    fn the_toggle_for_a_mac_receiver_is_lang1_only_when_it_advertises_the_feature() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link(&mut r.c, SUB, Role::Sub);
        let taps = |rx: &queue::Receiver| -> Vec<u16> {
            rx.try_iter().filter_map(|m| match m { Msg::Input(InputEvent::Key { usage, down: true }) => Some(usage), _ => None }).collect()
        };
        controlling(&mut r.c, SUB);
        r.c.capture_event(key(keymap::LANG1, true));
        r.c.capture_event(key(keymap::LANG1, false));
        assert_eq!(taps(&rx), vec![keymap::LANG1], "2.2 receiver");
        // A 2.1 mac receiver has no such feature: Caps Lock, as before.
        r.c.links.get_mut(SUB).unwrap().features = vec!["group".into(), "keyrules".into(), "displays".into()];
        controlling(&mut r.c, SUB);
        r.c.capture_event(key(keymap::LANG1, true));
        r.c.capture_event(key(keymap::LANG1, false));
        assert_eq!(taps(&rx), vec![0x39], "2.1 receiver");
    }
    #[test]
    fn returning_to_a_monitor_that_is_gone_uses_the_whole_edge() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        two_monitors(&r);
        r.c.state = State::Controlling { peer: SUB.into(), side: Side::Right, pos: 0.5, display: Some(5) };
        r.c.local(false);
        let expected = geometry::edge_position(&r.p.displays(), Side::Right, 0.5).unwrap();
        assert_eq!(r.p.warps.lock().unwrap().last().copied(), Some(expected));
        // The default per-monitor placement falls back the same way for an unknown monitor.
        r.p.warps.lock().unwrap().clear();
        r.p.place_on_return_display(Side::Left, 9, 0.25);
        assert_eq!(r.p.warps.lock().unwrap().last().copied(), geometry::edge_position(&r.p.displays(), Side::Left, 0.25));
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
        for expected in [a, saved, b, a, saved, b] {
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
        r.c.config.layout = vec![Cell::new(&me, 2, 1), Cell::new(SUB, 3, 1)];
        r.c.request(Request::Unpair { unpair: SUB.into() });
        assert!(r.c.config.layout.iter().all(|c| c.id != SUB));
        // Even if the app still hands over the old grid, the orphan is not shown.
        r.c.config.layout.push(Cell::new(SUB, 3, 1));
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
        // Every closing link sends ReleaseAll before its Bye: that is no working session.
        message(&mut r.c, MASTER, Msg::ReleaseAll);
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
            last_rx: Instant::now(), up_since: Instant::now(), last_ping: Instant::now(), ping: None, rtt: None,
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
            last_rx: Instant::now(), up_since: Instant::now(), last_ping: Instant::now(), ping: None, rtt: None,
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

    // ---- 2.2: monitors, pause, reconnecting ------------------------------------------

    fn big(x: f64) -> Rect { Rect::new(x, 0.0, 1000.0, 1000.0) }
    /// A link whose peer advertises exactly `features`.
    fn link_features(c: &mut Control, id: &str, role: Role, features: &[&str]) -> queue::Receiver {
        let rx = link(c, id, role);
        c.links.get_mut(id).unwrap().features = features.iter().map(|s| s.to_string()).collect();
        c.peer_features.insert(id.into(), features.iter().map(|s| s.to_string()).collect());
        rx
    }
    fn two_monitors(r: &Rig) { *r.p.rects.lock().unwrap() = vec![big(1000.0), big(0.0)]; }
    fn sent(rx: &queue::Receiver) -> Vec<Msg> { rx.try_iter().collect() }

    #[test]
    fn monitors_go_only_to_peers_with_the_feature_and_again_when_they_change() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        r.c.config.peers.push(peer(OTHER, Role::Sub));
        let new = link_features(&mut r.c, SUB, Role::Sub, &["group", "keyrules", "displays"]);
        let old = link_features(&mut r.c, OTHER, Role::Sub, &["group", "keyrules"]);
        two_monitors(&r);
        r.c.refresh_displays();
        let expected = vec![big(0.0), big(1000.0)];
        assert_eq!(r.c.my_displays, expected, "sorted by x");
        r.c.tell_displays();
        assert!(sent(&new).contains(&Msg::Displays { rects: expected.clone() }));
        assert!(!sent(&old).iter().any(|m| matches!(m, Msg::Displays { .. })), "a 2.1 peer never gets it");
        // Nothing changed: not again.
        r.c.tell_displays();
        assert!(sent(&new).is_empty());
        // Changed (polled at most every 2 s).
        *r.p.rects.lock().unwrap() = vec![big(0.0)];
        r.c.refresh_displays();
        assert_eq!(r.c.my_displays.len(), 2, "not polled again within 2 s");
        r.c.displays_checked = Some(Instant::now() - DISPLAYS_POLL);
        r.c.refresh_displays();
        r.c.tell_displays();
        assert_eq!(sent(&new), vec![Msg::Displays { rects: vec![big(0.0)] }]);
        // A new link to the same peer is told again.
        r.c.disconnect(SUB, "test");
        let again = link_features(&mut r.c, SUB, Role::Sub, &["displays"]);
        r.c.tell_displays();
        assert_eq!(sent(&again), vec![Msg::Displays { rects: vec![big(0.0)] }]);
        // Status carries ours.
        r.c.publish(true);
        assert_eq!(r.c.status.lock().unwrap().me.displays, vec![big(0.0)]);
    }
    #[test]
    fn a_peers_monitors_are_kept_sorted_validated_and_shown_in_the_status() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        let bad = Rect::new(0.0, 0.0, f64::NAN, 10.0);
        message(&mut r.c, SUB, Msg::Displays { rects: vec![big(1000.0), bad, big(0.0)] });
        assert_eq!(r.c.peer_displays[SUB], vec![big(0.0), big(1000.0)]);
        r.c.publish(true);
        assert_eq!(r.c.status.lock().unwrap().peers[0].displays, vec![big(0.0), big(1000.0)]);
        // The list survives a blip of the link, and an empty list forgets it.
        r.c.disconnect(SUB, "blip");
        assert!(r.c.peer_displays.contains_key(SUB));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        message(&mut r.c, SUB, Msg::Displays { rects: vec![] });
        assert!(!r.c.peer_displays.contains_key(SUB));
        // Not kept for a computer that is no longer known.
        message(&mut r.c, SUB, Msg::Displays { rects: vec![big(0.0)] });
        r.c.config.peers.clear();
        r.c.prune_peer_state();
        assert!(r.c.peer_displays.is_empty());
    }
    #[test]
    fn cells_follow_the_monitors_of_this_computer_and_of_its_peers() {
        let mut r = rig(Role::Master);
        let me = r.c.identity.id.clone();
        r.c.config.peers.push(peer(SUB, Role::Sub));
        r.c.config.layout = vec![Cell::new(&me, 2, 1), Cell::new(SUB, 3, 1)];
        let at = |cells: &[Cell], id: &str, d: u8| cells.iter().find(|c| c.id == id && c.display == d).map(|c| (c.col, c.row));
        // One monitor each: as saved.
        let cells = r.c.my_cells();
        assert_eq!((at(&cells, &me, 0), at(&cells, SUB, 0), cells.len()), (Some((2, 1)), Some((3, 1)), 2));
        // This computer has two monitors side by side and the peer is on its right: the right
        // monitor keeps the old cell (it faces the peer) and the left one takes the free spot
        // to its left.
        r.c.my_displays = vec![big(0.0), big(1000.0)];
        let cells = r.c.my_cells();
        assert_eq!((at(&cells, &me, 0), at(&cells, &me, 1)), (Some((1, 1)), Some((2, 1))));
        // The peer reports two monitors one above the other.
        r.c.peer_displays.insert(SUB.into(), vec![big(0.0), Rect::new(0.0, 1000.0, 1000.0, 1000.0)]);
        let cells = r.c.my_cells();
        assert_eq!(at(&cells, SUB, 1), Some((3, 2)));
        assert_eq!(layout::valid_cells(&cells), cells);
        // The monitor leaves again: its cell is not shown, but stays in the saved layout.
        r.c.config.layout = cells.clone();
        r.c.my_displays = vec![big(0.0)];
        r.c.peer_displays.insert(SUB.into(), vec![big(0.0)]);
        let cells = r.c.my_cells();
        assert!(cells.iter().all(|c| c.display == 0), "{cells:?}");
        assert_eq!(r.c.config.layout.iter().filter(|c| c.display == 1).count(), 2);
        // A peer that has not told its monitors (yet) keeps every cell it was given.
        r.c.peer_displays.clear();
        assert!(r.c.my_cells().iter().any(|c| c.id == SUB && c.display == 1));
        // A connected 2.1 peer has exactly one.
        let _rx = link_features(&mut r.c, SUB, Role::Sub, &["group", "keyrules"]);
        assert!(r.c.my_cells().iter().all(|c| c.id != SUB || c.display == 0));
    }
    #[test]
    fn the_cursor_crosses_at_the_monitor_that_has_the_neighbour_and_enters_the_matching_one() {
        let mut r = rig(Role::Master);
        let me = r.c.identity.id.clone();
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link_features(&mut r.c, SUB, Role::Sub, &["group", "keyrules", "displays"]);
        two_monitors(&r);
        r.c.refresh_displays();
        // Two monitors side by side; the peer is right of the second one and its second monitor.
        r.c.config.layout = vec![Cell::of_display(&me, 0, 1, 1), Cell::of_display(&me, 1, 2, 1), Cell::new(SUB, 3, 1)];
        r.c.peer_displays.insert(SUB.into(), vec![big(0.0), big(1000.0)]);
        r.c.config.layout.push(Cell::of_display(SUB, 1, 3, 2));
        // The seam between the two monitors, the left edge and the right edge of the first
        // monitor are no crossings.
        for at in [(999.0, 500.0), (1000.0, 500.0), (0.0, 500.0)] {
            *r.p.cursor_at.lock().unwrap() = Some(at);
            r.c.poll_cursor();
            assert!(matches!(r.c.state, State::Local), "{at:?}");
        }
        assert_eq!(*r.p.armed.lock().unwrap(), vec![Side::Right]);
        // The right edge of the second monitor, halfway down.
        *r.p.cursor_at.lock().unwrap() = Some((1999.0, 250.0));
        r.c.poll_cursor();
        let State::Controlling { side, pos, display, .. } = r.c.state.clone() else { panic!("not crossing") };
        assert_eq!((side, display), (Side::Right, Some(1)));
        assert!((pos - 250.0 / 999.0).abs() < 1e-9);
        let enter = sent(&rx).into_iter().find(|m| matches!(m, Msg::Enter { .. })).unwrap();
        assert_eq!(enter, Msg::Enter { side: Side::Right, pos: 250.0 / 999.0, display: Some(0) });
        // Back: the peer sends Leave with our monitor; we land on that monitor's right edge.
        message(&mut r.c, SUB, Msg::Leave { pos: 0.5, display: Some(0), side: None });
        assert!(matches!(r.c.state, State::Local));
        let (x, y) = *r.p.warps.lock().unwrap().last().unwrap();
        assert_eq!((x, y), (997.0, 499.5));
        // A Leave without a monitor (2.1 peer) is a position along the whole right edge,
        // which is the second monitor's.
        r.p.warps.lock().unwrap().clear();
        *r.p.cursor_at.lock().unwrap() = Some((1999.0, 250.0));
        r.c.state = State::Controlling { peer: SUB.into(), side: Side::Right, pos: 0.1, display: Some(1) };
        message(&mut r.c, SUB, Msg::Leave { pos: 1.0, display: None, side: None });
        assert_eq!(r.p.warps.lock().unwrap().last().copied(), Some((1997.0, 999.0)));
    }
    #[test]
    fn a_peer_without_the_displays_feature_gets_the_whole_edge_position() {
        let mut r = rig(Role::Master);
        let me = r.c.identity.id.clone();
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link_features(&mut r.c, SUB, Role::Sub, &["group", "keyrules"]);
        two_monitors(&r);
        r.c.refresh_displays();
        r.c.config.layout = vec![Cell::of_display(&me, 0, 1, 1), Cell::of_display(&me, 1, 2, 1), Cell::new(SUB, 3, 1)];
        // Top edge of the second monitor: no neighbour there, so nothing happens.
        *r.p.cursor_at.lock().unwrap() = Some((1500.0, 0.0));
        r.c.poll_cursor();
        assert!(matches!(r.c.state, State::Local));
        *r.p.cursor_at.lock().unwrap() = Some((1999.0, 500.0));
        r.c.poll_cursor();
        let enter = sent(&rx).into_iter().find(|m| matches!(m, Msg::Enter { .. })).unwrap();
        // Only the second monitor has a right edge, so whole and monitor positions agree here.
        assert_eq!(enter, Msg::Enter { side: Side::Right, pos: 500.0 / 999.0, display: None });
        // Staggered: the peer's whole-edge position differs from the monitor's.
        r.c.local(true);
        let _ = sent(&rx);
        *r.p.rects.lock().unwrap() = vec![big(0.0), Rect::new(1000.0, 500.0, 1000.0, 1000.0)];
        r.c.displays_checked = None;
        r.c.refresh_displays();
        *r.p.cursor_at.lock().unwrap() = Some((1999.0, 1000.0));
        r.c.poll_cursor();
        let enter = sent(&rx).into_iter().find(|m| matches!(m, Msg::Enter { .. })).unwrap();
        let Msg::Enter { pos, display, .. } = enter else { panic!() };
        assert_eq!(display, None);
        // The right edge's exposed parts: monitor 0 (0..999) is covered by nothing on its right
        // side? it is not (monitor 1 starts at x = 1000 and spans y 500..1499), so the whole
        // exposed right edge is y 0..499 of monitor 0 and 500..1499 of monitor 1.
        assert!((pos - (499.0 + 500.0) / (499.0 + 999.0)).abs() < 0.01, "{pos}");
    }
    #[test]
    fn a_platform_edge_hit_is_resolved_to_the_monitor_and_handed_back_when_nothing_is_there() {
        let mut r = rig(Role::Master);
        let me = r.c.identity.id.clone();
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link_features(&mut r.c, SUB, Role::Sub, &["group", "keyrules", "displays"]);
        two_monitors(&r);
        r.c.refresh_displays();
        r.p.blind.store(true, Ordering::Relaxed);
        // The peer is above the first monitor only.
        r.c.config.layout = vec![Cell::of_display(&me, 0, 1, 1), Cell::of_display(&me, 1, 2, 1), Cell::new(SUB, 1, 0)];
        // A hit on the top edge over the second monitor (the right 90 % of the edge): nothing
        // is there, the capture is released at once.
        *r.p.hit.lock().unwrap() = Some((Side::Top, 0.9));
        let stopped = r.p.stopped.load(Ordering::Relaxed);
        r.c.poll_cursor();
        assert!(matches!(r.c.state, State::Local));
        assert!(r.c.edge_hit.is_none());
        assert!(r.p.stopped.load(Ordering::Relaxed) > stopped);
        assert!(!sent(&rx).iter().any(|m| matches!(m, Msg::Enter { .. })));
        // Over the first monitor: control moves, with the position along that monitor's edge.
        *r.p.hit.lock().unwrap() = Some((Side::Top, 0.25));
        r.c.poll_cursor();
        assert!(matches!(r.c.state, State::Controlling { display: Some(0), .. }), "{:?}", r.c.state.peer());
        let Some(Msg::Enter { side, pos, display }) = sent(&rx).into_iter().find(|m| matches!(m, Msg::Enter { .. })) else { panic!() };
        assert_eq!((side, display), (Side::Top, Some(0)));
        // The exposed top edges are x 0..999 and 1000..1999 (1998 long in all): a quarter is
        // x = 499.5, the middle of the first monitor's edge.
        assert!((pos - 0.5).abs() < 1e-9, "{pos}");
    }
    #[test]
    fn app_driven_crossing_finds_the_monitor_from_the_whole_edge_position() {
        let mut r = rig(Role::Master);
        let me = r.c.identity.id.clone();
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link_features(&mut r.c, SUB, Role::Sub, &["displays"]);
        two_monitors(&r);
        r.c.refresh_displays();
        r.c.config.layout = vec![Cell::of_display(&me, 0, 1, 1), Cell::of_display(&me, 1, 2, 1), Cell::new(SUB, 3, 1)];
        r.c.cross_on_request(Side::Left, 0.5);
        assert!(matches!(r.c.state, State::Local), "monitor 0 has nothing on its left");
        r.c.cross_on_request(Side::Right, 0.5);
        assert!(matches!(r.c.state, State::Controlling { display: Some(1), .. }));
        assert!(sent(&rx).iter().any(|m| matches!(m, Msg::Enter { side: Side::Right, display: Some(0), .. })));
    }
    #[test]
    fn enter_lands_on_the_named_monitor_and_without_one_on_the_whole_edge() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link(&mut r.c, SUB, Role::Sub);
        two_monitors(&r);
        let warp = |r: &Rig| *r.p.warps.lock().unwrap().last().unwrap();
        // The peer left over its left side: this computer is entered over its right edge.
        message(&mut r.c, SUB, Msg::Enter { side: Side::Left, pos: 0.5, display: Some(0) });
        assert_eq!(warp(&r), (997.0, 499.5));
        assert!(matches!(r.c.state, State::Controlled { entered: Side::Right, .. }));
        r.c.local(false);
        message(&mut r.c, SUB, Msg::Enter { side: Side::Left, pos: 0.0, display: Some(1) });
        assert_eq!(warp(&r), (1997.0, 0.0));
        r.c.local(false);
        // Top edge, second monitor, near its end.
        message(&mut r.c, SUB, Msg::Enter { side: Side::Bottom, pos: 1.0, display: Some(1) });
        assert_eq!(warp(&r), (1999.0, 2.0));
        r.c.local(false);
        // No monitor number (2.1) or one that does not exist: the whole edge.
        message(&mut r.c, SUB, Msg::Enter { side: Side::Left, pos: 1.0, display: None });
        assert_eq!(warp(&r), (1997.0, 999.0));
        r.c.local(false);
        message(&mut r.c, SUB, Msg::Enter { side: Side::Left, pos: 1.0, display: Some(7) });
        assert_eq!(warp(&r), (1997.0, 999.0));
        r.c.local(false);
        // A single monitor ignores the number's effect: same point either way.
        *r.p.rects.lock().unwrap() = vec![];
        message(&mut r.c, SUB, Msg::Enter { side: Side::Left, pos: 0.5, display: Some(0) });
        assert_eq!(warp(&r), (97.0, 49.5));
    }
    #[test]
    fn a_controlled_computer_goes_back_only_over_the_monitor_that_has_the_peer_next_to_it() {
        let mut r = rig(Role::Master);
        let me = r.c.identity.id.clone();
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let rx = link_features(&mut r.c, SUB, Role::Sub, &["group", "keyrules", "displays"]);
        two_monitors(&r);
        r.c.refresh_displays();
        // The peer sits left of our first monitor and its second monitor is the one there.
        r.c.config.layout = vec![Cell::new(SUB, 0, 1), Cell::of_display(SUB, 1, 0, 2), Cell::of_display(&me, 0, 1, 1), Cell::of_display(&me, 1, 2, 1)];
        r.c.peer_displays.insert(SUB.into(), vec![big(0.0), big(1000.0)]);
        // Cursor at the left edge of the second monitor is impossible (the first is there), so the
        // test uses the left edge of the first, which has the peer.
        r.c.state = State::Controlled { peer: SUB.into(), entered: Side::Left };
        r.c.outward = true;
        *r.p.cursor_at.lock().unwrap() = Some((0.0, 400.0));
        r.c.poll_cursor();
        assert!(matches!(r.c.state, State::Local));
        let leave = sent(&rx).into_iter().find(|m| matches!(m, Msg::Leave { .. })).unwrap();
        assert_eq!(leave, Msg::Leave { pos: 400.0 / 999.0, display: Some(0), side: None });
        // Coming in on the right side: the second monitor has no peer next to it, so the cursor
        // stays on this computer.
        r.c.state = State::Controlled { peer: SUB.into(), entered: Side::Right };
        r.c.outward = true;
        *r.p.cursor_at.lock().unwrap() = Some((1999.0, 400.0));
        r.c.poll_cursor();
        assert!(matches!(r.c.state, State::Controlled { .. }));
        assert!(!sent(&rx).iter().any(|m| matches!(m, Msg::Leave { .. })));
        // With one monitor any edge it came in over leads back, as before 2.2; an older peer
        // gets the whole-edge position without a monitor.
        *r.p.rects.lock().unwrap() = vec![];
        r.c.my_displays = vec![];
        r.c.links.get_mut(SUB).unwrap().features = vec!["group".into()];
        r.c.config.layout.clear();
        *r.p.cursor_at.lock().unwrap() = Some((99.0, 50.0));
        r.c.state = State::Controlled { peer: SUB.into(), entered: Side::Right };
        r.c.config.corner_margin = 0.0;
        r.c.outward = true;
        r.c.poll_cursor();
        assert!(matches!(r.c.state, State::Local));
        assert!(sent(&rx).iter().any(|m| matches!(m, Msg::Leave { display: None, .. })));
    }

    #[test]
    fn a_monitor_without_a_cell_crosses_with_the_computer_level_neighbour_and_the_whole_edge() {
        let mut r = rig(Role::Master);
        let me = r.c.identity.id.clone();
        r.c.config.peers.push(peer(SUB, Role::Sub));
        r.c.config.peers.push(peer(OTHER, Role::Sub));
        r.c.config.peers.push(peer(THIRD, Role::Sub));
        let rx = link_features(&mut r.c, SUB, Role::Sub, &["group", "keyrules", "displays", "lang1-toggle"]);
        two_monitors(&r);
        r.c.refresh_displays();
        // Both sides of the one cell are taken, so the second monitor has no spot of its own.
        // The peer's second monitor (below its first) has no free spot either.
        r.c.config.layout = vec![Cell::new(&me, 2, 1), Cell::new(SUB, 3, 1), Cell::new(OTHER, 1, 1), Cell::new(THIRD, 3, 2)];
        r.c.peer_displays.insert(SUB.into(), vec![big(0.0), Rect::new(0.0, 1000.0, 1000.0, 1000.0)]);
        let cells = r.c.my_cells();
        assert_eq!(cells.iter().filter(|c| c.id == me || c.id == SUB).count(), 2, "{cells:?}");
        // The right edge of the second monitor still crosses to the neighbour of the cell. The
        // peer reports two monitors but has one cell: no monitor is named.
        *r.p.cursor_at.lock().unwrap() = Some((1999.0, 250.0));
        r.c.poll_cursor();
        assert!(matches!(r.c.state, State::Controlling { display: Some(1), .. }), "{:?}", r.c.state.peer());
        let enter = sent(&rx).into_iter().find(|m| matches!(m, Msg::Enter { .. })).unwrap();
        assert_eq!(enter, Msg::Enter { side: Side::Right, pos: 250.0 / 999.0, display: None });
        // A peer with one monitor has a real cell: it is named.
        r.c.local(true);
        let _ = sent(&rx);
        r.c.peer_displays.insert(SUB.into(), vec![big(0.0)]);
        r.c.cross_on_request(Side::Right, 0.5);
        let enter = sent(&rx).into_iter().find(|m| matches!(m, Msg::Enter { .. })).unwrap();
        assert!(matches!(enter, Msg::Enter { side: Side::Right, display: Some(0), .. }), "{enter:?}");
    }
    #[test]
    fn a_master_keeps_the_monitors_a_sub_told_it_and_uses_them_while_it_is_away() {
        let mut r = rig(Role::Master);
        let changes = recorded_peer_changes(&mut r);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        let _rx = link_features(&mut r.c, SUB, Role::Sub, &["group", "keyrules", "displays"]);
        message(&mut r.c, SUB, Msg::Displays { rects: vec![big(1000.0), big(0.0)] });
        let saved = changes.lock().unwrap().last().cloned().expect("saved");
        assert_eq!(saved.displays, vec![big(0.0), big(1000.0)]);
        // The same again is not saved again.
        let n = changes.lock().unwrap().len();
        message(&mut r.c, SUB, Msg::Displays { rects: vec![big(0.0), big(1000.0)] });
        assert_eq!(changes.lock().unwrap().len(), n);
        // A new start knows them from the config before the sub says anything.
        let mut fresh = rig(Role::Master);
        fresh.c.config.peers.push(saved);
        fresh.c.seed_peer_displays();
        assert_eq!(fresh.c.display_count(SUB), Some(2));
    }

    #[test]
    fn a_wrapped_computer_goes_back_over_any_edge_that_touches_the_controllers_monitor() {
        let mut r = rig(Role::Sub);
        let me = r.c.identity.id.clone();
        r.c.config.peers.push(peer(MASTER, Role::Master));
        let rx = link_features(&mut r.c, MASTER, Role::Master, &["group", "keyrules", "displays", "pass"]);
        *r.p.rects.lock().unwrap() = vec![big(0.0)];
        r.c.refresh_displays();
        // The master's monitors wrap this computer like an L: monitor 0 above it, monitor 1 left of it.
        r.c.group = Some((vec![Cell::of_display(MASTER, 0, 1, 0), Cell::of_display(MASTER, 1, 0, 1), Cell::new(&me, 1, 1)], vec![]));
        r.c.peer_displays.insert(MASTER.into(), vec![big(0.0), big(1000.0)]);
        // It came in over the top; pushed out over the left edge it goes back to monitor 1.
        r.c.state = State::Controlled { peer: MASTER.into(), entered: Side::Top };
        r.c.motion = (-5.0, 0.0);
        *r.p.cursor_at.lock().unwrap() = Some((0.0, 400.0));
        r.c.poll_cursor();
        assert!(matches!(r.c.state, State::Local));
        let leave = sent(&rx).into_iter().find(|m| matches!(m, Msg::Leave { .. })).unwrap();
        assert_eq!(leave, Msg::Leave { pos: 400.0 / 999.0, display: Some(1), side: Some(Side::Right) });
        // Moving the other way (inward) at that edge leaves nothing.
        r.c.state = State::Controlled { peer: MASTER.into(), entered: Side::Top };
        r.c.motion = (5.0, 0.0);
        r.c.last_cursor = Some((0.0, 400.0));
        r.c.poll_cursor();
        assert!(matches!(r.c.state, State::Controlled { .. }));
        // The bottom edge has nothing next to it: it stays.
        r.c.motion = (0.0, 5.0);
        *r.p.cursor_at.lock().unwrap() = Some((500.0, 999.0));
        r.c.poll_cursor();
        assert!(matches!(r.c.state, State::Controlled { .. }));
    }

    #[test]
    fn a_sub_asks_for_the_key_check_and_its_keys_show_on_the_master_as_each_computer_gets_them() {
        // The sub: the button asks the master and holds its own keyboard; Escape lets go.
        let mut s = rig(Role::Sub);
        s.c.config.peers.push(peer(MASTER, Role::Master));
        let to_master = link_features(&mut s.c, MASTER, Role::Master, &["group", "keyrules", "keycheck"]);
        s.c.request(Request::Action { action: "keycheck_open".into() });
        assert_eq!(sent(&to_master), vec![Msg::KeyCheckOpen]);
        assert!(s.c.probe.is_some());
        s.c.poll_cursor();
        assert!(s.c.probe.is_some(), "no crossing while held");
        s.c.capture_event(InputEvent::Key { usage: 0xe3, down: true });
        s.c.capture_event(InputEvent::Motion { dx: 5.0, dy: 0.0 });
        assert_eq!(sent(&to_master), vec![Msg::KeyProbe { usage: 0xe3, down: true }], "keys only");
        s.c.capture_event(InputEvent::Key { usage: 0x29, down: true });
        assert!(s.c.probe.is_none());
        assert_eq!(sent(&to_master), vec![Msg::KeyProbe { usage: 0xe3, down: false }], "a held key is let go, Escape is not sent");
        // The window closing on the master ends a check still going on.
        s.c.request(Request::Action { action: "keycheck_probe".into() });
        assert!(s.c.probe.is_some());
        message(&mut s.c, MASTER, Msg::KeyCheckEnd);
        assert!(s.c.probe.is_none());

        // The master: a Mac sub's Command shows as Ctrl here (Windows) and on a Linux sub.
        let mut m = rig(Role::Master);
        m.c.config.name = "Win".into();
        let opened = Arc::new(AtomicUsize::new(0));
        let count = opened.clone();
        m.c.hooks.open_key_check = Box::new(move || { count.fetch_add(1, Ordering::Relaxed); });
        let mut mac = peer(SUB, Role::Sub);
        mac.os = Os::Mac;
        mac.name = "Mac".into();
        let mut linux = peer(OTHER, Role::Sub);
        linux.os = Os::Linux;
        linux.name = "Linux".into();
        m.c.config.peers.extend([mac, linux]);
        let to_sub = link_features(&mut m.c, SUB, Role::Sub, &["group", "keyrules", "keycheck"]);
        message(&mut m.c, SUB, Msg::KeyProbe { usage: 0xe3, down: true });
        assert!(m.c.key_check.is_none(), "nothing is logged without the window");
        message(&mut m.c, SUB, Msg::KeyCheckOpen);
        assert_eq!(opened.load(Ordering::Relaxed), 1);
        message(&mut m.c, SUB, Msg::KeyProbe { usage: 0xe3, down: true });
        let entries = m.c.status.lock().unwrap().key_check.clone().unwrap().entries;
        let e = entries.last().unwrap();
        assert_eq!((e.from.as_str(), e.key.as_str()), ("Mac", "Command"));
        let got = |name: &str| e.to.iter().find(|t| t.name == name).map(|t| t.keys.clone());
        assert_eq!(got("Linux").as_deref(), Some("Ctrl"));
        assert_eq!(got("Win").as_deref(), Some("Ctrl"), "the master itself");
        assert_eq!(got("Mac"), None, "not the pressing computer");
        // Closing the window tells the subs.
        m.c.request(Request::Action { action: "keycheck_close".into() });
        assert!(m.c.key_check.is_none());
        assert!(sent(&to_sub).contains(&Msg::KeyCheckEnd));
        assert!(m.c.status.lock().unwrap().key_check.is_none());
    }

    #[test]
    fn a_controlled_computer_asks_its_controller_to_pass_control_to_the_computer_next_to_it() {
        let mut r = rig(Role::Sub);
        let me = r.c.identity.id.clone();
        r.c.config.peers.push(peer(MASTER, Role::Master));
        let rx = link_features(&mut r.c, MASTER, Role::Master, &["group", "keyrules", "displays", "pass"]);
        *r.p.rects.lock().unwrap() = vec![big(0.0)];
        r.c.refresh_displays();
        // The master above, another sub on the left.
        r.c.group = Some((vec![Cell::new(MASTER, 1, 0), Cell::new(OTHER, 0, 1), Cell::new(&me, 1, 1)], vec![]));
        r.c.state = State::Controlled { peer: MASTER.into(), entered: Side::Top };
        r.c.motion = (-5.0, 0.0);
        *r.p.cursor_at.lock().unwrap() = Some((0.0, 400.0));
        r.c.poll_cursor();
        let pass = sent(&rx).into_iter().find(|m| matches!(m, Msg::Pass { .. })).expect("pass asked");
        assert!(matches!(pass, Msg::Pass { ref to, side: Side::Left, display: Some(0), .. } if to == OTHER), "{pass:?}");
        // The cursor stays until the master lets go, then stays at the left edge.
        assert!(matches!(r.c.state, State::Controlled { .. }));
        r.c.poll_cursor();
        assert!(!sent(&rx).iter().any(|m| matches!(m, Msg::Pass { .. })), "not repeated at once");
        message(&mut r.c, MASTER, Msg::ReleaseAll);
        assert!(matches!(r.c.state, State::Local));
        assert!(r.p.cursor().unwrap().0 < 1.0, "{:?}", r.p.cursor());
        // A master before 2.4 cannot pass: nothing is asked and the cursor stays.
        r.c.links.get_mut(MASTER).unwrap().features = vec!["group".into(), "displays".into()];
        r.c.state = State::Controlled { peer: MASTER.into(), entered: Side::Top };
        r.c.pass_side = None;
        r.c.poll_cursor();
        assert!(!sent(&rx).iter().any(|m| matches!(m, Msg::Pass { .. } | Msg::Leave { .. })));
        assert!(matches!(r.c.state, State::Controlled { .. }));
    }

    #[test]
    fn the_controller_passes_control_on_to_the_computer_it_was_asked_for() {
        let mut r = rig(Role::Master);
        r.c.config.peers.push(peer(SUB, Role::Sub));
        r.c.config.peers.push(peer(OTHER, Role::Sub));
        let from = link_features(&mut r.c, SUB, Role::Sub, &["group", "keyrules", "displays", "pass"]);
        let to = link_features(&mut r.c, OTHER, Role::Sub, &["group", "keyrules", "displays", "pass"]);
        controlling(&mut r.c, SUB);
        message(&mut r.c, SUB, Msg::Pass { to: OTHER.into(), side: Side::Left, pos: 0.4, whole: 0.3, display: Some(0) });
        assert!(sent(&from).iter().any(|m| matches!(m, Msg::ReleaseAll)));
        assert!(sent(&to).iter().any(|m| matches!(m, Msg::Enter { side: Side::Left, .. })));
        assert!(matches!(&r.c.state, State::Controlling { peer, .. } if peer == OTHER));
        // Only the computer being controlled may ask, and only for an open link.
        message(&mut r.c, SUB, Msg::Pass { to: SUB.into(), side: Side::Right, pos: 0.5, whole: 0.5, display: None });
        assert!(matches!(&r.c.state, State::Controlling { peer, .. } if peer == OTHER));
        message(&mut r.c, OTHER, Msg::Pass { to: THIRD.into(), side: Side::Right, pos: 0.5, whole: 0.5, display: None });
        assert!(matches!(&r.c.state, State::Controlling { peer, .. } if peer == OTHER));
        // A Leave that names a side brings the cursor back at that side.
        message(&mut r.c, OTHER, Msg::Leave { pos: 0.5, display: None, side: Some(Side::Bottom) });
        assert!(matches!(r.c.state, State::Local));
        let (_, y) = *r.p.warps.lock().unwrap().last().unwrap();
        assert!(y > 90.0, "placed at the bottom edge, got y = {y}");
    }

    #[test]
    fn a_controlled_monitor_without_a_cell_may_go_back_like_in_2_1() {
        let mut r = rig(Role::Master);
        let me = r.c.identity.id.clone();
        r.c.config.peers.push(peer(SUB, Role::Sub));
        r.c.config.peers.push(peer(OTHER, Role::Sub));
        r.c.config.peers.push(peer(THIRD, Role::Sub));
        let rx = link_features(&mut r.c, SUB, Role::Sub, &["group", "keyrules", "displays"]);
        two_monitors(&r);
        r.c.refresh_displays();
        r.c.config.layout = vec![Cell::new(&me, 1, 1), Cell::new(SUB, 2, 1), Cell::new(OTHER, 0, 1), Cell::new(THIRD, 2, 2)];
        r.c.peer_displays.insert(SUB.into(), vec![big(0.0), Rect::new(0.0, 1000.0, 1000.0, 1000.0)]);
        // Entered over the right edge; the cursor is at the right edge of the second monitor,
        // which has no cell: the peer is the neighbour of the computer's cell on that side.
        r.c.state = State::Controlled { peer: SUB.into(), entered: Side::Right };
        r.c.outward = true;
        *r.p.cursor_at.lock().unwrap() = Some((1999.0, 400.0));
        r.c.poll_cursor();
        assert!(matches!(r.c.state, State::Local));
        let leave = sent(&rx).into_iter().find(|m| matches!(m, Msg::Leave { .. })).unwrap();
        assert_eq!(leave, Msg::Leave { pos: 400.0 / 999.0, display: None, side: None });
    }
    #[test]
    fn a_peer_that_said_paused_is_left_alone_for_a_minute_unless_it_dials() {
        let mut r = rig(Role::Master);
        let mut p = peer(SUB, Role::Sub);
        p.last_addr = "127.0.0.1:9".into();
        r.c.config.peers.push(p);
        let _rx = link(&mut r.c, SUB, Role::Sub);
        message(&mut r.c, SUB, Msg::Bye { reason: "paused".into() });
        assert!(r.c.links.is_empty());
        let until = r.c.peer_paused_until[SUB];
        assert!(until > Instant::now() + Duration::from_secs(50));
        // Long after, with no back-off left: still not dialled.
        r.c.down_since.insert(SUB.into(), Instant::now() - Duration::from_secs(60));
        r.c.retry.clear();
        r.c.dial_peers();
        assert!(r.c.workers.is_empty());
        // Another goodbye does not cause the quiet time.
        let _rx = link(&mut r.c, SUB, Role::Sub);
        assert!(!r.c.peer_paused_until.contains_key(SUB), "a link from the peer ends it");
        message(&mut r.c, SUB, Msg::Bye { reason: "bye".into() });
        assert!(!r.c.peer_paused_until.contains_key(SUB));
        // The quiet time runs out.
        r.c.peer_paused_until.insert(SUB.into(), Instant::now() - Duration::from_secs(1));
        r.c.down_since.insert(SUB.into(), Instant::now() - Duration::from_secs(60));
        r.c.retry.clear();
        r.c.dial_peers();
        assert_eq!(r.c.workers.len(), 1);
    }
    #[test]
    fn the_paused_bye_after_hello_stays_queued_for_the_closing_link() {
        let mut r = rig(Role::Master);
        let mut p = peer(SUB, Role::Sub);
        p.paused = true;
        r.c.config.peers.push(p);
        let (tx, rx) = queue::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        r.c.event(Event::Connected(SUB.into(), Link {
            token: 1, tx, cancel: cancel.clone(), name: SUB.into(), os: Os::Mac,
            role: Role::Sub, features: vec![], fp: SUB.repeat(4),
            addr: "127.0.0.1:47160".parse().unwrap(), listen_port: None, outgoing: true, paired: false,
            last_rx: Instant::now(), up_since: Instant::now(), last_ping: Instant::now(), ping: None, rtt: None,
            bulk: Arc::new(AtomicUsize::new(0)),
        }, false));
        // The dropped link flags the thread, whose goodbye flush still sends what is queued.
        assert!(cancel.load(Ordering::Acquire));
        let mut conn = rustls::Connection::Server(rustls::ServerConnection::new(crate::tls::server_config(&r.c.identity).unwrap()).unwrap());
        let mut out = Vec::new();
        let mut queued = None;
        flush_goodbye(&mut conn, &mut out, &rx, &mut queued).unwrap();
        assert!(!out.is_empty() && rx.try_recv().is_err());
    }
    // Pause.
    fn recorded_peer_changes(r: &mut Rig) -> Arc<Mutex<Vec<PeerConfig>>> {
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = log.clone();
        r.c.hooks.on_peer_changed = Box::new(move |p| seen.lock().unwrap().push(p));
        log
    }
    #[test]
    fn pausing_closes_the_link_with_bye_paused_keeps_the_pairing_and_resuming_redials() {
        let mut r = rig(Role::Master);
        let mut p = peer(SUB, Role::Sub);
        p.last_addr = "127.0.0.1:9".into();
        r.c.config.peers.push(p);
        let changes = recorded_peer_changes(&mut r);
        let rx = link(&mut r.c, SUB, Role::Sub);
        r.c.request(Request::Pause { pause: SUB.into(), paused: true });
        assert!(r.c.links.is_empty());
        let msgs = sent(&rx);
        assert!(msgs.contains(&Msg::Bye { reason: "paused".into() }), "{msgs:?}");
        assert!(r.c.config.peers[0].paused && !r.c.config.peers[0].fingerprint.is_empty());
        assert!(changes.lock().unwrap().last().unwrap().paused);
        r.c.publish(true);
        assert!(r.c.status.lock().unwrap().peers[0].paused);
        // No dialling while paused, even long after.
        r.c.down_since.insert(SUB.into(), Instant::now() - Duration::from_secs(60));
        r.c.retry.clear();
        r.c.dial_peers();
        assert!(r.c.workers.is_empty());
        // Same request again changes nothing.
        let n = changes.lock().unwrap().len();
        r.c.request(Request::Pause { pause: SUB.into(), paused: true });
        assert_eq!(changes.lock().unwrap().len(), n);
        // Resume: the flag clears, the back-off is gone and the next pass dials.
        r.c.retry.insert(SUB.into(), Retry { attempt: 5, next: Instant::now() + Duration::from_secs(60) });
        r.c.request(Request::Pause { pause: SUB.into(), paused: false });
        assert!(!r.c.config.peers[0].paused && !r.c.retry.contains_key(SUB));
        assert!(!changes.lock().unwrap().last().unwrap().paused);
        r.c.down_since.insert(SUB.into(), Instant::now() - Duration::from_secs(60));
        r.c.dial_peers();
        assert_eq!(r.c.workers.len(), 1);
        // An unknown id and a roster-only peer are ignored.
        r.c.request(Request::Pause { pause: "nobody".into(), paused: true });
        assert_eq!(changes.lock().unwrap().len(), n + 1);
    }
    #[test]
    fn a_link_from_a_paused_computer_is_refused_after_hello() {
        let mut r = rig(Role::Master);
        let mut p = peer(SUB, Role::Sub);
        p.paused = true;
        r.c.config.peers.push(p);
        let rx = link(&mut r.c, SUB, Role::Sub);
        assert!(!r.c.links.contains_key(SUB));
        assert_eq!(sent(&rx), vec![Msg::Bye { reason: "paused".into() }]);
        // Not paused: accepted.
        r.c.config.peers[0].paused = false;
        let _rx = link(&mut r.c, SUB, Role::Sub);
        assert!(r.c.links.contains_key(SUB));
    }
    #[test]
    fn a_paused_computer_is_never_a_crossing_target_and_a_config_pause_ends_the_link() {
        let mut r = rig(Role::Master);
        let me = r.c.identity.id.clone();
        r.c.config.peers.push(peer(SUB, Role::Sub));
        r.c.config.layout = vec![Cell::new(&me, 2, 1), Cell::new(SUB, 3, 1)];
        let rx = link(&mut r.c, SUB, Role::Sub);
        let cells = r.c.my_cells();
        assert_eq!(r.c.neighbour_peer(&cells, 0, Side::Right).map(|p| p.0), Some(SUB.to_string()));
        assert_eq!(r.c.armed_sides(&cells), vec![Side::Right]);
        r.c.config.peers[0].paused = true;
        assert_eq!(r.c.neighbour_peer(&cells, 0, Side::Right), None);
        assert!(r.c.armed_sides(&cells).is_empty());
        // The maintenance pass closes the link of a computer paused through the config.
        r.c.maintenance();
        assert!(r.c.links.is_empty());
        assert!(sent(&rx).contains(&Msg::Bye { reason: "paused".into() }));
        // The app's config snapshot with the flag set does the same at once.
        r.c.config.peers[0].paused = false;
        let rx = link(&mut r.c, SUB, Role::Sub);
        let mut config = r.c.config.clone();
        config.peers[0].paused = true;
        r.c.update(config);
        assert!(r.c.links.is_empty());
        assert!(sent(&rx).contains(&Msg::Bye { reason: "paused".into() }));
        // ... and a snapshot that clears it clears the back-off.
        r.c.retry.insert(SUB.into(), Retry { attempt: 3, next: Instant::now() + Duration::from_secs(30) });
        let mut config = r.c.config.clone();
        config.peers[0].paused = false;
        r.c.update(config);
        assert!(!r.c.retry.contains_key(SUB));
    }
    #[test]
    fn the_larger_id_dials_only_after_a_while_without_a_link() {
        let mut r = rig(Role::Master);
        let smaller = "0000000000000000";
        let mut p = peer(smaller, Role::Sub);
        p.last_addr = "127.0.0.1:9".into();
        r.c.config.peers.push(p);
        r.c.dial_peers();
        assert!(r.c.workers.is_empty(), "the smaller id dials first");
        r.c.down_since.insert(smaller.into(), Instant::now() - FALLBACK_DIAL);
        r.c.dial_peers();
        assert_eq!(r.c.workers.len(), 1, "after FALLBACK_DIAL it helps");
    }

    // Reconnecting.
    fn sa(s: &str) -> SocketAddr { s.parse().unwrap() }
    #[test]
    fn dial_order_prefers_ipv4_then_other_ipv6_then_link_local_and_keeps_scopes() {
        let ll = sa("[fe80::1%4]:47160");
        let ula = sa("[fd00::5]:47160");
        let v4 = sa("192.168.1.20:47160");
        let auto = sa("169.254.8.8:47160");
        let saved = sa("192.168.1.9:47160");
        // Whatever order discovery lists them in.
        for found in [vec![ll, ula, auto, v4], vec![v4, auto, ula, ll], vec![auto, ll, v4, ula]] {
            assert_eq!(dial_order(&found, Some(saved)), vec![v4, saved, auto, ula, ll]);
        }
        // Discovered before saved of the same quality, no duplicates, mapped IPv4 unwrapped.
        assert_eq!(dial_order(&[sa("[::ffff:192.168.1.20]:47160"), v4], Some(v4)), vec![v4]);
        // Not local, or link-local IPv6 without an interface, is dropped.
        assert_eq!(dial_order(&[sa("8.8.8.8:47160"), sa("[fe80::1]:47160")], None), Vec::<SocketAddr>::new());
        // The interface of a link-local address survives.
        let kept = dial_order(&[ll], None);
        assert_eq!(kept, vec![ll]);
        let SocketAddr::V6(v6) = kept[0] else { panic!() };
        assert_eq!(v6.scope_id(), 4);
        assert_eq!(canonical(ll), ll);
        assert_eq!(canonical(sa("[::ffff:10.0.0.2]:5")), sa("10.0.0.2:5"));
        // A saved address written by Display (with its scope) reads back.
        assert_eq!(ll.to_string().parse::<SocketAddr>().unwrap(), ll);
        assert_eq!(pick_candidate(&[], 3), None);
        assert_eq!(pick_candidate(&[v4, ll], 3), Some(ll));
        assert_eq!(pick_candidate(&[v4, ll], 4), Some(v4));
    }
    #[test]
    fn a_worse_inbound_address_does_not_replace_a_better_saved_one() {
        assert!(prefer_saved_addr("192.168.1.9:47160", sa("192.168.1.20:47160")));
        assert!(!prefer_saved_addr("192.168.1.9:47160", sa("[fe80::1%4]:47160")));
        assert!(!prefer_saved_addr("192.168.1.9:47160", sa("[fd00::1]:47160")));
        assert!(prefer_saved_addr("[fe80::1%4]:47160", sa("192.168.1.9:47160")));
        assert!(prefer_saved_addr("[fe80::1%4]:47160", sa("[fe80::2%4]:47160")));
        assert!(prefer_saved_addr("", sa("[fe80::1%4]:47160")));
        assert!(prefer_saved_addr("not an address", sa("[fe80::1%4]:47160")));
        assert!(prefer_saved_addr("8.8.8.8:47160", sa("[fe80::1%4]:47160")));
    }
    #[test]
    fn a_computer_that_shows_up_again_is_noticed() {
        let found = |id: &str, a: &[&str]| DiscoveredPeer { id: id.into(), name: id.into(), os: Os::Mac, fingerprint: id.repeat(4),
            addresses: a.iter().map(|s| sa(s)).collect(), port: 47160, seen: Instant::now() };
        let mut seen = HashMap::new();
        let list = [found("a", &["10.0.0.2:47160", "[fe80::1%4]:47160"]), found("b", &["10.0.0.3:47160"])];
        assert_eq!(track_discovered(&mut seen, &list), ["a", "b"]);
        // Same addresses in another order: not news.
        let list = [found("a", &["[fe80::1%4]:47160", "10.0.0.2:47160"]), found("b", &["10.0.0.3:47160"])];
        assert!(track_discovered(&mut seen, &list).is_empty());
        // New address (DHCP change) is news; b going away and coming back is too.
        let list = [found("a", &["10.0.0.9:47160"])];
        assert_eq!(track_discovered(&mut seen, &list), ["a"]);
        assert!(track_discovered(&mut seen, &list).is_empty());
        let list = [found("a", &["10.0.0.9:47160"]), found("b", &["10.0.0.3:47160"])];
        assert_eq!(track_discovered(&mut seen, &list), ["b"]);
    }
    #[test]
    fn failed_dials_are_logged_at_first_then_once_a_minute() {
        let mut failures = HashMap::new();
        let a = sa("10.0.0.2:47160");
        let t0 = Instant::now();
        let logged: Vec<bool> = (0..6).map(|i| dial_failure_loggable(&mut failures, a, t0 + Duration::from_secs(i * 4))).collect();
        assert_eq!(logged, [true, true, true, false, false, false]);
        assert!(dial_failure_loggable(&mut failures, a, t0 + Duration::from_secs(70)));
        assert!(!dial_failure_loggable(&mut failures, a, t0 + Duration::from_secs(74)));
        // Another address has its own streak.
        assert!(dial_failure_loggable(&mut failures, sa("10.0.0.3:47160"), t0));
    }
    #[test]
    fn a_dead_or_unbound_listener_is_bound_again() {
        let mut r = rig(Role::Master);
        r.c.config.port = 0;
        // Nothing wrong: nothing happens (the rig has no listener on purpose).
        r.c.ensure_listener(true);
        assert!(r.c.listener.is_none());
        // An earlier bind failed: the next reconnect binds, and the port shows in the status.
        r.c.bind_failed = true;
        r.c.ensure_listener(true);
        assert!(r.c.listener.is_some() && !r.c.bind_failed);
        assert_ne!(r.c.status.lock().unwrap().me.port, 0);
        // A try right after another waits unless asked to go now.
        r.c.listener = None;
        r.c.bind_failed = true;
        r.c.last_bind_try = Some(Instant::now());
        r.c.ensure_listener(false);
        assert!(r.c.listener.is_none());
        r.c.ensure_listener(true);
        assert!(r.c.listener.is_some());
        r.c.listener = None;
    }
}
