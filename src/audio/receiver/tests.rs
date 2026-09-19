use super::*;
use crate::audio::protocol::{RtpHeader, write_audio_packet};
use crate::audio::sender::AudioPacketizer;

fn audio(sequence: u16) -> ParsedPacket {
    ParsedPacket::Audio {
        rtp: RtpHeader {
            packet_type: RTP_PAYLOAD_TYPE_AUDIO,
            sequence_number: sequence,
            timestamp: u32::from(sequence) * 5,
            ssrc: 7,
        },
        payload: vec![sequence as u8; 8],
    }
}

fn drain(queue: &mut RtpAudioQueue) -> Vec<QueuedAudioFrame> {
    let mut frames = Vec::new();
    while let Some(frame) = queue.dequeue_ready() {
        frames.push(frame);
    }
    frames
}

fn synchronized_queue() -> RtpAudioQueue {
    let mut queue = RtpAudioQueue::new(5);
    for sequence in 0..8 {
        queue.add_packet(audio(sequence)).unwrap();
        drain(&mut queue);
    }
    assert!(!queue.synchronizing);
    queue
}

#[test]
fn delivers_in_order_without_waiting_for_parity_and_ignores_duplicates() {
    let mut queue = RtpAudioQueue::new(5);
    queue.add_packet(audio(0)).unwrap();
    for sequence in 4..8 {
        queue.add_packet(audio(sequence)).unwrap();
        assert_eq!(drain(&mut queue), vec![QueuedAudioFrame::Encoded(vec![sequence as u8; 8])]);
        queue.add_packet(audio(sequence)).unwrap();
        assert!(drain(&mut queue).is_empty());
    }
}

#[test]
fn restores_every_pair_of_lost_data_or_parity_shards() {
    let mut sender = AudioPacketizer::new(5, 7, true);
    for sequence in 0..4 {
        sender.push_encoded_frame(&[sequence; 8]).unwrap();
    }
    let mut datagrams = Vec::new();
    let expected: Vec<_> = (4..8).map(|sequence| {
        let payload: Vec<u8> = (0..251).map(|index| (index * 37 + sequence) as u8).collect();
        datagrams.extend(sender.push_encoded_frame(&payload).unwrap());
        QueuedAudioFrame::Encoded(payload)
    }).collect();
    assert_eq!(datagrams.len(), 6);
    for first_missing in 0..6 {
        for second_missing in first_missing + 1..6 {
            let mut receiver = AudioDepacketizer::new(5, 0);
            receiver.queue.add_packet(audio(0)).unwrap();
            let mut recovered = Vec::new();
            // 逆序到达, 同时覆盖 FEC 先于数据与数据乱序.
            for index in (0..6).rev() {
                if index != first_missing && index != second_missing {
                    recovered.extend(receiver.push_datagram(&datagrams[index].bytes).unwrap());
                    recovered.extend(receiver.push_datagram(&datagrams[index].bytes).unwrap());
                }
            }
            assert_eq!(recovered, expected, "missing {first_missing}, {second_missing}");
        }
    }
}

#[test]
fn late_packet_enables_reordering_grace_before_being_discarded() {
    let mut queue = synchronized_queue();
    queue.add_packet(audio(4)).unwrap();
    assert!(queue.received_oos_data);
    assert_eq!(queue.stats.packet_count_oos, 1);
    queue.add_packet(audio(9)).unwrap();
    queue.add_packet(audio(12)).unwrap();
    assert!(drain(&mut queue).is_empty());
    queue.add_packet(audio(8)).unwrap();
    assert_eq!(drain(&mut queue), vec![
        QueuedAudioFrame::Encoded(vec![8; 8]),
        QueuedAudioFrame::Encoded(vec![9; 8]),
    ]);
    assert_eq!(queue.stats.packet_count_fec_failed, 0);
}

#[test]
fn unrecoverable_block_produces_plc_then_advances_to_next_block() {
    let mut queue = synchronized_queue();
    queue.add_packet(audio(9)).unwrap();
    queue.add_packet(audio(12)).unwrap();
    assert_eq!(drain(&mut queue), vec![
        QueuedAudioFrame::Missing,
        QueuedAudioFrame::Encoded(vec![9; 8]),
        QueuedAudioFrame::Missing,
        QueuedAudioFrame::Missing,
        QueuedAudioFrame::Encoded(vec![12; 8]),
    ]);
    assert_eq!(queue.stats.packet_count_fec_failed, 1);
}

#[test]
fn expired_reordering_wait_can_advance_without_another_datagram() {
    let mut queue = synchronized_queue();
    queue.add_packet(audio(4)).unwrap();
    queue.add_packet(audio(9)).unwrap();
    queue.add_packet(audio(12)).unwrap();
    assert!(drain(&mut queue).is_empty());
    queue.blocks.front_mut().unwrap().queue_time = Instant::now() - Duration::from_secs(1);
    let mut receiver = AudioDepacketizer::new(5, 0);
    receiver.queue = queue;
    let frames = receiver.receive_timeout();
    assert_eq!(frames.len(), 5);
    assert_eq!(frames[0], QueuedAudioFrame::Missing);
    assert_eq!(frames[4], QueuedAudioFrame::Encoded(vec![12; 8]));
}

