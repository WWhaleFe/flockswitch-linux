//! Click-through animations drawn into an RGBA buffer: the countdown while the cursor
//! waits at a screen edge, and the highlight that shows where the cursor is. overlay.rs
//! puts the buffer on the screen; the macOS port draws the same effects with CoreGraphics
//! (overlay_macos.m). This file is byte-identical in the Windows and Linux ports.
//!
//! Every effect is one drawing function in a table, indexed by the number its enum has in
//! config.rs (EdgeAnimStyle, LocateStyle). To add an effect: write the function, add it to
//! the table, add the enum variant and its labels.
//!
//! Like overlay_macos.m, the effects work in points with y pointing up; `Canvas` turns
//! that into the buffer's pixels (y down). The shadows of CoreGraphics are emulated with
//! wider, low-alpha strokes.

use tiny_skia::{
    Color, FillRule, LineCap, Mask, Paint, Path, PathBuilder, Pixmap, Rect, Stroke, Transform,
};

/// How far into the screen the edge overlay reaches, in points.
pub const STRIP_DEPTH: f64 = 240.0;
/// Side of the square drawn around the cursor, in points.
pub const LOCATE_SIDE: f64 = 420.0;
pub const LOCATE_SECONDS: f64 = 1.3;
const LOCATE_FADE: f64 = 0.35;

pub const EDGE_LEFT: i32 = 0;
pub const EDGE_RIGHT: i32 = 1;
pub const EDGE_TOP: i32 = 2;
pub const EDGE_BOTTOM: i32 = 3;

const DIR_CLOCKWISE: i32 = 0;
const DIR_COUNTERCLOCKWISE: i32 = 1;
const DIR_BOTH: i32 = 2;

/// The accent of every effect: #FFCC00.
const YELLOW: (f32, f32, f32) = (1.0, 0.8, 0.0);

const PI: f64 = std::f64::consts::PI;
const FRAC_PI_2: f64 = std::f64::consts::FRAC_PI_2;

/// Extra stroke width (as a share of the blur) and opacity of each pass of a glow.
const GLOW_PASSES: [(f64, f64); 3] = [(1.8, 0.08), (1.1, 0.14), (0.5, 0.22)];

// ---- Drawing helpers -------------------------------------------------------------

#[derive(Clone, Copy)]
struct V {
    x: f64,
    y: f64,
}

fn v(x: f64, y: f64) -> V {
    V { x, y }
}

/// What the effects draw on: the buffer, plus the drawing state CoreGraphics keeps.
struct Canvas<'a> {
    pixmap: &'a mut Pixmap,
    /// From points with y up to the buffer's pixels with y down.
    to_pixels: Transform,
    line_width: f64,
    round_cap: bool,
    /// Blur and opacity of the glow around what is drawn next.
    glow: Option<(f64, f64)>,
}

impl Canvas<'_> {
    fn set_line_width(&mut self, width: f64) {
        self.line_width = width;
    }

    fn set_round_cap(&mut self) {
        self.round_cap = true;
    }

    fn glow(&mut self, blur: f64, alpha: f64) {
        self.glow = Some((blur, alpha));
    }

    fn no_glow(&mut self) {
        self.glow = None;
    }

    fn stroke_with(&mut self, path: &Path, width: f64, alpha: f64) {
        let stroke = Stroke {
            width: width as f32,
            line_cap: if self.round_cap { LineCap::Round } else { LineCap::Butt },
            ..Stroke::default()
        };
        self.pixmap.stroke_path(path, &yellow_paint(alpha), &stroke, self.to_pixels, None);
    }

    fn paint_glow(&mut self, path: &Path, width: f64, alpha: f64) {
        if let Some((blur, strength)) = self.glow {
            for (spread, share) in GLOW_PASSES {
                self.stroke_with(path, width + blur * spread, alpha * strength * share);
            }
        }
    }

    /// Strokes what `pb` holds in yellow, with the current width and glow.
    fn stroke(&mut self, pb: PathBuilder, alpha: f64) {
        if alpha <= 0.0 {
            return;
        }
        let Some(path) = pb.finish() else { return };
        self.paint_glow(&path, self.line_width, alpha);
        self.stroke_with(&path, self.line_width, alpha);
    }

    /// Fills what `pb` holds in yellow, with the current glow around its outline.
    fn fill(&mut self, pb: PathBuilder, alpha: f64) {
        if alpha <= 0.0 {
            return;
        }
        let Some(path) = pb.finish() else { return };
        self.paint_glow(&path, 0.0, alpha);
        self.pixmap.fill_path(&path, &yellow_paint(alpha), FillRule::Winding, self.to_pixels, None);
    }
}

