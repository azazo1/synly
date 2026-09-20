use crate::audio::capture::{AudioInput, CaptureStatus};
use crate::audio::config::{CaptureConfig, PlaybackConfig, StreamParams};
use crate::audio::error::{Error, Result};
use crate::audio::playback::AudioOutput;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr::NonNull;
use std::time::Duration;

fn checked_frame_layout(stream: &StreamParams) -> Result<(u32, usize)> {
    if !matches!(stream.sample_rate, 8000 | 12000 | 16000 | 24000 | 48000)
        || !matches!(stream.channels, 2 | 6 | 8)
        || !matches!(stream.packet_duration_ms, 5 | 10 | 20 | 40 | 60)
    {
        return Err(Error::InvalidConfig("macOS 音频参数不属于支持的 Opus PCM 格式"));
    }
    let frames = u32::try_from(stream.frame_size())
        .map_err(|_| Error::InvalidConfig("macOS 音频帧大小超过 u32"))?;
    let samples = stream.samples_per_frame();
    u32::try_from(samples).map_err(|_| Error::InvalidConfig("macOS 音频样本数超过 u32"))?;
    Ok((frames, samples))
}

pub fn open_input(config: &CaptureConfig, stream: &StreamParams) -> Result<Box<dyn AudioInput>> {
    if config.device_name.is_some() {
        return Err(Error::UnsupportedPlatform("macOS 系统音频 tap 尚不支持指定设备"));
    }
    if stream.channels != 2 {
        return Err(Error::UnsupportedPlatform(
            "macOS system audio capture is currently limited to stereo",
        ));
    }

    let (frame_size, samples_per_frame) = checked_frame_layout(stream)?;
    if unsafe { ar_macos_capture_supported() } == 0 {
        return Err(Error::UnsupportedPlatform("macOS 系统音频捕获需要 14.2 或更新版本"));
    }
    let handle = unsafe {
        ar_macos_capture_create(
            std::ptr::null(),
            stream.sample_rate,
            2,
            frame_size,
        )
    };
    let handle = NonNull::new(handle).ok_or_else(last_backend_error)?;
    Ok(Box::new(MacosInput { handle, samples_per_frame }))
}

pub fn open_output(config: &PlaybackConfig, stream: &StreamParams) -> Result<Box<dyn AudioOutput>> {
    if config.device_name.is_some() {
        return Err(Error::UnsupportedPlatform("macOS 播放尚不支持指定设备"));
    }

    let (frame_size, samples_per_frame) = checked_frame_layout(stream)?;
    let handle = unsafe {
        ar_macos_playback_create(
            stream.sample_rate,
            stream.channels as u32,
            frame_size,
        )
    };
    let handle = NonNull::new(handle).ok_or_else(last_backend_error)?;
    Ok(Box::new(MacosOutput { handle, samples_per_frame }))
}

struct MacosInput {
    handle: NonNull<c_void>,
    samples_per_frame: usize,
}

unsafe impl Send for MacosInput {}

impl Drop for MacosInput {
    fn drop(&mut self) {
        let mut dropped = 0;
        let mut high_water = 0;
        unsafe { ar_macos_capture_stats(self.handle.as_ptr(), &mut dropped, &mut high_water) };
        tracing::debug!(dropped_samples = dropped, high_water_samples = high_water, "关闭 macOS 捕获缓冲");
        let status = unsafe { ar_macos_capture_destroy(self.handle.as_ptr()) };
        if status != 0 {
            tracing::error!(status, "Core Audio 捕获清理失败, 已保留回调资源并禁用重新创建, 需要重启应用");
        }
    }
}

impl AudioInput for MacosInput {
    fn read_frame(&mut self, frame: &mut [f32], timeout: Duration) -> Result<CaptureStatus> {
        if frame.len() != self.samples_per_frame {
            return Err(Error::InvalidConfig("macOS 捕获缓冲必须为完整协商帧"));
        }
        let result = unsafe {
            ar_macos_capture_read(
                self.handle.as_ptr(),
                frame.as_mut_ptr(),
                frame.len() as u32,
                timeout.as_millis().min(u128::from(u32::MAX)) as u32,
            )
        };
        match result {
            0 => Ok(CaptureStatus::Ok),
            1 => Ok(CaptureStatus::Timeout),
            _ => Err(last_backend_error()),
        }
    }
}

struct MacosOutput {
    handle: NonNull<c_void>,
    samples_per_frame: usize,
}

unsafe impl Send for MacosOutput {}

