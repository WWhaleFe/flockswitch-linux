//! The background loop: finds devices, diverts the trigger buttons, and moves every
//! following device to the next channel when one of them is pressed.
//!
//! Devices are remembered once seen. A receiver tells us when a device links or
//! unlinks, and only that slot is re-read; the handles are re-opened only when the set
//! of receivers / Bluetooth devices changes. Rebuilding everything on every link change
//! used to drop a slow-waking keyboard from the list, so it stayed behind.

use crate::config::{button_name, Config, Gesture, BUTTONS};
use crate::device::{self, Device, DeviceKind, Pairing, Reporting, FEATURE_CHANGE_HOST, FEATURE_REPROG_V4};
use crate::hidpp::{self, Endpoint, REPORT_SHORT};
use crate::shared::{self, DeviceInfo, Shared};
use anyhow::{anyhow, Result};
use hidapi::HidApi;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Receiver notification sent when a paired device links or unlinks:
/// [10, slot, 41, protocol, device_info, wpid_lo, wpid_hi]. Bit 6 of device_info is set
/// while the link is down (seen on Bolt: 0x02 = mouse up, 0x42 = mouse down).
const NOTIF_DEVICE_CONNECTION: u8 = 0x41;
const LINK_DOWN: u8 = 0x40;
/// Give the device (and Logi Options+, which also reconfigures it) time to settle.
const SLOT_SETTLE: Duration = Duration::from_millis(1500);
const SLOT_RETRIES: u8 = 3;
const HEALTH_CHECK: Duration = Duration::from_secs(30);
const RESCAN_INTERVAL: Duration = Duration::from_secs(5);
/// How often to look for receivers being plugged in and Bluetooth devices (dis)connecting.
const ENUMERATE_INTERVAL: Duration = Duration::from_secs(3);
/// Rescans caused by an endpoint that keeps failing back off up to this long.
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// The settings window writes config.toml; pick its changes up quickly.
const CONFIG_CHECK: Duration = Duration::from_millis(700);
/// A haptic pulse takes a moment to play; Change Host right after would cut it short.
const HAPTIC_SETTLE: Duration = Duration::from_millis(150);

pub struct Engine {
    shared: Arc<Shared>,
    cfg: Config,
    cfg_version: u64,
    api: HidApi,
    endpoints: Vec<Endpoint>,
    /// Every device seen on the current endpoints, online or not right now.
    devices: Vec<Device>,
    /// Devices learned earlier, by key, so a paired-but-offline slot can still be listed.
    known: HashMap<String, Device>,
    /// The device whose button we use. Kept by value (endpoint, slot, features), never as
    /// a position in `devices`, so a rescan can't leave it pointing at the wrong entry.
    trigger: Option<Device>,
    /// Divert state of each button we diverted on the trigger, as found before we
    /// touched it; restored on exit or when the button is switched off.
    original_divert: HashMap<u16, Reporting>,
    /// Which of the trigger buttons the master mouse has.
    trigger_buttons: Vec<u16>,
    /// Haptic waveforms the master mouse offers (bit N = waveform N); None without a motor.
    trigger_haptics: Option<u32>,
    /// The master mouse was switched away from here; a pulse greets it when it is back.
    away_by_switch: bool,
    /// Trigger buttons held down right now (a press counts once until released).
    held: HashSet<u16>,
    /// A button with gestures is held: (button, total dx, total dy) of the mouse since.
    gesture: Option<(u16, i32, i32)>,
    /// Slots to re-read: (endpoint, slot) -> (when, attempts left).
    pending: HashMap<(usize, u8), (Instant, u8)>,
    last_check: Instant,
    /// Slots that answered last time we asked: (endpoint, slot).
    online: HashSet<(usize, u8)>,
    /// Every device ever seen, remembered on disk for the settings menu.
    catalog: Vec<DeviceInfo>,
    /// Keys of devices that carry the trigger button (can be the master mouse).
    masters: HashSet<String>,
    last_enumerate: Instant,
    /// Endpoints the OS listed at the last scan (opened or not), to spot changes.
    seen_endpoints: Vec<String>,
    backoff: Duration,
}

