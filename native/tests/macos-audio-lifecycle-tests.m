// 故障注入完整原生创建/销毁路径, 不打开设备. 系统对象均由内存替身提供.
#import <AudioToolbox/AudioToolbox.h>
#import <CoreAudio/CoreAudio.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <Foundation/Foundation.h>
#include <assert.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <time.h>
#include <mach/mach.h>
#include <mach/semaphore.h>

enum { MAX_FAKE_WATCHES = 64 };
static AudioObjectPropertyListenerBlock fake_listeners[MAX_FAKE_WATCHES];
static AudioObjectID watched_objects[MAX_FAKE_WATCHES];
static AudioObjectPropertyAddress watched_addresses[MAX_FAKE_WATCHES];
static unsigned watch_add_calls, watch_remove_calls;
static unsigned fail_watch_add, fail_watch_remove, notify_watch_add;
static unsigned notify_while_waiting, notify_queue_start;
static unsigned queue_start_calls;
static void notify_watch(unsigned index) {
  assert(index < MAX_FAKE_WATCHES && fake_listeners[index] != nil);
  fake_listeners[index](1, &watched_addresses[index]);
}
static OSStatus fake_add_listener(AudioObjectID object, const AudioObjectPropertyAddress *address,
                                  dispatch_queue_t queue, AudioObjectPropertyListenerBlock listener) {
  assert(queue == NULL);
  unsigned index = watch_add_calls++;
  assert(index < MAX_FAKE_WATCHES);
  if (watch_add_calls == fail_watch_add) return -21001;
  watched_objects[index] = object;
  watched_addresses[index] = *address;
  fake_listeners[index] = [listener copy];
  if (watch_add_calls == notify_watch_add) notify_watch(index);
  return noErr;
}
static OSStatus fake_remove_listener(AudioObjectID object, const AudioObjectPropertyAddress *address,
                                     dispatch_queue_t queue, AudioObjectPropertyListenerBlock listener) {
  assert(queue == NULL);
  watch_remove_calls++;
  for (unsigned index = 0; index < watch_add_calls; index++) {
    if (fake_listeners[index] == listener && watched_objects[index] == object &&
        memcmp(&watched_addresses[index], address, sizeof(*address)) == 0) {
      // 注销期间仍交付通知. 它只能更新独立信号, 不访问正在关闭的设备资源.
      notify_watch(index);
      if (watch_remove_calls == fail_watch_remove) return -21002;
      fake_listeners[index] = nil;
      return noErr;
    }
  }
  assert(false);
  return -21003;
}

static _Atomic uint64_t virtual_now;
static uint64_t virtual_waited;
static int health_clock_gettime(clockid_t clock, struct timespec *out) {
  uint64_t now = atomic_load(&virtual_now);
  if (now == 0) return clock_gettime(clock, out);
  assert(clock == CLOCK_MONOTONIC);
  out->tv_sec = (time_t)(now / 1000000000);
  out->tv_nsec = (long)(now % 1000000000);
  return 0;
}
static kern_return_t health_timedwait(semaphore_t semaphore, mach_timespec_t duration) {
  if (atomic_load(&virtual_now) == 0) return semaphore_timedwait(semaphore, duration);
  uint64_t elapsed = (uint64_t)duration.tv_sec * 1000000000 + duration.tv_nsec;
  atomic_fetch_add(&virtual_now, elapsed);
  virtual_waited += elapsed;
  if (notify_while_waiting != 0) {
    notify_watch(notify_while_waiting - 1);
    notify_while_waiting = 0;
  }
  return KERN_OPERATION_TIMED_OUT;
}

static _Atomic bool pause_dispose, dispose_entered, continue_dispose, reopen_entered;

