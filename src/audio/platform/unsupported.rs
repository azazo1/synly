use crate::audio::capture::AudioInput;
use crate::audio::config::{CaptureConfig, PlaybackConfig, StreamParams};
use crate::audio::error::{Error, Result};
use crate::audio::playback::AudioOutput;

pub fn open_input(_config: &CaptureConfig, _stream: &StreamParams) -> Result<Box<dyn AudioInput>> {
    Err(Error::UnsupportedPlatform(
        "native audio input backend is not implemented for this platform yet",
    ))
}

pub fn open_output(
    _config: &PlaybackConfig,
    _stream: &StreamParams,
) -> Result<Box<dyn AudioOutput>> {
    Err(Error::UnsupportedPlatform(
        "native audio output backend is not implemented for this platform yet",
    ))
}

