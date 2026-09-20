use super::*;
use super::crypto::{AudioDecryptor, AudioEncryptor};
use super::queue::FrameQueue;
use super::workers::Workers;
use crate::audio::codec::OpusEncoder;
use crate::audio::config::CodecConfig;
use crate::audio::playback::AudioOutput;
use crate::audio::receiver::{AudioDepacketizer, QueuedAudioFrame};
use crate::audio::sender::AudioPacketizer;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn dropping_audio_task_requests_stop() {
    let stop = CancellationToken::new();
    let worker_stop = stop.clone();
    let (finished, receive_finished) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        worker_stop.cancelled().await;
        let _ = finished.send(());
        Ok(())
    });
    drop(AudioTaskHandle { stop: stop.clone(), task: Some(task) });
    assert!(stop.is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), receive_finished).await.unwrap().unwrap();
}

#[tokio::test]
async fn device_initialization_failure_stops_other_stages() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender.connect(receiver.local_addr().unwrap()).await.unwrap();
    let stop = CancellationToken::new();
    let result = tokio::time::timeout(Duration::from_secs(2), send::run_with_input(
        sender, stop.clone(), [7; 32], AudioChannelDirection::HostToClient, stream.clone(),
        || Err(crate::audio::error::Error::UnsupportedPlatform("测试不支持捕获的平台")),
    )).await.unwrap();
    assert!(result.is_err());
    assert!(stop.is_cancelled());
    let stop = CancellationToken::new();
    let result = tokio::time::timeout(Duration::from_secs(2), receive::run_with_output(
        receiver, stop.clone(), [7; 32], AudioChannelDirection::HostToClient,
        IpAddr::V4(Ipv4Addr::LOCALHOST), stream,
        || Err(crate::audio::error::Error::UnsupportedPlatform("测试不支持播放的平台")),
    )).await.unwrap();
    assert!(result.is_err());
    assert!(stop.is_cancelled());
}

struct FeedInput {
    frames: std::sync::mpsc::Receiver<Vec<f32>>,
}

