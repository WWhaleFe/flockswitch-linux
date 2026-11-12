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
use crate::locate_logic::{self as logic, LocatePath, NoLocate, PortalState};
use crate::setup;
use crate::setup_logic::{self, After, Item, ItemId, State as SetupState};
use crate::shared::{self, DeviceInfo, HapticPreview, StatusFile};
use crate::update::{self, Release};
use crate::window_size::SizeGuard;
use anyhow::{anyhow, Result};
use auto_launch::AutoLaunch;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Margin, Pos2, Rect, Response, RichText, Role,
    Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2, WidgetInfo,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_secs(1);
/// The "Shortcuts" and "Edges" entries next to the buttons on the switching tab.
const SHORTCUTS_TAB: u16 = 0xFFFF;
const EDGES_TAB: u16 = 0xFFFE;

/// The settings window's size to start with and its limits, in points.
/// Tall enough for the General tab with every option of finding the cursor shown, a
/// shortcut another app took and an update error, in Korean and English.
const WINDOW_SIZE: [f32; 2] = [840.0, 1070.0];
const WINDOW_MIN: [f32; 2] = [760.0, 640.0];
const WINDOW_MAX: [f32; 2] = [1200.0, 1300.0];
/// Room left above and below the window when it grows to fit its page (panels, title bar).
const SCREEN_MARGIN: f32 = 90.0;

/// The app icon, scaled by build.rs (WINDOW_ICON there).
const ICON_SIDE: u32 = 96;
const ICON_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/window-icon.rgba"));

/// Which page the window opens on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Opening {
    Default,
    /// The General tab, checking for a new version (the "new version" menu item).
    Update,
    /// The General tab, for the setup check (its menu entry and notification).
    Setup,
}

/// Opens the window. `lock` keeps a second window from opening; a connection to it
/// means "Settings…" was chosen again, so the window comes back to the front (and to
/// the General tab when the connection says "setup" or "update").
pub fn run(lock: std::net::TcpListener, opening: Opening) -> Result<()> {
    if let Ok(cfg) = Config::load_or_create() {
        crate::i18n::set_language(&cfg.language);
    }
    let t = texts();
    let icon = egui::IconData { rgba: ICON_RGBA.to_vec(), width: ICON_SIDE, height: ICON_SIDE };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(window_title(t.settings_title))
            // Matches flockswitch.desktop, so the dock shows the app's name and icon.
            .with_app_id("flockswitch")
            .with_icon(icon)
            .with_inner_size(WINDOW_SIZE)
            .with_min_inner_size(WINDOW_MIN)
            .with_max_inner_size(WINDOW_MAX)
            .with_maximize_button(false)
            .with_active(true),
        ..Default::default()
    };
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
/// titles.
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
        visuals.selection.bg_fill = p.accent.gamma_multiply(0.35);
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

/// Colours for the current light/dark appearance. The accent matches the app icon.
#[derive(Clone, Copy)]
struct Palette {
    bg: Color32,
    card: Color32,
    border: Color32,
    track: Color32,
    hover: Color32,
    text: Color32,
    muted: Color32,
    accent: Color32,
    /// A chosen "Off" segment: neutral gray (Apple's systemGray), so it reads as
    /// switched off rather than as a choice made.
    off: Color32,
    green: Color32,
    amber: Color32,
    red: Color32,
}

impl Palette {
    fn new(dark: bool) -> Palette {
        if dark {
            Palette {
                bg: Color32::from_rgb(0x1C, 0x1C, 0x1F),
                card: Color32::from_rgb(0x2A, 0x2A, 0x2E),
                border: Color32::from_rgb(0x3A, 0x3A, 0x40),
                track: Color32::from_rgb(0x48, 0x48, 0x50),
                hover: Color32::from_rgb(0x33, 0x33, 0x39),
                text: Color32::from_rgb(0xF2, 0xF2, 0xF5),
                muted: Color32::from_rgb(0xA8, 0xA8, 0xB0),
                accent: Color32::from_rgb(0x6E, 0x7B, 0xFF),
                off: Color32::from_rgb(0x8E, 0x8E, 0x93),
                green: Color32::from_rgb(0x30, 0xD1, 0x58),
                amber: Color32::from_rgb(0xFF, 0xB3, 0x40),
                red: Color32::from_rgb(0xFF, 0x5A, 0x50),
            }
        } else {
            Palette {
                bg: Color32::from_rgb(0xF2, 0xF2, 0xF6),
                card: Color32::WHITE,
                border: Color32::from_rgb(0xE2, 0xE2, 0xE8),
                track: Color32::from_rgb(0xD5, 0xD5, 0xDC),
                hover: Color32::from_rgb(0xF4, 0xF4, 0xF8),
                text: Color32::from_rgb(0x1C, 0x1C, 0x1E),
                muted: Color32::from_rgb(0x60, 0x60, 0x68),
                accent: Color32::from_rgb(0x46, 0x55, 0xE8),
                off: Color32::from_rgb(0x8E, 0x8E, 0x93),
                green: Color32::from_rgb(0x1F, 0xA8, 0x45),
                amber: Color32::from_rgb(0xC2, 0x6A, 0x00),
                red: Color32::from_rgb(0xD7, 0x2C, 0x24),
            }
        }
    }
}

fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

/// The pages of the window, one tab each.
#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Switching,
    Devices,
    General,
}

/// One channel list: a button's press, or one gesture direction.
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
    tab: Tab,
    /// The button whose settings are shown, and for the gesture button which gesture.
    button_tab: u16,
    /// Channel whose keyboard shortcut is being recorded.
    recording: Option<u8>,
    gesture_tab: Gesture,
    uninstall: Uninstall,
    cfg: Config,
    devices: Vec<DeviceInfo>,
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
    /// The haptics window, while it is open; the main window waits for it.
    haptics_dialog: Option<HapticsDialog>,
    /// "Reset settings…" was pressed and waits for its confirmation.
    confirm_reset: bool,
    /// A colour being dragged in the picker; saved when the mouse button comes up.
    color_draft: Option<(EffectColor, [u8; 3])>,
    /// Keeps the window's size when it moves to a monitor with another scale.
    size_guard: SizeGuard,
    /// The list of saved settings files, while "Load settings…" is open.
    backup_picker: Option<BackupPicker>,
    /// What the last save or load did: whether it went well, and what to say.
    backup_note: Option<(bool, String)>,
    /// Save found the folder full (`backup::MAX_FILES`): Manage is shown as the way out.
    backup_full: bool,
    /// How many settings files the folder holds, as of the last refresh.
    backup_count: usize,
    /// Renaming and deleting the saved files, while "Manage…" is open.
    backup_manager: Option<BackupManager>,
    /// The tallest page the window has grown to fit (see `grow_to_fit`).
    grown_for: f32,
    /// Whether the window's first size was checked against the monitor.
    fitted_to_screen: bool,
    /// The setup check at the top of the General tab.
    setup: SetupUi,
    /// The tray menu or a notification asked for the General tab while the window was open.
    to_general: Arc<AtomicBool>,
}

