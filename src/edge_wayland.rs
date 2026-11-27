//! Choosing how the screen edges are watched, and what the two Wayland watchers share.
//!
//! An X11 session keeps the watcher in edge.rs, unchanged. A Wayland session, where no
//! app may read the cursor, is probed once: a compositor with wlr-layer-shell (KDE,
//! sway, Hyprland, other wlroots desktops) gets 1-pixel strips along the outer edges
//! (edge_layer.rs); one without it (GNOME) gets pointer barriers from the desktop
//! portal's InputCapture (edge_portal.rs). The choice is logged ("edge backend: …") and
//! written to the status file so the settings window can say what works and what does
//! not. FLOCKSWITCH_EDGE_BACKEND=layer-shell|input-capture|gnome-helper|x11|none forces one.
//! GNOME with the helper extension (gnome_ext.rs) is chosen before any of these: the
//! extension tells the cursor, so edge.rs watches it as in an X11 session and no portal
//! session is opened.

use crate::config::{Config, Edge, Edges, EDGES};
use crate::device::waveform;
use crate::edge::{rising_pulse, Rect, FINAL_PULSE, HAPTIC_TICK, STATUS_POLL};
use crate::edge_logic::{self as logic, Choice, DwellConfig, Effect, Probe, Reason, Side, Situation, Zone};
use crate::overlay;
use crate::shared::Shared;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// How often the watchers look (the same as edge.rs).
pub(crate) use crate::edge::POLL;

/// A Wayland session (XDG_SESSION_TYPE=wayland, or WAYLAND_DISPLAY set): no app may read
/// the cursor or grab keys for the whole desktop, so the edges, the cursor highlight and
/// the shortcuts go the Wayland ways.
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

/// The D-Bus connection the RemoteDesktop and InputCapture sessions use, with the app id
/// "flockswitch" registered first (org.freedesktop.host.portal.Registry).
///
/// The portal keeps a remembered answer (a restore token) under the caller's app id, and
/// for a host app it reads that id from how the process was started: "flockswitch" from
/// the app menu or autostart, nothing from a terminal. A token saved under one id is not
/// found under the other, and the system asks again. Registering makes the id the same
/// however flockswitch was started. It must be the first portal call on the connection,
/// so this connection is used for nothing else. Without the registry (an older portal) the
/// connection is still fine; the id then comes from how flockswitch was started.
pub async fn portal_connection() -> Result<ashpd::zbus::Connection, ashpd::Error> {
    use ashpd::zbus::{self, Connection};
    static CONNECTION: std::sync::Mutex<Option<Connection>> = std::sync::Mutex::new(None);
    if let Some(conn) = CONNECTION.lock().unwrap().clone() {
        return Ok(conn);
    }
    let conn = Connection::session().await?;
    let registered = within(Duration::from_secs(5), async {
        let registry = zbus::Proxy::new(
            &conn,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.host.portal.Registry",
        )
        .await?;
        let options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> = std::collections::HashMap::new();
        registry.call_method("Register", &("flockswitch", options)).await.map(|_| ())
    })
    .await;
    match registered {
        Some(Ok(())) => log::info!("portal: app id 'flockswitch' registered for the input sessions (remembered answers are kept under it)"),
        Some(Err(e)) => log::info!(
            "portal: registering the app id 'flockswitch' failed ({e}); remembered answers are kept under the id the portal takes from how flockswitch was started"
        ),
        None => log::info!("portal: the app id registry did not answer; remembered answers are kept under the id the portal takes from how flockswitch was started"),
    }
    let mut slot = CONNECTION.lock().unwrap();
    Ok(slot.get_or_insert(conn).clone())
}

/// Writes the backend and its state for the settings window.
pub fn set_status(shared: &Shared, backend: &str, note: &str) {
    let (backend, note) = (backend.to_string(), note.to_string());
    shared.set_status(|st| {
        st.edge_backend = backend;
        st.edge_note = note;
    });
}

