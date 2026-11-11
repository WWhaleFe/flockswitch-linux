//! Switching at the screen edge on a Wayland compositor with wlr-layer-shell (KDE
//! Plasma, sway, Hyprland, other wlroots desktops). No app may read the cursor there, but
//! a surface learns where the pointer is while it is over it: a transparent strip one
//! pixel wide lies on the overlay layer along each piece of an outer edge that has a
//! channel (corners and edges shared with another display left out, as on X11), and
//! the pointer resting on it is the cursor resting at the edge. The strip takes clicks
//! on that outermost row of pixels; nothing else changes, no permission is asked.
//!
//! With "only while Ctrl is held" a strip takes the keyboard while the pointer rests on
//! it (Wayland tells no app about keys otherwise), for at most `logic::CTRL_GRACE`
//! unless Ctrl is down, and gives it back the moment a key other than a modifier is
//! pressed or the pointer leaves.

use crate::edge::Rect;
use crate::edge_logic::{self as logic, CtrlKeys, Dwell, Segment, Side, Zone};
use crate::edge_wayland::{set_status, Watch, POLL};
use crate::overlay;
use crate::shared::Shared;
use crate::wl::{Anchor, Display, KeyEvent, Layer, PointerEvent, Wl};
use std::sync::Arc;
use std::time::Instant;

struct Strip {
    seg: Segment,
    display: Display,
    layer: Layer,
    /// The strip's top-left pixel in the desktop.
    origin: (f64, f64),
    drawn: bool,
}

/// The pointer on a strip: which one, and where in the desktop.
#[derive(Debug, Clone, Copy)]
struct OnStrip {
    strip: usize,
    at: (f64, f64),
}

fn rect(z: Zone) -> Rect {
    Rect { x: f64::from(z.x), y: f64::from(z.y), w: f64::from(z.w), h: f64::from(z.h) }
}

