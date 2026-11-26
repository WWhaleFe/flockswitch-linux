use crate::proto::Os;
use crate::InputEvent;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};
pub const LANG1: u16 = 0x90;
pub const LANG2: u16 = 0x91;
pub const VK_HANGUL: u16 = 0x15;
pub const VK_HANJA: u16 = 0x19;

const CAPS: u16 = 0x39;
const SPACE: u16 = 0x2c;
const RETURN: u16 = 0x28;
const BACKSPACE: u16 = 0x2a;
const PRINT_SCREEN: u16 = 0x46;
const HOME: u16 = 0x4a;
const END: u16 = 0x4d;
const RIGHT: u16 = 0x4f;
const LEFT: u16 = 0x50;
const DOWN: u16 = 0x51;
const UP: u16 = 0x52;
const F9: u16 = 0x42;
const LCTRL: u16 = 0xe0;
const LSHIFT: u16 = 0xe1;
const LALT: u16 = 0xe2;
const LGUI: u16 = 0xe3;
const RALT: u16 = 0xe6;
/// Caps Lock held this long is a real Caps Lock, not a Korean/English tap.
pub const CAPS_LONG_PRESS: Duration = Duration::from_millis(300);

/// How modifier keys are carried between a Mac and another system.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModMode {
    /// By function: Command is Ctrl, Control is Windows/Super.
    #[default]
    Function,
    /// By position on the keyboard: Control is Ctrl, Option is Windows/Super.
    Position,
    Off,
}
/// A key or chord that switches between Korean and English.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HangulKey {
    CapsLock,
    ControlSpace,
    ShiftSpace,
    SuperSpace,
    Lang1,
    RightAlt,
}
/// Key rules for one direction, from the sending OS to the receiving OS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DirRules {
    pub mod_mode: ModMode,
    /// Overrides on top of the mode: modifier usage -> usage.
    #[serde(with = "mods_serde")]
    pub mods: BTreeMap<u16, u16>,
    /// Keys that switch Korean/English on the sending computer.
    pub hangul_src: Vec<HangulKey>,
    /// What the receiving computer understands.
    pub hangul_dst: HangulKey,
    /// Mac source: Caps Lock held 300 ms or more stays a real Caps Lock.
    pub caps_long_press: bool,
    /// Mac Option+Return <-> the Hanja key.
    pub hanja: bool,
    /// Mac and other systems only: line edge, word, document edge, delete line/word.
    pub nav: [bool; 4],
    /// Mac and other systems only: screenshot shortcuts.
    pub capture: bool,
}
impl Default for DirRules {
    fn default() -> Self {
        Self {
            mod_mode: ModMode::Function,
            mods: BTreeMap::new(),
            hangul_src: vec![HangulKey::Lang1],
            hangul_dst: HangulKey::Lang1,
            caps_long_press: true,
            hanja: true,
            nav: [true; 4],
            capture: true,
        }
    }
}
mod mods_serde {
    use super::*;
    pub fn serialize<S: serde::Serializer>(m: &BTreeMap<u16, u16>, s: S) -> Result<S::Ok, S::Error> {
        m.iter().map(|(k, v)| (k.to_string(), *v)).collect::<BTreeMap<_, _>>().serialize(s)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<u16, u16>, D::Error> {
        BTreeMap::<String, u16>::deserialize(d)?
            .into_iter()
            .map(|(k, v)| k.parse().map(|k| (k, v)).map_err(serde::de::Error::custom))
            .collect()
    }
}
pub fn os_name(os: Os) -> &'static str {
    match os {
        Os::Mac => "mac",
        Os::Windows => "windows",
        Os::Linux => "linux",
        Os::Android => "android",
    }
}
/// The `keys` map key of a direction: "mac>windows".
pub fn pair_key(src: Os, dst: Os) -> String {
    format!("{}>{}", os_name(src), os_name(dst))
}
/// A valid direction: the sender is never Android and the two systems differ.
pub fn parse_pair(key: &str) -> Option<(Os, Os)> {
    let os = |s| match s {
        "mac" => Some(Os::Mac),
        "windows" => Some(Os::Windows),
        "linux" => Some(Os::Linux),
        "android" => Some(Os::Android),
        _ => None,
    };
    let (a, b) = key.split_once('>')?;
    let (src, dst) = (os(a)?, os(b)?);
    (src != Os::Android && src != dst).then_some((src, dst))
}
/// Every direction that has rules.
pub fn all_pairs() -> Vec<(Os, Os)> {
    let mut pairs = Vec::new();
    for src in [Os::Mac, Os::Windows, Os::Linux] {
        for dst in [Os::Mac, Os::Windows, Os::Linux, Os::Android] {
            if src != dst {
                pairs.push((src, dst));
            }
        }
    }
    pairs
}
impl DirRules {
    /// The defaults of a direction (the 2.0 behaviour).
    pub fn default_for(src: Os, dst: Os) -> Self {
        let (hangul_src, hangul_dst) = match (src, dst) {
            // A Mac user switches with Caps Lock or Shift+Space wherever the keys go.
            (Os::Mac, Os::Android) => (vec![HangulKey::CapsLock, HangulKey::ShiftSpace], HangulKey::ShiftSpace),
            (Os::Mac, _) => (vec![HangulKey::CapsLock, HangulKey::ShiftSpace], HangulKey::Lang1),
            (Os::Windows | Os::Linux, Os::Mac) => (vec![HangulKey::Lang1], HangulKey::CapsLock),
            (_, Os::Android) => (vec![HangulKey::Lang1], HangulKey::ShiftSpace),
            _ => (vec![HangulKey::Lang1], HangulKey::Lang1),
        };
        Self { hangul_src, hangul_dst, hanja: src != Os::Mac, ..Self::default() }
    }
    /// The rules the 2.0 settings stood for.
    pub fn from_legacy(src: Os, dst: Os, config: &crate::NetConfig) -> Self {
        let mut rules = Self::default_for(src, dst);
        if !config.swap_cmd_ctrl {
            rules.mod_mode = ModMode::Off;
        }
        if src == Os::Mac {
            rules.hangul_src.clear();
            if config.mac_capslock_to_hangul {
                rules.hangul_src.push(HangulKey::CapsLock);
            }
            if config.mac_shift_space_to_hangul {
                rules.hangul_src.push(HangulKey::ShiftSpace);
            }
        }
        if dst == Os::Mac && config.lang1_on_mac == "off" {
            rules.hangul_src.clear();
        }
        rules
    }
}

