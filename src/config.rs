//! User configuration, stored as TOML in the platform config directory.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Buttons that can switch, by HID++ control ID (REPROG_CONTROLS_V4).
pub const BUTTON_WHEEL_MODE: u16 = 0x00C4;
pub const BUTTON_GESTURE: u16 = 0x00C3;
pub const BUTTON_MIDDLE: u16 = 0x0052;
pub const BUTTONS: [u16; 3] = [BUTTON_WHEEL_MODE, BUTTON_GESTURE, BUTTON_MIDDLE];

/// Gesture: hold the button, move the mouse, let go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gesture {
    /// Pressed without moving.
    Press,
    Up,
    Down,
    Left,
    Right,
}

impl Gesture {
    /// Mouse movement (raw counts) below this while the button is held is a press.
    const MIN_MOVE: i32 = 50;

    /// Classifies the movement made while the button was held (y grows downwards).
    pub fn from_movement(dx: i32, dy: i32) -> Gesture {
        if dx.abs().max(dy.abs()) < Self::MIN_MOVE {
            Gesture::Press
        } else if dx.abs() >= dy.abs() {
            if dx > 0 { Gesture::Right } else { Gesture::Left }
        } else if dy > 0 {
            Gesture::Down
        } else {
            Gesture::Up
        }
    }
}

/// Where one hold-and-move gesture switches to: its own channels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MoveSetting {
    pub on: bool,
    pub channels: Vec<u8>,
}

impl Default for MoveSetting {
    fn default() -> Self {
        MoveSetting { on: false, channels: vec![1, 2, 3] }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Moves {
    pub up: MoveSetting,
    pub down: MoveSetting,
    pub left: MoveSetting,
    pub right: MoveSetting,
}

impl Moves {
    pub fn is_empty(&self) -> bool {
        *self == Moves::default()
    }

    /// Whether any direction is switched on.
    pub fn any_on(&self) -> bool {
        [&self.up, &self.down, &self.left, &self.right].iter().any(|m| m.on)
    }

    pub fn get(&self, g: Gesture) -> Option<&MoveSetting> {
        match g {
            Gesture::Up => Some(&self.up),
            Gesture::Down => Some(&self.down),
            Gesture::Left => Some(&self.left),
            Gesture::Right => Some(&self.right),
            Gesture::Press => None,
        }
    }

    pub fn get_mut(&mut self, g: Gesture) -> Option<&mut MoveSetting> {
        match g {
            Gesture::Up => Some(&mut self.up),
            Gesture::Down => Some(&mut self.down),
            Gesture::Left => Some(&mut self.left),
            Gesture::Right => Some(&mut self.right),
            Gesture::Press => None,
        }
    }
}

/// The next of `channels` after `current` (both 1-based), among the device's `count`,
/// like the Easy-Switch button under the mouse: with two channels it goes back and
/// forth, with one it always goes there. None when it is already there.
pub fn pick(channels: &[u8], current: u8, count: u8) -> Option<u8> {
    let usable: Vec<u8> = channels.iter().copied().filter(|&c| c <= count).collect();
    let next = match usable.iter().position(|&c| c == current) {
        Some(i) => usable[(i + 1) % usable.len()],
        None => *usable.first()?,
    };
    (next != current).then_some(next)
}

/// What one button does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ButtonSetting {
    /// Whether this button switches at all.
    pub on: bool,
    /// Channels (1-based) a press (without moving the mouse) steps through.
    pub channels: Vec<u8>,
    /// Hold-and-move gestures (used for the gesture button). While any is set, the
    /// mouse movement during a hold goes to flockswitch, so the cursor stays still.
    #[serde(skip_serializing_if = "Moves::is_empty")]
    pub moves: Moves,
}

impl Default for ButtonSetting {
    fn default() -> Self {
        ButtonSetting { on: false, channels: vec![1, 2, 3], moves: Moves::default() }
    }
}

impl ButtonSetting {
    /// The channel (1-based) a gesture leads to from the current one (0-based), given
    /// how many channels the device has; None when nothing should happen (already
    /// there, or that direction is off).
    pub fn channel_after(&self, gesture: Gesture, current0: u8, count: u8) -> Option<u8> {
        let current = current0 + 1;
        match self.moves.get(gesture) {
            None => pick(&self.channels, current, count),
            Some(m) if m.on => pick(&m.channels, current, count),
            Some(_) => None,
        }
    }

    /// Whether the button needs the mouse movement during a hold.
    pub fn uses_moves(&self) -> bool {
        self.moves.any_on()
    }
}

/// Each button that can switch has its own setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Buttons {
    /// Wheel mode-shift button (control 0x00C4).
    pub wheel_mode: ButtonSetting,
    /// Gesture (thumb) button (0x00C3).
    pub gesture: ButtonSetting,
    /// Middle button, the wheel click (0x0052).
    pub middle: ButtonSetting,
}

impl Default for Buttons {
    fn default() -> Self {
        Buttons {
            wheel_mode: ButtonSetting { on: true, ..ButtonSetting::default() },
            gesture: ButtonSetting::default(),
            middle: ButtonSetting::default(),
        }
    }
}

/// A side of the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

pub const EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

impl Edge {
    /// Name in the settings file and on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
            Edge::Top => "top",
            Edge::Bottom => "bottom",
        }
    }

    pub fn from_name(name: &str) -> Option<Edge> {
        EDGES.into_iter().find(|e| e.name() == name.trim().to_lowercase())
    }
}

/// The colour of the on-screen effects until another is chosen.
pub const DEFAULT_EFFECT_COLOR: &str = "#FFCC00";

/// Red, green and blue of a colour written as "#RRGGBB".
pub fn parse_color(text: &str) -> Option<[u8; 3]> {
    let hex = text.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// The sizes the settings window offers, in percent; the command line takes 50-200.
pub const EFFECT_SIZES: [u16; 4] = [75, 100, 125, 150];

/// "125" or "125%".
fn percent(text: &str) -> Option<u16> {
    text.trim().trim_end_matches('%').parse().ok()
}

/// A colour as the settings write it.
pub fn format_color([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02X}{g:02X}{b:02X}")
}

/// What is drawn near the cursor while it waits at a screen edge. The number of each
/// variant indexes the table of drawing functions in overlay_render.rs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeAnimStyle {
    None,
    #[default]
    Ring,
    Semicircle,
    Bar,
    Pulse,
    Laser,
    Vortex,
    Scan,
    Liquid,
    /// A different effect each time; see `Edges::countdown`. Has no drawing function of its own.
    Random,
}

pub const EDGE_ANIM_STYLES: [EdgeAnimStyle; 10] = [
    EdgeAnimStyle::None,
    EdgeAnimStyle::Ring,
    EdgeAnimStyle::Semicircle,
    EdgeAnimStyle::Bar,
    EdgeAnimStyle::Pulse,
    EdgeAnimStyle::Laser,
    EdgeAnimStyle::Vortex,
    EdgeAnimStyle::Scan,
    EdgeAnimStyle::Liquid,
    EdgeAnimStyle::Random,
];

/// A number below `n`, different from run to run. Good enough to choose an effect.
pub(crate) fn random_below(n: usize) -> usize {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new().build_hasher().finish() as usize % n
}

impl EdgeAnimStyle {
    /// Name in the settings file and on the command line.
    pub fn name(self) -> &'static str {
        match self {
            EdgeAnimStyle::None => "none",
            EdgeAnimStyle::Ring => "ring",
            EdgeAnimStyle::Semicircle => "semicircle",
            EdgeAnimStyle::Bar => "bar",
            EdgeAnimStyle::Pulse => "pulse",
            EdgeAnimStyle::Laser => "laser",
            EdgeAnimStyle::Vortex => "vortex",
            EdgeAnimStyle::Scan => "scan",
            EdgeAnimStyle::Liquid => "liquid",
            EdgeAnimStyle::Random => "random",
        }
    }

    fn from_name(name: &str) -> Option<EdgeAnimStyle> {
        EDGE_ANIM_STYLES.into_iter().find(|s| s.name() == name.trim().to_lowercase())
    }

    /// Whether the effect drains in a direction the user can choose.
    pub fn has_direction(self) -> bool {
        matches!(self, EdgeAnimStyle::Ring | EdgeAnimStyle::Semicircle | EdgeAnimStyle::Bar)
    }

    /// Whether the effect can shrink away instead of draining.
    pub fn can_shrink(self) -> bool {
        matches!(self, EdgeAnimStyle::Ring | EdgeAnimStyle::Semicircle)
    }
}

/// Which way the ring, semicircle or bar drains. Numbered as in overlay_render.rs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeAnimDirection {
    #[default]
    Clockwise,
    CounterClockwise,
    Both,
}

pub const EDGE_ANIM_DIRECTIONS: [EdgeAnimDirection; 3] =
    [EdgeAnimDirection::Clockwise, EdgeAnimDirection::CounterClockwise, EdgeAnimDirection::Both];

impl EdgeAnimDirection {
    fn from_name(name: &str) -> Option<EdgeAnimDirection> {
        Some(match name.trim().to_lowercase().as_str() {
            "clockwise" => EdgeAnimDirection::Clockwise,
            "counterclockwise" => EdgeAnimDirection::CounterClockwise,
            "both" => EdgeAnimDirection::Both,
            _ => return None,
        })
    }
}

/// The countdown effects that draw something; each has settings of its own.
pub const EDGE_EFFECTS: [EdgeAnimStyle; 8] = [
    EdgeAnimStyle::Ring,
    EdgeAnimStyle::Semicircle,
    EdgeAnimStyle::Bar,
    EdgeAnimStyle::Pulse,
    EdgeAnimStyle::Laser,
    EdgeAnimStyle::Vortex,
    EdgeAnimStyle::Scan,
    EdgeAnimStyle::Liquid,
];

