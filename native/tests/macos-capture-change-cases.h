// 属性通知的集成用例, 复用生命周期测试翻译单元中的内存设备和故障注入器.
static void test_capture_property_invalidation(void) {
  const AudioObjectPropertySelector selectors[] = {
    kAudioHardwarePropertyDefaultOutputDevice, kAudioDevicePropertyStreams,
    kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyDeviceIsAlive, kAudioStreamPropertyVirtualFormat,
  };
  const uint32_t reasons[] = {
    AR_AUDIO_CHANGE_ROUTE, AR_AUDIO_CHANGE_STREAMS, AR_AUDIO_CHANGE_BUFFER,
    AR_AUDIO_CHANGE_ALIVE, AR_AUDIO_CHANGE_FORMAT,
  };
  for (unsigned changed = 0; changed < 5; changed++) {
    reset_test();
    @autoreleasepool {
      void *handle = ar_macos_capture_create(NULL, 48000, 2, 240);
      assert(handle != NULL && watch_add_calls == 5);
      for (unsigned index = 0; index < 5; index++) {
        assert(watched_addresses[index].mSelector == selectors[index]);
        assert(watched_addresses[index].mScope == (index == 1 ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeGlobal));
        assert(watched_objects[index] == (index == 0 ? kAudioObjectSystemObject : index == 4 ? 103 : 102));
      }
      ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *)handle;
      AudioObjectPropertyAddress unrelated = {.mSelector = kAudioDevicePropertyNominalSampleRate};
      fake_listeners[0](1, &unrelated);
      assert(ar_audio_change_reasons(capture->state.changes) == 0);
      float frame[480] = {0};
      ar_capture_ring_write(&capture->state.ring, frame, 480);
      uint32_t consumed = atomic_load(&capture->state.ring.read_cursor);
      notify_watch(changed);
      assert(ar_audio_change_reasons(capture->state.changes) == reasons[changed]);
      // 一半路径由下一次 IOProc 检测, 另一半在无新回调时由工作线程检测.
      if (changed % 2 == 0) {
        AudioTimeStamp time = {0};
        AudioBufferList empty = {0};
        assert(capture_proc(102, &time, &empty, &time, &empty, &time, capture_context) == noErr);
      }
      assert(ar_macos_capture_read(handle, frame, 480, 0) == -1);
      assert(atomic_load(&capture->state.ring.failure) == AR_CAPTURE_CHANGED);
      assert(atomic_load(&capture->state.ring.read_cursor) == consumed);
      assert(ar_macos_capture_destroy(handle) == 0);
      assert(watch_remove_calls == 5 && ar_macos_audio_cleanup_failure() == 0);
      cleanup_count = 0;
      void *fresh = ar_macos_capture_create(NULL, 48000, 2, 240);
      assert(fresh != NULL && ar_macos_capture_read(fresh, frame, 480, 0) == 1);
      assert(ar_macos_capture_destroy(fresh) == 0);
    }
    reset_test();
  }
  reset_test();
  atomic_store(&virtual_now, 10000000000ULL);
  @autoreleasepool {
    void *handle = ar_macos_capture_create(NULL, 48000, 2, 240);
    assert(handle != NULL);
    float frame[480];
    notify_while_waiting = 5;
    assert(ar_macos_capture_read(handle, frame, 480, 200) == -1);
    ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *)handle;
    assert(atomic_load(&capture->state.ring.failure) == AR_CAPTURE_CHANGED);
    assert(ar_macos_capture_destroy(handle) == 0);
  }
  atomic_store(&virtual_now, 0);
  reset_test();
}

