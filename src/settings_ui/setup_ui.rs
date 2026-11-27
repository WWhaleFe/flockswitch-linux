//! The Linux setup check card and approval modal (udev rule, desktop portal, Korean font,
//! GNOME tray extension, app menu entry, Homebrew update).

use super::*;
use crate::setup;
use crate::setup_logic::{self, After, Item, ItemId, State as SetupState};
use std::collections::HashMap;

/// The setup check: what is missing, and the fixes the user approved.
#[derive(Default)]
pub(super) struct SetupUi {
    /// What the last check found (None until the first one is done).
    pub(super) found: Arc<Mutex<Option<Vec<Item>>>>,
    /// What the last check was started with; a change starts another.
    pub(super) asked: Option<setup::Inputs>,
    pub(super) checking: Arc<AtomicBool>,
    /// A fix finished: check again.
    pub(super) stale: Arc<AtomicBool>,
    /// The fixes run from this window and how they went, shown until it closes.
    pub(super) runs: Arc<Mutex<HashMap<ItemId, SetupRun>>>,
    /// The items whose commands wait for Run or Cancel: one, or all for "Install all".
    pub(super) confirm: Option<Vec<Item>>,
    /// Whose commands were just copied ("Copied" for a moment).
    pub(super) copied: Option<(ItemId, Instant)>,
    /// The Korean font installed from here was loaded.
    pub(super) fonts_reloaded: bool,
    /// The running flockswitch had a device open since this window opened.
    pub(super) devices_seen: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum SetupRun {
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

impl SettingsApp {
    /// What the setup check takes from the running flockswitch and the settings.
    fn setup_inputs(&mut self) -> setup::Inputs {
        self.setup.devices_seen |= !self.status.online.is_empty();
        let newer = match &*self.update.lock().unwrap() {
            UpdateUi::Available(r) | UpdateUi::InstallFailed(r, _) => Some(r.version.clone()),
            _ => self.status.update_available.clone(),
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
    pub(super) fn setup_tick(&mut self, ctx: &egui::Context) {
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

    pub(super) fn setup_items(&self) -> Vec<Item> {
        self.setup.found.lock().unwrap().clone().unwrap_or_default()
    }

    pub(super) fn setup_item(&self, id: ItemId) -> Option<Item> {
        self.setup_items().into_iter().find(|i| i.id == id)
    }

    /// How many missing items are currently pending (not set aside, not already fixed),
    /// for the badge next to General in the sidebar.
    pub(super) fn setup_pending_count(&self) -> usize {
        let items = self.setup_items();
        let snoozed = &self.cfg.setup.snoozed;
        let runs = self.setup.runs.lock().unwrap();
        items
            .iter()
            .filter(|i| {
                i.state == SetupState::Missing
                    && !setup_logic::is_snoozed(i, snoozed)
                    && !matches!(runs.get(&i.id), Some(SetupRun::Done(_) | SetupRun::BrewInstalled))
            })
            .count()
    }

    /// At the top of the General page, only while something is missing or set aside: a row
    /// for each item with why it is needed, Install (which first shows the commands) and
    /// Later. A fix run from here keeps its row, with how it went, until the window closes.
    pub(super) fn setup_card(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
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
            .filter(|i| i.id == ItemId::Network || runs.contains_key(&i.id) || (i.state == SetupState::Missing && !setup_logic::is_snoozed(i, &snoozed)))
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
                gap(ui);
                if set_aside_line(ui, p, t, hidden) {
                    action = Some(SetupAction::ShowAgain);
                }
            }
        } else {
            gap(ui);
            section_head(ui, p, t.setup_title, Some(t.setup_info), |_| {});
            card(ui, p, |ui| {
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
    pub(super) fn setup_confirm_ui(&mut self, ctx: &egui::Context, p: Palette) {
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
            .corner_radius(16)
            .inner_margin(Margin::same(18))
            .shadow(ctx.global_style().visuals.window_shadow);
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
            // Several items: shorter boxes, so the dialog fits the fixed window.
            let (admin_height, user_height) = if items.len() == 1 { (260.0, 140.0) } else { (200.0, 100.0) };
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
        ItemId::GnomeHelper if matches!(item.why, Some(crate::setup_logic::Why::HelperEnable)) => t.setup_turn_on,
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
        None if item.id == ItemId::Network => {}
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
        clicked = ui
            .add(egui::Label::new(RichText::new(t.setup_show_again).size(12.5).color(p.text).underline()).sense(Sense::click()))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked();
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