#[test]
fn one_timeout_drains_expired_blocks_across_a_completely_lost_block() {
    let mut queue = synchronized_queue();
    queue.add_packet(audio(4)).unwrap();
    for sequence in [9, 17, 20] {
        queue.add_packet(audio(sequence)).unwrap();
    }
    assert!(drain(&mut queue).is_empty());
    for block in &mut queue.blocks {
        block.queue_time = Instant::now() - Duration::from_secs(1);
    }
    let mut receiver = AudioDepacketizer::new(5, 0);
    receiver.queue = queue;
    assert_eq!(receiver.receive_timeout(), vec![
        QueuedAudioFrame::Missing,
        QueuedAudioFrame::Encoded(vec![9; 8]),
        QueuedAudioFrame::Missing,
        QueuedAudioFrame::Missing,
        QueuedAudioFrame::Missing,
        QueuedAudioFrame::Encoded(vec![17; 8]),
        QueuedAudioFrame::Missing,
        QueuedAudioFrame::Missing,
        QueuedAudioFrame::Encoded(vec![20; 8]),
    ]);
    assert_eq!(receiver.queue.stats.packet_count_fec_failed, 2);
}

#[test]
fn entirely_lost_block_resynchronizes_without_synthetic_backlog() {
    let mut queue = synchronized_queue();
    queue.add_packet(audio(16)).unwrap();
    assert_eq!(drain(&mut queue), vec![QueuedAudioFrame::Encoded(vec![16; 8])]);
    assert!(!queue.received_oos_data);
}

#[test]
fn startup_at_sequence_wrap_does_not_restart_synchronization() {
    let mut queue = RtpAudioQueue::new(5);
    queue.add_packet(audio(65532)).unwrap();
    for sequence in 0..4 {
        queue.add_packet(audio(sequence)).unwrap();
        assert_eq!(drain(&mut queue), vec![QueuedAudioFrame::Encoded(vec![sequence as u8; 8])]);
    }
    assert!(!queue.synchronizing);
}

#[test]
fn startup_drop_discards_parity_and_timeout_ends_only_started_window() {
    let mut sender = AudioPacketizer::new(5, 7, true);
    let mut receiver = AudioDepacketizer::new(5, 500);
    receiver.receive_timeout();
    assert_eq!(receiver.packets_to_drop, 100);
    for sequence in 0..12 {
        for packet in sender.push_encoded_frame(&[sequence; 8]).unwrap() {
            assert!(receiver.push_datagram(&packet.bytes).unwrap().is_empty());
        }
    }
    assert!(!receiver.queue.initialized);
    assert!(receiver.queue.blocks.is_empty());
    assert_eq!(receiver.packets_to_drop, 88);
    receiver.receive_timeout();
    assert_eq!(receiver.packets_to_drop, 0);
    for sequence in 12..16 {
        for packet in sender.push_encoded_frame(&[sequence; 8]).unwrap() {
            assert!(receiver.push_datagram(&packet.bytes).unwrap().is_empty());
        }
    }
    let packet = sender.push_encoded_frame(&[16; 8]).unwrap();
    assert_eq!(receiver.push_datagram(&packet[0].bytes).unwrap(), vec![QueuedAudioFrame::Encoded(vec![16; 8])]);
}

#[test]
fn rejects_mismatched_block_identity_without_poisoning_valid_shards() {
    let mut queue = synchronized_queue();
    queue.add_packet(audio(9)).unwrap();
    for field in 0..3 {
        let mut packet = audio(8);
        if let ParsedPacket::Audio { rtp, payload } = &mut packet {
            match field {
                0 => rtp.timestamp += 1,
                1 => rtp.ssrc += 1,
                _ => { payload.pop(); }
            }
        }
        assert!(queue.add_packet(packet).is_err());
    }
    queue.add_packet(audio(8)).unwrap();
    assert_eq!(drain(&mut queue).len(), 2);
}

#[test]
fn rejects_invalid_rtp_headers_and_empty_payloads() {
    let ParsedPacket::Audio { rtp, payload } = audio(4) else { unreachable!() };
    let packet = write_audio_packet(rtp, &payload);
    for flags in [0, 0x40, 0x81, 0x90, 0xa0] {
        let mut invalid = packet.clone();
        invalid[0] = flags;
        assert!(parse_datagram(&invalid).is_err());
    }
    assert!(parse_datagram(&packet[..12]).is_err());
    assert!(parse_datagram(&packet[..11]).is_err());
    assert!(parse_datagram(&packet).is_ok());
}
