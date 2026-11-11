//! The edge countdown on a Wayland compositor with wlr-layer-shell (KDE, sway,
//! Hyprland): a click-through surface on the overlay layer along the edge, drawn by
//! overlay_render.rs into a shared-memory buffer. Runs on the overlay thread with its
//! own connection.

use crate::edge::Rect;
use crate::overlay_render as render;
use crate::wl::{Anchor, Display, Layer, Wl};
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
        match Wl::connect(false) {
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

fn close(a: i32, b: f64) -> bool {
    (f64::from(a) - b).abs() <= 1.0
}
