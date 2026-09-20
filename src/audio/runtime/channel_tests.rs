use super::*;
use super::crypto::{AudioDecryptor, AudioEncryptor, derive_channel_secret};
use super::queue::FrameQueue;
use crate::audio::protocol::{RTP_PAYLOAD_TYPE_AUDIO, RtpHeader, write_audio_packet};
use crate::audio::receiver::{AudioDepacketizer, QueuedAudioFrame};
use std::sync::Arc;

#[test]
fn invalid_codec_is_rejected_before_registering_or_spawning_channel() {
    // 故意不创建 Tokio runtime. 若无效参数到达 socket 注册或 spawn, 测试会 panic.
    let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
    for duration in [0, 1, 3, 120, u32::MAX] {
        for direction in [AudioChannelDirection::HostToClient, AudioChannelDirection::ClientToHost] {
            let receiver = bind_and_spawn_receiver_with_config(
                [7; 32], direction, peer,
                CodecConfig { packet_duration_ms: duration, ..CodecConfig::default() },
            );
            let error = receiver.err().expect("无效接收参数必须同步失败");
            assert!(matches!(error.downcast_ref::<crate::audio::error::Error>(),
                Some(crate::audio::error::Error::InvalidConfig(_))));
            let sender = spawn_sender_with_config(
                [7; 32], [8; 32], direction, SocketAddr::new(peer, 9),
                CodecConfig { packet_duration_ms: duration, ..CodecConfig::default() },
            );
            let error = sender.err().expect("无效发送参数必须同步失败");
            assert!(matches!(error.downcast_ref::<crate::audio::error::Error>(),
                Some(crate::audio::error::Error::InvalidConfig(_))));
        }
    }
}

#[tokio::test]
async fn receiver_rebinding_rotates_keys_before_any_udp_packet() {
    let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let (_first, old_secret, old_id) = prepare_receiver([7; 32], peer).unwrap();
    let (_second, new_secret, new_id) = prepare_receiver([7; 32], peer).unwrap();
    assert_ne!(old_id, new_id);
    assert_ne!(old_secret, new_secret);
    assert_eq!(new_secret, derive_channel_secret([7; 32], new_id).unwrap());
    let mut sender = AudioEncryptor::new(old_secret, AudioChannelDirection::HostToClient).unwrap();
    let packet = sender.encrypt(b"old-channel").unwrap();
    let mut receiver = AudioDecryptor::new(new_secret, AudioChannelDirection::HostToClient).unwrap();
    assert!(receiver.decrypt(&packet).is_err());
}

fn audio(sequence_number: u16, payload: &[u8]) -> Vec<u8> {
    write_audio_packet(RtpHeader {
        packet_type: RTP_PAYLOAD_TYPE_AUDIO, sequence_number,
        timestamp: u32::from(sequence_number) * 5, ssrc: 7,
    }, payload)
}

#[tokio::test]
async fn stale_channel_cannot_bind_peer_or_advance_the_rtp_queue() {
    let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let (_old_socket, old_secret, _) = prepare_receiver([7; 32], peer).unwrap();
    let (socket, secret, channel_id) = prepare_receiver([7; 32], peer).unwrap();
    let address = SocketAddr::new(peer, socket.local_addr().unwrap().port());
    let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let stale_sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let mut old_cipher = AudioEncryptor::new(old_secret, AudioChannelDirection::HostToClient).unwrap();
    let mut cipher = AudioEncryptor::new(derive_channel_secret([7; 32], channel_id).unwrap(), AudioChannelDirection::HostToClient).unwrap();
    let queue = Arc::new(FrameQueue::new("channel-test", 30));
    let stop = CancellationToken::new();
    let task = tokio::spawn(receive::receive_packets(
        socket, Arc::clone(&queue), stop.clone(),
        AudioDecryptor::new(secret, AudioChannelDirection::HostToClient).unwrap(),
        peer, AudioDepacketizer::new(5, 0),
    ));
    let check = tokio::time::timeout(Duration::from_secs(2), async {
        stale_sender.send_to(&old_cipher.encrypt(&audio(0, b"stale")).unwrap(), address).await.unwrap();
        // RTP 队列按上游规则跳过首个不完整 FEC 块, 从下一块开始交付.
        sender.send_to(&cipher.encrypt(&audio(0, b"startup")).unwrap(), address).await.unwrap();
        sender.send_to(&cipher.encrypt(&audio(4, b"current")).unwrap(), address).await.unwrap();
        assert!(matches!(queue.pop().await, Some(QueuedAudioFrame::Encoded(bytes)) if bytes == b"current"));
        // 首个有效包绑定源端口后, 同 IP 的其他端口也不能注入合法计数器.
        let next = cipher.encrypt(&audio(5, b"next---")).unwrap();
        stale_sender.send_to(&next, address).await.unwrap();
        sender.send_to(&next, address).await.unwrap();
        assert!(matches!(queue.pop().await, Some(QueuedAudioFrame::Encoded(bytes)) if bytes == b"next---"));
    }).await;
    stop.cancel();
    task.await.unwrap().unwrap();
    assert!(check.is_ok());
}
