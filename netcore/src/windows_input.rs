//! Coordinate and wheel conversion shared with the Windows adapter.
pub fn absolute_coordinate(position: f64, origin: i32, size: i32) -> i32 {
    ((position - origin as f64) * 65535.0 / (size - 1).max(1) as f64)
        .round()
        .clamp(0.0, 65535.0) as i32
}

pub fn wheel_units(remainder: &mut f64, notches: f64) -> i32 {
    *remainder += notches * 120.0;
    let rounded = remainder.round();
    let units = if (*remainder - rounded).abs() < 1e-9 {
        rounded
    } else {
        remainder.trunc()
    } as i32;
    *remainder -= units as f64;
    units
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn virtual_desktop_with_negative_origin() {
        assert_eq!(absolute_coordinate(-1920.0, -1920, 3840), 0);
        assert_eq!(absolute_coordinate(1919.0, -1920, 3840), 65535);
        assert_eq!(absolute_coordinate(-0.5, -1920, 3840), 32768);
        assert_eq!(absolute_coordinate(-2000.0, -1920, 3840), 0);
        assert_eq!(absolute_coordinate(3000.0, -1920, 3840), 65535);
        assert_eq!(absolute_coordinate(0.0, 0, 1), 0);
    }
    #[test]
    fn fractional_wheel_and_direction_reversal() {
        let mut remainder = 0.0;
        assert_eq!(wheel_units(&mut remainder, 0.004), 0);
        assert_eq!(wheel_units(&mut remainder, 0.006), 1);
        assert!((remainder - 0.2).abs() < 1e-9);
        assert_eq!(wheel_units(&mut remainder, -0.01), -1);
        assert!(remainder.abs() < 1e-9);
        assert_eq!(wheel_units(&mut remainder, 1.0), 120);
        assert_eq!(wheel_units(&mut remainder, -1.0), -120);
    }
}
