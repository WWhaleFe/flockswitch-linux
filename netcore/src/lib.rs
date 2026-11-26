//! Shared networking core. Config directories are supplied by the embedding app.
pub mod discovery;
pub mod geometry;
pub mod identity;
pub mod keymap;
pub mod layout;
pub mod linux_input;
pub mod pairing;
pub mod proto;
mod service;
pub mod tls;
pub mod transfer;
pub mod windows_input;
pub use geometry::Rect;
pub use keymap::{DirRules, HangulKey, ModMode};
pub use layout::{Cell, Member};
pub use proto::{InputEvent, MouseButton, Os, Side};
use serde::{Deserialize, Serialize};
pub use service::Service;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::mpsc::Sender,
};

/// Sharing only ever talks to computers on the local network: private IPv4 ranges,
/// link-local and unique-local IPv6, and loopback. Anything else is refused both ways.
pub fn is_local_network(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_local_network(v4.into());
            }
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xffc0) == 0xfe80 || (first & 0xfe00) == 0xfc00
        }
    }
}

/// The port sharing uses unless the user changes it.
pub const DEFAULT_PORT: u16 = 47160;

/// Why a port cannot be used for sharing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortProblem {
    /// Below 1024: reserved for system services.
    Reserved,
    /// Used by flockswitch itself, or by mDNS.
    Ours,
    /// Another program on this computer is listening on it.
    InUse,
}

/// Single-instance ports for the app and settings, plus mDNS.
pub const KEPT_PORTS: [u16; 3] = [5353, 47851, 47852];

/// Checks a port the user typed. `current` is the port sharing listens on now (which
/// is busy because of flockswitch itself, so it is not reported as in use).
pub fn check_port(port: u16, current: u16) -> Result<(), PortProblem> {
    if port < 1024 {
        return Err(PortProblem::Reserved);
    }
    if KEPT_PORTS.contains(&port) {
        return Err(PortProblem::Ours);
    }
    if port != current && std::net::TcpListener::bind(("0.0.0.0", port)).is_err() {
        return Err(PortProblem::InUse);
    }
    Ok(())
}

