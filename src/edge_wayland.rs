//! Choosing how the screen edges are watched, and what the two Wayland watchers share.
//!
//! An X11 session keeps the watcher in edge.rs, unchanged. A Wayland session, where no
//! app may read the cursor, is probed once: a compositor with wlr-layer-shell (KDE,
//! sway, Hyprland, other wlroots desktops) gets 1-pixel strips along the outer edges
//! (edge_layer.rs); one without it (GNOME) gets pointer barriers from the desktop
//! portal's InputCapture (edge_portal.rs). The choice is logged ("edge backend: …") and
//! written to the status file so the settings window can say what works and what does
//! not. FLOCKSWITCH_EDGE_BACKEND=layer-shell|input-capture|x11|none forces one.

use crate::config::{Config, Edge, Edges, EDGES};
use crate::device::waveform;
use crate::edge::{rising_pulse, Rect, HAPTIC_TICK};
use crate::edge_logic::{self as logic, Choice, DwellConfig, Effect, Probe, Reason, Side, Situation};
use crate::overlay;
use crate::shared::Shared;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// How often the watchers look (the same as edge.rs).
pub const POLL: Duration = Duration::from_millis(25);
const STATUS_POLL: Duration = Duration::from_millis(300);
/// A pulse takes a moment to play; Change Host right after would cut it short.
const FINAL_PULSE: Duration = Duration::from_millis(150);

pub fn session_is_wayland() -> bool {
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    var("XDG_SESSION_TYPE").eq_ignore_ascii_case("wayland") || !var("WAYLAND_DISPLAY").is_empty()
}

fn desktop() -> String {
    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()
}

pub fn side_of(edge: Edge) -> Side {
    match edge {
        Edge::Left => Side::Left,
        Edge::Right => Side::Right,
        Edge::Top => Side::Top,
        Edge::Bottom => Side::Bottom,
    }
}

pub fn edge_of(side: Side) -> Edge {
    EDGES[side.index()]
}

/// Writes the backend and its state for the settings window.
pub fn set_status(shared: &Shared, backend: &str, note: &str) {
    let (backend, note) = (backend.to_string(), note.to_string());
    shared.set_status(|st| {
        st.edge_backend = backend;
        st.edge_note = note;
    });
}

/// Starts watching the edges in whichever way this session allows.
pub fn spawn(shared: Arc<Shared>) {
    let forced = std::env::var("FLOCKSWITCH_EDGE_BACKEND").ok().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
    if !session_is_wayland() && forced.is_none() {
        // An X11 session: the watcher in edge.rs, exactly as before.
        if std::env::var_os("DISPLAY").is_some() {
            log::info!("edge backend: x11 (XQueryPointer)");
            set_status(&shared, "x11", "");
        } else {
            let reason = Reason::NoDisplay;
            log::info!("edge backend: none ({})", reason.explain());
            set_status(&shared, "none", &reason.code());
        }
        crate::edge::spawn(shared);
        return;
    }
    let started = std::thread::Builder::new().name("edges".into()).spawn(move || {
        let mut tries = 0;
        let (probe, choice) = loop {
            let (probe, found) = probe(forced.clone());
            let choice = logic::choose(&probe);
            // A portal or compositor that is still starting (flockswitch started at
            // login) answers a little later: look again for a few minutes.
            let again = matches!(choice, Choice::None(Reason::NoSupport | Reason::NoWayland)) && tries < PROBE_RETRIES;
            if let Choice::None(reason) = &choice {
                log::info!("edge backend: none ({})", reason.explain());
                // What was found, for the settings window: a screenshot of it then says
                // why without the log.
                let code = match reason {
                    Reason::NoSupport | Reason::NoWayland => format!("{}:{found}", reason.code()),
                    _ => reason.code(),
                };
                set_status(&shared, choice.name(), &code);
            }
            if !again {
                break (probe, choice);
            }
            tries += 1;
            log::info!("edge probe: looking again in {} s ({tries}/{PROBE_RETRIES})", PROBE_RETRY.as_secs());
            std::thread::sleep(PROBE_RETRY);
        };
        match &choice {
            Choice::None(_) => {}
            Choice::InputCapture => {
                log::info!(
                    "edge backend: input-capture (xdg-desktop-portal InputCapture v{}{})",
                    probe.input_capture.unwrap_or(1),
                    probe.gnome.map(|v| format!(", GNOME {v}")).unwrap_or_default()
                );
                set_status(&shared, choice.name(), "off");
            }
            Choice::LayerShell => {
                log::info!("edge backend: layer-shell (1-pixel wlr-layer-shell strips along the edges that have a channel)");
                set_status(&shared, choice.name(), "ready");
            }
            Choice::X11 => {
                log::info!("edge backend: x11 (XQueryPointer)");
                set_status(&shared, choice.name(), "");
            }
        }
        match choice {
            Choice::X11 => crate::edge::spawn(shared),
            Choice::LayerShell => crate::edge_layer::run(shared),
            Choice::InputCapture => crate::edge_portal::run(shared, probe.input_capture.unwrap_or(1), probe.gnome),
            Choice::None(_) => {}
        }
    });
    if let Err(e) = started {
        log::warn!("edge switching: cannot start its thread: {e}");
    }
}

