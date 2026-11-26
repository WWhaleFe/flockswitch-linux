//! Pure network settings and presentation, shared with host tests.
use crate::config::Config;
use flock_net::{NetConfig, Role};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Apply the settings received from the master.
pub fn apply_master_settings(config: &mut Config, settings: &flock_net::MasterSettings) {
    settings.apply_to(&mut config.network);
    if let Some(pending) = &config.pending_key_rules {
        if *pending == config.network.all_rules() {
            config.pending_key_rules = None;
        } else {
            // An older response must not consume a newer edit, or replace it locally.
            config.network.keys = pending.clone();
        }
    }
    apply_app_settings(config, &settings.app);
}

pub fn runtime_config(full: &Config, wayland: bool, config_dir: PathBuf) -> NetConfig {
    let mut config = full.network.clone();
    config.config_dir = config_dir;
    config.on &= config.notice_accepted;
    config.app_drives_edges = !wayland;
    config.clipboard_limit_mb = 100;
    if config.role == Role::Master {
        config.app_settings = app_settings(full);
    }
    config
}

#[cfg(test)]
mod delivery_tests {
    use super::*;

    #[test]
    fn offline_edit_survives_old_master_settings_and_restart() {
        let mut sub = Config::default();
        sub.network.role = Role::Sub;
        let old = flock_net::MasterSettings::of(&sub.network, "master");
        sub.network.keys = sub.network.all_rules();
        sub.network.keys.get_mut("linux>mac").unwrap().capture = false;
        sub.pending_key_rules = Some(sub.network.keys.clone());
        let mut sub = Config::parse(&sub.to_text().unwrap()).unwrap();
        apply_master_settings(&mut sub, &old);
        assert!(!sub.network.keys["linux>mac"].capture);
        assert!(sub.pending_key_rules.is_some());
        let ack = flock_net::MasterSettings::of(&sub.network, "master");
        apply_master_settings(&mut sub, &ack);
        assert!(sub.pending_key_rules.is_none());
    }

    #[test]
    fn old_ack_does_not_delete_a_newer_edit() {
        let mut sub = Config::default();
        sub.network.keys = sub.network.all_rules();
        sub.network.keys.get_mut("linux>mac").unwrap().capture = false;
        let old_ack = flock_net::MasterSettings::of(&sub.network, "master");
        sub.network.keys.get_mut("mac>linux").unwrap().capture = false;
        sub.pending_key_rules = Some(sub.network.keys.clone());
        apply_master_settings(&mut sub, &old_ack);
        assert!(sub.pending_key_rules.is_some());
        assert!(!sub.network.keys["mac>linux"].capture);
    }
}

/// The effect settings a master hands to its subs: the countdown look and the cursor
/// highlight.
const OWN_EDGE_KEYS: [&str; 5] = ["left", "right", "top", "bottom", "on"];
pub(crate) fn app_settings(config: &Config) -> Value {
    let mut edges = serde_json::to_value(&config.edges).unwrap_or_default();
    if let Some(map) = edges.as_object_mut() {
        for key in OWN_EDGE_KEYS {
            map.remove(key);
        }
    }
    json!({ "edges": edges, "haptics": config.haptics, "locate": config.locate,
        "enabled": config.enabled, "trigger_device": config.trigger_device, "follow": config.follow,
        "keep_wheel_mode": config.keep_wheel_mode, "buttons": config.buttons, "hotkeys": config.hotkeys })
}

/// Puts the master's effect settings over a sub's own, keeping its channels and `edges.on`.
pub(crate) fn apply_app_settings(config: &mut Config, app: &Value) {
    macro_rules! read { ($($field:ident),*) => { $(
        if let Some(value) = app.get(stringify!($field)).and_then(|v| serde_json::from_value(v.clone()).ok()) { config.$field = value; }
    )* }; }
    read!(enabled, trigger_device, follow, keep_wheel_mode, buttons, hotkeys);
    if let Some(Value::Object(master)) = app.get("edges") {
        let mut edges = serde_json::to_value(&config.edges).unwrap_or_default();
        if let Some(map) = edges.as_object_mut() {
            for (key, value) in master {
                if !OWN_EDGE_KEYS.contains(&key.as_str()) {
                    map.insert(key.clone(), value.clone());
                }
            }
        }
        if let Ok(edges) = serde_json::from_value(edges) {
            config.edges = edges;
        }
    }
    if let Some(haptics) = app.get("haptics").and_then(|v| serde_json::from_value(v.clone()).ok()) {
        config.haptics = haptics;
    }
    if let Some(locate) = app.get("locate").and_then(|v| serde_json::from_value(v.clone()).ok()) {
        config.locate = locate;
    }
}

/// What this computer is doing on the network, for the header and the tray menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetState {
    Off,
    Idle,
    /// Controlling the computer of this name.
    Controlling(String),
    /// Being controlled by the computer of this name.
    Controlled(String),
}