static void test_capture_property_registration_failures(void) {
  for (unsigned index = 1; index <= 5; index++) {
    reset_test();
    @autoreleasepool {
      fail_watch_add = index;
      assert(ar_macos_capture_create(NULL, 48000, 2, 240) == NULL);
      assert(watch_add_calls == index && watch_remove_calls == index - 1);
      assert(ar_macos_audio_cleanup_failure() == 0);
    }
    reset_test();
    @autoreleasepool {
      // 注册期间通知也不能丢失, 后续旧格式初始化不得启动成功.
      notify_watch_add = index;
      assert(ar_macos_capture_create(NULL, 48000, 2, 240) == NULL);
      assert(watch_add_calls == 5 && watch_remove_calls == 5);
      assert(ar_macos_audio_cleanup_failure() == 0);
    }
    reset_test();
    @autoreleasepool {
      void *handle = ar_macos_capture_create(NULL, 48000, 2, 240);
      assert(handle != NULL);
      fail_watch_remove = index;
      assert(ar_macos_capture_destroy(handle) == -21002);
      assert(watch_remove_calls == index && cleanup_count == 0);
      assert(g_quarantined_capture == handle && ar_macos_audio_cleanup_failure() == -21002);
      assert_reopen_rejected();
      late_capture_callback();
      // 注销失败后的任何通知仍只能修改存活的信号对象.
      for (unsigned active = 0; active < 5; active++) if (fake_listeners[active] != nil) notify_watch(active);
      release_quarantined();
    }
    reset_test();
  }
  @autoreleasepool {
    // 部分注册失败后清理再失败, 无 IOProc/ring 的 owner 也进入安全隔离.
    fail_watch_add = 3;
    fail_watch_remove = 1;
    assert(ar_macos_capture_create(NULL, 48000, 2, 240) == NULL);
    assert(g_quarantined_capture != NULL && live_allocations == 0);
    assert_reopen_rejected();
    release_quarantined();
  }
  reset_test();
}

typedef struct {
  AudioObjectPropertyListenerBlock __unsafe_unretained block;
  _Atomic bool entered;
} ChangeRace;
static void *notify_during_capture_destruction(void *context) {
  ChangeRace *race = context;
  AudioObjectPropertyAddress format = {.mSelector = kAudioStreamPropertyVirtualFormat};
  atomic_store(&race->entered, true);
  for (unsigned index = 0; index < 20000; index++) race->block(1, &format);
  return NULL;
}
static void test_delayed_property_block_lifetime(void) {
  reset_test();
  __weak ARAudioChangeSignal *weakSignal;
  __weak ARAudioChanges *weakOwner;
  AudioObjectPropertyListenerBlock delayed;
  @autoreleasepool {
    void *handle = ar_macos_capture_create(NULL, 48000, 2, 240);
    assert(handle != NULL);
    ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *)handle;
    weakOwner = capture->propertyChanges;
    weakSignal = capture->propertyChanges->signal;
    // 模拟已排队/在途通知自己持有 block, 不把 Remove 当作回调完成屏障.
    delayed = [fake_listeners[0] copy];
    ChangeRace race = {.block = delayed};
    pthread_t notifier;
    assert(pthread_create(&notifier, NULL, notify_during_capture_destruction, &race) == 0);
    wait_for_test_flag(&race.entered);
    assert(ar_macos_capture_destroy(handle) == 0);
    // 将最后一个测试局部强引用也释放, 并发通知不得继续持有 capture/ring/converter.
    capture = nil;
    assert(pthread_join(notifier, NULL) == 0);
  }
  assert(weakOwner == nil && live_allocations == 0);
  @autoreleasepool {
    ARAudioChangeSignal *signal = weakSignal;
    assert(signal != nil);
    AudioObjectPropertyAddress route = {.mSelector = kAudioHardwarePropertyDefaultOutputDevice};
    delayed(1, &route);
    uint32_t reasons = ar_audio_change_reasons(&signal->state);
    assert((reasons & (AR_AUDIO_CHANGE_ROUTE | AR_AUDIO_CHANGE_FORMAT)) == (AR_AUDIO_CHANGE_ROUTE | AR_AUDIO_CHANGE_FORMAT));
  }
  delayed = nil;
  assert(weakSignal == nil);
  reset_test();
}

static void test_capture_property_changes(void) {
  test_capture_property_invalidation();
  test_capture_property_registration_failures();
  test_delayed_property_block_lifetime();
}