/// Maps a modifier usage for a direction: the override table, else the mode's table.
pub fn map_modifier(src: Os, dst: Os, rules: &DirRules, usage: u16) -> u16 {
    if !(LCTRL..=0xe7).contains(&usage) {
        return usage;
    }
    if let Some(&to) = rules.mods.get(&usage) {
        return to;
    }
    if rules.mod_mode == ModMode::Off || (src == Os::Mac) == (dst == Os::Mac) {
        return usage;
    }
    let (right, kind) = (if usage >= 0xe4 { 4 } else { 0 }, usage - LCTRL - if usage >= 0xe4 { 4 } else { 0 });
    // Kinds: 0 Ctrl, 1 Shift, 2 Alt/Option, 3 Win/Command.
    let to = match (src == Os::Mac, rules.mod_mode, kind) {
        (true, ModMode::Function, 3) => 0,
        (true, ModMode::Function, 0) => 3,
        (true, ModMode::Position, 2) => 3,
        (true, ModMode::Position, 3) => 2,
        (false, ModMode::Function, 0) => 3,
        (false, ModMode::Function, 3) => 0,
        (false, ModMode::Position, 3) => 2,
        (false, ModMode::Position, 2) => 3,
        _ => kind,
    };
    LCTRL + right + to
}

fn key(usage: u16, down: bool) -> InputEvent {
    InputEvent::Key { usage, down }
}
fn tap(usage: u16) -> [InputEvent; 2] {
    [key(usage, true), key(usage, false)]
}
/// `modifier` held around a tap of `usage`.
fn with_mod(modifier: u16, usage: u16) -> Vec<InputEvent> {
    vec![key(modifier, true), key(usage, true), key(usage, false), key(modifier, false)]
}
/// One tap of the receiving side's Korean/English key.
fn hangul_tap(dst: HangulKey) -> Vec<InputEvent> {
    match dst {
        HangulKey::Lang1 => tap(LANG1).into(),
        HangulKey::CapsLock => tap(CAPS).into(),
        HangulKey::RightAlt => tap(RALT).into(),
        HangulKey::ShiftSpace => with_mod(LSHIFT, SPACE),
        HangulKey::ControlSpace => with_mod(LCTRL, SPACE),
        HangulKey::SuperSpace => with_mod(LGUI, SPACE),
    }
}

#[derive(Clone, Copy)]
enum Caps {
    /// Pressed at this time; tap or real Caps Lock is decided on release or after 300 ms.
    Pending(Instant),
    Real,
}
/// Which modifiers are physically held on the sending computer.
#[derive(Default, Clone, Copy)]
struct Mods {
    ctrl: bool,
    shift: bool,
    alt: bool,
    gui: bool,
}
impl Mods {
    fn of(held: &HashSet<u16>) -> Self {
        let has = |a: u16| held.contains(&a) || held.contains(&(a + 4));
        Self { ctrl: has(LCTRL), shift: has(LSHIFT), alt: has(LALT), gui: has(LGUI) }
    }
    fn none_but_shift(self) -> bool {
        !(self.ctrl || self.alt || self.gui)
    }
    fn only(self, ctrl: bool, shift: bool, alt: bool, gui: bool) -> bool {
        (self.ctrl, self.shift, self.alt, self.gui) == (ctrl, shift, alt, gui)
    }
}

