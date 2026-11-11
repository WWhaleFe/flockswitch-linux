//! The parts of edge switching on Wayland that are plain logic, with no desktop to talk
//! to: which backend to use, where the pointer barriers (input capture) or the 1-pixel
//! strips (layer shell) go, how a captured pointer is followed while it rests against
//! an edge, and the wait at the edge itself. Only `std` is used, so the tests run on any
//! computer (`rustc --edition 2021 --test src/edge_logic.rs`).
//!
//! The wait (`Dwell`) does exactly what the X11 watcher in edge.rs does each time it
//! reads the cursor: the same countdown, pulses, one decision per visit, cooldown.

use std::time::{Duration, Instant};

/// After one switch, the next needs the cursor to leave the edge and this much time
/// (the same as edge.rs).
pub const COOLDOWN: Duration = Duration::from_secs(2);
/// A captured pointer has to come this far back into the screen (in pixels, net of
/// pushing outwards again) before it counts as having left the edge. On X11 one pixel
/// is enough; a captured pointer is not drawn moving, so a hand's tremor should not
/// let it go.
pub const INWARD_SLACK: f64 = 2.0;
/// Pieces of an outer edge shorter than this get no barrier or strip.
pub const MIN_SEGMENT: i32 = 16;
/// With "only while Ctrl is held", a layer-shell strip keeps the keyboard this long
/// for Ctrl to be pressed before it gives the keyboard back.
pub const CTRL_GRACE: Duration = Duration::from_millis(1500);
/// A captured pointer that is not counting down (the switch would not happen, or Ctrl
/// is not held) is given back once it slides this far along the edge: it is not drawn
/// moving while captured.
pub const SLIDE_RELEASE: f64 = 32.0;

/// A side of the screen, in the order of `config::EDGES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

pub const SIDES: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];

impl Side {
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn name(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Top => "top",
            Side::Bottom => "bottom",
        }
    }

    /// Left and right run up and down the screen.
    pub fn vertical(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
}

/// A display (a portal "zone", a Wayland output), in the desktop's logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Zone {
    /// The range along `side`: y for left and right, x for top and bottom.
    fn span(&self, side: Side) -> (i32, i32) {
        if side.vertical() {
            (self.y, self.y + self.h)
        } else {
            (self.x, self.x + self.w)
        }
    }
}

/// A piece of an outer edge that switches: the side, the display it belongs to, and the
/// range along the edge (`start..end`, in desktop pixels: y for left and right, x for
/// top and bottom). `live_start..live_end` is the part that counts; it is narrower than
/// the piece itself only when a desktop insists on barriers that fill whole edges, and
/// the corners are then left out by following the pointer instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub side: Side,
    pub zone: Zone,
    pub start: i32,
    pub end: i32,
    pub live_start: i32,
    pub live_end: i32,
}

impl Segment {
    /// The pointer barrier for this piece, as the InputCapture portal wants it: on the
    /// first column or row of pixels beyond the edge for right and bottom, both ends
    /// included.
    pub fn barrier(&self) -> (i32, i32, i32, i32) {
        let z = self.zone;
        match self.side {
            Side::Left => (z.x, self.start, z.x, self.end - 1),
            Side::Right => (z.x + z.w, self.start, z.x + z.w, self.end - 1),
            Side::Top => (self.start, z.y, self.end - 1, z.y),
            Side::Bottom => (self.start, z.y + z.h, self.end - 1, z.y + z.h),
        }
    }

    /// Where along its display's edge this piece starts, and how long it is (for a
    /// layer-shell strip anchored to that edge).
    pub fn offset_and_length(&self) -> (i32, i32) {
        (self.start - self.zone.span(self.side).0, self.end - self.start)
    }

    /// The point on the outermost row or column of pixels, at `along`.
    pub fn edge_point(&self, along: f64) -> (f64, f64) {
        let z = self.zone;
        match self.side {
            Side::Left => (f64::from(z.x), along),
            Side::Right => (f64::from(z.x + z.w - 1), along),
            Side::Top => (along, f64::from(z.y)),
            Side::Bottom => (along, f64::from(z.y + z.h - 1)),
        }
    }
}

