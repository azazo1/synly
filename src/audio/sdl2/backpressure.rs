// 对照 Moonlight sdlaud.cpp:112-125: 100 次轮询耗尽后仍允许入队.
use crate::audio::error::{Error, Result};
use std::time::Duration;

pub(super) fn wait_for_space(
    frame_bytes: usize,
    frame_ms: u32,
    mut queued_bytes: impl FnMut() -> Result<usize>,
    mut delay: impl FnMut(Duration),
) -> Result<()> {
    if frame_bytes == 0 || frame_ms == 0 {
        return Err(Error::InvalidConfig("SDL2 帧大小和时长必须为正数"));
    }
    for _ in 0..100 {
        // 闭包先检查 STOPPED, 再读取队列; 第 100 次等待后不追加检查.
        let queued = queued_bytes()?;
        if queued / frame_bytes <= (50 / frame_ms) as usize {
            break;
        }
        delay(Duration::from_millis(1));
    }
    Ok(())
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
                    let delays = Cell::new(0);
                    wait_for_space(bytes, ms,
                        || Ok(if delays.get() >= 3 { 0 } else { initial }),
                        |duration| { assert_eq!(duration, Duration::from_millis(1)); delays.set(delays.get() + 1); },
                    ).unwrap();
                    assert_eq!(delays.get(), if initial < boundary { 0 } else { 3 });
                }
            }
        }
    }

    #[test]
    fn exhausted_poll_budget_still_permits_queueing() {
        let polls = Cell::new(0);
        let delays = Cell::new(0);
        wait_for_space(1920, 5,
            || { polls.set(polls.get() + 1); Ok(1920 * 11) },
            |duration| { assert_eq!(duration, Duration::from_millis(1)); delays.set(delays.get() + 1); },
        ).unwrap();
        assert_eq!(polls.get(), 100);
        assert_eq!(delays.get(), 100);
    }

    #[test]
    fn stopped_device_fails_only_when_observed_during_polling() {
        for stop_at in [0, 2, 99, 100] {
            let delays = Cell::new(0);
            let result = wait_for_space(1920, 5,
                || if delays.get() >= stop_at {
                    Err(Error::Backend("模拟设备停止".into()))
                } else { Ok(1920 * 11) },
                |_| delays.set(delays.get() + 1),
            );
            assert_eq!(result.is_ok(), stop_at == 100);
            assert_eq!(delays.get(), stop_at);
        }
    }
}
