use crate::audio::error::{Error, Result};
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_uchar};
use std::ptr::NonNull;

const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_VBR_REQUEST: c_int = 4006;

#[derive(Clone, Copy, Debug)]
pub struct OpusMultistreamConfig {
    pub sample_rate: c_int,
    pub channel_count: c_int,
    pub streams: c_int,
    pub coupled_streams: c_int,
    pub samples_per_frame: c_int,
    pub mapping: [u8; 8],
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
            .ok_or_else(|| Error::Codec("opus encoder creation returned null".into()))?;

        let bitrate_res = unsafe {
            opus_multistream_encoder_ctl(inner.as_ptr(), OPUS_SET_BITRATE_REQUEST, bitrate as c_int)
        };
        if bitrate_res != 0 {
            return Err(Error::Codec(opus_error(bitrate_res)));
        }

        let vbr_res = unsafe {
            opus_multistream_encoder_ctl(inner.as_ptr(), OPUS_SET_VBR_REQUEST, 0 as c_int)
        };
        if vbr_res != 0 {
            return Err(Error::Codec(opus_error(vbr_res)));
        }

        Ok(Self { inner, config })
    }

    pub fn encode_float(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize> {
        if pcm.len() != self.config.samples_per_frame as usize * self.config.channel_count as usize
        {
            return Err(Error::Codec(
                "unexpected PCM frame size for opus encoder".into(),
            ));
        }
        let bytes = unsafe {
            opus_multistream_encode_float(
                self.inner.as_ptr(),
                pcm.as_ptr(),
                self.config.samples_per_frame,
                out.as_mut_ptr(),
                out.len() as c_int,
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
            .ok_or_else(|| Error::Codec("opus decoder creation returned null".into()))?;
        Ok(Self { inner, config })
    }

    pub fn decode_float(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<usize> {
        let max_samples = self.config.samples_per_frame as usize;
        let expected = max_samples * self.config.channel_count as usize;
        if out.len() < expected {
            return Err(Error::Codec("decoder output buffer is too small".into()));
        }
        let (packet_ptr, packet_len) = match packet {
            Some(packet) => (packet.as_ptr(), packet.len() as c_int),
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
        Ok(decoded as usize * self.config.channel_count as usize)
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { opus_multistream_decoder_destroy(self.inner.as_ptr()) };
    }
}

fn opus_error(code: c_int) -> String {
    unsafe {
        CStr::from_ptr(opus_strerror(code))
            .to_string_lossy()
            .into_owned()
    }
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