/// Applies a direction's key rules to the events leaving this computer. One per
/// controlled peer; reset whenever control starts or ends.
#[derive(Default)]
pub(crate) struct KeyMapper {
    held: HashSet<u16>,
    /// Source keys whose repeats and release are dropped (their tap was already sent).
    swallow: HashSet<u16>,
    /// Source keys rewritten as a whole chord: each press repeats it, the release is dropped.
    rewritten: HashSet<u16>,
    caps: Option<Caps>,
}
struct Ctx<'a> {
    src: Os,
    dst: Os,
    rules: &'a DirRules,
}
impl Ctx<'_> {
    fn map_mod(&self, usage: u16) -> u16 {
        map_modifier(self.src, self.dst, self.rules, usage)
    }
    /// `out` with the held modifiers in `release` lifted off the receiver first and put
    /// back after.
    fn around(&self, held: &HashSet<u16>, release: impl Fn(u16) -> bool, out: Vec<InputEvent>) -> Vec<InputEvent> {
        let lifted: Vec<u16> = (LCTRL..=0xe7).filter(|u| held.contains(u) && release(*u)).collect();
        let mut events: Vec<_> = lifted.iter().map(|&u| key(self.map_mod(u), false)).collect();
        events.extend(out);
        events.extend(lifted.iter().map(|&u| key(self.map_mod(u), true)));
        events
    }
}
fn is_kind(usage: u16, kind: u16) -> bool {
    (LCTRL..=0xe7).contains(&usage) && (usage - LCTRL) % 4 == kind
}
impl KeyMapper {
    pub(crate) fn map(&mut self, event: InputEvent, src: Os, dst: Os, rules: &DirRules, now: Instant) -> Vec<InputEvent> {
        let InputEvent::Key { usage, down } = event else { return vec![event] };
        let cx = Ctx { src, dst, rules };
        if !down {
            self.held.remove(&usage);
            if usage == CAPS {
                match self.caps.take() {
                    Some(Caps::Pending(at)) if at.elapsed_at(now) >= CAPS_LONG_PRESS => return tap(CAPS).into(),
                    Some(Caps::Pending(_)) => return hangul_tap(rules.hangul_dst),
                    Some(Caps::Real) => return vec![key(CAPS, false)],
                    None => {}
                }
            }
            if self.swallow.remove(&usage) || self.rewritten.remove(&usage) {
                return vec![];
            }
            return vec![key(cx.map_mod(usage), false)];
        }
        self.held.insert(usage);
        if self.swallow.contains(&usage) || (usage == CAPS && self.caps.is_some()) {
            return vec![];
        }
        if let Some(events) = self.hangul(&cx, usage, now) {
            return events;
        }
        if let Some(events) = self.chords(&cx, usage) {
            self.rewritten.insert(usage);
            return events;
        }
        self.rewritten.remove(&usage);
        vec![key(cx.map_mod(usage), true)]
    }
    /// Time-driven part: a Caps Lock held long enough turns into a real Caps Lock.
    pub(crate) fn tick(&mut self, now: Instant) -> Vec<InputEvent> {
        if let Some(Caps::Pending(at)) = self.caps {
            if at.elapsed_at(now) >= CAPS_LONG_PRESS {
                self.caps = Some(Caps::Real);
                return vec![key(CAPS, true)];
            }
        }
        vec![]
    }
    fn hangul(&mut self, cx: &Ctx, usage: u16, now: Instant) -> Option<Vec<InputEvent>> {
        let (rules, mods) = (cx.rules, Mods::of(&self.held));
        let dst = rules.hangul_dst;
        let listed = |k| rules.hangul_src.contains(&k);
        let plain = match usage {
            CAPS if listed(HangulKey::CapsLock) => HangulKey::CapsLock,
            LANG1 if listed(HangulKey::Lang1) => HangulKey::Lang1,
            RALT if listed(HangulKey::RightAlt) => HangulKey::RightAlt,
            _ => HangulKey::ShiftSpace,
        };
        if usage != SPACE {
            if plain == HangulKey::ShiftSpace || plain == dst {
                return None;
            }
            if plain == HangulKey::CapsLock && cx.src == Os::Mac && rules.caps_long_press {
                self.caps = Some(Caps::Pending(now));
                return Some(vec![]);
            }
            self.swallow.insert(usage);
            return Some(hangul_tap(dst));
        }
        let chord = if listed(HangulKey::ShiftSpace) && mods.only(false, true, false, false) {
            HangulKey::ShiftSpace
        } else if listed(HangulKey::ControlSpace) && mods.only(true, false, false, false) {
            HangulKey::ControlSpace
        } else if listed(HangulKey::SuperSpace) && mods.only(false, false, false, true) {
            HangulKey::SuperSpace
        } else {
            return None;
        };
        if chord == dst {
            return None;
        }
        self.swallow.insert(SPACE);
        let lift = |u: u16| match chord {
            HangulKey::ShiftSpace => is_kind(u, 1),
            HangulKey::ControlSpace => is_kind(u, 0),
            _ => is_kind(u, 3),
        };
        Some(cx.around(&self.held, lift, hangul_tap(dst)))
    }
    /// Hanja, navigation and capture chords. These match on the source keys, so they
    /// come before the modifier table, and write destination keys directly.
    fn chords(&self, cx: &Ctx, usage: u16) -> Option<Vec<InputEvent>> {
        let (rules, mods, held) = (cx.rules, Mods::of(&self.held), &self.held);
        let (mac_src, mac_dst) = (cx.src == Os::Mac, cx.dst == Os::Mac);
        let nav = rules.nav;
        let edge = |to: u16| tap(to).to_vec();
        // Hanja.
        if rules.hanja && cx.dst != Os::Android {
            if mac_src && usage == RETURN && mods.only(false, false, true, false) {
                return Some(cx.around(held, |u| is_kind(u, 2), tap(LANG2).into()));
            }
            if !mac_src && mac_dst && (usage == LANG2 || (cx.src == Os::Linux && usage == F9)) {
                return Some(with_mod(LALT, RETURN));
            }
            if !mac_src && !mac_dst && cx.src == Os::Linux && usage == F9 {
                return Some(tap(LANG2).into());
            }
        }
        if mac_src == mac_dst {
            return None;
        }
        if mac_src {
            // Mac to PC. Each case: kind of modifier consumed (2 Option, 3 Command) and output.
            let nav_case = match usage {
                LEFT | RIGHT if nav[0] && mods.only(false, mods.shift, false, true) => Some((3, edge(if usage == LEFT { HOME } else { END }))),
                LEFT | RIGHT if nav[1] && mods.only(false, mods.shift, true, false) => Some((2, with_mod(LCTRL, usage))),
                UP | DOWN if nav[2] && mods.only(false, mods.shift, false, true) => Some((3, with_mod(LCTRL, if usage == UP { HOME } else { END }))),
                BACKSPACE if nav[3] && mods.only(false, mods.shift, false, true) => {
                    let mut out = if mods.shift { edge(HOME) } else { with_mod(LSHIFT, HOME) };
                    out.extend(tap(BACKSPACE));
                    Some((3, out))
                }
                _ => None,
            };
            if let Some((kind, out)) = nav_case {
                return Some(cx.around(held, |u| is_kind(u, kind), out));
            }
            if rules.capture && mods.only(false, true, false, true) {
                let out = match usage {
                    0x20 => tap(PRINT_SCREEN).to_vec(),
                    0x21 if cx.dst == Os::Windows => {
                        vec![key(LGUI, true), key(LSHIFT, true), key(0x16, true), key(0x16, false), key(LSHIFT, false), key(LGUI, false)]
                    }
                    0x21 => with_mod(LSHIFT, PRINT_SCREEN),
                    _ => return None,
                };
                return Some(cx.around(held, |_| true, out));
            }
            return None;
        }
        // PC to Mac: Control (kind 0) is the consumed modifier, Shift stays.
        let nav_case = match usage {
            HOME | END if nav[0] && mods.none_but_shift() => Some(with_mod(LGUI, if usage == HOME { LEFT } else { RIGHT })),
            LEFT | RIGHT if nav[1] && mods.only(true, mods.shift, false, false) => Some(with_mod(LALT, usage)),
            HOME | END if nav[2] && mods.only(true, mods.shift, false, false) => Some(with_mod(LGUI, if usage == HOME { UP } else { DOWN })),
            BACKSPACE if nav[3] && mods.only(true, mods.shift, false, false) => Some(with_mod(LALT, BACKSPACE)),
            _ => None,
        };
        if let Some(out) = nav_case {
            return Some(cx.around(held, |u| is_kind(u, 0), out));
        }
        if rules.capture {
            let shot = |n: u16| vec![key(LGUI, true), key(LSHIFT, true), key(n, true), key(n, false), key(LSHIFT, false), key(LGUI, false)];
            let out = match (cx.src, usage) {
                (_, PRINT_SCREEN) if mods.only(false, false, false, false) => shot(0x20),
                (Os::Linux, PRINT_SCREEN) if mods.only(false, true, false, false) => shot(0x21),
                (Os::Windows, 0x16) if mods.only(false, true, false, true) => shot(0x21),
                _ => return None,
            };
            return Some(cx.around(held, |_| true, out));
        }
        None
    }
}
trait ElapsedAt {
    fn elapsed_at(self, now: Instant) -> Duration;
}
impl ElapsedAt for Instant {
    fn elapsed_at(self, now: Instant) -> Duration {
        now.saturating_duration_since(self)
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
mod rules_tests {
    use super::*;
    use crate::NetConfig;
    fn k(usage: u16, down: bool) -> InputEvent { key(usage, down) }
    fn run_at(src: Os, dst: Os, rules: &DirRules, input: Vec<InputEvent>) -> Vec<InputEvent> {
        let mut mapper = KeyMapper::default();
        let now = Instant::now();
        input.into_iter().flat_map(|e| mapper.map(e, src, dst, rules, now)).collect()
    }
    fn off(src: Os, dst: Os) -> DirRules {
        DirRules { mod_mode: ModMode::Off, ..DirRules::default_for(src, dst) }
    }
    fn taps(usage: u16) -> Vec<InputEvent> { vec![k(usage, true), k(usage, false)] }

    #[test]
    fn modifier_tables() {
        let usage = |src, dst, mode, u| map_modifier(src, dst, &DirRules { mod_mode: mode, ..DirRules::default() }, u);
        // Mac to PC by function: Command-Ctrl, Option-Alt, Control-Win, both sides.
        for (from, to) in [(0xe3, 0xe0), (0xe7, 0xe4), (0xe2, 0xe2), (0xe6, 0xe6), (0xe0, 0xe3), (0xe4, 0xe7), (0xe1, 0xe1), (0xe5, 0xe5)] {
            assert_eq!(usage(Os::Mac, Os::Windows, ModMode::Function, from), to, "{from:x}");
            assert_eq!(usage(Os::Mac, Os::Linux, ModMode::Function, from), to);
            assert_eq!(usage(Os::Mac, Os::Android, ModMode::Function, from), to);
            // PC to Mac is the inverse.
            assert_eq!(usage(Os::Windows, Os::Mac, ModMode::Function, to), from);
            assert_eq!(usage(Os::Linux, Os::Mac, ModMode::Function, to), from);
        }
        // By position: Control-Ctrl, Option-Win, Command-Alt.
        for (from, to) in [(0xe0, 0xe0), (0xe2, 0xe3), (0xe3, 0xe2), (0xe6, 0xe7), (0xe7, 0xe6), (0xe4, 0xe4), (0xe1, 0xe1)] {
            assert_eq!(usage(Os::Mac, Os::Windows, ModMode::Position, from), to, "{from:x}");
            assert_eq!(usage(Os::Windows, Os::Mac, ModMode::Position, to), from, "{to:x}");
        }
        for u in 0xe0..=0xe7 {
            assert_eq!(usage(Os::Mac, Os::Windows, ModMode::Off, u), u);
            assert_eq!(usage(Os::Windows, Os::Linux, ModMode::Function, u), u);
            assert_eq!(usage(Os::Linux, Os::Windows, ModMode::Position, u), u);
        }
        assert_eq!(usage(Os::Mac, Os::Windows, ModMode::Function, 0x04), 0x04);
    }
    #[test]
    fn modifier_overrides_win_in_every_mode() {
        let rules = DirRules { mods: BTreeMap::from([(0xe3, 0xe2), (0xe2, 0xe3)]), ..DirRules::default() };
        assert_eq!(map_modifier(Os::Mac, Os::Windows, &rules, 0xe3), 0xe2);
        assert_eq!(map_modifier(Os::Mac, Os::Windows, &rules, 0xe0), 0xe3);
        let rules = DirRules { mod_mode: ModMode::Off, ..rules };
        assert_eq!(map_modifier(Os::Mac, Os::Windows, &rules, 0xe2), 0xe3);
        assert_eq!(map_modifier(Os::Mac, Os::Windows, &rules, 0xe7), 0xe7);
    }
    #[test]
    fn pairs_and_defaults() {
        assert_eq!(pair_key(Os::Mac, Os::Windows), "mac>windows");
        assert_eq!(parse_pair("linux>android"), Some((Os::Linux, Os::Android)));
        assert_eq!(parse_pair("android>mac"), None);
        assert_eq!(parse_pair("mac>mac"), None);
        assert_eq!(parse_pair("mac>beos"), None);
        assert_eq!(all_pairs().len(), 9);
        let d = DirRules::default_for(Os::Mac, Os::Windows);
        assert_eq!((d.hangul_src.clone(), d.hangul_dst, d.hanja, d.caps_long_press), (vec![HangulKey::CapsLock, HangulKey::ShiftSpace], HangulKey::Lang1, false, true));
        let d = DirRules::default_for(Os::Linux, Os::Mac);
        assert_eq!((d.hangul_src.clone(), d.hangul_dst, d.hanja), (vec![HangulKey::Lang1], HangulKey::CapsLock, true));
        assert_eq!(DirRules::default_for(Os::Windows, Os::Android).hangul_dst, HangulKey::ShiftSpace);
        assert_eq!(DirRules::default_for(Os::Windows, Os::Linux).hangul_dst, HangulKey::Lang1);
        assert_eq!(d.nav, [true; 4]);
        assert!(d.capture);
    }
    #[test]
    fn rules_survive_json_and_toml_with_numeric_mods() {
        let rules = DirRules { mods: BTreeMap::from([(0xe3, 0xe2)]), hangul_src: vec![HangulKey::ControlSpace, HangulKey::CapsLock], ..DirRules::default() };
        let map = BTreeMap::from([("mac>windows".to_string(), rules.clone())]);
        let json = serde_json::to_string(&map).unwrap();
        assert_eq!(serde_json::from_str::<BTreeMap<String, DirRules>>(&json).unwrap(), map);
        assert!(json.contains("\"control_space\"") && json.contains("\"227\":226"));
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap { keys: BTreeMap<String, DirRules> }
        let text = toml::to_string(&Wrap { keys: map.clone() }).unwrap();
        assert_eq!(toml::from_str::<Wrap>(&text).unwrap().keys, map);
        let partial: DirRules = serde_json::from_str(r#"{"mod_mode":"position"}"#).unwrap();
        assert_eq!(partial.mod_mode, ModMode::Position);
        assert_eq!(partial.nav, [true; 4]);
    }
    #[test]
    fn legacy_fields_migrate() {
        let config = NetConfig::default();
        assert_eq!(DirRules::from_legacy(Os::Mac, Os::Windows, &config), DirRules::default_for(Os::Mac, Os::Windows));
        let config = NetConfig { swap_cmd_ctrl: false, mac_capslock_to_hangul: false, lang1_on_mac: "off".into(), ..NetConfig::default() };
        let r = DirRules::from_legacy(Os::Mac, Os::Windows, &config);
        assert_eq!((r.mod_mode, r.hangul_src), (ModMode::Off, vec![HangulKey::ShiftSpace]));
        let r = DirRules::from_legacy(Os::Windows, Os::Mac, &config);
        assert_eq!((r.mod_mode, r.hangul_src, r.hangul_dst), (ModMode::Off, vec![], HangulKey::CapsLock));
        let config = NetConfig { mac_shift_space_to_hangul: false, ..NetConfig::default() };
        assert_eq!(DirRules::from_legacy(Os::Mac, Os::Windows, &config).hangul_src, vec![HangulKey::CapsLock]);
        assert_eq!(DirRules::from_legacy(Os::Linux, Os::Mac, &config).hangul_src, vec![HangulKey::Lang1]);
    }

    // The 2.0 Mac Caps Lock and Shift+Space tests, through the new path.
    #[test]
    fn capslock_each_flip_is_one_hangul_tap() {
        let input = vec![k(0x39, true), k(0x39, false), k(0x39, true), k(0x39, false)];
        assert_eq!(run_at(Os::Mac, Os::Windows, &off(Os::Mac, Os::Windows), input),
            vec![k(LANG1, true), k(LANG1, false), k(LANG1, true), k(LANG1, false)]);
    }
    #[test]
    fn capslock_without_long_press_taps_on_down_and_swallows_the_up() {
        let rules = DirRules { caps_long_press: false, ..off(Os::Mac, Os::Windows) };
        let mut m = KeyMapper::default();
        let now = Instant::now();
        assert_eq!(m.map(k(0x39, true), Os::Mac, Os::Windows, &rules, now), taps(LANG1));
        assert_eq!(m.map(k(0x39, false), Os::Mac, Os::Windows, &rules, now), vec![]);
    }
    #[test]
    fn capslock_long_press_is_a_real_caps_lock() {
        let rules = off(Os::Mac, Os::Windows);
        let mut m = KeyMapper::default();
        let t0 = Instant::now();
        assert_eq!(m.map(k(0x39, true), Os::Mac, Os::Windows, &rules, t0), vec![]);
        assert_eq!(m.tick(t0 + Duration::from_millis(299)), vec![]);
        assert_eq!(m.tick(t0 + Duration::from_millis(300)), vec![k(0x39, true)]);
        assert_eq!(m.tick(t0 + Duration::from_millis(400)), vec![]);
        assert_eq!(m.map(k(0x39, true), Os::Mac, Os::Windows, &rules, t0 + Duration::from_millis(450)), vec![]);
        assert_eq!(m.map(k(0x39, false), Os::Mac, Os::Windows, &rules, t0 + Duration::from_millis(500)), vec![k(0x39, false)]);
        // Released at 300 ms without a tick in between: still a real Caps Lock, as a tap.
        assert_eq!(m.map(k(0x39, true), Os::Mac, Os::Windows, &rules, t0), vec![]);
        assert_eq!(m.map(k(0x39, false), Os::Mac, Os::Windows, &rules, t0 + Duration::from_millis(300)), taps(0x39));
        // A short press is the Korean/English tap.
        assert_eq!(m.map(k(0x39, true), Os::Mac, Os::Windows, &rules, t0), vec![]);
        assert_eq!(m.map(k(0x39, false), Os::Mac, Os::Windows, &rules, t0 + Duration::from_millis(120)), taps(LANG1));
    }
    #[test]
    fn shift_space_releases_and_restores_both_shifts_and_swallows_repeat_and_up() {
        let input = vec![k(0xe1, true), k(0xe5, true), k(0x2c, true), k(0x2c, true),
            k(0xe1, false), k(0x2c, false), k(0xe5, false), k(0x2c, true), k(0x2c, false)];
        assert_eq!(run_at(Os::Mac, Os::Windows, &off(Os::Mac, Os::Windows), input), vec![
            k(0xe1, true), k(0xe5, true), k(0xe1, false), k(0xe5, false),
            k(LANG1, true), k(LANG1, false), k(0xe1, true), k(0xe5, true),
            k(0xe1, false), k(0xe5, false), k(0x2c, true), k(0x2c, false)]);
    }
    #[test]
    fn shortcuts_unchanged_for_same_os_style_pairs_and_disabled_settings() {
        let input = vec![k(0x39, true), k(0x39, false), k(0xe1, true),
            k(0x2c, true), k(0x2c, false), k(0xe1, false)];
        for (src, dst) in [(Os::Mac, Os::Linux), (Os::Windows, Os::Mac), (Os::Windows, Os::Linux), (Os::Linux, Os::Windows)] {
            let rules = DirRules { hanja: false, nav: [false; 4], capture: false, ..off(src, dst) };
            let rules = if dst == Os::Mac { DirRules { hangul_src: vec![HangulKey::Lang1], ..rules } } else { rules };
            // A Mac source converts Caps Lock and Shift+Space by default; without them the keys pass.
            let rules = if src == Os::Mac { DirRules { hangul_src: vec![], ..rules } } else { rules };
            assert_eq!(run_at(src, dst, &rules, input.clone()), input, "{src:?}>{dst:?}");
        }
        let rules = DirRules { hangul_src: vec![], ..off(Os::Mac, Os::Windows) };
        assert_eq!(run_at(Os::Mac, Os::Windows, &rules, input.clone()), input);
    }
    #[test]
    fn mac_source_defaults_switch_korean_everywhere() {
        for dst in [Os::Windows, Os::Linux, Os::Android] {
            let rules = DirRules::default_for(Os::Mac, dst);
            assert_eq!(rules.hangul_src, vec![HangulKey::CapsLock, HangulKey::ShiftSpace], "{dst:?}");
        }
        assert_eq!(DirRules::default_for(Os::Mac, Os::Android).hangul_dst, HangulKey::ShiftSpace);
    }
    #[test]
    fn space_without_shift_or_with_other_modifiers_is_unchanged() {
        for modifier in [0, 0xe0, 0xe2, 0xe3, 0xe4, 0xe6, 0xe7] {
            let mut input = vec![k(0x2c, true), k(0x2c, false)];
            if modifier != 0 {
                input = vec![k(modifier, true), k(0xe1, true), k(0x2c, true),
                    k(0x2c, false), k(0xe1, false), k(modifier, false)];
            }
            assert_eq!(run_at(Os::Mac, Os::Windows, &off(Os::Mac, Os::Windows), input.clone()), input);
        }
    }
    #[test]
    fn either_shift_alone_works_and_options_are_independent() {
        for shift in [0xe1, 0xe5] {
            let rules = DirRules { hangul_src: vec![HangulKey::ShiftSpace], ..off(Os::Mac, Os::Windows) };
            assert_eq!(run_at(Os::Mac, Os::Windows, &rules, vec![k(shift, true), k(0x2c, true), k(0x2c, false)]),
                vec![k(shift, true), k(shift, false), k(LANG1, true), k(LANG1, false), k(shift, true)]);
        }
        let rules = DirRules { hangul_src: vec![HangulKey::CapsLock], caps_long_press: false, ..off(Os::Mac, Os::Windows) };
        assert_eq!(run_at(Os::Mac, Os::Windows, &rules, vec![k(0x39, true), k(0x39, false)]), taps(LANG1));
    }

    #[test]
    fn every_source_key_becomes_one_target_tap() {
        let shift_space = vec![k(LSHIFT, true), k(SPACE, true), k(SPACE, false), k(LSHIFT, false)];
        let cases: Vec<(Os, Os, HangulKey, HangulKey, Vec<InputEvent>, Vec<InputEvent>)> = vec![
            (Os::Windows, Os::Mac, HangulKey::Lang1, HangulKey::CapsLock, taps(LANG1), taps(0x39)),
            (Os::Windows, Os::Linux, HangulKey::RightAlt, HangulKey::Lang1, taps(0xe6), taps(LANG1)),
            (Os::Linux, Os::Windows, HangulKey::Lang1, HangulKey::RightAlt, taps(LANG1), taps(0xe6)),
            (Os::Windows, Os::Android, HangulKey::Lang1, HangulKey::ShiftSpace, taps(LANG1),
                vec![k(LSHIFT, true), k(SPACE, true), k(SPACE, false), k(LSHIFT, false)]),
            (Os::Linux, Os::Android, HangulKey::Lang1, HangulKey::ControlSpace, taps(LANG1),
                vec![k(LCTRL, true), k(SPACE, true), k(SPACE, false), k(LCTRL, false)]),
            (Os::Linux, Os::Windows, HangulKey::Lang1, HangulKey::SuperSpace, taps(LANG1),
                vec![k(LGUI, true), k(SPACE, true), k(SPACE, false), k(LGUI, false)]),
            (Os::Linux, Os::Windows, HangulKey::ShiftSpace, HangulKey::Lang1, shift_space.clone(),
                vec![k(LSHIFT, true), k(LSHIFT, false), k(LANG1, true), k(LANG1, false), k(LSHIFT, true), k(LSHIFT, false)]),
        ];
        for (src, dst, from, to, input, expected) in cases {
            let rules = DirRules { hangul_src: vec![from], hangul_dst: to, hanja: false, ..off(src, dst) };
            assert_eq!(run_at(src, dst, &rules, input), expected, "{src:?}>{dst:?} {from:?}->{to:?}");
        }
    }
    #[test]
    fn control_and_super_chords_are_lifted_and_restored_with_the_mod_table() {
        // Mac Control+Space to Windows Lang1: Control reaches Windows as Win, so that is lifted.
        let rules = DirRules { mod_mode: ModMode::Function, hangul_src: vec![HangulKey::ControlSpace], ..DirRules::default_for(Os::Mac, Os::Windows) };
        let out = run_at(Os::Mac, Os::Windows, &rules, vec![k(0xe0, true), k(SPACE, true), k(SPACE, false), k(0xe0, false)]);
        assert_eq!(out, vec![k(0xe3, true), k(0xe3, false), k(LANG1, true), k(LANG1, false), k(0xe3, true), k(0xe3, false)]);
        // Linux Super+Space to Mac Caps Lock.
        let rules = DirRules { hangul_src: vec![HangulKey::SuperSpace], ..DirRules::default_for(Os::Linux, Os::Mac) };
        let out = run_at(Os::Linux, Os::Mac, &rules, vec![k(0xe7, true), k(SPACE, true), k(SPACE, true), k(0xe7, false), k(SPACE, false)]);
        // Super is Control on the Mac in function mode.
        assert_eq!(out, vec![k(0xe4, true), k(0xe4, false), k(0x39, true), k(0x39, false), k(0xe4, true), k(0xe4, false)]);
        // Space with the wrong chord stays Space.
        let out = run_at(Os::Linux, Os::Mac, &rules, vec![k(0xe0, true), k(SPACE, true), k(SPACE, false)]);
        assert_eq!(out, vec![k(0xe3, true), k(SPACE, true), k(SPACE, false)]);
    }
    #[test]
    fn identical_source_and_target_key_passes_through() {
        let rules = DirRules::default_for(Os::Windows, Os::Linux);
        assert_eq!(run_at(Os::Windows, Os::Linux, &rules, vec![k(LANG1, true), k(LANG1, false)]), taps(LANG1));
    }

    #[test]
    fn hanja_both_ways() {
        let rules = DirRules { hanja: true, ..off(Os::Mac, Os::Windows) };
        let out = run_at(Os::Mac, Os::Linux, &rules, vec![k(0xe2, true), k(RETURN, true), k(RETURN, false), k(0xe2, false)]);
        assert_eq!(out, vec![k(0xe2, true), k(0xe2, false), k(LANG2, true), k(LANG2, false), k(0xe2, true), k(0xe2, false)]);
        // Off for the Mac source by default.
        let out = run_at(Os::Mac, Os::Windows, &DirRules::default_for(Os::Mac, Os::Windows), vec![k(RETURN, true), k(RETURN, false)]);
        assert_eq!(out, taps(RETURN));
        for (src, usage) in [(Os::Windows, LANG2), (Os::Linux, LANG2), (Os::Linux, F9)] {
            let rules = off(src, Os::Mac);
            let out = run_at(src, Os::Mac, &rules, vec![k(usage, true), k(usage, false)]);
            assert_eq!(out, vec![k(0xe2, true), k(RETURN, true), k(RETURN, false), k(0xe2, false)], "{src:?}");
        }
        let out = run_at(Os::Linux, Os::Windows, &off(Os::Linux, Os::Windows), vec![k(F9, true), k(F9, false)]);
        assert_eq!(out, taps(LANG2));
        let out = run_at(Os::Windows, Os::Linux, &off(Os::Windows, Os::Linux), vec![k(LANG2, true), k(LANG2, false)]);
        assert_eq!(out, taps(LANG2));
        let rules = DirRules { hanja: false, ..off(Os::Windows, Os::Mac) };
        assert_eq!(run_at(Os::Windows, Os::Mac, &rules, taps(LANG2)), taps(LANG2));
    }

    fn function(src: Os, dst: Os) -> DirRules { DirRules::default_for(src, dst) }
    #[test]
    fn nav_mac_to_pc_with_chord_ordering_and_restore() {
        let r = function(Os::Mac, Os::Windows);
        // Command+Left: Command is held as Ctrl; lifted around Home, then restored.
        let out = run_at(Os::Mac, Os::Windows, &r, vec![k(0xe3, true), k(LEFT, true), k(LEFT, false), k(0xe3, false)]);
        assert_eq!(out, vec![k(0xe0, true), k(0xe0, false), k(HOME, true), k(HOME, false), k(0xe0, true), k(0xe0, false)]);
        let out = run_at(Os::Mac, Os::Linux, &r, vec![k(0xe7, true), k(RIGHT, true), k(RIGHT, true), k(RIGHT, false)]);
        assert_eq!(out, vec![k(0xe4, true), k(0xe4, false), k(END, true), k(END, false), k(0xe4, true),
            k(0xe4, false), k(END, true), k(END, false), k(0xe4, true)]);
        // Option+Left -> Ctrl+Left (Option is Alt, lifted).
        let out = run_at(Os::Mac, Os::Windows, &r, vec![k(0xe2, true), k(LEFT, true), k(LEFT, false)]);
        assert_eq!(out, vec![k(0xe2, true), k(0xe2, false), k(0xe0, true), k(LEFT, true), k(LEFT, false), k(0xe0, false), k(0xe2, true)]);
        // Command+Up/Down -> Ctrl+Home/End.
        for (usage, to) in [(UP, HOME), (DOWN, END)] {
            let out = run_at(Os::Mac, Os::Windows, &r, vec![k(0xe3, true), k(usage, true)]);
            assert_eq!(out, vec![k(0xe0, true), k(0xe0, false), k(0xe0, true), k(to, true), k(to, false), k(0xe0, false), k(0xe0, true)]);
        }
        // Command+Backspace -> Shift+Home, Backspace.
        let out = run_at(Os::Mac, Os::Windows, &r, vec![k(0xe3, true), k(BACKSPACE, true), k(BACKSPACE, false)]);
        assert_eq!(out, vec![k(0xe0, true), k(0xe0, false), k(LSHIFT, true), k(HOME, true), k(HOME, false), k(LSHIFT, false),
            k(BACKSPACE, true), k(BACKSPACE, false), k(0xe0, true)]);
    }
    #[test]
    fn nav_keeps_shift_selecting_and_can_be_switched_off() {
        let r = function(Os::Mac, Os::Windows);
        let out = run_at(Os::Mac, Os::Windows, &r, vec![k(0xe1, true), k(0xe3, true), k(LEFT, true)]);
        // Shift is never lifted; Command (as Ctrl) is.
        assert_eq!(out, vec![k(0xe1, true), k(0xe0, true), k(0xe0, false), k(HOME, true), k(HOME, false), k(0xe0, true)]);
        let out = run_at(Os::Mac, Os::Windows, &r, vec![k(0xe1, true), k(0xe3, true), k(BACKSPACE, true)]);
        assert_eq!(out[2..], [k(0xe0, false), k(HOME, true), k(HOME, false), k(BACKSPACE, true), k(BACKSPACE, false), k(0xe0, true)]);
        for i in 0..4 {
            let mut nav = [true; 4];
            nav[i] = false;
            let r = DirRules { nav, capture: false, ..function(Os::Mac, Os::Windows) };
            let cases = [(0xe3, LEFT), (0xe2, LEFT), (0xe3, UP), (0xe3, BACKSPACE)];
            let out = run_at(Os::Mac, Os::Windows, &r, vec![k(cases[i].0, true), k(cases[i].1, true)]);
            let mapped = map_modifier(Os::Mac, Os::Windows, &r, cases[i].0);
            assert_eq!(out, vec![k(mapped, true), k(cases[i].1, true)], "nav[{i}] off");
        }
        // A second modifier cancels the rewrite.
        let out = run_at(Os::Mac, Os::Windows, &r, vec![k(0xe3, true), k(0xe2, true), k(LEFT, true)]);
        assert_eq!(out, vec![k(0xe0, true), k(0xe2, true), k(LEFT, true)]);
    }
    #[test]
    fn nav_pc_to_mac() {
        let r = function(Os::Windows, Os::Mac);
        let run = |input| run_at(Os::Windows, Os::Mac, &r, input);
        // Home/End -> Command+Left/Right.
        assert_eq!(run(vec![k(HOME, true), k(HOME, false)]), vec![k(0xe3, true), k(LEFT, true), k(LEFT, false), k(0xe3, false)]);
        assert_eq!(run(vec![k(END, true)])[1], k(RIGHT, true));
        // Shift+End keeps Shift on the result.
        let out = run(vec![k(0xe1, true), k(END, true)]);
        assert_eq!(out, vec![k(0xe1, true), k(0xe3, true), k(RIGHT, true), k(RIGHT, false), k(0xe3, false)]);
        // Ctrl+Left/Right -> Option+Left/Right; Ctrl (as Command) is lifted.
        let out = run(vec![k(0xe0, true), k(LEFT, true), k(LEFT, false)]);
        assert_eq!(out, vec![k(0xe3, true), k(0xe3, false), k(LALT, true), k(LEFT, true), k(LEFT, false), k(LALT, false), k(0xe3, true)]);
        // Ctrl+Home/End -> Command+Up/Down.
        let out = run(vec![k(0xe4, true), k(END, true)]);
        assert_eq!(out[1..], [k(0xe7, false), k(0xe3, true), k(DOWN, true), k(DOWN, false), k(0xe3, false), k(0xe7, true)]);
        // Ctrl+Backspace -> Option+Backspace.
        let out = run(vec![k(0xe0, true), k(BACKSPACE, true)]);
        assert_eq!(out[1..], [k(0xe3, false), k(LALT, true), k(BACKSPACE, true), k(BACKSPACE, false), k(LALT, false), k(0xe3, true)]);
        // Alt+Home is not navigation.
        assert_eq!(run(vec![k(0xe2, true), k(HOME, true)]), vec![k(0xe2, true), k(HOME, true)]);
        let r = DirRules { nav: [false; 4], ..r };
        assert_eq!(run_at(Os::Windows, Os::Mac, &r, vec![k(HOME, true), k(HOME, false)]), vec![k(HOME, true), k(HOME, false)]);
    }
    #[test]
    fn nav_release_after_rewritten_key_is_dropped_but_other_keys_pass() {
        let r = function(Os::Mac, Os::Windows);
        let out = run_at(Os::Mac, Os::Windows, &r, vec![k(0xe3, true), k(LEFT, true), k(LEFT, false), k(0xe3, false), k(LEFT, true), k(LEFT, false)]);
        assert_eq!(out[out.len() - 4..], [k(0xe0, true), k(0xe0, false), k(LEFT, true), k(LEFT, false)]);
        assert_eq!(out.iter().filter(|e| **e == k(LEFT, false)).count(), 1);
    }

    #[test]
    fn capture_shortcuts() {
        let shot = |src, dst, input| run_at(src, dst, &function(src, dst), input);
        let mac = |n| vec![k(0xe3, true), k(0xe1, true), k(n, true), k(n, false)];
        // Mac Cmd+Shift+3 -> Print Screen (modifiers lifted: Command as Ctrl, Shift).
        let out = shot(Os::Mac, Os::Windows, mac(0x20));
        assert_eq!(out[2..], [k(0xe1, false), k(0xe0, false), k(PRINT_SCREEN, true), k(PRINT_SCREEN, false), k(0xe1, true), k(0xe0, true)]);
        // Cmd+Shift+4 -> Win+Shift+S on Windows, Shift+Print Screen on Linux.
        let out = shot(Os::Mac, Os::Windows, mac(0x21));
        assert_eq!(out[2..], [k(0xe1, false), k(0xe0, false), k(0xe3, true), k(0xe1, true), k(0x16, true), k(0x16, false),
            k(0xe1, false), k(0xe3, false), k(0xe1, true), k(0xe0, true)]);
        let out = shot(Os::Mac, Os::Linux, mac(0x21));
        assert_eq!(out[4..8], [k(0xe1, true), k(PRINT_SCREEN, true), k(PRINT_SCREEN, false), k(0xe1, false)]);
        // And back.
        let shot_mac = |n| vec![k(0xe3, true), k(0xe1, true), k(n, true), k(n, false), k(0xe1, false), k(0xe3, false)];
        assert_eq!(shot(Os::Windows, Os::Mac, taps(PRINT_SCREEN)), shot_mac(0x20));
        assert_eq!(shot(Os::Linux, Os::Mac, taps(PRINT_SCREEN)), shot_mac(0x20));
        let out = shot(Os::Windows, Os::Mac, vec![k(0xe3, true), k(0xe1, true), k(0x16, true)]);
        assert_eq!(out[2..], [k(0xe1, false), k(0xe0, false), k(0xe3, true), k(0xe1, true), k(0x21, true), k(0x21, false),
            k(0xe1, false), k(0xe3, false), k(0xe1, true), k(0xe0, true)]);
        let out = shot(Os::Linux, Os::Mac, vec![k(0xe1, true), k(PRINT_SCREEN, true)]);
        assert_eq!(out[1..], [k(0xe1, false), k(0xe3, true), k(0xe1, true), k(0x21, true), k(0x21, false), k(0xe1, false), k(0xe3, false), k(0xe1, true)]);
        // Shift+Print Screen on Windows and Win+Shift+S on Linux are left alone.
        assert_eq!(shot(Os::Windows, Os::Mac, vec![k(0xe1, true), k(PRINT_SCREEN, true)]), vec![k(0xe1, true), k(PRINT_SCREEN, true)]);
        // Off.
        let r = DirRules { capture: false, ..function(Os::Windows, Os::Mac) };
        assert_eq!(run_at(Os::Windows, Os::Mac, &r, taps(PRINT_SCREEN)), taps(PRINT_SCREEN));
        let r = DirRules { capture: false, nav: [false; 4], ..function(Os::Mac, Os::Linux) };
        assert_eq!(run_at(Os::Mac, Os::Linux, &r, mac(0x20)).len(), 4);
    }
    #[test]
    fn plain_keys_pass_with_modifier_table() {
        let r = function(Os::Mac, Os::Windows);
        let out = run_at(Os::Mac, Os::Windows, &r, vec![k(0xe3, true), k(0x04, true), k(0x04, false), k(0xe3, false), k(0xe0, true), k(0xe0, false)]);
        assert_eq!(out, vec![k(0xe0, true), k(0x04, true), k(0x04, false), k(0xe0, false), k(0xe3, true), k(0xe3, false)]);
    }
}