/// The pieces of the displays' outer edges on the sides that have a channel: an edge of
/// a display beyond which no other display continues (between two displays the cursor
/// just crosses over), less `margin` pixels at each end of the display's edge (hot
/// corners, window buttons). The same rule as `edge::edge_at`, as ranges instead of a
/// test at one point. With `whole_edges` the barriers cover each display's edge from
/// end to end (as KDE's portal requires) and only the live part leaves out the corners;
/// an edge that partly continues onto another display then gets nothing.
pub fn outer_segments(zones: &[Zone], sides: [bool; 4], margin: i32, whole_edges: bool) -> Vec<Segment> {
    let margin = margin.max(0);
    let mut out = Vec::new();
    for (i, d) in zones.iter().enumerate() {
        for side in SIDES {
            if !sides[side.index()] || d.w <= 0 || d.h <= 0 {
                continue;
            }
            // The row or column just beyond this edge, and which ranges along it other
            // displays cover there.
            let blocked: Vec<(i32, i32)> = zones
                .iter()
                .enumerate()
                .filter(|&(j, _)| j != i)
                .filter_map(|(_, o)| {
                    // Whether `o` covers that row or column (x = d.x - 1 for the left edge).
                    let beyond = match side {
                        Side::Left => o.x < d.x && d.x <= o.x + o.w,
                        Side::Right => o.x <= d.x + d.w && d.x + d.w < o.x + o.w,
                        Side::Top => o.y < d.y && d.y <= o.y + o.h,
                        Side::Bottom => o.y <= d.y + d.h && d.y + d.h < o.y + o.h,
                    };
                    beyond.then(|| o.span(side))
                })
                .collect();
            let (lo, hi) = d.span(side);
            let (live_lo, live_hi) = (lo + margin, hi - margin);
            if whole_edges {
                let touches = blocked.iter().any(|&(a, b)| a < hi && b > lo);
                if !touches && live_hi - live_lo >= MIN_SEGMENT {
                    out.push(Segment { side, zone: *d, start: lo, end: hi, live_start: live_lo, live_end: live_hi });
                }
                continue;
            }
            for (start, end) in subtract((live_lo, live_hi), &blocked) {
                if end - start >= MIN_SEGMENT {
                    out.push(Segment { side, zone: *d, start, end, live_start: start, live_end: end });
                }
            }
        }
    }
    out
}

/// `range` less every range in `cut`, as the pieces left in order.
fn subtract(range: (i32, i32), cut: &[(i32, i32)]) -> Vec<(i32, i32)> {
    let mut pieces = vec![range];
    for &(a, b) in cut {
        pieces = pieces
            .into_iter()
            .flat_map(|(s, e)| {
                if b <= s || a >= e {
                    vec![(s, e)]
                } else {
                    [(s, a.max(s)), (b.min(e), e)].into_iter().filter(|(s, e)| e > s).collect()
                }
            })
            .collect();
    }
    pieces
}

/// The pointer while the compositor has handed it to us at a barrier: it is not drawn
/// moving, so where it would be is worked out from the relative motion.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Captured {
    pub seg: Segment,
    /// Position along the edge, in desktop pixels, and where it was captured.
    along: f64,
    start: f64,
    /// How far back into the screen it has come (0 = against the edge).
    depth: f64,
}

impl Captured {
    /// Captured at `cursor` (which the compositor reports just beyond the edge).
    pub fn new(seg: Segment, cursor: (f64, f64)) -> Captured {
        let along = if seg.side.vertical() { cursor.1 } else { cursor.0 };
        Captured { seg, along, start: along, depth: 0.0 }
    }

    /// How far it has slid along the edge since it was captured.
    pub fn slid(&self) -> f64 {
        (self.along - self.start).abs()
    }

    /// Relative motion while captured. Pushing outwards keeps it at the edge.
    pub fn motion(&mut self, dx: f64, dy: f64) {
        let (inward, along) = match self.seg.side {
            Side::Left => (dx, dy),
            Side::Right => (-dx, dy),
            Side::Top => (dy, dx),
            Side::Bottom => (-dy, dx),
        };
        self.depth = (self.depth + inward).max(0.0);
        self.along += along;
    }

    /// Still resting against the edge, in the part that switches.
    pub fn at_edge(&self) -> bool {
        self.depth <= INWARD_SLACK && self.along >= f64::from(self.seg.live_start) && self.along < f64::from(self.seg.live_end)
    }

    /// Where to give the pointer back: inside its display, as far in as it has come.
    pub fn position(&self) -> (f64, f64) {
        let z = self.seg.zone;
        let (lo, hi) = z.span(self.seg.side);
        let along = self.along.clamp(f64::from(lo), f64::from(hi - 1));
        let reach = if self.seg.side.vertical() { z.w } else { z.h };
        let depth = self.depth.min(f64::from(reach - 1).max(0.0));
        let (x, y) = self.seg.edge_point(along);
        match self.seg.side {
            Side::Left => (x + depth, y),
            Side::Right => (x - depth, y),
            Side::Top => (x, y + depth),
            Side::Bottom => (x, y - depth),
        }
    }

    /// The position along the edge (for the countdown, which is drawn there).
    pub fn edge_position(&self) -> (f64, f64) {
        let (lo, hi) = self.seg.zone.span(self.seg.side);
        self.seg.edge_point(self.along.clamp(f64::from(lo), f64::from(hi - 1)))
    }
}

