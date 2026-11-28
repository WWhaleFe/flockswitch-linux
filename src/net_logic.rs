//! Pure network settings and presentation, shared with host tests.
use crate::config::Config;
use flock_net::{NetConfig, Role};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Apply the settings received from the master.
pub fn apply_master_settings(config: &mut Config, settings: &flock_net::MasterSettings) {
    settings.apply_to(&mut config.network);
    if let Some(pending) = &config.pending_key_rules {
        // The master echoes its whole table: only the pairs we sent must match.
        if pending.iter().all(|(pair, rule)| settings.keys.get(pair) == Some(rule)) {
            config.pending_key_rules = None;
        } else {
            // An older response must not consume a newer edit, or replace it locally.
            config.network.keys.extend(pending.clone());
        }
    }
    apply_app_settings(config, &settings.app);
}

/// The name to show for the computer `id`: the live status (also for a sub known only
/// through the master), else the configured peer name. None when neither has one.
pub fn peer_name(net: &NetConfig, status: Option<&flock_net::Status>, id: &str) -> Option<String> {
    status
        .and_then(|s| s.peers.iter().find(|p| p.id == id))
        .map(|p| p.name.clone())
        .filter(|n| !n.is_empty())
        .or_else(|| net.peers.iter().find(|p| p.id == id).map(|p| p.name.clone()).filter(|n| !n.is_empty()))
}

/// Whether the cursor can cross over `side` now: this computer may send, is in control,
/// and a connected, paired computer sits on that side of the layout (a master's sub or,
/// on a sub, another sub).
pub fn side_ready(status: &flock_net::Status, side: flock_net::Side) -> bool {
    let next = flock_net::layout::neighbour_on_side(&status.layout, &status.me.id, side);
    status.running
        && status.me.can_send
        && status.state.mode == "local"
        && next.is_some_and(|id| status.peers.iter().any(|s| s.id == id && s.connected && !s.paused))
}

/// The master unpaired or was unpaired: forget the computer `id` and, when no master is
/// left to receive them, the key rule edits waiting for it.
pub fn forget_peer(config: &mut Config, id: &str) {
    config.network.peers.retain(|p| p.id != id);
    config.network.layout.retain(|cell| cell.id != id);
    if keys_undeliverable(config, None) {
        config.pending_key_rules = None;
    }
}

/// Whether pending key rule edits can never be delivered and must go: this is not a sub
/// with a master any more, or the connected master is a 2.0 one that ignores them.
pub fn keys_undeliverable(config: &Config, status: Option<&flock_net::Status>) -> bool {
    let Some(master) = config.network.peers.iter().find(|p| p.role == Role::Master) else { return true };
    config.network.role != Role::Sub
        || status.is_some_and(|s| s.peers.iter().any(|p| p.id == master.id && p.connected && !p.keyrules))
}

/// The whole pixels to move for `delta`, with the fraction kept in `rest` for the next
/// event: an X11 pointer sits on whole pixels, so slow motion would otherwise be lost.
pub fn whole_pixels(rest: &mut f64, delta: f64) -> f64 {
    let total = *rest + delta;
    let whole = total.trunc();
    *rest = total - whole;
    whole
}

/// Whether the desktop portals (InputCapture barriers) drive the edges: a Wayland session
/// that does not use the GNOME helper extension. With the helper, the app's own edge
/// watcher drives crossing, as on X11.
pub fn portal_edges(wayland: bool, helper: bool) -> bool {
    wayland && !helper
}

