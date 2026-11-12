//! Turns the scrolling a window reports into the Windows wheel units the
//! protocol carries, 120 to a notch.

const WINDOWS_WHEEL_DELTA: f64 = 120.0;
const PRECISE_SCROLL_POINTS_PER_NOTCH: f64 = 60.0;
const PRECISE_SCROLL_STEPS_PER_NOTCH: f64 = 8.0;

/// Converts high-frequency trackpad motion into small, high-resolution Windows
/// wheel steps. Accumulation prevents every tiny AppKit event from becoming a
/// scroll action, while subdivisions avoid the jump caused by full notches.
#[derive(Default)]
pub struct WheelNormalizer {
    horizontal_remainder: f64,
    vertical_remainder: f64,
}

impl WheelNormalizer {
    pub fn normalize(&mut self, horizontal: f64, vertical: f64, precise: bool) -> (i16, i16) {
        if precise {
            (
                precise_wheel_delta(horizontal, &mut self.horizontal_remainder),
                precise_wheel_delta(vertical, &mut self.vertical_remainder),
            )
        } else {
            self.horizontal_remainder = 0.0;
            self.vertical_remainder = 0.0;
            (coarse_wheel_delta(horizontal), coarse_wheel_delta(vertical))
        }
    }
}

fn precise_wheel_delta(delta: f64, remainder: &mut f64) -> i16 {
    if !delta.is_finite() || delta == 0.0 {
        return 0;
    }
    if *remainder != 0.0 && remainder.signum() != delta.signum() {
        *remainder = 0.0;
    }
    *remainder += delta;
    let points_per_step = PRECISE_SCROLL_POINTS_PER_NOTCH / PRECISE_SCROLL_STEPS_PER_NOTCH;
    let wheel_delta_per_step = WINDOWS_WHEEL_DELTA / PRECISE_SCROLL_STEPS_PER_NOTCH;
    let steps = (*remainder / points_per_step).trunc();
    *remainder -= steps * points_per_step;
    clamp_wheel_delta(steps * wheel_delta_per_step)
}

fn coarse_wheel_delta(delta: f64) -> i16 {
    clamp_wheel_delta(delta * WINDOWS_WHEEL_DELTA)
}

fn clamp_wheel_delta(delta: f64) -> i16 {
    delta
        .round()
        .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_delta_accumulates_precise_trackpad_movement_into_smooth_steps() {
        let mut normalizer = WheelNormalizer::default();

        assert_eq!(normalizer.normalize(0.0, 2.5, true), (0, 0));
        assert_eq!(normalizer.normalize(0.0, 2.5, true), (0, 0));
        assert_eq!(normalizer.normalize(0.0, 2.5, true), (0, 15));
        assert_eq!(normalizer.normalize(0.0, 52.5, true), (0, 105));
        assert_eq!(normalizer.normalize(-120.0, 0.0, true), (-240, 0));
    }

    #[test]
    fn wheel_delta_converts_coarse_mouse_steps_to_windows_notches() {
        let mut normalizer = WheelNormalizer::default();

        assert_eq!(normalizer.normalize(1.0, -2.0, false), (120, -240));
    }

    #[test]
    fn wheel_delta_discards_stale_remainder_when_direction_changes() {
        let mut normalizer = WheelNormalizer::default();

        assert_eq!(normalizer.normalize(0.0, 3.75, true), (0, 0));
        assert_eq!(normalizer.normalize(0.0, -3.75, true), (0, 0));
        assert_eq!(normalizer.normalize(0.0, -3.75, true), (0, -15));
    }
}