/// What the wait at an edge depends on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DwellConfig {
    /// The channel of each side (0 = none), in `SIDES` order.
    pub channels: [u8; 4],
    pub dwell: Duration,
    pub require_control: bool,
    /// Pulses while waiting.
    pub pulses: bool,
}

/// What the engine says right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Situation {
    /// The master mouse is on this computer.
    pub mouse_here: bool,
    /// This computer's channel.
    pub here: Option<u8>,
}

/// What a step of the wait asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Effect {
    /// Show the countdown on `side`, this far into the wait.
    CountdownBegin { side: Side, elapsed: Duration },
    CountdownEnd,
    /// One pulse on the mouse, at `progress` (0..1) of the wait.
    Pulse { progress: f64 },
    /// Move everything to `channel`.
    Switch { side: Side, channel: u8 },
}

/// The wait at an edge, fed once per poll with the side the pointer rests on (if any).
#[derive(Debug, Default)]
pub struct Dwell {
    resting: Option<(Side, Instant)>,
    /// The pointer has left the edges since the last decision, so a new rest counts.
    armed: bool,
    last_switch: Option<Instant>,
    last_tick: Option<Instant>,
    counting_down: bool,
}

impl Dwell {
    pub fn new() -> Dwell {
        Dwell { armed: true, ..Dwell::default() }
    }

    fn end_countdown(&mut self, out: &mut Vec<Effect>) {
        if std::mem::take(&mut self.counting_down) {
            out.push(Effect::CountdownEnd);
        }
    }

    pub fn counting_down(&self) -> bool {
        self.counting_down
    }

    fn cooling(&self, now: Instant) -> bool {
        self.last_switch.is_some_and(|t| now.saturating_duration_since(t) < COOLDOWN)
    }

    /// Whether resting on `side` now would lead to a switch (the channel is another
    /// computer's, the master mouse is here, no switch just happened).
    pub fn would_switch(&self, side: Side, cfg: &DwellConfig, sit: &Situation, now: Instant) -> bool {
        let channel = cfg.channels[side.index()];
        channel != 0 && sit.mouse_here && sit.here != Some(channel) && !self.cooling(now)
    }

    /// Edge switching is off: nothing waits.
    pub fn idle(&mut self, out: &mut Vec<Effect>) {
        self.resting = None;
        self.end_countdown(out);
    }

    /// One poll. `at` is the side the pointer rests against (None when it is not at an
    /// outer edge, or in a corner); `control` whether Ctrl is held; `gap` the time
    /// between pulses at a point of the wait.
    #[allow(clippy::too_many_arguments)]
    pub fn step(
        &mut self,
        at: Option<Side>,
        control: bool,
        cfg: &DwellConfig,
        sit: &Situation,
        now: Instant,
        gap: &dyn Fn(f64) -> Duration,
        out: &mut Vec<Effect>,
    ) {
        let Some(side) = at else {
            self.resting = None;
            self.armed = true;
            self.last_tick = None;
            self.end_countdown(out);
            return;
        };
        // With "hold Control" the wait starts only with the key down; letting go calls
        // it off, and pressing again at the same edge starts over.
        if cfg.require_control && !control {
            self.resting = None;
            self.last_tick = None;
            self.end_countdown(out);
            return;
        }
        let since = match self.resting {
            Some((s, t)) if s == side => t,
            _ => {
                self.resting = Some((side, now));
                self.last_tick = None;
                self.end_countdown(out);
                return;
            }
        };
        let waited = now.saturating_duration_since(since);
        let channel = cfg.channels[side.index()];
        let cooling = self.cooling(now);
        let will_switch = self.armed && channel != 0 && sit.mouse_here && sit.here != Some(channel) && !cooling;
        if will_switch {
            if !self.counting_down {
                out.push(Effect::CountdownBegin { side, elapsed: waited });
                self.counting_down = true;
            }
            if cfg.pulses {
                let progress = waited.as_secs_f64() / cfg.dwell.as_secs_f64().max(0.001);
                if self.last_tick.is_none_or(|t| now.saturating_duration_since(t) >= gap(progress)) {
                    self.last_tick = Some(now);
                    out.push(Effect::Pulse { progress });
                }
            }
        } else {
            self.end_countdown(out);
        }
        if !self.armed || waited < cfg.dwell {
            return;
        }
        // One decision per visit to the edge.
        self.armed = false;
        self.end_countdown(out);
        if channel == 0 || !sit.mouse_here || sit.here == Some(channel) || cooling {
            return;
        }
        self.last_switch = Some(now);
        out.push(Effect::Switch { side, channel });
    }
}

/// Ctrl as seen through captured key events.
#[derive(Debug, Default, Clone, Copy)]
pub struct CtrlKeys {
    left: bool,
    right: bool,
    /// From the compositor's modifier state, when it sends one.
    modifiers: Option<bool>,
}

