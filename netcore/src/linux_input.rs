//! Linux evdev codes, shared by X11 and EIS.
use crate::{InputEvent, MouseButton};

pub const KEYS: &[(u16, u16)] = &[
    (1, 0x29),
    (2, 0x1e),
    (3, 0x1f),
    (4, 0x20),
    (5, 0x21),
    (6, 0x22),
    (7, 0x23),
    (8, 0x24),
    (9, 0x25),
    (10, 0x26),
    (11, 0x27),
    (12, 0x2d),
    (13, 0x2e),
    (14, 0x2a),
    (15, 0x2b),
    (16, 0x14),
    (17, 0x1a),
    (18, 0x08),
    (19, 0x15),
    (20, 0x17),
    (21, 0x1c),
    (22, 0x18),
    (23, 0x0c),
    (24, 0x12),
    (25, 0x13),
    (26, 0x2f),
    (27, 0x30),
    (28, 0x28),
    (29, 0xe0),
    (30, 0x04),
    (31, 0x16),
    (32, 0x07),
    (33, 0x09),
    (34, 0x0a),
    (35, 0x0b),
    (36, 0x0d),
    (37, 0x0e),
    (38, 0x0f),
    (39, 0x33),
    (40, 0x34),
    (41, 0x35),
    (42, 0xe1),
    (43, 0x31),
    (44, 0x1d),
    (45, 0x1b),
    (46, 0x06),
    (47, 0x19),
    (48, 0x05),
    (49, 0x11),
    (50, 0x10),
    (51, 0x36),
    (52, 0x37),
    (53, 0x38),
    (54, 0xe5),
    (55, 0x55),
    (56, 0xe2),
    (57, 0x2c),
    (58, 0x39),
    (59, 0x3a),
    (60, 0x3b),
    (61, 0x3c),
    (62, 0x3d),
    (63, 0x3e),
    (64, 0x3f),
    (65, 0x40),
    (66, 0x41),
    (67, 0x42),
    (68, 0x43),
    (69, 0x53),
    (70, 0x47),
    (71, 0x5f),
    (72, 0x60),
    (73, 0x61),
    (74, 0x56),
    (75, 0x5c),
    (76, 0x5d),
    (77, 0x5e),
    (78, 0x57),
    (79, 0x59),
    (80, 0x5a),
    (81, 0x5b),
    (82, 0x62),
    (83, 0x63),
    (86, 0x64),
    (87, 0x44),
    (88, 0x45),
    (96, 0x58),
    (97, 0xe4),
    (98, 0x54),
    (99, 0x46),
    (100, 0xe6),
    (102, 0x4a),
    (103, 0x52),
    (104, 0x4b),
    (105, 0x50),
    (106, 0x4f),
    (107, 0x4d),
    (108, 0x51),
    (109, 0x4e),
    (110, 0x49),
    (111, 0x4c),
    (117, 0x67),
    (119, 0x48),
    (122, 0x90),
    (123, 0x91),
    (125, 0xe3),
    (126, 0xe7),
    (127, 0x65),
    (183, 0x68),
    (184, 0x69),
    (185, 0x6a),
    (186, 0x6b),
    (187, 0x6c),
    (188, 0x6d),
    (189, 0x6e),
    (190, 0x6f),
    (191, 0x70),
    (192, 0x71),
    (193, 0x72),
    (194, 0x73),
];
pub fn evdev_to_hid(code: u32) -> Option<u16> {
    KEYS.iter()
        .find(|(c, _)| u32::from(*c) == code)
        .map(|(_, h)| *h)
}
pub fn hid_to_evdev(usage: u16) -> Option<u32> {
    KEYS.iter()
        .find(|(_, h)| *h == usage)
        .map(|(c, _)| u32::from(*c))
}
pub fn button(code: u32) -> Option<MouseButton> {
    match code {
        1 | 272 => Some(MouseButton::Left),
        2 | 274 => Some(MouseButton::Middle),
        3 | 273 => Some(MouseButton::Right),
        8 | 275 => Some(MouseButton::Back),
        9 | 276 => Some(MouseButton::Forward),
        _ => None,
    }
}
pub fn xbutton(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
    }
}
pub fn evdev_button(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 272,
        MouseButton::Right => 273,
        MouseButton::Middle => 274,
        MouseButton::Back => 275,
        MouseButton::Forward => 276,
    }
}
pub fn wheel(button: u32, down: bool) -> Option<InputEvent> {
    if !down {
        return None;
    }
    let (dx, dy) = match button {
        4 => (0.0, 1.0),
        5 => (0.0, -1.0),
        6 => (1.0, 0.0),
        7 => (-1.0, 0.0),
        _ => return None,
    };
    Some(InputEvent::Scroll { dx, dy })
}
pub fn notches(remainder: &mut f64, delta: f64) -> i32 {
    if !delta.is_finite() {
        return 0;
    }
    *remainder += delta.clamp(-1000.0, 1000.0);
    let n = remainder.trunc() as i32;
    *remainder -= f64::from(n);
    n
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keys_round_trip() {
        for &(code, hid) in KEYS {
            assert_eq!(evdev_to_hid(code.into()), Some(hid));
            assert_eq!(hid_to_evdev(hid), Some(code.into()));
        }
        assert_eq!(evdev_to_hid(0), None);
        assert_eq!(hid_to_evdev(0), None);
        assert_eq!(hid_to_evdev(0x90), Some(122));
    }
    #[test]
    fn buttons_and_scroll() {
        for b in [
            MouseButton::Left,
            MouseButton::Right,
            MouseButton::Middle,
            MouseButton::Back,
            MouseButton::Forward,
        ] {
            assert_eq!(button(xbutton(b)), Some(b));
            assert_eq!(button(evdev_button(b)), Some(b));
        }
        assert_eq!(
            wheel(4, true),
            Some(InputEvent::Scroll { dx: 0.0, dy: 1.0 })
        );
        assert_eq!(
            wheel(7, true),
            Some(InputEvent::Scroll { dx: -1.0, dy: 0.0 })
        );
        assert_eq!(wheel(4, false), None);
        let mut r = 0.0;
        assert_eq!(notches(&mut r, 0.6), 0);
        assert_eq!(notches(&mut r, 0.6), 1);
        assert_eq!(notches(&mut r, -1.2), -1);
    }
}