impl Engine {
    pub fn new(shared: Arc<Shared>) -> Result<Engine> {
        let cfg = shared.config();
        let cfg_version = shared.version();
        let api = HidApi::new().map_err(|e| anyhow!("cannot start HID: {e}"))?;
        Ok(Engine {
            shared,
            cfg,
            cfg_version,
            api,
            endpoints: Vec::new(),
            devices: Vec::new(),
            known: HashMap::new(),
            trigger: None,
            original_divert: HashMap::new(),
            trigger_buttons: Vec::new(),
            trigger_haptics: None,
            away_by_switch: false,
            held: HashSet::new(),
            gesture: None,
            pending: HashMap::new(),
            last_check: Instant::now(),
            online: HashSet::new(),
            catalog: shared::load_catalog(),
            masters: HashSet::new(),
            last_enumerate: Instant::now(),
            seen_endpoints: Vec::new(),
            backoff: Duration::from_secs(1),
        })
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    pub fn endpoint(&mut self, i: usize) -> &mut Endpoint {
        &mut self.endpoints[i]
    }

    fn is_trigger(&self, endpoint: usize, slot: u8) -> bool {
        self.trigger.as_ref().is_some_and(|t| t.endpoint == endpoint && t.index == slot)
    }

    /// Re-opens all endpoints and reads every device. Used at start-up and when the set
    /// of receivers / Bluetooth devices changes. One failing endpoint is marked dead and
    /// skipped instead of aborting the whole scan.
    pub fn scan(&mut self) -> Result<()> {
        // The device keeps its divert while we re-open handles, so the button keeps
        // working through a rescan. Remember who had it to compare afterwards.
        let previous = self.trigger.take().map(|t| t.key());
        let previous_original = std::mem::take(&mut self.original_divert);
        self.held.clear();
        self.gesture = None;

        self.api.refresh_devices().map_err(|e| anyhow!("HID refresh failed: {e}"))?;
        // Close the old handles before opening new ones.
        self.endpoints.clear();
        self.endpoints = hidpp::discover(&self.api);
        self.seen_endpoints = hidpp::endpoint_keys(&self.api);
        self.seen_endpoints.sort();
        self.devices.clear();
        self.pending.clear();
        self.online.clear();
        self.masters.clear();
        self.last_enumerate = Instant::now();
        for i in 0..self.endpoints.len() {
            if self.shared.stopped() {
                return Ok(());
            }
            if let Err(e) = self.scan_endpoint(i) {
                log::warn!("{}: {e:#}", self.endpoints[i].product);
                self.endpoints[i].dead = true;
            }
        }
        self.trigger = self.pick_trigger()?;
        self.read_channel();
        if let Some(key) = previous.filter(|_| !previous_original.is_empty()) {
            if self.trigger.as_ref().is_some_and(|t| t.key() == key) {
                // Same master: still diverted; keep what we knew about the originals.
                self.original_divert = previous_original;
            } else if let Some(old) = self.devices.iter().find(|d| d.key() == key).cloned() {
                // The master changed: give the old one its buttons back if it is here.
                for (cid, orig) in previous_original {
                    self.restore_button(&old, cid, orig);
                }
            }
        }
        self.publish();
        Ok(())
    }

    fn scan_endpoint(&mut self, i: usize) -> Result<()> {
        if let Err(e) = device::enable_link_notifications(&mut self.endpoints[i]) {
            log::debug!("link notifications: {e:#}");
        }
        let pairings = device::receiver_pairings(&mut self.endpoints[i])?;
        for p in &pairings {
            log::debug!("receiver slot {}: {:?} {:04X}", p.slot, p.kind, p.wpid);
        }
        for slot in device::candidate_slots(&self.endpoints[i], &pairings) {
            if self.shared.stopped() {
                break;
            }
            let pairing = pairings.iter().find(|p| p.slot == slot);
            self.load_slot(i, slot, pairing)?;
        }
        Ok(())
    }

    /// Updates the remembered device list and copies what the UI shows into the shared
    /// status. The list is saved when a device appears or changes, not on every link flip.
    fn publish(&mut self) {
        let mut changed = false;
        for info in self.catalog.iter_mut() {
            info.online = false;
        }
        for d in &self.devices {
            if !self.online.contains(&(d.endpoint, d.index)) || !d.has(FEATURE_CHANGE_HOST) {
                continue;
            }
            let info = DeviceInfo {
                key: d.key(),
                name: d.name.clone(),
                kind: match d.kind {
                    Some(DeviceKind::Mouse) => "mouse",
                    Some(DeviceKind::Keyboard) => "keyboard",
                    _ => "other",
                }
                .into(),
                master: self.masters.contains(&d.key()),
                online: true,
            };
            match self.catalog.iter_mut().find(|c| c.key == info.key) {
                Some(c) => {
                    changed |= c.name != info.name || c.kind != info.kind || c.master != info.master;
                    *c = info;
                }
                None => {
                    changed = true;
                    self.catalog.push(info);
                }
            }
        }
        if changed {
            shared::save_catalog(&self.catalog);
        }
        let trigger = self.trigger.clone();
        let trigger_online = trigger.as_ref().is_some_and(|t| self.online.contains(&(t.endpoint, t.index)));
        let devices = self.catalog.clone();
        let buttons = self.trigger_buttons.clone();
        self.shared.set_status(|s| {
            s.buttons = buttons;
            s.trigger = trigger.as_ref().filter(|_| trigger_online).map(|t| t.name.clone());
            s.trigger_key = trigger.map(|t| t.key());
            s.devices = devices;
            s.hid_denied = hidpp::permission_denied();
        });
    }

    /// Picks up config changes made in the UI or in the settings file.
    fn reload_config(&mut self) -> Result<()> {
        let version = self.shared.version();
        if version == self.cfg_version {
            return Ok(());
        }
        self.cfg_version = version;
        let next = self.shared.config();
        let trigger_changed = next.triggers() != self.cfg.triggers() || next.trigger_device != self.cfg.trigger_device;
        if trigger_changed || (self.cfg.enabled && !next.enabled) {
            // Hand every button back, then divert the ones still wanted below.
            self.release();
            self.trigger = None;
            self.held.clear();
            self.gesture = None;
        }
        let intensity_changed = next.haptics.intensity != self.cfg.haptics.intensity;
        // A waveform just chosen, played once so the hand can judge it.
        let waveforms = |h: &crate::config::Haptics| [h.switch_waveform, h.arrive_waveform, h.edge_waveform];
        let new_waveform =
            waveforms(&next.haptics).into_iter().zip(waveforms(&self.cfg.haptics)).find(|(new, old)| new != old).map(|(new, _)| new);
        self.cfg = next;
        log::info!("settings changed ({})", self.mode_text());
        if intensity_changed {
            self.apply_haptic_intensity();
        }
        if let Some(waveform) = new_waveform {
            self.buzz(waveform);
        } else if intensity_changed && self.cfg.haptics.intensity != 0 {
            // A pulse at the new strength, so the hand can judge it.
            self.buzz(self.cfg.haptics.switch_waveform);
        }
        if self.cfg.enabled {
            self.ensure_diverted()?;
        } else {
            log::info!("paused: the button is back to normal");
        }
        self.publish();
        Ok(())
    }

    /// Reads one slot and records the device. A device that is offline right now but
    /// was seen before is still listed, so it can be moved as soon as it wakes.
    fn load_slot(&mut self, endpoint: usize, slot: u8, pairing: Option<&Pairing>) -> Result<bool> {
        let probed = device::probe_slot(&mut self.endpoints[endpoint], endpoint, slot, pairing)?;
        let online = probed.is_some();
        if online {
            self.online.insert((endpoint, slot));
        } else {
            self.online.remove(&(endpoint, slot));
        }
        let dev = match probed {
            Some(d) => {
                // Remove stale offline slots of the same device on this receiver.
                let key = d.key();
                self.devices.retain(|x| {
                    !(x.endpoint == endpoint
                        && x.index != slot
                        && x.key() == key
                        && !self.online.contains(&(x.endpoint, x.index)))
                });
                let is_mouse = d.kind.map_or(true, |k| k == DeviceKind::Mouse);
                // A mouse with any of the trigger buttons can be chosen as the master.
                let can_be_master = is_mouse
                    && d.has(FEATURE_REPROG_V4)
                    && d.has(FEATURE_CHANGE_HOST)
                    && !buttons_on(&d, &mut self.endpoints[endpoint], &BUTTONS).is_empty();
                if can_be_master {
                    self.masters.insert(d.key());
                } else if !d.has(FEATURE_CHANGE_HOST) || (is_mouse && !d.has(FEATURE_REPROG_V4)) {
                    // Device answered Root ping while still waking/handshaking; re-probe shortly.
                    self.pending.entry((endpoint, slot)).or_insert((Instant::now() + SLOT_SETTLE, SLOT_RETRIES));
                }
                self.known.insert(d.key(), d.clone());
                Some(d)
            }
            None => pairing.and_then(|p| {
                let key = format!("{:04X}", p.wpid);
                let already_on_endpoint = self.devices.iter().any(|x| x.endpoint == endpoint && x.key() == key);
                if already_on_endpoint {
                    None
                } else {
                    self.known.get(&key).map(|d| d.relocated(endpoint, slot))
                }
            }),
        };
        if let Some(d) = dev {
            // The slot was re-paired to another device: the old trigger is gone.
            if self.is_trigger(endpoint, slot) && self.trigger.as_ref().is_some_and(|t| t.key() != d.key()) {
                self.trigger = None;
                self.original_divert.clear();
                self.held.clear();
                self.gesture = None;
            }
            match self.devices.iter().position(|x| x.endpoint == endpoint && x.index == slot) {
                Some(i) => self.devices[i] = d,
                None => self.devices.push(d),
            }
        }
        Ok(online)
    }

    /// Chooses the master mouse: the configured one (or, with "auto", the first mouse)
    /// that is here and has at least one of the buttons switched on.
    fn pick_trigger(&mut self) -> Result<Option<Device>> {
        let want = self.cfg.trigger_device.trim().to_string();
        for d in self.devices.clone() {
            if !d.has(FEATURE_REPROG_V4)
                || !d.has(FEATURE_CHANGE_HOST)
                || !self.online.contains(&(d.endpoint, d.index))
                || self.endpoints[d.endpoint].dead
            {
                continue;
            }
            let matches = if want.eq_ignore_ascii_case("auto") {
                d.kind.map_or(true, |k| k == DeviceKind::Mouse)
            } else {
                d.key().eq_ignore_ascii_case(&want) || d.name.to_lowercase().contains(&want.to_lowercase())
            };
            if !matches {
                continue;
            }
            let has = buttons_on(&d, &mut self.endpoints[d.endpoint], &BUTTONS);
            if has.iter().any(|&c| self.cfg.button(c).on) {
                self.trigger_buttons = has;
                self.trigger_haptics = self.read_haptics(&d);
                return Ok(Some(d));
            }
        }
        self.trigger_buttons.clear();
        self.trigger_haptics = None;
        Ok(None)
    }

    /// The haptic waveforms the mouse offers, logged once; a mouse without a motor (or
    /// away right now) gives None, which is simply "no vibration".
    fn read_haptics(&mut self, d: &Device) -> Option<u32> {
        let ep = self.endpoints.get_mut(d.endpoint)?;
        let mask = d.haptic_waveforms(ep).ok().flatten()?;
        if self.trigger_haptics != Some(mask) {
            log::info!("haptics: {} offers waveforms {mask:#010x}", d.label());
            if self.cfg.haptics.intensity == 0 && d.haptic_config(ep).ok().flatten().is_some_and(|(flags, _)| flags & 0x01 == 0) {
                log::warn!("haptics are turned off on {} (a Logi Options+ setting), so it will not vibrate", d.label());
            }
        }
        Some(mask)
    }

    /// Puts the strength the settings ask for on the mouse (0x19B0 setConfiguration),
    /// turning haptics on if they were off; 0 leaves the mouse as Logi Options+ set it.
    /// The mouse forgets this when it loses power, so it is applied whenever it is here.
    fn apply_haptic_intensity(&mut self) {
        let wanted = self.cfg.haptics.intensity;
        let (Some(d), Some(_)) = (self.trigger.clone(), self.trigger_haptics) else { return };
        if wanted == 0 || !self.online.contains(&(d.endpoint, d.index)) {
            return;
        }
        let Some(ep) = self.endpoints.get_mut(d.endpoint) else { return };
        let Ok(Some((flags, now))) = d.haptic_config(ep) else { return };
        if now == wanted && flags & 0x01 != 0 {
            return;
        }
        match d.set_haptic_config(ep, flags | 0x01, wanted) {
            Ok(true) => log::info!("haptics: {} strength {wanted}% (was {now}%)", d.label()),
            Ok(false) => log::warn!("haptics: {} did not accept strength {wanted}%", d.label()),
            Err(e) => log::debug!("haptics: {e:#}"),
        }
    }

    /// Plays a haptic waveform on the master mouse, if it is here and has that waveform.
    /// Quiet on a mouse without a motor.
    fn buzz(&mut self, waveform: u8) -> bool {
        let (Some(d), Some(mask)) = (self.trigger.clone(), self.trigger_haptics) else { return false };
        let waveform = if waveform == crate::device::waveform::RANDOM {
            let offered: Vec<u8> = (0..32).filter(|&w| mask & (1u32 << w) != 0).collect();
            if offered.is_empty() {
                return false;
            }
            offered[crate::config::random_below(offered.len())]
        } else {
            waveform
        };
        if waveform > 31 || mask & (1u32 << waveform) == 0 {
            log::debug!("haptics: {} has no waveform {waveform}", d.label());
            return false;
        }
        if !self.online.contains(&(d.endpoint, d.index)) {
            return false;
        }
        let Some(ep) = self.endpoints.get_mut(d.endpoint) else { return false };
        match d.play_haptic(ep, waveform) {
            Ok(true) => true,
            Ok(false) => {
                log::debug!("haptics: {} did not confirm waveform {waveform}", d.label());
                false
            }
            Err(e) => {
                log::debug!("haptics: {e:#}");
                false
            }
        }
    }

    /// The master mouse is back on this computer after being switched away from here:
    /// a pulse says so.
    fn greet_trigger(&mut self) {
        if !self.away_by_switch || !self.trigger.as_ref().is_some_and(|t| self.online.contains(&(t.endpoint, t.index))) {
            return;
        }
        self.away_by_switch = false;
        if self.cfg.haptics.on_arrive {
            self.buzz(self.cfg.haptics.arrive_waveform);
        }
    }

    /// Plays what the haptics window asks to feel, at the strength chosen there; the
    /// mouse's own strength is put back afterwards. Quiet without a mouse with a motor.
    fn feel(&mut self, what: shared::HapticPreview, intensity: u8) {
        let (Some(d), Some(_)) = (self.trigger.clone(), self.trigger_haptics) else {
            log::info!("haptics preview: no mouse with a motor on this computer");
            return;
        };
        let before = match self.endpoints.get_mut(d.endpoint) {
            Some(ep) if intensity != 0 => d.haptic_config(ep).ok().flatten(),
            _ => None,
        };
        if let (Some((flags, _)), Some(ep)) = (before, self.endpoints.get_mut(d.endpoint)) {
            let _ = d.set_haptic_config(ep, flags | 0x01, intensity);
        }
        match what {
            shared::HapticPreview::Pulse(waveform) => {
                self.buzz(waveform);
            }
            // A second of waiting at an edge, as the edge watcher plays it.
            shared::HapticPreview::Edge { rising } => {
                let start = Instant::now();
                while start.elapsed() < Duration::from_secs(1) {
                    let progress = start.elapsed().as_secs_f64();
                    let (gap, waveform) = if rising {
                        crate::edge::rising_pulse(progress)
                    } else {
                        (crate::edge::HAPTIC_TICK, self.cfg.haptics.edge_waveform)
                    };
                    self.buzz(waveform);
                    std::thread::sleep(gap);
                }
            }
        }
        std::thread::sleep(HAPTIC_SETTLE);
        if let (Some((flags, old)), Some(ep)) = (before, self.endpoints.get_mut(d.endpoint)) {
            let _ = d.set_haptic_config(ep, flags, old);
        }
    }

    /// Makes sure the buttons switched on are diverted to us, and that a button switched
    /// off since an earlier run is back to normal. The state found before the first
    /// divert is written to disk, so even a killed run is undone correctly later.
    fn ensure_diverted(&mut self) -> Result<()> {
        if !self.cfg.enabled {
            return Ok(());
        }
        if self.trigger.is_none() {
            self.trigger = self.pick_trigger()?;
            if let Some(t) = &self.trigger {
                log::info!("trigger: {}", t.label());
                self.read_channel();
            }
            self.apply_haptic_intensity();
            self.greet_trigger();
            self.publish();
        }
        let Some(d) = self.trigger.clone() else { return Ok(()) };
        if self.endpoints.get(d.endpoint).is_none() {
            self.trigger = None;
            return Ok(());
        }
        for cid in BUTTONS {
            let wanted = self.cfg.button(cid).on;
            if !wanted && self.original_divert.contains_key(&cid) {
                continue; // release() handles buttons we diverted ourselves
            }
            let ep = &mut self.endpoints[d.endpoint];
            // None: the mouse lacks this button, or is not on this computer right now.
            let Some(now) = d.control_reporting(ep, cid)? else { continue };
            if !wanted {
                // Left diverted by an earlier run that used this button and was killed.
                if let Some(orig) = shared::saved_original_divert(&d.key(), cid) {
                    self.restore_button(&d, cid, orig);
                }
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(e) = self.original_divert.entry(cid) {
                // A record is written before every divert, so without one the current
                // state is not ours: Logi Options+ diverts the gesture and middle buttons
                // for its own features and must get them back that way.
                let original = shared::saved_original_divert(&d.key(), cid).unwrap_or(now);
                shared::remember_original_divert(&d.key(), cid, original);
                e.insert(original);
            }
            // Gestures need the movement during a hold; otherwise leave the cursor alone.
            let want = Reporting { diverted: true, raw_xy: self.cfg.button(cid).uses_moves() };
            if now != want {
                self.held.remove(&cid);
                let raw = (now.raw_xy != want.raw_xy).then_some(want.raw_xy);
                if d.set_reporting(ep, cid, true, raw)? {
                    let how = if want.raw_xy { " with gestures" } else { "" };
                    log::info!("{} button diverted{how} on {}", button_name(cid), d.label());
                } else {
                    log::warn!("{} did not confirm the button setting; retrying later", d.label());
                }
            }
        }
        Ok(())
    }

    /// Puts every diverted button back the way we found it. Safe to call at any time.
    pub fn release(&mut self) {
        let originals = std::mem::take(&mut self.original_divert);
        let Some(d) = self.trigger.clone() else { return };
        for (cid, orig) in originals {
            self.restore_button(&d, cid, orig);
        }
    }

    fn restore_button(&mut self, d: &Device, cid: u16, orig: Reporting) {
        let Some(ep) = self.endpoints.get_mut(d.endpoint) else { return };
        if ep.dead {
            return;
        }
        // Touch the raw XY flag only if it differs (not every button supports it).
        let raw = match d.control_reporting(ep, cid) {
            Ok(Some(now)) if now.raw_xy != orig.raw_xy => Some(orig.raw_xy),
            _ => None,
        };
        if let Err(e) = d.set_reporting(ep, cid, orig.diverted, raw) {
            log::warn!("could not restore the {} button: {e}", button_name(cid));
        }
        // Forget the record only once the device confirms its state. If the mouse is on
        // another computer the record stays, and the next run restores it.
        match d.control_reporting(ep, cid) {
            Ok(Some(now)) if now == orig => shared::forget_original_divert(&d.key(), cid),
            _ => log::info!("{} not reachable; its button will be restored next time", d.label()),
        }
    }

    /// Moves the followers, then the trigger device, to `channel` (1-based).
    /// Followers go first so the mouse — the thing the user just pressed — leaves last.
    /// Stale pairing slots answer with an error and are skipped on their own.
    pub fn switch_to(&mut self, channel: u8) -> Result<()> {
        // A pulse on the mouse as it goes, felt before Change Host takes it away.
        if self.cfg.haptics.on_switch && self.buzz(self.cfg.haptics.switch_waveform) {
            std::thread::sleep(HAPTIC_SETTLE);
        }
        let trigger = self.trigger.clone();
        let is_trigger = |d: &Device| trigger.as_ref().is_some_and(|t| t.endpoint == d.endpoint && t.index == d.index);
        let order: Vec<Device> = self
            .devices
            .iter()
            .filter(|d| !is_trigger(d))
            .cloned()
            .chain(trigger.clone())
            .collect();
        for d in order {
            if self.shared.stopped() {
                break;
            }
            if !d.has(FEATURE_CHANGE_HOST) || (!is_trigger(&d) && !self.cfg.follows(d.wpid, &d.name)) {
                continue;
            }
            let Some(ep) = self.endpoints.get_mut(d.endpoint) else { continue };
            if ep.dead {
                continue;
            }
            // One device failing must not keep the others (or the mouse) from moving.
            // The mouse was just pressed, so it is here: if it does not answer (a
            // Unifying mouse was seen to drop its link for a moment), ask again rather
            // than leave it behind while the group has gone.
            let tries = if is_trigger(&d) { 3 } else { 1 };
            let mut state = Ok(None);
            for attempt in 1..=tries {
                state = d.host_state(ep);
                if matches!(state, Ok(Some(_))) || attempt == tries {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
            let state = match state {
                Ok(s) => s,
                Err(e) => {
                    log::warn!("{}: {e:#}", d.label());
                    continue;
                }
            };
            match state {
                Some(s) if s.current + 1 == channel => log::debug!("{} already on {channel}", d.label()),
                Some(s) if channel <= s.count => match d.set_host(ep, channel - 1) {
                    Ok(_) => {
                        log::info!("{} -> channel {channel}", d.label());
                        if is_trigger(&d) {
                            self.away_by_switch = true;
                        }
                    }
                    Err(e) => log::warn!("{}: {e:#}", d.label()),
                },
                Some(_) => log::warn!("{} has no channel {channel}", d.label()),
                None if is_trigger(&d) => log::warn!("{} did not answer; it stays on this computer", d.label()),
                None => log::debug!("{} (slot {}) not on this computer, skipped", d.label(), d.index),
            }
        }
        Ok(())
    }

    /// A keyboard shortcut or the screen edge: the group and the master mouse go to
    /// `channel`. Only devices on this computer can be told to move.
    fn on_shortcut(&mut self, channel: u8) {
        if !self.cfg.enabled {
            return;
        }
        log::info!("shortcut or screen edge: channel {channel}");
        if let Err(e) = self.switch_to(channel) {
            log::warn!("{e:#}");
        }
        self.held.clear();
        self.gesture = None;
        self.shared.set_status(|s| s.last_switch = Some(format!("→ {channel}")));
    }

    /// Asks the master mouse, if it is here, which channel it is on — this computer's
    /// channel, which the settings window keeps on.
    fn read_channel(&mut self) {
        let Some(d) = self.trigger.clone() else { return };
        let Some(ep) = self.endpoints.get_mut(d.endpoint) else { return };
        if let Ok(Some(state)) = d.host_state(ep) {
            self.shared.set_status(|s| s.channel = Some(state.current + 1));
        }
    }

    /// Moves everything on, as set for this gesture of the button `cid`.
    fn on_press(&mut self, cid: u16, gesture: Gesture) -> Result<()> {
        let Some(d) = self.trigger.clone() else { return Ok(()) };
        let Some(ep) = self.endpoints.get_mut(d.endpoint) else { return Ok(()) };
        let Some(state) = d.host_state(ep)? else {
            log::warn!("button pressed but {} did not report its channel", d.label());
            return Ok(());
        };
        self.shared.set_status(|s| s.channel = Some(state.current + 1));
        match self.cfg.button(cid).channel_after(gesture, state.current, state.count) {
            Some(next) => {
                log::info!("{} button {gesture:?}: channel {} -> {next}", button_name(cid), state.current + 1);
                self.switch_to(next)?;
                // The release report never arrives once the mouse has left.
                self.held.clear();
                self.gesture = None;
                let text = format!("{} → {next}", state.current + 1);
                self.shared.set_status(|s| s.last_switch = Some(text));
            }
            None => log::info!("{} button {gesture:?}: nothing to do (already there, or not set)", button_name(cid)),
        }
        Ok(())
    }

    fn handle(&mut self, endpoint: usize, report: &[u8]) -> Result<()> {
        if report.len() >= 5 && report[0] == REPORT_SHORT && report[2] == NOTIF_DEVICE_CONNECTION {
            let slot = report[1];
            let up = report[4] & LINK_DOWN == 0;
            log::debug!("slot {slot} link {} ({:02x?})", if up { "up" } else { "down" }, report);
            if self.is_trigger(endpoint, slot) {
                self.held.clear();
                self.gesture = None;
            }
            if up {
                self.pending.insert((endpoint, slot), (Instant::now() + SLOT_SETTLE, SLOT_RETRIES));
            } else if self.online.remove(&(endpoint, slot)) {
                self.publish();
            }
            return Ok(());
        }
        let Some(t) = self.trigger.clone() else { return Ok(()) };
        if t.endpoint != endpoint {
            return Ok(());
        }
        // Mouse movement while a button with gestures is held.
        if let Some((dx, dy)) = t.raw_movement(report) {
            if let Some((_, x, y)) = &mut self.gesture {
                *x += i32::from(dx);
                *y += i32::from(dy);
            }
            return Ok(());
        }
        if let Some(pressed) = t.pressed_controls(report) {
            let down: HashSet<u16> = pressed.into_iter().filter(|&c| BUTTONS.contains(&c) && self.cfg.button(c).on).collect();
            // The first button that went down now; it decides where to go.
            let new_press = BUTTONS.into_iter().find(|c| down.contains(c) && !self.held.contains(c));
            self.held = down;
            if !self.cfg.enabled {
                self.gesture = None;
                return Ok(());
            }
            if let Some(cid) = new_press {
                if self.cfg.button(cid).uses_moves() {
                    // Decided on release, by where the mouse went meanwhile.
                    self.gesture = Some((cid, 0, 0));
                    log::info!("{} button held: waiting for release", button_name(cid));
                } else {
                    self.on_press(cid, Gesture::Press)?;
                }
            }
            if let Some((cid, dx, dy)) = self.gesture.filter(|(cid, _, _)| !self.held.contains(cid)) {
                self.gesture = None;
                let gesture = Gesture::from_movement(dx, dy);
                log::info!("{} button released: moved dx={dx} dy={dy} -> {gesture:?}", button_name(cid));
                self.on_press(cid, gesture)?;
            }
        }
        Ok(())
    }

    /// Re-reads slots whose link came up, retrying while the device is still waking.
    fn process_pending(&mut self) -> Result<()> {
        let now = Instant::now();
        let due: Vec<(usize, u8)> = self
            .pending
            .iter()
            .filter(|(_, (when, _))| *when <= now)
            .map(|(k, _)| *k)
            .collect();
        for (endpoint, slot) in due {
            if self.shared.stopped() {
                break;
            }
            let Some((_, left)) = self.pending.remove(&(endpoint, slot)) else { continue };
            if endpoint >= self.endpoints.len() {
                continue;
            }
            let pairings = device::receiver_pairings(&mut self.endpoints[endpoint])?;
            let pairing = pairings.iter().find(|p| p.slot == slot);
            let online = self.load_slot(endpoint, slot, pairing)?;
            // A device still waking asks for another look; that uses up this slot's
            // attempts, so one that simply lacks the features is not read forever.
            if left > 1 {
                if let Some(again) = self.pending.get_mut(&(endpoint, slot)) {
                    again.1 = left - 1;
                }
            } else {
                self.pending.remove(&(endpoint, slot));
            }
            self.publish();
            if online {
                if self.trigger.is_none() || self.is_trigger(endpoint, slot) {
                    self.ensure_diverted()?;
                }
                if self.is_trigger(endpoint, slot) {
                    self.read_channel();
                    self.apply_haptic_intensity();
                    self.greet_trigger();
                }
            } else if left > 1 {
                self.pending.insert((endpoint, slot), (now + SLOT_SETTLE, left - 1));
            }
        }
        Ok(())
    }

    /// Queues paired receiver slots that are offline right now, in case a device was
    /// asleep at start-up and has not sent a link notification since.
    fn queue_unlisted(&mut self) -> Result<()> {
        for endpoint in 0..self.endpoints.len() {
            if self.endpoints[endpoint].dead {
                continue;
            }
            for p in device::receiver_pairings(&mut self.endpoints[endpoint])? {
                if !self.online.contains(&(endpoint, p.slot)) {
                    self.pending.entry((endpoint, p.slot)).or_insert((Instant::now(), 1));
                }
            }
        }
        Ok(())
    }

    /// Runs until the shared stop flag is set. The button is handed back on the way out
    /// (also on errors and panics, through `Drop`).
    pub fn run(&mut self) -> Result<()> {
        self.scan()?;
        self.describe();
        self.ensure_diverted()?;

        let mut last_scan = Instant::now();
        let mut last_config = Instant::now();
        while !self.shared.stopped() {
            // The settings window just installed the udev rule: look again now.
            let mut asked = false;
            if last_config.elapsed() > CONFIG_CHECK {
                last_config = Instant::now();
                self.shared.reload_if_edited();
                if let Some((what, intensity)) = shared::take_haptic_preview() {
                    self.feel(what, intensity);
                }
                asked = shared::take_rescan_file();
                if asked {
                    log::info!("devices: looking again (udev rule installed)");
                    self.backoff = Duration::from_secs(1);
                }
            }
            if let Err(e) = self.reload_config() {
                log::warn!("applying settings failed: {e:#}");
            }
            if let Some(channel) = self.shared.take_switch_request().or_else(shared::take_switch_file) {
                self.on_shortcut(channel);
            }
            // `flockswitch locate`: the screen watcher (edge.rs on X11, locate_wayland.rs
            // on Wayland) shows the cursor.
            if shared::take_locate_file() {
                self.shared.request_locate(shared::LocateSource::Command);
            }
            if let Some(waveform) = self.shared.take_haptic_request() {
                self.buzz(waveform);
            }

            // Rescan when a receiver or Bluetooth device went away (read error, with
            // back-off) or when the OS lists a different set of endpoints.
            let dead = self.endpoints.iter().any(|e| e.dead);
            let mut rescan = asked || (dead && last_scan.elapsed() > self.backoff);
            if !rescan && self.last_enumerate.elapsed() > ENUMERATE_INTERVAL {
                self.last_enumerate = Instant::now();
                if self.api.refresh_devices().is_ok() {
                    let mut now = hidpp::endpoint_keys(&self.api);
                    now.sort();
                    if now != self.seen_endpoints {
                        rescan = true;
                        self.backoff = Duration::from_secs(1);
                    }
                }
            }
            if rescan || (self.endpoints.is_empty() && last_scan.elapsed() > RESCAN_INTERVAL) {
                last_scan = Instant::now();
                if rescan {
                    log::info!("devices changed, rescanning");
                }
                if let Err(e) = self.scan() {
                    log::warn!("rescan failed: {e:#}");
                }
                if !self.shared.stopped() {
                    if let Err(e) = self.ensure_diverted() {
                        log::warn!("{e:#}");
                    }
                }
                self.backoff = if self.endpoints.iter().any(|e| e.dead) {
                    (self.backoff * 5).min(MAX_BACKOFF)
                } else {
                    Duration::from_secs(1)
                };
                continue;
            }
            if self.endpoints.is_empty() {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }

            let wait = Duration::from_millis(if self.endpoints.len() > 1 { 5 } else { 20 });
            for i in 0..self.endpoints.len() {
                while let Some(r) = self.endpoints[i].next_report(wait) {
                    if let Err(e) = self.handle(i, &r) {
                        log::warn!("{e:#}");
                    }
                }
            }

            if let Err(e) = self.process_pending() {
                log::warn!("refresh failed: {e:#}");
            }
            let check_interval = if self.trigger.is_none() { RESCAN_INTERVAL } else { HEALTH_CHECK };
            if self.last_check.elapsed() > check_interval {
                self.last_check = Instant::now();
                if let Err(e) = self.queue_unlisted().and_then(|_| self.ensure_diverted()) {
                    log::warn!("health check failed: {e:#}");
                }
            }
        }
        self.release();
        Ok(())
    }

    fn mode_text(&self) -> String {
        let on = if self.cfg.enabled { "" } else { ", paused" };
        let buttons: Vec<String> = self
            .cfg
            .triggers()
            .into_iter()
            .map(|cid| format!("{} {:?}", button_name(cid), self.cfg.button(cid).channels))
            .collect();
        format!("{}{on}", buttons.join("; "))
    }

    pub fn describe(&self) {
        log::info!("buttons: {}", self.mode_text());
        let wanted: Vec<String> = self.cfg.triggers().iter().map(|&c| button_name(c).to_string()).collect();
        match &self.trigger {
            Some(t) => log::info!("trigger: {} (buttons {})", t.label(), wanted.join(", ")),
            None => log::warn!("no mouse with buttons {} on this computer yet", wanted.join(", ")),
        }
        for d in &self.devices {
            log::info!("device: {} (slot {})", d.label(), d.index);
        }
    }
}

/// The buttons of `cids` that `d` has (it answers for them), in the same order.
fn buttons_on(d: &Device, ep: &mut Endpoint, cids: &[u16]) -> Vec<u16> {
    cids.iter().copied().filter(|&c| matches!(d.control_reporting(ep, c), Ok(Some(_)))).collect()
}

impl Drop for Engine {
    /// Last chance to hand the button back (error return or panic unwinding).
    fn drop(&mut self) {
        self.release();
    }
}
