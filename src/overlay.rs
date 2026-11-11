//! Animations drawn over the desktop: the countdown at a screen edge and the highlight
//! that shows where the cursor is. The effects themselves are the table of drawing
//! functions in overlay_render.rs, one per `EdgeAnimStyle` / `LocateStyle`; this file
//! puts them on the screen in click-through X11 windows, updated at about 60 Hz by one
//! thread of its own, with its own X connection. X libraries are loaded at run time;
//! without them, without a 32-bit visual or a compositor it logs once and draws nothing.
//!
//! On Wayland the countdown goes to a click-through layer-shell surface where the
//! compositor has wlr-layer-shell (overlay_wayland.rs: KDE, sway, Hyprland), and on GNOME
//! to an X11 window through XWayland when its coordinates line up with the desktop's
//! (edge_portal.rs checks that and allows it).
//!
//! The cursor highlight needs the cursor position, which Wayland gives an app only while
//! the pointer is over one of its surfaces. So for the moment the highlight shows,
//! flockswitch puts a transparent surface that takes the pointer over every screen: on
//! GNOME an override-redirect XWayland window (`platform::Catcher`; XQueryPointer then
//! reads the position, and goes on reading it as the pointer moves), on the layer-shell
//! desktops one overlay-layer surface per output (overlay_wayland.rs). The compositor
//! hands the resting pointer to a surface that appears under it. The first click while
//! the highlight shows lands on that surface and only ends the highlight; the keyboard
//! stays with the window that has it. An X11 session reads the cursor as always.

use crate::config::{parse_color, Edge, Edges, Locate, DEFAULT_EFFECT_COLOR};
use crate::edge::Rect;
use crate::locate_logic::{LocatePath, CATCH_MAX, CATCH_WAIT};
use crate::overlay_render as render;
use crate::overlay_wayland as wayland;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_micros(16_667);

enum Command {
    /// `display` None: wherever the platform likes (a preview on Wayland, where the
    /// cursor is unknown). `cursor` None: read it from the X server.
    EdgeBegin {
        display: Option<Rect>,
        cursor: Option<(f64, f64)>,
        edge: i32,
        style: i32,
        direction: i32,
        shrink: bool,
        elapsed: Duration,
        duration: Duration,
        preview: bool,
        color: [u8; 3],
        size: f64,
    },
    /// Where the cursor is now, for a countdown begun with a cursor.
    EdgeCursor((f64, f64)),
    EdgeEnd,
    Locate(i32, [u8; 3], f64),
}

/// Where effects are drawn outside an X11 session.
const OUTSIDE_X11_NOTHING: u8 = 0;
const OUTSIDE_X11_XWAYLAND: u8 = 1;
const OUTSIDE_X11_LAYER_SHELL: u8 = 2;
static OUTSIDE_X11: AtomicU8 = AtomicU8::new(OUTSIDE_X11_NOTHING);

/// In a Wayland session, draw through XWayland (GNOME, whose coordinates were checked
/// to line up). Takes effect for the first effect drawn.
pub fn use_xwayland() {
    OUTSIDE_X11.store(OUTSIDE_X11_XWAYLAND, Ordering::Relaxed);
}

/// In a Wayland session, draw on layer-shell surfaces.
pub fn use_layer_shell() {
    OUTSIDE_X11.store(OUTSIDE_X11_LAYER_SHELL, Ordering::Relaxed);
}

/// Whether X11 windows may be used for drawing in this (Wayland) session.
pub(crate) fn xwayland_allowed() -> bool {
    OUTSIDE_X11.load(Ordering::Relaxed) == OUTSIDE_X11_XWAYLAND
}

static COMMANDS: OnceLock<Sender<Command>> = OnceLock::new();

/// Says this process has an interface and may draw. Without it (`flockswitch run`, as on
/// macOS) no thread is started and nothing is drawn.
pub fn set_ready() {
    COMMANDS.get_or_init(|| {
        let (commands, received) = channel();
        let started = std::thread::Builder::new().name("overlay".into()).spawn(move || run(received));
        if let Err(e) = started {
            log::warn!("overlay: cannot start its thread: {e}");
        }
        commands
    });
}

/// A colour from the settings as red, green and blue.
fn components(text: &str) -> [u8; 3] {
    parse_color(text).or(parse_color(DEFAULT_EFFECT_COLOR)).unwrap_or([255, 204, 0])
}

fn send(command: Command) {
    if let Some(commands) = COMMANDS.get() {
        let _ = commands.send(command);
    }
}

/// Starts the countdown on `edge` of `display`, `elapsed` into the wait of `duration`.
/// The effect is the chosen one with its own settings, or for Random one drawn now.
pub fn edge_begin(edges: &Edges, edge: Edge, display: &Rect, elapsed: Duration, duration: Duration, preview: bool) {
    edge_begin_at(edges, edge, Some(display), None, elapsed, duration, preview);
}

