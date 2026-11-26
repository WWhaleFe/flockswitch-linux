use flock_net::{Role, MasterSettings, identity::Identity, *};
use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "flock-net-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct FakePlatform {
    os: Os,
    cursor: Mutex<(f64, f64)>,
    sink: Mutex<Option<Sender<InputEvent>>>,
    injected: Mutex<Vec<InputEvent>>,
    stopped: AtomicUsize,
    no_cursor: AtomicBool,
    hit: Mutex<Option<(Side, f64)>>,
    armed: Mutex<Vec<Side>>,
    returned: Mutex<Option<(Side, f64)>>,
    control: AtomicBool,
    clipboard: Mutex<Option<String>>,
    files: Mutex<Vec<std::path::PathBuf>>,
    image: Mutex<Option<Vec<u8>>>,
    recv_dir: Mutex<Option<std::path::PathBuf>>,
}
impl FakePlatform {
    fn new(os: Os) -> Arc<Self> {
        Arc::new(Self {
            os,
            cursor: Mutex::new((50.0, 50.0)),
            sink: Mutex::new(None),
            injected: Mutex::new(vec![]),
            stopped: AtomicUsize::new(0),
            no_cursor: AtomicBool::new(false),
            hit: Mutex::new(None),
            armed: Mutex::new(vec![]),
            returned: Mutex::new(None),
            control: AtomicBool::new(true),
            clipboard: Mutex::new(None),
            files: Mutex::new(vec![]),
            image: Mutex::new(None),
            recv_dir: Mutex::new(None),
        })
    }
    fn send(&self, event: InputEvent) {
        self.sink
            .lock()
            .unwrap()
            .as_ref()
            .expect("capturing")
            .send(event)
            .unwrap();
    }
    fn records(&self) -> Vec<InputEvent> {
        self.injected.lock().unwrap().clone()
    }
}
impl Platform for FakePlatform {
    fn os(&self) -> Os {
        self.os
    }
    fn displays(&self) -> Vec<Rect> {
        vec![Rect::new(0.0, 0.0, 100.0, 100.0)]
    }
    fn cursor(&self) -> Option<(f64, f64)> {
        (!self.no_cursor.load(Ordering::Relaxed)).then(|| *self.cursor.lock().unwrap())
    }
    fn arm_edges(&self, sides: &[Side]) {
        *self.armed.lock().unwrap() = sides.to_vec();
    }
    fn take_edge_hit(&self) -> Option<(Side, f64)> {
        self.hit.lock().unwrap().take()
    }
    fn place_on_return(&self, side: Side, pos: f64) {
        *self.returned.lock().unwrap() = Some((side, pos));
        if let Some((x, y)) = geometry::edge_position(&self.displays(), side, pos) {
            self.warp(x, y);
        }
    }
    fn warp(&self, x: f64, y: f64) {
        *self.cursor.lock().unwrap() = (x, y);
    }
    fn start_capture(&self, sink: Sender<InputEvent>) -> anyhow::Result<()> {
        *self.sink.lock().unwrap() = Some(sink);
        Ok(())
    }
    fn stop_capture(&self) {
        *self.sink.lock().unwrap() = None;
        self.stopped.fetch_add(1, Ordering::Relaxed);
    }
    fn inject(&self, event: &InputEvent) {
        if let InputEvent::Motion { dx, dy } = event {
            let mut p = self.cursor.lock().unwrap();
            *p = geometry::clamp(&self.displays(), p.0 + dx, p.1 + dy).unwrap();
        }
        self.injected.lock().unwrap().push(event.clone());
    }
    fn ready(&self) -> Result<(), String> {
        Ok(())
    }
    fn control_down(&self) -> bool {
        self.control.load(Ordering::Relaxed)
    }
    fn clipboard_text(&self) -> Option<String> {
        self.clipboard.lock().unwrap().clone()
    }
    fn set_clipboard_text(&self, text: &str) {
        *self.clipboard.lock().unwrap() = Some(text.into());
    }
    fn clipboard_files(&self) -> Vec<std::path::PathBuf> {
        self.files.lock().unwrap().clone()
    }
    fn set_clipboard_files(&self, files: &[std::path::PathBuf]) {
        *self.files.lock().unwrap() = files.to_vec();
    }
    fn clipboard_image_png(&self) -> Option<Vec<u8>> {
        self.image.lock().unwrap().clone()
    }
    fn set_clipboard_image_png(&self, png: &[u8]) {
        *self.image.lock().unwrap() = Some(png.to_vec());
    }
    fn received_dir(&self) -> Option<std::path::PathBuf> {
        self.recv_dir.lock().unwrap().clone()
    }
}
fn wait(label: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out: {label}");
        thread::sleep(Duration::from_millis(5));
    }
}
fn hooks() -> (Hooks, Receiver<PeerConfig>, Receiver<u8>, Receiver<String>) {
    let (paired, rx) = mpsc::channel();
    let (channel, channels) = mpsc::channel();
    let (unpaired, unpairs) = mpsc::channel();
    (
        Hooks {
            on_paired: Box::new(move |p| {
                paired.send(p).unwrap();
            }),
            switch_logitech: Box::new(move |c| {
                channel.send(c).unwrap();
            }),
            on_unpaired: Box::new(move |id| {
                unpaired.send(id).unwrap();
            }),
            ..Hooks::default()
        },
        rx,
        channels,
        unpairs,
    )
}
fn config(dir: &TempDir, name: &str) -> NetConfig {
    NetConfig {
        on: true,
        port: 0,
        discovery: false,
        corner_margin: 10.0,
        notice_accepted: true,
        role: if name == "A" { Role::Master } else { Role::Sub },
        name: name.into(),
        config_dir: dir.0.clone(),
        ..NetConfig::default()
    }
}

