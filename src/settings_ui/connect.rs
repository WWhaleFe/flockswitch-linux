//! The network pages of the settings window: connection and layout, crossing, the
//! clipboard and this computer.

use super::*;
use crate::i18n::fill;
use crate::settings_logic as logic;
use flock_net::layout::{self, Cell, COLS, ROWS};
use flock_net::Role as NetRole;

/// One occupied cell of the layout map, as drawn.
struct MapCell {
    id: String,
    name: String,
    /// Under the name: the system, or "This computer", or "Disconnected".
    detail: String,
    mine: bool,
    away: bool,
    /// The mouse is on this computer.
    here: bool,
}

/// What a click on the map asked for.
#[derive(Debug, PartialEq)]
enum MapClick {
    Nothing,
    /// Pick up (or put down) this computer.
    Pick(String),
    /// Put the picked computer into this empty cell.
    Place(u8, u8),
}

impl SettingsApp {
    pub(super) fn connect_page(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let status = self.net_status.clone();
        let net = self.cfg.network.clone();
        let accepted = net.notice_accepted;
        let master = net.role == NetRole::Master;
        let live = status.as_ref().filter(|s| s.running && net.on);

        if page_header(ui, p, t.nav_connect, t.help_connect, Some(t.net_how_button)) {
            self.net_how = true;
        }

        if !accepted {
            gap(ui);
            card_pad(ui, p, |ui| {
                ui.label(RichText::new(t.net_notice_title).size(14.5).color(p.text));
                ui.add_space(4.0);
                self.notice_items(ui, p, t);
                ui.add_space(14.0);
                if button(ui, p, t.net_notice_accept, ButtonKind::Primary).clicked() {
                    self.apply(|c| c.network.notice_accepted = true);
                }
            });
        }

        // Sharing on or off, and this computer's part.
        gap(ui);
        card(ui, p, |ui| {
            let mut on = net.on && accepted;
            ui.add_enabled_ui(accepted, |ui| {
                let help = if accepted { t.net_share_help } else { t.net_notice_first };
                if switch_row(ui, p, t.net_share, Some(help), &mut on) {
                    self.apply(|c| c.network.on = on);
                }
            });
            if on {
                if let Some(error) = status.as_ref().and_then(|s| s.me.error.as_ref()) {
                    ui.colored_label(p.red, error);
                    ui.add_space(6.0);
                }
            }
            if accepted {
                let label = if self.net_notice_open { t.net_notice_hide } else { t.net_notice_review };
                let link = ui
                    .add(egui::Label::new(RichText::new(label).size(12.5).color(p.text).underline()).sense(Sense::click()))
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if link.clicked() {
                    self.net_notice_open = !self.net_notice_open;
                }
                if self.net_notice_open {
                    self.notice_items(ui, p, t);
                }
                ui.add_space(8.0);
            }
            divider(ui, p);
            // Master or sub: only while no computer is paired.
            let locked = !net.peers.is_empty();
            let mut help = if master { t.net_role_help_master } else { t.net_role_help_sub }.to_string();
            if locked {
                help = format!("{help}\n{}", t.net_role_locked);
            }
            let labels = [t.net_role_master.to_string(), t.net_role_sub.to_string()];
            let mut picked = None;
            ui.add_enabled_ui(!locked, |ui| {
                labeled_row_info(ui, p, t.net_role_row, Some(&help), t.net_role_info, 190.0, |ui| {
                    // The chosen role is the one place lime is used besides the "here" dot.
                    picked = segments_styled(ui, p, "net-role", &labels, Role::RadioButton, SEG_SMALL, &[], None, true, |i| (i == 1) == !master);
                    false
                });
            });
            if let Some(i) = picked {
                let role = if i == 1 { NetRole::Sub } else { NetRole::Master };
                if role != net.role {
                    self.apply(|c| c.network.role = role);
                }
            }
        });

        // Linux session capabilities: warn when sending or receiving is unavailable.
        if net.on {
            let support = crate::net::cached_support();
            if !support.send || !support.receive {
                gap(ui);
                note_box(ui, p, NoteKind::Error, &crate::net::support_text(crate::i18n::is_korean()), None);
            }
        }

        // The background app that does the sharing is not running.
        let background = crate::main_instance_running();
        if !background || status.as_ref().is_some_and(|s| net.on && !s.running) {
            gap(ui);
            if note_box(ui, p, NoteKind::Error, t.background_stopped, Some(t.background_restart)) {
                if let Err(e) = crate::net::launch_background() {
                    self.error = Some(e.to_string());
                }
            }
        }

        if let Some(pairing) = live.and_then(|s| s.pairing.as_ref()) {
            gap(ui);
            self.pairing_card(ui, t, p, pairing);
        }

        // The layout map.
        gap(ui);
        section_head(ui, p, t.layout_title, None, |ui| {
            if master {
                let text = fill(t.layout_subs, &[&net.peers.len().to_string(), &flock_net::MAX_SUBS.to_string()]);
                head_note(ui, &text, p.muted);
            }
        });
        card_pad(ui, p, |ui| self.layout_block(ui, t, p, status.as_ref()));

        // Nearby computers, and the paired ones.
        gap(ui);
        let discovered: Vec<_> = live
            .map(|s| s.discovered.iter().filter(|d| !d.paired && !net.peers.iter().any(|p| p.id == d.id)).cloned().collect())
            .unwrap_or_default();
        let searching = self.rescan_at.is_some_and(|at| at.elapsed() < Duration::from_secs(2)) || status.as_ref().is_some_and(|s| s.reconnecting);
        if searching {
            ui.ctx().request_repaint_after(Duration::from_millis(300));
        }
        let mut rescan = false;
        two_cols(ui, "nearby-paired", |left, right| {
            section_head(left, p, t.net_nearby, None, |_| {});
            card(left, p, |ui| self.nearby_rows(ui, t, p, &discovered, net.on));
            section_head(right, p, t.net_peers, None, |ui| {
                let label = if searching { t.rescanning } else { t.rescan };
                ui.add_enabled_ui(net.on && !searching, |ui| rescan = button_small(ui, p, label).clicked());
            });
            card(right, p, |ui| self.paired_rows(ui, t, p, live));
        });
        if rescan {
            crate::shared::request_reconnect();
            self.net_request(flock_net::Request::Action { action: "reconnect".into() });
            self.rescan_at = Some(Instant::now());
        }
        ui.add_space(8.0);
        ui.label(RichText::new(t.reconnect_note).size(12.5).color(p.muted));
    }