pub trait Platform: Send + Sync + 'static {
    fn os(&self) -> Os;
    fn displays(&self) -> Vec<Rect>;
    fn cursor(&self) -> Option<(f64, f64)>;
    fn warp(&self, x: f64, y: f64);
    /// Update barriers when connected peer edges change.
    fn arm_edges(&self, _sides: &[Side]) {}
    fn take_edge_hit(&self) -> Option<(Side, f64)> {
        None
    }
    fn place_on_return(&self, side: Side, pos: f64) {
        if let Some((x, y)) = geometry::edge_position(&self.displays(), side, pos) {
            self.warp(x, y);
        }
    }
    fn start_capture(&self, sink: Sender<InputEvent>) -> anyhow::Result<()>;
    /// Optional monotonic capture time for the event being consumed, in sink order.
    /// None uses the service's current time. Queue timestamps alongside the existing
    /// InputEvents when their original down/up timing must survive a delayed consumer.
    fn captured_at(&self, _event: &InputEvent) -> Option<std::time::Instant> { None }
    /// On a Caps Lock key-up only: consume the measured hold duration for that press.
    /// Mac capture that emits an immediate down/up pair supplies this hint to preserve
    /// long presses. None uses the down/up event times. No new InputEvent is required.
    /// See CAPS_LONG_PRESS.md for the capture contract and the legacy fallback.
    fn caps_hold_ms(&self) -> Option<u64> { None }
    fn stop_capture(&self);
    fn inject(&self, ev: &InputEvent);
    fn ready(&self) -> Result<(), String>;
    /// Optional native permission checks, published under me.permissions in net-status.
    fn permissions(&self) -> Option<Permissions> { None }
    /// Whether a Control key is held on this computer's own keyboard. Platforms that
    /// cannot tell say yes, so `require_control` never locks the cursor in.
    fn control_down(&self) -> bool {
        true
    }
    /// Text on this computer's clipboard, if any.
    fn clipboard_text(&self) -> Option<String> {
        None
    }
    fn set_clipboard_text(&self, _text: &str) {}
    /// Files copied in the file manager (Finder, Explorer), if that is what the clipboard holds.
    fn clipboard_files(&self) -> Vec<PathBuf> {
        Vec::new()
    }
    fn set_clipboard_files(&self, _files: &[PathBuf]) {}
    /// The clipboard's image as PNG, if it holds one.
    fn clipboard_image_png(&self) -> Option<Vec<u8>> {
        None
    }
    fn set_clipboard_image_png(&self, _png: &[u8]) {}
    /// Where received files are saved (made if missing). None: a "received" folder next
    /// to the settings.
    fn received_dir(&self) -> Option<PathBuf> {
        None
    }
}
pub struct Hooks {
    pub switch_logitech: Box<dyn Fn(u8) + Send + Sync>,
    pub on_paired: Box<dyn Fn(PeerConfig) + Send + Sync>,
    pub on_unpaired: Box<dyn Fn(String) + Send + Sync>,
    /// A saved peer changed (on a sub: the side the master placed it on). The app saves it.
    pub on_peer_changed: Box<dyn Fn(PeerConfig) + Send + Sync>,
    /// On a sub: the master's settings arrived or changed. The app applies them.
    pub on_master_settings: Box<dyn Fn(MasterSettings) + Send + Sync>,
    /// Optional fallible persistence before a master's key edit is acknowledged.
    pub persist_key_rules: Box<dyn Fn(&BTreeMap<String, DirRules>) -> anyhow::Result<()> + Send + Sync>,
    /// On a master: edited key rules were accepted and persisted.
    pub on_key_rules: Box<dyn Fn(BTreeMap<String, DirRules>) + Send + Sync>,
}
impl Default for Hooks {
    fn default() -> Self {
        Self {
            switch_logitech: Box::new(|_| {}),
            on_paired: Box::new(|_| {}),
            on_unpaired: Box::new(|_| {}),
            on_peer_changed: Box::new(|_| {}),
            on_master_settings: Box::new(|_| {}),
            on_key_rules: Box::new(|_| {}),
            persist_key_rules: Box::new(|_| Ok(())),
        }
    }
}

/// A computer's part in sharing. The master pairs with up to `MAX_SUBS` subs, places them
/// around its screen and hands them its settings; a sub pairs with one master only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    #[default]
    Master,
    Sub,
}
/// Most subs one master takes.
pub const MAX_SUBS: usize = 3;

/// The settings a master hands to its subs, which use them instead of their own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MasterSettings {
    pub master_name: String,
    pub require_control: bool,
    pub swap_cmd_ctrl: bool,
    #[serde(default = "enabled_by_default")]
    pub mac_capslock_to_hangul: bool,
    #[serde(default = "enabled_by_default")]
    pub mac_shift_space_to_hangul: bool,
    pub lang1_on_mac: String,
    pub share_clipboard: bool,
    pub clipboard_limit_mb: u32,
    pub corner_margin: f64,
    pub cross_delay_ms: u64,
    /// Key rules per OS pair (2.1). Empty: the 2.0 fields above stand for them.
    pub keys: BTreeMap<String, DirRules>,
    /// Let clipboard content pass on from one sub to another.
    pub clipboard_relay: bool,
    /// The app's own effect settings (countdown, haptics, cursor highlight), opaque here.
    pub app: serde_json::Value,
}
fn enabled_by_default() -> bool { true }