fn scenario(receiver: Os) {
    scenario_mode(receiver, false);
}
fn scenario_mode(receiver: Os, no_cursor: bool) {
    let da = TempDir::new();
    let db = TempDir::new();
    let a = FakePlatform::new(if no_cursor { Os::Linux } else { Os::Mac });
    a.no_cursor.store(no_cursor, Ordering::Relaxed);
    let b = FakePlatform::new(receiver);
    let mut ca = config(&da, "A");
    if no_cursor {
        ca.cross_delay_ms = 40;
    }
    let mut cb = config(&db, "B");
    let (ha, pa, channels, _) = hooks();
    let (hb, pb, bchannels, _) = hooks();
    let sa = Service::start(ca.clone(), a.clone(), ha).unwrap();
    let sb = Service::start(cb.clone(), b.clone(), hb).unwrap();
    sa.request(Request::PairAddr {
        pair_addr: sb.local_addr().to_string(),
    })
    .unwrap();
    wait("both show pairing code", || {
        sa.status().pairing.is_some() && sb.status().pairing.is_some()
    });
    let code = sa.status().pairing.unwrap().code;
    assert_eq!(code, sb.status().pairing.unwrap().code);
    assert_eq!(code.len(), 6);
    sa.request(Request::Accept { accept: true }).unwrap();
    write_request(&db.0, &Request::Accept { accept: true }).unwrap();
    let mut ab = pa.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut ba = pb.recv_timeout(Duration::from_secs(5)).unwrap();
    wait("request consumed", || !request_path(&db.0).exists());
    ab.side = Some(Side::Right);
    ab.logitech_channel = 2;
    ab.last_addr = sb.local_addr().to_string();
    ba.last_addr = sa.local_addr().to_string();
    ba.side = Some(Side::Left);
    ba.logitech_channel = 3;
    ca.peers = vec![ab];
    cb.peers = vec![ba];
    sa.update_config(ca.clone()).unwrap();
    sb.update_config(cb.clone()).unwrap();
    wait("paired connection", || {
        sa.status().peers.first().is_some_and(|p| p.connected)
            && sb.status().peers.first().is_some_and(|p| p.connected)
    });
    if no_cursor {
        wait("barriers armed", || {
            a.armed.lock().unwrap().contains(&Side::Right)
        });
        *a.hit.lock().unwrap() = Some((Side::Right, 0.5));
    } else {
        *a.clipboard.lock().unwrap() = Some("copied on A".into());
        a.warp(99.0, 50.0);
    }
    wait("A controls B", || {
        sa.status().state.mode == "controlling" && sb.status().state.mode == "controlled"
    });
    if !no_cursor {
        wait("clipboard carried to B", || {
            b.clipboard_text().as_deref() == Some("copied on A")
        });
        *b.clipboard.lock().unwrap() = Some("copied on B".into());
    }
    assert_eq!(channels.recv_timeout(Duration::from_secs(1)).unwrap(), 2);
    assert_eq!(b.cursor().unwrap().0, 2.0);
    let mut events = vec![
        InputEvent::Motion { dx: 10.0, dy: 4.0 },
        InputEvent::Key {
            usage: 0xe3,
            down: true,
        },
        InputEvent::Key {
            usage: 4,
            down: true,
        },
        InputEvent::Key {
            usage: 4,
            down: false,
        },
        InputEvent::Key {
            usage: 0xe3,
            down: false,
        },
    ];
    let shortcut_start = events.len();
    let shortcut = |usage, down| InputEvent::Key { usage, down };
    events.extend([
        shortcut(0x39, true), shortcut(0x39, false),
        shortcut(0xe1, true), shortcut(0x2c, true), shortcut(0x2c, false), shortcut(0xe1, false),
    ]);
    for e in &events {
        a.send(e.clone());
    }
    let mut expected = events.clone();
    if receiver == Os::Windows {
        expected[1] = InputEvent::Key {
            usage: 0xe0,
            down: true,
        };
        expected[4] = InputEvent::Key {
            usage: 0xe0,
            down: false,
        };
    }
    if receiver == Os::Windows {
        expected.truncate(shortcut_start);
        expected.extend([
            shortcut(0x90, true), shortcut(0x90, false),
            shortcut(0xe1, true), shortcut(0xe1, false),
            shortcut(0x90, true), shortcut(0x90, false),
            shortcut(0xe1, true), shortcut(0xe1, false),
        ]);
    }
    wait("input delivery", || b.records().len() >= expected.len());
    assert_eq!(&b.records()[..expected.len()], expected.as_slice());
    b.warp(0.0, 54.0);
    wait("Leave", || {
        sa.status().state.mode == "local" && sb.status().state.mode == "local"
    });
    if !no_cursor {
        wait("clipboard carried back to A", || {
            a.clipboard_text().as_deref() == Some("copied on B")
        });
    }
    assert_eq!(bchannels.recv_timeout(Duration::from_secs(1)).unwrap(), 3);
    assert_eq!(a.cursor.lock().unwrap().0, 97.0);
    assert_eq!(a.returned.lock().unwrap().unwrap().0, Side::Right);
    assert!(a.stopped.load(Ordering::Relaxed) > 0);
    if no_cursor {
        wait("barriers armed", || {
            a.armed.lock().unwrap().contains(&Side::Right)
        });
        *a.hit.lock().unwrap() = Some((Side::Right, 0.5));
    } else {
        a.warp(99.0, 50.0);
    }
    wait("control again", || {
        sa.status().state.mode == "controlling" && sb.status().state.mode == "controlled"
    });
    let held = InputEvent::Key {
        usage: 5,
        down: true,
    };
    a.send(held.clone());
    wait("held key injected", || b.records().contains(&held));
    let stopped = a.stopped.load(Ordering::Relaxed);
    let at = Instant::now();
    sb.stop();
    wait("link loss restores A", || sa.status().state.mode == "local");
    assert!(at.elapsed() < Duration::from_secs(3));
    assert!(a.stopped.load(Ordering::Relaxed) > stopped);
    assert!(b.records().contains(&InputEvent::Key {
        usage: 5,
        down: false
    }));
    assert!(!sb.status().running);
    wait("status persisted", || {
        read_status(&da.0).is_ok_and(|s| s.state.mode == "local")
    });
    sa.stop();
    assert!(a.armed.lock().unwrap().is_empty());
    assert!(!read_status(&da.0).unwrap().running);
}
#[test]
fn mac_to_mac_pair_enter_input_leave_disconnect() {
    scenario(Os::Mac);
}
#[test]
fn mac_to_windows_modifier_swap() {
    scenario(Os::Windows);
}

