use super::queue::FrameQueue;
use super::AUDIO_IO_TIMEOUT;
use crate::audio::capture::{AudioInput, CaptureStatus};
use crate::audio::config::StreamParams;
use crate::audio::error::{Error, Result};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const DEVICE_RETRY_DELAY: Duration = Duration::from_secs(5);

pub(super) fn run(
    open: impl FnMut() -> Result<Box<dyn AudioInput>>,
    samples: Arc<FrameQueue<Vec<f32>>>,
    stop: CancellationToken,
    stream: &StreamParams,
) -> Result<()> {
    run_with_retry_delay(open, samples, stop, stream, DEVICE_RETRY_DELAY)
}

fn run_with_retry_delay(
    mut open: impl FnMut() -> Result<Box<dyn AudioInput>>,
    samples: Arc<FrameQueue<Vec<f32>>>,
    stop: CancellationToken,
    stream: &StreamParams,
    retry_delay: Duration,
) -> Result<()> {
    let mut attempts = 0u64;
    loop {
        if stop.is_cancelled() { return Ok(()); }
        attempts = attempts.saturating_add(1);
        let started = Instant::now();
        let result = match open() {
            Ok(input) => {
                if stop.is_cancelled() { return Ok(()); }
                tracing::info!(attempts, elapsed_ms = started.elapsed().as_millis(), "系统音频捕获设备已就绪");
                capture_frames(input, &samples, &stop, stream)
            }
            Err(error) => Err(error),
        };
        if stop.is_cancelled() { return Ok(()); }
        match result {
            Ok(()) => return Ok(()),
            Err(error @ (Error::Backend(_) | Error::Io(_))) => {
                tracing::warn!(%error, attempts, retry_ms = retry_delay.as_millis(), "系统音频捕获不可用, 等待后重建");
            }
            Err(error) => return Err(error),
        }
        // Sunshine audio.cpp:258-267 保留编码线程, 只重建 microphone.
        // synly 初次打开失败也重试, 且读取错误后也退避, 防止设备反复失效时忙循环.
        // 不把 samples 当作第二个消费者: 编码任务继续处理已完整捕获的帧.
        // 本函数只在 Tokio blocking worker 中调用, 定时器和取消由异步运行时驱动.
        let cancelled = tokio::runtime::Handle::current().block_on(async {
            tokio::select! {
                biased;
                _ = stop.cancelled() => true,
                _ = tokio::time::sleep(retry_delay) => false,
            }
        });
        if cancelled { return Ok(()); }
    }
}

fn capture_frames(
    mut input: Box<dyn AudioInput>,
    samples: &FrameQueue<Vec<f32>>,
    stop: &CancellationToken,
    stream: &StreamParams,
) -> Result<()> {
    // 超时可复用缓冲. 失败或不完整的帧不交给编码任务, 重建后另起新缓冲.
    let mut frame = vec![0.0; stream.samples_per_frame()];
    while !stop.is_cancelled() {
        match input.read_frame(&mut frame, AUDIO_IO_TIMEOUT)? {
            CaptureStatus::Timeout => continue,
            CaptureStatus::Ok => {
                if stop.is_cancelled() || !samples.push(frame) { return Ok(()); }
                frame = vec![0.0; stream.samples_per_frame()];
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
