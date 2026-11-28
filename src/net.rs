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
/// What this session can do for sharing. With the GNOME helper extension in use (see
/// `edge_wayland::net_uses_helper`) that is everything, and no portal is asked.
pub fn probe() -> Support {
    if crate::edge_wayland::session_is_wayland() && crate::edge_wayland::net_uses_helper() {
        return Support { send: true, receive: true, reason: "GNOME helper extension".into() };
    }
    portal_probe()
}
/// `probe` through the desktop portals (Wayland) or XTest (X11).
pub fn portal_probe() -> Support {
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
    crate::net_logic::side_ready(&live.service.status(), side)
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
fn runtime_config(full: &Config, helper: bool) -> NetConfig {
    crate::net_logic::runtime_config(full, crate::net_logic::portal_edges(crate::edge_wayland::session_is_wayland(), helper), config_dir())
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
                can_send: true,
                displays: vec![],
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
            keys: Default::default(), key_check: None,
        },
    );
}
const PLATFORM_RETRY_MIN: Duration = Duration::from_secs(5);
const PLATFORM_RETRY_MAX: Duration = Duration::from_secs(60);
pub fn spawn(shared: Arc<Shared>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("network".into())
        .spawn(move || {
            let mut service: Option<Arc<Service>> = None;
            let mut service_keys = None;
            let mut mode = String::from("local");
            let mut platform: Option<Arc<dyn Platform>> = None;
            let mut previous = None;
            let mut version = None;
            let mut cached = None;
            let mut edges_allowed = false;
            let mut platform_retry = Instant::now() - PLATFORM_RETRY_MIN;
            // Wait before the next try; doubles after each failed start, reset after a success.
            let mut platform_wait = PLATFORM_RETRY_MIN;
            // The user cancelled or denied the portal dialog: only a settings change (or
            // turning sharing off and on) asks again.
            let mut platform_blocked = false;
            // Whether the GNOME helper extension is what this service runs on (net_gnome.rs).
            // Decided while there is no platform; kept while one is made or running, so
            // the settings (`app_drives_edges`) and the platform always agree.
            let mut helper = false;
            let mut starting: Option<(u64, std::sync::mpsc::Receiver<anyhow::Result<Arc<dyn Platform>>>)> = None;
            let mut retry = Instant::now() - Duration::from_secs(5);
            let mut key_retry = Instant::now() - Duration::from_secs(2);
            while !shared.stopped() {
                shared.reload_if_edited();
                let helper_now = if platform.is_some() || starting.is_some() {
                    helper
                } else {
                    crate::edge_wayland::session_is_wayland() && crate::edge_wayland::net_uses_helper()
                };
                if version != Some(shared.version()) || helper_now != helper {
                    helper = helper_now;
                    // Serializing the master's effect settings and copying peers is
                    // needed only when settings change, not every supervisor tick.
                    shared.read_config(|full| {
                        version = Some(shared.version());
                        let config = runtime_config(full, helper);
                        edges_allowed = full.network_edges_allowed() && config.on;
                        cached = Some(config);
                    });
                }
                let config = cached.as_ref().expect("initial network settings");
                EDGES_ALLOWED.store(edges_allowed, Ordering::Release);
                // Looking for computers again also asks the portal again after a cancel or deny.
                let reconnect_asked = crate::shared::take_reconnect_file();
                if !config.on {
                    if previous.as_ref().is_none_or(|c: &NetConfig| c.on) {
                        crate::net_wayland::disable();
                        LIVE.lock().unwrap().take();
                        mode = "local".into();
                        if let Some(s) = service.take() {
                            s.stop();
                        }
                        platform = None;
                        starting = None;
                        platform_wait = PLATFORM_RETRY_MIN;
                        platform_blocked = false;
                        platform_retry = Instant::now() - PLATFORM_RETRY_MIN;
                    }
                } else {
                    // Looking for computers again is a new request: ask again at once. Other
                    // settings changes are not: the network itself writes some (a computer's
                    // name, the master's effect settings), and each would bring the system's
                    // question back after the user turned it down.
                    if platform.is_none() && (crate::net_logic::ask_again_after_deny(reconnect_asked) || previous.as_ref().is_none_or(|c| !c.on)) {
                        if platform_blocked {
                            log::info!("network: asking for the input permission again ({})", if reconnect_asked { "looking for computers again" } else { "sharing turned on" });
                        }
                        platform_blocked = false;
                        platform_wait = PLATFORM_RETRY_MIN;
                        platform_retry = Instant::now() - PLATFORM_RETRY_MIN;
                    }
                    if platform.is_some() && crate::net_wayland::eis_opened() {
                        platform_wait = PLATFORM_RETRY_MIN;
                    }
                    // The Wayland EIS session ends for good when the compositor drops it:
                    // throw the platform away and build a new one after a pause. A session
                    // that never opened (dialog cancelled or denied) is not asked again.
                    if platform.is_some() {
                        if let Some(reason) = crate::net_wayland::eis_failed() {
                            let opened = crate::net_wayland::eis_opened();
                            let retry_later = opened || crate::net_wayland::eis_no_answer();
                            log::warn!(
                                "network: input session ended{}: {reason}",
                                if opened { ", making a new one (the saved restore token should keep the system from asking)" } else if retry_later { ", trying again" } else { "" }
                            );
                            LIVE.lock().unwrap().take();
                            if let Some(s) = service.take() {
                                s.stop();
                            }
                            mode = "local".into();
                            platform = None;
                            platform_retry = Instant::now();
                            if opened {
                                platform_wait = PLATFORM_RETRY_MIN;
                            } else if retry_later {
                                platform_wait = (platform_wait * 2).min(PLATFORM_RETRY_MAX);
                            } else {
                                log::info!("network: the input permission was turned down; not asking again until sharing is turned off and on or 연결 다시 찾기 (look again) is pressed");
                                platform_blocked = true;
                            }
                            publish_error(config, reason);
                        }
                    }
                    // A start that failed (no EIS yet, portal not answering) is tried again
                    // later. It runs on its own thread: probing the portal can take seconds
                    // and the supervisor must keep answering stop and settings changes.
                    if let Some((generation, rx)) = &starting {
                        match rx.try_recv() {
                            Ok(Ok(p)) => {
                                if crate::net_wayland::current_start(*generation) {
                                    platform = Some(p);
                                }
                                starting = None;
                            }
                            Ok(Err(e)) => {
                                platform_retry = Instant::now();
                                platform_wait = (platform_wait * 2).min(PLATFORM_RETRY_MAX);
                                publish_error(config, format!("{e:#}"));
                                starting = None;
                            }
                            Err(std::sync::mpsc::TryRecvError::Empty) => {}
                            Err(std::sync::mpsc::TryRecvError::Disconnected) => starting = None,
                        }
                    }
                    if platform.is_none()
                        && starting.is_none()
                        && !platform_blocked
                        && platform_retry.elapsed() >= platform_wait
                    {
                        platform_retry = Instant::now();
                        let generation = crate::net_wayland::begin_start();
                        let (tx, rx) = std::sync::mpsc::channel();
                        let wayland_shared = shared.clone();
                        let use_helper = helper;
                        let spawned = thread::Builder::new().name("network-platform".into()).spawn(move || {
                            let result: anyhow::Result<Arc<dyn Platform>> =
                                if use_helper {
                                    crate::net_gnome::Gnome::new().map(|p| Arc::new(p) as Arc<dyn Platform>)
                                } else if crate::edge_wayland::session_is_wayland() {
                                    crate::net_wayland::Wayland::new(wayland_shared, generation)
                                        .map(|p| Arc::new(p) as Arc<dyn Platform>)
                                } else {
                                    crate::net_x11::X11::new().map(|p| Arc::new(p) as Arc<dyn Platform>)
                                };
                            let _ = tx.send(result);
                        });
                        match spawned {
                            Ok(_) => starting = Some((generation, rx)),
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
                            // The hook receives the service's merged table, not the
                            // sub's patch. Compare with the table given to the service.
                            let previous_keys = Arc::new(Mutex::new(config.keys.clone()));
                            let persisted_keys = previous_keys.clone();
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
                                    if let Err(e) = unpaired.update_config(|c| crate::net_logic::forget_peer(c, &id)) {
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
                                persist_key_rules: Box::new(move |keys| {
                                    let mut previous = persisted_keys.lock().unwrap();
                                    key_rules.update_config(|c| {
                                        // update_config reads and locks the latest disk
                                        // settings. Only overwrite pairs changed by this edit.
                                        c.network.keys.extend(keys.iter()
                                            .filter(|(pair, rule)| previous.get(*pair) != Some(*rule))
                                            .map(|(pair, rule)| (pair.clone(), rule.clone())));
                                    })?;
                                    *previous = keys.clone();
                                    Ok(())
                                }),
                                open_key_check: Box::new(crate::tray::open_key_check_window),
                                ..flock_net::Hooks::default()
                            };
                            match Service::start(config.clone(), p.clone(), hooks) {
                                Ok(s) => {
                                    let s = Arc::new(s);
                                    *LIVE.lock().unwrap() = Some(Live { service: s.clone(), config: config.clone() });
                                    service = Some(s);
                                    service_keys = Some(previous_keys);
                                },
                                Err(e) => publish_error(config, format!("{e:#}")),
                            }
                        }
                    } else if previous.as_ref() != Some(config) {
                        if let Some(s) = &service {
                            if let Some(keys) = &service_keys {
                                let mut keys = keys.lock().unwrap();
                                if s.update_config(config.clone()).is_ok() { *keys = config.keys.clone(); }
                            }
                            if let Some(live) = LIVE.lock().unwrap().as_mut() { live.config = config.clone(); }
                        }
                    }
                }
                // Keep the outbox on disk across disabled sharing, failed sends and restarts.
                // Enqueueing is not an acknowledgement: only echoed master settings clear it.
                // It goes when it can never be delivered: no master, not a sub, or a 2.0
                // master that ignores key rule edits.
                if shared.read_config(|c| c.pending_key_rules.is_some()) {
                    let status = service.as_ref().map(|s| s.status());
                    if shared.read_config(|c| crate::net_logic::keys_undeliverable(c, status.as_ref())) {
                        if let Err(e) = shared.update_config(|c| {
                            if crate::net_logic::keys_undeliverable(c, status.as_ref()) { c.pending_key_rules = None; }
                        }) {
                            log::warn!("drop undeliverable key rules: {e:#}");
                        }
                    } else if config.on && config.role == flock_net::Role::Sub && key_retry.elapsed() >= Duration::from_secs(2) {
                        // netcore holds edits back until the master's settings arrived.
                        if let (Some(s), Some(status), Some(keys)) = (&service, &status, shared.read_config(|c| c.pending_key_rules.clone())) {
                            if status.following.is_some() {
                                key_retry = Instant::now();
                                if let Err(e) = s.send_key_rules(keys) {
                                    log::warn!("send key rules to the master (will retry): {e:#}");
                                }
                            }
                        }
                    }
                }
                // Woke from sleep or unlocked: look for the other computers again now.
                if crate::wake::take() {
                    if let Some(s) = &service {
                        // Only the network connections: the RemoteDesktop and InputCapture
                        // sessions stay, so the system does not ask again.
                        log::info!("wake or unlock: reconnecting (input sessions kept)");
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
            crate::net_wayland::disable();
            if let Some(s) = service {
                s.stop();
            }
            drop(platform);
        })
        .expect("spawning network service")
}


pub fn launch_background() -> anyhow::Result<()> {
    std::process::Command::new(std::env::current_exe()?).arg("tray").spawn()?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn allow_test_edges() { EDGES_ALLOWED.store(true, Ordering::Release); }
