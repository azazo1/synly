use super::*;
use super::super::crypto::AudioDecryptor;
use crate::audio::capture::CaptureStatus;
use crate::audio::error::{Error, Result as AudioResult};
use crate::audio::protocol::{ParsedPacket, parse_datagram};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct RecoveringInput {
    frames_left: usize,
    fails: bool,
    destroyed: Arc<AtomicUsize>,
}

impl Drop for RecoveringInput {
    fn drop(&mut self) { self.destroyed.fetch_add(1, Ordering::SeqCst); }
}

impl AudioInput for RecoveringInput {
    fn read_frame(&mut self, frame: &mut [f32], timeout: Duration) -> AudioResult<CaptureStatus> {
        if self.frames_left == 0 {
            if self.fails {
                frame.fill(f32::NAN);
                return Err(Error::Backend("模拟捕获设备断开".into()));
            }
            std::thread::sleep(timeout);
            return Ok(CaptureStatus::Timeout);
        }
        self.frames_left -= 1;
        frame.fill(0.1);
        Ok(CaptureStatus::Ok)
    }
}

#[tokio::test]
async fn capture_recovery_preserves_encoder_rtp_fec_and_aead_counter() {
    let stream = CodecConfig::default().stream_params().unwrap();
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sender.connect(receiver.local_addr().unwrap()).await.unwrap();
    let stop = CancellationToken::new();
    let attempts = Arc::new(AtomicUsize::new(0));
    let destroyed = Arc::new(AtomicUsize::new(0));
    let factory_attempts = Arc::clone(&attempts);
    let factory_destroyed = Arc::clone(&destroyed);
    let started = Instant::now();
    let task = tokio::spawn(run_with_input(sender, stop.clone(), [13; 32], AudioChannelDirection::HostToClient,
        stream.clone(), move || {
            let attempt = factory_attempts.fetch_add(1, Ordering::SeqCst);
            assert!(attempt <= 1);
            if attempt == 1 {
                assert_eq!(factory_destroyed.load(Ordering::SeqCst), 1);
                assert!(started.elapsed() >= Duration::from_secs(5));
            }
            Ok(Box::new(RecoveringInput { frames_left: if attempt == 0 { 3 } else { 2 },
                fails: attempt == 0, destroyed: Arc::clone(&factory_destroyed) }))
        }));
    let mut decryptor = AudioDecryptor::new([13; 32], AudioChannelDirection::HostToClient).unwrap();
    let mut reference = OpusEncoder::new(stream.opus_config(), stream.bitrate).unwrap();
    let mut expected = [0u8; 1400];
    let mut buffer = [0u8; 2048];
    let mut audio = 0u16;
    let mut fec = 0;
    let mut ssrc = None;
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        // 第一个 FEC 块在捕获失败前只有 3 帧, 恢复后的第 4 帧应补齐同一块.
        for counter in 0..7u64 {
            let size = receiver.recv(&mut buffer).await.unwrap();
            assert_eq!(u64::from_be_bytes(buffer[..8].try_into().unwrap()), counter);
            let packet = decryptor.decrypt(&buffer[..size]).unwrap();
            match parse_datagram(&packet).unwrap() {
                ParsedPacket::Audio { rtp, payload } => {
                    assert_eq!(rtp.sequence_number, audio);
                    assert_eq!(rtp.timestamp, u32::from(audio) * 5);
                    assert_eq!(*ssrc.get_or_insert(rtp.ssrc), rtp.ssrc);
                    // 连续编码的逐字节参考同时检测恢复时误重建 Opus encoder.
                    let size = reference.encode_float(&vec![0.1; stream.samples_per_frame()], &mut expected).unwrap();
                    assert_eq!(payload, expected[..size]);
                    audio += 1;
                }
                ParsedPacket::Fec { .. } => {
                    assert_eq!(audio, 4);
                    fec += 1;
                }
            }
        }
    }).await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), task).await.unwrap().unwrap().unwrap();
    assert!(result.is_ok());
    assert_eq!((audio, fec), (5, 2));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(destroyed.load(Ordering::SeqCst), 2);
}
