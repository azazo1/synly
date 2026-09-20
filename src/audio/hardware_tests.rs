// 仅显式运行的物理设备诊断, 不保存或回放捕获数据.
use super::config::{CaptureConfig, CodecConfig, PlaybackConfig};
use super::capture::CaptureStatus;
use std::time::{Duration, Instant};

fn tone_frame(stream: &super::config::StreamParams, frame_index: usize) -> Vec<f32> {
    (0..stream.samples_per_frame()).map(|index| {
        let sample = frame_index * stream.frame_size() + index / usize::from(stream.channels);
        let position = sample as f32 / stream.sample_rate as f32;
        let fade = (position / 0.02).min(1.0) * ((1.0 - position) / 0.02).clamp(0.0, 1.0);
        (position * 440.0 * std::f32::consts::TAU).sin() * 0.02 * fade
    }).collect()
}

#[test]
fn diagnostic_tone_has_no_frame_boundary_phase_reset() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let pcm: Vec<f32> = (0..200).flat_map(|index| tone_frame(&stream, index)).collect();
    assert_eq!(pcm.len(), 48000 * 2);
    assert_eq!(pcm[0], 0.0);
    assert!(pcm[pcm.len() - 1].abs() < 0.00001);
    let mut previous = 0.0f32;
    let mut energy = 0.0f64;
    for stereo in pcm.as_chunks::<2>().0 {
        assert_eq!(stereo[0], stereo[1]);
        assert!(stereo[0].is_finite() && stereo[0].abs() <= 0.020001);
        // 440 Hz 在 48 kHz 下相邻样本最大差约 0.001152, 包括跨帧位置.
        assert!((stereo[0] - previous).abs() < 0.0013);
        previous = stereo[0];
        energy += f64::from(stereo[0]).powi(2);
    }
    assert!(energy > 8.0 && energy < 10.0);
}

#[test]
#[ignore = "访问真实声卡并播放提示音, 必须经用户允许后单独运行"]
fn local_capture_and_playback() {
    assert_eq!(std::env::var("SYNLY_AUDIO_HARDWARE_TEST").as_deref(), Ok("1"));
    let _diagnostic_log = tracing::subscriber::set_default(
        tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).with_writer(std::io::stderr).finish(),
    );
    let stream = CodecConfig::default().stream_params().unwrap();
    eprintln!("[1/2] 播放低音量 440 Hz 提示音, 约 1 秒");
    {
        let mut output = super::platform::open_output(&PlaybackConfig::default(), &stream).unwrap();
        let mut longest_submit = Duration::ZERO;
        let playback_started = Instant::now();
        for frame_index in 0..200 {
            let pcm = tone_frame(&stream, frame_index);
            let started = Instant::now();
            output.submit_frame(&pcm, Duration::from_millis(200)).unwrap();
            longest_submit = longest_submit.max(started.elapsed());
        }
        eprintln!("供帧完成: elapsed_ms={}, longest_submit_us={}",
            playback_started.elapsed().as_millis(), longest_submit.as_micros());
        std::thread::sleep(Duration::from_millis(300));
    }
    eprintln!("[2/2] 采集系统音频 5 秒, 仅统计帧数与峰值, 不保存 PCM");
    let mut input = None;
    for attempt in 1..=3 {
        match super::platform::open_input(&CaptureConfig::default(), &stream) {
            Ok(device) => { input = Some(device); break; }
            Err(error) => {
                eprintln!("采集初始化第 {attempt}/3 次失败: {error}");
                if !matches!(error, super::error::Error::Backend(_) | super::error::Error::Io(_)) {
                    panic!("不可重试的采集错误: {error}");
                }
                if attempt < 3 {
                    eprintln!("按产品恢复间隔等待 5 秒后重新创建设备");
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        }
    }
    let mut input = input.expect("三次采集初始化均失败, 请检查设备变化诊断");
    let mut pcm = vec![0.0; stream.samples_per_frame()];
    let started = Instant::now();
    let mut frames = 0;
    let mut timeouts = 0;
    let mut peak = 0.0f32;
    let mut reported_second = 0;
    while started.elapsed() < Duration::from_secs(5) {
        match input.read_frame(&mut pcm, Duration::from_millis(200)).unwrap() {
            CaptureStatus::Ok => {
                frames += 1;
                for value in &pcm {
                    assert!(value.is_finite());
                    peak = peak.max(value.abs());
                }
            }
            CaptureStatus::Timeout => timeouts += 1,
        }
        let second = started.elapsed().as_secs();
        if second > reported_second {
            eprintln!("采集 {second} 秒: frames={frames}, timeouts={timeouts}, peak={peak:.6}");
            reported_second = second;
        }
    }
    drop(input);
    assert!(frames > 0, "未收到系统音频帧");
    eprintln!("真实设备测试结束: frames={frames}, timeouts={timeouts}, peak={peak:.6}. 非零峰值和听感需结合实际播放源确认");
}
