//! InputCapture bridge and RemoteDesktop EIS sender.
use crate::shared::Shared;
use flock_net::{linux_input as input, InputEvent, Os, Platform, Rect, Side};
use futures_util::{stream, StreamExt};
use reis::{
    ei,
    event::{Device, DeviceCapability, EiEvent},
};
use rustix::event::{eventfd, EventfdFlags};
use std::{
    collections::HashSet,
    os::{fd::OwnedFd, unix::net::UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender},
        Arc, Mutex, OnceLock,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Capture {
    pub sides: Vec<Side>,
    pub hit: Option<(Side, f64)>,
    /// An edge hit held back until Control is down ("hold Control to cross"): libei
    /// reports a Control held before the capture only a moment after it starts.
    pub deferred: Option<(Side, f64)>,
    pub active: bool,
    pub sink: Option<Sender<InputEvent>>,
    pub pending: Vec<InputEvent>,
    pub release: Option<Instant>,
    pub position: Option<(f64, f64)>,
    pub control: bool,
}
pub fn capture() -> &'static Mutex<Capture> {
    static CAPTURE: OnceLock<Mutex<Capture>> = OnceLock::new();
    CAPTURE.get_or_init(|| Mutex::new(Capture::default()))
}
pub fn disable() {
    let mut session = SESSION.lock().unwrap();
    session.generation += 1;
    clear_capture();
}

#[derive(Default)]
struct Session {
    generation: u64,
    /// A failed EIS session must be replaced by the supervisor.
    failed: Option<String>,
    /// A failure before opening is normally a cancelled or denied permission dialog.
    opened: bool,
    /// No create_session answer means retrying will not repeat a denied dialog.
    no_answer: bool,
}
static SESSION: Mutex<Session> = Mutex::new(Session {
    generation: 0, failed: None, opened: false, no_answer: false,
});

/// Reserve a generation before probing, so an off/on invalidates even a slow probe.
pub fn begin_start() -> u64 {
    let mut session = SESSION.lock().unwrap();
    *session = Session { generation: session.generation + 1, ..Session::default() };
    session.generation
}

pub fn current_start(generation: u64) -> bool {
    SESSION.lock().unwrap().generation == generation
}

fn with_session<T>(generation: u64, apply: impl FnOnce(&mut Session) -> T) -> Option<T> {
    let mut session = SESSION.lock().unwrap();
    (session.generation == generation).then(|| apply(&mut session))
}

fn disable_session(generation: u64) {
    with_session(generation, |session| {
        session.generation += 1;
        clear_capture();
    });
}

