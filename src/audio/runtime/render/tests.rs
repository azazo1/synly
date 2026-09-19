use super::*;
use super::super::workers::Workers;
use crate::audio::codec::OpusEncoder;
use crate::audio::config::CodecConfig;
use std::sync::atomic::{AtomicUsize, Ordering};

struct ProbeOutput {
    fail: bool,
    stop: CancellationToken,
    submitted: Arc<AtomicUsize>,
    destroyed: Arc<AtomicUsize>,
}

impl Drop for ProbeOutput {
    fn drop(&mut self) { self.destroyed.fetch_add(1, Ordering::SeqCst); }
}

impl AudioOutput for ProbeOutput {
    fn submit_frame(&mut self, pcm: &[f32], _timeout: Duration) -> Result<()> {
        self.submitted.fetch_add(1, Ordering::SeqCst);
        if self.fail { return Err(Error::Backend("模拟设备失效".into())); }
        // 恢复后的首个包是 PLC, 新解码器应输出静音. 旧包或旧解码器不能泄漏过来.
        assert!(pcm.iter().all(|sample| *sample == 0.0));
        self.stop.cancel();
        Ok(())
    }
}

async fn exercise_recovery(initial_failure: bool) {
    let stream = CodecConfig::default().stream_params().unwrap();
    let mut encoder = OpusEncoder::new(stream.opus_config(), stream.bitrate).unwrap();
    let input: Vec<_> = (0..stream.samples_per_frame()).map(|i| (i as f32 * 0.15).sin() * 0.5).collect();
    let mut packet = vec![0; 1400];
    let len = encoder.encode_float(&input, &mut packet).unwrap();
    packet.truncate(len);
    let stop = CancellationToken::new();
    let mut workers = Workers::new(stop.clone());
    let queue = workers.queue("recovery-test", 30);
    if !initial_failure { queue.push(QueuedAudioFrame::Encoded(packet.clone())); }
    let opened = Arc::new(AtomicUsize::new(0));
    let submitted = Arc::new(AtomicUsize::new(0));
    let destroyed = Arc::new(AtomicUsize::new(0));
    let factory_opened = Arc::clone(&opened);
    let factory_submitted = Arc::clone(&submitted);
    let factory_destroyed = Arc::clone(&destroyed);
    let factory_queue = Arc::clone(&queue);
    let factory_stop = stop.clone();
    let (ready, mut ready_rx) = tokio::sync::mpsc::unbounded_channel();
    let open = move || -> Result<Box<dyn AudioOutput>> {
        let attempt = factory_opened.fetch_add(1, Ordering::SeqCst);
        if initial_failure && attempt == 0 { return Err(Error::Backend("模拟初始无设备".into())); }
        assert!(attempt <= 1);
        if attempt == 1 {
            assert_eq!(factory_destroyed.load(Ordering::SeqCst), usize::from(!initial_failure));
            // 模拟在设备重建期间到达的旧网络音频.
            assert!(factory_queue.push(QueuedAudioFrame::Encoded(packet.clone())));
            ready.send(()).unwrap();
        }
        Ok(Box::new(ProbeOutput {
            fail: !initial_failure && attempt == 0,
            stop: factory_stop.clone(), submitted: Arc::clone(&factory_submitted), destroyed: Arc::clone(&factory_destroyed),
        }))
    };
    let worker_queue = Arc::clone(&queue);
    let worker_stop = stop.clone();
    workers.spawn_blocking(move || run_with_retry_delay(open, worker_queue, worker_stop, &stream, Duration::from_millis(20)).map_err(Into::into));
    let task = tokio::spawn(workers.finish());
    let outcome = tokio::time::timeout(Duration::from_secs(3), async {
        ready_rx.recv().await.unwrap();
        // PLC 在恢复窗口中可以丢弃, 窗口结束后的首个 PLC 必须只触发一次播放.
        while !stop.is_cancelled() {
            queue.push(QueuedAudioFrame::Missing);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert!(outcome.is_ok());
    assert_eq!(opened.load(Ordering::SeqCst), 2);
    assert_eq!(submitted.load(Ordering::SeqCst), if initial_failure { 1 } else { 2 });
    assert_eq!(destroyed.load(Ordering::SeqCst), if initial_failure { 1 } else { 2 });
    assert!(queue.dropped() >= 1);
}

#[tokio::test]
async fn initial_device_absence_retries_and_discards_initialization_backlog() {
    exercise_recovery(true).await;
}

#[tokio::test]
async fn device_failure_destroys_old_renderer_and_decoder_before_reopening() {
    exercise_recovery(false).await;
}

#[tokio::test]
async fn no_network_retry_wait_is_cancelled_without_reopening() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let stop = CancellationToken::new();
    let mut workers = Workers::new(stop.clone());
    let queue = workers.queue("cancel-retry", 30);
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_worker = Arc::clone(&attempts);
    let (entered, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let worker_stop = stop.clone();
    workers.spawn_blocking(move || run(move || {
        attempts_worker.fetch_add(1, Ordering::SeqCst);
        entered.send(()).unwrap();
        Err(Error::Backend("无播放设备".into()))
    }, queue, worker_stop, &stream).map_err(Into::into));
    let task = tokio::spawn(workers.finish());
    tokio::time::timeout(Duration::from_secs(2), entered_rx.recv()).await.unwrap().unwrap();
    stop.cancel();
    tokio::time::timeout(Duration::from_millis(500), task).await.unwrap().unwrap().unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancellation_during_open_releases_the_new_device_without_submission() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let stop = CancellationToken::new();
    let submitted = Arc::new(AtomicUsize::new(0));
    let destroyed = Arc::new(AtomicUsize::new(0));
    let mut workers = Workers::new(stop.clone());
    let queue = workers.queue("cancel-open", 30);
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let worker_stop = stop.clone();
    let worker_submitted = Arc::clone(&submitted);
    let worker_destroyed = Arc::clone(&destroyed);
    let cancel = stop.clone();
    let mut entered = Some(entered);
    workers.spawn_blocking(move || run(move || {
        entered.take().unwrap().send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        Ok(Box::new(ProbeOutput { fail: false, stop: worker_stop.clone(),
            submitted: Arc::clone(&worker_submitted), destroyed: Arc::clone(&worker_destroyed) }))
    }, queue, stop, &stream).map_err(Into::into));
    let task = tokio::spawn(workers.finish());
    tokio::time::timeout(Duration::from_secs(2), entered_rx).await.unwrap().unwrap();
    // 通过监督器的队列关闭路径取消, 不假定系统设备打开 API 可以被强行中断.
    cancel.cancel();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert_eq!(destroyed.load(Ordering::SeqCst), 1);
    assert_eq!(submitted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn retries_are_time_based_even_without_audio_packets() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let queue = Arc::new(FrameQueue::new("retry-clock", 30));
    let closer = Arc::clone(&queue);
    let stop = CancellationToken::new();
    let worker_stop = stop.clone();
    let (attempt, mut attempts) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::task::spawn_blocking(move || run_with_retry_delay(move || {
        attempt.send(Instant::now()).unwrap();
        Err(Error::Backend("设备尚未连接".into()))
    }, queue, worker_stop, &stream, Duration::from_millis(30)));
    // 直接运行的测试也遵守监督器关闭队列以唤醒等待的契约.
    let first = tokio::time::timeout(Duration::from_secs(2), attempts.recv()).await.unwrap().unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), attempts.recv()).await.unwrap().unwrap();
    stop.cancel();
    super::super::queue::StopQueue::close(&*closer);
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert!(second.duration_since(first) >= Duration::from_millis(30));
}

struct NetworkOutput {
    fail: bool,
    stop: CancellationToken,
}

impl AudioOutput for NetworkOutput {
    fn submit_frame(&mut self, pcm: &[f32], _timeout: Duration) -> Result<()> {
        if self.fail { return Err(Error::Backend("模拟网络播放期间设备失效".into())); }
        assert!(!pcm.is_empty() && pcm.iter().all(|value| value.is_finite()));
        self.stop.cancel();
        Ok(())
    }
}

#[tokio::test]
async fn udp_reception_survives_blocked_device_recreation() {
    use super::super::{AudioChannelDirection, crypto::{AudioEncryptor, AudioDecryptor}, receive};
    use crate::audio::receiver::AudioDepacketizer;
    use crate::audio::protocol::{write_audio_packet, RtpHeader, RTP_PAYLOAD_TYPE_AUDIO};
    use tokio::net::UdpSocket;
    use std::net::{IpAddr, Ipv4Addr};

    let stream = CodecConfig::default().stream_params().unwrap();
    let mut encoder = OpusEncoder::new(stream.opus_config(), stream.bitrate).unwrap();
    let mut payload = vec![0; 1400];
    let length = encoder.encode_float(&vec![0.0; stream.samples_per_frame()], &mut payload).unwrap();
    payload.truncate(length);
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender.connect(receiver.local_addr().unwrap()).await.unwrap();
    let stop = CancellationToken::new();
    let mut workers = Workers::new(stop.clone());
    let queue = workers.queue("network-recovery", 30);
    workers.spawn(receive::receive_packets(receiver, Arc::clone(&queue), stop.clone(),
        AudioDecryptor::new([7; 32], AudioChannelDirection::HostToClient).unwrap(),
        IpAddr::V4(Ipv4Addr::LOCALHOST), AudioDepacketizer::new(5, 0)));
    let worker_queue = Arc::clone(&queue);
    let worker_stop = stop.clone();
    let factory_stop = stop.clone();
    let (opening, mut opening_rx) = tokio::sync::mpsc::unbounded_channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let mut count = 0;
    workers.spawn_blocking(move || run_with_retry_delay(move || {
        count += 1;
        assert!(count <= 2);
        if count == 2 {
            opening.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        Ok(Box::new(NetworkOutput { fail: count == 1, stop: factory_stop.clone() }))
    }, worker_queue, worker_stop, &stream, Duration::from_millis(10)).map_err(Into::into));
    let task = tokio::spawn(workers.finish());
    let mut cipher = AudioEncryptor::new([7; 32], AudioChannelDirection::HostToClient).unwrap();
    let mut encrypt_frame = |sequence_number: u16| {
        let rtp = write_audio_packet(RtpHeader { packet_type: RTP_PAYLOAD_TYPE_AUDIO, sequence_number,
            timestamp: u32::from(sequence_number) * 5, ssrc: 7 }, &payload);
        cipher.encrypt(&rtp).unwrap()
    };
    let preparation = tokio::time::timeout(Duration::from_secs(2), async {
        // 首块为 RTP 同步, 第二块触发播放错误和设备重建.
        for sequence in 0..5 { sender.send(&encrypt_frame(sequence)).await.unwrap(); }
        opening_rx.recv().await.unwrap();
        for sequence in 5..70 { sender.send(&encrypt_frame(sequence)).await.unwrap(); }
        while queue.dropped() < 30 { tokio::task::yield_now().await; }
    }).await;
    // 无论断言是否成功, 都先释放设备打开替身, 防止测试留下阻塞线程.
    release.send(()).unwrap();
    let completion = if preparation.is_ok() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut sequence = 70;
            while !stop.is_cancelled() {
                sender.send(&encrypt_frame(sequence)).await.unwrap();
                sequence += 1;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }).await
    } else { preparation };
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert!(completion.is_ok());
    assert!(queue.dropped() >= 30);
}

#[test]
fn only_backend_and_io_errors_are_recoverable() {
    assert!(recoverable(&Error::Backend("设备离线".into())));
    assert!(recoverable(&Error::Io(std::io::Error::other("设备错误"))));
    for error in [Error::InvalidConfig("无效参数"), Error::Codec("解码错误".into()),
                  Error::Protocol("协议错误".into()), Error::UnsupportedPlatform("不支持的平台")] {
        assert!(!recoverable(&error));
    }
}
