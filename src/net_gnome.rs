//! Network sharing through the GNOME helper extension (gnome_ext.rs): this computer's
//! pointer, buttons, keys and scrolling are taken and handed on by the extension, and
//! the other computer's input is injected through it, so neither the RemoteDesktop nor
//! the InputCapture portal is opened and the system asks nothing. Modelled on net_x11.rs:
//! the cursor is always the real one, and crossing at an edge is the app's edge watcher's
//! call (edge.rs, `app_drives_edges`).
use crate::gnome_ext;
use flock_net::{InputEvent, Os, Platform, Rect};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
};

pub struct Gnome {
    clipboard: crate::clipboard::Clipboard,
    /// A capture was started here and not stopped since.
    capturing: AtomicBool,
}

impl Gnome {
    pub fn new() -> anyhow::Result<Self> {
        anyhow::ensure!(gnome_ext::available_waiting(), "the GNOME helper extension is not active");
        Ok(Self { clipboard: crate::clipboard::Clipboard::default(), capturing: AtomicBool::new(false) })
    }
}

/// What `ready()` says: while capturing, a capture the extension ended (it gave the
/// input back, or went away) is a failure, so netcore goes back to local and calls
/// `stop_capture`, which clears it; otherwise the extension must be there.
pub fn ready_state(extension: bool, capturing: bool, lost: bool) -> Result<(), String> {
    if capturing && lost {
        Err("The GNOME helper gave the input back; input was returned locally".into())
    } else if !extension {
        Err("The GNOME helper extension is not active".into())
    } else {
        Ok(())
    }
}

fn displays() -> Vec<Rect> {
    crate::edge::platform::displays().into_iter().map(|r| Rect::new(r.x, r.y, r.w, r.h)).collect()
}

impl Platform for Gnome {
    fn clipboard_text(&self) -> Option<String> { self.clipboard.text() }
    fn set_clipboard_text(&self, text: &str) { self.clipboard.set_text(text); }
    fn clipboard_files(&self) -> Vec<std::path::PathBuf> { self.clipboard.files() }
    fn set_clipboard_files(&self, files: &[std::path::PathBuf]) { self.clipboard.set_files(files); }
    fn clipboard_image_png(&self) -> Option<Vec<u8>> { self.clipboard.image() }
    fn set_clipboard_image_png(&self, png: &[u8]) { self.clipboard.set_image(png); }
    fn received_dir(&self) -> Option<std::path::PathBuf> { crate::net::received_dir() }
    fn os(&self) -> Os {
        Os::Linux
    }
    fn displays(&self) -> Vec<Rect> {
        displays()
    }
    fn cursor(&self) -> Option<(f64, f64)> {
        // Always the real position, as on X11: netcore needs it for the way back while
        // controlled, and leaving from here is the app's edge watcher's call.
        crate::edge::platform::cursor()
    }
    fn warp(&self, x: f64, y: f64) {
        crate::edge::platform::place(x, y)
    }
    fn control_down(&self) -> bool {
        crate::edge::platform::control_down()
    }
    fn ready(&self) -> Result<(), String> {
        ready_state(gnome_ext::available(), self.capturing.load(Ordering::Acquire), gnome_ext::capture_lost())
    }
    fn start_capture(&self, sink: Sender<InputEvent>) -> anyhow::Result<()> {
        anyhow::ensure!(crate::net::network_edges_allowed(), "Network edges are paused");
        gnome_ext::start_capture(sink)?;
        self.capturing.store(true, Ordering::Release);
        Ok(())
    }
    fn stop_capture(&self) {
        self.capturing.store(false, Ordering::Release);
        gnome_ext::stop_capture();
    }
    fn inject(&self, event: &InputEvent) {
        gnome_ext::inject(event);
    }
}

impl Drop for Gnome {
    fn drop(&mut self) {
        if self.capturing.swap(false, Ordering::AcqRel) {
            gnome_ext::stop_capture();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lost_capture_is_a_failure_until_stopped() {
        assert_eq!(ready_state(true, false, false), Ok(()));
        assert_eq!(ready_state(true, true, false), Ok(()));
        assert!(ready_state(true, true, true).is_err());
        // Not capturing: an old loss does not matter.
        assert_eq!(ready_state(true, false, true), Ok(()));
    }

    #[test]
    fn no_extension_is_never_ready() {
        assert!(ready_state(false, false, false).is_err());
        assert!(ready_state(false, true, false).is_err());
    }
}
