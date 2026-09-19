use super::*;

fn params(frame_ms: u32) -> StreamParams {
    StreamParams {
        sample_rate: 48_000,
        channels: 2,
        streams: 1,
        coupled_streams: 1,
        mapping: [0, 1, 0, 0, 0, 0, 0, 0],
        bitrate: 96_000,
        packet_duration_ms: frame_ms,
    }
}

fn playback(frame_ms: u32) -> SharedSampleRing {
    let budget = QueueBudget::from_stream(&params(frame_ms)).unwrap();
    SharedSampleRing::new(
        budget.playback_samples,
        budget.channels,
        budget.frame_samples,
        budget.playback_watermark,
        "测试播放队列已关闭",
    ).unwrap()
}

#[test]
fn playback_restart_is_reported_to_runtime_instead_of_reopening_in_place() {
    for result in [Ok(ThreadRunState::Restart), Err(Error::Backend("模拟 WASAPI 失败".into()))] {
        let ring = playback(5);
        finish_playback_stream(result, &ring);
        assert!(ring.lock_state().unwrap().closed);
        assert!(ring.lock_state().unwrap().error.is_some());
        assert!(ring.write_blocking(&vec![0.0; ring.frame_samples], Duration::ZERO).is_err());
    }
    let ring = playback(5);
    finish_playback_stream(Ok(ThreadRunState::Stop), &ring);
    assert!(ring.lock_state().unwrap().closed);
    assert!(ring.lock_state().unwrap().error.is_none());
}

#[test]
fn budgets_preserve_five_and_sixty_millisecond_frames() {
    let short = QueueBudget::from_stream(&params(5)).unwrap();
    assert_eq!((short.frame_samples, short.capture_samples), (480, 2880));
    assert_eq!((short.playback_watermark, short.playback_samples), (4800, 5280));
    let long = QueueBudget::from_stream(&params(60)).unwrap();
    assert_eq!((long.frame_samples, long.capture_samples), (5760, 5760));
    assert_eq!((long.playback_watermark, long.playback_samples), (4800, 10560));
}

#[test]
fn invalid_formats_and_unaligned_capacities_are_rejected() {
    for duration in [0, 1, 7, u32::MAX] {
        assert!(QueueBudget::from_stream(&params(duration)).is_err());
    }
    for rate in [0, 44_101, u32::MAX] {
        let mut stream = params(5);
        stream.sample_rate = rate;
        assert!(QueueBudget::from_stream(&stream).is_err());
    }
    let mut stream = params(5);
    stream.channels = 0;
    assert!(QueueBudget::from_stream(&stream).is_err());
    assert!(SharedSampleRing::new(7, 2, 2, 6, "test").is_err());
    assert!(SharedSampleRing::new(8, 2, 3, 6, "test").is_err());
    assert!(SharedSampleRing::new(8, 0, 2, 6, "test").is_err());
    assert!(SharedSampleRing::new(8, 2, 2, 7, "test").is_err());
}

#[test]
fn fifty_millisecond_watermark_accepts_one_whole_negotiated_frame() {
    for frame_ms in [5, 60] {
        let ring = playback(frame_ms);
        ring.write_silence_overwrite(ring.playback_watermark);
        let frame = vec![1.0; ring.frame_samples];
        ring.write_blocking(&frame, Duration::ZERO).unwrap();
        assert_eq!(ring.lock_state().unwrap().len, ring.playback_watermark + ring.frame_samples);
        assert!(ring.write_blocking(&frame, Duration::ZERO).is_err());
    }
}

#[test]
fn sixty_millisecond_backlog_blocks_even_when_capacity_is_available() {
    let ring = SharedSampleRing::new(12_000, 2, 5760, 4800, "test").unwrap();
    ring.write_blocking(&vec![1.0; 5760], Duration::ZERO).unwrap();
    // 故意使用更大容量, 验证不能仅凭剩余空间绕过 50 ms 软件水位.
    ring.read_partial_zero_fill(&mut vec![0.0; 958]);
    assert_eq!(ring.lock_state().unwrap().len, 4802);
    assert!(ring.lock_state().unwrap().available() >= 5760);
    assert!(ring.write_blocking(&vec![2.0; 5760], Duration::ZERO).is_err());
    ring.read_partial_zero_fill(&mut [0.0; 2]);
    ring.write_blocking(&vec![3.0; 5760], Duration::ZERO).unwrap();
}