/// `edge_begin` with the cursor given (Wayland: where the edge watcher saw it, in the
/// display's coordinates), or no display at all (a Wayland preview: the platform picks).
pub fn edge_begin_at(edges: &Edges, edge: Edge, display: Option<&Rect>, cursor: Option<(f64, f64)>, elapsed: Duration, duration: Duration, preview: bool) {
    let countdown = edges.countdown();
    let e = &countdown.settings;
    send(Command::EdgeBegin {
        display: display.copied(),
        cursor,
        edge: edge as i32,
        style: countdown.style as i32,
        direction: e.direction as i32,
        shrink: e.shrink,
        elapsed,
        duration,
        preview,
        color: components(&e.color),
        size: f64::from(e.size) / 100.0,
    });
}

/// The cursor moved while a countdown begun with `edge_begin_at(.., Some(cursor), ..)` runs.
pub fn edge_cursor(x: f64, y: f64) {
    send(Command::EdgeCursor((x, y)));
}

/// Takes the countdown away at once.
pub fn edge_end() {
    send(Command::EdgeEnd);
}

/// Shows where the cursor is, for a moment.
pub fn locate(locate: &Locate) {
    send(Command::Locate(locate.style.resolved() as i32, components(&locate.color), f64::from(locate.size) / 100.0));
}

struct EdgeEffect {
    layer: platform::Layer,
    /// Given by the edge watcher; None: read from the X server.
    cursor: Option<(f64, f64)>,
    edge: i32,
    style: i32,
    direction: i32,
    shrink: bool,
    preview: bool,
    started: Instant,
    duration: f64,
    color: [u8; 3],
    /// The chosen size (1 = 100%).
    size: f64,
}

struct LocateEffect {
    layer: platform::Layer,
    style: i32,
    full_screen: bool,
    started: Instant,
    color: [u8; 3],
    size: f64,
    /// GNOME on Wayland: the window over the screens that lets XWayland follow the
    /// pointer; it goes with the highlight.
    catcher: Option<platform::Catcher>,
}

/// GNOME on Wayland: the catcher is on the screen, waiting for the pointer to come over it.
struct Catching {
    catcher: platform::Catcher,
    since: Instant,
    style: i32,
    color: [u8; 3],
    size: f64,
}

/// What is being drawn, on the connection made when the first effect was asked for.
struct Effects {
    x: Option<platform::Connection>,
    /// The layer-shell painter, when that is where this session draws.
    wl: Option<wayland::Painter>,
    /// The drawing target (`OUTSIDE_X11`) a connection was last tried for: once per
    /// target is enough, and the reason is logged when it fails. A Wayland session may
    /// learn its target only later (edge_portal.rs, or the first highlight).
    tried: Option<u8>,
    edge: Option<EdgeEffect>,
    wl_edge: Option<wayland::EdgeEffect>,
    locate: Option<LocateEffect>,
    catching: Option<Catching>,
    wl_locate: Option<wayland::LocateEffect>,
}

impl Effects {
    fn open(&mut self) {
        let target = OUTSIDE_X11.load(Ordering::Relaxed);
        if self.tried == Some(target) {
            return;
        }
        self.tried = Some(target);
        if !crate::edge::available() && target == OUTSIDE_X11_LAYER_SHELL {
            if self.wl.is_none() {
                self.wl = wayland::Painter::open();
            }
        } else if self.x.is_none() {
            self.x = platform::Connection::open();
        }
    }

