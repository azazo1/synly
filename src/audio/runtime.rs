use crate::audio::capture::{CaptureStatus, open_input};
use crate::audio::codec::{OpusDecoder, OpusEncoder};
use crate::audio::config::{CaptureConfig, CodecConfig, DEFAULT_INITIAL_DROP_MS, PlaybackConfig};
use crate::audio::playback::open_output;
use crate::audio::protocol::RTPA_DATA_SHARDS;
use crate::audio::receiver::{AudioDepacketizer, QueuedAudioFrame};
use crate::audio::sender::AudioPacketizer;
use anyhow::{Context, Result, anyhow};
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle as StdJoinHandle};
use std::time::Duration;
use tokio::task::JoinHandle;

const AUDIO_SOCKET_TIMEOUT: Duration = Duration::from_millis(200);
const AUDIO_AAD: &[u8] = b"synly-audio-udp-v1";
const AUDIO_COUNTER_LEN: usize = 8;
const AUDIO_SAMPLE_QUEUE_LEN: usize = 30;
const AUDIO_PLAYBACK_PREBUFFER_MS: u32 = 30;
const AUDIO_PLAYBACK_QUEUE_MS: u32 = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioChannelDirection {
    HostToClient,
    ClientToHost,
}

impl AudioChannelDirection {
    fn as_label(self) -> &'static [u8] {
        match self {
            Self::HostToClient => b"host-to-client",
            Self::ClientToHost => b"client-to-host",
        }
    }
}

pub struct AudioTaskHandle {
    stop_flag: Arc<AtomicBool>,
    task: JoinHandle<Result<()>>,
}

impl AudioTaskHandle {
    pub async fn stop(self) -> Result<()> {
        self.stop_flag.store(true, Ordering::Relaxed);
        match self.task.await {
            Ok(result) => result,
            Err(err) => Err(err.into()),
        }
    }
}

pub fn bind_and_spawn_receiver(
    master_secret: [u8; 32],
    direction: AudioChannelDirection,
    expected_peer_ip: IpAddr,
) -> Result<(AudioTaskHandle, u16)> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).context("failed to bind audio UDP receiver")?;
    socket
        .set_read_timeout(Some(AUDIO_SOCKET_TIMEOUT))
        .context("failed to configure audio UDP receiver timeout")?;
    let local_port = socket
        .local_addr()
        .context("failed to read local audio UDP receiver address")?
        .port();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let task_stop_flag = Arc::clone(&stop_flag);
    let task = tokio::task::spawn_blocking(move || {
        run_receiver_loop(
            socket,
            task_stop_flag,
            master_secret,
            direction,
            expected_peer_ip,
        )
    });
    Ok((AudioTaskHandle { stop_flag, task }, local_port))
}

pub fn spawn_sender(
    master_secret: [u8; 32],
    direction: AudioChannelDirection,
    remote_addr: SocketAddr,
) -> Result<AudioTaskHandle> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).context("failed to bind audio UDP sender")?;
    socket
        .connect(remote_addr)
        .with_context(|| format!("failed to connect audio UDP sender to {remote_addr}"))?;
    socket
        .set_write_timeout(Some(AUDIO_SOCKET_TIMEOUT))
        .context("failed to configure audio UDP sender timeout")?;
    let stop_flag = Arc::new(AtomicBool::new(false));
    let task_stop_flag = Arc::clone(&stop_flag);
    let task = tokio::task::spawn_blocking(move || {
        run_sender_loop(
            socket,
            task_stop_flag,
            master_secret,
            direction,
            remote_addr,
        )
    });
    Ok(AudioTaskHandle { stop_flag, task })
}

