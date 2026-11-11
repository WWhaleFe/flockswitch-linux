//! Switching at the screen edge on GNOME's Wayland session (and on any desktop whose
//! portal has InputCapture but whose compositor has no layer shell), through the XDG
//! desktop portal's InputCapture interface.
//!
//! flockswitch asks for an input-capture session (the system asks the user once, and
//! with portal version 2 remembers the answer through a restore token), puts a pointer
//! barrier on each piece of an outer edge that has a channel (corners and edges shared
//! with another display left out, as on X11) and enables it. When the pointer pushes
//! against a barrier the compositor hands the pointer to flockswitch ("activated"): it
//! stays at the edge while the countdown runs, its relative motion arriving over libei
//! (reis). Moving back into the screen, sliding into a corner, a click, a key, or
//! a wheel turn gives the pointer back at once ("release", with the cursor put where it
//! would be); resting for the whole wait switches exactly as the X11 watcher does, and
//! then gives it back. Where resting would not switch (the master mouse is elsewhere,
//! Ctrl is not held) the pointer simply stays at the edge, as it would on X11, until it
//! moves back in or slides along the edge; giving it back at once would only have the
//! compositor hand it over again with the next push.
//!
//! Changing the barriers needs the session disabled and enabled again. GNOME before 49
//! loses the captured devices on that, so there the session is made again instead,
//! which asks the user again with portal version 1.

use crate::config::Config;
use crate::edge::Rect;
use crate::edge_logic::{self as logic, Captured, CtrlKeys, Dwell, Segment, Zone};
use crate::edge_wayland::{set_status, within, Watch, POLL};
use crate::overlay;
use crate::shared::Shared;
use ashpd::desktop::input_capture::{
    Activated, ActivatedBarrier, Barrier, BarrierID, Capabilities, ConnectToEISOptions, CreateSession2Options, CreateSessionOptions,
    Deactivated, Disabled, DisableOptions, EnableOptions, GetZonesOptions, InputCapture, ReleaseOptions, SetPointerBarriersOptions,
    StartOptions, ZonesChanged,
};
use ashpd::desktop::{PersistMode, ResponseError, Session};
use futures_util::stream::{self, LocalBoxStream};
use futures_util::StreamExt;
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The portal's version and what it can capture ("keyboard, pointer").
pub async fn probe() -> Result<(u32, String), String> {
    let ic = InputCapture::new().await.map_err(|e| format!("{e}"))?;
    let caps = ic.supported_capabilities().await.map_err(|e| format!("{e}"))?;
    Ok((ic.version(), caps_text(caps)))
}

