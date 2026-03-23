use crate::audio::capture::{AudioInput, CaptureStatus, open_input};
use crate::audio::codec::OpusEncoder;
use crate::audio::config::SenderConfig;
use crate::audio::error::{Error, Result};
use crate::audio::fec;
use crate::audio::protocol::{
    AudioFecHeader, RTP_PAYLOAD_TYPE_AUDIO, RTP_PAYLOAD_TYPE_FEC, RTPA_DATA_SHARDS,
    RTPA_FEC_SHARDS, RtpHeader, write_audio_packet, write_fec_packet,
};
use std::array;
use std::net::UdpSocket;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct OutboundDatagram {
    pub sequence_number: u16,
    pub packet_type: u8,
    pub payload_len: usize,
    pub bytes: Vec<u8>,
}

pub struct AudioPacketizer {
    sequence_number: u16,
    timestamp: u32,
    ssrc: u32,
    packet_duration_ms: u32,
    enable_fec: bool,
    block_payloads: [Option<Vec<u8>>; RTPA_DATA_SHARDS],
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
            sequence_number,
            packet_type: RTP_PAYLOAD_TYPE_AUDIO,
            payload_len: payload.len(),
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
                        sequence_number: rtp.sequence_number,
                        packet_type: RTP_PAYLOAD_TYPE_FEC,
                        payload_len: parity.len(),
                        bytes: write_fec_packet(rtp, fec_header, &parity),
                    });
                }
            }

            self.block_payloads.fill(None);
        }

        Ok(out)
    }
}

pub struct AudioSender {
    socket: UdpSocket,
    input: Box<dyn AudioInput>,
    encoder: OpusEncoder,
    packetizer: AudioPacketizer,
    pcm_buffer: Vec<f32>,
    encoded_buffer: Vec<u8>,
    timeout: Duration,
}

impl AudioSender {
    pub fn bind(config: SenderConfig) -> Result<Self> {
        let stream = config.stream_params()?;
        let socket = UdpSocket::bind(config.bind_addr)?;
        socket.connect(config.destination)?;
        let input = open_input(&config.capture, &stream)?;
        let encoder = OpusEncoder::new(stream.opus_config(), stream.bitrate)?;
        let packetizer =
            AudioPacketizer::new(stream.packet_duration_ms, config.ssrc, config.enable_fec);
        let pcm_buffer = vec![0.0; stream.samples_per_frame()];
        let encoded_buffer = vec![0u8; 1400];

        Ok(Self {
            socket,
            input,
            encoder,
            packetizer,
            pcm_buffer,
            encoded_buffer,
            timeout: config.read_timeout,
        })
    }

    pub fn pump_once(&mut self) -> Result<usize> {
        let status = self.input.read_frame(&mut self.pcm_buffer, self.timeout)?;
        match status {
            CaptureStatus::Timeout => Ok(0),
            CaptureStatus::Ok => {
                let encoded = self
                    .encoder
                    .encode_float(&self.pcm_buffer, &mut self.encoded_buffer)?;
                let datagrams = self
                    .packetizer
                    .push_encoded_frame(&self.encoded_buffer[..encoded])?;
                let sent = datagrams.len();
                for datagram in datagrams {
                    self.socket.send(&datagram.bytes)?;
                }
                Ok(sent)
            }
        }
    }

    pub fn run(&mut self) -> Result<()> {
        loop {
            self.pump_once()?;
        }
    }
}

