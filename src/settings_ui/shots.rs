//! Screenshot mode, for release pictures made on a CI runner. It is on only while the
//! environment variable FLOCKSWITCH_SHOTS names an output folder (FLOCKSWITCH_LANG picks
//! ko or en). The window then shows built-in demo data instead of the real settings and
//! status (the real settings folder is neither read nor written), takes one picture of
//! itself per page with eframe's own screenshot command, and closes.

use super::*;
use flock_net::layout::Cell;
use flock_net::{MeStatus, Os, PeerConfig, PeerStatus, Role as NetRole, Side, StateStatus, TransferStatus};
use std::path::PathBuf;
use std::sync::OnceLock;

/// The pages to capture, in order, and the name each file gets.
const PAGES: [(Page, &str); 7] = [
    (Page::Connect, "connect"),
    (Page::Cross, "cross"),
    (Page::Keys, "keys"),
    (Page::Clip, "clip"),
    (Page::This, "this"),
    (Page::Buttons, "buttons"),
    (Page::General, "general"),
];
/// Frames and time a page gets to settle before its picture is asked for.
const SETTLE_FRAMES: u32 = 10;
const SETTLE_TIME: Duration = Duration::from_millis(400);
/// The whole run must finish within this, or it gives up (a CI job must not hang).
const DEADLINE: Duration = Duration::from_secs(90);

/// The output folder, when screenshot mode is on.
pub(super) fn dir() -> Option<&'static PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("FLOCKSWITCH_SHOTS").filter(|d| !d.is_empty()).map(PathBuf::from)).as_ref()
}

pub(super) fn active() -> bool {
    dir().is_some()
}

/// "ko" unless FLOCKSWITCH_LANG says "en".
pub(super) fn language() -> &'static str {
    if std::env::var("FLOCKSWITCH_LANG").is_ok_and(|l| l == "en") { "en" } else { "ko" }
}

pub(super) struct Shots {
    dir: PathBuf,
    index: usize,
    frames: u32,
    since: Instant,
    started: Instant,
    asked: bool,
    /// Where the screenshot callback leaves the picture.
    slot: Arc<Mutex<Option<Arc<egui::ColorImage>>>>,
}

impl Shots {
    pub(super) fn new() -> Option<Shots> {
        let dir = dir()?.clone();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("cannot create {}: {e}", dir.display());
            std::process::exit(1);
        }
        Some(Shots { dir, index: 0, frames: 0, since: Instant::now(), started: Instant::now(), asked: false, slot: Arc::default() })
    }
}

const ME: &str = "1f6c0a52e9d34b78";
/// The other three, in the order they are paired: id, Korean name, English name, system.
const OTHERS: [(&str, &str, &str, Os); 3] = [
    ("a3d95e17c0b24f86", "Mac mini", "Mac mini", Os::Mac),
    ("5b82f4a09c1e7d63", "Windows PC", "Windows PC", Os::Windows),
    ("c47e1b93a5d02f68", "Galaxy S25", "Galaxy S25", Os::Android),
];
/// The direction shown on the keyboard page (index among the two of the pair).
const KEY_DIR: usize = 0;

fn other(i: usize) -> PeerConfig {
    let (id, ko, en, os) = OTHERS[i];
    let side = if i == 2 { Side::Bottom } else { Side::Right };
    PeerConfig {
        id: id.into(),
        name: if language() == "en" { en } else { ko }.into(),
        os,
        fingerprint: format!("{id}{id}"),
        side: Some(side),
        role: NetRole::Sub,
        ..Default::default()
    }
}

fn cell(id: &str, col: u8, row: u8) -> Cell {
    Cell { id: id.into(), col, row }
}

