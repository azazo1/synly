use crate::audio::capture::{CaptureStatus, open_input};
use crate::audio::codec::{OpusDecoder, OpusEncoder};
use crate::audio::config::{
    CaptureConfig, CodecConfig, DEFAULT_JITTER_BUFFER_MS, DEFAULT_REDUNDANCY_WINDOW_PACKETS,
    PlaybackConfig,
};
use crate::audio::playback::open_output;
use crate::audio::receiver::{AudioDepacketizer, QueuedAudioFrame};
use crate::audio::sender::AudioPacketizer;
use anyhow::{Context, Result, anyhow};
use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, Nonce, UnboundKey};
use ring::hkdf;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::task::JoinHandle;

const AUDIO_SOCKET_TIMEOUT: Duration = Duration::from_millis(200);
const AUDIO_AAD: &[u8] = b"synly-audio-udp-v1";
const AUDIO_COUNTER_LEN: usize = 8;

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
    let mut input = open_input(&CaptureConfig::default(), &stream).map_err(anyhow::Error::from)?;
    let mut encoder =
        OpusEncoder::new(stream.opus_config(), stream.bitrate).map_err(anyhow::Error::from)?;
    let mut packetizer = AudioPacketizer::new(
        stream.packet_duration_ms,
        rand::random::<u32>(),
        true,
        DEFAULT_REDUNDANCY_WINDOW_PACKETS,
    );
    let mut pcm_buffer = vec![0.0; stream.samples_per_frame()];
    let mut encoded_buffer = vec![0u8; 1400];
    let mut encryptor = AudioEncryptor::new(master_secret, direction)?;
    let local_addr = socket
        .local_addr()
        .context("failed to read local audio UDP sender address")?;

    println!("音频 UDP 已连接: {} -> {}", local_addr, remote_addr);

    while !stop_flag.load(Ordering::Relaxed) {
        match input
            .read_frame(&mut pcm_buffer, AUDIO_SOCKET_TIMEOUT)
            .map_err(anyhow::Error::from)?
        {
            CaptureStatus::Timeout => continue,
            CaptureStatus::Ok => {
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
        }
    }

    println!("音频 UDP 发送已停止。");
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
    let mut output =
        open_output(&PlaybackConfig::default(), &stream).map_err(anyhow::Error::from)?;
    let mut decoder = OpusDecoder::new(stream.opus_config()).map_err(anyhow::Error::from)?;
    let mut depacketizer =
        AudioDepacketizer::new(stream.packet_duration_ms, DEFAULT_JITTER_BUFFER_MS);
    let mut decode_buffer = vec![0.0; stream.samples_per_frame()];
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
        }
    }

    println!("音频 UDP 接收已停止。");
    Ok(())
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
    use super::{AudioChannelDirection, AudioDecryptor, AudioEncryptor};

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
}