/// The setup check: what is missing, and the fixes the user approved.
#[derive(Default)]
struct SetupUi {
    /// What the last check found (None until the first one is done).
    found: Arc<Mutex<Option<Vec<Item>>>>,
    /// What the last check was started with; a change starts another.
    asked: Option<setup::Inputs>,
    checking: Arc<AtomicBool>,
    /// A fix finished: check again.
    stale: Arc<AtomicBool>,
    /// The fixes run from this window and how they went, shown until it closes.
    runs: Arc<Mutex<HashMap<ItemId, SetupRun>>>,
    /// The items whose commands wait for Run or Cancel: one, or all for "Install all".
    confirm: Option<Vec<Item>>,
    /// Whose commands were just copied ("Copied" for a moment).
    copied: Option<(ItemId, Instant)>,
    /// The Korean font installed from here was loaded.
    fonts_reloaded: bool,
    /// The running flockswitch had a device open since this window opened.
    devices_seen: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum SetupRun {
    Running,
    Done(After),
    /// brew ran, but Homebrew does not have the new version yet.
    BrewSame,
    /// Upgraded with brew while no tray app runs: the next start is the new version.
    BrewInstalled,
    /// No administrator rights: the same commands, with sudo, for a terminal.
    NoAdmin(String),
    Failed { code: Option<i32>, tail: Vec<String>, terminal: String },
}

/// What a row of the setup card asks for.
enum SetupAction {
    Install(Item),
    /// Every missing item that can be installed, with one password.
    InstallAll(Vec<Item>),
    Later(Item),
    ShowAgain,
    Copy(ItemId, String),
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

/// The modal haptics window. Its controls change `draft`; nothing reaches the settings
/// file or the mouse until Apply or OK.
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
    Downloaded(std::path::PathBuf),
    /// The download matched its checksum and the new program is being put in place.
    Installing,
    InstallFailed(Release, String),
    /// Installed and the tray app is restarting into it: this window closes, and the
    /// new version opens it again.
    Restarting,
    /// Installed while no tray app runs: the next start is the new version.
    Installed,
}

impl SettingsApp {
    fn new(ctx: &egui::Context, raise: Arc<AtomicBool>, to_general: Arc<AtomicBool>, opening: Opening) -> SettingsApp {
        let autostart = crate::tray::autostart();
        let autostart_on = autostart.as_ref().is_some_and(|a| a.is_enabled().unwrap_or(false));
        let image = egui::ColorImage::from_rgba_unmultiplied([ICON_SIDE as usize; 2], ICON_RGBA);
        let mut app = SettingsApp {
            tab: if opening == Opening::Default { Tab::Switching } else { Tab::General },
            button_tab: BUTTON_WHEEL_MODE,
            recording: None,
            gesture_tab: Gesture::Press,
            uninstall: Uninstall::Idle,
            cfg: Config::load_or_create().unwrap_or_default(),
            devices: Vec::new(),
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
            haptics_dialog: None,
            confirm_reset: false,
            color_draft: None,
            size_guard: SizeGuard::default(),
            backup_picker: None,
            backup_note: None,
            backup_full: false,
            backup_count: 0,
            backup_manager: None,
            grown_for: 0.0,
            fitted_to_screen: false,
            setup: SetupUi::default(),
            to_general,
        };
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

    /// Asks GitHub for the latest version in the background.
    fn check_update(&self, ctx: &egui::Context) {
        let (state, ctx) = (self.update.clone(), ctx.clone());
        *state.lock().unwrap() = UpdateUi::Checking;
        std::thread::spawn(move || {
            let next = match update::latest() {
                Ok(r) if r.is_newer_than_this_build() => UpdateUi::Available(r),
                Ok(_) => UpdateUi::UpToDate,
                Err(e) => {
                    log::info!("update check failed: {e:#}");
                    UpdateUi::Failed(format!("{e:#}"))
                }
            };
            *state.lock().unwrap() = next;
            ctx.request_repaint();
        });
    }

    /// Downloads the new version into Downloads and shows it in the file manager.
    fn download_update(&self, ctx: &egui::Context, release: Release) {
        let Some((name, url, sums)) = release.asset.clone() else {
            update::open_url(&release.page);
            return;
        };
        let (state, ctx) = (self.update.clone(), ctx.clone());
        *state.lock().unwrap() = UpdateUi::Downloading;
        std::thread::spawn(move || {
            let next = match update::download(&name, &url, &sums) {
                Ok(path) => {
                    update::reveal(&path);
                    UpdateUi::Downloaded(path)
                }
                Err(e) => UpdateUi::Failed(format!("{e:#}")),
            };
            *state.lock().unwrap() = next;
            ctx.request_repaint();
        });
    }

    /// "Update now": downloads, checks and installs the new version, then has the tray
    /// app restart into it. Where this copy cannot replace itself it downloads instead.
    fn install_update(&self, ctx: &egui::Context, release: Release) {
        let update::InstallKind::SelfReplace(exe) = update::install_kind() else {
            return self.download_update(ctx, release);
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
                Ok(()) if crate::main_instance_running() => match update::start_new_version(&exe, &["tray", "--settings"]) {
                    Ok(()) => UpdateUi::Restarting,
                    Err(e) => {
                        log::warn!("update {}: installed, but cannot start it: {e:#}", release.version);
                        UpdateUi::Installed
                    }
                },
                Ok(()) => UpdateUi::Installed,
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
    /// also reflects changes made from the tray menu or by switching computers.
    fn refresh(&mut self) {
        self.last_refresh = Instant::now();
        if let Ok(cfg) = Config::load_or_create() {
            crate::i18n::set_language(&cfg.language);
            self.cfg = cfg;
        }
        self.status = shared::load_status_file();
        self.devices = shared::load_catalog();
        self.backup_count = backup::list(&backup::folder()).len();
        if self.backup_count < backup::MAX_FILES {
            self.backup_full = false;
        }
        for d in &mut self.devices {
            d.online = self.status.online.iter().any(|k| k == &d.key);
        }
    }

    /// Applies one change on top of the file's current content, validates, saves.
    fn apply(&mut self, change: impl FnOnce(&mut Config)) {
        let mut next = Config::load_or_create().unwrap_or_else(|_| self.cfg.clone());
        change(&mut next);
        match next.validate().and_then(|_| next.save()) {
            Ok(()) => {
                crate::i18n::set_language(&next.language);
                self.cfg = next;
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// The master mouse's key. With "auto" and the mouse away (so the engine has not
    /// picked it yet), assume the first known mouse with the button, as auto would.
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
        let ctx = ui.ctx().clone();
        let t = texts();
        if self.title != t.settings_title {
            self.title = t.settings_title;
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(window_title(t.settings_title)));
        }
        keep_size(&ctx, &mut self.size_guard, WINDOW_MIN, WINDOW_MAX);
        let p = Palette::new(ui.visuals().dark_mode);
        let panel = egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(18, 12));
        // After uninstalling, show only the result: re-reading the settings now would
        // create the deleted settings folder again.
        if self.uninstall_progress() {
            ctx.request_repaint_after(Duration::from_millis(300));
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
        ctx.request_repaint_after(REFRESH);
        self.setup_tick(&ctx);
        if self.to_general.swap(false, Ordering::Relaxed) {
            self.tab = Tab::General;
            self.stop_recording();
        }
        if let Some(n) = self.recording {
            self.capture_shortcut(&ctx, t, n);
        }

        // While the haptics window is open this one cannot be closed or used, and
        // coming back to it hands the focus straight on.
        let modal = self.haptics_dialog.is_some();
        if modal {
            self.backup_picker = None;
            self.backup_manager = None;
        }
        if modal {
            if ctx.input(|i| i.viewport().close_requested()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
            if ctx.input(|i| i.focused) {
                ctx.send_viewport_cmd_to(haptics_viewport(), egui::ViewportCommand::Focus);
            }
        }

        // The header and the tabs stay in place; only the page below them scrolls.
        egui::CentralPanel::default().frame(panel).show(ui, |ui| {
            if modal {
                ui.disable();
            }
            self.header(ui, t, p);
            if self.status.hid_denied {
                // The setup check on the General tab installs the rule.
                let open = (self.tab != Tab::General).then_some(t.setup_open);
                if warning(ui, p, t.hid_denied_title, t.hid_denied, open) {
                    self.tab = Tab::General;
                    self.stop_recording();
                }
            }
            ui.add_space(12.0);
            let tabs = [
                (Tab::Switching, t.section_switching),
                (Tab::Devices, t.section_devices),
                (Tab::General, t.section_general),
            ];
            let labels: Vec<String> = tabs.iter().map(|(_, l)| l.to_string()).collect();
            if let Some(i) = segments(ui, p, "tabs", &labels, Role::Tab, SEG_NORMAL, None, None, |i| tabs[i].0 == self.tab) {
                self.tab = tabs[i].0;
                self.stop_recording();
            }
            // A page taller than the window makes it grow once, as far as the screen allows;
            // what still does not fit scrolls.
            let page = egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                {
                    if let Some(err) = self.error.clone() {
                        ui.add_space(10.0);
                        egui::Frame::new()
                            .fill(p.red.gamma_multiply(0.12))
                            .stroke(Stroke::new(1.0, p.red.gamma_multiply(0.6)))
                            .corner_radius(8)
                            .inner_margin(Margin::symmetric(12, 8))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.label(RichText::new(err).color(p.red));
                            });
                    }
                    match self.tab {
                        Tab::Switching => self.switching_section(ui, t, p),
                        Tab::Devices => {
                            self.master_section(ui, t, p);
                            self.group_section(ui, t, p);
                        }
                        Tab::General => {
                            self.setup_card(ui, t, p);
                            two_columns(ui, |left, right| {
                                // As on the Mac: haptics under finding the cursor, the
                                // language at the top on the right.
                                notice(left, p, t);
                                self.locate_section(left, t, p);
                                self.haptics_summary(left, t, p);
                                self.preferences_card(right, t, p);
                                self.update_section(right, t, p);
                                self.report_section(right, t, p);
                                self.backup_section(right, t, p);
                                self.maintenance_card(right, t, p);
                            });
                        }
                    }

                    ui.add_space(12.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new(format!("{} · flockswitch {}", t.applies_immediately, env!("CARGO_PKG_VERSION")))
                                .size(12.0)
                                .color(p.muted),
                        );
                    });
                }
            });
            self.grow_to_fit(&ctx, page.content_size.y, page.inner_rect.height());
        });
        self.haptics_window(&ctx, t);
        self.backup_picker_ui(&ctx, t, p);
        self.backup_manager_ui(&ctx, t, p);
        self.setup_confirm_ui(&ctx, p);
    }
}