/// Runs `fut`, giving up after `limit` (a portal that is slow to start).
pub async fn within<T>(limit: Duration, fut: impl Future<Output = T>) -> Option<T> {
    let fut = std::pin::pin!(fut);
    let timer = std::pin::pin!(async_io::Timer::after(limit));
    match futures_util::future::select(fut, timer).await {
        futures_util::future::Either::Left((value, _)) => Some(value),
        futures_util::future::Either::Right(_) => None,
    }
}

const PROBE_RETRIES: u32 = 10;
const PROBE_RETRY: Duration = Duration::from_secs(30);

/// What this session offers, logged step by step, and a one-line summary of it.
fn probe(forced: Option<String>) -> (Probe, String) {
    let mut found = Vec::new();
    let wayland = session_is_wayland();
    let mut p = Probe { wayland, x11_display: std::env::var_os("DISPLAY").is_some(), forced, ..Probe::default() };
    log::info!(
        "edge probe: session {}, desktop '{}', WAYLAND_DISPLAY {}, DISPLAY {}{}",
        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "?".into()),
        desktop(),
        if std::env::var_os("WAYLAND_DISPLAY").is_some() { "set" } else { "unset" },
        if p.x11_display { "set" } else { "unset" },
        p.forced.as_deref().map(|f| format!(", forced '{f}'")).unwrap_or_default()
    );
    if wayland {
        match crate::wl::Wl::connect(false) {
            Ok(wl) => {
                p.layer_shell = Some(wl.has_layer_shell());
                let outputs: Vec<String> = wl.displays().iter().map(|d| format!("{} {:?} scale {}", d.name, d.rect, d.scale)).collect();
                log::info!(
                    "edge probe: wlr-layer-shell {}, xdg-output {}, outputs [{}]",
                    if wl.has_layer_shell() { "yes" } else { "no" },
                    if wl.has_xdg_output() { "yes" } else { "no" },
                    outputs.join("; ")
                );
            }
            Err(e) => {
                log::info!("edge probe: cannot connect to the Wayland compositor: {e}");
                found.push(format!("Wayland: {e}"));
            }
        }
        if p.layer_shell == Some(false) {
            found.push("wlr-layer-shell: no".into());
        }
    }
    let want_portal = p.layer_shell != Some(true) || p.forced.as_deref() == Some("input-capture");
    let gnome_desktop = desktop().to_ascii_lowercase().contains("gnome");
    async_io::block_on(async {
        if gnome_desktop {
            p.gnome = within(Duration::from_secs(5), gnome_shell_version()).await.flatten();
            if p.gnome.is_none() {
                p.gnome = gnome_shell_version_command();
            }
            log::info!("edge probe: GNOME Shell {}", p.gnome.map(|v| v.to_string()).unwrap_or_else(|| "version unknown".into()));
            found.push(format!("GNOME {}", p.gnome.map(|v| v.to_string()).unwrap_or_else(|| "?".into())));
        }
        if want_portal {
            match within(Duration::from_secs(25), crate::edge_portal::probe()).await {
                Some(Ok((version, caps))) => {
                    log::info!("edge probe: portal InputCapture v{version}, capabilities {caps}");
                    if caps.contains("pointer") {
                        p.input_capture = Some(version);
                    } else {
                        found.push(format!("InputCapture v{version}: {caps}"));
                    }
                }
                Some(Err(e)) => {
                    log::info!("edge probe: portal InputCapture: {e}");
                    found.push(format!("InputCapture: {e}"));
                }
                None => {
                    log::info!("edge probe: portal InputCapture: no answer in 25 s");
                    found.push("InputCapture: no answer".into());
                }
            }
        }
    });
    (p, found.join(" · "))
}