enum {
  NONE, Q_NEW, Q_ALLOC_1, Q_ALLOC_2, Q_ALLOC_3, Q_ENQUEUE_1, Q_ENQUEUE_2, Q_ENQUEUE_3,
  Q_START, Q_DISPOSE, TAP_CREATE, AGG_CREATE, STREAM_SIZE, STREAM_QUERY, FORMAT_QUERY,
  BUFFER_QUERY, IO_CREATE, IO_START, IO_STOP, IO_DESTROY, AGG_DESTROY, TAP_DESTROY
};
static int fail_primary, fail_cleanup;
static int allocation_step, fail_allocation, live_allocations;
static unsigned api_calls, enqueue_calls, dispose_calls;
static int cleanup_order[8], cleanup_count;
static OSStatus error_for(int site) { return (OSStatus)(-20000 - site); }
static OSStatus result_for(int site) {
  api_calls++;
  return site != NONE && (site == fail_primary || site == fail_cleanup) ? error_for(site) : noErr;
}
static void *checked_calloc(size_t count, size_t size) {
  if (++allocation_step == fail_allocation) return NULL;
  void *value = calloc(count, size);
  if (value) live_allocations++;
  return value;
}
static void checked_free(void *value) {
  if (value) live_allocations--;
  free(value);
}

typedef struct {
  AudioQueueOutputCallback callback;
  void *context;
  unsigned allocations, enqueues;
  bool disposed;
  float samples[3][8192];
  AudioQueueBuffer buffers[3];
} FakeQueue;
static FakeQueue fake_queues[4];
static unsigned queue_count;
static OSStatus fake_new_output(const AudioStreamBasicDescription *format, AudioQueueOutputCallback callback,
                               void *context, CFRunLoopRef runloop, CFStringRef mode, UInt32 flags, AudioQueueRef *out) {
  (void)format; (void)runloop; (void)mode; (void)flags;
  OSStatus result = result_for(Q_NEW);
  if (result != noErr) return result;
  assert(queue_count < 4);
  FakeQueue *queue = &fake_queues[queue_count++];
  memset(queue, 0, sizeof(*queue));
  queue->callback = callback;
  queue->context = context;
  *out = (AudioQueueRef)queue;
  return noErr;
}
static OSStatus fake_allocate(AudioQueueRef handle, UInt32 bytes, AudioQueueBufferRef *out) {
  FakeQueue *queue = (FakeQueue *)handle;
  unsigned index = queue->allocations++;
  assert(index < 3 && bytes <= sizeof(queue->samples[index]));
  OSStatus result = result_for(Q_ALLOC_1 + (int)index);
  if (result != noErr) return result;
  AudioQueueBuffer buffer = {.mAudioDataBytesCapacity = bytes, .mAudioData = queue->samples[index]};
  memcpy(&queue->buffers[index], &buffer, sizeof(buffer));
  *out = &queue->buffers[index];
  return noErr;
}
static OSStatus fake_enqueue(AudioQueueRef handle, AudioQueueBufferRef buffer, UInt32 count,
                             const AudioStreamPacketDescription *descriptions) {
  (void)buffer; (void)count; (void)descriptions;
  FakeQueue *queue = (FakeQueue *)handle;
  enqueue_calls++;
  unsigned index = queue->enqueues++;
  return result_for(index < 3 ? Q_ENQUEUE_1 + (int)index : NONE);
}
static OSStatus fake_queue_start(AudioQueueRef handle, const AudioTimeStamp *time) {
  (void)handle; (void)time;
  queue_start_calls++;
  if (notify_queue_start != 0) notify_watch(notify_queue_start - 1);
  return result_for(Q_START);
}
static void verify_closed_playback(void *context);
static void wait_for_test_flag(_Atomic bool *flag);
static OSStatus fake_dispose(AudioQueueRef handle, Boolean immediate) {
  assert(immediate);
  FakeQueue *queue = (FakeQueue *)handle;
  verify_closed_playback(queue->context);
  // 同步 dispose 内还可以交付已排队的回调. 上下文必须保持存活且不再 enqueue.
  unsigned before = enqueue_calls;
  AudioQueueBuffer buffer = {.mAudioDataBytesCapacity = sizeof(queue->samples[0]), .mAudioData = queue->samples[0]};
  queue->callback(queue->context, handle, &buffer);
  assert(enqueue_calls == before);
  dispose_calls++;
  if (atomic_load(&pause_dispose)) {
    atomic_store(&dispose_entered, true);
    wait_for_test_flag(&continue_dispose);
  }
  OSStatus result = result_for(Q_DISPOSE);
  if (result == noErr) queue->disposed = true;
  return result;
}

