use crate::audio::error::{Error, Result};

pub const RTP_HEADER_LEN: usize = 12;
pub const AUDIO_FEC_HEADER_LEN: usize = 12;
pub const RTP_PAYLOAD_TYPE_AUDIO: u8 = 97;
pub const RTP_PAYLOAD_TYPE_FEC: u8 = 127;
pub const RTPA_DATA_SHARDS: usize = 4;
pub const RTPA_FEC_SHARDS: usize = 2;
pub const RTPA_TOTAL_SHARDS: usize = RTPA_DATA_SHARDS + RTPA_FEC_SHARDS;
pub const OOS_WAIT_TIME_MS: u64 = 10;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtpHeader {
    pub packet_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioFecHeader {
    pub fec_shard_index: u8,
    pub payload_type: u8,
    pub base_sequence_number: u16,
    pub base_timestamp: u32,
    pub ssrc: u32,
}

#[derive(Clone, Debug)]
pub enum ParsedPacket {
    Audio {
        rtp: RtpHeader,
        payload: Vec<u8>,
    },
    Fec {
        rtp: RtpHeader,
        fec: AudioFecHeader,
        payload: Vec<u8>,
    },
}

pub fn write_audio_packet(rtp: RtpHeader, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
    write_rtp_header(rtp, &mut out);
    out.extend_from_slice(payload);
    out
}

pub fn write_fec_packet(rtp: RtpHeader, fec: AudioFecHeader, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RTP_HEADER_LEN + AUDIO_FEC_HEADER_LEN + payload.len());
    write_rtp_header(rtp, &mut out);
    out.push(fec.fec_shard_index);
    out.push(fec.payload_type);
    out.extend_from_slice(&fec.base_sequence_number.to_be_bytes());
    out.extend_from_slice(&fec.base_timestamp.to_be_bytes());
    out.extend_from_slice(&fec.ssrc.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn parse_datagram(packet: &[u8]) -> Result<ParsedPacket> {
    if packet.len() < RTP_HEADER_LEN {
        return Err(Error::Protocol("datagram shorter than RTP header".into()));
    }

    let rtp = parse_rtp_header(packet)?;
    let body = &packet[RTP_HEADER_LEN..];

    match rtp.packet_type {
        RTP_PAYLOAD_TYPE_AUDIO => Ok(ParsedPacket::Audio {
            rtp,
            payload: body.to_vec(),
        }),
        RTP_PAYLOAD_TYPE_FEC => {
            if body.len() < AUDIO_FEC_HEADER_LEN {
                return Err(Error::Protocol(
                    "datagram shorter than audio FEC header".into(),
                ));
            }
            let fec = AudioFecHeader {
                fec_shard_index: body[0],
                payload_type: body[1],
                base_sequence_number: u16::from_be_bytes([body[2], body[3]]),
                base_timestamp: u32::from_be_bytes([body[4], body[5], body[6], body[7]]),
                ssrc: u32::from_be_bytes([body[8], body[9], body[10], body[11]]),
            };
            Ok(ParsedPacket::Fec {
                rtp,
                fec,
                payload: body[AUDIO_FEC_HEADER_LEN..].to_vec(),
            })
        }
        other => Err(Error::Protocol(format!(
            "unsupported RTP payload type {other}"
        ))),
    }
}

fn parse_rtp_header(packet: &[u8]) -> Result<RtpHeader> {
    if packet.len() < RTP_HEADER_LEN {
        return Err(Error::Protocol("missing RTP header".into()));
    }

    Ok(RtpHeader {
        packet_type: packet[1] & 0x7f,
        sequence_number: u16::from_be_bytes([packet[2], packet[3]]),
        timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
        ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
    })
}

fn write_rtp_header(rtp: RtpHeader, out: &mut Vec<u8>) {
    out.push(0x80);
    out.push(rtp.packet_type);
    out.extend_from_slice(&rtp.sequence_number.to_be_bytes());
    out.extend_from_slice(&rtp.timestamp.to_be_bytes());
    out.extend_from_slice(&rtp.ssrc.to_be_bytes());
}

pub(crate) fn is_before16(a: u16, b: u16) -> bool {
    a != b && (a.wrapping_sub(b) as i16) < 0
}
