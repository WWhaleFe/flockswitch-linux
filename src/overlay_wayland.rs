//! The edge countdown on a Wayland compositor with wlr-layer-shell (KDE, sway,
//! Hyprland): a click-through surface on the overlay layer along the edge, drawn by
//! overlay_render.rs into a shared-memory buffer. Runs on the overlay thread with its
//! own connection.
//!
//! Finding the cursor there: no app may read the pointer, but a surface learns where it
//! is while it is over it. For the moment the highlight shows, a transparent surface on
//! the overlay layer covers each output and takes the pointer (wl_pointer enter and
//! motion give the position), and the highlight is drawn on a click-through surface of
//! its own that follows it. The first click lands on the covering surface and only ends
//! the highlight; the keyboard stays with the window that has it.

use crate::edge::Rect;
use crate::locate_logic::{self as logic, CATCH_MAX, CATCH_WAIT};
use crate::overlay_render as render;
use crate::wl::{Anchor, Display, Layer, PointerEvent, Wl};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tiny_skia::{Color, Pixmap};

/// How the countdown looks, as overlay.rs has it from the settings.
#[derive(Debug, Clone, Copy)]
pub struct Look {
    pub edge: i32,
    pub style: i32,
    pub direction: i32,
    pub shrink: bool,
    pub preview: bool,
    pub color: [u8; 3],
    pub size: f64,
}

pub struct Painter {
    wl: Wl,
}

/// How the cursor highlight looks, as overlay.rs has it from the settings.
#[derive(Debug, Clone, Copy)]
pub struct LocateLook {
    pub style: i32,
    pub color: [u8; 3],
    pub size: f64,
}

/// A transparent surface over one output that takes the pointer while the highlight shows.
struct Catcher {
    layer: Layer,
    display: Display,
    drawn: bool,
}

/// The click-through surface the highlight is drawn on: a square around the cursor, or
/// the whole output for the spotlight.
struct Shown {
    layer: Layer,
    display: Display,
    full: bool,
    side: i32,
    /// Its margins from the output's left and top edges (a square).
    margin: (i32, i32),
    logged: bool,
}

pub struct LocateEffect {
    look: LocateLook,
    catchers: Vec<Catcher>,
    asked: Instant,
    /// When every covering surface was on the screen: the wait for the pointer starts.
    covered: Option<Instant>,
    started: Option<Instant>,
    /// The covering surface the pointer is over, and where it is (desktop logical pixels).
    over: Option<usize>,
    cursor: Option<(f64, f64)>,
    shown: Option<Shown>,
    pixmap: Option<Pixmap>,
}

pub struct EdgeEffect {
    layer: Layer,
    display: Display,
    /// The surface's top-left corner in the desktop (logical pixels).
    origin: (f64, f64),
    look: Look,
    started: Instant,
    duration: f64,
    /// Where the cursor is (desktop logical pixels); None: the middle of the edge.
    pub cursor: Option<(f64, f64)>,
    pixmap: Option<Pixmap>,
    logged: bool,
}

fn once(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::Relaxed)
}

impl Painter {
    pub fn open() -> Option<Painter> {
        static WARNED: AtomicBool = AtomicBool::new(false);
        // With the pointer: the surfaces that find the cursor need it (the countdown's
        // take no input, so they never hear from it).
        match Wl::connect(true) {
            Ok(wl) if wl.has_layer_shell() => {
                log::info!("overlay: drawing on wlr-layer-shell surfaces ({} outputs)", wl.displays().len());
                Some(Painter { wl })
            }
            Ok(_) => {
                if once(&WARNED) {
                    log::info!("overlay: the compositor has no wlr-layer-shell; the countdown is not drawn");
                }
                None
            }
            Err(e) => {
                if once(&WARNED) {
                    log::warn!("overlay: cannot connect to Wayland ({e}); the countdown is not drawn");
                }
                None
            }
        }
    }

    /// Sends what is queued: a countdown taken away goes from the screen at once, even
    /// when nothing is drawn after it.
    pub fn flush(&self) {
        self.wl.flush();
    }

