//! The keyboard page: how keys are changed when the cursor crosses to a computer with
//! another operating system ("OS 전환 시"), and the table comparing the systems.

use super::*;
use crate::i18n::fill;
use crate::settings_logic::{self as logic, KeyEdit};
use flock_net::{HangulKey, ModMode, Os};

/// The name of a modifier of `kind` (0 Ctrl, 2 Alt, 3 Win/Command) on a system.
fn mod_name(os: Os, kind: u16) -> &'static str {
    match (os, kind) {
        (Os::Mac, 0) => "Control",
        (Os::Mac, 2) => "Option",
        (Os::Mac, _) => "Command",
        (_, 0) => "Ctrl",
        (_, 2) => "Alt",
        (Os::Windows, _) => "Windows",
        (Os::Linux, _) => "Super",
        (Os::Android, _) => "Meta",
    }
}

fn hangul_name(key: HangulKey, t: &Texts) -> &'static str {
    match key {
        HangulKey::CapsLock => "Caps Lock",
        HangulKey::ControlSpace => "Control+Space",
        HangulKey::ShiftSpace => "Shift+Space",
        HangulKey::SuperSpace => "Super+Space",
        HangulKey::Lang1 => t.hk_lang1,
        HangulKey::RightAlt => t.hk_right_alt,
    }
}

fn hanja_name(os: Os, t: &Texts) -> &'static str {
    match os {
        Os::Mac => "Option+Return",
        Os::Linux => t.hk_hanja_f9,
        _ => t.hk_hanja,
    }
}

fn keycmp_viewport() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("keys-compare")
}

/// A pane heading: a small bold line with room above it.
fn pane_head(ui: &mut Ui, p: Palette, text: &str, first: bool) {
    ui.add_space(if first { 14.0 } else { 20.0 });
    ui.label(RichText::new(text).font(bold(13.0)).color(p.text));
    ui.add_space(2.0);
}

/// A switch row whose second line is drawn by `help` (keycaps joined by arrows).
fn rich_switch_row(ui: &mut Ui, p: Palette, label: &str, height: f32, on: &mut bool, help: impl FnOnce(&mut Ui)) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| WidgetInfo::selected(Role::Switch, true, *on, label));
    if resp.clicked() {
        *on = !*on;
    }
    let painter = ui.painter();
    painter.text(rect.left_top() + Vec2::new(0.0, 9.0), Align2::LEFT_TOP, label, FontId::proportional(14.5), p.text);
    let how_on = ui.ctx().animate_bool_responsive(resp.id, *on);
    paint_switch(painter, p, Rect::from_center_size(Pos2::new(rect.right() - 20.0, rect.center().y), Vec2::new(40.0, 24.0)), how_on);
    let area = Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 32.0), Vec2::new(rect.width() - 60.0, height - 32.0));
    ui.new_child(UiBuilder::new().max_rect(area).layout(egui::Layout::left_to_right(egui::Align::Min))).scope(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(5.0, 2.0);
        help(ui);
    });
    resp.clicked()
}

/// `a -> b` as two keycaps.
fn key_arrow(ui: &mut Ui, p: Palette, a: &str, b: &str) {
    keycap(ui, p, a);
    ui.label(RichText::new("→").size(12.5).color(p.muted));
    keycap(ui, p, b);
}

impl SettingsApp {
    pub(super) fn keys_page(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        if page_header(ui, p, t.nav_keys, t.help_keys, Some(t.keys_cmp_button)) {
            self.keys_cmp = true;
        }
        // A sub may edit too: its change goes to the master, which hands it to everyone.
        if self.follows_master() {
            gap(ui);
            let note = if self.cfg.pending_key_rules.is_some() { t.keys_sync_pending } else { t.keys_note_sub };
            note_box(ui, p, NoteKind::Info, note, None);
        }
        gap(ui);
        section_head(ui, p, t.keys_section, None, |ui| head_note(ui, t.keys_section_note, p.muted));
        card_pad(ui, p, |ui| self.key_pane(ui, t, p));
    }

