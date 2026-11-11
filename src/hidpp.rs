//! HID++ transport: finds Logitech HID++ endpoints (receivers and directly connected
//! devices) and performs request/response exchanges on them.
//!
//! The design is single-threaded on purpose: every endpoint is polled from one loop,
//! and reports that arrive while we wait for a response are kept in `backlog` so the
//! caller can process them as notifications afterwards.

use anyhow::{anyhow, Result};
use hidapi::{HidApi, HidDevice};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Set by the last scan when a Logitech device could not be opened for lack of
/// permission: the udev rule that gives the logged-in user access is missing.
static PERMISSION_DENIED: AtomicBool = AtomicBool::new(false);

pub fn permission_denied() -> bool {
    PERMISSION_DENIED.load(Ordering::Relaxed)
}

/// What the last scan could not open. Without the udev rule the engine looks again every
/// few seconds; the same failures again go to the debug log only.
static OPEN_FAILURES: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub const LOGITECH_VID: u16 = 0x046d;
pub const REPORT_SHORT: u8 = 0x10;
pub const REPORT_LONG: u8 = 0x11;
pub const RECEIVER_INDEX: u8 = 0xFF;
pub const DIRECT_INDEX: u8 = 0xFF;

const SHORT_LEN: usize = 7;
const LONG_LEN: usize = 20;

/// USB receivers (devices live in slots 1..=6). Anything else exposing the HID++
/// collection — e.g. a mouse on a USB cable — is addressed directly at index 0xFF.
const RECEIVER_PIDS: &[u16] = &[
    0xC548, // Bolt
    0xC52B, 0xC532, // Unifying
    0xC52F, 0xC534, 0xC539, 0xC53A, 0xC53F, 0xC547, // nano / Lightspeed
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    /// Bolt / Unifying style receiver: devices live in slots 1..=6.
    Receiver,
    /// Device connected directly (Bluetooth or USB cable): device index 0xFF.
    Direct,
}

pub struct Endpoint {
    pub kind: EndpointKind,
    pub product: String,
    /// USB/Bluetooth product ID. For a Bluetooth device this identifies the model.
    pub product_id: u16,
    long: HidDevice,
    /// Separate handle for short reports. `None` when the OS exposes short and long
    /// reports on the same handle (macOS) or the endpoint has no short collection.
    short: Option<HidDevice>,
    /// The OS lists a separate short collection but it could not be opened (Windows),
    /// so short requests (receiver registers) cannot be sent.
    short_unavailable: bool,
    pub backlog: VecDeque<Vec<u8>>,
    /// Set when a read fails, e.g. the receiver was unplugged; the engine then rescans.
    pub dead: bool,
    next_swid: u8,
}

/// Reply to a request.
#[derive(Debug)]
pub enum Reply {
    Ok(Vec<u8>),
    /// The device or receiver answered with an error (HID++ 1.0 0x8F or HID++ 2.0 0xFF),
    /// e.g. because the device is not linked to this receiver right now.
    Error,
    /// No answer within the timeout — usually the device is asleep or on another host.
    Timeout,
}

impl Endpoint {
    /// Writes a report to the right handle.
    fn write(&self, data: &[u8]) -> Result<()> {
        anyhow::ensure!(!self.dead, "{} is gone", self.product);
        let dev = match (data[0], &self.short) {
            (REPORT_SHORT, Some(short)) => short,
            _ => &self.long,
        };
        dev.write(data).map_err(|e| anyhow!("HID write failed: {e}"))?;
        Ok(())
    }

