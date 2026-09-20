#![cfg(feature = "sdl2-audio")]

use crate::audio::config::{PlaybackConfig, StreamParams};
use crate::audio::error::{Error, Result};
use crate::audio::playback::AudioOutput;
use std::ffi::{c_char, c_void};
mod lifecycle;
mod backpressure;
mod submission;
use lifecycle::RuntimeUsers;
use std::time::Duration;

const SDL_INIT_AUDIO: u32 = 0x0000_0010;
const SDL_AUDIO_F32SYS: u16 = if cfg!(target_endian = "little") { 0x8120 } else { 0x9120 };
const SDL_AUDIO_STOPPED: i32 = 0;
static SDL_USERS: RuntimeUsers = RuntimeUsers::new();

#[repr(C)]
struct SdlAudioSpec {
    freq: i32,
    format: u16,
    channels: u8,
    silence: u8,
    samples: u16,
    padding: u16,
    size: u32,
    callback: Option<unsafe extern "C" fn(*mut c_void, *mut u8, i32)>,
    userdata: *mut c_void,
}

#[link(name = "SDL2")]
unsafe extern "C" {
    fn SDL_InitSubSystem(flags: u32) -> i32;
    fn SDL_QuitSubSystem(flags: u32);
    fn SDL_OpenAudioDevice(device: *const c_char, is_capture: i32, desired: *const SdlAudioSpec, obtained: *mut SdlAudioSpec, allowed_changes: i32) -> u32;
    fn SDL_CloseAudioDevice(device: u32);
    fn SDL_PauseAudioDevice(device: u32, pause_on: i32);
    fn SDL_GetAudioDeviceStatus(device: u32) -> i32;
    fn SDL_GetQueuedAudioSize(device: u32) -> u32;
    fn SDL_QueueAudio(device: u32, data: *const c_void, len: u32) -> i32;
    fn SDL_GetError() -> *const c_char;
    fn SDL_Delay(ms: u32);
}

fn sdl_error(context: &str) -> Error {
    let detail = unsafe { std::ffi::CStr::from_ptr(SDL_GetError()) }.to_string_lossy();
    Error::Backend(format!("SDL2 {context}: {detail}"))
}

fn requested_samples(stream: &StreamParams) -> Result<u16> {
    let samples = stream.frame_size().saturating_mul(3).max(480);
    u16::try_from(samples).map_err(|_| Error::InvalidConfig("SDL2 音频请求缓冲超过 u16"))
}

fn interleaved_frame_bytes(stream: &StreamParams) -> Result<usize> {
    stream.samples_per_frame().checked_mul(std::mem::size_of::<f32>())
        .ok_or(Error::InvalidConfig("SDL2 音频帧大小溢出"))
}

// 指定设备当前使用平台标识, 不能当作 SDL 显示名称或静默回退默认设备.
fn validate_request(config: &PlaybackConfig, stream: &StreamParams) -> Result<()> {
    if let Some(name) = &config.device_name {
        if name.is_empty() || name.contains('\0') {
            return Err(Error::InvalidConfig("音频设备标识为空或包含 NUL"));
        }
        return Err(Error::UnsupportedPlatform("SDL2 后端暂不支持平台设备标识, 请使用默认设备"));
    }
    if !matches!(stream.sample_rate, 8000 | 12000 | 16000 | 24000 | 48000)
        || !matches!(stream.channels, 2 | 6 | 8)
        || !matches!(stream.packet_duration_ms, 5 | 10 | 20 | 40 | 60)
    {
        return Err(Error::InvalidConfig("SDL2 播放参数不属于支持的 Opus PCM 格式"));
    }
    Ok(())
}

struct SdlRuntime;
impl SdlRuntime {
    fn acquire() -> Result<Self> {
        SDL_USERS.acquire(|| {
            if unsafe { SDL_InitSubSystem(SDL_INIT_AUDIO) } != 0 {
                return Err(sdl_error("音频子系统初始化失败"));
            }
            Ok(())
        })?;
        Ok(Self)
    }
}
impl Drop for SdlRuntime {
    fn drop(&mut self) {
        SDL_USERS.release(|| unsafe { SDL_QuitSubSystem(SDL_INIT_AUDIO) });
    }
}

