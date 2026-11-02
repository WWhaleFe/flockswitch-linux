//! HID++ 2.0 devices reachable through an endpoint, and the features flockswitch uses:
//! device name (0x0005), Change Host (0x1814) and Reprogrammable Controls v4 (0x1B04).

use crate::hidpp::{Endpoint, EndpointKind, Reply, DIRECT_INDEX};
use anyhow::Result;
use std::collections::HashMap;
use std::time::Duration;

pub const FEATURE_ROOT: u16 = 0x0000;
pub const FEATURE_NAME: u16 = 0x0005;
pub const FEATURE_CHANGE_HOST: u16 = 0x1814;
pub const FEATURE_REPROG_V4: u16 = 0x1B04;

/// Keyboards answer slowly when they wake from sleep; 0.6 s was measured to be too short.
pub const SLOW_TIMEOUT: Duration = Duration::from_millis(2000);
const FAST_TIMEOUT: Duration = Duration::from_millis(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Keyboard,
    Mouse,
    Other(u8),
}

impl DeviceKind {
    /// Kind code from a receiver pairing record (Bolt and Unifying share it).
    fn from_receiver(code: u8) -> Self {
        match code & 0x0F {
            1 => DeviceKind::Keyboard,
            2 => DeviceKind::Mouse,
            c => DeviceKind::Other(c),
        }
    }

    /// Kind code from the device itself (feature 0x0005, getDeviceType).
    fn from_device_type(code: u8) -> Self {
        match code {
            0 | 2 => DeviceKind::Keyboard,      // keyboard, numpad
            3..=5 => DeviceKind::Mouse,         // mouse, trackpad, trackball
            c => DeviceKind::Other(c),
        }
    }
}

/// A device slot as recorded by the receiver (known even when the device is offline).
#[derive(Debug, Clone)]
pub struct Pairing {
    pub slot: u8,
    pub kind: DeviceKind,
    pub wpid: u16,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub endpoint: usize,
    pub index: u8,
    pub name: String,
    pub kind: Option<DeviceKind>,
    pub wpid: Option<u16>,
    features: HashMap<u16, u8>,
}

/// How a control reports (REPROG_CONTROLS_V4 reporting flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reporting {
    /// Presses come to us as notifications instead of acting on the device.
    pub diverted: bool,
    /// While it is held, the mouse movement comes to us instead of moving the cursor.
    pub raw_xy: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct HostState {
    /// Number of host channels (Easy-Switch slots).
    pub count: u8,
    /// Current channel, 0-based.
    pub current: u8,
}

impl Device {
    pub fn label(&self) -> String {
        let wpid = self.wpid.map(|w| format!(" [{w:04X}]")).unwrap_or_default();
        format!("{}{}", self.name, wpid)
    }

    pub fn has(&self, feature: u16) -> bool {
        self.features.contains_key(&feature)
    }

    fn feature(&self, feature: u16) -> Option<u8> {
        self.features.get(&feature).copied()
    }

    /// Reads the current Easy-Switch channel. An idle keyboard can miss the first
    /// request while its radio wakes up, so a timeout is retried once. An error reply
    /// means the device is not linked to this computer and is returned at once.
    pub fn host_state(&self, ep: &mut Endpoint) -> Result<Option<HostState>> {
        let Some(fi) = self.feature(FEATURE_CHANGE_HOST) else { return Ok(None) };
        for _ in 0..2 {
            match ep.request(self.index, fi, 0, &[], SLOW_TIMEOUT)? {
                Reply::Ok(r) => return Ok(Some(HostState { count: r[0], current: r[1] })),
                Reply::Error => return Ok(None),
                Reply::Timeout => continue,
            }
        }
        Ok(None)
    }

    /// The same device at another endpoint/slot (feature indices do not change).
    pub fn relocated(&self, endpoint: usize, index: u8) -> Device {
        Device { endpoint, index, ..self.clone() }
    }

    /// Identity used to remember a device across reconnects and rescans.
    pub fn key(&self) -> String {
        match self.wpid {
            Some(w) => format!("{w:04X}"),
            None => self.name.clone(),
        }
    }

    /// Moves the device to another channel (0-based). The device drops its link right
    /// away, so a missing reply is expected and not treated as an error.
    pub fn set_host(&self, ep: &mut Endpoint, channel: u8) -> Result<bool> {
        let Some(fi) = self.feature(FEATURE_CHANGE_HOST) else { return Ok(false) };
        ep.request(self.index, fi, 1, &[channel], Duration::from_millis(300))?;
        Ok(true)
    }