    /// Reads one report from any handle, waiting at most `timeout`.
    pub fn read(&mut self, timeout: Duration) -> Option<Vec<u8>> {
        if self.dead {
            // Behave like an idle device so callers' loops still wait instead of spinning.
            std::thread::sleep(timeout);
            return None;
        }
        let mut buf = [0u8; 64];
        let ms = timeout.as_millis().max(1) as i32;
        // Split the wait between the two handles so neither starves.
        let per = if self.short.is_some() { (ms / 2).max(1) } else { ms };
        let handles = std::iter::once(&self.long).chain(self.short.as_ref());
        for dev in handles {
            match dev.read_timeout(&mut buf, per) {
                Ok(n) if n > 0 => return Some(buf[..n].to_vec()),
                Ok(_) => {}
                Err(e) => {
                    if !self.dead {
                        log::warn!("{} stopped responding: {e}", self.product);
                    }
                    self.dead = true;
                }
            }
        }
        None
    }

    /// Returns a queued notification first, then polls the device.
    pub fn next_report(&mut self, timeout: Duration) -> Option<Vec<u8>> {
        if let Some(r) = self.backlog.pop_front() {
            return Some(r);
        }
        self.read(timeout)
    }

    fn swid(&mut self) -> u8 {
        // Rotate 0x8..=0xF so late replies to an earlier request are never mistaken
        // for the current one. Logitech's own software uses the low range.
        self.next_swid = if self.next_swid >= 0xF { 0x8 } else { self.next_swid + 1 };
        self.next_swid
    }

    /// HID++ 2.0 feature request.
    pub fn request(
        &mut self,
        index: u8,
        feature: u8,
        function: u8,
        params: &[u8],
        timeout: Duration,
    ) -> Result<Reply> {
        let swid = self.swid();
        let mut pkt = vec![0u8; LONG_LEN];
        pkt[0] = REPORT_LONG;
        pkt[1] = index;
        pkt[2] = feature;
        pkt[3] = (function << 4) | swid;
        pkt[4..4 + params.len()].copy_from_slice(params);
        self.write(&pkt)?;

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline && !self.dead {
            let Some(r) = self.read(Duration::from_millis(20)) else { continue };
            if r.len() < SHORT_LEN || r[1] != index {
                self.backlog.push_back(r);
                continue;
            }
            // HID++ 2.0 answers are long reports; anything shorter is not our reply.
            if r[0] == REPORT_LONG && r.len() >= LONG_LEN && r[2] == feature && r[3] == pkt[3] {
                return Ok(Reply::Ok(r[4..].to_vec()));
            }
            // HID++ 2.0 error: [11, idx, FF, feature, fn|swid, code]
            if r[2] == 0xFF && r[3] == feature && r[4] == pkt[3] {
                return Ok(Reply::Error);
            }
            // HID++ 1.0 error from the receiver about this device: [10, idx, 8F, feature, fn|swid, code]
            if r[0] == REPORT_SHORT && r[2] == 0x8F && r[3] == feature && r[4] == pkt[3] {
                return Ok(Reply::Error);
            }
            self.backlog.push_back(r);
        }
        Ok(Reply::Timeout)
    }

    /// HID++ 1.0 register read on the receiver (sub_id 0x81 = short, 0x83 = long).
    pub fn read_register(&mut self, sub_id: u8, register: u8, p0: u8) -> Result<Reply> {
        self.register(sub_id, register, [p0, 0, 0])
    }

    /// HID++ 1.0 short register write on the receiver (sub_id 0x80).
    pub fn write_register(&mut self, register: u8, value: [u8; 3]) -> Result<Reply> {
        self.register(0x80, register, value)
    }

    fn register(&mut self, sub_id: u8, register: u8, params: [u8; 3]) -> Result<Reply> {
        if self.short_unavailable {
            return Ok(Reply::Timeout);
        }
        let pkt = [REPORT_SHORT, RECEIVER_INDEX, sub_id, register, params[0], params[1], params[2]];
        self.write(&pkt)?;

        let want_len = if sub_id == 0x83 { LONG_LEN } else { SHORT_LEN };
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline && !self.dead {
            let Some(r) = self.read(Duration::from_millis(20)) else { continue };
            if r.len() >= SHORT_LEN && r[1] == RECEIVER_INDEX {
                // Long register replies echo the sub-page in the first parameter byte.
                let same_page = sub_id != 0x83 || r[4] == params[0];
                if r.len() >= want_len && r[2] == sub_id && r[3] == register && same_page {
                    return Ok(Reply::Ok(r[4..].to_vec()));
                }
                if r[2] == 0x8F && r[3] == sub_id && r[4] == register {
                    return Ok(Reply::Error);
                }
            }
            self.backlog.push_back(r);
        }
        Ok(Reply::Timeout)
    }
}

