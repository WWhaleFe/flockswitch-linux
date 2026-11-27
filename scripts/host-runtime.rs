// Host-only adapters for executing production watcher and service hooks without X11.
mod shared {
    use crate::config::Config;
    use std::sync::{atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering}, Mutex};

    #[derive(Clone, Copy)]
    pub enum Wake { Unchanged, Disable, Retarget, Stop }

    pub struct Status {
        pub channel: Option<u8>,
        pub trigger: Option<String>,
        pub last_switch: Option<String>,
    }

    pub struct Shared {
        config: Mutex<Config>,
        version: AtomicU64,
        stop: AtomicBool,
        waits: AtomicUsize,
        wake: Wake,
        pub switches: Mutex<Vec<u8>>,
        pub saves: AtomicUsize,
        pub save_path: std::path::PathBuf,
    }

    impl Shared {
        pub fn new(wake: Wake, save_path: std::path::PathBuf) -> Self {
            let mut config = Config::default();
            config.edges.on = true;
            config.edges.left = 2;
            config.edges.dwell_ms = 50;
            config.edges.require_control = false;
            config.haptics.on_edge = false;
            config.locate.on = false;
            Self {
                config: Mutex::new(config), version: AtomicU64::new(0),
                stop: AtomicBool::new(false), waits: AtomicUsize::new(0), wake,
                switches: Mutex::new(vec![]), saves: AtomicUsize::new(0), save_path,
            }
        }
        pub fn config(&self) -> Config { self.config.lock().unwrap().clone() }
        pub fn version(&self) -> u64 { self.version.load(Ordering::SeqCst) }
        pub fn screen_generation(&self) -> u64 { self.version() }
        pub fn stopped(&self) -> bool { self.stop.load(Ordering::SeqCst) }
        pub fn wait_screen(&self, _: u64, _: std::time::Duration) {
            let wait = self.waits.fetch_add(1, Ordering::SeqCst) + 1;
            // First sample rests on the edge, second starts the countdown. At the
            // third sample the dwell has expired and settings/stop wake the watcher.
            if wait == 3 {
                std::thread::sleep(std::time::Duration::from_millis(50));
                match self.wake {
                    Wake::Disable => self.config.lock().unwrap().edges.on = false,
                    Wake::Retarget => self.config.lock().unwrap().edges.left = 3,
                    Wake::Stop => self.stop.store(true, Ordering::SeqCst),
                    Wake::Unchanged => {},
                }
                if matches!(self.wake, Wake::Disable | Wake::Retarget) {
                    self.version.fetch_add(1, Ordering::SeqCst);
                }
            }
            assert!(wait <= 6, "watcher did not stop");
            if wait == 6 { self.stop.store(true, Ordering::SeqCst); }
        }
        pub fn read_status<T>(&self, read: impl FnOnce(&Status) -> T) -> T {
            read(&Status { channel: Some(1), trigger: Some("mouse".into()), last_switch: None })
        }
        pub fn take_locate_request(&self) -> Option<()> { None }
        pub fn request_haptic(&self, _: u8) {}
        pub fn request_switch(&self, channel: u8) { self.switches.lock().unwrap().push(channel); }
        pub fn update_config(&self, change: impl FnOnce(&mut Config)) -> anyhow::Result<()> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            let mut current = self.config.lock().unwrap();
            // Match Shared::update_config: merge into the latest disk snapshot.
            let mut next = if self.save_path.is_file() {
                Config::parse(&std::fs::read_to_string(&self.save_path)?)?
            } else { current.clone() };
            change(&mut next);
            std::fs::write(&self.save_path, next.to_text()?)?;
            *current = next;
            self.version.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
}

mod overlay {
    use crate::{config::{Edge, Edges, Locate}, edge::Rect};
    thread_local! {
        pub static COUNTDOWNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        pub static ENDED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    pub fn edge_begin(_: &Edges, _: Edge, _: &Rect, _: std::time::Duration, _: std::time::Duration, _: bool) {
        COUNTDOWNS.with(|n| n.set(n.get() + 1));
    }
    pub fn edge_end() { ENDED.with(|n| n.set(n.get() + 1)); }
    pub fn locate(_: &Locate) {}
}

mod net {
    use crate::{config::Edge, net_logic::apply_master_settings, shared::Shared};
    use std::sync::{Arc, Mutex};
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/runtime-net.rs"));
    pub fn network_side_ready(_: Edge) -> bool { false }
    pub fn cross(_: Edge, _: f64, _: f64) { panic!("unexpected network crossing"); }

