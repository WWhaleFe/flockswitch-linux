//! The settings window. It runs as its own process (`flockswitch settings`) so the tray
//! app keeps its own event loop; changes are written to config.toml, which the running
//! engine reloads within a second. Every change applies immediately and the window
//! stays open, unlike a native menu that closes after each click.

use crate::backup;
use crate::config::{
    format_color, parse_color, pick, Config, EFFECT_SIZES, Edge, EdgeAnimDirection, EdgeAnimStyle, EffectSettings, Gesture, Haptics, LocateStyle,
    ShakeKey, BUTTON_GESTURE, BUTTON_MIDDLE, BUTTON_WHEEL_MODE, EDGE_ANIM_STYLES, SHAKE_KEYS, SHAKE_TIMES,
};
use crate::device::waveform;
use crate::hotkey::LOCATE;
use crate::i18n::{texts, Texts};
use crate::locate_logic::{self as locate_logic, LocatePath, NoLocate, PortalState};
use crate::net::NetState;
use crate::setup_logic::{Item, ItemId};
use crate::shared::{self, DeviceInfo, HapticPreview, StatusFile};
use crate::update::{self, Release};
use crate::window_size::SizeGuard;
use anyhow::{anyhow, Result};
use auto_launch::AutoLaunch;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Margin, Pos2, Rect, Response, RichText, Role,
    Sense, Shadow, Shape, Stroke, StrokeKind, Ui, UiBuilder, Vec2, WidgetInfo,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_secs(1);

/// The app icon, scaled by build.rs (WINDOW_ICON there).
const ICON_SIDE: u32 = 96;
const ICON_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/window-icon.rgba"));

/// The settings window's size. It is fixed: pages scroll inside it, and neither a page
/// change nor an opened row ever makes the window bigger or smaller.
const WINDOW_SIZE: [f32; 2] = [1040.0, 740.0];
/// The sidebar's width, and the room around the page beside it.
const SIDEBAR_WIDTH: f32 = 220.0;
const PAGE_PADDING: Vec2 = Vec2::new(28.0, 22.0);
/// Every section header (the small title above a box) is this tall, so boxes side by
/// side start at the same line.
const SECTION_HEAD: f32 = 32.0;

mod connect;
mod keys;
mod logitech;
mod setup_ui;
mod shots;

use setup_ui::{SetupRun, SetupUi};

/// Which page the window opens on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Opening {
    Default,
    /// The General page, checking for a new version (the "new version" menu item).
    Update,
    /// The General page, for the setup check (its menu entry and notification).
    Setup,
}

/// The window: one fixed size, no resizing, no maximize button, GNOME ASCII title safety.
fn window_builder(title: &str, icon: egui::IconData) -> egui::ViewportBuilder {
    egui::ViewportBuilder::default()
        .with_title(window_title(title))
        // Matches flockswitch.desktop, so the dock shows the app's name and icon.
        .with_app_id("flockswitch")
        .with_icon(icon)
        .with_inner_size(WINDOW_SIZE)
        .with_min_inner_size(WINDOW_SIZE)
        .with_max_inner_size(WINDOW_SIZE)
        .with_resizable(false)
        .with_maximize_button(false)
        .with_active(true)
}

/// The screenshot output folder (FLOCKSWITCH_SHOTS), when that mode is on.
pub fn shots_dir() -> Option<&'static std::path::PathBuf> {
    shots::dir()
}

/// Opens the window. `lock` keeps a second window from opening; a connection to it
/// means "Settings…" was chosen again, so the window comes back to the front (and to
/// the General page when the connection says "setup" or "update").
pub fn run(lock: std::net::TcpListener, opening: Opening) -> Result<()> {
    if shots::active() {
        crate::i18n::set_language(shots::language());
    } else if let Ok(cfg) = Config::load_or_create() {
        crate::i18n::set_language(&cfg.language);
    }
    let t = texts();
    let icon = egui::IconData { rgba: ICON_RGBA.to_vec(), width: ICON_SIDE, height: ICON_SIDE };
    let options = eframe::NativeOptions { viewport: window_builder(t.settings_title, icon), ..Default::default() };
    eframe::run_native(
        "flockswitch-settings",
        options,
        Box::new(|cc| {
            crate::overlay::set_ready();
            install_fonts(&cc.egui_ctx);
            install_style(&cc.egui_ctx);
            let raise = Arc::new(AtomicBool::new(false));
            let general = Arc::new(AtomicBool::new(false));
            let (ctx, flag, to_general) = (cc.egui_ctx.clone(), raise.clone(), general.clone());
            std::thread::spawn(move || {
                for knock in lock.incoming() {
                    // An older copy knocks without a word; a page name follows otherwise.
                    let mut page = [0u8; 16];
                    let n = knock
                        .ok()
                        .and_then(|mut s| {
                            use std::io::Read;
                            let _ = s.set_read_timeout(Some(Duration::from_millis(300)));
                            s.read(&mut page).ok()
                        })
                        .unwrap_or(0);
                    if matches!(&page[..n], b"setup" | b"update") {
                        to_general.store(true, Ordering::Relaxed);
                    }
                    flag.store(true, Ordering::Relaxed);
                    ctx.request_repaint();
                }
            });
            Ok(Box::new(SettingsApp::new(&cc.egui_ctx, raise, general, opening)))
        }),
    )
    .map_err(|e| anyhow!("cannot open the settings window: {e}"))
}

/// A system font file, memory-mapped rather than read: a CJK font collection is tens
/// of MB, and mapped pages are read only as glyphs are drawn and do not count against
/// the app's memory. Falls back to reading the file.
fn font_file(path: &str) -> Option<egui::epaint::text::Blob> {
    let file = std::fs::File::open(path).ok()?;
    // Safety: system font files are not modified while we run.
    match unsafe { memmap2::Mmap::map(&file) } {
        Ok(map) => Some(std::sync::Arc::new(map)),
        Err(_) => std::fs::read(path).ok().map(|b| std::sync::Arc::new(b) as egui::epaint::text::Blob),
    }
}

type Face = (egui::epaint::text::Blob, u32);

/// The file and face index fontconfig picks for a pattern.
fn fc_match(pattern: &str) -> Option<(String, u32)> {
    let out = std::process::Command::new("fc-match").arg("--format=%{file}\n%{index}").arg(pattern).output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let mut lines = text.lines();
    let file = lines.next()?.trim().to_string();
    let index = lines.next().and_then(|i| i.trim().parse().ok()).unwrap_or(0);
    (!file.is_empty()).then_some((file, index))
}

/// Regular and bold faces of a sans-serif font with Hangul: whatever fontconfig picks
/// for Korean (it knows every distribution's files), else a few well-known files.
fn korean_font() -> Option<(Face, Option<Face>)> {
    if let Some((path, index)) = fc_match("sans-serif:lang=ko") {
        if let Some(file) = font_file(&path) {
            let bold = fc_match("sans-serif:lang=ko:weight=bold")
                .filter(|b| *b != (path.clone(), index))
                .and_then(|(p, i)| font_file(&p).map(|f| (f, i)));
            return Some(((file, index), bold));
        }
    }
    // (regular file, face, bold file, face); face 1 of Noto Sans CJK is Korean.
    const KNOWN: [(&str, u32, &str, u32); 4] = [
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 1, "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc", 1),
        ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 1, "/usr/share/fonts/noto-cjk/NotoSansCJK-Bold.ttc", 1),
        ("/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc", 1, "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Bold.ttc", 1),
        ("/usr/share/fonts/truetype/nanum/NanumGothic.ttf", 0, "/usr/share/fonts/truetype/nanum/NanumGothicBold.ttf", 0),
    ];
    KNOWN.iter().find_map(|(regular, ri, bold, bi)| {
        let file = font_file(regular)?;
        Some(((file, *ri), font_file(bold).map(|f| (f, *bi))))
    })
}

/// Uses the system's Korean-capable font for everything (egui's own font has no
/// Hangul, and mixing it with a Korean fallback looks uneven), plus its bold face for
/// titles, and a symbol fallback for Mac keycap symbols (⌘ ⌥ ⇧ ⌫) if installed.
fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let defaults = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let mut bold_family = Vec::new();
    if let Some(((file, index), bold)) = korean_font() {
        fonts.font_data.insert("system".into(), std::sync::Arc::new(egui::FontData::from_blob(file, index)));
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts.families.entry(family).or_default().insert(0, "system".into());
        }
        bold_family.push(if let Some((file, index)) = bold {
            fonts.font_data.insert("system-bold".into(), std::sync::Arc::new(egui::FontData::from_blob(file, index)));
            "system-bold".to_string()
        } else {
            "system".to_string()
        });
    }
    bold_family.extend(defaults);
    // Mac key symbols (⌘ ⌥ ⇧ ⌫) in the keyboard rules and comparison table.
    if let Some((path, index)) = fc_match("sans-serif:charset=2318") {
        if let Some(file) = font_file(&path) {
            fonts.font_data.insert("symbols".into(), std::sync::Arc::new(egui::FontData::from_blob(file, index)));
            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                fonts.families.entry(family).or_default().push("symbols".into());
            }
            bold_family.push("symbols".to_string());
        }
    }
    fonts.families.insert(FontFamily::Name("bold".into()), bold_family);
    ctx.set_fonts(fonts);
}

fn install_style(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.scroll.bar_width = 6.0;
        style.spacing.interact_size.y = CONTROL_HEIGHT;
        style.spacing.button_padding = Vec2::new(10.0, 4.0);
        for (text_style, size) in [
            (egui::TextStyle::Body, 14.0),
            (egui::TextStyle::Button, 14.0),
            (egui::TextStyle::Small, 12.0),
            (egui::TextStyle::Heading, 20.0),
        ] {
            if let Some(font) = style.text_styles.get_mut(&text_style) {
                font.size = size;
            }
        }
    });
    for (theme, dark) in [(egui::Theme::Light, false), (egui::Theme::Dark, true)] {
        let p = Palette::new(dark);
        let mut visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        visuals.panel_fill = p.bg;
        visuals.window_fill = p.bg;
        visuals.override_text_color = Some(p.text);
        visuals.selection.bg_fill = p.strong.gamma_multiply(0.22);
        visuals.selection.stroke = Stroke::new(1.0, p.ink);
        // Drop-downs and the colour picker: framed like the buttons.
        for (w, fill) in [
            (&mut visuals.widgets.inactive, p.card),
            (&mut visuals.widgets.hovered, p.hover),
            (&mut visuals.widgets.active, p.hover),
            (&mut visuals.widgets.open, p.hover),
        ] {
            w.bg_fill = fill;
            w.weak_bg_fill = fill;
            w.bg_stroke = Stroke::new(1.0, p.track);
            w.corner_radius = CornerRadius::same(9);
            w.expansion = 0.0;
        }
        ctx.set_visuals_of(theme, visuals);
    }
}

/// Colours for the current light/dark appearance. They follow the app icon: a gray
/// ground, white cards, ink-black marks and one fluorescent lime accent.
#[derive(Clone, Copy)]
struct Palette {
    dark: bool,
    bg: Color32,
    /// The ground's gradient, top and bottom (the same as `bg` where it is flat).
    bg_top: Color32,
    bg_bottom: Color32,
    card: Color32,
    border: Color32,
    track: Color32,
    hover: Color32,
    text: Color32,
    muted: Color32,
    /// Lime. Only the "mouse is here" dot and the chosen role use it.
    accent: Color32,
    /// Text and marks on the lime (ink, never white).
    ink: Color32,
    /// What an "on" mark is made of (a switch, a primary button, a ticked box): ink on
    /// the light ground, near-white on the dark one, with `on_strong` drawn on it.
    strong: Color32,
    on_strong: Color32,
    /// A chosen segment: raised above the bar.
    seg_on: Color32,
    /// A chosen "Off" segment and a switch that is off: neutral gray.
    off: Color32,
    /// The dashed outline and the hatching of the "other computer" cards.
    dash: Color32,
    hatch: Color32,
    /// The soft shadow under cards.
    shadow: Color32,
    green: Color32,
    amber: Color32,
    red: Color32,
}

const INK: Color32 = Color32::from_rgb(0x1B, 0x1C, 0x1F);
const LIME: Color32 = Color32::from_rgb(0xD4, 0xF5, 0x3C);

impl Palette {
    fn new(dark: bool) -> Palette {
        if dark {
            let bg = INK;
            Palette {
                dark: true,
                bg,
                bg_top: bg,
                bg_bottom: bg,
                card: Color32::from_rgb(0x25, 0x26, 0x2A),
                border: Color32::from_rgb(0x34, 0x36, 0x3C),
                track: Color32::from_rgb(0x3A, 0x3C, 0x42),
                hover: Color32::from_rgb(0x2E, 0x30, 0x35),
                text: Color32::from_rgb(0xF2, 0xF3, 0xF5),
                muted: Color32::from_rgb(0xA3, 0xA7, 0xB0),
                accent: LIME,
                ink: INK,
                strong: Color32::from_rgb(0xF2, 0xF3, 0xF5),
                on_strong: INK,
                seg_on: Color32::from_rgb(0x4A, 0x4D, 0x55),
                off: Color32::from_rgb(0x5A, 0x5D, 0x64),
                dash: Color32::from_rgb(0x5A, 0x5D, 0x64),
                hatch: Color32::from_rgba_unmultiplied(0x6B, 0x6F, 0x78, 60),
                shadow: Color32::from_black_alpha(90),
                green: Color32::from_rgb(0x30, 0xD1, 0x58),
                amber: Color32::from_rgb(0xFF, 0xB3, 0x40),
                red: Color32::from_rgb(0xFF, 0x5A, 0x50),
            }
        } else {
            Palette {
                dark: false,
                bg: Color32::from_rgb(0xEC, 0xEE, 0xF1),
                bg_top: Color32::from_rgb(0xF0, 0xF1, 0xF3),
                bg_bottom: Color32::from_rgb(0xE4, 0xE6, 0xEA),
                card: Color32::from_rgb(0xFB, 0xFB, 0xFC),
                border: Color32::from_rgb(0xE1, 0xE3, 0xE8),
                track: Color32::from_rgb(0xE4, 0xE6, 0xEA),
                hover: Color32::from_rgb(0xF3, 0xF4, 0xF6),
                text: INK,
                muted: Color32::from_rgb(0x6B, 0x6F, 0x78),
                accent: LIME,
                ink: INK,
                strong: INK,
                on_strong: Color32::WHITE,
                seg_on: Color32::WHITE,
                off: Color32::from_rgb(0xC9, 0xCC, 0xD2),
                dash: Color32::from_rgb(0xA4, 0xA9, 0xB1),
                hatch: Color32::from_rgba_unmultiplied(0xCD, 0xD0, 0xD6, 110),
                // Ink at about 8%.
                shadow: Color32::from_rgba_unmultiplied(0x1B, 0x1C, 0x1F, 20),
                green: Color32::from_rgb(0x2D, 0xA4, 0x4E),
                amber: Color32::from_rgb(0xC9, 0x8A, 0x00),
                red: Color32::from_rgb(0xD9, 0x3B, 0x3B),
            }
        }
    }

    /// The soft wide shadow of cards: offset (0, 6), blur 24.
    fn card_shadow(&self) -> Shadow {
        Shadow { offset: [0, 6], blur: 24, spread: 0, color: self.shadow }
    }
}

/// Paints the ground: a very subtle vertical gradient where the palette has one.
fn paint_ground(ui: &Ui, p: Palette) {
    let rect = ui.max_rect();
    if p.bg_top == p.bg_bottom {
        ui.painter().rect_filled(rect, 0, p.bg);
        return;
    }
    let mut mesh = egui::Mesh::default();
    let uv = Pos2::ZERO;
    for (pos, color) in [
        (rect.left_top(), p.bg_top),
        (rect.right_top(), p.bg_top),
        (rect.right_bottom(), p.bg_bottom),
        (rect.left_bottom(), p.bg_bottom),
    ] {
        mesh.vertices.push(egui::epaint::Vertex { pos, uv, color });
    }
    mesh.indices.extend([0, 1, 2, 0, 2, 3]);
    ui.painter().add(Shape::mesh(mesh));
}

fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

/// The pages of the window, one entry each in the sidebar.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Page {
    Connect,
    Cross,
    Keys,
    Clip,
    This,
    Buttons,
    Hotkeys,
    Edges,
    Devices,
    Effects,
    General,
}

impl Page {
    /// The sidebar, top to bottom: the group title and its pages.
    fn groups(t: &Texts) -> [(&'static str, Vec<(Page, &'static str)>); 3] {
        [
            (
                t.group_network,
                vec![
                    (Page::Connect, t.nav_connect),
                    (Page::Cross, t.net_crossing),
                    (Page::Keys, t.nav_keys),
                    (Page::Clip, t.nav_clip),
                    (Page::This, t.net_this_computer),
                ],
            ),
            (
                t.group_logitech,
                vec![
                    (Page::Buttons, t.nav_buttons),
                    (Page::Hotkeys, t.nav_hotkeys),
                    (Page::Edges, t.nav_edges),
                    (Page::Devices, t.section_devices),
                ],
            ),
            (t.group_app, vec![(Page::Effects, t.section_effects), (Page::General, t.section_general)]),
        ]
    }

    /// The pages that show what the network is doing, and so refresh twice a second.
    fn is_network(self) -> bool {
        matches!(self, Page::Connect | Page::Cross | Page::Keys | Page::Clip | Page::This)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Slot {
    Press(u16),
    Move(u16, Gesture),
}

fn slot_channels(c: &Config, slot: Slot) -> Vec<u8> {
    match slot {
        Slot::Press(cid) => c.button(cid).channels.clone(),
        Slot::Move(cid, g) => c.button(cid).moves.get(g).map(|m| m.channels.clone()).unwrap_or_default(),
    }
}

fn slot_channels_mut(c: &mut Config, slot: Slot) -> Option<&mut Vec<u8>> {
    match slot {
        Slot::Press(cid) => Some(&mut c.button_mut(cid).channels),
        Slot::Move(cid, g) => Some(&mut c.button_mut(cid).moves.get_mut(g)?.channels),
    }
}

#[derive(PartialEq)]
enum Uninstall {
    Idle,
    Confirming,
    /// Waiting for the tray app to hand the button back and quit.
    Waiting(Instant),
    Done,
}

struct SettingsApp {
    net_status: Option<flock_net::Status>,
    net_read: Instant,
    net_address: String,
    /// The port as typed (Apply checks it), what is wrong with it, and one waiting for confirmation.
    net_port_edit: Option<String>,
    net_port_error: Option<&'static str>,
    net_port_confirm: Option<u16>,
    /// The cautions are open after consent.
    net_notice_open: bool,
    /// This computer's private IP addresses, and when they were read.
    net_ips: (Instant, Vec<String>),
    /// The "How does it work?" window is open.
    net_how: bool,
    page: Page,
    /// Channel or locate shortcut currently being recorded.
    recording: Option<u8>,
    gesture_tab: Gesture,
    devices: Vec<DeviceInfo>,
    haptics_dialog: Option<HapticsDialog>,
    edge_confirm: bool,
    /// The computer picked on the layout map, waiting for an empty cell.
    layout_pick: Option<String>,
    /// This computer's network id (from the status, or read from the identity file).
    my_id: Option<String>,
    /// The keyboard pane: which OS pair and which direction of it is shown.
    key_pair: usize,
    key_dir: usize,
    /// The "compare keys by system" window is open.
    keys_cmp: bool,
    /// The window looked up the newer version the tray app found (once).
    update_probed: bool,
    /// "Find connections again" was pressed (shows "Searching" for a moment).
    rescan_at: Option<Instant>,
    uninstall: Uninstall,
    cfg: Config,
    status: StatusFile,
    autostart: Option<AutoLaunch>,
    autostart_on: bool,
    error: Option<String>,
    last_refresh: Instant,
    /// Frames left in which to bring the window to the front (see `bring_to_front`).
    raise_frames: u8,
    /// Window title last set, to follow a language change.
    title: &'static str,
    raise: Arc<AtomicBool>,
    icon: egui::TextureHandle,
    /// New-version check and download, run on a background thread.
    update: Arc<Mutex<UpdateUi>>,
    /// When the e-mail address was copied (shows "Copied" for a moment).
    copied_at: Option<Instant>,
    /// "Reset settings…" was pressed and waits for its confirmation.
    confirm_reset: bool,
    /// A colour being dragged in the picker; saved when the mouse button comes up.
    color_draft: Option<(EffectColor, [u8; 3])>,
    /// Keeps the window's size when it moves to a monitor with another scale.
    size_guard: SizeGuard,
    /// "Load settings…" is open: the files to choose from.
    backup_picker: Option<BackupPicker>,
    /// What the last save or load did: whether it went well, and what to say.
    backup_note: Option<(bool, String)>,
    /// Save was refused: the folder is full (`backup::MAX_FILES`).
    backup_full: bool,
    /// How many settings files the folder has, re-read with the settings.
    backup_count: usize,
    /// "Manage…" is open.
    backup_manager: Option<BackupManager>,
    /// The Linux setup check at the top of the General page.
    setup: SetupUi,
    /// Screenshot mode (FLOCKSWITCH_SHOTS): demo data and one picture per page.
    shots: Option<shots::Shots>,
    /// The tray menu or a notification asked for the General page while the window was open.
    to_general: Arc<AtomicBool>,
}

/// Choosing a saved settings file to load.
struct BackupPicker {
    files: Vec<backup::Entry>,
    chosen: Option<usize>,
    /// Why the chosen file could not be loaded.
    error: Option<String>,
}

/// The saved settings files, to rename or delete.
struct BackupManager {
    files: Vec<backup::Entry>,
    /// The local time zone's offset, for the dates.
    offset: i64,
    /// The file being renamed, and the name typed so far.
    renaming: Option<(std::path::PathBuf, String)>,
    /// The file whose Delete waits for its confirmation.
    deleting: Option<std::path::PathBuf>,
    /// Why the rename or delete of a file did not happen.
    error: Option<(std::path::PathBuf, String)>,
    /// What the last delete did, for the line under the list.
    note: Option<(bool, String)>,
}

/// What one row of the manager asks for.
enum ManageAction {
    StartRename(std::path::PathBuf, String),
    Rename(std::path::PathBuf, String),
    StartDelete(std::path::PathBuf),
    Delete(std::path::PathBuf),
    Cancel,
}

/// Which on-screen effect a colour row sets.
#[derive(Clone, Copy, PartialEq)]
enum EffectColor {
    Edge,
    Locate,
}

/// The ready-made colours of a colour row; any other comes from the picker beside them.
const COLOR_PRESETS: [[u8; 3]; 7] = [
    [0xFF, 0xCC, 0x00],
    [0xFF, 0x7A, 0x1A],
    [0xFF, 0x3B, 0x30],
    [0xFF, 0x2D, 0x92],
    [0x0A, 0x84, 0xFF],
    [0x34, 0xC7, 0x59],
    [0xFF, 0xFF, 0xFF],
];

fn how_viewport() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("network-how")
}