/// How one countdown effect is drawn: `[edges.effects.<name>]` in the settings file.
/// `direction` is written only for the effects that have one (ring, semicircle, bar),
/// `shrink` only for those that can shrink (ring, semicircle); for the others they stay
/// at their defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EffectSettings {
    /// "#RRGGBB".
    pub color: String,
    /// Size in percent.
    pub size: u16,
    pub direction: EdgeAnimDirection,
    /// Shrinks away instead of draining.
    pub shrink: bool,
}

impl Default for EffectSettings {
    fn default() -> Self {
        EffectSettings { color: DEFAULT_EFFECT_COLOR.into(), size: 100, direction: EdgeAnimDirection::default(), shrink: false }
    }
}

/// The settings of every countdown effect. Unknown effect names in the file are skipped.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default)]
pub struct EdgeEffects {
    pub ring: EffectSettings,
    pub semicircle: EffectSettings,
    pub bar: EffectSettings,
    pub pulse: EffectSettings,
    pub laser: EffectSettings,
    pub vortex: EffectSettings,
    pub scan: EffectSettings,
    pub liquid: EffectSettings,
}

impl EdgeEffects {
    /// The settings of `style`; None for None and Random, which have none.
    pub fn get(&self, style: EdgeAnimStyle) -> Option<&EffectSettings> {
        Some(match style {
            EdgeAnimStyle::Ring => &self.ring,
            EdgeAnimStyle::Semicircle => &self.semicircle,
            EdgeAnimStyle::Bar => &self.bar,
            EdgeAnimStyle::Pulse => &self.pulse,
            EdgeAnimStyle::Laser => &self.laser,
            EdgeAnimStyle::Vortex => &self.vortex,
            EdgeAnimStyle::Scan => &self.scan,
            EdgeAnimStyle::Liquid => &self.liquid,
            EdgeAnimStyle::None | EdgeAnimStyle::Random => return None,
        })
    }

    pub fn get_mut(&mut self, style: EdgeAnimStyle) -> Option<&mut EffectSettings> {
        Some(match style {
            EdgeAnimStyle::Ring => &mut self.ring,
            EdgeAnimStyle::Semicircle => &mut self.semicircle,
            EdgeAnimStyle::Bar => &mut self.bar,
            EdgeAnimStyle::Pulse => &mut self.pulse,
            EdgeAnimStyle::Laser => &mut self.laser,
            EdgeAnimStyle::Vortex => &mut self.vortex,
            EdgeAnimStyle::Scan => &mut self.scan,
            EdgeAnimStyle::Liquid => &mut self.liquid,
            EdgeAnimStyle::None | EdgeAnimStyle::Random => return None,
        })
    }
}

/// Writes each effect with only the fields it has (see `EffectSettings`).
impl Serialize for EdgeEffects {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        #[derive(Serialize)]
        struct Written<'a> {
            color: &'a str,
            size: u16,
            #[serde(skip_serializing_if = "Option::is_none")]
            direction: Option<EdgeAnimDirection>,
            #[serde(skip_serializing_if = "Option::is_none")]
            shrink: Option<bool>,
        }
        let mut out = s.serialize_struct("EdgeEffects", EDGE_EFFECTS.len())?;
        for style in EDGE_EFFECTS {
            let e = self.get(style).expect("every effect has settings");
            let written = Written {
                color: &e.color,
                size: e.size,
                direction: style.has_direction().then_some(e.direction),
                shrink: style.can_shrink().then_some(e.shrink),
            };
            out.serialize_field(style.name(), &written)?;
        }
        out.end()
    }
}

/// Before 0.9.0 one colour, size, direction and shrink (`anim_color`, `anim_size`,
/// `anim_direction`, `anim_shrink`) served every effect. Fills in, from those, every
/// effect and field `[edges.effects]` lacks, so each effect starts as it looked.
fn fill_effects(edges: &mut toml::Table) {
    let legacy = |key: &str, default: toml::Value| edges.get(key).cloned().unwrap_or(default);
    let color = legacy("anim_color", toml::Value::String(DEFAULT_EFFECT_COLOR.into()));
    let size = legacy("anim_size", toml::Value::Integer(100));
    let direction = legacy("anim_direction", toml::Value::String("clockwise".into()));
    let shrink = legacy("anim_shrink", toml::Value::Boolean(false));
    let toml::Value::Table(effects) = edges.entry("effects").or_insert_with(|| toml::Value::Table(toml::Table::new())) else {
        return;
    };
    for style in EDGE_EFFECTS {
        let toml::Value::Table(e) = effects.entry(style.name()).or_insert_with(|| toml::Value::Table(toml::Table::new())) else {
            continue;
        };
        e.entry("color").or_insert_with(|| color.clone());
        e.entry("size").or_insert_with(|| size.clone());
        // Fields an effect does not have keep their defaults, whatever the file says.
        if style.has_direction() {
            e.entry("direction").or_insert_with(|| direction.clone());
        } else {
            e.remove("direction");
        }
        if style.can_shrink() {
            e.entry("shrink").or_insert_with(|| shrink.clone());
        } else {
            e.remove("shrink");
        }
    }
}

/// Whether `style` has the setting `field` (color, size, direction, shrink).
fn field_offered(style: EdgeAnimStyle, field: &str) -> bool {
    match field {
        "direction" => style.has_direction(),
        "shrink" => style.can_shrink(),
        _ => true,
    }
}

fn direction_value(value: &str, key: &str) -> Result<EdgeAnimDirection> {
    EdgeAnimDirection::from_name(value).with_context(|| format!("{key} must be clockwise, counterclockwise or both"))
}

/// What one countdown draws: the effect, and its settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Countdown {
    pub style: EdgeAnimStyle,
    pub settings: EffectSettings,
}

/// How the cursor is shown. The number of each variant indexes the table of drawing
/// functions in overlay_render.rs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocateStyle {
    #[default]
    Ring,
    Radar,
    Spotlight,
    /// A different effect each time; see `resolved`. Has no drawing function of its own.
    Random,
}

impl LocateStyle {
    /// The effect to draw now: for Random one of the others.
    pub fn resolved(self) -> LocateStyle {
        match self {
            LocateStyle::Random => [LocateStyle::Ring, LocateStyle::Radar, LocateStyle::Spotlight][random_below(3)],
            style => style,
        }
    }
}

/// A key that must be held while the mouse is shaken for the shake to count. Written
/// as in the Windows and macOS settings: none, ctrl, alt, shift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ShakeKey {
    #[default]
    None,
    Ctrl,
    Alt,
    Shift,
}

pub const SHAKE_KEYS: [ShakeKey; 4] = [ShakeKey::None, ShakeKey::Ctrl, ShakeKey::Alt, ShakeKey::Shift];

impl ShakeKey {
    /// Reads a key name, with the other names a settings file from another computer may
    /// use for the same keys (control, option).
    pub fn from_name(name: &str) -> Option<ShakeKey> {
        Some(match name.trim().to_lowercase().as_str() {
            "" | "none" | "off" => ShakeKey::None,
            "ctrl" | "control" => ShakeKey::Ctrl,
            "alt" | "option" | "opt" => ShakeKey::Alt,
            "shift" => ShakeKey::Shift,
            _ => return None,
        })
    }
}

/// A key this version does not offer (cmd from a Mac, or from a newer version) is read
/// as no key, rather than making the whole settings file unreadable.
impl<'de> Deserialize<'de> for ShakeKey {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<ShakeKey, D::Error> {
        let name = String::deserialize(d)?;
        Ok(ShakeKey::from_name(&name).unwrap_or_default())
    }
}

/// How long the shaking must go on, as the settings window offers it (ms).
pub const SHAKE_TIMES: [u64; 4] = [500, 1000, 1500, 2000];

/// Showing where the cursor is with a short highlight around it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Locate {
    pub on: bool,
    /// Shown when the shortcut below is pressed.
    pub on_hotkey: bool,
    /// e.g. "ctrl+alt+l"; empty for none.
    pub hotkey: String,
    /// Shown when the mouse arrives from another computer.
    pub on_arrival: bool,
    /// Shown when the mouse is shaken from side to side.
    pub on_shake: bool,
    /// How long (ms) the shaking must go on before it counts.
    pub shake_time_ms: u64,
    /// A key that must be held while shaking; none = any shake counts.
    pub shake_key: ShakeKey,
    pub style: LocateStyle,
    /// "#RRGGBB".
    pub color: String,
    /// Size of the highlight in percent.
    pub size: u16,
}

impl Default for Locate {
    fn default() -> Self {
        Locate {
            on: false,
            on_hotkey: true,
            hotkey: "ctrl+alt+l".into(),
            on_arrival: true,
            on_shake: false,
            shake_time_ms: 1000,
            shake_key: ShakeKey::None,
            style: LocateStyle::default(),
            color: DEFAULT_EFFECT_COLOR.into(),
            size: 100,
        }
    }
}

/// Switching at the screen edge: rest the cursor against an outer edge of the desktop
/// and the group goes to that edge's channel. Each computer watches its own screen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Edges {
    pub on: bool,
    /// How long (ms) the cursor must rest against the edge first.
    pub dwell_ms: u64,
    /// Channel (1-3) each edge goes to; 0 = that edge does nothing.
    pub left: u8,
    pub right: u8,
    pub top: u8,
    pub bottom: u8,
    /// Pixels at each end of an edge that do not count (hot corners, window buttons).
    pub corner_margin: u32,
    /// When the mouse comes back, put the cursor at the edge it left by.
    pub place_cursor: bool,
    /// The wait runs only while Control is held; letting go calls it off.
    pub require_control: bool,
    /// What is drawn while the cursor waits.
    pub anim_style: EdgeAnimStyle,
    /// The four below are the chosen effect's settings, written for versions before
    /// 0.9.0, which had one set for every effect; `effects` is what counts.
    pub anim_direction: EdgeAnimDirection,
    /// The ring or semicircle shrinks away instead of draining.
    pub anim_shrink: bool,
    /// "#RRGGBB".
    pub anim_color: String,
    /// Size of the countdown in percent.
    pub anim_size: u16,
    /// Each effect's own colour, size, direction and shrink. (A table: last.)
    pub effects: EdgeEffects,
}