    /// The pairing in progress: the six digits to compare, and its outcome.
    fn pairing_card(&mut self, ui: &mut Ui, t: &Texts, p: Palette, pairing: &flock_net::PairingStatus) {
        card_pad(ui, p, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(t.net_pairing).size(14.5).color(p.text));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(&pairing.peer_name).font(mono(12.0)).color(p.muted));
                });
            });
            ui.vertical_centered(|ui| {
                let code = &pairing.code;
                let shown = if code.len() == 6 && code.is_ascii() { format!("{} {}", &code[..3], &code[3..]) } else { code.clone() };
                ui.add_space(8.0);
                ui.label(RichText::new(shown).font(mono(36.0)).color(p.text));
                ui.add_space(4.0);
                let reason = match pairing.reason.as_deref() {
                    Some("both_master") => Some(t.net_reason_both_master),
                    Some("both_sub") => Some(t.net_reason_both_sub),
                    Some("full") => Some(t.net_reason_full),
                    Some("has_master") => Some(t.net_reason_has_master),
                    _ => None,
                };
                if let Some(reason) = reason {
                    ui.colored_label(p.red, reason);
                }
                match pairing.stage.as_str() {
                    "done" => {
                        ui.colored_label(p.green, t.net_done);
                    }
                    "failed" => {
                        ui.colored_label(p.red, t.net_failed);
                    }
                    stage => {
                        let wait = if stage == "peer_accepted" { t.net_peer_accepted } else { t.net_wait };
                        ui.label(RichText::new(format!("{} · {wait}", t.net_compare)).size(13.0).color(p.muted));
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            // Centred: both buttons and the gap between them.
                            let w = button_width(ui, t.cancel, ButtonKind::Plain) + button_width(ui, t.net_confirm, ButtonKind::Primary) + 8.0;
                            ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                            if button(ui, p, t.cancel, ButtonKind::Plain).clicked() {
                                self.net_request(flock_net::Request::Accept { accept: false });
                            }
                            if button(ui, p, t.net_confirm, ButtonKind::Primary).clicked() {
                                self.net_request(flock_net::Request::Accept { accept: true });
                            }
                        });
                    }
                }
            });
        });
    }

    /// Nearby computers not paired yet, then the field to add one by its IP address.
    fn nearby_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette, discovered: &[flock_net::DiscoveredStatus], on: bool) {
        if discovered.is_empty() {
            empty(ui, p, if on { t.net_empty } else { t.net_disabled });
        }
        for peer in discovered {
            let mut pair = false;
            labeled_row_help(ui, p, &peer.name, Some(&format!("{} · {}", os_label(peer.os), peer.addr)), 100.0, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| pair = button(ui, p, t.net_pair, ButtonKind::Primary).clicked());
                false
            });
            if pair {
                self.net_request(flock_net::Request::Pair { pair: peer.id.clone() });
            }
            divider(ui, p);
        }
        ui.add_space(DIVIDER_GAP);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let room = (ui.available_width() - button_width(ui, t.net_add_ip, ButtonKind::Plain) - 8.0).max(100.0);
            ui.add(egui::TextEdit::singleline(&mut self.net_address).hint_text(t.net_address_hint).desired_width(room));
            ui.add_enabled_ui(on && !self.net_address.trim().is_empty(), |ui| {
                if button(ui, p, t.net_add_ip, ButtonKind::Plain).clicked() {
                    let input = self.net_address.trim();
                    let address = if let Ok(ip) = input.parse::<std::net::IpAddr>() {
                        std::net::SocketAddr::new(ip, flock_net::NetConfig::default().port).to_string()
                    } else {
                        input.to_string()
                    };
                    if address.parse::<std::net::SocketAddr>().is_ok() {
                        self.net_request(flock_net::Request::PairAddr { pair_addr: address });
                    } else {
                        self.error = Some(t.net_address_hint.into());
                    }
                }
            });
        });
        ui.add_space(DIVIDER_GAP + 2.0);
    }

    /// The paired computers: their link, the switch for sending from them, "Unpair".
    fn paired_rows(&mut self, ui: &mut Ui, t: &Texts, p: Palette, live: Option<&flock_net::Status>) {
        let net = self.cfg.network.clone();
        let master = net.role == NetRole::Master;
        if net.peers.is_empty() {
            empty(ui, p, t.net_no_peers);
        }
        for (i, peer) in net.peers.iter().enumerate() {
            if i > 0 {
                divider(ui, p);
            }
            let link = live.and_then(|s| s.peers.iter().find(|s| s.id == peer.id)).filter(|s| s.connected);
            let state = match link {
                Some(l) => {
                    let delay = l.rtt_ms.map(|ms| format!(" · {ms:.1} ms")).unwrap_or_default();
                    format!("{} · {}{delay}", if master { os_label(peer.os) } else { t.net_role_master }, t.net_connected)
                }
                None => format!("{} · {}", if master { os_label(peer.os) } else { t.net_role_master }, t.net_offline),
            };
            let with_switch = master;
            let height = if with_switch { 88.0 } else { 58.0 };
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
            let painter = ui.painter();
            let left = rect.left();
            painter.text(Pos2::new(left, rect.top() + 11.0), Align2::LEFT_TOP, &peer.name, FontId::proportional(14.5), p.text);
            let dot = Pos2::new(left + 3.0, rect.top() + 38.0);
            painter.circle_filled(dot, 3.0, if link.is_some() { p.green } else { p.off });
            painter.text(Pos2::new(left + 14.0, rect.top() + 38.0), Align2::LEFT_CENTER, state, mono(12.0), p.muted);
            let mut flip = None;
            if with_switch {
                let area = Rect::from_min_size(Pos2::new(left, rect.top() + 54.0), Vec2::new(rect.width() - 100.0, 24.0));
                ui.new_child(UiBuilder::new().max_rect(area).layout(egui::Layout::left_to_right(egui::Align::Center))).scope(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let mut on = peer.can_send;
                    if mini_switch(ui, p, t.peer_send, &mut on) {
                        flip = Some(on);
                    }
                    ui.add(egui::Label::new(RichText::new(t.peer_send).size(12.5).color(p.muted)).truncate());
                });
            }
            let width = button_width(ui, t.net_unpair, ButtonKind::Danger);
            let area = Rect::from_min_size(Pos2::new(rect.right() - width, rect.top() + 8.0), Vec2::new(width, CONTROL_HEIGHT));
            let mut unpair = false;
            ui.new_child(UiBuilder::new().max_rect(area)).scope(|ui| unpair = button(ui, p, t.net_unpair, ButtonKind::Danger).clicked());
            if unpair {
                self.net_request(flock_net::Request::Unpair { unpair: peer.id.clone() });
            }
            if let Some(on) = flip {
                let id = peer.id.clone();
                self.apply(|c| {
                    if let Some(x) = c.network.peers.iter_mut().find(|x| x.id == id) {
                        x.can_send = on;
                    }
                });
            }
        }
        ui.add_space(DIVIDER_GAP);
    }

    /// The map of where the computers sit. The master moves them (click a computer, then
    /// an empty cell); a sub sees the master's layout, locked.
    fn layout_block(&mut self, ui: &mut Ui, t: &Texts, p: Palette, status: Option<&flock_net::Status>) {
        let net = self.cfg.network.clone();
        // Without an identity yet (sharing was never on) the map is shown but not editable.
        let (me, known_me) = match self.my_id() {
            Some(id) => (id, true),
            None => ("this-computer".to_string(), false),
        };
        let master = net.role == NetRole::Master;
        let editable = master && known_me;
        let running = status.filter(|s| s.running);
        let cells: Vec<Cell> = match running.map(|s| s.layout.clone()).filter(|c| !c.is_empty()) {
            Some(cells) if !master => cells,
            _ => logic::effective_cells(&net, &me),
        };
        // The dot follows the controlled computer by id, also when it is a sub reached
        // through the master (the roster), not only a paired one.
        let mouse_at = match (self.mouse_at(), running) {
            (MouseAt::There(_), Some(s)) => s.state.peer.clone().or_else(|| Some(me.clone())),
            _ => Some(me.clone()),
        };

        if !master {
            let name = net.peers.first().map(|p| p.name.clone()).unwrap_or_default();
            let mut note = fill(t.layout_sub_note, &[&name]);
            let side = net.peers.first().and_then(|m| layout::side_of(&cells, &me, &m.id));
            if let Some(side) = side {
                let word = match side {
                    flock_net::Side::Left => t.edge_left,
                    flock_net::Side::Right => t.edge_right,
                    flock_net::Side::Top => t.edge_top,
                    flock_net::Side::Bottom => t.edge_bottom,
                };
                note = format!("{note} {}", fill(t.layout_master_side, &[&word.to_lowercase()]));
            }
            note_box(ui, p, NoteKind::Info, &note, None);
            ui.add_space(14.0);
        }

        // Who each cell holds, as shown.
        let discovered = running.map(|s| s.discovered.clone()).unwrap_or_default();
        let my_name = running.map(|s| s.me.name.clone()).filter(|n| !n.is_empty()).or_else(|| Some(net.name.clone()).filter(|n| !n.is_empty()));
        let mut grid: Vec<Option<MapCell>> = (0..COLS * ROWS).map(|_| None).collect();
        for cell in &cells {
            let idx = usize::from(cell.row) * usize::from(COLS) + usize::from(cell.col);
            let mine = cell.id == me;
            let known = net.peers.iter().find(|x| x.id == cell.id);
            let found = discovered.iter().find(|d| d.id == cell.id);
            let connected = mine || running.is_some_and(|s| s.peers.iter().any(|x| x.id == cell.id && x.connected));
            let live = running.and_then(|s| s.peers.iter().find(|x| x.id == cell.id));
            let name = if mine {
                my_name.clone().unwrap_or_else(|| t.net_this_computer.to_string())
            } else {
                crate::net_logic::peer_name(&net, running, &cell.id)
                    .or_else(|| found.map(|d| d.name.clone()))
                    .unwrap_or_else(|| cell.id.chars().take(6).collect())
            };
            let os = live.and_then(|x| x.os).or_else(|| known.map(|x| x.os)).or_else(|| found.map(|d| d.os));
            let os_text = os.map(os_label).unwrap_or_default();
            let detail = if mine {
                t.net_this_computer.to_string()
            } else if connected {
                if !master && known.is_some() { t.net_role_master.to_string() } else { os_text.to_string() }
            } else {
                fill(t.layout_away, &[os_text])
            };
            grid[idx] = Some(MapCell {
                id: cell.id.clone(),
                name,
                detail,
                mine,
                away: !connected,
                here: mouse_at.as_deref() == Some(cell.id.as_str()),
            });
        }
        let picked = self.layout_pick.clone().filter(|_| editable);
        let click = draw_map(ui, p, t, &grid, picked.as_deref(), editable);

        // Computers paired but not on the map yet.
        let known: Vec<String> = std::iter::once(me.clone()).chain(net.peers.iter().map(|x| x.id.clone())).collect();
        let loose = logic::unplaced(&cells, &known);
        if editable && !loose.is_empty() {
            ui.add_space(10.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(t.layout_unplaced).size(12.5).color(p.muted));
                for id in &loose {
                    let name = crate::net_logic::peer_name(&net, running, id).unwrap_or_else(|| id.to_string());
                    let kind = if picked.as_deref() == Some(*id) { ButtonKind::Primary } else { ButtonKind::Plain };
                    if button(ui, p, &name, kind).clicked() {
                        self.layout_pick = if picked.as_deref() == Some(*id) { None } else { Some(id.to_string()) };
                    }
                }
            });
        }

        match click {
            MapClick::Nothing => {}
            MapClick::Pick(id) => self.layout_pick = if picked.as_deref() == Some(id.as_str()) { None } else { Some(id) },
            MapClick::Place(col, row) => {
                if let Some(id) = picked {
                    if let Some(next) = logic::move_to(&cells, &known, &id, col, row) {
                        self.apply(|c| logic::set_layout(&mut c.network, &me, next));
                    }
                }
                self.layout_pick = None;
            }
        }

        // What the map says, under it.
        let links = logic::direct_pairs(&cells, &me);
        let name_of = |id: &str| crate::net_logic::peer_name(&net, running, id).unwrap_or_else(|| id.chars().take(6).collect::<String>());
        let help = if !master {
            t.layout_help_sub.to_string()
        } else if self.layout_pick.is_some() {
            t.layout_help_pick.to_string()
        } else if links.is_empty() {
            format!("{} {}", t.layout_help_idle, t.layout_direct_none)
        } else {
            let list: Vec<String> = links.iter().map(|(a, b)| fill(t.layout_pair, &[&name_of(a), &name_of(b)])).collect();
            format!("{} {}", t.layout_help_idle, fill(t.layout_direct, &[&list.join(", ")]))
        };
        ui.add_space(12.0);
        ui.label(RichText::new(help).size(12.5).color(p.muted));
    }

    pub(super) fn cross_page(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let net = self.cfg.network.clone();
        let sub = self.follows_master();
        page_header(ui, p, t.net_crossing, t.help_cross, None);
        if sub {
            gap(ui);
            self.follow_banner(ui, p, t);
        }
        gap(ui);
        card(ui, p, |ui| {
            ui.add_enabled_ui(!sub, |ui| {
                let choices: Vec<(u64, String)> = [(0, ""), (300, "0.3"), (500, "0.5"), (1000, "1"), (2000, "2")]
                    .into_iter()
                    .map(|(ms, text)| (ms, if ms == 0 { t.net_delay_none.to_string() } else { format!("{text}{}", t.seconds_short) }))
                    .collect();
                let choices: Vec<(u64, &str)> = choices.iter().map(|(ms, s)| (*ms, s.as_str())).collect();
                let mut delay = net.cross_delay_ms;
                if labeled_row_help(ui, p, t.net_delay, Some(t.net_delay_help), 330.0, |ui| segmented(ui, p, "net-delay", &mut delay, &choices, SEG_SMALL)) {
                    self.apply(|c| c.network.cross_delay_ms = delay);
                }
                divider(ui, p);
                let mut control = net.require_control;
                if switch_row(ui, p, t.net_require_control, Some(t.net_require_control_help), &mut control) {
                    self.apply(|c| c.network.require_control = control);
                }
                divider(ui, p);
                let mut arrival = self.cfg.locate.on && self.cfg.locate.on_arrival;
                if switch_row(ui, p, t.net_arrival, Some(t.net_arrival_help), &mut arrival) {
                    self.apply(|c| {
                        c.locate.on_arrival = arrival;
                        if arrival {
                            c.locate.on = true;
                        }
                    });
                }
            });
        });
        if net.on && self.cfg.enabled {
            gap(ui);
            if note_box(ui, p, NoteKind::Warn, t.easy_switch_hint, Some(t.easy_switch_off)) {
                self.apply(|c| c.enabled = false);
            }
        }
        gap(ui);
        section_head(ui, p, t.back_title, None, |_| {});
        card(ui, p, |ui| {
            labeled_row_help(ui, p, t.back_key, Some(t.back_key_help), 270.0, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for key in ["Q", "Shift", "Alt", "Ctrl"] {
                        keycap(ui, p, key);
                    }
                });
                false
            });
        });
    }

    pub(super) fn clip_page(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        let net = self.cfg.network.clone();
        let sub = self.follows_master();
        let live = self.net_status.clone().filter(|s| s.running && net.on);
        page_header(ui, p, t.nav_clip, t.help_clip, None);
        if sub {
            gap(ui);
            self.follow_banner(ui, p, t);
        }
        gap(ui);
        card(ui, p, |ui| {
            ui.add_enabled_ui(!sub, |ui| {
                let mut share = net.share_clipboard;
                let help = if crate::edge_wayland::session_is_wayland() {
                    format!("{}\n{}", t.clip_share_help, t.clip_wayland_help)
                } else {
                    t.clip_share_help.to_string()
                };
                if switch_row(ui, p, t.net_clipboard, Some(&help), &mut share) {
                    self.apply(|c| c.network.share_clipboard = share);
                }
                divider(ui, p);
                ui.add_enabled_ui(net.share_clipboard, |ui| {
                    let mut relay = net.clipboard_relay;
                    if switch_row(ui, p, t.clip_relay, Some(t.clip_relay_help), &mut relay) {
                        self.apply(|c| c.network.clipboard_relay = relay);
                    }
                });
            });
        });

        gap(ui);
        let limit = if net.clipboard_limit_mb == 0 { t.clip_not_sent.to_string() } else { fill(t.clip_upto, &[&net.clipboard_limit_mb.to_string()]) };
        let folder = crate::net::received_dir();
        let received = live
            .as_ref()
            .and_then(|s| s.transfer.as_ref())
            .filter(|tr| tr.direction == "receive" && tr.kind == "files" && tr.stage == "done")
            .and_then(|tr| tr.saved_in.clone());
        let mut open = None;
        two_cols(ui, "clipboard", |left, right| {
            section_head(left, p, t.clip_sent, None, |_| {});
            card(left, p, |ui| {
                value_row(ui, p, t.clip_text, None, t.clip_text_always, false);
                divider(ui, p);
                value_row(ui, p, t.clip_image, Some(t.clip_image_help), &limit, true);
                divider(ui, p);
                value_row(ui, p, t.clip_files, Some(t.clip_files_help), &limit, true);
            });
            section_head(right, p, t.clip_received, None, |_| {});
            card(right, p, |ui| {
                let path = folder.as_ref().map(|f| home_relative(f)).unwrap_or_default();
                labeled_row_help(ui, p, t.clip_folder, Some(&path), 120.0, |ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if button_small(ui, p, t.backup_open).clicked() {
                            open = folder.clone();
                        }
                    });
                    false
                });
                divider(ui, p);
                let recent = received.as_deref().map_or(t.clip_none_yet.to_string(), |dir| home_relative(std::path::Path::new(dir)));
                labeled_row_help(ui, p, t.clip_recent, Some(&recent), 120.0, |ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_enabled_ui(received.is_some(), |ui| {
                            if button_small(ui, p, t.update_show).clicked() {
                                open = received.as_ref().map(std::path::PathBuf::from);
                            }
                        });
                    });
                    false
                });
            });
        });
        if let Some(dir) = open {
            let _ = std::fs::create_dir_all(&dir);
            update::open_url(&dir.display().to_string());
        }

        if let Some(tr) = live.as_ref().and_then(|s| s.transfer.as_ref()) {
            let mb = |bytes: u64| bytes as f64 / 1_048_576.0;
            let line = match tr.stage.as_str() {
                "running" => Some((
                    format!("{} · {:.1} / {:.1} MB", if tr.direction == "send" { t.net_sending } else { t.net_receiving }, mb(tr.done_bytes), mb(tr.total_bytes)),
                    p.text,
                    Some(if tr.total_bytes == 0 { 0.0 } else { (tr.done_bytes as f32 / tr.total_bytes as f32).clamp(0.0, 1.0) }),
                )),
                "done" if tr.direction == "receive" && tr.kind == "files" => {
                    Some((format!("{} {}", t.net_received, tr.saved_in.clone().unwrap_or_default()), p.green, None))
                }
                "too_large" => Some((t.net_too_large.to_string(), p.amber, None)),
                "failed" => Some((t.net_transfer_failed.to_string(), p.red, None)),
                _ => None,
            };
            if let Some((text, color, progress)) = line {
                gap(ui);
                card_pad(ui, p, |ui| {
                    ui.label(RichText::new(text).size(13.0).color(color));
                    if let Some(done) = progress {
                        ui.add_space(8.0);
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
                        ui.painter().rect_filled(rect, 3, p.track);
                        ui.painter().rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * done, 6.0)), 3, p.strong);
                    }
                    ui.add_space(8.0);
                    ui.label(RichText::new(t.clip_limit_note).size(12.5).color(p.muted));
                });
            }
        }
    }

    pub(super) fn this_page(&mut self, ui: &mut Ui, t: &Texts, p: Palette) {
        if self.shots.is_none() && self.net_ips.0.elapsed() >= Duration::from_secs(10) {
            self.net_ips = (Instant::now(), local_ips());
        }
        let status = self.net_status.clone();
        let net = self.cfg.network.clone();
        page_header(ui, p, t.net_this_computer, t.help_this, None);
        gap(ui);
        card(ui, p, |ui| {
            let role = if net.role == NetRole::Master {
                format!("{} {}/{}", t.net_badge_master, net.peers.len(), flock_net::MAX_SUBS)
            } else {
                let name = self.master_name();
                format!("{} {}", t.net_badge_sub, if name.is_empty() { t.net_no_master } else { &name })
            };
            let mut change = false;
            labeled_row(ui, p, t.net_role, 380.0, ROW_SMALL, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    change = button(ui, p, t.this_role_change, ButtonKind::Plain).clicked();
                    ui.label(RichText::new(role).font(bold(14.0)).color(p.text));
                });
                false
            });
            if change {
                self.page = Page::Connect;
            }
            divider(ui, p);
            let mut name = net.name.clone();
            if labeled_row_help(ui, p, t.net_name, Some(t.net_name_help), 240.0, |ui| {
                ui.add(egui::TextEdit::singleline(&mut name).hint_text(t.net_name_hint).desired_width(240.0)).changed()
            }) {
                self.apply(|c| c.network.name = name);
            }
            divider(ui, p);
            // The port is typed into a buffer and checked only on Apply.
            let current = status.as_ref().filter(|s| s.running).map_or(net.port, |s| s.me.port);
            let mut text = self.net_port_edit.clone().unwrap_or_else(|| net.port.to_string());
            let (mut apply, mut default) = (false, false);
            let typed = labeled_row_help(ui, p, t.net_port, Some(t.net_port_help), 330.0, |ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    apply = button(ui, p, t.net_port_apply, ButtonKind::Plain).clicked();
                    default = button(ui, p, t.net_port_default, ButtonKind::Plain).clicked();
                    ui.add(egui::TextEdit::singleline(&mut text).desired_width(64.0)).changed()
                })
                .inner
            });
            if typed {
                text.retain(|c| c.is_ascii_digit());
                text.truncate(5);
                self.net_port_edit = Some(text.clone());
                self.net_port_error = None;
            }
            if default {
                self.net_port_edit = Some(flock_net::DEFAULT_PORT.to_string());
                self.try_port(t, Some(flock_net::DEFAULT_PORT), current);
            } else if apply {
                self.try_port(t, text.parse().ok(), current);
            }
            if let Some(error) = self.net_port_error {
                ui.colored_label(p.red, error);
                ui.add_space(6.0);
            }
            divider(ui, p);
            let shown: Vec<String> = self
                .net_ips
                .1
                .iter()
                .map(|ip| if net.port == flock_net::DEFAULT_PORT { ip.clone() } else { format!("{ip}:{}", net.port) })
                .collect();
            value_row(ui, p, t.net_ip, Some(t.net_ip_help), &shown.join(", "), true);
            divider(ui, p);
            let id = status.as_ref().map(|s| s.me.id.get(..8).unwrap_or(&s.me.id).to_string()).unwrap_or_default();
            value_row(ui, p, t.net_id, None, &id, false);
        });
        gap(ui);
        note_box(ui, p, NoteKind::Info, &crate::net::support_text(crate::i18n::is_korean()), None);
    }
}

