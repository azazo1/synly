use super::mapping::InputPlatform;
use super::platform::ScrollSource;

const TRACKPAD_PIXELS_PER_NOTCH: f64 = 48.0;
const CURVE_MEDIUM_THRESHOLD: f64 = 80.0;
const CURVE_FAST_THRESHOLD: f64 = 240.0;
const CURVE_MEDIUM_FACTOR: f64 = 1.25;
const CURVE_FAST_FACTOR: f64 = 1.5;

/// 把源平台滚动换算成目标平台原生滚动, 并保留触控板连续滚动的小数余数.
pub struct ScrollTransformer {
    native_macos_to_windows: bool,
    native_windows_to_macos: bool,
    trackpad_x_remainder: f64,
    trackpad_y_remainder: f64,
}

impl ScrollTransformer {
    pub fn new(native_macos_to_windows: bool, native_windows_to_macos: bool) -> Self {
        Self {
            native_macos_to_windows,
            native_windows_to_macos,
            trackpad_x_remainder: 0.0,
            trackpad_y_remainder: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.trackpad_x_remainder = 0.0;
        self.trackpad_y_remainder = 0.0;
    }

    #[allow(clippy::too_many_arguments)]
    pub fn transform(
        &mut self,
        x: i32,
        y: i32,
        source: ScrollSource,
        local_platform: InputPlatform,
        remote_platform: InputPlatform,
        reverse_mouse_wheel: bool,
        reverse_trackpad: bool,
    ) -> (i32, i32) {
        let (x, y) = self.native_transform(x, y, source, local_platform, remote_platform);
        let x = if local_platform == remote_platform {
            x
        } else {
            x.saturating_neg()
        };
        let reverse = match source {
            ScrollSource::MouseWheel => reverse_mouse_wheel,
            ScrollSource::Trackpad => reverse_trackpad,
        };
        if reverse {
            (x.saturating_neg(), y.saturating_neg())
        } else {
            (x, y)
        }
    }

    fn native_transform(
        &mut self,
        x: i32,
        y: i32,
        source: ScrollSource,
        local_platform: InputPlatform,
        remote_platform: InputPlatform,
    ) -> (i32, i32) {
        match (local_platform, remote_platform, source) {
            (InputPlatform::Macos, InputPlatform::Windows, ScrollSource::MouseWheel)
                if self.native_macos_to_windows =>
            {
                (x, y)
            }
            (InputPlatform::Macos, InputPlatform::Windows, ScrollSource::Trackpad)
                if self.native_macos_to_windows =>
            {
                (
                    Self::trackpad_axis_to_notch(x, &mut self.trackpad_x_remainder),
                    Self::trackpad_axis_to_notch(y, &mut self.trackpad_y_remainder),
                )
            }
            (InputPlatform::Windows, InputPlatform::Macos, _)
                if self.native_windows_to_macos =>
            {
                (
                    windows_notch_to_macos_line(x),
                    windows_notch_to_macos_line(y),
                )
            }
            _ => (x, y),
        }
    }

    fn trackpad_axis_to_notch(delta: i32, remainder: &mut f64) -> i32 {
        *remainder += accelerated_scroll(delta as f64) / TRACKPAD_PIXELS_PER_NOTCH;
        let notches = remainder.trunc();
        *remainder -= notches;
        rounded_i32(notches)
    }
}

fn accelerated_scroll(value: f64) -> f64 {
    let magnitude = value.abs();
    let factor = if magnitude < CURVE_MEDIUM_THRESHOLD {
        1.0
    } else if magnitude < CURVE_FAST_THRESHOLD {
        CURVE_MEDIUM_FACTOR
    } else {
        CURVE_FAST_FACTOR
    };
    value * factor
}

fn windows_notch_to_macos_line(delta: i32) -> i32 {
    let pixels = delta as f64 * TRACKPAD_PIXELS_PER_NOTCH;
    rounded_i32(accelerated_scroll(pixels) / TRACKPAD_PIXELS_PER_NOTCH)
}

fn rounded_i32(value: f64) -> i32 {
    if value <= i32::MIN as f64 {
        i32::MIN
    } else if value >= i32::MAX as f64 {
        i32::MAX
    } else {
        value.round() as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transformer(native_macos_to_windows: bool, native_windows_to_macos: bool) -> ScrollTransformer {
        ScrollTransformer::new(native_macos_to_windows, native_windows_to_macos)
    }

    #[test]
    fn disabled_native_scroll_keeps_old_behavior() {
        let mut transformer = transformer(false, false);
        assert_eq!(
            transformer.transform(
                4,
                -7,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Windows,
                false,
                false,
            ),
            (-4, -7)
        );
        assert_eq!(
            transformer.transform(
                4,
                -7,
                ScrollSource::MouseWheel,
                InputPlatform::Windows,
                InputPlatform::Macos,
                true,
                false,
            ),
            (4, 7)
        );
    }

    #[test]
    fn trackpad_pixels_accumulate_into_windows_notches() {
        let mut transformer = transformer(true, false);
        assert_eq!(
            transformer.transform(
                48,
                48,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Windows,
                false,
                false,
            ),
            (-1, 1)
        );
        assert_eq!(
            transformer.transform(
                0,
                24,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Windows,
                false,
                false,
            ),
            (0, 0)
        );
        assert_eq!(
            transformer.transform(
                0,
                24,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Windows,
                false,
                false,
            ),
            (0, 1)
        );
    }

    #[test]
    fn windows_notches_apply_fixed_curve_to_macos_lines() {
        let mut transformer = transformer(false, true);
        assert_eq!(
            transformer.transform(
                1,
                1,
                ScrollSource::MouseWheel,
                InputPlatform::Windows,
                InputPlatform::Macos,
                false,
                false,
            ),
            (-1, 1)
        );
        assert_eq!(
            transformer.transform(
                2,
                2,
                ScrollSource::MouseWheel,
                InputPlatform::Windows,
                InputPlatform::Macos,
                false,
                false,
            ),
            (-3, 3)
        );
    }

    #[test]
    fn native_conversion_runs_before_reverse() {
        let mut transformer = transformer(true, false);
        assert_eq!(
            transformer.transform(
                48,
                48,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Windows,
                false,
                true,
            ),
            (1, -1)
        );
    }

    #[test]
    fn reset_discards_pending_trackpad_remainder() {
        let mut transformer = transformer(true, false);
        assert_eq!(
            transformer.transform(
                24,
                0,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Windows,
                false,
                false,
            ),
            (0, 0)
        );
        transformer.reset();
        assert_eq!(
            transformer.transform(
                24,
                0,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Windows,
                false,
                false,
            ),
            (0, 0)
        );
    }
}