    #[test]
    fn sub_key_edit_preserves_a_newer_master_edit_on_disk() {
        use crate::{config::Config, shared::Wake};
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("flockswitch-key-merge-{}-{stamp}.toml", std::process::id()));
        let shared = Arc::new(Shared::new(Wake::Unchanged, path.clone()));
        shared.update_config(|c| c.network.keys = c.network.all_rules()).unwrap();
        let hooks = hooks(shared.clone());
        let mut sub = shared.config().network.keys;
        sub.get_mut("linux>mac").unwrap().capture = false;
        // The settings window saves another pair before the service reloads it.
        let mut disk = shared.config();
        disk.network.keys.get_mut("mac>linux").unwrap().capture = false;
        disk.locate.on = true;
        std::fs::write(&path, disk.to_text().unwrap()).unwrap();
        (hooks.persist_key_rules)(&sub).unwrap();
        // The service still has its own merged snapshot for a second request;
        // Shared now includes the master's new pair. Compare against the service.
        sub.get_mut("mac>windows").unwrap().capture = false;
        (hooks.persist_key_rules)(&sub).unwrap();
        let saved = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(!saved.network.keys["mac>linux"].capture, "master's newer pair was lost");
        assert!(!saved.network.keys["linux>mac"].capture);
        assert!(!saved.network.keys["mac>windows"].capture);
        assert!(saved.locate.on);
    }

    #[test]
    fn key_rule_save_failure_is_returned_and_success_survives_restart() {
        use crate::{config::Config, shared::Wake};
        use std::sync::atomic::Ordering;
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
        }
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("flockswitch-hook-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let _cleanup = Cleanup(root.clone());
        let path = root.join("config.toml");
        // A directory at the destination deterministically rejects writes, even as root.
        std::fs::create_dir(&path).unwrap();
        let shared = Arc::new(Shared::new(Wake::Unchanged, path.clone()));
        let hooks = hooks(shared.clone());
        let before = shared.config().network.keys;
        let mut keys = shared.config().network.all_rules();
        keys.get_mut("linux>mac").unwrap().capture = false;
        let result = (hooks.persist_key_rules)(&keys);
        assert!(result.is_err(), "a failed save must prevent the master's acknowledgement");
        assert_eq!(shared.config().network.keys, before);
        std::fs::remove_dir(&path).unwrap();
        (hooks.persist_key_rules)(&keys).unwrap();
        (hooks.on_key_rules)(keys.clone());
        assert_eq!(shared.saves.load(Ordering::SeqCst), 2, "notification must not save twice");
        assert_eq!(shared.config().network.keys, keys);
        let restarted = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(restarted.network.keys, keys);
    }

    #[test]
    fn unpairing_from_the_other_side_forgets_the_peer_its_cell_and_pending_rules() {
        use crate::shared::Wake;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("flockswitch-unpair-{}-{stamp}.toml", std::process::id()));
        let shared = Arc::new(Shared::new(Wake::Unchanged, path.clone()));
        shared.update_config(|c| {
            c.network.role = flock_net::Role::Sub;
            c.network.peers.push(flock_net::PeerConfig { id: "master".into(), role: flock_net::Role::Master, ..Default::default() });
            c.network.layout.push(flock_net::Cell { id: "master".into(), col: 0, row: 0 });
            c.pending_key_rules = Some(c.network.all_rules());
        }).unwrap();
        let hooks = hooks(shared.clone());
        (hooks.on_unpaired)("master".into());
        let after = shared.config();
        let _ = std::fs::remove_file(&path);
        assert!(after.network.peers.is_empty() && after.network.layout.is_empty());
        assert!(after.pending_key_rules.is_none());
    }
}