/// `gnome-shell --version` ("GNOME Shell 46.0" → 46), where the D-Bus property is not
/// readable (also used by the setup check).
pub fn gnome_shell_version_command() -> Option<u32> {
    let out = std::process::Command::new("gnome-shell").arg("--version").stderr(std::process::Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let version = text.split_whitespace().find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    version.split('.').next()?.parse().ok()
}

/// The running GNOME Shell's major version ("46.2" → 46).
async fn gnome_shell_version() -> Option<u32> {
    let conn = ashpd::zbus::Connection::session().await.ok()?;
    let proxy = ashpd::zbus::Proxy::new(&conn, "org.gnome.Shell", "/org/gnome/Shell", "org.gnome.Shell").await.ok()?;
    let version: String = proxy.get_property("ShellVersion").await.ok()?;
    version.split('.').next()?.trim().parse().ok()
}

/// Plays the countdown once for the settings window, in a Wayland session: on a
/// layer-shell surface, or through XWayland on GNOME. The cursor position is unknown,
/// so it is drawn in the middle of the edge of the first display.
pub fn preview(edges: &Edges) {
    static TARGET: OnceLock<&'static str> = OnceLock::new();
    let target = *TARGET.get_or_init(|| {
        if crate::wl::Wl::connect(false).is_ok_and(|wl| wl.has_layer_shell()) {
            overlay::use_layer_shell();
            "layer-shell"
        } else if std::env::var_os("DISPLAY").is_some() {
            overlay::use_xwayland();
            "xwayland"
        } else {
            ""
        }
    });
    log::info!("preview: countdown on {}", if target.is_empty() { "nothing (no way to draw here)" } else { target });
    if target.is_empty() {
        return;
    }
    let edge = EDGES.into_iter().find(|&e| edges.channel(e) != 0).unwrap_or(Edge::Right);
    let duration = Duration::from_millis(edges.dwell_ms.max(1000));
    overlay::edge_begin_at(edges, edge, None, None, Duration::ZERO, duration, true);
}

/// The settings and the engine's status as the Wayland watchers follow them, the way
/// the X11 watcher in edge.rs does.
pub struct Watch {
    pub shared: Arc<Shared>,
    pub cfg: Config,
    version: u64,
    announced: bool,
    status_read: Option<Instant>,
    pub sit: Situation,
    last_switch_text: Option<String>,
    landing_channel: Option<u8>,
    told_place: bool,
    told_locate: bool,
}

impl Watch {
    pub fn new(shared: Arc<Shared>) -> Watch {
        let cfg = shared.config();
        let version = shared.version();
        Watch {
            shared,
            cfg,
            version,
            announced: false,
            status_read: None,
            sit: Situation::default(),
            last_switch_text: None,
            landing_channel: None,
            told_place: false,
            told_locate: false,
        }
    }

    pub fn active(&self) -> bool {
        self.cfg.enabled && self.edges_wanted()
    }

    /// Edge switching is set up, whether or not flockswitch is paused from the menu.
    pub fn edges_wanted(&self) -> bool {
        self.cfg.edges.on && self.cfg.edges.any()
    }

    /// Which sides have a channel (and so a barrier or a strip), paused or not.
    pub fn sides(&self) -> [bool; 4] {
        let on = self.edges_wanted();
        let mut sides = [false; 4];
        for edge in EDGES {
            sides[side_of(edge).index()] = on && self.cfg.edges.channel(edge) != 0;
        }
        sides
    }

    pub fn margin(&self) -> i32 {
        i32::try_from(self.cfg.edges.corner_margin).unwrap_or(i32::MAX)
    }

    pub fn dwell_config(&self) -> DwellConfig {
        let mut channels = [0; 4];
        for edge in EDGES {
            channels[side_of(edge).index()] = self.cfg.edges.channel(edge);
        }
        DwellConfig {
            channels,
            dwell: Duration::from_millis(self.cfg.edges.dwell_ms),
            require_control: self.cfg.edges.require_control,
            pulses: self.cfg.haptics.on_edge,
        }
    }

    /// The time between pulses at `progress` of the wait.
    pub fn gap(&self) -> impl Fn(f64) -> Duration {
        let progressive = self.cfg.haptics.progressive;
        move |progress| if progressive { rising_pulse(progress).0 } else { HAPTIC_TICK }
    }

    /// Once per poll: picks up changed settings and, every 300 ms, the engine's status.
    /// True when the settings changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if self.shared.version() != self.version {
            self.version = self.shared.version();
            self.cfg = self.shared.config();
            self.announced = false;
            changed = true;
        }
        let active = self.active();
        if !self.announced {
            self.announced = true;
            if active {
                let list: Vec<String> = EDGES
                    .iter()
                    .filter(|&&e| self.cfg.edges.channel(e) != 0)
                    .map(|&e| format!("{} → {}", e.name(), self.cfg.edges.channel(e)))
                    .collect();
                log::info!("edges: {} after {} ms", list.join(", "), self.cfg.edges.dwell_ms);
            } else if self.cfg.edges.on {
                log::info!("edges: on, but no edge has a channel");
            }
        }
        if self.status_read.is_none_or(|t| t.elapsed() >= STATUS_POLL) {
            let first = self.status_read.is_none();
            self.status_read = Some(Instant::now());
            let st = self.shared.status();
            self.sit.here = st.channel;
            if st.last_switch != self.last_switch_text {
                self.last_switch_text = st.last_switch.clone();
                self.landing_channel = st.last_switch.as_deref().and_then(|t| t.rsplit(' ').next()?.parse().ok());
            }
            let now_here = st.trigger.is_some();
            let arrived = now_here && !self.sit.mouse_here && !first;
            if arrived && active && self.cfg.edges.place_cursor {
                let edge = self.landing_channel.take().and_then(|c| self.cfg.edges.edge_for(c));
                if let Some(edge) = edge.filter(|_| !std::mem::replace(&mut self.told_place, true)) {
                    log::info!("mouse is back: Wayland lets no app move the cursor, so it is not put at the {} edge", edge.name());
                }
            }
            if arrived && self.cfg.locate.on && self.cfg.locate.on_arrival {
                self.tell_locate();
            }
            self.sit.mouse_here = now_here;
        }
        if self.shared.take_locate_request() && self.cfg.locate.on {
            self.tell_locate();
        }
        changed
    }

    fn tell_locate(&mut self) {
        if !std::mem::replace(&mut self.told_locate, true) {
            log::info!("find the cursor: not available on Wayland (no app may read the cursor position)");
        }
    }

    /// Carries out what a step of the wait asked for. `place` gives, for a side, the
    /// display the countdown goes on and the cursor, in the coordinates the overlay
    /// draws in (None: no countdown there). True when it switched.
    pub fn apply(&self, effects: &[Effect], place: impl Fn(Side) -> Option<(Rect, (f64, f64))>) -> bool {
        let mut switched = false;
        let haptics = &self.cfg.haptics;
        for effect in effects {
            match *effect {
                Effect::CountdownBegin { side, elapsed } => {
                    if let Some((display, cursor)) = place(side) {
                        let dwell = Duration::from_millis(self.cfg.edges.dwell_ms);
                        overlay::edge_begin_at(&self.cfg.edges, edge_of(side), Some(&display), Some(cursor), elapsed, dwell, false);
                    }
                }
                Effect::CountdownEnd => overlay::edge_end(),
                Effect::Pulse { progress } => {
                    let wave = if haptics.progressive { rising_pulse(progress).1 } else { haptics.edge_waveform };
                    self.shared.request_haptic(wave);
                }
                Effect::Switch { side, channel } => {
                    log::info!("{} edge: channel {channel}", side.name());
                    // The rising pulses end in one last pulse. With pulses on switching,
                    // the engine's own switch pulse is that one.
                    if haptics.on_edge && haptics.progressive && !haptics.on_switch {
                        self.shared.request_haptic(waveform::COMPLETED);
                        std::thread::sleep(FINAL_PULSE);
                    }
                    self.shared.request_switch(channel);
                    switched = true;
                }
            }
        }
        switched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edge::edge_at;
    use crate::edge_logic::{outer_segments, Zone, MIN_SEGMENT, SIDES};

    fn z(x: i32, y: i32, w: i32, h: i32) -> Zone {
        Zone { x, y, w, h }
    }

    fn layouts() -> Vec<Vec<Zone>> {
        vec![
            vec![z(0, 0, 1920, 1080)],
            vec![z(0, 0, 1920, 1080), z(1920, -200, 2560, 1440)],
            vec![z(0, 0, 1920, 1080), z(0, 1080, 1920, 1080)],
            vec![z(0, 0, 1920, 1080), z(1920, 500, 1280, 1024), z(-1280, 300, 1280, 800)],
            vec![z(0, 0, 2560, 1440), z(400, -1080, 1920, 1080)],
            vec![z(0, 0, 1920, 1080), z(1920, 10, 1920, 1080)],
        ]
    }

    /// The barriers and strips cover exactly the pixels where the X11 watcher sees the
    /// cursor at an outer edge, except bits shorter than MIN_SEGMENT.
    #[test]
    fn segments_match_the_x11_rule() {
        for zones in layouts() {
            let rects: Vec<Rect> = zones.iter().map(|z| Rect { x: f64::from(z.x), y: f64::from(z.y), w: f64::from(z.w), h: f64::from(z.h) }).collect();
            let segs = outer_segments(&zones, [true; 4], 80, false);
            for d in &zones {
                for side in SIDES {
                    let (lo, hi) = if side.vertical() { (d.y, d.y + d.h) } else { (d.x, d.x + d.w) };
                    let mut missing: Vec<i32> = Vec::new();
                    for a in lo..hi {
                        let (x, y) = match side {
                            Side::Left => (d.x, a),
                            Side::Right => (d.x + d.w - 1, a),
                            Side::Top => (a, d.y),
                            Side::Bottom => (a, d.y + d.h - 1),
                        };
                        let x11 = edge_at(&rects, f64::from(x), f64::from(y), 80.0) == Some(edge_of(side));
                        let ours = segs.iter().any(|s| s.side == side && s.zone == *d && a >= s.start && a < s.end);
                        assert!(!ours || x11, "{zones:?} {side:?} of {d:?} at {a}: a barrier where X11 sees no edge");
                        if x11 && !ours {
                            missing.push(a);
                        }
                    }
                    // What is left out comes in runs too short for a barrier.
                    let mut run = 0;
                    for (i, &a) in missing.iter().enumerate() {
                        run = if i > 0 && missing[i - 1] == a - 1 { run + 1 } else { 1 };
                        assert!(run < MIN_SEGMENT, "{zones:?} {side:?} of {d:?}: X11 edge at {a} has no barrier");
                    }
                }
            }
        }
    }

    #[test]
    fn sides_and_edges_correspond() {
        for edge in EDGES {
            assert_eq!(edge_of(side_of(edge)), edge);
            assert_eq!(side_of(edge).name(), edge.name());
        }
    }
}