impl crate::audio::capture::AudioInput for FeedInput {
    fn read_frame(&mut self, frame: &mut [f32], timeout: Duration) -> crate::audio::error::Result<crate::audio::capture::CaptureStatus> {
        match self.frames.recv_timeout(timeout) {
            Ok(samples) => {
                frame.copy_from_slice(&samples);
                Ok(crate::audio::capture::CaptureStatus::Ok)
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(crate::audio::capture::CaptureStatus::Timeout),
            Err(error) => Err(crate::audio::error::Error::Backend(error.to_string())),
        }
    }
}

#[tokio::test]
async fn capture_encode_and_udp_stages_deliver_decodable_audio_and_fec() {
    for layout in [crate::audio::AudioLayout::Stereo, crate::audio::AudioLayout::Surround51, crate::audio::AudioLayout::Surround71] {
        verify_capture_transport(layout).await;
    }
}

async fn verify_capture_transport(layout: crate::audio::AudioLayout) {
    let stream = CodecConfig { layout, ..CodecConfig::default() }.stream_params().unwrap();
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender.connect(receiver.local_addr().unwrap()).await.unwrap();
    let stop = CancellationToken::new();
    let (feed, frames) = std::sync::mpsc::channel();
    let mut frames = Some(frames);
    let task = tokio::spawn(send::run_with_input(
        sender, stop.clone(), [7; 32], AudioChannelDirection::HostToClient, stream.clone(),
        move || Ok(Box::new(FeedInput { frames: frames.take().expect("此测试不应重建设备") })),
    ));
    let mut reference_encoder = OpusEncoder::new(stream.opus_config(), stream.bitrate).unwrap();
    let mut reference_packets = Vec::new();
    for frame_index in 0..5 {
        let samples: Vec<f32> = (0..stream.samples_per_frame()).map(|index| {
            let channel = index % usize::from(stream.channels);
            let sample = frame_index * stream.frame_size() + index / usize::from(stream.channels);
            let frequency = 150.0 + channel as f32 * 75.0;
            (sample as f32 * frequency * std::f32::consts::TAU / stream.sample_rate as f32).sin() * 0.1
        }).collect();
        let mut encoded = vec![0; 1400];
        let length = reference_encoder.encode_float(&samples, &mut encoded).unwrap();
        encoded.truncate(length);
        reference_packets.push(encoded);
        feed.send(samples).unwrap();
    }
    let mut decryptor = AudioDecryptor::new([7; 32], AudioChannelDirection::HostToClient).unwrap();
    let mut decoder = crate::audio::codec::OpusDecoder::new(stream.opus_config()).unwrap();
    let mut buffer = [0u8; 2048];
    let mut pcm = vec![0.0; stream.samples_per_frame()];
    let mut audio_count = 0;
    let mut fec_count = 0;
    let reception = tokio::time::timeout(Duration::from_secs(2), async {
        for _ in 0..7 {
            let size = receiver.recv(&mut buffer).await.unwrap();
            let packet = decryptor.decrypt(&buffer[..size]).unwrap();
            match crate::audio::protocol::parse_datagram(&packet).unwrap() {
                crate::audio::protocol::ParsedPacket::Audio { rtp, payload } => {
                    assert_eq!(rtp.sequence_number, audio_count);
                    // 与绕过采集队列/分包/加密/UDP的独立编码器逐包比较, 捕获错序或声道截断.
                    assert_eq!(payload.as_slice(), reference_packets[audio_count as usize].as_slice());
                    assert_eq!(decoder.decode_float(Some(&payload), &mut pcm).unwrap(), pcm.len());
                    assert!(pcm.iter().all(|sample| sample.is_finite()));
                    audio_count += 1;
                }
                crate::audio::protocol::ParsedPacket::Fec { .. } => fec_count += 1,
            }
        }
    }).await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert!(reception.is_ok());
    assert_eq!((audio_count, fec_count), (5, 2));
}

#[tokio::test]
async fn multichannel_capture_to_playback_uses_full_pcm_frames() {
    for layout in [crate::audio::AudioLayout::Surround51, crate::audio::AudioLayout::Surround71] {
        let stream = CodecConfig { layout, ..CodecConfig::default() }.stream_params().unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.connect(receiver.local_addr().unwrap()).await.unwrap();
        let stop = CancellationToken::new();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let output_frames = Arc::clone(&recorded);
        let output_stop = stop.clone();
        let receive_task = tokio::spawn(receive::run_with_output(
            receiver, stop.clone(), [9; 32], AudioChannelDirection::ClientToHost,
            IpAddr::V4(Ipv4Addr::LOCALHOST), stream.clone(),
            move || Ok(Box::new(RecordingOutput {
                frames: Arc::clone(&output_frames), stop: output_stop.clone(), expected: 8,
            })),
        ));
        let (feed, frames) = std::sync::mpsc::channel();
        let mut frames = Some(frames);
        let send_task = tokio::spawn(send::run_with_input(
            sender, stop.clone(), [9; 32], AudioChannelDirection::ClientToHost, stream.clone(),
            move || Ok(Box::new(FeedInput { frames: frames.take().expect("测试采集端不应重建") })),
        ));
        // 按帧时长供给, 穿过产品接收端的启动丢弃窗口, 避免制造人为网络积压.
        let progress = tokio::time::timeout(Duration::from_secs(5), async {
            let mut timer = tokio::time::interval(Duration::from_millis(5));
            let mut sample_index = 0usize;
            loop {
                tokio::select! {
                    biased;
                    _ = stop.cancelled() => break,
                    _ = timer.tick() => {
                        let pcm = (0..stream.samples_per_frame()).map(|index| {
                            let channel = index % usize::from(stream.channels);
                            let sample = sample_index + index / usize::from(stream.channels);
                            (sample as f32 * (150.0 + channel as f32 * 75.0)
                                * std::f32::consts::TAU / stream.sample_rate as f32).sin() * 0.1
                        }).collect();
                        sample_index += stream.frame_size();
                        if feed.send(pcm).is_err() { break; }
                    }
                }
            }
        }).await;
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(2), send_task).await.unwrap().unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(2), receive_task).await.unwrap().unwrap().unwrap();
        assert!(progress.is_ok());
        let frames = recorded.lock().unwrap();
        assert_eq!(frames.len(), 8);
        assert!(frames.iter().all(|frame| frame.len() == stream.samples_per_frame()
            && frame.iter().all(|sample| sample.is_finite())));
        for channel in 0..usize::from(stream.channels) {
            let energy: f32 = frames.iter().flat_map(|frame| frame.iter().skip(channel)
                .step_by(usize::from(stream.channels))).map(|sample| sample * sample).sum();
            assert!(energy > 0.001, "{layout:?} 声道 {channel} 不应静音");
        }
    }
}

struct RecordingOutput {
    frames: Arc<Mutex<Vec<Vec<f32>>>>,
    stop: CancellationToken,
    expected: usize,
}

impl AudioOutput for RecordingOutput {
    fn submit_frame(&mut self, frame: &[f32], _timeout: Duration) -> crate::audio::error::Result<()> {
        let mut frames = self.frames.lock().unwrap();
        frames.push(frame.to_vec());
        if frames.len() == self.expected {
            self.stop.cancel();
        }
        Ok(())
    }
}

