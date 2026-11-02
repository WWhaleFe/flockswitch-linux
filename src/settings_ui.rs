//! The settings window. It runs as its own process (`flockswitch settings`) so the tray
//! app keeps its own event loop; changes are written to config.toml, which the running
//! engine reloads within a second. Every change applies immediately and the window
//! stays open, unlike a native menu that closes after each click.

use crate::config::{pick, Config, Gesture, BUTTON_GESTURE, BUTTON_MIDDLE, BUTTON_WHEEL_MODE};
use crate::i18n::{texts, Texts};
use crate::shared::{self, DeviceInfo, StatusFile};
use crate::update::{self, Release};
use anyhow::{anyhow, Result};
use auto_launch::AutoLaunch;
use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontFamily, FontId, Margin, Pos2, Rect, Response, RichText, Role,
    Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2, WidgetInfo,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_secs(1);
/// The "Shortcuts" entry next to the buttons on the switching tab.
const SHORTCUTS_TAB: u16 = 0xFFFF;

/// The app icon, scaled by build.rs (WINDOW_ICON there).
const ICON_SIDE: u32 = 96;
const ICON_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/window-icon.rgba"));

/// Opens the window. `lock` keeps a second window from opening; a connection to it
/// means "Settings…" was chosen again, so the window comes back to the front.
/// `show_update` opens on the General tab (from the "new version" menu item).
pub fn run(lock: std::net::TcpListener, show_update: bool) -> Result<()> {
    if let Ok(cfg) = Config::load_or_create() {
        crate::i18n::set_language(&cfg.language);
    }
    let t = texts();
    let icon = egui::IconData { rgba: ICON_RGBA.to_vec(), width: ICON_SIDE, height: ICON_SIDE };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(t.settings_title)
            // Matches flockswitch.desktop, so the dock shows the app's name and icon.
            .with_app_id("flockswitch")
            .with_icon(icon)
            .with_inner_size([520.0, 720.0])
            .with_min_inner_size([400.0, 360.0])
            .with_active(true),
        ..Default::default()
    };
    eframe::run_native(
        "flockswitch-settings",
        options,
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            install_style(&cc.egui_ctx);
            let raise = Arc::new(AtomicBool::new(false));
            let (ctx, flag) = (cc.egui_ctx.clone(), raise.clone());
            std::thread::spawn(move || {
                for _ in lock.incoming() {
                    flag.store(true, Ordering::Relaxed);
                    ctx.request_repaint();
                }
            });
            Ok(Box::new(SettingsApp::new(&cc.egui_ctx, raise, show_update)))
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
}