fn caps_text(caps: ashpd::enumflags2::BitFlags<Capabilities>) -> String {
    let names: Vec<&str> = [(Capabilities::Keyboard, "keyboard"), (Capabilities::Pointer, "pointer"), (Capabilities::Touchscreen, "touchscreen")]
        .into_iter()
        .filter(|(c, _)| caps.contains(*c))
        .map(|(_, n)| n)
        .collect();
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

fn token_path() -> std::path::PathBuf {
    Config::path().with_file_name("input-capture.token")
}

fn load_token() -> Option<String> {
    std::fs::read_to_string(token_path()).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

fn save_token(token: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(token_path())
        .and_then(|mut f| f.write_all(token.as_bytes()));
    if let Err(e) = written {
        log::warn!("input-capture: cannot save the restore token: {e}");
    }
}

/// The session's object path (for the log and its Closed signal), read by serializing
/// the session as D-Bus does: ashpd keeps it to itself.
fn session_path(session: &Session<InputCapture>) -> String {
    use ashpd::zvariant::{serialized::Context, to_bytes, LE};
    to_bytes(Context::new_dbus(LE, 0), session)
        .ok()
        .and_then(|bytes| bytes.get(4..).map(|b| String::from_utf8_lossy(b).trim_end_matches('\0').to_string()))
        .unwrap_or_default()
}

/// A signal of `proxy` as a stream that borrows nothing (ashpd's own streams borrow the
/// portal object).
async fn signal<T>(proxy: &ashpd::zbus::Proxy<'_>, name: &'static str) -> Result<LocalBoxStream<'static, T>, OpenError>
where
    T: for<'de> serde::Deserialize<'de> + ashpd::zvariant::Type + 'static,
{
    let stream = proxy.receive_signal(name).await.map_err(|e| OpenError::Failed(format!("listening for {name}: {e}")))?;
    Ok(stream.filter_map(|m| std::future::ready(m.body().deserialize::<T>().ok())).boxed_local())
}

/// The screens XWayland reports (its coordinates), to draw the countdown there.
fn xwayland_screens() -> Vec<Zone> {
    use x11_dl::xinerama::Xlib as Xinerama;
    use x11_dl::xlib::Xlib;
    if std::env::var_os("DISPLAY").is_none() {
        return Vec::new();
    }
    let (Ok(xlib), Ok(xin)) = (Xlib::open(), Xinerama::open()) else { return Vec::new() };
    // Safety: the display is opened, read and closed here; nothing else uses it.
    unsafe {
        let display = (xlib.XOpenDisplay)(std::ptr::null());
        if display.is_null() {
            return Vec::new();
        }
        let mut count = 0;
        let info = (xin.XineramaQueryScreens)(display, &mut count);
        let mut out = Vec::new();
        if !info.is_null() {
            for s in std::slice::from_raw_parts(info, count.max(0) as usize) {
                out.push(Zone { x: i32::from(s.x_org), y: i32::from(s.y_org), w: i32::from(s.width), h: i32::from(s.height) });
            }
            (xlib.XFree)(info.cast());
        }
        (xlib.XCloseDisplay)(display);
        out
    }
}

enum Ev {
    Activated(Activated),
    Deactivated(Option<u32>),
    Disabled,
    ZonesChanged,
    Closed,
    Ei(Result<EiEvent, reis::Error>),
    EiGone,
    Tick,
}

/// Why a session ended.
enum End {
    Stop,
    /// Edge switching was turned off.
    Inactive,
    /// Made again because something went wrong (devices lost, a call failed, …).
    Again(String),
    /// Made again because the barriers must change and cannot be changed in place.
    Changed(String),
    /// The portal closed it.
    Closed,
}

enum OpenError {
    Denied,
    Failed(String),
}

impl From<ashpd::Error> for OpenError {
    fn from(e: ashpd::Error) -> Self {
        match e {
            ashpd::Error::Response(ResponseError::Cancelled) => OpenError::Denied,
            other => OpenError::Failed(format!("{other}")),
        }
    }
}

struct Live {
    ic: InputCapture,
    session: Session<InputCapture>,
    path: String,
    ei: reis::event::Connection,
    events: Option<LocalBoxStream<'static, Ev>>,
    enabled: bool,
    /// Barrier id → the piece of edge it lies on.
    barriers: HashMap<u32, Segment>,
    zones: Vec<Zone>,
    /// The sides and margin the barriers were made for.
    made_for: ([bool; 4], i32),
    /// XWayland coordinates = zone coordinates × this, when they line up.
    xwayland: Option<f64>,
    /// The libei devices that move the pointer and type.
    pointers: usize,
    /// flockswitch is paused from the menu: the session stays (asking again on resume
    /// would bring the question back), but nothing is held.
    paused: bool,
}

struct Capture {
    id: Option<u32>,
    cap: Captured,
    since: Instant,
    last_drawn: (f64, f64),
}

struct Portal {
    watch: Watch,
    version: u32,
    reconfigure: bool,
    dwell: Dwell,
    ctrl: CtrlKeys,
    capture: Option<Capture>,
    /// Motion that came over libei before the portal's "activated".
    early_motion: (f64, f64),
    out: Vec<logic::Effect>,
}

pub fn run(shared: Arc<Shared>, version: u32, gnome: Option<u32>) {
    let portal_is_gnome = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase().contains("gnome");
    let reconfigure = logic::can_reconfigure(gnome, portal_is_gnome);
    log::info!(
        "input-capture: portal v{version}; barriers are changed {}",
        if reconfigure { "in place (Disable, SetPointerBarriers, Enable)" } else { "by making the session again (GNOME before 49)" }
    );
    let mut portal = Portal {
        watch: Watch::new(shared),
        version,
        reconfigure,
        dwell: Dwell::new(),
        ctrl: CtrlKeys::default(),
        capture: None,
        early_motion: (0.0, 0.0),
        out: Vec::new(),
    };
    async_io::block_on(portal.run());
    overlay::edge_end();
}

impl Portal {
    fn shared(&self) -> &Shared {
        &self.watch.shared
    }

    fn note(&self, note: &str) {
        set_status(self.shared(), "input-capture", note);
    }

    async fn run(&mut self) {
        // Denied (or ended by the system): asked again only after the settings change.
        let mut denied_for: Option<u64> = None;
        // Sessions that failed or ended soon after they were made, one after another.
        // Each new session may ask the user again, so after a few flockswitch waits
        // for the settings to change instead of asking on and on.
        let mut failures = 0u32;
        let mut noted_off = false;
        loop {
            if self.shared().stopped() {
                return;
            }
            self.watch.poll();
            if !self.watch.active() {
                if !noted_off {
                    noted_off = true;
                    self.note("off");
                }
                async_io::Timer::after(POLL * 4).await;
                continue;
            }
            noted_off = false;
            if denied_for == Some(self.watch.shared.version()) {
                async_io::Timer::after(POLL * 4).await;
                continue;
            }
            denied_for = None;
            self.note(&format!("asking:{}", self.version));
            let opened = Instant::now();
            match self.open().await {
                Ok(mut live) => {
                    let end = self.session(&mut live).await;
                    self.end_capture(&mut live, None, "session ends").await;
                    match live.session.close().await {
                        Ok(()) => log::info!("input-capture: session {} closed", live.path),
                        Err(e) => log::info!("input-capture: closing session {}: {e}", live.path),
                    }
                    let quick = opened.elapsed() < Duration::from_secs(30);
                    failures = if quick && matches!(end, End::Again(_)) { failures + 1 } else { 0 };
                    match end {
                        End::Stop => return,
                        End::Inactive => log::info!("input-capture: edge switching is off; session closed"),
                        End::Again(why) if failures >= 3 => {
                            log::warn!("input-capture: {why}, the third time in a row soon after starting; trying again when the edge settings change");
                            self.note("error");
                            denied_for = Some(self.watch.shared.version());
                            failures = 0;
                        }
                        End::Again(why) => {
                            log::info!("input-capture: making the session again ({why})");
                            async_io::Timer::after(Duration::from_secs(u64::from(failures) * 3)).await;
                        }
                        End::Changed(why) => log::info!("input-capture: making the session again ({why}; the system may ask again)"),
                        // Ended from the system (its "stop" button, a portal restart): made
                        // again at once only where that asks nothing.
                        End::Closed if self.version >= 2 && load_token().is_some() => {
                            log::info!("input-capture: the portal closed the session; making it again in 3 s (restore token, no question)");
                            async_io::Timer::after(Duration::from_secs(3)).await;
                        }
                        End::Closed => {
                            log::info!("input-capture: the portal closed the session; asking again when the edge settings change");
                            self.note("closed");
                            denied_for = Some(self.watch.shared.version());
                        }
                    }
                }
                Err(OpenError::Denied) => {
                    log::info!("input-capture: the user did not allow input capture; asking again when the edge settings change");
                    self.note("denied");
                    denied_for = Some(self.watch.shared.version());
                }
                Err(OpenError::Failed(e)) if failures >= 2 => {
                    log::warn!("input-capture: {e}; the third time in a row; trying again when the edge settings change");
                    self.note("error");
                    denied_for = Some(self.watch.shared.version());
                    failures = 0;
                }
                Err(OpenError::Failed(e)) => {
                    failures += 1;
                    let wait = Duration::from_secs(10 * u64::from(failures));
                    log::warn!("input-capture: {e}; trying again in {} s", wait.as_secs());
                    self.note("error");
                    async_io::Timer::after(wait).await;
                }
            }
        }
    }

    /// Asks for a session, connects to EIS and listens to its signals.
    async fn open(&mut self) -> Result<Live, OpenError> {
        let ic = InputCapture::new().await?;
        let wanted = Capabilities::Pointer | Capabilities::Keyboard;
        let (session, caps) = if self.version >= 2 {
            let session = ic.create_session2(CreateSession2Options::default()).await?;
            let token = load_token();
            log::info!(
                "input-capture: CreateSession2 → {}; Start (remember until revoked, {})",
                session_path(&session),
                if token.is_some() { "with the saved restore token" } else { "no restore token yet: the system asks" }
            );
            let options = StartOptions::default().set_capabilities(wanted).set_persist_mode(PersistMode::ExplicitlyRevoked).set_restore_token(token);
            let response = ic.start(&session, None, options).await?.response()?;
            match response.restore_token() {
                Some(t) => {
                    save_token(t);
                    log::info!("input-capture: restore token saved (no question next time)");
                }
                None => log::info!("input-capture: no restore token (the answer is not remembered)"),
            }
            (session, response.capabilities())
        } else {
            log::info!("input-capture: CreateSession (portal v1: the system asks every time a session is made)");
            ic.create_session(None, CreateSessionOptions::default().set_capabilities(wanted)).await?
        };
        let path = session_path(&session);
        log::info!("input-capture: session {path} allowed with {}", caps_text(caps));
        match self.connect(ic, session, path).await {
            Ok(live) => Ok(live),
            Err((session, e)) => {
                let _ = session.close().await;
                Err(e)
            }
        }
    }

    /// Listens to the session's signals and connects to EIS; on failure the session is
    /// handed back to be closed.
    async fn connect(&mut self, ic: InputCapture, session: Session<InputCapture>, path: String) -> Result<Live, (Session<InputCapture>, OpenError)> {
        match self.connect_inner(&ic, &session, &path).await {
            Ok((ei, events)) => Ok(Live {
                ic,
                session,
                path,
                ei,
                events: Some(events),
                enabled: false,
                barriers: HashMap::new(),
                zones: Vec::new(),
                made_for: ([false; 4], 0),
                xwayland: None,
                pointers: 0,
                paused: false,
            }),
            Err(e) => Err((session, e)),
        }
    }

    async fn connect_inner(
        &mut self,
        ic: &InputCapture,
        session: &Session<InputCapture>,
        path: &str,
    ) -> Result<(reis::event::Connection, LocalBoxStream<'static, Ev>), OpenError> {
        let path = path.to_string();
        // Subscribed before Enable, so no activation is missed.
        let activated = signal::<Activated>(ic, "Activated").await?;
        let deactivated = signal::<Deactivated>(ic, "Deactivated").await?;
        let disabled = signal::<Disabled>(ic, "Disabled").await?;
        let zones_changed = signal::<ZonesChanged>(ic, "ZonesChanged").await?;
        // The session's own Closed signal (without its path, the portal's signals still
        // tell when it stops working).
        let closed: LocalBoxStream<'static, Ev> = match ashpd::zbus::Proxy::new(ic.connection(), "org.freedesktop.portal.Desktop", path.clone(), "org.freedesktop.portal.Session").await {
            Ok(proxy) => match proxy.receive_signal("Closed").await {
                Ok(stream) => stream.map(|_| Ev::Closed).boxed_local(),
                Err(e) => {
                    log::info!("input-capture: cannot listen for the session's Closed signal: {e}");
                    stream::pending().boxed_local()
                }
            },
            Err(e) => {
                log::info!("input-capture: no proxy for session '{path}': {e}");
                stream::pending().boxed_local()
            }
        };
        let fd = ic.connect_to_eis(session, ConnectToEISOptions::default()).await?;
        let context = ei::Context::new(UnixStream::from(fd)).map_err(|e| OpenError::Failed(format!("libei socket: {e}")))?;
        let (ei, ei_events) = within(Duration::from_secs(10), context.handshake_async_io("flockswitch", ei::handshake::ContextType::Receiver))
            .await
            .ok_or_else(|| OpenError::Failed("libei handshake: no answer in 10 s".into()))?
            .map_err(|e| OpenError::Failed(format!("libei handshake: {e}")))?;
        log::info!("input-capture: connected to EIS (libei receiver)");
        // flockswitch has one session at a time (an old one is closed before the next is
        // made), so the portal's signals are all about it.
        let streams: Vec<LocalBoxStream<'static, Ev>> = vec![
            activated.map(Ev::Activated).boxed_local(),
            deactivated.map(|d| Ev::Deactivated(d.activation_id())).boxed_local(),
            disabled.map(|_| Ev::Disabled).boxed_local(),
            zones_changed.map(|_| Ev::ZonesChanged).boxed_local(),
            closed,
            ei_events.map(Ev::Ei).chain(stream::once(async { Ev::EiGone })).boxed_local(),
            async_io::Timer::interval(POLL).map(|_| Ev::Tick).boxed_local(),
        ];
        Ok((ei, stream::select_all(streams).boxed_local()))
    }

    /// Reads the zones, puts the barriers and enables the session.
    async fn arm(&mut self, live: &mut Live) -> Result<(), String> {
        let err = |e: ashpd::Error| format!("{e}");
        if live.enabled {
            match live.ic.disable(&live.session, DisableOptions::default()).await {
                Ok(()) => log::info!("input-capture: disabled to change the barriers"),
                Err(e) => log::info!("input-capture: disable: {e}"),
            }
            live.enabled = false;
        }
        let zones = live.ic.zones(&live.session, GetZonesOptions::default()).await.map_err(err)?.response().map_err(err)?;
        live.zones = zones.regions().iter().map(|r| Zone { x: r.x_offset(), y: r.y_offset(), w: r.width() as i32, h: r.height() as i32 }).collect();
        log::info!("input-capture: zones {:?} (zone set {})", live.zones, zones.zone_set());
        let sides = self.watch.sides();
        let margin = self.watch.margin();
        live.made_for = (sides, margin);
        live.barriers.clear();
        for whole in [false, true] {
            let segments = logic::outer_segments(&live.zones, sides, margin, whole);
            if segments.is_empty() {
                log::info!("input-capture: no outer edge with a channel to put a barrier on");
                break;
            }
            let barriers: Vec<Barrier> = segments
                .iter()
                .enumerate()
                .filter_map(|(i, s)| Some(Barrier::new(BarrierID::new(i as u32 + 1)?, s.barrier())))
                .collect();
            for (i, s) in segments.iter().enumerate() {
                log::info!("input-capture: barrier {}: {} edge of {:?} at {:?}", i + 1, s.side.name(), s.zone, s.barrier());
            }
            let response = live
                .ic
                .set_pointer_barriers(&live.session, &barriers, zones.zone_set(), SetPointerBarriersOptions::default())
                .await
                .map_err(err)?
                .response()
                .map_err(err)?;
            let failed: Vec<u32> = response.failed_barriers().iter().map(|id| id.get()).collect();
            live.barriers = segments.iter().enumerate().map(|(i, s)| (i as u32 + 1, *s)).filter(|(id, _)| !failed.contains(id)).collect();
            if failed.is_empty() {
                break;
            }
            log::info!("input-capture: barriers refused: {failed:?}");
            if !live.barriers.is_empty() || whole || margin == 0 {
                break;
            }
            log::info!("input-capture: trying barriers along whole edges (corners then left out by following the pointer)");
        }
        if live.barriers.is_empty() {
            self.note("no-barriers");
            return Ok(());
        }
        live.ic.enable(&live.session, EnableOptions::default()).await.map_err(err)?;
        live.enabled = true;
        log::info!("input-capture: enabled with {} barriers", live.barriers.len());
        self.note(&format!("ready:{}", self.version));
        // The countdown: through XWayland when its screens are the zones (or a multiple).
        let screens = xwayland_screens();
        live.xwayland = logic::xwayland_scale(&live.zones, &screens);
        match live.xwayland {
            Some(k) => {
                overlay::use_xwayland();
                log::info!("input-capture: countdown through XWayland (its screens {screens:?} are the zones × {k})");
            }
            None => log::info!("input-capture: countdown not drawn (XWayland screens {screens:?} do not line up with the zones)"),
        }
        Ok(())
    }

    /// Runs a session until it ends.
    async fn session(&mut self, live: &mut Live) -> End {
        if let Err(e) = self.arm(live).await {
            return End::Again(format!("setting the barriers failed: {e}"));
        }
        let mut events = live.events.take().expect("the session's events");
        while let Some(ev) = events.next().await {
            if self.shared().stopped() {
                return End::Stop;
            }
            match ev {
                Ev::Tick => {
                    if self.watch.poll() {
                        if !self.watch.edges_wanted() {
                            return End::Inactive;
                        }
                        let paused = !self.watch.cfg.enabled;
                        if paused != live.paused {
                            live.paused = paused;
                            self.end_capture(live, None, "paused").await;
                            if paused && self.reconfigure && live.enabled {
                                match live.ic.disable(&live.session, DisableOptions::default()).await {
                                    Ok(()) => log::info!("input-capture: paused: disabled"),
                                    Err(e) => log::info!("input-capture: paused: disable: {e}"),
                                }
                                live.enabled = false;
                            } else if paused {
                                log::info!("input-capture: paused: the pointer is given back at once at the edges");
                            } else if !live.enabled {
                                if let Err(e) = self.arm(live).await {
                                    return End::Again(format!("enabling after the pause failed: {e}"));
                                }
                            } else {
                                log::info!("input-capture: resumed");
                            }
                        }
                        if !live.paused && live.made_for != (self.watch.sides(), self.watch.margin()) {
                            self.end_capture(live, None, "the edge settings changed").await;
                            if !self.reconfigure {
                                return End::Changed("the edges with a channel changed".into());
                            }
                            if let Err(e) = self.arm(live).await {
                                return End::Again(format!("changing the barriers failed: {e}"));
                            }
                        }
                    }
                    self.follow(live).await;
                }
                Ev::Activated(a) => self.activated(live, a).await,
                Ev::Deactivated(id) => {
                    if self.capture.as_ref().is_some_and(|c| id.is_none() || c.id == id) {
                        log::info!("input-capture: deactivated by the compositor (activation {id:?})");
                        self.capture = None;
                        self.wait_step(live, None);
                    }
                }
                Ev::Disabled => {
                    log::info!("input-capture: disabled by the system");
                    self.capture = None;
                    self.wait_step(live, None);
                    live.enabled = false;
                    if !self.reconfigure {
                        return End::Changed("disabled by the system".into());
                    }
                    if live.paused {
                        continue;
                    }
                    async_io::Timer::after(Duration::from_secs(1)).await;
                    if let Err(e) = self.arm(live).await {
                        return End::Again(format!("enabling again failed: {e}"));
                    }
                }
                Ev::ZonesChanged => {
                    log::info!("input-capture: the displays changed");
                    self.end_capture(live, None, "the displays changed").await;
                    live.enabled = false;
                    if !self.reconfigure {
                        return End::Changed("the displays changed".into());
                    }
                    if live.paused {
                        continue;
                    }
                    if let Err(e) = self.arm(live).await {
                        return End::Again(format!("new barriers failed: {e}"));
                    }
                }
                Ev::Closed => return End::Closed,
                Ev::EiGone => return End::Again("the libei connection ended".into()),
                Ev::Ei(Err(e)) => return End::Again(format!("libei: {e}")),
                Ev::Ei(Ok(e)) => {
                    if let Some(end) = self.ei_event(live, e).await {
                        return end;
                    }
                }
            }
        }
        End::Again("the event streams ended".into())
    }

    async fn activated(&mut self, live: &mut Live, a: Activated) {
        let id = a.activation_id();
        let at = a.cursor_position().map(|(x, y)| (f64::from(x), f64::from(y)));
        let barrier = match a.barrier_id() {
            Some(ActivatedBarrier::Barrier(b)) => Some(b.get()),
            _ => None,
        };
        log::info!("input-capture: activated (activation {id:?}, session {}) at {at:?} by barrier {barrier:?}", a.session_handle().as_str());
        // The barrier hit, or the one nearest to where the pointer is.
        let seg = barrier.and_then(|b| live.barriers.get(&b).copied()).or_else(|| {
            let (x, y) = at?;
            live.barriers.values().min_by(|a, b| distance(a, x, y).total_cmp(&distance(b, x, y))).copied()
        });
        let Some(seg) = seg else {
            self.release(live, id, Instant::now(), None, "no barrier known for it").await;
            return;
        };
        if live.paused {
            let at = at.map(|p| Captured::new(seg, p).position());
            self.release(live, id, Instant::now(), at, "paused").await;
            return;
        }
        let mut cap = Captured::new(seg, at.unwrap_or_else(|| seg.edge_point(f64::from(seg.start + seg.end) / 2.0)));
        let early = std::mem::take(&mut self.early_motion);
        if early != (0.0, 0.0) {
            cap.motion(early.0, early.1);
        }
        self.capture = Some(Capture { id, cap, since: Instant::now(), last_drawn: cap.edge_position() });
        let now = Instant::now();
        if live.pointers == 0 {
            self.end_capture(live, Some(cap.position()), "no pointer device over libei, so the pointer cannot be followed").await;
            return;
        }
        if !cap.at_edge() {
            self.end_capture(live, Some(cap.position()), "in a corner").await;
            return;
        }
        if !self.dwell.would_switch(seg.side, &self.watch.dwell_config(), &self.watch.sit, now) {
            log::info!("input-capture: held without a countdown: {}", self.why_not(seg.side));
        }
        self.follow(live).await;
    }

    fn why_not(&self, side: logic::Side) -> String {
        let channel = self.watch.dwell_config().channels[side.index()];
        if !self.watch.sit.mouse_here {
            "the master mouse is not on this computer".into()
        } else if self.watch.sit.here == Some(channel) {
            format!("channel {channel} is this computer")
        } else {
            "just switched".into()
        }
    }

    /// One step of the wait while (or after) the pointer is held.
    fn wait_step(&mut self, live: &Live, at: Option<logic::Side>) -> bool {
        self.out.clear();
        let cfg = self.watch.dwell_config();
        let gap = self.watch.gap();
        if self.watch.active() {
            self.dwell.step(at, self.ctrl.held(), &cfg, &self.watch.sit, Instant::now(), &gap, &mut self.out);
        } else {
            self.dwell.idle(&mut self.out);
        }
        let capture = self.capture.as_ref().map(|c| c.cap);
        let xwayland = live.xwayland;
        let place = |side: logic::Side| {
            let k = xwayland?;
            let cap = capture.filter(|c| c.seg.side == side)?;
            let z = cap.seg.zone;
            let (x, y) = cap.edge_position();
            Some((Rect { x: f64::from(z.x) * k, y: f64::from(z.y) * k, w: f64::from(z.w) * k, h: f64::from(z.h) * k }, (x * k, y * k)))
        };
        let out = std::mem::take(&mut self.out);
        let switched = self.watch.apply(&out, place);
        self.out = out;
        switched
    }

    /// Follows the held pointer: the wait, and when to give it back.
    async fn follow(&mut self, live: &mut Live) {
        let Some(c) = self.capture.as_ref() else {
            self.wait_step(live, None);
            return;
        };
        let cap = c.cap;
        let at = cap.at_edge().then_some(cap.seg.side);
        let switched = self.wait_step(live, at);
        if switched {
            self.end_capture(live, Some(cap.position()), "switched").await;
        } else if at.is_none() {
            self.end_capture(live, Some(cap.position()), "moved away from the edge").await;
        } else if !self.dwell.counting_down() && cap.slid() > logic::SLIDE_RELEASE {
            self.end_capture(live, Some(cap.position()), "slid along the edge without a countdown").await;
        } else if self.dwell.counting_down() {
            if let (Some(k), Some(c)) = (live.xwayland, self.capture.as_mut()) {
                let p = c.cap.edge_position();
                if p != c.last_drawn {
                    c.last_drawn = p;
                    overlay::edge_cursor(p.0 * k, p.1 * k);
                }
            }
        }
    }

    /// Gives the pointer back (at `at`, if given) and ends the wait.
    async fn end_capture(&mut self, live: &mut Live, at: Option<(f64, f64)>, why: &str) {
        let Some(c) = self.capture.take() else { return };
        self.release(live, c.id, c.since, at, why).await;
    }

    async fn release(&mut self, live: &mut Live, id: Option<u32>, since: Instant, at: Option<(f64, f64)>, why: &str) {
        let mut options = ReleaseOptions::default().set_activation_id(id);
        if let Some(p) = at {
            options = options.set_cursor_position(p);
        }
        match live.ic.release(&live.session, options).await {
            Ok(()) => log::info!("input-capture: released (activation {id:?}) at {at:?} after {} ms: {why}", since.elapsed().as_millis()),
            Err(e) => log::info!("input-capture: release (activation {id:?}): {e}"),
        }
        self.wait_step(live, None);
        self.ctrl.forget();
        self.early_motion = (0.0, 0.0);
    }

    async fn ei_event(&mut self, live: &mut Live, event: EiEvent) -> Option<End> {
        match event {
            EiEvent::SeatAdded(e) => {
                e.seat.bind_capabilities(DeviceCapability::Pointer | DeviceCapability::Button | DeviceCapability::Scroll | DeviceCapability::Keyboard);
                let _ = live.ei.flush();
                log::info!("input-capture: libei seat {:?}: bound pointer, button, scroll, keyboard", e.seat.name());
            }
            EiEvent::DeviceAdded(e) => {
                let pointer = e.device.has_capability(DeviceCapability::Pointer);
                if pointer {
                    live.pointers += 1;
                }
                log::info!(
                    "input-capture: libei device {:?} added ({}{})",
                    e.device.name(),
                    if pointer { "pointer" } else { "" },
                    if e.device.has_capability(DeviceCapability::Keyboard) { " keyboard" } else { "" }
                );
            }
            EiEvent::DeviceRemoved(e) => {
                if e.device.has_capability(DeviceCapability::Pointer) {
                    live.pointers = live.pointers.saturating_sub(1);
                }
                log::info!("input-capture: libei device {:?} removed", e.device.name());
            }
            EiEvent::DeviceStartEmulating(e) => {
                log::debug!("input-capture: libei start emulating (sequence {})", e.sequence);
                if self.capture.is_none() {
                    self.early_motion = (0.0, 0.0);
                }
            }
            EiEvent::DeviceStopEmulating(_) => {
                log::debug!("input-capture: libei stop emulating");
                self.early_motion = (0.0, 0.0);
            }
            EiEvent::PointerMotion(m) => match self.capture.as_mut() {
                Some(c) => c.cap.motion(f64::from(m.dx), f64::from(m.dy)),
                None => {
                    self.early_motion.0 += f64::from(m.dx);
                    self.early_motion.1 += f64::from(m.dy);
                }
            },
            EiEvent::Button(b) if b.state == ei::button::ButtonState::Press => {
                if let Some(c) = self.capture.as_ref() {
                    let at = c.cap.position();
                    self.end_capture(live, Some(at), "a button was pressed").await;
                }
            }
            EiEvent::ScrollDelta(_) | EiEvent::ScrollDiscrete(_) => {
                if let Some(c) = self.capture.as_ref() {
                    let at = c.cap.position();
                    self.end_capture(live, Some(at), "the wheel turned").await;
                }
            }
            EiEvent::KeyboardKey(k) => {
                let down = k.state == ei::keyboard::KeyState::Press;
                self.ctrl.key(k.key, down);
                if down && !logic::is_modifier(k.key) {
                    if let Some(c) = self.capture.as_ref() {
                        let at = c.cap.position();
                        self.end_capture(live, Some(at), "a key was typed").await;
                    }
                }
            }
            EiEvent::KeyboardModifiers(m) => self.ctrl.modifiers(m.depressed),
            EiEvent::Disconnected(d) => return Some(End::Again(format!("libei disconnected: {:?} {}", d.reason, d.explanation.unwrap_or_default()))),
            _ => {}
        }
        None
    }
}

/// How far the point is from a barrier's piece of edge.
fn distance(seg: &Segment, x: f64, y: f64) -> f64 {
    let (x1, y1, x2, y2) = seg.barrier();
    let (cx, cy) = (x.clamp(f64::from(x1.min(x2)), f64::from(x1.max(x2))), y.clamp(f64::from(y1.min(y2)), f64::from(y1.max(y2))));
    (x - cx).hypot(y - cy)
}