fn clear_capture() {
    REMOTE.store(false, Ordering::Release);
    *REMOTE_ENDED.lock().unwrap() = None;
    let mut c = capture().lock().unwrap();
    c.sides.clear();
    c.sink = None;
    c.hit = None;
    c.deferred = None;
    c.pending.clear();
    if c.active {
        c.release = Some(Instant::now());
    }
}
pub fn captured_event(event: InputEvent) {
    let mut c = capture().lock().unwrap();
    if let Some(sink) = &c.sink {
        let _ = sink.send(event);
    } else if c.pending.len() < 512 {
        c.pending.push(event);
    }
}
#[derive(Default)]
struct Model {
    cursor: Option<(f64, f64)>,
    displays: Vec<Rect>,
    error: Option<String>,
    pointer: bool,
    keyboard: bool,
}
enum Command {
    Input(InputEvent),
    Warp(f64, f64),
    Quit,
}
/// Whether another computer controls this one now. netcore warps the pointer when that
/// control starts and when it ends, and in between injects the controller's input, so:
/// a warp clears it and injected motion, scrolling or a press sets it (the key and button
/// releases sent after control ends do not). Wayland has no global cursor; the position
/// this app set is only true while that control lasts.
static REMOTE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub fn eis_opened() -> bool {
    SESSION.lock().unwrap().opened
}
pub fn eis_no_answer() -> bool {
    SESSION.lock().unwrap().no_answer
}
pub fn eis_failed() -> Option<String> {
    SESSION.lock().unwrap().failed.clone()
}
pub fn remote_controlled() -> bool {
    REMOTE.load(Ordering::Acquire)
}
/// When the last remote control ended (the warp back at the edge it came in by).
static REMOTE_ENDED: Mutex<Option<Instant>> = Mutex::new(None);
/// The compositor may report the barrier that the controller's last push touched only
/// after netcore has already handed control back; that touch is still theirs.
const REMOTE_TAIL: Duration = Duration::from_millis(500);
/// Controlled from elsewhere now, or a moment ago.
pub fn remote_or_just_ended() -> bool {
    remote_controlled() || REMOTE_ENDED.lock().unwrap().is_some_and(|t| t.elapsed() < REMOTE_TAIL)
}
pub struct Wayland {
    generation: u64,
    clipboard: crate::clipboard::Clipboard,
    commands: Sender<Command>,
    /// Wakes the EIS loop for a queued command; idle, it waits for the compositor only.
    wake: Arc<OwnedFd>,
    /// Wakes the `cancelled` future in `receive`, which may be waiting on the portal.
    cancel_wake: Arc<OwnedFd>,
    model: Arc<Mutex<Model>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    send: bool,
    capturing: AtomicBool,
    cancelled: Arc<AtomicBool>,
}
impl Wayland {
    pub fn new(shared: Arc<Shared>, generation: u64) -> anyhow::Result<Self> {
        let support = crate::net::portal_probe();
        anyhow::ensure!(support.receive, "{}", support.reason);
        anyhow::ensure!(
            !shared.stopped() && shared.config().network.on,
            "Network sharing was disabled"
        );
        // Hold the generation lock through registration. A cancelled probe must not
        // reset the new session's status or install a receiver after sharing restarts.
        let session = SESSION.lock().unwrap();
        anyhow::ensure!(session.generation == generation, "Network start was superseded");
        let (commands, rx) = mpsc::channel();
        let wake = Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?);
        let worker_wake = wake.clone();
        let cancel_wake = Arc::new(eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK)?);
        let worker_cancel_wake = cancel_wake.clone();
        let model = Arc::new(Mutex::new(Model::default()));
        let state = model.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let worker = thread::Builder::new()
            .name("network-eis".into())
            .spawn(move || {
                async_io::block_on(async {
                    if let Err(e) = receive(shared, rx, worker_wake, worker_cancel_wake, state.clone(), cancel, generation).await {
                        let text = format!("{e:#}");
                        state.lock().unwrap().error = Some(text.clone());
                        with_session(generation, |session| session.failed = Some(text));
                    }
                });
            })?;
        Ok(Self {
            generation,
            clipboard: crate::clipboard::Clipboard::default(),
            commands,
            wake,
            cancel_wake,
            model,
            worker: Mutex::new(Some(worker)),
            send: support.send,
            capturing: AtomicBool::new(false),
            cancelled,
        })
    }
}
impl Wayland {
    fn command(&self, command: Command) {
        if self.commands.send(command).is_ok() {
            let _ = rustix::io::write(&*self.wake, &1u64.to_ne_bytes());
        }
    }
}
impl Platform for Wayland {
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
        self.model.lock().unwrap().displays.clone()
    }
    fn cursor(&self) -> Option<(f64, f64)> {
        // Only while controlled from elsewhere is the position known (it is where this
        // app moved the pointer); netcore needs it then to find the way back. Otherwise
        // the edges come from the InputCapture barriers (take_edge_hit), and an old
        // position left at the edge would look like a push against it.
        if remote_controlled() { self.model.lock().unwrap().cursor } else { None }
    }
    fn warp(&self, x: f64, y: f64) {
        with_session(self.generation, |_| {
            if REMOTE.swap(false, Ordering::AcqRel) {
                *REMOTE_ENDED.lock().unwrap() = Some(Instant::now());
            }
        });
        self.model.lock().unwrap().cursor = Some((x, y));
        self.command(Command::Warp(x, y));
    }
    fn arm_edges(&self, sides: &[Side]) {
        with_session(self.generation, |_| {
            capture().lock().unwrap().sides = if self.send { sides.to_vec() } else { vec![] };
        });
    }
    fn control_down(&self) -> bool { capture().lock().unwrap().control }
    fn take_edge_hit(&self) -> Option<(Side, f64)> {
        with_session(self.generation, |_| {
            let mut c = capture().lock().unwrap();
            if crate::net::network_edges_allowed() { c.hit.take() } else { c.hit = None; None }
        }).flatten()
    }
    fn place_on_return(&self, side: Side, pos: f64) {
        with_session(self.generation, |_| {
            let mut c = capture().lock().unwrap();
            // The portal writes its zones separately from the receiver's device regions.
            c.position = flock_net::geometry::edge_position(&capture_displays(), side, pos);
        });
    }
    fn place_on_return_display(&self, side: Side, display: u8, pos: f64) {
        with_session(self.generation, |_| {
            let mut c = capture().lock().unwrap();
            // Same zones as place_on_return, for one monitor of several.
            c.position = flock_net::geometry::display_edge_position(&capture_displays(), display, side, pos);
        });
    }
    fn start_capture(&self, sink: Sender<InputEvent>) -> anyhow::Result<()> {
        let session = SESSION.lock().unwrap();
        anyhow::ensure!(session.generation == self.generation, "Network session was superseded");
        let mut c = capture().lock().unwrap();
        anyhow::ensure!(crate::net::network_edges_allowed(), "Network edges are paused");
        anyhow::ensure!(self.send && c.active, "GNOME InputCapture is not active");
        for e in c.pending.drain(..) {
            let _ = sink.send(e);
        }
        c.sink = Some(sink);
        self.capturing.store(true, Ordering::Release);
        Ok(())
    }
    fn stop_capture(&self) {
        self.capturing.store(false, Ordering::Release);
        with_session(self.generation, |_| {
            let mut c = capture().lock().unwrap();
            c.sink = None;
            c.hit = None;
            c.deferred = None;
            c.pending.clear();
            if c.active {
                c.release = Some(Instant::now() + Duration::from_millis(20));
            }
        });
    }
    fn inject(&self, event: &InputEvent) {
        if matches!(event, InputEvent::Motion { .. } | InputEvent::Scroll { .. } | InputEvent::Key { down: true, .. } | InputEvent::Button { down: true, .. }) {
            with_session(self.generation, |_| REMOTE.store(true, Ordering::Release));
        }
        self.command(Command::Input(event.clone()));
    }
    fn ready(&self) -> Result<(), String> {
        if self.capturing.load(Ordering::Acquire) && !capture().lock().unwrap().active {
            return Err("InputCapture ended; input was returned locally".into());
        }
        let m = self.model.lock().unwrap();
        if let Some(e) = &m.error {
            Err(e.clone())
        } else if m.pointer && m.keyboard && !m.displays.is_empty() {
            Ok(())
        } else {
            Err("Waiting for RemoteDesktop pointer and keyboard permission".into())
        }
    }
}
impl Drop for Wayland {
    fn drop(&mut self) {
        disable_session(self.generation);
        self.cancelled.store(true, Ordering::Release);
        let _ = rustix::io::write(&*self.cancel_wake, &1u64.to_ne_bytes());
        self.command(Command::Quit);
        if let Some(h) = self.worker.lock().unwrap().take() {
            let _ = h.join();
        }
    }
}
pub fn zones() -> &'static Mutex<Vec<Rect>> {
    static ZONES: OnceLock<Mutex<Vec<Rect>>> = OnceLock::new();
    ZONES.get_or_init(|| Mutex::new(Vec::new()))
}
fn capture_displays() -> Vec<Rect> {
    zones().lock().unwrap().clone()
}
async fn receive(
    shared: Arc<Shared>,
    rx: mpsc::Receiver<Command>,
    wake: Arc<OwnedFd>,
    cancel_wake: Arc<OwnedFd>,
    model: Arc<Mutex<Model>>,
    cancel: Arc<AtomicBool>,
    generation: u64,
) -> anyhow::Result<()> {
    use ashpd::desktop::{
        remote_desktop::{self as rd, DeviceType, RemoteDesktop},
        CreateSessionOptions, PersistMode,
    };
    let (portal, session) = crate::edge_wayland::within(Duration::from_secs(5), async {
        let portal = RemoteDesktop::with_connection(crate::edge_wayland::portal_connection().await?).await?;
        let session = portal
            .create_session(CreateSessionOptions::default())
            .await?;
        Ok::<_, ashpd::Error>((portal, session))
    })
    .await
    .ok_or_else(|| {
        with_session(generation, |session| session.no_answer = true);
        anyhow::anyhow!("RemoteDesktop did not answer")
    })??;
    // Set once Start has answered, for what becomes of the saved token if this ends first.
    let path = crate::config::Config::path().with_file_name("remote-desktop.token");
    let token = crate::net_logic::load_restore_token(&path);
    let token_settled = std::cell::Cell::new(false);
    let result = async {
        log::info!(
            "remote-desktop: SelectDevices (pointer, keyboard; remember until revoked, {})",
            if token.is_some() { "with the saved restore token: the system should not ask" } else { "no restore token saved: the system asks" }
        );
        set_remote_status(&shared, generation, if token.is_some() { "restoring" } else { "asking" });
        portal
            .select_devices(
                &session,
                rd::SelectDevicesOptions::default()
                    .set_devices(DeviceType::Pointer | DeviceType::Keyboard)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(token.as_deref()),
            )
            .await?
            .response()?;
        let response = portal
            .start(&session, None, rd::StartOptions::default())
            .await?
            .response()?;
        let action = crate::net_logic::token_after_start(token.as_deref(), true, response.restore_token());
        token_settled.set(true);
        log::info!("{}", crate::net_logic::token_log("remote-desktop", token.is_some(), &action));
        set_remote_status(&shared, generation, if matches!(action, crate::net_logic::TokenAfterStart::Save(_)) { "remembered" } else { "not-remembered" });
        if let Err(e) = crate::net_logic::apply_token(&path, &action) {
            log::warn!("remote-desktop: cannot update the restore token file: {e}");
        }
        anyhow::ensure!(
            response.devices().contains(DeviceType::Pointer)
                && response.devices().contains(DeviceType::Keyboard),
            "RemoteDesktop requires pointer and keyboard permission"
        );
        let fd = portal
            .connect_to_eis(&session, rd::ConnectToEISOptions::default())
            .await?;
        let context = ei::Context::new(UnixStream::from(fd))?;
        let (connection, events) = context
            .handshake_async_io("flockswitch", ei::handshake::ContextType::Sender)
            .await?;
        anyhow::ensure!(with_session(generation, |session| session.opened = true).is_some(), "Network start was superseded");
        enum Ev {
            Ei(Result<EiEvent, reis::Error>),
            Gone,
            Tick,
        }
        let mut events = Box::pin(stream::select(
            events
                .map(Ev::Ei)
                .chain(stream::once(async { Ev::Gone }))
                .boxed_local(),
            stream::unfold(async_io::Async::new(wake)?, |wake| async move {
                wake.read_with(|fd| {
                    rustix::io::read(fd, &mut [0u8; 8]).map_err(std::io::Error::from)
                })
                .await
                .ok()?;
                Some((Ev::Tick, wake))
            }),
        ));
        let mut emulation = Emulation {
            connection: connection.clone(),
            model: model.clone(),
            devices: vec![],
            keys: HashSet::new(),
            buttons: HashSet::new(),
        };
        let mut sequence = 0u32;
        let mut scroll_active: Option<Instant> = None;
        loop {
            // A scroll that stops is ended after a pause; that is the only timer.
            let next = match scroll_active {
                Some(t) => {
                    let end = async_io::Timer::at(t + Duration::from_millis(150));
                    match futures_util::future::select(events.next(), Box::pin(end)).await {
                        futures_util::future::Either::Left((event, _)) => event,
                        futures_util::future::Either::Right(_) => Some(Ev::Tick),
                    }
                }
                None => events.next().await,
            };
            let Some(event) = next else { break };
            if shared.stopped() || !shared.config().network.on || !current_start(generation) {
                break;
            }
            match event {
                Ev::Ei(Ok(EiEvent::SeatAdded(e))) => e.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Button
                        | DeviceCapability::Scroll
                        | DeviceCapability::Keyboard,
                ),
                Ev::Ei(Ok(EiEvent::DeviceResumed(e))) => {
                    sequence = sequence.wrapping_add(1);
                    e.device
                        .device()
                        .start_emulating(connection.serial(), sequence);
                    if !emulation.devices.contains(&e.device) {
                        emulation.devices.push(e.device);
                    }
                    update_devices(&emulation.devices, &model);
                }
                Ev::Ei(Ok(EiEvent::DevicePaused(e))) => {
                    emulation.devices.retain(|d| d != &e.device);
                    update_devices(&emulation.devices, &model);
                }
                Ev::Ei(Ok(EiEvent::DeviceRemoved(e))) => {
                    emulation.devices.retain(|d| d != &e.device);
                    update_devices(&emulation.devices, &model);
                }
                Ev::Ei(Ok(EiEvent::Disconnected(d))) => {
                    anyhow::bail!("EIS disconnected: {:?}", d.reason)
                }
                Ev::Ei(Err(e)) => anyhow::bail!("EIS: {e}"),
                Ev::Gone => anyhow::bail!("RemoteDesktop EIS connection closed"),
                _ => {}
            }
            // A full batch means more may be queued: go on at once, so a key-up never
            // waits for the next input event.
            loop {
                let mut taken = 0;
                for command in rx.try_iter().take(512) {
                    taken += 1;
                    match command {
                        Command::Quit => return Ok(()),
                        Command::Warp(x, y) => move_pointer(&emulation.devices, &model, x, y),
                        Command::Input(event) => {
                            if matches!(event, InputEvent::Scroll { .. }) {
                                scroll_active = Some(Instant::now());
                            }
                            let changed = match event {
                                InputEvent::Key { usage, down } => {
                                    if down {
                                        emulation.keys.insert(usage)
                                    } else {
                                        emulation.keys.remove(&usage)
                                    }
                                }
                                InputEvent::Button { button, down } => {
                                    if down {
                                        emulation.buttons.insert(button)
                                    } else {
                                        emulation.buttons.remove(&button)
                                    }
                                }
                                _ => true,
                            };
                            if changed {
                                inject(&emulation.devices, &model, &event);
                            }
                        }
                    }
                    for d in &emulation.devices {
                        d.device().frame(connection.serial(), monotonic_micros());
                    }
                }
                if taken < 512 {
                    break;
                }
            }
            if scroll_active.is_some_and(|t| t.elapsed() >= Duration::from_millis(150)) {
                for d in &emulation.devices {
                    if let Some(scroll) = d.interface::<ei::Scroll>() {
                        scroll.scroll_stop(1, 1, 0);
                        d.device().frame(connection.serial(), monotonic_micros());
                    }
                }
                scroll_active = None;
            }
            connection.flush()?;
        }
        drop(emulation);
        Ok(())
    };
    let cancelled = async {
        // Dropping the platform writes the eventfd; it is never read, so it stays readable.
        if let Ok(fd) = async_io::Async::new(cancel_wake) {
            while !cancel.load(Ordering::Acquire) && !shared.stopped() {
                if fd.readable().await.is_err() {
                    break;
                }
            }
        }
    };
    let result = match futures_util::future::select(Box::pin(result), Box::pin(cancelled)).await {
        futures_util::future::Either::Left((result, _)) => result,
        futures_util::future::Either::Right(((), _)) => Ok(()),
    };
    if !token_settled.get() {
        // Cancelled, denied or cut off before Start answered: a token it was asked with
        // is spent on the portal's side.
        let action = crate::net_logic::token_after_start(token.as_deref(), false, None);
        if action != crate::net_logic::TokenAfterStart::Keep {
            log::info!("remote-desktop: the session ended before Start answered; the saved restore token is spent and removed (the system asks next time)");
            if let Err(e) = crate::net_logic::apply_token(&path, &action) {
                log::warn!("remote-desktop: cannot remove the restore token file: {e}");
            }
        }
        set_remote_status(&shared, generation, "");
    }
    log::info!("remote-desktop: closing the session");
    let _ = crate::edge_wayland::within(Duration::from_secs(2), session.close()).await;
    result
}