/// What the update card shows.
#[derive(Clone, Default)]
enum UpdateUi {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Failed(String),
    Downloading,
    /// The download matched its checksum and the new program is being put in place.
    Installing,
    InstallFailed(Release, String),
    /// Installed and the tray app is restarting into it: this window closes, and the
    /// new version opens it again.
    Restarting,
    /// Installed, but starting the new version failed; the next start uses it.
    Installed,
}

impl SettingsApp {
    fn new(ctx: &egui::Context, raise: Arc<AtomicBool>, to_general: Arc<AtomicBool>, opening: Opening) -> SettingsApp {
        let autostart = crate::tray::autostart();
        let autostart_on = autostart.as_ref().is_some_and(|a| a.is_enabled().unwrap_or(false));
        let image = egui::ColorImage::from_rgba_unmultiplied([ICON_SIDE as usize; 2], ICON_RGBA);
        let mut app = SettingsApp {
            net_status: None,
            net_read: Instant::now() - Duration::from_secs(1),
            net_address: String::new(),
            net_port_edit: None,
            net_port_error: None,
            net_port_confirm: None,
            net_notice_open: false,
            net_ips: (Instant::now() - Duration::from_secs(60), Vec::new()),
            net_how: false,
            page: if opening == Opening::Default { Page::Connect } else { Page::General },
            recording: None,
            gesture_tab: Gesture::Press,
            devices: Vec::new(),
            haptics_dialog: None,
            edge_confirm: false,
            layout_pick: None,
            my_id: None,
            key_pair: 0,
            key_dir: 0,
            keys_cmp: false,
            update_probed: false,
            rescan_at: None,
            uninstall: Uninstall::Idle,
            cfg: if shots::active() { Config::default() } else { Config::load_or_create().unwrap_or_default() },
            status: StatusFile::default(),
            autostart,
            autostart_on,
            error: None,
            last_refresh: Instant::now() - REFRESH,
            // Started from the tray app: come to the front once at start.
            raise_frames: 2,
            raise,
            title: texts().settings_title,
            icon: ctx.load_texture("app-icon", image, egui::TextureOptions::LINEAR),
            update: Arc::new(Mutex::new(UpdateUi::Idle)),
            copied_at: None,
            confirm_reset: false,
            color_draft: None,
            size_guard: SizeGuard::default(),
            backup_picker: None,
            backup_note: None,
            backup_full: false,
            backup_count: 0,
            backup_manager: None,
            setup: SetupUi::default(),
            to_general,
            shots: shots::Shots::new(),
        };
        if app.shots.is_some() {
            // The pictures are always dark, whatever the machine is set to.
            ctx.set_theme(egui::Theme::Dark);
            app.cfg = shots::demo_config();
            app.net_status = Some(shots::demo_status(&app.cfg));
            app.status = shots::demo_device_status();
            app.net_ips.1 = vec!["192.168.0.12".into()];
            *app.update.lock().unwrap() = UpdateUi::UpToDate;
            return app;
        }
        app.refresh();
        // Open before the tray app's first setup check (a new settings file): that check
        // need not open it again.
        if app.cfg.setup.first_check {
            app.apply(|c| c.setup.first_check = false);
        }
        if opening == Opening::Update || app.cfg.check_updates {
            app.check_update(ctx);
        }
        app
    }

    /// Asks GitHub for the latest version in the background. It never installs: a newer
    /// one is offered with "Update now" (automatic install is the tray app's daily check,
    /// with that setting on).
    fn check_update(&self, ctx: &egui::Context) {
        let (state, ctx) = (self.update.clone(), ctx.clone());
        *state.lock().unwrap() = UpdateUi::Checking;
        std::thread::spawn(move || {
            let next = match update::latest() {
                Ok(r) if r.is_newer_than_this_build() => {
                    log::info!("update check: latest {} (newer)", r.version);
                    UpdateUi::Available(r)
                }
                Ok(r) => {
                    log::info!("update check: latest {} (up to date)", r.version);
                    UpdateUi::UpToDate
                }
                Err(e) => {
                    log::info!("update check failed: {e:#}");
                    UpdateUi::Failed(format!("{e:#}"))
                }
            };
            *state.lock().unwrap() = next;
            ctx.request_repaint();
        });
    }

    /// "Update now": downloads, checks and installs the new version, then has the tray
    /// app restart into it. Protected folders use administrator authentication.
    fn install_update(&self, ctx: &egui::Context, release: Release) {
        let exe = match update::manual_target(&update::install_kind()) {
            Ok(exe) => exe,
            Err(e) => {
                *self.update.lock().unwrap() = UpdateUi::InstallFailed(release, format!("{e:#}"));
                return;
            }
        };
        // A release without a file for this computer: its page says what to do.
        if release.asset.is_none() {
            update::open_url(&release.page);
            return;
        }
        let (state, ctx) = (self.update.clone(), ctx.clone());
        *state.lock().unwrap() = UpdateUi::Downloading;
        log::info!("update {}: installing over {} (from the settings window)", release.version, exe.display());
        std::thread::spawn(move || {
            let phase = |p: update::Phase| {
                if p == update::Phase::Installing {
                    *state.lock().unwrap() = UpdateUi::Installing;
                    ctx.request_repaint();
                }
            };
            let next = match update::install(&release, &exe, &phase) {
                // The new tray app takes over from the running one and reopens this window.
                Ok(()) => match update::start_new_version(&exe, &["tray", "--settings"]) {
                    Ok(()) => UpdateUi::Restarting,
                    Err(e) => {
                        log::warn!("update {}: installed, but cannot start it: {e:#}", release.version);
                        UpdateUi::Installed
                    }
                },
                Err(e) => {
                    log::warn!("update {}: not installed: {e:#}", release.version);
                    UpdateUi::InstallFailed(release, format!("{e:#}"))
                }
            };
            *state.lock().unwrap() = next;
            ctx.request_repaint();
        });
    }

    /// Re-reads settings, the remembered devices and what the engine sees, so the window
    /// also reflects changes made by hand or by a master.
    fn refresh(&mut self) {
        if self.shots.is_some() {
            return;
        }
        self.last_refresh = Instant::now();
        if let Ok(cfg) = Config::load_or_create() {
            crate::i18n::set_language(&cfg.language);
            self.cfg = cfg;
        }
        self.status = shared::load_status_file();
        self.devices = shared::load_catalog();
        for d in &mut self.devices {
            d.online = self.status.online.iter().any(|key| key.eq_ignore_ascii_case(&d.key));
        }
        self.backup_count = backup::list(&backup::folder()).len();
        if self.backup_count < backup::MAX_FILES {
            self.backup_full = false;
        }
    }

    /// Applies one change on top of the file's current content, validates, saves.
    fn apply(&mut self, change: impl FnOnce(&mut Config)) {
        match Config::update(change) {
            Ok(next) => {
                crate::i18n::set_language(&next.language);
                self.cfg = next;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }
}

impl eframe::App for SettingsApp {
    /// Runs even while the window is minimized or fully covered (when `ui` does not),
    /// so "Settings…" can still bring it back.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.bring_to_front(ctx);
    }

    /// Closed while recording a shortcut: give the tray app its shortcuts back now.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_recording();
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

impl SettingsApp {
    /// One frame of the window (`App::ui`, without the frame handle, so a test can run it).
    fn show(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        self.shots_step(&ctx);
        let t = texts();
        if self.title != t.settings_title {
            self.title = t.settings_title;
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(window_title(t.settings_title)));
        }
        keep_size(&ctx, &mut self.size_guard, WINDOW_SIZE, WINDOW_SIZE);
        let p = Palette::new(ui.visuals().dark_mode);
        paint_ground(ui, p);
        // After uninstalling, show only the result: re-reading the settings now would
        // create the deleted settings folder again.
        if self.uninstall_progress() {
            ctx.request_repaint_after(Duration::from_millis(300));
            let panel = egui::Frame::new().inner_margin(Margin::symmetric(18, 12));
            egui::CentralPanel::default().frame(panel).show(ui, |ui| self.uninstall_result(ui, t, p));
            return;
        }
        // Updated: the new version opens this window again once it has started.
        if matches!(*self.update.lock().unwrap(), UpdateUi::Restarting) {
            self.haptics_dialog = None;
            self.stop_recording();
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.last_refresh.elapsed() >= REFRESH {
            self.refresh();
        }
        self.poll_network();
        if self.shots.is_none() {
            self.setup_tick(&ctx);
        }
        if self.to_general.swap(false, Ordering::Relaxed) {
            self.page = Page::General;
            self.stop_recording();
            self.layout_pick = None;
        }
        // The tray app knows of a newer version: look it up here too, so that "Update
        // now" is on the page without waiting for a check.
        if !self.update_probed && self.status.update_available.is_some() {
            self.update_probed = true;
            if matches!(*self.update.lock().unwrap(), UpdateUi::Idle | UpdateUi::UpToDate | UpdateUi::Failed(_)) {
                self.check_update(&ctx);
            }
        }
        ctx.request_repaint_after(if self.page.is_network() { Duration::from_millis(500) } else { REFRESH });
        if let Some(n) = self.recording {
            self.capture_shortcut(&ctx, t, n);
        }

        // While the haptics window is open this one cannot be closed or used, and
        // coming back to it hands the focus straight on.
        if self.haptics_dialog.is_some() {
            self.backup_picker = None;
            self.backup_manager = None;
            if ctx.input(|i| i.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
            if ctx.input(|i| i.focused) {
                ctx.send_viewport_cmd_to(haptics_viewport(), egui::ViewportCommand::Focus);
            }
        }
        let modal = self.edge_confirm
            || self.haptics_dialog.is_some()
            || self.backup_picker.is_some()
            || self.backup_manager.is_some()
            || self.net_port_confirm.is_some()
            || self.setup.confirm.is_some();

        // The sidebar and the page are two fixed areas; only the page scrolls, inside its
        // area, so the window itself never changes size.
        let full = ui.max_rect();
        let side_rect = Rect::from_min_size(full.min, Vec2::new(SIDEBAR_WIDTH, full.height()));
        let main_rect = Rect::from_min_max(Pos2::new(side_rect.right(), full.top()), full.max);
        ui.painter().vline(side_rect.right(), full.y_range(), Stroke::new(1.0, p.border));

        let mut side = ui.new_child(UiBuilder::new().max_rect(side_rect.shrink2(Vec2::new(12.0, 16.0))));
        if modal {
            side.disable();
        }
        // The status card keeps the bottom (measured first, so its height is reserved); the
        // pages above it scroll if they do not fit.
        let inner = side.max_rect();
        let mut probe = side.new_child(UiBuilder::new().max_rect(inner).invisible());
        self.status_card(&mut probe, t, p);
        let card_height = probe.min_rect().height();
        let card_rect = Rect::from_min_max(Pos2::new(inner.left(), inner.bottom() - card_height), inner.max);
        let nav_rect = Rect::from_min_max(inner.min, Pos2::new(inner.right(), card_rect.top() - 12.0));
        let mut card = side.new_child(UiBuilder::new().max_rect(card_rect));
        self.status_card(&mut card, t, p);
        let mut nav = side.new_child(UiBuilder::new().max_rect(nav_rect));
        egui::ScrollArea::vertical().id_salt("sidebar").auto_shrink([false, false]).show(&mut nav, |ui| self.sidebar(ui, t, p));

        let mut main = ui.new_child(UiBuilder::new().max_rect(main_rect.shrink2(Vec2::new(PAGE_PADDING.x, PAGE_PADDING.y))));
        if modal {
            main.disable();
        }
        egui::ScrollArea::vertical().id_salt(("page", self.page)).auto_shrink([false, false]).show(&mut main, |ui| {
            // Room on the right for the scroll bar, so rows never run under it.
            ui.set_max_width(ui.available_width() - 10.0);
            if self.status.hid_denied {
                let open = (self.page != Page::General).then_some(t.setup_open);
                let msg = format!("{}: {}", t.hid_denied_title, t.hid_denied);
                if note_box(ui, p, NoteKind::Error, &msg, open) {
                    self.page = Page::General;
                    self.stop_recording();
                }
                ui.add_space(14.0);
            }
            if let Some(err) = self.error.clone() {
                if note_box(ui, p, NoteKind::Error, &err, None) {
                    self.error = None;
                }
                ui.add_space(14.0);
            }
            self.page_body(ui, t, p);
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(format!("{} · flockswitch {}", t.applies_immediately, env!("CARGO_PKG_VERSION"))).size(12.0).color(p.muted));
            });
            ui.add_space(4.0);
        });
        self.port_confirm_window(&ctx, t, p);
        self.how_window(&ctx, t);
        self.keycmp_window(&ctx, t);
        self.haptics_window(&ctx, t);
        self.edge_confirm_window(&ctx, t, p);
        self.backup_picker_ui(&ctx, t, p);
        self.backup_manager_ui(&ctx, t, p);
        self.setup_confirm_ui(&ctx, p);
    }

    /// Un-minimizes and focuses the window when it starts and whenever "Settings…" is
    /// chosen again. Repeated on the next frame: un-minimizing takes effect first.
    fn bring_to_front(&mut self, ctx: &egui::Context) {
        if self.raise.swap(false, Ordering::Relaxed) {
            self.raise_frames = 2;
        }
        if self.raise_frames == 0 {
            return;
        }
        self.raise_frames -= 1;
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint_after(Duration::from_millis(100));
    }

    fn net_request(&mut self, request: flock_net::Request) {
        if let Err(e) = flock_net::write_request(crate::net::config_dir(), &request) {
            self.error = Some(format!("{e:#}"));
        }
    }

    /// A sub with a master follows the master's settings, so they are read-only here.
    fn follows_master(&self) -> bool {
        self.cfg.network.role == flock_net::Role::Sub && !self.cfg.network.peers.is_empty()
    }

    fn master_name(&self) -> String {
        self.net_status
            .as_ref()
            .and_then(|s| s.following.clone())
            .or_else(|| self.cfg.network.peers.first().map(|p| p.name.clone()))
            .unwrap_or_default()
    }

    /// "Following the master's settings", above what a sub cannot change.
    fn follow_banner(&self, ui: &mut Ui, p: Palette, t: &Texts) {
        note_box(ui, p, NoteKind::Info, &format!("{}{}{}", t.net_follow_pre, self.master_name(), t.net_follow_post), None);
    }

    fn notice_items(&self, ui: &mut Ui, p: Palette, t: &Texts) {
        for (i, item) in t.net_notice_items.iter().enumerate() {
            ui.add_space(6.0);
            ui.label(RichText::new(format!("{}. {item}", i + 1)).size(13.0).color(p.text));
        }
    }

    /// Checks the port typed and, when it is fine, saves it (a port other than the
    /// default first asks for confirmation).
    fn try_port(&mut self, t: &Texts, port: Option<u16>, current: u16) {
        let Some(port) = port else {
            self.net_port_error = Some(t.net_port_invalid);
            return;
        };
        match flock_net::check_port(port, current) {
            Err(flock_net::PortProblem::Reserved) => self.net_port_error = Some(t.net_port_reserved),
            Err(flock_net::PortProblem::Ours) => self.net_port_error = Some(t.net_port_ours),
            Err(flock_net::PortProblem::InUse) => self.net_port_error = Some(t.net_port_in_use),
            Ok(()) => {
                self.net_port_error = None;
                if port == self.cfg.network.port {
                    self.net_port_edit = None;
                } else if port == flock_net::DEFAULT_PORT {
                    self.apply(|c| c.network.port = port);
                    self.net_port_edit = None;
                } else {
                    self.net_port_confirm = Some(port);
                }
            }
        }
    }

