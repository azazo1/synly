use crate::audio::codec::OpusMultistreamConfig;
use crate::audio::error::{Error, Result};
use serde::{Deserialize, Serialize};

pub const SAMPLE_RATE: u32 = 48_000;
pub const DEFAULT_PACKET_DURATION_MS: u32 = 5;
pub const DEFAULT_INITIAL_DROP_MS: u32 = 500;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioLayout {
    #[default]
    Stereo,
    Surround51,
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
        let samples = u64::from(self.packet_duration_ms) * u64::from(self.sample_rate) / 1000;
        usize::try_from(samples).unwrap_or(usize::MAX)
    }

    pub fn samples_per_frame(&self) -> usize {
        self.frame_size().saturating_mul(self.channels as usize)
    }

    pub fn opus_config(&self) -> OpusMultistreamConfig {
        OpusMultistreamConfig {
            // 无法表示的配置交由 codec 验证拒绝, 避免截断后意外变成合法帧.
            sample_rate: i32::try_from(self.sample_rate).unwrap_or(0),
            channel_count: i32::from(self.channels),
            streams: i32::from(self.streams),
            coupled_streams: i32::from(self.coupled_streams),
            samples_per_frame: i32::try_from(self.frame_size()).unwrap_or(0),
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
        if !matches!(self.packet_duration_ms, 5 | 10 | 20 | 40 | 60) {
            return Err(Error::InvalidConfig(
                "Opus 整数毫秒帧时长必须为 5, 10, 20, 40 或 60 ms",
            ));
        }

        // Sunshine audio.cpp:51-100 与 platform/common.h:279-320 的默认编码表.
        // PCM 顺序为 FL,FR,FC,LFE,BL,BR,SL,SR. 不套用 RTSP 描述兼容旋转,
        // 也不使用 Moonlight 为旧 GFE 无 surround-params 提供的硬编码 fallback.
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
                mapping: [0, 1, 2, 3, 4, 5, 0, 0],
                bitrate: 256_000,
                packet_duration_ms: self.packet_duration_ms,
            },
            (AudioLayout::Surround51, true) => StreamParams {
                sample_rate: SAMPLE_RATE,
                channels: 6,
                streams: 6,
                coupled_streams: 0,
                mapping: [0, 1, 2, 3, 4, 5, 0, 0],
                bitrate: 1_536_000,
                packet_duration_ms: self.packet_duration_ms,
            },
            (AudioLayout::Surround71, false) => StreamParams {
                sample_rate: SAMPLE_RATE,
                channels: 8,
                streams: 5,
                coupled_streams: 3,
                mapping: [0, 1, 2, 3, 4, 5, 6, 7],
                bitrate: 450_000,
                packet_duration_ms: self.packet_duration_ms,
            },
            (AudioLayout::Surround71, true) => StreamParams {
                sample_rate: SAMPLE_RATE,
                channels: 8,
                streams: 8,
                coupled_streams: 0,
                mapping: [0, 1, 2, 3, 4, 5, 6, 7],
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