static AudioDeviceIOProc capture_proc;
static void *capture_context;
static bool missing_tap_id, missing_aggregate_id, aggregate_exta;
static OSStatus fake_create_tap(id description, AudioObjectID *out) {
  (void)description;
  OSStatus status = result_for(TAP_CREATE);
  if (status == noErr && !missing_tap_id) *out = 101;
  return status;
}
static OSStatus fake_create_aggregate(CFDictionaryRef description, AudioObjectID *out) {
  (void)description;
  OSStatus status = result_for(AGG_CREATE);
  if (status == noErr && !missing_aggregate_id) *out = 102;
  return status == noErr && aggregate_exta ? 'ExtA' : status;
}
static OSStatus fake_set_property(AudioObjectID object, const AudioObjectPropertyAddress *address,
                                 UInt32 qualifier_size, const void *qualifier, UInt32 size, const void *data) {
  (void)object; (void)address; (void)qualifier_size; (void)qualifier; (void)size; (void)data;
  return noErr;
}
static OSStatus fake_property_size(AudioObjectID object, const AudioObjectPropertyAddress *address,
                                  UInt32 qualifier_size, const void *qualifier, UInt32 *size) {
  (void)object; (void)address; (void)qualifier_size; (void)qualifier;
  *size = sizeof(AudioStreamID);
  return result_for(STREAM_SIZE);
}
static OSStatus fake_property(AudioObjectID object, const AudioObjectPropertyAddress *address,
                             UInt32 qualifier_size, const void *qualifier, UInt32 *size, void *out) {
  (void)object; (void)qualifier_size; (void)qualifier;
  switch (address->mSelector) {
    case kAudioDevicePropertyStreams:
      *(AudioStreamID *)out = 103;
      *size = sizeof(AudioStreamID);
      return result_for(STREAM_QUERY);
    case kAudioStreamPropertyVirtualFormat: {
      AudioStreamBasicDescription format = {0};
      format.mSampleRate = 44100;
      format.mFormatID = kAudioFormatLinearPCM;
      format.mFormatFlags = kAudioFormatFlagsNativeFloatPacked;
      format.mBytesPerFrame = format.mBytesPerPacket = 8;
      format.mFramesPerPacket = 1;
      format.mChannelsPerFrame = 2;
      format.mBitsPerChannel = 32;
      memcpy(out, &format, sizeof(format));
      *size = sizeof(format);
      return result_for(FORMAT_QUERY);
    }
    case kAudioDevicePropertyBufferFrameSize:
      *(UInt32 *)out = 512;
      *size = sizeof(UInt32);
      return result_for(BUFFER_QUERY);
    default: assert(false); return -1;
  }
}
static OSStatus fake_create_proc(AudioObjectID device, AudioDeviceIOProc proc, void *context, AudioDeviceIOProcID *out) {
  assert(device == 102);
  OSStatus status = result_for(IO_CREATE);
  if (status == noErr) {
    capture_proc = proc;
    capture_context = context;
    *out = (AudioDeviceIOProcID)(uintptr_t)104;
  }
  return status;
}
static OSStatus fake_start_proc(AudioObjectID device, AudioDeviceIOProcID proc) {
  (void)device; (void)proc;
  return result_for(IO_START);
}
static void verify_closed_capture(void *context);
static void late_capture_callback(void) {
  if (capture_proc == NULL) return;
  verify_closed_capture(capture_context);
  // 即使输入无效, 关闭后的回调也必须只读存活的关闭状态并立即返回.
  AudioTimeStamp time = {0};
  AudioBufferList empty = {0};
  assert(capture_proc(102, &time, &empty, &time, &empty, &time, capture_context) == noErr);
}
static OSStatus fake_stop_proc(AudioObjectID device, AudioDeviceIOProcID proc) {
  (void)device; (void)proc;
  cleanup_order[cleanup_count++] = IO_STOP;
  late_capture_callback();
  return result_for(IO_STOP);
}
static OSStatus fake_destroy_proc(AudioObjectID device, AudioDeviceIOProcID proc) {
  (void)device; (void)proc;
  cleanup_order[cleanup_count++] = IO_DESTROY;
  late_capture_callback();
  OSStatus status = result_for(IO_DESTROY);
  if (status == noErr) { capture_proc = NULL; capture_context = NULL; }
  return status;
}
static OSStatus fake_destroy_aggregate(AudioObjectID device) {
  assert(device == 102);
  cleanup_order[cleanup_count++] = AGG_DESTROY;
  return result_for(AGG_DESTROY);
}
static OSStatus fake_destroy_tap(AudioObjectID tap) {
  assert(tap == 101);
  cleanup_order[cleanup_count++] = TAP_DESTROY;
  return result_for(TAP_DESTROY);
}
#define calloc checked_calloc
#define free checked_free
#define AudioQueueNewOutput fake_new_output
#define AudioQueueAllocateBuffer fake_allocate
#define AudioQueueEnqueueBuffer fake_enqueue
#define AudioQueueStart fake_queue_start
#define AudioQueueDispose fake_dispose
#define AudioHardwareCreateProcessTap fake_create_tap
#define AudioHardwareCreateAggregateDevice fake_create_aggregate
#define AudioObjectSetPropertyData fake_set_property
#define AudioObjectGetPropertyDataSize fake_property_size
#define AudioObjectGetPropertyData fake_property
#define AudioDeviceCreateIOProcID fake_create_proc
#define AudioDeviceStart fake_start_proc
#define AudioDeviceStop fake_stop_proc
#define AudioDeviceDestroyIOProcID fake_destroy_proc
#define AudioHardwareDestroyAggregateDevice fake_destroy_aggregate
#define AudioHardwareDestroyProcessTap fake_destroy_tap
#define AudioObjectAddPropertyListenerBlock fake_add_listener
#define AudioObjectRemovePropertyListenerBlock fake_remove_listener
#define clock_gettime health_clock_gettime
#define semaphore_timedwait health_timedwait
#include "../macos_audio.m"
#undef clock_gettime
#undef semaphore_timedwait
#undef calloc
#undef free

