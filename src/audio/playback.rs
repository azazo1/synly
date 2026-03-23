use crate::audio::config::{PlaybackConfig, StreamParams};
use crate::audio::error::Result;
use crate::audio::platform;
use std::time::Duration;

pub trait AudioOutput: Send {
    fn submit_frame(&mut self, frame: &[f32], timeout: Duration) -> Result<()>;
}

pub fn open_output(config: &PlaybackConfig, stream: &StreamParams) -> Result<Box<dyn AudioOutput>> {
    platform::open_output(config, stream)
}