impl Default for Edges {
    fn default() -> Self {
        Edges {
            on: false,
            dwell_ms: 500,
            left: 0,
            right: 0,
            top: 0,
            bottom: 0,
            corner_margin: 80,
            place_cursor: true,
            require_control: false,
            anim_style: EdgeAnimStyle::default(),
            anim_direction: EdgeAnimDirection::default(),
            anim_shrink: false,
            anim_color: DEFAULT_EFFECT_COLOR.into(),
            anim_size: 100,
            effects: EdgeEffects::default(),
        }
    }
}

impl Edges {
    /// The chosen effect's settings; None for None and Random.
    pub fn chosen(&self) -> Option<&EffectSettings> {
        self.effects.get(self.anim_style)
    }

    /// Copies the chosen effect's settings into the keys of versions before 0.9.0 (left
    /// as they are for None and Random, and for a field the effect does not have).
    pub fn mirror_legacy(&mut self) {
        let style = self.anim_style;
        let Some(e) = self.effects.get(style).cloned() else { return };
        self.anim_color = e.color;
        self.anim_size = e.size;
        if style.has_direction() {
            self.anim_direction = e.direction;
        }
        if style.can_shrink() {
            self.anim_shrink = e.shrink;
        }
    }

    /// What the next countdown draws. For Random a random effect with its own colour
    /// and size, and a random direction and shrink where it has them.
    pub fn countdown(&self) -> Countdown {
        self.countdown_with(random_below)
    }

    /// `countdown` with the random numbers from `below(n)`, a number below n.
    pub fn countdown_with(&self, mut below: impl FnMut(usize) -> usize) -> Countdown {
        let random = self.anim_style == EdgeAnimStyle::Random;
        let style = if random { EDGE_EFFECTS[below(EDGE_EFFECTS.len()) % EDGE_EFFECTS.len()] } else { self.anim_style };
        let mut settings = self.effects.get(style).cloned().unwrap_or_default();
        if random {
            if style.has_direction() {
                settings.direction = EDGE_ANIM_DIRECTIONS[below(EDGE_ANIM_DIRECTIONS.len()) % EDGE_ANIM_DIRECTIONS.len()];
            }
            if style.can_shrink() {
                settings.shrink = below(2) % 2 == 1;
            }
        }
        Countdown { style, settings }
    }

    pub fn channel(&self, edge: Edge) -> u8 {
        match edge {
            Edge::Left => self.left,
            Edge::Right => self.right,
            Edge::Top => self.top,
            Edge::Bottom => self.bottom,
        }
    }

    pub fn channel_mut(&mut self, edge: Edge) -> &mut u8 {
        match edge {
            Edge::Left => &mut self.left,
            Edge::Right => &mut self.right,
            Edge::Top => &mut self.top,
            Edge::Bottom => &mut self.bottom,
        }
    }

    /// The (first) edge that leads to `channel`.
    pub fn edge_for(&self, channel: u8) -> Option<Edge> {
        EDGES.into_iter().find(|&e| channel != 0 && self.channel(e) == channel)
    }

    /// Whether any edge has a channel.
    pub fn any(&self) -> bool {
        EDGES.into_iter().any(|e| self.channel(e) != 0)
    }
}

/// Vibration on a mouse with a haptic motor (MX Master 4): a pulse when the mouse is
/// switched away and when it comes back, and pulses while the cursor waits at a screen
/// edge. Waveforms are the IDs of HID++ feature 0x19B0 (see `device::waveform`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Haptics {
    /// A pulse as the mouse is switched away.
    pub on_switch: bool,
    /// A pulse when the mouse is back. Settings from before 0.9.0 had this under
    /// `on_switch`; `Config::parse` carries it over.
    pub on_arrive: bool,
    /// Pulses while the cursor rests at a screen edge, waiting to switch.
    pub on_edge: bool,
    pub switch_waveform: u8,
    pub arrive_waveform: u8,
    pub edge_waveform: u8,
    /// Strength in percent (1-100), put on the mouse whenever it is here; 0 leaves the
    /// mouse as Logi Options+ set it.
    pub intensity: u8,
    /// The pulses at a screen edge get faster and stronger as the switch nears, instead
    /// of repeating `edge_waveform` evenly.
    pub progressive: bool,
}

impl Default for Haptics {
    fn default() -> Self {
        Haptics {
            on_switch: true,
            on_arrive: true,
            on_edge: true,
            switch_waveform: crate::device::waveform::COMPLETED,
            arrive_waveform: crate::device::waveform::DAMP_COLLISION,
            edge_waveform: crate::device::waveform::SUBTLE_COLLISION,
            intensity: 0,
            progressive: true,
        }
    }
}

/// Names used for the buttons in the settings file and on the command line.
pub fn button_name(cid: u16) -> &'static str {
    match cid {
        BUTTON_WHEEL_MODE => "wheel_mode",
        BUTTON_GESTURE => "gesture",
        BUTTON_MIDDLE => "middle",
        _ => "?",
    }
}

fn button_by_name(name: &str) -> Option<u16> {
    let name = name.trim().to_lowercase();
    BUTTONS
        .into_iter()
        .find(|&c| button_name(c) == name)
        .or_else(|| hex_u16::parse(&name).ok().filter(|c| BUTTONS.contains(c)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Master switch. When false the buttons are left alone.
    pub enabled: bool,
    /// Which device carries the trigger buttons: "auto" (first mouse that has them),
    /// a wireless product ID such as "B042", or part of the device name.
    pub trigger_device: String,
    /// Devices that move together with the trigger device: ["all"], [] for none, or a
    /// list of wireless product IDs / name fragments, e.g. ["B366", "MX Keys"].
    pub follow: Vec<String>,
    /// Menu and settings language: "ko" or "en". New settings start with the system's.
    pub language: String,
    /// Look for a new version on GitHub at start and once a day.
    pub check_updates: bool,
    /// Install a new version found by that check (after its SHA-256 check) and restart
    /// into it. Not for a Homebrew install, which `brew upgrade` updates.
    pub auto_update: bool,
    /// Per-button settings. (Tables last: TOML writes tables after plain values.)
    pub buttons: Buttons,
    /// Keyboard shortcuts, one per channel.
    pub hotkeys: Hotkeys,
    /// Switching at the screen edge.
    pub edges: Edges,
    /// Vibration on a mouse with a haptic motor.
    pub haptics: Haptics,
    /// Showing where the cursor is.
    pub locate: Locate,
    /// The setup check's items set aside with "Later", and whether its first check is
    /// still to come. Written only when there is something, and kept out of settings
    /// backups (it is about this computer).
    #[serde(skip_serializing_if = "Setup::is_empty")]
    pub setup: Setup,
}

/// What the setup check remembers on this computer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Setup {
    /// Items set aside ("udev-rule", "tray-icon", "brew-update:0.11.1", …): no longer
    /// shown or counted until "Show again".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub snoozed: Vec<String>,
    /// Set when the tray app started without a settings file: its first setup check
    /// opens the settings window on the General tab if something is missing, then
    /// clears it (as does opening the settings window), so that happens once.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub first_check: bool,
}

impl Setup {
    pub fn is_empty(&self) -> bool {
        self.snoozed.is_empty() && !self.first_check
    }
}

/// Keyboard shortcuts that send the group and the master mouse to a channel; pressed on
/// any keyboard connected to this computer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub on: bool,
    /// Shortcut for channel 1, 2 and 3, e.g. "ctrl+alt+1"; empty for none.
    pub keys: Vec<String>,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys { on: false, keys: (1..=3).map(|n| format!("ctrl+alt+{n}")).collect() }
    }
}

impl Hotkeys {
    /// The shortcut of `channel` (1-based); empty when none.
    pub fn key(&self, channel: u8) -> &str {
        self.keys.get(usize::from(channel).wrapping_sub(1)).map_or("", String::as_str)
    }

    pub fn set_key(&mut self, channel: u8, key: &str) {
        self.keys.resize(3, String::new());
        if let Some(k) = self.keys.get_mut(usize::from(channel).wrapping_sub(1)) {
            *k = key.trim().to_lowercase();
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: true,
            trigger_device: "auto".into(),
            follow: vec!["all".into()],
            language: crate::i18n::system_language().into(),
            check_updates: true,
            auto_update: true,
            buttons: Buttons::default(),
            hotkeys: Hotkeys::default(),
            edges: Edges::default(),
            haptics: Haptics::default(),
            locate: Locate::default(),
            setup: Setup::default(),
        }
    }
}

/// Turns `mode = "fixed"` + `target = n` into `channels = [n]`.
fn fixed_to_channels(t: &mut toml::Table) {
    let fixed = t.get("mode").and_then(|m| m.as_str()) == Some("fixed");
    let target = t.get("target").and_then(|v| v.as_integer()).unwrap_or(1);
    t.remove("mode");
    t.remove("target");
    if fixed {
        t.insert("channels".into(), toml::Value::Array(vec![toml::Value::Integer(target)]));
    }
}

