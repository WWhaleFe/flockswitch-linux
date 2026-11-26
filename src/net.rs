//! Network service lifetime and session capabilities.
use crate::{
    config::{Config, Edge},
    shared::Shared,
};
use flock_net::{NetConfig, Platform, Service, Side};
use crate::net_logic::apply_master_settings;
pub use crate::net_logic::NetState;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Where files received from another computer are saved.
pub fn received_dir() -> Option<PathBuf> {
    Some(dirs::download_dir()?.join("flockswitch"))
}

pub fn config_dir() -> PathBuf {
    Config::path()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

/// The running service and the config it runs with, for the edge watcher.
struct Live {
    service: Arc<Service>,
    config: NetConfig,
}
static LIVE: Mutex<Option<Live>> = Mutex::new(None);

static EDGES_ALLOWED: AtomicBool = AtomicBool::new(false);
pub fn network_edges_allowed() -> bool { EDGES_ALLOWED.load(Ordering::Acquire) }

pub fn side(edge: Edge) -> Side {
    match edge {
        Edge::Left => Side::Left,
        Edge::Right => Side::Right,
        Edge::Top => Side::Top,
        Edge::Bottom => Side::Bottom,
    }
}
#[derive(Clone, Debug, Default)]
pub struct Support {
    pub send: bool,
    pub receive: bool,
    pub reason: String,
}
pub fn probe() -> Support {
    if !crate::edge_wayland::session_is_wayland() {
        return match crate::net_x11::probe() {
            Ok(()) => Support {
                send: true,
                receive: true,
                reason: "X11 / XTest".into(),
            },
            Err(reason) => Support {
                reason,
                ..Support::default()
            },
        };
    }
    async_io::block_on(async {
        crate::edge_wayland::within(Duration::from_secs(3), async {
            use ashpd::desktop::remote_desktop::{DeviceType, RemoteDesktop};
            let receive = match RemoteDesktop::new().await {
                Ok(rd) if rd.version() >= 2 => rd.available_device_types().await.is_ok_and(|d| {
                    d.contains(DeviceType::Pointer) && d.contains(DeviceType::Keyboard)
                }),
                _ => false,
            };
            let gnome = std::env::var("XDG_CURRENT_DESKTOP")
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("gnome");
            let send = receive
                && gnome
                && crate::edge_portal::probe()
                    .await
                    .is_ok_and(|(_, caps)| caps.contains("pointer") && caps.contains("keyboard"));
            Support {
                send,
                receive,
                reason: if receive {
                    if send {
                        "InputCapture + RemoteDesktop (EIS)"
                    } else {
                        "RemoteDesktop (EIS); sending requires GNOME InputCapture"
                    }
                } else {
                    "RemoteDesktop ConnectToEIS with pointer and keyboard is unavailable"
                }
                .into(),
            }
        })
        .await
        .unwrap_or(Support {
            reason: "Portal probe timed out".into(),
            ..Support::default()
        })
    })
}
pub fn describe(s: &Support, korean: bool) -> String {
    if korean {
        format!(
            "네트워크 공유: 보내기 {} / 받기 {}. {}",
            if s.send { "가능" } else { "불가" },
            if s.receive { "가능" } else { "불가" },
            s.reason
        )
    } else {
        format!(
            "Network sharing: sending {} / receiving {}. {}",
            if s.send { "available" } else { "unavailable" },
            if s.receive {
                "available"
            } else {
                "unavailable"
            },
            s.reason
        )
    }
}
pub fn cached_support() -> &'static Support {
    // The settings process probes once without opening permission sessions.
    static SUPPORT: std::sync::OnceLock<Support> = std::sync::OnceLock::new();
    SUPPORT.get_or_init(probe)
}
pub fn support_text(korean: bool) -> String {
    let support = cached_support();
    let scope = if korean {
        "X11: 보내기·받기. GNOME Wayland: InputCapture와 RemoteDesktop 필요. 다른 Wayland: ConnectToEIS가 있으면 받기만 가능."
    } else {
        "X11: send and receive. GNOME Wayland requires InputCapture and RemoteDesktop. Other Wayland desktops can only receive through ConnectToEIS."
    };
    format!("{}\n{scope}", describe(support, korean))
}
/// Whether the cursor can cross over `edge` now: this computer is in control and a
/// connected, paired computer sits on that side.
pub fn network_side_ready(edge: Edge) -> bool {
    if !network_edges_allowed() { return false; }
    let live = LIVE.lock().unwrap();
    let Some(live) = live.as_ref() else { return false };
    let side = side(edge);
    let status = live.service.status();
    // The computer in the neighbouring cell of the layout (a master's sub or, on a sub,
    // another sub), as long as its link is up.
    let next = flock_net::layout::neighbour(&status.layout, &status.me.id, side);
    status.running
        && status.state.mode == "local"
        && next.is_some_and(|id| status.peers.iter().any(|s| s.id == id && s.connected))
}