    /// The tabs (the system to pair with, the direction) and the rules of that direction.
    fn key_pane(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let me = Os::Linux;
        let pairs = logic::pair_systems(me);
        self.key_pair = self.key_pair.min(pairs.len() - 1);
        let other = pairs[self.key_pair];
        let dirs = logic::directions(me, other);
        self.key_dir = self.key_dir.min(dirs.len() - 1);

        let width = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, CONTROL_HEIGHT + 2.0), Sense::hover());
        let left_w = (width * 0.56).min(pairs.len() as f32 * 150.0);
        let right_w = (dirs.len() as f32 * 190.0).min(width - left_w - 16.0);
        let pair_labels: Vec<String> = pairs.iter().map(|o| fill(t.keys_pair_tab, &[os_label(*o)])).collect();
        let dir_labels: Vec<String> = dirs.iter().map(|(a, b)| format!("{} → {}", os_label(*a), os_label(*b))).collect();
        let mut l = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_size(rect.min, Vec2::new(left_w, rect.height()))));
        if let Some(i) = segments(&mut l, p, "key-pairs", &pair_labels, Role::Tab, SEG_SMALL, &[], None, |i| i == self.key_pair) {
            self.key_pair = i;
            self.key_dir = 0;
        }
        let mut r = ui.new_child(UiBuilder::new().max_rect(Rect::from_min_size(Pos2::new(rect.right() - right_w, rect.top()), Vec2::new(right_w, rect.height()))));
        if let Some(i) = segments(&mut r, p, "key-dirs", &dir_labels, Role::Tab, SEG_SMALL, &[], None, |i| i == self.key_dir) {
            self.key_dir = i;
        }
        let (src, dst) = dirs[self.key_dir.min(dirs.len() - 1)];
        self.key_rows(ui, t, p, src, dst);
    }

    /// Every rule of one direction, as rows.
    fn key_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette, src: Os, dst: Os) {
        let Some(rules) = self.cfg.network.rules_for(src, dst) else { return };
        let cross = (src == Os::Mac) != (dst == Os::Mac);
        let mut edits: Vec<KeyEdit> = Vec::new();

        pane_head(ui, p, t.keys_mods_head, true);
        if cross {
            let modes = [(ModMode::Function, t.keys_mode_function), (ModMode::Position, t.keys_mode_position), (ModMode::Off, t.keys_mode_off)];
            let help = match rules.mod_mode {
                ModMode::Function => t.keys_mode_help_function,
                ModMode::Position => t.keys_mode_help_position,
                ModMode::Off => t.keys_mode_help_off,
            };
            let mut mode = rules.mod_mode;
            if labeled_row_help(ui, p, t.keys_mode, Some(help), 340.0, |ui| segmented(ui, p, &format!("key-mode-{src:?}{dst:?}"), &mut mode, &modes, SEG_SMALL)) {
                edits.push(KeyEdit::ModMode(mode));
            }
            divider(ui, p);
        }
        // The table: each modifier of the sender and what the receiver gets.
        let help = fill(t.keys_table_help, &[os_label(src), os_label(dst)]);
        let fixed = cross && rules.mod_mode == ModMode::Off;
        row_with_help(ui, p, t.keys_table, Some(&help), None, 290.0, ROW_SMALL, 3.0 * 34.0, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            for kind in logic::bottom_row(src) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(86.0, 28.0), Sense::hover());
                    let name = mod_name(src, kind);
                    let galley = ui.painter().layout_no_wrap(name.to_string(), mono(12.0), p.text);
                    let cap = Rect::from_center_size(rect.right_center() - Vec2::new(galley.size().x / 2.0 + 7.0, 0.0), Vec2::new(galley.size().x + 14.0, 22.0));
                    ui.painter().rect_filled(cap, 5, p.card);
                    ui.painter().rect_stroke(cap, 5, Stroke::new(1.0, p.track), StrokeKind::Inside);
                    ui.painter().galley(cap.center() - galley.size() / 2.0, galley, p.text);
                    ui.label(RichText::new("→").size(12.5).color(p.muted));
                    if fixed {
                        keycap(ui, p, mod_name(dst, kind));
                        return;
                    }
                    let mut target = logic::mod_target(src, dst, &rules, kind);
                    let options: Vec<(u16, &str)> = logic::bottom_row(dst).into_iter().map(|k| (k, mod_name(dst, k))).collect();
                    if combo_w(ui, &format!("key-mod-{src:?}{dst:?}{kind}"), &mut target, &options, 130.0) {
                        edits.push(KeyEdit::Modifier { from: kind, to: target });
                    }
                });
            }
            false
        });

        if src != Os::Android {
            pane_head(ui, p, t.keys_hangul_head, false);
            let sources = logic::hangul_sources(src);
            let labels: Vec<String> = sources.iter().map(|k| hangul_name(*k, t).to_string()).collect();
            let help = fill(t.keys_hsrc_help, &[os_label(dst)]);
            let mut picked = None;
            labeled_row_help(ui, p, &fill(t.keys_hsrc, &[os_label(src)]), Some(&help), 380.0, |ui| {
                picked = segments(ui, p, &format!("key-hsrc-{src:?}{dst:?}"), &labels, Role::CheckBox, SEG_SMALL, &[], None, |i| rules.hangul_src.contains(&sources[i]));
                false
            });
            if let Some(i) = picked {
                edits.push(KeyEdit::HangulSource(sources[i], !rules.hangul_src.contains(&sources[i])));
            }
            divider(ui, p);

            let targets = logic::hangul_targets(dst);
            let labels: Vec<String> = targets.iter().map(|k| hangul_name(*k, t).to_string()).collect();
            let help = match dst {
                Os::Linux => Some(t.keys_hdst_help_linux),
                Os::Android => Some(t.keys_hdst_help_android),
                Os::Mac => Some(t.keys_hdst_help_mac),
                Os::Windows => None,
            };
            let mut picked = None;
            labeled_row_help(ui, p, &fill(t.keys_hsrc, &[os_label(dst)]), help, 380.0, |ui| {
                picked = segments(ui, p, &format!("key-hdst-{src:?}{dst:?}"), &labels, Role::RadioButton, SEG_SMALL, &[], None, |i| rules.hangul_dst == targets[i]);
                false
            });
            if let Some(i) = picked {
                edits.push(KeyEdit::HangulTarget(targets[i]));
            }

            if src == Os::Mac && rules.hangul_src.contains(&HangulKey::CapsLock) {
                divider(ui, p);
                let mut on = rules.caps_long_press;
                if switch_row(ui, p, t.keys_caps_long, Some(t.keys_caps_long_help), &mut on) {
                    edits.push(KeyEdit::CapsLongPress(on));
                }
            }
            if logic::has_hanja(src) && logic::has_hanja(dst) {
                divider(ui, p);
                let help = format!("{} {} → {} {}", os_label(src), hanja_name(src, t), os_label(dst), hanja_name(dst, t));
                let mut on = rules.hanja;
                if switch_row(ui, p, t.keys_hanja, Some(&help), &mut on) {
                    edits.push(KeyEdit::Hanja(on));
                }
            }
        }

        if cross && dst != Os::Android {
            pane_head(ui, p, t.keys_nav_head, false);
            let nav: [(&str, &str, &str); 4] = if src == Os::Mac {
                [
                    ("⌘← / ⌘→", "Home / End", t.nav_line),
                    ("⌥← / ⌥→", "Ctrl+← / →", t.nav_word),
                    ("⌘↑ / ⌘↓", "Ctrl+Home / End", t.nav_doc),
                    ("⌘⌫", "Shift+Home, ⌫", t.nav_del_line),
                ]
            } else {
                [
                    ("Home / End", "⌘← / ⌘→", t.nav_line),
                    ("Ctrl+← / →", "⌥← / ⌥→", t.nav_word),
                    ("Ctrl+Home / End", "⌘↑ / ⌘↓", t.nav_doc),
                    ("Ctrl+⌫", "⌥⌫", t.nav_del_word),
                ]
            };
            for (i, (from, to, label)) in nav.into_iter().enumerate() {
                if i > 0 {
                    divider(ui, p);
                }
                let mut on = rules.nav[i];
                if rich_switch_row(ui, p, label, 58.0, &mut on, |ui| key_arrow(ui, p, from, to)) {
                    edits.push(KeyEdit::Nav(i, on));
                }
            }
            pane_head(ui, p, t.keys_capture_head, false);
            let shot = |windows: bool| if windows { "Win+Shift+S" } else { "Shift+Print Screen" };
            let caps: [(&str, String); 2] = if src == Os::Mac {
                [("⌘⇧3", "Print Screen".into()), ("⌘⇧4", shot(dst == Os::Windows).into())]
            } else {
                [("Print Screen", "⌘⇧3".into()), (shot(src == Os::Windows), "⌘⇧4".into())]
            };
            let mut on = rules.capture;
            if rich_switch_row(ui, p, t.keys_capture, 84.0, &mut on, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (i, (a, b)) in caps.iter().enumerate() {
                        if i > 0 {
                            ui.label(RichText::new("·").color(p.muted));
                        }
                        key_arrow(ui, p, a, b);
                    }
                });
            }) {
                edits.push(KeyEdit::Capture(on));
            }
        }
        if dst == Os::Android {
            ui.add_space(14.0);
            ui.label(RichText::new(fill(t.keys_android_note, &[os_label(src)])).size(12.5).color(p.muted));
        }
        for edit in edits {
            self.edit_key_rules(src, dst, edit);
        }
    }

    /// Saves one change to the rules. A sub also sends the rules to its master.
    fn edit_key_rules(&mut self, src: Os, dst: Os, edit: KeyEdit) {
        self.apply(|c| {
            let before = c.network.all_rules();
            logic::edit_keys(&mut c.network, src, dst, edit);
            if c.network.role == flock_net::Role::Sub && !c.network.peers.is_empty() {
                // Only the pairs this edit changed go to the master, which merges them per pair.
                let changed: Vec<_> = c.network.all_rules().into_iter()
                    .filter(|(pair, rule)| before.get(pair) != Some(rule))
                    .collect();
                if !changed.is_empty() {
                    c.pending_key_rules.get_or_insert_with(Default::default).extend(changed);
                }
            }
        });
    }

    /// "Compare keys by system": a window with the table of what each system calls what.
    pub(super) fn keycmp_window(&mut self, ctx: &egui::Context, t: &Texts) {
        if !self.keys_cmp {
            return;
        }
        let builder = egui::ViewportBuilder::default()
            .with_title(window_title(t.keys_cmp_button))
            .with_inner_size([820.0, 560.0])
            .with_min_inner_size([620.0, 400.0]);
        ctx.show_viewport_immediate(keycmp_viewport(), builder, |ui, _| {
            if ui.ctx().input(|i| i.viewport().close_requested()) {
                self.keys_cmp = false;
                return;
            }
            let p = Palette::new(ui.visuals().dark_mode);
            let panel = egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(20, 16));
            let mut close = false;
            egui::CentralPanel::default().frame(panel).show(ui, |ui| {
                ui.label(RichText::new(t.keys_cmp_button).font(bold(17.0)).color(p.text));
                ui.add_space(12.0);
                egui::ScrollArea::vertical().auto_shrink([false, false]).max_height(ui.available_height() - 48.0).show(ui, |ui| {
                    comparison_table(ui, p, t);
                    ui.add_space(12.0);
                    ui.label(RichText::new(t.keys_cmp_note).size(12.5).color(p.muted));
                });
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    close = button(ui, p, t.close, ButtonKind::Plain).clicked();
                });
            });
            if close {
                self.keys_cmp = false;
            }
        });
    }
}

