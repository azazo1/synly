use crate::audio::capture::AudioInput;
use crate::audio::config::{CaptureConfig, PlaybackConfig, StreamParams};
use crate::audio::error::Result;
use crate::audio::playback::AudioOutput;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

pub fn open_input(config: &CaptureConfig, stream: &StreamParams) -> Result<Box<dyn AudioInput>> {
    #[cfg(target_os = "macos")]
    {
        macos::open_input(config, stream)
    }
    #[cfg(target_os = "windows")]
    {
        windows::open_input(config, stream)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        unsupported::open_input(config, stream)
    }
}

pub fn open_output(config: &PlaybackConfig, stream: &StreamParams) -> Result<Box<dyn AudioOutput>> {
    #[cfg(target_os = "macos")]
    {
        macos::open_output(config, stream)
    }
    #[cfg(target_os = "windows")]
    {
        windows::open_output(config, stream)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        unsupported::open_output(config, stream)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{config::CodecConfig, error::Error};

    #[cfg(target_os = "windows")]
    #[test]
    fn malformed_endpoint_selection_fails_before_device_startup() {
        let stream = CodecConfig::default().stream_params().unwrap();
        for id in ["", "设备\0后缀"] {
            let capture = CaptureConfig { device_name: Some(id.into()) };
            let playback = PlaybackConfig { device_name: Some(id.into()) };
            assert!(matches!(open_input(&capture, &stream), Err(Error::InvalidConfig(_))));
            assert!(matches!(open_output(&playback, &stream), Err(Error::InvalidConfig(_))));
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unsupported_device_selection_is_not_a_retryable_device_outage() {
        let stream = CodecConfig::default().stream_params().unwrap();
        let capture = CaptureConfig { device_name: Some("测试指定设备".into()) };
        let playback = PlaybackConfig { device_name: Some("测试指定设备".into()) };
        assert!(matches!(open_input(&capture, &stream), Err(Error::UnsupportedPlatform(_))));
        assert!(matches!(open_output(&playback, &stream), Err(Error::UnsupportedPlatform(_))));
    }
}