pub(super) struct SdlOutput {
    _runtime: SdlRuntime,
    device: u32,
    frame_bytes: usize,
    frame_duration: Duration,
    buffer: Vec<f32>,
}

impl SdlOutput {
    pub(super) fn open(config: &PlaybackConfig, stream: &StreamParams) -> Result<Box<dyn AudioOutput>> {
        Self::open_device(config, stream).map(|output| Box::new(output) as Box<dyn AudioOutput>)
    }

    fn open_device(config: &PlaybackConfig, stream: &StreamParams) -> Result<Self> {
        validate_request(config, stream)?;
        let frame_bytes = interleaved_frame_bytes(stream)?;
        let samples = requested_samples(stream)?;
        let desired = SdlAudioSpec {
            freq: i32::try_from(stream.sample_rate).map_err(|_| Error::InvalidConfig("SDL2 采样率不可表示"))?,
            format: SDL_AUDIO_F32SYS,
            channels: stream.channels,
            silence: 0,
            samples,
            padding: 0,
            size: 0,
            callback: None,
            userdata: std::ptr::null_mut(),
        };
        // 参数和一帧内存准备完成后才接触 SDL, 分配失败不会遗留打开的设备.
        let buffer = vec![0.0; stream.samples_per_frame()];
        let runtime = SdlRuntime::acquire()?;
        let mut obtained = SdlAudioSpec { ..desired };
        let device = unsafe { SDL_OpenAudioDevice(std::ptr::null(), 0, &desired, &mut obtained, 0) };
        if device == 0 {
            return Err(sdl_error("播放设备打开失败"));
        }
        unsafe { SDL_PauseAudioDevice(device, 0) };
        tracing::info!(sample_rate = stream.sample_rate, channels = stream.channels, requested_samples = samples, obtained_samples = obtained.samples, "SDL2 音频 renderer 已启动");
        Ok(Self { _runtime: runtime, device, frame_bytes, frame_duration: Duration::from_millis(u64::from(stream.packet_duration_ms)), buffer })
    }
}

impl AudioOutput for SdlOutput {
    fn submit_frame(&mut self, frame: &[f32], _timeout: Duration) -> Result<()> {
        // SDL renderer 使用上游固定轮询次数, 不采用原生后端的调用方时间预算.
        debug_assert_eq!(self.frame_bytes, std::mem::size_of_val(self.buffer.as_slice()));
        submission::submit(
            frame,
            &mut self.buffer,
            self.frame_duration.as_millis() as u32,
            || {
                if unsafe { SDL_GetAudioDeviceStatus(self.device) } == SDL_AUDIO_STOPPED {
                    return Err(sdl_error("音频设备已停止"));
                }
                Ok(unsafe { SDL_GetQueuedAudioSize(self.device) as usize })
            },
            |duration| unsafe { SDL_Delay(duration.as_millis() as u32) },
            |pcm, byte_len| {
                if unsafe { SDL_QueueAudio(self.device, pcm.as_ptr().cast(), byte_len) } < 0 {
                    return Err(sdl_error("音频帧入队失败"));
                }
                Ok(())
            },
        )
    }
}

impl Drop for SdlOutput {
    fn drop(&mut self) {
        unsafe { SDL_PauseAudioDevice(self.device, 1); SDL_CloseAudioDevice(self.device); }
    }
}

#[cfg(test)]
mod dummy_tests;
#[cfg(test)]
mod validation_tests;

#[cfg(test)]
mod tests {
    use super::{interleaved_frame_bytes, requested_samples};
    use crate::audio::config::{AudioLayout, CodecConfig};

    #[test]
    fn requested_sdl_buffer_uses_frame_duration_not_interleaved_count() {
        for layout in [AudioLayout::Stereo, AudioLayout::Surround51, AudioLayout::Surround71] {
            let stream = CodecConfig { layout, ..CodecConfig::default() }.stream_params().unwrap();
            assert_eq!(requested_samples(&stream).unwrap(), 720);
            assert_eq!(interleaved_frame_bytes(&stream).unwrap(), stream.samples_per_frame() * 4);
        }
        let stream = CodecConfig { packet_duration_ms: 20, ..CodecConfig::default() }.stream_params().unwrap();
        assert_eq!(requested_samples(&stream).unwrap(), 2_880);
    }
}
