use super::*;
use std::fmt::Write;

// 原始 Moonlight C 队列的独立输出, 生成流程与边界见 docs/audio-queue-vectors.md.
const VECTORS: &str = include_str!("../../../native/tests/audio-queue-vectors.tsv");

fn payload(sequence: u16) -> Vec<u8> {
    (0..16).map(|index| (u32::from(sequence) * 37 + u32::from(sequence >> 8) + index * 17) as u8).collect()
}

fn packet(event: &str) -> ParsedPacket {
    use crate::audio::protocol::RtpHeader;
    let (sequence, fec_index) = if let Some(value) = event.strip_prefix('a') {
        (value.parse::<u16>().unwrap(), None)
    } else {
        let (sequence, index) = event.strip_prefix('f').unwrap().split_once('/').unwrap();
        (sequence.parse::<u16>().unwrap(), Some(index.parse::<usize>().unwrap()))
    };
    let rtp = RtpHeader {
        packet_type: if fec_index.is_some() { protocol::RTP_PAYLOAD_TYPE_FEC } else { RTP_PAYLOAD_TYPE_AUDIO },
        sequence_number: sequence, timestamp: u32::from(sequence) * 5, ssrc: 7,
    };
    match fec_index {
        None => ParsedPacket::Audio { rtp, payload: payload(sequence) },
        Some(index) => {
            let data: [Vec<u8>; 4] = array::from_fn(|offset| payload(sequence.wrapping_add(offset as u16)));
            let mut parity = [vec![0u8; 16], vec![0u8; 16]];
            let [first, second] = &mut parity;
            fec::encode_audio_block(array::from_fn(|index| data[index].as_slice()), [first, second]).unwrap();
            ParsedPacket::Fec {
                fec: AudioFecHeader { fec_shard_index: index as u8, payload_type: RTP_PAYLOAD_TYPE_AUDIO,
                    base_sequence_number: sequence, base_timestamp: u32::from(sequence) * 5, ssrc: 7 },
                payload: parity[index].clone(),
            }
        }
    }
}

fn trace(events: &str) -> String {
    let mut queue = RtpAudioQueue::new(5);
    let mut output = String::new();
    for event in events.split_whitespace() {
        // 独立 C 驱动冻结时钟. 测试同步冻结已有块的经过时长, 不依赖机器调度速度.
        for block in &mut queue.blocks { block.queue_time = Instant::now() + Duration::from_secs(3600); }
        queue.add_packet(packet(event)).unwrap();
        for block in &mut queue.blocks { block.queue_time = Instant::now() + Duration::from_secs(3600); }
        while let Some(frame) = queue.dequeue_ready() {
            match frame {
                QueuedAudioFrame::Missing => output.push_str("missing"),
                QueuedAudioFrame::Encoded(bytes) => {
                    for byte in bytes { write!(&mut output, "{byte:02x}").unwrap(); }
                }
            }
            output.push(',');
        }
        output.push(';');
    }
    output
}

#[test]
fn queue_matches_independent_upstream_c_traces() {
    let mut compared = 0;
    let mut eager_recovery = 0;
    for line in VECTORS.lines() {
        let mut fields = line.split('\t');
        let name = fields.next().unwrap();
        let events = fields.next().unwrap();
        let expected = fields.next().unwrap();
        assert!(fields.next().is_none());
        if name == "startup-wrap-difference" { continue; }
        let actual = trace(events);
        let loss_case = name.starts_with("loss-");
        let tokens: Vec<_> = events.split_whitespace().collect();
        let previous_data: Vec<u16> = tokens.iter().skip(1).take(3)
            .filter_map(|event| event.strip_prefix('a').map(|value| value.parse().unwrap())).collect();
        let next_expected = (4..8u16).find(|sequence| !previous_data.contains(sequence)).unwrap_or(8);
        let last_is_expected = tokens.last().unwrap().strip_prefix('a')
            .is_some_and(|value| value.parse::<u16>().unwrap() == next_expected);
        let loss_mask = if loss_case { u8::from_str_radix(&name[5..7], 16).unwrap() } else { 0 };
        if loss_case && last_is_expected && loss_mask & 0x0f != 0 {
            // C 的顺序包快速返回跳过 completeFecBlock. Rust 应立刻补齐剩余数据,
            // 但此前每个事件和该事件已交付的前缀仍必须逐字节一致.
            let delivered = expected.matches(',').count();
            assert!(delivered < 4);
            let mut corrected = expected.strip_suffix(';').unwrap().to_string();
            for sequence in (4 + delivered as u16)..8 {
                for byte in payload(sequence) { write!(&mut corrected, "{byte:02x}").unwrap(); }
                corrected.push(',');
            }
            corrected.push(';');
            assert_eq!(actual, corrected, "顺序包触发 FEC 的差异超出预期: {name}");
            eager_recovery += 1;
        } else {
            assert_eq!(actual, expected, "上游队列轨迹不一致: {name}, {events}");
            compared += 1;
        }
        if loss_case {
            let flattened = actual.replace(';', "");
            let mut complete = String::new();
            for sequence in 4..8 {
                for byte in payload(sequence) { write!(&mut complete, "{byte:02x}").unwrap(); }
                complete.push(',');
            }
            assert_eq!(flattened, complete, "4 个有效分片必须恢复完整数据: {name}");
        }
    }
    assert_eq!(compared, 273);
    assert_eq!(eager_recovery, 96);
}

#[test]
fn explicit_initialization_state_fixes_upstream_wrap_sentinel() {
    let line = VECTORS.lines().find(|line| line.starts_with("startup-wrap-difference\t")).unwrap();
    let mut fields = line.split('\t');
    fields.next();
    let events = fields.next().unwrap();
    let upstream = fields.next().unwrap();
    let actual = trace(events);
    // 原 C 实现把 oldestRtpBaseSequenceNumber == 0 当成未初始化, 丢弃回绕后的首块.
    assert_eq!(upstream.matches(',').count(), 1);
    assert_eq!(actual.matches(',').count(), 5);
    let mut expected = String::from(";");
    for sequence in 0..5 {
        for byte in payload(sequence) { write!(&mut expected, "{byte:02x}").unwrap(); }
        expected.push_str(",;");
    }
    assert_eq!(actual, expected);
    assert_ne!(actual, upstream);
}