/// Settings of versions before 0.3.1: one mode and channel list for every button.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Legacy {
    mode: Option<String>,
    channels: Option<Vec<u8>>,
    target: Option<u8>,
    #[serde(deserialize_with = "hex_u16_list::deserialize")]
    triggers: Vec<u16>,
    #[serde(deserialize_with = "hex_u16_opt::deserialize")]
    trigger_cid: Option<u16>,
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("flockswitch")
            .join("config.toml")
    }

    /// The app was called "hostswitch" before 0.3.0. Moves that settings folder (config,
    /// remembered devices, saved button state) to the new name once, if present.
    pub fn migrate_old_name() {
        let Some(base) = dirs::config_dir() else { return };
        let (old, new) = (base.join("hostswitch"), base.join("flockswitch"));
        if old.is_dir() && !new.exists() {
            let _ = std::fs::rename(&old, &new);
        }
    }

    /// Loads the config, writing the defaults on first run so users have a file to edit.
    pub fn load_or_create() -> Result<Config> {
        let path = Self::path();
        if !path.exists() {
            let cfg = Config::default();
            cfg.save()?;
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let cfg = Config::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Reads a settings file, upgrading the shared mode/channels and `triggers` /
    /// `trigger_cid` of older versions into per-button settings.
    pub fn parse(text: &str) -> Result<Config> {
        let mut table: toml::Table = text.parse()?;
        // Before 0.3.1 a button could "always go to `target`": that is cycling through
        // that one channel. Applies to each button and gesture direction.
        if let Some(toml::Value::Table(buttons)) = table.get_mut("buttons") {
            for (_, button) in buttons.iter_mut() {
                if let toml::Value::Table(b) = button {
                    fixed_to_channels(b);
                    if let Some(toml::Value::Table(moves)) = b.get_mut("moves") {
                        for (_, m) in moves.iter_mut() {
                            if let toml::Value::Table(m) = m {
                                fixed_to_channels(m);
                            }
                        }
                    }
                }
            }
        }
        // Before 0.9.0 one switch covered the pulse on leaving and the one on arriving.
        if let Some(toml::Value::Table(h)) = table.get_mut("haptics") {
            if !h.contains_key("on_arrive") {
                let leaving = h.get("on_switch").and_then(|v| v.as_bool()).unwrap_or(true);
                h.insert("on_arrive".into(), toml::Value::Boolean(leaving));
            }
        }
        // Before 0.9.0 every countdown effect shared one colour, size and direction.
        if let Some(toml::Value::Table(e)) = table.get_mut("edges") {
            fill_effects(e);
        }
        let mut cfg: Config = toml::Value::Table(table.clone()).try_into()?;
        cfg.edges.mirror_legacy();
        if !table.contains_key("buttons") {
            let old: Legacy = toml::Value::Table(table).try_into()?;
            let mut setting = ButtonSetting::default();
            let mut channels = old.channels;
            match old.mode.as_deref() {
                Some("fixed") => channels = Some(vec![old.target.unwrap_or(1)]),
                // "toggle" (between the first two channels) is cycling through those two.
                Some("toggle") => {
                    if let Some(c) = &mut channels {
                        c.truncate(2);
                    }
                }
                _ => {}
            }
            if let Some(c) = channels {
                setting.channels = c;
            }
            let mut on = old.triggers;
            if on.is_empty() {
                on.extend(old.trigger_cid);
            }
            if !on.iter().any(|c| BUTTONS.contains(c)) {
                on = vec![BUTTON_WHEEL_MODE];
            }
            for cid in BUTTONS {
                *cfg.button_mut(cid) = ButtonSetting { on: on.contains(&cid), ..setting.clone() };
            }
        }
        if !["ko", "en"].contains(&cfg.language.as_str()) {
            cfg.language = crate::i18n::system_language().into();
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, self.to_text()?)?;
        Ok(())
    }

    /// The settings file's text, with the keys of older versions following the chosen
    /// countdown effect.
    pub fn to_text(&self) -> Result<String> {
        let mut cfg = self.clone();
        cfg.edges.mirror_legacy();
        Ok(toml::to_string_pretty(&cfg)?)
    }

    /// The setting of a trigger button (`cid` must be one of `BUTTONS`).
    pub fn button(&self, cid: u16) -> &ButtonSetting {
        match cid {
            BUTTON_GESTURE => &self.buttons.gesture,
            BUTTON_MIDDLE => &self.buttons.middle,
            _ => &self.buttons.wheel_mode,
        }
    }

    pub fn button_mut(&mut self, cid: u16) -> &mut ButtonSetting {
        match cid {
            BUTTON_GESTURE => &mut self.buttons.gesture,
            BUTTON_MIDDLE => &mut self.buttons.middle,
            _ => &mut self.buttons.wheel_mode,
        }
    }

    /// Control IDs of the buttons that are switched on.
    pub fn triggers(&self) -> Vec<u16> {
        BUTTONS.into_iter().filter(|&c| self.button(c).on).collect()
    }

    /// The shortcut in slot `n`: channel 1-3, or `hotkey::LOCATE`.
    pub fn shortcut(&self, n: u8) -> &str {
        if n == crate::hotkey::LOCATE { &self.locate.hotkey } else { self.hotkeys.key(n) }
    }

    pub fn set_shortcut(&mut self, n: u8, key: &str) {
        if n == crate::hotkey::LOCATE {
            self.locate.hotkey = key.trim().to_lowercase();
        } else {
            self.hotkeys.set_key(n, key);
        }
    }

    /// Changes one setting from the command line, validating the result.
    /// `channels` applies to every button, or to one with a prefix such as
    /// `gesture.channels`.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        // Change a copy, so an invalid value leaves the settings as they were.
        let mut next = self.clone();
        next.set_unchecked(key, value)?;
        next.edges.mirror_legacy();
        next.validate()?;
        *self = next;
        Ok(())
    }

    fn set_unchecked(&mut self, key: &str, value: &str) -> Result<()> {
        if key == "hotkeys" || key == "hotkeys.on" {
            self.hotkeys.on = value.parse().context("hotkeys must be true or false")?;
            return Ok(());
        }
        if let Some(what) = key.strip_prefix("haptics.").or((key == "haptics").then_some("on")) {
            let flag = || value.parse::<bool>().with_context(|| format!("{key} must be true or false"));
            let wave = || {
                if value.trim().eq_ignore_ascii_case("random") {
                    return Ok(crate::device::waveform::RANDOM);
                }
                value.trim().parse::<u8>().with_context(|| format!("{key} must be a waveform number (0-31) or random"))
            };
            match what {
                "on" => {
                    self.haptics.on_switch = flag()?;
                    self.haptics.on_arrive = self.haptics.on_switch;
                    self.haptics.on_edge = self.haptics.on_switch;
                }
                "switch" => self.haptics.on_switch = flag()?,
                "arrive" => self.haptics.on_arrive = flag()?,
                "edge" => self.haptics.on_edge = flag()?,
                "switch_waveform" => self.haptics.switch_waveform = wave()?,
                "arrive_waveform" => self.haptics.arrive_waveform = wave()?,
                "edge_waveform" => self.haptics.edge_waveform = wave()?,
                "progressive" => self.haptics.progressive = flag()?,
                "intensity" | "strength" => {
                    let v = value.trim().trim_end_matches('%');
                    self.haptics.intensity = if v.eq_ignore_ascii_case("options+") || v.eq_ignore_ascii_case("default") {
                        0
                    } else {
                        v.parse::<u8>().context("haptics.intensity must be 0 (as Options+ sets it) or 1-100 percent")?
                    };
                }
                _ => anyhow::bail!("unknown setting '{key}' (haptics.switch, haptics.arrive, haptics.edge, haptics.progressive, haptics.intensity, haptics.switch_waveform, haptics.arrive_waveform, haptics.edge_waveform)"),
            }
            return Ok(());
        }
        if let Some(what) = key.strip_prefix("locate.").or((key == "locate").then_some("on")) {
            let flag = || value.parse::<bool>().with_context(|| format!("{key} must be true or false"));
            match what {
                "on" => self.locate.on = flag()?,
                "arrival" => self.locate.on_arrival = flag()?,
                "shake" => self.locate.on_shake = flag()?,
                "shake_time" | "shake_time_ms" => {
                    let v = value.trim().trim_end_matches("ms").trim();
                    self.locate.shake_time_ms = v.parse().context("locate.shake_time must be milliseconds, e.g. 1000")?;
                }
                "shake_key" => {
                    self.locate.shake_key = ShakeKey::from_name(value).context("locate.shake_key must be none, ctrl, alt or shift")?;
                }
                "shortcut" => self.locate.on_hotkey = flag()?,
                "color" => self.locate.color = value.trim().to_uppercase(),
                "size" => self.locate.size = percent(value).context("locate.size must be a percentage, e.g. 125")?,
                "hotkey" => {
                    let value = if value.eq_ignore_ascii_case("none") { "" } else { value };
                    self.locate.hotkey = value.trim().to_lowercase();
                }
                "style" => {
                    self.locate.style = match value.trim().to_lowercase().as_str() {
                        "ring" => LocateStyle::Ring,
                        "radar" => LocateStyle::Radar,
                        "spotlight" => LocateStyle::Spotlight,
                        "random" => LocateStyle::Random,
                        _ => anyhow::bail!("locate.style must be ring, radar, spotlight or random"),
                    };
                }
                _ => anyhow::bail!("unknown setting '{key}' (locate, locate.style, locate.color, locate.size, locate.hotkey, locate.shortcut, locate.arrival, locate.shake, locate.shake_time, locate.shake_key)"),
            }
            return Ok(());
        }
        if key == "edges" || key == "edges.on" {
            self.edges.on = value.parse().context("edges must be true or false")?;
            return Ok(());
        }
        if let Some(what) = key.strip_prefix("edge.") {
            match what {
                "dwell" | "dwell_ms" => self.edges.dwell_ms = value.trim().parse().context("edge.dwell must be milliseconds, e.g. 500")?,
                "cursor" | "place_cursor" => self.edges.place_cursor = value.parse().context("edge.cursor must be true or false")?,
                "corner" | "corner_margin" => self.edges.corner_margin = value.trim().parse().context("edge.corner must be pixels, e.g. 80")?,
                "control" | "require_control" => self.edges.require_control = value.parse().context("edge.control must be true or false")?,
                "anim" | "anim_style" => {
                    self.edges.anim_style = EdgeAnimStyle::from_name(value)
                        .context("edge.anim must be none, ring, semicircle, bar, pulse, laser, vortex, scan, liquid or random")?;
                }
                // The keys of versions before 0.9.0: the chosen effect, or every effect
                // while none or Random is chosen.
                "anim_direction" | "anim_shrink" | "anim_color" | "anim_size" => {
                    let field = &what["anim_".len()..];
                    let chosen = self.edges.anim_style;
                    let styles: Vec<EdgeAnimStyle> =
                        if self.edges.effects.get(chosen).is_some() { vec![chosen] } else { EDGE_EFFECTS.to_vec() };
                    for style in styles.into_iter().filter(|s| field_offered(*s, field)) {
                        self.set_effect(style, field, value, key)?;
                    }
                    match field {
                        "direction" => self.edges.anim_direction = direction_value(value, key)?,
                        "shrink" => self.edges.anim_shrink = value.parse().with_context(|| format!("{key} must be true or false"))?,
                        "color" => self.edges.anim_color = value.trim().to_uppercase(),
                        _ => self.edges.anim_size = percent(value).with_context(|| format!("{key} must be a percentage, e.g. 125"))?,
                    }
                }
                effect_field if effect_field.contains('.') => {
                    let (name, field) = effect_field.split_once('.').unwrap_or_default();
                    let style = EDGE_EFFECTS
                        .into_iter()
                        .find(|s| s.name() == name.trim().to_lowercase())
                        .with_context(|| format!("unknown effect '{name}' (ring, semicircle, bar, pulse, laser, vortex, scan, liquid)"))?;
                    anyhow::ensure!(
                        ["color", "size", "direction", "shrink"].contains(&field),
                        "unknown setting '{key}' (edge.{name}.color, edge.{name}.size, edge.{name}.direction, edge.{name}.shrink)"
                    );
                    anyhow::ensure!(field_offered(style, field), "the {name} effect has no {field} (direction: ring, semicircle, bar; shrink: ring, semicircle)");
                    self.set_effect(style, field, value, key)?;
                }
                side => {
                    let edge = Edge::from_name(side).with_context(|| format!("unknown edge '{side}' (left, right, top, bottom)"))?;
                    let channel = if value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("off") {
                        0
                    } else {
                        value.trim().parse::<u8>().context("the edge's channel must be 1, 2, 3 or none")?
                    };
                    *self.edges.channel_mut(edge) = channel;
                }
            }
            return Ok(());
        }
        if let Some(n) = key.strip_prefix("hotkey.") {
            let channel = n.parse::<u8>().ok().filter(|c| (1..=3).contains(c)).context("use hotkey.1, hotkey.2 or hotkey.3")?;
            let value = if value.eq_ignore_ascii_case("none") { "" } else { value };
            self.hotkeys.set_key(channel, value);
            return Ok(());
        }
        let list = |v: &str| -> Vec<String> {
            v.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
        };
        let (which, field) = match key.split_once('.') {
            Some((b, f)) => {
                let cid = button_by_name(b).with_context(|| format!("unknown button '{b}' (wheel_mode, gesture, middle)"))?;
                (vec![cid], f)
            }
            None => (BUTTONS.to_vec(), key),
        };
        match field {
            "enabled" => self.enabled = value.parse().context("enabled must be true or false")?,
            "on" if which.len() == 1 => self.button_mut(which[0]).on = value.parse().context("on must be true or false")?,
            "channels" => {
                let channels: Vec<u8> = list(value)
                    .iter()
                    .map(|c| c.parse::<u8>().context("channels must be numbers like 1,2"))
                    .collect::<Result<_>>()?;
                which.iter().for_each(|&c| self.button_mut(c).channels = channels.clone());
            }
            "follow" => {
                self.follow = if value.eq_ignore_ascii_case("none") { vec![] } else { list(value) }
            }
            "trigger_device" => self.trigger_device = value.trim().to_string(),
            "language" => self.language = value.trim().to_lowercase(),
            "check_updates" => self.check_updates = value.parse().context("check_updates must be true or false")?,
            "auto_update" => self.auto_update = value.parse().context("auto_update must be true or false")?,
            // Which buttons are on, e.g. "wheel_mode,gesture" or "0x00C4,0x00C3".
            // `triggers` / `trigger_cid` are the names older versions used.
            "buttons" | "triggers" | "trigger_cid" => {
                let on: Vec<u16> = list(value)
                    .iter()
                    .map(|v| button_by_name(v).with_context(|| format!("unknown button '{v}' (wheel_mode, gesture, middle)")))
                    .collect::<Result<_>>()?;
                for cid in BUTTONS {
                    self.button_mut(cid).on = on.contains(&cid);
                }
            }
            _ => anyhow::bail!("unknown setting '{key}'"),
        }
        Ok(())
    }

    /// Sets `field` (color, size, direction, shrink) of one countdown effect.
    fn set_effect(&mut self, style: EdgeAnimStyle, field: &str, value: &str, key: &str) -> Result<()> {
        let Some(e) = self.edges.effects.get_mut(style) else { return Ok(()) };
        match field {
            "color" => e.color = value.trim().to_uppercase(),
            "size" => e.size = percent(value).with_context(|| format!("{key} must be a percentage, e.g. 125"))?,
            "direction" => e.direction = direction_value(value, key)?,
            "shrink" => e.shrink = value.parse().with_context(|| format!("{key} must be true or false"))?,
            _ => anyhow::bail!("unknown setting '{key}'"),
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        for cid in BUTTONS {
            let b = self.button(cid);
            let name = button_name(cid);
            anyhow::ensure!(b.channels.iter().all(|&c| (1..=3).contains(&c)), "{name}: channels must be between 1 and 3");
            if b.on {
                anyhow::ensure!(!b.channels.is_empty(), "{name}: turn on at least one channel");
            }
            for m in [&b.moves.up, &b.moves.down, &b.moves.left, &b.moves.right] {
                anyhow::ensure!(m.channels.iter().all(|&c| (1..=3).contains(&c)), "{name}: channels must be between 1 and 3");
                if b.on && m.on {
                    anyhow::ensure!(!m.channels.is_empty(), "{name}: turn on at least one channel");
                }
            }
        }
        anyhow::ensure!(!self.triggers().is_empty(), "turn on at least one button");
        anyhow::ensure!(["ko", "en"].contains(&self.language.as_str()), "language must be ko or en");
        for e in EDGES {
            anyhow::ensure!(self.edges.channel(e) <= 3, "edge.{}: the channel must be 1, 2, 3 or none", e.name());
        }
        anyhow::ensure!((100..=5000).contains(&self.edges.dwell_ms), "edge.dwell must be between 100 and 5000 ms");
        anyhow::ensure!(self.edges.corner_margin <= 1000, "edge.corner must be at most 1000 pixels");
        anyhow::ensure!((200..=5000).contains(&self.locate.shake_time_ms), "locate.shake_time must be between 200 and 5000 ms");
        anyhow::ensure!(parse_color(&self.edges.anim_color).is_some(), "edge.anim_color must be a colour like #FFCC00");
        anyhow::ensure!(parse_color(&self.locate.color).is_some(), "locate.color must be a colour like #FFCC00");
        for (key, size) in [("edge.anim_size", self.edges.anim_size), ("locate.size", self.locate.size)] {
            anyhow::ensure!(EFFECT_SIZES.contains(&size) || (50..=200).contains(&size), "{key} must be between 50 and 200 percent");
        }
        for style in EDGE_EFFECTS {
            let (name, e) = (style.name(), self.edges.effects.get(style).expect("every effect has settings"));
            anyhow::ensure!(parse_color(&e.color).is_some(), "edge.{name}.color must be a colour like #FFCC00");
            anyhow::ensure!((50..=200).contains(&e.size), "edge.{name}.size must be between 50 and 200 percent");
        }
        for w in [self.haptics.switch_waveform, self.haptics.arrive_waveform, self.haptics.edge_waveform] {
            anyhow::ensure!(w <= 31 || w == crate::device::waveform::RANDOM, "haptics: waveforms are numbered 0 to 31, or random");
        }
        anyhow::ensure!(self.haptics.intensity <= 100, "haptics.intensity must be 0 (as Options+ sets it) or 1-100 percent");
        anyhow::ensure!(self.hotkeys.keys.len() <= 3, "hotkeys: one shortcut per channel (1-3)");
        let mut seen = Vec::new();
        for key in self.hotkeys.keys.iter().chain([&self.locate.hotkey]).filter(|k| !k.is_empty()) {
            let parsed = crate::hotkey::parse(key)?;
            anyhow::ensure!(!seen.contains(&parsed), "shortcut '{key}' is used for two channels");
            seen.push(parsed);
        }
        Ok(())
    }

    /// True when every device follows ("all").
    pub fn follows_all(&self) -> bool {
        self.follow.iter().any(|f| f.eq_ignore_ascii_case("all"))
    }

    /// Ticks or unticks one device in the group. "all" is expanded to the given device
    /// list first, and collapsed back to "all" when every device is ticked again.
    pub fn toggle_follow(&mut self, devices: &[String], key: &str) {
        let mut set: Vec<String> = if self.follows_all() { devices.to_vec() } else { self.follow.clone() };
        if let Some(i) = set.iter().position(|f| f.eq_ignore_ascii_case(key)) {
            set.remove(i);
        } else {
            set.push(key.to_string());
        }
        let every = !devices.is_empty() && devices.iter().all(|d| set.iter().any(|f| f.eq_ignore_ascii_case(d)));
        self.follow = if every { vec!["all".into()] } else { set };
    }

    /// Whether a device with this product ID / name should follow the trigger device.
    pub fn follows(&self, wpid: Option<u16>, name: &str) -> bool {
        self.follow.iter().any(|f| {
            let f = f.trim();
            f.eq_ignore_ascii_case("all")
                || wpid.is_some_and(|w| f.eq_ignore_ascii_case(&format!("{w:04X}")))
                || (!f.is_empty() && name.to_lowercase().contains(&f.to_lowercase()))
        })
    }
}

/// Control IDs are written as "0x00C4"; either that form or a plain number is accepted.
mod hex_u16 {
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(untagged)]
    pub enum Raw {
        Num(u16),
        Text(String),
    }

    pub fn parse(t: &str) -> Result<u16, std::num::ParseIntError> {
        let t = t.trim();
        let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
        u16::from_str_radix(hex, 16)
    }

    pub fn from_raw<E: serde::de::Error>(r: Raw) -> Result<u16, E> {
        match r {
            Raw::Num(n) => Ok(n),
            Raw::Text(t) => parse(&t).map_err(E::custom),
        }
    }
}

