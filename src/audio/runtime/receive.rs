use super::crypto::AudioDecryptor;
use super::queue::FrameQueue;
use super::workers::Workers;
use super::{AUDIO_IO_TIMEOUT, AudioChannelDirection};
use crate::audio::config::{CodecConfig, DEFAULT_INITIAL_DROP_MS, PlaybackConfig, StreamParams};
use crate::audio::playback::{AudioOutput, open_output};
use crate::audio::receiver::{AudioDepacketizer, QueuedAudioFrame};
use anyhow::{Context, Result};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

pub(super) async fn run(
    socket: UdpSocket,
    stop: CancellationToken,
    master_secret: [u8; 32],
    direction: AudioChannelDirection,
    expected_peer_ip: IpAddr,
) -> Result<()> {
    let stream = CodecConfig::default().stream_params()?;
    let playback_stream = stream.clone();
    run_with_output(socket, stop, master_secret, direction, expected_peer_ip, stream, move || {
        open_output(&PlaybackConfig::default(), &playback_stream)
    }).await
}

pub(super) async fn run_with_output(
    socket: UdpSocket,
    stop: CancellationToken,
    master_secret: [u8; 32],
    direction: AudioChannelDirection,
    expected_peer_ip: IpAddr,
    stream: StreamParams,
    open: impl FnMut() -> crate::audio::error::Result<Box<dyn AudioOutput>> + Send + 'static,
) -> Result<()> {
    let decryptor = AudioDecryptor::new(master_secret, direction)?;
    let depacketizer = AudioDepacketizer::new(stream.packet_duration_ms, DEFAULT_INITIAL_DROP_MS);
    let mut workers = Workers::new(stop.clone());
    // Moonlight initializeAudioStream 的解码队列上限为 30 包.
    let packets = workers.queue("decode", 30);
    let decode_queue = Arc::clone(&packets);
    let decode_stop = stop.clone();
    workers.spawn_blocking(move || {
        super::render::run(open, decode_queue, decode_stop, &stream).map_err(Into::into)
    });
    workers.spawn(receive_packets(socket, packets, stop, decryptor, expected_peer_ip, depacketizer));
    let result = workers.finish().await;
    tracing::info!(success = result.is_ok(), "音频接收链路已停止");
    result
}

pub(super) async fn receive_packets(
    socket: UdpSocket,
    packets: Arc<FrameQueue<QueuedAudioFrame>>,
    stop: CancellationToken,
    mut decryptor: AudioDecryptor,
    expected_peer_ip: IpAddr,
    mut depacketizer: AudioDepacketizer,
) -> Result<()> {
    let mut read_buffer = [0u8; 2048];
    let local_addr = socket.local_addr()?;
    let mut bound_peer: Option<SocketAddr> = None;
    let mut received = 0u64;
    let mut invalid = 0u64;
    tracing::info!(%local_addr, %expected_peer_ip, "音频 UDP 接收端已监听");
    loop {
        let received_packet = tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            result = tokio::time::timeout(AUDIO_IO_TIMEOUT, socket.recv_from(&mut read_buffer)) => result,
        };
        let ready = match received_packet {
            Err(_) => depacketizer.receive_timeout(),
            Ok(Err(error)) => return Err(error).context("接收音频 UDP 失败"),
            Ok(Ok((packet_len, remote_addr))) => {
                if remote_addr.ip() != expected_peer_ip || bound_peer.is_some_and(|peer| peer != remote_addr) {
                    continue;
                }
                let packet = match decryptor.decrypt(&read_buffer[..packet_len]) {
                    Ok(packet) => packet,
                    Err(_) => { invalid += 1; continue; }
                };
                let ready = match depacketizer.push_datagram(&packet) {
                    Ok(ready) => ready,
                    Err(error) => {
                        invalid += 1;
                        tracing::debug!(%error, "丢弃无效音频数据包");
                        continue;
                    }
                };
                if bound_peer.is_none() {
                    bound_peer = Some(remote_addr);
                    tracing::info!(%remote_addr, %local_addr, "音频 UDP 接收端已连接");
                }
                received += 1;
                ready
            }
        };
        for frame in ready {
            if !packets.push(frame) { return Ok(()); }
        }
    }
    tracing::debug!(received, invalid, "音频 UDP 接收统计");
    Ok(())
}