#[test]
fn submission_size_is_checked_even_while_recovering() {
    let ring = playback(5);
    for size in [0, 1, 2, 479, 481, 960] {
        assert!(ring.write_blocking(&vec![0.0; size], Duration::ZERO).is_err());
    }
    ring.begin_recovery().unwrap();
    assert!(ring.write_blocking(&[0.0; 2], Duration::ZERO).is_err());
    ring.write_blocking(&vec![0.0; 480], Duration::ZERO).unwrap();
}

fn start_waiting_writer(ring: &Arc<SharedSampleRing>) -> (JoinHandle<Result<()>>, mpsc::Receiver<Duration>) {
    let (tx, rx) = mpsc::channel();
    ring.lock_state().unwrap().writer_wait_hook = Some(tx);
    let writer_ring = Arc::clone(ring);
    let writer = thread::spawn(move || {
        writer_ring.write_blocking(&vec![2.0; writer_ring.frame_samples], Duration::from_secs(10))
    });
    (writer, rx)
}

#[test]
fn consumption_wakes_a_real_backpressured_writer() {
    let ring = Arc::new(playback(5));
    ring.write_silence_overwrite(5280);
    let (writer, waiting) = start_waiting_writer(&ring);
    let requested_wait = waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(requested_wait <= MAX_PLAYBACK_WAIT);
    // 接收钩子后再获取 ring 锁, 保证生产者已进入真正的条件变量等待.
    let mut consumed = [0.0; 480];
    ring.read_partial_zero_fill(&mut consumed);
    writer.join().unwrap().unwrap();
    assert_eq!(ring.lock_state().unwrap().len, 5280);
}

#[test]
fn blocked_old_writer_is_dropped_even_if_recovery_finishes_before_it_wakes() {
    let ring = Arc::new(playback(5));
    ring.write_silence_overwrite(5280);
    let (writer, waiting) = start_waiting_writer(&ring);
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    ring.begin_recovery().unwrap();
    ring.finish_recovery().unwrap();
    writer.join().unwrap().unwrap();
    let state = ring.lock_state().unwrap();
    assert_eq!(state.len, 0);
    assert_eq!(state.dropped_samples, 5280 + 480);
}

#[test]
fn close_wakes_blocked_writer_and_wins_over_recovery() {
    let ring = Arc::new(playback(5));
    ring.write_silence_overwrite(5280);
    let (writer, waiting) = start_waiting_writer(&ring);
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    ring.close(None);
    assert!(ring.finish_recovery().is_err());
    assert!(writer.join().unwrap().is_err());
}

#[test]
fn playback_wait_is_capped_and_does_not_enqueue_on_timeout() {
    let ring = Arc::new(playback(5));
    ring.write_silence_overwrite(5280);
    let (writer, waiting) = start_waiting_writer(&ring);
    assert!(waiting.recv_timeout(Duration::from_secs(2)).unwrap() <= MAX_PLAYBACK_WAIT);
    assert!(writer.join().unwrap().is_err());
    let state = ring.lock_state().unwrap();
    assert_eq!(state.len, 5280);
    assert_eq!(state.dropped_samples, 480);
}

#[test]
fn capture_overflow_retains_latest_complete_channel_frames() {
    let ring = SharedSampleRing::new(8, 2, 2, 8, "test").unwrap();
    ring.write_overwrite(&[1.0, 101.0, 2.0, 102.0, 3.0, 103.0]);
    ring.read_partial_zero_fill(&mut [0.0; 2]);
    ring.write_overwrite(&[4.0, 104.0, 5.0, 105.0, 6.0, 106.0]);
    let mut output = [0.0; 8];
    ring.read_exact(&mut output, Duration::ZERO).unwrap();
    assert_eq!(output, [3.0, 103.0, 4.0, 104.0, 5.0, 105.0, 6.0, 106.0]);
    assert_eq!(ring.lock_state().unwrap().dropped_samples, 2);
    ring.write_overwrite(&[0.0, 100.0, 1.0, 101.0, 2.0, 102.0, 3.0, 103.0, 4.0, 104.0]);
    ring.read_exact(&mut output, Duration::ZERO).unwrap();
    assert_eq!(output, [1.0, 101.0, 2.0, 102.0, 3.0, 103.0, 4.0, 104.0]);
    assert_eq!(ring.lock_state().unwrap().dropped_samples, 4);
}

