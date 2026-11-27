//! The layout grid: where each computer's monitors sit and who is next to whom.
use crate::geometry::{self, Rect};
use crate::proto::{Os, Side};
use crate::PeerConfig;
use serde::{Deserialize, Serialize};

pub const COLS: u8 = 6;
pub const ROWS: u8 = 4;
/// Where the master sits when nothing else says so.
pub const MASTER_CELL: (u8, u8) = (2, 1);

/// One monitor of one computer on the 6x4 grid (col 0..5, row 0..3). Saved 5x3 layouts
/// stay valid: every cell of them fits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub id: String,
    pub col: u8,
    pub row: u8,
    /// The computer's monitor (position in `geometry::sorted_displays`, 0 = first).
    /// Layouts and peers from before 2.2 have none, which means the first.
    #[serde(default)]
    pub display: u8,
}
impl Cell {
    /// The cell of the computer's first monitor.
    pub fn new(id: &str, col: u8, row: u8) -> Self {
        Self { id: id.into(), col, row, display: 0 }
    }
    pub fn of_display(id: &str, display: u8, col: u8, row: u8) -> Self {
        Self { id: id.into(), col, row, display }
    }
}
/// A computer in the master's group, as the master tells its subs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub name: String,
    pub os: Os,
    pub fingerprint: String,
    /// Where to reach it (empty for the master itself).
    #[serde(default)]
    pub addr: String,
    #[serde(default = "yes")]
    pub can_send: bool,
    /// Optional features the computer advertised in its Hello (see `proto::FEATURES`).
    #[serde(default)]
    pub features: Vec<String>,
}
pub(crate) fn yes() -> bool {
    true
}

