//! Network service lifetime and session capabilities.
use crate::{
    config::{Config, Edge},
    shared::Shared,
};
use flock_net::{NetConfig, Platform, Service, Side};
use std::{
    path::PathBuf,
    sync::Arc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub fn config_dir() -> PathBuf {
    Config::path()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}
pub fn side(edge: Edge) -> Side {
    match edge {
        Edge::Left => Side::Left,
        Edge::Right => Side::Right,
        Edge::Top => Side::Top,
        Edge::Bottom => Side::Bottom,
    }
}
pub fn owns_edge(config: &NetConfig, edge: Edge) -> bool {
    config.on && config.peers.iter().any(|p| p.side == Some(side(edge)))
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
pub fn support_text(korean: bool) -> String {
    // The settings process probes once without opening permission sessions.
    static SUPPORT: std::sync::OnceLock<Support> = std::sync::OnceLock::new();
    let support = SUPPORT.get_or_init(probe);
    let scope = if korean {
        "X11: 보내기·받기. GNOME Wayland: InputCapture와 RemoteDesktop 필요. 다른 Wayland: ConnectToEIS가 있으면 받기만 가능."
    } else {
        "X11: send and receive. GNOME Wayland requires InputCapture and RemoteDesktop. Other Wayland desktops can only receive through ConnectToEIS."
    };
    format!("{}\n{scope}", describe(support, korean))
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
        },
    );
}
pub fn spawn(shared: Arc<Shared>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("network".into())
        .spawn(move || {
            let mut service: Option<Service> = None;
            let mut platform: Option<Arc<dyn Platform>> = None;
            let mut previous = None;
            let mut attempted = false;
            let mut retry = Instant::now() - Duration::from_secs(5);
            while !shared.stopped() {
                shared.reload_if_edited();
                let mut config = shared.config().network;
                config.config_dir = config_dir();
                if !config.on {
                    if let Some(s) = service.take() {
                        s.stop();
                    }
                    platform = None;
                    attempted = false;
                    crate::net_wayland::disable();
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
                            Err(e) => publish_error(&config, format!("{e:#}")),
                        }
                    }
                    if service.is_none() && retry.elapsed() >= Duration::from_secs(5) {
                        retry = Instant::now();
                        if let Some(p) = platform.as_ref() {
                            let switch = shared.clone();
                            let paired = shared.clone();
                            let unpaired = shared.clone();
                            let hooks = flock_net::Hooks {
                                switch_logitech: Box::new(move |channel| {
                                    switch.request_switch(channel)
                                }),
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
                                ..flock_net::Hooks::default()
                            };
                            match Service::start(config.clone(), p.clone(), hooks) {
                                Ok(s) => service = Some(s),
                                Err(e) => publish_error(&config, format!("{e:#}")),
                            }
                        }
                    } else if previous.as_ref() != Some(&config) {
                        if let Some(s) = &service {
                            let _ = s.update_config(config.clone());
                        }
                    }
                }
                previous = Some(config);
                thread::sleep(Duration::from_millis(100));
            }
            if let Some(s) = service {
                s.stop();
            }
            drop(platform);
            crate::net_wayland::disable();
        })
        .expect("spawning network service")
}