fn yellow_paint(alpha: f64) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(Color::from_rgba(YELLOW.0, YELLOW.1, YELLOW.2, clamp01(alpha) as f32).unwrap_or(Color::TRANSPARENT));
    paint
}

fn circle_path(pb: &mut PathBuilder, o: V, r: f64) {
    if r > 0.0 {
        pb.push_circle(o.x as f32, o.y as f32, r as f32);
    }
}

fn line_path(pb: &mut PathBuilder, a: V, b: V) {
    pb.move_to(a.x as f32, a.y as f32);
    pb.line_to(b.x as f32, b.y as f32);
}

/// An arc from `start` to `end` (radians, y up) in the given direction, with the same
/// meaning of `clockwise` as CGContextAddArc. `joined` continues the current subpath
/// with a line to the arc's start, as CoreGraphics does when a point is current.
fn arc_path(pb: &mut PathBuilder, o: V, r: f64, start: f64, end: f64, clockwise: bool, joined: bool) {
    let mut sweep = if clockwise { start - end } else { end - start };
    if sweep < 0.0 {
        sweep += 2.0 * PI * (-sweep / (2.0 * PI)).ceil();
    }
    let sweep = sweep.min(2.0 * PI) * if clockwise { -1.0 } else { 1.0 };
    let segments = (sweep.abs() / FRAC_PI_2).ceil().max(1.0) as usize;
    let step = sweep / segments as f64;
    // Handle length of a Bézier segment approximating a circle arc of `step`.
    let k = 4.0 / 3.0 * (step / 4.0).tan();
    let at = |a: f64| v(o.x + r * a.cos(), o.y + r * a.sin());
    let first = at(start);
    if joined {
        pb.line_to(first.x as f32, first.y as f32);
    } else {
        pb.move_to(first.x as f32, first.y as f32);
    }
    for i in 0..segments {
        let (a0, a1) = (start + step * i as f64, start + step * (i + 1) as f64);
        let (p0, p1) = (at(a0), at(a1));
        let c1 = v(p0.x - r * k * a0.sin(), p0.y + r * k * a0.cos());
        let c2 = v(p1.x + r * k * a1.sin(), p1.y - r * k * a1.cos());
        pb.cubic_to(c1.x as f32, c1.y as f32, c2.x as f32, c2.y as f32, p1.x as f32, p1.y as f32);
    }
}

/// Repeatable noise in 0..1, so particles and sparks stay put from frame to frame.
fn noise(n: i32) -> f64 {
    let s = (f64::from(n) * 127.1).sin() * 43758.5453;
    s - s.floor()
}

fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

fn smooth(from: f64, to: f64, v: f64) -> f64 {
    let x = clamp01((v - from) / (to - from));
    x * x * (3.0 - 2.0 * x)
}

// ---- Edge countdown effects ------------------------------------------------------

struct EdgeFrame {
    size: V,
    /// The cursor, projected onto the screen edge.
    anchor: V,
    /// Unit vector pointing into the screen.
    inward: V,
    /// Unit vector along the edge.
    along: V,
    /// Share of the wait that has passed (0..1); at 1 the effect has gone.
    p: f64,
    /// Seconds since the wait began.
    t: f64,
    direction: i32,
    shrink: bool,
}

impl EdgeFrame {
    /// A point `a` along the edge and `d` into the screen, from the cursor.
    fn from_anchor(&self, a: f64, d: f64) -> V {
        v(self.anchor.x + self.along.x * a + self.inward.x * d, self.anchor.y + self.along.y * a + self.inward.y * d)
    }

    fn inward_angle(&self) -> f64 {
        self.inward.y.atan2(self.inward.x)
    }
}

type EdgeRenderer = fn(&mut Canvas, &EdgeFrame);