/// Evdev codes.
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_RIGHTCTRL: u32 = 97;
const MODIFIER_KEYS: [u32; 9] = [29, 97, 42, 54, 56, 100, 125, 126, 58];
/// The Control bit of an xkb modifier mask (standard keymaps: Shift, Lock, Control).
const XKB_CONTROL: u32 = 1 << 2;

/// Whether an evdev key is a modifier (Ctrl, Shift, Alt, Super, Caps Lock): pressing one
/// is not typing.
pub fn is_modifier(code: u32) -> bool {
    MODIFIER_KEYS.contains(&code)
}

impl CtrlKeys {
    pub fn key(&mut self, code: u32, down: bool) {
        match code {
            KEY_LEFTCTRL => self.left = down,
            KEY_RIGHTCTRL => self.right = down,
            _ => return,
        }
        self.modifiers = None;
    }

    pub fn modifiers(&mut self, depressed: u32) {
        self.modifiers = Some(depressed & XKB_CONTROL != 0);
    }

    pub fn held(&self) -> bool {
        self.modifiers.unwrap_or(false) || self.left || self.right
    }

    /// A new capture: what was seen during the last one no longer holds.
    pub fn forget(&mut self) {
        *self = CtrlKeys::default();
    }
}

/// How XWayland's screen coordinates relate to the portal's zones: the factor by which
/// they are larger (1 unless XWayland renders at the monitors' native scale), when the
/// two describe the same layout; None when they do not line up.
pub fn xwayland_scale(zones: &[Zone], screens: &[Zone]) -> Option<f64> {
    if zones.is_empty() || zones.len() != screens.len() {
        return None;
    }
    let sorted = |list: &[Zone]| {
        let mut v = list.to_vec();
        v.sort_by_key(|z| (z.x, z.y));
        v
    };
    let (zones, screens) = (sorted(zones), sorted(screens));
    let k = f64::from(screens[0].w) / f64::from(zones[0].w.max(1));
    let close = |a: i32, b: i32| (f64::from(a) * k - f64::from(b)).abs() <= 1.0;
    let all = zones.iter().zip(&screens).all(|(z, s)| close(z.x, s.x) && close(z.y, s.y) && close(z.w, s.w) && close(z.h, s.h));
    (all && (0.5..=4.0).contains(&k)).then_some(k)
}

/// Which way edge switching watches the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// Xlib: read the cursor (an X11 session).
    X11,
    /// The XDG desktop portal's InputCapture: pointer barriers (GNOME).
    InputCapture,
    /// wlr-layer-shell strips along the edges (KDE, sway, Hyprland, other wlroots).
    LayerShell,
    None(Reason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Neither WAYLAND_DISPLAY nor DISPLAY.
    NoDisplay,
    /// A Wayland session whose compositor could not be reached.
    NoWayland,
    /// GNOME older than 45 (no input capture).
    GnomeTooOld(u32),
    /// Neither layer shell nor the InputCapture portal.
    NoSupport,
    /// Asked for with FLOCKSWITCH_EDGE_BACKEND, but not available.
    Forced(String),
}

impl Choice {
    pub fn name(&self) -> &'static str {
        match self {
            Choice::X11 => "x11",
            Choice::InputCapture => "input-capture",
            Choice::LayerShell => "layer-shell",
            Choice::None(_) => "none",
        }
    }
}

impl Reason {
    /// The code written to the status file for the settings window.
    pub fn code(&self) -> String {
        match self {
            Reason::NoDisplay => "no-display".into(),
            Reason::NoWayland => "no-wayland".into(),
            Reason::GnomeTooOld(v) => format!("gnome-old:{v}"),
            Reason::NoSupport => "no-support".into(),
            Reason::Forced(name) => format!("forced:{name}"),
        }
    }

    /// For the log.
    pub fn explain(&self) -> String {
        match self {
            Reason::NoDisplay => "no display (neither WAYLAND_DISPLAY nor DISPLAY is set)".into(),
            Reason::NoWayland => "cannot connect to the Wayland compositor".into(),
            Reason::GnomeTooOld(v) => format!("GNOME {v} has no input capture (GNOME 45 or newer has); an X11 session would work"),
            Reason::NoSupport => {
                "the compositor has no wlr-layer-shell and the desktop portal has no InputCapture; an X11 session, GNOME 45+ or a layer-shell desktop (KDE, sway, Hyprland) would work".into()
            }
            Reason::Forced(name) => format!("FLOCKSWITCH_EDGE_BACKEND={name} is not available here"),
        }
    }
}

