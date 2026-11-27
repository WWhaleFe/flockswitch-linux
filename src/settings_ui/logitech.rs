//! The Logitech pages (buttons, shortcuts, edges, devices) and the app pages (effects,
//! general) of the settings window.

use super::*;
use crate::setup_logic::ItemId;

impl SettingsApp {
    /// The note a sub shows once, at the top of a page whose settings the master decides.
    fn follow_note(&self, ui: &mut Ui, t: &Texts, p: Palette) {
        if self.follows_master() {
            gap(ui);
            self.follow_banner(ui, p, t);
        }
    }

    // ---- Buttons ----------------------------------------------------------------

    pub(super) fn buttons_page(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        page_header(ui, p, t.nav_buttons, t.help_buttons, None);
        gap(ui);
        note_box(ui, p, NoteKind::Info, t.switching_help, None);
        if self.cfg.network.on && self.cfg.enabled {
            ui.add_space(8.0);
            if note_box(ui, p, NoteKind::Warn, t.easy_switch_hint, Some(t.easy_switch_off)) {
                self.apply(|c| c.enabled = false);
            }
        }
        gap(ui);
        let here = self.status.channel.filter(|c| (1..=3).contains(c));
        card(ui, p, |ui| {
            let mut enabled = self.cfg.enabled;
            let help = here.map(|c| format!("{} {c}: {}", t.channel, t.here_channel));
            if switch_row(ui, p, t.enabled, help.as_deref(), &mut enabled) {
                self.apply(|c| c.enabled = enabled);
            }
        });

        gap(ui);
        section_head(ui, p, t.buttons_section, None, |_| {});
        card(ui, p, |ui| {
            let known = self.status.trigger.is_some();
            let buttons = self.status.buttons.clone();
            let missing = move |cid: u16| known && !buttons.contains(&cid);
            // The wheel-mode button, and putting the wheel back when the mouse returns.
            let help = missing(BUTTON_WHEEL_MODE).then_some(t.not_on_mouse);
            self.channel_row(ui, t, p, t.button_wheel_mode, help, Slot::Press(BUTTON_WHEEL_MODE));
            if self.cfg.button(BUTTON_WHEEL_MODE).on {
                divider(ui, p);
                let mut keep = self.cfg.keep_wheel_mode;
                let (flipped, label) = switch_row_full(ui, p, t.keep_wheel_mode, Some(t.keep_wheel_row_help), &mut keep);
                info_icon(ui, p, Pos2::new(label.right() + 16.0, label.center().y), "keep-wheel-help", t.keep_wheel_mode_help);
                if flipped {
                    self.apply(|c| c.keep_wheel_mode = keep);
                }
            }
            divider(ui, p);
            // The gesture button: press and four directions, each with its own channel.
            let gestures = [
                (Gesture::Press, t.gesture_press),
                (Gesture::Up, "↑"),
                (Gesture::Down, "↓"),
                (Gesture::Left, "←"),
                (Gesture::Right, "→"),
            ];
            let moves = self.cfg.button(BUTTON_GESTURE).moves.clone();
            let labels: Vec<String> = gestures
                .iter()
                .map(|(g, l)| {
                    let on = if *g == Gesture::Press { self.cfg.button(BUTTON_GESTURE).on } else { moves.get(*g).is_some_and(|m| m.on) };
                    if on { format!("{l} ✓") } else { l.to_string() }
                })
                .collect();
            let mut picked = None;
            let help = if missing(BUTTON_GESTURE) { t.not_on_mouse } else { t.gesture_row_help };
            labeled_row_help(ui, p, t.button_gesture, Some(help), 340.0, |ui| {
                picked = segments(ui, p, "gesture-tabs", &labels, Role::Tab, SEG_SMALL, &[], None, |i| gestures[i].0 == self.gesture_tab);
                false
            });
            if let Some(i) = picked {
                self.gesture_tab = gestures[i].0;
            }
            hint(ui, p, t.gesture_hint);
            let g = self.gesture_tab;
            let slot = if g == Gesture::Press { Slot::Press(BUTTON_GESTURE) } else { Slot::Move(BUTTON_GESTURE, g) };
            self.channel_row(ui, t, p, t.channels, None, slot);
            divider(ui, p);
            let help = if missing(BUTTON_MIDDLE) { t.not_on_mouse } else { t.middle_row_help };
            self.channel_row(ui, t, p, t.button_middle_short, Some(help), Slot::Press(BUTTON_MIDDLE));
            hint(ui, p, t.buttons_hint);
            ui.add_space(8.0);
        });
    }

