//! Switching at the screen edge, as Logi Flow does but without a network: when the
//! cursor rests against an outer edge of the desktop for a moment, the group and the
//! mouse go to the channel set for that edge. Each computer watches only its own
//! screen, so nothing is sent between computers; the one the mouse arrives at puts
//! its cursor at the edge the mouse left by, as if it had crossed over.
//!
//! The shared part (edge geometry, the watcher) is the same in every port; the
//! `platform` module at the end reads the cursor and the displays.

use crate::config::{Edge, Edges, EDGES};
use crate::device::waveform;
use crate::overlay;
use crate::shared::Shared;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the cursor is read. Reading it is cheap; 25 ms is finer than anyone moves.
const POLL: Duration = Duration::from_millis(25);
/// How often the engine's status (this computer's channel, is the mouse here) is read.
const STATUS_POLL: Duration = Duration::from_millis(300);
/// How often the display arrangement is re-read.
const DISPLAYS_POLL: Duration = Duration::from_secs(2);
/// After one switch, the next needs the cursor to leave the edge and this much time.
const COOLDOWN: Duration = Duration::from_secs(2);
/// How far in from the edge the cursor is put when the mouse arrives.
const INSET: f64 = 24.0;
/// While the cursor waits at an edge, the mouse pulses this often (if it has a motor).
pub(crate) const HAPTIC_TICK: Duration = Duration::from_millis(150);
/// A pulse takes a moment to play; Change Host right after would cut it short.
const FINAL_PULSE: Duration = Duration::from_millis(150);
/// A shake is this many turns within this long, each after this many points of travel.
const SHAKE_TURNS: usize = 4;
const SHAKE_WINDOW: Duration = Duration::from_millis(900);
const SHAKE_MIN_RUN: f64 = 80.0;
/// After a shake is noticed, the next one counts only after this long.
const SHAKE_REST: Duration = Duration::from_secs(2);

/// A display, in the global desktop coordinates the platform uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// The outer edge the cursor touches, if any: an edge of the display it is on beyond
/// which no other display continues (between two displays the cursor just crosses
/// over). The last `margin` pixels at each end do not count: hot corners, the window
/// buttons and the menu bar's corners live there.
pub fn edge_at(displays: &[Rect], x: f64, y: f64, margin: f64) -> Option<Edge> {
    let d = displays.iter().find(|d| d.contains(x, y))?;
    let beyond = |px: f64, py: f64| displays.iter().any(|o| o.contains(px, py));
    let along_y = y >= d.y + margin && y < d.y + d.h - margin;
    let along_x = x >= d.x + margin && x < d.x + d.w - margin;
    if along_y && x <= d.x && !beyond(d.x - 1.0, y) {
        return Some(Edge::Left);
    }
    if along_y && x >= d.x + d.w - 1.0 && !beyond(d.x + d.w, y) {
        return Some(Edge::Right);
    }
    if along_x && y <= d.y && !beyond(x, d.y - 1.0) {
        return Some(Edge::Top);
    }
    if along_x && y >= d.y + d.h - 1.0 && !beyond(x, d.y + d.h) {
        return Some(Edge::Bottom);
    }
    None
}

/// Where the cursor goes when the mouse arrives by `edge`: the middle of that edge
/// of the display that reaches furthest that way, a little way in.
pub fn landing(displays: &[Rect], edge: Edge) -> Option<(f64, f64)> {
    let d = match edge {
        Edge::Left => displays.iter().min_by(|a, b| a.x.total_cmp(&b.x)),
        Edge::Right => displays.iter().max_by(|a, b| (a.x + a.w).total_cmp(&(b.x + b.w))),
        Edge::Top => displays.iter().min_by(|a, b| a.y.total_cmp(&b.y)),
        Edge::Bottom => displays.iter().max_by(|a, b| (a.y + a.h).total_cmp(&(b.y + b.h))),
    }?;
    Some(match edge {
        Edge::Left => (d.x + INSET, d.y + d.h / 2.0),
        Edge::Right => (d.x + d.w - INSET, d.y + d.h / 2.0),
        Edge::Top => (d.x + d.w / 2.0, d.y + INSET),
        Edge::Bottom => (d.x + d.w / 2.0, d.y + d.h - INSET),
    })
}