#[test]
fn return_hotkey_and_remote_link_loss_release() {
    let da = TempDir::new();
    let db = TempDir::new();
    let a = FakePlatform::new(Os::Mac);
    let b = FakePlatform::new(Os::Mac);
    let ia = Identity::load_or_create(&da.0).unwrap();
    let ib = Identity::load_or_create(&db.0).unwrap();
    // Reserve the listener addresses before configuring the paired identities.
    let mut ca = config(&da, "A");
    let cb = config(&db, "B");
    let sb = Service::start(cb.clone(), b.clone(), Hooks::default()).unwrap();
    ca.peers.push(PeerConfig {
        id: ib.id.clone(),
        name: "B".into(),
        os: Os::Mac,
        fingerprint: ib.fingerprint.clone(),
        side: Some(Side::Right),
        last_addr: sb.local_addr().to_string(),
        ..PeerConfig::default()
    });
    let sa = Service::start(ca.clone(), a.clone(), Hooks::default()).unwrap();
    let mut cb = cb;
    cb.peers.push(PeerConfig {
        id: ia.id.clone(),
        name: "A".into(),
        os: Os::Mac,
        fingerprint: ia.fingerprint.clone(),
        last_addr: sa.local_addr().to_string(),
        ..PeerConfig::default()
    });
    sb.update_config(cb).unwrap();
    wait("paired link", || {
        sa.status().peers.first().is_some_and(|p| p.connected)
            && sb.status().peers.first().is_some_and(|p| p.connected)
    });
    a.warp(99.0, 50.0);
    wait("capture", || {
        sa.status().state.mode == "controlling" && sb.status().state.mode == "controlled"
    });
    for usage in [0xe0, 0xe2, 0xe1, 0x14] {
        a.send(InputEvent::Key { usage, down: true });
    }
    wait("hotkey restores both", || {
        sa.status().state.mode == "local" && sb.status().state.mode == "local"
    });
    assert!(!b
        .records()
        .iter()
        .any(|e| matches!(e, InputEvent::Key { usage: 0x14, .. })));
    for usage in [0xe0, 0xe2, 0xe1] {
        wait("modifier released", || {
            b.records()
                .contains(&InputEvent::Key { usage, down: false })
        });
    }
    a.warp(99.0, 50.0);
    wait("second capture", || {
        sa.status().state.mode == "controlling" && sb.status().state.mode == "controlled"
    });
    a.send(InputEvent::Key {
        usage: 7,
        down: true,
    });
    a.send(InputEvent::Button {
        button: MouseButton::Left,
        down: true,
    });
    wait("pressed input", || {
        b.records().contains(&InputEvent::Button {
            button: MouseButton::Left,
            down: true,
        })
    });
    sa.stop();
    wait("B local on link loss", || sb.status().state.mode == "local");
    assert!(b.records().contains(&InputEvent::Key {
        usage: 7,
        down: false
    }));
    assert!(b.records().contains(&InputEvent::Button {
        button: MouseButton::Left,
        down: false
    }));
    sb.stop();
}

