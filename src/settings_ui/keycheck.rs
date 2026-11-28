//! The key check window (2.5, `flockswitch keycheck`): opened on the master by the network
//! service when a key check is asked for on any computer of the group. It shows the
//! service's log (`Status::key_check`): each key pressed and what every other computer gets
//! for it. Closing it ends the check everywhere.
use super::*;
use flock_net::{KeyCheckEntry, KeyCheckStatus, Os};

/// The window gives up when the service shows no key check this long after it opened.
const NO_CHECK_GRACE: Duration = Duration::from_secs(4);

pub fn run(lock: std::net::TcpListener) -> Result<()> {
    if let Ok(cfg) = Config::load_or_create() {
        crate::i18n::set_language(&cfg.language);
    }
    let t = texts();
    let icon = egui::IconData { rgba: ICON_RGBA.to_vec(), width: ICON_SIDE, height: ICON_SIDE };
    let viewport = egui::ViewportBuilder::default()
        .with_title(t.keycheck_title)
        .with_icon(icon)
        .with_inner_size([620.0, 640.0])
        .with_min_inner_size([480.0, 420.0])
        .with_active(true);
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native(
        "flockswitch-keycheck",
        options,
        Box::new(|cc| {
            install_fonts(&cc.egui_ctx);
            install_style(&cc.egui_ctx);
            // Asked again while open: come to the front.
            let raise = Arc::new(AtomicBool::new(false));
            let (ctx, flag) = (cc.egui_ctx.clone(), raise.clone());
            std::thread::spawn(move || {
                for _ in lock.incoming() {
                    flag.store(true, Ordering::Relaxed);
                    ctx.request_repaint();
                }
            });
            Ok(Box::new(KeyCheckApp { raise, opened: Instant::now(), last_read: None, check: None, closing: false }))
        }),
    )
    .map_err(|e| anyhow!("cannot open the key check window: {e}"))
}

struct KeyCheckApp {
    raise: Arc<AtomicBool>,
    opened: Instant,
    last_read: Option<Instant>,
    check: Option<KeyCheckStatus>,
    closing: bool,
}

fn request(action: &str) {
    let request = flock_net::Request::Action { action: action.into() };
    if let Err(e) = flock_net::write_request(crate::net::config_dir(), &request) {
        log::warn!("key check: {e:#}");
    }
}

/// "Windows", "macOS", "Linux" or "Android".
pub(super) fn os_name(os: Os) -> &'static str {
    match os {
        Os::Mac => "macOS",
        Os::Windows => "Windows",
        Os::Linux => "Linux",
        Os::Android => "Android",
    }
}

impl eframe::App for KeyCheckApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let t = texts();
        if self.raise.swap(false, Ordering::Relaxed) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if self.last_read.is_none_or(|at| at.elapsed() >= Duration::from_millis(150)) {
            self.last_read = Some(Instant::now());
            self.check = flock_net::read_status(crate::net::config_dir()).ok().and_then(|s| s.key_check).filter(|k| k.open);
        }
        ctx.request_repaint_after(Duration::from_millis(150));
        // Closed here, or the check ended elsewhere (the app quit): close the window.
        if ctx.input(|i| i.viewport().close_requested()) && !self.closing {
            self.closing = true;
            request("keycheck_close");
        }
        if self.check.is_none() && self.opened.elapsed() >= NO_CHECK_GRACE && !self.closing {
            self.closing = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        let p = Palette::new(ui.visuals().dark_mode);
        let panel = egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(20, 16));
        let mut close = false;
        egui::CentralPanel::default().frame(panel).show(ui, |ui| {
            ui.label(RichText::new(t.keycheck_title).font(bold(18.0)).color(p.text));
            ui.add_space(6.0);
            ui.label(RichText::new(t.keycheck_help).size(13.0).color(p.muted));
            ui.add_space(10.0);
            let probing = self.check.as_ref().is_some_and(|k| k.probing);
            ui.horizontal(|ui| {
                if probing {
                    ui.label(RichText::new(t.keycheck_probing).size(13.5).color(p.text));
                } else if button(ui, p, t.keycheck_probe, ButtonKind::Primary).clicked() {
                    request("keycheck_probe");
                }
            });
            ui.add_space(12.0);
            let entries = self.check.as_ref().map(|k| k.entries.as_slice()).unwrap_or_default();
            egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 48.0).show(ui, |ui| {
                if entries.is_empty() {
                    ui.label(RichText::new(t.keycheck_empty).size(13.5).color(p.muted));
                }
                for entry in entries.iter().rev() {
                    entry_card(ui, p, t, entry);
                    ui.add_space(8.0);
                }
            });
            ui.add_space(10.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close = button(ui, p, t.close, ButtonKind::Plain).clicked();
            });
        });
        if close && !self.closing {
            self.closing = true;
            request("keycheck_close");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

/// One key: where it was pressed, then a row per other computer.
fn entry_card(ui: &mut Ui, p: Palette, t: &Texts, entry: &KeyCheckEntry) {
    card_pad(ui, p, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{} ({})", entry.from, os_name(entry.from_os))).size(13.0).color(p.muted));
            ui.label(RichText::new(&entry.key).font(bold(15.0)).color(p.text));
            if entry.released {
                ui.label(RichText::new(t.keycheck_released).size(12.5).color(p.muted));
            }
        });
        ui.add_space(4.0);
        for target in &entry.to {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("→").size(13.0).color(p.muted));
                ui.label(RichText::new(format!("{} ({})", target.name, os_name(target.os))).size(13.0).color(p.text));
                let keys = if target.keys.is_empty() { t.keycheck_nothing } else { target.keys.as_str() };
                ui.label(RichText::new(keys).font(bold(14.0)).color(if target.keys.is_empty() { p.muted } else { p.text }));
                if target.live {
                    ui.label(RichText::new(t.keycheck_live).size(12.5).color(p.muted));
                }
            });
        }
    });
}
