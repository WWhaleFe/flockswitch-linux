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
    collections::{HashMap, HashSet},
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

const POLL: Duration = Duration::from_millis(8);
const HEARTBEAT: Duration = Duration::from_millis(500);
const DEAD: Duration = Duration::from_secs(2);
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
            clip_origin: None,
            trusted: vec![],
            group: None,
            reconnecting: None,
            skew: Duration::ZERO,
            last_wall: SystemTime::now(),
            outgoing: None,
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
    listener.set_nonblocking(true)?;
    Ok(listener)
}

struct Link {
    token: u64,
    tx: Sender<Msg>,
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
    fn send(&self, msg: Msg) {
        let _ = self.tx.send(msg);
    }
}
impl Drop for Link {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
enum Event {
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
    listener: Option<TcpListener>,
    status: Arc<Mutex<Status>>,
    events: Sender<Event>,
    event_rx: Receiver<Event>,
    rx: Receiver<Command>,
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
        while !stop {
            let tick = Instant::now();
            for _ in 0..128 {
                match self.rx.try_recv() {
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
            if self.config.on {
                for _ in 0..16 {
                    let result = self.listener.as_ref().map(|l| l.accept());
                    match result {
                        Some(Ok((s, a))) => self.spawn_link(Some(s), a, false, None),
                        Some(Err(e)) if e.kind() != std::io::ErrorKind::WouldBlock => {
                            log::warn!("listener: {e}");
                            break;
                        }
                        _ => break,
                    }
                }
            }
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
            thread::sleep(POLL.saturating_sub(tick.elapsed()));
        }
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
        for (_, handle) in self.workers.drain(..) {
            let _ = handle.join();
        }
        self.config.on = false;
        // Persist the final snapshot within the same write rate limit.
        thread::sleep(Duration::from_millis(300).saturating_sub(self.last_write.elapsed()));
        self.publish(true);
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
        self.config = config;
        self.enforce_send_permission();
        self.sync_sides();
        if rebind {
            self.listener = None;
            if self.config.on {
                match bind(self.config.port) {
                    Ok(l) => {
                        let port = l.local_addr().unwrap().port();
                        self.status.lock().unwrap().me.port = port;
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
                self.config.peers.retain(|p| p.id != unpair);
                if was_master {
                    self.clear_group();
                }
                self.disconnect(&unpair, "unpaired");
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
                    Msg::Ping { t } => l.send(Msg::Pong { t }),
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
                            self.key_rules_edited(keys);
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
                        if self.config.share_clipboard {
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
                    Msg::Bye { .. } => self.disconnect(&id, "bye"),
                }
            }
        }
    }
    fn disconnect(&mut self, id: &str, reason: &str) {
        if self.state.peer() == Some(id) {
            self.local(false);
        }
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
            let Some(l) = self.links.get(&p.id).filter(|l| l.paired && l.role == Role::Sub) else {
                continue;
            };
            let now = (p.side, settings.clone(), group.clone());
            if self.told.get(&p.id) != Some(&now) {
                l.send(Msg::Layout { side: p.side });
                l.send(Msg::MasterSettings(settings.clone()));
                if l.features.iter().any(|f| f == "group") {
                    l.send(Msg::Group { cells: group.0.clone(), roster: group.1.clone() });
                }
                self.told.insert(p.id.clone(), now);
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
                layout::with_master(&self.config.layout, me)
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
                None => l.send(Msg::Input(event)),
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
        // Discard events from an earlier capture before installing the next sink.
        while self.captured.try_recv().is_ok() {}
        if let Err(e) = self.platform.start_capture(self.capture.clone()) {
            self.edge_hit = None;
            self.edge_wait = None;
            self.platform.stop_capture();
            self.status.lock().unwrap().me.error = Some(e.to_string());
            return;
        }
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
        let limit = u64::from(crate::transfer::LIMIT_MB) * 1024 * 1024;
        if limit > 0 {
            let files = self.platform.clipboard_files();
            if !files.is_empty() {
                let id = self.take_transfer_id();
                match Outgoing::files(id, &files, limit) {
                    Ok(out) => {
                        // Hash exactly what will travel, after filtering and size checks.
                        let sig = match files_sig(out.file_paths().expect("file transfer")) {
                            Ok(sig) => sig,
                            Err(e) => {
                                log::info!("clipboard files not read: {e:#}");
                                self.transfer_note("send", "files", "failed");
                                return;
                            }
                        };
                        if self.clip_wanted(peer, &sig) { self.begin_send(peer, "files", out, sig); }
                    }
                    Err(e) => {
                        log::info!("clipboard files not sent: {e:#}");
                        let large = e.to_string().starts_with("too large");
                        self.transfer_note("send", "files", if large { "too_large" } else { "failed" });
                    }
                }
                return;
            }
            if let Some(png) = self.platform.clipboard_image_png() {
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
                if l.tx.send(Msg::Clipboard { text }).is_ok() {
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
        self.status.lock().unwrap().transfer = Some(TransferStatus {
            direction: "send".into(),
            kind: kind.into(),
            done_bytes: 0,
            total_bytes: out.total,
            stage: "running".into(),
            saved_in: None,
        });
        // A newer transfer replaces one still going; the peer drops the old one when the
        // new one starts.
        if let Some((old_peer, _)) = self.outgoing.take() {
            self.pending_clip.remove(&old_peer);
        }
        self.pending_clip.insert(peer.into(), (out.id(), sig));
        self.outgoing = Some((peer.into(), out));
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
                    if matches!(msg, Msg::ClipChunk { .. }) {
                        link.bulk.fetch_add(1, Ordering::AcqRel);
                    }
                    if link.tx.send(msg).is_err() {
                        self.pending_clip.remove(peer.as_str());
                        self.outgoing = None;
                        self.transfer_note("send", "files", "failed");
                        return;
                    }
                    if done {
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
    fn clip_start(&mut self, peer: &str, id: u32, kind: crate::transfer::ClipKind, files: Vec<crate::transfer::FileMeta>) {
        use crate::transfer::{ClipKind, Incoming};
        if let Some((_, old)) = self.incoming.take() {
            old.abandon();
        }
        let kind_name = match kind {
            ClipKind::Files => "files",
            ClipKind::Image => "image",
            ClipKind::Text => "text",
        };
        if !self.config.share_clipboard {
            return;
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
        use crate::transfer::Received;
        let Some((from, incoming)) = self.incoming.take() else { return };
        if from != peer || incoming.id != id {
            self.incoming = Some((from, incoming));
            return;
        }
        let dest = self
            .platform
            .received_dir()
            .unwrap_or_else(|| self.config.config_dir.join("flockswitch").join("received"));
        match incoming.finish(&dest) {
            Ok(Received::Files(saved)) => {
                // Hashing reads every file back, so do it before the clipboard is set: anyone
                // who sees the files on the clipboard then also sees the transfer as done.
                if let Ok(sig) = files_sig(&saved) {
                    self.clip_received(peer, sig);
                }
                self.transfer_finished(Some(dest.display().to_string()));
                self.platform.set_clipboard_files(&saved);
            }
            Ok(Received::Image(png)) => {
                self.platform.set_clipboard_image_png(&png);
                self.clip_received(peer, clip_sig(b"image", &[&png]));
                self.transfer_finished(None);
            }
            Ok(Received::Text(text)) => {
                self.platform.set_clipboard_text(&text);
                self.clip_received(peer, clip_sig(b"text", &[text.as_bytes()]));
                self.transfer_finished(None);
            }
            Err(e) => {
                log::warn!("clipboard transfer: {e:#}");
                self.transfer_note("receive", "files", "failed");
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
                connected: self.links.get(&p.id).is_some_and(|l| l.paired),
                rtt_ms: self.links.get(&p.id).and_then(|l| l.rtt),
                can_send: self.peer_can_send(&p.id),
                via_master: p.via_master,
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
    cancel: Arc<AtomicBool>,
    addr: SocketAddr,
    outgoing: bool,
    pair: bool,
    expected: Option<String>,
}
impl Worker {
    fn run(self, socket: Option<TcpStream>) {
        let token = NEXT_LINK.fetch_add(1, Ordering::Relaxed);
        let mut remote = None;
        if let Err(e) = self.transport(socket, token, &mut remote) {
            log::debug!("network link {}: {e:#}", self.addr);
        }
        if let Some(id) = remote {
            let _ = self.events.send(Event::Closed(id, token));
        }
        if self.outgoing {
            let _ = self.events.send(Event::DialDone(self.addr));
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
        let (tx, rx) = mpsc::channel();
        let mut tx = Some(tx);
        let bulk = Arc::new(AtomicUsize::new(0));
        let mut pending_clip = None;
        let mut bytes = Vec::new();
        let mut chunk = [0; 8192];
        loop {
            let cancelled = self.cancel.load(Ordering::Acquire);
            // A new message only once the socket took everything before it, so a
            // transfer never piles up ahead of the input that follows it.
            for _ in 0..128 {
                if conn.wants_write() {
                    break;
                }
                match rx.try_recv() {
                    Ok(msg) => {
                        proto::write_frame(conn.writer(), &msg)?;
                        if let Msg::ClipDone { id } = msg {
                            pending_clip = Some(id);
                        }
                        if matches!(msg, Msg::ClipChunk { .. }) {
                            bulk.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                    Err(_) => break,
                }
            }
            if cancelled {
                conn.send_close_notify();
            }
            while conn.wants_write() {
                match conn.write_tls(&mut socket) {
                    Ok(0) => anyhow::bail!("socket closed"),
                    Ok(_) => {}
                    Err(e) if transient(&e) => break,
                    Err(e) => return Err(e.into()),
                }
            }
            if cancelled {
                return Ok(());
            }
            if !conn.wants_write() {
                if let (Some(id), Some(peer)) = (pending_clip.take(), remote.as_ref()) {
                    self.events.send(Event::ClipSent(peer.clone(), token, id))?;
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
                    let mut addr = self.addr;
                    if !self.outgoing { addr.set_port(listen_port.unwrap_or(47160)); }
                    self.events.send(Event::Connected(
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
                    self.events.send(Event::Message(
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
            thread::sleep(Duration::from_millis(2));
        }
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
fn files_sig(files: &[std::path::PathBuf]) -> Result<Vec<u8>> {
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
            let n = file.read(&mut buffer)?;
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
        stopped: AtomicUsize,
        mac: AtomicBool,
    }
    impl Platform for TestPlatform {
        fn os(&self) -> Os { if self.mac.load(Ordering::Relaxed) { Os::Mac } else { Os::Windows } }
        fn displays(&self) -> Vec<Rect> { vec![Rect::new(0.0, 0.0, 100.0, 100.0)] }
        fn cursor(&self) -> Option<(f64, f64)> { Some((50.0, 50.0)) }
        fn warp(&self, _: f64, _: f64) {}
        fn start_capture(&self, _: Sender<InputEvent>) -> Result<()> { Ok(()) }
        fn stop_capture(&self) { self.stopped.fetch_add(1, Ordering::Relaxed); }
        fn inject(&self, event: &InputEvent) { self.injected.lock().unwrap().push(event.clone()); }
        fn ready(&self) -> Result<(), String> { Ok(()) }
        fn clipboard_text(&self) -> Option<String> { self.text.lock().unwrap().clone() }
        fn set_clipboard_text(&self, text: &str) { *self.text.lock().unwrap() = Some(text.into()); }
        fn clipboard_files(&self) -> Vec<std::path::PathBuf> { self.files.lock().unwrap().clone() }
    }
    struct Rig { c: Control, p: Arc<TestPlatform>, dir: std::path::PathBuf }
    impl Drop for Rig {
        fn drop(&mut self) {
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
        let (_, rx) = mpsc::channel();
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
            clip_origin: None,
            trusted: vec![],
            group: None,
            reconnecting: None,
            skew: Duration::ZERO,
            last_wall: SystemTime::now(),
            outgoing: None,
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
    fn link(c: &mut Control, id: &str, role: Role) -> Receiver<Msg> {
        let (tx, rx) = mpsc::channel();
        c.event(Event::Connected(id.into(), Link {
            token: 1, tx, cancel: Arc::new(AtomicBool::new(false)), name: id.into(), os: Os::Mac,
            role, features: proto::FEATURES.iter().map(|s| s.to_string()).collect(), fp: id.repeat(4),
            addr: "127.0.0.1:47160".parse().unwrap(), listen_port: None, outgoing: true, paired: false,
            last_rx: Instant::now(), last_ping: Instant::now(), ping: None, rtt: None,
            bulk: Arc::new(AtomicUsize::new(0)),
        }, false));
        rx
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

}
