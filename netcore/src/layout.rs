//! The layout grid: where each computer sits and who is next to whom.
use crate::proto::{Os, Side};
use crate::PeerConfig;
use serde::{Deserialize, Serialize};

pub const COLS: u8 = 5;
pub const ROWS: u8 = 3;
/// Where the master sits when nothing else says so.
pub const MASTER_CELL: (u8, u8) = (2, 1);

/// One computer's cell on the 5x3 grid (col 0..4, row 0..2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    pub id: String,
    pub col: u8,
    pub row: u8,
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

/// The cells inside the grid, one per id and one per position (the first wins).
pub fn valid_cells(cells: &[Cell]) -> Vec<Cell> {
    let mut out: Vec<Cell> = Vec::new();
    for c in cells {
        let fits = c.col < COLS && c.row < ROWS;
        if fits && !out.iter().any(|o| o.id == c.id || (o.col, o.row) == (c.col, c.row)) {
            out.push(c.clone());
        }
    }
    out
}
/// The layout the 2.0 sides stand for: the master in the middle, peers around it.
pub fn cells_from_sides(master: &str, peers: &[PeerConfig]) -> Vec<Cell> {
    let mut cells = vec![Cell { id: master.into(), col: MASTER_CELL.0, row: MASTER_CELL.1 }];
    for p in peers {
        let Some(side) = p.side else { continue };
        let (col, row) = step(MASTER_CELL, side).unwrap_or(MASTER_CELL);
        cells.push(Cell { id: p.id.clone(), col, row });
    }
    valid_cells(&cells)
}
/// A master's layout with the master itself in it (in the middle if it was left out).
pub fn with_master(cells: &[Cell], master: &str) -> Vec<Cell> {
    let mut cells = valid_cells(cells);
    if !cells.iter().any(|c| c.id == master) {
        // The middle if free, else the free cell nearest to it.
        let free = (0..ROWS)
            .flat_map(|row| (0..COLS).map(move |col| (col, row)))
            .filter(|&at| !cells.iter().any(|c| (c.col, c.row) == at))
            .min_by_key(|&(col, row)| col.abs_diff(MASTER_CELL.0) + row.abs_diff(MASTER_CELL.1));
        let (col, row) = free.unwrap_or_else(|| {
            // A full saved grid must still include the master. Evict the middle cell.
            cells.retain(|c| (c.col, c.row) != MASTER_CELL);
            MASTER_CELL
        });
        cells.push(Cell { id: master.into(), col, row });
    }
    cells
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
/// The computer in the cell next to `id`'s on `side`.
pub fn neighbour<'a>(cells: &'a [Cell], id: &str, side: Side) -> Option<&'a str> {
    let me = cells.iter().find(|c| c.id == id)?;
    let at = step((me.col, me.row), side)?;
    cells.iter().find(|c| (c.col, c.row) == at).map(|c| c.id.as_str())
}
/// The side of `from` that `to` is on, if the two share an edge.
pub fn side_of(cells: &[Cell], from: &str, to: &str) -> Option<Side> {
    [Side::Left, Side::Right, Side::Top, Side::Bottom]
        .into_iter()
        .find(|&s| neighbour(cells, from, s) == Some(to))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cell(id: &str, col: u8, row: u8) -> Cell {
        Cell { id: id.into(), col, row }
    }
    fn peer(id: &str, side: Option<Side>) -> PeerConfig {
        PeerConfig { id: id.into(), side, ..PeerConfig::default() }
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
    }
    #[test]
    fn layout_from_2_0_sides() {
        let peers = [peer("a", Some(Side::Left)), peer("b", Some(Side::Right)), peer("c", Some(Side::Top)), peer("n", None)];
        let cells = cells_from_sides("m", &peers);
        let at = |id: &str| cells.iter().find(|c| c.id == id).map(|c| (c.col, c.row));
        assert_eq!(at("m"), Some((2, 1)));
        assert_eq!((at("a"), at("b"), at("c")), (Some((1, 1)), Some((3, 1)), Some((2, 0))));
        assert_eq!(at("n"), None);
        assert_eq!(cells_from_sides("m", &[peer("d", Some(Side::Bottom))])[1], cell("d", 2, 2));
        // Two peers on one side: the first keeps it.
        let both = cells_from_sides("m", &[peer("a", Some(Side::Left)), peer("b", Some(Side::Left))]);
        assert_eq!(both.len(), 2);
    }
    #[test]
    fn invalid_cells_are_dropped() {
        let cells = valid_cells(&[cell("a", 5, 0), cell("b", 0, 3), cell("c", 1, 1), cell("c", 2, 2), cell("d", 1, 1), cell("e", 4, 2)]);
        assert_eq!(cells, vec![cell("c", 1, 1), cell("e", 4, 2)]);
        assert_eq!(with_master(&[cell("x", 0, 0)], "m"), vec![cell("x", 0, 0), cell("m", 2, 1)]);
        assert_eq!(with_master(&[cell("m", 0, 0)], "m"), vec![cell("m", 0, 0)]);
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
                if (col, row) != (4, 2) {
                    full.push(cell(&format!("p{col}{row}"), col, row));
                }
            }
        }
        let cells = with_master(&full, "m");
        assert_eq!(cells.len(), full.len() + 1);
        assert_eq!(cells.iter().find(|c| c.id == "m").map(|c| (c.col, c.row)), Some((4, 2)));
    }
}