impl SettingsApp {
    fn new(ctx: &egui::Context, raise: Arc<AtomicBool>, show_update: bool) -> SettingsApp {
        let autostart = crate::tray::autostart();
        let autostart_on = autostart.as_ref().is_some_and(|a| a.is_enabled().unwrap_or(false));
        let image = egui::ColorImage::from_rgba_unmultiplied([ICON_SIDE as usize; 2], ICON_RGBA);
        let mut app = SettingsApp {
            tab: if show_update { Tab::General } else { Tab::Switching },
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
            // Started from a menu-bar app: come to the front once at start.
            raise_frames: 2,
            raise,
            title: texts().settings_title,
            icon: ctx.load_texture("app-icon", image, egui::TextureOptions::LINEAR),
            update: Arc::new(Mutex::new(UpdateUi::Idle)),
            copied_at: None,
        };
        app.refresh();
        if show_update || app.cfg.check_updates {
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
        let Some((name, url)) = release.asset.clone() else {
            update::open_url(&release.page);
            return;
        };
        let (state, ctx) = (self.update.clone(), ctx.clone());
        *state.lock().unwrap() = UpdateUi::Downloading;
        std::thread::spawn(move || {
            let next = match update::download(&name, &url) {
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
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(t.settings_title.into()));
        }
        let p = Palette::new(ui.visuals().dark_mode);
        let panel = egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(18, 12));
        // After uninstalling, show only the result: re-reading the settings now would
        // create the deleted settings folder again.
        if self.uninstall_progress() {
            ctx.request_repaint_after(Duration::from_millis(300));
            egui::CentralPanel::default().frame(panel).show(ui, |ui| self.uninstall_result(ui, t, p));
            return;
        }
        if self.last_refresh.elapsed() >= REFRESH {
            self.refresh();
        }
        ctx.request_repaint_after(REFRESH);
        if let Some(n) = self.recording {
            self.capture_shortcut(&ctx, t, n);
        }

        // The header and the tabs stay in place; only the page below them scrolls.
        egui::CentralPanel::default().frame(panel).show(ui, |ui| {
            self.header(ui, t, p);
            if self.status.hid_denied {
                warning(ui, p, t.hid_denied_title, t.hid_denied);
            }
            ui.add_space(12.0);
            let tabs = [
                (Tab::Switching, t.section_switching),
                (Tab::Devices, t.section_devices),
                (Tab::General, t.section_general),
            ];
            let labels: Vec<String> = tabs.iter().map(|(_, l)| l.to_string()).collect();
            if let Some(i) = segments(ui, p, "tabs", &labels, Role::Tab, SEG_NORMAL, None, |i| tabs[i].0 == self.tab) {
                self.tab = tabs[i].0;
            }
            // Pages are short enough to fit; if one grows (many devices, a long error),
            // the window grows with it instead of scrolling.
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
                            notice(ui, p, t);
                            self.general_section(ui, t, p);
                            self.update_section(ui, t, p);
                            self.report_section(ui, t, p);
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
            let overflow = page.content_size.y - page.inner_rect.height();
            if overflow > 1.0 {
                let (size, monitor) = ctx.input(|i| (i.viewport().inner_rect.map(|r| r.size()), i.viewport().monitor_size));
                if let Some(size) = size {
                    let max = monitor.map_or(f32::INFINITY, |m| m.y - 80.0);
                    let height = (size.y + overflow).min(max);
                    if height > size.y + 1.0 {
                        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(size.x, height)));
                    }
                }
            }
        });
    }
}

impl SettingsApp {
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
        let tab_ids: Vec<u16> = buttons.iter().map(|b| b.0).chain([SHORTCUTS_TAB]).collect();
        let labels: Vec<String> = buttons
            .iter()
            .map(|(cid, short, _)| (*short, self.cfg.button(*cid).on))
            .chain([(t.hotkeys_short, self.cfg.hotkeys.on)])
            .map(|(short, on)| if on { format!("{short} ✓") } else { short.to_string() })
            .collect();
        if let Some(i) = segments(ui, p, "button-tabs", &labels, Role::Tab, SEG_NORMAL, None, |i| tab_ids[i] == self.button_tab) {
            self.button_tab = tab_ids[i];
            self.stop_recording();
        }
        if self.button_tab == SHORTCUTS_TAB {
            ui.add_space(8.0);
            card(ui, p, |ui| self.shortcut_rows(ui, t, p));
            return;
        }
        let (cid, _, label) = buttons.into_iter().find(|b| b.0 == self.button_tab).unwrap_or(buttons[0]);

