use crate::audio::error::{Error, Result};
use crate::audio::fec;
use crate::audio::protocol::{
    AudioFecHeader, RTP_PAYLOAD_TYPE_AUDIO, RTP_PAYLOAD_TYPE_FEC, RTPA_DATA_SHARDS,
    RTPA_FEC_SHARDS, RtpHeader, write_audio_packet, write_fec_packet,
};
use std::array;

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
    block_payloads: [Option<Vec<u8>>; RTPA_DATA_SHARDS],
    fec_payloads: [Vec<u8>; RTPA_FEC_SHARDS],
    block_base_sequence: u16,
    block_base_timestamp: u32,
}

impl AudioPacketizer {
    pub fn new(packet_duration_ms: u32, ssrc: u32, enable_fec: bool) -> Self {
        Self {
            sequence_number: 0,
            timestamp: 0,
            ssrc,
            packet_duration_ms,
            enable_fec,
            block_payloads: array::from_fn(|_| None),
            fec_payloads: array::from_fn(|_| Vec::new()),
            block_base_sequence: 0,
            block_base_timestamp: 0,
        }
    }

    pub fn push_encoded_frame(&mut self, payload: &[u8]) -> Result<Vec<OutboundDatagram>> {
        let mut out = Vec::with_capacity(1 + RTPA_FEC_SHARDS);
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
        out.push(OutboundDatagram {
            bytes: write_audio_packet(rtp, payload),
        });

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

                self.fec_payloads[0].resize(block_size, 0);
                self.fec_payloads[1].resize(block_size, 0);
                let (first, rest) = self.fec_payloads.split_at_mut(1);
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
                    [&mut first[0], &mut rest[0]],
                )?;

                for (fec_index, parity) in self.fec_payloads.iter().enumerate() {
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
                        bytes: write_fec_packet(rtp, fec_header, parity),
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
    use crate::audio::protocol::{
        AudioFecHeader, ParsedPacket, RTP_PAYLOAD_TYPE_AUDIO, parse_datagram,
    };

    #[test]
    fn completed_block_emits_four_audio_packets_and_two_fec_packets() {
        let mut packetizer = AudioPacketizer::new(5, 77, true);
        let payload = vec![1u8; 16];
        let mut datagrams = Vec::new();

        for _ in 0..4 {
            datagrams.extend(packetizer.push_encoded_frame(&payload).unwrap());
        }

        assert_eq!(datagrams.len(), 6);

        for (index, datagram) in datagrams.iter().take(4).enumerate() {
            match parse_datagram(&datagram.bytes).unwrap() {
                ParsedPacket::Audio { rtp, payload } => {
                    assert_eq!(rtp.packet_type, RTP_PAYLOAD_TYPE_AUDIO);
                    assert_eq!(rtp.sequence_number, index as u16);
                    assert_eq!(rtp.timestamp, (index as u32) * 5);
                    assert_eq!(payload, vec![1u8; 16]);
                }
                other => panic!("expected audio packet, got {other:?}"),
            }
        }

        for (index, datagram) in datagrams.iter().skip(4).enumerate() {
            match parse_datagram(&datagram.bytes).unwrap() {
                ParsedPacket::Fec { fec, payload } => {
                    assert_eq!(payload.len(), 16);
                    assert_eq!(
                        fec,
                        AudioFecHeader {
                            fec_shard_index: index as u8,
                            payload_type: RTP_PAYLOAD_TYPE_AUDIO,
                            base_sequence_number: 0,
                            base_timestamp: 0,
                            ssrc: 77,
                        }
                    );
                }
                other => panic!("expected fec packet, got {other:?}"),
            }
        }

        match parse_datagram(&datagrams[4].bytes).unwrap() {
            ParsedPacket::Fec { fec, .. } => assert_eq!(fec.payload_type, RTP_PAYLOAD_TYPE_AUDIO),
            _ => unreachable!(),
        }
    }
}