    /// A row choosing where one press goes: Off, or a channel (this computer's own is
    /// shown but locked).
    fn channel_row(&mut self, ui: &mut Ui, t: &Texts, p: Palette, label: &str, help: Option<&str>, slot: Slot) {
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
        let mut clicked = None;
        labeled_row_help(ui, p, label, help, 250.0, |ui| {
            // Segment 0 is "Off"; this computer's own channel is shown but locked.
            let locked: Vec<usize> = here.map(|c| c as usize).into_iter().collect();
            clicked = segments(ui, p, &format!("channels-{slot:?}"), &labels, Role::RadioButton, SEG_SMALL, &locked, Some(0), |i| {
                if i == 0 { !is_on } else { target == Some(i as u8) }
            });
            false
        });
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

    // ---- Shortcuts --------------------------------------------------------------

    pub(super) fn hotkeys_page(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        page_header(ui, p, t.nav_hotkeys, t.help_hotkeys, None);
        gap(ui);
        card(ui, p, |ui| {
            let mut on = self.cfg.hotkeys.on;
            let (flipped, label) = switch_row_full(ui, p, t.hotkeys_title, Some(t.hotkeys_linux_help), &mut on);
            info_icon(ui, p, Pos2::new(label.right() + 16.0, label.center().y), "hotkeys-help", t.hotkey_help);
            if flipped {
                self.apply(|c| c.hotkeys.on = on);
                self.stop_recording();
            }
            hint(ui, p, t.hotkeys_hint);
            ui.add_space(8.0);
        });
        if crate::edge_wayland::session_is_wayland() && self.cfg.hotkeys.on {
            // Wayland: registered through the system's shortcut service; how that stands.
            gap(ui);
            self.portal_box(ui, t, p, false);
        }
        if self.cfg.hotkeys.on {
            gap(ui);
            card(ui, p, |ui| {
                for n in 1..=3u8 {
                    if n > 1 {
                        divider(ui, p);
                    }
                    self.shortcut_row(ui, t, p, n, &format!("{} {n}", t.channel), None);
                }
            });
        }
    }

    // ---- Edges ------------------------------------------------------------------

    pub(super) fn edges_page(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        page_header(ui, p, t.nav_edges, t.help_edges, None);
        if self.cfg.network.on {
            gap(ui);
            note_box(ui, p, NoteKind::Info, t.edges_network_hint, None);
        }
        self.follow_note(ui, t, p);
        if !crate::edge::platform::x11_available() {
            // Wayland: what the running flockswitch found it can use here.
            gap(ui);
            let (text, problem) = edge_backend_text(t, &self.status.edge_backend, &self.status.edge_note, self.cfg.edges.require_control);
            let open_setup = (problem && self.setup_item(ItemId::Portal).is_some_and(|i| i.actionable())).then_some(t.setup_open);
            let kind = if problem { NoteKind::Error } else { NoteKind::Info };
            if note_box(ui, p, kind, &text, open_setup) {
                self.page = Page::General;
                self.stop_recording();
            }
            if self.status.edge_backend == "none" {
                return;
            }
        }
        gap(ui);
        card(ui, p, |ui| {
            let mut on = self.cfg.edges.on;
            if switch_row(ui, p, t.edges_title, Some(t.edges_hint), &mut on) {
                if on && self.cfg.network.on {
                    self.edge_confirm = true;
                } else {
                    self.apply(|c| c.edges.on = on);
                }
            }
        });
        gap(ui);
        // While the network has the edges, these are shown but off.
        let editable = !self.cfg.network.on;
        ui.add_enabled_ui(editable, |ui| {
            two_cols(ui, "edges", |left, right| {
                section_head(left, p, t.edges_sides, None, |_| {});
                card(left, p, |ui| self.edge_sides(ui, t, p));
                section_head(right, p, t.edges_method, None, |_| {});
                card(right, p, |ui| self.edge_method(ui, t, p));
            });
        });
    }

    /// The channel each screen edge goes to.
    fn edge_sides(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let here = self.status.channel.filter(|c| (1..=3).contains(c));
        let labels: Vec<String> = [t.off.to_string()].into_iter().chain((1..=3).map(|n| n.to_string())).collect();
        let sides = [(Edge::Left, t.edge_left), (Edge::Right, t.edge_right), (Edge::Top, t.edge_top), (Edge::Bottom, t.edge_bottom)];
        for (i, (edge, label)) in sides.into_iter().enumerate() {
            if i > 0 {
                divider(ui, p);
            }
            let current = self.cfg.edges.channel(edge);
            let mut clicked = None;
            labeled_row(ui, p, label, 210.0, ROW_SMALL, |ui| {
                // Segment 0 is "off"; this computer's own channel is shown but locked.
                let locked: Vec<usize> = here.map(|c| c as usize).into_iter().collect();
                clicked = segments(ui, p, &format!("edge-{edge:?}"), &labels, Role::RadioButton, SEG_SMALL, &locked, Some(0), |i| i as u8 == current);
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
        ui.add_space(8.0);
    }

    /// How the edge switch behaves: the wait, putting the cursor back, Control.
    fn edge_method(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let choices: Vec<(u64, String)> = [(300, "0.3"), (500, "0.5"), (1000, "1"), (2000, "2")]
            .into_iter()
            .map(|(ms, text)| (ms, format!("{text}{}", t.seconds_short)))
            .collect();
        let choices: Vec<(u64, &str)> = choices.iter().map(|(ms, s)| (*ms, s.as_str())).collect();
        let mut dwell = self.cfg.edges.dwell_ms;
        if labeled_row(ui, p, t.edge_dwell, 260.0, ROW_SMALL, |ui| segmented(ui, p, "edge-dwell", &mut dwell, &choices, SEG_SMALL)) {
            self.apply(|c| c.edges.dwell_ms = dwell);
        }
        divider(ui, p);
        let mut place = self.cfg.edges.place_cursor;
        let place_help = if crate::edge::platform::x11_available() || self.status.edge_backend == "gnome-helper" {
            t.edges_corner_hint.to_string()
        } else {
            format!("{}\n{}", t.edge_place_wayland_help, t.edges_corner_hint)
        };
        if switch_row(ui, p, t.edge_place, Some(&place_help), &mut place) {
            self.apply(|c| c.edges.place_cursor = place);
        }
        divider(ui, p);
        // The master decides this one for a sub.
        let sub = self.follows_master();
        ui.add_enabled_ui(!sub, |ui| {
            let mut control = self.cfg.edges.require_control;
            if switch_row(ui, p, t.edge_require_control, Some(t.edge_require_control_hint), &mut control) {
                self.apply(|c| c.edges.require_control = control);
            }
        });
        divider(ui, p);
        let mut go = false;
        labeled_row(ui, p, t.edge_anim, 150.0, ROW_SMALL, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| go = button(ui, p, t.edges_set_in_effects, ButtonKind::Plain).clicked());
            false
        });
        if go {
            self.page = Page::Effects;
        }
    }

    // ---- Devices ----------------------------------------------------------------

    pub(super) fn devices_page(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        page_header(ui, p, t.section_devices, t.help_devices, None);
        gap(ui);
        two_cols(ui, "devices", |left, right| {
            self.master_section(left, t, p);
            self.group_section(right, t, p);
        });
    }

    // ---- Effects ----------------------------------------------------------------

    pub(super) fn effects_page(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        page_header(ui, p, t.section_effects, t.help_effects, None);
        self.follow_note(ui, t, p);
        let sub = self.follows_master();
        if sub {
            // A sub follows the master's effects: an open haptics window would let it change them anyway.
            self.haptics_dialog = None;
        }
        gap(ui);
        ui.add_enabled_ui(!sub, |ui| {
            two_cols(ui, "effects", |left, right| {
                self.edge_effects_card(left, t, p);
                self.locate_section(right, t, p);
            });
            gap(ui);
            self.haptics_summary(ui, t, p);
        });
    }

    // ---- General ----------------------------------------------------------------

    pub(super) fn general_page(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        page_header(ui, p, t.section_general, "", None);
        self.setup_card(ui, t, p);
        gap(ui);
        note_box(ui, p, NoteKind::Info, &format!("{}: {}", t.notice_title, t.notice), None);
        gap(ui);
        two_cols(ui, "general-1", |left, right| {
            self.preferences_card(left, t, p);
            self.update_section(right, t, p);
        });
        gap(ui);
        two_cols(ui, "general-2", |left, right| {
            self.backup_section(left, t, p);
            self.report_section(right, t, p);
        });
        gap(ui);
        self.maintenance_card(ui, t, p);
    }
}