/// The demo settings: this computer as master with two computers side by side to its
/// right (so that crossing from one to the other shows) and a phone below it.
pub(super) fn demo_config() -> Config {
    let mut cfg = Config::default();
    cfg.language = language().into();
    let net = &mut cfg.network;
    net.on = true;
    net.notice_accepted = true;
    net.name = if language() == "en" { "ThinkPad" } else { "ThinkPad" }.into();
    net.role = NetRole::Master;
    net.peers = (0..3).map(other).collect();
    net.layout = vec![cell(ME, 1, 1), cell(OTHERS[0].0, 2, 1), cell(OTHERS[1].0, 3, 1), cell(OTHERS[2].0, 1, 2)];
    let mut rules = flock_net::DirRules::default_for(Os::Mac, Os::Windows);
    rules.hangul_src = vec![flock_net::HangulKey::CapsLock];
    rules.caps_long_press = true;
    net.keys.insert(flock_net::keymap::pair_key(Os::Mac, Os::Windows), rules);
    cfg
}

/// The demo status: everyone connected, the mouse on the second computer, a received folder.
pub(super) fn demo_status(cfg: &Config) -> flock_net::Status {
    let peer = |p: &PeerConfig, rtt: f64| PeerStatus {
        id: p.id.clone(),
        name: p.name.clone(),
        os: Some(p.os),
        connected: true,
        rtt_ms: Some(rtt),
        can_send: true,
        via_master: false,
        keyrules: true,
    };
    let rtts = [2.4, 3.1, 9.6];
    flock_net::Status {
        running: true,
        me: MeStatus { id: ME.into(), name: cfg.network.name.clone(), port: flock_net::DEFAULT_PORT, error: None, can_send: true },
        discovered: Vec::new(),
        peers: cfg.network.peers.iter().zip(rtts).map(|(p, r)| peer(p, r)).collect(),
        state: StateStatus { mode: "controlling".into(), peer: Some(OTHERS[1].0.into()) },
        pairing: None,
        transfer: Some(TransferStatus {
            direction: "receive".into(),
            kind: "files".into(),
            done_bytes: 0,
            total_bytes: 0,
            stage: "done".into(),
            saved_in: Some(format!("~/Downloads/flockswitch/{}", OTHERS[1].2)),
        }),
        following: None,
        layout: cfg.network.layout.clone(),
        reconnecting: false,
        keys: cfg.network.all_rules(),
    }
}

pub(super) fn demo_device_status() -> StatusFile {
    StatusFile {
        trigger: Some("MX Master 4".into()),
        channel: Some(1),
        buttons: vec![BUTTON_WHEEL_MODE, BUTTON_GESTURE],
        version: env!("CARGO_PKG_VERSION").into(),
        ..Default::default()
    }
}

impl SettingsApp {
    /// One step of the run, at the start of every frame: save the picture that arrived,
    /// move to the next page, ask for the next picture once the page has settled.
    pub(super) fn shots_step(&mut self, ctx: &egui::Context) {
        let Some(shots) = self.shots.as_mut() else { return };
        ctx.request_repaint();
        if shots.started.elapsed() > DEADLINE {
            eprintln!("screenshot mode: gave up after {} s", DEADLINE.as_secs());
            std::process::exit(1);
        }
        let image = shots.slot.lock().unwrap().take();
        if let Some(image) = image.filter(|_| shots.asked) {
            let name = format!("{}-{}.png", shots.index + 1, PAGES[shots.index].1);
            if let Err(e) = save_png(&shots.dir.join(&name), &image) {
                eprintln!("cannot write {name}: {e}");
                std::process::exit(1);
            }
            shots.index += 1;
            shots.frames = 0;
            shots.since = Instant::now();
            shots.asked = false;
            if shots.index >= PAGES.len() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
        }
        let Some((page, _)) = PAGES.get(shots.index) else { return };
        self.page = *page;
        if *page == Page::Keys {
            (self.key_pair, self.key_dir) = (0, KEY_DIR);
        }
        if !shots.asked {
            shots.frames += 1;
            if shots.frames >= SETTLE_FRAMES && shots.since.elapsed() >= SETTLE_TIME {
                shots.asked = true;
                let (slot, repaint) = (shots.slot.clone(), ctx.clone());
                ctx.request_screenshot(move |image| {
                    *slot.lock().unwrap() = Some(image);
                    repaint.request_repaint();
                });
            }
        }
    }
}

fn save_png(path: &std::path::Path, image: &egui::ColorImage) -> Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut encoder = png::Encoder::new(file, image.width() as u32, image.height() as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
    writer.write_image_data(&bytes)?;
    Ok(())
}
