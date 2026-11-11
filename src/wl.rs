//! A small Wayland client for what edge switching puts on the screen there: the
//! 1-pixel strips along the edges that notice the pointer (edge_layer.rs) and the
//! click-through countdown (overlay_wayland.rs), both wlr-layer-shell surfaces. Each
//! user opens its own connection on its own thread. libwayland-client is loaded at run
//! time (wayland-dlopen), so on a computer without it `connect` just fails.

use crate::edge_logic::Zone;
use memmap2::MmapMut;
use std::fs::File;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::{wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

pub use zwlr_layer_surface_v1::Anchor;

/// What one output says about itself.
#[derive(Debug, Clone, Default)]
pub struct OutputInfo {
    pub name: String,
    pub scale: i32,
    /// wl_output's position and current mode (physical pixels), and its transform.
    position: (i32, i32),
    mode: (i32, i32),
    rotated: bool,
    /// xdg-output's logical position and size, when the compositor has it.
    logical_position: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
}

impl OutputInfo {
    /// Where the output is in the desktop, in logical pixels.
    pub fn rect(&self) -> Option<Zone> {
        let (x, y) = self.logical_position.unwrap_or(self.position);
        let (w, h) = self.logical_size.or_else(|| {
            let scale = self.scale.max(1);
            let (w, h) = (self.mode.0 / scale, self.mode.1 / scale);
            (w > 0 && h > 0).then_some(if self.rotated { (h, w) } else { (w, h) })
        })?;
        (w > 0 && h > 0).then_some(Zone { x, y, w, h })
    }
}

struct Output {
    global: u32,
    output: wl_output::WlOutput,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    info: Arc<Mutex<OutputInfo>>,
}

/// A display as the strips and the countdown use it.
#[derive(Debug, Clone)]
pub struct Display {
    pub global: u32,
    pub rect: Zone,
    pub scale: i32,
    pub name: String,
}

/// What the pointer did over our surfaces.
#[derive(Debug, Clone, PartialEq)]
pub enum PointerEvent {
    Enter { surface: ObjectId, x: f64, y: f64 },
    Motion { x: f64, y: f64 },
    Leave { surface: ObjectId },
    /// A button or the wheel: someone is using the edge, not resting there.
    Press,
}

/// What the keyboard did while one of our surfaces had it (evdev key codes).
#[derive(Debug, Clone, PartialEq)]
pub enum KeyEvent {
    Enter { keys: Vec<u32> },
    Leave,
    Key { code: u32, down: bool },
    Modifiers { depressed: u32 },
}

#[derive(Debug, Default)]
pub struct LayerState {
    pub configured: Option<(u32, u32)>,
    pub closed: bool,
}

pub struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    xdg_outputs: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    cursor_shape: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    seats: Vec<Seat>,
    shapes: Vec<(ObjectId, wp_cursor_shape_device_v1::WpCursorShapeDeviceV1)>,
    outputs: Vec<Output>,
    want_pointer: bool,
    /// Outputs came, went or changed since this was last cleared.
    pub outputs_changed: bool,
    pub pointer_events: Vec<PointerEvent>,
    pub key_events: Vec<KeyEvent>,
}

struct Seat {
    global: u32,
    seat: wl_seat::WlSeat,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
}

pub struct Wl {
    conn: Connection,
    queue: EventQueue<State>,
    pub qh: QueueHandle<State>,
    pub state: State,
}