#[test]
fn silent_peer_triggers_two_second_safety_return() {
    use flock_net::{
        proto::{read_frame, write_frame, Msg, PROTO_VERSION},
        tls,
    };
    use std::{io::Read, net::TcpListener, sync::atomic::AtomicBool};
    let d1 = TempDir::new();
    let d2 = TempDir::new();
    let i1 = Identity::load_or_create(&d1.0).unwrap();
    let i2 = Identity::load_or_create(&d2.0).unwrap();
    let (local_dir, remote) = if i1.id < i2.id { (&d1, i2) } else { (&d2, i1) };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let remote_id = remote.id.clone();
    let remote_fp = remote.fingerprint.clone();
    let done = Arc::new(AtomicBool::new(false));
    let shutdown = done.clone();
    let server = thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let mut stream = rustls::StreamOwned::new(
            rustls::ServerConnection::new(tls::server_config(&remote).unwrap()).unwrap(),
            socket,
        );
        assert!(matches!(
            read_frame(&mut stream).unwrap(),
            Msg::Hello { .. }
        ));
        write_frame(
            &mut stream,
            &Msg::Hello {
                proto: PROTO_VERSION,
                id: remote.id,
                name: "silent".into(),
                os: Os::Windows,
                app_version: "test".into(),
                role: Role::Sub,
            },
        )
        .unwrap();
        // Drain traffic without replying, keeping TCP open while the heartbeat expires.
        let mut bytes = [0; 4096];
        while !shutdown.load(Ordering::Relaxed) {
            match stream.read(&mut bytes) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
        }
    });
    let platform = FakePlatform::new(Os::Mac);
    let mut cfg = config(local_dir, "controller");
    cfg.peers.push(PeerConfig {
        id: remote_id,
        name: "silent".into(),
        os: Os::Windows,
        fingerprint: remote_fp,
        side: Some(Side::Right),
        last_addr: addr.to_string(),
        ..PeerConfig::default()
    });
    let service = Service::start(cfg, platform.clone(), Hooks::default()).unwrap();
    wait("silent peer connected", || {
        service.status().peers.first().is_some_and(|p| p.connected)
    });
    platform.warp(99.0, 50.0);
    wait("silent peer controlling", || {
        service.status().state.mode == "controlling"
    });
    let started = Instant::now();
    wait("heartbeat timeout", || {
        service.status().state.mode == "local"
    });
    assert!(started.elapsed() > Duration::from_millis(1500));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(platform.stopped.load(Ordering::Relaxed) > 0);
    assert_eq!(platform.cursor().unwrap().0, 97.0);
    service.stop();
    done.store(true, Ordering::Relaxed);
    server.join().unwrap();
}

