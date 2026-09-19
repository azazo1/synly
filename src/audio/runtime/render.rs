use super::queue::FrameQueue;
use super::AUDIO_IO_TIMEOUT;
use crate::audio::codec::OpusDecoder;
use crate::audio::config::StreamParams;
use crate::audio::error::{Error, Result};
use crate::audio::playback::AudioOutput;
use crate::audio::receiver::QueuedAudioFrame;
use super::sdl_policy;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const DEVICE_RETRY_DELAY: Duration = Duration::from_secs(1);

pub(super) fn run(
    open: impl FnMut() -> Result<Box<dyn AudioOutput>>,
    packets: Arc<FrameQueue<QueuedAudioFrame>>,
    stop: CancellationToken,
    stream: &StreamParams,
) -> Result<()> {
    run_with_retry_delay(open, packets, stop, stream, DEVICE_RETRY_DELAY)
}

fn recoverable(error: &Error) -> bool {
    matches!(error, Error::Backend(_) | Error::Io(_))
}

fn run_with_retry_delay(
    mut open: impl FnMut() -> Result<Box<dyn AudioOutput>>,
    packets: Arc<FrameQueue<QueuedAudioFrame>>,
    stop: CancellationToken,
    stream: &StreamParams,
    retry_delay: Duration,
) -> Result<()> {
    // 配置/编解码错误不应被无设备重试掩盖, 在首次打开设备前检查.
    drop(OpusDecoder::new(stream.opus_config())?);
    let mut recovering = false;
    let mut attempts = 0u64;
    loop {
        if stop.is_cancelled() { return Ok(()); }
        let started = Instant::now();
        attempts = attempts.saturating_add(1);
        let result = match open() {
            Ok(output) => {
                let opened = Instant::now();
                if stop.is_cancelled() { return Ok(()); }
                if recovering {
                    let elapsed = opened.duration_since(started);
                    tracing::info!(attempts, elapsed_ms = elapsed.as_millis(), "播放设备重建成功, 开始恢复丢帧窗口");
                    // Moonlight audio.cpp:245-249 用初始化耗时作为恢复后的丢帧时长.
                    // 同时清掉重建期间的有界积压, 不重启 UDP/AEAD/RTP 状态.
                    if !packets.discard_until(opened + elapsed) || stop.is_cancelled() { return Ok(()); }
                    tracing::info!("播放恢复丢帧窗口结束");
                }
                // 每次设备重建都新建 Opus 解码器. 函数返回前先释放旧设备和解码器.
                decode_frames(output, Arc::clone(&packets), stop.clone(), stream)
            }
            Err(error) => Err(error),
        };
        if stop.is_cancelled() { return Ok(()); }
        match result {
            Ok(()) => return Ok(()),
            Err(error) if recoverable(&error) => {
                tracing::warn!(%error, attempts, retry_ms = retry_delay.as_millis(), "播放设备不可用, 等待后重建");
            }
            Err(error) => return Err(error),
        }
        recovering = true;
        // 用单调时间替代上游每 200 个样本重试, 各种帧时长和断流时均为 1 秒.
        // 监督器取消时关闭队列, 同时唤醒没有网络输入的重试等待.
        if !packets.discard_until(Instant::now() + retry_delay) { return Ok(()); }
    }
}

pub(super) fn decode_frames(
    mut output: Box<dyn AudioOutput>,
    packets: Arc<FrameQueue<QueuedAudioFrame>>,
    stop: CancellationToken,
    stream: &StreamParams,
) -> Result<()> {
    let mut decoder = OpusDecoder::new(stream.opus_config())?;
    let mut buffer = vec![0.0; stream.samples_per_frame()];
    let mut skipped = 0u64;
    tracing::info!(frame_ms = stream.packet_duration_ms, "音频解码播放任务已启动");
    while let Some(frame) = packets.pop_blocking() {
        if stop.is_cancelled() { break; }
        let packet = match &frame {
            QueuedAudioFrame::Encoded(packet) => Some(packet.as_slice()),
            QueuedAudioFrame::Missing => None,
        };
        let decoded = match decoder.decode_float(packet, &mut buffer) {
            Ok(decoded) => decoded,
            Err(error) if packet.is_some() => {
                tracing::debug!(%error, "音频帧解码失败, 使用 Opus 丢包补偿");
                decoder.decode_float(None, &mut buffer)?
            }
            Err(error) => return Err(error),
        };
        // 保留 Moonlight SDL 的网络积压规则, 先解码推进状态再丢弃过期 PCM.
        if sdl_policy::should_drop_network_backlog(packets.len(), stream.packet_duration_ms) {
            skipped += 1;
            continue;
        }
        if stop.is_cancelled() { break; }
        output.submit_frame(&buffer[..decoded], AUDIO_IO_TIMEOUT)?;
    }
    tracing::debug!(skipped_frames = skipped, "音频解码播放统计");
    Ok(())
}

#[cfg(test)]
mod tests;