impl Wl {
    /// Connects and reads the globals and the outputs. `want_pointer`: also follow the
    /// pointer over our surfaces.
    pub fn connect(want_pointer: bool) -> Result<Wl, String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("{e}"))?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State {
            compositor: None,
            shm: None,
            layer_shell: None,
            xdg_outputs: None,
            cursor_shape: None,
            seats: Vec::new(),
            shapes: Vec::new(),
            outputs: Vec::new(),
            want_pointer,
            outputs_changed: true,
            pointer_events: Vec::new(),
            key_events: Vec::new(),
        };
        // Globals, then what the outputs (and their xdg-outputs) say.
        for _ in 0..2 {
            queue.roundtrip(&mut state).map_err(|e| format!("{e}"))?;
        }
        Ok(Wl { conn, queue, qh, state })
    }

    pub fn has_layer_shell(&self) -> bool {
        self.state.layer_shell.is_some() && self.state.compositor.is_some() && self.state.shm.is_some()
    }

    pub fn has_cursor_shape(&self) -> bool {
        self.state.cursor_shape.is_some()
    }

    pub fn has_xdg_output(&self) -> bool {
        self.state.xdg_outputs.is_some()
    }

    /// The outputs whose place in the desktop is known.
    pub fn displays(&self) -> Vec<Display> {
        self.state
            .outputs
            .iter()
            .filter_map(|o| {
                let info = o.info.lock().unwrap().clone();
                Some(Display { global: o.global, rect: info.rect()?, scale: info.scale.max(1), name: info.name })
            })
            .collect()
    }

    /// Sends what is queued (a surface taken away, for one).
    pub fn flush(&self) {
        if let Err(e) = self.conn.flush() {
            log::debug!("wayland: flush: {e}");
        }
    }

    /// Sends what is queued, waits up to `timeout` for events and handles them.
    pub fn dispatch(&mut self, timeout: Duration) -> Result<(), String> {
        let err = |e: &dyn std::fmt::Display| format!("{e}");
        self.queue.dispatch_pending(&mut self.state).map_err(|e| err(&e))?;
        self.conn.flush().map_err(|e| err(&e))?;
        if let Some(guard) = self.queue.prepare_read() {
            let readable = {
                let fd = guard.connection_fd();
                let mut fds = [rustix::event::PollFd::from_borrowed_fd(fd, rustix::event::PollFlags::IN)];
                let ts = rustix::event::Timespec::try_from(timeout).ok();
                match rustix::event::poll(&mut fds, ts.as_ref()) {
                    Ok(n) => n > 0,
                    Err(rustix::io::Errno::INTR) => false,
                    Err(e) => return Err(format!("poll: {e}")),
                }
            };
            if readable {
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(err(&e)),
                }
            }
        }
        self.queue.dispatch_pending(&mut self.state).map_err(|e| err(&e))?;
        self.conn.flush().map_err(|e| err(&e))?;
        Ok(())
    }

    fn output(&self, global: u32) -> Option<&wl_output::WlOutput> {
        self.state.outputs.iter().find(|o| o.global == global).map(|o| &o.output)
    }

    /// A surface on the overlay layer of `output`, anchored by `anchor` with `margin`
    /// (top, right, bottom, left) and `size` in logical pixels. It never takes the
    /// keyboard, nothing moves to make room for it, and with `input` false every click
    /// goes through it.
    pub fn layer(&mut self, output: u32, namespace: &str, anchor: Anchor, margin: (i32, i32, i32, i32), size: (u32, u32), input: bool) -> Option<Layer> {
        let (Some(compositor), Some(shell)) = (self.state.compositor.as_ref(), self.state.layer_shell.as_ref()) else { return None };
        let output = self.output(output)?.clone();
        let surface = compositor.create_surface(&self.qh, ());
        let state = Arc::new(Mutex::new(LayerState::default()));
        let layer = shell.get_layer_surface(&surface, Some(&output), zwlr_layer_shell_v1::Layer::Overlay, namespace.into(), &self.qh, state.clone());
        layer.set_anchor(anchor);
        layer.set_size(size.0, size.1);
        layer.set_margin(margin.0, margin.1, margin.2, margin.3);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
        if !input {
            let region = compositor.create_region(&self.qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
        }
        surface.commit();
        Some(Layer { surface, layer, state, buffers: Vec::new(), scale: 1 })
    }

    /// A shared-memory buffer of `w`×`h` pixels (ARGB, premultiplied).
    fn buffer(&self, w: i32, h: i32) -> Option<ShmBuffer> {
        let shm = self.state.shm.as_ref()?;
        let stride = w.checked_mul(4)?;
        let size = stride.checked_mul(h)?;
        if size <= 0 {
            return None;
        }
        let fd = rustix::fs::memfd_create("flockswitch", rustix::fs::MemfdFlags::CLOEXEC).ok()?;
        let file = File::from(fd);
        file.set_len(size as u64).ok()?;
        // Safety: the file was just made for this buffer and is only resized above.
        let map = unsafe { MmapMut::map_mut(&file) }.ok()?;
        let pool = shm.create_pool(file.as_fd(), size, &self.qh, ());
        let busy = Arc::new(AtomicBool::new(false));
        let buffer = pool.create_buffer(0, w, h, stride, wl_shm::Format::Argb8888, &self.qh, busy.clone());
        pool.destroy();
        Some(ShmBuffer { buffer, busy, map, width: w, height: h })
    }
}

struct ShmBuffer {
    buffer: wl_buffer::WlBuffer,
    /// The compositor still reads it.
    busy: Arc<AtomicBool>,
    map: MmapMut,
    width: i32,
    height: i32,
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

/// A layer-shell surface with its buffers.
pub struct Layer {
    pub surface: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    state: Arc<Mutex<LayerState>>,
    buffers: Vec<ShmBuffer>,
    scale: i32,
}

impl Layer {
    /// The size the compositor gave it (logical pixels), once it has.
    pub fn configured(&self) -> Option<(u32, u32)> {
        self.state.lock().unwrap().configured
    }