impl SettingsApp {
    /// When the page shown is taller than the view, makes the window taller, once per
    /// page height: up to the monitor's height less a margin, and `WINDOW_MAX`. It never
    /// makes the window smaller, nor grows again for a page it has grown for before (so a
    /// window the user made smaller stays so). The new size is the one `keep_size` keeps.
    /// At the start, a window taller than the monitor allows is made to fit it.
    fn grow_to_fit(&mut self, ctx: &egui::Context, content: f32, view: f32) {
        // The first frames lay out before the fonts and the window have settled.
        if ctx.cumulative_frame_nr() < 3 {
            return;
        }
        let (inner, monitor) = ctx.input(|i| (i.viewport().inner_rect.map(|r| r.size()), i.viewport().monitor_size));
        // Without the monitor's size there is no knowing how far it can grow.
        let (Some(inner), Some(monitor)) = (inner, monitor) else { return };
        let limit = (monitor.y - SCREEN_MARGIN).clamp(WINDOW_MIN[1], WINDOW_MAX[1]);
        if !self.fitted_to_screen {
            self.fitted_to_screen = true;
            if inner.y > limit + 1.0 {
                log::info!("window: {:.0} tall on a monitor {:.0} tall; down to {limit:.0}", inner.y, monitor.y);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(inner.x, limit)));
                self.size_guard.remember([inner.x, limit]);
                // The page that does not fit now is not grown for again.
                self.grown_for = content;
                return;
            }
        }
        if content <= view + 1.0 || content <= self.grown_for + 1.0 {
            return;
        }
        self.grown_for = content;
        let want = (inner.y + (content - view).ceil() + 2.0).min(limit);
        if want <= inner.y + 1.0 {
            return;
        }
        log::info!("window: {:.0} tall for a page of {content:.0} in {view:.0}; growing to {want:.0}", inner.y);
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(inner.x, want)));
        self.size_guard.remember([inner.x, want]);
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

    /// App icon, name, what the engine is doing and the last switch.
    fn header(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 48.0), Sense::hover());
        let painter = ui.painter();
        let icon = Rect::from_min_size(rect.min + Vec2::new(0.0, 2.0), Vec2::splat(44.0));
        let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        painter.image(self.icon.id(), icon, uv, Color32::WHITE);

        let x = icon.right() + 12.0;
        painter.text(Pos2::new(x, rect.top() + 3.0), Align2::LEFT_TOP, "flockswitch", bold(19.0), p.text);

        let (color, line) = if !self.cfg.enabled {
            (p.muted, t.paused.to_string())
        } else if let Some(trigger) = &self.status.trigger {
            (p.green, format!("{} · {trigger}", t.running))
        } else {
            (p.amber, t.searching.to_string())
        };
        let y = rect.top() + 35.0;
        painter.circle_filled(Pos2::new(x + 5.0, y), 4.5, color);
        let status = painter.text(Pos2::new(x + 16.0, y), Align2::LEFT_CENTER, line, FontId::proportional(14.0), p.text);

        if let Some(last) = &self.status.last_switch {
            let label = format!("{} · {} {last}", t.last_switch, t.channel);
            let galley = painter.layout_no_wrap(label, FontId::proportional(12.0), p.muted);
            // On the same line when it fits, otherwise left out (the tray menu shows it too).
            if status.right() + 16.0 + galley.size().x + 16.0 <= rect.right() {
                let pill = Rect::from_min_size(
                    Pos2::new(rect.right() - galley.size().x - 16.0, y - 11.0),
                    Vec2::new(galley.size().x + 16.0, 22.0),
                );
                painter.rect_filled(pill, 11, p.card);
                painter.rect_stroke(pill, 11, Stroke::new(1.0, p.border), StrokeKind::Inside);
                painter.galley(pill.center() - galley.size() / 2.0, galley, p.muted);
            }
        }
    }

    /// The master switch, then the settings of one button at a time.
    fn switching_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        ui.add_space(12.0);
        info_box(ui, p, t.switching_help);
        ui.add_space(10.0);
        card(ui, p, |ui| {
            let mut enabled = self.cfg.enabled;
            if switch_row(ui, p, t.enabled, None, &mut enabled) {
                self.apply(|c| c.enabled = enabled);
            }
        });

        let buttons = [
            (BUTTON_WHEEL_MODE, t.button_wheel_mode_short, t.button_wheel_mode),
            (BUTTON_GESTURE, t.button_gesture_short, t.button_gesture),
            (BUTTON_MIDDLE, t.button_middle_short, t.button_middle),
        ];
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new(t.section_buttons).font(bold(13.0)).color(p.muted));
        });
        ui.add_space(4.0);
        let tab_ids: Vec<u16> = buttons.iter().map(|b| b.0).chain([SHORTCUTS_TAB, EDGES_TAB]).collect();
        let labels: Vec<String> = buttons
            .iter()
            .map(|(cid, short, _)| (*short, self.cfg.button(*cid).on))
            .chain([(t.hotkeys_short, self.cfg.hotkeys.on), (t.edges_short, self.cfg.edges.any())])
            .map(|(short, on)| if on { format!("{short} ✓") } else { short.to_string() })
            .collect();
        if let Some(i) = segments(ui, p, "button-tabs", &labels, Role::Tab, SEG_NORMAL, None, None, |i| tab_ids[i] == self.button_tab) {
            self.button_tab = tab_ids[i];
            self.stop_recording();
        }
        if self.button_tab == SHORTCUTS_TAB {
            ui.add_space(8.0);
            card(ui, p, |ui| self.shortcut_rows(ui, t, p));
            return;
        }
        if self.button_tab == EDGES_TAB {
            ui.add_space(8.0);
            card(ui, p, |ui| self.edge_rows(ui, t, p));
            return;
        }
        let (cid, _, label) = buttons.into_iter().find(|b| b.0 == self.button_tab).unwrap_or(buttons[0]);

        ui.add_space(8.0);
        card(ui, p, |ui| {
            // Only known while the master mouse is on this computer. The button is
            // turned off with the "Off" segment of its channel row.
            let missing = self.status.trigger.is_some() && !self.status.buttons.contains(&cid);
            title_row(ui, p, label, missing.then_some(t.not_on_mouse));
            divider(ui, p);
            if cid == BUTTON_GESTURE {
                self.gesture_rows(ui, t, p, cid);
            } else {
                self.action_rows(ui, t, p, Slot::Press(cid));
            }
            if cid == BUTTON_MIDDLE {
                hint(ui, p, t.buttons_hint);
            }
            ui.add_space(10.0);
        });
    }

    /// Keyboard shortcuts on/off and one shortcut per channel.
    fn shortcut_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let mut on = self.cfg.hotkeys.on;
        let row = ui.cursor().min;
        if switch_row(ui, p, t.hotkeys_title, None, &mut on) {
            self.apply(|c| c.hotkeys.on = on);
            self.stop_recording();
        }
        label_info(ui, p, row, 40.0, t.hotkeys_title, t.hotkey_help);
        hint(ui, p, t.hotkeys_hint);
        if crate::edge_wayland::session_is_wayland() && self.cfg.hotkeys.on {
            // Wayland: registered through the system's shortcut service; how that stands.
            ui.add_space(8.0);
            self.portal_box(ui, t, p, false);
        }
        if !self.cfg.hotkeys.on {
            ui.add_space(10.0);
            return;
        }
        ui.add_space(6.0);
        divider(ui, p);
        for n in 1..=3u8 {
            self.shortcut_row(ui, t, p, n, &format!("{} {n}", t.channel), None);
        }
        ui.add_space(10.0);
    }

    /// One recordable shortcut: its slot `n` (channel, or LOCATE), the keys and buttons
    /// to change or clear them, and an "i" after the label explaining `info`.
    fn shortcut_row(&mut self, ui: &mut Ui, t: &Texts, p: Palette, n: u8, label: &str, info: Option<&str>) {
        let width = (ui.available_width() * 0.66).min(300.0);
        let (mut start, mut clear) = (false, false);
        let key = self.cfg.shortcut(n).to_string();
        let recording = self.recording == Some(n);
        let failed = !recording && !key.is_empty() && self.status.hotkeys_failed.contains(&n);
        let row = ui.cursor().min;
        let (shown, color) = if recording {
            (t.hotkey_press.to_string(), p.accent)
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
            label_info(ui, p, row, ROW_SMALL, label, text);
        }
        if failed {
            hint(ui, p, t.hotkey_taken);
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
        let with_commands = |text: &str| format!("{text}\n\n{}", logic::fallback_commands(&exe).join("\n"));
        match PortalState::parse(&self.status.shortcuts_portal) {
            Some(PortalState::Bound(version)) => {
                if compact {
                    return;
                }
                info_box(ui, p, &t.portal_bound.replace("{desktop}", &desktop_name()));
                if version >= 2 {
                    ui.add_space(6.0);
                    if button(ui, p, t.portal_change, ButtonKind::Plain).clicked() {
                        shared::request_configure_shortcuts();
                    }
                }
            }
            Some(PortalState::Asking) => info_box(ui, p, t.portal_asking),
            Some(PortalState::Denied) => error_box(ui, p, &with_commands(t.portal_denied)),
            Some(PortalState::Missing) => info_box(ui, p, &with_commands(t.portal_missing)),
            Some(PortalState::NoAppId) => {
                error_box(ui, p, &with_commands(t.portal_no_app_id));
                if self.setup_item(ItemId::MenuEntry).is_some_and(|i| i.actionable()) {
                    ui.add_space(6.0);
                    if button(ui, p, t.setup_open, ButtonKind::Primary).clicked() {
                        self.tab = Tab::General;
                        self.stop_recording();
                    }
                }
            }
            Some(PortalState::Error) => error_box(ui, p, &with_commands(t.portal_error)),
            // Nothing to register yet (just turned on: the running app follows within a second).
            Some(PortalState::Off) => {}
            // No running flockswitch of this version has said anything yet.
            None => {
                if !compact {
                    info_box(ui, p, t.portal_waiting);
                }
            }
        }
        ui.add_space(4.0);
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
            Some(b) if logic::trigger_matches(key, &b.trigger) => hint(ui, p, &t.portal_trigger_same.replace("{trigger}", &logic::pretty_trigger(&b.trigger))),
            Some(b) => amber_hint(ui, p, &t.portal_trigger_other.replace("{trigger}", &logic::pretty_trigger(&b.trigger))),
        }
    }

    /// Switching at the screen edge, in two columns so the page fits the window: where
    /// each side goes on the left, how the wait looks and feels on the right.
    fn edge_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        if !crate::edge::available() {
            // Wayland: what the running flockswitch found it can use here.
            let (text, problem) = edge_backend_text(t, &self.status.edge_backend, &self.status.edge_note, self.cfg.edges.require_control);
            ui.add_space(8.0);
            if problem {
                error_box(ui, p, &text);
            } else {
                info_box(ui, p, &text);
            }
            // The desktop portal is missing where installing it helps: the setup check
            // on the General tab installs it.
            if problem && self.setup_item(ItemId::Portal).is_some_and(|i| i.actionable()) {
                ui.add_space(8.0);
                if button(ui, p, t.setup_open, ButtonKind::Primary).clicked() {
                    self.tab = Tab::General;
                    self.stop_recording();
                }
            }
            if self.status.edge_backend == "none" {
                ui.add_space(10.0);
                return;
            }
            ui.add_space(4.0);
        }
        // No on/off switch: an edge is on when it has a channel, off on "Off".
        title_row(ui, p, t.edges_title, None);
        hint(ui, p, t.edges_hint);
        ui.add_space(6.0);
        divider(ui, p);
        two_columns(ui, |left, right| {
            self.edge_sides(left, t, p);
            self.edge_wait(right, t, p);
        });
        ui.add_space(8.0);
    }

    /// The channel of each side, how long the cursor must rest there, and its return.
    fn edge_sides(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let here = self.status.channel.filter(|c| (1..=3).contains(c));
        let labels: Vec<String> = [t.off.to_string()].into_iter().chain((1..=3).map(|n| n.to_string())).collect();
        let width = (ui.available_width() * 0.66).min(260.0);
        let sides = [(Edge::Left, t.edge_left), (Edge::Right, t.edge_right), (Edge::Top, t.edge_top), (Edge::Bottom, t.edge_bottom)];
        for (edge, label) in sides {
            let current = self.cfg.edges.channel(edge);
            let mut clicked = None;
            labeled_row(ui, p, label, width, ROW_SMALL, |ui| {
                // Segment 0 is "off"; this computer's own channel is shown but locked.
                clicked = segments(ui, p, &format!("edge-{edge:?}"), &labels, Role::RadioButton, SEG_SMALL, here.map(|c| c as usize), Some(0), |i| {
                    i as u8 == current
                });
                false
            });
            if let Some(n) = clicked.map(|i| i as u8).filter(|&n| n != current) {
                self.apply(|c| {
                    *c.edges.channel_mut(edge) = n;
                    c.edges.on = c.edges.any();
                });
            }
        }
        if let Some(c) = here {
            hint(ui, p, &format!("{} {c}: {}", t.channel, t.here_channel));
        }
        ui.add_space(4.0);
        divider(ui, p);
        let choices: Vec<(u64, String)> = [(300, "0.3"), (500, "0.5"), (1000, "1"), (2000, "2")]
            .into_iter()
            .map(|(ms, text)| (ms, format!("{text}{}", t.seconds_short)))
            .collect();
        let choices: Vec<(u64, &str)> = choices.iter().map(|(ms, s)| (*ms, s.as_str())).collect();
        let mut dwell = self.cfg.edges.dwell_ms;
        if labeled_row(ui, p, t.edge_dwell, width, ROW_SMALL, |ui| segmented(ui, p, "edge-dwell", &mut dwell, &choices, SEG_SMALL)) {
            self.apply(|c| c.edges.dwell_ms = dwell);
        }
        divider(ui, p);
        let mut place = self.cfg.edges.place_cursor;
        if switch_row(ui, p, t.edge_place, None, &mut place) {
            self.apply(|c| c.edges.place_cursor = place);
        }
        hint(ui, p, t.edges_corner_hint);
    }

    /// What it takes to start the wait, and what shows while it runs.
    fn edge_wait(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let mut control = self.cfg.edges.require_control;
        if switch_row(ui, p, t.edge_require_control, None, &mut control) {
            self.apply(|c| c.edges.require_control = control);
        }
        hint(ui, p, t.edge_require_control_hint);
        ui.add_space(6.0);
        divider(ui, p);
        self.edge_anim_rows(ui, t, p);
    }

    /// Which countdown is drawn at the edge, and how it drains where that can be chosen.
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
                crate::edge::preview(&self.cfg.edges);
            } else {
                crate::edge_wayland::preview(&self.cfg.edges);
            }
        }
        // Random draws an effect with that effect's own settings; it has none to set.
        if self.cfg.edges.anim_style == EdgeAnimStyle::Random {
            hint(ui, p, t.edge_random_hint);
            ui.add_space(4.0);
            return;
        }
        // The rest are the chosen effect's own: switching effect shows its values.
        let style = self.cfg.edges.anim_style;
        let Some(effect) = self.cfg.edges.chosen().cloned() else { return };
        if style.can_shrink() {
            let mut shrink = effect.shrink;
            if switch_row(ui, p, t.edge_anim_shrink, None, &mut shrink) {
                self.apply_effect(|e| e.shrink = shrink);
            }
        }
        if style.has_direction() && !(style.can_shrink() && effect.shrink) {
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

    /// Press and the four hold-and-move gestures, one at a time.
    fn gesture_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette, cid: u16) {
        let gestures = [
            (Gesture::Press, t.gesture_press),
            (Gesture::Up, "↑"),
            (Gesture::Down, "↓"),
            (Gesture::Left, "←"),
            (Gesture::Right, "→"),
        ];
        let moves = self.cfg.button(cid).moves.clone();
        let labels: Vec<String> = gestures
            .iter()
            .map(|(g, l)| if moves.get(*g).is_some_and(|m| m.on) { format!("{l} ✓") } else { l.to_string() })
            .collect();
        ui.add_space(10.0);
        if let Some(i) = segments(ui, p, "gesture-tabs", &labels, Role::Tab, SEG_SMALL, None, None, |i| gestures[i].0 == self.gesture_tab) {
            self.gesture_tab = gestures[i].0;
        }
        hint(ui, p, t.gesture_hint);
        let g = self.gesture_tab;
        let slot = if g == Gesture::Press { Slot::Press(cid) } else { Slot::Move(cid, g) };
        self.action_rows(ui, t, p, slot);
    }

    /// 이동할 채널 of one slot: "Off", or the one channel a press sends everything to.
    /// Settings are kept per computer, so this computer's own channel is shown but
    /// cannot be chosen. "Off" turns the button (or the gesture direction) off.
    fn action_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette, slot: Slot) {
        let channels = slot_channels(&self.cfg, slot);
        let is_on = match slot {
            Slot::Press(cid) => self.cfg.button(cid).on,
            Slot::Move(cid, g) => self.cfg.button(cid).moves.get(g).is_some_and(|m| m.on),
        };
        let here = self.status.channel.filter(|c| (1..=3).contains(c));
        // Shown as chosen: where a press goes from here (older settings may list several).
        let target = match here {
            Some(c) if is_on => pick(&channels, c, 3),
            None if is_on => channels.first().copied(),
            _ => None,
        };
        let labels: Vec<String> = [t.off.to_string()].into_iter().chain((1..=3).map(|n| n.to_string())).collect();
        let seg_width = (ui.available_width() * 0.6).min(260.0);
        let mut clicked = None;
        ui.add_space(2.0);
        labeled_row(ui, p, t.channels, seg_width, ROW_SMALL, |ui| {
            // Segment 0 is "Off"; this computer's own channel is shown but locked.
            let locked = here.map(|c| c as usize);
            clicked = segments(ui, p, &format!("channels-{slot:?}"), &labels, Role::RadioButton, SEG_SMALL, locked, Some(0), |i| {
                if i == 0 { !is_on } else { target == Some(i as u8) }
            });
            false
        });
        if let Some(c) = here {
            hint(ui, p, &format!("{} {c}: {}", t.channel, t.here_channel));
        }
        let Some(i) = clicked else { return };
        if i == 0 {
            if !is_on {
                return;
            }
            match slot {
                Slot::Press(cid) if self.cfg.triggers() == [cid] => self.error = Some(t.need_one_button.into()),
                Slot::Press(cid) => self.apply(|c| c.button_mut(cid).on = false),
                Slot::Move(cid, g) => self.apply(|c| {
                    if let Some(m) = c.button_mut(cid).moves.get_mut(g) {
                        m.on = false;
                    }
                }),
            }
            return;
        }
        let n = i as u8;
        if is_on && channels == [n] {
            return;
        }
        self.apply(|c| {
            match slot {
                Slot::Press(cid) => c.button_mut(cid).on = true,
                Slot::Move(cid, g) => {
                    if let Some(m) = c.button_mut(cid).moves.get_mut(g) {
                        m.on = true;
                    }
                }
            }
            if let Some(channels) = slot_channels_mut(c, slot) {
                *channels = vec![n];
            }
        });
    }

    fn master_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        section(ui, p, t.master, |ui| {
            let auto = self.cfg.trigger_device.eq_ignore_ascii_case("auto");
            let now = self.status.trigger.as_ref().map(|name| format!("{} {name}", t.current));
            let row = Row { title: t.master_auto, subtitle: now.as_deref(), badge: None };
            if choice_row(ui, p, Mark::Radio, auto, &row).clicked() && !auto {
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
                if choice_row(ui, p, Mark::Radio, chosen, &row).clicked() && !chosen {
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
            if choice_row(ui, p, Mark::Check, all, &row).clicked() {
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
                if choice_row(ui, p, Mark::Check, on, &row).clicked() {
                    let key = d.key.clone();
                    let keys = keys.clone();
                    self.apply(|c| c.toggle_follow(&keys, &key));
                }
            }
        });
    }

    /// Haptics in one line, with the button that opens their window.
    fn haptics_summary(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let h = &self.cfg.haptics;
        let state = if h.on_switch || h.on_edge { t.haptics_state_on } else { t.haptics_state_off };
        let strength = if h.intensity == 0 { t.haptics_follow.to_string() } else { format!("{}%", h.intensity) };
        let line = format!("{state} · {} {strength}", t.haptics_intensity);
        let mut open = false;
        section(ui, p, t.section_haptics, |ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_SMALL), Sense::hover());
            ui.painter().text(Pos2::new(rect.left(), rect.center().y), Align2::LEFT_CENTER, &line, FontId::proportional(14.0), p.text);
            let width = button_width(ui, t.haptics_open, ButtonKind::Plain);
            let area = Rect::from_min_max(
                Pos2::new(rect.right() - width, rect.center().y - 15.0),
                Pos2::new(rect.right(), rect.center().y + 15.0),
            );
            ui.new_child(UiBuilder::new().max_rect(area)).scope(|ui| {
                open = button(ui, p, t.haptics_open, ButtonKind::Plain).clicked();
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

    /// The haptics window, a real window of its own, while it is open.
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

    /// Highlighting the cursor: on/off, the effect, and what brings it up.
    fn locate_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        section(ui, p, t.section_locate, |ui| {
            // Wayland: what brings the highlight up here, and whether it can be drawn.
            let wayland = !crate::edge::available();
            if wayland {
                ui.add_space(8.0);
                let exe = crate::tray::launch_path().map(|p| p.display().to_string()).unwrap_or_else(|| "flockswitch".into());
                let how = t.locate_wayland_how.replace("{desktop}", &desktop_name()).replace("{command}", &format!("{exe} locate"));
                info_box(ui, p, &how);
                match wayland_locate_path() {
                    LocatePath::None(NoLocate::Misaligned) => {
                        ui.add_space(6.0);
                        error_box(ui, p, t.locate_none_misaligned);
                    }
                    LocatePath::None(_) => {
                        ui.add_space(6.0);
                        error_box(ui, p, t.locate_none_nothing);
                    }
                    _ => {}
                }
                ui.add_space(4.0);
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
                ui.add_space(6.0);
                hint(ui, p, t.locate_shake_x11);
                ui.add_space(10.0);
                return;
            }
            let mut on = self.cfg.locate.on_shake;
            if switch_row(ui, p, t.locate_shake, None, &mut on) {
                self.apply(|c| c.locate.on_shake = on);
            }
            if self.cfg.locate.on_shake {
                self.shake_rows(ui, t, p);
            }
        });
    }

    /// How long the mouse must be shaken, and the key to hold meanwhile.
    fn shake_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let width = (ui.available_width() * 0.66).min(260.0);
        let times: Vec<(u64, String)> =
            SHAKE_TIMES.iter().map(|&ms| (ms, format!("{}{}", ms as f64 / 1000.0, t.seconds_short))).collect();
        let times: Vec<(u64, &str)> = times.iter().map(|(ms, s)| (*ms, s.as_str())).collect();
        let mut time = self.cfg.locate.shake_time_ms;
        if labeled_row(ui, p, t.locate_shake_time, width, ROW_SMALL, |ui| segmented(ui, p, "shake-time", &mut time, &times, SEG_SMALL)) {
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
        ui.add_space(12.0);
        card(ui, p, |ui| {
            let mut language = self.cfg.language.clone();
            let choices = [("ko", "한국어"), ("en", "English")];
            let width = (ui.available_width() * 0.62).min(250.0);
            let mut chosen = language.as_str();
            if labeled_row(ui, p, t.language, width, ROW_SMALL, |ui| segmented(ui, p, "language", &mut chosen, &choices, SEG_SMALL)) {
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
        ui.add_space(12.0);
        card(ui, p, |ui| {
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

    /// Current version, the new one if any, and the automatic check and install.
    fn update_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let state = self.update.lock().unwrap().clone();
        let ctx = ui.ctx().clone();
        let kind = update::install_kind();
        // A Homebrew copy: brew updates it once approved (the setup check's item).
        let brew_item = self.setup_item(ItemId::BrewUpdate).filter(Item::actionable);
        let brew_running = self.setup.runs.lock().unwrap().get(&ItemId::BrewUpdate) == Some(&SetupRun::Running);
        section(ui, p, t.section_update, |ui| {
            ui.add_space(10.0);
            let (status, color) = match &state {
                UpdateUi::Available(_) if brew_running => (t.update_installing.to_string(), p.muted),
                UpdateUi::Idle => (String::new(), p.muted),
                UpdateUi::Checking => (t.update_checking.to_string(), p.muted),
                UpdateUi::UpToDate => (t.update_up_to_date.to_string(), p.green),
                UpdateUi::Available(r) => (format!("{} {}", t.update_available, r.version), p.accent),
                UpdateUi::Failed(_) => (t.update_failed.to_string(), p.amber),
                UpdateUi::Downloading => (t.update_downloading.to_string(), p.muted),
                UpdateUi::Downloaded(_) => (t.update_downloaded.to_string(), p.green),
                UpdateUi::Installing => (t.update_installing.to_string(), p.muted),
                UpdateUi::InstallFailed(..) => (t.update_install_failed.to_string(), p.amber),
                UpdateUi::Restarting => (t.update_restarting.to_string(), p.green),
                UpdateUi::Installed => (t.update_installed.to_string(), p.green),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} {}", t.update_current, env!("CARGO_PKG_VERSION"))).size(14.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(status).size(13.0).color(color));
                });
            });
            if let UpdateUi::Failed(why) | UpdateUi::InstallFailed(_, why) = &state {
                hint(ui, p, why);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                match &state {
                    UpdateUi::Available(r) | UpdateUi::InstallFailed(r, _) => {
                        // Homebrew updates its own copy; a folder that cannot be written
                        // gets the file in Downloads.
                        match kind {
                            update::InstallKind::SelfReplace(_) => {
                                if button(ui, p, t.update_now, ButtonKind::Primary).clicked() {
                                    self.install_update(&ctx, r.clone());
                                }
                            }
                            update::InstallKind::DownloadOnly => {
                                if button(ui, p, t.update_download, ButtonKind::Primary).clicked() {
                                    self.download_update(&ctx, r.clone());
                                }
                            }
                            update::InstallKind::Homebrew => {
                                if let Some(item) = brew_item.clone() {
                                    ui.add_enabled_ui(!brew_running, |ui| {
                                        if button(ui, p, t.update_now, ButtonKind::Primary).clicked() {
                                            self.setup.confirm = Some(vec![item]);
                                        }
                                    });
                                }
                            }
                        }
                        if button(ui, p, t.update_page, ButtonKind::Plain).clicked() {
                            update::open_url(&r.page);
                        }
                    }
                    UpdateUi::Downloaded(path) => {
                        if button(ui, p, t.update_show, ButtonKind::Plain).clicked() {
                            update::reveal(path);
                        }
                    }
                    UpdateUi::Checking | UpdateUi::Downloading | UpdateUi::Installing | UpdateUi::Restarting | UpdateUi::Installed => {}
                    _ => {
                        if button(ui, p, t.update_check, ButtonKind::Plain).clicked() {
                            self.check_update(&ctx);
                        }
                        if button(ui, p, t.update_page, ButtonKind::Plain).clicked() {
                            update::open_url(update::RELEASES_PAGE);
                        }
                    }
                }
            });
            ui.add_space(6.0);
            divider(ui, p);
            let mut on = self.cfg.check_updates;
            let row = ui.cursor().min;
            if switch_row(ui, p, t.update_auto, None, &mut on) {
                self.apply(|c| c.check_updates = on);
            }
            label_info(ui, p, row, 40.0, t.update_auto, t.update_auto_hint);
            divider(ui, p);
            // "Update now" above installs whatever this says; it only governs the
            // installs the daily check makes by itself.
            let mut on = self.cfg.auto_update;
            let row = ui.cursor().min;
            if switch_row(ui, p, t.update_install_auto, None, &mut on) {
                self.apply(|c| c.auto_update = on);
            }
            match kind {
                update::InstallKind::SelfReplace(_) => {
                    label_info(ui, p, row, 40.0, t.update_install_auto, t.update_install_hint);
                    ui.add_space(4.0);
                }
                update::InstallKind::Homebrew => hint(ui, p, t.update_brew),
                update::InstallKind::DownloadOnly => hint(ui, p, t.update_download_only),
            }
            ui.add_space(6.0);
        });
    }

    /// The developer's address, with a ready-made mail and a GitHub link.
    fn report_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let copied = self.copied_at.is_some_and(|at| at.elapsed() < Duration::from_secs(2));
        section_with_info(ui, p, t.section_report, Some(t.report_desc), |ui| {
            ui.add_space(10.0);
            ui.label(RichText::new(update::REPORT_EMAIL).font(bold(14.0)).color(p.text));
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                if button(ui, p, t.report_mail, ButtonKind::Primary).clicked() {
                    let body = update::report_body(&self.cfg, &self.devices, self.status.trigger.as_deref());
                    update::open_url(&update::report_mailto(t.report_subject, &body));
                }
                let copy = if copied { t.report_copied } else { t.report_copy };
                if button(ui, p, copy, ButtonKind::Plain).clicked() {
                    ui.ctx().copy_text(update::REPORT_EMAIL.to_string());
                    self.copied_at = Some(Instant::now());
                    ui.ctx().request_repaint_after(Duration::from_secs(2));
                }
                if button(ui, p, t.report_issue, ButtonKind::Plain).clicked() {
                    update::open_url(update::ISSUES_PAGE);
                }
            });
            ui.add_space(12.0);
        });
    }

    /// Saving the settings to a file, loading a saved one, managing them, and their folder.
    fn backup_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let (mut save, mut load, mut manage, mut open) = (false, false, false, false);
        let count = format!("{}/{}", self.backup_count, backup::MAX_FILES);
        let count_color = if self.backup_count >= backup::MAX_FILES { p.red } else { p.muted };
        section_full(ui, p, t.section_backup, Some(t.backup_info), Some((count.as_str(), count_color)), |ui| {
            ui.add_space(10.0);
            // Four buttons in one column: as wide as their labels, so they share one line.
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                save = compact_button(ui, p, t.backup_save, ButtonKind::Plain).clicked();
                load = compact_button(ui, p, t.backup_load, ButtonKind::Plain).clicked();
                // When Save found no room, Manage stands out as what to do next.
                let kind = if self.backup_full { ButtonKind::Primary } else { ButtonKind::Plain };
                manage = compact_button(ui, p, t.backup_manage, kind).clicked();
                open = compact_button(ui, p, t.backup_open, ButtonKind::Plain).clicked();
            });
            // What the last save or load did, until the window closes.
            if let Some((fine, text)) = &self.backup_note {
                ui.add_space(6.0);
                ui.label(RichText::new(text).size(12.5).color(if *fine { p.green } else { p.red }));
            }
            ui.add_space(10.0);
        });
        if save {
            let current = Config::load_or_create().unwrap_or_else(|_| self.cfg.clone());
            self.backup_full = false;
            self.backup_note = Some(match backup::export(&current) {
                Ok(backup::Export::Saved(path)) => (true, format!("{} {}", t.backup_saved, file_label(&path))),
                Ok(backup::Export::Full) => {
                    self.backup_full = true;
                    (false, t.backup_full.to_string())
                }
                Err(e) => (false, format!("{} {e:#}", t.backup_save_failed)),
            });
            self.backup_count = backup::list(&backup::folder()).len();
        }
        if load {
            self.stop_recording();
            self.backup_note = None;
            self.backup_picker = Some(BackupPicker { files: backup::list(&backup::folder()), chosen: None, error: None });
        }
        if manage {
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
        if open {
            let dir = backup::folder();
            match std::fs::create_dir_all(&dir) {
                Ok(()) => update::open_url(&dir.display().to_string()),
                Err(e) => self.backup_note = Some((false, format!("{}: {e}", dir.display()))),
            }
        }
    }

    /// The saved settings files, newest first, each with Rename and Delete.
    fn backup_manager_ui(&mut self, ctx: &egui::Context, t: &Texts, p: Palette) {
        let Some(manager) = self.backup_manager.as_mut() else { return };
        let mut action = None;
        let mut close = false;
        let frame = egui::Frame::new()
            .fill(p.bg)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(14)
            .inner_margin(Margin::symmetric(20, 16));
        let shown = egui::Modal::new(egui::Id::unique("backup-manager")).frame(frame).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(t.backup_manage_title).font(bold(16.0)).color(p.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let full = manager.files.len() >= backup::MAX_FILES;
                    let count = format!("{}/{}", manager.files.len(), backup::MAX_FILES);
                    ui.label(RichText::new(count).font(bold(13.0)).color(if full { p.red } else { p.muted }));
                });
            });
            ui.add_space(2.0);
            ui.label(RichText::new(backup::folder().display().to_string()).size(12.0).color(p.muted));
            ui.add_space(10.0);
            card(ui, p, |ui| {
                if manager.files.is_empty() {
                    empty(ui, p, t.backup_none);
                    return;
                }
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
            if let Some((fine, text)) = &manager.note {
                ui.add_space(8.0);
                ui.label(RichText::new(text).size(12.5).color(if *fine { p.green } else { p.red }));
            }
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = button(ui, p, t.close, ButtonKind::Plain).clicked();
            });
        });
        if close || shown.should_close() {
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

    /// The saved settings files, newest first: one is chosen, then loaded once confirmed.
    fn backup_picker_ui(&mut self, ctx: &egui::Context, t: &Texts, p: Palette) {
        let Some(picker) = self.backup_picker.as_mut() else { return };
        let (mut cancel, mut load) = (false, false);
        let frame = egui::Frame::new()
            .fill(p.bg)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(14)
            .inner_margin(Margin::symmetric(20, 16));
        let shown = egui::Modal::new(egui::Id::unique("backup-picker")).frame(frame).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(RichText::new(t.backup_pick_title).font(bold(16.0)).color(p.text));
            ui.add_space(2.0);
            ui.label(RichText::new(backup::folder().display().to_string()).size(12.0).color(p.muted));
            ui.add_space(10.0);
            card(ui, p, |ui| {
                if picker.files.is_empty() {
                    empty(ui, p, t.backup_none);
                    return;
                }
                egui::ScrollArea::vertical().max_height(250.0).auto_shrink([false, true]).show(ui, |ui| {
                    for (i, file) in picker.files.iter().enumerate() {
                        if i > 0 {
                            divider(ui, p);
                        }
                        let row = Row { title: &file.name, subtitle: None, badge: None };
                        if choice_row(ui, p, Mark::Radio, picker.chosen == Some(i), &row).clicked() {
                            picker.chosen = Some(i);
                            picker.error = None;
                        }
                    }
                });
            });
            if picker.chosen.is_some() {
                ui.add_space(10.0);
                ui.label(RichText::new(t.backup_confirm).size(13.0).color(p.text));
            }
            if let Some(err) = &picker.error {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("{} {err}", t.backup_bad)).size(12.5).color(p.red));
            }
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                cancel = button(ui, p, t.cancel, ButtonKind::Plain).clicked();
                ui.add_enabled_ui(picker.chosen.is_some(), |ui| {
                    load = button(ui, p, t.backup_apply, ButtonKind::Primary).clicked();
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
                // What the setup check set aside belongs to this computer.
                self.apply(|c| *c = Config { setup: c.setup.clone(), ..loaded });
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
        self.header(ui, t, p);
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

// ---- The setup check ---------------------------------------------------------------

impl SettingsApp {
    /// What the setup check takes from the running flockswitch and the settings.
    fn setup_inputs(&mut self) -> setup::Inputs {
        self.setup.devices_seen |= !self.status.online.is_empty();
        let newer = match &*self.update.lock().unwrap() {
            UpdateUi::Available(r) | UpdateUi::InstallFailed(r, _) => Some(r.version.clone()),
            _ => None,
        };
        setup::Inputs {
            hid_denied: self.status.hid_denied,
            devices_open: self.setup.devices_seen,
            edge_backend: self.status.edge_backend.clone(),
            edge_note: self.status.edge_note.clone(),
            korean: crate::i18n::is_korean(),
            newer_version: newer,
        }
    }

    /// Checks again (in the background) when what the check depends on changed or a fix
    /// finished, and loads a Korean font installed from here.
    fn setup_tick(&mut self, ctx: &egui::Context) {
        let font_done = self.setup.runs.lock().unwrap().get(&ItemId::KoreanFont) == Some(&SetupRun::Done(After::Fonts));
        if font_done && !self.setup.fonts_reloaded {
            self.setup.fonts_reloaded = true;
            log::info!("setup: loading the Korean font just installed");
            install_fonts(ctx);
        }
        if self.setup.checking.load(Ordering::Relaxed) {
            return;
        }
        let inputs = self.setup_inputs();
        let stale = self.setup.stale.swap(false, Ordering::Relaxed);
        if !stale && self.setup.asked.as_ref() == Some(&inputs) {
            return;
        }
        self.setup.asked = Some(inputs.clone());
        self.setup.checking.store(true, Ordering::Relaxed);
        let (found, checking, ctx) = (self.setup.found.clone(), self.setup.checking.clone(), ctx.clone());
        std::thread::spawn(move || {
            let items = setup::detect(&inputs);
            *found.lock().unwrap() = Some(items);
            checking.store(false, Ordering::Relaxed);
            ctx.request_repaint();
        });
    }

    fn setup_items(&self) -> Vec<Item> {
        self.setup.found.lock().unwrap().clone().unwrap_or_default()
    }

    fn setup_item(&self, id: ItemId) -> Option<Item> {
        self.setup_items().into_iter().find(|i| i.id == id)
    }

    /// At the top of the General tab, only while something is missing: a row for each
    /// item with why it is needed, Install (which first shows the commands) and Later.
    /// A fix run from here keeps its row, with how it went, until the window closes.
    fn setup_card(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let items = self.setup_items();
        if items.is_empty() {
            return;
        }
        // Korean without a font to draw it: this card speaks English.
        let t = if setup::korean_unreadable(&items) { crate::i18n::english() } else { t };
        let snoozed = self.cfg.setup.snoozed.clone();
        let runs = self.setup.runs.lock().unwrap().clone();
        let rows: Vec<&Item> = items
            .iter()
            .filter(|i| runs.contains_key(&i.id) || (i.state == SetupState::Missing && !setup_logic::is_snoozed(i, &snoozed)))
            .collect();
        // Set aside, and not run from here (a run keeps its row).
        let not_run: Vec<Item> = items.iter().filter(|i| !runs.contains_key(&i.id)).cloned().collect();
        let hidden = setup_logic::snoozed_count(&not_run, &snoozed);
        let copied = self.setup.copied.filter(|(_, at)| at.elapsed() < Duration::from_secs(2)).map(|(id, _)| id);
        let busy = runs.values().any(|r| *r == SetupRun::Running);
        // Two or more to install (not run from here yet, or to try again): all of them
        // with one password.
        let all: Vec<Item> = setup_logic::installable(rows.iter().copied())
            .into_iter()
            .filter(|i| matches!(runs.get(&i.id), None | Some(SetupRun::NoAdmin(_) | SetupRun::Failed { .. })))
            .cloned()
            .collect();
        let mut action = None;
        if rows.is_empty() {
            if hidden > 0 {
                ui.add_space(8.0);
                if set_aside_line(ui, p, t, hidden) {
                    action = Some(SetupAction::ShowAgain);
                }
            }
        } else {
            section_with_info(ui, p, t.setup_title, Some(t.setup_info), |ui| {
                if all.len() >= 2 {
                    let title = t.setup_all_title.replace("{n}", &all.len().to_string());
                    if titled_row(ui, p, &title, t.setup_all_text, &[(t.setup_all, ButtonKind::Primary)], !busy).is_some() {
                        action = Some(SetupAction::InstallAll(all.clone()));
                    }
                    divider(ui, p);
                }
                for (i, item) in rows.iter().enumerate() {
                    if i > 0 {
                        divider(ui, p);
                    }
                    if let Some(a) = setup_row(ui, p, t, item, runs.get(&item.id), busy, copied == Some(item.id)) {
                        action = Some(a);
                    }
                }
                if hidden > 0 {
                    divider(ui, p);
                    if set_aside_line(ui, p, t, hidden) {
                        action = Some(SetupAction::ShowAgain);
                    }
                }
                ui.add_space(4.0);
            });
        }
        match action {
            Some(SetupAction::Install(item)) => self.setup.confirm = Some(vec![item]),
            Some(SetupAction::InstallAll(items)) => self.setup.confirm = Some(items),
            Some(SetupAction::Later(item)) => {
                log::info!("setup {}: set aside", item.snooze_key());
                self.apply(|c| setup_logic::snooze(&mut c.setup.snoozed, &item));
                shared::request_setup_recheck();
            }
            Some(SetupAction::ShowAgain) => {
                self.apply(|c| c.setup.snoozed.clear());
                shared::request_setup_recheck();
            }
            Some(SetupAction::Copy(id, text)) => {
                ui.ctx().copy_text(text);
                self.setup.copied = Some((id, Instant::now()));
                ui.ctx().request_repaint_after(Duration::from_secs(2));
            }
            None => {}
        }
    }

    /// The commands of the items chosen (one, or all of them for "Install all"), for
    /// approval: what runs as administrator (the system asks for the password, once) and
    /// what runs as the user. Nothing runs before Run.
    fn setup_confirm_ui(&mut self, ctx: &egui::Context, p: Palette) {
        let Some(chosen) = self.setup.confirm.clone() else { return };
        let items: Vec<Item> = chosen.into_iter().filter(|i| i.fix.is_some()).collect();
        if items.is_empty() {
            self.setup.confirm = None;
            return;
        }
        let fixes: Vec<&setup_logic::Fix> = items.iter().filter_map(|i| i.fix.as_ref()).collect();
        let admin: Vec<&Item> = items.iter().filter(|i| i.needs_admin()).collect();
        let admin_script = match admin.as_slice() {
            [] => None,
            [one] => one.fix.as_ref().map(setup_logic::Fix::admin_script),
            many => Some(setup_logic::combined_admin_script(many)),
        };
        // Each item's own user steps run on their own, after the administrator steps.
        let user_script: Vec<String> = items
            .iter()
            .filter_map(|i| Some((i, i.fix.as_ref().filter(|f| !f.user.is_empty())?)))
            .map(|(i, f)| if items.len() == 1 { f.user_script() } else { format!("# {}\n{}", i.id.key(), f.user_script()) })
            .collect();
        let t = if setup::korean_unreadable(&self.setup_items()) { crate::i18n::english() } else { texts() };
        let mut notes: Vec<&str> = Vec::new();
        for note in fixes.iter().filter_map(|f| after_note(t, f.after)) {
            if !notes.contains(&note) {
                notes.push(note);
            }
        }
        let (mut run, mut cancel) = (false, false);
        let frame = egui::Frame::new()
            .fill(p.bg)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(14)
            .inner_margin(Margin::symmetric(20, 16));
        let shown = egui::Modal::new(egui::Id::unique("setup-confirm")).frame(frame).show(ctx, |ui| {
            ui.set_width(640.0);
            if let [item] = items.as_slice() {
                ui.label(RichText::new(setup::title(t, item)).font(bold(16.0)).color(p.text));
                ui.add_space(4.0);
                ui.label(RichText::new(setup::why(t, item)).size(12.5).color(p.muted));
            } else {
                ui.label(RichText::new(t.setup_all_title.replace("{n}", &items.len().to_string())).font(bold(16.0)).color(p.text));
                ui.add_space(4.0);
                for item in &items {
                    ui.label(RichText::new(format!("• {}", setup::title(t, item))).size(13.0).color(p.text));
                }
                ui.add_space(4.0);
                ui.label(RichText::new(t.setup_all_text).size(12.5).color(p.muted));
            }
            ui.add_space(10.0);
            // Several items: shorter boxes, so the dialog fits the smallest window.
            let (admin_height, user_height) = if items.len() == 1 { (300.0, 160.0) } else { (230.0, 110.0) };
            if let Some(script) = &admin_script {
                ui.label(RichText::new(t.setup_as_admin).size(13.0).color(p.text));
                ui.add_space(4.0);
                command_box(ui, p, "setup-confirm-admin", &format!("# pkexec /bin/sh -c\n{script}"), admin_height);
                ui.add_space(10.0);
            }
            if !user_script.is_empty() {
                ui.label(RichText::new(t.setup_as_user).size(13.0).color(p.text));
                ui.add_space(4.0);
                command_box(ui, p, "setup-confirm-user", &format!("# /bin/sh -c\n{}", user_script.join("\n")), user_height);
                ui.add_space(10.0);
            }
            for note in &notes {
                ui.label(RichText::new(*note).size(12.5).color(p.amber));
                ui.add_space(4.0);
            }
            ui.add_space(8.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                cancel = button(ui, p, t.cancel, ButtonKind::Plain).clicked();
                run = button(ui, p, t.setup_run, ButtonKind::Primary).clicked();
            });
        });
        let keys = items.iter().map(|i| i.id.key()).collect::<Vec<_>>().join("+");
        if cancel || shown.should_close() {
            log::info!("setup {keys}: not approved");
            self.setup.confirm = None;
        } else if run {
            self.setup.confirm = None;
            self.start_setup_run(ctx, items);
        }
    }

    /// Runs approved fixes in the background (several: one pkexec for all their
    /// administrator steps); each row shows how its own went. Afterwards the check runs
    /// again here and in the tray app.
    fn start_setup_run(&mut self, ctx: &egui::Context, items: Vec<Item>) {
        let (runs, stale, update, ctx) = (self.setup.runs.clone(), self.setup.stale.clone(), self.update.clone(), ctx.clone());
        for item in &items {
            runs.lock().unwrap().insert(item.id, SetupRun::Running);
        }
        std::thread::spawn(move || {
            let refs: Vec<&Item> = items.iter().collect();
            let outcomes = setup::run_all(&refs, setup_logic::AdminTool::Pkexec, false);
            // Let udev apply a new rule, then the engine looks for the devices again.
            let replugged = outcomes.iter().any(|(id, o)| {
                *o == setup::Outcome::Done && items.iter().any(|i| i.id == *id && i.fix.as_ref().is_some_and(|f| f.after == After::Replug))
            });
            if replugged {
                std::thread::sleep(Duration::from_secs(2));
                shared::request_rescan();
            }
            for (id, outcome) in outcomes {
                let Some(item) = items.iter().find(|i| i.id == id) else { continue };
                let terminal = item.fix.as_ref().map(setup_logic::Fix::terminal).unwrap_or_default();
                let after = item.fix.as_ref().map_or(After::Menu, |f| f.after);
                let next = match outcome {
                    setup::Outcome::Done if after == After::Restart => brew_restart(&update),
                    setup::Outcome::Done => SetupRun::Done(after),
                    setup::Outcome::NoAdmin => SetupRun::NoAdmin(terminal),
                    setup::Outcome::Failed { code, tail } => SetupRun::Failed { code, tail, terminal },
                };
                runs.lock().unwrap().insert(id, next);
            }
            stale.store(true, Ordering::Relaxed);
            shared::request_setup_recheck();
            ctx.request_repaint();
        });
    }
}

/// After `brew upgrade`: the tray app restarts into the new version (from
/// `<prefix>/bin/flockswitch`) and this window closes and comes back in it, the way an
/// update does. When Homebrew had no newer version yet, nothing restarts.
fn brew_restart(update: &Mutex<UpdateUi>) -> SetupRun {
    let installed = setup::brew_installed_version();
    log::info!("update: Homebrew has flockswitch {} now", installed.as_deref().unwrap_or("?"));
    if installed.as_deref().is_some_and(|v| !update::is_newer(v, env!("CARGO_PKG_VERSION"))) {
        return SetupRun::BrewSame;
    }
    let restarting = match setup::brew_program() {
        Some(exe) if crate::main_instance_running() => match update::start_new_version(&exe, &["tray", "--settings"]) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("update: upgraded with brew, but cannot start it: {e:#}");
                false
            }
        },
        _ => false,
    };
    *update.lock().unwrap() = if restarting { UpdateUi::Restarting } else { UpdateUi::Installed };
    if restarting { SetupRun::Done(After::Restart) } else { SetupRun::BrewInstalled }
}