/// The backend the edge worker of this process chose (None: not chosen yet, or this
/// process runs no edge worker, like the settings window).
static CHOSEN: std::sync::Mutex<Option<Choice>> = std::sync::Mutex::new(None);

fn choose_backend(choice: &Choice) {
    *CHOSEN.lock().unwrap() = Some(choice.clone());
}

/// The edge worker is watching through the GNOME helper extension.
pub fn helper_edges() -> bool {
    *CHOSEN.lock().unwrap() == Some(Choice::GnomeHelper)
}

/// Whether the network side uses the GNOME helper extension (net_gnome.rs) instead of the
/// RemoteDesktop and InputCapture portals; see `logic::net_uses_helper`.
pub fn net_uses_helper() -> bool {
    let chosen = CHOSEN.lock().unwrap().clone();
    // Asked last: it may look for the extension, which the other answers can rule out.
    logic::net_uses_helper(true, forced_backend().as_deref(), chosen.as_ref()) && crate::gnome_ext::available()
}

/// FLOCKSWITCH_EDGE_BACKEND, lower case.
fn forced_backend() -> Option<String> {
    std::env::var("FLOCKSWITCH_EDGE_BACKEND").ok().map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty())
}

/// The note written to the status file when no way works: what was found, where that
/// says why.
fn none_note(reason: &Reason, found: &str) -> String {
    match reason {
        Reason::NoSupport | Reason::NoWayland => format!("{}:{found}", reason.code()),
        _ => reason.code(),
    }
}

/// What the edge watcher would write to the status file (backend, note), found once
/// now: for `flockswitch setup` while flockswitch is not running. `portal_limit` bounds
/// the wait for the desktop portal's answer.
pub fn probe_now(portal_limit: Duration) -> (String, String) {
    let forced = forced_backend();
    if !session_is_wayland() && forced.is_none() {
        return if std::env::var_os("DISPLAY").is_some() { ("x11".into(), String::new()) } else { ("none".into(), Reason::NoDisplay.code()) };
    }
    let (probe, found) = probe(forced, portal_limit);
    match logic::choose(&probe) {
        Choice::None(reason) => ("none".into(), none_note(&reason, &found)),
        choice => (choice.name().into(), String::new()),
    }
}

/// Starts watching the edges in whichever way this session allows.
pub fn spawn(shared: Arc<Shared>) {
    let forced = forced_backend();
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
            let (probe, found) = probe(forced.clone(), PORTAL_WAIT);
            let choice = logic::choose(&probe);
            // A portal or compositor that is still starting (flockswitch started at
            // login) answers a little later: look again for a few minutes.
            let again = matches!(choice, Choice::None(Reason::NoSupport | Reason::NoWayland)) && tries < PROBE_RETRIES;
            if let Choice::None(reason) = &choice {
                log::info!("edge backend: none ({})", reason.explain());
                // What was found, for the settings window: a screenshot of it then says
                // why without the log.
                set_status(&shared, choice.name(), &none_note(reason, &found));
            }
            if !again {
                break (probe, choice);
            }
            tries += 1;
            log::info!("edge probe: looking again in {} s ({tries}/{PROBE_RETRIES})", PROBE_RETRY.as_secs());
            std::thread::sleep(PROBE_RETRY);
        };
        choose_backend(&choice);
        match &choice {
            Choice::None(_) => {}
            Choice::GnomeHelper => {
                log::info!("edge backend: gnome-helper (the GNOME Shell extension reads the cursor; no portal is used)");
                set_status(&shared, choice.name(), "");
            }
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
            Choice::GnomeHelper => {
                helper_countdown();
                crate::edge::spawn(shared);
            }
            Choice::LayerShell => crate::edge_layer::run(shared),
            Choice::InputCapture => crate::edge_portal::run(shared, probe.input_capture.unwrap_or(1), probe.gnome),
            Choice::None(_) => {}
        }
    });
    if let Err(e) = started {
        log::warn!("edge switching: cannot start its thread: {e}");
    }
}