    /// Reads how a control reports right now; None when the device lacks it (or is away).
    pub fn control_reporting(&self, ep: &mut Endpoint, cid: u16) -> Result<Option<Reporting>> {
        let Some(fi) = self.feature(FEATURE_REPROG_V4) else { return Ok(None) };
        Ok(match ep.request(self.index, fi, 2, &cid.to_be_bytes(), FAST_TIMEOUT)? {
            Reply::Ok(r) => Some(Reporting { diverted: r[2] & 0x01 != 0, raw_xy: r[2] & 0x10 != 0 }),
            _ => None,
        })
    }

    /// Diverts (or releases) a control so its presses come to us as notifications
    /// instead of acting on the device, and with `raw_xy` also the mouse movement while
    /// it is held (`None` leaves that as it is). Not persistent: the device forgets it
    /// when it reconnects, which is why the engine re-applies it.
    pub fn set_reporting(&self, ep: &mut Endpoint, cid: u16, divert: bool, raw_xy: Option<bool>) -> Result<bool> {
        let Some(fi) = self.feature(FEATURE_REPROG_V4) else { return Ok(false) };
        let [hi, lo] = cid.to_be_bytes();
        // bit0 divert + bit1 "divert valid"; bit4 raw XY + bit5 "raw XY valid".
        let mut flags = 0x02 | u8::from(divert);
        if let Some(raw) = raw_xy {
            flags |= 0x20 | (u8::from(raw) << 4);
        }
        // An idle mouse can be slow to answer; a missed reply is checked by the caller.
        Ok(matches!(
            ep.request(self.index, fi, 3, &[hi, lo, flags, 0, 0], SLOW_TIMEOUT)?,
            Reply::Ok(_)
        ))
    }

    /// If `report` is a "diverted raw XY" notification from this device (the mouse moved
    /// while a button with raw XY diverted is held), returns the movement (dx, dy).
    pub fn raw_movement(&self, report: &[u8]) -> Option<(i16, i16)> {
        let fi = self.feature(FEATURE_REPROG_V4)?;
        if report.len() < 8 || report[1] != self.index || report[2] != fi || report[3] & 0xF0 != 0x10 {
            return None;
        }
        Some((i16::from_be_bytes([report[4], report[5]]), i16::from_be_bytes([report[6], report[7]])))
    }

