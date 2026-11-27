//! The GNOME helper extension's client: the pointer, the screen edges and this
//! computer's mouse and keyboard on GNOME Wayland, without the desktop portals (which
//! ask again and again). The extension is packaging/linux/gnome-extension/ and runs
//! inside the compositor; docs/GNOME_HELPER.md describes both sides.
//!
//! Everything here is blocking, with short timeouts, on a session connection of its own.
//! Where the extension is not there (not installed, not enabled, another desktop) nothing
//! is called and `available()` is false, so the portal paths work exactly as before.

use crate::edge::Rect;
use ashpd::zbus::{
    self,
    message::{Flags, Message, Type},
    Connection, MessageStream,
};
use flock_net::{linux_input as input, InputEvent};
use futures_util::StreamExt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const BUS_NAME: &str = "io.github.wwhalefe.Flockswitch";
const OBJECT_PATH: &str = "/io/github/wwhalefe/Flockswitch";
const INTERFACE: &str = "io.github.wwhalefe.Flockswitch1";
/// The API version this client speaks (the extension's `Hello`).
const API_VERSION: u32 = 1;
/// How long one call may take: the compositor answers in microseconds when it can.
const TIMEOUT: Duration = Duration::from_millis(300);
/// Making a connection to the bus may take longer than a call.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// While the extension is not found, look again at most this often.
const RECHECK: Duration = Duration::from_secs(10);
/// The extension gives input back after 3 s without a KeepAlive; one a second is plenty.
const KEEPALIVE: Duration = Duration::from_secs(1);
/// How often the capture thread looks at its stop flag while no input arrives.
const STOP_POLL: Duration = Duration::from_millis(50);
/// The Control bit of the modifiers `GetPointer` returns (Clutter's, the same as X11's).
const CONTROL: u32 = 1 << 2;

// ---------------------------------------------------------------------------------------
// Pure parts: whether to use the extension, and how its data and InputEvents map.

/// FLOCKSWITCH_GNOME_HELPER=0 (or off, false, no) turns the extension off.
pub fn enabled_by_env(value: Option<&str>) -> bool {
    !value.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"))
}

/// A GNOME session on Wayland: the only place the extension is ever asked for.
pub fn gnome_wayland(wayland: bool, desktop: &str) -> bool {
    wayland && desktop.to_ascii_lowercase().contains("gnome")
}

/// Whether the extension is to be looked for (again) now: never looked for yet, or not
/// found at the last look and `RECHECK` has passed. A found extension is not asked again.
pub fn check_due(found: bool, since_last: Option<Duration>) -> bool {
    match since_last {
        None => true,
        Some(age) => !found && age >= RECHECK,
    }
}

/// Whether a `Hello` answer is a version this client understands.
pub fn version_ok(version: u32) -> bool {
    version >= API_VERSION
}

/// Whether a D-Bus error name says the extension is gone (not just slow).
fn gone_error(name: &str) -> bool {
    matches!(name, "org.freedesktop.DBus.Error.ServiceUnknown" | "org.freedesktop.DBus.Error.NameHasNoOwner")
}

/// Whether a Control key is down in the modifiers `GetPointer` returned.
pub fn control_held(mods: u32) -> bool {
    mods & CONTROL != 0
}

/// A monitor as `GetMonitors` returns it (x, y, width, height).
pub fn rect_of((x, y, w, h): (i32, i32, i32, i32)) -> Rect {
    Rect { x: f64::from(x), y: f64::from(y), w: f64::from(w), h: f64::from(h) }
}

/// What the extension tells about captured input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Captured {
    Motion(f64, f64),
    Button(u32, bool),
    Key(u32, bool),
    /// Wheel clicks, Clutter's sign: positive is down and right.
    Scroll(f64, f64),
    /// The extension gave the input back on its own.
    Ended,
}

