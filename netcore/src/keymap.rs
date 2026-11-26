use crate::proto::Os;
pub const LANG1: u16 = 0x90;
pub const LANG2: u16 = 0x91;
pub const VK_HANGUL: u16 = 0x15;
pub const VK_HANJA: u16 = 0x19;

/// Sending-side Mac shortcuts. Physical held keys stay in the service's key set;
/// temporary Shift releases only affect the Windows input stream.
#[derive(Default)]
pub(crate) struct MacHangul {
    space: bool,
}
impl MacHangul {
    pub(crate) fn map(
        &mut self, event: crate::InputEvent, sender: Os, receiver: Os,
        config: &crate::NetConfig, held: &std::collections::HashSet<u16>,
    ) -> Vec<crate::InputEvent> {
        use crate::InputEvent::Key;
        if sender != Os::Mac || receiver != Os::Windows { return vec![event]; }
        let Key { usage, down } = event else { return vec![event]; };
        let tap = || vec![Key { usage: LANG1, down: true }, Key { usage: LANG1, down: false }];
        // The Mac capture adapter turns each Caps Lock flagsChanged flip into a
        // down/up pair. Emit one Hangul tap on down and swallow its synthetic up.
        if usage == 0x39 && config.mac_capslock_to_hangul {
            return if down { tap() } else { vec![] };
        }
        if usage == 0x2c {
            if self.space {
                if !down { self.space = false; }
                return vec![]; // Includes autorepeat, and the matching Space up.
            }
            let shifts: Vec<_> = [0xe1, 0xe5].into_iter().filter(|u| held.contains(u)).collect();
            let other = [0xe0, 0xe2, 0xe3, 0xe4, 0xe6, 0xe7].iter().any(|u| held.contains(u));
            if down && config.mac_shift_space_to_hangul && !shifts.is_empty() && !other {
                self.space = true;
                let mut events: Vec<_> = shifts.iter().map(|&usage| Key { usage, down: false }).collect();
                events.extend(tap());
                events.extend(shifts.into_iter().map(|usage| Key { usage, down: true }));
                return events;
            }
        }
        vec![event]
    }
}