/// `portal_edges`: whether the portals, not the app's edge watcher, drive the edges.
pub fn runtime_config(full: &Config, portal_edges: bool, config_dir: PathBuf) -> NetConfig {
    let mut config = full.network.clone();
    config.config_dir = config_dir;
    config.on &= config.notice_accepted;
    config.app_drives_edges = !portal_edges;
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
        sub.pending_key_rules = Some([("linux>mac".to_string(), sub.network.keys["linux>mac"].clone())].into());
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
        sub.pending_key_rules = Some([("mac>linux".to_string(), sub.network.keys["mac>linux"].clone())].into());
        apply_master_settings(&mut sub, &old_ack);
        assert!(sub.pending_key_rules.is_some());
        assert!(!sub.network.keys["mac>linux"].capture);
    }

    #[test]
    fn only_the_pending_pairs_are_checked_against_the_echo() {
        let mut sub = Config::default();
        sub.network.keys = sub.network.all_rules();
        sub.network.keys.get_mut("linux>mac").unwrap().capture = false;
        sub.pending_key_rules = Some([("linux>mac".to_string(), sub.network.keys["linux>mac"].clone())].into());
        let mut echo = flock_net::MasterSettings::of(&sub.network, "master");
        let other = echo.keys.get_mut("mac>windows").unwrap();
        other.capture = !other.capture;
        apply_master_settings(&mut sub, &echo);
        assert!(sub.pending_key_rules.is_none());
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
            peer_name(&config.network, Some(status), id).unwrap_or_else(|| id.to_string())
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
    fn slow_motion_adds_up_to_whole_pixels() {
        let mut rest = 0.0;
        let moved: f64 = (0..12).map(|_| whole_pixels(&mut rest, 0.25)).sum();
        assert_eq!(moved, 3.0);
        let mut rest = 0.0;
        let moved: f64 = (0..12).map(|_| whole_pixels(&mut rest, -0.25)).sum();
        assert_eq!(moved, -3.0);
        let mut rest = 0.5;
        assert_eq!(whole_pixels(&mut rest, 1.75), 2.0);
        assert_eq!(rest, 0.25);
    }
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
            me: flock_net::MeStatus { id: String::new(), name: String::new(), port: 0, error: None, can_send: true, displays: vec![] },
            discovered: vec![],
            peers: vec![],
            state: flock_net::StateStatus { mode: mode.into(), peer: peer.map(Into::into) },
            pairing: None,
            transfer: None,
            following: None,
            layout: vec![],
            reconnecting: false,
            keys: Default::default(), key_check: None,
        };
        assert_eq!(NetState::of(&config, None), NetState::Off);
        assert_eq!(NetState::of(&config, Some(&status("local", None))), NetState::Idle);
        assert_eq!(NetState::of(&config, Some(&status("controlling", Some("p1")))), NetState::Controlling("Laptop".into()));
        assert_eq!(NetState::of(&config, Some(&status("controlled", Some("p1")))), NetState::Controlled("Laptop".into()));
        config.network.on = false;
        assert_eq!(NetState::of(&config, Some(&status("local", None))), NetState::Off);
    }

    fn peer_status(id: &str, name: &str, connected: bool, keyrules: bool) -> flock_net::PeerStatus {
        flock_net::PeerStatus { id: id.into(), name: name.into(), os: None, connected, rtt_ms: None, can_send: true, via_master: true, keyrules, displays: vec![], paused: false }
    }
    fn live(peers: Vec<flock_net::PeerStatus>, mode: &str, peer: Option<&str>) -> flock_net::Status {
        flock_net::Status {
            running: true,
            me: flock_net::MeStatus { id: "me".into(), name: String::new(), port: 0, error: None, can_send: true, displays: vec![] },
            discovered: vec![],
            peers,
            state: flock_net::StateStatus { mode: mode.into(), peer: peer.map(Into::into) },
            pairing: None,
            transfer: None,
            following: None,
            layout: vec![
                flock_net::Cell::new("me", 1, 1),
                flock_net::Cell::new("p2", 2, 1),
            ],
            reconnecting: false,
            keys: Default::default(), key_check: None,
        }
    }

    #[test]
    fn names_come_from_the_roster_before_the_config_before_the_id() {
        let mut config = Config::default();
        config.network.on = true;
        config.network.notice_accepted = true;
        config.network.peers.push(flock_net::PeerConfig { id: "p1".into(), name: "Laptop".into(), ..Default::default() });
        let status = live(vec![peer_status("p1", "Renamed", true, true), peer_status("p2", "Desk", true, true), peer_status("p3", "", true, true)], "controlling", Some("p2"));
        assert_eq!(NetState::of(&config, Some(&status)), NetState::Controlling("Desk".into()));
        assert_eq!(peer_name(&config.network, Some(&status), "p1").as_deref(), Some("Renamed"));
        assert_eq!(peer_name(&config.network, Some(&status), "p3"), None);
        assert_eq!(peer_name(&config.network, None, "p1").as_deref(), Some("Laptop"));
        let unknown = live(vec![], "controlled", Some("zz"));
        assert_eq!(NetState::of(&config, Some(&unknown)), NetState::Controlled("zz".into()));
    }

    #[test]
    fn crossing_needs_the_right_to_send_and_a_live_neighbour() {
        let mut status = live(vec![peer_status("p2", "Desk", true, true)], "local", None);
        assert!(side_ready(&status, flock_net::Side::Right));
        assert!(!side_ready(&status, flock_net::Side::Left));
        status.me.can_send = false;
        assert!(!side_ready(&status, flock_net::Side::Right));
        status.me.can_send = true;
        status.peers[0].connected = false;
        assert!(!side_ready(&status, flock_net::Side::Right));
    }

    #[test]
    fn unpairing_drops_the_peer_its_cell_and_the_edits_waiting_for_the_master() {
        let mut sub = Config::default();
        sub.network.role = Role::Sub;
        sub.network.peers.push(flock_net::PeerConfig { id: "master".into(), role: Role::Master, ..Default::default() });
        sub.network.peers.push(flock_net::PeerConfig { id: "other".into(), role: Role::Sub, ..Default::default() });
        sub.network.layout.push(flock_net::Cell::new("master", 0, 0));
        sub.network.layout.push(flock_net::Cell::new("other", 1, 0));
        sub.pending_key_rules = Some(sub.network.all_rules());
        forget_peer(&mut sub, "other");
        assert!(sub.pending_key_rules.is_some() && sub.network.layout.len() == 1);
        forget_peer(&mut sub, "master");
        assert!(sub.pending_key_rules.is_none() && sub.network.peers.is_empty() && sub.network.layout.is_empty());
    }

    #[test]
    fn edits_are_undeliverable_for_a_non_sub_or_a_keyrules_less_master() {
        let mut sub = Config::default();
        sub.network.role = Role::Sub;
        assert!(keys_undeliverable(&sub, None));
        sub.network.peers.push(flock_net::PeerConfig { id: "m".into(), role: Role::Master, ..Default::default() });
        assert!(!keys_undeliverable(&sub, None));
        assert!(!keys_undeliverable(&sub, Some(&live(vec![peer_status("m", "M", true, true)], "local", None))));
        assert!(!keys_undeliverable(&sub, Some(&live(vec![peer_status("m", "M", false, false)], "local", None))));
        assert!(keys_undeliverable(&sub, Some(&live(vec![peer_status("m", "M", true, false)], "local", None))));
        sub.network.role = Role::Master;
        assert!(keys_undeliverable(&sub, None));
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
    fn the_app_drives_the_edges_unless_the_portals_do() {
        // X11, and Wayland with the GNOME helper: the app's edge watcher crosses.
        assert!(!portal_edges(false, false));
        assert!(!portal_edges(false, true));
        assert!(!portal_edges(true, true));
        // Wayland through the portals: the barriers do.
        assert!(portal_edges(true, false));
        let mut c = Config::default();
        c.network.on = true;
        c.network.notice_accepted = true;
        for (wayland, helper, drives) in [(false, false, true), (true, false, false), (true, true, true)] {
            let net = runtime_config(&c, portal_edges(wayland, helper), PathBuf::new());
            assert_eq!(net.app_drives_edges, drives, "wayland {wayland}, helper {helper}");
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

/// A portal restore token as read from its file: trimmed, none when the file is empty.
/// An empty token would make the portal refuse the whole request.
pub fn parse_restore_token(text: &str) -> Option<String> {
    let token = text.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// The saved restore token of a portal ("remote-desktop.token", "input-capture.token").
pub fn load_restore_token(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path).ok().as_deref().and_then(parse_restore_token)
}

/// What becomes of the token file once a portal session has started, or failed to.
#[derive(Debug, PartialEq, Eq)]
pub enum TokenAfterStart {
    /// The portal returned a token: keep it for the next session (the same text when it
    /// restored the old one, a new one after the user answered).
    Save(String),
    /// The saved token is spent: the portal deletes a token's answer as soon as a session
    /// asks with it, and keeps it again only when that session starts with "remember".
    Remove,
    /// Nothing to change.
    Keep,
}

/// `used`: the token the session asked with; `started`: Start answered with success;
/// `returned`: the token in that answer.
pub fn token_after_start(used: Option<&str>, started: bool, returned: Option<&str>) -> TokenAfterStart {
    match (started, returned) {
        (true, Some(token)) if !token.trim().is_empty() => TokenAfterStart::Save(token.trim().to_string()),
        // Started without a token: the user unticked "remember", or the portal cannot
        // remember. Started after failing (cancelled, closed, cut off): the token was
        // looked up, so it is gone on the portal's side either way.
        _ if used.is_some() => TokenAfterStart::Remove,
        _ => TokenAfterStart::Keep,
    }
}

/// Applies `action` to the token file at `path` (owner-only, replaced in one step).
pub fn apply_token(path: &std::path::Path, action: &TokenAfterStart) -> std::io::Result<()> {
    match action {
        TokenAfterStart::Save(token) => {
            use std::io::Write;
            #[cfg(unix)]
            use std::os::unix::fs::OpenOptionsExt;
            let tmp = path.with_extension("token.new");
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut f = options.open(&tmp)?;
            f.write_all(token.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        }
        TokenAfterStart::Remove => match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        },
        TokenAfterStart::Keep => Ok(()),
    }
}

/// Words for the log: what happened to a portal's remembered answer.
pub fn token_log(portal: &str, used: bool, action: &TokenAfterStart) -> String {
    match action {
        TokenAfterStart::Save(_) if used => format!("{portal}: restore token saved again (the remembered answer stays; no question next time)"),
        TokenAfterStart::Save(_) => format!("{portal}: new restore token saved (the answer is remembered; no question next time)"),
        TokenAfterStart::Remove => format!("{portal}: no restore token came back, the saved one is spent and removed (the system asks next time)"),
        TokenAfterStart::Keep => format!("{portal}: no restore token (the answer is not remembered; the system asks next time)"),
    }
}

/// Whether the network may ask for the RemoteDesktop permission again after the user
/// cancelled or denied it: only when they ask for it (looking for computers again).
/// Settings written while sharing runs (a computer's name, the master's effect settings,
/// key rules) must not bring the system's question back. Turning sharing off and on
/// clears the block elsewhere.
pub fn ask_again_after_deny(reconnect_asked: bool) -> bool {
    reconnect_asked
}

#[cfg(test)]
mod token_tests {
    use super::*;

    #[test]
    fn empty_or_padded_token_files() {
        assert_eq!(parse_restore_token(""), None);
        assert_eq!(parse_restore_token(" \n"), None);
        assert_eq!(parse_restore_token("abc\n").as_deref(), Some("abc"));
    }

    #[test]
    fn newest_token_is_kept_and_spent_ones_go() {
        assert_eq!(token_after_start(None, true, Some("new")), TokenAfterStart::Save("new".into()));
        assert_eq!(token_after_start(Some("old"), true, Some("old")), TokenAfterStart::Save("old".into()));
        assert_eq!(token_after_start(Some("old"), true, Some("new")), TokenAfterStart::Save("new".into()));
        // "Remember" unticked: the old token was looked up, so it is spent.
        assert_eq!(token_after_start(Some("old"), true, None), TokenAfterStart::Remove);
        // Cancelled, or cut off between SelectDevices and Start.
        assert_eq!(token_after_start(Some("old"), false, None), TokenAfterStart::Remove);
        assert_eq!(token_after_start(None, false, None), TokenAfterStart::Keep);
        assert_eq!(token_after_start(None, true, None), TokenAfterStart::Keep);
        assert_eq!(token_after_start(None, true, Some("  ")), TokenAfterStart::Keep);
    }

    #[test]
    fn token_file_round_trip() {
        let dir = std::env::temp_dir().join(format!("flockswitch-token-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("remote-desktop.token");
        apply_token(&path, &TokenAfterStart::Save("t1".into())).unwrap();
        assert_eq!(load_restore_token(&path).as_deref(), Some("t1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        apply_token(&path, &TokenAfterStart::Save("t2".into())).unwrap();
        assert_eq!(load_restore_token(&path).as_deref(), Some("t2"));
        apply_token(&path, &TokenAfterStart::Keep).unwrap();
        assert_eq!(load_restore_token(&path).as_deref(), Some("t2"));
        apply_token(&path, &TokenAfterStart::Remove).unwrap();
        assert_eq!(load_restore_token(&path), None);
        apply_token(&path, &TokenAfterStart::Remove).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_looking_again_lifts_a_deny() {
        assert!(ask_again_after_deny(true));
        assert!(!ask_again_after_deny(false));
    }
}
