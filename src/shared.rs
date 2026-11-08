//! State shared between the engine thread and the tray UI.

use crate::config::Config;
use crate::device::Reporting;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

/// A device seen at least once, remembered on disk so settings can list it even while
/// it is connected to another computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Wireless product ID ("B042") or Bluetooth product ID; used in `follow` and
    /// `trigger_device`.
    pub key: String,
    pub name: String,
    /// "mouse", "keyboard" or "other".
    pub kind: String,
    /// Has one of the trigger buttons (e.g. MX Master wheel mode-shift), so it can be the master.
    pub master: bool,
    #[serde(skip)]
    pub online: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// Name of the master mouse, when it is on this computer.
    pub trigger: Option<String>,
    /// Key of the master mouse currently in use (resolves "auto").
    pub trigger_key: Option<String>,
    /// Every device seen so far.
    pub devices: Vec<DeviceInfo>,
    /// Last switch, e.g. "2 → 1".
    pub last_switch: Option<String>,
    /// Trigger buttons the master mouse has (control IDs).
    pub buttons: Vec<u16>,
    /// Channels whose keyboard shortcut could not be registered.
    pub hotkeys_failed: Vec<u8>,
    /// This computer's channel (1-based): the one the master mouse was on when last
    /// seen here. Kept while the mouse is away.
    pub channel: Option<u8>,
    /// A Logitech device could not be opened for lack of permission (no udev rule).
    pub hid_denied: bool,
    /// New-version check and download, shown in the menu.
    pub update: crate::update::UpdateState,
}

/// What the engine sees right now, written for the settings window (a separate
/// process) to show the master mouse and which devices are on this computer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusFile {
    pub trigger: Option<String>,
    pub trigger_key: Option<String>,
    pub last_switch: Option<String>,
    #[serde(default)]
    pub online: Vec<String>,
    /// Trigger buttons the master mouse has (control IDs).
    #[serde(default)]
    pub buttons: Vec<u16>,
    /// Version of the running menu-bar app (empty before 0.3.5).
    #[serde(default)]
    pub version: String,
    /// Channels whose keyboard shortcut could not be registered.
    #[serde(default)]
    pub hotkeys_failed: Vec<u8>,
    /// This computer's channel (1-based), for the 이동할 채널 rows.
    #[serde(default)]
    pub channel: Option<u8>,
    /// No permission to open a Logitech device: the settings window says how to fix it.
    #[serde(default)]
    pub hid_denied: bool,
}

fn status_path() -> std::path::PathBuf {
    Config::path().with_file_name("status.toml")
}

pub fn save_status_file(s: &StatusFile) {
    if let Ok(text) = toml::to_string_pretty(s) {
        let _ = std::fs::write(status_path(), text);
    }
}

pub fn load_status_file() -> StatusFile {
    std::fs::read_to_string(status_path())
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

/// The settings window asks the running tray app to uninstall by creating this file;
/// the tray app hands the button back, removes everything and quits.
pub fn uninstall_request_path() -> std::path::PathBuf {
    Config::path().with_file_name("uninstall.request")
}

/// `flockswitch quit` (or a newer copy taking over) asks the
/// running tray app to hand the button back and quit by creating this file.
pub fn quit_request_path() -> std::path::PathBuf {
    Config::path().with_file_name("quit.request")
}

/// `flockswitch switch <N>` (a desktop shortcut, on Wayland) asks the running copy to
/// move everything by writing the channel here.
pub fn switch_request_path() -> std::path::PathBuf {
    Config::path().with_file_name("switch.request")
}

/// The channel asked for through the request file, if any; the file is removed.
pub fn take_switch_file() -> Option<u8> {
    let path = switch_request_path();
    let text = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    text.trim().parse().ok().filter(|n| (1..=3).contains(n))
}

fn config_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(Config::path()).and_then(|m| m.modified()).ok()
}

#[derive(Default, Serialize, Deserialize)]
struct Catalog {
    #[serde(default)]
    device: Vec<DeviceInfo>,
}

fn catalog_path() -> std::path::PathBuf {
    Config::path().with_file_name("devices.toml")
}

/// Devices remembered from earlier runs (all marked offline until seen again).
pub fn load_catalog() -> Vec<DeviceInfo> {
    std::fs::read_to_string(catalog_path())
        .ok()
        .and_then(|t| toml::from_str::<Catalog>(&t).ok())
        .map(|c| c.device)
        .unwrap_or_default()
}

pub fn save_catalog(devices: &[DeviceInfo]) {
    let text = toml::to_string_pretty(&Catalog { device: devices.to_vec() }).unwrap_or_default();
    if let Err(e) = std::fs::write(catalog_path(), text) {
        log::warn!("cannot save the device list: {e}");
    }
}

/// Trigger-button state found before flockswitch first diverted it, kept on disk so a
/// crashed or killed run can still be undone correctly by the next run.
#[derive(Default, Serialize, Deserialize)]
struct DivertRecords {
    #[serde(default)]
    record: Vec<DivertRecord>,
}

#[derive(Clone, Serialize, Deserialize)]
struct DivertRecord {
    key: String,
    cid: u16,
    originally_diverted: bool,
    /// Whether the mouse movement during a hold was diverted (Options+ gestures).
    #[serde(default)]
    originally_raw_xy: bool,
}

fn divert_path() -> std::path::PathBuf {
    Config::path().with_file_name("button-state.toml")
}

