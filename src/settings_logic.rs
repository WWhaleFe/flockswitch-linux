//! The settings window's logic that needs no drawing: the layout map (who sits in which
//! cell) and the key rules per OS pair ("OS 전환 시"). The window shows these and calls
//! here to change them, so what a click does can be tested without a window.

use flock_net::keymap::{self, map_modifier};
use flock_net::layout::{self, Cell, COLS, ROWS};
use flock_net::{DirRules, HangulKey, ModMode, NetConfig, Os};

// ---- Layout map ------------------------------------------------------------------

/// The grid as it is now: the master's own layout (the master in it), or the one the
/// peers' 2.0 sides stand for while it has not been edited.
pub fn effective_cells(net: &NetConfig, me: &str) -> Vec<Cell> {
    if net.layout.is_empty() {
        layout::cells_from_sides(me, &net.peers)
    } else {
        layout::with_master(&net.layout, me)
    }
}

pub fn cell_at(cells: &[Cell], col: u8, row: u8) -> Option<&Cell> {
    cells.iter().find(|c| (c.col, c.row) == (col, row))
}

/// Moves (or first places) computer `id` so that its monitor `display` lands on (`col`, `row`).
/// All of that computer's monitor tiles move together by the same number of cells, so the
/// shape of its monitor arrangement stays as it is. None when that is not allowed: a tile
/// would leave the grid or land on another computer's cell, or `id` is not one of `known`
/// (this computer and the paired ones). Landing on the computer's own tiles is fine.
pub fn move_to(cells: &[Cell], known: &[String], id: &str, display: u8, col: u8, row: u8) -> Option<Vec<Cell>> {
    let offset = group_offset(cells, known, id, display, col, row)?;
    let mut next: Vec<Cell> = cells.iter().filter(|c| c.id != id).cloned().collect();
    let own: Vec<&Cell> = cells.iter().filter(|c| c.id == id).collect();
    if own.is_empty() {
        next.push(Cell::of_display(id, display, col, row));
    } else {
        next.extend(own.into_iter().filter_map(|c| shifted(c, offset)));
    }
    Some(next)
}

/// How far (columns, rows) the tiles of `id` move when its monitor `display` goes to
/// (`col`, `row`); (0, 0) for a computer that has no tile yet. None when it may not go there.
pub fn group_offset(cells: &[Cell], known: &[String], id: &str, display: u8, col: u8, row: u8) -> Option<(i16, i16)> {
    if col >= COLS || row >= ROWS || !known.iter().any(|k| k == id) {
        return None;
    }
    let own: Vec<&Cell> = cells.iter().filter(|c| c.id == id).collect();
    // The picked monitor is the anchor; a monitor without a tile of its own moves the whole
    // computer by its first tile.
    let offset = match own.iter().find(|c| c.display == display).or(own.first()) {
        Some(anchor) => (i16::from(col) - i16::from(anchor.col), i16::from(row) - i16::from(anchor.row)),
        None => (0, 0),
    };
    if own.is_empty() {
        return cell_at(cells, col, row).is_none().then_some(offset);
    }
    for c in &own {
        let to = shifted(c, offset)?;
        if cell_at(cells, to.col, to.row).is_some_and(|o| o.id != id) {
            return None;
        }
    }
    Some(offset)
}

/// The tiles of `id` in `cells` moved by `offset`; tiles that would leave the grid are left
/// out (for a monitor that is unplugged now, whose saved tile follows its computer).
pub fn shift(cells: &[Cell], id: &str, offset: (i16, i16)) -> Vec<Cell> {
    cells.iter().filter_map(|c| if c.id == id { shifted(c, offset) } else { Some(c.clone()) }).collect()
}

fn shifted(c: &Cell, (dc, dr): (i16, i16)) -> Option<Cell> {
    let col = u8::try_from(i16::from(c.col) + dc).ok().filter(|v| *v < COLS)?;
    let row = u8::try_from(i16::from(c.row) + dr).ok().filter(|v| *v < ROWS)?;
    Some(Cell { col, row, ..c.clone() })
}