static void verify_closed_playback(void *context) {
  ARPlaybackEngine *engine = context;
  assert(atomic_load(&engine->ring.samples.closed));
  assert(engine->ring.samples.data != NULL);
}
static void verify_closed_capture(void *context) {
  ARCaptureState *state = context;
  assert(atomic_load(&state->ring.closed));
  assert(state->ring.data != NULL && state->converter.output != NULL);
}
static void reset_test(void) {
  assert(live_allocations == 0);
  assert(g_quarantined_capture == NULL && g_quarantined_playback == NULL);
  for (unsigned index = 0; index < MAX_FAKE_WATCHES; index++) assert(fake_listeners[index] == nil);
  watch_add_calls = watch_remove_calls = 0;
  fail_watch_add = fail_watch_remove = notify_watch_add = notify_while_waiting = 0;
  notify_queue_start = queue_start_calls = 0;
  fail_primary = fail_cleanup = fail_allocation = allocation_step = 0;
  api_calls = enqueue_calls = dispose_calls = queue_count = 0;
  cleanup_count = 0;
  missing_tap_id = missing_aggregate_id = aggregate_exta = false;
  capture_proc = NULL;
  capture_context = NULL;
  atomic_store(&g_audio_cleanup_failure, 0);
}
// 仅测试翻译单元恢复替身 API 后回收隔离对象. 生产代码不提供清除 poison 或重用句柄接口.
static void release_quarantined(void) {
  fail_primary = fail_cleanup = 0;
  fail_watch_remove = 0;
  cleanup_count = 0;
  while (g_quarantined_capture) {
    void *handle = g_quarantined_capture;
    ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *)handle;
    g_quarantined_capture = capture->quarantineNext;
    assert(ar_macos_capture_destroy(handle) == 0);
  }
  while (g_quarantined_playback) {
    ARPlaybackEngine *engine = g_quarantined_playback;
    g_quarantined_playback = engine->quarantine_next;
    assert(ar_macos_playback_destroy(engine) == 0);
  }
}
static void assert_reopen_rejected(void) {
  unsigned before = api_calls;
  unsigned registrations_before = watch_add_calls;
  int allocations_before = allocation_step;
  for (unsigned i = 0; i < 100; i++) {
    assert(ar_macos_playback_create(48000, 2, 240) == NULL);
    assert(ar_macos_capture_create(NULL, 48000, 2, 240) == NULL);
  }
  assert(api_calls == before && allocation_step == allocations_before);
  assert(watch_add_calls == registrations_before);
}