/// A row with a label (and help) and a value in monospace at the right.
fn value_row(ui: &mut Ui, p: Palette, label: &str, help: Option<&str>, value: &str, strong: bool) {
    let width = (ui.painter().layout_no_wrap(value.to_string(), mono(12.0), p.text).size().x + 4.0).clamp(60.0, ui.available_width() * 0.5);
    labeled_row_help(ui, p, label, help, width, |ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value).font(mono(12.0)).color(if strong { p.text } else { p.muted }));
        });
        false
    });
}

/// A small plain button (the ones in a section header).
fn button_small(ui: &mut Ui, p: Palette, label: &str) -> Response {
    button(ui, p, label, ButtonKind::Plain)
}

/// Draws the 5x3 map and says what was clicked.
fn draw_map(ui: &mut Ui, p: Palette, t: &Texts, grid: &[Option<MapCell>], picked: Option<&str>, editable: bool) -> MapClick {
    let (pad, gutter, cell_h) = (14.0_f32, 10.0_f32, 76.0_f32);
    let (cols, rows) = (f32::from(COLS), f32::from(ROWS));
    let width = ui.available_width();
    let height = rows * cell_h + (rows - 1.0) * gutter + 2.0 * pad;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
    let painter = ui.painter().clone();
    painter.rect_filled(rect, 12, p.hover);
    // A faint dotted ground, like graph paper.
    let mut y = rect.top() + 8.0;
    while y < rect.bottom() - 4.0 {
        let mut x = rect.left() + 8.0;
        while x < rect.right() - 4.0 {
            painter.circle_filled(Pos2::new(x, y), 0.8, p.track);
            x += 16.0;
        }
        y += 16.0;
    }
    let cell_w = (width - 2.0 * pad - (cols - 1.0) * gutter) / cols;
    let mut click = MapClick::Nothing;
    for row in 0..ROWS {
        for col in 0..COLS {
            let r = Rect::from_min_size(
                rect.min + Vec2::new(pad + f32::from(col) * (cell_w + gutter), pad + f32::from(row) * (cell_h + gutter)),
                Vec2::new(cell_w, cell_h),
            );
            let occupant = grid[usize::from(row) * usize::from(COLS) + usize::from(col)].as_ref();
            let drop = occupant.is_none() && picked.is_some() && editable;
            let sense = if editable && (occupant.is_some() || drop) { Sense::click() } else { Sense::hover() };
            let resp = ui.interact(r, ui.scope_id().with(("map-cell", col, row)), sense);
            let resp = if sense == Sense::click() { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
            match occupant {
                Some(cell) => {
                    resp.widget_info(|| WidgetInfo::labeled(Role::Button, editable, &cell.name));
                    if cell.mine {
                        painter.add(p.card_shadow().as_shape(r, 12));
                        painter.rect_filled(r, 12, p.card);
                        painter.rect_stroke(r, 12, Stroke::new(1.0, p.border), StrokeKind::Inside);
                    } else {
                        painter.add(Shape::Vec(ghost_shapes(r, 12.0, p)));
                    }
                    if picked == Some(cell.id.as_str()) {
                        painter.rect_stroke(r.expand(2.0), 14, Stroke::new(2.0, p.text), StrokeKind::Outside);
                    }
                    let name_w = r.width() - 28.0 - if cell.here { 16.0 } else { 0.0 };
                    let name = elided(ui, &cell.name, bold(14.0), p.text, name_w);
                    painter.galley(r.left_top() + Vec2::new(14.0, 12.0), name, p.text);
                    if cell.here {
                        // The one lime mark: the mouse is here.
                        let c = Pos2::new(r.right() - 18.0, r.top() + 21.0);
                        painter.circle_filled(c, 6.0, p.accent.gamma_multiply(0.35));
                        painter.circle_filled(c, 4.0, p.accent);
                        let hover = Rect::from_center_size(c, Vec2::splat(16.0));
                        ui.interact(hover, ui.scope_id().with(("map-here", col, row)), Sense::hover()).on_hover_text(t.layout_here);
                    }
                    let dot = Pos2::new(r.left() + 17.0, r.bottom() - 18.0);
                    painter.circle_filled(dot, 3.0, if cell.away { p.off } else { p.green });
                    let detail = elided(ui, &cell.detail, FontId::proportional(12.0), p.muted, r.width() - 40.0);
                    painter.galley(Pos2::new(r.left() + 28.0, r.bottom() - 18.0 - detail.size().y / 2.0), detail, p.muted);
                    if resp.clicked() {
                        click = MapClick::Pick(cell.id.clone());
                    }
                }
                None => {
                    resp.widget_info(|| WidgetInfo::labeled(Role::Button, drop, t.layout_empty));
                    if drop {
                        painter.rect_filled(r, 12, p.text.gamma_multiply(if resp.hovered() { 0.14 } else { 0.07 }));
                        painter.rect_stroke(r, 12, Stroke::new(1.5, p.muted), StrokeKind::Inside);
                    } else {
                        painter.rect_stroke(r, 12, Stroke::new(1.0, p.border), StrokeKind::Inside);
                    }
                    if resp.clicked() {
                        click = MapClick::Place(col, row);
                    }
                }
            }
        }
    }
    click
}
