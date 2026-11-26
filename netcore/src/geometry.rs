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
}
fn vertical(side: Side) -> bool {
    matches!(side, Side::Left | Side::Right)
}
fn edges(displays: &[Rect], side: Side) -> Vec<Edge> {
    let mut result = Vec::new();
    for r in displays.iter().copied().filter(|r| r.valid()) {
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
                .map(|(start, end)| Edge { fixed, start, end }),
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
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let r = displays
        .iter()
        .copied()
        .find(|r| r.valid() && r.contains(x, y))?;
    let beyond = |x, y| displays.iter().any(|r| r.valid() && r.contains(x, y));
    let margin = corner_margin.max(0.0);
    let along_y = y >= r.y + margin && y < r.y + r.height - margin;
    let along_x = x >= r.x + margin && x < r.x + r.width - margin;
    if along_y && x <= r.x && !beyond(r.x - 1.0, y) {
        return Some(Side::Left);
    }
    if along_y && x >= r.x + r.width - 1.0 && !beyond(r.x + r.width, y) {
        return Some(Side::Right);
    }
    if along_x && y <= r.y && !beyond(x, r.y - 1.0) {
        return Some(Side::Top);
    }
    if along_x && y >= r.y + r.height - 1.0 && !beyond(x, r.y + r.height) {
        return Some(Side::Bottom);
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
}
