// Moonlight sdlaud.cpp:96-133 的本地提交控制流.
// 网络积压丢弃由 runtime/render 在调用这里前完成.
use super::backpressure;
use crate::audio::error::{Error, Result};
use std::time::Duration;

pub(super) fn submit(
    frame: &[f32],
    buffer: &mut [f32],
    frame_ms: u32,
    queued_bytes: impl FnMut() -> Result<usize>,
    delay: impl FnMut(Duration),
    queue: impl FnOnce(&[f32], u32) -> Result<()>,
) -> Result<()> {
    if frame.is_empty() { return Ok(()); }
    if frame.len() != buffer.len() {
        return Err(Error::Backend("SDL2 音频提交必须为完整协商帧".into()));
    }
    let bytes = std::mem::size_of_val(buffer);
    let byte_len = u32::try_from(bytes)
        .map_err(|_| Error::InvalidConfig("SDL2 音频帧字节数超过 u32"))?;
    backpressure::wait_for_space(bytes, frame_ms, queued_bytes, delay)?;
    buffer.copy_from_slice(frame);
    if let Err(error) = queue(buffer, byte_len) {
        // 与上游一致: 入队失败仅丢弃本帧, 不让上层因此重建 renderer.
        tracing::error!(%error, "SDL2 音频帧已丢弃");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn queue_failure_drops_only_current_frame_and_next_frame_can_succeed() {
        let mut buffer = [0.0; 480];
        for fail in [true, false] {
            let frame = [if fail { 0.25 } else { -0.5 }; 480];
            let called = Cell::new(false);
            submit(&frame, &mut buffer, 5, || Ok(0),
                |_| panic!("低水位不应等待"),
                |pcm, bytes| {
                    called.set(true);
                    assert_eq!(pcm, frame);
                    assert_eq!(bytes, 1920);
                    if fail { Err(Error::Backend("注入 SDL_QueueAudio 失败".into())) } else { Ok(()) }
                },
            ).unwrap();
            assert!(called.get());
            assert_eq!(buffer, frame);
        }
    }

    #[test]
    fn empty_and_incomplete_frames_never_query_or_queue() {
        let mut buffer = [0.5; 480];
        for frame in [&[][..], &[0.1][..]] {
            let result = submit(frame, &mut buffer, 5,
                || panic!("不应查询设备"), |_| panic!("不应等待"),
                |_, _| panic!("不应入队"),
            );
            assert_eq!(result.is_ok(), frame.is_empty());
            assert_eq!(buffer, [0.5; 480]);
        }
    }

    #[test]
    fn stopped_device_prevents_queue_but_exhausted_wait_does_not() {
        let mut buffer = [0.0; 480];
        let frame = [0.25; 480];
        let result = submit(&frame, &mut buffer, 5,
            || Err(Error::Backend("注入 SDL_AUDIO_STOPPED".into())),
            |_| panic!("已停止设备不应等待"), |_, _| panic!("已停止设备不应入队"),
        );
        assert!(matches!(result, Err(Error::Backend(_))));
        assert_eq!(buffer, [0.0; 480]);
        let delays = Cell::new(0);
        let calls = Cell::new(0);
        submit(&frame, &mut buffer, 5, || Ok(1920 * 11),
            |_| delays.set(delays.get() + 1),
            |pcm, _| {
                assert_eq!(delays.get(), 100);
                assert_eq!(pcm, frame);
                calls.set(calls.get() + 1);
                Ok(())
            },
        ).unwrap();
        assert_eq!(calls.get(), 1);
    }
}