/// The cells inside the grid, one per (computer, monitor) and one per position (the
/// first wins).
pub fn valid_cells(cells: &[Cell]) -> Vec<Cell> {
    let mut out: Vec<Cell> = Vec::new();
    for c in cells {
        let fits = c.col < COLS && c.row < ROWS;
        if fits
            && !out
                .iter()
                .any(|o| (o.id == c.id && o.display == c.display) || (o.col, o.row) == (c.col, c.row))
        {
            out.push(c.clone());
        }
    }
    out
}
/// The layout the 2.0 sides stand for: the master in the middle, peers around it.
pub fn cells_from_sides(master: &str, peers: &[PeerConfig]) -> Vec<Cell> {
    let mut cells = vec![Cell::new(master, MASTER_CELL.0, MASTER_CELL.1)];
    for p in peers {
        let Some(side) = p.side else { continue };
        let (col, row) = step(MASTER_CELL, side).unwrap_or(MASTER_CELL);
        cells.push(Cell::new(&p.id, col, row));
    }
    valid_cells(&cells)
}
/// A master's layout with the master itself in it (in the middle if it was left out).
pub fn with_master(cells: &[Cell], master: &str) -> Vec<Cell> {
    with_master_displays(cells, master, &[])
}
/// Like `with_master`, and every monitor of the master has a cell too: the first one in
/// the middle (or nearest free), the others next to it as the monitors are arranged
/// (see `auto_place`). `displays` is the master's monitor list; empty means one.
pub fn with_master_displays(cells: &[Cell], master: &str, displays: &[Rect]) -> Vec<Cell> {
    let mut cells = valid_cells(cells);
    if !cells.iter().any(|c| c.id == master) {
        // The middle if free, else the free cell nearest to it.
        let (col, row) = nearest_free(&cells, MASTER_CELL).unwrap_or_else(|| {
            // A full saved grid must still include the master. Evict the middle cell.
            cells.retain(|c| (c.col, c.row) != MASTER_CELL);
            MASTER_CELL
        });
        cells.push(Cell::new(master, col, row));
    }
    auto_place(&cells, master, displays)
}
/// The free cell nearest to `to` (Manhattan distance; ties go to the lower row, then column).
fn nearest_free(cells: &[Cell], to: (u8, u8)) -> Option<(u8, u8)> {
    (0..ROWS)
        .flat_map(|row| (0..COLS).map(move |col| (col, row)))
        .filter(|&at| !cells.iter().any(|c| (c.col, c.row) == at))
        .min_by_key(|&(col, row)| col.abs_diff(to.0) + row.abs_diff(to.1))
}
/// Adds the monitor cells computer `id` is missing, next to its existing cells and following
/// the real arrangement: a monitor whose rectangle is right of an already placed one goes to
/// the right of that one's cell, below goes below, and so on. If that exact spot is taken (or
/// off the grid) the monitor stays unplaced: it then leaves and is entered with the
/// computer-level rule of `neighbour_of`. `displays` is the computer's monitor list (any
/// order; empty or one monitor adds nothing). A computer with no cell at all is left alone
/// (the user has not placed it yet). Cells of monitor numbers beyond the list are not touched.
///
/// A 2.1 layout has one cell per computer, which reads as monitor 0. When that single cell
/// of a multi-monitor computer has neighbours, it is given to the monitor that faces them if
/// one clearly does (see `facing_display`), so the other monitors are placed on the free
/// side. Only a lone cell of monitor 0 is changed this way.
pub fn auto_place(cells: &[Cell], id: &str, displays: &[Rect]) -> Vec<Cell> {
    let rects = geometry::sorted_displays(displays);
    let mut out = cells.to_vec();
    if rects.len() > 1 {
        let own: Vec<usize> = (0..out.len()).filter(|&i| out[i].id == id).collect();
        if let [i] = own[..] {
            if out[i].display == 0 {
                if let Some(d) = facing_display(&out, &out[i], &rects) {
                    out[i].display = d;
                }
            }
        }
    }
    for k in 0..rects.len().min(usize::from(u8::MAX) + 1) {
        let k8 = k as u8;
        if out.iter().any(|c| c.id == id && c.display == k8) {
            continue;
        }
        let center = |r: Rect| (r.x + r.width / 2.0, r.y + r.height / 2.0);
        let (kx, ky) = center(rects[k]);
        // The placed monitor of this computer that is closest to the new one.
        let anchor = out
            .iter()
            .filter(|c| c.id == id && usize::from(c.display) < rects.len())
            .min_by(|a, b| {
                let dist = |c: &Cell| {
                    let (x, y) = center(rects[usize::from(c.display)]);
                    (x - kx).powi(2) + (y - ky).powi(2)
                };
                dist(a).total_cmp(&dist(b)).then(a.display.cmp(&b.display))
            })
            .cloned();
        let Some(anchor) = anchor else { return out };
        let (ax, ay) = center(rects[usize::from(anchor.display)]);
        let (dx, dy) = (kx - ax, ky - ay);
        let side = if dx.abs() >= dy.abs() {
            if dx >= 0.0 { Side::Right } else { Side::Left }
        } else if dy >= 0.0 {
            Side::Bottom
        } else {
            Side::Top
        };
        if let Some((col, row)) = step((anchor.col, anchor.row), side) {
            if !out.iter().any(|c| (c.col, c.row) == (col, row)) {
                out.push(Cell::of_display(id, k8, col, row));
            }
        }
    }
    out
}
/// The monitor (of the sorted `rects`) that the other computers around `cell` face: for each
/// side with a neighbour, the monitor furthest out on that side. Some only when every such
/// side has one clear (strictly furthest) monitor and they are all the same one.
fn facing_display(cells: &[Cell], cell: &Cell, rects: &[Rect]) -> Option<u8> {
    let mut found: Option<usize> = None;
    for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
        let Some(at) = step((cell.col, cell.row), side) else { continue };
        if !cells.iter().any(|c| (c.col, c.row) == at && c.id != cell.id) {
            continue;
        }
        // Larger is further out on `side`.
        let out = |r: &Rect| match side {
            Side::Left => -(r.x + r.width / 2.0),
            Side::Right => r.x + r.width / 2.0,
            Side::Top => -(r.y + r.height / 2.0),
            Side::Bottom => r.y + r.height / 2.0,
        };
        let best = rects.iter().map(out).fold(f64::NEG_INFINITY, f64::max);
        let mut at_best = (0..rects.len()).filter(|&i| (out(&rects[i]) - best).abs() < 1e-6);
        let (Some(first), None) = (at_best.next(), at_best.next()) else { return None };
        if found.is_some_and(|f| f != first) {
            return None;
        }
        found = Some(first);
    }
    found.and_then(|d| u8::try_from(d).ok())
}
/// Drops the cells of monitors that no longer exist. `count` gives a computer's monitor
/// count, or None when it is not known (then its cells stay). The saved layout keeps them;
/// they only stop counting while the monitor is away.
pub fn live_cells(cells: &[Cell], count: impl Fn(&str) -> Option<usize>) -> Vec<Cell> {
    cells
        .iter()
        .filter(|c| count(&c.id).is_none_or(|n| usize::from(c.display) < n.max(1)))
        .cloned()
        .collect()
}
fn step((col, row): (u8, u8), side: Side) -> Option<(u8, u8)> {
    let (c, r) = match side {
        Side::Left => (col.checked_sub(1)?, row),
        Side::Right => (col + 1, row),
        Side::Top => (col, row.checked_sub(1)?),
        Side::Bottom => (col, row + 1),
    };
    (c < COLS && r < ROWS).then_some((c, r))
}
/// The computer in the cell next to `id`'s first monitor on `side`.
pub fn neighbour<'a>(cells: &'a [Cell], id: &str, side: Side) -> Option<&'a str> {
    neighbour_of(cells, id, 0, side).map(|(id, _)| id)
}
/// The computer and monitor in the cell next to monitor `display` of `id` on `side`. A
/// neighbour that is the same computer (another of its own monitors) is no crossing: the
/// operating system moves the cursor there.
///
/// A monitor that has no cell of its own (a 2.1 layout, or a spot that was taken) uses the
/// computer-level rule of 2.1: the neighbour on that side of any of the computer's cells. If
/// several cells have a different neighbour there, the cell whose monitor number is closest to
/// `display` wins (the lower number on a tie), as the missing monitor's own row or column is
/// not known. None when the computer has no cell at all.
pub fn neighbour_of<'a>(cells: &'a [Cell], id: &str, display: u8, side: Side) -> Option<(&'a str, u8)> {
    let next = |me: &Cell| {
        let at = step((me.col, me.row), side)?;
        cells.iter().find(|c| (c.col, c.row) == at && c.id != id).map(|c| (c.id.as_str(), c.display))
    };
    if let Some(me) = cells.iter().find(|c| c.id == id && c.display == display) {
        return next(me);
    }
    cells
        .iter()
        .filter(|c| c.id == id)
        .filter_map(|c| next(c).map(|n| (c.display, n)))
        .min_by_key(|(d, _)| (d.abs_diff(display), *d))
        .map(|(_, n)| n)
}
/// The computer next to any monitor of `id` on `side` (the first monitor that has one).
pub fn neighbour_on_side<'a>(cells: &'a [Cell], id: &str, side: Side) -> Option<&'a str> {
    cells
        .iter()
        .filter(|c| c.id == id)
        .find_map(|c| neighbour_of(cells, id, c.display, side).map(|(other, _)| other))
}
/// The side of `from` (of any of its monitors) that `to` is on, if the two share an edge.
pub fn side_of(cells: &[Cell], from: &str, to: &str) -> Option<Side> {
    cells.iter().filter(|c| c.id == from).find_map(|c| {
        [Side::Left, Side::Right, Side::Top, Side::Bottom]
            .into_iter()
            .find(|&s| neighbour_of(cells, from, c.display, s).is_some_and(|(id, _)| id == to))
    })
}
/// Every computer that shares an edge with any monitor of `id`.
pub fn neighbours<'a>(cells: &'a [Cell], id: &str) -> Vec<&'a str> {
    let mut out: Vec<&str> = Vec::new();
    for c in cells.iter().filter(|c| c.id == id) {
        for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            if let Some((other, _)) = neighbour_of(cells, id, c.display, side) {
                if !out.contains(&other) {
                    out.push(other);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cell(id: &str, col: u8, row: u8) -> Cell {
        Cell::new(id, col, row)
    }
    fn mon(id: &str, display: u8, col: u8, row: u8) -> Cell {
        Cell::of_display(id, display, col, row)
    }
    fn peer(id: &str, side: Option<Side>) -> PeerConfig {
        PeerConfig { id: id.into(), side, ..PeerConfig::default() }
    }
    fn at(cells: &[Cell], id: &str, display: u8) -> Option<(u8, u8)> {
        cells.iter().find(|c| c.id == id && c.display == display).map(|c| (c.col, c.row))
    }
    #[test]
    fn grid_is_6_by_4_and_old_5_by_3_layouts_fit() {
        assert_eq!((COLS, ROWS, MASTER_CELL), (6, 4, (2, 1)));
        let old: Vec<Cell> = (0..3).flat_map(|row| (0..5).map(move |col| cell(&format!("p{col}{row}"), col, row))).collect();
        assert_eq!(valid_cells(&old), old);
    }
    #[test]
    fn a_cell_without_display_reads_as_the_first_monitor() {
        let c: Cell = serde_json::from_str(r#"{"id":"a","col":1,"row":2}"#).unwrap();
        assert_eq!(c, cell("a", 1, 2));
        let c: Cell = toml::from_str("id = \"a\"\ncol = 1\nrow = 2\n").unwrap();
        assert_eq!(c.display, 0);
        let json = serde_json::to_string(&mon("a", 1, 0, 0)).unwrap();
        assert_eq!(serde_json::from_str::<Cell>(&json).unwrap().display, 1);
    }
    #[test]
    fn neighbours_share_an_edge() {
        let cells = [cell("m", 2, 1), cell("l", 1, 1), cell("r", 3, 1), cell("t", 2, 0), cell("d", 0, 2)];
        assert_eq!(neighbour(&cells, "m", Side::Left), Some("l"));
        assert_eq!(neighbour(&cells, "m", Side::Right), Some("r"));
        assert_eq!(neighbour(&cells, "m", Side::Top), Some("t"));
        assert_eq!(neighbour(&cells, "m", Side::Bottom), None);
        // Sub to sub: the one on the left has the master on its right, and nothing diagonal.
        assert_eq!(neighbour(&cells, "l", Side::Right), Some("m"));
        assert_eq!(neighbour(&cells, "l", Side::Bottom), None);
        assert_eq!(neighbour(&cells, "l", Side::Left), None);
        assert_eq!(neighbour(&cells, "d", Side::Top), None);
        assert_eq!(neighbour(&cells, "d", Side::Left), None);
        assert_eq!(neighbour(&cells, "unknown", Side::Left), None);
        let line = [cell("a", 3, 1), cell("b", 4, 1)];
        assert_eq!(neighbour(&line, "a", Side::Right), Some("b"));
        assert_eq!(neighbour(&line, "b", Side::Right), None);
        assert_eq!(side_of(&cells, "m", "t"), Some(Side::Top));
        assert_eq!(side_of(&cells, "l", "m"), Some(Side::Right));
        assert_eq!(side_of(&cells, "l", "r"), None);
        // The grid is wider and taller now.
        let edge = [cell("a", 4, 3), cell("b", 5, 3), cell("c", 4, 2)];
        assert_eq!(neighbour(&edge, "a", Side::Right), Some("b"));
        assert_eq!(neighbour(&edge, "b", Side::Right), None);
        assert_eq!(neighbour(&edge, "a", Side::Bottom), None);
        assert_eq!(neighbour(&edge, "a", Side::Top), Some("c"));
    }
    #[test]
    fn neighbour_of_works_per_monitor_and_skips_own_monitors() {
        // Master has two monitors side by side, a laptop sits right of the second one,
        // a tablet above the first.
        let cells = [mon("m", 0, 1, 1), mon("m", 1, 2, 1), cell("lap", 3, 1), cell("tab", 1, 0)];
        assert_eq!(neighbour_of(&cells, "m", 1, Side::Right), Some(("lap", 0)));
        assert_eq!(neighbour_of(&cells, "m", 0, Side::Right), None, "own monitor is not a crossing");
        assert_eq!(neighbour_of(&cells, "m", 1, Side::Left), None);
        assert_eq!(neighbour_of(&cells, "m", 0, Side::Top), Some(("tab", 0)));
        assert_eq!(neighbour_of(&cells, "m", 1, Side::Top), None);
        assert_eq!(neighbour_of(&cells, "lap", 0, Side::Left), Some(("m", 1)));
        assert_eq!(neighbour_of(&cells, "lap", 1, Side::Left), Some(("m", 1)), "no cell for this monitor: the computer's cell decides");
        assert_eq!(neighbour_of(&cells, "ghost", 0, Side::Left), None);
        // Display 0 of the old functions.
        assert_eq!(neighbour(&cells, "m", Side::Top), Some("tab"));
        assert_eq!(neighbour(&cells, "m", Side::Right), None);
        assert_eq!(side_of(&cells, "m", "lap"), Some(Side::Right));
        assert_eq!(side_of(&cells, "m", "tab"), Some(Side::Top));
        assert_eq!(side_of(&cells, "lap", "m"), Some(Side::Left));
        assert_eq!(neighbours(&cells, "m"), ["tab", "lap"]);
        // Any monitor counts for `neighbour_on_side`.
        assert_eq!(neighbour_on_side(&cells, "m", Side::Right), Some("lap"));
        assert_eq!(neighbour_on_side(&cells, "m", Side::Top), Some("tab"));
        assert_eq!(neighbour_on_side(&cells, "m", Side::Bottom), None);
        assert_eq!(neighbour_on_side(&cells, "ghost", Side::Top), None);
    }
    #[test]
    fn layout_from_2_0_sides() {
        let peers = [peer("a", Some(Side::Left)), peer("b", Some(Side::Right)), peer("c", Some(Side::Top)), peer("n", None)];
        let cells = cells_from_sides("m", &peers);
        let find = |id: &str| cells.iter().find(|c| c.id == id).map(|c| (c.col, c.row));
        assert_eq!(find("m"), Some((2, 1)));
        assert_eq!((find("a"), find("b"), find("c")), (Some((1, 1)), Some((3, 1)), Some((2, 0))));
        assert_eq!(find("n"), None);
        assert_eq!(cells_from_sides("m", &[peer("d", Some(Side::Bottom))])[1], cell("d", 2, 2));
        // Two peers on one side: the first keeps it.
        let both = cells_from_sides("m", &[peer("a", Some(Side::Left)), peer("b", Some(Side::Left))]);
        assert_eq!(both.len(), 2);
    }
    #[test]
    fn invalid_cells_are_dropped() {
        let cells = valid_cells(&[cell("a", 6, 0), cell("b", 0, 4), cell("c", 1, 1), cell("c", 2, 2), cell("d", 1, 1), cell("e", 5, 3)]);
        assert_eq!(cells, vec![cell("c", 1, 1), cell("e", 5, 3)]);
        assert_eq!(with_master(&[cell("x", 0, 0)], "m"), vec![cell("x", 0, 0), cell("m", 2, 1)]);
        assert_eq!(with_master(&[cell("m", 0, 0)], "m"), vec![cell("m", 0, 0)]);
    }
    #[test]
    fn one_cell_per_computer_and_monitor_and_per_position() {
        let cells = valid_cells(&[mon("a", 0, 1, 1), mon("a", 1, 2, 1), mon("a", 1, 3, 1), mon("a", 2, 1, 1), mon("b", 0, 2, 1), mon("b", 0, 4, 2)]);
        // b's first cell loses its position to a's second monitor; its next one is free.
        assert_eq!(cells, vec![mon("a", 0, 1, 1), mon("a", 1, 2, 1), mon("b", 0, 4, 2)]);
        let cells = valid_cells(&[mon("a", 1, 1, 1), mon("a", 0, 2, 1), mon("a", 0, 3, 3)]);
        assert_eq!(cells, vec![mon("a", 1, 1, 1), mon("a", 0, 2, 1)]);
    }
    #[test]
    fn full_grid_still_reserves_a_master_cell() {
        let full: Vec<_> = (0..ROWS).flat_map(|row| (0..COLS).map(move |col| cell(&format!("p{col}{row}"), col, row))).collect();
        let fixed = with_master(&full, "master");
        assert_eq!(fixed.len(), (COLS * ROWS) as usize);
        assert!(fixed.contains(&cell("master", MASTER_CELL.0, MASTER_CELL.1)));
        assert_eq!(valid_cells(&fixed), fixed);
    }

    #[test]
    fn master_never_loses_its_cell() {
        // Someone else holds the middle: the master takes the nearest free cell.
        let cells = with_master(&[cell("x", 2, 1), cell("y", 1, 1)], "m");
        assert!(cells.iter().any(|c| c.id == "m"));
        assert_eq!(cells.len(), 3);
        assert_eq!(cells.iter().find(|c| c.id == "m").map(|c| (c.col, c.row)), Some((2, 0)));
        // Every cell taken near the middle still leaves the master somewhere.
        let mut full = vec![];
        for row in 0..ROWS {
            for col in 0..COLS {
                if (col, row) != (5, 3) {
                    full.push(cell(&format!("p{col}{row}"), col, row));
                }
            }
        }
        let cells = with_master(&full, "m");
        assert_eq!(cells.len(), full.len() + 1);
        assert_eq!(cells.iter().find(|c| c.id == "m").map(|c| (c.col, c.row)), Some((5, 3)));
    }
    fn rect(x: f64, y: f64) -> Rect {
        Rect::new(x, y, 100.0, 100.0)
    }
    #[test]
    fn auto_placement_follows_the_monitor_arrangement() {
        // Right of the first monitor.
        let cells = auto_place(&[cell("a", 1, 1)], "a", &[rect(0.0, 0.0), rect(100.0, 0.0)]);
        assert_eq!((at(&cells, "a", 0), at(&cells, "a", 1)), (Some((1, 1)), Some((2, 1))));
        // Left of it (the list order is the screen order, not the argument order).
        let cells = auto_place(&[cell("a", 3, 1)], "a", &[rect(100.0, 0.0), rect(0.0, 0.0)]);
        assert_eq!(at(&cells, "a", 1), Some((4, 1)));
        let cells = auto_place(&[mon("a", 1, 3, 1)], "a", &[rect(100.0, 0.0), rect(0.0, 0.0)]);
        assert_eq!(at(&cells, "a", 0), Some((2, 1)), "monitor 0 is the left one, placed left of monitor 1");
        // Below and above (sorted by x then y: the upper one is index 0).
        let cells = auto_place(&[cell("a", 1, 1)], "a", &[rect(0.0, 0.0), rect(0.0, 100.0)]);
        assert_eq!(at(&cells, "a", 1), Some((1, 2)));
        let cells = auto_place(&[mon("a", 1, 1, 2)], "a", &[rect(0.0, 0.0), rect(0.0, 100.0)]);
        assert_eq!(at(&cells, "a", 0), Some((1, 1)));
        // Three in a row: each goes next to the closest placed one.
        let cells = auto_place(&[cell("a", 0, 0)], "a", &[rect(0.0, 0.0), rect(100.0, 0.0), rect(200.0, 0.0)]);
        assert_eq!((at(&cells, "a", 1), at(&cells, "a", 2)), (Some((1, 0)), Some((2, 0))));
    }
    #[test]
    fn auto_placement_only_uses_the_exact_spot_and_leaves_others_alone() {
        // The spot right of the monitor is taken (and the lone cell has a neighbour on the
        // right, so the right monitor keeps it): the left monitor goes to the free spot left.
        let before = [cell("a", 1, 1), cell("b", 2, 1)];
        let cells = auto_place(&before, "a", &[rect(0.0, 0.0), rect(100.0, 0.0)]);
        assert_eq!((at(&cells, "a", 1), at(&cells, "a", 0)), (Some((1, 1)), Some((0, 1))));
        // The spot is taken and there is no way to give the cell to another monitor: unplaced.
        let before = [cell("a", 1, 1), cell("b", 2, 1), cell("c", 0, 1)];
        let cells = auto_place(&before, "a", &[rect(0.0, 0.0), rect(100.0, 0.0)]);
        assert_eq!(cells.iter().filter(|c| c.id == "a").count(), 1, "{cells:?}");
        assert_eq!(cells.len(), 3);
        // A computer that is not placed yet, one monitor, a complete set and a full grid change nothing.
        let before = [cell("a", 1, 1), cell("b", 4, 3)];
        assert_eq!(auto_place(&before, "zzz", &[rect(0.0, 0.0), rect(100.0, 0.0)]), before);
        assert_eq!(auto_place(&before, "a", &[rect(0.0, 0.0)]), before);
        assert_eq!(auto_place(&before, "a", &[]), before);
        let two = auto_place(&before, "a", &[rect(0.0, 0.0), rect(100.0, 0.0)]);
        assert_eq!(auto_place(&two, "a", &[rect(0.0, 0.0), rect(100.0, 0.0)]), two);
        let full: Vec<_> = (0..ROWS).flat_map(|row| (0..COLS).map(move |col| cell(&format!("p{col}{row}"), col, row))).collect();
        assert_eq!(auto_place(&full, "p00", &[rect(0.0, 0.0), rect(100.0, 0.0)]), full);
        // At the grid edge there is no spot: the monitor stays unplaced.
        let cells = auto_place(&[cell("a", 5, 0)], "a", &[rect(0.0, 0.0), rect(100.0, 0.0)]);
        assert_eq!(cells, vec![cell("a", 5, 0)]);
        // The result is always a valid layout.
        assert_eq!(valid_cells(&cells), cells);
    }
    #[test]
    fn the_lone_cell_goes_to_the_monitor_that_faces_the_neighbours() {
        let two = [rect(0.0, 0.0), rect(100.0, 0.0)];
        // Master M with two monitors, a sub S on the right (the 2.1 layout of the bug report).
        let cells = auto_place(&[cell("m", 2, 1), cell("s", 3, 1)], "m", &two);
        assert_eq!((at(&cells, "m", 1), at(&cells, "m", 0)), (Some((2, 1)), Some((1, 1))));
        assert_eq!(at(&cells, "s", 0), Some((3, 1)));
        // S on the left: monitor 0 keeps the cell, monitor 1 goes to the free spot on its right.
        let cells = auto_place(&[cell("m", 2, 1), cell("s", 1, 1)], "m", &two);
        assert_eq!((at(&cells, "m", 0), at(&cells, "m", 1)), (Some((2, 1)), Some((3, 1))));
        // Neighbours on both sides, or none: nothing clearly faces them, the cell stays monitor 0.
        let cells = auto_place(&[cell("m", 2, 1), cell("s", 1, 1), cell("t", 3, 1)], "m", &two);
        assert_eq!(at(&cells, "m", 0), Some((2, 1)));
        assert_eq!(at(&cells, "m", 1), None, "both neighbouring spots are taken");
        let cells = auto_place(&[cell("m", 2, 1)], "m", &two);
        assert_eq!((at(&cells, "m", 0), at(&cells, "m", 1)), (Some((2, 1)), Some((3, 1))));
        // Stacked monitors with a neighbour on the right: no monitor is further right.
        let cells = auto_place(&[cell("m", 2, 1), cell("s", 3, 1)], "m", &[rect(0.0, 0.0), rect(0.0, 100.0)]);
        assert_eq!((at(&cells, "m", 0), at(&cells, "m", 1)), (Some((2, 1)), Some((2, 2))));
        // Above: the upper monitor (index 0) faces it.
        let cells = auto_place(&[cell("m", 2, 1), cell("s", 2, 0)], "m", &[rect(0.0, 0.0), rect(0.0, 100.0)]);
        assert_eq!((at(&cells, "m", 0), at(&cells, "m", 1)), (Some((2, 1)), Some((2, 2))));
        // A cell that already names a monitor, or a complete set, is never reassigned.
        let cells = auto_place(&[mon("m", 1, 2, 1), cell("s", 3, 1)], "m", &two);
        assert_eq!((at(&cells, "m", 1), at(&cells, "m", 0)), (Some((2, 1)), Some((1, 1))));
    }
    #[test]
    fn a_monitor_without_a_cell_uses_the_computer_level_neighbour() {
        // m has one cell, so both its monitors cross to the neighbour of that cell.
        let cells = [cell("m", 2, 1), cell("s", 3, 1), cell("t", 2, 0)];
        assert_eq!(neighbour_of(&cells, "m", 1, Side::Right), Some(("s", 0)));
        assert_eq!(neighbour_of(&cells, "m", 5, Side::Top), Some(("t", 0)));
        assert_eq!(neighbour_of(&cells, "m", 1, Side::Left), None);
        assert_eq!(neighbour_of(&cells, "ghost", 1, Side::Left), None);
        // Several cells with different neighbours: the closest monitor number decides.
        let cells = [mon("m", 0, 1, 1), mon("m", 2, 3, 1), cell("l", 0, 1), cell("r", 4, 1), cell("u", 1, 0), cell("v", 3, 0)];
        assert_eq!(neighbour_of(&cells, "m", 1, Side::Top), Some(("u", 0)), "tie goes to the lower monitor");
        assert_eq!(neighbour_of(&cells, "m", 3, Side::Top), Some(("v", 0)));
        assert_eq!(neighbour_of(&cells, "m", 1, Side::Right), Some(("r", 0)), "only monitor 2's cell has one");
        // A monitor with a cell of its own never borrows another's neighbour.
        assert_eq!(neighbour_of(&cells, "m", 0, Side::Right), None);
    }
    #[test]
    fn the_master_gets_a_cell_for_every_monitor() {
        let two = [rect(0.0, 0.0), rect(100.0, 0.0)];
        let cells = with_master_displays(&[cell("x", 0, 0)], "m", &two);
        assert_eq!((at(&cells, "m", 0), at(&cells, "m", 1)), (Some((2, 1)), Some((3, 1))));
        // A master that already has its first cell only gets the missing one.
        let cells = with_master_displays(&[cell("m", 4, 2)], "m", &two);
        assert_eq!((at(&cells, "m", 0), at(&cells, "m", 1)), (Some((4, 2)), Some((5, 2))));
        // One monitor: same as with_master.
        assert_eq!(with_master_displays(&[cell("x", 0, 0)], "m", &[rect(0.0, 0.0)]), with_master(&[cell("x", 0, 0)], "m"));
    }
    #[test]
    fn cells_of_monitors_that_are_gone_stop_counting() {
        let cells = [mon("a", 0, 1, 1), mon("a", 1, 2, 1), mon("b", 0, 3, 1), mon("b", 1, 4, 1), mon("c", 2, 0, 0)];
        let live = live_cells(&cells, |id| match id {
            "a" => Some(1),
            "b" => Some(2),
            _ => None,
        });
        assert_eq!(live, vec![mon("a", 0, 1, 1), mon("b", 0, 3, 1), mon("b", 1, 4, 1), mon("c", 2, 0, 0)]);
        assert_eq!(live_cells(&cells, |_| Some(0)).len(), 2, "zero monitors counts as one: only the first monitor of a and b stays");
    }
}
