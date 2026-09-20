use super::*;
use std::time::Instant;
use crate::audio::config::{AudioLayout, CodecConfig};

unsafe extern "C" {
    fn SDL_GetCurrentAudioDriver() -> *const c_char;
    fn SDL_WasInit(flags: u32) -> u32;
}

#[test]
#[ignore = "必须通过 audio-sdl2-dummy-test 单独运行, 使用 dummy 驱动"]
fn dummy_playback_exercises_real_sdl_queue_and_reinitialization() {
    assert_eq!(std::env::var("SDL_AUDIODRIVER").as_deref(), Ok("dummy"));
    assert_eq!(unsafe { SDL_WasInit(SDL_INIT_AUDIO) }, 0);
    // 先确认驱动, 再允许产品路径打开设备. 外层引用同时验证嵌套引用释放.
    for layout in [AudioLayout::Stereo, AudioLayout::Surround51, AudioLayout::Surround71] {
        for ms in [5, 10, 20, 40, 60] {
            eprintln!("SDL dummy 验证: {layout:?}, {ms} ms");
            let runtime = SdlRuntime::acquire().unwrap();
            let driver = unsafe { SDL_GetCurrentAudioDriver() };
            assert!(!driver.is_null());
            assert_eq!(unsafe { std::ffi::CStr::from_ptr(driver) }.to_bytes(), b"dummy");
            let stream = CodecConfig { layout, packet_duration_ms: ms, ..CodecConfig::default() }.stream_params().unwrap();
            let mut output = SdlOutput::open_device(&PlaybackConfig::default(), &stream).unwrap();
            let frame = vec![0.125; stream.samples_per_frame()];
            unsafe { SDL_PauseAudioDevice(output.device, 1) };
            let initial = unsafe { SDL_GetQueuedAudioSize(output.device) };
            assert!(output.submit_frame(&frame[..frame.len() - 1], Duration::ZERO).is_err());
            assert_eq!(unsafe { SDL_GetQueuedAudioSize(output.device) }, initial);
            // 暂停消费者以确定地构造整包水位, 不依赖线程调度.
            let packets = 50 / ms + 1;
            for _ in 0..packets {
                output.submit_frame(&frame, Duration::ZERO).unwrap();
            }
            let queued = unsafe { SDL_GetQueuedAudioSize(output.device) };
            assert_eq!(queued as usize, packets as usize * output.frame_bytes);
            output.submit_frame(&[], Duration::ZERO).unwrap();
            assert_eq!(unsafe { SDL_GetQueuedAudioSize(output.device) }, queued);
            // 对照上游: 暂停状态不算 STOPPED, 耗尽轮询仍追加一帧.
            output.submit_frame(&frame, Duration::ZERO).unwrap();
            assert_eq!(unsafe { SDL_GetQueuedAudioSize(output.device) } as usize,
                queued as usize + output.frame_bytes);
            unsafe { SDL_PauseAudioDevice(output.device, 0) };
            // 驱动实际消费队列后应恢复提交. 不断言精确调度时延.
            let deadline = Instant::now() + Duration::from_secs(3);
            while unsafe { SDL_GetQueuedAudioSize(output.device) } != 0 {
                assert!(Instant::now() < deadline, "dummy 队列未消费");
                std::thread::sleep(Duration::from_millis(2));
            }
            output.submit_frame(&frame, Duration::from_millis(100)).unwrap();
            drop(output);
            assert_ne!(unsafe { SDL_WasInit(SDL_INIT_AUDIO) }, 0);
            drop(runtime);
            assert_eq!(unsafe { SDL_WasInit(SDL_INIT_AUDIO) }, 0);
        }
    }
}