    /// Puts a surface along `look.edge` of the output that is `display` (or the first
    /// output, for a preview).
    pub fn begin_edge(&mut self, display: Option<Rect>, cursor: Option<(f64, f64)>, look: Look, elapsed: Duration, duration: Duration) -> Option<EdgeEffect> {
        if let Err(e) = self.wl.dispatch(Duration::ZERO) {
            log::warn!("overlay: Wayland connection lost ({e})");
            return None;
        }
        let displays = self.wl.displays();
        let chosen = match display {
            Some(want) => displays.iter().find(|d| close(d.rect.x, want.x) && close(d.rect.y, want.y) && close(d.rect.w, want.w) && close(d.rect.h, want.h)).or_else(|| {
                let (cx, cy) = (want.x + want.w / 2.0, want.y + want.h / 2.0);
                displays.iter().find(|d| {
                    let r = d.rect;
                    cx >= f64::from(r.x) && cx < f64::from(r.x + r.w) && cy >= f64::from(r.y) && cy < f64::from(r.y + r.h)
                })
            }),
            None => displays.first(),
        };
        let Some(chosen) = chosen.cloned() else {
            log::info!("overlay: no output for the countdown at {display:?} (outputs: {:?})", displays.iter().map(|d| d.rect).collect::<Vec<_>>());
            return None;
        };
        let r = chosen.rect;
        let depth = (render::STRIP_DEPTH.round() as i32).min(if matches!(look.edge, render::EDGE_LEFT | render::EDGE_RIGHT) { r.w } else { r.h }).max(1);
        let (anchor, size, origin) = match look.edge {
            render::EDGE_LEFT => (Anchor::Left | Anchor::Top, (depth, r.h), (r.x, r.y)),
            render::EDGE_RIGHT => (Anchor::Right | Anchor::Top, (depth, r.h), (r.x + r.w - depth, r.y)),
            render::EDGE_TOP => (Anchor::Top | Anchor::Left, (r.w, depth), (r.x, r.y)),
            _ => (Anchor::Bottom | Anchor::Left, (r.w, depth), (r.x, r.y + r.h - depth)),
        };
        let layer = self.wl.layer(chosen.global, "flockswitch-countdown", anchor, (0, 0, 0, 0), (size.0 as u32, size.1 as u32), false)?;
        let started = Instant::now().checked_sub(elapsed).unwrap_or_else(Instant::now);
        Some(EdgeEffect {
            layer,
            display: chosen,
            origin: (f64::from(origin.0), f64::from(origin.1)),
            look,
            started,
            duration: duration.as_secs_f64().max(0.05),
            cursor,
            pixmap: None,
            logged: false,
        })
    }

    /// Draws the next frame. False when the countdown has finished (a preview) or the
    /// compositor took the surface away.
    pub fn draw_edge(&mut self, e: &mut EdgeEffect) -> bool {
        if let Err(err) = self.wl.dispatch(Duration::ZERO) {
            log::warn!("overlay: Wayland connection lost ({err})");
            return false;
        }
        let t = e.started.elapsed().as_secs_f64();
        if (e.look.preview && t >= e.duration) || e.layer.closed() {
            return false;
        }
        let r = e.display.rect;
        let (cx, cy) = e.cursor.unwrap_or((f64::from(r.x) + f64::from(r.w) / 2.0, f64::from(r.y) + f64::from(r.h) / 2.0));
        let scale = e.display.scale.max(1);
        let s = f64::from(scale);
        let (look, origin, duration) = (e.look, e.origin, e.duration);
        let pixmap_slot = &mut e.pixmap;
        let drawn = e.layer.draw(&self.wl, scale, |bytes, bw, bh| {
            let pixmap = match pixmap_slot {
                Some(p) if p.width() == bw as u32 && p.height() == bh as u32 => p,
                _ => match Pixmap::new(bw as u32, bh as u32) {
                    Some(p) => pixmap_slot.insert(p),
                    None => return,
                },
            };
            pixmap.fill(Color::TRANSPARENT);
            let draw = render::EdgeDraw {
                edge: look.edge,
                style: look.style,
                direction: look.direction,
                shrink: look.shrink,
                cursor: ((cx - origin.0) * s, (cy - origin.1) * s),
                scale: s,
                t,
                duration,
                color: look.color,
                size: look.size,
            };
            render::draw_edge(pixmap, &draw);
            for (out, rgba) in bytes.chunks_exact_mut(4).zip(pixmap.data().chunks_exact(4)) {
                out.copy_from_slice(&[rgba[2], rgba[1], rgba[0], rgba[3]]);
            }
        });
        if drawn && !e.logged {
            e.logged = true;
            log::info!("overlay: countdown on {} at {:?}, scale {scale}", if e.display.name.is_empty() { "an output" } else { &e.display.name }, e.layer.configured());
        }
        true
    }
}