/// The captured input as netcore's event. Scrolling flips the sign (netcore: positive is
/// up and left); a button or key netcore has no name for is dropped.
pub fn input_event(captured: Captured) -> Option<InputEvent> {
    match captured {
        Captured::Motion(dx, dy) => Some(InputEvent::Motion { dx, dy }),
        Captured::Button(code, down) => input::button(code).map(|button| InputEvent::Button { button, down }),
        Captured::Key(code, down) => input::evdev_to_hid(code).map(|usage| InputEvent::Key { usage, down }),
        Captured::Scroll(dx, dy) => Some(InputEvent::Scroll { dx: -dx, dy: -dy }),
        Captured::Ended => None,
    }
}

/// A call that injects input into this computer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Inject {
    Motion(f64, f64),
    /// The extension's absolute move; the network only moves the pointer relatively.
    #[allow(dead_code)]
    MoveTo(f64, f64),
    Button(u32, bool),
    Key(u32, bool),
    Scroll(f64, f64),
}

impl Inject {
    /// The extension's method name.
    fn method(self) -> &'static str {
        match self {
            Inject::Motion(..) => "Motion",
            Inject::MoveTo(..) => "MoveTo",
            Inject::Button(..) => "Button",
            Inject::Key(..) => "Key",
            Inject::Scroll(..) => "Scroll",
        }
    }
}

/// The call that makes `event` happen on this computer. Scrolling flips the sign back
/// (see `input_event`).
pub fn inject_call(event: &InputEvent) -> Option<Inject> {
    match *event {
        InputEvent::Motion { dx, dy } => Some(Inject::Motion(dx, dy)),
        InputEvent::Button { button, down } => Some(Inject::Button(input::evdev_button(button), down)),
        InputEvent::Key { usage, down } => input::hid_to_evdev(usage).map(|code| Inject::Key(code, down)),
        InputEvent::Scroll { dx, dy } => Some(Inject::Scroll(-dx, -dy)),
    }
}

/// Reads a signal's arguments into what it tells. None: not one of the extension's.
fn parse_signal(member: &str, message: &Message) -> Option<Captured> {
    let body = message.body();
    match member {
        "CapturedMotion" => body.deserialize::<(f64, f64)>().ok().map(|(dx, dy)| Captured::Motion(dx, dy)),
        "CapturedButton" => body.deserialize::<(u32, bool)>().ok().map(|(c, d)| Captured::Button(c, d)),
        "CapturedKey" => body.deserialize::<(u32, bool)>().ok().map(|(c, d)| Captured::Key(c, d)),
        "CapturedScroll" => body.deserialize::<(f64, f64)>().ok().map(|(dx, dy)| Captured::Scroll(dx, dy)),
        "CaptureEnded" => Some(Captured::Ended),
        _ => None,
    }
}

/// Whether `message` is one of the extension's signals from `owner` (the bus name's
/// owner when the capture began): the member name if so.
fn signal_from(message: &Message, owner: &str) -> Option<String> {
    let header = message.header();
    if header.message_type() != Type::Signal
        || header.path().map(|p| p.as_str()) != Some(OBJECT_PATH)
        || header.interface().map(|i| i.as_str()) != Some(INTERFACE)
        || header.sender().map(|s| s.as_str()) != Some(owner)
    {
        return None;
    }
    header.member().map(|m| m.to_string())
}

// ---------------------------------------------------------------------------------------
// The connection and the calls.

static CONNECTION: Mutex<Option<Connection>> = Mutex::new(None);

/// Runs `fut` to the end, or gives up after `limit`.
fn block<T>(limit: Duration, fut: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    match async_io::block_on(crate::edge_wayland::within(limit, fut)) {
        Some(result) => result,
        None => anyhow::bail!("the GNOME helper did not answer in {} ms", limit.as_millis()),
    }
}

/// The shared connection for reading and injecting.
fn connection() -> anyhow::Result<Connection> {
    if let Some(conn) = CONNECTION.lock().unwrap().clone() {
        return Ok(conn);
    }
    let conn = block(CONNECT_TIMEOUT, async { Ok(Connection::session().await?) })?;
    Ok(CONNECTION.lock().unwrap().get_or_insert(conn).clone())
}