    fn port_confirm_window(&mut self, ctx: &egui::Context, t: &Texts, p: Palette) {
        let Some(port) = self.net_port_confirm else { return };
        let (mut close, mut change) = (false, false);
        let frame = egui::Frame::new()
            .fill(p.bg)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(16)
            .inner_margin(Margin::same(18))
            .shadow(ctx.global_style().visuals.window_shadow);
        let response = egui::Modal::new(egui::Id::unique("port-confirm")).frame(frame).show(ctx, |ui| {
            ui.set_width(420.0);
            ui.label(RichText::new(format!("{}: {port}", t.net_port)).font(bold(16.0)).color(p.text));
            ui.add_space(8.0);
            ui.label(RichText::new(t.net_port_confirm).size(13.0).color(p.text));
            ui.add_space(14.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                change = button(ui, p, t.net_port_change, ButtonKind::Primary).clicked();
                close = button(ui, p, t.cancel, ButtonKind::Plain).clicked();
            });
        });
        if change {
            self.apply(|c| c.network.port = port);
            self.net_port_edit = None;
            self.net_port_confirm = None;
        } else if close || response.should_close() {
            self.net_port_confirm = None;
        }
    }

    /// "How does it work?": a window of its own with a diagram and the steps.
    fn how_window(&mut self, ctx: &egui::Context, t: &Texts) {
        if !self.net_how {
            return;
        }
        let builder = egui::ViewportBuilder::default()
            .with_title(window_title(t.net_how_button))
            .with_inner_size([560.0, 640.0])
            .with_min_inner_size([420.0, 360.0]);
        ctx.show_viewport_immediate(how_viewport(), builder, |ui, _| {
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                self.net_how = false;
                return;
            }
            let p = Palette::new(ui.visuals().dark_mode);
            let panel = egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(18, 14));
            egui::CentralPanel::default().frame(panel).show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    how_diagram(ui, p, t);
                    ui.add_space(14.0);
                    for (i, item) in t.net_how_items.iter().enumerate() {
                        let (title, body) = item.split_once(" - ").unwrap_or(("", item));
                        ui.label(RichText::new(format!("{}. {title}", i + 1)).font(bold(14.0)).color(p.text));
                        ui.add_space(2.0);
                        ui.label(RichText::new(body).size(13.0).color(p.text));
                        ui.add_space(12.0);
                    }
                });
            });
        });
    }

    /// The countdown effect: which one, and its look. A sub shows the master's settings
    /// (the page says so once and turns the rows off).
    fn edge_effects_card(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        section_with(ui, p, t.section_edge_effects, None, Some((t.fx_saved_each, p.muted)), |ui| {
            self.edge_anim_rows(ui, t, p);
            ui.add_space(8.0);
        });
    }

    /// Which countdown is drawn at the edge, and the chosen effect's own look: how it
    /// drains (where that can be chosen), its colour and size.
    fn edge_anim_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let width = (ui.available_width() * 0.72).min(280.0);
        let options: Vec<(EdgeAnimStyle, &str)> = EDGE_ANIM_STYLES.into_iter().zip(t.edge_anim_names).collect();
        let mut style = self.cfg.edges.anim_style;
        let (mut changed, mut preview) = (false, false);
        labeled_row(ui, p, t.edge_anim, width, ROW_SMALL, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                preview = style != EdgeAnimStyle::None && button(ui, p, t.preview, ButtonKind::Plain).clicked();
                changed = combo(ui, "edge-anim", &mut style, &options);
            });
            false
        });
        if changed {
            self.apply(|c| c.edges.anim_style = style);
        }
        if preview {
            if crate::edge::available() {
                crate::edge::preview(&self.cfg.edges, self.cfg.network.cross_delay_ms);
            } else {
                crate::edge_wayland::preview(&self.cfg.edges, self.cfg.network.cross_delay_ms);
            }
        }
        // What is shown below is the effect now chosen, with its own settings.
        let style = self.cfg.edges.anim_style;
        // Random draws each effect with that effect's own settings: nothing to set here.
        if style == EdgeAnimStyle::Random {
            hint(ui, p, t.edge_random_hint);
            return;
        }
        let Some(effect) = self.cfg.edges.chosen().cloned() else { return };
        if style.can_shrink() {
            let mut shrink = effect.shrink;
            if switch_row(ui, p, t.edge_anim_shrink, None, &mut shrink) {
                self.apply_effect(|e| e.shrink = shrink);
            }
        }
        if style.has_direction() && !(effect.shrink && style.can_shrink()) {
            let labels = if style == EdgeAnimStyle::Bar {
                [t.bar_forward, t.bar_reverse, t.bar_both]
            } else {
                [t.dir_clockwise, t.dir_counter, t.dir_both]
            };
            let choices = [
                (EdgeAnimDirection::Clockwise, labels[0]),
                (EdgeAnimDirection::CounterClockwise, labels[1]),
                (EdgeAnimDirection::Both, labels[2]),
            ];
            let mut direction = effect.direction;
            if labeled_row(ui, p, t.edge_anim_direction, width, ROW_SMALL, |ui| segmented(ui, p, "edge-anim-dir", &mut direction, &choices, SEG_SMALL)) {
                self.apply_effect(|e| e.direction = direction);
            }
        }
        self.effect_color_row(ui, t, p, EffectColor::Edge, false);
        let mut size = effect.size;
        if size_row(ui, p, t, "edge-size", &mut size) {
            self.apply_effect(|e| e.size = size);
        }
    }

    /// Changes the settings of the countdown effect chosen in the file.
    fn apply_effect(&mut self, change: impl FnOnce(&mut EffectSettings)) {
        self.apply(|c| {
            let style = c.edges.anim_style;
            if let Some(e) = c.edges.effects.get_mut(style) {
                change(e);
            }
        });
    }

    fn saved_color(&self, slot: EffectColor) -> String {
        match slot {
            EffectColor::Edge => self.cfg.edges.chosen().map_or_else(|| self.cfg.edges.anim_color.clone(), |e| e.color.clone()),
            EffectColor::Locate => self.cfg.locate.color.clone(),
        }
    }

    fn save_color(&mut self, slot: EffectColor, color: [u8; 3]) {
        let text = format_color(color);
        match slot {
            EffectColor::Edge => self.apply_effect(|e| e.color = text),
            EffectColor::Locate => self.apply(|c| c.locate.color = text),
        }
    }

    /// Ready-made colours and a picker for any other, with a Preview button if asked for.
    /// Returns true when it was pressed.
    fn effect_color_row(&mut self, ui: &mut Ui, t: &Texts, p: Palette, slot: EffectColor, offer_preview: bool) -> bool {
        let saved = parse_color(&self.saved_color(slot)).unwrap_or(COLOR_PRESETS[0]);
        let mut shown = self.color_draft.filter(|(s, _)| *s == slot).map_or(saved, |(_, c)| c);
        let (mut preset, mut dragged, mut preview) = (None, false, false);
        let width = ui.available_width() - 70.0;
        labeled_row(ui, p, t.effect_color, width, ROW_SMALL, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                preview = offer_preview && button(ui, p, t.preview, ButtonKind::Plain).clicked();
                let own = !COLOR_PRESETS.contains(&shown);
                let mut picked = if own { shown } else { [0x8E, 0x8E, 0x93] };
                ui.scope(|ui| {
                    ui.spacing_mut().interact_size = Vec2::splat(18.0);
                    let widgets = &mut ui.visuals_mut().widgets;
                    for w in [&mut widgets.inactive, &mut widgets.hovered, &mut widgets.active, &mut widgets.open] {
                        w.corner_radius = CornerRadius::same(9);
                    }
                    let resp = ui.color_edit_button_srgb(&mut picked);
                    if resp.changed() {
                        shown = picked;
                        dragged = true;
                    }
                    let c = resp.rect.center();
                    if own {
                        ui.painter().circle_stroke(c, 11.0, Stroke::new(1.8, p.text));
                    } else {
                        ui.painter().text(c, Align2::CENTER_CENTER, "+", FontId::proportional(15.0), Color32::WHITE);
                    }
                });
                for color in COLOR_PRESETS.iter().rev() {
                    if color_swatch(ui, p, *color, *color == shown).clicked() {
                        preset = Some(*color);
                    }
                }
            });
            false
        });
        if let Some(color) = preset {
            self.color_draft = None;
            self.save_color(slot, color);
        } else if dragged {
            self.color_draft = Some((slot, shown));
            ui.ctx().request_repaint();
        }
        // Saved once the mouse button is up, not on every step of a drag.
        if let Some((s, color)) = self.color_draft.filter(|(s, _)| *s == slot) {
            if !ui.ctx().input(|i| i.pointer.any_down()) {
                self.color_draft = None;
                self.save_color(s, color);
            }
        }
        preview
    }

    fn stop_recording(&mut self) {
        if self.recording.take().is_some() {
            crate::hotkey::end_pause();
        }
    }

    /// Takes the next key combination pressed in this window as channel `n`'s shortcut.
    fn capture_shortcut(&mut self, ctx: &egui::Context, t: &Texts, n: u8) {
        crate::hotkey::hold_pause();
        ctx.request_repaint_after(Duration::from_millis(500));
        let events = ctx.input(|i| i.events.clone());
        for event in events {
            let egui::Event::Key { key, physical_key, pressed: true, modifiers, .. } = event else { continue };
            // The physical key, not what the keyboard layout makes of it with the modifiers held.
            let key = physical_key.unwrap_or(key);
            if key == egui::Key::Escape {
                self.stop_recording();
                return;
            }
            let Some(text) = shortcut_text(key, modifiers) else { continue };
            self.stop_recording();
            if (LOCATE..=3).any(|m| m != n && self.cfg.shortcut(m) == text) {
                self.error = Some(t.hotkey_duplicate.into());
                return;
            }
            self.apply(|c| c.set_shortcut(n, &text));
            return;
        }
    }

    /// One recordable shortcut: its slot `n` (channel, or LOCATE), the keys and buttons
    /// to change or clear them, and an "i" after the label explaining `info`.
    fn shortcut_row(&mut self, ui: &mut Ui, t: &Texts, p: Palette, n: u8, label: &str, info: Option<&str>) {
        let top_left = ui.cursor().min;
        let width = (ui.available_width() * 0.66).min(300.0);
        let (mut start, mut clear) = (false, false);
        let key = self.cfg.shortcut(n).to_string();
        let recording = self.recording == Some(n);
        let failed = !recording && !key.is_empty() && self.status.hotkeys_failed.contains(&n);
        let (shown, color) = if recording {
            (t.hotkey_press.to_string(), p.amber)
        } else if key.is_empty() {
            (t.hotkey_none.to_string(), p.muted)
        } else {
            (crate::hotkey::display(&key), if failed { p.amber } else { p.text })
        };
        labeled_row(ui, p, label, width, ROW_SMALL, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if !recording && !key.is_empty() && button(ui, p, t.hotkey_clear, ButtonKind::Plain).clicked() {
                    clear = true;
                }
                if !recording && button(ui, p, t.hotkey_change, ButtonKind::Plain).clicked() {
                    start = true;
                }
                ui.add_space(4.0);
                ui.label(RichText::new(shown).font(bold(14.0)).color(color));
            });
            false
        });
        if let Some(text) = info {
            let label_width = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(14.5), p.text).size().x;
            let center = Pos2::new(top_left.x + label_width + 16.0, top_left.y + ROW_SMALL / 2.0);
            info_icon(ui, p, center, &format!("shortcut-help-{n}"), text);
        }
        if failed {
            amber_hint(ui, p, t.hotkey_taken);
        }
        if crate::edge_wayland::session_is_wayland() && !recording && !key.is_empty() {
            self.portal_trigger_hint(ui, t, p, n, &key);
        }
        if start {
            self.recording = Some(n);
            self.error = None;
            crate::hotkey::hold_pause();
        }
        if clear {
            self.apply(|c| c.set_shortcut(n, ""));
        }
    }

    /// Wayland: how the shortcuts through the system's shortcut service stand, as the
    /// running flockswitch wrote it. `compact`: only what needs the user (a question,
    /// a refusal, no service), not the note on a successful registration.
    fn portal_box(&mut self, ui: &mut Ui, t: &Texts, p: Palette, compact: bool) {
        let exe = crate::tray::launch_path().map(|p| p.display().to_string()).unwrap_or_else(|| "flockswitch".into());
        let with_commands = |text: &str| format!("{text}\n\n{}", locate_logic::fallback_commands(&exe).join("\n"));
        match PortalState::parse(&self.status.shortcuts_portal) {
            Some(PortalState::Bound(version)) => {
                if compact {
                    return;
                }
                let change = (version >= 2).then_some(t.portal_change);
                if note_box(ui, p, NoteKind::Info, &t.portal_bound.replace("{desktop}", &desktop_name()), change) {
                    shared::request_configure_shortcuts();
                }
            }
            Some(PortalState::Asking) => {
                note_box(ui, p, NoteKind::Info, t.portal_asking, None);
            }
            Some(PortalState::Denied) => {
                note_box(ui, p, NoteKind::Error, &with_commands(t.portal_denied), None);
            }
            Some(PortalState::Missing) => {
                note_box(ui, p, NoteKind::Info, &with_commands(t.portal_missing), None);
            }
            Some(PortalState::NoAppId) => {
                let open = self.setup_item(ItemId::MenuEntry).is_some_and(|i| i.actionable()).then_some(t.setup_open);
                if note_box(ui, p, NoteKind::Error, &with_commands(t.portal_no_app_id), open) {
                    self.page = Page::General;
                    self.stop_recording();
                }
            }
            Some(PortalState::Error) => {
                note_box(ui, p, NoteKind::Error, &with_commands(t.portal_error), None);
            }
            Some(PortalState::Off) => {}
            None => {
                if !compact {
                    note_box(ui, p, NoteKind::Info, t.portal_waiting, None);
                }
            }
        }
    }

    /// Wayland: under a shortcut's row, the key the system bound it to, in amber when
    /// that is not the one chosen here (the system keeps the user's own choice).
    fn portal_trigger_hint(&self, ui: &mut Ui, t: &Texts, p: Palette, n: u8, key: &str) {
        if !matches!(PortalState::parse(&self.status.shortcuts_portal), Some(PortalState::Bound(_))) {
            return;
        }
        match self.status.shortcuts_bound.iter().find(|b| b.slot == n) {
            None => hint(ui, p, t.portal_not_bound),
            Some(b) if b.trigger.trim().is_empty() => amber_hint(ui, p, t.portal_trigger_none),
            Some(b) if locate_logic::trigger_matches(key, &b.trigger) => {
                hint(ui, p, &t.portal_trigger_same.replace("{trigger}", &locate_logic::pretty_trigger(&b.trigger)))
            }
            Some(b) => amber_hint(ui, p, &t.portal_trigger_other.replace("{trigger}", &locate_logic::pretty_trigger(&b.trigger))),
        }
    }

    /// Highlighting the cursor: on/off, the effect, and what brings it up.
    fn locate_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        section(ui, p, t.section_locate, |ui| {
            self.locate_rows(ui, t, p);
            ui.add_space(8.0);
        });
    }

    fn locate_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        // Wayland: what brings the highlight up here, and whether it can be drawn.
        let wayland = !crate::edge::available();
        if wayland {
            ui.add_space(8.0);
            let exe = crate::tray::launch_path().map(|p| p.display().to_string()).unwrap_or_else(|| "flockswitch".into());
            let how = t.locate_wayland_how.replace("{desktop}", &desktop_name()).replace("{command}", &format!("{exe} locate"));
            note_box(ui, p, NoteKind::Info, &how, None);
            match wayland_locate_path() {
                LocatePath::None(NoLocate::Misaligned) => {
                    ui.add_space(6.0);
                    note_box(ui, p, NoteKind::Error, t.locate_none_misaligned, None);
                }
                LocatePath::None(_) => {
                    ui.add_space(6.0);
                    note_box(ui, p, NoteKind::Error, t.locate_none_nothing, None);
                }
                _ => {}
            }
            ui.add_space(6.0);
        }
        let mut on = self.cfg.locate.on;
        if switch_row(ui, p, t.locate_on, None, &mut on) {
            self.apply(|c| c.locate.on = on);
            self.stop_recording();
        }
        if !self.cfg.locate.on {
            return;
        }
        divider(ui, p);
        let choices = [
            (LocateStyle::Ring, t.locate_ring),
            (LocateStyle::Radar, t.locate_radar),
            (LocateStyle::Spotlight, t.locate_spotlight),
            (LocateStyle::Random, t.locate_random),
        ];
        let mut style = self.cfg.locate.style;
        let width = (ui.available_width() * 0.8).min(340.0);
        if labeled_row(ui, p, t.locate_style, width, ROW_SMALL, |ui| combo(ui, "locate-style", &mut style, &choices)) {
            self.apply(|c| c.locate.style = style);
        }
        if self.effect_color_row(ui, t, p, EffectColor::Locate, true) {
            crate::overlay::locate(&self.cfg.locate);
        }
        let mut size = self.cfg.locate.size;
        if size_row(ui, p, t, "locate-size", &mut size) {
            self.apply(|c| c.locate.size = size);
        }
        divider(ui, p);
        let mut on = self.cfg.locate.on_hotkey;
        if switch_row(ui, p, t.locate_use_hotkey, None, &mut on) {
            self.apply(|c| c.locate.on_hotkey = on);
            self.stop_recording();
        }
        if self.cfg.locate.on_hotkey {
            self.shortcut_row(ui, t, p, LOCATE, t.locate_key, Some(t.hotkey_help));
            if wayland {
                ui.add_space(6.0);
                self.portal_box(ui, t, p, true);
            }
        }
        divider(ui, p);
        let mut on = self.cfg.locate.on_arrival;
        if switch_row(ui, p, t.locate_arrival, None, &mut on) {
            self.apply(|c| c.locate.on_arrival = on);
        }
        divider(ui, p);
        if wayland {
            // Shaking needs the cursor all the time, which Wayland gives no app.
            ui.add_space(4.0);
            hint(ui, p, t.locate_shake_x11);
            return;
        }
        let mut on = self.cfg.locate.on_shake;
        if switch_row(ui, p, t.locate_shake, None, &mut on) {
            self.apply(|c| c.locate.on_shake = on);
        }
        if self.cfg.locate.on_shake {
            self.shake_rows(ui, t, p);
        }
    }

    /// How long the mouse must be shaken, and a key to hold meanwhile.
    fn shake_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let width = (ui.available_width() * 0.66).min(260.0);
        let labels: Vec<String> = SHAKE_TIMES.iter().map(|&ms| format!("{}{}", ms as f64 / 1000.0, t.seconds_short)).collect();
        let choices: Vec<(u64, &str)> = SHAKE_TIMES.iter().copied().zip(labels.iter().map(String::as_str)).collect();
        let mut time = self.cfg.locate.shake_time_ms;
        if labeled_row(ui, p, t.locate_shake_time, width, ROW_SMALL, |ui| segmented(ui, p, "shake-time", &mut time, &choices, SEG_SMALL)) {
            self.apply(|c| c.locate.shake_time_ms = time);
        }
        let names = [t.hotkey_none, "Ctrl", "Alt", "Shift"];
        let keys: Vec<(ShakeKey, &str)> = SHAKE_KEYS.into_iter().zip(names).collect();
        let mut key = self.cfg.locate.shake_key;
        if labeled_row(ui, p, t.locate_shake_key, width, ROW_SMALL, |ui| segmented(ui, p, "shake-key", &mut key, &keys, SEG_SMALL)) {
            self.apply(|c| c.locate.shake_key = key);
        }
    }

    /// The language, and starting at login.
    fn preferences_card(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        section(ui, p, t.general_basic, |ui| {
            let mut language = self.cfg.language.clone();
            let choices = [("ko", "한국어"), ("en", "English")];
            let mut chosen = language.as_str();
            if labeled_row(ui, p, t.language, 210.0, ROW_SMALL, |ui| segmented(ui, p, "language", &mut chosen, &choices, SEG_SMALL)) {
                language = chosen.to_string();
                self.apply(|c| c.language = language);
            }
            if let Some(al) = self.autostart.as_ref() {
                divider(ui, p);
                let mut on = self.autostart_on;
                if switch_row(ui, p, t.autostart, None, &mut on) {
                    match if on { al.enable() } else { al.disable() } {
                        Ok(()) => self.autostart_on = on,
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
            }
        });
    }

    /// Putting the settings back, and uninstalling.
    fn maintenance_card(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        section(ui, p, t.section_reset, |ui| {
            let labels: &[(&str, ButtonKind)] = if self.confirm_reset {
                &[(t.reset_confirm, ButtonKind::DangerFilled), (t.cancel, ButtonKind::Plain)]
            } else {
                &[(t.reset_button, ButtonKind::Plain)]
            };
            match (self.confirm_reset, confirm_row(ui, p, t.reset_explain, labels)) {
                (false, Some(_)) => self.confirm_reset = true,
                (true, Some(0)) => {
                    self.confirm_reset = false;
                    self.stop_recording();
                    self.apply(|c| *c = Config { language: c.language.clone(), setup: c.setup.clone(), ..Config::default() });
                }
                (true, Some(_)) => self.confirm_reset = false,
                _ => {}
            }
            divider(ui, p);
            self.uninstall_row(ui, t, p);
        });
    }

    fn uninstall_row(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let labels: &[(&str, ButtonKind)] = match self.uninstall {
            Uninstall::Confirming => &[(t.uninstall_confirm, ButtonKind::DangerFilled), (t.cancel, ButtonKind::Plain)],
            _ => &[(t.uninstall_button, ButtonKind::Danger)],
        };
        let clicked = confirm_row(ui, p, t.uninstall_explain, labels);
        match (&self.uninstall, clicked) {
            (Uninstall::Idle, Some(_)) => self.uninstall = Uninstall::Confirming,
            (Uninstall::Confirming, Some(0)) => self.start_uninstall(),
            (Uninstall::Confirming, Some(_)) => self.uninstall = Uninstall::Idle,
            _ => {}
        }
    }

    /// Current version, the new one if any with "Update now" (SelfReplace / Homebrew /
    /// DownloadOnly), and the automatic checks.
    fn update_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let state = self.update.lock().unwrap().clone();
        let ctx = ui.ctx().clone();
        let current = env!("CARGO_PKG_VERSION");
        let kind = update::install_kind();
        // A Homebrew copy: brew updates it once approved (the setup check's item).
        let brew_item = self.setup_item(ItemId::BrewUpdate).filter(Item::actionable);
        let brew_running = self.setup.runs.lock().unwrap().get(&ItemId::BrewUpdate) == Some(&SetupRun::Running);
        let brew_waiting = matches!(kind, update::InstallKind::Homebrew) && brew_item.is_none() && !brew_running;
        section(ui, p, t.section_update, |ui| {
            let busy = brew_running
                || matches!(
                    state,
                    UpdateUi::Checking | UpdateUi::Downloading | UpdateUi::Installing | UpdateUi::Restarting | UpdateUi::Installed
                );
            let version_line = format!("{} {current}", t.update_current);
            let new_help = match kind {
                update::InstallKind::SelfReplace(_) => crate::i18n::fill(t.update_new_help, &[current]),
                update::InstallKind::Homebrew => crate::i18n::fill(t.update_new_help_brew, &[current]),
                update::InstallKind::DownloadOnly => crate::i18n::fill(t.update_new_help_download, &[current]),
            };
            let (title, help) = match &state {
                UpdateUi::Available(_) | UpdateUi::InstallFailed(..) if brew_waiting => (version_line, t.update_brew_preparing.to_string()),
                UpdateUi::Available(_) if brew_running => (version_line, t.update_installing.to_string()),
                UpdateUi::Available(r) | UpdateUi::InstallFailed(r, _) => (crate::i18n::fill(t.update_new, &[&r.version]), new_help),
                UpdateUi::Idle => (version_line, String::new()),
                UpdateUi::Checking => (version_line, t.update_checking.to_string()),
                UpdateUi::UpToDate => (version_line, t.update_up_to_date.to_string()),
                UpdateUi::Failed(_) => (version_line, t.update_failed.to_string()),
                UpdateUi::Downloading => (version_line, t.update_downloading.to_string()),
                UpdateUi::Installing => (version_line, t.update_installing.to_string()),
                UpdateUi::Restarting => (version_line, t.update_restarting.to_string()),
                UpdateUi::Installed => (version_line, t.update_installed.to_string()),
            };
            // The first button is the rightmost. Homebrew waits for its setup action.
            let labels: Vec<(&str, ButtonKind)> = match &state {
                UpdateUi::Available(_) | UpdateUi::InstallFailed(..) if brew_waiting => vec![(t.update_page, ButtonKind::Plain)],
                UpdateUi::Available(_) | UpdateUi::InstallFailed(..) if !brew_running => {
                    vec![(t.update_now, ButtonKind::Primary), (t.update_page, ButtonKind::Plain)]
                }
                _ if busy => vec![],
                _ => vec![(t.update_page, ButtonKind::Plain), (t.update_check, ButtonKind::Plain)],
            };
            let width = labels.iter().map(|(l, k)| button_width(ui, l, *k)).sum::<f32>() + 8.0 * labels.len().saturating_sub(1) as f32;
            let mut clicked = None;
            labeled_row_help(ui, p, &title, Some(&help).filter(|h| !h.is_empty()).map(String::as_str), width.max(10.0), |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    for (i, (label, kind)) in labels.iter().enumerate() {
                        if button(ui, p, label, *kind).clicked() {
                            clicked = Some(i);
                        }
                    }
                });
                false
            });
            match (&state, clicked) {
                (UpdateUi::Available(r) | UpdateUi::InstallFailed(r, _), Some(0)) if !brew_running && !brew_waiting => match (matches!(kind, update::InstallKind::Homebrew), brew_item.clone()) {
                    (true, Some(item)) => self.setup.confirm = Some(vec![item]),
                    (false, _) => self.install_update(&ctx, r.clone()),
                    (true, None) => {},
                },
                (UpdateUi::Available(r) | UpdateUi::InstallFailed(r, _), Some(_)) => update::open_url(&r.page),
                (_, Some(0)) => update::open_url(update::RELEASES_PAGE),
                (_, Some(_)) => self.check_update(&ctx),
                _ => {}
            }
            match &state {
                UpdateUi::Failed(why) => hint(ui, p, why),
                UpdateUi::InstallFailed(_, why) => hint(ui, p, &format!("{}: {why}", t.update_install_failed)),
                _ => {}
            }
            divider(ui, p);
            let mut on = self.cfg.check_updates;
            let (flipped, label) = switch_row_full(ui, p, t.update_auto, None, &mut on);
            info_icon(ui, p, Pos2::new(label.right() + 16.0, label.center().y), "update-auto-help", t.update_auto_hint);
            if flipped {
                self.apply(|c| c.check_updates = on);
            }
            divider(ui, p);
            let mut on = self.cfg.auto_update;
            let note = match kind {
                update::InstallKind::SelfReplace(_) => None,
                update::InstallKind::Homebrew => Some(t.update_brew),
                update::InstallKind::DownloadOnly => Some(t.update_download_only),
            };
            let (flipped, label) = switch_row_full(ui, p, t.update_install_auto, note, &mut on);
            if matches!(kind, update::InstallKind::SelfReplace(_)) {
                info_icon(ui, p, Pos2::new(label.right() + 16.0, label.center().y), "auto-update-help", t.update_install_hint);
            }
            if flipped {
                self.apply(|c| c.auto_update = on);
            }
            ui.add_space(4.0);
        });
    }

    /// The developer's address, with a ready-made mail and a GitHub link.
    fn report_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let copied = self.copied_at.is_some_and(|at| at.elapsed() < Duration::from_secs(2));
        section_with(ui, p, t.section_report, Some(t.report_desc), None, |ui| {
            ui.add_space(10.0);
            ui.label(RichText::new(update::REPORT_EMAIL).font(bold(14.0)).color(p.text));
            ui.add_space(8.0);
            let copy = if copied { t.report_copied } else { t.report_copy };
            let buttons = [(t.report_mail, ButtonKind::Primary), (copy, ButtonKind::Plain), (t.report_issue, ButtonKind::Plain)];
            match button_row_kinds(ui, p, &buttons) {
                Some(0) => {
                    let body = update::report_body(&self.cfg, &self.devices, self.status.trigger.as_deref());
                    update::open_url(&update::report_mailto(t.report_subject, &body));
                }
                Some(1) => {
                    ui.ctx().copy_text(update::REPORT_EMAIL.to_string());
                    self.copied_at = Some(Instant::now());
                    ui.ctx().request_repaint_after(Duration::from_secs(2));
                }
                Some(2) => update::open_url(update::ISSUES_PAGE),
                _ => {}
            }
            ui.add_space(12.0);
        });
    }

    /// Saving the settings to a file, loading one, managing the saved ones, and the
    /// folder they are in. How many are saved shows at the right of the title.
    fn backup_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let count = format!("{}/{}", self.backup_count, backup::MAX_FILES);
        let full = self.backup_count >= backup::MAX_FILES;
        let count_color = if full { p.amber } else { p.muted };
        section_with(ui, p, t.section_backup, Some(t.backup_info), Some((&count, count_color)), |ui| {
            ui.add_space(10.0);
            let labels = [t.backup_save, t.backup_load, t.backup_manage, t.backup_open];
            match button_row(ui, p, &labels) {
                Some(0) => {
                    let current = Config::load_or_create().unwrap_or_else(|_| self.cfg.clone());
                    self.backup_full = false;
                    self.backup_note = Some(match backup::export(&current) {
                        Ok(backup::Export::Saved(path)) => (true, format!("{} {}", t.backup_saved, file_name(&path))),
                        Ok(backup::Export::Full) => {
                            self.backup_full = true;
                            (false, t.backup_full.to_string())
                        }
                        Err(e) => (false, format!("{} {e:#}", t.backup_save_failed)),
                    });
                    self.backup_count = backup::list(&backup::folder()).len();
                }
                Some(1) => {
                    self.stop_recording();
                    self.backup_note = None;
                    self.backup_picker = Some(BackupPicker { files: backup::list(&backup::folder()), chosen: None, error: None });
                }
                Some(2) => {
                    self.stop_recording();
                    self.backup_manager = Some(BackupManager {
                        files: backup::list(&backup::folder()),
                        offset: backup::local_offset(),
                        renaming: None,
                        deleting: None,
                        error: None,
                        note: None,
                    });
                }
                Some(3) => {
                    let dir = backup::folder();
                    match std::fs::create_dir_all(&dir) {
                        Ok(()) => update::open_url(&dir.display().to_string()),
                        Err(e) => self.backup_note = Some((false, format!("{}: {e}", dir.display()))),
                    }
                }
                _ => {}
            }
            if self.backup_full {
                ui.add_space(4.0);
                if confirm_row_in(ui, p, t.backup_full, p.red, &[(t.backup_manage, ButtonKind::Plain)]).is_some() {
                    self.stop_recording();
                    self.backup_manager = Some(BackupManager {
                        files: backup::list(&backup::folder()),
                        offset: backup::local_offset(),
                        renaming: None,
                        deleting: None,
                        error: None,
                        note: None,
                    });
                }
            } else if let Some((ok, text)) = &self.backup_note {
                ui.add_space(6.0);
                ui.label(RichText::new(text).size(12.5).color(if *ok { p.green } else { p.red }));
                ui.add_space(6.0);
            } else {
                ui.add_space(12.0);
            }
        });
    }

    /// "Manage…": the saved files, newest first, each with Rename and Delete.
    fn backup_manager_ui(&mut self, ctx: &egui::Context, t: &Texts, p: Palette) {
        let Some(manager) = self.backup_manager.as_mut() else { return };
        let mut action = None;
        let mut close = false;
        let frame = egui::Frame::new()
            .fill(p.bg)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(16)
            .inner_margin(Margin::same(18))
            .shadow(ctx.global_style().visuals.window_shadow);
        let shown = egui::Modal::new(egui::Id::unique("backup-manager")).frame(frame).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.horizontal(|ui| {
                ui.label(RichText::new(t.backup_manage_title).font(bold(16.0)).color(p.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let full = manager.files.len() >= backup::MAX_FILES;
                    let count = format!("{}/{}", manager.files.len(), backup::MAX_FILES);
                    ui.label(RichText::new(count).size(13.0).color(if full { p.amber } else { p.muted }));
                });
            });
            ui.add_space(2.0);
            hint(ui, p, &home_relative(&backup::folder()));
            ui.add_space(10.0);
            if manager.files.is_empty() {
                empty_card(ui, p, t.backup_none);
            } else {
                card(ui, p, |ui| {
                    egui::ScrollArea::vertical().max_height(330.0).auto_shrink([false, true]).show(ui, |ui| {
                        let files = manager.files.clone();
                        for (i, file) in files.iter().enumerate() {
                            if i > 0 {
                                divider(ui, p);
                            }
                            let when = backup::format_time(file.modified, manager.offset);
                            if let Some(a) = manage_row(ui, p, t, file, &when, manager) {
                                action = Some(a);
                            }
                        }
                    });
                });
            }
            if let Some((fine, text)) = &manager.note {
                ui.add_space(8.0);
                ui.label(RichText::new(text).size(12.5).color(if *fine { p.green } else { p.red }));
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                if button(ui, p, t.backup_open, ButtonKind::Plain).clicked() {
                    let dir = backup::folder();
                    let _ = std::fs::create_dir_all(&dir);
                    update::open_url(&dir.display().to_string());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    close = button(ui, p, t.close, ButtonKind::Primary).clicked();
                });
            });
        });
        if shown.should_close() {
            if manager.renaming.is_some() {
                manager.renaming = None;
                manager.error = None;
                return;
            }
            close = true;
        }
        if close {
            self.backup_manager = None;
            return;
        }
        let Some(action) = action else { return };
        match action {
            ManageAction::StartRename(path, name) => {
                manager.renaming = Some((path, name));
                manager.deleting = None;
                manager.error = None;
            }
            ManageAction::StartDelete(path) => {
                manager.deleting = Some(path);
                manager.renaming = None;
                manager.error = None;
            }
            ManageAction::Cancel => {
                manager.renaming = None;
                manager.deleting = None;
                manager.error = None;
            }
            ManageAction::Rename(path, typed) => match backup::rename(&path, &typed) {
                Ok(_) => {
                    manager.renaming = None;
                    manager.error = None;
                    manager.files = backup::list(&backup::folder());
                }
                Err(e) => {
                    let why = match e {
                        backup::NameError::Empty => t.backup_name_empty.to_string(),
                        backup::NameError::TooLong => t.backup_name_long.to_string(),
                        backup::NameError::BadChar(c) => format!("{} {c}", t.backup_name_bad),
                        backup::NameError::Taken => t.backup_name_taken.to_string(),
                        backup::NameError::Failed(why) => format!("{} {why}", t.backup_rename_failed),
                    };
                    manager.error = Some((path, why));
                }
            },
            ManageAction::Delete(path) => {
                let name = path.file_stem().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                manager.deleting = None;
                match backup::delete(&path) {
                    Ok(backup::Deleted::Trashed) => manager.note = Some((true, format!("{} {name}", t.backup_trashed))),
                    Ok(backup::Deleted::Removed) => manager.note = Some((true, format!("{} {name}", t.backup_removed))),
                    Err(e) => manager.error = Some((path, format!("{} {e:#}", t.backup_delete_failed))),
                }
                manager.files = backup::list(&backup::folder());
            }
        }
        self.backup_count = manager.files.len();
        if self.backup_count < backup::MAX_FILES {
            self.backup_full = false;
            if self.backup_note.as_ref().is_some_and(|(_, n)| n == t.backup_full) {
                self.backup_note = None;
            }
        }
    }

    /// "Load settings…": the saved files, newest first: one is chosen, then loaded once confirmed.
    fn backup_picker_ui(&mut self, ctx: &egui::Context, t: &Texts, p: Palette) {
        let Some(picker) = self.backup_picker.as_mut() else { return };
        let (mut cancel, mut load) = (false, false);
        let frame = egui::Frame::new()
            .fill(p.bg)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(16)
            .inner_margin(Margin::same(18))
            .shadow(ctx.global_style().visuals.window_shadow);
        let shown = egui::Modal::new(egui::Id::unique("backup-picker")).frame(frame).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.label(RichText::new(t.backup_pick_title).font(bold(16.0)).color(p.text));
            ui.add_space(2.0);
            hint(ui, p, &home_relative(&backup::folder()));
            ui.add_space(10.0);
            if picker.files.is_empty() {
                empty_card(ui, p, t.backup_none);
            } else {
                card(ui, p, |ui| {
                    egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                        for (i, file) in picker.files.iter().enumerate() {
                            if i > 0 {
                                divider(ui, p);
                            }
                            let row = Row { title: &file.name, subtitle: None, badge: None };
                            if choice_row(ui, p, picker.chosen == Some(i), &row).clicked() {
                                picker.chosen = Some(i);
                                picker.error = None;
                            }
                        }
                    });
                });
                if picker.chosen.is_some() {
                    ui.add_space(8.0);
                    hint(ui, p, t.backup_confirm);
                }
            }
            if let Some(err) = &picker.error {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("{} {err}", t.backup_bad)).size(12.5).color(p.red));
            }
            ui.add_space(14.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                if button(ui, p, t.backup_open, ButtonKind::Plain).clicked() {
                    let dir = backup::folder();
                    let _ = std::fs::create_dir_all(&dir);
                    update::open_url(&dir.display().to_string());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if picker.chosen.is_some() && button(ui, p, t.backup_apply, ButtonKind::Primary).clicked() {
                        load = true;
                    }
                    cancel = button(ui, p, t.cancel, ButtonKind::Plain).clicked();
                });
            });
        });
        if cancel || shown.should_close() {
            self.backup_picker = None;
            return;
        }
        let Some(file) = load.then(|| picker.chosen.and_then(|i| picker.files.get(i)).cloned()).flatten() else { return };
        match backup::read(&file.path) {
            Ok(loaded) => {
                log::info!("settings loaded from {}", file.path.display());
                // What the setup check set aside and the pairing state belong to this computer.
                self.apply(|c| *c = backup::restore(c, loaded));
                match self.error.take() {
                    None => {
                        self.backup_picker = None;
                        self.backup_note = Some((true, t.backup_loaded.to_string()));
                    }
                    Some(e) => self.set_picker_error(e),
                }
            }
            Err(e) => {
                log::info!("settings not loaded from {}: {e:#}", file.path.display());
                self.set_picker_error(format!("{e:#}"));
            }
        }
    }

    fn set_picker_error(&mut self, error: String) {
        if let Some(picker) = self.backup_picker.as_mut() {
            picker.error = Some(error);
        }
    }

    /// Asks the running tray app to uninstall (it owns the button), or does it here
    /// when no tray app is running.
    fn start_uninstall(&mut self) {
        if crate::main_instance_running() {
            match std::fs::write(shared::uninstall_request_path(), b"") {
                Ok(()) => self.uninstall = Uninstall::Waiting(Instant::now()),
                Err(e) => {
                    self.error = Some(e.to_string());
                    self.uninstall = Uninstall::Idle;
                }
            }
        } else {
            crate::uninstall();
            self.uninstall = Uninstall::Done;
        }
    }

    /// Advances the uninstall state; returns true once it has started.
    fn uninstall_progress(&mut self) -> bool {
        if let Uninstall::Waiting(since) = self.uninstall {
            if !crate::main_instance_running() {
                self.uninstall = Uninstall::Done;
            } else if since.elapsed() > Duration::from_secs(15) {
                self.error = Some("flockswitch did not respond; quit it from the menu and try again".into());
                let _ = std::fs::remove_file(shared::uninstall_request_path());
                self.uninstall = Uninstall::Idle;
            }
        }
        matches!(self.uninstall, Uninstall::Waiting(_) | Uninstall::Done)
    }

    fn uninstall_result(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        ui.label(RichText::new(crate::APP_NAME).font(bold(19.0)).color(p.text));
        ui.add_space(16.0);
        card(ui, p, |ui| {
            ui.add_space(12.0);
            if self.uninstall == Uninstall::Done {
                ui.label(t.uninstall_done);
                ui.add_space(8.0);
                if button(ui, p, t.close, ButtonKind::Plain).clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            } else {
                ui.label(t.uninstall_waiting);
            }
            ui.add_space(12.0);
        });
    }
}

