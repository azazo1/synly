use super::*;
use crate::audio::config::{AudioLayout, CodecConfig};

fn stereo_config() -> OpusMultistreamConfig {
    CodecConfig::default().stream_params().unwrap().opus_config()
}

fn signal(config: OpusMultistreamConfig, frame: usize) -> Vec<f32> {
    (0..config.pcm_len()).map(|index| {
        let channel = index % config.channel_count as usize;
        let sample = frame * config.samples_per_frame as usize
            + index / config.channel_count as usize;
        let frequency = 300.0 + channel as f32 * 170.0;
        (sample as f32 * frequency * std::f32::consts::TAU / config.sample_rate as f32).sin() * 0.2
    }).collect()
}

fn encode(encoder: &mut OpusEncoder, config: OpusMultistreamConfig, frame: usize) -> Vec<u8> {
    let mut packet = vec![0; 65_536];
    let size = encoder.encode_float(&signal(config, frame), &mut packet).unwrap();
    assert!(size > 0);
    packet.truncate(size);
    packet
}

#[test]
fn round_trips_all_upstream_layouts_with_cbr_and_channel_remapping() {
    for layout in [AudioLayout::Stereo, AudioLayout::Surround51, AudioLayout::Surround71] {
        for high_quality in [false, true] {
            let params = CodecConfig { layout, high_quality, ..CodecConfig::default() }
                .stream_params().unwrap();
            let config = params.opus_config();
            let mut encoder = OpusEncoder::new(config, params.bitrate).unwrap();
            let mut decoder = OpusDecoder::new(config).unwrap();
            let mut remapped = config;
            remapped.mapping[..config.channel_count as usize].reverse();
            let mut remapped_decoder = OpusDecoder::new(remapped).unwrap();
            let mut packet_size = None;
            for frame in 0..4 {
                let packet = encode(&mut encoder, config, frame);
                assert_eq!(*packet_size.get_or_insert(packet.len()), packet.len());
                let expected = config.pcm_len();
                let mut output = vec![f32::NAN; expected + 8];
                let mut reversed = vec![f32::NAN; expected];
                assert_eq!(decoder.decode_float(Some(&packet), &mut output).unwrap(), expected);
                assert_eq!(remapped_decoder.decode_float(Some(&packet), &mut reversed).unwrap(), expected);
                assert!(output[..expected].iter().all(|sample| sample.is_finite()));
                assert!(output[..expected].iter().any(|sample| sample.abs() > 0.001));
                assert!(output[expected..].iter().all(|sample| sample.is_nan()));
                let channels = config.channel_count as usize;
                for (normal, remapped) in output[..expected].chunks_exact(channels)
                    .zip(reversed.chunks_exact(channels))
                {
                    for channel in 0..channels {
                        assert!((normal[channel] - remapped[channels - channel - 1]).abs() < 0.000_001);
                    }
                }
            }
        }
    }
}

#[test]
fn round_trips_each_supported_sample_rate_and_frame_duration() {
    for sample_rate in [8_000, 12_000, 16_000, 24_000, 48_000] {
        for units in [1, 2, 4, 8, 16, 24] {
            let config = OpusMultistreamConfig {
                sample_rate,
                samples_per_frame: sample_rate / 400 * units,
                ..stereo_config()
            };
            let mut encoder = OpusEncoder::new(config, 96_000).unwrap();
            let mut decoder = OpusDecoder::new(config).unwrap();
            let packet = encode(&mut encoder, config, 0);
            let mut output = vec![f32::NAN; config.pcm_len()];
            assert_eq!(decoder.decode_float(Some(&packet), &mut output).unwrap(), output.len());
            assert!(output.iter().all(|sample| sample.is_finite()));
        }
    }
}