/// The word on an item's button: 설치 / Install, 추가 / Add, 켜기 / Turn on, 업데이트 / Update.
fn setup_button<'a>(t: &'a Texts, item: &Item) -> &'a str {
    match item.id {
        ItemId::MenuEntry => t.setup_add,
        ItemId::BrewUpdate => t.setup_update,
        ItemId::TrayIcon if !item.needs_admin() => t.setup_turn_on,
        _ => t.setup_install,
    }
}

/// What to do once a fix went through.
fn after_text<'a>(t: &'a Texts, after: After) -> &'a str {
    match after {
        After::Replug => t.setup_after_replug,
        After::Relogin => t.setup_after_relogin,
        After::MaybeRelogin => t.setup_after_maybe_relogin,
        After::Fonts => t.setup_after_fonts,
        After::Menu => t.setup_after_menu,
        After::Restart => t.setup_after_restart,
    }
}

/// Said before Run, where something remains to be done afterwards.
fn after_note<'a>(t: &'a Texts, after: After) -> Option<&'a str> {
    match after {
        After::Replug => Some(t.setup_note_replug),
        After::Relogin => Some(t.setup_note_relogin),
        After::Restart => Some(t.setup_note_restart),
        After::MaybeRelogin | After::Fonts | After::Menu => None,
    }
}

