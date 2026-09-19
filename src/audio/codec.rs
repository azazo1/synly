use crate::audio::error::{Error, Result};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uchar};
use std::ptr::NonNull;

const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_VBR_REQUEST: c_int = 4006;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod upstream_tests;

#[derive(Clone, Copy, Debug)]
pub struct OpusMultistreamConfig {
    pub sample_rate: c_int,
    pub channel_count: c_int,
    pub streams: c_int,
    pub coupled_streams: c_int,
    pub samples_per_frame: c_int,
    pub mapping: [u8; 8],
}

impl OpusMultistreamConfig {
    fn validate(&self) -> Result<()> {
        if !matches!(self.sample_rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000) {
            return Err(Error::InvalidConfig("Opus 采样率不受支持"));
        }
        if !(1..=self.mapping.len() as c_int).contains(&self.channel_count) {
            return Err(Error::InvalidConfig("Opus 声道数必须在 1 到 8 之间"));
        }
        if !(1..=self.channel_count).contains(&self.streams)
            || !(0..=self.streams).contains(&self.coupled_streams)
            || self.streams + self.coupled_streams > self.channel_count
        {
            return Err(Error::InvalidConfig("Opus streams 与 coupled_streams 超出声道范围"));
        }
        let coded_channels = self.streams + self.coupled_streams;
        if self.mapping[..self.channel_count as usize]
            .iter()
            .any(|&channel| channel != 255 && c_int::from(channel) >= coded_channels)
        {
            return Err(Error::InvalidConfig("Opus mapping 引用了不存在的编码声道"));
        }
        // 以 2.5 ms 为单位比较, 避免整数毫秒截断短帧.
        if ![1, 2, 4, 8, 16, 24].iter().any(|&units| {
            self.samples_per_frame == self.sample_rate / 400 * units
        }) {
            return Err(Error::InvalidConfig("Opus 帧时长必须为 2.5, 5, 10, 20, 40 或 60 ms"));
        }
        Ok(())
    }

    fn pcm_len(&self) -> usize {
        self.samples_per_frame as usize * self.channel_count as usize
    }
}

pub struct OpusEncoder {
    inner: NonNull<OpusMSEncoder>,
    config: OpusMultistreamConfig,
}

pub struct OpusDecoder {
    inner: NonNull<OpusMSDecoder>,
    config: OpusMultistreamConfig,
}

unsafe impl Send for OpusEncoder {}
unsafe impl Send for OpusDecoder {}

impl OpusEncoder {
    pub fn new(config: OpusMultistreamConfig, bitrate: u32) -> Result<Self> {
        config.validate()?;
        // 编码器必须能为每个编码声道找到 PCM 输入, 解码器则允许静音或重复映射.
        let mapping = &config.mapping[..config.channel_count as usize];
        if (0..config.streams + config.coupled_streams)
            .any(|channel| !mapping.contains(&(channel as u8)))
        {
            return Err(Error::InvalidConfig("Opus 编码 mapping 未覆盖全部编码声道"));
        }
        let bitrate = c_int::try_from(bitrate)
            .ok()
            .filter(|&value| value > 0)
            .ok_or(Error::InvalidConfig("Opus 码率必须为正数且不超过 i32::MAX"))?;
        let mut err = 0;
        let inner = unsafe {
            opus_multistream_encoder_create(
                config.sample_rate,
                config.channel_count,
                config.streams,
                config.coupled_streams,
                config.mapping.as_ptr(),
                OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut err,
            )
        };

        if err != 0 {
            return Err(Error::Codec(opus_error(err)));
        }

        let inner = NonNull::new(inner)
            .ok_or_else(|| Error::Codec("Opus 编码器创建返回空指针".into()))?;
        // ctl 失败也必须通过 Drop 释放已经创建的编码器.
        let encoder = Self { inner, config };

        let bitrate_res = unsafe {
            opus_multistream_encoder_ctl(encoder.inner.as_ptr(), OPUS_SET_BITRATE_REQUEST, bitrate)
        };
        if bitrate_res != 0 {
            return Err(Error::Codec(opus_error(bitrate_res)));
        }

        let vbr_res = unsafe {
            opus_multistream_encoder_ctl(encoder.inner.as_ptr(), OPUS_SET_VBR_REQUEST, 0 as c_int)
        };
        if vbr_res != 0 {
            return Err(Error::Codec(opus_error(vbr_res)));
        }

        Ok(encoder)
    }