    fn connection(&mut self) -> Option<&platform::Connection> {
        self.open();
        self.x.as_ref().filter(|x| x.has_compositor())
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::EdgeBegin { display, cursor, edge, style, direction, shrink, elapsed, duration, preview, color, size } => {
                self.edge = None;
                if self.wl_edge.take().is_some() {
                    if let Some(wl) = &self.wl {
                        wl.flush();
                    }
                }
                if !render::edge_has_effect(style) {
                    return;
                }
                self.open();
                let look = wayland::Look { edge, style, direction, shrink, preview, color, size };
                if let Some(wl) = self.wl.as_mut() {
                    self.wl_edge = wl.begin_edge(display, cursor, look, elapsed, duration);
                    return;
                }
                self.edge = self.connection().and_then(|x| {
                    let display = display.unwrap_or_else(|| x.display_for_preview());
                    begin_edge(x, &display, cursor, edge, style, direction, shrink, elapsed, duration, preview, color, size)
                });
            }
            Command::EdgeCursor(at) => {
                if let Some(e) = self.edge.as_mut().filter(|e| e.cursor.is_some()) {
                    e.cursor = Some(at);
                }
                if let Some(e) = self.wl_edge.as_mut() {
                    e.cursor = Some(at);
                }
            }
            Command::EdgeEnd => {
                self.edge = None;
                if self.wl_edge.take().is_some() {
                    if let Some(wl) = &self.wl {
                        wl.flush();
                    }
                }
            }
            Command::Locate(style, color, size) => {
                self.end_locate();
                if !render::locate_exists(style) {
                    return;
                }
                if crate::edge::available() {
                    // An X11 session, as always.
                    self.locate = self.connection().and_then(|x| begin_locate(x, style, color, size, None, None));
                } else {
                    self.locate_on_wayland(style, color, size);
                }
            }
        }
    }

    fn end_locate(&mut self) {
        self.locate = None;
        self.catching = None;
        if self.wl_locate.take().is_some() {
            if let Some(wl) = &self.wl {
                wl.flush();
            }
        }
    }

    /// The highlight in a Wayland session: the pointer is caught for a moment by
    /// surfaces over every screen (see the top of this file).
    fn locate_on_wayland(&mut self, style: i32, color: [u8; 3], size: f64) {
        let (path, detail) = crate::locate_wayland::path();
        match path {
            LocatePath::XWayland(k) => {
                use_xwayland();
                let catcher = self.connection().and_then(|x| x.catcher());
                let Some(catcher) = catcher else {
                    log::info!("locate: no XWayland window could be put over the screens ({detail}); not shown");
                    return;
                };
                let (w, h) = catcher.size();
                log::info!(
                    "locate: through XWayland ({detail}; × {k}): a transparent {w}x{h} window over every screen takes the pointer; waiting up to {} ms for it",
                    CATCH_WAIT.as_millis()
                );
                self.catching = Some(Catching { catcher, since: Instant::now(), style, color, size });
            }
            LocatePath::LayerShell => {
                use_layer_shell();
                self.open();
                let look = wayland::LocateLook { style, color, size };
                match self.wl.as_mut() {
                    Some(wl) => self.wl_locate = wl.begin_locate(look),
                    None => log::info!("locate: cannot draw on layer-shell surfaces ({detail}); not shown"),
                }
            }
            // A Wayland session never chooses X11, but should it: as always.
            LocatePath::X11 => self.locate = self.connection().and_then(|x| begin_locate(x, style, color, size, None, None)),
            LocatePath::None(why) => log::info!("locate: not shown: {} ({detail})", why.explain()),
        }
    }

    fn animating(&self) -> bool {
        self.edge.is_some() || self.wl_edge.is_some() || self.locate.is_some() || self.catching.is_some() || self.wl_locate.is_some()
    }

    fn tick(&mut self) {
        if let Some(wl) = self.wl.as_mut() {
            if self.wl_edge.as_mut().is_some_and(|e| !wl.draw_edge(e)) {
                self.wl_edge = None;
                wl.flush();
            }
            if self.wl_locate.as_mut().is_some_and(|l| !wl.draw_locate(l)) {
                self.wl_locate = None;
                wl.flush();
            }
        }
        let Some(x) = self.x.as_ref() else { return };
        if self.edge.as_mut().is_some_and(|e| !draw_edge(x, e)) {
            self.edge = None;
        }
        if let Some(c) = self.catching.as_ref() {
            let seen = c.catcher.events(x);
            let waited = c.since.elapsed();
            if let Some(at) = seen.at {
                let c = self.catching.take().expect("checked above");
                // The pointer is over an XWayland window now, so the X server knows where.
                let (pos, how) = match x.cursor() {
                    Some(p) => (p, "XQueryPointer"),
                    None => (at, "the X event"),
                };
                log::info!(
                    "locate: position {pos:?} (XWayland pixels, drawn at scale {}) from {how}, {} ms after the window appeared",
                    x.scale(),
                    waited.as_millis()
                );
                self.locate = begin_locate(x, c.style, c.color, c.size, Some(pos), Some(c.catcher));
                if self.locate.is_none() {
                    log::info!("locate: the highlight could not be drawn; the window over the screens is gone");
                }
            } else if seen.pressed {
                log::info!("locate: a click came before the pointer was seen; nothing shown");
                self.catching = None;
            } else if waited >= CATCH_WAIT {
                log::info!(
                    "locate: the pointer did not come over the window within {} ms (the compositor did not hand it over); not shown",
                    CATCH_WAIT.as_millis()
                );
                self.catching = None;
            }
        }
        if self.locate.as_mut().is_some_and(|l| !draw_locate(x, l)) {
            self.locate = None;
        }
    }
}