fn edge_ring(c: &mut Canvas, f: &EdgeFrame) {
    const RADIUS: f64 = 24.0;
    let left = 1.0 - f.p;
    if left <= 0.002 {
        return;
    }
    let o = f.from_anchor(0.0, RADIUS + 18.0);
    c.set_round_cap();
    c.set_line_width(5.0);
    if f.shrink {
        c.glow(12.0, 0.9);
        let mut pb = PathBuilder::new();
        circle_path(&mut pb, o, RADIUS * left);
        c.stroke(pb, 1.0);
        return;
    }
    let mut pb = PathBuilder::new();
    circle_path(&mut pb, o, RADIUS);
    c.stroke(pb, 0.18);
    c.glow(12.0, 0.9);
    const TOP: f64 = FRAC_PI_2;
    let mut pb = PathBuilder::new();
    match f.direction {
        DIR_COUNTERCLOCKWISE => arc_path(&mut pb, o, RADIUS, TOP + 2.0 * PI * f.p, TOP + 2.0 * PI, false, false),
        DIR_BOTH => arc_path(&mut pb, o, RADIUS, -FRAC_PI_2 - PI * left, -FRAC_PI_2 + PI * left, false, false),
        _ => arc_path(&mut pb, o, RADIUS, TOP - 2.0 * PI * f.p, TOP - 2.0 * PI, true, false),
    }
    c.stroke(pb, 1.0);
}

fn edge_semicircle(c: &mut Canvas, f: &EdgeFrame) {
    const RADIUS: f64 = 54.0;
    let left = 1.0 - f.p;
    if left <= 0.002 {
        return;
    }
    let o = f.anchor;
    let mid = f.inward_angle();
    c.set_round_cap();
    if f.shrink {
        c.glow(14.0, 0.8);
        let mut pb = PathBuilder::new();
        pb.move_to(o.x as f32, o.y as f32);
        arc_path(&mut pb, o, RADIUS * left, mid - FRAC_PI_2, mid + FRAC_PI_2, false, true);
        pb.close();
        c.fill(pb, 0.85);
        return;
    }
    c.set_line_width(6.0);
    let mut pb = PathBuilder::new();
    arc_path(&mut pb, o, RADIUS, mid - FRAC_PI_2, mid + FRAC_PI_2, false, false);
    c.stroke(pb, 0.18);

    let (from, to, clockwise) = match f.direction {
        DIR_COUNTERCLOCKWISE => (mid - FRAC_PI_2 + PI * f.p, mid + FRAC_PI_2, false),
        DIR_BOTH => (mid - FRAC_PI_2 * left, mid + FRAC_PI_2 * left, false),
        _ => (mid + FRAC_PI_2 - PI * f.p, mid - FRAC_PI_2, true),
    };
    let mut pb = PathBuilder::new();
    pb.move_to(o.x as f32, o.y as f32);
    arc_path(&mut pb, o, RADIUS, from, to, clockwise, true);
    pb.close();
    c.fill(pb, 0.16);
    c.glow(12.0, 0.9);
    let mut pb = PathBuilder::new();
    arc_path(&mut pb, o, RADIUS, from, to, clockwise, false);
    c.stroke(pb, 1.0);
}

fn edge_bar(c: &mut Canvas, f: &EdgeFrame) {
    const LENGTH: f64 = 260.0;
    const INSET: f64 = 5.0;
    let (mut low, mut high) = (-LENGTH / 2.0, LENGTH / 2.0);
    // Clockwise eats the bar from the top, or from the left on a horizontal edge.
    let vertical = f.along.y != 0.0;
    let from_high = vertical == (f.direction == DIR_CLOCKWISE);
    if f.direction == DIR_BOTH {
        low += LENGTH * f.p / 2.0;
        high -= LENGTH * f.p / 2.0;
    } else if from_high {
        high -= LENGTH * f.p;
    } else {
        low += LENGTH * f.p;
    }
    c.set_round_cap();
    c.set_line_width(6.0);
    let mut pb = PathBuilder::new();
    line_path(&mut pb, f.from_anchor(-LENGTH / 2.0, INSET), f.from_anchor(LENGTH / 2.0, INSET));
    c.stroke(pb, 0.16);
    if high - low <= 0.5 {
        return;
    }
    c.glow(14.0, 1.0);
    let mut pb = PathBuilder::new();
    line_path(&mut pb, f.from_anchor(low, INSET), f.from_anchor(high, INSET));
    c.stroke(pb, 1.0);
}

fn edge_pulse(c: &mut Canvas, f: &EdgeFrame) {
    // The last fifth of the wait pulls the waves in to a final ping.
    let contract = smooth(0.8, 1.0, f.p);
    let reach = 130.0 * (1.0 - contract);
    c.glow(8.0, 0.7);
    c.set_line_width(2.5);
    for i in 0..3 {
        let phase = (f.t * 1.4 + f64::from(i) / 3.0) % 1.0;
        let mut pb = PathBuilder::new();
        circle_path(&mut pb, f.anchor, phase * reach);
        c.stroke(pb, (1.0 - phase) * 0.9 * (1.0 - contract * 0.5));
    }
    let core = (8.0 + 3.0 * (f.t * 10.0).sin()) * (1.0 - contract) + 16.0 * (PI * contract).sin() * (1.0 - contract);
    if core <= 0.05 {
        return;
    }
    c.glow(16.0, 1.0);
    let mut pb = PathBuilder::new();
    circle_path(&mut pb, f.anchor, core);
    c.fill(pb, 1.0);
}