/// The comparison table: a row per kind of key, a column per system.
fn comparison_table(ui: &mut Ui, p: Palette, t: &Texts) {
    let width = ui.available_width() - 8.0;
    let label_w = 130.0;
    let col_w = (width - label_w) / 4.0;
    let (header, _) = ui.allocate_exact_size(Vec2::new(width, 28.0), Sense::hover());
    for (i, os) in [Os::Mac, Os::Windows, Os::Linux, Os::Android].into_iter().enumerate() {
        ui.painter().text(
            Pos2::new(header.left() + label_w + i as f32 * col_w, header.center().y),
            Align2::LEFT_CENTER,
            os_label(os),
            bold(13.0),
            p.muted,
        );
    }
    for (row, label) in t.keys_cmp_labels.iter().enumerate() {
        let cells = &t.keys_cmp_cells[row];
        let galleys: Vec<_> = cells.iter().map(|c| ui.painter().layout(c.to_string(), FontId::proportional(13.0), p.text, col_w - 12.0)).collect();
        let label_g = ui.painter().layout(label.to_string(), bold(13.0), p.text, label_w - 12.0);
        let height = galleys.iter().map(|g| g.size().y).fold(label_g.size().y, f32::max) + 18.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        ui.painter().hline(rect.x_range(), rect.top(), Stroke::new(1.0, p.border));
        ui.painter().galley(Pos2::new(rect.left(), rect.top() + 9.0), label_g, p.text);
        for (i, g) in galleys.into_iter().enumerate() {
            ui.painter().galley(Pos2::new(rect.left() + label_w + i as f32 * col_w, rect.top() + 9.0), g, p.text);
        }
    }
}