/// A method call with an answer.
fn call<B>(conn: &Connection, method: &'static str, body: &B, limit: Duration) -> anyhow::Result<Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let result = block(limit, async { Ok(conn.call_method(Some(BUS_NAME), OBJECT_PATH, Some(INTERFACE), method, body).await?) });
    if let Err(e) = &result {
        if e.chain().filter_map(|c| c.downcast_ref::<zbus::Error>()).any(|z| {
            matches!(z, zbus::Error::MethodError(name, _, _) if gone_error(name.as_str()))
        }) {
            note_gone();
        }
    }
    result.map_err(|e| e.context(format!("GNOME helper {method}")))
}

/// The message of a call nobody waits for: input at 1000 Hz must not wait for round trips.
fn noreply_message<B>(method: &'static str, body: &B) -> anyhow::Result<Message>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    Ok(Message::method_call(OBJECT_PATH, method)?
        .destination(BUS_NAME)?
        .interface(INTERFACE)?
        .with_flags(Flags::NoReplyExpected)?
        .build(body)?)
}

/// Sends a call without waiting for an answer (blocking only while it is handed to the bus).
fn call_noreply<B>(conn: &Connection, method: &'static str, body: &B) -> anyhow::Result<()>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let message = noreply_message(method, body)?;
    block(TIMEOUT, async { Ok(conn.send(&message).await?) })
}

/// The bus name's owner, if it has one.
fn owner(conn: &Connection) -> anyhow::Result<String> {
    block(TIMEOUT, async {
        let bus = zbus::fdo::DBusProxy::new(conn).await?;
        let name = zbus::names::BusName::try_from(BUS_NAME)?;
        Ok(bus.get_name_owner(name).await?.to_string())
    })
}

// ---------------------------------------------------------------------------------------
// Whether the extension is there.

struct State {
    found: bool,
    checked: Option<Instant>,
    checking: bool,
}

static STATE: Mutex<State> = Mutex::new(State { found: false, checked: None, checking: false });

/// Looks for the extension now: the name has an owner and `Hello` answers.
fn look() -> bool {
    let found = (|| -> anyhow::Result<bool> {
        let conn = connection()?;
        owner(&conn)?;
        let reply = call(&conn, "Hello", &(), TIMEOUT)?;
        Ok(version_ok(reply.body().deserialize::<u32>()?))
    })();
    match found {
        Ok(found) => found,
        Err(e) => {
            log::debug!("gnome helper: not found: {e:#}");
            false
        }
    }
}

fn store(found: bool) {
    let mut state = STATE.lock().unwrap();
    if state.checked.is_none() || state.found != found {
        log::info!("gnome helper: extension {}", if found { "found" } else { "not found" });
    }
    state.found = found;
    state.checked = Some(Instant::now());
    state.checking = false;
}

/// A call showed the extension is gone: look again after the usual pause.
fn note_gone() {
    store(false);
}

/// Whether the GNOME helper extension is there: a GNOME Wayland session whose Shell
/// answers `Hello`. The first call looks and waits for the answer; after that the answer
/// is kept, and while there was none it is looked for again every 10 s (on a thread of
/// its own, so a caller never waits), so turning the extension on later is noticed.
/// No D-Bus call is ever made outside a GNOME Wayland session.
pub fn available() -> bool {
    check(false)
}

/// Like `available`, but a look that is due is waited for (for a thread that may block,
/// such as the edge worker's probe).
pub fn available_waiting() -> bool {
    check(true)
}

fn check(wait: bool) -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if !enabled_by_env(std::env::var("FLOCKSWITCH_GNOME_HELPER").ok().as_deref())
        || !gnome_wayland(crate::edge_wayland::session_is_wayland(), &desktop)
    {
        return false;
    }
    {
        let mut state = STATE.lock().unwrap();
        if !check_due(state.found, state.checked.map(|t| t.elapsed())) {
            return state.found;
        }
        if state.checked.is_some() && !wait {
            // A later look: on a thread of its own, one at a time.
            if !state.checking {
                state.checking = true;
                if std::thread::Builder::new().name("gnome-helper-check".into()).spawn(|| store(look())).is_err() {
                    state.checking = false;
                }
            }
            return state.found;
        }
        // Nobody has looked yet (or the caller waits): this caller looks, and a caller
        // meanwhile finds the old answer.
        state.checked = Some(Instant::now());
    }
    let found = look();
    store(found);
    found
}

