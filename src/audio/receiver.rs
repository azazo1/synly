// 接收状态机移植自 moonlight-common-c/src/RtpAudioQueue.c.
// 上游版本与对应函数见 docs/audio-port.md.
#[cfg(test)]
mod tests;
#[cfg(test)]
mod upstream_tests;

use crate::audio::error::{Error, Result};
use crate::audio::fec;
use crate::audio::protocol::{
    self, AudioFecHeader, OOS_WAIT_TIME_MS, ParsedPacket, RTP_PAYLOAD_TYPE_AUDIO, RTPA_DATA_SHARDS,
    RTPA_FEC_SHARDS, parse_datagram,
};
use std::array;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Default)]
pub struct RtpAudioStats {
    pub packet_count_audio: u64,
    pub packet_count_invalid: u64,
    pub packet_count_fec: u64,
    pub packet_count_fec_invalid: u64,
    pub packet_count_fec_recovered: u64,
    pub packet_count_fec_failed: u64,
    pub packet_count_oos: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueuedAudioFrame {
    Encoded(Vec<u8>),
    Missing,
}

#[derive(Debug)]
struct FecBlock {
    fec_header: AudioFecHeader,
    block_size: usize,
    data_shards: [Option<Vec<u8>>; RTPA_DATA_SHARDS],
    fec_shards: [Option<Vec<u8>>; RTPA_FEC_SHARDS],
    queue_time: Instant,
    fully_reassembled: bool,
    next_data_index: usize,
    allow_discontinuity: bool,
}

impl FecBlock {
    fn new(fec_header: AudioFecHeader, block_size: usize) -> Self {
        Self {
            fec_header,
            block_size,
            data_shards: array::from_fn(|_| None),
            fec_shards: array::from_fn(|_| None),
            queue_time: Instant::now(),
            fully_reassembled: false,
            next_data_index: 0,
            allow_discontinuity: false,
        }
    }

    fn data_received(&self) -> usize {
        self.data_shards
            .iter()
            .filter(|shard| shard.is_some())
            .count()
    }

    fn fec_received(&self) -> usize {
        self.fec_shards
            .iter()
            .filter(|shard| shard.is_some())
            .count()
    }
}

pub struct RtpAudioQueue {
    blocks: VecDeque<FecBlock>,
    next_rtp_sequence_number: u16,
    oldest_rtp_base_sequence_number: u16,
    last_oos_sequence_number: u16,
    received_oos_data: bool,
    synchronizing: bool,
    initialized: bool,
    stats: RtpAudioStats,
    packet_duration_ms: u32,
}

impl RtpAudioQueue {
    pub fn new(packet_duration_ms: u32) -> Self {
        Self {
            blocks: VecDeque::new(),
            next_rtp_sequence_number: 0,
            oldest_rtp_base_sequence_number: 0,
            last_oos_sequence_number: 0,
            received_oos_data: false,
            synchronizing: true,
            initialized: false,
            stats: RtpAudioStats::default(),
            packet_duration_ms,
        }
    }

    pub fn add_packet(&mut self, packet: ParsedPacket) -> Result<()> {
        match &packet {
            ParsedPacket::Audio { rtp, .. } => {
                self.stats.packet_count_audio += 1;
                // 与 getFecBlockForRtpPacket 一致, 必须在丢弃旧块之前记录迟到包.
                if !self.synchronizing
                    && protocol::is_before16(
                        rtp.sequence_number,
                        self.oldest_rtp_base_sequence_number,
                    )
                {
                    self.last_oos_sequence_number = rtp.sequence_number;
                    self.stats.packet_count_oos += 1;
                    if !self.received_oos_data {
                        tracing::debug!(sequence = rtp.sequence_number, "音频检测到迟到包, 延长恢复等待");
                        self.received_oos_data = true;
                    }
                } else if self.received_oos_data
                    && protocol::is_before16(
                        self.oldest_rtp_base_sequence_number,
                        self.last_oos_sequence_number,
                    )
                {
                    self.received_oos_data = false;
                }
            }
            ParsedPacket::Fec { .. } => self.stats.packet_count_fec += 1,
        }

        let (fec_header, block_size) = self.block_identity(&packet)?;

        // 单独保存初始化状态, 避免起始块为 65532 时下一块回绕至 0 导致重复同步.
        if !self.initialized {
            self.initialized = true;
            self.next_rtp_sequence_number = fec_header
                .base_sequence_number
                .wrapping_add(RTPA_DATA_SHARDS as u16);
            self.oldest_rtp_base_sequence_number = self.next_rtp_sequence_number;
            return Ok(());
        }

        if protocol::is_before16(
            fec_header.base_sequence_number,
            self.oldest_rtp_base_sequence_number,
        ) {
            return Ok(());
        }

        let index = self.ensure_block(fec_header, block_size)?;
        let block = self
            .blocks
            .get_mut(index)
            .ok_or_else(|| Error::Protocol("failed to locate queued FEC block".into()))?;

        if block.fully_reassembled {
            return Ok(());
        }

        match packet {
            ParsedPacket::Audio { rtp, payload } => {
                let shard_index = rtp
                    .sequence_number
                    .wrapping_sub(block.fec_header.base_sequence_number)
                    as usize;
                if shard_index >= RTPA_DATA_SHARDS {
                    self.stats.packet_count_invalid += 1;
                    return Err(Error::Protocol(
                        "audio shard index exceeded FEC data span".into(),
                    ));
                }
                if block.data_shards[shard_index].is_some() {
                    return Ok(());
                }
                block.data_shards[shard_index] = Some(payload);
            }
            ParsedPacket::Fec { fec, payload, .. } => {
                let shard_index = fec.fec_shard_index as usize;
                if shard_index >= RTPA_FEC_SHARDS {
                    self.stats.packet_count_fec_invalid += 1;
                    return Err(Error::Protocol(
                        "audio FEC shard index exceeded parity span".into(),
                    ));
                }
                if block.fec_shards[shard_index].is_some() {
                    return Ok(());
                }
                block.fec_shards[shard_index] = Some(payload);
            }
        }

        self.try_complete_block(index)?;
        if !self.has_packet_ready() {
            self.handle_missing_packets();
        }
        Ok(())
    }