static void test_playback_lifecycle(void) {
  for (int step = 1; step <= 2; step++) {
    reset_test();
    fail_allocation = step;
    assert(ar_macos_playback_create(48000, 2, 240) == NULL);
    assert(api_calls == 0 && live_allocations == 0);
  }
  for (int site = Q_NEW; site <= Q_START; site++) {
    reset_test();
    fail_primary = site;
    assert(ar_macos_playback_create(48000, 2, 240) == NULL);
    assert(live_allocations == 0 && ar_macos_audio_cleanup_failure() == 0);
    assert(dispose_calls == (site == Q_NEW ? 0 : 1));
  }
  reset_test();
  void *handle = ar_macos_playback_create(48000, 2, 240);
  assert(handle != NULL);
  assert(ar_macos_playback_destroy(handle) == 0);
  assert(fake_queues[0].disposed && live_allocations == 0);

  for (int site = Q_ALLOC_1; site <= Q_START; site++) {
    reset_test();
    fail_primary = site;
    fail_cleanup = Q_DISPOSE;
    assert(ar_macos_playback_create(48000, 2, 240) == NULL);
    assert(g_quarantined_playback != NULL && live_allocations == 2);
    assert(ar_macos_audio_cleanup_failure() == error_for(Q_DISPOSE));
    assert_reopen_rejected();
    FakeQueue *queue = &fake_queues[0];
    AudioQueueBuffer buffer = {.mAudioDataBytesCapacity = sizeof(queue->samples[0]), .mAudioData = queue->samples[0]};
    queue->callback(queue->context, (AudioQueueRef)queue, &buffer);
    release_quarantined();
  }
  reset_test();
  void *first = ar_macos_playback_create(48000, 2, 240);
  void *second = ar_macos_playback_create(48000, 2, 240);
  assert(first && second);
  fail_cleanup = Q_DISPOSE;
  assert(ar_macos_playback_destroy(first) == error_for(Q_DISPOSE));
  assert(ar_macos_playback_destroy(second) == error_for(Q_DISPOSE));
  assert(live_allocations == 4);
  assert_reopen_rejected();
  release_quarantined();
  reset_test();
}
static void test_capture_lifecycle(void) {
  // 两个 converter 缓冲及 ring 分配逐点失败时, 已创建的 tap/aggregate 都应释放.
  for (int step = 1; step <= 3; step++) {
    reset_test();
    @autoreleasepool {
      fail_allocation = step;
      assert(ar_macos_capture_create(NULL, 48000, 2, 240) == NULL);
    }
    assert(live_allocations == 0 && ar_macos_audio_cleanup_failure() == 0);
    assert(cleanup_count == 2 && cleanup_order[0] == AGG_DESTROY && cleanup_order[1] == TAP_DESTROY);
  }
  // 尚无 IOProc/ring 时也可能清理 tap 失败, 不依赖 ring 已初始化才能隔离对象.
  reset_test();
  @autoreleasepool {
    fail_primary = AGG_CREATE;
    fail_cleanup = TAP_DESTROY;
    assert(ar_macos_capture_create(NULL, 48000, 2, 240) == NULL);
    assert(g_quarantined_capture != NULL && live_allocations == 0);
    assert(ar_macos_audio_cleanup_failure() == error_for(TAP_DESTROY));
    assert_reopen_rejected();
    release_quarantined();
  }
  for (int site = TAP_CREATE; site <= IO_START; site++) {
    reset_test();
    @autoreleasepool {
      fail_primary = site;
      assert(ar_macos_capture_create(NULL, 48000, 2, 240) == NULL);
    }
    assert(live_allocations == 0 && ar_macos_audio_cleanup_failure() == 0);
    assert(capture_proc == NULL);
  }
  reset_test();
  @autoreleasepool {
    void *handle = ar_macos_capture_create(NULL, 48000, 2, 240);
    assert(handle != NULL);
    assert(ar_macos_capture_destroy(handle) == 0);
  }
  int expected[] = {IO_STOP, IO_DESTROY, AGG_DESTROY, TAP_DESTROY};
  assert(cleanup_count == 4 && memcmp(expected, cleanup_order, sizeof(expected)) == 0);
  assert(live_allocations == 0);

  for (int site = IO_STOP; site <= TAP_DESTROY; site++) {
    reset_test();
    @autoreleasepool {
      void *handle = ar_macos_capture_create(NULL, 48000, 2, 240);
      assert(handle != NULL);
      fail_cleanup = site;
      assert(ar_macos_capture_destroy(handle) == error_for(site));
      assert(g_quarantined_capture == handle && live_allocations == 3);
      assert(cleanup_count == site - IO_STOP + 1);
      assert(memcmp(expected, cleanup_order, (size_t)cleanup_count * sizeof(int)) == 0);
      assert_reopen_rejected();
      late_capture_callback();
      release_quarantined();
    }
    assert(live_allocations == 0);
  }
  reset_test();
  @autoreleasepool {
    fail_primary = IO_START;
    fail_cleanup = IO_STOP;
    assert(ar_macos_capture_create(NULL, 48000, 2, 240) == NULL);
    assert(g_quarantined_capture != NULL && live_allocations == 3);
    assert_reopen_rejected();
    late_capture_callback();
    release_quarantined();
  }
  reset_test();
}
static void wait_for_test_flag(_Atomic bool *flag) {
  uint64_t deadline = ar_audio_monotonic_ns() + 5000000000ULL;
  while (!atomic_load(flag)) {
    assert(ar_audio_monotonic_ns() < deadline);
    sched_yield();
  }
}
static void *destroy_while_reopening(void *handle) {
  assert(ar_macos_playback_destroy(handle) == error_for(Q_DISPOSE));
  return NULL;
}
static void *reopen_while_destroying(void *context) {
  (void)context;
  atomic_store(&reopen_entered, true);
  assert(ar_macos_playback_create(48000, 2, 240) == NULL);
  return NULL;
}
static void test_serialized_cleanup_failure(void) {
  reset_test();
  void *handle = ar_macos_playback_create(48000, 2, 240);
  assert(handle != NULL);
  fail_cleanup = Q_DISPOSE;
  atomic_store(&pause_dispose, true);
  atomic_store(&dispose_entered, false);
  atomic_store(&continue_dispose, false);
  atomic_store(&reopen_entered, false);
  pthread_t destroyer, opener;
  assert(pthread_create(&destroyer, NULL, destroy_while_reopening, handle) == 0);
  wait_for_test_flag(&dispose_entered);
  assert(pthread_create(&opener, NULL, reopen_while_destroying, NULL) == 0);
  wait_for_test_flag(&reopen_entered);
  atomic_store(&continue_dispose, true);
  assert(pthread_join(destroyer, NULL) == 0);
  assert(pthread_join(opener, NULL) == 0);
  assert(queue_count == 1 && live_allocations == 2);
  atomic_store(&pause_dispose, false);
  release_quarantined();
  reset_test();
}