#[derive(Default)]
struct Group {
    short: Option<String>,
    long: Option<String>,
    kind: Option<EndpointKind>,
    product: String,
    product_id: u16,
    usage_page: u16,
}

/// Groups the HID++ collections the OS currently lists by endpoint.
fn groups(api: &HidApi) -> BTreeMap<String, Group> {
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();

    for info in api.device_list().filter(|d| d.vendor_id() == LOGITECH_VID) {
        let receiver = RECEIVER_PIDS.contains(&info.product_id());
        let ff00 = if receiver { EndpointKind::Receiver } else { EndpointKind::Direct };
        let (kind, is_long) = match (info.usage_page(), info.usage()) {
            (0xFF00, 0x0001) => (ff00, false),
            (0xFF00, 0x0002) => (ff00, true),
            (0xFF43, 0x0202) => (EndpointKind::Direct, true),
            (0xFF43, 0x0101) => (EndpointKind::Direct, false),
            _ => continue,
        };
        let path = info.path().to_string_lossy().into_owned();
        if is_receiver_child(&path) {
            continue;
        }
        let g = groups.entry(group_key(&path)).or_default();
        if is_long {
            g.long = Some(path);
        } else {
            g.short = Some(path);
        }
        g.kind = Some(kind);
        g.product = info.product_string().unwrap_or("").to_string();
        g.product_id = info.product_id();
        g.usage_page = info.usage_page();
    }
    groups.retain(|_, g| g.long.is_some());
    groups
}

/// Keys of the endpoints the OS lists right now; cheap, opens nothing. Used to notice
/// a receiver being plugged in or a Bluetooth device (dis)connecting.
pub fn endpoint_keys(api: &HidApi) -> Vec<String> {
    groups(api).into_keys().collect()
}

/// Opens every Logitech HID++ endpoint on this machine.
pub fn discover(api: &HidApi) -> Vec<Endpoint> {
    let mut out = Vec::new();
    let mut failures = Vec::new();
    PERMISSION_DENIED.store(false, Ordering::Relaxed);
    for g in groups(api).into_values() {
        let (Some(long_path), Some(kind)) = (g.long, g.kind) else { continue };
        let open = |p: &str| api.open_path(&std::ffi::CString::new(p).unwrap());
        let long = match open(&long_path) {
            Ok(d) => d,
            Err(e) => {
                if access_denied(&long_path) {
                    PERMISSION_DENIED.store(true, Ordering::Relaxed);
                    failures.push(format!("no permission to open {long_path} ({}): the udev rule is missing", g.product));
                } else {
                    failures.push(format!("cannot open {long_path}: {e}"));
                }
                continue;
            }
        };
        let mut kind = kind;
        let mut short_unavailable = false;
        let short = match g.short {
            Some(sp) if sp != long_path => match open(&sp) {
                Ok(d) => Some(d),
                Err(e) => {
                    failures.push(format!("cannot open the short-report collection {sp}: {e}"));
                    short_unavailable = true;
                    None
                }
            },
            _ => None,
        };
        let mut ep = Endpoint {
            kind,
            product: g.product,
            product_id: g.product_id,
            long,
            short,
            backlog: VecDeque::new(),
            short_unavailable,
            dead: false,
            next_swid: 0x8,
        };
        // A receiver not in our list still answers the receiver register 0x02
        // (connected-device count); a cable-connected device does not.
        if kind == EndpointKind::Direct && g.usage_page == 0xFF00 {
            if let Ok(Reply::Ok(_)) = ep.read_register(0x81, 0x02, 0) {
                kind = EndpointKind::Receiver;
                ep.kind = kind;
            }
        }
        log::debug!("endpoint {:?} '{}' (separate short handle: {})", kind, ep.product, ep.short.is_some());
        out.push(ep);
    }
    let mut reported = OPEN_FAILURES.lock().unwrap();
    let level = if *reported == failures { log::Level::Debug } else { log::Level::Warn };
    for f in &failures {
        log::log!(level, "{f}");
    }
    *reported = failures;
    out
}

