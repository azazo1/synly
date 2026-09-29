use super::mapping::InputPlatform;
use super::platform::ScrollSource;

const PIXELS_PER_NOTCH: f64 = 48.0;
const CURVE_MEDIUM_THRESHOLD: f64 = 80.0;
const CURVE_FAST_THRESHOLD: f64 = 240.0;
const CURVE_MEDIUM_FACTOR: f64 = 1.25;
const CURVE_FAST_FACTOR: f64 = 1.5;

/// 把源平台普通滚轮换算成目标平台原生滚动, 触控板保持直通.
pub struct ScrollTransformer {
    native_macos_to_windows: bool,
    native_windows_to_macos: bool,
}

impl ScrollTransformer {
    pub fn new(native_macos_to_windows: bool, native_windows_to_macos: bool) -> Self {
        Self {
            native_macos_to_windows,
            native_windows_to_macos,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn transform(
        &self,
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
        &self,
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
                (wheel_axis_to_notch(x), wheel_axis_to_notch(y))
            }
            (InputPlatform::Windows, InputPlatform::Macos, ScrollSource::MouseWheel)
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
}

/// 以 Chromium 在 Windows 上一格 (120) 约滚动 100 逻辑像素为基准 (6/5), 实测偏慢, 再乘 1.5 倍,
/// 即约 67 像素一格 (9/5). 用整数分子分母避免浮点累计误差.
const WHEEL_UNITS_NUMERATOR: i64 = 9;
const WHEEL_UNITS_DENOMINATOR: i64 = 5;

/// 把 macOS 触控板像素增量换算为 Windows 高精度滚轮单位, 保留余量避免慢速滚动丢失.
#[derive(Default)]
pub struct PreciseWheelConverter {
    remainder_x: i64,
    remainder_y: i64,
}

impl PreciseWheelConverter {
    pub fn convert(&mut self, pixel_x: i32, pixel_y: i32) -> (i32, i32) {
        (
            take_whole(&mut self.remainder_x, pixel_x),
            take_whole(&mut self.remainder_y, pixel_y),
        )
    }
}

/// remainder 以 1/WHEEL_UNITS_DENOMINATOR 单位为刻度.
fn take_whole(remainder: &mut i64, pixels: i32) -> i32 {
    let delta = i64::from(pixels) * WHEEL_UNITS_NUMERATOR;
    // 换向时丢弃旧方向余量, 避免反向第一下被抵消.
    if *remainder * delta < 0 {
        *remainder = 0;
    }
    let total = *remainder + delta;
    *remainder = total % WHEEL_UNITS_DENOMINATOR;
    (total / WHEEL_UNITS_DENOMINATOR).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn wheel_axis_to_notch(delta: i32) -> i32 {
    delta.signum()
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
    let pixels = delta as f64 * PIXELS_PER_NOTCH;
    rounded_i32(accelerated_scroll(pixels) / PIXELS_PER_NOTCH)
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

    fn transformer(
        native_macos_to_windows: bool,
        native_windows_to_macos: bool,
    ) -> ScrollTransformer {
        ScrollTransformer::new(native_macos_to_windows, native_windows_to_macos)
    }

    #[test]
    fn disabled_native_scroll_keeps_old_behavior() {
        let transformer = transformer(false, false);
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
    fn native_scroll_keeps_trackpad_unchanged() {
        let transformer = transformer(true, true);
        assert_eq!(
            transformer.transform(
                48,
                480,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Windows,
                false,
                false,
            ),
            (-48, 480)
        );
        assert_eq!(
            transformer.transform(
                3,
                2,
                ScrollSource::Trackpad,
                InputPlatform::Windows,
                InputPlatform::Macos,
                false,
                false,
            ),
            (-3, 2)
        );
    }

    #[test]
    fn mouse_wheel_to_windows_uses_event_count_not_accelerated_magnitude() {
        let transformer = transformer(true, false);
        assert_eq!(
            transformer.transform(
                0,
                9,
                ScrollSource::MouseWheel,
                InputPlatform::Macos,
                InputPlatform::Windows,
                false,
                false,
            ),
            (0, 1)
        );
        assert_eq!(
            transformer.transform(
                0,
                -9,
                ScrollSource::MouseWheel,
                InputPlatform::Macos,
                InputPlatform::Windows,
                false,
                false,
            ),
            (0, -1)
        );
    }

    #[test]
    fn windows_notches_apply_fixed_curve_to_macos_lines() {
        let transformer = transformer(false, true);
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
    fn precise_wheel_keeps_fraction_across_slow_events() {
        let mut converter = PreciseWheelConverter::default();
        // 每次 1 像素 = 1.8 单位, 5 次后累计 9 单位, 不应因取整丢失.
        let total: i32 = (0..5).map(|_| converter.convert(0, 1).1).sum();
        assert_eq!(total, 9);
        let total: i32 = (0..5).map(|_| converter.convert(0, -1).1).sum();
        assert_eq!(total, -9);
    }

    #[test]
    fn precise_wheel_drops_remainder_on_direction_change() {
        let mut converter = PreciseWheelConverter::default();
        assert_eq!(converter.convert(0, 1), (0, 1));
        // 残留 0.8, 反向时应清零, 反向第一下完整生效.
        assert_eq!(converter.convert(0, -1), (0, -1));
    }

    #[test]
    fn native_conversion_runs_before_reverse() {
        let transformer = transformer(true, false);
        assert_eq!(
            transformer.transform(
                48,
                48,
                ScrollSource::MouseWheel,
                InputPlatform::Macos,
                InputPlatform::Windows,
                true,
                false,
            ),
            (1, -1)
        );
    }
}
