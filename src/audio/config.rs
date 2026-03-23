use crate::audio::codec::OpusMultistreamConfig;
use crate::audio::error::{Error, Result};

pub const SAMPLE_RATE: u32 = 48_000;
pub const DEFAULT_PACKET_DURATION_MS: u32 = 5;
pub const DEFAULT_JITTER_BUFFER_MS: u32 = 30;
pub const DEFAULT_REDUNDANCY_WINDOW_PACKETS: usize = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioLayout {
    Stereo,
    #[expect(
        dead_code,
        reason = "surround layouts are supported by the codec layer but not yet exposed by the CLI"
    )]
    Surround51,
    #[expect(
        dead_code,
        reason = "surround layouts are supported by the codec layer but not yet exposed by the CLI"
    )]
    Surround71,
}

#[derive(Clone, Debug)]
pub struct StreamParams {
    pub sample_rate: u32,
    pub channels: u8,
    pub streams: u8,
    pub coupled_streams: u8,
    pub mapping: [u8; 8],
    pub bitrate: u32,
    pub packet_duration_ms: u32,
}

impl StreamParams {
    pub fn frame_size(&self) -> usize {
        (self.packet_duration_ms as usize * self.sample_rate as usize) / 1000
    }

    pub fn samples_per_frame(&self) -> usize {
        self.frame_size() * self.channels as usize
    }

    pub fn opus_config(&self) -> OpusMultistreamConfig {
        OpusMultistreamConfig {
            sample_rate: self.sample_rate as i32,
            channel_count: self.channels as i32,
            streams: self.streams as i32,
            coupled_streams: self.coupled_streams as i32,
            samples_per_frame: self.frame_size() as i32,
            mapping: self.mapping,
        }
    }
}

#[derive(Clone, Debug)]
pub struct CodecConfig {
    pub layout: AudioLayout,
    pub packet_duration_ms: u32,
    pub high_quality: bool,
}

impl Default for CodecConfig {
    fn default() -> Self {
        Self {
            layout: AudioLayout::Stereo,
            packet_duration_ms: DEFAULT_PACKET_DURATION_MS,
            high_quality: false,
        }
    }
}

impl CodecConfig {
    pub fn stream_params(&self) -> Result<StreamParams> {
        if self.packet_duration_ms == 0 {
            return Err(Error::InvalidConfig(
                "packet duration must be greater than zero",
            ));
        }

        let params = match (self.layout, self.high_quality) {
            (AudioLayout::Stereo, false) => StreamParams {
                sample_rate: SAMPLE_RATE,
                channels: 2,
                streams: 1,
                coupled_streams: 1,
                mapping: [0, 1, 0, 0, 0, 0, 0, 0],
                bitrate: 96_000,
                packet_duration_ms: self.packet_duration_ms,
            },
            (AudioLayout::Stereo, true) => StreamParams {
                sample_rate: SAMPLE_RATE,
                channels: 2,
                streams: 1,
                coupled_streams: 1,
                mapping: [0, 1, 0, 0, 0, 0, 0, 0],
                bitrate: 512_000,
                packet_duration_ms: self.packet_duration_ms,
            },
            (AudioLayout::Surround51, false) => StreamParams {
                sample_rate: SAMPLE_RATE,
                channels: 6,
                streams: 4,
                coupled_streams: 2,
                mapping: [0, 4, 1, 5, 2, 3, 0, 0],
                bitrate: 256_000,
                packet_duration_ms: self.packet_duration_ms,
            },
            (AudioLayout::Surround51, true) => StreamParams {
                sample_rate: SAMPLE_RATE,
                channels: 6,
                streams: 6,
                coupled_streams: 0,
                mapping: [0, 4, 1, 5, 2, 3, 0, 0],
                bitrate: 1_536_000,
                packet_duration_ms: self.packet_duration_ms,
            },
            (AudioLayout::Surround71, false) => StreamParams {
                sample_rate: SAMPLE_RATE,
                channels: 8,
                streams: 5,
                coupled_streams: 3,
                mapping: [0, 6, 1, 7, 2, 3, 4, 5],
                bitrate: 450_000,
                packet_duration_ms: self.packet_duration_ms,
            },
            (AudioLayout::Surround71, true) => StreamParams {
                sample_rate: SAMPLE_RATE,
                channels: 8,
                streams: 8,
                coupled_streams: 0,
                mapping: [0, 6, 1, 7, 2, 3, 4, 5],
                bitrate: 2_048_000,
                packet_duration_ms: self.packet_duration_ms,
            },
        };

        Ok(params)
    }
}

#[derive(Clone, Debug, Default)]
pub struct CaptureConfig {
    pub device_name: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct PlaybackConfig {
    pub device_name: Option<String>,
}