#[test]
fn plc_fills_exactly_one_frame_and_recovers_on_the_next_packet() {
    for layout in [AudioLayout::Stereo, AudioLayout::Surround51, AudioLayout::Surround71] {
        let params = CodecConfig { layout, ..CodecConfig::default() }.stream_params().unwrap();
        let config = params.opus_config();
        let mut encoder = OpusEncoder::new(config, params.bitrate).unwrap();
        let mut decoder = OpusDecoder::new(config).unwrap();
        let expected = config.pcm_len();
        let mut output = vec![f32::NAN; expected + 8];
        for frame in 0..4 {
            let packet = encode(&mut encoder, config, frame);
            decoder.decode_float(Some(&packet), &mut output).unwrap();
        }
        for frame in 4..7 {
            let _lost = encode(&mut encoder, config, frame);
            output.fill(f32::NAN);
            assert_eq!(decoder.decode_float(None, &mut output).unwrap(), expected);
            assert!(output[..expected].iter().all(|sample| sample.is_finite()));
            assert!(output[expected..].iter().all(|sample| sample.is_nan()));
            if frame == 4 {
                assert!(output[..expected].iter().any(|sample| sample.abs() > 0.001));
            }
        }
        let packet = encode(&mut encoder, config, 7);
        assert_eq!(decoder.decode_float(Some(&packet), &mut output).unwrap(), expected);
        assert!(output[..expected].iter().all(|sample| sample.is_finite()));
        assert!(output[..expected].iter().any(|sample| sample.abs() > 0.001));
    }
}

#[test]
fn rejects_wrong_packet_duration_without_advancing_decoder_state() {
    let config = stereo_config();
    let mut encoder = OpusEncoder::new(config, 96_000).unwrap();
    let mut decoder = OpusDecoder::new(config).unwrap();
    let mut control = OpusDecoder::new(config).unwrap();
    let mut output = vec![0.0; config.pcm_len()];
    let mut expected = output.clone();
    for frame in 0..4 {
        let packet = encode(&mut encoder, config, frame);
        decoder.decode_float(Some(&packet), &mut output).unwrap();
        control.decode_float(Some(&packet), &mut expected).unwrap();
    }
    for samples_per_frame in [120, 480] {
        let other = OpusMultistreamConfig { samples_per_frame, ..config };
        let mut other_encoder = OpusEncoder::new(other, 96_000).unwrap();
        let packet = encode(&mut other_encoder, other, 0);
        output.fill(123.0);
        assert!(decoder.decode_float(Some(&packet), &mut output).is_err());
        assert!(output.iter().all(|&sample| sample == 123.0));
    }
    assert!(decoder.decode_float(Some(&[]), &mut output).is_err());
    assert!(decoder.decode_float(Some(&[3]), &mut output).is_err());
    decoder.decode_float(None, &mut output).unwrap();
    control.decode_float(None, &mut expected).unwrap();
    assert_eq!(output, expected);
    let packet = encode(&mut encoder, config, 5);
    decoder.decode_float(Some(&packet), &mut output).unwrap();
    control.decode_float(Some(&packet), &mut expected).unwrap();
    assert_eq!(output, expected);
}

#[test]
fn rejects_invalid_configurations_before_ffi() {
    let base = stereo_config();
    let mut configurations = Vec::new();
    for channel_count in [-1, 0, 9, c_int::MAX] {
        configurations.push(OpusMultistreamConfig { channel_count, ..base });
    }
    for streams in [-1, 0, 3, c_int::MAX] {
        configurations.push(OpusMultistreamConfig { streams, ..base });
    }
    for coupled_streams in [-1, 2, c_int::MAX] {
        configurations.push(OpusMultistreamConfig { coupled_streams, ..base });
    }
    configurations.push(OpusMultistreamConfig { streams: 2, coupled_streams: 1, ..base });
    configurations.push(OpusMultistreamConfig { mapping: [0, 2, 0, 0, 0, 0, 0, 0], ..base });
    for sample_rate in [-1, 0, 44_100, c_int::MAX] {
        configurations.push(OpusMultistreamConfig { sample_rate, ..base });
    }
    for samples_per_frame in [-1, 0, 1, 288, 961, c_int::MAX] {
        configurations.push(OpusMultistreamConfig { samples_per_frame, ..base });
    }
    for config in configurations {
        assert!(matches!(OpusEncoder::new(config, 96_000), Err(Error::InvalidConfig(_))));
        assert!(matches!(OpusDecoder::new(config), Err(Error::InvalidConfig(_))));
    }
    for bitrate in [0, c_int::MAX as u32 + 1, u32::MAX] {
        assert!(matches!(OpusEncoder::new(base, bitrate), Err(Error::InvalidConfig(_))));
    }
}