/// One item of the setup card: its name and why on the left, its buttons on the right,
/// and under it how its fix went (or the commands to run by hand).
fn setup_row(ui: &mut Ui, p: Palette, t: &Texts, item: &Item, run: Option<&SetupRun>, busy: bool, copied: bool) -> Option<SetupAction> {
    let mut labels: Vec<(&str, ButtonKind)> = Vec::new();
    let mut actions: Vec<SetupAction> = Vec::new();
    match run {
        Some(SetupRun::Running | SetupRun::Done(_) | SetupRun::BrewInstalled) => {}
        Some(SetupRun::NoAdmin(_) | SetupRun::Failed { .. } | SetupRun::BrewSame) => {
            labels.push((t.setup_retry, ButtonKind::Plain));
            actions.push(SetupAction::Install(item.clone()));
        }
        None => {
            if item.fix.is_some() {
                labels.push((setup_button(t, item), ButtonKind::Primary));
                actions.push(SetupAction::Install(item.clone()));
            }
            labels.push((t.setup_later, ButtonKind::Plain));
            actions.push(SetupAction::Later(item.clone()));
        }
    }
    let mut action = titled_row(ui, p, &setup::title(t, item), &setup::why(t, item), &labels, !busy).and_then(|i| actions.into_iter().nth(i));
    let copy_label = if copied { t.setup_copied } else { t.setup_copy };
    let mut commands = |ui: &mut Ui, note: &str, color: Color32, text: &str, copy: Option<&str>| {
        ui.label(RichText::new(note).size(12.5).color(color));
        ui.add_space(4.0);
        command_box(ui, p, &format!("setup-{}", item.id.key()), text, 150.0);
        if let Some(copy) = copy {
            ui.add_space(6.0);
            if compact_button(ui, p, copy_label, ButtonKind::Plain).clicked() {
                action = Some(SetupAction::Copy(item.id, copy.to_string()));
            }
        }
        ui.add_space(8.0);
    };
    match run {
        Some(SetupRun::Running) => {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(RichText::new(t.setup_running).size(12.5).color(p.muted));
            });
            ui.add_space(8.0);
        }
        Some(SetupRun::Done(after)) => {
            ui.label(RichText::new(format!("✓ {}", after_text(t, *after))).size(12.5).color(p.green));
            ui.add_space(8.0);
        }
        Some(SetupRun::BrewSame) => {
            ui.label(RichText::new(t.setup_brew_same).size(12.5).color(p.amber));
            ui.add_space(8.0);
        }
        Some(SetupRun::BrewInstalled) => {
            ui.label(RichText::new(format!("✓ {}", t.update_installed)).size(12.5).color(p.green));
            ui.add_space(8.0);
        }
        Some(SetupRun::NoAdmin(terminal)) => commands(ui, t.setup_no_admin, p.amber, terminal, Some(terminal)),
        Some(SetupRun::Failed { code, tail, terminal }) => {
            let code = code.map_or_else(|| "?".to_string(), |c| c.to_string());
            commands(ui, &t.setup_failed.replace("{code}", &code), p.red, &tail.join("\n"), Some(terminal));
        }
        None => {
            if let (None, Some(manual)) = (&item.fix, &item.manual) {
                let note = if item.id == ItemId::BrewUpdate { t.setup_manual } else { t.setup_no_package_manager };
                commands(ui, note, p.muted, manual, Some(manual));
            }
        }
    }
    action
}