impl NetState {
    /// From the settings and the service's status (None: no status yet).
    pub fn of(config: &Config, status: Option<&flock_net::Status>) -> NetState {
        let Some(status) = status.filter(|s| s.running && config.network.on && config.network.notice_accepted) else {
            return NetState::Off;
        };
        let name = || {
            let id = status.state.peer.as_deref().unwrap_or_default();
            config.network.peers.iter().find(|p| p.id == id).map_or_else(|| id.to_string(), |p| p.name.clone())
        };
        match status.state.mode.as_str() {
            "controlling" => NetState::Controlling(name()),
            "controlled" => NetState::Controlled(name()),
            _ => NetState::Idle,
        }
    }
}

impl NetState {
    /// The line the header and the tray menu show.
    pub fn text(&self, t: &crate::i18n::Texts) -> String {
        match self {
            NetState::Off => t.net_state_off.to_string(),
            NetState::Idle => t.net_state_idle.to_string(),
            NetState::Controlling(name) => format!("{}{name}{}", t.net_state_controlling_pre, t.net_state_controlling_post),
            NetState::Controlled(name) => format!("{}{name}{}", t.net_state_controlled_pre, t.net_state_controlled_post),
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn effect_settings_follow_the_master() {
        let mut master = Config::default();
        master.edges.anim_style = crate::config::EdgeAnimStyle::Bar;
        master.locate.on = true;
        let app = app_settings(&master);
        let mut sub = Config::default();
        apply_app_settings(&mut sub, &app);
        assert_eq!(sub.edges.anim_style, crate::config::EdgeAnimStyle::Bar);
        assert_eq!(sub.edges.effects, master.edges.effects);
        assert_eq!(sub.locate, master.locate);
        // Haptics follow the master, but local edge channels and ownership remain local.
        let mut old = app.clone();
        old["haptics"] = json!({ "intensity": 60 });
        old["edges"]["left"] = json!(2);
        let mut sub = Config::default();
        apply_app_settings(&mut sub, &old);
        assert_eq!(sub.edges, master.edges);
        assert_eq!(sub.haptics.intensity, 60);
        master.enabled = false;
        master.hotkeys.on = true;
        master.trigger_device = "B042".into();
        master.follow = vec!["MX Keys".into()];
        master.keep_wheel_mode = false;
        master.buttons.gesture.on = true;
        sub.edges.on = true; sub.edges.right = 3;
        apply_app_settings(&mut sub, &app_settings(&master));
        assert!(!sub.enabled && sub.hotkeys.on && !sub.keep_wheel_mode);
        assert_eq!(sub.buttons, master.buttons);
        assert_eq!(sub.follow, master.follow);
        assert_eq!(sub.trigger_device, master.trigger_device);
        assert!(sub.edges.on && sub.edges.right == 3);
    }
    #[test]
    fn network_state_text_source() {
        let mut config = Config::default();
        config.network.on = true;
        config.network.notice_accepted = true;
        config.network.peers.push(flock_net::PeerConfig { id: "p1".into(), name: "Laptop".into(), ..Default::default() });
        let status = |mode: &str, peer: Option<&str>| flock_net::Status {
            running: true,
            me: flock_net::MeStatus { id: String::new(), name: String::new(), port: 0, error: None },
            discovered: vec![],
            peers: vec![],
            state: flock_net::StateStatus { mode: mode.into(), peer: peer.map(Into::into) },
            pairing: None,
            transfer: None,
            following: None,
            layout: vec![],
            reconnecting: false,
            keys: Default::default(),
        };
        assert_eq!(NetState::of(&config, None), NetState::Off);
        assert_eq!(NetState::of(&config, Some(&status("local", None))), NetState::Idle);
        assert_eq!(NetState::of(&config, Some(&status("controlling", Some("p1")))), NetState::Controlling("Laptop".into()));
        assert_eq!(NetState::of(&config, Some(&status("controlled", Some("p1")))), NetState::Controlled("Laptop".into()));
        config.network.on = false;
        assert_eq!(NetState::of(&config, Some(&status("local", None))), NetState::Off);
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;
    #[test]
    fn consent_fixed_limit_and_backend_edge_driver() {
        let mut c = Config::default();
        c.network.on = true;
        c.network.clipboard_limit_mb = 1;
        assert!(!runtime_config(&c, false, PathBuf::new()).on);
        c.network.notice_accepted = true;
        for wayland in [false, true] {
            let net = runtime_config(&c, wayland, PathBuf::from("local"));
            assert!(net.on);
            assert_eq!(net.clipboard_limit_mb, 100);
            assert_eq!(net.app_drives_edges, !wayland);
            assert_eq!(net.config_dir, PathBuf::from("local"));
            assert!(net.app_settings.get("buttons").is_some());
        }
    }
    #[test]
    fn linux_preferences_and_edge_owner_stay_local() {
        let mut sub = Config::default();
        sub.language = "en".into();
        sub.edges.on = true;
        sub.edges.left = 3;
        sub.network.role = Role::Sub;
        let mut master = Config::default();
        master.language = "ko".into();
        master.edges.on = false;
        master.edges.left = 2;
        master.edges.corner_margin = 150;
        apply_app_settings(&mut sub, &app_settings(&master));
        assert_eq!(sub.language, "en");
        assert_eq!(sub.network.role, Role::Sub);
        assert!(sub.edges.on);
        assert_eq!(sub.edges.left, 3);
        assert_eq!(sub.edges.corner_margin, 150);
    }
}
