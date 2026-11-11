//! Finding the cursor in a Wayland session. No app may follow the cursor there, so the
//! highlight shows only when something asks for it: the mouse arriving from another
//! computer (the engine knows), the shortcut (registered through the system's shortcut
//! service, shortcuts_portal.rs), or `flockswitch locate` (for a custom shortcut of the
//! desktop). This watcher takes those and hands them to overlay.rs, which draws the
//! highlight the way `path` finds for this session. Shaking the mouse needs the cursor
//! all the time and stays X11 only (edge.rs). An X11 session does all of this in edge.rs.

use crate::edge::{POLL, STATUS_POLL};
use crate::edge_logic::{self, Zone};
use crate::edge_wayland::session_is_wayland;
use crate::locate_logic::{self as logic, LocatePath};
use crate::overlay;
use crate::shared::Shared;
use std::sync::Arc;
use std::time::Instant;

/// How the highlight can be drawn in this session, and what that was decided from (for
/// the log). Asked each time, as the displays may have changed: a few milliseconds.
pub fn path() -> (LocatePath, String) {
    let wayland = session_is_wayland();
    let x11_display = std::env::var_os("DISPLAY").is_some();
    if !wayland {
        return (logic::locate_path(false, x11_display, None, None), "X11 session".into());
    }
    let (layer_shell, zones, problem) = match crate::wl::Wl::connect(false, false) {
        Ok(wl) => (Some(wl.has_layer_shell()), wl.displays().iter().map(|d| d.rect).collect::<Vec<Zone>>(), None),
        Err(e) => (None, Vec::new(), Some(e)),
    };
    if layer_shell == Some(true) {
        return (LocatePath::LayerShell, format!("wlr-layer-shell, outputs {zones:?}"));
    }
    let screens = if x11_display { crate::edge_portal::xwayland_screens() } else { Vec::new() };
    let k = edge_logic::xwayland_scale(&zones, &screens);
    let mut detail = format!("no wlr-layer-shell; Wayland outputs {zones:?}, XWayland screens {screens:?}");
    if let Some(e) = problem {
        detail.push_str(&format!(", Wayland: {e}"));
    }
    (logic::locate_path(true, x11_display, layer_shell, k), detail)
}

/// Watches for what asks to show the cursor, in a Wayland session (the tray app).
pub fn spawn(shared: Arc<Shared>) {
    // It only looks at shared flags and hands requests on: a small stack is plenty.
    let started = std::thread::Builder::new().name("locate".into()).stack_size(256 * 1024).spawn(move || watch(shared));
    if let Err(e) = started {
        log::warn!("locate: cannot start its thread: {e}");
    }
}

fn watch(shared: Arc<Shared>) {
    log::info!(
        "locate: Wayland session: the cursor is shown on arrival from another computer, with the shortcut (through the system) or with `flockswitch locate`; shaking the mouse needs an X11 session"
    );
    let mut cfg = shared.config();
    let mut version = shared.version();
    let mut status_read: Option<Instant> = None;
    let mut mouse_here = false;
    let mut told_shake = false;
    while !shared.stopped() {
        std::thread::sleep(POLL);
        if shared.version() != version {
            version = shared.version();
            cfg = shared.config();
        }
        if cfg.locate.on && cfg.locate.on_shake && !std::mem::replace(&mut told_shake, true) {
            log::info!("locate: shaking the mouse is not watched on Wayland (no app sees the cursor move)");
        }
        if status_read.is_none_or(|t| t.elapsed() >= STATUS_POLL) {
            let first = status_read.is_none();
            status_read = Some(Instant::now());
            let now_here = shared.read_status(|st| st.trigger.is_some());
            if now_here && !mouse_here && !first && cfg.locate.on && cfg.locate.on_arrival {
                log::info!("locate: the master mouse arrived from another computer");
                overlay::locate(&cfg.locate);
            }
            mouse_here = now_here;
        }
        if let Some(source) = shared.take_locate_request() {
            if cfg.locate.on {
                log::info!("locate: asked by {}", source.name());
                overlay::locate(&cfg.locate);
            } else {
                log::info!("locate: asked by {}, but finding the cursor is off in the settings", source.name());
            }
        }
    }
}