/// "2 set aside for later · Show again". True when Show again was clicked.
fn set_aside_line(ui: &mut Ui, p: Palette, t: &Texts, n: usize) -> bool {
    let mut clicked = false;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(t.setup_set_aside.replace("{n}", &n.to_string())).size(12.5).color(p.muted));
        clicked = ui.link(RichText::new(t.setup_show_again).size(12.5).color(p.accent)).clicked();
    });
    ui.add_space(6.0);
    clicked
}

/// A row with a title and, under it, an explanation on the left and buttons on the
/// right. Returns the index of the button clicked.
fn titled_row(ui: &mut Ui, p: Palette, title: &str, text: &str, labels: &[(&str, ButtonKind)], enabled: bool) -> Option<usize> {
    let buttons: f32 = if labels.is_empty() {
        0.0
    } else {
        labels.iter().map(|(l, k)| button_width(ui, l, *k)).sum::<f32>() + 8.0 * (labels.len() - 1) as f32
    };
    let width = ui.available_width();
    let wrap = (width - buttons - 16.0).max(160.0);
    let title_text = ui.painter().layout(title.to_string(), FontId::proportional(14.5), p.text, wrap);
    let body = ui.painter().layout(text.to_string(), FontId::proportional(12.5), p.muted, wrap);
    let (title_h, body_h) = (title_text.size().y, if text.is_empty() { 0.0 } else { body.size().y + 3.0 });
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, (title_h + body_h + 20.0).max(48.0)), Sense::hover());
    let top = rect.center().y - (title_h + body_h) / 2.0;
    ui.painter().galley(Pos2::new(rect.left(), top), title_text, p.text);
    if !text.is_empty() {
        ui.painter().galley(Pos2::new(rect.left(), top + title_h + 3.0), body, p.muted);
    }
    let mut clicked = None;
    if !labels.is_empty() {
        let area = Rect::from_min_max(
            Pos2::new(rect.right() - buttons, rect.center().y - 15.0),
            Pos2::new(rect.right(), rect.center().y + 15.0),
        );
        ui.new_child(UiBuilder::new().max_rect(area)).scope(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.add_enabled_ui(enabled, |ui| {
                ui.horizontal(|ui| {
                    for (i, (label, kind)) in labels.iter().enumerate() {
                        if button(ui, p, label, *kind).clicked() {
                            clicked = Some(i);
                        }
                    }
                });
            });
        });
    }
    clicked
}

