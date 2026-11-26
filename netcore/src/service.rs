use crate::{
    discovery::{DiscoveredPeer, Discovery},
    identity::Identity,
    pairing::Pairing,
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
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(8);
const HEARTBEAT: Duration = Duration::from_millis(500);
const DEAD: Duration = Duration::from_secs(2);
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
            keys: HashSet::new(),
            clip_sig: None,
            outgoing: None,
            incoming: None,
            next_transfer: 1,
            buttons: HashSet::new(),
            captured_keys: HashSet::new(),
            mac_hangul: keymap::MacHangul::default(),
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
    fp: String,
    addr: SocketAddr,
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
    told: HashMap<String, (Option<Side>, MasterSettings)>,
    keys: HashSet<u16>,
    /// Fingerprint of the clipboard content last sent or received, so unchanged content
    /// is not sent again.
    clip_sig: Option<Vec<u8>>,
    /// A clipboard transfer on its way out, to this peer.
    outgoing: Option<(String, crate::transfer::Outgoing)>,
    /// A clipboard transfer on its way in, from this peer.
    incoming: Option<(String, crate::transfer::Incoming)>,
    next_transfer: u32,
    buttons: HashSet<MouseButton>,
    captured_keys: HashSet<u16>,
    mac_hangul: keymap::MacHangul,
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
                    Err(_) => break,
                }
            }
            if stop {
                break;
            }
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
            || config.lang1_on_mac != self.config.lang1_on_mac
            || config.mac_capslock_to_hangul != self.config.mac_capslock_to_hangul
            || config.mac_shift_space_to_hangul != self.config.mac_shift_space_to_hangul
        {
            self.local(true);
        }
        config.config_dir = self.config.config_dir.clone();
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
                self.config.peers.retain(|p| p.id != unpair);
                self.disconnect(&unpair, "unpaired");
                (self.hooks.on_unpaired)(unpair);
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
            if !l.outgoing {
                if let Some(addr) = self
                    .discovered
                    .iter()
                    .find(|p| p.id == id && p.fingerprint == l.fp)
                    .and_then(|p| p.addresses.first())
                {
                    l.addr = *addr;
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
            Event::DialDone(addr) => {
                self.inflight.remove(&addr);
            }
            Event::Connected(id, mut link, pair) => {
                if !self.config.on || id == self.identity.id {
                    return;
                }
                if let Some(p) = self.config.peers.iter().find(|p| p.id == id) {
                    if p.fingerprint != link.fp {
                        log::warn!("fingerprint mismatch for {id}");
                        return;
                    }
                    link.paired = true;
                    if let Ok(addr) = p.last_addr.parse() {
                        if !link.outgoing {
                            link.addr = addr;
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
                self.retry.remove(&id);
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
                        if self.config.role == Role::Sub && l.role == Role::Master {
                            let mine = side.map(Side::opposite);
                            if let Some(p) = self.config.peers.iter_mut().find(|p| p.id == id) {
                                if p.side != mine {
                                    p.side = mine;
                                    (self.hooks.on_peer_changed)(p.clone());
                                }
                            }
                        }
                    }
                    Msg::MasterSettings(settings) => {
                        if self.config.role == Role::Sub && l.role == Role::Master {
                            settings.apply_to(&mut self.config);
                            self.status.lock().unwrap().following = Some(settings.master_name.clone());
                            if self.master.as_ref() != Some(&settings) {
                                (self.hooks.on_master_settings)(settings.clone());
                            }
                            self.master = Some(settings);
                        }
                    }
                    Msg::Pong { t } => {
                        if let Some((sent, at)) = l.ping {
                            if sent == t {
                                l.rtt = Some(at.elapsed().as_secs_f64() * 1000.0);
                                l.ping = None;
                            }
                        }
                    }
                    Msg::Enter { side, pos } => {
                        if matches!(self.state, State::Local)
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
                        let sig = clip_sig(b"text", &[text.as_bytes()]);
                        if self.config.share_clipboard && self.clip_sig.as_ref() != Some(&sig) {
                            self.platform.set_clipboard_text(&text);
                            self.clip_sig = Some(sig);
                        }
                    }
                    Msg::ClipStart { id: tid, kind, files } => self.clip_start(&id, tid, kind, files),
                    Msg::ClipChunk { id: tid, file, data } => self.clip_chunk(&id, tid, file, &data),
                    Msg::ClipDone { id: tid } => self.clip_done(&id, tid),
                    Msg::Input(event) => {
                        if matches!(&self.state,State::Controlled{peer,..} if peer==&id)
                            && event.valid()
                        {
                            self.inject(event);
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
        let seconds = [1, 2, 5, 10][retry.attempt.min(3)];
        retry.attempt += 1;
        retry.next = Instant::now() + Duration::from_secs(seconds);
    }
    /// On a master: tells each connected sub where it sits and which settings to use,
    /// whenever that changed (or the sub just connected).
    fn tell_subs(&mut self) {
        if self.config.role != Role::Master {
            self.told.clear();
            return;
        }
        let settings = MasterSettings::of(&self.config, &computer_name(&self.config));
        self.told.retain(|id, _| self.links.get(id).is_some_and(|l| l.paired));
        for p in &self.config.peers {
            let Some(l) = self.links.get(&p.id).filter(|l| l.paired && l.role == Role::Sub) else {
                continue;
            };
            let now = (p.side, settings.clone());
            if self.told.get(&p.id) != Some(&now) {
                l.send(Msg::Layout { side: p.side });
                l.send(Msg::MasterSettings(settings.clone()));
                self.told.insert(p.id.clone(), now);
            }
        }
    }
    fn maintenance(&mut self) {
        self.tell_subs();
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
        let peers = self.config.peers.clone();
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
            let discovered = self
                .discovered
                .iter()
                .find(|d| d.id == p.id && d.fingerprint == p.fingerprint)
                .and_then(|d| d.addresses.first())
                .copied();
            if let Some(addr) = discovered.or_else(|| p.last_addr.parse().ok()) {
                if !self.inflight.contains(&addr) {
                    self.spawn_link(None, addr, false, Some(p.id.clone()));
                    self.schedule_retry(&p.id);
                }
            }
        }
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
        self.mac_hangul = keymap::MacHangul::default();
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
    fn inject(&mut self, mut event: InputEvent) {
        match &mut event {
            InputEvent::Key { usage, down } => {
                if self.platform.os() == Os::Mac {
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
            for mut event in self.mac_hangul.map(event, self.platform.os(), l.os, &self.config, &self.captured_keys) {
                if let InputEvent::Key { usage, .. } = &mut event {
                    *usage = keymap::swap_cmd_ctrl(*usage, self.platform.os(), l.os, self.config.swap_cmd_ctrl);
                }
                l.send(Msg::Input(event));
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
        self.mac_hangul = keymap::MacHangul::default();
        self.switch_channel(&peer);
    }
    fn cross_on_request(&mut self, side: Side, pos: f64) {
        if !self.config.on || !matches!(self.state, State::Local) || !pos.is_finite() {
            return;
        }
        if self.platform.ready().is_err() {
            return;
        }
        let Some(peer) = self
            .config
            .peers
            .iter()
            .find(|p| p.side == Some(side) && self.links.get(&p.id).is_some_and(|l| l.paired))
            .map(|p| p.id.clone())
        else {
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
            if self
                .config
                .peers
                .iter()
                .any(|p| p.side == Some(side) && self.links.get(&p.id).is_some_and(|l| l.paired))
            {
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
                let Some(peer) = self
                    .config
                    .peers
                    .iter()
                    .find(|p| {
                        p.side == Some(side) && self.links.get(&p.id).is_some_and(|l| l.paired)
                    })
                    .map(|p| p.id.clone())
                else {
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
                    if self
                        .config
                        .peers
                        .iter()
                        .any(|p| p.id == peer && p.side == Some(entered))
                    {
                        self.switch_channel(&peer);
                    }
                }
            }
            State::Controlling { .. } => {}
        }
        self.last_cursor = cursor;
    }
    /// While controlled: Control held on the controlling computer's keyboard (as injected
    /// here) or on this computer's own. With Cmd/Ctrl swapped between a Mac and another
    /// system, the controller's Control key arrives here as Cmd/Windows.
    fn control_held(&self, peer: &str) -> bool {
        const CTRL: [u16; 2] = [0xE0, 0xE4];
        const GUI: [u16; 2] = [0xE3, 0xE7];
        let mine_mac = self.platform.os() == Os::Mac;
        let swapped = self.config.swap_cmd_ctrl
            && self
                .config
                .peers
                .iter()
                .find(|p| p.id == peer)
                .is_some_and(|p| (p.os == Os::Mac) != mine_mac);
        let control = if swapped { GUI } else { CTRL };
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
                let sig = files_sig(&files);
                if self.clip_sig.as_ref() == Some(&sig) {
                    return;
                }
                self.clip_sig = Some(sig);
                let id = self.take_transfer_id();
                match Outgoing::files(id, &files, limit) {
                    Ok(out) => self.begin_send(peer, "files", out),
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
                if self.clip_sig.as_ref() == Some(&sig) {
                    return;
                }
                self.clip_sig = Some(sig);
                if png.len() as u64 > limit {
                    self.transfer_note("send", "image", "too_large");
                    return;
                }
                let id = self.take_transfer_id();
                self.begin_send(peer, "image", Outgoing::bytes(id, ClipKind::Image, "clipboard.png", png));
                return;
            }
        }
        let Some(text) = self.platform.clipboard_text().filter(|t| !t.is_empty()) else {
            return;
        };
        let sig = clip_sig(b"text", &[text.as_bytes()]);
        if self.clip_sig.as_ref() == Some(&sig) {
            return;
        }
        self.clip_sig = Some(sig);
        if text.len() <= INLINE_TEXT {
            if let Some(l) = self.links.get(peer) {
                l.send(Msg::Clipboard { text });
            }
        } else if text.len() as u64 <= MAX_TEXT {
            let id = self.take_transfer_id();
            self.begin_send(peer, "text", Outgoing::bytes(id, ClipKind::Text, "clipboard.txt", text.into_bytes()));
        } else {
            self.transfer_note("send", "text", "too_large");
        }
    }
    fn take_transfer_id(&mut self) -> u32 {
        self.next_transfer = self.next_transfer.wrapping_add(1).max(1);
        self.next_transfer
    }
    fn begin_send(&mut self, peer: &str, kind: &str, out: crate::transfer::Outgoing) {
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
                    link.send(msg);
                    if done {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    log::warn!("clipboard transfer: {e:#}");
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
                self.platform.set_clipboard_files(&saved);
                self.clip_sig = Some(files_sig(&saved));
                self.transfer_finished(Some(dest.display().to_string()));
            }
            Ok(Received::Image(png)) => {
                self.platform.set_clipboard_image_png(&png);
                self.clip_sig = Some(clip_sig(b"image", &[&png]));
                self.transfer_finished(None);
            }
            Ok(Received::Text(text)) => {
                self.platform.set_clipboard_text(&text);
                self.clip_sig = Some(clip_sig(b"text", &[text.as_bytes()]));
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
            .map(|p| PeerStatus {
                id: p.id.clone(),
                connected: self.links.get(&p.id).is_some_and(|l| l.paired),
                rtt_ms: self.links.get(&p.id).and_then(|l| l.rtt),
            })
            .collect();
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
            },
        )?;
        let (tx, rx) = mpsc::channel();
        let mut tx = Some(tx);
        let bulk = Arc::new(AtomicUsize::new(0));
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
                    // Hello has no listen port. Discovery or saved config supplies it on inbound links.
                    let addr = if self.outgoing {
                        self.addr
                    } else {
                        SocketAddr::new(self.addr.ip(), 47160)
                    };
                    self.events.send(Event::Connected(
                        id,
                        Link {
                            token,
                            tx: tx.take().unwrap(),
                            cancel: self.cancel.clone(),
                            name,
                            os,
                            role,
                            fp: fp.clone(),
                            addr,
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
/// Files are told apart by path, size and modification time, without reading them.
fn files_sig(files: &[std::path::PathBuf]) -> Vec<u8> {
    let mut parts = Vec::new();
    for f in files {
        let meta = std::fs::metadata(f).ok();
        let modified = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        parts.push(format!("{}|{}|{}", f.display(), meta.map_or(0, |m| m.len()), modified).into_bytes());
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    clip_sig(b"files", &refs)
}