// ---------------------------------------------------------------------------------------
// Reading and moving the pointer.

/// The pointer in the desktop's logical coordinates and the modifier bits.
pub fn pointer() -> Option<(f64, f64, u32)> {
    if !available() {
        return None;
    }
    let conn = connection().ok()?;
    let reply = call(&conn, "GetPointer", &(), TIMEOUT).ok()?;
    reply.body().deserialize::<(f64, f64, u32)>().ok()
}

/// The monitors, in the desktop's logical coordinates.
pub fn displays() -> Vec<Rect> {
    if !available() {
        return Vec::new();
    }
    let read = || -> anyhow::Result<Vec<Rect>> {
        let conn = connection()?;
        let reply = call(&conn, "GetMonitors", &(), TIMEOUT)?;
        Ok(reply.body().deserialize::<Vec<(i32, i32, i32, i32)>>()?.into_iter().map(rect_of).collect())
    };
    read().unwrap_or_else(|e| {
        log::debug!("gnome helper: monitors: {e:#}");
        Vec::new()
    })
}

/// Moves the pointer there.
pub fn warp(x: f64, y: f64) {
    if !available() {
        return;
    }
    let result = connection().and_then(|conn| call(&conn, "Warp", &(x, y), TIMEOUT).map(|_| ()));
    if let Err(e) = result {
        log::debug!("gnome helper: warp: {e:#}");
    }
}

// ---------------------------------------------------------------------------------------
// Injecting input.

/// Makes `call` happen, without waiting for the answer.
pub fn send(call: Inject) {
    if !available() {
        return;
    }
    let Ok(conn) = connection() else { return };
    let result = match call {
        Inject::Motion(a, b) | Inject::MoveTo(a, b) | Inject::Scroll(a, b) => call_noreply(&conn, call.method(), &(a, b)),
        Inject::Button(code, down) | Inject::Key(code, down) => call_noreply(&conn, call.method(), &(code, down)),
    };
    if let Err(e) = result {
        log::debug!("gnome helper: {}: {e:#}", call.method());
    }
}

/// Makes `event` happen on this computer (no answer is waited for).
pub fn inject(event: &InputEvent) {
    match inject_call(event) {
        Some(Inject::Motion(dx, dy)) => motion(dx, dy),
        Some(Inject::Button(code, down)) => button(code, down),
        Some(Inject::Key(code, down)) => key(code, down),
        Some(Inject::Scroll(dx, dy)) => scroll(dx, dy),
        Some(Inject::MoveTo(x, y)) => move_to(x, y),
        None => {}
    }
}

pub fn motion(dx: f64, dy: f64) {
    send(Inject::Motion(dx, dy));
}

#[allow(dead_code)]
pub fn move_to(x: f64, y: f64) {
    send(Inject::MoveTo(x, y));
}

pub fn button(evdev: u32, down: bool) {
    send(Inject::Button(evdev, down));
}

pub fn key(evdev: u32, down: bool) {
    send(Inject::Key(evdev, down));
}

pub fn scroll(dx: f64, dy: f64) {
    send(Inject::Scroll(dx, dy));
}

// ---------------------------------------------------------------------------------------
// Capturing this computer's mouse and keyboard.