    pub fn dequeue_ready(&mut self) -> Option<QueuedAudioFrame> {
        // 每次出队都推进恢复状态, 避免前一块释放后剩余块只能靠新 UDP 包唤醒.
        if !self.has_packet_ready() {
            self.handle_missing_packets();
        }
        if let Some(block) = self.blocks.front_mut() {
            let expected_seq = block
                .fec_header
                .base_sequence_number
                .wrapping_add(block.next_data_index as u16);
            if block.allow_discontinuity
                && expected_seq == self.next_rtp_sequence_number
                && block.data_shards[block.next_data_index].is_none()
            {
                block.next_data_index += 1;
                self.next_rtp_sequence_number = self.next_rtp_sequence_number.wrapping_add(1);
                let finished = block.next_data_index == RTPA_DATA_SHARDS;
                if finished {
                    self.free_block_head();
                }
                return Some(QueuedAudioFrame::Missing);
            }
        }

        if !self.has_packet_ready() {
            return None;
        }

        let block = self.blocks.front_mut()?;
        let shard = block.data_shards[block.next_data_index].clone()?;
        block.next_data_index += 1;
        self.next_rtp_sequence_number = self.next_rtp_sequence_number.wrapping_add(1);
        let finished = block.next_data_index == RTPA_DATA_SHARDS;
        if finished {
            self.free_block_head();
        }
        Some(QueuedAudioFrame::Encoded(shard))
    }

    fn block_identity(&mut self, packet: &ParsedPacket) -> Result<(AudioFecHeader, usize)> {
        match packet {
            ParsedPacket::Audio { rtp, payload } => {
                if payload.is_empty() {
                    self.stats.packet_count_invalid += 1;
                    return Err(Error::Protocol("empty audio payload".into()));
                }
                let base_sequence =
                    (rtp.sequence_number / RTPA_DATA_SHARDS as u16) * RTPA_DATA_SHARDS as u16;
                let offset = rtp.sequence_number.wrapping_sub(base_sequence) as u32;
                let base_timestamp = rtp.timestamp.wrapping_sub(offset * self.packet_duration_ms);
                Ok((
                    AudioFecHeader {
                        fec_shard_index: 0,
                        payload_type: rtp.packet_type,
                        base_sequence_number: base_sequence,
                        base_timestamp,
                        ssrc: rtp.ssrc,
                    },
                    payload.len(),
                ))
            }
            ParsedPacket::Fec { fec, payload, .. } => {
                if fec.base_sequence_number % RTPA_DATA_SHARDS as u16 != 0
                    || fec.fec_shard_index as usize >= RTPA_FEC_SHARDS
                    || fec.payload_type != RTP_PAYLOAD_TYPE_AUDIO
                    || payload.is_empty()
                {
                    self.stats.packet_count_fec_invalid += 1;
                    return Err(Error::Protocol("invalid audio FEC block header or payload".into()));
                }
                Ok((*fec, payload.len()))
            }
        }
    }

    fn ensure_block(&mut self, fec_header: AudioFecHeader, block_size: usize) -> Result<usize> {
        for (index, block) in self.blocks.iter().enumerate() {
            if block.fec_header.base_sequence_number == fec_header.base_sequence_number {
                if block.block_size != block_size {
                    self.stats.packet_count_fec_invalid += 1;
                    return Err(Error::Protocol(
                        "audio block size mismatch within a FEC block".into(),
                    ));
                }
                if block.fec_header.payload_type != fec_header.payload_type
                    || block.fec_header.base_timestamp != fec_header.base_timestamp
                    || block.fec_header.ssrc != fec_header.ssrc
                {
                    self.stats.packet_count_fec_invalid += 1;
                    return Err(Error::Protocol("inconsistent audio FEC block identity".into()));
                }
                return Ok(index);
            }
            if protocol::is_before16(
                fec_header.base_sequence_number,
                block.fec_header.base_sequence_number,
            ) {
                self.blocks
                    .insert(index, FecBlock::new(fec_header, block_size));
                return Ok(index);
            }
        }

        self.blocks.push_back(FecBlock::new(fec_header, block_size));
        Ok(self.blocks.len() - 1)
    }

