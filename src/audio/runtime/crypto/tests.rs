use super::*;

fn pair() -> (AudioEncryptor, AudioDecryptor) {
    let secret = derive_channel_secret([7; 32], [9; 32]).unwrap();
    (AudioEncryptor::new(secret, AudioChannelDirection::HostToClient).unwrap(),
     AudioDecryptor::new(secret, AudioChannelDirection::HostToClient).unwrap())
}

#[test]
fn authenticated_packets_preserve_payload_and_reject_duplicate_counters() {
    let (mut sender, mut receiver) = pair();
    let first = sender.encrypt(b"first").unwrap();
    let second = sender.encrypt(b"second").unwrap();
    assert_eq!(receiver.decrypt(&second).unwrap(), b"second");
    assert_eq!(receiver.decrypt(&first).unwrap(), b"first");
    assert!(receiver.decrypt(&first).is_err());
    assert!(receiver.decrypt(&second).is_err());
}

#[test]
fn window_accepts_sixty_three_packets_of_reordering_but_not_sixty_four() {
    let (mut sender, mut receiver) = pair();
    let packets: Vec<_> = (0..130).map(|value| sender.encrypt(&[value]).unwrap()).collect();
    assert_eq!(receiver.decrypt(&packets[64]).unwrap(), [64]);
    assert_eq!(receiver.decrypt(&packets[1]).unwrap(), [1]);
    assert!(receiver.decrypt(&packets[0]).is_err());
    assert!(receiver.decrypt(&packets[1]).is_err());
    // 跃迁超过整窗口后不保留旧位, 新窗口内尚未收到的包仍可接收.
    assert_eq!(receiver.decrypt(&packets[129]).unwrap(), [129]);
    assert_eq!(receiver.decrypt(&packets[66]).unwrap(), [66]);
    assert!(receiver.decrypt(&packets[65]).is_err());
}

#[test]
fn forged_counter_tag_and_body_do_not_advance_the_window() {
    let (mut sender, mut receiver) = pair();
    let packet = sender.encrypt(b"rtp-data").unwrap();
    for index in [0, 7, 8, packet.len() - 1] {
        let mut forged = packet.clone();
        forged[index] ^= 0x80;
        assert!(receiver.decrypt(&forged).is_err());
        assert_eq!(receiver.highest_counter, None);
    }
    for length in 0..packet.len() {
        assert!(receiver.decrypt(&packet[..length]).is_err());
    }
    assert_eq!(receiver.decrypt(&packet).unwrap(), b"rtp-data");
    let next = sender.encrypt(b"next").unwrap();
    let mut forged = next.clone();
    forged[..8].copy_from_slice(&u64::MAX.to_be_bytes());
    assert!(receiver.decrypt(&forged).is_err());
    assert_eq!(receiver.highest_counter, Some(0));
    assert_eq!(receiver.decrypt(&next).unwrap(), b"next");
}

#[test]
fn fresh_channels_and_directions_have_distinct_keys() {
    let first_secret = derive_channel_secret([1; 32], [2; 32]).unwrap();
    let second_secret = derive_channel_secret([1; 32], [3; 32]).unwrap();
    assert_ne!(first_secret, second_secret);
    assert_ne!(first_secret, derive_channel_secret([2; 32], [2; 32]).unwrap());
    let mut old_sender = AudioEncryptor::new(first_secret, AudioChannelDirection::HostToClient).unwrap();
    let old_packet = old_sender.encrypt(b"same-payload").unwrap();
    let mut sender = AudioEncryptor::new(second_secret, AudioChannelDirection::HostToClient).unwrap();
    let packet = sender.encrypt(b"same-payload").unwrap();
    // 两次计数器都从 0 开始, 比较不含计数器的实际密文.
    assert_eq!(&packet[..8], &old_packet[..8]);
    assert_ne!(&packet[8..], &old_packet[8..]);
    let mut receiver = AudioDecryptor::new(second_secret, AudioChannelDirection::HostToClient).unwrap();
    assert!(receiver.decrypt(&old_packet).is_err());
    assert!(receiver.highest_counter.is_none());
    assert_eq!(receiver.decrypt(&packet).unwrap(), b"same-payload");
    let mut opposite = AudioDecryptor::new(second_secret, AudioChannelDirection::ClientToHost).unwrap();
    assert!(opposite.decrypt(&packet).is_err());
}

#[test]
fn exhausted_nonce_counter_never_wraps_or_encrypts_again() {
    let (mut sender, mut receiver) = pair();
    sender.counter = u64::MAX - 1;
    let last = sender.encrypt(b"last").unwrap();
    assert_eq!(receiver.decrypt(&last).unwrap(), b"last");
    for _ in 0..2 {
        assert!(sender.encrypt(b"must-not-wrap").is_err());
        assert_eq!(sender.counter, u64::MAX);
    }
}