/// `next` plus the saved cells of monitors the service does not list now (a monitor that
/// is unplugged): they stay in the saved layout, as long as their spot is still free.
pub fn keep_hidden(mut next: Vec<Cell>, saved: &[Cell]) -> Vec<Cell> {
    for c in saved {
        let listed = next.iter().any(|n| n.id == c.id && n.display == c.display);
        let known = next.iter().any(|n| n.id == c.id);
        if !listed && known && cell_at(&next, c.col, c.row).is_none() {
            next.push(c.clone());
        }
    }
    next
}

/// The known computers that have no cell (peers without a 2.0 side start like this).
pub fn unplaced<'a>(cells: &[Cell], known: &'a [String]) -> Vec<&'a str> {
    known.iter().filter(|k| !cells.iter().any(|c| &c.id == *k)).map(String::as_str).collect()
}

/// Pairs of computers other than `me` that share an edge: they cross to each other
/// without passing through `me`.
pub fn direct_pairs(cells: &[Cell], me: &str) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for (i, a) in cells.iter().enumerate() {
        for b in &cells[i + 1..] {
            let near = a.col.abs_diff(b.col) + a.row.abs_diff(b.row) == 1;
            let new = !pairs.iter().any(|(x, y)| (x, y) == (&a.id, &b.id) || (x, y) == (&b.id, &a.id));
            if near && a.id != me && b.id != me && a.id != b.id && new {
                pairs.push((a.id.clone(), b.id.clone()));
            }
        }
    }
    pairs
}

/// Saves `cells` as the master's layout and re-derives every peer's 2.0 side from it
/// (the service does the same once it runs; this keeps the file right meanwhile).
pub fn set_layout(net: &mut NetConfig, me: &str, cells: Vec<Cell>) {
    let cells = layout::with_master(&keep_hidden(cells, &net.layout), me);
    for p in &mut net.peers {
        p.side = layout::side_of(&cells, me, &p.id);
    }
    net.layout = cells;
}

// ---- Key rules per OS pair -------------------------------------------------------

/// The systems a computer of `os` can be paired with in the keyboard pane, in tab order.
pub fn pair_systems(os: Os) -> Vec<Os> {
    [Os::Mac, Os::Windows, Os::Linux].into_iter().filter(|o| *o != os).chain([Os::Android]).collect()
}

/// The directions shown for the pair of `os` and `other`: both ways, except that nothing
/// is sent from Android.
pub fn directions(os: Os, other: Os) -> Vec<(Os, Os)> {
    if other == Os::Android {
        vec![(os, other)]
    } else {
        vec![(os, other), (other, os)]
    }
}

/// The three bottom-row modifiers of a system, left to right, as the kinds
/// 0 Ctrl, 2 Alt, 3 Win/Command (HID usage 0xe0 + kind).
pub fn bottom_row(os: Os) -> [u16; 3] {
    if os == Os::Mac {
        [0, 2, 3]
    } else {
        [0, 3, 2]
    }
}

const LCTRL: u16 = 0xe0;

/// The kind (0 Ctrl, 2 Alt, 3 Win/Command) `src_kind` is sent as under `rules`.
pub fn mod_target(src: Os, dst: Os, rules: &DirRules, src_kind: u16) -> u16 {
    map_modifier(src, dst, rules, LCTRL + src_kind) - LCTRL
}

/// What each Korean/English key of a system is, for the source choices.
pub fn hangul_sources(os: Os) -> &'static [HangulKey] {
    match os {
        Os::Mac => &[HangulKey::CapsLock, HangulKey::ControlSpace, HangulKey::ShiftSpace],
        Os::Windows => &[HangulKey::Lang1, HangulKey::RightAlt, HangulKey::ShiftSpace],
        Os::Linux => &[HangulKey::Lang1, HangulKey::ShiftSpace, HangulKey::SuperSpace],
        Os::Android => &[],
    }
}

/// What a system understands as its Korean/English key, for the target choices.
pub fn hangul_targets(os: Os) -> &'static [HangulKey] {
    match os {
        Os::Mac => &[HangulKey::CapsLock, HangulKey::ControlSpace],
        Os::Windows => &[HangulKey::Lang1],
        Os::Linux => &[HangulKey::Lang1, HangulKey::ShiftSpace, HangulKey::SuperSpace],
        Os::Android => &[HangulKey::ShiftSpace, HangulKey::ControlSpace],
    }
}

/// Whether the system has a Hanja key (or chord) to match.
pub fn has_hanja(os: Os) -> bool {
    os != Os::Android
}