/// One file in the manager: its name and date with Rename and Delete; while renaming, a
/// text field with OK and Cancel; while deleting, the confirmation. Returns what was asked.
fn manage_row(ui: &mut Ui, p: Palette, t: &Texts, file: &backup::Entry, when: &str, m: &mut BackupManager) -> Option<ManageAction> {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 50.0), Sense::hover());
    let renaming = m.renaming.as_ref().is_some_and(|(path, _)| *path == file.path);
    let deleting = m.deleting.as_ref() == Some(&file.path);
    let labels: &[(&str, ButtonKind)] = if renaming {
        &[(t.ok, ButtonKind::Primary), (t.cancel, ButtonKind::Plain)]
    } else if deleting {
        &[(t.backup_delete_confirm, ButtonKind::DangerFilled), (t.cancel, ButtonKind::Plain)]
    } else {
        &[(t.backup_rename, ButtonKind::Plain), (t.backup_delete, ButtonKind::Danger)]
    };
    let buttons: f32 = labels.iter().map(|(l, k)| button_width(ui, l, *k)).sum::<f32>() + 8.0 * (labels.len() - 1) as f32;
    let text_right = rect.right() - buttons - 12.0;
    let mut clicked = None;
    match m.renaming.as_mut().filter(|(path, _)| *path == file.path) {
        Some((_, typed)) => {
            let field = Rect::from_min_max(Pos2::new(rect.left(), rect.center().y - CONTROL_HEIGHT / 2.0), Pos2::new(text_right, rect.center().y + CONTROL_HEIGHT / 2.0));
            let edit = egui::TextEdit::singleline(typed).font(FontId::proportional(14.5)).align(Align2::LEFT_CENTER);
            let resp = ui.new_child(UiBuilder::new().max_rect(field)).add_sized(field.size(), edit);
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                clicked = Some(0);
            }
            // Ready to type as soon as Rename is pressed.
            if ui.memory(|mem| mem.focused().is_none()) {
                resp.request_focus();
            }
        }
        None => {
            let clip = Rect::from_min_max(rect.min, Pos2::new(text_right, rect.max.y));
            let painter = ui.painter().with_clip_rect(clip.intersect(ui.clip_rect()));
            let name = elided(ui, file.stem(), FontId::proportional(14.5), p.text, clip.width());
            painter.galley(Pos2::new(rect.left(), rect.center().y - 1.0 - name.size().y), name, p.text);
            painter.text(Pos2::new(rect.left(), rect.center().y + 3.0), Align2::LEFT_TOP, when, FontId::proportional(12.5), p.muted);
        }
    }
    let area = Rect::from_min_max(
        Pos2::new(rect.right() - buttons, rect.center().y - CONTROL_HEIGHT / 2.0),
        Pos2::new(rect.right(), rect.center().y + CONTROL_HEIGHT / 2.0),
    );
    ui.new_child(UiBuilder::new().max_rect(area)).scope(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.horizontal(|ui| {
            for (i, (label, kind)) in labels.iter().enumerate() {
                if button(ui, p, label, *kind).clicked() {
                    clicked = Some(i);
                }
            }
        });
    });
    if let Some((path, why)) = &m.error {
        if *path == file.path {
            ui.label(RichText::new(why).size(12.5).color(p.red));
            ui.add_space(6.0);
        }
    }
    let typed = m.renaming.as_ref().filter(|(path, _)| *path == file.path).map(|(_, typed)| typed.clone());
    match (clicked?, typed) {
        (0, Some(typed)) => Some(ManageAction::Rename(file.path.clone(), typed)),
        (0, None) if deleting => Some(ManageAction::Delete(file.path.clone())),
        (0, None) => Some(ManageAction::StartRename(file.path.clone(), file.stem().to_string())),
        (_, None) if !deleting => Some(ManageAction::StartDelete(file.path.clone())),
        _ => Some(ManageAction::Cancel),
    }
}