impl Painter {
    /// Puts a transparent surface that takes the pointer over every output; the
    /// highlight starts once the pointer is seen over one (`draw_locate`).
    pub fn begin_locate(&mut self, look: LocateLook) -> Option<LocateEffect> {
        if let Err(e) = self.wl.dispatch(Duration::ZERO) {
            log::warn!("overlay: Wayland connection lost ({e})");
            return None;
        }
        self.wl.state.pointer_events.clear();
        let displays = self.wl.displays();
        let mut catchers = Vec::new();
        for d in &displays {
            let all = Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right;
            if let Some(layer) = self.wl.layer(d.global, "flockswitch-locate", all, (0, 0, 0, 0), (0, 0), true) {
                catchers.push(Catcher { layer, display: d.clone(), drawn: false });
            }
        }
        if catchers.is_empty() {
            log::info!("locate: no output to put a surface on (outputs: {:?}); not shown", displays.iter().map(|d| d.rect).collect::<Vec<_>>());
            return None;
        }
        let names: Vec<String> = catchers.iter().map(|c| format!("{} {:?} scale {}", output_name(&c.display), c.display.rect, c.display.scale)).collect();
        log::info!(
            "locate: through layer shell: a transparent surface over each output takes the pointer ({}); waiting up to {} ms for it",
            names.join("; "),
            CATCH_WAIT.as_millis()
        );
        Some(LocateEffect {
            look,
            catchers,
            asked: Instant::now(),
            covered: None,
            started: None,
            over: None,
            cursor: None,
            shown: None,
            pixmap: None,
        })
    }