/// One change made in the "OS 전환 시" pane.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum KeyEdit {
    /// The basis of the modifier table; the table starts again from it.
    ModMode(ModMode),
    /// The source modifier of this kind is sent as a modifier of that kind.
    Modifier { from: u16, to: u16 },
    /// A source key for Korean/English on or off.
    HangulSource(HangulKey, bool),
    HangulTarget(HangulKey),
    CapsLongPress(bool),
    Hanja(bool),
    Nav(usize, bool),
    Capture(bool),
}

/// Applies `edit` to the rules of `src` -> `dst` in `net.keys`. The first edit writes
/// every direction's rules in effect (the 2.0 fields stand for the untouched ones), so
/// nothing else changes meaning.
pub fn edit_keys(net: &mut NetConfig, src: Os, dst: Os, edit: KeyEdit) {
    let Some(mut rules) = net.rules_for(src, dst) else { return };
    if net.keys.is_empty() {
        net.migrate_keys();
    }
    match edit {
        KeyEdit::ModMode(mode) => {
            rules.mod_mode = mode;
            rules.mods.clear();
        }
        KeyEdit::Modifier { from, to } => {
            // The mode's own table is what a row shows when nothing is overridden.
            let plain = DirRules { mods: Default::default(), ..rules.clone() };
            let (left, right) = (LCTRL + from, LCTRL + 4 + from);
            if mod_target(src, dst, &plain, from) == to {
                rules.mods.remove(&left);
                rules.mods.remove(&right);
            } else {
                rules.mods.insert(left, LCTRL + to);
                rules.mods.insert(right, LCTRL + 4 + to);
            }
        }
        KeyEdit::HangulSource(key, on) => {
            rules.hangul_src.retain(|k| *k != key);
            if on {
                rules.hangul_src.push(key);
                let order = hangul_sources(src);
                rules.hangul_src.sort_by_key(|k| order.iter().position(|o| o == k).unwrap_or(usize::MAX));
            }
        }
        KeyEdit::HangulTarget(key) => rules.hangul_dst = key,
        KeyEdit::CapsLongPress(on) => rules.caps_long_press = on,
        KeyEdit::Hanja(on) => rules.hanja = on,
        KeyEdit::Nav(i, on) => {
            if let Some(slot) = rules.nav.get_mut(i) {
                *slot = on;
            }
        }
        KeyEdit::Capture(on) => rules.capture = on,
    }
    net.keys.insert(keymap::pair_key(src, dst), rules);
}

#[cfg(test)]
mod tests {
    use super::*;
    use flock_net::{PeerConfig, Side};