        ui.add_space(8.0);
        card(ui, p, |ui| {
            // Only known while the master mouse is on this computer.
            let missing = self.status.trigger.is_some() && !self.status.buttons.contains(&cid);
            let mut on = self.cfg.button(cid).on;
            if switch_row(ui, p, label, missing.then_some(t.not_on_mouse), &mut on) {
                if !on && self.cfg.triggers() == [cid] {
                    self.error = Some(t.need_one_button.into());
                } else {
                    self.apply(|c| c.button_mut(cid).on = on);
                }
            }
            if !self.cfg.button(cid).on {
                return;
            }
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
        if switch_row(ui, p, t.hotkeys_title, None, &mut on) {
            self.apply(|c| c.hotkeys.on = on);
            self.stop_recording();
        }
        hint(ui, p, t.hotkeys_hint);
        if crate::hotkey::unsupported_session() {
            // Wayland: the desktop's own shortcut settings run the command instead.
            let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "flockswitch".into());
            let commands: Vec<String> = (1..=3).map(|n| format!("{exe} switch {n}")).collect();
            ui.add_space(8.0);
            info_box(ui, p, &format!("{}\n\n{}", t.hotkeys_wayland, commands.join("\n")));
            ui.add_space(10.0);
            return;
        }
        if !self.cfg.hotkeys.on {
            ui.add_space(10.0);
            return;
        }
        ui.add_space(6.0);
        divider(ui, p);
        let width = (ui.available_width() * 0.66).min(300.0);
        let (mut start, mut clear) = (None, None);
        for n in 1..=3u8 {
            let key = self.cfg.hotkeys.key(n).to_string();
            let recording = self.recording == Some(n);
            let failed = !recording && !key.is_empty() && self.status.hotkeys_failed.contains(&n);
            let (shown, color) = if recording {
                (t.hotkey_press.to_string(), p.accent)
            } else if key.is_empty() {
                (t.hotkey_none.to_string(), p.muted)
            } else {
                (crate::hotkey::display(&key), if failed { p.amber } else { p.text })
            };
            labeled_row(ui, p, &format!("{} {n}", t.channel), width, ROW_SMALL, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if !recording && !key.is_empty() && button(ui, p, t.hotkey_clear, ButtonKind::Plain).clicked() {
                        clear = Some(n);
                    }
                    if !recording && button(ui, p, t.hotkey_change, ButtonKind::Plain).clicked() {
                        start = Some(n);
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new(shown).font(bold(14.0)).color(color));
                });
                false
            });
            if failed {
                hint(ui, p, t.hotkey_taken);
            }
        }
        hint(ui, p, t.hotkey_rule);
        ui.add_space(10.0);
        if let Some(n) = start {
            self.recording = Some(n);
            self.error = None;
            crate::hotkey::hold_pause();
        }
        if let Some(n) = clear {
            self.apply(|c| c.hotkeys.set_key(n, ""));
        }
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
            // The physical key: with ⌥ held, macOS turns 1 into ¡.
            let key = physical_key.unwrap_or(key);
            if key == egui::Key::Escape {
                self.stop_recording();
                return;
            }
            let Some(text) = shortcut_text(key, modifiers) else { continue };
            self.stop_recording();
            if (1..=3).any(|m| m != n && self.cfg.hotkeys.key(m) == text) {
                self.error = Some(t.hotkey_duplicate.into());
                return;
            }
            self.apply(|c| c.hotkeys.set_key(n, &text));
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
        if let Some(i) = segments(ui, p, "gesture-tabs", &labels, Role::Tab, SEG_SMALL, None, |i| gestures[i].0 == self.gesture_tab) {
            self.gesture_tab = gestures[i].0;
        }
        hint(ui, p, t.gesture_hint);
        let g = self.gesture_tab;
        if g == Gesture::Press {
            self.action_rows(ui, t, p, Slot::Press(cid));
            return;
        }
        ui.add_space(4.0);
        let mut on = moves.get(g).is_some_and(|m| m.on);
        if switch_row(ui, p, t.gesture_on, None, &mut on) {
            self.apply(|c| {
                if let Some(m) = c.button_mut(cid).moves.get_mut(g) {
                    m.on = on;
                }
            });
        }
        if on {
            self.action_rows(ui, t, p, Slot::Move(cid, g));
        }
    }

    /// 이동할 채널 of one slot: the one channel a press sends everything to. Settings are
    /// kept per computer, so this computer's own channel is shown but cannot be chosen.
    fn action_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette, slot: Slot) {
        let channels = slot_channels(&self.cfg, slot);
        let here = self.status.channel.filter(|c| (1..=3).contains(c));
        // Shown as chosen: where a press goes from here (older settings may list several).
        let target = match here {
            Some(c) => pick(&channels, c, 3),
            None => channels.first().copied(),
        };
        let numbers: Vec<String> = (1..=3).map(|n| n.to_string()).collect();
        let seg_width = (ui.available_width() * 0.5).min(210.0);
        let mut clicked = None;
        ui.add_space(2.0);
        labeled_row(ui, p, t.channels, seg_width, ROW_SMALL, |ui| {
            let locked = here.map(|c| c as usize - 1);
            clicked = segments(ui, p, &format!("channels-{slot:?}"), &numbers, Role::RadioButton, SEG_SMALL, locked, |i| {
                target == Some(i as u8 + 1)
            })
            .map(|i| i as u8 + 1);
            false
        });
        if let Some(c) = here {
            hint(ui, p, &format!("{} {c}: {}", t.channel, t.here_channel));
        }
        let Some(n) = clicked else { return };
        if channels == [n] {
            return;
        }
        self.apply(|c| {
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

    /// Language, start at login, and uninstall.
    fn general_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        ui.add_space(12.0);
        card(ui, p, |ui| {
            let mut language = self.cfg.language.clone();
            let choices = [("ko", "한국어"), ("en", "English")];
            let width = (ui.available_width() * 0.62).min(250.0);
            let mut chosen = language.as_str();
            if labeled_row(ui, p, t.language, width, ROW_NORMAL, |ui| segmented(ui, p, "language", &mut chosen, &choices, SEG_NORMAL)) {
                language = chosen.to_string();
                self.apply(|c| c.language = language);
            }
            divider(ui, p);
            if let Some(al) = &self.autostart {
                let mut on = self.autostart_on;
                if switch_row(ui, p, t.autostart, None, &mut on) {
                    let res = if on { al.enable() } else { al.disable() };
                    match res {
                        Ok(()) => self.autostart_on = on,
                        Err(e) => self.error = Some(e.to_string()),
                    }
                }
                divider(ui, p);
            }
            self.uninstall_row(ui, t, p);
        });
    }

    /// What uninstalling does on the left, the button (then confirm/cancel) on the right.
    fn uninstall_row(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let labels: &[(&str, ButtonKind)] = match self.uninstall {
            Uninstall::Confirming => &[(t.uninstall_confirm, ButtonKind::DangerFilled), (t.cancel, ButtonKind::Plain)],
            _ => &[(t.uninstall_button, ButtonKind::Danger)],
        };
        let buttons: f32 = labels.iter().map(|(l, k)| button_width(ui, l, *k)).sum::<f32>() + 8.0 * (labels.len() - 1) as f32;
        let width = ui.available_width();
        let text = ui.painter().layout(t.uninstall_explain.to_string(), FontId::proportional(12.5), p.muted, width - buttons - 16.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, (text.size().y + 20.0).max(48.0)), Sense::hover());
        ui.painter().galley(Pos2::new(rect.left(), rect.center().y - text.size().y / 2.0), text, p.muted);
        let area = Rect::from_min_max(
            Pos2::new(rect.right() - buttons, rect.center().y - 15.0),
            Pos2::new(rect.right(), rect.center().y + 15.0),
        );
        let mut clicked = None;
        ui.scope_builder(UiBuilder::new().max_rect(area), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            ui.horizontal(|ui| {
                for (i, (label, kind)) in labels.iter().enumerate() {
                    if button(ui, p, label, *kind).clicked() {
                        clicked = Some(i);
                    }
                }
            });
        });
        match (&self.uninstall, clicked) {
            (Uninstall::Idle, Some(_)) => self.uninstall = Uninstall::Confirming,
            (Uninstall::Confirming, Some(0)) => self.start_uninstall(),
            (Uninstall::Confirming, Some(_)) => self.uninstall = Uninstall::Idle,
            _ => {}
        }
    }

    /// Current version, the new one if any, and the automatic check.
    fn update_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let state = self.update.lock().unwrap().clone();
        let ctx = ui.ctx().clone();
        section(ui, p, t.section_update, |ui| {
            ui.add_space(10.0);
            let (status, color) = match &state {
                UpdateUi::Idle => (String::new(), p.muted),
                UpdateUi::Checking => (t.update_checking.to_string(), p.muted),
                UpdateUi::UpToDate => (t.update_up_to_date.to_string(), p.green),
                UpdateUi::Available(r) => (format!("{} {}", t.update_available, r.version), p.accent),
                UpdateUi::Failed(_) => (t.update_failed.to_string(), p.amber),
                UpdateUi::Downloading => (t.update_downloading.to_string(), p.muted),
                UpdateUi::Downloaded(_) => (t.update_downloaded.to_string(), p.green),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} {}", t.update_current, env!("CARGO_PKG_VERSION"))).size(14.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(status).size(13.0).color(color));
                });
            });
            if let UpdateUi::Failed(why) = &state {
                hint(ui, p, why);
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                match &state {
                    UpdateUi::Available(r) => {
                        if button(ui, p, t.update_download, ButtonKind::Primary).clicked() {
                            self.download_update(&ctx, r.clone());
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
                    UpdateUi::Checking | UpdateUi::Downloading => {}
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
            if switch_row(ui, p, t.update_auto, None, &mut on) {
                self.apply(|c| c.check_updates = on);
            }
            hint(ui, p, t.update_auto_hint);
            ui.add_space(10.0);
        });
    }

    /// The developer's address, with a ready-made mail and a GitHub link.
    fn report_section(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let copied = self.copied_at.is_some_and(|at| at.elapsed() < Duration::from_secs(2));
        section(ui, p, t.section_report, |ui| {
            ui.add_space(10.0);
            ui.label(RichText::new(t.report_desc).size(12.5).color(p.muted));
            ui.add_space(6.0);
            ui.label(RichText::new(update::REPORT_EMAIL).font(bold(14.0)).color(p.text));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
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

/// A titled card: the title sits above a rounded panel, as in the system settings.
fn section(ui: &mut Ui, p: Palette, title: &str, add: impl FnOnce(&mut Ui)) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new(title).font(bold(13.0)).color(p.muted));
    });
    ui.add_space(2.0);
    card(ui, p, add);
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

/// Something to fix before flockswitch can work, under the header.
fn warning(ui: &mut Ui, p: Palette, title: &str, text: &str) {
    ui.add_space(12.0);
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

/// A thin line between rows, edge to edge inside the card.
fn divider(ui: &mut Ui, p: Palette) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.center().y, Stroke::new(1.0, p.border));
}