#[test]
fn decoder_supports_duplicate_and_silent_mapping_but_encoder_requires_all_inputs() {
    let config = stereo_config();
    let mut encoder = OpusEncoder::new(config, 96_000).unwrap();
    let packet = encode(&mut encoder, config, 0);
    for second in [0, 255] {
        let mapped = OpusMultistreamConfig { mapping: [0, second, 0, 0, 0, 0, 0, 0], ..config };
        assert!(matches!(OpusEncoder::new(mapped, 96_000), Err(Error::InvalidConfig(_))));
        let mut decoder = OpusDecoder::new(mapped).unwrap();
        let mut output = vec![f32::NAN; mapped.pcm_len()];
        assert_eq!(decoder.decode_float(Some(&packet), &mut output).unwrap(), output.len());
        for channels in output.as_chunks::<2>().0 {
            assert!(channels[0].is_finite());
            assert_eq!(channels[1], if second == 255 { 0.0 } else { channels[0] });
        }
    }
}

#[test]
fn rejects_invalid_buffer_lengths_and_keeps_valid_buffers_usable() {
    assert!(opus_buffer_len(0).is_err());
    assert_eq!(opus_buffer_len(c_int::MAX as usize).unwrap(), c_int::MAX);
    assert!(opus_buffer_len(c_int::MAX as usize + 1).is_err());
    assert!(opus_buffer_len(usize::MAX).is_err());
    let config = stereo_config();
    let mut encoder = OpusEncoder::new(config, 96_000).unwrap();
    let pcm = signal(config, 0);
    assert!(encoder.encode_float(&pcm, &mut []).is_err());
    assert!(encoder.encode_float(&pcm[..pcm.len() - 1], &mut [0; 1400]).is_err());
    let packet = encode(&mut encoder, config, 0);
    let mut decoder = OpusDecoder::new(config).unwrap();
    let mut short = vec![123.0; config.pcm_len() - 1];
    assert!(decoder.decode_float(Some(&packet), &mut short).is_err());
    assert!(decoder.decode_float(None, &mut short).is_err());
    assert!(short.iter().all(|&sample| sample == 123.0));
    let mut output = vec![f32::NAN; config.pcm_len()];
    assert_eq!(decoder.decode_float(Some(&packet), &mut output).unwrap(), output.len());
}

#[test]
fn stream_params_reject_invalid_durations_and_do_not_wrap_large_frame_sizes() {
    for packet_duration_ms in [5, 10, 20, 40, 60] {
        let params = CodecConfig { packet_duration_ms, ..CodecConfig::default() }
            .stream_params().unwrap();
        assert_eq!(params.frame_size(), packet_duration_ms as usize * 48);
        assert_eq!(params.samples_per_frame(), params.frame_size() * 2);
        assert!(params.opus_config().validate().is_ok());
    }
    for packet_duration_ms in [0, 1, 2, 3, 6, 15, 25, 61, u32::MAX] {
        assert!(matches!(CodecConfig { packet_duration_ms, ..CodecConfig::default() }
            .stream_params(), Err(Error::InvalidConfig(_))));
    }
    let mut params = CodecConfig::default().stream_params().unwrap();
    params.packet_duration_ms = u32::MAX;
    assert!(params.opus_config().validate().is_err());
    params.sample_rate = u32::MAX;
    assert!(params.samples_per_frame() > 0);
    assert!(params.opus_config().validate().is_err());
}