    fn cell(id: &str, col: u8, row: u8) -> Cell {
        Cell::new(id, col, row)
    }
    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }
    fn peer(id: &str, side: Option<Side>) -> PeerConfig {
        PeerConfig { id: id.into(), side, ..PeerConfig::default() }
    }

    #[test]
    fn layout_starts_from_the_2_0_sides() {
        let net = NetConfig { peers: vec![peer("a", Some(Side::Left)), peer("b", None)], ..NetConfig::default() };
        let cells = effective_cells(&net, "me");
        assert_eq!(cells, vec![cell("me", 2, 1), cell("a", 1, 1)]);
        assert_eq!(unplaced(&cells, &ids(&["me", "a", "b"])), vec!["b"]);
    }

    #[test]
    fn a_click_on_a_computer_then_an_empty_cell_moves_it() {
        let known = ids(&["me", "a", "b"]);
        let cells = vec![cell("me", 2, 1), cell("a", 1, 1)];
        // To an empty cell.
        let moved = move_to(&cells, &known, "a", 0, 4, 2).unwrap();
        assert_eq!(cell_at(&moved, 4, 2).map(|c| c.id.as_str()), Some("a"));
        assert!(cell_at(&moved, 1, 1).is_none());
        assert_eq!(moved.len(), 2);
        // An unplaced computer is placed.
        let placed = move_to(&cells, &known, "b", 0, 0, 0).unwrap();
        assert_eq!(placed.len(), 3);
        // The master moves too.
        assert!(move_to(&cells, &known, "me", 0, 2, 0).is_some());
        // Taken cell, outside the grid, unknown computer: refused.
        assert!(move_to(&cells, &known, "a", 0, 2, 1).is_none());
        assert!(move_to(&cells, &known, "a", 0, COLS, 0).is_none());
        assert!(move_to(&cells, &known, "a", 0, 0, ROWS).is_none());
        assert!(move_to(&cells, &known, "ghost", 0, 0, 0).is_none());
    }

    #[test]
    fn moving_one_monitor_tile_moves_the_whole_computer_and_keeps_its_shape() {
        let known = ids(&["me", "a"]);
        // a has two monitors side by side, left of the master.
        let cells = vec![cell("me", 2, 1), Cell::of_display("a", 0, 0, 1), Cell::of_display("a", 1, 1, 1)];
        // Its right monitor goes to (5, 3): the left one follows to (4, 3).
        let moved = move_to(&cells, &known, "a", 1, 5, 3).unwrap();
        assert_eq!(moved.len(), 3);
        assert!(moved.contains(&Cell::of_display("a", 0, 4, 3)) && moved.contains(&Cell::of_display("a", 1, 5, 3)));
        assert!(moved.contains(&cell("me", 2, 1)));
        // One step right lands the left monitor on the right one's old cell: allowed.
        let moved = move_to(&cells, &known, "a", 0, 1, 0).unwrap();
        assert!(moved.contains(&Cell::of_display("a", 0, 1, 0)) && moved.contains(&Cell::of_display("a", 1, 2, 0)));
        // The other monitor would land on the master, or leave the grid: refused.
        assert!(move_to(&cells, &known, "a", 0, 1, 1).is_none());
        assert!(move_to(&cells, &known, "a", 1, 0, 0).is_none());
        assert_eq!(group_offset(&cells, &known, "a", 1, 5, 3), Some((4, 2)));
        // Tiles of an unplugged monitor follow with `shift`; one pushed off the grid is dropped.
        let saved = vec![cell("me", 2, 1), Cell::of_display("a", 0, 0, 1), Cell::of_display("a", 2, 0, 0)];
        assert_eq!(shift(&saved, "a", (1, 0)), vec![cell("me", 2, 1), Cell::of_display("a", 0, 1, 1), Cell::of_display("a", 2, 1, 0)]);
        assert_eq!(shift(&saved, "a", (0, -1)), vec![cell("me", 2, 1), Cell::of_display("a", 0, 0, 0)]);
        // Two monitors of one computer side by side are not a pair of computers.
        let two = vec![cell("me", 2, 1), Cell::of_display("a", 0, 0, 0), Cell::of_display("a", 1, 1, 0)];
        assert!(direct_pairs(&two, "me").is_empty());
    }

    #[test]
    fn cells_of_unplugged_monitors_survive_a_save() {
        let saved = vec![cell("me", 2, 1), cell("a", 1, 1), Cell::of_display("a", 1, 0, 1), Cell::of_display("gone", 1, 5, 3)];
        // The service lists only the first monitor of a now.
        let shown = vec![cell("me", 2, 1), cell("a", 1, 1)];
        let kept = keep_hidden(shown.clone(), &saved);
        assert!(kept.contains(&Cell::of_display("a", 1, 0, 1)));
        assert!(!kept.iter().any(|c| c.id == "gone"), "a computer not shown at all is dropped");
        // Its spot was taken meanwhile: not restored.
        let taken = vec![cell("me", 2, 1), cell("a", 1, 1), cell("b", 0, 1)];
        assert_eq!(keep_hidden(taken.clone(), &saved), taken);
    }

    #[test]
    fn subs_side_by_side_cross_to_each_other() {
        let cells = vec![cell("me", 2, 1), cell("a", 1, 1), cell("b", 1, 0), cell("c", 4, 2)];
        // a and b share an edge; me and a do too, but that is not between two subs.
        assert_eq!(direct_pairs(&cells, "me"), vec![("a".to_string(), "b".to_string())]);
        // Diagonal is not an edge.
        let cells = vec![cell("me", 2, 1), cell("a", 0, 0), cell("b", 1, 1)];
        assert!(direct_pairs(&cells, "me").is_empty());
    }

    #[test]
    fn saving_a_layout_derives_the_sides_and_keeps_the_master() {
        let mut net = NetConfig { peers: vec![peer("a", Some(Side::Left)), peer("b", Some(Side::Right))], ..NetConfig::default() };
        // Move a above the master, drop b to a cell that is no neighbour; master left out.
        set_layout(&mut net, "me", vec![cell("a", 2, 0), cell("b", 4, 2)]);
        assert!(net.layout.iter().any(|c| c.id == "me" && (c.col, c.row) == (2, 1)));
        assert_eq!(net.peers[0].side, Some(Side::Top));
        assert_eq!(net.peers[1].side, None);
        // A later read gives the same grid, not the old sides.
        assert_eq!(effective_cells(&net, "me"), net.layout);
    }

    #[test]
    fn pairs_and_directions_of_linux() {
        assert_eq!(pair_systems(Os::Linux), vec![Os::Mac, Os::Windows, Os::Android]);
        assert_eq!(pair_systems(Os::Mac), vec![Os::Windows, Os::Linux, Os::Android]);
        assert_eq!(pair_systems(Os::Windows), vec![Os::Mac, Os::Linux, Os::Android]);
        assert_eq!(directions(Os::Linux, Os::Mac), vec![(Os::Linux, Os::Mac), (Os::Mac, Os::Linux)]);
        assert_eq!(directions(Os::Linux, Os::Windows), vec![(Os::Linux, Os::Windows), (Os::Windows, Os::Linux)]);
        assert_eq!(directions(Os::Linux, Os::Android), vec![(Os::Linux, Os::Android)]);
    }

    #[test]
    fn the_table_shows_the_modes_keys_and_an_override_wins() {
        let net = NetConfig::default();
        let rules = net.rules_for(Os::Mac, Os::Linux).unwrap();
        // By function: Control -> Super, Option -> Alt, Command -> Ctrl.
        assert_eq!(mod_target(Os::Mac, Os::Linux, &rules, 0), 3);
        assert_eq!(mod_target(Os::Mac, Os::Linux, &rules, 2), 2);
        assert_eq!(mod_target(Os::Mac, Os::Linux, &rules, 3), 0);
        let by_position = DirRules { mod_mode: ModMode::Position, ..rules.clone() };
        assert_eq!(mod_target(Os::Mac, Os::Linux, &by_position, 0), 0);
        assert_eq!(mod_target(Os::Mac, Os::Linux, &by_position, 2), 3);
        assert_eq!(mod_target(Os::Mac, Os::Linux, &by_position, 3), 2);
        // Linux -> Mac by function is the inverse.
        let back = net.rules_for(Os::Linux, Os::Mac).unwrap();
        assert_eq!(mod_target(Os::Linux, Os::Mac, &back, 0), 3);
        assert_eq!(mod_target(Os::Linux, Os::Mac, &back, 3), 0);
    }

    #[test]
    fn the_first_edit_writes_every_direction_so_nothing_else_changes() {
        // 2.0 settings: no swap, Caps Lock only.
        let mut net = NetConfig { swap_cmd_ctrl: false, mac_shift_space_to_hangul: false, ..NetConfig::default() };
        let before = net.all_rules();
        edit_keys(&mut net, Os::Linux, Os::Mac, KeyEdit::Hanja(true));
        assert_eq!(net.keys.len(), 9);
        for (key, rules) in &before {
            let mut expected = rules.clone();
            if key == "linux>mac" {
                expected.hanja = true;
            }
            assert_eq!(net.keys.get(key), Some(&expected), "{key}");
        }
        // Between two computers of one system there are no rules.
        let mut same = NetConfig::default();
        edit_keys(&mut same, Os::Linux, Os::Linux, KeyEdit::Capture(false));
        assert!(same.keys.is_empty());
    }

    #[test]
    fn the_modifier_table_edits_both_sides_and_a_mode_change_starts_it_again() {
        let mut net = NetConfig::default();
        // Ctrl (0) -> Alt (2) instead of Command: both Ctrl keys, left and right.
        edit_keys(&mut net, Os::Linux, Os::Mac, KeyEdit::Modifier { from: 0, to: 2 });
        let rules = net.rules_for(Os::Linux, Os::Mac).unwrap();
        assert_eq!(rules.mods.get(&0xe0), Some(&0xe2));
        assert_eq!(rules.mods.get(&0xe4), Some(&0xe6));
        assert_eq!(mod_target(Os::Linux, Os::Mac, &rules, 0), 2);
        // Choosing what the mode gives anyway takes the override away.
        edit_keys(&mut net, Os::Linux, Os::Mac, KeyEdit::Modifier { from: 0, to: 3 });
        assert!(net.rules_for(Os::Linux, Os::Mac).unwrap().mods.is_empty());
        // A custom entry is dropped when the basis changes.
        edit_keys(&mut net, Os::Linux, Os::Mac, KeyEdit::Modifier { from: 3, to: 2 });
        edit_keys(&mut net, Os::Linux, Os::Mac, KeyEdit::ModMode(ModMode::Position));
        let rules = net.rules_for(Os::Linux, Os::Mac).unwrap();
        assert!(rules.mods.is_empty());
        assert_eq!(rules.mod_mode, ModMode::Position);
        // Other directions were not touched.
        assert!(net.rules_for(Os::Mac, Os::Linux).unwrap().mods.is_empty());
        assert_eq!(net.rules_for(Os::Mac, Os::Linux).unwrap().mod_mode, ModMode::Function);
        // The change is what the sender uses, and survives a TOML round trip.
        let again: NetConfig = toml::from_str(&toml::to_string(&net).unwrap()).unwrap();
        assert_eq!(again.rules_for(Os::Linux, Os::Mac), net.rules_for(Os::Linux, Os::Mac));
    }

    #[test]
    fn korean_english_keys_are_a_multi_choice_in_the_systems_order() {
        let mut net = NetConfig::default();
        let src = |net: &NetConfig| net.rules_for(Os::Mac, Os::Linux).unwrap().hangul_src;
        assert_eq!(src(&net), vec![HangulKey::CapsLock, HangulKey::ShiftSpace]);
        edit_keys(&mut net, Os::Mac, Os::Linux, KeyEdit::HangulSource(HangulKey::ControlSpace, true));
        assert_eq!(src(&net), vec![HangulKey::CapsLock, HangulKey::ControlSpace, HangulKey::ShiftSpace]);
        edit_keys(&mut net, Os::Mac, Os::Linux, KeyEdit::HangulSource(HangulKey::CapsLock, false));
        edit_keys(&mut net, Os::Mac, Os::Linux, KeyEdit::HangulSource(HangulKey::ShiftSpace, false));
        assert_eq!(src(&net), vec![HangulKey::ControlSpace]);
        // None at all is allowed: nothing is turned into the Korean/English key.
        edit_keys(&mut net, Os::Mac, Os::Linux, KeyEdit::HangulSource(HangulKey::ControlSpace, false));
        assert!(src(&net).is_empty());
        edit_keys(&mut net, Os::Mac, Os::Linux, KeyEdit::HangulTarget(HangulKey::SuperSpace));
        assert_eq!(net.rules_for(Os::Mac, Os::Linux).unwrap().hangul_dst, HangulKey::SuperSpace);
        assert_eq!(net.rules_for(Os::Mac, Os::Windows).unwrap().hangul_dst, HangulKey::Lang1);
        // The choices of each system.
        assert_eq!(hangul_sources(Os::Linux), &[HangulKey::Lang1, HangulKey::ShiftSpace, HangulKey::SuperSpace]);
        assert!(hangul_sources(Os::Android).is_empty());
        assert_eq!(hangul_targets(Os::Android), &[HangulKey::ShiftSpace, HangulKey::ControlSpace]);
    }

    #[test]
    fn caps_long_press_hanja_navigation_and_capture_are_switches() {
        let mut net = NetConfig::default();
        edit_keys(&mut net, Os::Mac, Os::Linux, KeyEdit::CapsLongPress(false));
        edit_keys(&mut net, Os::Linux, Os::Mac, KeyEdit::Hanja(false));
        edit_keys(&mut net, Os::Mac, Os::Linux, KeyEdit::Nav(2, false));
        edit_keys(&mut net, Os::Linux, Os::Mac, KeyEdit::Nav(3, false));
        edit_keys(&mut net, Os::Mac, Os::Linux, KeyEdit::Capture(false));
        let r = |net: &NetConfig, a, b| net.rules_for(a, b).unwrap();
        assert!(!r(&net, Os::Mac, Os::Linux).caps_long_press && r(&net, Os::Mac, Os::Windows).caps_long_press);
        assert!(!r(&net, Os::Linux, Os::Mac).hanja && r(&net, Os::Windows, Os::Mac).hanja);
        assert_eq!(r(&net, Os::Mac, Os::Linux).nav, [true, true, false, true]);
        assert_eq!(r(&net, Os::Linux, Os::Mac).nav, [true, true, true, false]);
        assert!(!r(&net, Os::Mac, Os::Linux).capture && r(&net, Os::Mac, Os::Windows).capture);
        // An index past the four is ignored.
        edit_keys(&mut net, Os::Mac, Os::Linux, KeyEdit::Nav(9, false));
        assert_eq!(r(&net, Os::Mac, Os::Linux).nav, [true, true, false, true]);
    }

    #[test]
    fn bottom_rows_match_by_position() {
        assert_eq!(bottom_row(Os::Mac), [0, 2, 3]);
        assert_eq!(bottom_row(Os::Linux), [0, 3, 2]);
        assert!(has_hanja(Os::Linux) && !has_hanja(Os::Android));
    }
}