    pub fn encode_float(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize> {
        if pcm.len() != self.config.pcm_len() {
            return Err(Error::Codec("PCM 长度与 Opus 编码帧不一致".into()));
        }
        let out_len = opus_buffer_len(out.len())?;
        let bytes = unsafe {
            opus_multistream_encode_float(
                self.inner.as_ptr(),
                pcm.as_ptr(),
                self.config.samples_per_frame,
                out.as_mut_ptr(),
                out_len,
            )
        };
        if bytes < 0 {
            return Err(Error::Codec(opus_error(bytes)));
        }
        Ok(bytes as usize)
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { opus_multistream_encoder_destroy(self.inner.as_ptr()) };
    }
}

impl OpusDecoder {
    pub fn new(config: OpusMultistreamConfig) -> Result<Self> {
        config.validate()?;
        let mut err = 0;
        let inner = unsafe {
            opus_multistream_decoder_create(
                config.sample_rate,
                config.channel_count,
                config.streams,
                config.coupled_streams,
                config.mapping.as_ptr(),
                &mut err,
            )
        };
        if err != 0 {
            return Err(Error::Codec(opus_error(err)));
        }
        let inner = NonNull::new(inner)
            .ok_or_else(|| Error::Codec("Opus 解码器创建返回空指针".into()))?;
        Ok(Self { inner, config })
    }

    pub fn decode_float(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<usize> {
        let expected = self.config.pcm_len();
        if out.len() < expected {
            return Err(Error::Codec("Opus 解码输出缓冲区太小".into()));
        }
        let (packet_ptr, packet_len) = match packet {
            Some(packet) => {
                let len = opus_buffer_len(packet.len())?;
                // 多流包首个流的 TOC 即可检查帧长, 避免错误帧推进解码状态.
                let samples = unsafe {
                    opus_packet_get_nb_samples(packet.as_ptr(), len, self.config.sample_rate)
                };
                if samples < 0 {
                    return Err(Error::Codec(opus_error(samples)));
                }
                if samples != self.config.samples_per_frame {
                    return Err(Error::Codec("Opus 数据包帧长与协商值不一致".into()));
                }
                (packet.as_ptr(), len)
            }
            None => (std::ptr::null(), 0),
        };
        let decoded = unsafe {
            opus_multistream_decode_float(
                self.inner.as_ptr(),
                packet_ptr,
                packet_len,
                out.as_mut_ptr(),
                self.config.samples_per_frame,
                0,
            )
        };
        if decoded < 0 {
            return Err(Error::Codec(opus_error(decoded)));
        }
        if decoded != self.config.samples_per_frame {
            return Err(Error::Codec("Opus 解码帧长与协商值不一致".into()));
        }
        Ok(expected)
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { opus_multistream_decoder_destroy(self.inner.as_ptr()) };
    }
}

fn opus_buffer_len(len: usize) -> Result<c_int> {
    c_int::try_from(len)
        .ok()
        .filter(|&value| value > 0)
        .ok_or_else(|| Error::Codec("Opus 缓冲区长度必须为正数且不超过 i32::MAX".into()))
}

fn opus_error(code: c_int) -> String {
    let detail = unsafe {
        CStr::from_ptr(opus_strerror(code))
            .to_string_lossy()
            .into_owned()
    };
    format!("Opus 操作失败: {detail} (错误码 {code})")
}

#[repr(C)]
struct OpusMSEncoder {
    _private: [u8; 0],
}

#[repr(C)]
struct OpusMSDecoder {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn opus_strerror(error: c_int) -> *const c_char;

    fn opus_packet_get_nb_samples(data: *const u8, len: c_int, fs: c_int) -> c_int;

    fn opus_multistream_encoder_create(
        fs: c_int,
        channels: c_int,
        streams: c_int,
        coupled_streams: c_int,
        mapping: *const c_uchar,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusMSEncoder;

    fn opus_multistream_encoder_destroy(st: *mut OpusMSEncoder);

    fn opus_multistream_encoder_ctl(st: *mut OpusMSEncoder, request: c_int, ...) -> c_int;

    fn opus_multistream_encode_float(
        st: *mut OpusMSEncoder,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut u8,
        max_data_bytes: c_int,
    ) -> c_int;

    fn opus_multistream_decoder_create(
        fs: c_int,
        channels: c_int,
        streams: c_int,
        coupled_streams: c_int,
        mapping: *const c_uchar,
        error: *mut c_int,
    ) -> *mut OpusMSDecoder;

    fn opus_multistream_decoder_destroy(st: *mut OpusMSDecoder);

    fn opus_multistream_decode_float(
        st: *mut OpusMSDecoder,
        data: *const u8,
        len: c_int,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
}