fn hint(ui: &mut Ui, p: Palette, text: &str) {
    ui.add_space(2.0);
    ui.label(RichText::new(text).size(12.5).color(p.muted));
}

fn empty(ui: &mut Ui, p: Palette, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).size(13.0).color(p.muted));
    ui.add_space(8.0);
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

const ROW_NORMAL: f32 = 52.0;
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
    let size = if height < ROW_NORMAL { 13.5 } else { 14.0 };
    ui.painter().text(Pos2::new(rect.left(), rect.center().y), Align2::LEFT_CENTER, label, bold(size), p.text);
    let area = Rect::from_min_max(
        Pos2::new(rect.right() - width, rect.center().y - 15.0),
        Pos2::new(rect.right(), rect.center().y + 15.0),
    );
    ui.scope_builder(UiBuilder::new().max_rect(area), control).inner
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
const SEG_SMALL: SegSize = SegSize { height: 26.0, text: 13.0 };

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
    match segments(ui, p, name, &labels, Role::RadioButton, size, None, |i| options[i].0 == *value) {
        Some(i) if options[i].0 != *value => {
            *value = options[i].0;
            true
        }
        _ => false,
    }
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
    ui.painter().layout_no_wrap(label.to_string(), button_font(kind), Color32::WHITE).size().x + 28.0
}

fn button(ui: &mut Ui, p: Palette, label: &str, kind: ButtonKind) -> Response {
    let font = button_font(kind);
    let size = Vec2::new(button_width(ui, label, kind), 30.0);
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
