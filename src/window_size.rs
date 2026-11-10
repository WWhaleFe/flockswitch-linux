//! Keeps a window at its size in points when the display scale under it changes.
//!
//! On X11 the scale is Xft.dpi when the desktop sets it (one value for every monitor,
//! which can still change while the window is open), and otherwise worked out from
//! each monitor's size in millimetres, so it can differ from monitor to monitor; on
//! Wayland every output has its own. When the scale changes the window is resized in
//! pixels, and the window manager may round, refuse or apply the change twice, so the
//! window can come out much larger or smaller than it was. The settings windows
//! remember their size in points while the scale stays put, and ask for it again when
//! the scale changes or the size ends up outside the window's limits.

use std::time::{Duration, Instant};

/// After the scale changes, resizes keep being put right for this long (the window
/// manager may apply its own resize a little later).
const SETTLE: Duration = Duration::from_millis(1500);
/// Sizes this close (in points) are the same.
const SLACK: f32 = 1.0;

#[derive(Debug, Default)]
pub struct SizeGuard {
    /// The scale (native pixels per point) last seen.
    scale: Option<f32>,
    /// The size to keep, in points.
    size: Option<[f32; 2]>,
    settle_until: Option<Instant>,
    /// The size seen when the kept size was last asked for, so it is asked once per change.
    asked_at: Option<[f32; 2]>,
}

impl SizeGuard {
    /// Takes the window's scale and inner size (points) as of this frame, and its
    /// limits. Returns the size to ask for, if the window should be resized.
    pub fn next(&mut self, scale: f32, size: [f32; 2], min: [f32; 2], max: [f32; 2], now: Instant) -> Option<[f32; 2]> {
        let fits = (0..2).all(|i| size[i] >= min[i] - SLACK && size[i] <= max[i] + SLACK);
        if self.scale.is_some_and(|s| (s - scale).abs() > 0.001) {
            self.settle_until = Some(now + SETTLE);
            self.asked_at = None;
        }
        self.scale = Some(scale);
        let settling = self.settling(now);
        let Some(want) = self.size else {
            let start = [0, 1].map(|i| size[i].clamp(min[i], max[i]));
            self.size = Some(start);
            return (!fits).then_some(start);
        };
        // The user resized it: keep that size from now on.
        if fits && !settling {
            self.size = Some(size);
            self.asked_at = None;
            return None;
        }
        let close = (0..2).all(|i| (size[i] - want[i]).abs() <= SLACK);
        if close || self.asked_at == Some(size) {
            return None;
        }
        self.asked_at = Some(size);
        Some(want)
    }

    /// True for a moment after the scale changed (repaint meanwhile to keep watching).
    pub fn settling(&self, now: Instant) -> bool {
        self.settle_until.is_some_and(|t| now < t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: [f32; 2] = [760.0, 640.0];
    const MAX: [f32; 2] = [1200.0, 1100.0];

    #[test]
    fn keeps_the_size_across_a_scale_change() {
        let mut g = SizeGuard::default();
        let t = Instant::now();
        assert_eq!(g.next(2.0, [840.0, 900.0], MIN, MAX, t), None);
        // The user makes it wider: that is the size from now on.
        assert_eq!(g.next(2.0, [900.0, 900.0], MIN, MAX, t + Duration::from_secs(1)), None);
        // Dragged onto a monitor at 1×: the window comes out twice as large in points.
        let moved = t + Duration::from_secs(2);
        assert_eq!(g.next(1.0, [1200.0, 1100.0], MIN, MAX, moved), Some([900.0, 900.0]));
        // Asked once per size seen, not every frame.
        assert_eq!(g.next(1.0, [1200.0, 1100.0], MIN, MAX, moved + Duration::from_millis(16)), None);
        // The window manager applies its own resize a little later: put right again.
        assert_eq!(g.next(1.0, [1000.0, 1000.0], MIN, MAX, moved + Duration::from_millis(300)), Some([900.0, 900.0]));
        assert_eq!(g.next(1.0, [900.0, 900.0], MIN, MAX, moved + Duration::from_millis(400)), None);
        assert!(g.settling(moved + Duration::from_millis(400)));
        // Afterwards resizing by hand counts again.
        let later = moved + Duration::from_secs(3);
        assert!(!g.settling(later));
        assert_eq!(g.next(1.0, [800.0, 700.0], MIN, MAX, later), None);
        assert_eq!(g.next(1.5, [800.0, 760.0], MIN, MAX, later + Duration::from_secs(1)), Some([800.0, 700.0]));
    }

    #[test]
    fn puts_back_a_size_outside_the_limits() {
        let mut g = SizeGuard::default();
        let t = Instant::now();
        assert_eq!(g.next(1.0, [840.0, 900.0], MIN, MAX, t), None);
        assert_eq!(g.next(1.0, [1500.0, 1300.0], MIN, MAX, t + Duration::from_secs(1)), Some([840.0, 900.0]));
        assert_eq!(g.next(1.0, [1500.0, 1300.0], MIN, MAX, t + Duration::from_secs(2)), None, "asked already");
        // Starting outside them: clamped.
        let mut g = SizeGuard::default();
        assert_eq!(g.next(1.0, [2000.0, 500.0], MIN, MAX, t), Some([1200.0, 640.0]));
    }

    #[test]
    fn a_fixed_size_window() {
        let size = [620.0, 440.0];
        let mut g = SizeGuard::default();
        let t = Instant::now();
        assert_eq!(g.next(1.25, size, size, size, t), None);
        assert_eq!(g.next(1.0, [775.0, 550.0], size, size, t + Duration::from_secs(1)), Some(size));
        assert_eq!(g.next(1.0, [620.5, 440.0], size, size, t + Duration::from_secs(1)), None);
    }
}