fn edge_laser(c: &mut Canvas, f: &EdgeFrame) {
    const INSET: f64 = 3.0;
    let here = f.anchor.x * f.along.x + f.anchor.y * f.along.y;
    let total = f.size.x * f.along.x + f.size.y * f.along.y;
    let ends = [-here, total - here];
    let closing = f.p * f.p * (3.0 - 2.0 * f.p);
    let fade = 1.0 - smooth(0.95, 1.0, f.p);
    let frame = (f.t * 60.0).floor() as i32;
    c.set_round_cap();
    c.glow(12.0, 0.9);
    if fade > 0.0 {
        for (side, &end) in ends.iter().enumerate() {
            let head = end * (1.0 - closing);
            let tail = (end - head).abs().min(240.0) * if end < 0.0 { -1.0 } else { 1.0 };
            const STEPS: i32 = 14;
            for k in 0..STEPS {
                let a0 = head + tail * f64::from(k) / f64::from(STEPS);
                let a1 = head + tail * f64::from(k + 1) / f64::from(STEPS);
                let jitter = (noise(frame + k * 7 + side as i32) - 0.5) * 2.4;
                c.set_line_width(3.5);
                let mut pb = PathBuilder::new();
                line_path(&mut pb, f.from_anchor(a0, INSET + jitter), f.from_anchor(a1, INSET + jitter));
                c.stroke(pb, (1.0 - f64::from(k) / f64::from(STEPS)).powf(1.5) * fade);
            }
        }
    }
    let burst = smooth(0.84, 1.0, f.p);
    if burst <= 0.0 || burst >= 1.0 {
        return;
    }
    c.no_glow();
    c.glow(6.0, 1.0);
    c.set_line_width(1.6);
    let mid = f.inward_angle();
    let mut pb = PathBuilder::new();
    for i in 0..18 {
        let angle = mid + (noise(i * 3) - 0.5) * PI * 0.95;
        let r0 = 8.0 + 70.0 * burst * noise(i * 5);
        let r1 = r0 + 12.0 + 26.0 * noise(i * 7);
        line_path(
            &mut pb,
            v(f.anchor.x + angle.cos() * r0, f.anchor.y + angle.sin() * r0),
            v(f.anchor.x + angle.cos() * r1, f.anchor.y + angle.sin() * r1),
        );
    }
    c.stroke(pb, 1.0 - burst);
}

fn edge_vortex(c: &mut Canvas, f: &EdgeFrame) {
    let o = f.from_anchor(0.0, 72.0);
    let closing = f.p.powf(1.6);
    let fade = 1.0 - smooth(0.92, 1.0, f.p);
    if fade <= 0.0 {
        return;
    }
    c.glow(6.0, 0.8);
    for i in 0..56 {
        let speed = (3.0 + 11.0 * f.p) * (0.7 + 0.6 * noise(i + 100));
        let angle = noise(i) * 2.0 * PI + f.t * speed;
        let radius = (24.0 + 58.0 * noise(i + 200)) * (1.0 - closing);
        let size = (1.2 + 2.4 * noise(i + 300)) * (1.0 - f.p * 0.5);
        let mut pb = PathBuilder::new();
        circle_path(&mut pb, v(o.x + angle.cos() * radius, o.y + angle.sin() * radius), size);
        c.fill(pb, (0.35 + 0.65 * noise(i + 400)) * fade);
    }
    c.glow(18.0, 1.0);
    let mut pb = PathBuilder::new();
    circle_path(&mut pb, o, (3.0 + 13.0 * f.p) * fade);
    c.fill(pb, fade);
}

