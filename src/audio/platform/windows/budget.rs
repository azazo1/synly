use super::{Error, Result, StreamParams};
use std::time::Duration;

pub(super) const MAX_PLAYBACK_WAIT: Duration = Duration::from_millis(100);

pub(super) struct QueueBudget {
    pub channels: usize,
    pub frame_samples: usize,
    // 打开设备前的容量下限, 捕获循环启动前还需加上设备实际最大单包余量.
    pub capture_samples: usize,
    pub playback_samples: usize,
    pub playback_watermark: usize,
}

impl QueueBudget {
    pub fn from_stream(stream: &StreamParams) -> Result<Self> {
        if stream.channels == 0 || stream.sample_rate == 0
            || !matches!(stream.packet_duration_ms, 5 | 10 | 20 | 40 | 60)
        {
            return Err(Error::Backend("Windows 音频流参数无效".into()));
        }
        let channels = usize::from(stream.channels);
        let sample_rate = usize::try_from(stream.sample_rate)
            .map_err(|_| Error::Backend("Windows 音频采样率不可表示".into()))?;
        let samples_for_ms = |millis: usize| -> Result<usize> {
            let scaled = sample_rate.checked_mul(millis)
                .ok_or_else(|| Error::Backend("Windows 音频帧数溢出".into()))?;
            if !scaled.is_multiple_of(1000) {
                return Err(Error::Backend("Windows 音频时长必须对应整数采样帧".into()));
            }
            (scaled / 1000).checked_mul(channels)
                .ok_or_else(|| Error::Backend("Windows 音频样本数溢出".into()))
        };
        stream.sample_rate.checked_mul(u32::from(stream.channels))
            .and_then(|value| value.checked_mul(std::mem::size_of::<f32>() as u32))
            .ok_or_else(|| Error::Backend("Windows 音频字节率溢出".into()))?;
        let frame_samples = samples_for_ms(stream.packet_duration_ms as usize)?;
        let capture_samples = samples_for_ms(30)?.max(frame_samples);
        let playback_watermark = samples_for_ms(50)?;
        let playback_samples = playback_watermark.checked_add(frame_samples)
            .ok_or_else(|| Error::Backend("Windows 播放队列容量溢出".into()))?;
        Ok(Self { channels, frame_samples, capture_samples, playback_samples, playback_watermark })
    }
}
