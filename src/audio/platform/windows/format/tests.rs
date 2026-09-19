use super::*;
use crate::audio::codec::{OpusDecoder, OpusEncoder};
use crate::audio::config::{AudioLayout, CodecConfig};
use super::super::{QueueBudget, SharedSampleRing};
use std::mem::{offset_of, size_of};
use std::time::Duration;

#[test]
fn extensible_abi_matches_windows_sdk_offsets_and_bytes() {
    assert_eq!(size_of::<WaveFormatEx>(), 18);
    assert_eq!(size_of::<WaveFormatExtensible>(), 40);
    assert_eq!(offset_of!(WaveFormatEx, cb_size), 16);
    assert_eq!(offset_of!(WaveFormatExtensible, valid_bits), 18);
    assert_eq!(offset_of!(WaveFormatExtensible, channel_mask), 20);
    assert_eq!(offset_of!(WaveFormatExtensible, subformat), 24);
    for (channels, mask) in [(2, 0x3u32), (6, 0x3f), (8, 0x63f)] {
        let format = WasapiSpec { sample_rate: 48_000, channels }.wave_format().unwrap();
        let bytes = unsafe { std::slice::from_raw_parts((&format as *const WaveFormatExtensible).cast::<u8>(), 40) };
        assert_eq!(&bytes[0..2], &0xfffeu16.to_le_bytes());
        assert_eq!(&bytes[2..4], &channels.to_le_bytes());
        assert_eq!(&bytes[4..8], &48_000u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &(48_000u32 * u32::from(channels) * 4).to_le_bytes());
        assert_eq!(&bytes[12..14], &(channels * 4).to_le_bytes());
        assert_eq!(&bytes[14..20], &[32, 0, 22, 0, 32, 0]);
        assert_eq!(&bytes[20..24], &mask.to_le_bytes());
        assert_eq!(&bytes[24..40], &[3, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]);
    }
}
#[test]
fn unsupported_layouts_zero_rate_and_byte_rate_overflow_fail_before_ffi() {
    for channels in [0, 1, 3, 4, 5, 7, 9, u16::MAX] {
        assert!(matches!(WasapiSpec { sample_rate: 48_000, channels }.wave_format(), Err(Error::UnsupportedPlatform(_))));
    }
    for sample_rate in [0, u32::MAX] {
        assert!(matches!(WasapiSpec { sample_rate, channels: 8 }.wave_format(), Err(Error::InvalidConfig(_))));
    }
}
#[test]
fn only_compatible_native_speaker_masks_are_used() {
    for (channels, canonical, supported) in [(2, 0x3, vec![0x3]), (6, 0x3f, vec![0x3f, 0x60f]), (8, 0x63f, vec![0x63f])] {
        for mask in [0, 0x3, 0x3f, 0x60f, 0x63f, 0xff, u32::MAX] {
            let mut format = WasapiSpec { sample_rate: 48_000, channels }.wave_format().unwrap();
            let accepted = format.prefer_native_mask(channels, mask);
            assert_eq!(accepted, supported.contains(&mask));
            let actual = format.channel_mask;
            assert_eq!(actual, if accepted { mask } else { canonical });
            assert!(!format.prefer_native_mask(channels + 1, mask));
        }
    }
}
#[test]
fn multichannel_queues_keep_interleaved_frames_at_all_durations() {
    for layout in [AudioLayout::Stereo, AudioLayout::Surround51, AudioLayout::Surround71] {
        for packet_duration_ms in [5, 10, 20, 40, 60] {
            let params = CodecConfig { layout, packet_duration_ms, ..CodecConfig::default() }.stream_params().unwrap();
            let budget = QueueBudget::from_stream(&params).unwrap();
            let ring = SharedSampleRing::new(budget.capture_samples, budget.channels, budget.frame_samples, budget.capture_samples, "测试").unwrap();
            let input: Vec<_> = (0..budget.frame_samples).map(|i| i as f32).collect();
            for _ in 0..3 {
                ring.write_overwrite(&input);
                let mut output = vec![f32::NAN; input.len()];
                assert!(ring.read_exact(&mut output, Duration::ZERO).unwrap());
                assert_eq!(input, output);
            }
            let output = SharedSampleRing::new(budget.playback_samples, budget.channels, budget.frame_samples, budget.playback_watermark, "测试").unwrap();
            output.write_blocking(&input, Duration::ZERO).unwrap();
            let mut samples = vec![f32::NAN; input.len() + budget.channels];
            assert_eq!(output.read_partial_zero_fill(&mut samples), input.len());
            assert_eq!(samples[..input.len()], input);
            assert!(samples[input.len()..].iter().all(|value| *value == 0.0));
        }
    }
}
#[test]
fn opus_roundtrip_preserves_each_windows_pcm_channel_position() {
    // 各扬声器单独输入, 验证编码映射不会让声道索引偏移. 先跨过编码器延迟再比较能量.
    for layout in [AudioLayout::Stereo, AudioLayout::Surround51, AudioLayout::Surround71] {
        for high_quality in [false, true] {
            let params = CodecConfig { layout, high_quality, ..CodecConfig::default() }.stream_params().unwrap();
            let config = params.opus_config();
            for active in 0..usize::from(params.channels) {
                let mut encoder = OpusEncoder::new(config, params.bitrate).unwrap();
                let mut decoder = OpusDecoder::new(config).unwrap();
                let mut energy = vec![0.0f64; usize::from(params.channels)];
                for frame in 0..12 {
                    let mut pcm = vec![0.0; params.samples_per_frame()];
                    for (index, samples) in pcm.chunks_exact_mut(energy.len()).enumerate() {
                        samples[active] = ((frame * params.frame_size() + index) as f32 * 300.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.25;
                    }
                    let mut packet = vec![0; 65536];
                    let length = encoder.encode_float(&pcm, &mut packet).unwrap();
                    let mut decoded = vec![0.0; pcm.len()];
                    assert_eq!(decoder.decode_float(Some(&packet[..length]), &mut decoded).unwrap(), pcm.len());
                    if frame >= 4 {
                        for samples in decoded.chunks_exact(energy.len()) {
                            for (channel, sample) in samples.iter().enumerate() { energy[channel] += f64::from(*sample).powi(2); }
                        }
                    }
                }
                assert!(energy[active] > 1.0);
                for (channel, value) in energy.iter().enumerate() {
                    if channel != active { assert!(*value < energy[active] * 0.01, "active={active}, other={channel}, energy={energy:?}"); }
                }
            }
        }
    }
}