/// Commands or a program's output, as they are, in a box whose text can be selected.
fn command_box(ui: &mut Ui, p: Palette, id: &str, text: &str, max_height: f32) {
    egui::Frame::new()
        .fill(p.bg)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(8)
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt(id).max_height(max_height).auto_shrink([false, true]).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(text).monospace().size(12.0).color(p.text)).selectable(true).wrap());
            });
        });
}

fn device_subtitle(d: &DeviceInfo, t: &Texts) -> String {
    let kind = match d.kind.as_str() {
        "mouse" => t.kind_mouse,
        "keyboard" => t.kind_keyboard,
        _ => t.kind_other,
    };
    format!("{kind} · {}", d.key)
}

fn away_badge<'a>(d: &DeviceInfo, t: &'a Texts, p: Palette) -> Option<(&'a str, Color32)> {
    (!d.online).then_some((t.away, p.amber))
}

// ---- Building blocks -------------------------------------------------------------

/// A file's name, for saying where something was saved.
fn file_label(path: &std::path::Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
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

/// An "i" just after the label of the row that starts at `row` and is `height` tall,
/// for a label drawn in the rows' usual font.
fn label_info(ui: &mut Ui, p: Palette, row: Pos2, height: f32, label: &str, text: &str) {
    let width = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(14.5), p.text).size().x;
    info_icon(ui, p, label, Pos2::new(row.x + width + 15.0, row.y + height / 2.0), text);
}

/// A small round "i" centred on `at` that shows `text` while the pointer rests on it.
/// It takes clicks itself, so clicking it never flips the row's switch beneath.
fn info_icon(ui: &mut Ui, p: Palette, name: &str, at: Pos2, text: &str) -> Response {
    let rect = Rect::from_center_size(at, Vec2::splat(18.0));
    let resp = ui.interact(rect, egui::Id::unique(("info", name)), Sense::click()).on_hover_cursor(egui::CursorIcon::Help);
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, text));
    let hot = resp.hovered() || resp.has_focus();
    let color = if hot { p.accent } else { p.muted };
    let painter = ui.painter();
    if hot {
        painter.circle_filled(at, 8.5, p.accent.gamma_multiply(0.14));
    }
    painter.circle_stroke(at, 7.5, Stroke::new(1.3, color));
    painter.text(at + Vec2::new(0.0, 0.5), Align2::CENTER_CENTER, "i", bold(11.5), color);
    resp.on_hover_ui(|ui| {
        ui.set_max_width(380.0);
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(RichText::new(text).size(12.5).color(p.text));
    })
}

/// Two columns side by side, each with the normal spacing inside.
fn two_columns(ui: &mut Ui, add: impl FnOnce(&mut Ui, &mut Ui)) {
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.x = 28.0;
        ui.columns(2, |columns| {
            let (left, right) = columns.split_at_mut(1);
            left[0].spacing_mut().item_spacing.x = 8.0;
            right[0].spacing_mut().item_spacing.x = 8.0;
            add(&mut left[0], &mut right[0]);
        });
    });
}

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

/// A titled card: the title sits above a rounded panel, as in the system settings.
fn section(ui: &mut Ui, p: Palette, title: &str, add: impl FnOnce(&mut Ui)) {
    section_with_info(ui, p, title, None, add);
}

/// A titled card with an "i" after the title that explains `info`.
fn section_with_info(ui: &mut Ui, p: Palette, title: &str, info: Option<&str>, add: impl FnOnce(&mut Ui)) {
    section_full(ui, p, title, info, None, add);
}

/// A titled card with an optional "i" after the title and a short text at the right end
/// of the title line (`trailing`, in its colour).
fn section_full(ui: &mut Ui, p: Palette, title: &str, info: Option<&str>, trailing: Option<(&str, Color32)>, add: impl FnOnce(&mut Ui)) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        let label = ui.label(RichText::new(title).font(bold(13.0)).color(p.muted));
        if let Some(text) = info {
            let at = Pos2::new(label.rect.right() + 13.0, label.rect.center().y);
            info_icon(ui, p, title, at, text);
        }
        if let Some((text, color)) = trailing {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(4.0);
                ui.label(RichText::new(text).font(bold(13.0)).color(color));
            });
        }
    });
    ui.add_space(2.0);
    card(ui, p, add);
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
            painter.text(Pos2::new(rect.left(), rect.center().y - 1.0), Align2::LEFT_BOTTOM, file.stem(), FontId::proportional(14.5), p.text);
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

fn card(ui: &mut Ui, p: Palette, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.border))
        .corner_radius(12)
        .inner_margin(Margin::symmetric(18, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            add(ui);
        });
}

/// What the app expects before it is set up: device access, and no other app using the buttons.
fn notice(ui: &mut Ui, p: Palette, t: &Texts) {
    ui.add_space(12.0);
    egui::Frame::new()
        .fill(p.accent.gamma_multiply(0.10))
        .stroke(Stroke::new(1.0, p.accent.gamma_multiply(0.45)))
        .corner_radius(12)
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(RichText::new(format!("ⓘ  {}", t.notice_title)).font(bold(14.0)).color(p.accent));
            ui.label(RichText::new(t.notice).size(12.5).color(p.text));
        });
}

/// Something to fix before flockswitch can work, under the header, with a button that
/// leads to the fix when there is one. True when it was clicked.
fn warning(ui: &mut Ui, p: Palette, title: &str, text: &str, button_label: Option<&str>) -> bool {
    ui.add_space(12.0);
    let mut clicked = false;
    egui::Frame::new()
        .fill(p.amber.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0, p.amber.gamma_multiply(0.6)))
        .corner_radius(12)
        .inner_margin(Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.label(RichText::new(format!("⚠  {title}")).font(bold(14.0)).color(p.amber));
            ui.label(RichText::new(text).size(12.5).color(p.text));
            if let Some(label) = button_label {
                ui.add_space(4.0);
                clicked = button(ui, p, label, ButtonKind::Plain).clicked();
            }
        });
    clicked
}

/// The desktop's name for the Wayland notes ("ubuntu:GNOME" → "GNOME").
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

fn desktop_name() -> String {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    desktop.rsplit(':').next().map(str::trim).filter(|d| !d.is_empty()).unwrap_or("Wayland").to_string()
}

/// What the running flockswitch says about watching the edges in this Wayland session
/// (from the status file): the text to show, and whether it is a problem.
fn edge_backend_text(t: &Texts, backend: &str, note: &str, require_control: bool) -> (String, bool) {
    let desktop = desktop_name();
    let fill = |text: &str| text.replace("{desktop}", &desktop);
    let (code, arg) = note.split_once(':').unwrap_or((note, ""));
    match backend {
        "input-capture" => match code {
            "asking" => (fill(t.edges_capture_asking), false),
            "ready" => {
                let mut text = fill(t.edges_capture_ready);
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
            _ => (fill(t.edges_capture_off), false),
        },
        "layer-shell" => {
            let mut text = fill(t.edges_layer_ready);
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
                _ => fill(t.edges_none_support),
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

/// A box of text about something that does not work, in red.
fn error_box(ui: &mut Ui, p: Palette, text: &str) {
    egui::Frame::new()
        .fill(p.red.gamma_multiply(0.12))
        .stroke(Stroke::new(1.0, p.red.gamma_multiply(0.6)))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).size(12.5).color(p.red));
        });
}

/// A quiet box of explanation text.
fn info_box(ui: &mut Ui, p: Palette, text: &str) {
    egui::Frame::new()
        .fill(p.card.gamma_multiply(0.5))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(text).size(12.5).color(p.muted));
        });
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

fn empty(ui: &mut Ui, p: Palette, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).size(13.0).color(p.muted));
    ui.add_space(8.0);
}

