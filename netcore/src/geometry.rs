use crate::proto::Side;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
impl Rect {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
    fn valid(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.width >= 1.0
            && self.height >= 1.0
    }
    fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}
#[derive(Clone, Copy)]
struct Edge {
    fixed: f64,
    start: f64,
    end: f64,
    /// Index of the monitor this segment belongs to, in the list given to `edges`.
    display: usize,
}
fn vertical(side: Side) -> bool {
    matches!(side, Side::Left | Side::Right)
}
fn edges(displays: &[Rect], side: Side) -> Vec<Edge> {
    let mut result = Vec::new();
    for (display, r) in displays.iter().copied().enumerate().filter(|(_, r)| r.valid()) {
        let (fixed, start, end, outside) = match side {
            Side::Left => (r.x, r.y, r.y + r.height - 1.0, r.x - 0.5),
            Side::Right => (
                r.x + r.width - 1.0,
                r.y,
                r.y + r.height - 1.0,
                r.x + r.width,
            ),
            Side::Top => (r.y, r.x, r.x + r.width - 1.0, r.y - 0.5),
            Side::Bottom => (
                r.y + r.height - 1.0,
                r.x,
                r.x + r.width - 1.0,
                r.y + r.height,
            ),
        };
        let mut intervals = vec![(start, end)];
        for other in displays.iter().copied().filter(|o| o.valid()) {
            let (lo, hi, covered) = if vertical(side) {
                (
                    other.y,
                    other.y + other.height - 1.0,
                    outside >= other.x && outside < other.x + other.width,
                )
            } else {
                (
                    other.x,
                    other.x + other.width - 1.0,
                    outside >= other.y && outside < other.y + other.height,
                )
            };
            if !covered {
                continue;
            }
            intervals = intervals
                .into_iter()
                .flat_map(|(a, b)| {
                    let mut parts = Vec::new();
                    if hi < a || lo > b {
                        parts.push((a, b));
                    } else {
                        if lo > a {
                            parts.push((a, lo - 1.0));
                        }
                        if hi < b {
                            parts.push((hi + 1.0, b));
                        }
                    }
                    parts
                })
                .collect();
        }
        result.extend(
            intervals
                .into_iter()
                .filter(|(a, b)| b >= a)
                .map(|(start, end)| Edge { fixed, start, end, display }),
        );
    }
    result.sort_by(|a, b| {
        a.start
            .total_cmp(&b.start)
            .then(a.fixed.total_cmp(&b.fixed))
    });
    result
}
pub fn edge_at(displays: &[Rect], x: f64, y: f64, corner_margin: f64) -> Option<Side> {
    edge_hit(displays, x, y, corner_margin).map(|(_, side)| side)
}
/// The monitor (index into `sorted_displays`) and side of the outer edge the cursor is at.
pub fn edge_at_display(displays: &[Rect], x: f64, y: f64, corner_margin: f64) -> Option<(u8, Side)> {
    let sorted = sorted_displays(displays);
    edge_hit(&sorted, x, y, corner_margin).map(|(i, side)| (i.min(u8::MAX as usize) as u8, side))
}
fn edge_hit(displays: &[Rect], x: f64, y: f64, corner_margin: f64) -> Option<(usize, Side)> {
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let (index, r) = displays
        .iter()
        .copied()
        .enumerate()
        .find(|(_, r)| r.valid() && r.contains(x, y))?;
    let beyond = |x, y| displays.iter().any(|r| r.valid() && r.contains(x, y));
    let margin = corner_margin.max(0.0);
    let along_y = y >= r.y + margin && y < r.y + r.height - margin;
    let along_x = x >= r.x + margin && x < r.x + r.width - margin;
    if along_y && x <= r.x && !beyond(r.x - 1.0, y) {
        return Some((index, Side::Left));
    }
    if along_y && x >= r.x + r.width - 1.0 && !beyond(r.x + r.width, y) {
        return Some((index, Side::Right));
    }
    if along_x && y <= r.y && !beyond(x, r.y - 1.0) {
        return Some((index, Side::Top));
    }
    if along_x && y >= r.y + r.height - 1.0 && !beyond(x, r.y + r.height) {
        return Some((index, Side::Bottom));
    }
    None
}
/// The usable monitors in a stable order: by x, then y of the top-left corner. A monitor
/// index (`Cell::display`, `Msg::Displays`, `Enter::display`) is a position in this list.
pub fn sorted_displays(displays: &[Rect]) -> Vec<Rect> {
    let mut list: Vec<Rect> = displays.iter().copied().filter(|r| r.valid()).collect();
    list.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    list
}
/// Position (0..1) of the point along the `side` edge of one monitor.
pub fn display_edge_ratio(display: Rect, side: Side, x: f64, y: f64) -> f64 {
    if !display.valid() || !x.is_finite() || !y.is_finite() {
        return 0.0;
    }
    let (p, start, length) = if vertical(side) {
        (y, display.y, display.height - 1.0)
    } else {
        (x, display.x, display.width - 1.0)
    };
    if length > 0.0 {
        ((p - start) / length).clamp(0.0, 1.0)
    } else {
        0.0
    }
}
/// Two points inside the `side` edge of monitor `display`, `pos` (0..1) along that edge.
/// None when the monitor does not exist.
pub fn display_edge_position(displays: &[Rect], display: u8, side: Side, pos: f64) -> Option<(f64, f64)> {
    let r = *sorted_displays(displays).get(usize::from(display))?;
    let pos = if pos.is_finite() { pos.clamp(0.0, 1.0) } else { 0.5 };
    let (x, y) = if vertical(side) {
        let y = r.y + pos * (r.height - 1.0);
        (if side == Side::Left { r.x + 2.0 } else { r.x + r.width - 1.0 - 2.0 }, y)
    } else {
        let x = r.x + pos * (r.width - 1.0);
        (x, if side == Side::Top { r.y + 2.0 } else { r.y + r.height - 1.0 - 2.0 })
    };
    clamp(&[r], x, y)
}
/// Which monitor holds the point `pos` (0..1) along the exposed `side` edge (the scale
/// `edge_ratio` and `Platform::take_edge_hit` use), and the position along that monitor's
/// own edge.
pub fn display_for_edge_pos(displays: &[Rect], side: Side, pos: f64) -> Option<(u8, f64)> {
    let sorted = sorted_displays(displays);
    let edges = edges(&sorted, side);
    let total: f64 = edges.iter().map(|e| e.end - e.start).sum();
    let mut distance = if pos.is_finite() { pos.clamp(0.0, 1.0) } else { 0.5 } * total.max(0.0);
    let last = edges.len().checked_sub(1)?;
    for (i, e) in edges.into_iter().enumerate() {
        let length = e.end - e.start;
        if distance <= length || i == last {
            let p = e.start + distance.min(length);
            let r = sorted[e.display];
            let (start, size) = if vertical(side) { (r.y, r.height - 1.0) } else { (r.x, r.width - 1.0) };
            let ratio = if size > 0.0 { ((p - start) / size).clamp(0.0, 1.0) } else { 0.0 };
            return Some((e.display.min(u8::MAX as usize) as u8, ratio));
        }
        distance -= length;
    }
    None
}