/// Native macOS key codes. macOS defines no kVK codes for F21-F24.
pub const MAC_KEYS: &[(u16, u16)] = &[
    (0x04, 0x00),
    (0x05, 0x0b),
    (0x06, 0x08),
    (0x07, 0x02),
    (0x08, 0x0e),
    (0x09, 0x03),
    (0x0a, 0x05),
    (0x0b, 0x04),
    (0x0c, 0x22),
    (0x0d, 0x26),
    (0x0e, 0x28),
    (0x0f, 0x25),
    (0x10, 0x2e),
    (0x11, 0x2d),
    (0x12, 0x1f),
    (0x13, 0x23),
    (0x14, 0x0c),
    (0x15, 0x0f),
    (0x16, 0x01),
    (0x17, 0x11),
    (0x18, 0x20),
    (0x19, 0x09),
    (0x1a, 0x0d),
    (0x1b, 0x07),
    (0x1c, 0x10),
    (0x1d, 0x06),
    (0x1e, 0x12),
    (0x1f, 0x13),
    (0x20, 0x14),
    (0x21, 0x15),
    (0x22, 0x17),
    (0x23, 0x16),
    (0x24, 0x1a),
    (0x25, 0x1c),
    (0x26, 0x19),
    (0x27, 0x1d),
    (0x28, 0x24),
    (0x29, 0x35),
    (0x2a, 0x33),
    (0x2b, 0x30),
    (0x2c, 0x31),
    (0x2d, 0x1b),
    (0x2e, 0x18),
    (0x2f, 0x21),
    (0x30, 0x1e),
    (0x31, 0x2a),
    (0x33, 0x29),
    (0x34, 0x27),
    (0x35, 0x32),
    (0x36, 0x2b),
    (0x37, 0x2f),
    (0x38, 0x2c),
    (0x39, 0x39),
    (0x3a, 0x7a),
    (0x3b, 0x78),
    (0x3c, 0x63),
    (0x3d, 0x76),
    (0x3e, 0x60),
    (0x3f, 0x61),
    (0x40, 0x62),
    (0x41, 0x64),
    (0x42, 0x65),
    (0x43, 0x6d),
    (0x44, 0x67),
    (0x45, 0x6f),
    (0x49, 0x72),
    (0x4a, 0x73),
    (0x4b, 0x74),
    (0x4c, 0x75),
    (0x4d, 0x77),
    (0x4e, 0x79),
    (0x4f, 0x7c),
    (0x50, 0x7b),
    (0x51, 0x7d),
    (0x52, 0x7e),
    (0x53, 0x47),
    (0x54, 0x4b),
    (0x55, 0x43),
    (0x56, 0x4e),
    (0x57, 0x45),
    (0x58, 0x4c),
    (0x59, 0x53),
    (0x5a, 0x54),
    (0x5b, 0x55),
    (0x5c, 0x56),
    (0x5d, 0x57),
    (0x5e, 0x58),
    (0x5f, 0x59),
    (0x60, 0x5b),
    (0x61, 0x5c),
    (0x62, 0x52),
    (0x63, 0x41),
    (0x64, 0x0a),
    (0x65, 0x6e),
    (0x67, 0x51),
    (0x68, 0x69),
    (0x69, 0x6b),
    (0x6a, 0x71),
    (0x6b, 0x6a),
    (0x6c, 0x40),
    (0x6d, 0x4f),
    (0x6e, 0x50),
    (0x6f, 0x5a),
    (LANG1, 0x68),
    (LANG2, 0x66),
    (0xe0, 0x3b),
    (0xe1, 0x38),
    (0xe2, 0x3a),
    (0xe3, 0x37),
    (0xe4, 0x3e),
    (0xe5, 0x3c),
    (0xe6, 0x3d),
    (0xe7, 0x36),
];
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScanCode {
    pub code: u16,
    pub extended: bool,
}
impl ScanCode {
    pub const fn new(code: u16, extended: bool) -> Self {
        Self { code, extended }
    }
}
pub const WINDOWS_KEYS: &[(u16, ScanCode)] = &[
    (0x04, ScanCode::new(0x1e, false)),
    (0x05, ScanCode::new(0x30, false)),
    (0x06, ScanCode::new(0x2e, false)),
    (0x07, ScanCode::new(0x20, false)),
    (0x08, ScanCode::new(0x12, false)),
    (0x09, ScanCode::new(0x21, false)),
    (0x0a, ScanCode::new(0x22, false)),
    (0x0b, ScanCode::new(0x23, false)),
    (0x0c, ScanCode::new(0x17, false)),
    (0x0d, ScanCode::new(0x24, false)),
    (0x0e, ScanCode::new(0x25, false)),
    (0x0f, ScanCode::new(0x26, false)),
    (0x10, ScanCode::new(0x32, false)),
    (0x11, ScanCode::new(0x31, false)),
    (0x12, ScanCode::new(0x18, false)),
    (0x13, ScanCode::new(0x19, false)),
    (0x14, ScanCode::new(0x10, false)),
    (0x15, ScanCode::new(0x13, false)),
    (0x16, ScanCode::new(0x1f, false)),
    (0x17, ScanCode::new(0x14, false)),
    (0x18, ScanCode::new(0x16, false)),
    (0x19, ScanCode::new(0x2f, false)),
    (0x1a, ScanCode::new(0x11, false)),
    (0x1b, ScanCode::new(0x2d, false)),
    (0x1c, ScanCode::new(0x15, false)),
    (0x1d, ScanCode::new(0x2c, false)),
    (0x1e, ScanCode::new(0x02, false)),
    (0x1f, ScanCode::new(0x03, false)),
    (0x20, ScanCode::new(0x04, false)),
    (0x21, ScanCode::new(0x05, false)),
    (0x22, ScanCode::new(0x06, false)),
    (0x23, ScanCode::new(0x07, false)),
    (0x24, ScanCode::new(0x08, false)),
    (0x25, ScanCode::new(0x09, false)),
    (0x26, ScanCode::new(0x0a, false)),
    (0x27, ScanCode::new(0x0b, false)),
    (0x28, ScanCode::new(0x1c, false)),
    (0x29, ScanCode::new(0x01, false)),
    (0x2a, ScanCode::new(0x0e, false)),
    (0x2b, ScanCode::new(0x0f, false)),
    (0x2c, ScanCode::new(0x39, false)),
    (0x2d, ScanCode::new(0x0c, false)),
    (0x2e, ScanCode::new(0x0d, false)),
    (0x2f, ScanCode::new(0x1a, false)),
    (0x30, ScanCode::new(0x1b, false)),
    (0x31, ScanCode::new(0x2b, false)),
    (0x33, ScanCode::new(0x27, false)),
    (0x34, ScanCode::new(0x28, false)),
    (0x35, ScanCode::new(0x29, false)),
    (0x36, ScanCode::new(0x33, false)),
    (0x37, ScanCode::new(0x34, false)),
    (0x38, ScanCode::new(0x35, false)),
    (0x39, ScanCode::new(0x3a, false)),
    (0x3a, ScanCode::new(0x3b, false)),
    (0x3b, ScanCode::new(0x3c, false)),
    (0x3c, ScanCode::new(0x3d, false)),
    (0x3d, ScanCode::new(0x3e, false)),
    (0x3e, ScanCode::new(0x3f, false)),
    (0x3f, ScanCode::new(0x40, false)),
    (0x40, ScanCode::new(0x41, false)),
    (0x41, ScanCode::new(0x42, false)),
    (0x42, ScanCode::new(0x43, false)),
    (0x43, ScanCode::new(0x44, false)),
    (0x44, ScanCode::new(0x57, false)),
    (0x45, ScanCode::new(0x58, false)),
    (0x47, ScanCode::new(0x46, false)),
    (0x53, ScanCode::new(0x45, false)),
    (0x55, ScanCode::new(0x37, false)),
    (0x56, ScanCode::new(0x4a, false)),
    (0x57, ScanCode::new(0x4e, false)),
    (0x59, ScanCode::new(0x4f, false)),
    (0x5a, ScanCode::new(0x50, false)),
    (0x5b, ScanCode::new(0x51, false)),
    (0x5c, ScanCode::new(0x4b, false)),
    (0x5d, ScanCode::new(0x4c, false)),
    (0x5e, ScanCode::new(0x4d, false)),
    (0x5f, ScanCode::new(0x47, false)),
    (0x60, ScanCode::new(0x48, false)),
    (0x61, ScanCode::new(0x49, false)),
    (0x62, ScanCode::new(0x52, false)),
    (0x63, ScanCode::new(0x53, false)),
    (0x64, ScanCode::new(0x56, false)),
    (0x68, ScanCode::new(0x64, false)),
    (0x69, ScanCode::new(0x65, false)),
    (0x6a, ScanCode::new(0x66, false)),
    (0x6b, ScanCode::new(0x67, false)),
    (0x6c, ScanCode::new(0x68, false)),
    (0x6d, ScanCode::new(0x69, false)),
    (0x6e, ScanCode::new(0x6a, false)),
    (0x6f, ScanCode::new(0x6b, false)),
    (0x70, ScanCode::new(0x6c, false)),
    (0x71, ScanCode::new(0x6d, false)),
    (0x72, ScanCode::new(0x6e, false)),
    (0x73, ScanCode::new(0x76, false)),
    (0x90, ScanCode::new(0x72, false)),
    (0x91, ScanCode::new(0x71, false)),
    (0xe0, ScanCode::new(0x1d, false)),
    (0xe1, ScanCode::new(0x2a, false)),
    (0xe2, ScanCode::new(0x38, false)),
    (0xe5, ScanCode::new(0x36, false)),
    (0x46, ScanCode::new(0x37, true)),
    (0x49, ScanCode::new(0x52, true)),
    (0x4a, ScanCode::new(0x47, true)),
    (0x4b, ScanCode::new(0x49, true)),
    (0x4c, ScanCode::new(0x53, true)),
    (0x4d, ScanCode::new(0x4f, true)),
    (0x4e, ScanCode::new(0x51, true)),
    (0x4f, ScanCode::new(0x4d, true)),
    (0x50, ScanCode::new(0x4b, true)),
    (0x51, ScanCode::new(0x50, true)),
    (0x52, ScanCode::new(0x48, true)),
    (0x54, ScanCode::new(0x35, true)),
    (0x58, ScanCode::new(0x1c, true)),
    (0x65, ScanCode::new(0x5d, true)),
    (0xe3, ScanCode::new(0x5b, true)),
    (0xe4, ScanCode::new(0x1d, true)),
    (0xe6, ScanCode::new(0x38, true)),
    (0xe7, ScanCode::new(0x5c, true)),
];