static void test_capture_stall_recovery_boundary(void) {
  reset_test();
  atomic_store(&virtual_now, 10000000000ULL);
  @autoreleasepool {
    void *handle = ar_macos_capture_create(NULL, 48000, 2, 240);
    assert(handle != NULL);
    float frame[480];
    atomic_fetch_add(&virtual_now, AR_CAPTURE_STALL_NS - 1000000);
    assert(ar_macos_capture_read(handle, frame, 480, 0) == 1);
    virtual_waited = 0;
    // 读取方要求 60 秒, 但距离无回调期限只剩 1 ms. 不能无限返回普通 Timeout.
    assert(ar_macos_capture_read(handle, frame, 480, 60000) == -1);
    assert(virtual_waited == 1000000);
    ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *)handle;
    assert(atomic_load(&capture->state.ring.failure) == AR_CAPTURE_STALLED);
    assert(atomic_load(&capture->state.health.last_callback_ns) == AR_CAPTURE_EXPIRED);
    late_capture_callback();
    assert(atomic_load(&capture->state.health.last_callback_ns) == AR_CAPTURE_EXPIRED);
    assert(ar_macos_capture_destroy(handle) == 0);
    // 停滞本身是可恢复的 Backend 错误, 不触发清理失败的永久隔离.
    assert(ar_macos_audio_cleanup_failure() == 0);
    cleanup_count = 0;
    handle = ar_macos_capture_create(NULL, 48000, 2, 240);
    assert(handle != NULL);
    assert(ar_macos_capture_read(handle, frame, 480, 0) == 1);
    assert(ar_macos_capture_destroy(handle) == 0);
  }
  atomic_store(&virtual_now, 0);
  reset_test();

  atomic_store(&virtual_now, 10000000000ULL);
  @autoreleasepool {
    void *handle = ar_macos_capture_create(NULL, 48000, 2, 240);
    assert(handle != NULL);
    AudioTimeStamp time = {0};
    AudioBufferList input = {0}, output = {0};
    float frame[480];
    // 持续空回调跨过多次 5 秒窗口, 不生成静音也不重建设备.
    for (unsigned second = 0; second < 20; second++) {
      atomic_fetch_add(&virtual_now, 1000000000);
      assert(capture_proc(102, &time, &input, &time, &output, &time, capture_context) == noErr);
      assert(ar_macos_capture_read(handle, frame, 480, 0) == 1);
    }
    // NULL 数据且字节数有效, 经真实 AudioConverter 产生等长静音, 也属于正常心跳.
    input.mNumberBuffers = 1;
    input.mBuffers[0] = (AudioBuffer){.mNumberChannels = 2, .mDataByteSize = 512 * 2 * sizeof(float), .mData = NULL};
    unsigned frames_read = 0;
    for (unsigned second = 0; second < 20; second++) {
      atomic_fetch_add(&virtual_now, 1000000000);
      assert(capture_proc(102, &time, &input, &time, &output, &time, capture_context) == noErr);
      int status;
      while ((status = ar_macos_capture_read(handle, frame, 480, 0)) == 0) {
        for (unsigned i = 0; i < 480; i++) assert(frame[i] == 0);
        frames_read++;
      }
      assert(status == 1);
    }
    assert(frames_read > 0);
    // 先缓冲一整帧, 停滞判定仍应拒绝旧积压, 而非成功读取后延长窗口.
    assert(capture_proc(102, &time, &input, &time, &output, &time, capture_context) == noErr);
    ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *)handle;
    uint32_t before = atomic_load(&capture->state.ring.read_cursor);
    atomic_fetch_add(&virtual_now, AR_CAPTURE_STALL_NS);
    assert(ar_macos_capture_read(handle, frame, 480, 0) == -1);
    assert(atomic_load(&capture->state.ring.read_cursor) == before);
    assert(ar_macos_capture_destroy(handle) == 0);
  }
  atomic_store(&virtual_now, 0);
  reset_test();

  // 停滞后的清理本身失败时, 保留第 12 轮的致命隔离, 不允许重建.
  atomic_store(&virtual_now, 10000000000ULL);
  @autoreleasepool {
    void *handle = ar_macos_capture_create(NULL, 48000, 2, 240);
    float frame[480];
    assert(handle != NULL);
    atomic_fetch_add(&virtual_now, AR_CAPTURE_STALL_NS);
    assert(ar_macos_capture_read(handle, frame, 480, 0) == -1);
    fail_cleanup = IO_STOP;
    assert(ar_macos_capture_destroy(handle) == error_for(IO_STOP));
    assert_reopen_rejected();
    late_capture_callback();
    release_quarantined();
  }
  atomic_store(&virtual_now, 0);
  reset_test();
}