    fn try_complete_block(&mut self, index: usize) -> Result<()> {
        let Some(block) = self.blocks.get_mut(index) else {
            return Ok(());
        };

        let data_received = block.data_received();
        let fec_received = block.fec_received();
        if data_received == RTPA_DATA_SHARDS {
            block.fully_reassembled = true;
            return Ok(());
        }
        if data_received + fec_received < RTPA_DATA_SHARDS {
            return Ok(());
        }

        let recovered = fec::recover_audio_block(&mut block.data_shards, &block.fec_shards)?;
        if recovered > 0 {
            self.stats.packet_count_fec_recovered += recovered as u64;
        }
        block.fully_reassembled = true;
        Ok(())
    }

    fn has_packet_ready(&self) -> bool {
        let Some(block) = self.blocks.front() else {
            return false;
        };
        if block.allow_discontinuity {
            return true;
        }
        if block.next_data_index >= RTPA_DATA_SHARDS {
            return false;
        }
        let expected_seq = block
            .fec_header
            .base_sequence_number
            .wrapping_add(block.next_data_index as u16);
        expected_seq == self.next_rtp_sequence_number
            && block.data_shards[block.next_data_index].is_some()
    }

    fn handle_missing_packets(&mut self) {
        let block_count = self.blocks.len();
        let Some(head) = self.blocks.front_mut() else {
            return;
        };

        if protocol::is_before16(
            self.next_rtp_sequence_number,
            head.fec_header.base_sequence_number,
        ) {
            self.next_rtp_sequence_number = head.fec_header.base_sequence_number;
            self.oldest_rtp_base_sequence_number = head.fec_header.base_sequence_number;
            // 跨过全丢块后, 若新队头也缺首包, 继续评估其已到期的恢复窗口.
            // 有首包时立即交付, 不能将整块提前标记为丢失.
            if head.data_shards[head.next_data_index].is_some() {
                return;
            }
        }

        if block_count == 1 {
            return;
        }

        if !self.received_oos_data
            || head.queue_time.elapsed()
                > Duration::from_millis(
                    (self.packet_duration_ms as u64 * RTPA_DATA_SHARDS as u64) + OOS_WAIT_TIME_MS,
                )
        {
            self.stats.packet_count_fec_failed += 1;
            tracing::debug!(
                base_sequence = head.fec_header.base_sequence_number,
                data_shards = head.data_received(),
                fec_shards = head.fec_received(),
                "音频 FEC 无法恢复, 使用 Opus 丢包补偿"
            );
            head.allow_discontinuity = true;
        }
    }

    fn free_block_head(&mut self) {
        if let Some(block) = self.blocks.pop_front() {
            self.oldest_rtp_base_sequence_number = block
                .fec_header
                .base_sequence_number
                .wrapping_add(RTPA_DATA_SHARDS as u16);
            self.synchronizing = false;
        }
    }
}

pub struct AudioDepacketizer {
    queue: RtpAudioQueue,
    packets_to_drop: u32,
    received_packet: bool,
}

impl AudioDepacketizer {
    pub fn new(packet_duration_ms: u32, initial_drop_ms: u32) -> Self {
        Self {
            queue: RtpAudioQueue::new(packet_duration_ms),
            packets_to_drop: initial_drop_ms.checked_div(packet_duration_ms).unwrap_or(0),
            received_packet: false,
        }
    }

    pub fn push_datagram(&mut self, datagram: &[u8]) -> Result<Vec<QueuedAudioFrame>> {
        let parsed = parse_datagram(datagram)?;
        self.received_packet = true;
        if self.packets_to_drop > 0 {
            // AudioReceiveThreadProc 丢弃整个启动窗口, 只有数据包消耗计数.
            // FEC 同时丢弃, 否则会重建本应舍弃的过期音频.
            if matches!(parsed, ParsedPacket::Audio { .. }) {
                self.packets_to_drop -= 1;
            }
            return Ok(Vec::new());
        }

        self.queue.add_packet(parsed)?;
        Ok(self.drain_ready())
    }

    pub fn receive_timeout(&mut self) -> Vec<QueuedAudioFrame> {
        // 已收到有效包后 socket 超时表示启动积压已排空, 与 Moonlight 一致.
        if self.received_packet {
            self.packets_to_drop = 0;
        }
        self.drain_ready()
    }

    fn drain_ready(&mut self) -> Vec<QueuedAudioFrame> {
        let mut ready = Vec::new();
        while let Some(frame) = self.queue.dequeue_ready() {
            ready.push(frame);
        }
        ready
    }
}