/// What is known about this session when choosing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    /// XDG_SESSION_TYPE=wayland or WAYLAND_DISPLAY set.
    pub wayland: bool,
    /// DISPLAY set.
    pub x11_display: bool,
    /// FLOCKSWITCH_EDGE_BACKEND, lower case.
    pub forced: Option<String>,
    /// None: the compositor could not be reached (or not asked); Some(has layer shell).
    pub layer_shell: Option<bool>,
    /// The InputCapture portal's version, when it is there and can capture the pointer.
    pub input_capture: Option<u32>,
    /// The running GNOME Shell's major version, if it is GNOME.
    pub gnome: Option<u32>,
}

/// Picks the backend: X11 where the cursor can be read; on Wayland a layer-shell strip
/// first (no permission, no capture, the corners left out exactly), then the portal's
/// input capture (GNOME, which has no layer shell).
pub fn choose(p: &Probe) -> Choice {
    if let Some(name) = p.forced.as_deref() {
        let possible = match name {
            // edge.rs reads the cursor only in an X11 session (XWayland sees it only
            // over X11 windows).
            "x11" => !p.wayland && p.x11_display,
            "layer-shell" => p.layer_shell == Some(true),
            "input-capture" => p.input_capture.is_some(),
            "none" => return Choice::None(Reason::Forced(name.into())),
            _ => false,
        };
        return match name {
            "x11" if possible => Choice::X11,
            "layer-shell" if possible => Choice::LayerShell,
            "input-capture" if possible => Choice::InputCapture,
            _ => Choice::None(Reason::Forced(name.into())),
        };
    }
    if !p.wayland {
        return if p.x11_display { Choice::X11 } else { Choice::None(Reason::NoDisplay) };
    }
    if p.layer_shell == Some(true) {
        return Choice::LayerShell;
    }
    if p.input_capture.is_some() {
        return Choice::InputCapture;
    }
    if p.layer_shell.is_none() && p.gnome.is_none() {
        return Choice::None(Reason::NoWayland);
    }
    match p.gnome {
        Some(v) if v < 45 => Choice::None(Reason::GnomeTooOld(v)),
        _ => Choice::None(Reason::NoSupport),
    }
}