#[test]
fn no_global_cursor_edge_hit_enter_leave_and_disconnect() {
    scenario_mode(Os::Linux, true);
}

#[test]
fn require_control_gates_crossing_both_ways() {
    let da = TempDir::new();
    let db = TempDir::new();
    let a = FakePlatform::new(Os::Mac);
    let b = FakePlatform::new(Os::Mac);
    let mut ca = config(&da, "A");
    let mut cb = config(&db, "B");
    ca.require_control = true;
    cb.require_control = true;
    let (ha, pa, _, _) = hooks();
    let (hb, pb, _, _) = hooks();
    let sa = Service::start(ca.clone(), a.clone(), ha).unwrap();
    let sb = Service::start(cb.clone(), b.clone(), hb).unwrap();
    sa.request(Request::PairAddr {
        pair_addr: sb.local_addr().to_string(),
    })
    .unwrap();
    wait("both show pairing code", || {
        sa.status().pairing.is_some() && sb.status().pairing.is_some()
    });
    sa.request(Request::Accept { accept: true }).unwrap();
    sb.request(Request::Accept { accept: true }).unwrap();
    let mut ab = pa.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut ba = pb.recv_timeout(Duration::from_secs(5)).unwrap();
    ab.side = Some(Side::Right);
    ab.last_addr = sb.local_addr().to_string();
    ba.side = Some(Side::Left);
    ba.last_addr = sa.local_addr().to_string();
    ca.peers = vec![ab];
    cb.peers = vec![ba];
    sa.update_config(ca).unwrap();
    sb.update_config(cb).unwrap();
    wait("paired connection", || {
        sa.status().peers.first().is_some_and(|p| p.connected)
            && sb.status().peers.first().is_some_and(|p| p.connected)
    });

    // At the edge without Control: stays here.
    a.control.store(false, Ordering::Relaxed);
    a.warp(99.0, 50.0);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(sa.status().state.mode, "local");
    // Control down at the same edge: crosses.
    a.control.store(true, Ordering::Relaxed);
    wait("A controls B", || {
        sa.status().state.mode == "controlling" && sb.status().state.mode == "controlled"
    });

    // Pushed back against B's left edge without Control (neither keyboard): stays on B.
    b.control.store(false, Ordering::Relaxed);
    a.send(InputEvent::Motion { dx: -200.0, dy: 0.0 });
    thread::sleep(Duration::from_millis(300));
    assert_eq!(sb.status().state.mode, "controlled");
    // Control held on A's keyboard (arrives at B as injected Control): comes back.
    a.send(InputEvent::Key {
        usage: 0xe0,
        down: true,
    });
    a.send(InputEvent::Motion { dx: -5.0, dy: 0.0 });
    wait("back on A", || {
        sa.status().state.mode == "local" && sb.status().state.mode == "local"
    });
    sa.stop();
    sb.stop();
}