mod net_wayland {
    use flock_net::{InputEvent, Side};
    use std::{sync::{atomic::Ordering, mpsc::Sender, Mutex, OnceLock}, time::Instant};
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/runtime-wayland.rs"));
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn a_late_probe_and_old_eis_callbacks_cannot_register_in_the_new_generation() {
        let _guard = TEST_LOCK.lock().unwrap();
        let old = begin_start();
        let (finish_probe, probe_finished) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            probe_finished.recv().unwrap();
            assert!(!current_start(old), "a superseded probe must be rejected");
            assert!(with_session(old, |session| {
                session.failed = Some("old failure".into());
                session.opened = true;
                session.no_answer = true;
            }).is_none());
            drop(Wayland { generation: old });
        });
        disable();
        let new = begin_start();
        with_session(new, |session| session.failed = Some("current failure".into())).unwrap();
        finish_probe.send(()).unwrap();
        worker.join().unwrap();
        let session = SESSION.lock().unwrap();
        assert_eq!(session.generation, new);
        assert_eq!(session.failed.as_deref(), Some("current failure"));
        assert!(!session.opened && !session.no_answer);
    }

    #[test]
    fn dropping_an_old_start_preserves_the_new_sessions_capture() {
        let _guard = TEST_LOCK.lock().unwrap();
        let old = Wayland { generation: begin_start() };
        disable();
        let new = Wayland { generation: begin_start() };
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut c = capture().lock().unwrap();
            c.sides = vec![Side::Right];
            c.active = true;
            c.sink = Some(tx);
            c.hit = Some((Side::Right, 0.5));
            c.pending.push(InputEvent::Key { usage: 4, down: true });
        }
        REMOTE.store(true, Ordering::Release);
        drop(old);
        let preserved = {
            let c = capture().lock().unwrap();
            if let Some(sink) = &c.sink { sink.send(InputEvent::Key { usage: 5, down: true }).unwrap(); }
            c.sides == vec![Side::Right] && rx.try_recv().is_ok()
                && c.hit == Some((Side::Right, 0.5)) && c.pending.len() == 1
                && c.release.is_none() && REMOTE.load(Ordering::Acquire)
        };
        drop(new);
        assert!(preserved, "old platform Drop cleared the new session");
        let c = capture().lock().unwrap();
        assert!(c.sides.is_empty() && c.sink.is_none() && c.pending.is_empty());
        assert!(c.hit.is_none() && c.release.is_some());
        assert!(!REMOTE.load(Ordering::Acquire));
    }
}

mod edge {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/runtime-edge.rs"));
    mod platform {
        use super::Rect;
        pub fn available() -> bool { true }
        pub fn displays() -> Vec<Rect> { vec![Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 }] }
        pub fn cursor() -> Option<(f64, f64)> { Some((0.0, 500.0)) }
        pub fn pointer() -> Option<(f64, f64, u32)> { Some((0.0, 500.0, 0)) }
        pub fn key_held(_: u32, _: crate::config::ShakeKey) -> bool { true }
        pub fn control_down() -> bool { true }
        pub fn place(_: f64, _: f64) {}
    }

    fn run_watcher(wake: crate::shared::Wake) -> Vec<u8> {
        let shared = Arc::new(Shared::new(wake, std::path::PathBuf::new()));
        watch(shared.clone());
        assert!(crate::overlay::COUNTDOWNS.with(|n| n.get()) > 0, "test must reach the countdown");
        assert_eq!(crate::overlay::COUNTDOWNS.with(|n| n.get()), crate::overlay::ENDED.with(|n| n.get()));
        let switches = shared.switches.lock().unwrap().clone();
        switches
    }
    #[test]
    fn disabling_edges_during_wait_cancels_the_switch() {
        assert!(run_watcher(crate::shared::Wake::Disable).is_empty());
    }
    #[test]
    fn retargeting_edges_during_wait_uses_the_new_channel() {
        assert_eq!(run_watcher(crate::shared::Wake::Retarget), vec![3]);
    }
    #[test]
    fn stopping_during_wait_cancels_the_switch() {
        assert!(run_watcher(crate::shared::Wake::Stop).is_empty());
    }
    #[test]
    fn unchanged_settings_still_switch_once() {
        assert_eq!(run_watcher(crate::shared::Wake::Unchanged), vec![2]);
    }
}
