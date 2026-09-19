//! Sunshine Windows audio.cpp:60-130,326-397 的浮点 PCM 格式边界.
use super::{Error, Guid, Result, WasapiSpec, WaveFormatEx};

pub(super) const EXTENSIBLE: u16 = 0xfffe;
const FLOAT: Guid = Guid::new(3, 0, 0x0010, [0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]);
const STEREO: u32 = 0x3;
const SURROUND51_BACK: u32 = 0x3f;
const SURROUND51_SIDE: u32 = 0x60f;
const SURROUND71: u32 = 0x63f;

// Windows SDK mmreg.h 的布局: WAVEFORMATEX 为 18 字节, 扩展为 22 字节.
// 不能使用自然对齐的 20 字节 Rust header 嵌入扩展, 否则 ValidBits 偏移错误.
#[repr(C, packed(1))]
pub(super) struct WaveFormatExtensible {
    pub(super) format: WaveFormatEx,
    pub(super) valid_bits: u16,
    pub(super) channel_mask: u32,
    pub(super) subformat: Guid,
}

impl WasapiSpec {
    pub(super) fn wave_format(&self) -> Result<WaveFormatExtensible> {
        let mask = match self.channels {
            2 => STEREO,
            6 => SURROUND51_BACK,
            8 => SURROUND71,
            _ => return Err(Error::UnsupportedPlatform("Windows 音频只支持 stereo, 5.1 和 7.1 布局")),
        };
        if self.sample_rate == 0 {
            return Err(Error::InvalidConfig("Windows 音频采样率不能为零"));
        }
        let block_align = self.channels * 4;
        let byte_rate = self.sample_rate.checked_mul(u32::from(block_align))
            .ok_or(Error::InvalidConfig("Windows 音频字节率溢出"))?;
        Ok(WaveFormatExtensible {
            format: WaveFormatEx {
                w_format_tag: EXTENSIBLE, n_channels: self.channels,
                n_samples_per_sec: self.sample_rate, n_avg_bytes_per_sec: byte_rate,
                n_block_align: block_align, w_bits_per_sample: 32, cb_size: 22,
            },
            valid_bits: 32, channel_mask: mask, subformat: FLOAT,
        })
    }
}

impl WaveFormatExtensible {
    // Windows PCM 和 Synly PCM 都按 FL,FR,FC,LFE,BL,BR,SL,SR 排列.
    // 六声道 side/back 两种 5.1 顺序相同, 末两路解释为环绕对.
    // 不盲从其它同声道数的自定义 mask, 避免 height/wide 被误认为 rear/side.
    pub(super) fn prefer_native_mask(&mut self, channels: u16, mask: u32) -> bool {
        if channels != self.format.n_channels { return false; }
        let supported = match channels {
            2 => mask == STEREO,
            6 => mask == SURROUND51_BACK || mask == SURROUND51_SIDE,
            8 => mask == SURROUND71,
            _ => false,
        };
        if supported { self.channel_mask = mask; }
        supported
    }
}

#[cfg(test)]
mod tests;