fn run_sender_loop(
    socket: UdpSocket,
    stop_flag: Arc<AtomicBool>,
    master_secret: [u8; 32],
    direction: AudioChannelDirection,
    remote_addr: SocketAddr,
) -> Result<()> {
    let codec = CodecConfig::default();
    let stream = codec.stream_params().map_err(anyhow::Error::from)?;
    let capture = AudioCaptureWorker::start(Arc::clone(&stop_flag), stream.clone())?;
    let mut encoder =
        OpusEncoder::new(stream.opus_config(), stream.bitrate).map_err(anyhow::Error::from)?;
    let mut packetizer =
        AudioPacketizer::new(stream.packet_duration_ms, rand::random::<u32>(), true);
    let mut encoded_buffer = vec![0u8; 1400];
    let mut encryptor = AudioEncryptor::new(master_secret, direction)?;
    let local_addr = socket
        .local_addr()
        .context("failed to read local audio UDP sender address")?;

    println!("音频 UDP 已连接: {} -> {}", local_addr, remote_addr);

    let loop_result = (|| -> Result<()> {
        while !stop_flag.load(Ordering::Relaxed) {
            let pcm_buffer = match capture.recv() {
                Ok(frame) => frame,
                Err(_) if stop_flag.load(Ordering::Relaxed) => break,
                Err(err) => return Err(err),
            };

            let encoded = encoder
                .encode_float(&pcm_buffer, &mut encoded_buffer)
                .map_err(anyhow::Error::from)?;
            let datagrams = packetizer
                .push_encoded_frame(&encoded_buffer[..encoded])
                .map_err(anyhow::Error::from)?;
            for datagram in datagrams {
                if stop_flag.load(Ordering::Relaxed) {
                    break;
                }
                let encrypted = encryptor.encrypt(&datagram.bytes)?;
                socket.send(&encrypted).with_context(|| {
                    format!("failed to send encrypted audio UDP packet to {remote_addr}")
                })?;
            }
        }

        Ok(())
    })();

    let capture_result = capture.finish();
    match (loop_result, capture_result) {
        (Err(err), _) => Err(err),
        (Ok(()), Err(err)) => Err(err),
        (Ok(()), Ok(())) => {
            println!("音频 UDP 发送已停止。");
            Ok(())
        }
    }
}

struct AudioCaptureWorker {
    rx: Receiver<Vec<f32>>,
    thread: Option<StdJoinHandle<Result<()>>>,
}