/// Whether the running flockswitch's input capture is one the system cannot remember
/// (portal version 1, GNOME 50 and older), from the status file's edge note ("ready:1",
/// "asking:1", "off:1"). Unknown or another backend: false.
pub fn input_capture_forgets(backend: &str, note: &str) -> bool {
    backend == "input-capture"
        && note.split_once(':').is_some_and(|(code, version)| matches!(code, "ready" | "asking" | "off") && version.trim() == "1")
}

/// Which notes about the system's permission questions the Connect page shows on a
/// Wayland session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PortalNotes {
    /// The GNOME helper extension is in use: one line saying so, and no other note.
    pub helper: bool,
    /// GNOME 50 and older ask for input capture at every start.
    pub every: bool,
    /// The remote control answer was given without Remember.
    pub not_remembered: bool,
    /// Where a note above shows, a sentence about installing the helper extension.
    pub install_hint: bool,
}

/// The notes for the running flockswitch's edge backend and note, the remote control
/// status ("not-remembered"), and whether this is GNOME.
pub fn portal_notes(backend: &str, note: &str, remote_desktop: &str, gnome: bool) -> PortalNotes {
    if backend == "gnome-helper" {
        return PortalNotes { helper: true, ..PortalNotes::default() };
    }
    let every = input_capture_forgets(backend, note);
    let not_remembered = remote_desktop == "not-remembered";
    PortalNotes { helper: false, every, not_remembered, install_hint: gnome && (every || not_remembered) }
}

