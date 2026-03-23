use crate::audio::error::{Error, Result};
use crate::audio::fec;
use crate::audio::protocol::{
    self, AudioFecHeader, OOS_WAIT_TIME_MS, ParsedPacket, RTPA_DATA_SHARDS, RTPA_FEC_SHARDS,
    parse_datagram,
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

#[derive(Clone, Debug)]
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
            stats: RtpAudioStats::default(),
            packet_duration_ms,
        }
    }

    pub fn add_packet(&mut self, packet: ParsedPacket) -> Result<()> {
        match &packet {
            ParsedPacket::Audio { .. } => self.stats.packet_count_audio += 1,
            ParsedPacket::Fec { .. } => self.stats.packet_count_fec += 1,
        }

        let (fec_header, block_size) = self.block_identity(&packet)?;

        if self.synchronizing && self.oldest_rtp_base_sequence_number == 0 {
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
                if block.data_shards[shard_index].is_none() {
                    block.data_shards[shard_index] = Some(payload);
                }

                if !self.synchronizing
                    && protocol::is_before16(
                        rtp.sequence_number,
                        self.oldest_rtp_base_sequence_number,
                    )
                {
                    self.last_oos_sequence_number = rtp.sequence_number;
                    self.stats.packet_count_oos += 1;
                    self.received_oos_data = true;
                } else if self.received_oos_data
                    && protocol::is_before16(
                        self.oldest_rtp_base_sequence_number,
                        self.last_oos_sequence_number,
                    )
                {
                    self.received_oos_data = false;
                }
            }
            ParsedPacket::Fec { fec, payload, .. } => {
                let shard_index = fec.fec_shard_index as usize;
                if shard_index >= RTPA_FEC_SHARDS {
                    self.stats.packet_count_fec_invalid += 1;
                    return Err(Error::Protocol(
                        "audio FEC shard index exceeded parity span".into(),
                    ));
                }
                if block.fec_shards[shard_index].is_none() {
                    block.fec_shards[shard_index] = Some(payload);
                }
            }
        }

        self.try_complete_block(index)?;
        if !self.has_packet_ready() {
            self.handle_missing_packets();
        }
        Ok(())
    }

    pub fn dequeue_ready(&mut self) -> Option<QueuedAudioFrame> {
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
                if fec.base_sequence_number % RTPA_DATA_SHARDS as u16 != 0 {
                    self.stats.packet_count_fec_invalid += 1;
                    return Err(Error::Protocol(
                        "audio FEC block is not aligned to 4-packet boundary".into(),
                    ));
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
            return;
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
    startup_buffer_packets: usize,
    buffering_startup: bool,
}

impl AudioDepacketizer {
    pub fn new(packet_duration_ms: u32, initial_buffer_ms: u32) -> Self {
        let startup_buffer_packets = if packet_duration_ms == 0 {
            0
        } else {
            initial_buffer_ms.div_ceil(packet_duration_ms) as usize
        };
        Self {
            queue: RtpAudioQueue::new(packet_duration_ms),
            startup_buffer_packets,
            buffering_startup: startup_buffer_packets > 0,
        }
    }

    pub fn push_datagram(&mut self, datagram: &[u8]) -> Result<Vec<QueuedAudioFrame>> {
        let parsed = parse_datagram(datagram)?;
        self.queue.add_packet(parsed)?;
        if self.buffering_startup && self.queue.ready_packet_count() < self.startup_buffer_packets {
            return Ok(Vec::new());
        }
        self.buffering_startup = false;
        let mut ready = Vec::new();
        while let Some(frame) = self.queue.dequeue_ready() {
            ready.push(frame);
        }
        Ok(ready)
    }
}

impl RtpAudioQueue {
    fn ready_packet_count(&self) -> usize {
        let mut count = 0usize;
        let mut next_sequence = self.next_rtp_sequence_number;

        for block in &self.blocks {
            let mut data_index = block.next_data_index;
            while data_index < RTPA_DATA_SHARDS {
                let expected_sequence = block
                    .fec_header
                    .base_sequence_number
                    .wrapping_add(data_index as u16);
                if expected_sequence != next_sequence {
                    return count;
                }

                if block.data_shards[data_index].is_some() || block.allow_discontinuity {
                    count += 1;
                    next_sequence = next_sequence.wrapping_add(1);
                    data_index += 1;
                    continue;
                }

                return count;
            }
        }

        count
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioDepacketizer, QueuedAudioFrame};
    use crate::audio::sender::AudioPacketizer;

    #[test]
    fn depacketizer_buffers_startup_audio_instead_of_dropping_it() {
        let mut packetizer = AudioPacketizer::new(5, 11, false, 0);
        let mut depacketizer = AudioDepacketizer::new(5, 10);

        for payload in 0..4u8 {
            let datagrams = packetizer.push_encoded_frame(&[payload]).unwrap();
            assert!(
                depacketizer
                    .push_datagram(&datagrams[0].bytes)
                    .unwrap()
                    .is_empty()
            );
        }

        let fifth = packetizer.push_encoded_frame(&[4]).unwrap();
        assert!(
            depacketizer
                .push_datagram(&fifth[0].bytes)
                .unwrap()
                .is_empty()
        );

        let sixth = packetizer.push_encoded_frame(&[5]).unwrap();
        let ready = depacketizer.push_datagram(&sixth[0].bytes).unwrap();
        let payloads = ready
            .into_iter()
            .map(|frame| match frame {
                QueuedAudioFrame::Encoded(payload) => payload,
                QueuedAudioFrame::Missing => panic!("unexpected packet loss during startup"),
            })
            .collect::<Vec<_>>();
        assert_eq!(payloads, vec![vec![4], vec![5]]);
    }
}
