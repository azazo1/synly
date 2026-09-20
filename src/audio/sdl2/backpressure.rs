// 对照 Moonlight sdlaud.cpp 的整包水位和 1 ms 轮询.
// Synly 超时后拒绝新帧, 不复制上游耗尽等待后继续入队的行为.
use crate::audio::error::{Error, Result};
use std::time::Duration;

pub(super) fn wait_for_space(
    frame_bytes: usize,
    frame_ms: u32,
    timeout: Duration,
    mut queued_bytes: impl FnMut() -> Result<usize>,
    mut elapsed: impl FnMut() -> Duration,
    mut delay: impl FnMut(Duration),
) -> Result<()> {
    if frame_bytes == 0 || frame_ms == 0 {
        return Err(Error::InvalidConfig("SDL2 帧大小和时长必须为正数"));
    }
    let budget = timeout.min(Duration::from_millis(100));
    let mut waited = false;
    loop {
        // 每轮先检查设备状态, 包括最后一次等待之后.
        let queued = queued_bytes()?;
        let spent = elapsed();
        let over_watermark = queued / frame_bytes > (50 / frame_ms) as usize;
        if !over_watermark && (!waited || spent < budget) {
            return Ok(());
        }
        if spent >= budget {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut, "SDL2 播放队列等待超时",
            )));
        }
        delay((budget - spent).min(Duration::from_millis(1)));
        waited = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn whole_packet_watermark_waits_then_accepts_consumption() {
        for ms in [5, 10, 20, 40, 60] {
            for channels in [2, 6, 8] {
                let bytes = 48 * ms as usize * channels * 4;
                let boundary = (50 / ms + 1) as usize * bytes;
                for initial in [boundary - channels * 4, boundary] {
                    let clock = Cell::new(Duration::ZERO);
                    let delays = Cell::new(0);
                    wait_for_space(bytes, ms, Duration::from_millis(100),
                        || Ok(if delays.get() >= 3 { 0 } else { initial }),
                        || clock.get(),
                        |duration| { clock.set(clock.get() + duration); delays.set(delays.get() + 1); },
                    ).unwrap();
                    assert_eq!(delays.get(), if initial < boundary { 0 } else { 3 });
                }
            }
        }
    }

    #[test]
    fn timeout_caps_wait_and_never_accepts_late_drain() {
        for timeout in [Duration::ZERO, Duration::from_micros(500), Duration::from_millis(8), Duration::from_secs(1)] {
            let clock = Cell::new(Duration::ZERO);
            let budget = timeout.min(Duration::from_millis(100));
            let result = wait_for_space(1920, 5, timeout,
                || Ok(if clock.get() >= budget && !budget.is_zero() { 0 } else { 1920 * 11 }),
                || clock.get(),
                |duration| { assert!(duration <= Duration::from_millis(1)); clock.set(clock.get() + duration); },
            );
            assert!(matches!(result, Err(Error::Io(ref error)) if error.kind() == std::io::ErrorKind::TimedOut));
            assert_eq!(clock.get(), budget);
        }
        wait_for_space(1920, 5, Duration::ZERO, || Ok(0), || Duration::ZERO,
            |_| panic!("空队列不得等待"),
        ).unwrap();
    }

    #[test]
    fn device_failure_is_checked_before_queue_and_after_last_wait() {
        for stop_ms in [0, 2, 100] {
            let clock = Cell::new(Duration::ZERO);
            let result = wait_for_space(1920, 5, Duration::from_secs(1),
                || if clock.get() >= Duration::from_millis(stop_ms) {
                    Err(Error::Backend("模拟设备停止".into()))
                } else { Ok(1920 * 11) },
                || clock.get(),
                |duration| clock.set(clock.get() + duration),
            );
            assert!(matches!(result, Err(Error::Backend(_))));
            assert_eq!(clock.get(), Duration::from_millis(stop_ms));
        }
    }
}
