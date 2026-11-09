//! Animations drawn over the desktop: the countdown at a screen edge and the highlight
//! that shows where the cursor is. The effects themselves are the table of drawing
//! functions in overlay_render.rs, one per `EdgeAnimStyle` / `LocateStyle`; this file
//! puts them on the screen in click-through X11 windows, updated at about 60 Hz by one
//! thread of its own, with its own X connection. X libraries are loaded at run time;
//! without them, without a 32-bit visual or a compositor it logs once and draws nothing.
//! Wayland lets no app place such windows, so nothing is drawn there either.

use crate::config::{Edge, Edges, LocateStyle};
use crate::edge::Rect;
use crate::overlay_render as render;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_micros(16_667);

enum Command {
    EdgeBegin { display: Rect, edge: i32, style: i32, direction: i32, shrink: bool, elapsed: Duration, duration: Duration, preview: bool },
    EdgeEnd,
    Locate(i32),
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

fn send(command: Command) {
    if let Some(commands) = COMMANDS.get() {
        let _ = commands.send(command);
    }
}

/// Starts the countdown on `edge` of `display`, `elapsed` into the wait of `duration`.
pub fn edge_begin(edges: &Edges, edge: Edge, display: &Rect, elapsed: Duration, duration: Duration, preview: bool) {
    send(Command::EdgeBegin {
        display: *display,
        edge: edge as i32,
        style: edges.anim_style as i32,
        direction: edges.anim_direction as i32,
        shrink: edges.anim_shrink,
        elapsed,
        duration,
        preview,
    });
}

/// Takes the countdown away at once.
pub fn edge_end() {
    send(Command::EdgeEnd);
}

/// Shows where the cursor is, for a moment.
pub fn locate(style: LocateStyle) {
    send(Command::Locate(style as i32));
}

struct EdgeEffect {
    layer: platform::Layer,
    edge: i32,
    style: i32,
    direction: i32,
    shrink: bool,
    preview: bool,
    started: Instant,
    duration: f64,
}

struct LocateEffect {
    layer: platform::Layer,
    style: i32,
    full_screen: bool,
    started: Instant,
}

/// What is being drawn, on the connection made when the first effect was asked for.
struct Effects {
    x: Option<platform::Connection>,
    /// Trying to connect once is enough; the reason is logged when it fails.
    tried: bool,
    edge: Option<EdgeEffect>,
    locate: Option<LocateEffect>,
}

impl Effects {
    fn connection(&mut self) -> Option<&platform::Connection> {
        if !self.tried {
            self.tried = true;
            self.x = platform::Connection::open();
        }
        self.x.as_ref().filter(|x| x.has_compositor())
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::EdgeBegin { display, edge, style, direction, shrink, elapsed, duration, preview } => {
                self.edge = None;
                if render::edge_has_effect(style) {
                    self.edge = self.connection().and_then(|x| begin_edge(x, &display, edge, style, direction, shrink, elapsed, duration, preview));
                }
            }
            Command::EdgeEnd => self.edge = None,
            Command::Locate(style) => {
                self.locate = None;
                if render::locate_exists(style) {
                    self.locate = self.connection().and_then(|x| begin_locate(x, style));
                }
            }
        }
    }

    fn animating(&self) -> bool {
        self.edge.is_some() || self.locate.is_some()
    }

    fn tick(&mut self) {
        let Some(x) = self.x.as_ref() else { return };
        if self.edge.as_mut().is_some_and(|e| !draw_edge(x, e)) {
            self.edge = None;
        }
        if self.locate.as_mut().is_some_and(|l| !draw_locate(x, l)) {
            self.locate = None;
        }
    }
}

fn run(commands: Receiver<Command>) {
    let mut effects = Effects { x: None, tried: false, edge: None, locate: None };
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
fn begin_edge(x: &platform::Connection, display: &Rect, edge: i32, style: i32, direction: i32, shrink: bool, elapsed: Duration, duration: Duration, preview: bool) -> Option<EdgeEffect> {
    let depth = (render::STRIP_DEPTH * x.scale()).round() as i32;
    let (sx, sy, w, h) = strip_of(display, edge, depth);
    let layer = x.layer(sx, sy, w, h)?;
    let started = Instant::now().checked_sub(elapsed).unwrap_or_else(Instant::now);
    Some(EdgeEffect { layer, edge, style, direction, shrink, preview, started, duration: duration.as_secs_f64().max(0.05) })
}

/// Draws the next frame of the countdown. False when it has finished.
fn draw_edge(x: &platform::Connection, effect: &mut EdgeEffect) -> bool {
    let t = effect.started.elapsed().as_secs_f64();
    if effect.preview && t >= effect.duration {
        return false;
    }
    let (lx, ly) = effect.layer.position();
    let (mx, my) = x.cursor().unwrap_or((lx, ly));
    let draw = render::EdgeDraw {
        edge: effect.edge,
        style: effect.style,
        direction: effect.direction,
        shrink: effect.shrink,
        cursor: (f64::from(mx - lx), f64::from(my - ly)),
        scale: x.scale(),
        t,
        duration: effect.duration,
    };
    effect.layer.draw(x, |pixmap| render::draw_edge(pixmap, &draw));
    true
}

fn begin_locate(x: &platform::Connection, style: i32) -> Option<LocateEffect> {
    let (mx, my) = x.cursor()?;
    // Only the spotlight darkens the whole display; the others follow the cursor.
    let full_screen = style == render::LOCATE_SPOTLIGHT;
    let (lx, ly, w, h) = if full_screen {
        x.display_at(mx, my)
    } else {
        let side = (render::LOCATE_SIDE * x.scale()).round() as i32;
        (mx - side / 2, my - side / 2, side, side)
    };
    let layer = x.layer(lx, ly, w, h)?;
    Some(LocateEffect { layer, style, full_screen, started: Instant::now() })
}

/// Draws the next frame of the highlight. False when it has finished.
fn draw_locate(x: &platform::Connection, effect: &mut LocateEffect) -> bool {
    let t = effect.started.elapsed().as_secs_f64();
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
    let (style, scale) = (effect.style, x.scale());
    effect.layer.draw(x, |pixmap| render::draw_locate(pixmap, style, center, scale, t));
    true
}

mod platform {
    use std::ffi::{c_int, c_ulong, CStr, CString};
    use std::mem::zeroed;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
    use std::sync::OnceLock;
    use tiny_skia::{Color, Pixmap};
    use x11_dl::xfixes::Xlib as XFixes;
    use x11_dl::xinerama::Xlib as Xinerama;
    use x11_dl::xlib::{
        AllocNone, CWBackPixel, CWBorderPixel, CWColormap, CWOverrideRedirect, Display, InputOutput, LSBFirst, TrueColor,
        Visual, XErrorEvent, XImage, XSetWindowAttributes, XVisualInfo, Xlib, ZPixmap, GC,
    };

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
    }

    fn once() -> bool {
        static WARNED: AtomicBool = AtomicBool::new(false);
        !WARNED.swap(true, Ordering::Relaxed)
    }

    impl Connection {
        /// Connects, or logs once why nothing can be drawn.
        pub fn open() -> Option<Connection> {
            if !crate::edge::available() {
                if once() {
                    log::info!("overlay: not drawn in this session (X11 only)");
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
                Some(Connection { xlib, xfixes, xinerama, display, root, screen, visual: vinfo.visual, colormap, scale, compositor })
            }
        }

        /// Whether a compositor is running now (it may be started or stopped at any
        /// time); without one the windows would come out as black rectangles.
        pub fn has_compositor(&self) -> bool {
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