impl AudioCaptureWorker {
    fn start(
        stop_flag: Arc<AtomicBool>,
        stream: crate::audio::config::StreamParams,
    ) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel(AUDIO_SAMPLE_QUEUE_LEN);
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("synly-audio-capture".into())
            .spawn(move || capture_worker_main(stop_flag, stream, tx, ready_tx))
            .map_err(|err| anyhow!("failed to spawn audio capture worker: {err}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                rx,
                thread: Some(thread),
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(anyhow!(message))
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!(
                    "audio capture worker exited before startup completed"
                ))
            }
        }
    }

    fn recv(&self) -> Result<Vec<f32>> {
        self.rx
            .recv()
            .map_err(|_| anyhow!("audio capture worker exited unexpectedly"))
    }

    fn finish(mut self) -> Result<()> {
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(result) => result,
                Err(_) => Err(anyhow!("audio capture worker panicked")),
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for AudioCaptureWorker {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn capture_worker_main(
    stop_flag: Arc<AtomicBool>,
    stream: crate::audio::config::StreamParams,
    tx: SyncSender<Vec<f32>>,
    ready_tx: mpsc::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let capture_config = CaptureConfig {
        continuous_audio: true,
        ..CaptureConfig::default()
    };
    let mut input = match open_input(&capture_config, &stream) {
        Ok(input) => input,
        Err(err) => {
            let message = err.to_string();
            let _ = ready_tx.send(Err(message.clone()));
            return Err(anyhow!(message));
        }
    };
    let capture_timeout = capture_wait_duration(stream.packet_duration_ms);
    let timeout_frames = capture_timeout_frames(stream.packet_duration_ms);
    let frame_samples = stream.samples_per_frame();
    let silence_frame = vec![0.0; frame_samples];

    let _ = ready_tx.send(Ok(()));

    while !stop_flag.load(Ordering::Relaxed) {
        let mut frame = vec![0.0; frame_samples];
        match input
            .read_frame(&mut frame, capture_timeout)
            .map_err(anyhow::Error::from)?
        {
            CaptureStatus::Ok => {
                tx.send(frame)
                    .map_err(|_| anyhow!("audio capture queue has been closed"))?;
            }
            CaptureStatus::Timeout => {
                // Mirror Sunshine's continuous-audio behavior by inserting silence
                // for each frame interval that elapsed without captured samples.
                for _ in 0..timeout_frames {
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }
                    tx.send(silence_frame.clone())
                        .map_err(|_| anyhow!("audio capture queue has been closed"))?;
                }
            }
        }
    }

    Ok(())
}

fn run_receiver_loop(
    socket: UdpSocket,
    stop_flag: Arc<AtomicBool>,
    master_secret: [u8; 32],
    direction: AudioChannelDirection,
    expected_peer_ip: IpAddr,
) -> Result<()> {
    let codec = CodecConfig::default();
    let stream = codec.stream_params().map_err(anyhow::Error::from)?;
    let playback = AudioPlaybackWorker::start(stream.clone())?;
    let mut depacketizer =
        AudioDepacketizer::new(stream.packet_duration_ms, DEFAULT_INITIAL_DROP_MS);
    let mut read_buffer = vec![0u8; 2048];
    let decryptor = AudioDecryptor::new(master_secret, direction)?;
    let local_addr = socket
        .local_addr()
        .context("failed to read local audio UDP receiver address")?;
    let mut bound_peer = None;

    println!(
        "音频 UDP 已监听: {}，等待 {} 的加密音频流。",
        local_addr, expected_peer_ip
    );

    let loop_result = (|| -> Result<()> {
        while !stop_flag.load(Ordering::Relaxed) {
            let (packet_len, remote_addr) = match socket.recv_from(&mut read_buffer) {
                Ok(result) => result,
                Err(err)
                    if err.kind() == std::io::ErrorKind::WouldBlock
                        || err.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(err) => return Err(err).context("failed to receive audio UDP packet"),
            };

            if remote_addr.ip() != expected_peer_ip {
                continue;
            }
            if let Some(bound_peer) = bound_peer
                && bound_peer != remote_addr
            {
                continue;
            }

            let packet = match decryptor.decrypt(&read_buffer[..packet_len]) {
                Ok(packet) => packet,
                Err(_) => continue,
            };

            if bound_peer.is_none() {
                bound_peer = Some(remote_addr);
                println!("音频 UDP 已连接: {} -> {}", remote_addr, local_addr);
            }

            let ready = depacketizer
                .push_datagram(&packet)
                .map_err(anyhow::Error::from)?;
            for frame in ready {
                playback.submit(frame)?;
            }
        }

        Ok(())
    })();

    let playback_result = playback.finish();
    match (loop_result, playback_result) {
        (Err(err), _) => Err(err),
        (Ok(()), Err(err)) => Err(err),
        (Ok(()), Ok(())) => {
            println!("音频 UDP 接收已停止。");
            Ok(())
        }
    }
}

struct AudioPlaybackWorker {
    tx: Option<SyncSender<QueuedAudioFrame>>,
    thread: Option<StdJoinHandle<Result<()>>>,
}

impl AudioPlaybackWorker {
    fn start(stream: crate::audio::config::StreamParams) -> Result<Self> {
        let queue_capacity =
            buffered_audio_frame_count(stream.packet_duration_ms, AUDIO_PLAYBACK_QUEUE_MS)
                .max(playback_prebuffer_frames(stream.packet_duration_ms) * 2);
        let (tx, rx) = mpsc::sync_channel(queue_capacity);
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("synly-audio-playback".into())
            .spawn(move || playback_worker_main(rx, stream, ready_tx))
            .map_err(|err| anyhow!("failed to spawn audio playback worker: {err}"))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx: Some(tx),
                thread: Some(thread),
            }),
            Ok(Err(message)) => {
                let _ = thread.join();
                Err(anyhow!(message))
            }
            Err(_) => {
                let _ = thread.join();
                Err(anyhow!(
                    "audio playback worker exited before startup completed"
                ))
            }
        }
    }

    fn submit(&self, frame: QueuedAudioFrame) -> Result<()> {
        self.tx
            .as_ref()
            .ok_or_else(|| anyhow!("audio playback worker has already stopped"))?
            .send(frame)
            .map_err(|_| anyhow!("audio playback worker exited unexpectedly"))
    }

    fn finish(mut self) -> Result<()> {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(result) => result,
                Err(_) => Err(anyhow!("audio playback worker panicked")),
            }
        } else {
            Ok(())
        }
    }
}