/// The channel a "last switch" status text ends with ("1 → 2", "→ 2").
fn last_channel(text: &str) -> Option<u8> {
    text.rsplit(' ').next()?.parse().ok()
}

/// How the pulses go at `progress` (0..1) of the wait at an edge: the gap to the next
/// one and its waveform, quicker and sharper as the switch nears.
pub(crate) fn rising_pulse(progress: f64) -> (Duration, u8) {
    if progress < 0.40 {
        (Duration::from_millis(180), waveform::SUBTLE_COLLISION)
    } else if progress < 0.75 {
        (Duration::from_millis(110), waveform::DAMP_COLLISION)
    } else {
        (Duration::from_millis(60), waveform::SHARP_COLLISION)
    }
}

/// Notices the cursor being shaken from side to side.
#[derive(Default)]
struct Shake {
    last_x: Option<f64>,
    /// Which way the cursor is going (+1 or -1), and how far it has come that way.
    heading: f64,
    run: f64,
    turns: VecDeque<Instant>,
    noticed: Option<Instant>,
}

impl Shake {
    /// Takes the cursor's x position; true at the moment a shake is noticed.
    fn update(&mut self, x: f64, now: Instant) -> bool {
        let Some(last) = self.last_x.replace(x) else { return false };
        let dx = x - last;
        if dx == 0.0 {
            return false;
        }
        if dx.signum() == self.heading {
            self.run += dx.abs();
        } else {
            if self.run >= SHAKE_MIN_RUN {
                self.turns.push_back(now);
            }
            self.heading = dx.signum();
            self.run = dx.abs();
        }
        self.turns.retain(|t| now.duration_since(*t) <= SHAKE_WINDOW);
        if self.turns.len() < SHAKE_TURNS || self.noticed.is_some_and(|t| now.duration_since(t) < SHAKE_REST) {
            return false;
        }
        self.noticed = Some(now);
        self.turns.clear();
        true
    }
}

/// Plays the countdown once, at an edge of the display the cursor is on, for the
/// settings window. At least a second, so a short wait can still be judged.
pub fn preview(edges: &Edges) {
    let displays = platform::displays();
    let cursor = platform::cursor();
    let on_cursor = cursor.and_then(|(x, y)| displays.iter().find(|d| d.contains(x, y)));
    let Some(display) = on_cursor.or(displays.first()) else { return };
    let edge = EDGES.into_iter().find(|&e| edges.channel(e) != 0).unwrap_or(Edge::Right);
    let duration = Duration::from_millis(edges.dwell_ms.max(1000));
    overlay::edge_begin(edges, edge, display, Duration::ZERO, duration, true);
}

/// Takes the countdown away, if one is showing.
fn end_countdown(showing: &mut bool) {
    if std::mem::take(showing) {
        overlay::edge_end();
    }
}

/// Whether this computer can watch its screen edges at all.
pub fn available() -> bool {
    platform::available()
}

/// Starts the watcher on its own thread; it runs until the shared stop flag is set.
pub fn spawn(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("edges".into())
        .spawn(move || watch(shared))
        .expect("spawning the edge watcher");
}