#[test]
fn nothing_listens_until_the_notice_is_accepted() {
    let d = TempDir::new();
    let mut c = config(&d, "A");
    c.notice_accepted = false;
    let (h, _, _, _) = hooks();
    let s = Service::start(c, FakePlatform::new(Os::Mac), h).unwrap();
    thread::sleep(Duration::from_millis(200));
    assert!(!s.status().running);
    s.stop();
}

#[test]
fn app_driven_edges_cross_only_on_request() {
    let da = TempDir::new();
    let db = TempDir::new();
    let a = FakePlatform::new(Os::Mac);
    let b = FakePlatform::new(Os::Windows);
    let mut ca = config(&da, "A");
    ca.app_drives_edges = true;
    let mut cb = config(&db, "B");
    let (ha, pa, _, _) = hooks();
    let (hb, pb, _, _) = hooks();
    let sa = Service::start(ca.clone(), a.clone(), ha).unwrap();
    let sb = Service::start(cb.clone(), b.clone(), hb).unwrap();
    sa.request(Request::PairAddr {
        pair_addr: sb.local_addr().to_string(),
    })
    .unwrap();
    wait("both show pairing code", || {
        sa.status().pairing.is_some() && sb.status().pairing.is_some()
    });
    sa.request(Request::Accept { accept: true }).unwrap();
    sb.request(Request::Accept { accept: true }).unwrap();
    let mut ab = pa.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut ba = pb.recv_timeout(Duration::from_secs(5)).unwrap();
    ab.side = Some(Side::Right);
    ab.last_addr = sb.local_addr().to_string();
    ba.side = Some(Side::Left);
    ba.last_addr = sa.local_addr().to_string();
    ca.peers = vec![ab];
    cb.peers = vec![ba];
    sa.update_config(ca).unwrap();
    sb.update_config(cb).unwrap();
    wait("paired connection", || {
        sa.status().peers.first().is_some_and(|p| p.connected)
            && sb.status().peers.first().is_some_and(|p| p.connected)
    });
    // At the edge, the app's countdown has not ended: nothing happens.
    a.warp(99.0, 50.0);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(sa.status().state.mode, "local");
    // A side with no peer: ignored.
    sa.cross(Side::Left, 0.5).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(sa.status().state.mode, "local");
    // The countdown ends: cross.
    sa.cross(Side::Right, 0.25).unwrap();
    wait("A controls B", || {
        sa.status().state.mode == "controlling" && sb.status().state.mode == "controlled"
    });
    let (bx, by) = b.cursor().unwrap();
    assert!(bx == 2.0 && (by - 25.0).abs() < 1.0, "{bx},{by}");
    // The way back is still decided by the service.
    a.send(InputEvent::Motion { dx: -50.0, dy: 0.0 });
    wait("back on A", || {
        sa.status().state.mode == "local" && sb.status().state.mode == "local"
    });
    sa.stop();
    sb.stop();
}