/// Whether changing the barriers of an enabled session works (Disable, set, Enable).
/// GNOME before 49 drops the captured devices on Disable and never makes new ones, so
/// there the session is made again instead.
pub fn can_reconfigure(gnome: Option<u32>, portal_is_gnome: bool) -> bool {
    !portal_is_gnome || gnome.is_some_and(|v| v >= 49)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn z(x: i32, y: i32, w: i32, h: i32) -> Zone {
        Zone { x, y, w, h }
    }

    const ALL: [bool; 4] = [true; 4];

    #[test]
    fn one_display_gets_four_barriers_without_corners() {
        let segs = outer_segments(&[z(0, 0, 1920, 1080)], ALL, 80, false);
        assert_eq!(segs.len(), 4);
        let left = segs.iter().find(|s| s.side == Side::Left).unwrap();
        assert_eq!(left.barrier(), (0, 80, 0, 999));
        let right = segs.iter().find(|s| s.side == Side::Right).unwrap();
        assert_eq!(right.barrier(), (1920, 80, 1920, 999));
        let top = segs.iter().find(|s| s.side == Side::Top).unwrap();
        assert_eq!(top.barrier(), (80, 0, 1839, 0));
        let bottom = segs.iter().find(|s| s.side == Side::Bottom).unwrap();
        assert_eq!(bottom.barrier(), (80, 1080, 1839, 1080));
        assert_eq!(bottom.offset_and_length(), (80, 1760));
    }

    #[test]
    fn only_sides_with_a_channel() {
        let segs = outer_segments(&[z(0, 0, 1920, 1080)], [false, true, false, false], 80, false);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].side, Side::Right);
    }

    #[test]
    fn edges_between_displays_get_none() {
        // As in edge.rs: 1920x1080 with a 2560x1440 to its right, a little higher.
        let zones = [z(0, 0, 1920, 1080), z(1920, -200, 2560, 1440)];
        let segs = outer_segments(&zones, ALL, 80, false);
        // The first display's right edge continues onto the second all the way.
        assert!(!segs.iter().any(|s| s.side == Side::Right && s.zone == zones[0]));
        // The second's left edge: only where the first is not beside it (above y=0 and
        // below y=1080), less the corner margins of the second display.
        let left2: Vec<(i32, i32)> = segs.iter().filter(|s| s.side == Side::Left && s.zone == zones[1]).map(|s| (s.start, s.end)).collect();
        assert_eq!(left2, vec![(-120, 0), (1080, 1160)]);
        assert!(segs.iter().any(|s| s.side == Side::Left && s.zone == zones[0]));
        assert!(segs.iter().any(|s| s.side == Side::Right && s.zone == zones[1]));
        assert_eq!(segs.iter().filter(|s| s.side == Side::Top).count(), 2);
    }

    #[test]
    fn stacked_displays_share_no_barrier() {
        let zones = [z(0, 0, 1920, 1080), z(0, 1080, 1920, 1080)];
        let segs = outer_segments(&zones, ALL, 80, false);
        assert!(!segs.iter().any(|s| s.side == Side::Bottom && s.zone == zones[0]));
        assert!(!segs.iter().any(|s| s.side == Side::Top && s.zone == zones[1]));
        assert_eq!(segs.len(), 6);
    }

    #[test]
    fn short_leftovers_are_dropped() {
        // Offset by 10 pixels: the uncovered bits of the joined edges are too short.
        let zones = [z(0, 0, 1920, 1080), z(1920, 10, 1920, 1080)];
        let segs = outer_segments(&zones, ALL, 0, false);
        assert!(segs.iter().filter(|s| s.side == Side::Right && s.zone == zones[0]).all(|s| s.end - s.start >= MIN_SEGMENT));
        assert!(!segs.iter().any(|s| s.side == Side::Right && s.zone == zones[0]));
    }

    #[test]
    fn whole_edges_for_portals_that_insist() {
        let zones = [z(0, 0, 1920, 1080), z(1920, 500, 1920, 1080)];
        let segs = outer_segments(&zones, ALL, 80, true);
        let left = segs.iter().find(|s| s.side == Side::Left && s.zone == zones[0]).unwrap();
        assert_eq!(left.barrier(), (0, 0, 0, 1079));
        assert_eq!((left.live_start, left.live_end), (80, 1000));
        // Partly beside the other display: no whole-edge barrier is possible there.
        assert!(!segs.iter().any(|s| s.side == Side::Right && s.zone == zones[0]));
        assert!(!segs.iter().any(|s| s.side == Side::Left && s.zone == zones[1]));
    }

    #[test]
    fn subtracting_ranges() {
        assert_eq!(subtract((0, 100), &[]), vec![(0, 100)]);
        assert_eq!(subtract((0, 100), &[(20, 30)]), vec![(0, 20), (30, 100)]);
        assert_eq!(subtract((0, 100), &[(-10, 200)]), Vec::<(i32, i32)>::new());
        assert_eq!(subtract((0, 100), &[(90, 200), (-5, 5)]), vec![(5, 90)]);
    }

    fn right_edge() -> Segment {
        outer_segments(&[z(0, 0, 1920, 1080)], [false, true, false, false], 80, false)[0]
    }

    #[test]
    fn captured_pointer_pushing_outwards_stays() {
        let mut c = Captured::new(right_edge(), (1935.0, 500.0));
        assert!(c.at_edge());
        c.motion(30.0, 4.0);
        c.motion(1.0, -2.0);
        assert!(c.at_edge());
        assert_eq!(c.position(), (1919.0, 502.0));
    }

    #[test]
    fn moving_back_in_lets_go() {
        let mut c = Captured::new(right_edge(), (1920.0, 500.0));
        c.motion(-1.5, 0.0);
        assert!(c.at_edge(), "a tremor is not leaving");
        c.motion(5.0, 0.0);
        assert!(c.at_edge(), "pushing out again takes it back to the edge");
        c.motion(-2.5, 0.0);
        assert!(!c.at_edge());
        assert_eq!(c.position(), (1919.0 - 2.5, 500.0));
    }

    #[test]
    fn sliding_into_a_corner_lets_go() {
        let mut c = Captured::new(right_edge(), (1920.0, 100.0));
        c.motion(0.0, -19.0);
        assert!(c.at_edge());
        c.motion(0.0, -2.0);
        assert!(!c.at_edge(), "within 80 px of the corner");
        c.motion(0.0, -500.0);
        assert_eq!(c.position(), (1919.0, 0.0), "given back on the display");
    }

    #[test]
    fn sliding_along_is_measured() {
        let mut c = Captured::new(right_edge(), (1925.0, 500.0));
        c.motion(3.0, 20.0);
        c.motion(3.0, -50.0);
        assert_eq!(c.slid(), 30.0);
        assert!(c.slid() < SLIDE_RELEASE);
        c.motion(0.0, -5.0);
        assert!(c.slid() > SLIDE_RELEASE);
    }

    #[test]
    fn captured_in_a_corner_of_a_whole_edge_barrier() {
        let seg = outer_segments(&[z(0, 0, 1920, 1080)], [true, false, false, false], 80, true)[0];
        assert!(!Captured::new(seg, (-3.0, 20.0)).at_edge());
        assert!(Captured::new(seg, (-3.0, 500.0)).at_edge());
        let mut top = Captured::new(outer_segments(&[z(0, 0, 1920, 1080)], [false, false, true, false], 80, false)[0], (900.0, -7.0));
        top.motion(3.0, 2.5);
        assert!(!top.at_edge());
        assert_eq!(top.position(), (903.0, 2.5));
        assert_eq!(top.edge_position(), (903.0, 0.0));
    }

    fn cfg(dwell_ms: u64) -> DwellConfig {
        DwellConfig { channels: [0, 2, 0, 0], dwell: Duration::from_millis(dwell_ms), require_control: false, pulses: true }
    }

    const HERE: Situation = Situation { mouse_here: true, here: Some(1) };
    const POLL: Duration = Duration::from_millis(25);

    /// Runs the wait with one sample per poll; returns every effect with its poll number.
    fn run(samples: &[(Option<Side>, bool)], cfg: &DwellConfig, sit: &Situation) -> Vec<(usize, Effect)> {
        let mut d = Dwell::new();
        let start = Instant::now();
        let gap = |_: f64| Duration::from_millis(150);
        let mut all = Vec::new();
        for (i, &(at, control)) in samples.iter().enumerate() {
            let mut out = Vec::new();
            d.step(at, control, cfg, sit, start + POLL * i as u32, &gap, &mut out);
            all.extend(out.into_iter().map(|e| (i, e)));
        }
        all
    }

    fn at(side: Option<Side>, polls: usize) -> Vec<(Option<Side>, bool)> {
        vec![(side, false); polls]
    }

    fn switches(effects: &[(usize, Effect)]) -> Vec<(usize, u8)> {
        effects.iter().filter_map(|(i, e)| if let Effect::Switch { channel, .. } = e { Some((*i, *channel)) } else { None }).collect()
    }

    #[test]
    fn resting_for_the_whole_wait_switches_once() {
        let e = run(&at(Some(Side::Right), 100), &cfg(500), &HERE);
        // The first poll starts the rest; the switch comes 500 ms (20 polls) later.
        assert_eq!(switches(&e), vec![(20, 2)]);
        assert!(matches!(e[0], (1, Effect::CountdownBegin { side: Side::Right, .. })));
        assert!(e.iter().any(|(i, e)| *i == 20 && *e == Effect::CountdownEnd));
        // Pulses every 150 ms while waiting: polls 1, 7, 13, 19.
        let pulses: Vec<usize> = e.iter().filter(|(_, e)| matches!(e, Effect::Pulse { .. })).map(|(i, _)| *i).collect();
        assert_eq!(pulses, vec![1, 7, 13, 19]);
    }

    #[test]
    fn leaving_early_calls_it_off() {
        let samples = [at(Some(Side::Right), 10), at(None, 2), at(Some(Side::Right), 10)].concat();
        let e = run(&samples, &cfg(500), &HERE);
        assert!(switches(&e).is_empty());
        assert_eq!(e.iter().filter(|(_, e)| *e == Effect::CountdownEnd).count(), 1);
    }

    #[test]
    fn one_decision_per_visit_and_a_cooldown() {
        // Rest, switch; stay on (no second switch); leave and come back at once (cooling).
        let samples = [at(Some(Side::Right), 40), at(None, 1), at(Some(Side::Right), 40)].concat();
        let e = run(&samples, &cfg(300), &HERE);
        assert_eq!(switches(&e), vec![(12, 2)]);
        // After the cooldown a new visit switches again.
        let samples = [at(Some(Side::Right), 20), at(None, 80), at(Some(Side::Right), 20)].concat();
        assert_eq!(switches(&run(&samples, &cfg(300), &HERE)).len(), 2);
    }

    #[test]
    fn no_switch_to_this_computer_or_without_the_mouse() {
        let own = Situation { mouse_here: true, here: Some(2) };
        let e = run(&at(Some(Side::Right), 60), &cfg(300), &own);
        assert!(e.is_empty(), "{e:?}");
        let away = Situation { mouse_here: false, here: Some(1) };
        assert!(run(&at(Some(Side::Right), 60), &cfg(300), &away).is_empty());
        // A side without a channel.
        assert!(run(&at(Some(Side::Left), 60), &cfg(300), &HERE).is_empty());
    }

    #[test]
    fn only_while_control_is_held() {
        let mut c = cfg(300);
        c.require_control = true;
        assert!(switches(&run(&at(Some(Side::Right), 60), &c, &HERE)).is_empty());
        // Ctrl down after 10 polls: the wait starts then.
        let samples: Vec<(Option<Side>, bool)> = (0..60).map(|i| (Some(Side::Right), i >= 10)).collect();
        assert_eq!(switches(&run(&samples, &c, &HERE)), vec![(22, 2)]);
        // Letting go halfway calls it off; pressing again starts over.
        let samples: Vec<(Option<Side>, bool)> = (0..60).map(|i| (Some(Side::Right), !(8..10).contains(&i))).collect();
        assert_eq!(switches(&run(&samples, &c, &HERE)), vec![(22, 2)]);
    }

    #[test]
    fn would_switch_follows_the_situation() {
        let d = Dwell::new();
        let now = Instant::now();
        assert!(d.would_switch(Side::Right, &cfg(300), &HERE, now));
        assert!(!d.would_switch(Side::Left, &cfg(300), &HERE, now));
        assert!(!d.would_switch(Side::Right, &cfg(300), &Situation { mouse_here: false, here: None }, now));
    }

    #[test]
    fn ctrl_from_keys_and_modifiers() {
        let mut k = CtrlKeys::default();
        assert!(!k.held());
        k.key(KEY_LEFTCTRL, true);
        assert!(k.held());
        k.key(KEY_LEFTCTRL, false);
        assert!(!k.held());
        k.modifiers(XKB_CONTROL | 1);
        assert!(k.held());
        k.key(KEY_RIGHTCTRL, false);
        assert!(!k.held(), "a key event supersedes the modifier state");
        k.modifiers(1);
        assert!(!k.held());
        assert!(is_modifier(29) && is_modifier(42) && !is_modifier(30));
    }

    #[test]
    fn xwayland_coordinates() {
        let zones = [z(0, 0, 1920, 1080), z(1920, 0, 1280, 720)];
        assert_eq!(xwayland_scale(&zones, &[z(1920, 0, 1280, 720), z(0, 0, 1920, 1080)]), Some(1.0));
        assert_eq!(xwayland_scale(&zones, &[z(0, 0, 3840, 2160), z(3840, 0, 2560, 1440)]), Some(2.0));
        assert_eq!(xwayland_scale(&zones, &[z(0, 0, 1920, 1080)]), None);
        assert_eq!(xwayland_scale(&zones, &[z(0, 0, 1920, 1080), z(1920, 100, 1280, 720)]), None);
        assert_eq!(xwayland_scale(&[], &[]), None);
    }

    fn probe() -> Probe {
        Probe { wayland: true, x11_display: true, ..Probe::default() }
    }

    #[test]
    fn choosing_the_backend() {
        assert_eq!(choose(&Probe { wayland: false, x11_display: true, ..Probe::default() }), Choice::X11);
        assert_eq!(choose(&Probe::default()), Choice::None(Reason::NoDisplay));
        // KDE and wlroots: layer shell, even with the portal there.
        assert_eq!(choose(&Probe { layer_shell: Some(true), input_capture: Some(1), ..probe() }), Choice::LayerShell);
        // GNOME: no layer shell, the portal.
        assert_eq!(choose(&Probe { layer_shell: Some(false), input_capture: Some(1), gnome: Some(46), ..probe() }), Choice::InputCapture);
        assert_eq!(choose(&Probe { layer_shell: Some(false), gnome: Some(43), ..probe() }), Choice::None(Reason::GnomeTooOld(43)));
        assert_eq!(choose(&Probe { layer_shell: Some(false), gnome: Some(46), ..probe() }), Choice::None(Reason::NoSupport));
        assert_eq!(choose(&Probe { layer_shell: Some(false), ..probe() }), Choice::None(Reason::NoSupport));
        assert_eq!(choose(&Probe { layer_shell: None, ..probe() }), Choice::None(Reason::NoWayland));
        // XWayland's DISPLAY does not make a Wayland session an X11 one.
        assert_ne!(choose(&Probe { layer_shell: Some(false), ..probe() }), Choice::X11);
    }

    #[test]
    fn forcing_the_backend() {
        let both = Probe { layer_shell: Some(true), input_capture: Some(2), ..probe() };
        assert_eq!(choose(&Probe { forced: Some("input-capture".into()), ..both.clone() }), Choice::InputCapture);
        assert_eq!(choose(&Probe { forced: Some("x11".into()), ..both.clone() }), Choice::None(Reason::Forced("x11".into())));
        assert_eq!(choose(&Probe { forced: Some("x11".into()), wayland: false, ..both.clone() }), Choice::X11);
        assert_eq!(choose(&Probe { forced: Some("none".into()), ..both.clone() }), Choice::None(Reason::Forced("none".into())));
        let gnome = Probe { layer_shell: Some(false), input_capture: Some(1), ..probe() };
        assert_eq!(choose(&Probe { forced: Some("layer-shell".into()), ..gnome }), Choice::None(Reason::Forced("layer-shell".into())));
        assert_eq!(choose(&Probe { forced: Some("bogus".into()), ..both }), Choice::None(Reason::Forced("bogus".into())));
    }

    #[test]
    fn reconfiguring_gnome() {
        assert!(!can_reconfigure(Some(46), true));
        assert!(!can_reconfigure(None, true));
        assert!(can_reconfigure(Some(49), true));
        assert!(can_reconfigure(None, false));
    }

    #[test]
    fn reason_codes() {
        assert_eq!(Reason::GnomeTooOld(43).code(), "gnome-old:43");
        assert_eq!(Choice::LayerShell.name(), "layer-shell");
    }
}