fn watch(shared: Arc<Shared>) {
    if !available() {
        log::info!("edge switching: not available on this computer");
        return;
    }
    let mut cfg = shared.config();
    let mut version = shared.version();
    let mut announced = false;
    let mut status_read: Option<Instant> = None;
    // From the engine's status: this computer's channel, whether the master mouse is
    // here, and the "last switch" text as last seen.
    let (mut here, mut mouse_here, mut last_switch_text): (Option<u8>, bool, Option<String>) = (None, false, None);
    // The mouse was sent away from here by this channel; when it comes back the cursor
    // goes to that channel's edge. Used once.
    let mut landing_channel: Option<u8> = None;
    let mut displays: Vec<Rect> = Vec::new();
    let mut displays_read: Option<Instant> = None;
    // The edge the cursor has rested on, and since when.
    let mut resting: Option<(Edge, Instant)> = None;
    // The cursor has left the edges since the last decision, so a new rest counts.
    let mut armed = true;
    let mut last_switch: Option<Instant> = None;
    let mut last_tick: Option<Instant> = None;
    // The countdown is on the screen.
    let mut counting_down = false;
    let mut shake = Shake::default();

    while !shared.stopped() {
        std::thread::sleep(POLL);
        if shared.version() != version {
            version = shared.version();
            cfg = shared.config();
            announced = false;
        }
        let active = cfg.enabled && cfg.edges.on && cfg.edges.any();
        if !announced {
            announced = true;
            if active {
                let list: Vec<String> = crate::config::EDGES
                    .iter()
                    .filter(|&&e| cfg.edges.channel(e) != 0)
                    .map(|&e| format!("{} → {}", e.name(), cfg.edges.channel(e)))
                    .collect();
                log::info!("edges: {} after {} ms", list.join(", "), cfg.edges.dwell_ms);
            } else if cfg.edges.on {
                log::info!("edges: on, but no edge has a channel");
            }
        }
        if status_read.is_none_or(|t| t.elapsed() >= STATUS_POLL) {
            let first = status_read.is_none();
            status_read = Some(Instant::now());
            let st = shared.status();
            here = st.channel;
            if st.last_switch != last_switch_text {
                last_switch_text = st.last_switch.clone();
                landing_channel = st.last_switch.as_deref().and_then(last_channel);
            }
            let now_here = st.trigger.is_some();
            let arrived = now_here && !mouse_here && !first;
            if arrived && active && cfg.edges.place_cursor {
                // The mouse has come back from the channel it was sent to: put the
                // cursor at that channel's edge, as if it had come in from there.
                if let Some(edge) = landing_channel.take().and_then(|c| cfg.edges.edge_for(c)) {
                    if displays.is_empty() {
                        displays = platform::displays();
                    }
                    if let Some((x, y)) = landing(&displays, edge) {
                        log::info!("mouse is back: cursor to the {} edge", edge.name());
                        platform::place(x, y);
                        resting = None;
                        armed = false;
                    }
                }
            }
            if arrived && cfg.locate.on && cfg.locate.on_arrival {
                overlay::locate(&cfg.locate);
            }
            mouse_here = now_here;
        }
        if shared.take_locate_request() && cfg.locate.on {
            overlay::locate(&cfg.locate);
        }
        if cfg.locate.on && cfg.locate.on_shake {
            if platform::cursor().is_some_and(|(x, _)| shake.update(x, Instant::now())) {
                overlay::locate(&cfg.locate);
            }
        }
        if !active {
            resting = None;
            end_countdown(&mut counting_down);
            continue;
        }
        if displays_read.is_none_or(|t| t.elapsed() >= DISPLAYS_POLL) {
            displays_read = Some(Instant::now());
            displays = platform::displays();
        }
        let Some((x, y)) = platform::cursor() else { continue };
        let Some(edge) = edge_at(&displays, x, y, f64::from(cfg.edges.corner_margin)) else {
            resting = None;
            armed = true;
            last_tick = None;
            end_countdown(&mut counting_down);
            continue;
        };
        // With "hold Control" the wait starts only with the key down; letting go calls
        // it off, and pressing again at the same edge starts over.
        if cfg.edges.require_control && !platform::control_down() {
            resting = None;
            last_tick = None;
            end_countdown(&mut counting_down);
            continue;
        }
        let since = match resting {
            Some((e, t)) if e == edge => t,
            _ => {
                resting = Some((edge, Instant::now()));
                last_tick = None;
                end_countdown(&mut counting_down);
                continue;
            }
        };
        let channel = cfg.edges.channel(edge);
        let dwell = Duration::from_millis(cfg.edges.dwell_ms);
        let cooling = last_switch.is_some_and(|t| t.elapsed() < COOLDOWN);
        let will_switch = armed && channel != 0 && mouse_here && here != Some(channel) && !cooling;
        if will_switch {
            if !counting_down {
                if let Some(display) = displays.iter().find(|d| d.contains(x, y)) {
                    overlay::edge_begin(&cfg.edges, edge, display, since.elapsed(), dwell, false);
                    counting_down = true;
                }
            }
            // Waiting at an edge that will switch: the mouse pulses so the hand knows.
            if cfg.haptics.on_edge {
                let progress = since.elapsed().as_secs_f64() / dwell.as_secs_f64();
                let (gap, wave) =
                    if cfg.haptics.progressive { rising_pulse(progress) } else { (HAPTIC_TICK, cfg.haptics.edge_waveform) };
                if last_tick.is_none_or(|t| t.elapsed() >= gap) {
                    last_tick = Some(Instant::now());
                    shared.request_haptic(wave);
                }
            }
        } else {
            end_countdown(&mut counting_down);
        }
        if !armed || since.elapsed() < dwell {
            continue;
        }
        // One decision per visit to the edge.
        armed = false;
        end_countdown(&mut counting_down);
        if channel == 0 {
            continue;
        }
        if !mouse_here {
            log::debug!("{} edge: the master mouse is not here", edge.name());
            continue;
        }
        if here == Some(channel) {
            log::debug!("{} edge: channel {channel} is this computer", edge.name());
            continue;
        }
        if cooling {
            continue;
        }
        log::info!("{} edge: channel {channel}", edge.name());
        last_switch = Some(Instant::now());
        // The rising pulses end in one last pulse. With pulses on switching, the
        // engine's own switch pulse is that one.
        if cfg.haptics.on_edge && cfg.haptics.progressive && !cfg.haptics.on_switch {
            shared.request_haptic(waveform::COMPLETED);
            std::thread::sleep(FINAL_PULSE);
        }
        shared.request_switch(channel);
    }
    end_countdown(&mut counting_down);
}