mod hex_u16_list {
    use super::hex_u16::{self, Raw};
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u16>, D::Error> {
        Vec::<Raw>::deserialize(d)?.into_iter().map(hex_u16::from_raw).collect()
    }
}

mod hex_u16_opt {
    use super::hex_u16::{self, Raw};
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u16>, D::Error> {
        Option::<Raw>::deserialize(d)?.map(hex_u16::from_raw).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button(channels: &[u8]) -> ButtonSetting {
        ButtonSetting { on: true, channels: channels.to_vec(), ..ButtonSetting::default() }
    }

    fn press(b: &ButtonSetting, current0: u8, count: u8) -> Option<u8> {
        b.channel_after(Gesture::Press, current0, count)
    }

    #[test]
    fn cycle_wraps_like_the_hardware_button() {
        let b = button(&[1, 2, 3]);
        assert_eq!(press(&b, 0, 3), Some(2));
        assert_eq!(press(&b, 1, 3), Some(3));
        assert_eq!(press(&b, 2, 3), Some(1));
    }

    #[test]
    fn cycle_skips_channels_the_device_lacks() {
        assert_eq!(press(&button(&[1, 2, 3]), 1, 2), Some(1));
    }

    #[test]
    fn cycle_from_unlisted_channel_goes_to_first() {
        assert_eq!(press(&button(&[1, 2]), 2, 3), Some(1));
    }

    #[test]
    fn two_channels_bounce() {
        let b = button(&[1, 2]);
        assert_eq!(press(&b, 0, 3), Some(2));
        assert_eq!(press(&b, 1, 3), Some(1));
        assert_eq!(press(&b, 2, 3), Some(1));
    }

    #[test]
    fn one_channel_always_goes_there() {
        let b = button(&[2]);
        assert_eq!(press(&b, 1, 3), None);
        assert_eq!(press(&b, 0, 3), Some(2));
        assert_eq!(press(&b, 2, 3), Some(2));
        assert_eq!(press(&button(&[3]), 0, 2), None); // the device has no channel 3
    }

    #[test]
    fn gestures() {
        assert_eq!(Gesture::from_movement(10, -20), Gesture::Press);
        assert_eq!(Gesture::from_movement(300, 40), Gesture::Right);
        assert_eq!(Gesture::from_movement(-300, 100), Gesture::Left);
        assert_eq!(Gesture::from_movement(20, -200), Gesture::Up);
        assert_eq!(Gesture::from_movement(-30, 200), Gesture::Down);
        let mut b = button(&[1, 2]);
        assert!(!b.uses_moves());
        b.moves.left = MoveSetting { on: true, channels: vec![1] };
        b.moves.right = MoveSetting { on: true, channels: vec![3] };
        b.moves.up = MoveSetting { on: true, channels: vec![1, 3] };
        b.moves.down.channels = vec![2]; // set but off
        assert_eq!(b.channel_after(Gesture::Press, 0, 3), Some(2));
        assert_eq!(b.channel_after(Gesture::Right, 0, 3), Some(3));
        assert_eq!(b.channel_after(Gesture::Left, 0, 3), None); // already on 1
        assert_eq!(b.channel_after(Gesture::Up, 0, 3), Some(3));
        assert_eq!(b.channel_after(Gesture::Up, 2, 3), Some(1));
        assert_eq!(b.channel_after(Gesture::Down, 0, 3), None); // off
        assert!(b.uses_moves());
    }

    #[test]
    fn moves_round_trip_only_when_set() {
        let mut c = Config::default();
        assert!(!toml::to_string_pretty(&c).unwrap().contains("moves"));
        c.buttons.gesture.moves.left = MoveSetting { on: true, channels: vec![2] };
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(text.contains("[buttons.gesture.moves.left]"), "{text}");
        assert_eq!(Config::parse(&text).unwrap().buttons, c.buttons);
    }

    #[test]
    fn buttons_round_trip() {
        let mut c = Config::default();
        c.buttons.gesture = ButtonSetting { on: true, channels: vec![3], ..ButtonSetting::default() };
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(text.contains("[buttons.gesture]"), "{text}");
        assert!(!text.contains("mode =") && !text.contains("target"), "{text}");
        let back = Config::parse(&text).unwrap();
        assert_eq!(back.buttons, c.buttons);
        assert_eq!(back.triggers(), vec![BUTTON_WHEEL_MODE, BUTTON_GESTURE]);
    }

    #[test]
    fn fixed_target_becomes_one_channel() {
        let text = "[buttons.gesture]\non = true\nmode = \"fixed\"\ntarget = 3\nchannels = [1, 2]\n\
                    [buttons.gesture.moves.left]\non = true\nmode = \"fixed\"\ntarget = 2\n\
                    [buttons.wheel_mode]\non = true\nmode = \"cycle\"\ntarget = 3\nchannels = [1, 2]\n";
        let c = Config::parse(text).unwrap();
        assert_eq!(c.buttons.gesture.channels, vec![3]);
        assert_eq!(c.buttons.gesture.moves.left.channels, vec![2]);
        assert_eq!(c.buttons.wheel_mode.channels, vec![1, 2]);
    }

    #[test]
    fn shared_settings_of_older_versions_become_per_button() {
        let c = Config::parse("mode = \"fixed\"\ntarget = 2\nchannels = [1, 2]\ntriggers = [\"0x00C4\", \"0x0052\"]").unwrap();
        assert_eq!(c.triggers(), vec![BUTTON_WHEEL_MODE, BUTTON_MIDDLE]);
        for cid in BUTTONS {
            assert_eq!(c.button(cid).channels, vec![2]);
        }
        // "toggle" (first two channels) became cycling through those two.
        let c = Config::parse("mode = \"toggle\"\nchannels = [1, 3, 2]").unwrap();
        assert_eq!(c.buttons.wheel_mode.channels, vec![1, 3]);
        // A single trigger_cid (0.3.0), or nothing at all.
        assert_eq!(Config::parse("trigger_cid = \"0x00C3\"").unwrap().triggers(), vec![BUTTON_GESTURE]);
        assert_eq!(Config::parse("trigger_cid = 196").unwrap().triggers(), vec![BUTTON_WHEEL_MODE]);
        assert_eq!(Config::parse("enabled = true").unwrap().triggers(), vec![BUTTON_WHEEL_MODE]);
        // "auto" was offered for a while; it becomes the system's language.
        let lang = Config::parse("language = \"auto\"").unwrap().language;
        assert!(lang == "ko" || lang == "en");
    }

    #[test]
    fn set_from_command_line() {
        let mut c = Config::default();
        c.set("channels", "1, 2").unwrap();
        assert!(BUTTONS.iter().all(|&b| c.button(b).channels == vec![1, 2]));
        c.set("gesture.channels", "3").unwrap();
        assert_eq!(c.buttons.gesture.channels, vec![3]);
        assert_eq!(c.buttons.wheel_mode.channels, vec![1, 2]);
        c.set("middle.on", "true").unwrap();
        assert_eq!(c.triggers(), vec![BUTTON_WHEEL_MODE, BUTTON_MIDDLE]);
        c.set("buttons", "gesture, 0x00c4").unwrap();
        assert_eq!(c.triggers(), vec![BUTTON_WHEEL_MODE, BUTTON_GESTURE]);
        assert!(c.set("buttons", "").is_err());
        assert!(c.set("mode", "fixed").is_err());
        assert!(c.set("channels", "").is_err()); // a button that is on needs a channel
        c.set("follow", "none").unwrap();
        assert!(c.follow.is_empty());
        assert!(c.set("channels", "4").is_err());
        assert!(c.set("thumb.channels", "1").is_err());
        assert!(c.set("bogus", "1").is_err());
    }

    #[test]
    fn untick_one_device_from_all() {
        let mut c = Config::default(); // follow = ["all"]
        let devices = vec!["B366".to_string(), "B377".to_string()];
        c.toggle_follow(&devices, "B366");
        assert_eq!(c.follow, vec!["B377".to_string()]);
    }

    #[test]
    fn ticking_the_last_one_collapses_to_all() {
        let mut c = Config { follow: vec!["B377".into()], ..Config::default() };
        let devices = vec!["B366".to_string(), "B377".to_string()];
        c.toggle_follow(&devices, "B366");
        assert_eq!(c.follow, vec!["all".to_string()]);
    }

    #[test]
    fn follow_matching() {
        let mut c = Config::default();
        assert!(c.follows(Some(0xB366), "MX Mechanical"));
        c.follow = vec!["b366".into()];
        assert!(c.follows(Some(0xB366), "x"));
        assert!(!c.follows(Some(0xB042), "MX Master 4"));
        c.follow = vec!["keys".into()];
        assert!(c.follows(None, "MX Keys S"));
        c.follow = vec![];
        assert!(!c.follows(Some(0xB366), "MX Mechanical"));
    }

    #[test]
    fn edges() {
        let mut c = Config::default();
        assert!(!c.edges.on && !c.edges.any());
        c.set("edges", "true").unwrap();
        c.set("edge.right", "2").unwrap();
        c.set("edge.left", "none").unwrap();
        c.set("edge.dwell", "800").unwrap();
        assert!(c.edges.on && c.edges.any());
        assert_eq!(c.edges.channel(Edge::Right), 2);
        assert_eq!(c.edges.edge_for(2), Some(Edge::Right));
        assert_eq!(c.edges.edge_for(1), None);
        assert_eq!(c.edges.dwell_ms, 800);
        assert!(c.set("edge.right", "4").is_err());
        assert!(c.set("edge.dwell", "50").is_err());
        assert!(c.set("edge.middle", "1").is_err());
        let back = Config::parse(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(back.edges, c.edges);
        // Settings from before edges existed have them off.
        assert_eq!(Config::parse("enabled = true").unwrap().edges, Edges::default());
    }

    #[test]
    fn haptics() {
        let mut c = Config::default();
        assert!(c.haptics.on_switch && c.haptics.on_edge);
        c.set("haptics.edge", "false").unwrap();
        c.set("haptics.switch_waveform", "12").unwrap();
        assert!(c.haptics.on_switch && !c.haptics.on_edge);
        assert_eq!(c.haptics.switch_waveform, 12);
        assert!(c.set("haptics.edge_waveform", "40").is_err());
        c.set("haptics.intensity", "75%").unwrap();
        assert_eq!(c.haptics.intensity, 75);
        c.set("haptics.intensity", "options+").unwrap();
        assert_eq!(c.haptics.intensity, 0);
        assert!(c.set("haptics.intensity", "120").is_err());
        assert!(c.set("haptics.bogus", "1").is_err());
        assert!(c.haptics.progressive);
        c.set("haptics.progressive", "false").unwrap();
        assert!(!c.haptics.progressive);
        c.set("haptics.arrive_waveform", "random").unwrap();
        assert_eq!(c.haptics.arrive_waveform, crate::device::waveform::RANDOM);
        c.set("haptics.arrive", "false").unwrap();
        assert!(c.haptics.on_switch && !c.haptics.on_arrive);
        c.set("haptics", "false").unwrap();
        assert!(!c.haptics.on_switch && !c.haptics.on_arrive && !c.haptics.on_edge);
        // Older settings: the arrival pulse followed the switch pulse.
        let old = Config::parse("[haptics]\non_switch = false\non_edge = true\n").unwrap();
        assert!(!old.haptics.on_arrive && old.haptics.on_edge);
        assert!(Config::parse("[haptics]\non_edge = true\n").unwrap().haptics.on_arrive);
        let back = Config::parse(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(back.haptics, c.haptics);
        assert_eq!(Config::parse("enabled = true").unwrap().haptics, Haptics::default());
    }

    #[test]
    fn edge_countdown_settings() {
        let mut c = Config::default();
        assert!(!c.edges.require_control);
        assert_eq!(c.edges.anim_style, EdgeAnimStyle::Ring);
        c.set("edge.control", "true").unwrap();
        c.set("edge.anim", "Vortex").unwrap();
        c.set("edge.anim_direction", "both").unwrap();
        c.set("edge.anim_shrink", "true").unwrap();
        assert!(c.edges.require_control && c.edges.anim_shrink);
        assert_eq!(c.edges.anim_style, EdgeAnimStyle::Vortex);
        assert_eq!(c.edges.anim_direction, EdgeAnimDirection::Both);
        assert!(c.set("edge.anim", "sparkle").is_err());
        assert_eq!(c.edges.anim_color, DEFAULT_EFFECT_COLOR);
        c.set("edge.anim_color", "#0a84ff").unwrap();
        assert_eq!(c.edges.anim_color, "#0A84FF");
        assert_eq!(parse_color(&c.edges.anim_color), Some([0x0A, 0x84, 0xFF]));
        assert_eq!(format_color([0x0A, 0x84, 0xFF]), "#0A84FF");
        for bad in ["red", "#12345", "#GGGGGG", "0A84FF", "#한글한"] {
            assert!(c.set("edge.anim_color", bad).is_err(), "{bad}");
        }
        assert!(c.set("edge.anim_direction", "up").is_err());
        let back = Config::parse(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(back.edges, c.edges);
        c.set("edge.anim", "random").unwrap();
        for _ in 0..200 {
            let picked = c.edges.countdown().style;
            assert!(picked != EdgeAnimStyle::None && picked != EdgeAnimStyle::Random, "{picked:?}");
        }
        c.set("edge.anim", "bar").unwrap();
        assert_eq!(c.edges.countdown().style, EdgeAnimStyle::Bar);
        // The table of drawing functions in overlay_render.rs is indexed by these numbers.
        for (i, style) in EDGE_ANIM_STYLES.into_iter().enumerate() {
            assert_eq!(style as usize, i);
            assert_eq!(EdgeAnimStyle::from_name(style.name()), Some(style));
        }
    }

    #[test]
    fn each_effect_has_its_own_settings() {
        let mut c = Config::default();
        c.set("edge.ring.direction", "both").unwrap();
        c.set("edge.ring.color", "#0a84ff").unwrap();
        c.set("edge.bar.size", "150").unwrap();
        c.set("edge.semicircle.shrink", "true").unwrap();
        // Choosing "both" for the ring leaves the bar as it was.
        assert_eq!(c.edges.effects.ring.direction, EdgeAnimDirection::Both);
        assert_eq!(c.edges.effects.bar.direction, EdgeAnimDirection::Clockwise);
        assert_eq!(c.edges.effects.ring.color, "#0A84FF");
        assert_eq!(c.edges.effects.bar.color, DEFAULT_EFFECT_COLOR);
        assert_eq!((c.edges.effects.bar.size, c.edges.effects.ring.size), (150, 100));
        assert!(c.edges.effects.semicircle.shrink && !c.edges.effects.ring.shrink);
        // The keys older versions read follow the chosen effect (the ring).
        assert_eq!((c.edges.anim_color.as_str(), c.edges.anim_direction, c.edges.anim_size), ("#0A84FF", EdgeAnimDirection::Both, 100));
        c.set("edge.anim", "bar").unwrap();
        assert_eq!((c.edges.anim_color.as_str(), c.edges.anim_direction, c.edges.anim_size), (DEFAULT_EFFECT_COLOR, EdgeAnimDirection::Clockwise, 150));
        // The old keys change the chosen effect only...
        c.set("edge.anim_color", "#34c759").unwrap();
        assert_eq!((c.edges.effects.bar.color.as_str(), c.edges.effects.ring.color.as_str()), ("#34C759", "#0A84FF"));
        // ...or every effect while none or Random is chosen.
        c.set("edge.anim", "random").unwrap();
        c.set("edge.anim_size", "75").unwrap();
        assert!(EDGE_EFFECTS.iter().all(|&s| c.edges.effects.get(s).unwrap().size == 75));
        c.set("edge.anim", "none").unwrap();
        c.set("edge.anim_direction", "counterclockwise").unwrap();
        assert!(EDGE_EFFECTS.iter().filter(|s| s.has_direction()).all(|&s| c.edges.effects.get(s).unwrap().direction == EdgeAnimDirection::CounterClockwise));
        assert_eq!(c.edges.effects.pulse.direction, EdgeAnimDirection::Clockwise, "pulse has no direction");
        // Errors: an effect without that setting, unknown effects or fields, bad values.
        assert!(c.set("edge.pulse.direction", "both").is_err());
        assert!(c.set("edge.bar.shrink", "true").is_err());
        assert!(c.set("edge.sparkle.color", "#FFFFFF").is_err());
        assert!(c.set("edge.ring.speed", "2").is_err());
        assert!(c.set("edge.ring.color", "red").is_err());
        assert!(c.set("edge.ring.size", "300").is_err());
        assert!(c.set("edge.ring.direction", "up").is_err());
        assert_eq!(c.edges.effects.ring.color, "#0A84FF", "a refused value changes nothing");
        // Each effect is written with only the fields it has, and read back the same.
        let text = c.to_text().unwrap();
        assert!(text.contains("[edges.effects.ring]") && text.contains("[edges.effects.liquid]"), "{text}");
        let pulse = text.split("[edges.effects.pulse]").nth(1).unwrap().split("[edges.effects.").next().unwrap();
        assert!(pulse.contains("color") && !pulse.contains("direction") && !pulse.contains("shrink"), "{pulse}");
        let bar = text.split("[edges.effects.bar]").nth(1).unwrap().split("[edges.effects.").next().unwrap();
        assert!(bar.contains("direction") && !bar.contains("shrink"), "{bar}");
        assert_eq!(Config::parse(&text).unwrap().edges, c.edges);
    }

    #[test]
    fn effects_start_from_the_settings_of_older_versions() {
        let old = "[edges]\non = true\nright = 2\nanim_style = \"semicircle\"\nanim_color = \"#FF3B30\"\nanim_size = 125\n\
                   anim_direction = \"both\"\nanim_shrink = true\n";
        let c = Config::parse(old).unwrap();
        for style in EDGE_EFFECTS {
            let e = c.edges.effects.get(style).unwrap();
            assert_eq!((e.color.as_str(), e.size), ("#FF3B30", 125), "{style:?}");
            assert_eq!(e.direction, if style.has_direction() { EdgeAnimDirection::Both } else { EdgeAnimDirection::Clockwise }, "{style:?}");
            assert_eq!(e.shrink, style.can_shrink(), "{style:?}");
        }
        // A file with some effects (or fields) only: the rest come from the old keys.
        let mixed = "[edges]\nanim_color = \"#FF3B30\"\nanim_direction = \"both\"\n\
                     [edges.effects.ring]\ncolor = \"#FFFFFF\"\n[edges.effects.bar]\nsize = 75\ndirection = \"counterclockwise\"\n";
        let c = Config::parse(mixed).unwrap();
        assert_eq!((c.edges.effects.ring.color.as_str(), c.edges.effects.ring.size, c.edges.effects.ring.direction), ("#FFFFFF", 100, EdgeAnimDirection::Both));
        assert_eq!((c.edges.effects.bar.color.as_str(), c.edges.effects.bar.size, c.edges.effects.bar.direction), ("#FF3B30", 75, EdgeAnimDirection::CounterClockwise));
        assert_eq!(c.edges.effects.scan.color, "#FF3B30");
        // The chosen effect (the ring, by default) is what the old keys say from now on.
        assert_eq!(c.edges.anim_color, "#FFFFFF");
        // No [edges] at all: every effect at its defaults.
        assert_eq!(Config::parse("enabled = true").unwrap().edges.effects, EdgeEffects::default());
    }

    #[test]
    fn unknown_effects_and_fields_are_skipped() {
        let text = "[edges]\nanim_style = \"pulse\"\n[edges.effects.sparkle]\ncolor = \"#000000\"\n\
                    [edges.effects.pulse]\ncolor = \"#0A84FF\"\nsize = 150\nspeed = 3\ndirection = \"both\"\nshrink = true\n";
        let c = Config::parse(text).unwrap();
        let pulse = &c.edges.effects.pulse;
        assert_eq!((pulse.color.as_str(), pulse.size), ("#0A84FF", 150));
        // A direction or shrink on an effect that has none is not taken.
        assert_eq!((pulse.direction, pulse.shrink), (EdgeAnimDirection::Clockwise, false));
        assert!(!c.to_text().unwrap().contains("sparkle"));
        assert!(Config::parse("[edges.effects.ring]\ncolor = \"red\"\n").unwrap().validate().is_err());
    }

    #[test]
    fn random_draws_an_effect_with_its_settings_and_random_options() {
        let mut c = Config::default();
        c.set("edge.bar.color", "#FF2D92").unwrap();
        c.set("edge.bar.size", "125").unwrap();
        c.set("edge.bar.direction", "clockwise").unwrap();
        c.set("edge.ring.shrink", "false").unwrap();
        // A chosen effect keeps its saved direction and shrink.
        c.set("edge.anim", "bar").unwrap();
        let fixed = c.edges.countdown_with(|n| n - 1);
        assert_eq!((fixed.style, fixed.settings.clone()), (EdgeAnimStyle::Bar, c.edges.effects.bar.clone()));
        c.set("edge.anim", "random").unwrap();
        // The numbers drawn pick the bar (index 2), then its direction (both).
        let mut drawn = vec![2, 2].into_iter();
        let d = c.edges.countdown_with(|n| drawn.next().unwrap() % n);
        assert_eq!(d.style, EdgeAnimStyle::Bar);
        assert_eq!((d.settings.color.as_str(), d.settings.size, d.settings.direction), ("#FF2D92", 125, EdgeAnimDirection::Both));
        assert!(!d.settings.shrink, "the bar cannot shrink");
        // The ring: a direction and a shrink are drawn.
        let mut drawn = vec![0, 1, 1].into_iter();
        let d = c.edges.countdown_with(|n| drawn.next().unwrap() % n);
        assert_eq!((d.style, d.settings.direction, d.settings.shrink), (EdgeAnimStyle::Ring, EdgeAnimDirection::CounterClockwise, true));
        // An effect without options draws nothing more.
        let mut calls = 0;
        let d = c.edges.countdown_with(|_| {
            calls += 1;
            3
        });
        assert_eq!((d.style, calls), (EdgeAnimStyle::Pulse, 1));
        // With real random numbers every effect, direction and shrink comes up.
        let (mut styles, mut directions, mut shrinks) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..2000 {
            let d = c.edges.countdown();
            assert!(EDGE_EFFECTS.contains(&d.style), "{d:?}");
            styles.push(d.style);
            if d.style == EdgeAnimStyle::Ring {
                directions.push(d.settings.direction);
                shrinks.push(d.settings.shrink);
            }
        }
        assert!(EDGE_EFFECTS.iter().all(|s| styles.contains(s)));
        assert!(EDGE_ANIM_DIRECTIONS.iter().all(|d| directions.contains(d)));
        assert!(shrinks.contains(&true) && shrinks.contains(&false));
        // None draws nothing.
        c.set("edge.anim", "none").unwrap();
        assert_eq!(c.edges.countdown().style, EdgeAnimStyle::None);
    }

    #[test]
    fn locate() {
        let mut c = Config::default();
        assert!(!c.locate.on);
        assert_eq!(c.shortcut(crate::hotkey::LOCATE), "ctrl+alt+l");
        c.set("locate", "true").unwrap();
        c.set("locate.style", "spotlight").unwrap();
        c.set("locate.shake", "true").unwrap();
        c.set("locate.hotkey", "ctrl+alt+shift+k").unwrap();
        assert!(c.locate.on && c.locate.on_shake);
        assert_eq!(c.locate.style, LocateStyle::Spotlight);
        assert_eq!(c.locate.hotkey, "ctrl+alt+shift+k");
        assert!(c.set("locate.hotkey", "ctrl+alt+1").is_err(), "already channel 1's");
        assert!(c.set("locate.style", "laser").is_err());
        c.set("locate.style", "random").unwrap();
        assert!((0..100).all(|_| c.locate.style.resolved() != LocateStyle::Random));
        c.set("locate.style", "spotlight").unwrap();
        c.set("locate.size", "150%").unwrap();
        c.set("edge.anim_size", "75").unwrap();
        assert_eq!((c.locate.size, c.edges.anim_size), (150, 75));
        assert!(c.set("locate.size", "300").is_err());
        c.set("locate.color", "#34c759").unwrap();
        assert_eq!(c.locate.color, "#34C759");
        assert!(c.set("locate.color", "green").is_err());
        assert!(c.set("locate.bogus", "1").is_err());
        c.set("locate.hotkey", "none").unwrap();
        assert_eq!(c.locate.hotkey, "");
        let back = Config::parse(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(back.locate, c.locate);
        assert_eq!(Config::parse("enabled = true").unwrap().locate, Locate::default());
    }

    #[test]
    fn shake_settings() {
        let mut c = Config::default();
        assert_eq!((c.locate.shake_time_ms, c.locate.shake_key), (1000, ShakeKey::None));
        c.set("locate.shake_time", "1500").unwrap();
        c.set("locate.shake_key", "ctrl").unwrap();
        assert_eq!((c.locate.shake_time_ms, c.locate.shake_key), (1500, ShakeKey::Ctrl));
        c.set("locate.shake_time_ms", "500ms").unwrap();
        assert_eq!(c.locate.shake_time_ms, 500);
        assert!(c.set("locate.shake_time", "50").is_err());
        assert!(c.set("locate.shake_time", "soon").is_err());
        assert!(c.set("locate.shake_key", "hyper").is_err());
        c.set("locate.shake_key", "Shift").unwrap();
        let text = toml::to_string_pretty(&c).unwrap();
        assert!(text.contains("shake_key = \"shift\"") && text.contains("shake_time_ms = 500"), "{text}");
        assert_eq!(Config::parse(&text).unwrap().locate, c.locate);
        // Settings from before 0.8.0 shake for a second with no key.
        let old = Config::parse("[locate]\non = true\non_shake = true\n").unwrap();
        assert_eq!((old.locate.shake_time_ms, old.locate.shake_key), (1000, ShakeKey::None));
        // Another computer's key names: the same keys, or none for one Linux lacks.
        assert_eq!(Config::parse("[locate]\nshake_key = \"control\"\n").unwrap().locate.shake_key, ShakeKey::Ctrl);
        assert_eq!(Config::parse("[locate]\nshake_key = \"cmd\"\n").unwrap().locate.shake_key, ShakeKey::None);
    }

    #[test]
    fn hotkeys() {
        let mut c = Config::default();
        assert!(!c.hotkeys.on);
        assert_eq!(c.hotkeys.key(2), "ctrl+alt+2");
        c.set("hotkeys", "true").unwrap();
        c.set("hotkey.3", "Ctrl+Shift+F5").unwrap();
        assert_eq!(c.hotkeys.key(3), "ctrl+shift+f5");
        c.set("hotkey.1", "none").unwrap();
        assert_eq!(c.hotkeys.key(1), "");
        assert!(c.set("hotkey.3", "ctrl+alt+2").is_err(), "same shortcut twice");
        assert!(c.set("hotkey.2", "ctrl+alt+").is_err());
        assert!(c.set("hotkey.4", "ctrl+alt+4").is_err());
        let back = Config::parse(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(back.hotkeys, c.hotkeys);
        // Settings from before shortcuts existed get them off.
        assert!(!Config::parse("enabled = true").unwrap().hotkeys.on);
    }

    #[test]
    fn updates() {
        let mut c = Config::default();
        assert!(c.check_updates && c.auto_update);
        c.set("auto_update", "false").unwrap();
        assert!(!c.auto_update && c.check_updates);
        assert!(c.set("auto_update", "sometimes").is_err());
        assert!(!c.auto_update, "an invalid value changes nothing");
        c.set("check_updates", "false").unwrap();
        let back = Config::parse(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert!(!back.check_updates && !back.auto_update);
        // Settings from before 0.7.0 install updates by themselves.
        let old = Config::parse("enabled = true\ncheck_updates = true\n").unwrap();
        assert!(old.check_updates && old.auto_update);
    }
}