pub fn run(shared: Arc<Shared>) {
    let mut wl = match Wl::connect(true, true) {
        Ok(wl) if wl.has_layer_shell() => wl,
        Ok(_) => {
            log::warn!("layer-shell: the compositor has no wlr-layer-shell after all");
            set_status(&shared, "none", "no-support");
            return;
        }
        Err(e) => {
            log::warn!("layer-shell: cannot connect to Wayland: {e}");
            set_status(&shared, "none", "no-wayland");
            return;
        }
    };
    log::info!("layer-shell: connected (cursor-shape {})", if wl.has_cursor_shape() { "yes" } else { "no: the compositor picks the cursor over the strips" });
    overlay::use_layer_shell();
    let mut watch = Watch::new(shared.clone());
    let mut dwell = Dwell::new();
    let mut strips: Vec<Strip> = Vec::new();
    let mut built: Option<(Vec<Segment>, [bool; 4])> = None;
    let mut on: Option<OnStrip> = None;
    let mut ctrl = CtrlKeys::default();
    // The strip holding the keyboard (for the Ctrl check), and since when.
    let mut keyboard: Option<(usize, Instant)> = None;
    let mut told_ctrl = false;
    let mut out = Vec::new();

    while !shared.stopped() {
        if let Err(e) = wl.dispatch(POLL) {
            log::warn!("layer-shell: Wayland connection lost ({e}); edge switching stops");
            set_status(&shared, "none", "no-wayland");
            break;
        }
        let changed = watch.poll();
        if changed || wl.state.outputs_changed {
            wl.state.outputs_changed = false;
            let displays = wl.displays();
            let zones: Vec<Zone> = displays.iter().map(|d| d.rect).collect();
            // Paused from the menu: no strips (they would only take clicks).
            let sides = if watch.active() { watch.sides() } else { [false; 4] };
            let segments = logic::outer_segments(&zones, sides, watch.margin(), false);
            if built.as_ref() != Some(&(segments.clone(), sides)) {
                strips.clear();
                on = None;
                keyboard = None;
                if !segments.is_empty() || built.is_some() {
                    log::info!("layer-shell: outputs {:?}", displays.iter().map(|d| (d.name.as_str(), d.rect, d.scale)).collect::<Vec<_>>());
                }
                for seg in &segments {
                    if let Some(strip) = make_strip(&mut wl, &displays, seg) {
                        strips.push(strip);
                    }
                }
                built = Some((segments, sides));
            }
        }
        for strip in strips.iter_mut().filter(|s| !s.drawn) {
            if let Some(size) = strip.layer.configured() {
                // A transparent buffer: the surface exists (and takes the pointer) only
                // with one attached.
                strip.drawn = strip.layer.draw(&wl, strip.display.scale, |bytes, _, _| bytes.fill(0));
                if strip.drawn {
                    log::info!("layer-shell: {} strip on {} configured {}x{}", strip.seg.side.name(), strip.display.name, size.0, size.1);
                }
            }
        }
        if let Some(i) = strips.iter().position(|s| s.layer.closed()) {
            log::info!("layer-shell: the compositor closed the {} strip; made again with the next output change", strips[i].seg.side.name());
            strips.clear();
            built = None;
            on = None;
            keyboard = None;
        }

        let before = on;
        for event in std::mem::take(&mut wl.state.pointer_events) {
            match event {
                PointerEvent::Enter { surface, x, y } => {
                    on = strips.iter().position(|s| s.layer.id() == surface).map(|i| OnStrip { strip: i, at: (strips[i].origin.0 + x, strips[i].origin.1 + y) });
                    if let Some(o) = on {
                        log::info!("layer-shell: pointer on the {} strip at {:?}", strips[o.strip].seg.side.name(), o.at);
                    }
                }
                PointerEvent::Motion { x, y } => {
                    if let Some(o) = on.as_mut() {
                        let origin = strips[o.strip].origin;
                        o.at = (origin.0 + x, origin.1 + y);
                    }
                }
                PointerEvent::Leave { surface } => {
                    if on.is_some_and(|o| strips[o.strip].layer.id() == surface) {
                        on = None;
                    }
                }
                PointerEvent::Press => {}
            }
        }

        // "Only while Ctrl is held": borrow the keyboard while resting on a strip.
        let want_keys = watch.cfg.edges.require_control && watch.active();
        if want_keys && !told_ctrl {
            told_ctrl = true;
            log::info!("layer-shell: 'only while Ctrl is held' is on: a strip takes the keyboard while the pointer rests on it, to see Ctrl");
        }
        for event in std::mem::take(&mut wl.state.key_events) {
            match event {
                KeyEvent::Enter { keys } => {
                    ctrl.forget();
                    for code in keys {
                        ctrl.key(code, true);
                    }
                }
                KeyEvent::Leave => ctrl.forget(),
                KeyEvent::Key { code, down } => {
                    if down && !logic::is_modifier(code) {
                        // Typing, not switching: give the keyboard back.
                        if let Some((i, _)) = keyboard.take() {
                            log::info!("layer-shell: a key was typed at the edge; keyboard given back");
                            strips[i].layer.set_keyboard(false);
                        }
                    }
                    ctrl.key(code, down);
                }
                KeyEvent::Modifiers { depressed } => ctrl.modifiers(depressed),
            }
        }
        let now = Instant::now();
        let worth = |o: OnStrip| dwell.would_switch(strips[o.strip].seg.side, &watch.dwell_config(), &watch.sit, now);
        match (on, keyboard) {
            (Some(o), None) if want_keys && before.is_none_or(|b| b.strip != o.strip) && worth(o) => {
                log::info!("layer-shell: keyboard taken on the {} strip to see Ctrl", strips[o.strip].seg.side.name());
                strips[o.strip].layer.set_keyboard(true);
                keyboard = Some((o.strip, now));
            }
            (Some(o), Some((i, since))) if i == o.strip && !ctrl.held() && now.duration_since(since) >= logic::CTRL_GRACE => {
                strips[i].layer.set_keyboard(false);
                keyboard = None;
                ctrl.forget();
            }
            (o, Some((i, _))) if !want_keys || o.is_none_or(|o| o.strip != i) => {
                strips[i].layer.set_keyboard(false);
                keyboard = None;
                ctrl.forget();
            }
            _ => {}
        }

        out.clear();
        if watch.active() {
            let at = on.map(|o| strips[o.strip].seg.side);
            let gap = watch.gap();
            dwell.step(at, ctrl.held(), &watch.dwell_config(), &watch.sit, now, &gap, &mut out);
        } else {
            dwell.idle(&mut out);
        }
        let place = |side: Side| {
            let o = on?;
            let strip = &strips[o.strip];
            (strip.seg.side == side).then(|| (rect(strip.seg.zone), o.at))
        };
        let switched = watch.apply(&out, place);
        if dwell.counting_down() {
            if let (Some(o), Some(b)) = (on, before) {
                if o.at != b.at {
                    overlay::edge_cursor(o.at.0, o.at.1);
                }
            }
        }
        if switched {
            if let Some((i, _)) = keyboard.take() {
                strips[i].layer.set_keyboard(false);
            }
            ctrl.forget();
        }
    }
    overlay::edge_end();
}

/// A strip along `seg` on its output.
fn make_strip(wl: &mut Wl, displays: &[Display], seg: &Segment) -> Option<Strip> {
    let display = displays.iter().find(|d| d.rect == seg.zone)?.clone();
    let (offset, length) = seg.offset_and_length();
    let z = seg.zone;
    let length = length.max(1) as u32;
    let (anchor, margin, size, origin) = match seg.side {
        Side::Left => (Anchor::Left | Anchor::Top, (offset, 0, 0, 0), (1, length), (z.x, seg.start)),
        Side::Right => (Anchor::Right | Anchor::Top, (offset, 0, 0, 0), (1, length), (z.x + z.w - 1, seg.start)),
        Side::Top => (Anchor::Top | Anchor::Left, (0, 0, 0, offset), (length, 1), (seg.start, z.y)),
        Side::Bottom => (Anchor::Bottom | Anchor::Left, (0, 0, 0, offset), (length, 1), (seg.start, z.y + z.h - 1)),
    };
    let layer = wl.layer(display.global, "flockswitch-edge", anchor, margin, size, true)?;
    log::info!(
        "layer-shell: {} strip on {} {:?}: {} px from its start, {} px long",
        seg.side.name(),
        display.label(),
        z,
        offset,
        length
    );
    Some(Strip { seg: *seg, display, layer, origin: (f64::from(origin.0), f64::from(origin.1)), drawn: false })
}
