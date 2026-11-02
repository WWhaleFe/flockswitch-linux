//! Tray icon drawn in code: two opposing arrows (⇄).

type Rgba = [u8; 4];

/// Icon pixels (RGBA, row-major). With `tile` the arrows sit on a rounded square,
/// as in the app icon; without it only the arrows are drawn.
pub fn arrows(size: u32, arrow: Rgba, tile: Option<Rgba>) -> Vec<u8> {
    const SS: u32 = 4; // supersampling per axis
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut a_hits, mut t_hits) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if in_arrows(x, y) {
                        a_hits += 1;
                    }
                    if tile.is_some() && in_rounded_square(x, y, 0.22) {
                        t_hits += 1;
                    }
                }
            }
            let n = (SS * SS) as f32;
            let (ac, tc) = (a_hits as f32 / n, t_hits as f32 / n);
            let px = match tile {
                Some(t) => blend(t, tc, arrow, ac),
                None => [arrow[0], arrow[1], arrow[2], (arrow[3] as f32 * ac) as u8],
            };
            out.extend_from_slice(&px);
        }
    }
    out
}

/// Arrow drawn over a tile: arrow coverage on top of tile coverage.
fn blend(tile: Rgba, tc: f32, arrow: Rgba, ac: f32) -> Rgba {
    let ac = ac.min(tc); // arrows never stick out of the tile
    let mix = |t: u8, a: u8| (t as f32 * (1.0 - ac) + a as f32 * ac) as u8;
    [
        mix(tile[0], arrow[0]),
        mix(tile[1], arrow[1]),
        mix(tile[2], arrow[2]),
        (tile[3] as f32 * tc) as u8,
    ]
}

fn in_arrows(x: f32, y: f32) -> bool {
    const HALF: f32 = 0.055;
    // Upper arrow pointing right, lower arrow pointing left.
    capsule(x, y, (0.18, 0.36), (0.68, 0.36), HALF)
        || triangle(x, y, (0.86, 0.36), (0.62, 0.20), (0.62, 0.52))
        || capsule(x, y, (0.82, 0.64), (0.32, 0.64), HALF)
        || triangle(x, y, (0.14, 0.64), (0.38, 0.48), (0.38, 0.80))
}

fn capsule(x: f32, y: f32, a: (f32, f32), b: (f32, f32), r: f32) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

fn triangle(x: f32, y: f32, a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let side = |p: (f32, f32), q: (f32, f32)| (q.0 - p.0) * (y - p.1) - (q.1 - p.1) * (x - p.0);
    let (d1, d2, d3) = (side(a, b), side(b, c), side(c, a));
    let neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(neg && pos)
}

fn in_rounded_square(x: f32, y: f32, r: f32) -> bool {
    let cx = x.clamp(r, 1.0 - r);
    let cy = y.clamp(r, 1.0 - r);
    (x - cx).powi(2) + (y - cy).powi(2) <= r * r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_something_in_the_middle_and_nothing_in_the_corner() {
        let px = arrows(32, [0, 0, 0, 255], None);
        let alpha = |x: usize, y: usize| px[(y * 32 + x) * 4 + 3];
        assert_eq!(alpha(0, 0), 0);
        assert!(alpha(16, 11) > 200, "upper arrow shaft");
        assert!(alpha(16, 20) > 200, "lower arrow shaft");
    }
}