/// Position along the exposed edge segments, ordered by desktop coordinate.
pub fn edge_ratio(displays: &[Rect], side: Side, x: f64, y: f64) -> f64 {
    let edges = edges(displays, side);
    let total: f64 = edges.iter().map(|e| e.end - e.start).sum();
    let (f, p) = if vertical(side) { (x, y) } else { (y, x) };
    let mut best = (f64::INFINITY, 0.0);
    let mut offset = 0.0;
    for e in edges {
        let q = p.clamp(e.start, e.end);
        let distance = (f - e.fixed).powi(2) + (p - q).powi(2);
        if distance < best.0 {
            best = (distance, offset + q - e.start);
        }
        offset += e.end - e.start;
    }
    if total > 0.0 {
        (best.1 / total).clamp(0.0, 1.0)
    } else {
        0.0
    }
}
/// Place the cursor two points inside the selected edge.
pub fn edge_position(displays: &[Rect], side: Side, pos: f64) -> Option<(f64, f64)> {
    let edges = edges(displays, side);
    let total: f64 = edges.iter().map(|e| e.end - e.start).sum();
    let mut distance = if pos.is_finite() {
        pos.clamp(0.0, 1.0)
    } else {
        0.5
    } * total.max(0.0);
    let last = edges.len().saturating_sub(1);
    for (i, e) in edges.into_iter().enumerate() {
        let length = e.end - e.start;
        if distance <= length || i == last {
            let p = e.start + distance.min(length);
            let f = e.fixed
                + match side {
                    Side::Left | Side::Top => 2.0,
                    _ => -2.0,
                };
            let (x, y) = if vertical(side) { (f, p) } else { (p, f) };
            return clamp(displays, x, y);
        }
        distance -= length;
    }
    None
}
pub fn clamp(displays: &[Rect], x: f64, y: f64) -> Option<(f64, f64)> {
    let (x, y) = (
        if x.is_finite() { x } else { 0.0 },
        if y.is_finite() { y } else { 0.0 },
    );
    let mut best = None;
    let mut distance = f64::INFINITY;
    for r in displays.iter().copied().filter(|r| r.valid()) {
        if r.contains(x, y) {
            return Some((x, y));
        }
        let p = (
            x.clamp(r.x, r.x + r.width - 1.0),
            y.clamp(r.y, r.y + r.height - 1.0),
        );
        let d = (p.0 - x).powi(2) + (p.1 - y).powi(2);
        if d < distance {
            best = Some(p);
            distance = d;
        }
    }
    best
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn outer_edges_and_ratios() {
        let d = [
            Rect::new(0.0, 0.0, 100.0, 100.0),
            Rect::new(100.0, 0.0, 100.0, 100.0),
        ];
        assert_eq!(edge_at(&d, 99.0, 50.0, 5.0), None);
        assert_eq!(edge_at(&d, 100.0, 50.0, 5.0), None);
        assert_eq!(edge_at(&d, 199.0, 50.0, 5.0), Some(Side::Right));
        assert_eq!(edge_at(&d, 0.0, 2.0, 5.0), None);
        for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            for pos in [0.0, 0.1, 0.5, 0.9, 1.0] {
                let (x, y) = edge_position(&d, side, pos).unwrap();
                assert!((edge_ratio(&d, side, x, y) - pos).abs() < 1e-9);
                assert_eq!(clamp(&d, x, y), Some((x, y)));
            }
        }
        let stagger = [d[0], Rect::new(100.0, 50.0, 100.0, 100.0)];
        assert_eq!(edge_at(&stagger, 99.0, 25.0, 0.0), Some(Side::Right));
        assert_eq!(edge_at(&stagger, 99.0, 75.0, 0.0), None);
        assert_eq!(clamp(&stagger, 150.0, 0.0), Some((150.0, 50.0)));
    }
    fn two() -> [Rect; 2] {
        // Listed right to left on purpose: the index follows the sorted order.
        [Rect::new(100.0, 0.0, 200.0, 100.0), Rect::new(0.0, 0.0, 100.0, 100.0)]
    }
    #[test]
    fn displays_are_sorted_by_x_then_y_and_unusable_ones_dropped() {
        let list = [Rect::new(100.0, 50.0, 10.0, 10.0), Rect::new(0.0, 0.0, 0.5, 10.0), Rect::new(100.0, 0.0, 10.0, 10.0), Rect::new(-50.0, 0.0, 10.0, 10.0), Rect::new(f64::NAN, 0.0, 10.0, 10.0)];
        let sorted = sorted_displays(&list);
        assert_eq!(sorted.iter().map(|r| (r.x, r.y)).collect::<Vec<_>>(), [(-50.0, 0.0), (100.0, 0.0), (100.0, 50.0)]);
        assert_eq!(sorted_displays(&sorted), sorted);
    }
    #[test]
    fn edge_at_display_names_the_monitor_of_the_outer_edge() {
        let d = two();
        // The right monitor (index 1 after sorting) owns the right edge, the left one the left edge.
        assert_eq!(edge_at_display(&d, 299.0, 50.0, 5.0), Some((1, Side::Right)));
        assert_eq!(edge_at_display(&d, 0.0, 50.0, 5.0), Some((0, Side::Left)));
        assert_eq!(edge_at_display(&d, 50.0, 0.0, 5.0), Some((0, Side::Top)));
        assert_eq!(edge_at_display(&d, 150.0, 99.0, 5.0), Some((1, Side::Bottom)));
        // The seam between the two is not an outer edge.
        assert_eq!(edge_at_display(&d, 99.0, 50.0, 5.0), None);
        assert_eq!(edge_at_display(&d, 100.0, 50.0, 5.0), None);
        assert_eq!(edge_at_display(&d, f64::NAN, 50.0, 5.0), None);
        // Same answer as the side-only function.
        assert_eq!(edge_at(&d, 299.0, 50.0, 5.0), Some(Side::Right));
    }
    #[test]
    fn display_ratio_and_position_round_trip_per_monitor() {
        let d = two();
        let right = sorted_displays(&d)[1];
        for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            for display in 0..2u8 {
                for pos in [0.0, 0.25, 0.5, 1.0] {
                    let (x, y) = display_edge_position(&d, display, side, pos).unwrap();
                    let r = sorted_displays(&d)[usize::from(display)];
                    assert!(x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height, "{side:?} {display} {pos}");
                    assert!((display_edge_ratio(r, side, x, y) - pos).abs() < 1e-9, "{side:?} {display} {pos}");
                }
            }
        }
        // Two points inside the edge.
        assert_eq!(display_edge_position(&d, 1, Side::Left, 0.0), Some((102.0, 0.0)));
        assert_eq!(display_edge_position(&d, 1, Side::Right, 1.0), Some((297.0, 99.0)));
        assert_eq!(display_edge_position(&d, 0, Side::Top, 0.5).map(|p| p.1), Some(2.0));
        assert_eq!(display_edge_position(&d, 2, Side::Left, 0.5), None);
        assert_eq!(display_edge_position(&d, 0, Side::Left, f64::NAN).map(|p| p.1), Some(49.5));
        assert_eq!(display_edge_ratio(right, Side::Left, 100.0, 49.5), 0.5);
        assert_eq!(display_edge_ratio(right, Side::Left, 100.0, -20.0), 0.0);
        assert_eq!(display_edge_ratio(Rect::new(0.0, 0.0, 0.0, 0.0), Side::Left, 0.0, 0.0), 0.0);
    }
    #[test]
    fn whole_edge_position_maps_to_the_monitor_and_back() {
        let d = two();
        // Top edge: left monitor covers 0..99 of the 0..299 span, right one the rest.
        let (display, ratio) = display_for_edge_pos(&d, Side::Top, 0.1).unwrap();
        assert_eq!(display, 0);
        assert!((ratio - 29.8 / 99.0).abs() < 1e-6, "{ratio}");
        let (display, ratio) = display_for_edge_pos(&d, Side::Top, 0.9).unwrap();
        assert_eq!(display, 1);
        let (x, y) = display_edge_position(&d, display, Side::Top, ratio).unwrap();
        assert!((edge_ratio(&d, Side::Top, x, y) - 0.9).abs() < 0.01);
        // Left and right edges belong to one monitor each.
        assert_eq!(display_for_edge_pos(&d, Side::Right, 0.5).map(|p| p.0), Some(1));
        assert_eq!(display_for_edge_pos(&d, Side::Left, 0.5).map(|p| p.0), Some(0));
        assert_eq!(display_for_edge_pos(&d, Side::Left, 2.0).map(|p| p.0), Some(0));
        assert_eq!(display_for_edge_pos(&[], Side::Left, 0.5), None);
        // Staggered: only the exposed part of the right edge of the first monitor counts.
        let stagger = [Rect::new(0.0, 0.0, 100.0, 100.0), Rect::new(100.0, 50.0, 100.0, 100.0)];
        let (display, ratio) = display_for_edge_pos(&stagger, Side::Right, 0.1).unwrap();
        assert_eq!(display, 0);
        assert!(ratio < 0.5, "{ratio}");
        assert_eq!(display_for_edge_pos(&stagger, Side::Right, 0.9).map(|p| p.0), Some(1));
    }
    #[test]
    fn a_single_monitor_matches_the_whole_edge_functions() {
        let one = [Rect::new(10.0, 20.0, 300.0, 200.0)];
        for side in [Side::Left, Side::Right, Side::Top, Side::Bottom] {
            for pos in [0.0, 0.3, 1.0] {
                let (x, y) = edge_position(&one, side, pos).unwrap();
                assert_eq!(display_edge_position(&one, 0, side, pos), Some((x, y)));
                let (d, r) = display_for_edge_pos(&one, side, pos).unwrap();
                assert_eq!(d, 0);
                assert!((r - pos).abs() < 1e-9);
                assert!((display_edge_ratio(one[0], side, x, y) - pos).abs() < 1e-9);
            }
        }
    }
}