/// "flockswitch-settings-….toml" of a path.
fn file_name(path: &std::path::Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// A path under the home folder as "~/…".
fn home_relative(path: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| path.strip_prefix(h).ok().map(std::path::Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// Puts the window (the current viewport) back to its size in points when the display
/// scale under it changed or it ended up outside `min`..`max` (see window_size.rs).
fn keep_size(ctx: &egui::Context, guard: &mut SizeGuard, min: [f32; 2], max: [f32; 2]) {
    let (scale, size) = ctx.input(|i| (i.viewport().native_pixels_per_point, i.viewport().inner_rect.map(|r| r.size())));
    let (Some(scale), Some(size)) = (scale, size) else { return };
    let now = Instant::now();
    if let Some(want) = guard.next(scale, [size.x, size.y], min, max, now) {
        log::info!("window: {:.0}×{:.0} at scale {scale}; back to {:.0}×{:.0}", size.x, size.y, want[0], want[1]);
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::from(want)));
    }
    if guard.settling(now) {
        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

/// The window title. On GNOME's Wayland session the title bar is drawn by winit with one
/// font that has no Hangul, so a Korean title would show as boxes there: "flockswitch".
fn window_title(text: &str) -> String {
    let wayland = crate::edge_wayland::session_is_wayland();
    let gnome = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase().contains("gnome");
    if wayland && gnome && !text.is_ascii() {
        "flockswitch".into()
    } else {
        text.into()
    }
}

/// The desktop's name for the Wayland notes ("ubuntu:GNOME" → "GNOME").
fn desktop_name() -> String {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    desktop.rsplit(':').next().map(str::trim).filter(|d| !d.is_empty()).unwrap_or("Wayland").to_string()
}

/// What the running flockswitch says about watching the edges in this Wayland session
/// (from the status file): the text to show, and whether it is a problem.
fn edge_backend_text(t: &Texts, backend: &str, note: &str, require_control: bool) -> (String, bool) {
    let desktop = desktop_name();
    let sub = |text: &str| text.replace("{desktop}", &desktop);
    let (code, arg) = note.split_once(':').unwrap_or((note, ""));
    match backend {
        "input-capture" => match code {
            "asking" => (sub(t.edges_capture_asking), false),
            "ready" => {
                let mut text = sub(t.edges_capture_ready);
                text.push(' ');
                text.push_str(if arg == "1" { t.edges_capture_every } else { t.edges_capture_once });
                if require_control {
                    text.push(' ');
                    text.push_str(t.edges_capture_ctrl);
                }
                (text, false)
            }
            "denied" => (t.edges_capture_denied.to_string(), true),
            "closed" => (t.edges_capture_closed.to_string(), true),
            "no-barriers" => (t.edges_capture_no_barriers.to_string(), true),
            "error" => (t.edges_capture_error.to_string(), true),
            _ => (sub(t.edges_capture_off), false),
        },
        "layer-shell" => {
            let mut text = sub(t.edges_layer_ready);
            if require_control {
                text.push(' ');
                text.push_str(t.edges_layer_ctrl);
            }
            (text, false)
        }
        "none" => {
            let text = match code {
                "gnome-old" => return (t.edges_none_gnome.replace("{version}", arg), true),
                "no-wayland" => t.edges_none_wayland.to_string(),
                "no-display" => t.edges_none_display.to_string(),
                "forced" => return (t.edges_none_forced.replace("{name}", arg), true),
                _ => sub(t.edges_none_support),
            };
            // What the running flockswitch found (GNOME version, the portal's answer).
            if arg.is_empty() {
                (text, true)
            } else {
                (format!("{text}\n{}", t.edges_none_found.replace("{found}", arg)), true)
            }
        }
        _ => (t.edges_unknown.to_string(), false),
    }
}

/// How this Wayland session can draw the cursor highlight, found once per window (the
/// settings window's own Preview draws the same way as the running app).
fn wayland_locate_path() -> LocatePath {
    static PATH: std::sync::OnceLock<LocatePath> = std::sync::OnceLock::new();
    *PATH.get_or_init(|| {
        let (path, detail) = crate::locate_wayland::path();
        log::info!("locate: {path:?} ({detail})");
        path
    })
}

// ---- Building blocks -------------------------------------------------------------

/// A round button showing one colour, ringed when it is the current one.
fn color_swatch(ui: &mut Ui, p: Palette, [r, g, b]: [u8; 3], selected: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, selected, format_color([r, g, b])));
    let painter = ui.painter();
    painter.circle_filled(rect.center(), 9.0, Color32::from_rgb(r, g, b));
    painter.circle_stroke(rect.center(), 9.0, Stroke::new(1.0, p.track));
    if selected {
        painter.circle_stroke(rect.center(), 11.0, Stroke::new(1.8, p.text));
    }
    resp
}

/// This computer's private IPv4 addresses, read from `ip`.
fn local_ips() -> Vec<String> {
    // Query local interfaces only. This never opens a network connection.
    let Ok(out) = std::process::Command::new("ip").args(["-o", "-4", "addr", "show", "scope", "global"]).output() else {
        return Vec::new();
    };
    let mut ips: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.split_once('/')?.0.parse::<std::net::Ipv4Addr>().ok())
        .filter(|ip| ip.is_private())
        .map(|ip| ip.to_string())
        .collect();
    ips.sort();
    ips.dedup();
    ips
}

/// The picture at the top of the "How does it work?" window: this computer, the
/// encrypted network, the other computer, and the way back.
fn how_diagram(ui: &mut Ui, p: Palette, t: &Texts) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 118.0), Sense::hover());
    let painter = ui.painter();
    let gap = 44.0;
    let w = (rect.width() - 2.0 * gap) / 3.0;
    let h = 58.0;
    let boxes: Vec<Rect> = (0..3)
        .map(|i| Rect::from_min_size(Pos2::new(rect.left() + i as f32 * (w + gap), rect.top() + 4.0), Vec2::new(w, h)))
        .collect();
    for (i, (r, label)) in boxes.iter().zip([t.net_how_this, t.net_how_link, t.net_how_other]).enumerate() {
        if i == 1 {
            // The network between the two computers: the dashed, hatched card.
            painter.add(Shape::Vec(ghost_shapes(*r, 12.0, p)));
        } else {
            painter.add(p.card_shadow().as_shape(*r, 12));
            painter.rect_filled(*r, 12, p.card);
            painter.rect_stroke(*r, 12, Stroke::new(1.0, p.border), StrokeKind::Inside);
        }
        let (first, rest) = label.split_once(" / ").unwrap_or((label, ""));
        let text = if rest.is_empty() { first.to_string() } else { format!("{first}\n{rest}") };
        let galley = painter.layout(text, FontId::proportional(12.5), p.text, w - 12.0);
        painter.galley(r.center() - galley.size() / 2.0, galley, p.text);
    }
    let arrow = Stroke::new(1.6, p.text);
    let head = |tip: Pos2, dir: Vec2, stroke: Stroke| {
        let side = Vec2::new(-dir.y, dir.x);
        painter.line_segment([tip, tip - dir * 7.0 + side * 4.0], stroke);
        painter.line_segment([tip, tip - dir * 7.0 - side * 4.0], stroke);
    };
    for pair in boxes.windows(2) {
        let (from, to) = (Pos2::new(pair[0].right() + 4.0, pair[0].center().y), Pos2::new(pair[1].left() - 4.0, pair[0].center().y));
        painter.line_segment([from, to], arrow);
        head(to, Vec2::new(1.0, 0.0), arrow);
    }
    // The way back runs under the boxes, from the last to the first.
    let (start, end) = (Pos2::new(boxes[2].center().x, boxes[2].bottom() + 4.0), Pos2::new(boxes[0].center().x, boxes[0].bottom() + 4.0));
    let low = rect.top() + h + 30.0;
    let back = Stroke::new(2.2, p.text);
    painter.line_segment([start, Pos2::new(start.x, low)], back);
    painter.line_segment([Pos2::new(start.x, low), Pos2::new(end.x, low)], back);
    painter.line_segment([Pos2::new(end.x, low), end], back);
    head(end, Vec2::new(0.0, -1.0), back);
    let galley = painter.layout_no_wrap(t.net_how_back.to_string(), FontId::proportional(12.0), p.muted);
    let label = Rect::from_center_size(Pos2::new(rect.center().x, low), galley.size() + Vec2::new(12.0, 2.0));
    painter.rect_filled(label, 4, p.bg);
    painter.galley(label.center() - galley.size() / 2.0, galley, p.muted);
}

/// The look of "the other computer", like the back card of the app icon: a dashed outline
/// and light diagonal hatching inside a faint card. Drawn behind what is on it.
fn ghost_shapes(rect: Rect, radius: f32, p: Palette) -> Vec<Shape> {
    let mut shapes = vec![Shape::rect_filled(rect, radius, p.card.gamma_multiply(0.45))];
    // Hatching: lines rising to the right, cut where they leave the rounded rectangle.
    let inside = |pos: Pos2| {
        let r = rect.shrink(1.0);
        if !r.contains(pos) {
            return false;
        }
        let corner = Pos2::new(
            pos.x.clamp(r.left() + radius, r.right() - radius),
            pos.y.clamp(r.top() + radius, r.bottom() - radius),
        );
        (pos - corner).length() <= radius - 1.0
    };
    let (w, h) = (rect.width(), rect.height());
    let mut k = -h;
    while k < w {
        // From the bottom edge up and to the right, in 3 px steps.
        let at = |t: f32| Pos2::new(rect.left() + k + t, rect.bottom() - t);
        let (mut run_start, mut t) = (None, 0.0_f32);
        while t <= h + 3.0 {
            let ok = inside(at(t));
            match (ok, run_start) {
                (true, None) => run_start = Some(t),
                (false, Some(a)) => {
                    shapes.push(Shape::line_segment([at(a), at(t - 3.0)], Stroke::new(1.0, p.hatch)));
                    run_start = None;
                }
                _ => {}
            }
            t += 3.0;
        }
        if let Some(a) = run_start {
            shapes.push(Shape::line_segment([at(a), at(t - 3.0)], Stroke::new(1.0, p.hatch)));
        }
        k += 9.0;
    }
    // The dashed outline: 6 on, 5 off.
    let mut path = Vec::new();
    let r = rect.shrink(0.6);
    for (center, from) in [
        (Pos2::new(r.right() - radius, r.top() + radius), -90.0_f32),
        (Pos2::new(r.right() - radius, r.bottom() - radius), 0.0),
        (Pos2::new(r.left() + radius, r.bottom() - radius), 90.0),
        (Pos2::new(r.left() + radius, r.top() + radius), 180.0),
    ] {
        for step in 0..=6 {
            let a = (from + 15.0 * step as f32).to_radians();
            path.push(center + Vec2::new(a.cos(), a.sin()) * radius);
        }
    }
    path.push(path[0]);
    shapes.extend(Shape::dashed_line(&path, Stroke::new(1.2, p.dash), 6.0, 5.0));
    shapes
}

/// Space between the blocks of a page.
const BLOCK_GAP: f32 = 16.0;

fn gap(ui: &mut Ui) {
    ui.add_space(BLOCK_GAP);
}

/// The title of a page and, under it, one line saying what it is for; a button may sit
/// at the right end (true when pressed).
fn page_header(ui: &mut Ui, p: Palette, title: &str, help: &str, button_label: Option<&str>) -> bool {
    let width = ui.available_width();
    let button_w = button_label.map_or(0.0, |l| button_width(ui, l, ButtonKind::Plain));
    let text_w = (width - button_w - 16.0).max(120.0);
    let help = ui.painter().layout(help.to_string(), FontId::proportional(13.0), p.muted, text_w);
    let height = (30.0 + help.size().y + 4.0).max(52.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let painter = ui.painter();
    painter.text(rect.left_top() + Vec2::new(0.0, 2.0), Align2::LEFT_TOP, title, bold(20.0), p.text);
    painter.galley(rect.left_top() + Vec2::new(0.0, 30.0), help, p.muted);
    let mut pressed = false;
    if let Some(label) = button_label {
        let area = Rect::from_min_size(Pos2::new(rect.right() - button_w, rect.bottom() - CONTROL_HEIGHT - 2.0), Vec2::new(button_w, CONTROL_HEIGHT));
        ui.new_child(UiBuilder::new().max_rect(area)).scope(|ui| pressed = button(ui, p, label, ButtonKind::Plain).clicked());
    }
    pressed
}

/// The small muted title above a box. Every one is `SECTION_HEAD` tall, whatever sits
/// at its right end (`right` is laid out right to left), so boxes side by side start on
/// one line.
fn section_head(ui: &mut Ui, p: Palette, title: &str, info: Option<&str>, right: impl FnOnce(&mut Ui)) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), SECTION_HEAD), Sense::hover());
    let mut inner = ui.new_child(
        UiBuilder::new().max_rect(rect.shrink2(Vec2::new(4.0, 0.0))).layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    let label = inner.label(RichText::new(title).font(bold(13.0)).color(p.muted));
    if let Some(text) = info {
        let (r, _) = inner.allocate_exact_size(Vec2::new(20.0, label.rect.height()), Sense::hover());
        info_icon(&mut inner, p, r.center(), title, text);
    }
    inner.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
    ui.add_space(6.0);
}

/// A short note in `color` at the right end of a section header.
fn head_note(ui: &mut Ui, text: &str, color: Color32) {
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(color)).truncate());
}

fn section(ui: &mut Ui, p: Palette, title: &str, add: impl FnOnce(&mut Ui)) {
    section_with(ui, p, title, None, None, add);
}

/// A header and a card under it. `info` adds an "i" after the title explaining it, and
/// `note` a short text in its colour at the right end.
fn section_with(ui: &mut Ui, p: Palette, title: &str, info: Option<&str>, note: Option<(&str, Color32)>, add: impl FnOnce(&mut Ui)) {
    section_head(ui, p, title, info, |ui| {
        if let Some((text, color)) = note {
            head_note(ui, text, color);
        }
    });
    card(ui, p, add);
}

/// What a column of `two_cols` asks its card to do: grow to `target`, and report its own
/// height in `slot`.
#[derive(Clone, Copy)]
struct Fill {
    target: f32,
    top: f32,
    slot: egui::Id,
}

impl Default for Fill {
    fn default() -> Self {
        Fill { target: 0.0, top: 0.0, slot: egui::Id::NULL }
    }
}

/// Two columns side by side, each holding a section (`section_head` and one card). The
/// cards end on the same line: each frame the taller one's height is remembered and the
/// other card grows to it. `add` gets the left and the right column.
fn two_cols(ui: &mut Ui, name: &str, add: impl FnOnce(&mut Ui, &mut Ui)) {
    let id = ui.scope_id().with(("two-cols", name));
    let (hl, hr) = ui.data(|d| d.get_temp::<(f32, f32)>(id)).unwrap_or((0.0, 0.0));
    let target = hl.max(hr);
    let gutter = 16.0;
    let width = ui.available_width();
    let col_w = ((width - gutter) / 2.0).floor();
    let top = ui.cursor().min;
    let rect = |i: f32| Rect::from_min_size(top + Vec2::new(i * (col_w + gutter), 0.0), Vec2::new(col_w, 4000.0));
    let mut l = ui.new_child(UiBuilder::new().id_salt(("col", 0)).max_rect(rect(0.0)));
    let mut r = ui.new_child(UiBuilder::new().id_salt(("col", 1)).max_rect(rect(1.0)));
    for (i, col) in [&mut l, &mut r].into_iter().enumerate() {
        let fill = Fill { target, top: top.y, slot: id.with(i) };
        let key = col.scope_id();
        col.data_mut(|d| d.insert_temp(key, fill));
    }
    add(&mut l, &mut r);
    let natural = |col: &Ui, i: usize| col.data(|d| d.get_temp::<f32>(id.with(i))).unwrap_or(col.min_rect().height());
    let heights = (natural(&l, 0), natural(&r, 1));
    if (heights.0 - hl).abs() > 0.5 || (heights.1 - hr).abs() > 0.5 {
        ui.data_mut(|d| d.insert_temp(id, heights));
        ui.ctx().request_repaint();
    }
    let bottom = l.min_rect().bottom().max(r.min_rect().bottom());
    ui.allocate_rect(Rect::from_min_max(top, Pos2::new(top.x + width, bottom)), Sense::hover());
}

fn card(ui: &mut Ui, p: Palette, add: impl FnOnce(&mut Ui)) {
    card_with(ui, p, Margin::symmetric(18, 6), add);
}

/// A card with room around its content (a map, a form), not rows.
fn card_pad(ui: &mut Ui, p: Palette, add: impl FnOnce(&mut Ui)) {
    card_with(ui, p, Margin::symmetric(18, 16), add);
}

fn card_with(ui: &mut Ui, p: Palette, margin: Margin, add: impl FnOnce(&mut Ui)) {
    // In a `two_cols` column: grow to the line the taller card ends on.
    let key = ui.scope_id();
    let fill = ui.data(|d| d.get_temp::<Fill>(key));
    if fill.is_some() {
        ui.data_mut(|d| d.remove_temp::<Fill>(key));
    }
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .shadow(p.card_shadow())
        .corner_radius(16)
        .inner_margin(margin)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui);
            if let Some(fill) = fill {
                let natural = ui.cursor().top() + f32::from(margin.bottom) - fill.top;
                ui.data_mut(|d| d.insert_temp(fill.slot, natural));
                ui.add_space((fill.target - natural).max(0.0));
            }
        });
}