/// A capture that is going on: its connection (the extension sends the input to the
/// connection that asked for it, so every call about it goes over this one) and the
/// thread that forwards it.
struct Capture {
    conn: Connection,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

static CAPTURE: Mutex<Option<Capture>> = Mutex::new(None);
/// The capture ended on its own (the extension gave the input back, or went away).
static LOST: AtomicBool = AtomicBool::new(false);

/// Whether a capture ended by itself and `stop_capture` has not yet been called for it.
/// The platform reports that through `ready()`, so netcore goes back to local.
pub fn capture_lost() -> bool {
    LOST.load(Ordering::Acquire)
}

/// Takes this computer's pointer, buttons, keys and scrolling away from the desktop and
/// sends them to `sink`, until `stop_capture` or the extension gives them back (then
/// `capture_lost()` is true).
pub fn start_capture(sink: Sender<InputEvent>) -> anyhow::Result<()> {
    anyhow::ensure!(available(), "the GNOME helper extension is not active");
    stop_capture();
    let conn = block(CONNECT_TIMEOUT, async { Ok(Connection::session().await?) })?;
    let owner = owner(&conn)?;
    // Listening starts before the capture does, so no first event is missed.
    let stream = MessageStream::from(conn.clone());
    let ok = call(&conn, "StartCapture", &(), CONNECT_TIMEOUT)?.body().deserialize::<bool>()?;
    anyhow::ensure!(ok, "the GNOME helper could not take the input (another program holds a grab)");
    LOST.store(false, Ordering::Release);
    let stop = Arc::new(AtomicBool::new(false));
    let (thread_conn, thread_stop) = (conn.clone(), stop.clone());
    let thread = std::thread::Builder::new()
        .name("gnome-helper-capture".into())
        .spawn(move || forward(thread_conn, stream, owner, sink, thread_stop))?;
    *CAPTURE.lock().unwrap() = Some(Capture { conn, stop, thread: Some(thread) });
    Ok(())
}

/// Gives the input back. Safe to call when nothing is captured; clears `capture_lost()`.
pub fn stop_capture() {
    let capture = CAPTURE.lock().unwrap().take();
    if let Some(mut capture) = capture {
        capture.stop.store(true, Ordering::Release);
        if let Err(e) = call(&capture.conn, "StopCapture", &(), TIMEOUT) {
            log::debug!("gnome helper: StopCapture: {e:#}");
        }
        if let Some(thread) = capture.thread.take() {
            let _ = thread.join();
        }
    }
    LOST.store(false, Ordering::Release);
}

/// The capture thread: hands the extension's signals on as events and keeps it alive.
/// It ends when told to, or, setting `LOST`, when the extension says it gave the input
/// back, the name changed hands or went away, or the KeepAlive cannot be sent.
fn forward(conn: Connection, mut stream: MessageStream, owner_at_start: String, sink: Sender<InputEvent>, stop: Arc<AtomicBool>) {
    use futures_util::future::{select, Either};
    let lost = async_io::block_on(async {
        let mut next_alive = Instant::now() + KEEPALIVE;
        loop {
            if stop.load(Ordering::Acquire) {
                return false;
            }
            let wait = next_alive.saturating_duration_since(Instant::now()).min(STOP_POLL);
            let timer = async_io::Timer::after(wait);
            match select(std::pin::pin!(stream.next()), std::pin::pin!(timer)).await {
                Either::Left((Some(Ok(message)), _)) => {
                    let Some(member) = signal_from(&message, &owner_at_start) else { continue };
                    match parse_signal(&member, &message) {
                        Some(Captured::Ended) => {
                            log::info!("gnome helper: the extension gave the input back");
                            return true;
                        }
                        Some(captured) => {
                            if let Some(event) = input_event(captured) {
                                if stop.load(Ordering::Acquire) || sink.send(event).is_err() {
                                    return false;
                                }
                            }
                        }
                        None => {}
                    }
                }
                Either::Left((Some(Err(_)), _)) => {}
                Either::Left((None, _)) => {
                    log::info!("gnome helper: the connection closed during a capture");
                    return true;
                }
                Either::Right(_) => {}
            }
            if Instant::now() >= next_alive {
                next_alive = Instant::now() + KEEPALIVE;
                // The name must still be owned by the Shell that began the capture; a
                // restarted Shell or a disabled extension holds no capture any more.
                let alive = async {
                    let bus = zbus::fdo::DBusProxy::new(&conn).await?;
                    let name = zbus::names::BusName::try_from(BUS_NAME)?;
                    anyhow::Ok(bus.get_name_owner(name).await?.to_string())
                };
                let same = matches!(crate::edge_wayland::within(TIMEOUT, alive).await, Some(Ok(o)) if o == owner_at_start);
                if !same {
                    log::info!("gnome helper: the extension is gone during a capture");
                    return true;
                }
                let sent = async {
                    let message = noreply_message("KeepAlive", &())?;
                    conn.send(&message).await?;
                    anyhow::Ok(())
                };
                if !matches!(crate::edge_wayland::within(TIMEOUT, sent).await, Some(Ok(()))) {
                    log::info!("gnome helper: KeepAlive could not be sent");
                    return true;
                }
            }
        }
    });
    if lost {
        LOST.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flock_net::MouseButton;

    #[test]
    fn env_switch() {
        assert!(enabled_by_env(None));
        assert!(enabled_by_env(Some("1")));
        assert!(enabled_by_env(Some("")));
        assert!(!enabled_by_env(Some("0")));
        assert!(!enabled_by_env(Some(" Off ")));
        assert!(!enabled_by_env(Some("false")));
    }

    #[test]
    fn only_gnome_on_wayland_is_asked() {
        assert!(gnome_wayland(true, "ubuntu:GNOME"));
        assert!(gnome_wayland(true, "GNOME"));
        assert!(!gnome_wayland(false, "GNOME"));
        assert!(!gnome_wayland(true, "KDE"));
        assert!(!gnome_wayland(true, ""));
    }

    #[test]
    fn looks_again_only_while_not_found() {
        assert!(check_due(false, None));
        assert!(check_due(true, None));
        assert!(!check_due(false, Some(Duration::from_secs(9))));
        assert!(check_due(false, Some(Duration::from_secs(10))));
        assert!(!check_due(true, Some(Duration::from_secs(3600))));
    }

    #[test]
    fn hello_version() {
        assert!(!version_ok(0));
        assert!(version_ok(1));
        assert!(version_ok(2));
    }

    #[test]
    fn gone_errors() {
        assert!(gone_error("org.freedesktop.DBus.Error.ServiceUnknown"));
        assert!(gone_error("org.freedesktop.DBus.Error.NameHasNoOwner"));
        assert!(!gone_error("org.freedesktop.DBus.Error.AccessDenied"));
        assert!(!gone_error("org.freedesktop.DBus.Error.NoReply"));
    }

    #[test]
    fn control_from_the_modifier_bits() {
        assert!(control_held(1 << 2));
        assert!(control_held((1 << 2) | 1));
        assert!(!control_held(1));
        assert!(!control_held((1 << 1) | (1 << 3)));
        assert!(!control_held(0));
    }

    #[test]
    fn monitors_become_rects() {
        assert_eq!(rect_of((1920, -200, 2560, 1440)), Rect { x: 1920.0, y: -200.0, w: 2560.0, h: 1440.0 });
    }

    #[test]
    fn captured_input_becomes_events() {
        assert_eq!(input_event(Captured::Motion(3.0, -4.0)), Some(InputEvent::Motion { dx: 3.0, dy: -4.0 }));
        assert_eq!(input_event(Captured::Button(0x110, true)), Some(InputEvent::Button { button: MouseButton::Left, down: true }));
        assert_eq!(input_event(Captured::Button(0x111, false)), Some(InputEvent::Button { button: MouseButton::Right, down: false }));
        assert_eq!(input_event(Captured::Button(0x112, true)), Some(InputEvent::Button { button: MouseButton::Middle, down: true }));
        assert_eq!(input_event(Captured::Button(0x2ff, true)), None, "no such button");
        // KEY_A is evdev 30, HID usage 0x04.
        assert_eq!(input_event(Captured::Key(30, true)), Some(InputEvent::Key { usage: 0x04, down: true }));
        assert_eq!(input_event(Captured::Key(0x2ff, true)), None, "no such key");
        assert_eq!(input_event(Captured::Ended), None);
    }

    #[test]
    fn captured_scrolling_flips_its_sign() {
        // Clutter: positive is down and right. netcore: positive is up and left.
        assert_eq!(input_event(Captured::Scroll(0.0, 1.0)), Some(InputEvent::Scroll { dx: 0.0, dy: -1.0 }));
        assert_eq!(input_event(Captured::Scroll(2.0, 0.0)), Some(InputEvent::Scroll { dx: -2.0, dy: 0.0 }));
    }

    #[test]
    fn events_become_calls() {
        assert_eq!(inject_call(&InputEvent::Motion { dx: 1.5, dy: -2.0 }), Some(Inject::Motion(1.5, -2.0)));
        assert_eq!(inject_call(&InputEvent::Button { button: MouseButton::Left, down: true }), Some(Inject::Button(0x110, true)));
        assert_eq!(inject_call(&InputEvent::Button { button: MouseButton::Forward, down: false }), Some(Inject::Button(0x114, false)));
        assert_eq!(inject_call(&InputEvent::Key { usage: 0x04, down: false }), Some(Inject::Key(30, false)));
        assert_eq!(inject_call(&InputEvent::Key { usage: 0xffff, down: true }), None);
        // netcore: positive dy is wheel up; the extension: positive is down.
        assert_eq!(inject_call(&InputEvent::Scroll { dx: 1.0, dy: 2.0 }), Some(Inject::Scroll(-1.0, -2.0)));
    }

    #[test]
    fn calls_name_the_extensions_methods() {
        assert_eq!(Inject::Motion(0.0, 0.0).method(), "Motion");
        assert_eq!(Inject::MoveTo(0.0, 0.0).method(), "MoveTo");
        assert_eq!(Inject::Button(0, true).method(), "Button");
        assert_eq!(Inject::Key(0, true).method(), "Key");
        assert_eq!(Inject::Scroll(0.0, 0.0).method(), "Scroll");
    }

    fn signal(member: &str, body: &(impl serde::Serialize + zbus::zvariant::DynamicType), sender: &str) -> Message {
        Message::signal(OBJECT_PATH, INTERFACE, member.to_string()).unwrap().sender(sender).unwrap().build(body).unwrap()
    }

    #[test]
    fn signals_are_read_from_their_arguments() {
        let m = signal("CapturedMotion", &(1.5f64, -2.5f64), ":1.7");
        assert_eq!(parse_signal("CapturedMotion", &m), Some(Captured::Motion(1.5, -2.5)));
        let m = signal("CapturedButton", &(0x110u32, true), ":1.7");
        assert_eq!(parse_signal("CapturedButton", &m), Some(Captured::Button(0x110, true)));
        let m = signal("CapturedKey", &(30u32, false), ":1.7");
        assert_eq!(parse_signal("CapturedKey", &m), Some(Captured::Key(30, false)));
        let m = signal("CapturedScroll", &(0.0f64, 1.0f64), ":1.7");
        assert_eq!(parse_signal("CapturedScroll", &m), Some(Captured::Scroll(0.0, 1.0)));
        let m = signal("CaptureEnded", &(), ":1.7");
        assert_eq!(parse_signal("CaptureEnded", &m), Some(Captured::Ended));
        assert_eq!(parse_signal("Other", &m), None);
        // Arguments of the wrong type are not guessed at.
        let m = signal("CapturedMotion", &(1u32, 2u32), ":1.7");
        assert_eq!(parse_signal("CapturedMotion", &m), None);
    }

    #[test]
    fn only_the_owners_signals_on_our_interface_count() {
        let m = signal("CapturedMotion", &(1.0f64, 1.0f64), ":1.7");
        assert_eq!(signal_from(&m, ":1.7").as_deref(), Some("CapturedMotion"));
        assert_eq!(signal_from(&m, ":1.8"), None, "another sender");
        let other_path = Message::signal("/other", INTERFACE, "CapturedMotion").unwrap().sender(":1.7").unwrap().build(&(1.0f64, 1.0f64)).unwrap();
        assert_eq!(signal_from(&other_path, ":1.7"), None);
        let other_iface = Message::signal(OBJECT_PATH, "org.example.Other", "CapturedMotion").unwrap().sender(":1.7").unwrap().build(&(1.0f64, 1.0f64)).unwrap();
        assert_eq!(signal_from(&other_iface, ":1.7"), None);
    }

    #[test]
    fn no_dbus_outside_gnome_wayland() {
        // Nothing here runs a session bus; outside GNOME Wayland (or with the switch off)
        // the answers come without one.
        if !gnome_wayland(crate::edge_wayland::session_is_wayland(), &std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()) {
            assert!(!available());
            assert_eq!(pointer(), None);
            assert!(displays().is_empty());
        }
    }
}
