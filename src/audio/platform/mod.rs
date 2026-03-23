use crate::audio::capture::AudioInput;
use crate::audio::config::{CaptureConfig, PlaybackConfig, StreamParams};
use crate::audio::error::Result;
use crate::audio::playback::AudioOutput;

#[cfg(not(target_os = "windows"))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

pub fn open_input(config: &CaptureConfig, stream: &StreamParams) -> Result<Box<dyn AudioInput>> {
    #[cfg(target_os = "windows")]
    {
        return windows::open_input(config, stream);
    }
    #[cfg(not(target_os = "windows"))]
    {
        return unsupported::open_input(config, stream);
    }
}

pub fn open_output(config: &PlaybackConfig, stream: &StreamParams) -> Result<Box<dyn AudioOutput>> {
    #[cfg(target_os = "windows")]
    {
        return windows::open_output(config, stream);
    }
    #[cfg(not(target_os = "windows"))]
    {
        return unsupported::open_output(config, stream);
    }
}