/// The cursor and the displays, from the X server (root-window coordinates, y down),
/// through Xlib and Xinerama loaded at run time. Wayland lets no app read the cursor
/// position, so there this is not available (XWayland sees it only over X11 windows).
mod platform {
    use super::Rect;
    use std::sync::OnceLock;
    use x11_dl::xinerama::Xlib as Xinerama;
    use x11_dl::xlib::{ControlMask, Display, Xlib};

    struct X {
        xlib: Xlib,
        xinerama: Option<Xinerama>,
        display: *mut Display,
        root: u64,
    }
    // Safety: only one thread per process makes X calls on this connection (the watcher
    // in the background process, the window's thread for the preview in the settings one).
    unsafe impl Send for X {}
    unsafe impl Sync for X {}

    static X11: OnceLock<Option<X>> = OnceLock::new();

    fn wayland() -> bool {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        var("XDG_SESSION_TYPE").eq_ignore_ascii_case("wayland") || !var("WAYLAND_DISPLAY").is_empty()
    }

    /// An X11 session: cursor and displays can be read.
    pub fn available() -> bool {
        !wayland() && std::env::var_os("DISPLAY").is_some()
    }

    fn x() -> Option<&'static X> {
        X11.get_or_init(|| {
            if !available() {
                return None;
            }
            let xlib = Xlib::open().ok()?;
            let display = unsafe { (xlib.XOpenDisplay)(std::ptr::null()) };
            if display.is_null() {
                log::warn!("edge switching: cannot open the X display");
                return None;
            }
            let root = unsafe { (xlib.XDefaultRootWindow)(display) };
            Some(X { xlib, xinerama: Xinerama::open().ok(), display, root })
        })
        .as_ref()
    }

    /// The cursor position and the modifier keys, as of now.
    fn pointer() -> Option<(f64, f64, u32)> {
        let x = x()?;
        let (mut root, mut child) = (0u64, 0u64);
        let (mut rx, mut ry, mut wx, mut wy) = (0i32, 0i32, 0i32, 0i32);
        let mut mask = 0u32;
        let found = unsafe {
            (x.xlib.XQueryPointer)(x.display, x.root, &mut root, &mut child, &mut rx, &mut ry, &mut wx, &mut wy, &mut mask)
        };
        (found != 0).then(|| (f64::from(rx), f64::from(ry), mask))
    }

    /// Where the cursor is right now.
    pub fn cursor() -> Option<(f64, f64)> {
        pointer().map(|(x, y, _)| (x, y))
    }

    /// Whether a Control key is held right now.
    pub fn control_down() -> bool {
        pointer().is_some_and(|(_, _, mask)| mask & ControlMask != 0)
    }

    pub fn displays() -> Vec<Rect> {
        let Some(x) = x() else { return Vec::new() };
        if let Some(xin) = &x.xinerama {
            let mut count = 0i32;
            let info = unsafe { (xin.XineramaQueryScreens)(x.display, &mut count) };
            if !info.is_null() {
                let screens = unsafe { std::slice::from_raw_parts(info, count.max(0) as usize) };
                let out: Vec<Rect> = screens
                    .iter()
                    .map(|s| Rect { x: f64::from(s.x_org), y: f64::from(s.y_org), w: f64::from(s.width), h: f64::from(s.height) })
                    .collect();
                unsafe { (x.xlib.XFree)(info.cast()) };
                if !out.is_empty() {
                    return out;
                }
            }
        }
        // No Xinerama: the whole root window as one display.
        let screen = unsafe { (x.xlib.XDefaultScreen)(x.display) };
        let (w, h) = unsafe { ((x.xlib.XDisplayWidth)(x.display, screen), (x.xlib.XDisplayHeight)(x.display, screen)) };
        vec![Rect { x: 0.0, y: 0.0, w: f64::from(w), h: f64::from(h) }]
    }

    /// Moves the cursor there.
    pub fn place(px: f64, py: f64) {
        let Some(x) = x() else { return };
        unsafe {
            (x.xlib.XWarpPointer)(x.display, 0, x.root, 0, 0, 0, 0, px as i32, py as i32);
            (x.xlib.XFlush)(x.display);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn two_side_by_side() -> Vec<Rect> {
        // Main display 1920×1080 with a second one to its right, a little higher.
        vec![Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 }, Rect { x: 1920.0, y: -200.0, w: 2560.0, h: 1440.0 }]
    }

    #[test]
    fn outer_edges_only() {
        let d = two_side_by_side();
        assert_eq!(edge_at(&d, 0.0, 500.0, 80.0), Some(Edge::Left));
        assert_eq!(edge_at(&d, 1919.0, 500.0, 80.0), None, "the second display continues there");
        assert_eq!(edge_at(&d, 1919.0, 1000.0, 80.0), None, "the join runs the whole height of the first display");
        assert_eq!(edge_at(&d, 4479.0, 500.0, 80.0), Some(Edge::Right));
        assert_eq!(edge_at(&d, 960.0, 0.0, 80.0), Some(Edge::Top));
        assert_eq!(edge_at(&d, 960.0, 1079.0, 80.0), Some(Edge::Bottom));
        assert_eq!(edge_at(&d, 3000.0, -200.0, 80.0), Some(Edge::Top));
        assert_eq!(edge_at(&d, 500.0, 500.0, 80.0), None, "the middle of the screen");
    }

    #[test]
    fn corners_do_not_count() {
        let d = two_side_by_side();
        assert_eq!(edge_at(&d, 0.0, 10.0, 80.0), None);
        assert_eq!(edge_at(&d, 0.0, 1075.0, 80.0), None);
        assert_eq!(edge_at(&d, 10.0, 0.0, 80.0), None);
        assert_eq!(edge_at(&d, 0.0, 80.0, 80.0), Some(Edge::Left));
    }

    #[test]
    fn cursor_outside_every_display() {
        assert_eq!(edge_at(&two_side_by_side(), -5.0, 500.0, 80.0), None);
        assert_eq!(edge_at(&[], 0.0, 0.0, 80.0), None);
    }

    #[test]
    fn landing_spots() {
        let d = two_side_by_side();
        assert_eq!(landing(&d, Edge::Left), Some((24.0, 540.0)));
        assert_eq!(landing(&d, Edge::Right), Some((4480.0 - 24.0, 520.0)));
        assert_eq!(landing(&d, Edge::Top), Some((3200.0, -176.0)));
        // The second display reaches lower (-200 + 1440 = 1240), so the bottom edge is its.
        assert_eq!(landing(&d, Edge::Bottom), Some((3200.0, 1216.0)));
        assert_eq!(landing(&[], Edge::Left), None);
    }

    /// Reads this computer's cursor and displays: `cargo test platform -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn platform_reads() {
        let displays = platform::displays();
        let cursor = platform::cursor();
        println!("displays: {displays:?}\ncursor: {cursor:?}");
        assert!(!displays.is_empty());
        assert!(cursor.is_some());
        let (x, y) = cursor.unwrap();
        println!("edge now: {:?}", edge_at(&displays, x, y, 80.0));
    }

    #[test]
    fn pulses_quicken_and_sharpen_towards_the_switch() {
        assert_eq!(rising_pulse(0.0), (Duration::from_millis(180), waveform::SUBTLE_COLLISION));
        assert_eq!(rising_pulse(0.39), (Duration::from_millis(180), waveform::SUBTLE_COLLISION));
        assert_eq!(rising_pulse(0.40), (Duration::from_millis(110), waveform::DAMP_COLLISION));
        assert_eq!(rising_pulse(0.74), (Duration::from_millis(110), waveform::DAMP_COLLISION));
        assert_eq!(rising_pulse(0.75), (Duration::from_millis(60), waveform::SHARP_COLLISION));
        assert_eq!(rising_pulse(1.2), (Duration::from_millis(60), waveform::SHARP_COLLISION));
    }

    /// Feeds x positions, one poll apart; returns at which positions a shake was noticed.
    fn shake_on(xs: &[f64]) -> Vec<usize> {
        let mut s = Shake::default();
        let start = Instant::now();
        xs.iter()
            .enumerate()
            .filter(|(i, x)| s.update(**x, start + POLL * *i as u32))
            .map(|(i, _)| i)
            .collect()
    }

    #[test]
    fn a_quick_shake_is_noticed_once() {
        // Back and forth by 300 points, three polls each way.
        let xs: Vec<f64> = (0..40).map(|i| [0.0, 100.0, 200.0, 300.0, 200.0, 100.0][i % 6]).collect();
        let noticed = shake_on(&xs);
        assert_eq!(noticed.len(), 1, "{noticed:?}");
    }

    #[test]
    fn ordinary_movement_is_not_a_shake() {
        let straight: Vec<f64> = (0..60).map(|i| i as f64 * 40.0).collect();
        assert!(shake_on(&straight).is_empty());
        // Small wobbles never travel far enough to count.
        let wobble: Vec<f64> = (0..60).map(|i| if i % 2 == 0 { 0.0 } else { 20.0 }).collect();
        assert!(shake_on(&wobble).is_empty());
        // Far enough, but so slowly the turns fall out of the window.
        let slow: Vec<f64> = (0..400).map(|i| [0.0, 5.0, 10.0, 15.0][i % 4] + ((i / 40) % 2) as f64 * 200.0).collect();
        assert!(shake_on(&slow).len() <= 1);
    }

    #[test]
    fn last_switch_text() {
        assert_eq!(last_channel("1 → 2"), Some(2));
        assert_eq!(last_channel("→ 3"), Some(3));
        assert_eq!(last_channel(""), None);
    }
}