    /// Follows the pointer and draws the next frame of the highlight. False when it has
    /// finished (or the pointer never came), and everything is to be taken away.
    pub fn draw_locate(&mut self, e: &mut LocateEffect) -> bool {
        if let Err(err) = self.wl.dispatch(Duration::ZERO) {
            log::warn!("overlay: Wayland connection lost ({err})");
            return false;
        }
        let now = Instant::now();
        if e.catchers.iter().any(|c| c.layer.closed()) {
            log::info!("locate: the compositor took a surface away; ended");
            return false;
        }
        for c in e.catchers.iter_mut().filter(|c| !c.drawn && c.layer.configured().is_some()) {
            // A transparent frame: the surface is on the screen (and takes the pointer)
            // only with one. One buffer pixel per logical pixel is enough for nothing.
            c.drawn = c.layer.draw(&self.wl, 1, |bytes, _, _| bytes.fill(0));
        }
        if e.covered.is_none() && e.catchers.iter().all(|c| c.drawn) {
            e.covered = Some(now);
        }
        for event in std::mem::take(&mut self.wl.state.pointer_events) {
            match event {
                PointerEvent::Enter { surface, x, y } => {
                    e.over = e.catchers.iter().position(|c| c.layer.id() == surface);
                    if let Some(i) = e.over {
                        let d = &e.catchers[i].display;
                        let at = logic::on_desktop((d.rect.x, d.rect.y), (x, y));
                        if e.cursor.is_none() {
                            let since = e.covered.unwrap_or(e.asked);
                            log::info!(
                                "locate: position {at:?} (desktop, logical) from wl_pointer.enter on {} at ({x:.1}, {y:.1}), {} ms after the surfaces appeared",
                                output_name(d),
                                now.duration_since(since).as_millis()
                            );
                        }
                        e.cursor = Some(at);
                    }
                }
                PointerEvent::Motion { x, y } => {
                    if let Some(i) = e.over {
                        let r = e.catchers[i].display.rect;
                        e.cursor = Some(logic::on_desktop((r.x, r.y), (x, y)));
                    }
                }
                PointerEvent::Leave { surface } => {
                    if e.over.is_some_and(|i| e.catchers[i].layer.id() == surface) {
                        e.over = None;
                    }
                }
                PointerEvent::Press => {
                    let after = e.started.map(|s| now.duration_since(s).as_millis()).unwrap_or(0);
                    log::info!("locate: a click ended it after {after} ms (that click goes no further)");
                    return false;
                }
            }
        }
        let Some(cursor) = e.cursor else {
            let waited = e.covered.map(|t| now.duration_since(t));
            if waited.is_some_and(|w| w >= CATCH_WAIT) || now.duration_since(e.asked) >= Duration::from_secs(1) {
                log::info!(
                    "locate: the pointer did not come over the surfaces within {} ms (the compositor did not hand it over); not shown",
                    CATCH_WAIT.as_millis()
                );
                return false;
            }
            return true;
        };
        let started = *e.started.get_or_insert(now);
        let t = now.duration_since(started).as_secs_f64();
        if t >= render::LOCATE_SECONDS.min(CATCH_MAX) {
            log::info!("locate: done after {} ms; the surfaces are gone", (t * 1000.0).round());
            return false;
        }
        // The output the pointer is on.
        let display = e.over.map(|i| e.catchers[i].display.clone()).or_else(|| {
            e.catchers
                .iter()
                .find(|c| {
                    let r = c.display.rect;
                    cursor.0 >= f64::from(r.x) && cursor.0 < f64::from(r.x + r.w) && cursor.1 >= f64::from(r.y) && cursor.1 < f64::from(r.y + r.h)
                })
                .map(|c| c.display.clone())
        });
        let Some(display) = display else { return true };
        let full = e.look.style == render::LOCATE_SPOTLIGHT;
        if e.shown.as_ref().is_none_or(|s| s.display.global != display.global) {
            e.shown = None;
            let r = display.rect;
            let side = (render::LOCATE_SIDE * e.look.size).round() as i32;
            let (left, top, side, _) = logic::place_square((r.x, r.y, r.w, r.h), cursor, side);
            let layer = if full {
                let all = Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right;
                self.wl.layer(display.global, "flockswitch-highlight", all, (0, 0, 0, 0), (0, 0), false)
            } else {
                self.wl.layer(display.global, "flockswitch-highlight", Anchor::Top | Anchor::Left, (top, 0, 0, left), (side as u32, side as u32), false)
            };
            let Some(layer) = layer else { return true };
            e.shown = Some(Shown { layer, display, full, side, margin: (left, top), logged: false });
        }
        let Some(shown) = e.shown.as_mut() else { return true };
        let r = shown.display.rect;
        let center = if shown.full {
            (cursor.0 - f64::from(r.x), cursor.1 - f64::from(r.y))
        } else {
            let (left, top, _, center) = logic::place_square((r.x, r.y, r.w, r.h), cursor, shown.side);
            if (left, top) != shown.margin {
                shown.layer.set_margin(top, 0, 0, left);
                shown.margin = (left, top);
            }
            center
        };
        let scale = shown.display.scale.max(1);
        let s = f64::from(scale);
        let look = e.look;
        let pixmap_slot = &mut e.pixmap;
        let drawn = shown.layer.draw(&self.wl, scale, |bytes, bw, bh| {
            let pixmap = match pixmap_slot {
                Some(p) if p.width() == bw as u32 && p.height() == bh as u32 => p,
                _ => match Pixmap::new(bw as u32, bh as u32) {
                    Some(p) => pixmap_slot.insert(p),
                    None => return,
                },
            };
            pixmap.fill(Color::TRANSPARENT);
            render::draw_locate(pixmap, look.style, (center.0 * s, center.1 * s), s, t, look.color, look.size);
            for (out, rgba) in bytes.chunks_exact_mut(4).zip(pixmap.data().chunks_exact(4)) {
                out.copy_from_slice(&[rgba[2], rgba[1], rgba[0], rgba[3]]);
            }
        });
        if drawn && !shown.logged {
            shown.logged = true;
            let what = if shown.full { "the whole output".to_string() } else { format!("a {} px square", shown.side) };
            log::info!("locate: highlight on {} ({what}), scale {scale}", output_name(&shown.display));
        }
        true
    }
}

fn output_name(d: &Display) -> &str {
    if d.name.is_empty() {
        "an output"
    } else {
        &d.name
    }
}

fn close(a: i32, b: f64) -> bool {
    (f64::from(a) - b).abs() <= 1.0
}