    pub fn closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// Takes the keyboard (exclusive) or leaves it to the windows (none).
    pub fn set_keyboard(&self, take: bool) {
        use zwlr_layer_surface_v1::KeyboardInteractivity;
        self.layer.set_keyboard_interactivity(if take { KeyboardInteractivity::Exclusive } else { KeyboardInteractivity::None });
        self.surface.commit();
    }

    pub fn id(&self) -> ObjectId {
        self.surface.id()
    }

    /// Draws a frame at `scale` buffer pixels per logical pixel: `paint` fills the BGRA
    /// bytes (premultiplied, as wl_shm's ARGB8888 lies in memory). False when it cannot
    /// yet (not configured, or both buffers still on the screen).
    pub fn draw(&mut self, wl: &Wl, scale: i32, paint: impl FnOnce(&mut [u8], i32, i32)) -> bool {
        let Some((w, h)) = self.configured() else { return false };
        let scale = scale.max(1);
        let (bw, bh) = (w as i32 * scale, h as i32 * scale);
        if bw <= 0 || bh <= 0 {
            return false;
        }
        self.buffers.retain(|b| (b.width, b.height) == (bw, bh) || b.busy.load(Ordering::Relaxed));
        let free = self.buffers.iter().position(|b| !b.busy.load(Ordering::Relaxed) && (b.width, b.height) == (bw, bh));
        let index = match free {
            Some(i) => i,
            None if self.buffers.len() < 2 => {
                let Some(b) = wl.buffer(bw, bh) else { return false };
                self.buffers.push(b);
                self.buffers.len() - 1
            }
            None => return false,
        };
        let b = &mut self.buffers[index];
        paint(&mut b.map, bw, bh);
        b.busy.store(true, Ordering::Relaxed);
        if scale != self.scale {
            self.surface.set_buffer_scale(scale);
            self.scale = scale;
        }
        self.surface.attach(Some(&b.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, bw, bh);
        self.surface.commit();
        true
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        // The buffers go after the surface that shows them.
        self.layer.destroy();
        self.surface.destroy();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(state: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match event {
            wl_registry::Event::Global { name, interface, version } => match interface.as_str() {
                "wl_compositor" if version >= 4 => state.compositor = Some(registry.bind(name, version.min(5), qh, ())),
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "zwlr_layer_shell_v1" => state.layer_shell = Some(registry.bind(name, version.min(4), qh, ())),
                "zxdg_output_manager_v1" => {
                    let manager: zxdg_output_manager_v1::ZxdgOutputManagerV1 = registry.bind(name, version.min(3), qh, ());
                    for o in state.outputs.iter_mut().filter(|o| o.xdg.is_none()) {
                        o.xdg = Some(manager.get_xdg_output(&o.output, qh, o.info.clone()));
                    }
                    state.xdg_outputs = Some(manager);
                }
                "wp_cursor_shape_manager_v1" if state.want_pointer => state.cursor_shape = Some(registry.bind(name, 1, qh, ())),
                "wl_seat" if state.want_pointer => {
                    let seat: wl_seat::WlSeat = registry.bind(name, version.min(5), qh, ());
                    state.seats.push(Seat { global: name, seat, pointer: None, keyboard: None });
                }
                "wl_output" if version >= 2 => {
                    let info = Arc::new(Mutex::new(OutputInfo { scale: 1, ..OutputInfo::default() }));
                    let output: wl_output::WlOutput = registry.bind(name, version.min(4), qh, info.clone());
                    let xdg = state.xdg_outputs.as_ref().map(|m| m.get_xdg_output(&output, qh, info.clone()));
                    state.outputs.push(Output { global: name, output, xdg, info });
                    state.outputs_changed = true;
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.outputs.iter().position(|o| o.global == name) {
                    let o = state.outputs.remove(i);
                    if let Some(xdg) = o.xdg {
                        xdg.destroy();
                    }
                    if o.output.version() >= 3 {
                        o.output.release();
                    }
                    state.outputs_changed = true;
                }
                state.seats.retain(|s| s.global != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, Arc<Mutex<OutputInfo>>> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, info: &Arc<Mutex<OutputInfo>>, _: &Connection, _: &QueueHandle<Self>) {
        let mut info = info.lock().unwrap();
        match event {
            wl_output::Event::Geometry { x, y, transform, .. } => {
                info.position = (x, y);
                info.rotated = matches!(
                    transform,
                    WEnum::Value(
                        wl_output::Transform::_90 | wl_output::Transform::_270 | wl_output::Transform::Flipped90 | wl_output::Transform::Flipped270
                    )
                );
            }
            wl_output::Event::Mode { flags, width, height, .. } => {
                if matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current)) {
                    info.mode = (width, height);
                }
            }
            wl_output::Event::Scale { factor } => info.scale = factor,
            wl_output::Event::Name { name } => info.name = name,
            wl_output::Event::Done => state.outputs_changed = true,
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, Arc<Mutex<OutputInfo>>> for State {
    fn event(state: &mut Self, _: &zxdg_output_v1::ZxdgOutputV1, event: zxdg_output_v1::Event, info: &Arc<Mutex<OutputInfo>>, _: &Connection, _: &QueueHandle<Self>) {
        let mut info = info.lock().unwrap();
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => info.logical_position = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => info.logical_size = Some((width, height)),
            zxdg_output_v1::Event::Name { name } if info.name.is_empty() => info.name = name,
            zxdg_output_v1::Event::Done => state.outputs_changed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(state: &mut Self, seat: &wl_seat::WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event else { return };
        let Some(entry) = state.seats.iter_mut().find(|s| &s.seat == seat) else { return };
        let has = caps.contains(wl_seat::Capability::Pointer);
        match (&entry.pointer, has) {
            (None, true) => {
                let pointer = seat.get_pointer(qh, ());
                if let Some(manager) = &state.cursor_shape {
                    state.shapes.push((pointer.id(), manager.get_pointer(&pointer, qh, ())));
                }
                entry.pointer = Some(pointer);
            }
            (Some(_), false) => {
                if let Some(pointer) = entry.pointer.take() {
                    state.shapes.retain(|(id, _)| *id != pointer.id());
                    if pointer.version() >= 3 {
                        pointer.release();
                    }
                }
            }
            _ => {}
        }
        let has = caps.contains(wl_seat::Capability::Keyboard);
        match (&entry.keyboard, has) {
            (None, true) => entry.keyboard = Some(seat.get_keyboard(qh, ())),
            (Some(_), false) => {
                if let Some(keyboard) = entry.keyboard.take().filter(|k| k.version() >= 3) {
                    keyboard.release();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, pointer: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { serial, surface, surface_x, surface_y } => {
                // Our surfaces show the ordinary arrow (without cursor-shape the
                // compositor keeps whatever it showed).
                if let Some((_, device)) = state.shapes.iter().find(|(id, _)| *id == pointer.id()) {
                    device.set_shape(serial, wp_cursor_shape_device_v1::Shape::Default);
                }
                state.pointer_events.push(PointerEvent::Enter { surface: surface.id(), x: surface_x, y: surface_y });
            }
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => state.pointer_events.push(PointerEvent::Motion { x: surface_x, y: surface_y }),
            wl_pointer::Event::Leave { surface, .. } => state.pointer_events.push(PointerEvent::Leave { surface: surface.id() }),
            wl_pointer::Event::Button { state: WEnum::Value(wl_pointer::ButtonState::Pressed), .. } | wl_pointer::Event::Axis { .. } => {
                state.pointer_events.push(PointerEvent::Press)
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            // The keymap's file descriptor is closed as the event is dropped.
            wl_keyboard::Event::Enter { keys, .. } => {
                let keys = keys.chunks_exact(4).map(|b| u32::from_ne_bytes([b[0], b[1], b[2], b[3]])).collect();
                state.key_events.push(KeyEvent::Enter { keys });
            }
            wl_keyboard::Event::Leave { .. } => state.key_events.push(KeyEvent::Leave),
            wl_keyboard::Event::Key { key, state: key_state, .. } => {
                let down = matches!(key_state, WEnum::Value(wl_keyboard::KeyState::Pressed));
                state.key_events.push(KeyEvent::Key { code: key, down });
            }
            wl_keyboard::Event::Modifiers { mods_depressed, .. } => state.key_events.push(KeyEvent::Modifiers { depressed: mods_depressed }),
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, Arc<Mutex<LayerState>>> for State {
    fn event(
        _: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        data: &Arc<Mutex<LayerState>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                data.lock().unwrap().configured = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => data.lock().unwrap().closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, Arc<AtomicBool>> for State {
    fn event(_: &mut Self, _: &wl_buffer::WlBuffer, event: wl_buffer::Event, busy: &Arc<AtomicBool>, _: &Connection, _: &QueueHandle<Self>) {
        if let wl_buffer::Event::Release = event {
            busy.store(false, Ordering::Relaxed);
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore zwlr_layer_shell_v1::ZwlrLayerShellV1);
delegate_noop!(State: ignore zxdg_output_manager_v1::ZxdgOutputManagerV1);
delegate_noop!(State: ignore wp_cursor_shape_manager_v1::WpCursorShapeManagerV1);
delegate_noop!(State: ignore wp_cursor_shape_device_v1::WpCursorShapeDeviceV1);
