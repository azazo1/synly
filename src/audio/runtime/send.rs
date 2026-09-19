use super::crypto::AudioEncryptor;
use super::queue::FrameQueue;
use super::workers::Workers;
use super::{AUDIO_IO_TIMEOUT, AudioChannelDirection};
use crate::audio::capture::{AudioInput, open_input};
use crate::audio::codec::OpusEncoder;
use crate::audio::config::{CaptureConfig, CodecConfig, StreamParams};
use crate::audio::sender::AudioPacketizer;
use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

pub(super) async fn run(
    socket: UdpSocket,
    stop: CancellationToken,
    master_secret: [u8; 32],
    direction: AudioChannelDirection,
) -> Result<()> {
    let stream = CodecConfig::default().stream_params()?;
    let capture_stream = stream.clone();
    run_with_input(socket, stop, master_secret, direction, stream, move || {
        open_input(&CaptureConfig::default(), &capture_stream)
    }).await
}

pub(super) async fn run_with_input(
    socket: UdpSocket,
    stop: CancellationToken,
    master_secret: [u8; 32],
    direction: AudioChannelDirection,
    stream: StreamParams,
    open: impl FnMut() -> crate::audio::error::Result<Box<dyn AudioInput>> + Send + 'static,
) -> Result<()> {
    let mut workers = Workers::new(stop.clone());
    // Sunshine audio::capture 使用 30 帧, mail::audio_packets 使用默认 32 包队列.
    let samples = workers.queue("capture", 30);
    let packets = workers.queue("encode", 32);
    let capture_queue = Arc::clone(&samples);
    let capture_stop = stop.clone();
    let capture_stream = stream.clone();
    workers.spawn_blocking(move || {
        super::capture::run(open, capture_queue, capture_stop, &capture_stream).map_err(Into::into)
    });
    let encode_queue = Arc::clone(&packets);
    let encode_stop = stop.clone();
    let encode_stream = stream.clone();
    workers.spawn_blocking(move || encode_frames(samples, encode_queue, encode_stop, &encode_stream));
    workers.spawn(send_packets(socket, packets, stop, master_secret, direction, stream.packet_duration_ms));
    let result = workers.finish().await;
    tracing::info!(success = result.is_ok(), "音频发送链路已停止");
    result
}

fn encode_frames(
    samples: Arc<FrameQueue<Vec<f32>>>,
    packets: Arc<FrameQueue<Vec<u8>>>,
    stop: CancellationToken,
    stream: &StreamParams,
) -> Result<()> {
    let mut encoder = OpusEncoder::new(stream.opus_config(), stream.bitrate)?;
    let mut encoded = vec![0u8; 1400];
    tracing::info!(bitrate = stream.bitrate, frame_ms = stream.packet_duration_ms, "Opus 编码任务已启动");
    while let Some(frame) = samples.pop_blocking() {
        if stop.is_cancelled() { break; }
        let size = encoder.encode_float(&frame, &mut encoded)?;
        if !packets.push(encoded[..size].to_vec()) { break; }
    }
    Ok(())
}

async fn send_packets(
    socket: UdpSocket,
    packets: Arc<FrameQueue<Vec<u8>>>,
    stop: CancellationToken,
    master_secret: [u8; 32],
    direction: AudioChannelDirection,
    packet_duration_ms: u32,
) -> Result<()> {
    let mut packetizer = AudioPacketizer::new(packet_duration_ms, rand::random(), true);
    let mut encryptor = AudioEncryptor::new(master_secret, direction)?;
    tracing::info!(local_addr = %socket.local_addr()?, remote_addr = %socket.peer_addr()?, "音频 UDP 发送端已连接");
    let mut sent = 0u64;
    while let Some(packet) = packets.pop().await {
        if stop.is_cancelled() { break; }
        for datagram in packetizer.push_encoded_frame(&packet)? {
            let encrypted = encryptor.encrypt(&datagram.bytes)?;
            tokio::select! {
                _ = stop.cancelled() => return Ok(()),
                result = tokio::time::timeout(AUDIO_IO_TIMEOUT, socket.send(&encrypted)) => {
                    result.context("发送音频 UDP 超时")?.context("发送音频 UDP 失败")?;
                    sent += 1;
                }
            }
        }
    }
    tracing::debug!(datagrams = sent, "音频 UDP 发送统计");
    Ok(())
}

#[cfg(test)]
mod tests;
