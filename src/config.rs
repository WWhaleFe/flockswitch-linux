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
}

impl Default for Edges {
    fn default() -> Self {
        Edges { on: false, dwell_ms: 500, left: 0, right: 0, top: 0, bottom: 0, corner_margin: 80, place_cursor: true }
    }
}

impl Edges {
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
    /// A pulse as the mouse leaves, and one when it is back.
    pub on_switch: bool,
    /// Pulses while the cursor rests at a screen edge, waiting to switch.
    pub on_edge: bool,
    pub switch_waveform: u8,
    pub arrive_waveform: u8,
    pub edge_waveform: u8,
}

impl Default for Haptics {
    fn default() -> Self {
        Haptics {
            on_switch: true,
            on_edge: true,
            switch_waveform: crate::device::waveform::COMPLETED,
            arrive_waveform: crate::device::waveform::DAMP_COLLISION,
            edge_waveform: crate::device::waveform::SUBTLE_COLLISION,
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
    /// Per-button settings. (Tables last: TOML writes tables after plain values.)
    pub buttons: Buttons,
    /// Keyboard shortcuts, one per channel.
    pub hotkeys: Hotkeys,
    /// Switching at the screen edge.
    pub edges: Edges,
    /// Vibration on a mouse with a haptic motor.
    pub haptics: Haptics,
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
            buttons: Buttons::default(),
            hotkeys: Hotkeys::default(),
            edges: Edges::default(),
            haptics: Haptics::default(),
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
        let mut cfg: Config = toml::Value::Table(table.clone()).try_into()?;
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
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
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

    /// Changes one setting from the command line, validating the result.
    /// `channels` applies to every button, or to one with a prefix such as
    /// `gesture.channels`.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        // Change a copy, so an invalid value leaves the settings as they were.
        let mut next = self.clone();
        next.set_unchecked(key, value)?;
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
            let wave = || value.trim().parse::<u8>().with_context(|| format!("{key} must be a waveform number (0-31)"));
            match what {
                "on" => {
                    self.haptics.on_switch = flag()?;
                    self.haptics.on_edge = self.haptics.on_switch;
                }
                "switch" => self.haptics.on_switch = flag()?,
                "edge" => self.haptics.on_edge = flag()?,
                "switch_waveform" => self.haptics.switch_waveform = wave()?,
                "arrive_waveform" => self.haptics.arrive_waveform = wave()?,
                "edge_waveform" => self.haptics.edge_waveform = wave()?,
                _ => anyhow::bail!("unknown setting '{key}' (haptics.switch, haptics.edge, haptics.switch_waveform, haptics.arrive_waveform, haptics.edge_waveform)"),
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
        for w in [self.haptics.switch_waveform, self.haptics.arrive_waveform, self.haptics.edge_waveform] {
            anyhow::ensure!(w <= 31, "haptics: waveforms are numbered 0 to 31");
        }
        anyhow::ensure!(self.hotkeys.keys.len() <= 3, "hotkeys: one shortcut per channel (1-3)");
        let mut seen = Vec::new();
        for key in self.hotkeys.keys.iter().filter(|k| !k.is_empty()) {
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
        assert!(c.set("haptics.bogus", "1").is_err());
        c.set("haptics", "false").unwrap();
        assert!(!c.haptics.on_switch && !c.haptics.on_edge);
        let back = Config::parse(&toml::to_string_pretty(&c).unwrap()).unwrap();
        assert_eq!(back.haptics, c.haptics);
        assert_eq!(Config::parse("enabled = true").unwrap().haptics, Haptics::default());
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
}