fn run(commands: Receiver<Command>) {
    let mut effects = Effects { x: None, wl: None, tried: None, edge: None, wl_edge: None, locate: None, catching: None, wl_locate: None };
    let mut next_frame = Instant::now();
    loop {
        let received = if effects.animating() {
            commands.recv_timeout(next_frame.saturating_duration_since(Instant::now()))
        } else {
            commands.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match received {
            Ok(command) => effects.handle(command),
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        while let Ok(command) = commands.try_recv() {
            effects.handle(command);
        }
        if Instant::now() >= next_frame {
            effects.tick();
            next_frame = Instant::now() + FRAME;
        }
    }
}

/// Where the strip along `edge` of `display` goes, in desktop pixels.
fn strip_of(display: &Rect, edge: i32, depth: i32) -> (i32, i32, i32, i32) {
    let (x, y, w, h) = (display.x as i32, display.y as i32, display.w as i32, display.h as i32);
    match edge {
        render::EDGE_LEFT => (x, y, depth, h),
        render::EDGE_RIGHT => (x + w - depth, y, depth, h),
        render::EDGE_TOP => (x, y, w, depth),
        _ => (x, y + h - depth, w, depth),
    }
}

#[allow(clippy::too_many_arguments)]
fn begin_edge(
    x: &platform::Connection,
    display: &Rect,
    cursor: Option<(f64, f64)>,
    edge: i32,
    style: i32,
    direction: i32,
    shrink: bool,
    elapsed: Duration,
    duration: Duration,
    preview: bool,
    color: [u8; 3],
    size: f64,
) -> Option<EdgeEffect> {
    let depth = (render::STRIP_DEPTH * x.scale()).round() as i32;
    let (sx, sy, w, h) = strip_of(display, edge, depth);
    let layer = x.layer(sx, sy, w, h)?;
    let started = Instant::now().checked_sub(elapsed).unwrap_or_else(Instant::now);
    // A preview where the cursor is unknown (Wayland): the middle of the edge.
    let cursor = cursor.or_else(|| (!crate::edge::available()).then(|| (display.x + display.w / 2.0, display.y + display.h / 2.0)));
    Some(EdgeEffect { layer, cursor, edge, style, direction, shrink, preview, started, duration: duration.as_secs_f64().max(0.05), color, size })
}

/// Draws the next frame of the countdown. False when it has finished.
fn draw_edge(x: &platform::Connection, effect: &mut EdgeEffect) -> bool {
    let t = effect.started.elapsed().as_secs_f64();
    if effect.preview && t >= effect.duration {
        return false;
    }
    let (lx, ly) = effect.layer.position();
    let (mx, my) = match effect.cursor {
        Some((cx, cy)) => (cx.round() as i32, cy.round() as i32),
        None => x.cursor().unwrap_or((lx, ly)),
    };
    let draw = render::EdgeDraw {
        edge: effect.edge,
        style: effect.style,
        direction: effect.direction,
        shrink: effect.shrink,
        cursor: (f64::from(mx - lx), f64::from(my - ly)),
        scale: x.scale(),
        t,
        duration: effect.duration,
        color: effect.color,
        size: effect.size,
    };
    effect.layer.draw(x, |pixmap| render::draw_edge(pixmap, &draw));
    true
}

/// Starts the highlight at the cursor, read now or given as `at` (with the window that
/// lets XWayland follow the pointer, on GNOME's Wayland session).
fn begin_locate(
    x: &platform::Connection,
    style: i32,
    color: [u8; 3],
    size: f64,
    at: Option<(i32, i32)>,
    catcher: Option<platform::Catcher>,
) -> Option<LocateEffect> {
    let (mx, my) = match at {
        Some(p) => p,
        None => x.cursor()?,
    };
    // Only the spotlight darkens the whole display; the others follow the cursor.
    let full_screen = style == render::LOCATE_SPOTLIGHT;
    let (lx, ly, w, h) = if full_screen {
        x.display_at(mx, my)
    } else {
        let side = (render::LOCATE_SIDE * x.scale() * size).round() as i32;
        (mx - side / 2, my - side / 2, side, side)
    };
    let layer = x.layer(lx, ly, w, h)?;
    Some(LocateEffect { layer, style, full_screen, started: Instant::now(), color, size, catcher })
}

/// Draws the next frame of the highlight. False when it has finished.
fn draw_locate(x: &platform::Connection, effect: &mut LocateEffect) -> bool {
    let t = effect.started.elapsed().as_secs_f64();
    if let Some(c) = &effect.catcher {
        if t >= render::LOCATE_SECONDS.min(CATCH_MAX) {
            log::info!("locate: done after {} ms; the window over the screens is gone", (t * 1000.0).round());
            return false;
        }
        // The click lands on the window over the screens: it only ends the highlight.
        if c.events(x).pressed {
            log::info!("locate: a click ended it after {} ms (that click goes no further)", (t * 1000.0).round());
            return false;
        }
    }
    if t >= render::LOCATE_SECONDS {
        return false;
    }
    let cursor = x.cursor();
    let (lx, ly) = effect.layer.position();
    let (w, h) = effect.layer.size();
    let center = if effect.full_screen {
        let (mx, my) = cursor.unwrap_or((lx + w / 2, ly + h / 2));
        (f64::from(mx - lx), f64::from(my - ly))
    } else {
        if let Some((mx, my)) = cursor {
            effect.layer.move_to(x, mx - w / 2, my - h / 2);
        }
        (f64::from(w) / 2.0, f64::from(h) / 2.0)
    };
    let (style, scale, color, size) = (effect.style, x.scale(), effect.color, effect.size);
    effect.layer.draw(x, |pixmap| render::draw_locate(pixmap, style, center, scale, t, color, size));
    true
}

mod platform {
    use std::ffi::{c_int, c_ulong, CStr, CString};
    use std::mem::zeroed;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
    use std::sync::OnceLock;
    use tiny_skia::{Color, Pixmap};
    use x11_dl::xcursor::Xcursor;
    use x11_dl::xfixes::Xlib as XFixes;
    use x11_dl::xinerama::Xlib as Xinerama;
    use x11_dl::xlib::{
        AllocNone, ButtonPress, ButtonPressMask, CWBackPixel, CWBorderPixel, CWColormap, CWCursor, CWEventMask, CWOverrideRedirect,
        Display, EnterNotify, EnterWindowMask, InputOutput, LSBFirst, MotionNotify, PointerMotionMask, TrueColor, Visual, XErrorEvent,
        XEvent, XImage, XSetWindowAttributes, XVisualInfo, Xlib, ZPixmap, GC,
    };

    /// What the catcher listens to: the pointer coming over it or moving there, a click.
    const CATCHER_EVENTS: std::ffi::c_long = EnterWindowMask | PointerMotionMask | ButtonPressMask;

    /// ShapeInput from X11/extensions/shape.h: the part of a window that takes input.
    const SHAPE_INPUT: c_int = 2;

    type ErrorHandler = unsafe extern "C" fn(*mut Display, *mut XErrorEvent) -> c_int;

    static OWN_DISPLAY: AtomicPtr<Display> = AtomicPtr::new(null_mut());
    static PREVIOUS_HANDLER: OnceLock<Option<ErrorHandler>> = OnceLock::new();

    /// X errors on the overlay's own connection must not end the app, as Xlib's default
    /// handler would. Errors on other connections go to whoever handled them before.
    unsafe extern "C" fn on_error(display: *mut Display, event: *mut XErrorEvent) -> c_int {
        if display == OWN_DISPLAY.load(Ordering::Relaxed) {
            log::debug!("overlay: X error {}", unsafe { (*event).error_code });
            return 0;
        }
        match PREVIOUS_HANDLER.get() {
            Some(Some(previous)) => unsafe { previous(display, event) },
            _ => 0,
        }
    }

    /// The overlay's X connection: only the overlay thread uses it.
    pub struct Connection {
        xlib: Xlib,
        xfixes: XFixes,
        xinerama: Option<Xinerama>,
        display: *mut Display,
        root: u64,
        screen: c_int,
        visual: *mut Visual,
        colormap: u64,
        scale: f64,
        /// `_NET_WM_CM_S<screen>`: owned by the running compositor.
        compositor: u64,
        /// libXcursor, for the ordinary arrow over the catcher (None: the root window's).
        xcursor: Option<Xcursor>,
    }

    fn once() -> bool {
        static WARNED: AtomicBool = AtomicBool::new(false);
        !WARNED.swap(true, Ordering::Relaxed)
    }

    impl Connection {
        /// Connects, or logs once why nothing can be drawn.
        pub fn open() -> Option<Connection> {
            if !crate::edge::available() && !super::xwayland_allowed() {
                if once() {
                    log::info!("overlay: no way to draw found in this Wayland session yet (layer shell, or XWayland lined up with the outputs)");
                }
                return None;
            }
            let (Ok(xlib), Ok(xfixes)) = (Xlib::open(), XFixes::open()) else {
                log::warn!("overlay: needs libX11 and libXfixes; not drawn");
                return None;
            };
            // Safety: every pointer below is checked before it is used; the display
            // stays open for the life of this thread and nothing else uses it.
            unsafe {
                let display = (xlib.XOpenDisplay)(null());
                if display.is_null() {
                    log::warn!("overlay: cannot open the X display; not drawn");
                    return None;
                }
                OWN_DISPLAY.store(display, Ordering::Relaxed);
                PREVIOUS_HANDLER.get_or_init(|| (xlib.XSetErrorHandler)(Some(on_error)));
                let (mut event_base, mut error_base) = (0, 0);
                if (xfixes.XFixesQueryExtension)(display, &mut event_base, &mut error_base) == 0 {
                    log::warn!("overlay: the X server has no XFixes; not drawn");
                    return None;
                }
                let screen = (xlib.XDefaultScreen)(display);
                let root = (xlib.XRootWindow)(display, screen);
                let mut vinfo: XVisualInfo = zeroed();
                if (xlib.XMatchVisualInfo)(display, screen, 32, TrueColor, &mut vinfo) == 0 || vinfo.red_mask != 0xff_0000 || vinfo.blue_mask != 0xff {
                    log::warn!("overlay: no 32-bit ARGB visual on this display; not drawn");
                    return None;
                }
                let colormap = (xlib.XCreateColormap)(display, root, vinfo.visual, AllocNone);
                let name = CString::new(format!("_NET_WM_CM_S{screen}")).expect("no NUL in the atom name");
                let compositor = (xlib.XInternAtom)(display, name.as_ptr(), 0);
                let scale = dpi_scale(&xlib, display);
                let xinerama = Xinerama::open().ok();
                let xcursor = Xcursor::open().ok();
                Some(Connection { xlib, xfixes, xinerama, display, root, screen, visual: vinfo.visual, colormap, scale, compositor, xcursor })
            }
        }

        /// Whether a compositor is running now (it may be started or stopped at any
        /// time); without one the windows would come out as black rectangles.
        pub fn has_compositor(&self) -> bool {
            // XWayland: the Wayland compositor composites every X11 window.
            if super::xwayland_allowed() {
                return true;
            }
            // Safety: reading a selection owner on our own open display.
            let owner = unsafe { (self.xlib.XGetSelectionOwner)(self.display, self.compositor) };
            if owner == 0 && once() {
                log::warn!("overlay: no compositor running; not drawn");
            }
            owner != 0
        }

        pub fn scale(&self) -> f64 {
            self.scale
        }

        /// Where the cursor is now, in root-window pixels.
        pub fn cursor(&self) -> Option<(i32, i32)> {
            let (mut root, mut child) = (0u64, 0u64);
            let (mut rx, mut ry, mut wx, mut wy) = (0, 0, 0, 0);
            let mut mask = 0u32;
            // Safety: plain out-parameters on our own open display.
            let found = unsafe {
                (self.xlib.XQueryPointer)(self.display, self.root, &mut root, &mut child, &mut rx, &mut ry, &mut wx, &mut wy, &mut mask)
            };
            (found != 0).then_some((rx, ry))
        }

        /// The display at a point: x, y, width, height in root-window pixels.
        pub fn display_at(&self, px: i32, py: i32) -> (i32, i32, i32, i32) {
            let whole = unsafe {
                ((self.xlib.XDisplayWidth)(self.display, self.screen), (self.xlib.XDisplayHeight)(self.display, self.screen))
            };
            let Some(xin) = &self.xinerama else { return (0, 0, whole.0, whole.1) };
            let mut count = 0;
            // Safety: the returned array holds `count` screens and is freed below.
            unsafe {
                let info = (xin.XineramaQueryScreens)(self.display, &mut count);
                if info.is_null() {
                    return (0, 0, whole.0, whole.1);
                }
                let screens = std::slice::from_raw_parts(info, count.max(0) as usize);
                let hit = screens.iter().find(|s| {
                    let (x, y) = (i32::from(s.x_org), i32::from(s.y_org));
                    px >= x && py >= y && px < x + i32::from(s.width) && py < y + i32::from(s.height)
                });
                let found = hit.map(|s| (i32::from(s.x_org), i32::from(s.y_org), i32::from(s.width), i32::from(s.height)));
                (self.xlib.XFree)(info.cast());
                found.unwrap_or((0, 0, whole.0, whole.1))
            }
        }

        /// The display the cursor is on (or the first), for a preview that was given none.
        pub fn display_for_preview(&self) -> super::Rect {
            let (px, py) = self.cursor().unwrap_or((0, 0));
            let (x, y, w, h) = self.display_at(px, py);
            super::Rect { x: f64::from(x), y: f64::from(y), w: f64::from(w), h: f64::from(h) }
        }

        /// A click-through window, or None when one cannot be made.
        pub fn layer(&self, x: i32, y: i32, w: i32, h: i32) -> Option<Layer> {
            if w <= 0 || h <= 0 {
                return None;
            }
            let pixmap = Pixmap::new(w as u32, h as u32)?;
            let len = (w as usize) * (h as usize) * 4;
            // Safety: attributes and region are built here; the window is destroyed in
            // `Layer::destroy`, before this connection ends.
            unsafe {
                let mut attributes: XSetWindowAttributes = zeroed();
                attributes.override_redirect = 1;
                attributes.colormap = self.colormap;
                attributes.border_pixel = 0;
                attributes.background_pixel = 0;
                let mask = (CWOverrideRedirect | CWColormap | CWBorderPixel | CWBackPixel) as c_ulong;
                let window = (self.xlib.XCreateWindow)(
                    self.display, self.root, x, y, w as u32, h as u32, 0, 32, InputOutput as u32, self.visual, mask, &mut attributes,
                );
                if window == 0 {
                    log::warn!("overlay: cannot create a window");
                    return None;
                }
                // An empty input shape: every click goes to whatever is below.
                let region = (self.xfixes.XFixesCreateRegion)(self.display, null_mut(), 0);
                (self.xfixes.XFixesSetWindowShapeRegion)(self.display, window, SHAPE_INPUT, 0, 0, region);
                (self.xfixes.XFixesDestroyRegion)(self.display, region);
                let gc = (self.xlib.XCreateGC)(self.display, window, 0, null_mut());
                let mut layer = Layer { window, gc, pixmap, bgra: vec![0; len], image: zeroed(), x, y, mapped: false, gone: None };
                layer.gone = Some((self.xlib.XDestroyWindow, self.xlib.XFreeGC, self.xlib.XFlush, self.display));
                layer.init_image(self);
                Some(layer)
            }
        }
    }

    impl Connection {
        /// A transparent window over the whole X screen that takes the pointer, mapped
        /// at once (GNOME's Wayland session, through XWayland). XWayland knows where the
        /// pointer is only while it is over one of its windows; the compositor hands the
        /// resting pointer to a window that appears under it. Clicks land on it; keys do
        /// not (it never takes the keyboard).
        pub fn catcher(&self) -> Option<Catcher> {
            // Safety: plain queries and a window made here, destroyed in `Catcher::drop`
            // before this connection ends.
            unsafe {
                let (w, h) = ((self.xlib.XDisplayWidth)(self.display, self.screen), (self.xlib.XDisplayHeight)(self.display, self.screen));
                if w <= 0 || h <= 0 {
                    return None;
                }
                let mut attributes: XSetWindowAttributes = zeroed();
                attributes.override_redirect = 1;
                attributes.colormap = self.colormap;
                attributes.border_pixel = 0;
                attributes.background_pixel = 0;
                attributes.event_mask = CATCHER_EVENTS;
                let mut mask = CWOverrideRedirect | CWColormap | CWBorderPixel | CWBackPixel | CWEventMask;
                let cursor = self.arrow();
                if cursor != 0 {
                    attributes.cursor = cursor;
                    mask |= CWCursor;
                }
                let window = (self.xlib.XCreateWindow)(
                    self.display, self.root, 0, 0, w as u32, h as u32, 0, 32, InputOutput as u32, self.visual, mask as c_ulong, &mut attributes,
                );
                if window == 0 {
                    log::warn!("overlay: cannot create the window that reads the pointer");
                    if cursor != 0 {
                        (self.xlib.XFreeCursor)(self.display, cursor);
                    }
                    return None;
                }
                (self.xlib.XMapRaised)(self.display, window);
                (self.xlib.XFlush)(self.display);
                Some(Catcher {
                    window,
                    cursor,
                    size: (w, h),
                    check: self.xlib.XCheckWindowEvent,
                    gone: Some((self.xlib.XDestroyWindow, self.xlib.XFreeCursor, self.xlib.XFlush, self.display)),
                })
            }
        }

        /// The theme's ordinary arrow, or 0 (then the window shows the root window's).
        fn arrow(&self) -> c_ulong {
            let Some(xc) = &self.xcursor else { return 0 };
            for name in [c"default", c"left_ptr"] {
                // Safety: a cursor loaded on our own open display, freed with the catcher.
                let cursor = unsafe { (xc.XcursorLibraryLoadCursor)(self.display, name.as_ptr()) };
                if cursor != 0 {
                    return cursor;
                }
            }
            0
        }
    }

    /// What the catcher saw since it was last asked.
    #[derive(Debug, Default)]
    pub struct Seen {
        /// Where the pointer came over it or moved to last (root-window pixels).
        pub at: Option<(i32, i32)>,
        pub pressed: bool,
    }

    /// Calls that take the catcher down, kept with it so `Drop` needs no connection.
    type CatcherTeardown = (
        unsafe extern "C" fn(*mut Display, u64) -> c_int,
        unsafe extern "C" fn(*mut Display, c_ulong) -> c_int,
        unsafe extern "C" fn(*mut Display) -> c_int,
        *mut Display,
    );

    /// The window over the screens that takes the pointer for a moment (see `catcher`).
    pub struct Catcher {
        window: u64,
        cursor: c_ulong,
        size: (i32, i32),
        check: unsafe extern "C" fn(*mut Display, c_ulong, std::ffi::c_long, *mut XEvent) -> c_int,
        gone: Option<CatcherTeardown>,
    }

    impl Catcher {
        pub fn size(&self) -> (i32, i32) {
            self.size
        }

        /// Takes the window's events that arrived (only its own; the other windows of
        /// this connection select none).
        pub fn events(&self, x: &Connection) -> Seen {
            let mut seen = Seen::default();
            // Safety: our window on our own connection; the event is a plain out-value.
            unsafe {
                let mut event: XEvent = zeroed();
                while (self.check)(x.display, self.window, CATCHER_EVENTS, &mut event) != 0 {
                    let kind = event.get_type();
                    if kind == EnterNotify {
                        seen.at = Some((event.crossing.x_root, event.crossing.y_root));
                    } else if kind == MotionNotify {
                        seen.at = Some((event.motion.x_root, event.motion.y_root));
                    } else if kind == ButtonPress {
                        seen.pressed = true;
                    }
                }
            }
            seen
        }
    }

    impl Drop for Catcher {
        fn drop(&mut self) {
            if let Some((destroy, free_cursor, flush, display)) = self.gone.take() {
                // Safety: made by `Connection::catcher` on this connection, which outlives it.
                unsafe {
                    destroy(display, self.window);
                    if self.cursor != 0 {
                        free_cursor(display, self.cursor);
                    }
                    flush(display);
                }
            }
        }
    }

    /// Xft.dpi from the desktop's resources, as a scale; 1 without one.
    fn dpi_scale(xlib: &Xlib, display: *mut Display) -> f64 {
        // Safety: the string (if any) stays valid for as long as the display is open.
        let text = unsafe {
            let raw = (xlib.XResourceManagerString)(display);
            if raw.is_null() {
                return 1.0;
            }
            CStr::from_ptr(raw).to_string_lossy().into_owned()
        };
        let dpi = text.lines().find_map(|line| line.strip_prefix("Xft.dpi:")).and_then(|v| v.trim().parse::<f64>().ok());
        (dpi.unwrap_or(96.0) / 96.0).clamp(1.0, 4.0)
    }

    /// Calls that tear the window down, kept with it so `Drop` needs no connection.
    type Teardown = (
        unsafe extern "C" fn(*mut Display, u64) -> c_int,
        unsafe extern "C" fn(*mut Display, GC) -> c_int,
        unsafe extern "C" fn(*mut Display) -> c_int,
        *mut Display,
    );

    /// A click-through window with its drawing surface.
    pub struct Layer {
        window: u64,
        gc: GC,
        pixmap: Pixmap,
        /// The window's pixels: premultiplied BGRA as the 32-bit visual takes them.
        bgra: Vec<u8>,
        image: XImage,
        x: i32,
        y: i32,
        mapped: bool,
        gone: Option<Teardown>,
    }

    impl Layer {
        fn init_image(&mut self, x: &Connection) {
            let (w, h) = (self.pixmap.width(), self.pixmap.height());
            self.image.width = w as c_int;
            self.image.height = h as c_int;
            self.image.format = ZPixmap;
            self.image.data = self.bgra.as_mut_ptr().cast();
            self.image.byte_order = LSBFirst;
            self.image.bitmap_unit = 32;
            self.image.bitmap_bit_order = LSBFirst;
            self.image.bitmap_pad = 32;
            self.image.depth = 32;
            self.image.bytes_per_line = (w * 4) as c_int;
            self.image.bits_per_pixel = 32;
            self.image.red_mask = 0xff_0000;
            self.image.green_mask = 0xff00;
            self.image.blue_mask = 0xff;
            // Safety: the image is fully described above.
            unsafe { (x.xlib.XInitImage)(&mut self.image) };
        }

        pub fn position(&self) -> (i32, i32) {
            (self.x, self.y)
        }

        pub fn size(&self) -> (i32, i32) {
            (self.pixmap.width() as i32, self.pixmap.height() as i32)
        }

        pub fn move_to(&mut self, x: &Connection, to_x: i32, to_y: i32) {
            if (to_x, to_y) != (self.x, self.y) {
                (self.x, self.y) = (to_x, to_y);
                // Safety: our window on our own connection.
                unsafe { (x.xlib.XMoveWindow)(x.display, self.window, to_x, to_y) };
            }
        }

        /// Draws a frame and puts it on the screen.
        pub fn draw(&mut self, x: &Connection, paint: impl FnOnce(&mut Pixmap)) {
            self.pixmap.fill(Color::TRANSPARENT);
            paint(&mut self.pixmap);
            for (out, rgba) in self.bgra.chunks_exact_mut(4).zip(self.pixmap.data().chunks_exact(4)) {
                out.copy_from_slice(&[rgba[2], rgba[1], rgba[0], rgba[3]]);
            }
            // Safety: the image points at `bgra`, which is exactly as large as it says
            // and is not resized.
            unsafe {
                (x.xlib.XPutImage)(x.display, self.window, self.gc, &mut self.image, 0, 0, 0, 0, self.pixmap.width(), self.pixmap.height());
                if !self.mapped {
                    (x.xlib.XMapRaised)(x.display, self.window);
                    self.mapped = true;
                }
                (x.xlib.XFlush)(x.display);
            }
        }
    }

    impl Drop for Layer {
        fn drop(&mut self) {
            if let Some((destroy, free_gc, flush, display)) = self.gone.take() {
                // Safety: the window and GC were made by `Connection::layer` on this
                // connection, which outlives every layer.
                unsafe {
                    free_gc(display, self.gc);
                    destroy(display, self.window);
                    flush(display);
                }
            }
        }
    }
}