#[tokio::test]
async fn decoder_skips_pcm_until_network_backlog_is_at_most_thirty_ms() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let samples = stream.samples_per_frame();
    let stop = CancellationToken::new();
    let mut workers = Workers::new(stop.clone());
    let queue = workers.queue("decode-test", 30);
    for _ in 0..10 { queue.push(QueuedAudioFrame::Missing); }
    let frames = Arc::new(Mutex::new(Vec::new()));
    let output = Box::new(RecordingOutput { frames: Arc::clone(&frames), stop: stop.clone(), expected: 7 });
    workers.spawn_blocking(move || render::decode_frames(output, queue, stop, &stream).map_err(Into::into));
    tokio::time::timeout(Duration::from_secs(2), workers.finish()).await.unwrap().unwrap();
    let frames = frames.lock().unwrap();
    assert_eq!(frames.len(), 7);
    assert!(frames.iter().all(|frame| frame.len() == samples && frame.iter().all(|sample| sample.is_finite())));
}

struct PausedOutput {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: std::sync::mpsc::Receiver<()>,
    submissions: Arc<AtomicUsize>,
}

impl AudioOutput for PausedOutput {
    fn submit_frame(&mut self, _frame: &[f32], _timeout: Duration) -> crate::audio::error::Result<()> {
        self.submissions.fetch_add(1, Ordering::Relaxed);
        if let Some(entered) = self.entered.take() {
            let _ = entered.send(());
            self.release.recv_timeout(Duration::from_secs(3))
                .map_err(|_| crate::audio::error::Error::Backend("测试播放等待超时".into()))?;
        }
        Ok(())
    }
}

async fn send_encoded_frame(
    socket: &UdpSocket,
    packetizer: &mut AudioPacketizer,
    encryptor: &mut AudioEncryptor,
    payload: &[u8],
) {
    for packet in packetizer.push_encoded_frame(payload).unwrap() {
        let packet = encryptor.encrypt(&packet.bytes).unwrap();
        socket.send(&packet).await.unwrap();
    }
}

#[tokio::test]
async fn stalled_playback_does_not_block_udp_receive_and_queue_remains_bounded() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let mut encoder = OpusEncoder::new(stream.opus_config(), stream.bitrate).unwrap();
    let pcm: Vec<_> = (0..stream.samples_per_frame()).map(|index| (index as f32 * 0.1).sin() * 0.1).collect();
    let mut encoded = [0u8; 1400];
    let length = encoder.encode_float(&pcm, &mut encoded).unwrap();
    let payload = &encoded[..length];
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender.connect(receiver.local_addr().unwrap()).await.unwrap();
    let stop = CancellationToken::new();
    let mut workers = Workers::new(stop.clone());
    let queue: Arc<FrameQueue<QueuedAudioFrame>> = workers.queue("decode-test", 30);
    let decode_queue = Arc::clone(&queue);
    let decode_stop = stop.clone();
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let submissions = Arc::new(AtomicUsize::new(0));
    let output = Box::new(PausedOutput { entered: Some(entered), release: release_rx, submissions: Arc::clone(&submissions) });
    workers.spawn_blocking(move || render::decode_frames(output, decode_queue, decode_stop, &stream).map_err(Into::into));
    workers.spawn(receive::receive_packets(
        receiver, Arc::clone(&queue), stop.clone(),
        AudioDecryptor::new([7; 32], AudioChannelDirection::HostToClient).unwrap(),
        IpAddr::V4(Ipv4Addr::LOCALHOST), AudioDepacketizer::new(5, 0),
    ));
    let supervisor = tokio::spawn(workers.finish());
    let mut packetizer = AudioPacketizer::new(5, 7, true);
    let mut encryptor = AudioEncryptor::new([7; 32], AudioChannelDirection::HostToClient).unwrap();
    for _ in 0..5 { send_encoded_frame(&sender, &mut packetizer, &mut encryptor, payload).await; }
    tokio::time::timeout(Duration::from_secs(2), entered_rx).await.unwrap().unwrap();
    for _ in 5..36 { send_encoded_frame(&sender, &mut packetizer, &mut encryptor, payload).await; }
    let receive_progress = tokio::time::timeout(Duration::from_secs(2), async {
        while queue.dropped() < 30 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }).await;
    // 先释放测试播放端, 确保失败断言也不会遗留阻塞线程.
    stop.cancel();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), supervisor).await.unwrap().unwrap().unwrap();
    assert!(receive_progress.is_ok());
    assert_eq!(queue.dropped(), 30);
    assert_eq!(submissions.load(Ordering::Relaxed), 1);
}