fn load_divert_records() -> DivertRecords {
    std::fs::read_to_string(divert_path())
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_divert_records(r: &DivertRecords) {
    let path = divert_path();
    if r.record.is_empty() {
        let _ = std::fs::remove_file(path);
    } else if let Ok(text) = toml::to_string_pretty(r) {
        if let Err(e) = std::fs::write(path, text) {
            log::warn!("cannot save the button state: {e}");
        }
    }
}

/// The original state recorded for this device/button, if a previous run diverted it.
pub fn saved_original_divert(key: &str, cid: u16) -> Option<Reporting> {
    load_divert_records()
        .record
        .iter()
        .find(|r| r.key == key && r.cid == cid)
        .map(|r| Reporting { diverted: r.originally_diverted, raw_xy: r.originally_raw_xy })
}

pub fn remember_original_divert(key: &str, cid: u16, original: Reporting) {
    let mut r = load_divert_records();
    r.record.retain(|x| !(x.key == key && x.cid == cid));
    r.record.push(DivertRecord {
        key: key.into(),
        cid,
        originally_diverted: original.diverted,
        originally_raw_xy: original.raw_xy,
    });
    save_divert_records(&r);
}

pub fn forget_original_divert(key: &str, cid: u16) {
    let mut r = load_divert_records();
    r.record.retain(|x| !(x.key == key && x.cid == cid));
    save_divert_records(&r);
}

pub struct Shared {
    config: Mutex<Config>,
    /// Modification time of config.toml as last read or written by us.
    config_mtime: Mutex<Option<std::time::SystemTime>>,
    /// Bumped on every config change so the engine knows to reload.
    version: AtomicU64,
    status: Mutex<Status>,
    last_status_file: Mutex<StatusFile>,
    pub stop: AtomicBool,
    /// Channel asked for by a keyboard shortcut, for the engine to act on (0 = none).
    switch_request: AtomicU8,
    /// Haptic waveform the edge watcher asks the engine to play on the mouse (+1; 0 = none).
    haptic_request: AtomicU8,
}

impl Shared {
    pub fn new(cfg: Config) -> Shared {
        crate::i18n::set_language(&cfg.language);
        Shared {
            config: Mutex::new(cfg),
            config_mtime: Mutex::new(config_mtime()),
            version: AtomicU64::new(0),
            status: Mutex::new(Status::default()),
            last_status_file: Mutex::new(StatusFile { online: vec!["-".into()], ..StatusFile::default() }),
            stop: AtomicBool::new(false),
            switch_request: AtomicU8::new(0),
            haptic_request: AtomicU8::new(0),
        }
    }

    pub fn config(&self) -> Config {
        self.config.lock().unwrap().clone()
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// Applies a change, validates it, saves it, and tells the engine. On error nothing
    /// changes, so the UI can simply redraw from the current config.
    pub fn update_config(&self, change: impl FnOnce(&mut Config)) -> Result<()> {
        let mut current = self.config.lock().unwrap();
        let mut next = current.clone();
        change(&mut next);
        next.validate()?;
        next.save()?;
        crate::i18n::set_language(&next.language);
        *current = next;
        *self.config_mtime.lock().unwrap() = config_mtime();
        self.version.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Picks up edits made to config.toml by hand. An invalid edit is reported once and
    /// ignored, so the running settings stay as they were.
    pub fn reload_if_edited(&self) {
        let now = config_mtime();
        if now.is_none() || now == *self.config_mtime.lock().unwrap() {
            return;
        }
        // Read and parse without holding any lock.
        let parsed = std::fs::read_to_string(Config::path())
            .map_err(anyhow::Error::from)
            .and_then(|t| Config::parse(&t))
            .and_then(|c| c.validate().map(|_| c));
        // Lock order: config, then config_mtime — the same as update_config.
        let mut current = self.config.lock().unwrap();
        let mut seen = self.config_mtime.lock().unwrap();
        if now == *seen {
            return; // our own save raced with this check
        }
        *seen = now;
        match parsed {
            Ok(cfg) => {
                crate::i18n::set_language(&cfg.language);
                *current = cfg;
                self.version.fetch_add(1, Ordering::SeqCst);
                log::info!("settings file changed, reloaded");
            }
            Err(e) => log::warn!("settings file has an error, keeping current settings: {e:#}"),
        }
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn set_status(&self, change: impl FnOnce(&mut Status)) {
        let mut st = self.status.lock().unwrap();
        change(&mut st);
        let file = StatusFile {
            trigger: st.trigger.clone(),
            trigger_key: st.trigger_key.clone(),
            last_switch: st.last_switch.clone(),
            online: st.devices.iter().filter(|d| d.online).map(|d| d.key.clone()).collect(),
            buttons: st.buttons.clone(),
            hotkeys_failed: st.hotkeys_failed.clone(),
            channel: st.channel,
            hid_denied: st.hid_denied,
            version: env!("CARGO_PKG_VERSION").into(),
        };
        drop(st);
        // Only touch the disk when something the settings window shows has changed.
        let mut last = self.last_status_file.lock().unwrap();
        if *last != file {
            save_status_file(&file);
            *last = file;
        }
    }

    /// A keyboard shortcut was pressed: the engine moves everything to `channel`.
    pub fn request_switch(&self, channel: u8) {
        self.switch_request.store(channel, Ordering::SeqCst);
    }

    pub fn take_switch_request(&self) -> Option<u8> {
        Some(self.switch_request.swap(0, Ordering::SeqCst)).filter(|&c| c > 0)
    }

    /// Asks the engine to play a haptic waveform on the master mouse (if it has a motor).
    pub fn request_haptic(&self, waveform: u8) {
        self.haptic_request.store(waveform.saturating_add(1), Ordering::SeqCst);
    }

    pub fn take_haptic_request(&self) -> Option<u8> {
        Some(self.haptic_request.swap(0, Ordering::SeqCst)).filter(|&w| w > 0).map(|w| w - 1)
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}
