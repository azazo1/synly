//! 固定 Sunshine 40b36212 的默认编码参数对照, 不使用 Synly 配置创建参考端.
//! 参数来自 src/audio.cpp:51-100 与 src/platform/common.h:279-320.
//! 参考路径直接调用同一个已链接 Opus 的 C API, 不运行 Sunshine/RTSP 或物理设备.
use super::*;
use crate::audio::config::{AudioLayout, CodecConfig};

#[derive(Clone, Copy)]
struct Reference {
    layout: AudioLayout,
    high_quality: bool,
    channels: c_int,
    streams: c_int,
    coupled: c_int,
    bitrate: c_int,
}
const PRESETS: [Reference; 6] = [
    Reference { layout: AudioLayout::Stereo, high_quality: false, channels: 2, streams: 1, coupled: 1, bitrate: 96_000 },
    Reference { layout: AudioLayout::Stereo, high_quality: true, channels: 2, streams: 1, coupled: 1, bitrate: 512_000 },
    Reference { layout: AudioLayout::Surround51, high_quality: false, channels: 6, streams: 4, coupled: 2, bitrate: 256_000 },
    Reference { layout: AudioLayout::Surround51, high_quality: true, channels: 6, streams: 6, coupled: 0, bitrate: 1_536_000 },
    Reference { layout: AudioLayout::Surround71, high_quality: false, channels: 8, streams: 5, coupled: 3, bitrate: 450_000 },
    Reference { layout: AudioLayout::Surround71, high_quality: true, channels: 8, streams: 8, coupled: 0, bitrate: 2_048_000 },
];
// 原生 PCM 的 FL,FR,FC,LFE,BL,BR,SL,SR 顺序, 不是旧 GFE 的硬编码 fallback.
const IDENTITY: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

struct ReferenceEncoder(NonNull<OpusMSEncoder>);
impl ReferenceEncoder {
    fn new(preset: Reference) -> Self {
        let mut error = 0;
        let raw = unsafe { opus_multistream_encoder_create(48_000, preset.channels, preset.streams,
            preset.coupled, IDENTITY.as_ptr(), 2051, &mut error) };
        let encoder = Self(NonNull::new(raw).expect("参考编码器必须创建成功"));
        assert_eq!(error, 0);
        // 常量直接来自 Opus API, 不复用生产构造函数或它读取的配置.
        assert_eq!(unsafe { opus_multistream_encoder_ctl(encoder.0.as_ptr(), 4002, preset.bitrate) }, 0);
        assert_eq!(unsafe { opus_multistream_encoder_ctl(encoder.0.as_ptr(), 4006, 0 as c_int) }, 0);
        encoder
    }
    fn encode(&mut self, pcm: &[f32], frame_size: c_int) -> Vec<u8> {
        let mut packet = vec![0; 65536];
        let size = unsafe { opus_multistream_encode_float(self.0.as_ptr(), pcm.as_ptr(), frame_size,
            packet.as_mut_ptr(), packet.len() as c_int) };
        assert!(size > 0);
        packet.truncate(size as usize);
        packet
    }
}
impl Drop for ReferenceEncoder {
    fn drop(&mut self) { unsafe { opus_multistream_encoder_destroy(self.0.as_ptr()); } }
}
struct ReferenceDecoder(NonNull<OpusMSDecoder>);
impl ReferenceDecoder {
    fn new(preset: Reference) -> Self {
        let mut error = 0;
        let raw = unsafe { opus_multistream_decoder_create(48_000, preset.channels, preset.streams,
            preset.coupled, IDENTITY.as_ptr(), &mut error) };
        let decoder = Self(NonNull::new(raw).expect("参考解码器必须创建成功"));
        assert_eq!(error, 0);
        decoder
    }
    fn decode(&mut self, packet: Option<&[u8]>, pcm: &mut [f32], frame_size: c_int) {
        let (data, length) = packet.map_or((std::ptr::null(), 0), |data| (data.as_ptr(), data.len() as c_int));
        let size = unsafe { opus_multistream_decode_float(self.0.as_ptr(), data, length, pcm.as_mut_ptr(), frame_size, 0) };
        assert_eq!(size, frame_size);
    }
}
impl Drop for ReferenceDecoder {
    fn drop(&mut self) { unsafe { opus_multistream_decoder_destroy(self.0.as_ptr()); } }
}
fn params(preset: Reference, duration: u32) -> crate::audio::config::StreamParams {
    CodecConfig { layout: preset.layout, high_quality: preset.high_quality, packet_duration_ms: duration }.stream_params().unwrap()
}
fn signal(channels: usize, frames: usize, packet: usize, active: Option<usize>) -> Vec<f32> {
    (0..frames * channels).map(|index| {
        let channel = index % channels;
        if active.is_some_and(|value| value != channel) { return 0.0; }
        let time = (packet * frames + index / channels) as f32 / 48_000.0;
        let frequency = if active.is_some() { 300.0 } else { 170.0 + channel as f32 * 113.0 };
        (time * frequency * std::f32::consts::TAU).sin() * 0.25
    }).collect()
}