pub fn hid_to_mac(usage: u16) -> Option<u16> {
    MAC_KEYS.iter().find(|(u, _)| *u == usage).map(|(_, k)| *k)
}
pub fn mac_to_hid(key: u16) -> Option<u16> {
    MAC_KEYS.iter().find(|(_, k)| *k == key).map(|(u, _)| *u)
}
pub fn hid_to_windows(usage: u16) -> Option<ScanCode> {
    WINDOWS_KEYS
        .iter()
        .find(|(u, _)| *u == usage)
        .map(|(_, k)| *k)
}
pub fn windows_to_hid(scan: ScanCode) -> Option<u16> {
    WINDOWS_KEYS
        .iter()
        .find(|(_, k)| *k == scan)
        .map(|(u, _)| *u)
}
pub fn windows_key_to_hid(vk: u16, scan: ScanCode) -> Option<u16> {
    match vk {
        VK_HANGUL => Some(LANG1),
        VK_HANJA => Some(LANG2),
        _ => windows_to_hid(scan),
    }
}
pub fn hid_to_windows_vk(usage: u16) -> Option<u16> {
    match usage {
        LANG1 => Some(VK_HANGUL),
        LANG2 => Some(VK_HANJA),
        _ => None,
    }
}
pub fn swap_cmd_ctrl(usage: u16, sender: Os, receiver: Os, enabled: bool) -> u16 {
    if !enabled || (sender == Os::Mac) == (receiver == Os::Mac) {
        return usage;
    }
    match usage {
        0xe0 => 0xe3,
        0xe3 => 0xe0,
        0xe4 => 0xe7,
        0xe7 => 0xe4,
        _ => usage,
    }
}
pub fn mac_received_usage(usage: u16, lang1_on_mac: &str) -> Option<u16> {
    if usage == LANG1 {
        if lang1_on_mac == "off" {
            None
        } else {
            Some(0x39)
        }
    } else {
        Some(usage)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_entries_round_trip() {
        assert_eq!(hid_to_windows(0x73), Some(ScanCode::new(0x76, false)));
        assert_eq!(hid_to_mac(0x04), Some(0));
        assert_eq!(hid_to_windows(0x04), Some(ScanCode::new(0x1e, false)));
        for &(u, k) in MAC_KEYS {
            assert_eq!(hid_to_mac(u), Some(k));
            assert_eq!(mac_to_hid(k), Some(u));
        }
        for &(u, k) in WINDOWS_KEYS {
            assert_eq!(hid_to_windows(u), Some(k));
            assert_eq!(windows_to_hid(k), Some(u));
        }
        for u in 0x3a..=0x45 {
            assert!(hid_to_mac(u).is_some());
            assert!(hid_to_windows(u).is_some());
        }
        for u in 0x68..=0x73 {
            assert!(hid_to_windows(u).is_some());
        }
        assert_eq!(
            windows_key_to_hid(
                VK_HANGUL,
                ScanCode {
                    code: 0x38,
                    extended: true
                }
            ),
            Some(LANG1)
        );
        assert_eq!(hid_to_windows_vk(LANG2), Some(VK_HANJA));
        assert_eq!(mac_received_usage(LANG1, "capslock"), Some(0x39));
        assert_eq!(mac_received_usage(LANG1, "off"), None);
    }
    #[test]
    fn modifier_swap() {
        assert_eq!(swap_cmd_ctrl(0xe3, Os::Mac, Os::Mac, true), 0xe3);
        assert_eq!(swap_cmd_ctrl(0xe3, Os::Mac, Os::Windows, true), 0xe0);
        assert_eq!(swap_cmd_ctrl(0xe0, Os::Windows, Os::Mac, true), 0xe3);
    }
}

#[cfg(test)]
mod mac_hangul_tests {
    use super::*;
    use crate::{InputEvent, NetConfig};
    use std::collections::HashSet;
    fn key(usage: u16, down: bool) -> InputEvent { InputEvent::Key { usage, down } }
    fn run(sender: Os, receiver: Os, config: NetConfig, input: Vec<InputEvent>) -> Vec<InputEvent> {
        let mut mapper = MacHangul::default();
        let mut held = HashSet::new();
        input.into_iter().flat_map(|event| {
            if let InputEvent::Key { usage, down } = &event {
                if *down { held.insert(*usage); } else { held.remove(usage); }
            }
            mapper.map(event, sender, receiver, &config, &held)
        }).collect()
    }
    #[test]
    fn capslock_each_flip_is_one_hangul_tap() {
        let input = vec![key(0x39, true), key(0x39, false), key(0x39, true), key(0x39, false)];
        assert_eq!(run(Os::Mac, Os::Windows, NetConfig::default(), input),
            vec![key(LANG1, true), key(LANG1, false), key(LANG1, true), key(LANG1, false)]);
    }
    #[test]
    fn shift_space_releases_and_restores_both_shifts_and_swallows_repeat_and_up() {
        let input = vec![key(0xe1, true), key(0xe5, true), key(0x2c, true), key(0x2c, true),
            key(0xe1, false), key(0x2c, false), key(0xe5, false), key(0x2c, true), key(0x2c, false)];
        assert_eq!(run(Os::Mac, Os::Windows, NetConfig::default(), input), vec![
            key(0xe1, true), key(0xe5, true), key(0xe1, false), key(0xe5, false),
            key(LANG1, true), key(LANG1, false), key(0xe1, true), key(0xe5, true),
            key(0xe1, false), key(0xe5, false), key(0x2c, true), key(0x2c, false)]);
    }
    #[test]
    fn shortcuts_unchanged_for_other_directions_and_disabled_settings() {
        let input = vec![key(0x39, true), key(0x39, false), key(0xe1, true),
            key(0x2c, true), key(0x2c, false), key(0xe1, false)];
        for (sender, receiver) in [(Os::Mac, Os::Mac), (Os::Mac, Os::Linux),
            (Os::Windows, Os::Mac), (Os::Windows, Os::Windows), (Os::Windows, Os::Linux)] {
            assert_eq!(run(sender, receiver, NetConfig::default(), input.clone()), input);
        }
        let config = NetConfig { mac_capslock_to_hangul: false, mac_shift_space_to_hangul: false, ..NetConfig::default() };
        assert_eq!(run(Os::Mac, Os::Windows, config, input.clone()), input);
    }
    #[test]
    fn space_without_shift_or_with_other_modifiers_is_unchanged() {
        for modifier in [0, 0xe0, 0xe2, 0xe3, 0xe4, 0xe6, 0xe7] {
            let mut input = vec![key(0x2c, true), key(0x2c, false)];
            if modifier != 0 {
                input = vec![key(modifier, true), key(0xe1, true), key(0x2c, true),
                    key(0x2c, false), key(0xe1, false), key(modifier, false)];
            }
            assert_eq!(run(Os::Mac, Os::Windows, NetConfig::default(), input.clone()), input);
        }
    }
    #[test]
    fn either_shift_alone_works_and_options_are_independent() {
        for shift in [0xe1, 0xe5] {
            let config = NetConfig { mac_capslock_to_hangul: false, ..NetConfig::default() };
            assert_eq!(run(Os::Mac, Os::Windows, config, vec![key(shift, true), key(0x2c, true), key(0x2c, false)]),
                vec![key(shift, true), key(shift, false), key(LANG1, true), key(LANG1, false), key(shift, true)]);
        }
        let config = NetConfig { mac_shift_space_to_hangul: false, ..NetConfig::default() };
        assert_eq!(run(Os::Mac, Os::Windows, config, vec![key(0x39, true), key(0x39, false)]),
            vec![key(LANG1, true), key(LANG1, false)]);
    }
}
