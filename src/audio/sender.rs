use crate::audio::error::{Error, Result};
use crate::audio::fec;
use crate::audio::protocol::{
    AudioFecHeader, RTP_PAYLOAD_TYPE_AUDIO, RTP_PAYLOAD_TYPE_FEC, RTPA_DATA_SHARDS,
    RTPA_FEC_SHARDS, RtpHeader, write_audio_packet, write_fec_packet,
};
use std::array;
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct OutboundDatagram {
    pub bytes: Vec<u8>,
}

pub struct AudioPacketizer {
    sequence_number: u16,
    timestamp: u32,
    ssrc: u32,
    packet_duration_ms: u32,
    enable_fec: bool,
    audio_redundancy_packets: usize,
    block_payloads: [Option<Vec<u8>>; RTPA_DATA_SHARDS],
    block_base_sequence: u16,
    block_base_timestamp: u32,
    recent_audio_datagrams: VecDeque<Vec<u8>>,
}

impl AudioPacketizer {
    pub fn new(
        packet_duration_ms: u32,
        ssrc: u32,
        enable_fec: bool,
        audio_redundancy_packets: usize,
    ) -> Self {
        Self {
            sequence_number: 0,
            timestamp: 0,
            ssrc,
            packet_duration_ms,
            enable_fec,
            audio_redundancy_packets,
            block_payloads: array::from_fn(|_| None),
            block_base_sequence: 0,
            block_base_timestamp: 0,
            recent_audio_datagrams: VecDeque::with_capacity(audio_redundancy_packets),
        }
    }

    pub fn push_encoded_frame(&mut self, payload: &[u8]) -> Result<Vec<OutboundDatagram>> {
        let mut out = Vec::with_capacity(1 + self.audio_redundancy_packets + RTPA_FEC_SHARDS);
        let sequence_number = self.sequence_number;
        let timestamp = self.timestamp;
        let shard_index = sequence_number as usize % RTPA_DATA_SHARDS;

        if shard_index == 0 {
            self.block_base_sequence = sequence_number;
            self.block_base_timestamp = timestamp;
        }

        self.block_payloads[shard_index] = Some(payload.to_vec());

        let rtp = RtpHeader {
            packet_type: RTP_PAYLOAD_TYPE_AUDIO,
            sequence_number,
            timestamp,
            ssrc: self.ssrc,
        };
        let primary_audio = write_audio_packet(rtp, payload);
        out.push(OutboundDatagram {
            bytes: primary_audio.clone(),
        });
        for redundant in self
            .recent_audio_datagrams
            .iter()
            .rev()
            .take(self.audio_redundancy_packets)
        {
            out.push(OutboundDatagram {
                bytes: redundant.clone(),
            });
        }
        if self.audio_redundancy_packets > 0 {
            self.recent_audio_datagrams.push_back(primary_audio);
            while self.recent_audio_datagrams.len() > self.audio_redundancy_packets {
                self.recent_audio_datagrams.pop_front();
            }
        }

        self.sequence_number = self.sequence_number.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(self.packet_duration_ms);

        if self.enable_fec && shard_index + 1 == RTPA_DATA_SHARDS {
            let equal_sizes = self.block_payloads.iter().all(|shard| {
                shard.as_ref().map(|shard| shard.len())
                    == self.block_payloads[0].as_ref().map(|shard| shard.len())
            });

            if equal_sizes {
                let block_size = self.block_payloads[0]
                    .as_ref()
                    .map(|payload| payload.len())
                    .ok_or_else(|| {
                        Error::Protocol("missing first payload in completed audio FEC block".into())
                    })?;

                let mut parity0 = vec![0u8; block_size];
                let mut parity1 = vec![0u8; block_size];
                fec::encode_audio_block(
                    [
                        self.block_payloads[0]
                            .as_deref()
                            .ok_or_else(|| Error::Protocol("missing audio shard 0".into()))?,
                        self.block_payloads[1]
                            .as_deref()
                            .ok_or_else(|| Error::Protocol("missing audio shard 1".into()))?,
                        self.block_payloads[2]
                            .as_deref()
                            .ok_or_else(|| Error::Protocol("missing audio shard 2".into()))?,
                        self.block_payloads[3]
                            .as_deref()
                            .ok_or_else(|| Error::Protocol("missing audio shard 3".into()))?,
                    ],
                    [&mut parity0, &mut parity1],
                )?;

                for (fec_index, parity) in [parity0, parity1].into_iter().enumerate() {
                    let rtp = RtpHeader {
                        packet_type: RTP_PAYLOAD_TYPE_FEC,
                        sequence_number: sequence_number.wrapping_add(fec_index as u16 + 1),
                        timestamp: self.block_base_timestamp,
                        ssrc: self.ssrc,
                    };
                    let fec_header = AudioFecHeader {
                        fec_shard_index: fec_index as u8,
                        payload_type: RTP_PAYLOAD_TYPE_AUDIO,
                        base_sequence_number: self.block_base_sequence,
                        base_timestamp: self.block_base_timestamp,
                        ssrc: self.ssrc,
                    };
                    out.push(OutboundDatagram {
                        bytes: write_fec_packet(rtp, fec_header, &parity),
                    });
                }
            }

            self.block_payloads.fill(None);
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::AudioPacketizer;

    #[test]
    fn packetizer_repeats_the_previous_audio_packet_once() {
        let mut packetizer = AudioPacketizer::new(5, 7, false, 1);

        let first = packetizer.push_encoded_frame(&[1, 2, 3]).unwrap();
        let second = packetizer.push_encoded_frame(&[4, 5, 6]).unwrap();

        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 2);
        assert_eq!(second[1].bytes, first[0].bytes);
    }
}