fn edge_scan(c: &mut Canvas, f: &EdgeFrame) {
    let o = f.from_anchor(0.0, 72.0);
    let radius = 46.0 * (1.0 - f.p * 0.25);
    let mut alpha = 1.0 - smooth(0.96, 1.0, f.p);
    // The closer to the end, the faster and harder it flickers.
    let unrest = smooth(0.55, 1.0, f.p);
    if unrest > 0.0 && (f.t * (18.0 + 60.0 * unrest)) % 1.0 < 0.1 + 0.4 * unrest {
        alpha *= 0.25;
    }
    if alpha <= 0.0 {
        return;
    }

    c.glow(8.0, 0.8);
    c.set_line_width(2.0);
    let mut pb = PathBuilder::new();
    for i in 0..36 {
        let angle = f64::from(i) * PI / 18.0 + f.t * 0.8;
        let inner = radius - if i % 3 == 0 { 9.0 } else { 4.0 };
        line_path(
            &mut pb,
            v(o.x + angle.cos() * inner, o.y + angle.sin() * inner),
            v(o.x + angle.cos() * radius, o.y + angle.sin() * radius),
        );
    }
    c.stroke(pb, alpha * 0.9);

    let sweep = f.t * (4.0 + 18.0 * f.p * f.p);
    c.set_line_width(2.4);
    for k in 0..12 {
        let angle = sweep - f64::from(k) * 0.09;
        let mut pb = PathBuilder::new();
        line_path(&mut pb, o, v(o.x + angle.cos() * (radius - 3.0), o.y + angle.sin() * (radius - 3.0)));
        c.stroke(pb, alpha * (1.0 - f64::from(k) / 12.0).powi(2));
    }

    // Torn horizontal slices near the end.
    if unrest > 0.25 {
        let frame = (f.t * 30.0).floor() as i32;
        c.no_glow();
        for i in 0..5 {
            let y = o.y + (noise(frame + i * 11) - 0.5) * radius * 2.0;
            let x = o.x - radius + noise(frame + i * 13) * radius * 0.6 + (noise(frame + i * 17) - 0.5) * 22.0 * unrest;
            let width = radius * (0.6 + noise(frame + i * 19));
            let height = 2.0 + 3.0 * noise(frame + i * 23);
            if let Some(rect) = Rect::from_xywh(x as f32, y as f32, width as f32, height as f32) {
                let mut pb = PathBuilder::new();
                pb.push_rect(rect);
                c.fill(pb, alpha * 0.8);
            }
        }
    }
}

fn edge_liquid(c: &mut Canvas, f: &EdgeFrame) {
    const RADIUS: f64 = 58.0;
    let o = f.anchor;
    let mid = f.inward_angle();
    let level = RADIUS * (1.0 - f.p);

    c.set_line_width(2.5);
    let mut pb = PathBuilder::new();
    arc_path(&mut pb, o, RADIUS, mid - FRAC_PI_2, mid + FRAC_PI_2, false, false);
    pb.close();
    c.stroke(pb, 0.45 * (1.0 - smooth(0.9, 1.0, f.p)));

    if level <= 0.3 {
        return;
    }
    // The fluid stays inside the semicircle.
    let mut clip = PathBuilder::new();
    clip.move_to(o.x as f32, o.y as f32);
    arc_path(&mut clip, o, RADIUS - 2.0, mid - FRAC_PI_2, mid + FRAC_PI_2, false, true);
    clip.close();
    let Some(clip) = clip.finish() else { return };
    let Some(mut mask) = Mask::new(c.pixmap.width(), c.pixmap.height()) else { return };
    mask.fill_path(&clip, FillRule::Winding, true, c.to_pixels);

    let mut pb = PathBuilder::new();
    let start = f.from_anchor(-RADIUS, 0.0);
    pb.move_to(start.x as f32, start.y as f32);
    let mut a = -RADIUS;
    while a <= RADIUS {
        let wave = (a * 0.13 + f.t * 7.0).sin() * 3.0 * (level / 8.0).min(1.0);
        let q = f.from_anchor(a, level + wave);
        pb.line_to(q.x as f32, q.y as f32);
        a += 3.0;
    }
    let end = f.from_anchor(RADIUS, 0.0);
    pb.line_to(end.x as f32, end.y as f32);
    pb.close();
    let Some(path) = pb.finish() else { return };
    c.pixmap.fill_path(&path, &yellow_paint(0.9), FillRule::Winding, c.to_pixels, Some(&mask));
}

/// In the order of EdgeAnimStyle in config.rs; its first variant, None, draws nothing.
const EDGE_RENDERERS: [Option<EdgeRenderer>; 9] = [
    None,
    Some(edge_ring),
    Some(edge_semicircle),
    Some(edge_bar),
    Some(edge_pulse),
    Some(edge_laser),
    Some(edge_vortex),
    Some(edge_scan),
    Some(edge_liquid),
];

// ---- Locate-the-cursor effects ---------------------------------------------------

struct LocateFrame {
    size: V,
    center: V,
    /// Seconds since the effect began.
    t: f64,
    /// 1 until the fade-out, then down to 0.
    fade: f64,
}