/// EIS can describe one wheel movement in both units within a device frame.
#[derive(Default)]
pub struct ScrollFrame {
    continuous: (f64, f64),
    discrete: (i64, i64),
}
impl ScrollFrame {
    pub fn continuous(&mut self, dx: f64, dy: f64) {
        self.continuous.0 += dx;
        self.continuous.1 += dy;
    }
    pub fn discrete(&mut self, dx: i32, dy: i32) {
        self.discrete.0 += i64::from(dx);
        self.discrete.1 += i64::from(dy);
    }
    pub fn event(self) -> InputEvent {
        let axis = |continuous: f64, discrete: i64| {
            if discrete != 0 {
                -(discrete as f64) / 120.0
            } else {
                -continuous / 10.0
            }
        };
        InputEvent::Scroll {
            dx: axis(self.continuous.0, self.discrete.0),
            dy: axis(self.continuous.1, self.discrete.1),
        }
    }
}
#[cfg(test)]
mod scroll_tests {
    use super::*;
    #[test]
    fn eis_dual_units_are_not_counted_twice() {
        let mut frame = ScrollFrame::default();
        frame.continuous(5.0, -10.0);
        frame.discrete(0, -120);
        assert_eq!(frame.event(), InputEvent::Scroll { dx: -0.5, dy: 1.0 });
        let mut frame = ScrollFrame::default();
        frame.discrete(60, 0);
        frame.discrete(60, 0);
        assert_eq!(frame.event(), InputEvent::Scroll { dx: -1.0, dy: -0.0 });
    }
}