#[cfg(test)]
mod portal_note_tests {
    use super::{input_capture_forgets, portal_notes, PortalNotes};

    #[test]
    fn the_helper_replaces_the_portal_notes() {
        let n = portal_notes("gnome-helper", "", "not-remembered", true);
        assert_eq!(n, PortalNotes { helper: true, ..PortalNotes::default() });
        // Even if a stale status still says something about the portals.
        assert!(!portal_notes("gnome-helper", "ready:1", "not-remembered", true).every);
    }

    #[test]
    fn without_the_helper_the_notes_stay_and_gnome_gets_the_hint() {
        let n = portal_notes("input-capture", "ready:1", "", true);
        assert_eq!(n, PortalNotes { helper: false, every: true, not_remembered: false, install_hint: true });
        let n = portal_notes("input-capture", "ready:2", "not-remembered", true);
        assert_eq!(n, PortalNotes { helper: false, every: false, not_remembered: true, install_hint: true });
        // Nothing to say: no hint either. Not GNOME: the notes, but no hint.
        assert_eq!(portal_notes("input-capture", "ready:2", "", true), PortalNotes::default());
        assert!(!portal_notes("input-capture", "ready:1", "", false).install_hint);
        assert!(portal_notes("input-capture", "ready:1", "", false).every);
    }

    #[test]
    fn only_version_one_input_capture_forgets() {
        assert!(input_capture_forgets("input-capture", "ready:1"));
        assert!(input_capture_forgets("input-capture", "asking:1"));
        assert!(input_capture_forgets("input-capture", "off:1"));
        assert!(!input_capture_forgets("input-capture", "ready:2"));
        assert!(!input_capture_forgets("input-capture", "off"));
        assert!(!input_capture_forgets("input-capture", "denied"));
        assert!(!input_capture_forgets("layer-shell", "ready:1"));
    }
}