#[test]
fn silence_overflow_and_malformed_input_preserve_alignment() {
    let ring = SharedSampleRing::new(8, 2, 2, 8, "test").unwrap();
    ring.write_overwrite(&[1.0, 101.0]);
    ring.write_silence_overwrite(10);
    assert_eq!(ring.lock_state().unwrap().dropped_samples, 4);
    ring.write_overwrite(&[999.0]);
    let mut output = [1.0; 9];
    assert_eq!(ring.read_partial_zero_fill(&mut output), 8);
    assert_eq!(output, [0.0; 9]);
    let state = ring.lock_state().unwrap();
    assert_eq!(state.dropped_samples, 5);
    assert_eq!(state.high_water_samples, 8);
}

#[test]
fn sixty_millisecond_capture_preserves_six_device_packets_and_remainder() {
    let budget = QueueBudget::from_stream(&params(60)).unwrap();
    let ring = SharedSampleRing::new(
        budget.capture_samples, budget.channels, budget.frame_samples,
        budget.capture_samples, "test",
    ).unwrap();
    ring.configure_capture_packet(512, 48_000).unwrap();
    assert_eq!(ring.lock_state().unwrap().buffer.len(), (2880 + 512) * 2);
    let mut frame = vec![0.0; budget.frame_samples];
    for packet_index in 0..6 {
        let packet: Vec<f32> = (packet_index * 1024..(packet_index + 1) * 1024)
            .map(|sample| sample as f32).collect();
        ring.write_overwrite(&packet);
        if packet_index < 5 {
            assert!(!ring.read_exact(&mut frame, Duration::ZERO).unwrap());
        }
    }
    assert!(ring.read_exact(&mut frame, Duration::ZERO).unwrap());
    assert_eq!(frame, (0..5760).map(|sample| sample as f32).collect::<Vec<_>>());
    let mut remainder = vec![0.0; (3072 - 2880) * 2];
    assert!(ring.read_exact(&mut remainder, Duration::ZERO).unwrap());
    assert_eq!(remainder, (5760..6144).map(|sample| sample as f32).collect::<Vec<_>>());
    let state = ring.lock_state().unwrap();
    assert_eq!(state.len, 0);
    assert_eq!(state.dropped_samples, 0);
}

#[test]
fn capture_resize_accounts_for_packets_larger_than_negotiated_frame() {
    let budget = QueueBudget::from_stream(&params(5)).unwrap();
    let ring = SharedSampleRing::new(
        budget.capture_samples, budget.channels, budget.frame_samples,
        budget.capture_samples, "test",
    ).unwrap();
    ring.configure_capture_packet(16, 48_000).unwrap();
    assert_eq!(ring.lock_state().unwrap().buffer.len(), 2880);
    ring.configure_capture_packet(4096, 48_000).unwrap();
    assert_eq!(ring.lock_state().unwrap().buffer.len(), (240 + 4096) * 2);
    ring.write_silence_overwrite(4096 * 2);
    assert_eq!(ring.lock_state().unwrap().dropped_samples, 0);
}

#[test]
fn capture_resize_clears_old_audio_without_reopening_recovery_or_closed_ring() {
    let ring = SharedSampleRing::new(8, 2, 2, 8, "test").unwrap();
    ring.write_overwrite(&[1.0, 2.0]);
    ring.configure_capture_packet(4, 48_000).unwrap();
    {
        let state = ring.lock_state().unwrap();
        assert_eq!(state.len, 0);
        assert_eq!(state.dropped_samples, 2);
        assert_eq!(state.generation, 0);
        assert!(!state.recovering);
    }
    ring.begin_recovery().unwrap();
    let generation = ring.lock_state().unwrap().generation;
    ring.configure_capture_packet(4096, 48_000).unwrap();
    {
        let state = ring.lock_state().unwrap();
        assert!(state.recovering);
        assert_eq!(state.generation, generation);
        assert_eq!(state.len, 0);
    }
    ring.close(None);
    assert!(ring.configure_capture_packet(512, 48_000).is_err());
    assert!(ring.configure_capture_packet(0, 48_000).is_err());
}

#[test]
fn dropped_counter_saturates() {
    let ring = playback(5);
    ring.lock_state().unwrap().dropped_samples = u64::MAX - 1;
    ring.begin_recovery().unwrap();
    ring.write_silence_overwrite(480);
    assert_eq!(ring.lock_state().unwrap().dropped_samples, u64::MAX);
}