type LocateRenderer = fn(&mut Canvas, &LocateFrame);

fn locate_ring(c: &mut Canvas, f: &LocateFrame) {
    c.glow(14.0, f.fade);
    c.set_line_width(5.0);
    for i in 0..3 {
        let phase = clamp01((f.t - f64::from(i) * 0.16) / 0.85);
        if phase <= 0.0 {
            continue;
        }
        let eased = 1.0 - (1.0 - phase).powi(3);
        let mut pb = PathBuilder::new();
        circle_path(&mut pb, f.center, 170.0 * (1.0 - eased) + 26.0);
        c.stroke(pb, (1.0 - phase * 0.75) * f.fade);
    }
}

fn locate_radar(c: &mut Canvas, f: &LocateFrame) {
    const REACH: f64 = 110.0;
    c.glow(8.0, 0.8 * f.fade);
    c.set_line_width(1.6);
    for i in 1..=3 {
        let mut pb = PathBuilder::new();
        circle_path(&mut pb, f.center, REACH * f64::from(i) / 3.0);
        c.stroke(pb, 0.45 * f.fade);
    }
    let sweep = FRAC_PI_2 - f.t * 7.0;
    c.set_line_width(2.6);
    for k in 0..26 {
        let angle = sweep + f64::from(k) * 0.05;
        let mut pb = PathBuilder::new();
        line_path(&mut pb, f.center, v(f.center.x + angle.cos() * REACH, f.center.y + angle.sin() * REACH));
        c.stroke(pb, (1.0 - f64::from(k) / 26.0).powi(2) * f.fade);
    }
    let ping = (f.t * 1.6) % 1.0;
    c.set_line_width(3.5);
    let mut pb = PathBuilder::new();
    circle_path(&mut pb, f.center, ping * REACH);
    c.stroke(pb, (1.0 - ping) * f.fade);
}

fn locate_spotlight(c: &mut Canvas, f: &LocateFrame) {
    let closing = 1.0 - (1.0 - clamp01(f.t / 0.4)).powi(3);
    let radius = 600.0 - 490.0 * closing;
    let dim = 0.5 * clamp01(f.t / 0.2) * f.fade;
    if dim > 0.0 {
        let mut pb = PathBuilder::new();
        if let Some(rect) = Rect::from_xywh(0.0, 0.0, f.size.x as f32, f.size.y as f32) {
            pb.push_rect(rect);
        }
        circle_path(&mut pb, f.center, radius);
        if let Some(path) = pb.finish() {
            let mut paint = Paint::default();
            paint.anti_alias = true;
            paint.set_color(Color::from_rgba(0.0, 0.0, 0.0, clamp01(dim) as f32).unwrap_or(Color::TRANSPARENT));
            c.pixmap.fill_path(&path, &paint, FillRule::EvenOdd, c.to_pixels, None);
        }
    }
    c.glow(18.0, f.fade);
    c.set_line_width(4.0);
    let mut pb = PathBuilder::new();
    circle_path(&mut pb, f.center, radius);
    c.stroke(pb, f.fade);
}

/// In the order of LocateStyle in config.rs.
const LOCATE_RENDERERS: [LocateRenderer; 3] = [locate_ring, locate_radar, locate_spotlight];
/// LocateStyle::Spotlight: the one effect that darkens the whole display.
pub const LOCATE_SPOTLIGHT: i32 = 2;

// ---- Entry points ----------------------------------------------------------------

/// Whether `style` (EdgeAnimStyle) draws anything.
pub fn edge_has_effect(style: i32) -> bool {
    usize::try_from(style).ok().and_then(|s| EDGE_RENDERERS.get(s)).is_some_and(Option::is_some)
}

/// Whether `style` (LocateStyle) is one of the locate effects.
pub fn locate_exists(style: i32) -> bool {
    usize::try_from(style).is_ok_and(|s| s < LOCATE_RENDERERS.len())
}

fn canvas(pixmap: &mut Pixmap, scale: f64) -> Canvas<'_> {
    let height = pixmap.height() as f32;
    let scale = scale as f32;
    Canvas { pixmap, to_pixels: Transform::from_row(scale, 0.0, 0.0, -scale, 0.0, height), line_width: 1.0, round_cap: false, glow: None }
}