/// A row with just a label (the setting's rows follow), and a `note` in smaller amber
/// text under it.
fn title_row(ui: &mut Ui, p: Palette, label: &str, note: Option<&str>) {
    let h = if note.is_some() { 50.0 } else { 40.0 };
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let painter = ui.painter();
    let left = Pos2::new(rect.left(), rect.center().y);
    match note {
        Some(note) => {
            painter.text(left - Vec2::new(0.0, 1.0), Align2::LEFT_BOTTOM, label, FontId::proportional(14.5), p.text);
            painter.text(left + Vec2::new(0.0, 3.0), Align2::LEFT_TOP, note, FontId::proportional(12.5), p.amber);
        }
        None => {
            painter.text(left, Align2::LEFT_CENTER, label, FontId::proportional(14.5), p.text);
        }
    }
}

/// A row with a label and an on/off switch on the right. Returns true when flipped.
/// A `note` shows in smaller text under the label.
fn switch_row(ui: &mut Ui, p: Palette, label: &str, note: Option<&str>, on: &mut bool) -> bool {
    let h = if note.is_some() { 50.0 } else { 40.0 };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| WidgetInfo::selected(Role::Switch, true, *on, label));
    if resp.clicked() {
        *on = !*on;
    }
    let painter = ui.painter();
    let left = Pos2::new(rect.left(), rect.center().y);
    match note {
        Some(note) => {
            painter.text(left - Vec2::new(0.0, 1.0), Align2::LEFT_BOTTOM, label, FontId::proportional(14.5), p.text);
            painter.text(left + Vec2::new(0.0, 3.0), Align2::LEFT_TOP, note, FontId::proportional(12.5), p.amber);
        }
        None => {
            painter.text(left, Align2::LEFT_CENTER, label, FontId::proportional(14.5), p.text);
        }
    }

    let how_on = ui.ctx().animate_bool_responsive(resp.id, *on);
    let track = Rect::from_center_size(Pos2::new(rect.right() - 20.0, rect.center().y), Vec2::new(40.0, 24.0));
    painter.rect_filled(track, 12, p.track.lerp_to_gamma(p.accent, how_on));
    let x = egui::lerp(track.left() + 12.0..=track.right() - 12.0, how_on);
    painter.circle_filled(Pos2::new(x, track.center().y), 9.5, Color32::WHITE);
    resp.clicked()
}

const ROW_SMALL: f32 = 42.0;

/// A row `height` tall with a bold label on the left and a control `width` wide on the
/// right.
fn labeled_row(
    ui: &mut Ui,
    p: Palette,
    label: &str,
    width: f32,
    height: f32,
    control: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    // The same text as a switch row's label, so the rows of a card read alike.
    ui.painter().text(Pos2::new(rect.left(), rect.center().y), Align2::LEFT_CENTER, label, FontId::proportional(14.5), p.text);
    let area = Rect::from_min_max(
        Pos2::new(rect.right() - width, rect.center().y - 15.0),
        Pos2::new(rect.right(), rect.center().y + 15.0),
    );
    // A child, not a scope: a scope would pull the cursor back up to the control's
    // bottom, and the next row or divider would crowd it.
    ui.new_child(UiBuilder::new().max_rect(area)).scope(control).inner
}

/// Equal-width buttons in one row. `selected(i)` says which are on; returns the index
/// clicked. One-choice rows (mode, tabs, language) draw them on a shared bar; toggle
/// rows (channels) as separate outlined buttons in the same positions, so the rows
/// line up.
/// Height and text size of a segment bar.
#[derive(Clone, Copy)]
struct SegSize {
    height: f32,
    text: f32,
}
const SEG_NORMAL: SegSize = SegSize { height: 30.0, text: 14.0 };
/// Every button, drop-down and segment bar inside a card is this tall.
const CONTROL_HEIGHT: f32 = 28.0;
/// Buttons are at least this wide, so short labels line up with long ones.
const BUTTON_MIN_WIDTH: f32 = 84.0;
const SEG_SMALL: SegSize = SegSize { height: CONTROL_HEIGHT, text: 13.0 };

/// A chosen segment's accent, drained of most of its colour and sunk toward the track:
/// reads as taken, but not available.
fn locked_fill(p: Palette) -> Color32 {
    let [r, g, b, _] = p.accent.to_array();
    let gray = 0.3 * r as f32 + 0.59 * g as f32 + 0.11 * b as f32;
    let [tr, tg, tb, _] = p.track.to_array();
    let mix = |c: u8, t: u8| (c as f32 * 0.25 + gray * 0.25 + t as f32 * 0.5) as u8;
    Color32::from_rgb(mix(r, tr), mix(g, tg), mix(b, tb))
}

#[allow(clippy::too_many_arguments)]
fn segments(
    ui: &mut Ui,
    p: Palette,
    name: &str,
    labels: &[String],
    role: Role,
    size: SegSize,
    // A segment shown but not clickable (this computer's channel).
    locked: Option<usize>,
    // The "Off" segment: chosen, it is drawn gray instead of in the accent colour.
    off: Option<usize>,
    selected: impl Fn(usize) -> bool,
) -> Option<usize> {
    let toggles = role == Role::CheckBox;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), size.height), Sense::hover());
    if !toggles {
        ui.painter().rect_filled(rect, 8, p.track.gamma_multiply(0.55));
    }
    let w = rect.width() / labels.len() as f32;
    let mut clicked = None;
    for (i, label) in labels.iter().enumerate() {
        let seg = Rect::from_min_size(Pos2::new(rect.left() + w * i as f32, rect.top()), Vec2::new(w, rect.height()))
            .shrink(3.0);
        // Named per bar: the rows sit in look-alike child areas, so an id derived from
        // the area would be shared and one bar's click would also hit the other.
        let id = egui::Id::unique(("segment", name, i));
        let locked = locked == Some(i);
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
            (locked_fill(p), Color32::WHITE.gamma_multiply(0.7))
        } else if on && off == Some(i) {
            (p.off, Color32::WHITE)
        } else if on {
            (p.accent, Color32::WHITE)
        } else if resp.hovered() {
            (if toggles { p.hover } else { p.card.gamma_multiply(0.6) }, p.text)
        } else {
            (if toggles { p.card } else { Color32::TRANSPARENT }, p.text)
        };
        painter.rect_filled(seg, 7, fill);
        if toggles && !on && !locked {
            painter.rect_stroke(seg, 7, Stroke::new(1.0, p.track), StrokeKind::Inside);
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
    match segments(ui, p, name, &labels, Role::RadioButton, size, None, None, |i| options[i].0 == *value) {
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

/// Width of what sits right of a situation's name in the haptics window.
const SITUATION_CONTROLS: f32 = 360.0;
/// The haptics window's rows are taller than a card's usual ones, for air between them.
const DIALOG_ROW: f32 = 50.0;

/// A situation of the haptics window on one line: its name, the control that shapes its
/// pulse, a button to feel it, and its switch. True when Feel was pressed.
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

/// A row of the haptics window: its name, and a control `width` wide on the right.
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

/// The on/off switch of a row, on its own. True when flipped.
fn toggle(ui: &mut Ui, p: Palette, label: &str, on: &mut bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(40.0, 24.0), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| WidgetInfo::selected(Role::Switch, true, *on, label));
    if resp.clicked() {
        *on = !*on;
    }
    let how_on = ui.ctx().animate_bool_responsive(resp.id, *on);
    let painter = ui.painter();
    painter.rect_filled(rect, 12, p.track.lerp_to_gamma(p.accent, how_on));
    let x = egui::lerp(rect.left() + 12.0..=rect.right() - 12.0, how_on);
    painter.circle_filled(Pos2::new(x, rect.center().y), 9.5, Color32::WHITE);
    resp.clicked()
}

/// An explanation on the left and buttons on the right. Returns the index clicked.
fn confirm_row(ui: &mut Ui, p: Palette, explain: &str, labels: &[(&str, ButtonKind)]) -> Option<usize> {
    let buttons: f32 = labels.iter().map(|(l, k)| button_width(ui, l, *k)).sum::<f32>() + 8.0 * (labels.len() - 1) as f32;
    let width = ui.available_width();
    let text = ui.painter().layout(explain.to_string(), FontId::proportional(12.5), p.muted, width - buttons - 16.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, (text.size().y + 20.0).max(48.0)), Sense::hover());
    ui.painter().galley(Pos2::new(rect.left(), rect.center().y - text.size().y / 2.0), text, p.muted);
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

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Radio,
    Check,
}

struct Row<'a> {
    title: &'a str,
    subtitle: Option<&'a str>,
    badge: Option<(&'a str, Color32)>,
}

/// A selectable list row: radio/check mark, title, optional second line and badge.
fn choice_row(ui: &mut Ui, p: Palette, mark: Mark, selected: bool, row: &Row) -> Response {
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
            painter.circle_filled(c, 10.0, p.accent);
            painter.circle_filled(c, 4.0, Color32::WHITE);
        }
        (Mark::Radio, false) => {
            painter.circle_stroke(c, 9.25, Stroke::new(1.5, p.track));
        }
        (Mark::Check, true) => {
            let r = Rect::from_center_size(c, Vec2::splat(20.0));
            painter.rect_filled(r, 6, p.accent);
            let tick = Stroke::new(2.2, Color32::WHITE);
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

fn button_width(ui: &Ui, label: &str, kind: ButtonKind) -> f32 {
    text_button_width(ui, label, kind).max(BUTTON_MIN_WIDTH)
}

/// A button's width from its label alone.
fn text_button_width(ui: &Ui, label: &str, kind: ButtonKind) -> f32 {
    ui.painter().layout_no_wrap(label.to_string(), button_font(kind), Color32::WHITE).size().x + 28.0
}

fn button(ui: &mut Ui, p: Palette, label: &str, kind: ButtonKind) -> Response {
    let width = button_width(ui, label, kind);
    button_of_width(ui, p, label, kind, width)
}

/// A button no wider than its label needs (down to `COMPACT_MIN_WIDTH`), for a row of
/// several that must share a column.
fn compact_button(ui: &mut Ui, p: Palette, label: &str, kind: ButtonKind) -> Response {
    let width = text_button_width(ui, label, kind).max(COMPACT_MIN_WIDTH);
    button_of_width(ui, p, label, kind, width)
}

/// The narrowest a compact button gets.
const COMPACT_MIN_WIDTH: f32 = 64.0;

fn button_of_width(ui: &mut Ui, p: Palette, label: &str, kind: ButtonKind, width: f32) -> Response {
    let font = button_font(kind);
    let size = Vec2::new(width, CONTROL_HEIGHT);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| WidgetInfo::labeled(Role::Button, true, label));
    let hovered = resp.hovered();
    let (fill, stroke, color) = match kind {
        ButtonKind::Plain => (if hovered { p.hover } else { p.card }, p.track, p.text),
        ButtonKind::Primary => (if hovered { p.accent.gamma_multiply(0.85) } else { p.accent }, p.accent, Color32::WHITE),
        ButtonKind::Danger => (if hovered { p.red.gamma_multiply(0.10) } else { p.card }, p.red.gamma_multiply(0.7), p.red),
        ButtonKind::DangerFilled => (if hovered { p.red.gamma_multiply(0.85) } else { p.red }, p.red, Color32::WHITE),
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(9), fill);
    painter.rect_stroke(rect, 9, Stroke::new(1.0, stroke), StrokeKind::Inside);
    painter.text(rect.center(), Align2::CENTER_CENTER, label, font, color);
    resp
}

/// A shortcut as stored in the settings ("ctrl+alt+1"), or None for keys that make no
/// good shortcut: a number, letter or F key, with ⌃/⌥/⌘ (Ctrl/Alt) unless it is an F key.
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