/// The RemoteDesktop permission for the settings window: "asking", "restoring",
/// "remembered", "not-remembered", or empty.
fn set_remote_status(shared: &Shared, generation: u64, note: &str) {
    if current_start(generation) {
        let note = note.to_string();
        shared.set_status(|st| st.remote_desktop = note);
    }
}
struct Emulation {
    connection: reis::event::Connection,
    model: Arc<Mutex<Model>>,
    devices: Vec<Device>,
    keys: HashSet<u16>,
    buttons: HashSet<flock_net::MouseButton>,
}
impl Drop for Emulation {
    fn drop(&mut self) {
        for usage in self.keys.drain() {
            inject(
                &self.devices,
                &self.model,
                &InputEvent::Key { usage, down: false },
            );
        }
        for button in self.buttons.drain() {
            inject(
                &self.devices,
                &self.model,
                &InputEvent::Button {
                    button,
                    down: false,
                },
            );
        }
        for d in &self.devices {
            if let Some(scroll) = d.interface::<ei::Scroll>() {
                scroll.scroll_stop(1, 1, 0);
            }
            d.device()
                .frame(self.connection.serial(), monotonic_micros());
            d.device().stop_emulating(self.connection.serial());
        }
        let _ = self.connection.flush();
    }
}

fn update_devices(devices: &[Device], model: &Mutex<Model>) {
    let mut m = model.lock().unwrap();
    m.pointer = devices.iter().any(|d| {
        d.has_capability(DeviceCapability::Pointer)
            || d.has_capability(DeviceCapability::PointerAbsolute)
    });
    m.keyboard = devices
        .iter()
        .any(|d| d.has_capability(DeviceCapability::Keyboard));
    let regions: Vec<_> = devices
        .iter()
        .filter(|d| d.has_capability(DeviceCapability::PointerAbsolute))
        .flat_map(|d| d.regions())
        .map(|r| {
            Rect::new(
                f64::from(r.x),
                f64::from(r.y),
                f64::from(r.width),
                f64::from(r.height),
            )
        })
        .collect();
    if !regions.is_empty() {
        m.displays = regions;
    } else if m.displays.is_empty() {
        // Relative-only EIS devices use the compositor's advertised output layout.
        if let Ok(wl) = crate::wl::Wl::connect(false, false) {
            m.displays = wl
                .displays()
                .iter()
                .map(|d| d.rect)
                .map(|r| Rect::new(r.x.into(), r.y.into(), r.w.into(), r.h.into()))
                .collect();
        }
    }
}
fn move_pointer(devices: &[Device], model: &Mutex<Model>, x: f64, y: f64) {
    let mut m = model.lock().unwrap();
    let Some((x, y)) = flock_net::geometry::clamp(&m.displays, x, y) else {
        return;
    };
    let old = m.cursor.unwrap_or((x, y));
    m.cursor = Some((x, y));
    if let Some(p) = devices
        .iter()
        .find(|d| {
            d.regions().iter().any(|r| {
                x >= f64::from(r.x)
                    && y >= f64::from(r.y)
                    && x < f64::from(r.x + r.width)
                    && y < f64::from(r.y + r.height)
            })
        })
        .and_then(|d| d.interface::<ei::PointerAbsolute>())
    {
        p.motion_absolute(x as f32, y as f32);
    } else if let Some(p) = devices.iter().find_map(|d| d.interface::<ei::Pointer>()) {
        p.motion_relative((x - old.0) as f32, (y - old.1) as f32);
    }
}
fn inject(devices: &[Device], model: &Mutex<Model>, event: &InputEvent) {
    match *event {
        InputEvent::Motion { dx, dy } => {
            let old = model.lock().unwrap().cursor.unwrap_or((0.0, 0.0));
            move_pointer(devices, model, old.0 + dx, old.1 + dy);
        }
        InputEvent::Button { button, down } => {
            if let Some(b) = devices.iter().find_map(|d| d.interface::<ei::Button>()) {
                b.button(
                    input::evdev_button(button),
                    if down {
                        ei::button::ButtonState::Press
                    } else {
                        ei::button::ButtonState::Released
                    },
                );
            }
        }
        InputEvent::Key { usage, down } => {
            if let (Some(k), Some(code)) = (
                devices.iter().find_map(|d| d.interface::<ei::Keyboard>()),
                input::hid_to_evdev(usage),
            ) {
                k.key(
                    code,
                    if down {
                        ei::keyboard::KeyState::Press
                    } else {
                        ei::keyboard::KeyState::Released
                    },
                );
            }
        }
        InputEvent::Scroll { dx, dy } => {
            if let Some(s) = devices.iter().find_map(|d| d.interface::<ei::Scroll>()) {
                s.scroll((-dx * 10.0) as f32, (-dy * 10.0) as f32);
            }
        }
    }
}

fn monotonic_micros() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    now.tv_sec as u64 * 1_000_000 + now.tv_nsec as u64 / 1000
}