impl Drop for MacosOutput {
    fn drop(&mut self) {
        let mut dropped = 0;
        let mut high_water = 0;
        unsafe { ar_macos_playback_stats(self.handle.as_ptr(), &mut dropped, &mut high_water) };
        let resumed_gaps = unsafe { ar_macos_playback_resumed_gaps(self.handle.as_ptr()) };
        if resumed_gaps != 0 {
            tracing::warn!(resumed_gaps, "macOS 播放曾欠载补零后恢复, 可能产生点噪");
        }
        tracing::debug!(dropped_samples = dropped, high_water_samples = high_water, resumed_gaps, "关闭 macOS 播放缓冲");
        let status = unsafe { ar_macos_playback_destroy(self.handle.as_ptr()) };
        if status != 0 {
            tracing::error!(status, "AudioQueue 销毁失败, 已保留回调资源并禁用重新创建, 需要重启应用");
        }
    }
}

impl AudioOutput for MacosOutput {
    fn submit_frame(&mut self, frame: &[f32], timeout: Duration) -> Result<()> {
        if frame.len() != self.samples_per_frame {
            return Err(Error::InvalidConfig("macOS 播放缓冲必须为完整协商帧"));
        }
        let result = unsafe {
            ar_macos_playback_submit(
                self.handle.as_ptr(),
                frame.as_ptr(),
                frame.len() as u32,
                timeout.as_millis().min(u128::from(u32::MAX)) as u32,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(last_backend_error())
        }
    }
}

fn last_backend_error() -> Error {
    let mut message = [0 as c_char; 512];
    unsafe {
        ar_macos_copy_error(message.as_mut_ptr(), message.len() as u32);
        let message = CStr::from_ptr(message.as_ptr()).to_string_lossy().into_owned();
        if ar_macos_audio_cleanup_failure() != 0 {
            Error::BackendFatal(message)
        } else {
            Error::Backend(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::CodecConfig;

    #[test]
    fn malformed_stream_fails_before_native_device_access() {
        let original = CodecConfig::default().stream_params().unwrap();
        for duration in [0, 1, u32::MAX] {
            let mut stream = original.clone();
            stream.packet_duration_ms = duration;
            assert!(matches!(open_input(&CaptureConfig::default(), &stream), Err(Error::InvalidConfig(_))));
            assert!(matches!(open_output(&PlaybackConfig::default(), &stream), Err(Error::InvalidConfig(_))));
        }
        for rate in [0, 44100, u32::MAX] {
            let mut stream = original.clone();
            stream.sample_rate = rate;
            assert!(matches!(open_input(&CaptureConfig::default(), &stream), Err(Error::InvalidConfig(_))));
            assert!(matches!(open_output(&PlaybackConfig::default(), &stream), Err(Error::InvalidConfig(_))));
        }
    }

    #[test]
    fn partial_frames_never_reach_native_handles() {
        // 测试只执行提前返回路径, 禁止调用/销毁此哨兵句柄.
        let mut input = std::mem::ManuallyDrop::new(MacosInput {
            handle: NonNull::dangling(), samples_per_frame: 480,
        });
        let mut output = std::mem::ManuallyDrop::new(MacosOutput {
            handle: NonNull::dangling(), samples_per_frame: 480,
        });
        for length in [0, 1, 479, 481] {
            let mut frame = vec![0.0; length];
            assert!(matches!(input.read_frame(&mut frame, Duration::ZERO), Err(Error::InvalidConfig(_))));
            assert!(matches!(output.submit_frame(&frame, Duration::ZERO), Err(Error::InvalidConfig(_))));
        }
    }
}

unsafe extern "C" {
    fn ar_macos_audio_cleanup_failure() -> c_int;
    fn ar_macos_capture_supported() -> c_int;
    fn ar_macos_copy_error(out: *mut c_char, capacity: u32);
    fn ar_macos_capture_stats(handle: *mut c_void, dropped: *mut u64, high_water: *mut u32);
    fn ar_macos_playback_resumed_gaps(handle: *mut c_void) -> u32;
    fn ar_macos_playback_stats(handle: *mut c_void, dropped: *mut u64, high_water: *mut u32);

    fn ar_macos_capture_create(
        device_name: *const c_char,
        sample_rate: u32,
        channels: u32,
        frame_size: u32,
    ) -> *mut c_void;

    fn ar_macos_capture_destroy(handle: *mut c_void) -> c_int;

    fn ar_macos_capture_read(
        handle: *mut c_void,
        out_samples: *mut f32,
        sample_count: u32,
        timeout_ms: u32,
    ) -> c_int;

    fn ar_macos_playback_create(sample_rate: u32, channels: u32, frame_size: u32) -> *mut c_void;

    fn ar_macos_playback_destroy(handle: *mut c_void) -> c_int;

    fn ar_macos_playback_submit(
        handle: *mut c_void,
        samples: *const f32,
        sample_count: u32,
        timeout_ms: u32,
    ) -> c_int;
}