/// The kernel's receiver driver (hid-logitech-dj) adds a HID device for each paired
/// device, below the receiver's own. Those reach the same devices as the receiver's
/// slots (and the kernel rewrites their device index), so only the receiver is used,
/// as Solaar does.
fn is_receiver_child(path: &str) -> bool {
    let Some(node) = Path::new(path).file_name() else { return false };
    let Ok(dev) = std::fs::canonicalize(Path::new("/sys/class/hidraw").join(node).join("device")) else {
        return false;
    };
    dev.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).is_some_and(is_hid_id)
}

/// A HID device's name in sysfs: bus:vendor:product.instance, e.g. "0003:046D:C548.0004".
fn is_hid_id(name: &str) -> bool {
    let hex = |s: &str, len: usize| s.len() >= len && s.chars().all(|c| c.is_ascii_hexdigit());
    let Some((ids, instance)) = name.split_once('.') else { return false };
    let parts: Vec<&str> = ids.split(':').collect();
    parts.len() == 3 && parts.iter().all(|p| p.len() == 4 && hex(p, 4)) && hex(instance, 4)
}

/// The device node exists but this user may not open it.
fn access_denied(path: &str) -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
}

/// Windows exposes each top-level collection as its own path, e.g.
/// `\\?\hid#vid_046d&pid_c548&mi_02&col01#7&19de0263&0&0000#{guid}` and
/// `...&col02#7&19de0263&0&0001#{guid}`. Drop the `&colNN` part and the last segment of
/// the instance id so both collections of one receiver share a key. Other platforms
/// (macOS, Linux) already use one path per interface and are returned unchanged.
fn group_key(path: &str) -> String {
    let lower = path.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('#').collect();
    if parts.len() < 3 || !parts[1].contains("vid_") {
        return lower;
    }
    let interface = match parts[1].find("&col") {
        Some(i) => &parts[1][..i],
        None => parts[1],
    };
    let instance = match parts[2].rfind('&') {
        Some(i) => &parts[2][..i],
        None => parts[2],
    };
    format!("{interface}#{instance}")
}

#[cfg(test)]
mod tests {
    use super::{group_key, is_hid_id};

    #[test]
    fn hid_ids_in_sysfs() {
        assert!(is_hid_id("0003:046D:C548.0004"));
        assert!(is_hid_id("0005:046D:B034.000A"));
        assert!(!is_hid_id("1-2:1.2")); // a USB interface: the receiver itself sits below it
        assert!(!is_hid_id("uhid"));
        assert!(!is_hid_id("0003:046D:C548"));
    }

    #[test]
    fn windows_collections_share_a_key() {
        let a = r"\\?\hid#vid_046d&pid_c548&mi_02&col01#7&19de0263&0&0000#{4d1e55b2}";
        let b = r"\\?\hid#vid_046d&pid_c548&mi_02&col02#7&19de0263&0&0001#{4d1e55b2}";
        assert_eq!(group_key(a), group_key(b));
    }

    #[test]
    fn different_receivers_stay_apart() {
        let a = r"\\?\hid#vid_046d&pid_c548&mi_02&col02#7&19de0263&0&0001#{4d1e55b2}";
        let b = r"\\?\hid#vid_046d&pid_c548&mi_02&col02#7&2aaa0000&0&0001#{4d1e55b2}";
        assert_ne!(group_key(a), group_key(b));
    }

    #[test]
    fn mac_path_unchanged() {
        assert_eq!(group_key("DevSrvsID:4294970000"), "devsrvsid:4294970000");
    }
}
