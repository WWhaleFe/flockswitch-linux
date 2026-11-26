//! X11 capture and XTest injection on a dedicated display connection.
use flock_net::{linux_input as input, InputEvent, Os, Platform, Rect};
use std::{
    ptr,
    sync::{
        mpsc::{self, Sender},
        Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use x11_dl::{xinput2 as xi, xlib, xtest};
use rustix::event::{eventfd, poll, EventfdFlags, PollFd, PollFlags};
use std::{os::fd::{BorrowedFd, OwnedFd}, sync::Arc};

enum Command {
    Capture(Sender<InputEvent>, Sender<Result<(), String>>),
    Stop(Sender<()>),
    Inject(InputEvent),
    Quit,
}
pub fn probe() -> Result<(), String> {
    let x = xlib::Xlib::open().map_err(|e| e.to_string())?;
    let test = xtest::Xf86vmode::open().map_err(|e| e.to_string())?;
    unsafe {
        let d = (x.XOpenDisplay)(ptr::null());
        if d.is_null() {
            return Err("Cannot open the X11 display".into());
        }
        let (mut a, mut b, mut major, mut minor) = (0, 0, 0, 0);
        let available = (test.XTestQueryExtension)(d, &mut a, &mut b, &mut major, &mut minor) != 0;
        (x.XCloseDisplay)(d);
        if available {
            Ok(())
        } else {
            Err("XTest is unavailable".into())
        }
    }
}
pub struct X11 {
    clipboard: crate::clipboard::Clipboard,
    commands: Sender<Command>,
    wake: Arc<OwnedFd>,
    worker: Mutex<Option<JoinHandle<()>>>,
}
impl X11 {
    pub fn new() -> anyhow::Result<Self> {
        let (commands, rx) = mpsc::channel();
        let (ready, initialized) = mpsc::sync_channel(1);
        let wake = Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?);
        let worker_wake = wake.clone();
        let worker = thread::Builder::new()
            .name("network-x11".into())
            .stack_size(256 * 1024)
            .spawn(move || unsafe {
                let result = (|| -> anyhow::Result<_> {
                    let x = xlib::Xlib::open()?;
                    let test = xtest::Xf86vmode::open()?;
                    let d = (x.XOpenDisplay)(ptr::null());
                    anyhow::ensure!(!d.is_null(), "Cannot open the X11 display");
                    let (mut a, mut b, mut major, mut minor) = (0, 0, 0, 0);
                    if (test.XTestQueryExtension)(d, &mut a, &mut b, &mut major, &mut minor) == 0 {
                        (x.XCloseDisplay)(d);
                        anyhow::bail!("XTest is unavailable");
                    }
                    Ok((x, test, d))
                })();
                let (x, test, d) = match result {
                    Ok(value) => {
                        let _ = ready.send(Ok(()));
                        value
                    }
                    Err(e) => {
                        let _ = ready.send(Err(format!("{e:#}")));
                        return;
                    }
                };
                let root = (x.XDefaultRootWindow)(d);
                let mut raw = xi::XInput2::open().ok().and_then(|input| {
                    let (mut opcode, mut event, mut error) = (0, 0, 0);
                    if (x.XQueryExtension)(
                        d,
                        c"XInputExtension".as_ptr(),
                        &mut opcode,
                        &mut event,
                        &mut error,
                    ) == 0
                    {
                        return None;
                    }
                    let (mut major, mut minor) = (2, 1);
                    if (input.XIQueryVersion)(d, &mut major, &mut minor) != xlib::Success as i32 {
                        return None;
                    }
                    Some((input, opcode))
                });
                // Raw motion is wanted only while capturing: selected on grab, cleared on
                // release, so moving the local mouse does not wake this thread when idle.
                let select_raw = |raw: &Option<(xi::XInput2, i32)>, on: bool| -> bool {
                    raw.as_ref().is_none_or(|(input, _)| {
                        let mut bits = [0u8; 4];
                        if on {
                            xi::XISetMask(&mut bits, xi::XI_RawMotion);
                        }
                        let mut mask = xi::XIEventMask {
                            deviceid: xi::XIAllMasterDevices,
                            mask_len: bits.len() as i32,
                            mask: bits.as_mut_ptr(),
                        };
                        (input.XISelectEvents)(d, root, &mut mask, 1) == xlib::Success as i32
                    })
                };
                let mut motion_rest = (0.0, 0.0);
                let screen = (x.XDefaultScreen)(d);
                let park = (
                    (x.XDisplayWidth)(d, screen) / 2,
                    (x.XDisplayHeight)(d, screen) / 2,
                );
                let bits: [std::os::raw::c_char; 1] = [0];
                let bitmap = (x.XCreateBitmapFromData)(d, root, bits.as_ptr(), 1, 1);
                let mut color: xlib::XColor = std::mem::zeroed();
                let blank =
                    (x.XCreatePixmapCursor)(d, bitmap, bitmap, &mut color, &mut color, 0, 0);
                (x.XFreePixmap)(d, bitmap);
                let mut sink: Option<Sender<InputEvent>> = None;
                let mut scroll = (0.0, 0.0);
                let mut running = true;
                while running {
                    // Clear before draining commands: a concurrent send either joins
                    // this batch or leaves the fd readable for the next iteration.
                    let mut count = [0u8; 8];
                    let _ = rustix::io::read(&*worker_wake, &mut count);
                    for command in rx.try_iter() {
                        match command {
                            Command::Capture(sender, ack) => {
                                let mask = (xlib::PointerMotionMask
                                    | xlib::ButtonPressMask
                                    | xlib::ButtonReleaseMask)
                                    as u32;
                                let p = (x.XGrabPointer)(
                                    d,
                                    root,
                                    0,
                                    mask,
                                    xlib::GrabModeAsync,
                                    xlib::GrabModeAsync,
                                    root,
                                    blank,
                                    xlib::CurrentTime,
                                );
                                let k = (x.XGrabKeyboard)(
                                    d,
                                    root,
                                    0,
                                    xlib::GrabModeAsync,
                                    xlib::GrabModeAsync,
                                    xlib::CurrentTime,
                                );
                                if p != xlib::GrabSuccess || k != xlib::GrabSuccess {
                                    (x.XUngrabPointer)(d, xlib::CurrentTime);
                                    (x.XUngrabKeyboard)(d, xlib::CurrentTime);
                                    let _ = ack.send(Err(
                                        "Cannot grab the X11 pointer and keyboard".into(),
                                    ));
                                } else {
                                    (x.XWarpPointer)(d, 0, root, 0, 0, 0, 0, park.0, park.1);
                                    (x.XSync)(d, 0);
                                    while (x.XPending)(d) > 0 {
                                        let mut e = std::mem::zeroed();
                                        (x.XNextEvent)(d, &mut e);
                                    }
                                    let mut keys: [std::os::raw::c_char; 32] = [0; 32];
                                    (x.XQueryKeymap)(d, keys.as_mut_ptr());
                                    for code in [29u32, 42, 54, 56, 97, 100, 125, 126] {
                                        let key = code + 8;
                                        if keys[(key / 8) as usize] as u8 & (1 << (key % 8)) != 0 {
                                            if let Some(usage) = input::evdev_to_hid(code) {
                                                let _ = sender
                                                    .send(InputEvent::Key { usage, down: true });
                                            }
                                        }
                                    }
                                    // Without raw motion the plain pointer events are used.
                                    if !select_raw(&raw, true) {
                                        raw = None;
                                    }
                                    sink = Some(sender);
                                    let _ = ack.send(Ok(()));
                                }
                            }
                            Command::Stop(ack) => {
                                sink = None;
                                select_raw(&raw, false);
                                (x.XUngrabPointer)(d, xlib::CurrentTime);
                                (x.XUngrabKeyboard)(d, xlib::CurrentTime);
                                (x.XSync)(d, 0);
                                let _ = ack.send(());
                            }
                            Command::Quit => {
                                running = false;
                                break;
                            }
                            Command::Inject(event) => {
                                match event {
                                    InputEvent::Motion { dx, dy } => {
                                        if let Some((px, py)) = crate::edge::platform::cursor() {
                                            let displays = displays();
                                            // Whole pixels only; the fraction waits for the next event.
                                            let wx = crate::net_logic::whole_pixels(&mut motion_rest.0, dx);
                                            let wy = crate::net_logic::whole_pixels(&mut motion_rest.1, dy);
                                            if let Some((px, py)) = flock_net::geometry::clamp(
                                                &displays,
                                                px + wx,
                                                py + wy,
                                            ) {
                                                (test.XTestFakeMotionEvent)(
                                                    d, -1, px as i32, py as i32, 0,
                                                );
                                            }
                                        }
                                    }
                                    InputEvent::Button { button, down } => {
                                        (test.XTestFakeButtonEvent)(
                                            d,
                                            input::xbutton(button),
                                            i32::from(down),
                                            0,
                                        );
                                    }
                                    InputEvent::Key { usage, down } => {
                                        if let Some(code) = input::hid_to_evdev(usage) {
                                            (test.XTestFakeKeyEvent)(
                                                d,
                                                code + 8,
                                                i32::from(down),
                                                0,
                                            );
                                        }
                                    }
                                    InputEvent::Scroll { dx, dy } => {
                                        for (n, positive, negative) in [
                                            (input::notches(&mut scroll.0, dx), 6, 7),
                                            (input::notches(&mut scroll.1, dy), 4, 5),
                                        ] {
                                            let button = if n > 0 { positive } else { negative };
                                            for _ in 0..n.unsigned_abs() {
                                                (test.XTestFakeButtonEvent)(d, button, 1, 0);
                                                (test.XTestFakeButtonEvent)(d, button, 0, 0);
                                            }
                                        }
                                    }
                                }
                                (x.XSync)(d, 0);
                            }
                        }
                    }
                    while (x.XPending)(d) > 0 {
                        let mut e: xlib::XEvent = std::mem::zeroed();
                        (x.XNextEvent)(d, &mut e);
                        let Some(s) = sink.as_ref() else { continue };
                        let event = match e.get_type() {
                            xlib::GenericEvent => {
                                let mut cookie = e.generic_event_cookie;
                                if raw
                                    .as_ref()
                                    .is_none_or(|(_, opcode)| *opcode != cookie.extension)
                                    || cookie.evtype != xi::XI_RawMotion
                                    || (x.XGetEventData)(d, &mut cookie) == 0
                                {
                                    continue;
                                }
                                let raw_event = &*cookie.data.cast::<xi::XIRawEvent>();
                                let values = raw_event.valuators;
                                let (mut dx, mut dy, mut index) = (0.0, 0.0, 0);
                                if !values.mask.is_null() && !values.values.is_null() {
                                    let mask = std::slice::from_raw_parts(
                                        values.mask,
                                        values.mask_len.max(0) as usize,
                                    );
                                    for axis in 0..2 {
                                        if mask.first().is_some_and(|m| m & (1 << axis) != 0) {
                                            let value = *values.values.add(index);
                                            index += 1;
                                            if axis == 0 {
                                                dx = value;
                                            } else {
                                                dy = value;
                                            }
                                        }
                                    }
                                }
                                (x.XFreeEventData)(d, &mut cookie);
                                Some(InputEvent::Motion { dx, dy })
                            }
                            xlib::MotionNotify => {
                                let mut m = e.motion;
                                if raw.is_none() {
                                    let mut queued = std::mem::zeroed();
                                    while (x.XCheckTypedEvent)(d, xlib::MotionNotify, &mut queued)
                                        != 0
                                    {
                                        m = queued.motion;
                                    }
                                }
                                let (dx, dy) = (m.x_root - park.0, m.y_root - park.1);
                                if dx == 0 && dy == 0 {
                                    continue;
                                }
                                (x.XWarpPointer)(d, 0, root, 0, 0, 0, 0, park.0, park.1);
                                raw.is_none().then_some(InputEvent::Motion {
                                    dx: f64::from(dx),
                                    dy: f64::from(dy),
                                })
                            }
                            xlib::ButtonPress | xlib::ButtonRelease => {
                                let b = e.button;
                                let down = e.get_type() == xlib::ButtonPress;
                                input::button(b.button)
                                    .map(|button| InputEvent::Button { button, down })
                                    .or_else(|| input::wheel(b.button, down))
                            }
                            xlib::KeyPress | xlib::KeyRelease => {
                                input::evdev_to_hid(e.key.keycode.saturating_sub(8)).map(|usage| {
                                    InputEvent::Key {
                                        usage,
                                        down: e.get_type() == xlib::KeyPress,
                                    }
                                })
                            }
                            _ => None,
                        };
                        if let Some(event) = event {
                            let _ = s.send(event);
                        }
                    }
                    (x.XFlush)(d);
                    if running && (x.XPending)(d) == 0 {
                        // Safety: the X connection owns this fd until XCloseDisplay
                        // below. Commands and X events both wake the same poll.
                        let display_fd = BorrowedFd::borrow_raw((x.XConnectionNumber)(d));
                        let mut fds = [PollFd::new(&display_fd, PollFlags::IN), PollFd::new(&*worker_wake, PollFlags::IN)];
                        match poll(&mut fds, None) {
                            Ok(_) | Err(rustix::io::Errno::INTR) => {}
                            Err(e) => {
                                log::warn!("network: X11 event wait failed: {e}");
                                break;
                            }
                        }
                    }
                }
                (x.XUngrabPointer)(d, xlib::CurrentTime);
                (x.XUngrabKeyboard)(d, xlib::CurrentTime);
                (x.XFreeCursor)(d, blank);
                (x.XCloseDisplay)(d);
            })?;
        initialized.recv()?.map_err(anyhow::Error::msg)?;
        Ok(Self {
            clipboard: crate::clipboard::Clipboard::default(),
            commands,
            wake,
            worker: Mutex::new(Some(worker)),
        })
    }

    fn send(&self, command: Command) -> anyhow::Result<()> {
        self.commands.send(command)?;
        loop {
            match rustix::io::write(&*self.wake, &1u64.to_ne_bytes()) {
                Ok(_) | Err(rustix::io::Errno::AGAIN) => return Ok(()),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }
}
fn displays() -> Vec<Rect> {
    crate::edge::platform::displays()
        .into_iter()
        .map(|r| Rect::new(r.x, r.y, r.w, r.h))
        .collect()
}
impl Platform for X11 {
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
        // Always the real position: netcore needs it for the way back while controlled,
        // whoever owns the edges. Leaving from here is the app's edge watcher's call
        // (app_drives_edges is always set on X11), never netcore's.
        crate::edge::platform::cursor()
    }
    fn warp(&self, x: f64, y: f64) {
        crate::edge::platform::place(x, y)
    }
    fn control_down(&self) -> bool { crate::edge::platform::control_down() }
    fn ready(&self) -> Result<(), String> {
        Ok(())
    }
    fn start_capture(&self, sink: Sender<InputEvent>) -> anyhow::Result<()> {
        anyhow::ensure!(crate::net::network_edges_allowed(), "Network edges are paused");
        let (ack, done) = mpsc::channel();
        self.send(Command::Capture(sink, ack))?;
        done.recv_timeout(Duration::from_secs(2))?
            .map_err(anyhow::Error::msg)
    }
    fn stop_capture(&self) {
        let (ack, done) = mpsc::channel();
        if self.send(Command::Stop(ack)).is_ok() {
            let _ = done.recv_timeout(Duration::from_secs(2));
        }
    }
    fn inject(&self, event: &InputEvent) {
        let _ = self.send(Command::Inject(event.clone()));
    }
}
impl Drop for X11 {
    fn drop(&mut self) {
        let _ = self.send(Command::Quit);
        if let Some(h) = self.worker.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}

/// Requires a disposable X11 server (CI: xvfb-run, one test at a time).
#[cfg(test)]
mod smoke_tests {
    use super::*;
    use std::{path::PathBuf, sync::Once, time::Instant};

    #[test]
    fn commands_wake_an_idle_worker_without_x_events() {
        let (commands, received) = mpsc::channel();
        let wake = Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).unwrap());
        let worker_wake = wake.clone();
        let (done, completed) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut fds = [PollFd::new(&*worker_wake, PollFlags::IN)];
            poll(&mut fds, None).unwrap();
            assert!(matches!(received.recv().unwrap(), Command::Quit));
            done.send(()).unwrap();
        });
        let platform = X11 {
            clipboard: crate::clipboard::Clipboard::default(),
            commands,
            wake,
            worker: Mutex::new(None),
        };
        platform.send(Command::Quit).unwrap();
        completed.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
    }

    fn platform() -> X11 {
        static INIT: Once = Once::new();
        INIT.call_once(|| unsafe {
            let x = xlib::Xlib::open().unwrap();
            assert_ne!((x.XInitThreads)(), 0);
        });
        crate::net::allow_test_edges();
        X11::new().unwrap()
    }
    fn wait_cursor(p: &X11, at: (f64, f64)) {
        let until = Instant::now() + Duration::from_secs(2);
        while p.cursor() != Some(at) && Instant::now() < until {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(p.cursor(), Some(at));
    }
    #[test]
    #[ignore = "requires X11/XTest, run under xvfb-run"]
    fn x11_displays() {
        let p = platform();
        assert!(!p.displays().is_empty());
        assert!(p.displays().iter().all(|d| d.width > 0.0 && d.height > 0.0));
    }
    #[test]
    #[ignore = "requires X11/XTest, run under xvfb-run"]
    fn x11_cursor_warp_round_trip() {
        let p = platform();
        p.warp(100.0, 120.0);
        wait_cursor(&p, (100.0, 120.0));
        p.warp(140.0, 150.0);
        wait_cursor(&p, (140.0, 150.0));
    }
    #[test]
    #[ignore = "requires X11/XTest, run under xvfb-run"]
    fn x11_inject_pointer_motion() {
        let p = platform();
        p.warp(100.0, 100.0);
        wait_cursor(&p, (100.0, 100.0));
        p.inject(&InputEvent::Motion { dx: 13.0, dy: 17.0 });
        wait_cursor(&p, (113.0, 117.0));
    }
    #[test]
    #[ignore = "requires X11 selection, run under xvfb-run"]
    fn x11_clipboard_text_round_trip() {
        let p = platform();
        p.set_clipboard_text("flockswitch 한글 clipboard");
        // A second connection must read the selection after the setter returned.
        let mut reader = arboard::Clipboard::new().unwrap();
        assert_eq!(reader.get_text().unwrap(), "flockswitch 한글 clipboard");
        assert_eq!(p.clipboard_text().as_deref(), Some("flockswitch 한글 clipboard"));
    }
    #[test]
    #[ignore = "requires X11 selection, run under xvfb-run"]
    fn x11_clipboard_png_round_trip() {
        let p = platform();
        let pixels: Vec<u8> = (0..24).collect();
        let png = crate::clipboard::encode_png(2, 3, &pixels).unwrap();
        p.set_clipboard_image_png(&png);
        let mut reader = arboard::Clipboard::new().unwrap();
        let image = reader.get_image().unwrap();
        assert_eq!((image.width, image.height, image.bytes.as_ref()), (2, 3, pixels.as_slice()));
        assert_eq!(crate::clipboard::decode_png(&p.clipboard_image_png().unwrap()), Some((2, 3, pixels)));
    }
    #[test]
    #[ignore = "requires X11 text/uri-list, run under xvfb-run"]
    fn x11_clipboard_files_round_trip() {
        let p = platform();
        // arboard puts only existing files on the clipboard (it canonicalizes each path),
        // as the files received over the network are.
        let dir = std::env::temp_dir().join(format!("flock-x11-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let files: Vec<PathBuf> = ["a b.txt", "한글.png"].iter().map(|n| {
            let f = dir.join(n);
            std::fs::write(&f, b"x").unwrap();
            f.canonicalize().unwrap()
        }).collect();
        p.set_clipboard_files(&files);
        let mut reader = arboard::Clipboard::new().unwrap();
        assert_eq!(reader.get().file_list().unwrap(), files);
        assert_eq!(p.clipboard_files(), files);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
