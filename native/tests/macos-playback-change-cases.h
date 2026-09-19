// 播放通知集成测试复用生命周期 fixture, 不打开真实 AudioQueue 或默认输出设备.
static void test_playback_route_change(void) {
  for (unsigned via_callback = 0; via_callback < 2; via_callback++) {
    reset_test();
    void *handle = ar_macos_playback_create(48000, 2, 240);
    assert(handle != NULL && watch_add_calls == 1);
    assert(watched_objects[0] == kAudioObjectSystemObject);
    assert(watched_addresses[0].mSelector == kAudioHardwarePropertyDefaultOutputDevice);
    assert(watched_addresses[0].mScope == kAudioObjectPropertyScopeGlobal);
    ARPlaybackEngine *engine = handle;
    float frame[480] = {0};
    assert(ar_macos_playback_submit(handle, frame, 480, 0) == 0);
    uint32_t read_before = atomic_load(&engine->ring.samples.read_cursor);
    uint32_t write_before = atomic_load(&engine->ring.samples.write_cursor);
    unsigned enqueues_before = enqueue_calls;
    notify_watch(0);
    if (via_callback) {
      FakeQueue *queue = &fake_queues[0];
      queue->callback(queue->context, (AudioQueueRef)queue, &queue->buffers[0]);
    }
    assert(ar_macos_playback_submit(handle, frame, 480, 0) == -1);
    assert(atomic_load(&engine->ring.samples.failure) == AR_PLAYBACK_CHANGED);
    assert(atomic_load(&engine->ring.samples.read_cursor) == read_before);
    assert(atomic_load(&engine->ring.samples.write_cursor) == write_before);
    assert(enqueue_calls == enqueues_before);
    assert(ar_macos_playback_destroy(handle) == 0);
    assert(watch_remove_calls == 1 && live_allocations == 0);
    assert(ar_macos_audio_cleanup_failure() == 0);
    handle = ar_macos_playback_create(48000, 2, 240);
    assert(handle != NULL && ar_macos_playback_submit(handle, frame, 480, 0) == 0);
    assert(ar_macos_playback_destroy(handle) == 0);
  }
  reset_test();
  atomic_store(&virtual_now, 10000000000ULL);
  void *handle = ar_macos_playback_create(48000, 2, 240);
  assert(handle != NULL);
  float frame[480] = {0};
  // 55 ms 软件积压让下一次 submit 阻塞. 通知即使没有输出回调也须在预算结束后报告失效.
  for (unsigned index = 0; index < 11; index++) assert(ar_macos_playback_submit(handle, frame, 480, 0) == 0);
  notify_while_waiting = 1;
  virtual_waited = 0;
  assert(ar_macos_playback_submit(handle, frame, 480, 60000) == -1);
  assert(virtual_waited == 100000000);
  assert(atomic_load(&((ARPlaybackEngine *)handle)->ring.samples.failure) == AR_PLAYBACK_CHANGED);
  assert(ar_macos_playback_destroy(handle) == 0);
  atomic_store(&virtual_now, 0);
  reset_test();
}
static void test_playback_route_initialization_and_cleanup(void) {
  reset_test();
  fail_watch_add = 1;
  assert(ar_macos_playback_create(48000, 2, 240) == NULL);
  assert(queue_count == 0 && watch_remove_calls == 0 && live_allocations == 0);
  reset_test();
  notify_watch_add = 1;
  assert(ar_macos_playback_create(48000, 2, 240) == NULL);
  assert(queue_start_calls == 0 && watch_remove_calls == 1 && live_allocations == 0);
  reset_test();
  // 最后检查与 Start 之间到达通知, 也不能在第一帧继续提交到旧实例.
  notify_queue_start = 1;
  void *handle = ar_macos_playback_create(48000, 2, 240);
  assert(handle != NULL);
  float frame[480] = {0};
  assert(ar_macos_playback_submit(handle, frame, 480, 0) == -1);
  assert(ar_macos_playback_destroy(handle) == 0);
  reset_test();
  for (unsigned initialization_failed = 0; initialization_failed < 2; initialization_failed++) {
    if (initialization_failed) fail_primary = Q_START;
    fail_watch_remove = 1;
    handle = ar_macos_playback_create(48000, 2, 240);
    if (initialization_failed) assert(handle == NULL);
    else {
      assert(handle != NULL);
      assert(ar_macos_playback_destroy(handle) == -21002);
    }
    assert(g_quarantined_playback != NULL && live_allocations == 2 && dispose_calls == 0);
    assert(ar_macos_audio_cleanup_failure() == -21002);
    assert_reopen_rejected();
    notify_watch(0);
    FakeQueue *queue = &fake_queues[0];
    queue->callback(queue->context, (AudioQueueRef)queue, &queue->buffers[0]);
    release_quarantined();
    reset_test();
  }
}
static void test_playback_delayed_route_block(void) {
  reset_test();
  __weak ARAudioChanges *weakOwner;
  __weak ARAudioChangeSignal *weakSignal;
  AudioObjectPropertyListenerBlock delayed;
  @autoreleasepool {
    void *handle = ar_macos_playback_create(48000, 2, 240);
    assert(handle != NULL);
    ARPlaybackEngine *engine = handle;
    ARAudioChanges *changes = (__bridge ARAudioChanges *)engine->changes_handle;
    weakOwner = changes;
    weakSignal = changes->signal;
    delayed = [fake_listeners[0] copy];
    ChangeRace race = {.block = delayed};
    pthread_t notifier;
    assert(pthread_create(&notifier, NULL, notify_during_capture_destruction, &race) == 0);
    wait_for_test_flag(&race.entered);
    assert(ar_macos_playback_destroy(handle) == 0);
    changes = nil;
    assert(pthread_join(notifier, NULL) == 0);
  }
  assert(weakOwner == nil && live_allocations == 0);
  @autoreleasepool {
    ARAudioChangeSignal *signal = weakSignal;
    assert(signal != nil);
    AudioObjectPropertyAddress route = {.mSelector = kAudioHardwarePropertyDefaultOutputDevice};
    delayed(1, &route);
    assert(ar_audio_change_reasons(&signal->state) & AR_AUDIO_CHANGE_ROUTE);
  }
  delayed = nil;
  assert(weakSignal == nil);
  reset_test();
}
static void test_duplex_route_signal_isolation(void) {
  reset_test();
  @autoreleasepool {
    void *capture = ar_macos_capture_create(NULL, 48000, 2, 240);
    void *playback = ar_macos_playback_create(48000, 2, 240);
    assert(capture && playback && watch_add_calls == 6);
    float frame[480] = {0};
    notify_watch(0);
    assert(ar_macos_capture_read(capture, frame, 480, 0) == -1);
    assert(ar_macos_playback_submit(playback, frame, 480, 0) == 0);
    assert(ar_macos_capture_destroy(capture) == 0);
    // 同一系统事件向两个注册分别交付, 任一方向清理都不释放另一方向的信号.
    notify_watch(5);
    assert(ar_macos_playback_submit(playback, frame, 480, 0) == -1);
    assert(ar_macos_playback_destroy(playback) == 0);
  }
  reset_test();
}
static void test_playback_property_changes(void) {
  test_playback_route_change();
  test_playback_route_initialization_and_cleanup();
  test_playback_delayed_route_block();
  test_duplex_route_signal_isolation();
}
