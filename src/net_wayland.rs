//! InputCapture bridge and RemoteDesktop EIS sender.
use crate::shared::Shared;
use flock_net::{linux_input as input, InputEvent, Os, Platform, Rect, Side};
use futures_util::{stream, StreamExt};
use reis::{
    ei,
    event::{Device, DeviceCapability, EiEvent},
};
use std::{
    collections::HashSet,
    os::unix::net::UnixStream,
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
    let mut c = capture().lock().unwrap();
    c.sides.clear();
    c.sink = None;
    c.hit = None;
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
pub struct Wayland {
    clipboard: crate::clipboard::Clipboard,
    commands: Sender<Command>,
    model: Arc<Mutex<Model>>,
    worker: Mutex<Option<JoinHandle<()>>>,
    send: bool,
    capturing: AtomicBool,
    cancelled: Arc<AtomicBool>,
}
impl Wayland {
    pub fn new(shared: Arc<Shared>) -> anyhow::Result<Self> {
        let support = crate::net::probe();
        anyhow::ensure!(support.receive, "{}", support.reason);
        anyhow::ensure!(
            !shared.stopped() && shared.config().network.on,
            "Network sharing was disabled"
        );
        let (commands, rx) = mpsc::channel();
        let model = Arc::new(Mutex::new(Model::default()));
        let state = model.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let worker = thread::Builder::new()
            .name("network-eis".into())
            .spawn(move || {
                async_io::block_on(async {
                    if let Err(e) = receive(shared, rx, state.clone(), cancel).await {
                        state.lock().unwrap().error = Some(format!("{e:#}"));
                    }
                });
            })?;
        Ok(Self {
            clipboard: crate::clipboard::Clipboard::default(),
            commands,
            model,
            worker: Mutex::new(Some(worker)),
            send: support.send,
            capturing: AtomicBool::new(false),
            cancelled,
        })
    }
}
impl Platform for Wayland {
    fn clipboard_text(&self) -> Option<String> { self.clipboard.text() }
    fn set_clipboard_text(&self, text: &str) { self.clipboard.set_text(text); }
    fn clipboard_files(&self) -> Vec<std::path::PathBuf> { self.clipboard.files() }
    fn set_clipboard_files(&self, files: &[std::path::PathBuf]) { self.clipboard.set_files(files); }
    fn clipboard_image_png(&self) -> Option<Vec<u8>> { self.clipboard.image() }
    fn set_clipboard_image_png(&self, png: &[u8]) { self.clipboard.set_image(png); }
    fn received_dir(&self) -> Option<std::path::PathBuf> { Some(dirs::download_dir()?.join("flockswitch")) }
    fn os(&self) -> Os {
        Os::Linux
    }
    fn displays(&self) -> Vec<Rect> {
        self.model.lock().unwrap().displays.clone()
    }
    fn cursor(&self) -> Option<(f64, f64)> {
        if !crate::net::network_edges_allowed() { return None; }
        if capture().lock().unwrap().active {
            None
        } else {
            self.model.lock().unwrap().cursor
        }
    }
    fn warp(&self, x: f64, y: f64) {
        self.model.lock().unwrap().cursor = Some((x, y));
        let _ = self.commands.send(Command::Warp(x, y));
    }
    fn arm_edges(&self, sides: &[Side]) {
        capture().lock().unwrap().sides = if self.send { sides.to_vec() } else { vec![] };
    }
    fn control_down(&self) -> bool { capture().lock().unwrap().control }
    fn take_edge_hit(&self) -> Option<(Side, f64)> {
        let mut c = capture().lock().unwrap();
        if crate::net::network_edges_allowed() { c.hit.take() } else { c.hit = None; None }
    }
    fn place_on_return(&self, side: Side, pos: f64) {
        let mut c = capture().lock().unwrap();
        // The portal writes its zones separately from the receiver's device regions.
        c.position = flock_net::geometry::edge_position(&capture_displays(), side, pos);
    }
    fn start_capture(&self, sink: Sender<InputEvent>) -> anyhow::Result<()> {
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
        let mut c = capture().lock().unwrap();
        c.sink = None;
        c.hit = None;
        c.pending.clear();
        if c.active {
            c.release = Some(Instant::now() + Duration::from_millis(20));
        }
    }
    fn inject(&self, event: &InputEvent) {
        let _ = self.commands.send(Command::Input(event.clone()));
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
        disable();
        self.cancelled.store(true, Ordering::Release);
        let _ = self.commands.send(Command::Quit);
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
    model: Arc<Mutex<Model>>,
    cancel: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    use ashpd::desktop::{
        remote_desktop::{self as rd, DeviceType, RemoteDesktop},
        CreateSessionOptions, PersistMode,
    };
    let (portal, session) = crate::edge_wayland::within(Duration::from_secs(5), async {
        let portal = RemoteDesktop::new().await?;
        let session = portal
            .create_session(CreateSessionOptions::default())
            .await?;
        Ok::<_, ashpd::Error>((portal, session))
    })
    .await
    .ok_or_else(|| anyhow::anyhow!("RemoteDesktop did not answer"))??;
    let result = async {
        let path = crate::config::Config::path().with_file_name("remote-desktop.token");
        let token = std::fs::read_to_string(&path).ok();
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
        anyhow::ensure!(
            response.devices().contains(DeviceType::Pointer)
                && response.devices().contains(DeviceType::Keyboard),
            "RemoteDesktop requires pointer and keyboard permission"
        );
        if let Some(token) = response.restore_token() {
            use std::{
                io::Write,
                os::unix::fs::{OpenOptionsExt, PermissionsExt},
            };
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            f.write_all(token.as_bytes())?;
        }
        let fd = portal
            .connect_to_eis(&session, rd::ConnectToEISOptions::default())
            .await?;
        let context = ei::Context::new(UnixStream::from(fd))?;
        let (connection, events) = context
            .handshake_async_io("flockswitch", ei::handshake::ContextType::Sender)
            .await?;
        enum Ev {
            Ei(Result<EiEvent, reis::Error>),
            Gone,
            Tick,
        }
        let mut events = stream::select(
            events
                .map(Ev::Ei)
                .chain(stream::once(async { Ev::Gone }))
                .boxed_local(),
            async_io::Timer::interval(Duration::from_millis(4)).map(|_| Ev::Tick),
        );
        let mut emulation = Emulation {
            connection: connection.clone(),
            model: model.clone(),
            devices: vec![],
            keys: HashSet::new(),
            buttons: HashSet::new(),
        };
        let mut sequence = 0u32;
        let mut scroll_active: Option<Instant> = None;
        while let Some(event) = events.next().await {
            if shared.stopped() || !shared.config().network.on {
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
            for command in rx.try_iter().take(512) {
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
        loop {
            if cancel.load(Ordering::Acquire) || shared.stopped() {
                break;
            }
            async_io::Timer::after(Duration::from_millis(25)).await;
        }
    };
    let result = match futures_util::future::select(Box::pin(result), Box::pin(cancelled)).await {
        futures_util::future::Either::Left((result, _)) => result,
        futures_util::future::Either::Right(((), _)) => Ok(()),
    };
    let _ = crate::edge_wayland::within(Duration::from_secs(2), session.close()).await;
    result
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
