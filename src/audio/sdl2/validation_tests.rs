use super::*;
use crate::audio::config::{AudioLayout, CodecConfig};

#[test]
fn supported_pcm_requests_have_nonzero_bounded_buffers() {
    for rate in [8000, 12000, 16000, 24000, 48000] {
        for layout in [AudioLayout::Stereo, AudioLayout::Surround51, AudioLayout::Surround71] {
            for ms in [5, 10, 20, 40, 60] {
                let mut stream = CodecConfig { layout, packet_duration_ms: ms, ..CodecConfig::default() }.stream_params().unwrap();
                stream.sample_rate = rate;
                validate_request(&PlaybackConfig::default(), &stream).unwrap();
                let bytes = interleaved_frame_bytes(&stream).unwrap();
                assert!(bytes > 0 && u32::try_from(bytes).is_ok());
                assert!(requested_samples(&stream).unwrap() >= 480);
            }
        }
    }
}

#[test]
fn invalid_requests_fail_through_platform_entry_without_opening_sdl() {
    let original = CodecConfig::default().stream_params().unwrap();
    let config = PlaybackConfig::default();
    for rate in [0, 44100, u32::MAX] {
        let mut stream = original.clone();
        stream.sample_rate = rate;
        assert!(matches!(crate::audio::platform::open_output(&config, &stream), Err(Error::InvalidConfig(_))));
    }
    for channels in [0, 1, 3, 255] {
        let mut stream = original.clone();
        stream.channels = channels;
        assert!(matches!(crate::audio::platform::open_output(&config, &stream), Err(Error::InvalidConfig(_))));
    }
    for ms in [0, 1, 3, u32::MAX] {
        let mut stream = original.clone();
        stream.packet_duration_ms = ms;
        assert!(matches!(crate::audio::platform::open_output(&config, &stream), Err(Error::InvalidConfig(_))));
    }
    for name in ["", "设备\0后缀"] {
        let config = PlaybackConfig { device_name: Some(name.into()) };
        assert!(matches!(crate::audio::platform::open_output(&config, &original), Err(Error::InvalidConfig(_))));
    }
    let selected = PlaybackConfig { device_name: Some("平台设备标识".into()) };
    assert!(matches!(crate::audio::platform::open_output(&selected, &original), Err(Error::UnsupportedPlatform(_))));
}