static void test_real_capture_stall_wait(void) {
  reset_test();
  assert(atomic_load(&virtual_now) == 0);
  @autoreleasepool {
    void *handle = ar_macos_capture_create(NULL, 48000, 2, 240);
    assert(handle != NULL);
    ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *)handle;
    uint64_t started = atomic_load(&capture->state.health.last_callback_ns);
    float frame[480];
    // 设备仍为内存替身, 但这里使用真实单调时钟与 Mach 定时等待, 不跳过 5 秒期限.
    assert(ar_macos_capture_read(handle, frame, 480, 60000) == -1);
    uint64_t elapsed = ar_audio_monotonic_ns() - started;
    assert(elapsed >= AR_CAPTURE_STALL_NS && elapsed < AR_CAPTURE_STALL_NS + 3000000000ULL);
    assert(atomic_load(&capture->state.ring.failure) == AR_CAPTURE_STALLED);
    assert(ar_macos_capture_destroy(handle) == 0);
  }
  reset_test();
}

#include "macos-capture-change-cases.h"
#include "macos-playback-change-cases.h"

static void test_creation_object_ids(void) {
  for (int scenario = 0; scenario < 4; scenario++) {
    reset_test();
    @autoreleasepool {
      missing_tap_id = scenario == 0;
      missing_aggregate_id = scenario == 1 || scenario == 2;
      aggregate_exta = scenario >= 2;
      void *handle = ar_macos_capture_create(NULL, 48000, 2, 240);
      if (scenario == 3) {
        assert(handle != NULL);
        assert(ar_macos_capture_destroy(handle) == 0);
      } else {
        assert(handle == NULL);
        assert(capture_proc == NULL);
        if (scenario != 0) {
          assert(cleanup_count == 1 && cleanup_order[0] == TAP_DESTROY);
        }
      }
    }
  }
  reset_test();
}

int main(void) {
  test_creation_object_ids();
  puts("[1/5] AudioQueue 各初始化故障, 同步销毁与隔离后迟到回调");
  test_playback_lifecycle();
  test_serialized_cleanup_failure();
  puts("[2/5] tap/aggregate/IOProc 逐阶段清理, 强引用隔离与重建熔断");
  test_capture_lifecycle();
  puts("[3/5] 无回调停滞, 静音/空回调及清理后重建边界");
  test_capture_stall_recovery_boundary();
  puts("      使用真实时钟等待无回调期限, 预计 5 秒");
  fflush(stdout);
  test_real_capture_stall_wait();
  puts("[4/5] 属性通知失效, 部分注册回收和在途 block 生命周期");
  test_capture_property_changes();
  puts("[5/5] 播放默认设备变化, 提交失效和双向信号隔离");
  test_playback_property_changes();
  puts("macOS 音频生命周期故障测试通过");
  return 0;
}