#[derive(Clone, Copy, PartialEq)]
enum NoteKind {
    Info,
    Warn,
    Error,
}

/// A rounded note with a mark at the left and, if asked, a button at the right (true
/// when pressed).
fn note_box(ui: &mut Ui, p: Palette, kind: NoteKind, text: &str, button_label: Option<&str>) -> bool {
    let (fill, color) = match kind {
        NoteKind::Info => (p.track.gamma_multiply(if p.dark { 0.7 } else { 0.8 }), p.text.gamma_multiply(0.85)),
        NoteKind::Warn => (p.amber.gamma_multiply(0.15), if p.dark { p.amber } else { p.amber.lerp_to_gamma(Color32::BLACK, 0.4) }),
        NoteKind::Error => (p.red.gamma_multiply(0.12), p.red),
    };
    let width = ui.available_width();
    let button_w = button_label.map_or(0.0, |l| button_width(ui, l, ButtonKind::Plain) + 12.0);
    let text_w = (width - 32.0 - 28.0 - button_w).max(100.0);
    let galley = ui.painter().layout(text.to_string(), FontId::proportional(13.0), color, text_w);
    let height = (galley.size().y + 24.0).max(46.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 12, fill);
    let c = Pos2::new(rect.left() + 24.0, rect.center().y);
    painter.circle_stroke(c, 7.5, Stroke::new(1.4, color));
    painter.circle_filled(c + Vec2::new(0.0, -3.0), 1.1, color);
    painter.line_segment([c + Vec2::new(0.0, -0.8), c + Vec2::new(0.0, 3.6)], Stroke::new(1.5, color));
    painter.galley(Pos2::new(rect.left() + 44.0, rect.center().y - galley.size().y / 2.0), galley, color);
    let mut pressed = false;
    if let Some(label) = button_label {
        let w = button_w - 12.0;
        let area = Rect::from_center_size(Pos2::new(rect.right() - 16.0 - w / 2.0, rect.center().y), Vec2::new(w, CONTROL_HEIGHT));
        ui.new_child(UiBuilder::new().max_rect(area)).scope(|ui| pressed = button(ui, p, label, ButtonKind::Plain).clicked());
    }
    pressed
}

/// A key as a small keycap.
fn keycap(ui: &mut Ui, p: Palette, text: &str) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), FontId::monospace(12.0), p.text);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(galley.size().x + 14.0, 22.0), Sense::hover());
    ui.painter().rect_filled(rect, 5, p.card);
    ui.painter().rect_stroke(rect, 5, Stroke::new(1.0, p.track), StrokeKind::Inside);
    ui.painter().galley(rect.center() - galley.size() / 2.0, galley, p.text);
}

/// A small switch (a paired computer's "send from this computer"). True when flipped.
fn mini_switch(ui: &mut Ui, p: Palette, label: &str, on: &mut bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(32.0, 20.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| WidgetInfo::selected(Role::Switch, true, *on, label));
    if resp.clicked() {
        *on = !*on;
    }
    let how_on = ui.ctx().animate_bool_responsive(resp.id, *on);
    let painter = ui.painter();
    painter.rect_filled(rect, 10, p.off.lerp_to_gamma(p.strong, how_on));
    let x = egui::lerp(rect.left() + 10.0..=rect.right() - 10.0, how_on);
    painter.circle_filled(Pos2::new(x, rect.center().y), 7.5, Color32::WHITE.lerp_to_gamma(p.on_strong, how_on));
    resp.clicked()
}

/// A drop-down `width` wide. Returns true on change.
fn combo_w<T: PartialEq + Copy>(ui: &mut Ui, name: &str, value: &mut T, options: &[(T, &str)], width: f32) -> bool {
    let shown = options.iter().find(|(v, _)| v == value).map_or("", |(_, l)| *l);
    let mut changed = false;
    egui::ComboBox::from_id_salt(name).selected_text(shown).width(width).show_ui(ui, |ui| {
        for (v, label) in options {
            changed |= ui.selectable_value(value, *v, *label).changed();
        }
    });
    changed
}

/// "macOS", "Windows"...: a system's name as shown.
fn os_label(os: flock_net::Os) -> &'static str {
    match os {
        flock_net::Os::Mac => "macOS",
        flock_net::Os::Windows => "Windows",
        flock_net::Os::Linux => "Linux",
        flock_net::Os::Android => "Android",
    }
}

fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

/// A card for "the other computer" (an offline one, one not yet paired, an empty list):
/// dashed outline and hatching instead of a solid fill.
fn ghost_card(ui: &mut Ui, p: Palette, add: impl FnOnce(&mut Ui)) {
    let slot = ui.painter().add(Shape::Noop);
    let shown = egui::Frame::new().corner_radius(16).inner_margin(Margin::symmetric(18, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 0.0;
        add(ui);
    });
    ui.painter().set(slot, Shape::Vec(ghost_shapes(shown.response.rect, 16.0, p)));
}

/// A ghost card holding only a line saying there is nothing here yet.
fn empty_card(ui: &mut Ui, p: Palette, text: &str) {
    ghost_card(ui, p, |ui| empty(ui, p, text));
}

/// A thin line between rows, edge to edge inside the card, with room above and below so
/// a row's control never sits on it.
fn divider(ui: &mut Ui, p: Palette) {
    ui.add_space(DIVIDER_GAP);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.border));
    ui.add_space(DIVIDER_GAP);
}

/// The room a divider keeps above and below it.
const DIVIDER_GAP: f32 = 4.0;

fn hint(ui: &mut Ui, p: Palette, text: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(text).size(12.5).color(p.muted));
}

/// A hint about something to look at, in amber.
fn amber_hint(ui: &mut Ui, p: Palette, text: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(text).size(12.5).color(p.amber));
}

fn empty(ui: &mut Ui, p: Palette, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).size(13.0).color(p.muted));
    ui.add_space(8.0);
}

/// A row with a label and an on/off switch on the right. Returns true when flipped.
/// A `note` shows in smaller text under the label.
fn switch_row(ui: &mut Ui, p: Palette, label: &str, note: Option<&str>, on: &mut bool) -> bool {
    switch_row_full(ui, p, label, note, on).0
}

/// `switch_row`, also giving where its label was drawn (to put something after it).
/// A long `note` wraps under the label.
fn switch_row_full(ui: &mut Ui, p: Palette, label: &str, note: Option<&str>, on: &mut bool) -> (bool, Rect) {
    let width = ui.available_width();
    let note = note.map(|n| ui.painter().layout(n.to_string(), FontId::proportional(12.5), p.muted, (width - 40.0 - 24.0).max(60.0)));
    let h = note.as_ref().map_or(40.0, |g| (18.0 + 4.0 + g.size().y + 18.0).max(50.0));
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, h), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| WidgetInfo::selected(Role::Switch, true, *on, label));
    if resp.clicked() {
        *on = !*on;
    }
    let painter = ui.painter();
    let label_rect = match note {
        Some(note) => {
            let top = rect.center().y - (18.0 + 4.0 + note.size().y) / 2.0;
            painter.galley(Pos2::new(rect.left(), top + 22.0), note, p.muted);
            painter.text(Pos2::new(rect.left(), top), Align2::LEFT_TOP, label, FontId::proportional(14.5), p.text)
        }
        None => painter.text(Pos2::new(rect.left(), rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(14.5), p.text),
    };

    let how_on = ui.ctx().animate_bool_responsive(resp.id, *on);
    let track = Rect::from_center_size(Pos2::new(rect.right() - 20.0, rect.center().y), Vec2::new(40.0, 24.0));
    paint_switch(painter, p, track, how_on);
    (resp.clicked(), label_rect)
}

/// The switch itself, `how_on` from 0 (off) to 1 (on): neutral gray to strong ink/white.
fn paint_switch(painter: &egui::Painter, p: Palette, track: Rect, how_on: f32) {
    painter.rect_filled(track, 12, p.off.lerp_to_gamma(p.strong, how_on));
    let x = egui::lerp(track.left() + 12.0..=track.right() - 12.0, how_on);
    let knob = Pos2::new(x, track.center().y);
    painter.circle_filled(knob, 9.5, Color32::WHITE.lerp_to_gamma(p.on_strong, how_on));
    painter.circle_stroke(knob, 9.5, Stroke::new(1.0, p.ink.gamma_multiply(0.10)));
}

/// A small round "i" centred at `center` that explains `text` while hovered. It takes
/// clicks too, so clicking it does not flip the row it sits on.
fn info_icon(ui: &mut Ui, p: Palette, center: Pos2, id: &str, text: &str) {
    let rect = Rect::from_center_size(center, Vec2::splat(20.0));
    let resp = ui.interact(rect, egui::Id::unique(("info-icon", id)), Sense::click());
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, text));
    let color = if resp.hovered() { p.text } else { p.muted };
    let painter = ui.painter();
    painter.circle_stroke(center, 8.0, Stroke::new(1.3, color));
    painter.circle_filled(center + Vec2::new(0.0, -3.7), 1.25, color);
    painter.line_segment([center + Vec2::new(0.0, -1.2), center + Vec2::new(0.0, 4.3)], Stroke::new(1.7, color));
    resp.on_hover_ui(|ui| {
        ui.set_max_width(400.0);
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(RichText::new(text).size(12.5).color(p.text));
    });
}

const ROW_SMALL: f32 = 42.0;

/// A row at least `height` tall with a label on the left and a control `width` wide on
/// the right.
fn labeled_row(
    ui: &mut Ui,
    p: Palette,
    label: &str,
    width: f32,
    height: f32,
    control: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    row_with_help(ui, p, label, None, None, width, height, CONTROL_HEIGHT + 2.0, control)
}

/// `labeled_row` with a line of help under the label.
fn labeled_row_help(
    ui: &mut Ui,
    p: Palette,
    label: &str,
    help: Option<&str>,
    width: f32,
    control: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    row_with_help(ui, p, label, help, None, width, ROW_SMALL, CONTROL_HEIGHT + 2.0, control)
}

/// `labeled_row_help` with an "i" after the label explaining `info`.
fn labeled_row_info(
    ui: &mut Ui,
    p: Palette,
    label: &str,
    help: Option<&str>,
    info: &str,
    width: f32,
    control: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    row_with_help(ui, p, label, help, Some(info), width, ROW_SMALL, CONTROL_HEIGHT + 2.0, control)
}

/// The row itself. The label and the help wrap in the room the control leaves; the row
/// is as tall as they need, at least `height` and `control_height`.
#[allow(clippy::too_many_arguments)]
fn row_with_help(
    ui: &mut Ui,
    p: Palette,
    label: &str,
    help: Option<&str>,
    info: Option<&str>,
    width: f32,
    height: f32,
    control_height: f32,
    control: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    let avail = ui.available_width();
    // Never so wide that the label is left with a sliver.
    let width = width.min(avail * 0.64);
    let text_w = (avail - width - 16.0).max(80.0);
    let label_g = ui.painter().layout(label.to_string(), FontId::proportional(14.5), p.text, text_w);
    let help_g = help.map(|h| ui.painter().layout(h.to_string(), FontId::proportional(12.5), p.muted, text_w));
    let text_h = label_g.size().y + help_g.as_ref().map_or(0.0, |g| 3.0 + g.size().y);
    let h = height.max(text_h + 18.0).max(control_height + 12.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(avail, h), Sense::hover());
    let top = rect.center().y - text_h / 2.0;
    let painter = ui.painter();
    let label_h = label_g.size().y;
    let label_right = label_g.rows.first().map_or(0.0, |r| r.rect().right());
    painter.galley(Pos2::new(rect.left(), top), label_g, p.text);
    if let Some(text) = info {
        info_icon(ui, p, Pos2::new(rect.left() + label_right + 16.0, top + label_h / 2.0), label, text);
    }
    let painter = ui.painter();
    if let Some(g) = help_g {
        painter.galley(Pos2::new(rect.left(), top + label_h + 3.0), g, p.muted);
    }
    let area = Rect::from_min_max(
        Pos2::new(rect.right() - width, rect.center().y - control_height / 2.0),
        Pos2::new(rect.right(), rect.center().y + control_height / 2.0),
    );
    // A child, not a scope: a scope would pull the cursor back up to the control's
    // bottom, and the next row or divider would crowd it.
    ui.new_child(UiBuilder::new().max_rect(area)).scope(control).inner
}

/// Height and text size of a segment bar.
#[derive(Clone, Copy)]
struct SegSize {
    height: f32,
    text: f32,
}
/// Every button, drop-down and segment bar inside a card is this tall.
const CONTROL_HEIGHT: f32 = 28.0;
/// Buttons are at least this wide, so short labels line up with long ones.
const BUTTON_MIN_WIDTH: f32 = 84.0;
const SEG_SMALL: SegSize = SegSize { height: CONTROL_HEIGHT, text: 13.0 };

/// A taken segment: the gray of "Off" sunk toward the track, so it reads as taken, but
/// not available.
fn locked_fill(p: Palette) -> Color32 {
    p.off.lerp_to_gamma(p.track, 0.5)
}

/// Equal-width segments in one row on a shared bar. `selected(i)` says which is on;
/// returns the index clicked.
#[allow(clippy::too_many_arguments)]
fn segments(
    ui: &mut Ui,
    p: Palette,
    name: &str,
    labels: &[String],
    role: Role,
    size: SegSize,
    locked: &[usize],
    off: Option<usize>,
    selected: impl Fn(usize) -> bool,
) -> Option<usize> {
    segments_styled(ui, p, name, labels, role, size, locked, off, false, selected)
}

/// `segments`; with `lime` the chosen segment is lime (the computer's role, nothing else).
#[allow(clippy::too_many_arguments)]
fn segments_styled(
    ui: &mut Ui,
    p: Palette,
    name: &str,
    labels: &[String],
    role: Role,
    size: SegSize,
    locked: &[usize],
    off: Option<usize>,
    lime: bool,
    selected: impl Fn(usize) -> bool,
) -> Option<usize> {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), size.height), Sense::hover());
    ui.painter().rect_filled(rect, 8, p.track);
    let w = rect.width() / labels.len() as f32;
    let mut clicked = None;
    for (i, label) in labels.iter().enumerate() {
        let seg = Rect::from_min_size(Pos2::new(rect.left() + w * i as f32, rect.top()), Vec2::new(w, rect.height()))
            .shrink(3.0);
        // Named per bar: the rows sit in look-alike child areas, so an id derived from
        // the area would be shared and one bar's click would also hit the other.
        let id = egui::Id::unique(("segment", name, i));
        let locked = locked.contains(&i);
        let resp = if locked {
            ui.interact(seg, id, Sense::hover())
        } else {
            ui.interact(seg, id, Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand)
        };
        let on = selected(i);
        resp.widget_info(|| WidgetInfo::selected(role, !locked, on, label));
        if resp.clicked() {
            clicked = Some(i);
        }
        let painter = ui.painter();
        let (fill, color) = if locked {
            (locked_fill(p), p.muted)
        } else if on && off == Some(i) {
            (p.off, p.text)
        } else if on && lime {
            (p.accent, p.ink)
        } else if on {
            (p.seg_on, p.text)
        } else if resp.hovered() {
            (p.hover, p.text)
        } else {
            (Color32::TRANSPARENT, p.muted)
        };
        painter.rect_filled(seg, 7, fill);
        if on && !lime && off != Some(i) && !locked {
            painter.rect_stroke(seg, 7, Stroke::new(1.0, p.border), StrokeKind::Inside);
        }
        if locked {
            // A faint inner shadow: pressed in, not pressable.
            painter.rect_stroke(seg, 7, Stroke::new(1.5, Color32::from_black_alpha(45)), StrokeKind::Inside);
        }
        let font = if on || locked { bold(size.text) } else { FontId::proportional(size.text) };
        // A trailing " ✓" (button or gesture is on) is drawn, not taken from the font,
        // so it looks the same in every font.
        let (text, mark) = match label.strip_suffix(" ✓") {
            Some(text) => (text, true),
            None => (label.as_str(), false),
        };
        let galley = painter.layout_no_wrap(text.to_string(), font, color);
        let mark_width = if mark { size.text * 0.9 } else { 0.0 };
        let left = seg.center().x - (galley.size().x + mark_width) / 2.0;
        let text_right = left + galley.size().x;
        painter.galley(Pos2::new(left, seg.center().y - galley.size().y / 2.0), galley, color);
        if mark {
            let u = size.text / 14.0;
            let (x, y) = (text_right + 4.0 * u, seg.center().y);
            let tick = Stroke::new(1.6 * u, color);
            painter.line_segment([Pos2::new(x, y), Pos2::new(x + 3.0 * u, y + 3.0 * u)], tick);
            painter.line_segment([Pos2::new(x + 3.0 * u, y + 3.0 * u), Pos2::new(x + 8.0 * u, y - 3.5 * u)], tick);
        }
    }
    clicked
}

/// One-of-several choice on a segment bar. Returns true on change.
fn segmented<T: PartialEq + Copy>(
    ui: &mut Ui,
    p: Palette,
    name: &str,
    value: &mut T,
    options: &[(T, &str)],
    size: SegSize,
) -> bool {
    let labels: Vec<String> = options.iter().map(|(_, l)| l.to_string()).collect();
    match segments(ui, p, name, &labels, Role::RadioButton, size, &[], None, |i| options[i].0 == *value) {
        Some(i) if options[i].0 != *value => {
            *value = options[i].0;
            true
        }
        _ => false,
    }
}

/// One-of-many choice in a drop-down filling the space it is given. Returns true on change.
fn combo<T: PartialEq + Copy>(ui: &mut Ui, name: &str, value: &mut T, options: &[(T, &str)]) -> bool {
    let shown = options.iter().find(|(v, _)| v == value).map_or("", |(_, l)| *l);
    let mut changed = false;
    egui::ComboBox::from_id_salt(name).selected_text(shown).width(ui.available_width().min(220.0)).show_ui(ui, |ui| {
        for (v, label) in options {
            changed |= ui.selectable_value(value, *v, *label).changed();
        }
    });
    changed
}

/// How large an effect is drawn. True when changed.
fn size_row(ui: &mut Ui, p: Palette, t: &Texts, name: &str, size: &mut u16) -> bool {
    let labels: Vec<String> = EFFECT_SIZES.iter().map(|s| format!("{s}%")).collect();
    let choices: Vec<(u16, &str)> = EFFECT_SIZES.iter().copied().zip(labels.iter().map(String::as_str)).collect();
    let width = (ui.available_width() * 0.72).min(300.0);
    labeled_row(ui, p, t.effect_size, width, ROW_SMALL, |ui| segmented(ui, p, name, size, &choices, SEG_SMALL))
}

/// An explanation on the left and buttons on the right. Returns the index clicked.
fn confirm_row(ui: &mut Ui, p: Palette, explain: &str, labels: &[(&str, ButtonKind)]) -> Option<usize> {
    confirm_row_in(ui, p, explain, p.muted, labels)
}

/// `confirm_row` with the explanation in `color`.
fn confirm_row_in(ui: &mut Ui, p: Palette, explain: &str, color: Color32, labels: &[(&str, ButtonKind)]) -> Option<usize> {
    let buttons: f32 = labels.iter().map(|(l, k)| button_width(ui, l, *k)).sum::<f32>() + 8.0 * (labels.len() - 1) as f32;
    let width = ui.available_width();
    let text = ui.painter().layout(explain.to_string(), FontId::proportional(12.5), color, (width - buttons - 16.0).max(80.0));
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, (text.size().y + 20.0).max(48.0)), Sense::hover());
    ui.painter().galley(Pos2::new(rect.left(), rect.center().y - text.size().y / 2.0), text, color);
    let area = Rect::from_min_max(
        Pos2::new(rect.right() - buttons, rect.center().y - 15.0),
        Pos2::new(rect.right(), rect.center().y + 15.0),
    );
    let mut clicked = None;
    ui.new_child(UiBuilder::new().max_rect(area)).scope(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.horizontal(|ui| {
            for (i, (label, kind)) in labels.iter().enumerate() {
                if button(ui, p, label, *kind).clicked() {
                    clicked = Some(i);
                }
            }
        });
    });
    clicked
}