/// Where the countdown can be drawn with the helper: through XWayland, when its screens
/// are the extension's monitors (or a multiple), as the portal backend decides it; else
/// the wait goes without a drawing.
fn helper_countdown() {
    let zones: Vec<Zone> = crate::gnome_ext::displays()
        .iter()
        .map(|d| Zone { x: d.x as i32, y: d.y as i32, w: d.w as i32, h: d.h as i32 })
        .collect();
    let screens = crate::edge_portal::xwayland_screens();
    match logic::xwayland_scale(&zones, &screens) {
        Some(k) => {
            overlay::use_gnome_helper(k);
            log::info!("gnome helper: countdown through XWayland (its screens {screens:?} are the monitors {zones:?} × {k})");
        }
        None => log::info!("gnome helper: countdown not drawn (XWayland screens {screens:?} do not line up with the monitors {zones:?})"),
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
/// How long the watcher waits for the desktop portal (it may still be starting at login).
const PORTAL_WAIT: Duration = Duration::from_secs(25);

/// What this session offers, logged step by step, and a one-line summary of it.
fn probe(forced: Option<String>, portal_limit: Duration) -> (Probe, String) {
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
        match crate::wl::Wl::connect(false, false) {
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
    let gnome_desktop = desktop().to_ascii_lowercase().contains("gnome");
    // The helper extension answers: no portal is asked (unless a way is forced).
    if wayland && gnome_desktop && crate::gnome_ext::available_waiting() {
        p.gnome_helper = true;
        log::info!("edge probe: GNOME helper extension answers");
        found.push("GNOME helper".into());
    }
    let helper = p.gnome_helper && p.forced.as_deref().is_none_or(|f| f == "gnome-helper");
    let want_portal = !helper && (p.layer_shell != Some(true) || p.forced.as_deref() == Some("input-capture"));
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
            match within(portal_limit, crate::edge_portal::probe()).await {
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
                    log::info!("edge probe: portal InputCapture: no answer in {} s", portal_limit.as_secs());
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
pub fn preview(edges: &Edges, delay_ms: u64) {
    static TARGET: OnceLock<&'static str> = OnceLock::new();
    let target = *TARGET.get_or_init(|| {
        if crate::wl::Wl::connect(false, false).is_ok_and(|wl| wl.has_layer_shell()) {
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
    let (edge, duration) = crate::edge::preview_edge(edges, delay_ms);
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
        }
    }

    pub fn active(&self) -> bool {
        // Without a computer placed on a side there is nothing to arm: opening a
        // capture session then would only report that no barrier could be made.
        self.cfg.logitech_edges_allowed()
            || (self.network_wanted() && !self.network_sides().is_empty())
    }

    /// The sides a paired computer is placed on, connected or not. Barriers follow the
    /// placement, not the connection: a computer dropping and coming back must not
    /// rebuild the capture session (GNOME before 49 would ask for permission again).
    /// Pushing at a side whose computer is away is handed back by netcore at once.
    pub fn network_sides(&self) -> Vec<flock_net::Side> {
        if let Some(status) = crate::net::live_status().filter(|s| s.running && !s.layout.is_empty() && !s.me.id.is_empty()) {
            let sides: Vec<flock_net::Side> = [flock_net::Side::Left, flock_net::Side::Right, flock_net::Side::Top, flock_net::Side::Bottom]
                .into_iter()
                .filter(|&side| flock_net::layout::neighbour_on_side(&status.layout, &status.me.id, side).is_some())
                .collect();
            if !sides.is_empty() {
                return sides;
            }
        }
        self.cfg.network.peers.iter().filter_map(|p| p.side).collect()
    }

    pub fn network_wanted(&self) -> bool {
        self.cfg.network_edges_allowed() && self.cfg.network.notice_accepted && std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase().contains("gnome")
    }

    /// Edge switching is set up, whether or not flockswitch is paused from the menu.
    pub fn edges_wanted(&self) -> bool {
        (self.cfg.edges.on && self.cfg.edges.any()) || self.network_wanted()
    }

    /// Which sides have a channel (and so a barrier or a strip), paused or not.
    pub fn sides(&self) -> [bool; 4] {
        let on = self.edges_wanted();
        let mut sides = [false; 4];
        for edge in EDGES {
            sides[side_of(edge).index()] = on && self.cfg.logitech_edges_allowed() && self.cfg.edges.channel(edge) != 0;
            if self.network_wanted() && self.network_sides().contains(&crate::net::side(edge)) {
                sides[side_of(edge).index()] = true;
            }
        }
        sides
    }

    /// The settings that decide whether input capture is wanted and where, from the
    /// configuration only (not the live network layout): after a "no" the system is asked
    /// again only when one of these changes.
    pub fn ask_key(&self) -> String {
        let e = &self.cfg.edges;
        format!(
            "{:?}",
            (
                self.edges_wanted(),
                self.network_wanted(),
                self.cfg.logitech_edges_allowed(),
                (e.left, e.right, e.top, e.bottom),
                self.cfg.network.peers.iter().map(|p| p.side).collect::<Vec<_>>(),
            )
        )
    }

    pub fn margin(&self) -> i32 {
        if self.network_wanted() { self.cfg.network.corner_margin.clamp(0.0, 1000.0) as i32 }
        else { i32::try_from(self.cfg.edges.corner_margin).unwrap_or(i32::MAX) }
    }

    pub fn dwell_config(&self) -> DwellConfig {
        let mut channels = [0; 4];
        for edge in EDGES {
            channels[side_of(edge).index()] = if !self.cfg.logitech_edges_allowed() {0} else {self.cfg.edges.channel(edge)};
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

    /// The idle dispatch deadline, capped by the next engine status check.
    pub fn dispatch_interval(&self, pointer_on_edge: bool) -> Duration {
        let wait = crate::edge_logic::dispatch_interval(pointer_on_edge);
        wait.min(self.status_read.map_or(Duration::ZERO, |t| STATUS_POLL.saturating_sub(t.elapsed())))
    }

    /// Once per poll: picks up changed settings and, every 300 ms, engine status.
    /// True when the settings changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        if self.shared.version() != self.version {
            self.version = self.shared.version();
            self.cfg = self.shared.config();
            self.announced = false;
            changed = true;
        }
        if !std::mem::replace(&mut self.announced, true) {
            crate::edge::announce(&self.cfg);
        }
        if self.status_read.is_none_or(|t| t.elapsed() >= STATUS_POLL) {
            let first = self.status_read.is_none();
            self.status_read = Some(Instant::now());
            let last_seen = &self.last_switch_text;
            let (channel, now_here, switch_changed) =
                self.shared.read_status(|st| (st.channel, st.trigger.is_some(), (st.last_switch != *last_seen).then(|| st.last_switch.clone())));
            self.sit.here = channel;
            if let Some(text) = switch_changed {
                self.landing_channel = text.as_deref().and_then(crate::edge::last_channel);
                self.last_switch_text = text;
            }
            let arrived = now_here && !self.sit.mouse_here && !first;
            if arrived && self.cfg.logitech_edges_allowed() && self.cfg.edges.place_cursor {
                let edge = self.landing_channel.take().and_then(|c| self.cfg.edges.edge_for(c));
                if let Some(edge) = edge.filter(|_| !std::mem::replace(&mut self.told_place, true)) {
                    log::info!("mouse is back: Wayland lets no app move the cursor, so it is not put at the {} edge", edge.name());
                }
            }
            // Finding the cursor on arrival and on request is locate_wayland.rs's, which
            // runs whether or not the edges are watched.
            self.sit.mouse_here = now_here;
        }
        changed
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