impl Drop for AudioPlaybackWorker {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn playback_worker_main(
    rx: Receiver<QueuedAudioFrame>,
    stream: crate::audio::config::StreamParams,
    ready_tx: mpsc::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let mut output = match open_output(&PlaybackConfig::default(), &stream) {
        Ok(output) => output,
        Err(err) => {
            let message = err.to_string();
            let _ = ready_tx.send(Err(message.clone()));
            return Err(anyhow!(message));
        }
    };
    let mut decoder = match OpusDecoder::new(stream.opus_config()) {
        Ok(decoder) => decoder,
        Err(err) => {
            let message = err.to_string();
            let _ = ready_tx.send(Err(message.clone()));
            return Err(anyhow!(message));
        }
    };
    let mut decode_buffer = vec![0.0; stream.samples_per_frame()];
    let prebuffer_frames = playback_prebuffer_frames(stream.packet_duration_ms);
    let mut pending = VecDeque::new();
    // Keep a small prebuffer so packet arrival jitter does not turn into per-packet zero-fills.
    let mut buffering = true;
    let mut disconnected = false;

    let _ = ready_tx.send(Ok(()));

    loop {
        if buffering && !disconnected {
            while pending.len() < prebuffer_frames {
                match rx.recv_timeout(AUDIO_SOCKET_TIMEOUT) {
                    Ok(frame) => pending.push_back(frame),
                    Err(RecvTimeoutError::Timeout) => {
                        if pending.is_empty() {
                            continue;
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }

            if !disconnected && pending.len() < prebuffer_frames {
                continue;
            }
            buffering = false;
        } else if !disconnected {
            while let Ok(frame) = rx.try_recv() {
                pending.push_back(frame);
            }
        }

        let Some(frame) = pending.pop_front() else {
            if disconnected {
                return Ok(());
            }
            buffering = true;
            continue;
        };

        let decoded = match frame {
            QueuedAudioFrame::Encoded(packet) => decoder
                .decode_float(Some(&packet), &mut decode_buffer)
                .map_err(anyhow::Error::from)?,
            QueuedAudioFrame::Missing => decoder
                .decode_float(None, &mut decode_buffer)
                .map_err(anyhow::Error::from)?,
        };

        output
            .submit_frame(&decode_buffer[..decoded], AUDIO_SOCKET_TIMEOUT)
            .map_err(anyhow::Error::from)?;

        if pending.is_empty() && !disconnected {
            buffering = true;
        }
    }
}

fn playback_prebuffer_frames(packet_duration_ms: u32) -> usize {
    buffered_audio_frame_count(packet_duration_ms, AUDIO_PLAYBACK_PREBUFFER_MS).max(2)
}

fn buffered_audio_frame_count(packet_duration_ms: u32, target_ms: u32) -> usize {
    let packet_duration_ms = packet_duration_ms.max(1);
    target_ms.div_ceil(packet_duration_ms) as usize
}

fn capture_wait_duration(packet_duration_ms: u32) -> Duration {
    Duration::from_millis((packet_duration_ms.max(1) * RTPA_DATA_SHARDS as u32).max(10) as u64)
}

fn capture_timeout_frames(packet_duration_ms: u32) -> usize {
    let capture_wait_ms = capture_wait_duration(packet_duration_ms).as_millis() as u32;
    buffered_audio_frame_count(packet_duration_ms, capture_wait_ms)
}

struct AudioEncryptor {
    key: LessSafeKey,
    nonce_prefix: [u8; 4],
    counter: u64,
}

impl AudioEncryptor {
    fn new(master_secret: [u8; 32], direction: AudioChannelDirection) -> Result<Self> {
        let (key, nonce_prefix) = derive_directional_key(master_secret, direction)?;
        Ok(Self {
            key,
            nonce_prefix,
            counter: 0,
        })
    }

    fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let counter = self.counter;
        self.counter = self.counter.wrapping_add(1);
        let mut in_out = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(self.nonce(counter), Aad::from(AUDIO_AAD), &mut in_out)
            .map_err(|_| anyhow!("failed to encrypt audio UDP packet"))?;
        let mut packet = Vec::with_capacity(AUDIO_COUNTER_LEN + in_out.len());
        packet.extend_from_slice(&counter.to_be_bytes());
        packet.extend_from_slice(&in_out);
        Ok(packet)
    }

    fn nonce(&self, counter: u64) -> Nonce {
        build_nonce(self.nonce_prefix, counter)
    }
}

struct AudioDecryptor {
    key: LessSafeKey,
    nonce_prefix: [u8; 4],
}

impl AudioDecryptor {
    fn new(master_secret: [u8; 32], direction: AudioChannelDirection) -> Result<Self> {
        let (key, nonce_prefix) = derive_directional_key(master_secret, direction)?;
        Ok(Self { key, nonce_prefix })
    }

    fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        if ciphertext.len() <= AUDIO_COUNTER_LEN {
            return Err(anyhow!("encrypted audio UDP packet is too short"));
        }
        let (counter_bytes, body) = ciphertext.split_at(AUDIO_COUNTER_LEN);
        let counter = u64::from_be_bytes(
            counter_bytes
                .try_into()
                .map_err(|_| anyhow!("invalid audio UDP packet counter"))?,
        );
        let mut in_out = body.to_vec();
        let plaintext = self
            .key
            .open_in_place(
                build_nonce(self.nonce_prefix, counter),
                Aad::from(AUDIO_AAD),
                &mut in_out,
            )
            .map_err(|_| anyhow!("failed to decrypt audio UDP packet"))?;
        Ok(plaintext.to_vec())
    }
}

fn derive_directional_key(
    master_secret: [u8; 32],
    direction: AudioChannelDirection,
) -> Result<(LessSafeKey, [u8; 4])> {
    let salt = hkdf::Salt::new(hkdf::HKDF_SHA256, b"synly-audio-udp-key");
    let prk = salt.extract(&master_secret);
    let key_bytes = hkdf_expand::<32>(&prk, &[b"key", direction.as_label()])?;
    let nonce_prefix = hkdf_expand::<4>(&prk, &[b"nonce", direction.as_label()])?;
    let key = LessSafeKey::new(
        UnboundKey::new(&CHACHA20_POLY1305, &key_bytes)
            .map_err(|_| anyhow!("failed to initialize audio AEAD key"))?,
    );
    Ok((key, nonce_prefix))
}

fn build_nonce(prefix: [u8; 4], counter: u64) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[..4].copy_from_slice(&prefix);
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::assume_unique_for_key(nonce)
}

fn hkdf_expand<const N: usize>(prk: &hkdf::Prk, info: &[&[u8]]) -> Result<[u8; N]> {
    let mut output = [0u8; N];
    prk.expand(info, HkdfLen(N))
        .map_err(|_| anyhow!("failed to expand audio HKDF output"))?
        .fill(&mut output)
        .map_err(|_| anyhow!("failed to fill audio HKDF output"))?;
    Ok(output)
}

#[derive(Clone, Copy)]
struct HkdfLen(usize);

impl hkdf::KeyType for HkdfLen {
    fn len(&self) -> usize {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AudioChannelDirection, AudioDecryptor, AudioEncryptor, buffered_audio_frame_count,
        capture_timeout_frames, playback_prebuffer_frames,
    };

    #[test]
    fn audio_crypto_roundtrip_preserves_payload() {
        let secret = [7u8; 32];
        let mut sender = AudioEncryptor::new(secret, AudioChannelDirection::HostToClient).unwrap();
        let receiver = AudioDecryptor::new(secret, AudioChannelDirection::HostToClient).unwrap();
        let payload = b"rtp-packet".to_vec();

        let packet = sender.encrypt(&payload).unwrap();
        let decoded = receiver.decrypt(&packet).unwrap();

        assert_eq!(decoded, payload);
    }

    #[test]
    fn playback_prebuffer_rounds_up_to_whole_packets() {
        assert_eq!(buffered_audio_frame_count(5, 30), 6);
        assert_eq!(buffered_audio_frame_count(7, 30), 5);
        assert_eq!(playback_prebuffer_frames(50), 2);
    }

    #[test]
    fn capture_timeout_expands_to_whole_frame_burst() {
        assert_eq!(capture_timeout_frames(5), 4);
        assert_eq!(capture_timeout_frames(7), 4);
        assert_eq!(capture_timeout_frames(20), 4);
    }
}