/// Plain buttons side by side across the row. When they do not fit at their usual
/// width, each gets its label's width with less padding, and what is left of the row
/// is shared equally. Returns the index clicked.
fn button_row(ui: &mut Ui, p: Palette, labels: &[&str]) -> Option<usize> {
    let pairs: Vec<(&str, ButtonKind)> = labels.iter().map(|&l| (l, ButtonKind::Plain)).collect();
    button_row_kinds(ui, p, &pairs)
}

/// Like `button_row`, allowing each button its own `ButtonKind`.
fn button_row_kinds(ui: &mut Ui, p: Palette, buttons: &[(&str, ButtonKind)]) -> Option<usize> {
    let gap = 8.0;
    let room = ui.available_width() - gap * (buttons.len() - 1) as f32;
    let usual: Vec<f32> = buttons.iter().map(|(l, k)| button_width(ui, l, *k)).collect();
    let tight: Vec<f32> = buttons.iter().map(|(l, k)| text_width(ui, l, *k) + 16.0).collect();
    let widths: Vec<f32> = if usual.iter().sum::<f32>() <= room {
        usual
    } else {
        let spare = ((room - tight.iter().sum::<f32>()) / buttons.len() as f32).max(0.0);
        tight.iter().map(|w| w + spare).collect()
    };
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        for (i, ((label, kind), width)) in buttons.iter().zip(widths).enumerate() {
            if button_sized(ui, p, label, *kind, width).clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

/// `text` on one line, cut with "…" to fit `width`.
fn elided(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping { max_width: width.max(1.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.fonts_mut(|f| f.layout_job(job))
}

struct Row<'a> {
    title: &'a str,
    subtitle: Option<&'a str>,
    badge: Option<(&'a str, Color32)>,
}

/// A selectable list row: radio mark, title, optional second line and badge.
fn choice_row(ui: &mut Ui, p: Palette, selected: bool, row: &Row) -> Response {
    let h = if row.subtitle.is_some() { 46.0 } else { 38.0 };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| WidgetInfo::selected(Role::RadioButton, true, selected, row.title));
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect.expand2(Vec2::new(8.0, -3.0)), 8, p.hover);
    }

    let c = Pos2::new(rect.left() + 10.0, rect.center().y);
    if selected {
        painter.circle_filled(c, 10.0, p.strong);
        painter.circle_filled(c, 4.0, p.on_strong);
    } else {
        painter.circle_stroke(c, 9.25, Stroke::new(1.5, p.off));
    }

    let x = rect.left() + 32.0;
    let mut right = rect.right();
    if let Some((text, color)) = row.badge {
        let galley = painter.layout_no_wrap(text.to_string(), FontId::proportional(12.0), color);
        let pill = Rect::from_min_size(
            Pos2::new(right - galley.size().x - 16.0, rect.center().y - 11.0),
            Vec2::new(galley.size().x + 16.0, 22.0),
        );
        painter.rect_filled(pill, 11, color.gamma_multiply(0.14));
        painter.galley(pill.center() - galley.size() / 2.0, galley, color);
        right = pill.left() - 8.0;
    }
    let clip = Rect::from_min_max(rect.min, Pos2::new(right, rect.max.y));
    let painter = painter.with_clip_rect(clip.intersect(painter.clip_rect()));
    match row.subtitle {
        Some(sub) => {
            painter.text(Pos2::new(x, rect.center().y - 1.0), Align2::LEFT_BOTTOM, row.title, FontId::proportional(14.5), p.text);
            painter.text(Pos2::new(x, rect.center().y + 3.0), Align2::LEFT_TOP, sub, FontId::proportional(12.5), p.muted);
        }
        None => {
            painter.text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, row.title, FontId::proportional(14.5), p.text);
        }
    }
    resp
}

#[derive(Clone, Copy, PartialEq)]
enum ButtonKind {
    Plain,
    Primary,
    Danger,
    DangerFilled,
}

fn button_font(kind: ButtonKind) -> FontId {
    if matches!(kind, ButtonKind::DangerFilled | ButtonKind::Primary) { bold(14.0) } else { FontId::proportional(14.0) }
}

fn text_width(ui: &Ui, label: &str, kind: ButtonKind) -> f32 {
    ui.painter().layout_no_wrap(label.to_string(), button_font(kind), Color32::WHITE).size().x
}

fn button_width(ui: &Ui, label: &str, kind: ButtonKind) -> f32 {
    (text_width(ui, label, kind) + 28.0).max(BUTTON_MIN_WIDTH)
}

fn button(ui: &mut Ui, p: Palette, label: &str, kind: ButtonKind) -> Response {
    let width = button_width(ui, label, kind);
    button_sized(ui, p, label, kind, width)
}

/// A button no wider than its label needs (down to `COMPACT_MIN_WIDTH`), for rows of
/// several buttons inside a narrow card or header.
fn compact_button(ui: &mut Ui, p: Palette, label: &str, kind: ButtonKind) -> Response {
    let width = (text_width(ui, label, kind) + 22.0).max(64.0);
    button_sized(ui, p, label, kind, width)
}

fn button_sized(ui: &mut Ui, p: Palette, label: &str, kind: ButtonKind, width: f32) -> Response {
    let font = button_font(kind);
    let size = Vec2::new(width, CONTROL_HEIGHT);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    let hovered = resp.hovered();
    let (fill, stroke, color) = match kind {
        ButtonKind::Plain => (if hovered { p.hover } else { p.card }, p.track, p.text),
        // Ink with white text (near-white with ink text in dark mode).
        ButtonKind::Primary => (if hovered { p.strong.gamma_multiply(0.85) } else { p.strong }, p.strong, p.on_strong),
        // Red text on a light red tint.
        ButtonKind::Danger => (p.red.gamma_multiply(if hovered { 0.22 } else { 0.12 }), Color32::TRANSPARENT, p.red),
        ButtonKind::DangerFilled => (if hovered { p.red.gamma_multiply(0.85) } else { p.red }, p.red, Color32::WHITE),
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(9), fill);
    painter.rect_stroke(rect, 9, Stroke::new(1.0, stroke), StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, font, color);
    resp
}

/// A shortcut as stored in the settings ("ctrl+alt+1"), or None for keys that make no
/// good shortcut: a number, letter or F key, with Ctrl/Alt unless it is an F key.
fn shortcut_text(key: egui::Key, m: egui::Modifiers) -> Option<String> {
    let name = key.name();
    let f_key = name.len() > 1 && name.starts_with('F') && name[1..].parse::<u8>().is_ok();
    let plain = name.len() == 1 && name.chars().all(|c| c.is_ascii_alphanumeric());
    if !(f_key || plain) || (!f_key && !(m.ctrl || m.alt || m.mac_cmd)) {
        return None;
    }
    let mods = [(m.ctrl, "ctrl"), (m.alt, "alt"), (m.shift, "shift"), (m.mac_cmd, "super")];
    let mut parts: Vec<&str> = mods.iter().filter(|x| x.0).map(|x| x.1).collect();
    parts.push(name);
    Some(parts.join("+").to_lowercase())
}

impl SettingsApp {
    fn master_key(&self) -> Option<String> {
        if self.cfg.trigger_device.eq_ignore_ascii_case("auto") {
            self.status
                .trigger_key
                .clone()
                .or_else(|| self.devices.iter().find(|d| d.master).map(|d| d.key.clone()))
        } else {
            Some(self.cfg.trigger_device.clone())
        }
    }

    fn master_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        section(ui, p, t.master, |ui| {
            let auto = self.cfg.trigger_device.eq_ignore_ascii_case("auto");
            let now = self.status.trigger.as_ref().map(|name| format!("{} {name}", t.current));
            let row = Row { title: t.master_auto, subtitle: now.as_deref(), badge: None };
            if device_choice_row(ui, p, Mark::Radio, auto, &row).clicked() && !auto {
                self.apply(|c| c.trigger_device = "auto".into());
            }
            let masters: Vec<DeviceInfo> = self.devices.iter().filter(|d| d.master).cloned().collect();
            if masters.is_empty() {
                divider(ui, p);
                empty(ui, p, t.no_master);
            }
            for d in masters {
                divider(ui, p);
                let chosen = !auto && self.cfg.trigger_device.eq_ignore_ascii_case(&d.key);
                let subtitle = device_subtitle(&d, t);
                let row = Row { title: &d.name, subtitle: Some(&subtitle), badge: away_badge(&d, t, p) };
                if device_choice_row(ui, p, Mark::Radio, chosen, &row).clicked() && !chosen {
                    let key = d.key.clone();
                    self.apply(|c| c.trigger_device = key);
                }
            }
        });
    }

    fn group_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        section(ui, p, t.follow, |ui| {
            ui.add_space(4.0);
            hint(ui, p, t.follow_hint);
            let master = self.master_key();
            let others: Vec<DeviceInfo> = self
                .devices
                .iter()
                .filter(|d| !master.as_ref().is_some_and(|m| m.eq_ignore_ascii_case(&d.key)))
                .cloned()
                .collect();
            let keys: Vec<String> = others.iter().map(|d| d.key.clone()).collect();

            let all = self.cfg.follows_all();
            let row = Row { title: t.follow_all, subtitle: None, badge: None };
            if device_choice_row(ui, p, Mark::Check, all, &row).clicked() {
                self.apply(|c| c.follow = if all { vec![] } else { vec!["all".into()] });
            }
            if others.is_empty() {
                divider(ui, p);
                empty(ui, p, t.no_devices);
            }
            for d in &others {
                divider(ui, p);
                let on = all || self.cfg.follow.iter().any(|f| f.eq_ignore_ascii_case(&d.key));
                let subtitle = device_subtitle(d, t);
                let row = Row { title: &d.name, subtitle: Some(&subtitle), badge: away_badge(d, t, p) };
                if device_choice_row(ui, p, Mark::Check, on, &row).clicked() {
                    let key = d.key.clone();
                    let keys = keys.clone();
                    self.apply(|c| c.toggle_follow(&keys, &key));
                }
            }
        });
    }

    fn haptics_summary(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let h = &self.cfg.haptics;
        let state = if h.on_switch || h.on_edge { t.haptics_state_on } else { t.haptics_state_off };
        let strength = if h.intensity == 0 { t.haptics_follow.to_string() } else { format!("{}%", h.intensity) };
        let line = format!("{state} · {} {strength}", t.haptics_intensity);
        let mut open = false;
        section(ui, p, t.section_haptics, |ui| {
            labeled_row_help(ui, p, &line, Some(t.haptics_summary_help), 130.0, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    open = button(ui, p, t.haptics_open, ButtonKind::Plain).clicked();
                });
                false
            });
        });
        if open {
            self.stop_recording();
            let position = ui
                .ctx()
                .input(|i| i.viewport().outer_rect)
                .map(|r| r.center() - Vec2::from(HAPTICS_DIALOG_SIZE) / 2.0);
            let current = self.cfg.haptics.clone();
            self.haptics_dialog =
                Some(HapticsDialog { draft: current.clone(), opened_with: current, position, size_guard: SizeGuard::default() });
        }
    }

    fn haptics_window(&mut self, ctx: &egui::Context, t: &Texts) {
        let Some(dialog) = &self.haptics_dialog else { return };
        let mut builder = egui::ViewportBuilder::default()
            .with_title(window_title(t.haptics_dialog_title))
            .with_inner_size(HAPTICS_DIALOG_SIZE)
            .with_resizable(false)
            .with_minimize_button(false)
            .with_maximize_button(false);
        if let Some(position) = dialog.position {
            builder = builder.with_position(position);
        }
        ctx.show_viewport_immediate(haptics_viewport(), builder, |ui, _| self.haptics_dialog_ui(ui, t));
    }

    fn haptics_dialog_ui(&mut self, ui: &mut Ui, t: &Texts) {
        let Some(mut dialog) = self.haptics_dialog.take() else { return };
        keep_size(ui.ctx(), &mut dialog.size_guard, HAPTICS_DIALOG_SIZE, HAPTICS_DIALOG_SIZE);
        let p = Palette::new(ui.visuals().dark_mode);
        let closing = ui.ctx().input(|i| i.viewport().close_requested());
        let (mut apply, mut ok, mut close) = (false, false, closing);
        let panel = egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(18, 14));
        let d = &mut dialog.draft;
        let error = self.error.clone();
        let mut feel = None;
        egui::CentralPanel::default().frame(panel).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 46.0).show(ui, |ui| {
                // What haptics are, then each situation, in two cards with room between rows.
                let mut on = d.on_switch || d.on_arrive || d.on_edge;
                card(ui, p, |ui| {
                    let before = on;
                    dialog_row(ui, p, t.haptics_master, 48.0, |ui| {
                        toggle(ui, p, t.haptics_master, &mut on);
                    });
                    if on != before {
                        d.on_switch = on;
                        d.on_arrive = on;
                        d.on_edge = on;
                    }
                    divider(ui, p);
                    ui.add_enabled_ui(on, |ui| {
                        // 0 = as Logi Options+ sets it; otherwise the strength in percent.
                        let choices: [(u8, &str); 5] = [(0, t.haptics_follow), (25, "25%"), (50, "50%"), (75, "75%"), (100, "100%")];
                        dialog_row(ui, p, t.haptics_intensity, SITUATION_CONTROLS, |ui| {
                            segmented(ui, p, "haptic-intensity", &mut d.intensity, &choices, SEG_SMALL);
                        });
                    });
                });
                ui.add_space(12.0);
                card(ui, p, |ui| {
                    ui.add_enabled_ui(on, |ui| {
                        let pulses: Vec<(u8, &str)> =
                            waveform::ALL.into_iter().zip(t.waveform_names).chain([(waveform::RANDOM, t.locate_random)]).collect();
                        if situation_row(ui, p, t, t.haptics_switch, &mut d.on_switch, |ui| {
                            combo(ui, "pulse-leave", &mut d.switch_waveform, &pulses);
                        }) {
                            feel = Some(HapticPreview::Pulse(d.switch_waveform));
                        }
                        divider(ui, p);
                        if situation_row(ui, p, t, t.haptics_arrive, &mut d.on_arrive, |ui| {
                            combo(ui, "pulse-arrive", &mut d.arrive_waveform, &pulses);
                        }) {
                            feel = Some(HapticPreview::Pulse(d.arrive_waveform));
                        }
                        divider(ui, p);
                        let modes = [(true, t.haptics_rising), (false, t.haptics_even)];
                        if situation_row(ui, p, t, t.haptics_edge, &mut d.on_edge, |ui| {
                            segmented(ui, p, "edge-pattern", &mut d.progressive, &modes, SEG_SMALL);
                        }) {
                            feel = Some(HapticPreview::Edge { rising: d.progressive });
                        }
                    });
                });
                ui.add_space(8.0);
                // Below the cards, wrapping to the window's width.
                egui::Frame::new().inner_margin(Margin::symmetric(4, 0)).show(ui, |ui| hint(ui, p, t.haptics_hint));
                if let Some(err) = &error {
                    ui.add_space(8.0);
                    ui.label(RichText::new(err).color(p.red));
                }
            });
            ui.add_space(10.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                close |= button(ui, p, t.close, ButtonKind::Plain).clicked();
                ok = button(ui, p, t.ok, ButtonKind::Primary).clicked();
                apply = button(ui, p, t.apply, ButtonKind::Plain).clicked();
            });
        });
        if let Some(what) = feel {
            shared::request_haptic_preview(what, dialog.draft.intensity);
        }
        if close {
            // Back to how it was when the window opened, undoing an Apply too.
            if self.cfg.haptics != dialog.opened_with {
                let original = dialog.opened_with.clone();
                self.apply(|c| c.haptics = original);
            }
            return;
        }
        if apply || ok {
            let chosen = dialog.draft.clone();
            self.apply(|c| c.haptics = chosen);
            if ok && self.error.is_none() {
                return;
            }
        }
        self.haptics_dialog = Some(dialog);
    }

    fn edge_confirm_window(&mut self, ctx: &egui::Context, t: &Texts, p: Palette) {
        if !self.edge_confirm {
            return;
        }
        let (mut close, mut on) = (false, false);
        let frame = egui::Frame::new()
            .fill(p.bg)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(16)
            .inner_margin(Margin::same(18))
            .shadow(ctx.global_style().visuals.window_shadow);
        let response = egui::Modal::new(egui::Id::unique("edge-confirm")).frame(frame).show(ctx, |ui| {
            ui.set_width(400.0);
            ui.label(RichText::new(t.edges_title).font(bold(16.0)).color(p.text));
            ui.add_space(8.0);
            ui.label(RichText::new(t.edges_confirm).size(13.0).color(p.text));
            ui.add_space(14.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                on = button(ui, p, t.turn_on, ButtonKind::Primary).clicked();
                close = button(ui, p, t.cancel, ButtonKind::Plain).clicked();
            });
        });
        if on {
            self.apply(|c| c.edges.on = true);
            self.edge_confirm = false;
        } else if close || response.should_close() {
            self.edge_confirm = false;
        }
    }
}

struct HapticsDialog {
    draft: Haptics,
    /// What Close goes back to, even after an Apply.
    opened_with: Haptics,
    /// Top left of the window, centred on the main window when opened.
    position: Option<Pos2>,
    size_guard: SizeGuard,
}

const HAPTICS_DIALOG_SIZE: [f32; 2] = [620.0, 440.0];

fn haptics_viewport() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("haptics-settings")
}

fn device_subtitle(d: &DeviceInfo, t: &Texts) -> String {
    let kind = match d.kind.as_str() {
        "mouse" => t.kind_mouse,
        "keyboard" => t.kind_keyboard,
        _ => t.kind_other,
    };
    format!("{kind} · {}", d.key)
}

fn dialog_row(ui: &mut Ui, p: Palette, label: &str, width: f32, control: impl FnOnce(&mut Ui)) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), DIALOG_ROW), Sense::hover());
    ui.painter().text(Pos2::new(rect.left(), rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(14.5), p.text);
    let area = Rect::from_min_max(
        Pos2::new(rect.right() - width, rect.center().y - 15.0),
        Pos2::new(rect.right(), rect.center().y + 15.0),
    );
    ui.new_child(UiBuilder::new().max_rect(area)).scope(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Radio,
    Check,
}

fn device_choice_row(ui: &mut Ui, p: Palette, mark: Mark, selected: bool, row: &Row) -> Response {
    let h = if row.subtitle.is_some() { 46.0 } else { 38.0 };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let role = if mark == Mark::Radio { Role::RadioButton } else { Role::CheckBox };
    resp.widget_info(|| WidgetInfo::selected(role, true, selected, row.title));
    let painter = ui.painter();
    if resp.hovered() {
        painter.rect_filled(rect.expand2(Vec2::new(8.0, -3.0)), 8, p.hover);
    }

    let c = Pos2::new(rect.left() + 10.0, rect.center().y);
    match (mark, selected) {
        (Mark::Radio, true) => {
            painter.circle_filled(c, 10.0, p.strong);
            painter.circle_filled(c, 4.0, p.on_strong);
        }
        (Mark::Radio, false) => {
            painter.circle_stroke(c, 9.25, Stroke::new(1.5, p.track));
        }
        (Mark::Check, true) => {
            let r = Rect::from_center_size(c, Vec2::splat(20.0));
            painter.rect_filled(r, 6, p.strong);
            let tick = Stroke::new(2.2, p.on_strong);
            painter.line_segment([c + Vec2::new(-5.0, 0.5), c + Vec2::new(-1.5, 4.0)], tick);
            painter.line_segment([c + Vec2::new(-1.5, 4.0), c + Vec2::new(5.5, -3.5)], tick);
        }
        (Mark::Check, false) => {
            let r = Rect::from_center_size(c, Vec2::splat(18.5));
            painter.rect_stroke(r, 5, Stroke::new(1.5, p.track), StrokeKind::Inside);
        }
    }

    let x = rect.left() + 32.0;
    let mut right = rect.right();
    if let Some((text, color)) = row.badge {
        let galley = painter.layout_no_wrap(text.to_string(), FontId::proportional(12.0), color);
        let pill = Rect::from_min_size(
            Pos2::new(right - galley.size().x - 16.0, rect.center().y - 11.0),
            Vec2::new(galley.size().x + 16.0, 22.0),
        );
        painter.rect_filled(pill, 11, color.gamma_multiply(0.14));
        painter.galley(pill.center() - galley.size() / 2.0, galley, color);
        right = pill.left() - 8.0;
    }
    let clip = Rect::from_min_max(rect.min, Pos2::new(right, rect.max.y));
    let painter = painter.with_clip_rect(clip.intersect(painter.clip_rect()));
    match row.subtitle {
        Some(sub) => {
            painter.text(Pos2::new(x, rect.center().y - 1.0), Align2::LEFT_BOTTOM, row.title, FontId::proportional(14.5), p.text);
            painter.text(Pos2::new(x, rect.center().y + 3.0), Align2::LEFT_TOP, sub, FontId::proportional(12.5), p.muted);
        }
        None => {
            painter.text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, row.title, FontId::proportional(14.5), p.text);
        }
    }
    resp
}