#[test]
fn files_and_images_travel_with_the_cursor() {
    let da = TempDir::new();
    let db = TempDir::new();
    let a = FakePlatform::new(Os::Mac);
    let b = FakePlatform::new(Os::Windows);
    *b.recv_dir.lock().unwrap() = Some(db.0.join("Downloads"));
    let mut ca = config(&da, "A");
    let mut cb = config(&db, "B");
    let (ha, pa, _, _) = hooks();
    let (hb, pb, _, _) = hooks();
    let sa = Service::start(ca.clone(), a.clone(), ha).unwrap();
    let sb = Service::start(cb.clone(), b.clone(), hb).unwrap();
    sa.request(Request::PairAddr {
        pair_addr: sb.local_addr().to_string(),
    })
    .unwrap();
    wait("both show pairing code", || {
        sa.status().pairing.is_some() && sb.status().pairing.is_some()
    });
    sa.request(Request::Accept { accept: true }).unwrap();
    sb.request(Request::Accept { accept: true }).unwrap();
    let mut ab = pa.recv_timeout(Duration::from_secs(5)).unwrap();
    let mut ba = pb.recv_timeout(Duration::from_secs(5)).unwrap();
    ab.side = Some(Side::Right);
    ab.last_addr = sb.local_addr().to_string();
    ba.side = Some(Side::Left);
    ba.last_addr = sa.local_addr().to_string();
    ca.peers = vec![ab];
    cb.peers = vec![ba];
    sa.update_config(ca).unwrap();
    sb.update_config(cb).unwrap();
    wait("paired connection", || {
        sa.status().peers.first().is_some_and(|p| p.connected)
            && sb.status().peers.first().is_some_and(|p| p.connected)
    });

    // Files copied on A go along when the cursor crosses.
    let body: Vec<u8> = (0..1_000_003u32).map(|i| (i % 253) as u8).collect();
    let src = da.0.join("report.bin");
    std::fs::write(&src, &body).unwrap();
    *a.files.lock().unwrap() = vec![src];
    a.warp(99.0, 50.0);
    wait("A controls B", || sa.status().state.mode == "controlling");
    wait("file arrived on B", || !b.clipboard_files().is_empty());
    let got = b.clipboard_files();
    assert_eq!(got, vec![db.0.join("Downloads").join("report.bin")]);
    assert_eq!(std::fs::read(&got[0]).unwrap(), body);
    assert_eq!(sb.status().transfer.unwrap().stage, "done");
    // Input still flows after the transfer.
    a.send(InputEvent::Motion { dx: 5.0, dy: 0.0 });
    wait("input after transfer", || {
        b.injected.lock().unwrap().iter().any(|e| matches!(e, InputEvent::Motion { .. }))
    });

    // An image copied on B comes back to A.
    *b.files.lock().unwrap() = vec![];
    *b.image.lock().unwrap() = Some(vec![137, 80, 78, 71, 9, 9, 9]);
    a.send(InputEvent::Motion { dx: -500.0, dy: 0.0 });
    wait("back on A", || sa.status().state.mode == "local");
    wait("image on A", || a.clipboard_image_png().is_some());
    assert_eq!(a.clipboard_image_png().unwrap(), vec![137, 80, 78, 71, 9, 9, 9]);
    sa.stop();
    sb.stop();
}