#[test]
fn default_presets_match_pinned_sunshine_parameters() {
    for reference in PRESETS {
        for duration in [5, 10, 20, 40, 60] {
            let stream = params(reference, duration);
            assert_eq!((stream.sample_rate, i32::from(stream.channels), i32::from(stream.streams), i32::from(stream.coupled_streams), stream.bitrate),
                (48_000, reference.channels, reference.streams, reference.coupled, reference.bitrate as u32));
            assert_eq!(&stream.mapping[..reference.channels as usize], &IDENTITY[..reference.channels as usize]);
            assert_eq!(stream.frame_size(), duration as usize * 48);
        }
    }
}

#[test]
fn encoded_packets_and_cross_decoded_pcm_match_direct_c_api() {
    for reference in PRESETS {
        for duration in [5, 10, 20, 40, 60] {
            let stream = params(reference, duration);
            let mut encoder = OpusEncoder::new(stream.opus_config(), stream.bitrate).unwrap();
            let mut decoder = OpusDecoder::new(stream.opus_config()).unwrap();
            let mut direct_encoder = ReferenceEncoder::new(reference);
            let mut direct_decoder = ReferenceDecoder::new(reference);
            for frame in 0..6 {
                let input = signal(reference.channels as usize, duration as usize * 48, frame, None);
                let expected = direct_encoder.encode(&input, (duration * 48) as c_int);
                let mut packet = vec![0; 65536];
                let size = encoder.encode_float(&input, &mut packet).unwrap();
                // 同一进程同一 Opus 构建中比较, 不依赖跨版本或跨CPU的压缩字节稳定性.
                assert_eq!(size, expected.len());
                assert!(packet[..size] == expected, "编码字节不一致: channels={}, hq={}, duration={duration}, frame={frame}", reference.channels, reference.high_quality);
                let mut actual = vec![f32::NAN; input.len()];
                let mut reference_pcm = vec![f32::NAN; input.len()];
                // 原始 C 编码器 -> Synly 解码器, Synly 编码器 -> 原始 C 解码器.
                // 同时在第3帧模拟丢包, 验证PLC和随后恢复仍一致.
                let lost = frame == 2;
                let written = decoder.decode_float((!lost).then_some(expected.as_slice()), &mut actual).unwrap();
                direct_decoder.decode((!lost).then_some(&packet[..size]), &mut reference_pcm, (duration * 48) as c_int);
                assert_eq!(written, input.len());
                assert!(actual.iter().all(|sample| sample.is_finite()));
                assert!(actual.iter().zip(reference_pcm).all(|(a, b)| a.to_bits() == b.to_bits()), "PCM 不一致: channels={}, hq={}, duration={duration}, frame={frame}", reference.channels, reference.high_quality);
            }
        }
    }
}

#[test]
fn reference_decoder_keeps_each_speaker_at_its_pcm_index() {
    for reference in PRESETS {
        let stream = params(reference, 5);
        for active in 0..reference.channels as usize {
            let mut encoder = OpusEncoder::new(stream.opus_config(), stream.bitrate).unwrap();
            let mut decoder = ReferenceDecoder::new(reference);
            let mut energy = vec![0.0f64; reference.channels as usize];
            for frame in 0..12 {
                let pcm = signal(reference.channels as usize, 240, frame, Some(active));
                let mut packet = vec![0; 65536];
                let length = encoder.encode_float(&pcm, &mut packet).unwrap();
                let mut decoded = vec![f32::NAN; pcm.len()];
                decoder.decode(Some(&packet[..length]), &mut decoded, 240);
                if frame >= 4 {
                    for samples in decoded.chunks_exact(energy.len()) {
                        for (channel, value) in samples.iter().enumerate() { energy[channel] += f64::from(*value).powi(2); }
                    }
                }
            }
            assert!(energy[active] > 1.0, "目标扬声器没有信号: active={active}, energy={energy:?}");
            for (channel, value) in energy.iter().enumerate() {
                if channel != active { assert!(*value < energy[active] * 0.01, "扬声器映射错误: active={active}, other={channel}, energy={energy:?}"); }
            }
        }
    }
}