fn away_badge<'a>(d: &DeviceInfo, t: &'a Texts, p: Palette) -> Option<(&'a str, Color32)> {
    (!d.online).then_some((t.away, p.amber))
}

fn situation_row(ui: &mut Ui, p: Palette, t: &Texts, label: &str, on: &mut bool, control: impl FnOnce(&mut Ui)) -> bool {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), DIALOG_ROW), Sense::hover());
    ui.painter().text(Pos2::new(rect.left(), rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(14.5), p.text);
    let area = Rect::from_min_max(
        Pos2::new(rect.right() - SITUATION_CONTROLS, rect.center().y - 15.0),
        Pos2::new(rect.right(), rect.center().y + 15.0),
    );
    let mut pressed = false;
    ui.new_child(UiBuilder::new().max_rect(area)).scope(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            toggle(ui, p, label, on);
            let enabled = *on;
            ui.add_enabled_ui(enabled, |ui| {
                pressed = button(ui, p, t.haptics_feel, ButtonKind::Plain).clicked();
                control(ui);
            });
        });
    });
    pressed
}

fn toggle(ui: &mut Ui, p: Palette, label: &str, on: &mut bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(40.0, 24.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| WidgetInfo::selected(Role::Switch, true, *on, label));
    if resp.clicked() {
        *on = !*on;
    }
    let how_on = ui.ctx().animate_bool_responsive(resp.id, *on);
    paint_switch(ui.painter(), p, rect, how_on);
    resp.clicked()
}

const SITUATION_CONTROLS: f32 = 360.0;
const DIALOG_ROW: f32 = 50.0;

/// Where the mouse is, for the status card.
#[derive(Debug, Clone, PartialEq)]
enum MouseAt {
    Here,
    /// On another computer of this name (this one controls it).
    There(String),
    /// Another computer (of this name) is using this one.
    Controlled(String),
}

impl SettingsApp {
    /// Re-reads the service's status twice a second.
    fn poll_network(&mut self) {
        if self.shots.is_some() {
            return;
        }
        if self.net_read.elapsed() >= Duration::from_millis(500) {
            self.net_status = flock_net::read_status(crate::net::config_dir()).ok();
            self.net_read = Instant::now();
        }
    }

    fn mouse_at(&self) -> MouseAt {
        match NetState::of(&self.cfg, self.net_status.as_ref()) {
            NetState::Controlling(name) => MouseAt::There(name),
            NetState::Controlled(name) => MouseAt::Controlled(name),
            _ => MouseAt::Here,
        }
    }

    /// This computer's network id: from the running service, else from the identity file
    /// when one exists (read once). None before sharing has ever been switched on: the
    /// settings window does not make an identity of its own.
    fn my_id(&mut self) -> Option<String> {
        if let Some(s) = self.net_status.as_ref().filter(|s| s.running && !s.me.id.is_empty()) {
            self.my_id = Some(s.me.id.clone());
        }
        if self.my_id.is_none() {
            let file = crate::net::config_dir().join("flockswitch/net/identity.der");
            if file.exists() {
                self.my_id = flock_net::identity::Identity::load_or_create(&crate::net::config_dir()).ok().map(|i| i.id);
            }
        }
        self.my_id.clone()
    }

    /// The left column: the app and the pages (the status card is drawn under it).
    fn sidebar(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let width = ui.available_width();
        let pending = self.setup_pending_count();
        // The app icon, its name and version.
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 46.0), Sense::hover());
        let icon = Rect::from_min_size(rect.min + Vec2::new(8.0, 2.0), Vec2::splat(34.0));
        egui::Image::new((self.icon.id(), icon.size())).corner_radius(9).paint_at(ui, icon);
        let painter = ui.painter();
        painter.text(Pos2::new(icon.right() + 10.0, rect.top() + 6.0), Align2::LEFT_TOP, crate::APP_NAME, bold(15.0), p.text);
        painter.text(Pos2::new(icon.right() + 10.0, rect.top() + 26.0), Align2::LEFT_TOP, env!("CARGO_PKG_VERSION"), mono(11.0), p.muted);

        let mut go = None;
        for (i, (title, pages)) in Page::groups(t).into_iter().enumerate() {
            ui.add_space(if i == 0 { 8.0 } else { 20.0 });
            let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 20.0), Sense::hover());
            ui.painter().text(rect.left_center() + Vec2::new(12.0, 0.0), Align2::LEFT_CENTER, title, bold(12.0), p.muted);
            ui.add_space(4.0);
            for (page, label) in pages {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::click());
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                let on = page == self.page;
                resp.widget_info(|| WidgetInfo::selected(Role::Tab, true, on, label));
                let painter = ui.painter();
                if on {
                    painter.rect_filled(rect, 8, p.card);
                    painter.rect_stroke(rect, 8, Stroke::new(1.0, p.border), StrokeKind::Inside);
                    painter.circle_filled(Pos2::new(rect.left() + 15.0, rect.center().y), 3.0, p.text);
                } else if resp.hovered() {
                    painter.rect_filled(rect, 8, p.text.gamma_multiply(0.05));
                }
                let color = if on { p.text } else { p.text.gamma_multiply(0.78) };
                let font = if on { bold(14.0) } else { FontId::proportional(14.0) };
                painter.text(Pos2::new(rect.left() + 28.0, rect.center().y), Align2::LEFT_CENTER, label, font, color);
                // Linux setup badge on General when missing setup items remain.
                if page == Page::General && pending > 0 {
                    let count = pending.to_string();
                    let galley = painter.layout_no_wrap(count, bold(11.0), Color32::WHITE);
                    let w = (galley.size().x + 10.0).max(18.0);
                    let pill = Rect::from_center_size(Pos2::new(rect.right() - 10.0 - w / 2.0, rect.center().y), Vec2::new(w, 18.0));
                    painter.rect_filled(pill, 9, p.red);
                    painter.galley(pill.center() - galley.size() / 2.0, galley, Color32::WHITE);
                }
                if resp.clicked() {
                    go = Some(page);
                }
            }
        }
        if let Some(page) = go {
            if page != self.page {
                self.stop_recording();
                self.layout_pick = None;
                self.page = page;
            }
        }
    }

    /// Where the mouse is, what the network is doing, and the device.
    fn status_card(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let at = self.mouse_at();
        let net = &self.cfg.network;
        let status = self.net_status.as_ref().filter(|s| s.running);
        let state = NetState::of(&self.cfg, self.net_status.as_ref());
        let net_line = if state == NetState::Off {
            t.status_net_off.to_string()
        } else if net.role == flock_net::Role::Master {
            let up = status.map_or(0, |s| s.peers.iter().filter(|x| x.connected && !x.via_master).count());
            crate::i18n::fill(t.status_master, &[&up.to_string(), &flock_net::MAX_SUBS.to_string()])
        } else {
            match net.peers.first() {
                Some(master) => crate::i18n::fill(t.status_sub, &[&master.name]),
                None => t.status_sub_none.to_string(),
            }
        };
        let (line, here) = match &at {
            MouseAt::Here => (t.status_here.to_string(), true),
            MouseAt::There(name) => (crate::i18n::fill(t.status_there, &[name]), false),
            MouseAt::Controlled(name) => (crate::i18n::fill(t.status_controlled, &[name]), true),
        };
        let device = match &self.status.trigger {
            Some(name) => match self.status.channel {
                Some(c) => format!("{name} · {}", crate::i18n::fill(t.status_channel, &[&c.to_string()])),
                None => name.clone(),
            },
            None => t.searching.to_string(),
        };
        egui::Frame::new()
            .fill(p.card)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(10)
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    // The one lime mark: the mouse is on this computer.
                    let (dot, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                    if here {
                        ui.painter().circle_filled(dot.center(), 6.0, p.accent.gamma_multiply(0.35));
                        ui.painter().circle_filled(dot.center(), 4.0, p.accent);
                    } else {
                        ui.painter().circle_filled(dot.center(), 4.0, p.off);
                    }
                    ui.add(egui::Label::new(RichText::new(line).size(13.0).color(p.text)).wrap());
                });
                ui.label(RichText::new(net_line).size(12.0).color(p.muted));
                ui.label(RichText::new(device).font(mono(11.0)).color(p.muted));
            });
    }

    fn page_body(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        match self.page {
            Page::Connect => self.connect_page(ui, t, p),
            Page::Cross => self.cross_page(ui, t, p),
            Page::Keys => self.keys_page(ui, t, p),
            Page::Clip => self.clip_page(ui, t, p),
            Page::This => self.this_page(ui, t, p),
            Page::Buttons => self.buttons_page(ui, t, p),
            Page::Hotkeys => self.hotkeys_page(ui, t, p),
            Page::Edges => self.edges_page(ui, t, p),
            Page::Devices => self.devices_page(ui, t, p),
            Page::Effects => self.effects_page(ui, t, p),
            Page::General => self.general_page(ui, t, p),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_has_one_fixed_size() {
        let builder = window_builder("x", egui::IconData { rgba: vec![0; 4], width: 1, height: 1 });
        let size = Vec2::from(WINDOW_SIZE);
        assert_eq!(builder.inner_size, Some(size));
        // The smallest and the largest are the size itself, and it cannot be dragged.
        assert_eq!(builder.min_inner_size, Some(size));
        assert_eq!(builder.max_inner_size, Some(size));
        assert_eq!(builder.resizable, Some(false));
        assert_eq!(builder.maximize_button, Some(false));
    }

    #[test]
    fn the_fixed_size_fits_a_small_laptop_screen_and_the_widest_page() {
        assert!(WINDOW_SIZE[0] <= 1280.0 && WINDOW_SIZE[1] <= 800.0, "{WINDOW_SIZE:?}");
        // The page beside the sidebar is wide enough for the 5-cell layout map: each
        // cell at least 100 points (padding, scroll gutter, card margins, map padding).
        let page = WINDOW_SIZE[0] - SIDEBAR_WIDTH - 2.0 * PAGE_PADDING.x - 10.0 - 2.0 * 18.0 - 2.0 * 14.0;
        assert!((page - 4.0 * 10.0) / 5.0 >= 100.0, "{page}");
        // The sidebar holds its three groups and eleven pages without scrolling.
        let sidebar = 46.0 + (8.0 + 24.0) + 2.0 * (20.0 + 24.0) + 11.0 * 34.0 + 24.0 + 90.0;
        assert!(sidebar + 32.0 <= WINDOW_SIZE[1], "{sidebar}");
    }

    #[test]
    fn every_section_header_has_one_height() {
        // Boxes side by side start on one line because every header is this tall.
        assert!(SECTION_HEAD >= 28.0 && SECTION_HEAD <= 40.0);
        assert!(CONTROL_HEIGHT + 2.0 <= SECTION_HEAD, "a button in a header must fit");
    }

    #[test]
    fn every_page_is_in_the_sidebar_once() {
        for t in [&crate::i18n::texts()] {
            let pages: Vec<Page> = Page::groups(t).into_iter().flat_map(|(_, p)| p.into_iter().map(|(page, _)| page)).collect();
            assert_eq!(pages.len(), 11);
            for page in [
                Page::Connect,
                Page::Cross,
                Page::Keys,
                Page::Clip,
                Page::This,
                Page::Buttons,
                Page::Hotkeys,
                Page::Edges,
                Page::Devices,
                Page::Effects,
                Page::General,
            ] {
                assert_eq!(pages.iter().filter(|p| **p == page).count(), 1, "{page:?}");
            }
            for (title, pages) in Page::groups(t) {
                assert!(!title.is_empty() && pages.iter().all(|(_, label)| !label.is_empty()));
            }
        }
        // The groups, in the mockup's order.
        crate::i18n::set_language("ko");
        let t = crate::i18n::texts();
        let groups = Page::groups(t);
        assert_eq!(groups.iter().map(|g| g.0).collect::<Vec<_>>(), ["네트워크", "로지텍", "앱"]);
        assert_eq!(
            groups[0].1.iter().map(|p| p.1).collect::<Vec<_>>(),
            ["연결과 배치", "넘기기", "키보드", "클립보드", "이 컴퓨터"]
        );
        assert_eq!(groups[1].1.iter().map(|p| p.1).collect::<Vec<_>>(), ["버튼 전환", "단축키 전환", "모서리 전환", "장치"]);
        assert_eq!(groups[2].1.iter().map(|p| p.1).collect::<Vec<_>>(), ["효과", "일반"]);
    }

    #[test]
    fn the_palette_keeps_lime_for_the_dot_and_the_role() {
        for dark in [false, true] {
            let p = Palette::new(dark);
            // Marks that are "on" are not lime: switches, primary buttons, raised segments.
            assert_ne!(p.strong, LIME);
            assert_ne!(p.seg_on, LIME);
            assert_ne!(p.on_strong, LIME);
            assert_eq!(p.accent, LIME);
        }
    }

    /// The widest right edge and the lowest bottom edge of any text drawn.
    fn text_extent(shape: &Shape, within: &mut (f32, f32, String)) {
        match shape {
            Shape::Text(t) => {
                let r = t.galley.rect.translate(t.pos.to_vec2());
                if r.right() > within.0 {
                    within.0 = r.right();
                    within.2 = t.galley.text().chars().take(30).collect();
                }
                within.1 = within.1.max(r.bottom());
            }
            Shape::Vec(v) => v.iter().for_each(|s| text_extent(s, within)),
            _ => {}
        }
    }

    fn text_rects(shape: &Shape, out: &mut Vec<(Rect, String)>) {
        match shape {
            Shape::Text(t) => out.push((t.galley.rect.translate(t.pos.to_vec2()), t.galley.text().chars().take(24).collect())),
            Shape::Vec(v) => v.iter().for_each(|s| text_rects(s, out)),
            _ => {}
        }
    }

    fn shape_text_has(shape: &Shape, needle: &str) -> bool {
        match shape {
            Shape::Text(t) => t.galley.text().contains(needle),
            Shape::Vec(v) => v.iter().any(|s| shape_text_has(s, needle)),
            _ => false,
        }
    }

    /// Runs every page for a few frames without a window, to catch a panic or a layout
    /// that asks for the impossible. It points HOME at a throwaway folder (so nothing of
    /// the real settings is touched) and so runs alone:
    /// `cargo test settings_ui -- --ignored every_page_renders`
    #[test]
    #[ignore = "uses its own HOME; run it by name"]
    fn every_page_renders() {
        let home = std::env::temp_dir().join(format!("flockswitch-ui-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::env::set_var("HOME", &home);
        let mut cfg = Config::default();
        cfg.check_updates = false;
        cfg.network.on = true;
        cfg.network.notice_accepted = true;
        for (id, name, side) in [("peer-a", "Laptop", Some(flock_net::Side::Left)), ("peer-b", "ThinkPad", Some(flock_net::Side::Top)), ("peer-c", "Studio PC", None)] {
            cfg.network.peers.push(flock_net::PeerConfig { id: id.into(), name: name.into(), os: flock_net::Os::Windows, side, ..Default::default() });
        }
        cfg.save().unwrap();
        let status: flock_net::Status = serde_json::from_value(serde_json::json!({
            "running": true,
            "me": {"id": "me-id", "name": "Linux PC", "port": 47160, "error": null},
            "discovered": [{"id": "near", "name": "Office Mac", "os": "mac", "addr": "192.168.0.31:47160", "paired": false}],
            "peers": [{"id": "peer-a", "connected": true, "rtt_ms": 3.4}, {"id": "peer-b", "connected": false, "rtt_ms": null}],
            "state": {"mode": "controlling", "peer": "peer-a"},
            "pairing": {"peer_name": "Office Mac", "code": "482917", "stage": "wait"},
            "transfer": {"direction": "send", "kind": "files", "done_bytes": 18000000, "total_bytes": 43000000, "stage": "running", "saved_in": null},
            "layout": [{"id": "me-id", "col": 2, "row": 1}, {"id": "peer-a", "col": 1, "row": 1}, {"id": "peer-b", "col": 1, "row": 0}]
        })).unwrap();
        flock_net::write_status(crate::net::config_dir(), &status).unwrap();

        let ctx = egui::Context::default();
        install_fonts(&ctx);
        install_style(&ctx);
        let mut app = SettingsApp::new(&ctx, Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)), Opening::Default);
        let pages = [Page::Connect, Page::Cross, Page::Keys, Page::Clip, Page::This, Page::Buttons, Page::Hotkeys, Page::Edges, Page::Devices, Page::Effects, Page::General];
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::from(WINDOW_SIZE));
        let mut overlaps: Vec<String> = Vec::new();
        for role_sub in [false, true] {
            if role_sub {
                app.apply(|c| c.network.role = flock_net::Role::Sub);
                app.apply(|c| c.network.peers.truncate(1));
            }
            for lang in ["ko", "en"] {
                crate::i18n::set_language(lang);
                for page in pages {
                    app.page = page;
                    app.layout_pick = Some("peer-a".into());
                    for pair in 0..3 {
                        app.key_pair = pair;
                        for dir in 0..2 {
                            app.key_dir = dir;
                            for frame in 0..3 {
                                let input = egui::RawInput { screen_rect: Some(screen), time: Some(frame as f64 * 0.1), ..Default::default() };
                                let mut out = ctx.run_ui(input, |ui| app.show(ui));
                                out.textures_delta.clear();
                                assert!(!out.shapes.is_empty(), "{page:?} drew nothing");
                                let clash = out.shapes.iter().any(|s| shape_text_has(&s.shape, "clash"));
                                assert!(!clash, "{page:?} (sub: {role_sub}): widget id clash");
                                let mut extent = (0.0, 0.0, String::new());
                                out.shapes.iter().for_each(|s| text_extent(&s.shape, &mut extent));
                                if frame == 2 {
                                    let mut rects = Vec::new();
                                    out.shapes.iter().for_each(|s| text_rects(&s.shape, &mut rects));
                                    assert!(rects.len() > 8, "{page:?}: only {} texts drawn", rects.len());
                                    for (i, (a, ta)) in rects.iter().enumerate() {
                                        for (b, tb) in &rects[i + 1..] {
                                            let overlap = a.intersect(*b);
                                            let small = a.area().min(b.area());
                                            if overlap.is_positive() && small > 0.0 && overlap.area() > 0.25 * small {
                                                overlaps.push(format!("{page:?}/{lang}/sub={role_sub}: {ta:?} over {tb:?}"));
                                            }
                                        }
                                    }
                                }
                                assert!(extent.0 <= WINDOW_SIZE[0] - 6.0, "{page:?} {lang} (sub: {role_sub}): text reaches {} at {:?}", extent.0, extent.2);
                            }
                            if page != Page::Keys {
                                break;
                            }
                        }
                        if page != Page::Keys {
                            break;
                        }
                    }
                }
            }
        }
        app.keys_cmp = true;
        let _ = std::fs::remove_dir_all(&home);
        overlaps.sort();
        overlaps.dedup();
        assert!(overlaps.is_empty(), "{}", overlaps.join("\n"));
    }
}