fn hooks_full() -> (Hooks, Receiver<PeerConfig>, Receiver<PeerConfig>, Receiver<MasterSettings>) {
    let (paired, pairs) = mpsc::channel();
    let (changed, changes) = mpsc::channel();
    let (settings, settled) = mpsc::channel();
    (
        Hooks {
            on_paired: Box::new(move |p| {
                paired.send(p).unwrap();
            }),
            on_peer_changed: Box::new(move |p| {
                let _ = changed.send(p);
            }),
            on_master_settings: Box::new(move |m| {
                let _ = settings.send(m);
            }),
            ..Hooks::default()
        },
        pairs,
        changes,
        settled,
    )
}

#[test]
fn two_masters_or_two_subs_do_not_pair() {
    for (ra, rb, reason) in [(Role::Master, Role::Master, "both_master"), (Role::Sub, Role::Sub, "both_sub")] {
        let da = TempDir::new();
        let db = TempDir::new();
        let mut ca = config(&da, "A");
        let mut cb = config(&db, "B");
        ca.role = ra;
        cb.role = rb;
        let sa = Service::start(ca, FakePlatform::new(Os::Mac), Hooks::default()).unwrap();
        let sb = Service::start(cb, FakePlatform::new(Os::Windows), Hooks::default()).unwrap();
        sa.request(Request::PairAddr {
            pair_addr: sb.local_addr().to_string(),
        })
        .unwrap();
        wait("refused", || {
            sa.status().pairing.is_some_and(|p| p.stage == "failed" && p.reason.as_deref() == Some(reason))
        });
        sa.stop();
        sb.stop();
    }
}

#[test]
fn a_master_takes_three_subs_and_hands_them_its_layout_and_settings() {
    let dm = TempDir::new();
    let mut cm = config(&dm, "A");
    cm.require_control = true;
    cm.cross_delay_ms = 300;
    cm.app_settings = serde_json::json!({"countdown": "ring"});
    let (hm, mpairs, _, _) = hooks_full();
    let sm = Service::start(cm.clone(), FakePlatform::new(Os::Mac), hm).unwrap();
    let mut subs = vec![];
    for (i, side) in [Side::Right, Side::Left, Side::Top].into_iter().enumerate() {
        let d = TempDir::new();
        let c = config(&d, &format!("S{i}"));
        let (h, pairs, changes, settled) = hooks_full();
        let s = Service::start(c.clone(), FakePlatform::new(Os::Windows), h).unwrap();
        sm.request(Request::PairAddr {
            pair_addr: s.local_addr().to_string(),
        })
        .unwrap();
        wait("pairing code", || sm.status().pairing.is_some_and(|p| p.stage == "waiting") && s.status().pairing.is_some());
        sm.request(Request::Accept { accept: true }).unwrap();
        s.request(Request::Accept { accept: true }).unwrap();
        let mut to_sub = mpairs.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(to_sub.role, Role::Sub);
        let to_master = pairs.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(to_master.role, Role::Master);
        to_sub.side = Some(side);
        to_sub.last_addr = s.local_addr().to_string();
        cm.peers.push(to_sub);
        sm.update_config(cm.clone()).unwrap();
        let mut csub = c.clone();
        csub.peers = vec![to_master];
        s.update_config(csub).unwrap();
        // The sub learns where it sits (the master is on the opposite side) and the settings.
        let changed = changes.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(changed.side, Some(side.opposite()));
        let settings = settled.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(settings.require_control);
        assert_eq!(settings.cross_delay_ms, 300);
        assert_eq!(settings.app["countdown"], "ring");
        wait("following", || s.status().following.as_deref() == Some("A"));
        subs.push((d, s));
    }
    // A fourth sub is refused.
    let d4 = TempDir::new();
    let s4 = Service::start(config(&d4, "S3"), FakePlatform::new(Os::Windows), Hooks::default()).unwrap();
    sm.request(Request::PairAddr {
        pair_addr: s4.local_addr().to_string(),
    })
    .unwrap();
    wait("fourth refused", || {
        sm.status().pairing.is_some_and(|p| p.stage == "failed" && p.reason.as_deref() == Some("full"))
    });
    s4.stop();
    for (_, s) in &subs {
        s.stop();
    }
    sm.stop();
}