/// One frame of an edge countdown.
pub struct EdgeDraw {
    /// EDGE_LEFT, EDGE_RIGHT, EDGE_TOP or EDGE_BOTTOM.
    pub edge: i32,
    /// EdgeAnimStyle, by number.
    pub style: i32,
    /// EdgeAnimDirection, by number.
    pub direction: i32,
    pub shrink: bool,
    /// The cursor, in the buffer's pixels (y down).
    pub cursor: (f64, f64),
    /// Buffer pixels per point.
    pub scale: f64,
    /// Seconds since the wait began.
    pub t: f64,
    /// Seconds the wait takes.
    pub duration: f64,
}

/// Draws one frame of an edge countdown on a cleared `pixmap`, the strip along the edge.
pub fn draw_edge(pixmap: &mut Pixmap, d: &EdgeDraw) {
    let Some(render) = usize::try_from(d.style).ok().and_then(|s| EDGE_RENDERERS.get(s).copied()).flatten() else { return };
    debug_assert!((EDGE_LEFT..=EDGE_BOTTOM).contains(&d.edge));
    let size = v(f64::from(pixmap.width()) / d.scale, f64::from(pixmap.height()) / d.scale);
    let local = v(d.cursor.0 / d.scale, size.y - d.cursor.1 / d.scale);
    let (anchor, inward, along) = match d.edge {
        EDGE_LEFT => (v(0.0, local.y), v(1.0, 0.0), v(0.0, 1.0)),
        EDGE_RIGHT => (v(size.x, local.y), v(-1.0, 0.0), v(0.0, 1.0)),
        EDGE_TOP => (v(local.x, size.y), v(0.0, -1.0), v(1.0, 0.0)),
        _ => (v(local.x, 0.0), v(0.0, 1.0), v(1.0, 0.0)),
    };
    let frame = EdgeFrame {
        size,
        anchor,
        inward,
        along,
        p: clamp01(d.t / d.duration),
        t: d.t,
        direction: d.direction,
        shrink: d.shrink,
    };
    let mut c = canvas(pixmap, d.scale);
    render(&mut c, &frame);
}

/// Draws one frame of the cursor highlight on a cleared `pixmap`. `center` is in the
/// buffer's pixels (y down), `t` the seconds since the effect began.
pub fn draw_locate(pixmap: &mut Pixmap, style: i32, center: (f64, f64), scale: f64, t: f64) {
    let Some(render) = usize::try_from(style).ok().and_then(|s| LOCATE_RENDERERS.get(s).copied()) else { return };
    let size = v(f64::from(pixmap.width()) / scale, f64::from(pixmap.height()) / scale);
    let frame = LocateFrame {
        size,
        center: v(center.0 / scale, size.y - center.1 / scale),
        t,
        fade: clamp01((LOCATE_SECONDS - t) / LOCATE_FADE),
    };
    let mut c = canvas(pixmap, scale);
    render(&mut c, &frame);
}

#[cfg(test)]
mod tests {
    use super::*;

    const STYLES: i32 = EDGE_RENDERERS.len() as i32;

    fn opaque_pixels(pixmap: &Pixmap) -> usize {
        pixmap.pixels().iter().filter(|p| p.alpha() > 0).count()
    }

    fn drawn(edge: i32, style: i32, direction: i32, shrink: bool, progress: f64) -> Pixmap {
        let mut pixmap = Pixmap::new(1920, 240).unwrap();
        let cursor = if edge == EDGE_BOTTOM { (900.0, 230.0) } else { (900.0, 10.0) };
        let d = EdgeDraw { edge, style, direction, shrink, cursor, scale: 1.0, t: progress * 2.0, duration: 2.0 };
        draw_edge(&mut pixmap, &d);
        pixmap
    }

    #[test]
    fn the_tables_follow_the_enum_numbers() {
        assert!(!edge_has_effect(0), "None draws nothing");
        for style in 1..STYLES {
            assert!(edge_has_effect(style));
        }
        assert!(!edge_has_effect(STYLES) && !edge_has_effect(-1));
        assert!(locate_exists(LOCATE_SPOTLIGHT) && !locate_exists(3) && !locate_exists(-1));
    }

    #[test]
    fn every_edge_effect_paints_midway() {
        for style in 1..STYLES {
            for edge in [EDGE_LEFT, EDGE_RIGHT, EDGE_TOP, EDGE_BOTTOM] {
                for direction in [DIR_CLOCKWISE, DIR_COUNTERCLOCKWISE, DIR_BOTH] {
                    for shrink in [false, true] {
                        let pixmap = drawn(edge, style, direction, shrink, 0.5);
                        assert!(opaque_pixels(&pixmap) > 50, "style {style} edge {edge} direction {direction} shrink {shrink}");
                    }
                }
            }
        }
    }