    /// If `report` is a "diverted buttons" notification from this device, returns the
    /// control IDs currently held down.
    pub fn pressed_controls(&self, report: &[u8]) -> Option<Vec<u16>> {
        let fi = self.feature(FEATURE_REPROG_V4)?;
        if report.len() < 12 || report[1] != self.index || report[2] != fi || report[3] != 0x00 {
            return None;
        }
        Some(
            report[4..12]
                .chunks(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .filter(|&c| c != 0)
                .collect(),
        )
    }
}

fn feature_index(ep: &mut Endpoint, index: u8, feature: u16, timeout: Duration) -> Result<Option<u8>> {
    Ok(match ep.request(index, 0x00, 0, &feature.to_be_bytes(), timeout)? {
        Reply::Ok(r) if r[0] != 0 || feature == FEATURE_ROOT => Some(r[0]),
        _ => None,
    })
}

fn read_name(ep: &mut Endpoint, index: u8, fi: u8) -> Result<String> {
    let Reply::Ok(c) = ep.request(index, fi, 0, &[], SLOW_TIMEOUT)? else { return Ok(String::new()) };
    let len = c[0] as usize;
    let mut name = Vec::new();
    while name.len() < len {
        let Reply::Ok(chunk) = ep.request(index, fi, 1, &[name.len() as u8], SLOW_TIMEOUT)? else { break };
        let take = (len - name.len()).min(chunk.len());
        name.extend_from_slice(&chunk[..take]);
    }
    Ok(String::from_utf8_lossy(&name).trim_end_matches('\0').to_string())
}

/// Reads the receiver's pairing table. Works even when devices are asleep or away.
/// Bolt and Unifying keep it in register 0xB5 with different sub-pages and layouts.
pub fn receiver_pairings(ep: &mut Endpoint) -> Result<Vec<Pairing>> {
    let mut out = Vec::new();
    if ep.kind != EndpointKind::Receiver {
        return Ok(out);
    }
    // Bolt: sub-page 0x50 + slot -> [sub, kind, wpid_lo, wpid_hi, serial(4), ...]
    for slot in 1..=6u8 {
        if let Reply::Ok(r) = ep.read_register(0x83, 0xB5, 0x50 + slot)? {
            if r.len() >= 4 {
                out.push(Pairing {
                    slot,
                    kind: DeviceKind::from_receiver(r[1]),
                    wpid: u16::from_le_bytes([r[2], r[3]]),
                });
            }
        }
    }
    if !out.is_empty() {
        return Ok(out);
    }
    // Unifying: sub-page 0x20 + (slot - 1) -> [sub, dest_id, interval, wpid_hi, wpid_lo,
    // _, _, kind]. Unlike Bolt the pages count from 0: 0x20 is slot 1 (checked on a
    // real receiver: 0x20 = MX Master 3 in slot 1, 0x21 = MX Keys in slot 2).
    for slot in 1..=6u8 {
        if let Reply::Ok(r) = ep.read_register(0x83, 0xB5, 0x20 + slot - 1)? {
            if r.len() >= 8 {
                out.push(Pairing {
                    slot,
                    kind: DeviceKind::from_receiver(r[7]),
                    wpid: u16::from_be_bytes([r[3], r[4]]),
                });
            }
        }
    }
    Ok(out)
}

/// Slots worth asking on an endpoint: the receiver's recorded slots when its table is
/// readable, all six otherwise, or the single direct index for Bluetooth/USB devices.
pub fn candidate_slots(ep: &Endpoint, pairings: &[Pairing]) -> Vec<u8> {
    match ep.kind {
        EndpointKind::Direct => vec![DIRECT_INDEX],
        EndpointKind::Receiver if !pairings.is_empty() => pairings.iter().map(|p| p.slot).collect(),
        EndpointKind::Receiver => (1..=6).collect(),
    }
}

/// Asks one slot for its identity and features. Returns None when nothing answers.
pub fn probe_slot(
    ep: &mut Endpoint,
    endpoint: usize,
    index: u8,
    pairing: Option<&Pairing>,
) -> Result<Option<Device>> {
    // Root feature lookup doubles as a ping; idle keyboards need the long timeout.
    if feature_index(ep, index, FEATURE_ROOT, SLOW_TIMEOUT)?.is_none() {
        log::debug!("slot {index}: not online");
        return Ok(None);
    }
    let mut features = HashMap::new();
    for f in [FEATURE_NAME, FEATURE_CHANGE_HOST, FEATURE_REPROG_V4] {
        if let Some(fi) = feature_index(ep, index, f, SLOW_TIMEOUT)? {
            features.insert(f, fi);
        }
    }
    let name = match features.get(&FEATURE_NAME) {
        Some(&fi) => read_name(ep, index, fi)?,
        None => String::new(),
    };
    // Receiver devices are identified by the pairing record; a Bluetooth device has no
    // record, so ask it for its type and use its Bluetooth product ID as the identity.
    let kind = match (pairing, features.get(&FEATURE_NAME)) {
        (Some(p), _) => Some(p.kind),
        (None, Some(&fi)) => match ep.request(index, fi, 2, &[], SLOW_TIMEOUT)? {
            Reply::Ok(r) => Some(DeviceKind::from_device_type(r[0])),
            _ => None,
        },
        (None, None) => None,
    };
    let wpid = match pairing {
        Some(p) => Some(p.wpid),
        None if ep.kind == EndpointKind::Direct => Some(ep.product_id),
        None => None,
    };
    Ok(Some(Device {
        endpoint,
        index,
        name: if name.is_empty() { ep.product.clone() } else { name },
        kind,
        wpid,
        features,
    }))
}

/// Asks a receiver to report devices linking and unlinking (notification 0x41), which
/// is how flockswitch notices the mouse coming back. Logi Options+ usually has this on
/// already; only the "wireless notifications" bit is added, other flags are kept.
pub fn enable_link_notifications(ep: &mut Endpoint) -> Result<()> {
    if ep.kind != EndpointKind::Receiver {
        return Ok(());
    }
    if let Reply::Ok(r) = ep.read_register(0x81, 0x00, 0)? {
        let flags = [r[0], r[1], r[2]];
        if flags[1] & 0x01 == 0 {
            ep.write_register(0x00, [flags[0], flags[1] | 0x01, flags[2]])?;
            log::info!("turned on link notifications on {}", ep.product);
        }
    }
    Ok(())
}