/// Moves control to the computer on `edge`, entering at the spot matching (x, y).
pub fn cross(edge: Edge, x: f64, y: f64) {
    let live = LIVE.lock().unwrap();
    let Some(live) = live.as_ref() else { return };
    if !network_edges_allowed() || !live.config.on || !live.config.app_drives_edges { return; }
    let side = side(edge);
    let pos = flock_net::geometry::edge_ratio(&crate::edge::platform::displays().into_iter().map(|r| flock_net::Rect::new(r.x, r.y, r.w, r.h)).collect::<Vec<_>>(), side, x, y);
    let _ = live.service.cross(side, pos);
}

/// The running service's status, for the tray menu (which lives in the same process).
pub fn live_status() -> Option<flock_net::Status> {
    LIVE.lock().unwrap().as_ref().map(|live| live.service.status())
}

/// The network config the service runs with: sharing only after consent, the master's
/// effect settings attached, and the app driving the edges when there is a wait.
fn runtime_config(full: &Config) -> NetConfig {
    crate::net_logic::runtime_config(full, crate::edge_wayland::session_is_wayland(), config_dir())
}

fn publish_error(config: &NetConfig, error: String) {
    log::warn!("network: {error}");
    let _ = flock_net::write_status(
        config_dir(),
        &flock_net::Status {
            running: false,
            me: flock_net::MeStatus {
                id: String::new(),
                name: config.name.clone(),
                port: config.port,
                error: Some(error),
            },
            discovered: vec![],
            peers: vec![],
            state: flock_net::StateStatus {
                mode: "local".into(),
                peer: None,
            },
            pairing: None,
            transfer: None,
            following: None,
            layout: vec![],
            reconnecting: false,
            keys: Default::default(),
        },
    );
}
pub fn spawn(shared: Arc<Shared>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("network".into())
        .spawn(move || {
            let mut service: Option<Arc<Service>> = None;
            let mut mode = String::from("local");
            let mut platform: Option<Arc<dyn Platform>> = None;
            let mut previous = None;
            let mut version = None;
            let mut cached = None;
            let mut edges_allowed = false;
            let mut attempted = false;
            let mut retry = Instant::now() - Duration::from_secs(5);
            let mut key_retry = Instant::now() - Duration::from_secs(2);
            while !shared.stopped() {
                shared.reload_if_edited();
                if version != Some(shared.version()) {
                    // Serializing the master's effect settings and copying peers is
                    // needed only when settings change, not every supervisor tick.
                    shared.read_config(|full| {
                        version = Some(shared.version());
                        let config = runtime_config(full);
                        edges_allowed = full.network_edges_allowed() && config.on;
                        cached = Some(config);
                    });
                }
                let config = cached.as_ref().expect("initial network settings");
                EDGES_ALLOWED.store(edges_allowed, Ordering::Release);
                if !config.on {
                    if previous.as_ref().is_none_or(|c: &NetConfig| c.on) {
                        LIVE.lock().unwrap().take();
                        mode = "local".into();
                        if let Some(s) = service.take() {
                            s.stop();
                        }
                        platform = None;
                        attempted = false;
                        crate::net_wayland::disable();
                    }
                } else {
                    if !attempted {
                        attempted = true;
                        let result: anyhow::Result<Arc<dyn Platform>> =
                            if crate::edge_wayland::session_is_wayland() {
                                crate::net_wayland::Wayland::new(shared.clone())
                                    .map(|p| Arc::new(p) as Arc<dyn Platform>)
                            } else {
                                crate::net_x11::X11::new().map(|p| Arc::new(p) as Arc<dyn Platform>)
                            };
                        match result {
                            Ok(p) => platform = Some(p),
                            Err(e) => publish_error(config, format!("{e:#}")),
                        }
                    }
                    if service.is_none() && retry.elapsed() >= Duration::from_secs(5) {
                        retry = Instant::now();
                        if let Some(p) = platform.as_ref() {
                            let paired = shared.clone();
                            let unpaired = shared.clone();
                            let changed = shared.clone();
                            let master = shared.clone();
                            let key_rules = shared.clone();
                            let hooks = flock_net::Hooks {
                                switch_logitech: Box::new(|_| {}),
                                on_paired: Box::new(move |peer| {
                                    if let Err(e) = paired.update_config(|c| {
                                        c.network.peers.retain(|p| p.id != peer.id);
                                        c.network.peers.push(peer);
                                    }) {
                                        log::error!("save paired computer: {e:#}");
                                    }
                                }),
                                on_unpaired: Box::new(move |id| {
                                    if let Err(e) = unpaired
                                        .update_config(|c| c.network.peers.retain(|p| p.id != id))
                                    {
                                        log::error!("save unpaired computer: {e:#}");
                                    }
                                }),
                                on_peer_changed: Box::new(move |peer| {
                                    if let Err(e) = changed.update_config(|c| {
                                        match c.network.peers.iter_mut().find(|p| p.id == peer.id) {
                                            Some(slot) => *slot = peer,
                                            None => c.network.peers.push(peer),
                                        }
                                    }) {
                                        log::error!("save computer: {e:#}");
                                    }
                                }),
                                on_master_settings: Box::new(move |settings| {
                                    if let Err(e) = master.update_config(|c| {
                                        apply_master_settings(c, &settings);
                                    }) {
                                        log::error!("save master settings: {e:#}");
                                    }
                                }),
                                on_key_rules: Box::new(move |keys| {
                                    if let Err(e) = key_rules.update_config(|c| c.network.keys = keys) {
                                        log::error!("save key rules from sub: {e:#}");
                                    }
                                }),
                                ..flock_net::Hooks::default()
                            };
                            match Service::start(config.clone(), p.clone(), hooks) {
                                Ok(s) => {
                                    let s = Arc::new(s);
                                    *LIVE.lock().unwrap() = Some(Live { service: s.clone(), config: config.clone() });
                                    service = Some(s);
                                },
                                Err(e) => publish_error(config, format!("{e:#}")),
                            }
                        }
                    } else if previous.as_ref() != Some(config) {
                        if let Some(s) = &service {
                            let _ = s.update_config(config.clone());
                            if let Some(live) = LIVE.lock().unwrap().as_mut() { live.config = config.clone(); }
                        }
                    }
                }
                // Keep the outbox on disk across disabled sharing, failed sends and restarts.
                // Enqueueing is not an acknowledgement: only echoed master settings clear it.
                if config.on && config.role == flock_net::Role::Sub && key_retry.elapsed() >= Duration::from_secs(2) {
                    if let (Some(s), Some(keys)) = (&service, shared.read_config(|c| c.pending_key_rules.clone())) {
                        key_retry = Instant::now();
                        if let Err(e) = s.send_key_rules(keys) {
                            log::warn!("send key rules to the master (will retry): {e:#}");
                        }
                    }
                }
                // Woke from sleep or unlocked: look for the other computers again now.
                if crate::wake::take() {
                    if let Some(s) = &service {
                        log::info!("wake or unlock: reconnecting");
                        let _ = s.reconnect();
                    }
                }
                if let Some(service) = &service {
                    let now = service.status().state.mode;
                    if (mode == "local" && now == "controlled") || (mode == "controlling" && now == "local") {
                        let locate = shared.read_config(|c| c.locate.clone());
                        if locate.on && locate.on_arrival { crate::overlay::locate(&locate); }
                    }
                    mode = now;
                }
                if previous.as_ref() != Some(config) {
                    previous = Some(config.clone());
                }
                thread::sleep(Duration::from_millis(100));
            }
            LIVE.lock().unwrap().take();
            EDGES_ALLOWED.store(false, Ordering::Release);
            if let Some(s) = service {
                s.stop();
            }
            drop(platform);
            crate::net_wayland::disable();
        })
        .expect("spawning network service")
}


pub fn launch_background() -> anyhow::Result<()> {
    std::process::Command::new(std::env::current_exe()?).arg("tray").spawn()?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn allow_test_edges() { EDGES_ALLOWED.store(true, Ordering::Release); }