    #[test]
    fn none_never_paints() {
        assert_eq!(opaque_pixels(&drawn(EDGE_RIGHT, 0, DIR_CLOCKWISE, false, 0.5)), 0);
    }

    #[test]
    fn effects_are_gone_when_the_wait_ends() {
        // Like overlay_macos.m: every effect has gone at progress 1 except the bar,
        // whose dim track stays until the window is taken away.
        for style in 1..STYLES {
            if style == 3 {
                continue;
            }
            for edge in [EDGE_LEFT, EDGE_RIGHT, EDGE_TOP, EDGE_BOTTOM] {
                for shrink in [false, true] {
                    let pixmap = drawn(edge, style, DIR_CLOCKWISE, shrink, 1.0);
                    assert_eq!(opaque_pixels(&pixmap), 0, "style {style} edge {edge}");
                }
            }
        }
        let bar = drawn(EDGE_RIGHT, 3, DIR_CLOCKWISE, false, 1.0);
        assert!(bar.pixels().iter().all(|p| p.alpha() < 64), "only the dim track of the bar is left");
    }

    #[test]
    fn the_ring_drains_the_chosen_way() {
        // With the ring's top at 12 o'clock, clockwise leaves the left half at the half way
        // point and counter-clockwise the right half; both directions leave the bottom.
        fn lit(pixmap: &Pixmap, left: bool) -> usize {
            let center = 900;
            pixmap
                .pixels()
                .chunks(1920)
                .flat_map(|row| row.iter().enumerate())
                .filter(|(x, p)| p.alpha() > 200 && ((*x < center) == left))
                .count()
        }
        let clockwise = drawn(EDGE_BOTTOM, 1, DIR_CLOCKWISE, false, 0.5);
        assert!(lit(&clockwise, true) > 2 * lit(&clockwise, false), "clockwise leaves the left half");
        let counter = drawn(EDGE_BOTTOM, 1, DIR_COUNTERCLOCKWISE, false, 0.5);
        assert!(lit(&counter, false) > 2 * lit(&counter, true), "counter-clockwise leaves the right half");
    }

    #[test]
    fn the_scale_keeps_the_size_in_points() {
        let mut one = Pixmap::new(1920, 240).unwrap();
        let mut two = Pixmap::new(3840, 480).unwrap();
        let d = EdgeDraw { edge: EDGE_TOP, style: 1, direction: DIR_BOTH, shrink: false, cursor: (900.0, 0.0), scale: 1.0, t: 0.0, duration: 2.0 };
        draw_edge(&mut one, &d);
        draw_edge(&mut two, &EdgeDraw { cursor: (1800.0, 0.0), scale: 2.0, ..d });
        let ratio = opaque_pixels(&two) as f64 / opaque_pixels(&one) as f64;
        assert!((3.2..4.8).contains(&ratio), "{ratio}");
    }

    #[test]
    fn the_locate_effects_paint_then_fade_away() {
        for style in 0..3 {
            let mut midway = Pixmap::new(420, 420).unwrap();
            draw_locate(&mut midway, style, (210.0, 210.0), 1.0, 0.5);
            assert!(opaque_pixels(&midway) > 200, "style {style}");
            let mut end = Pixmap::new(420, 420).unwrap();
            draw_locate(&mut end, style, (210.0, 210.0), 1.0, LOCATE_SECONDS);
            assert_eq!(opaque_pixels(&end), 0, "style {style}");
        }
    }

    #[test]
    fn the_spotlight_dims_around_the_cursor_only() {
        let mut pixmap = Pixmap::new(1200, 800).unwrap();
        draw_locate(&mut pixmap, LOCATE_SPOTLIGHT, (600.0, 400.0), 1.0, 0.6);
        let at = |x: usize, y: usize| pixmap.pixels()[y * 1200 + x].alpha();
        assert!(at(5, 5) > 100, "the far corner is darkened");
        assert_eq!(at(600, 400), 0, "the cursor is left in the light");
    }

    #[test]
    fn angles_of_a_clockwise_arc_go_down() {
        // Three quarters of a circle: start on the right, down through the bottom.
        let mut pb = PathBuilder::new();
        arc_path(&mut pb, v(0.0, 0.0), 10.0, 0.0, FRAC_PI_2, true, false);
        let bounds = pb.finish().unwrap().bounds();
        assert!(bounds.left() <= -9.9 && bounds.top() <= -9.9, "{bounds:?}");
    }
}