impl Default for MasterSettings {
    fn default() -> Self {
        // A 2.0 master sends no `keys`: empty means its 2.0 fields stand for the rules.
        Self { keys: BTreeMap::new(), ..Self::of(&NetConfig::default(), "") }
    }
}
impl MasterSettings {
    pub fn of(config: &NetConfig, master_name: &str) -> Self {
        // 2.0 subs only read the old fields, so they mirror the rules they stand for.
        let mut old = config.clone();
        if let Some(r) = config.keys.get("mac>windows") {
            old.swap_cmd_ctrl = r.mod_mode != ModMode::Off;
            old.mac_capslock_to_hangul = r.hangul_src.contains(&HangulKey::CapsLock);
            old.mac_shift_space_to_hangul = r.hangul_src.contains(&HangulKey::ShiftSpace);
        }
        if let Some(r) = config.keys.get("windows>mac") {
            old.lang1_on_mac = if r.hangul_src.contains(&HangulKey::Lang1) { "capslock" } else { "off" }.into();
        }
        Self {
            master_name: master_name.into(),
            require_control: config.require_control,
            swap_cmd_ctrl: old.swap_cmd_ctrl,
            mac_capslock_to_hangul: old.mac_capslock_to_hangul,
            mac_shift_space_to_hangul: old.mac_shift_space_to_hangul,
            lang1_on_mac: old.lang1_on_mac,
            keys: config.all_rules(),
            clipboard_relay: config.clipboard_relay,
            share_clipboard: config.share_clipboard,
            clipboard_limit_mb: config.clipboard_limit_mb,
            corner_margin: config.corner_margin,
            cross_delay_ms: config.cross_delay_ms,
            app: config.app_settings.clone(),
        }
    }
    /// Puts the master's values over a sub's own.
    pub fn apply_to(&self, config: &mut NetConfig) {
        config.require_control = self.require_control;
        config.swap_cmd_ctrl = self.swap_cmd_ctrl;
        config.mac_capslock_to_hangul = self.mac_capslock_to_hangul;
        config.mac_shift_space_to_hangul = self.mac_shift_space_to_hangul;
        config.lang1_on_mac = self.lang1_on_mac.clone();
        config.keys = self.keys.clone();
        config.clipboard_relay = self.clipboard_relay;
        config.share_clipboard = self.share_clipboard;
        config.clipboard_limit_mb = self.clipboard_limit_mb;
        config.corner_margin = self.corner_margin;
        config.cross_delay_ms = self.cross_delay_ms;
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetConfig {
    pub on: bool,
    pub name: String,
    pub port: u16,
    pub swap_cmd_ctrl: bool,
    #[serde(default = "enabled_by_default")]
    pub mac_capslock_to_hangul: bool,
    #[serde(default = "enabled_by_default")]
    pub mac_shift_space_to_hangul: bool,
    pub lang1_on_mac: String,
    pub cross_delay_ms: u64,
    pub peers: Vec<PeerConfig>,
    /// On the master: the layout grid, the master included under its own id. Empty: built
    /// from the peers' 2.0 sides.
    pub layout: Vec<Cell>,
    /// Key rules per OS pair ("mac>windows"). A pair missing here follows the 2.0 fields.
    pub keys: BTreeMap<String, DirRules>,
    /// Let clipboard content pass on from one sub to another.
    pub clipboard_relay: bool,
    pub discovery: bool,
    pub corner_margin: f64,
    /// Cross to another computer (and back) only while Control is held.
    pub require_control: bool,
    /// The user has read and agreed to the network cautions. Until then sharing stays
    /// off however `on` is set: nothing listens, dials or is discovered.
    pub notice_accepted: bool,
    /// Carry copied text, images and files along when the cursor moves to another computer.
    pub share_clipboard: bool,
    /// Size limit for images and files carried along, in MB (0 = text only).
    pub clipboard_limit_mb: u32,
    /// This computer's part: master (up to three subs) or sub (one master).
    pub role: Role,
    /// On a master: the app's effect settings handed to subs. Set by the app at run time.
    #[serde(skip)]
    pub app_settings: serde_json::Value,
    /// The app watches the edges itself (countdown, haptics) and calls `Service::cross`;
    /// the service then only decides the way back. Set by the app at run time.
    #[serde(skip)]
    pub app_drives_edges: bool,
    /// Runtime location, excluded from the app's [network] config.
    #[serde(skip)]
    pub config_dir: PathBuf,
}
impl Default for NetConfig {
    fn default() -> Self {
        Self {
            on: false,
            name: String::new(),
            port: 47160,
            swap_cmd_ctrl: true,
            mac_capslock_to_hangul: true,
            mac_shift_space_to_hangul: true,
            lang1_on_mac: "capslock".into(),
            cross_delay_ms: 0,
            peers: Vec::new(),
            layout: Vec::new(),
            keys: BTreeMap::new(),
            clipboard_relay: true,
            discovery: true,
            corner_margin: 80.0,
            require_control: false,
            notice_accepted: false,
            share_clipboard: true,
            clipboard_limit_mb: transfer::DEFAULT_LIMIT_MB,
            app_drives_edges: false,
            role: Role::Master,
            app_settings: serde_json::Value::Null,
            config_dir: PathBuf::new(),
        }
    }
}
impl NetConfig {
    /// The key rules for events leaving a `src` computer for a `dst` one; None between
    /// two computers of the same OS.
    pub fn rules_for(&self, src: Os, dst: Os) -> Option<DirRules> {
        if src == dst {
            return None;
        }
        Some(self.keys.get(&keymap::pair_key(src, dst)).cloned().unwrap_or_else(|| DirRules::from_legacy(src, dst, self)))
    }
    /// The rules in effect for every direction.
    pub fn all_rules(&self) -> BTreeMap<String, DirRules> {
        keymap::all_pairs()
            .into_iter()
            .filter_map(|(s, d)| self.rules_for(s, d).map(|r| (keymap::pair_key(s, d), r)))
            .collect()
    }
    /// Writes the rules the 2.0 fields stand for into `keys`, for every direction not set.
    pub fn migrate_keys(&mut self) {
        self.keys = self.all_rules();
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerConfig {
    pub id: String,
    pub name: String,
    pub os: Os,
    pub fingerprint: String,
    /// None is serialized as "off" in config files.
    #[serde(with = "peer_side")]
    pub side: Option<Side>,
    pub logitech_channel: u8,
    pub last_addr: String,
    /// The peer's part (a master's peers are subs, a sub's peer is its master).
    #[serde(default)]
    pub role: Role,
    /// Master only: this computer's mouse and keyboard may be used to drive the others.
    pub can_send: bool,
    /// A sub known only from its master's roster (runtime, never saved or shown as paired).
    pub via_master: bool,
}
impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            os: Os::Mac,
            fingerprint: String::new(),
            side: None,
            logitech_channel: 0,
            last_addr: String::new(),
            role: Role::Master,
            can_send: true,
            via_master: false,
        }
    }
}
mod peer_side {
    use super::*;
    pub fn serialize<S: serde::Serializer>(side: &Option<Side>, s: S) -> Result<S::Ok, S::Error> {
        match side {
            Some(side) => side.serialize(s),
            None => s.serialize_str("off"),
        }
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Side>, D::Error> {
        let s = String::deserialize(d)?;
        match s.as_str() {
            "off" => Ok(None),
            "left" => Ok(Some(Side::Left)),
            "right" => Ok(Some(Side::Right)),
            "top" => Ok(Some(Side::Top)),
            "bottom" => Ok(Some(Side::Bottom)),
            _ => Err(serde::de::Error::custom("invalid peer side")),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub running: bool,
    pub me: MeStatus,
    pub discovered: Vec<DiscoveredStatus>,
    pub peers: Vec<PeerStatus>,
    pub state: StateStatus,
    pub pairing: Option<PairingStatus>,
    /// A clipboard transfer going on, or the last one's outcome.
    #[serde(default)]
    pub transfer: Option<TransferStatus>,
    /// On a sub: the master whose settings are in use.
    #[serde(default)]
    pub following: Option<String>,
    /// The layout grid in effect.
    #[serde(default)]
    pub layout: Vec<Cell>,
    /// A reconnect is under way (after `Service::reconnect` or a wake-up).
    #[serde(default)]
    pub reconnecting: bool,
    /// The key rules in effect, every direction.
    #[serde(default)]
    pub keys: BTreeMap<String, DirRules>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferStatus {
    /// "send" or "receive".
    pub direction: String,
    /// "files", "image" or "text".
    pub kind: String,
    pub done_bytes: u64,
    pub total_bytes: u64,
    /// "running", "done", "too_large" or "failed".
    pub stage: String,
    /// For received files: the folder they were saved in.
    pub saved_in: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permissions {
    pub accessibility: bool,
    pub input_monitoring: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeStatus {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveredStatus {
    pub id: String,
    pub name: String,
    pub os: Os,
    pub addr: String,
    pub paired: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub id: String,
    pub connected: bool,
    pub rtt_ms: Option<f64>,
    #[serde(default = "layout::yes")]
    pub can_send: bool,
    /// Known only through the master's roster (a sub-to-sub link).
    #[serde(default)]
    pub via_master: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateStatus {
    pub mode: String,
    pub peer: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairingStatus {
    pub peer_name: String,
    pub code: String,
    pub stage: String,
    /// Why pairing was refused: "both_master", "both_sub", "full" (the master has three
    /// subs) or "has_master" (the sub already has one).
    #[serde(default)]
    pub reason: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Request {
    Pair { pair: String },
    PairAddr { pair_addr: String },
    Accept { accept: bool },
    Unpair { unpair: String },
    /// `{"action":"reconnect"}`: same as `Service::reconnect`.
    Action { action: String },
}
pub fn status_path(config_dir: impl AsRef<Path>) -> PathBuf {
    config_dir.as_ref().join("flockswitch/net-status.json")
}
pub fn request_path(config_dir: impl AsRef<Path>) -> PathBuf {
    config_dir.as_ref().join("flockswitch/net-request.json")
}
pub fn read_status(config_dir: impl AsRef<Path>) -> anyhow::Result<Status> {
    Ok(serde_json::from_slice(&fs::read(status_path(config_dir))?)?)
}
pub fn write_request(config_dir: impl AsRef<Path>, request: &Request) -> anyhow::Result<()> {
    write_json(&request_path(config_dir), request)
}
pub fn take_request(config_dir: impl AsRef<Path>) -> anyhow::Result<Option<Request>> {
    let path = request_path(config_dir);
    // Claim the file before reading so a concurrent replacement is not deleted.
    let claimed = path.with_extension("reading");
    match fs::rename(&path, &claimed) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let bytes = fs::read(&claimed);
    let _ = fs::remove_file(&claimed);
    Ok(Some(serde_json::from_slice(&bytes?)?))
}
pub fn write_status(config_dir: impl AsRef<Path>, status: &Status) -> anyhow::Result<()> {
    write_status_with_permissions(config_dir, status, None)
}
/// Native apps may attach permission checks without changing the shared status schema.
pub fn write_status_with_permissions(config_dir: impl AsRef<Path>, status: &Status, permissions: Option<Permissions>) -> anyhow::Result<()> {
    let mut value = serde_json::to_value(status)?;
    if let Some(permissions) = permissions {
        value["me"]["permissions"] = serde_json::to_value(permissions)?;
    }
    write_json(&status_path(config_dir), &value)
}
fn write_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(value)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_local_network_addresses() {
        for ip in ["192.168.1.185", "10.0.0.2", "172.20.1.1", "169.254.3.4", "127.0.0.1", "fe80::1", "fd12::5", "::1", "::ffff:192.168.0.9"] {
            assert!(is_local_network(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "172.32.0.1", "100.64.0.1", "2001:db8::1", "::ffff:1.1.1.1"] {
            assert!(!is_local_network(ip.parse().unwrap()), "{ip}");
        }
    }
    #[test]
    fn port_checks() {
        assert_eq!(check_port(80, DEFAULT_PORT), Err(PortProblem::Reserved));
        assert_eq!(check_port(47851, DEFAULT_PORT), Err(PortProblem::Ours));
        assert_eq!(check_port(5353, DEFAULT_PORT), Err(PortProblem::Ours));
        assert_eq!(check_port(DEFAULT_PORT, DEFAULT_PORT), Ok(()));
        let busy = match std::net::TcpListener::bind(("0.0.0.0", 0)) {
            Ok(busy) => busy,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!("port_checks: socket assertions unavailable: {e}");
                return;
            }
            Err(e) => panic!("binding test port: {e}"),
        };
        let port = busy.local_addr().unwrap().port();
        assert_eq!(check_port(port, DEFAULT_PORT), Err(PortProblem::InUse));
        assert_eq!(check_port(port, port), Ok(()));
        drop(busy);
        assert_eq!(check_port(port, DEFAULT_PORT), Ok(()));
    }
    #[test]
    fn notice_must_be_accepted() {
        assert!(!NetConfig::default().notice_accepted);
    }
    #[test]
    fn config_defaults_and_side() {
        let cfg: NetConfig = serde_json::from_str("{}").unwrap();
        assert!(!cfg.on);
        assert_eq!(cfg.port, 47160);
        assert!(cfg.swap_cmd_ctrl);
        let p: PeerConfig = serde_json::from_str(r#"{"side":"off","os":"windows"}"#).unwrap();
        assert_eq!(p.side, None);
        assert_eq!(serde_json::to_value(p).unwrap()["side"], "off");
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    #[test]
    fn permission_status_preserves_legacy_readers_and_separate_checks() {
        let dir = std::env::temp_dir().join(format!("flock-permissions-{}", std::process::id()));
        let status: Status = serde_json::from_value(serde_json::json!({
            "running": true, "me": {"id": "test", "name": "Mac", "port": 47160, "error": null},
            "discovered": [], "peers": [], "state": {"mode": "local", "peer": null},
            "pairing": null, "transfer": null, "following": null
        })).unwrap();
        for accessibility in [false, true] {
            for input_monitoring in [false, true] {
                let permissions = Permissions { accessibility, input_monitoring };
                write_status_with_permissions(&dir, &status, Some(permissions)).unwrap();
                let value: serde_json::Value = serde_json::from_slice(&fs::read(status_path(&dir)).unwrap()).unwrap();
                assert_eq!(serde_json::from_value::<Permissions>(value["me"]["permissions"].clone()).unwrap(), permissions);
                assert_eq!(read_status(&dir).unwrap(), status);
            }
        }
        write_status(&dir, &status).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&fs::read(status_path(&dir)).unwrap()).unwrap();
        assert!(value["me"].get("permissions").is_none());
        fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod hangul_settings_tests {
    use super::*;
    #[test]
    fn defaults_old_packets_and_master_propagation() {
        let old: NetConfig = serde_json::from_str("{}").unwrap();
        assert!(old.mac_capslock_to_hangul && old.mac_shift_space_to_hangul);
        let old: MasterSettings = serde_json::from_str("{}").unwrap();
        assert!(old.mac_capslock_to_hangul && old.mac_shift_space_to_hangul);
        let master = NetConfig { mac_capslock_to_hangul: false, mac_shift_space_to_hangul: false, ..NetConfig::default() };
        let settings = MasterSettings::of(&master, "Mac");
        let packet = serde_json::to_string(&settings).unwrap();
        let settings: MasterSettings = serde_json::from_str(&packet).unwrap();
        let mut sub = NetConfig::default();
        settings.apply_to(&mut sub);
        assert!(!sub.mac_capslock_to_hangul && !sub.mac_shift_space_to_hangul);
        MasterSettings::of(&NetConfig::default(), "Mac").apply_to(&mut sub);
        assert!(sub.mac_capslock_to_hangul && sub.mac_shift_space_to_hangul);
    }
}

#[cfg(test)]
mod v2_1_tests {
    use super::*;
    #[test]
    fn reconnect_action_request_and_old_requests_parse() {
        let r: Request = serde_json::from_str(r#"{"action":"reconnect"}"#).unwrap();
        assert_eq!(r, Request::Action { action: "reconnect".into() });
        assert_eq!(serde_json::from_str::<Request>(r#"{"pair":"x"}"#).unwrap(), Request::Pair { pair: "x".into() });
        assert_eq!(serde_json::from_str::<Request>(r#"{"accept":true}"#).unwrap(), Request::Accept { accept: true });
    }
    #[test]
    fn a_2_0_network_section_loads_with_new_defaults() {
        let old = r#"
            on = true
            name = "Desk"
            swap_cmd_ctrl = false
            lang1_on_mac = "off"
            mac_shift_space_to_hangul = false
            [[peers]]
            id = "abc"
            name = "PC"
            os = "windows"
            side = "right"
            logitech_channel = 2
            last_addr = "10.0.0.2:47160"
            role = "sub"
        "#;
        let cfg: NetConfig = toml::from_str(old).unwrap();
        assert!(cfg.layout.is_empty() && cfg.keys.is_empty() && cfg.clipboard_relay);
        assert!(cfg.peers[0].can_send && !cfg.peers[0].via_master);
        // The 2.0 fields stand for the rules.
        let r = cfg.rules_for(Os::Mac, Os::Windows).unwrap();
        assert_eq!(r.mod_mode, ModMode::Off);
        assert_eq!(r.hangul_src, vec![HangulKey::CapsLock]);
        assert!(cfg.rules_for(Os::Windows, Os::Mac).unwrap().hangul_src.is_empty());
        assert!(cfg.rules_for(Os::Mac, Os::Mac).is_none());
        assert_eq!(cfg.all_rules().len(), 9);
        // Materialising them keeps the behaviour and survives a TOML round trip.
        let mut migrated = cfg.clone();
        migrated.migrate_keys();
        assert_eq!(migrated.keys.len(), 9);
        assert_eq!(migrated.rules_for(Os::Mac, Os::Windows), cfg.rules_for(Os::Mac, Os::Windows));
        let again: NetConfig = toml::from_str(&toml::to_string(&migrated).unwrap()).unwrap();
        assert_eq!(again, migrated);
        // A rule set in `keys` wins over the 2.0 fields.
        let mut set = cfg.clone();
        set.keys.insert("mac>windows".into(), DirRules { mod_mode: ModMode::Position, ..DirRules::default() });
        assert_eq!(set.rules_for(Os::Mac, Os::Windows).unwrap().mod_mode, ModMode::Position);
        assert_eq!(set.rules_for(Os::Mac, Os::Linux).unwrap().mod_mode, ModMode::Off);
    }
    #[test]
    fn master_settings_carry_keys_and_relay_and_mirror_the_2_0_fields() {
        let old: MasterSettings = serde_json::from_str(r#"{"swap_cmd_ctrl":false}"#).unwrap();
        assert!(old.keys.is_empty() && old.clipboard_relay);
        let mut master = NetConfig { clipboard_relay: false, ..NetConfig::default() };
        master.keys.insert("mac>windows".into(), DirRules { mod_mode: ModMode::Off, hangul_src: vec![HangulKey::ShiftSpace], ..DirRules::default() });
        master.keys.insert("windows>mac".into(), DirRules { hangul_src: vec![], ..DirRules::default() });
        let settings = MasterSettings::of(&master, "M");
        assert!(!settings.swap_cmd_ctrl && !settings.mac_capslock_to_hangul && settings.mac_shift_space_to_hangul);
        assert_eq!(settings.lang1_on_mac, "off");
        let json = serde_json::to_string(&settings).unwrap();
        let settings: MasterSettings = serde_json::from_str(&json).unwrap();
        let mut sub = NetConfig::default();
        settings.apply_to(&mut sub);
        assert_eq!(sub.keys.get("mac>windows"), master.keys.get("mac>windows"));
        assert_eq!(sub.keys.get("windows>mac"), master.keys.get("windows>mac"));
        assert!(!sub.clipboard_relay);
        assert_eq!(sub.rules_for(Os::Mac, Os::Windows), master.rules_for(Os::Mac, Os::Windows));
        // Every direction travels with its effective rules, also those not edited.
        assert_eq!(settings.keys.len(), 9);
        assert_eq!(sub.rules_for(Os::Mac, Os::Linux), master.rules_for(Os::Mac, Os::Linux));
        assert_eq!(sub.all_rules(), master.all_rules());
    }
    #[test]
    fn status_from_2_0_still_reads() {
        let status: Status = serde_json::from_value(serde_json::json!({
            "running": true, "me": {"id": "t", "name": "Mac", "port": 47160, "error": null},
            "discovered": [], "peers": [{"id": "p", "connected": true, "rtt_ms": null}],
            "state": {"mode": "local", "peer": null}, "pairing": null
        })).unwrap();
        assert!(status.peers[0].can_send && !status.peers[0].via_master);
        assert!(status.layout.is_empty() && !status.reconnecting && status.keys.is_empty());
    }
}
