//! Moonlight SDL renderer 的可移植播放策略.
//!
//! 这里仅保存 renderer 与网络积压之间的纯策略, 不包含 SDL 或系统设备句柄.
//! WASAPI/AudioQueue 后端可以复用阈值, 设备水位和错误语义仍由平台实现负责.

pub(super) const MAX_PENDING_AUDIO_MS: usize = 30;

pub(super) fn should_drop_network_backlog(pending_frames: usize, packet_duration_ms: u32) -> bool {
    pending_frames.saturating_mul(packet_duration_ms as usize) > MAX_PENDING_AUDIO_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_drop_starts_after_thirty_ms() {
        assert!(!should_drop_network_backlog(6, 5));
        assert!(should_drop_network_backlog(7, 5));
        assert!(!should_drop_network_backlog(3, 10));
        assert!(should_drop_network_backlog(4, 10));
        assert!(!should_drop_network_backlog(0, u32::MAX));
    }
}
