// 仅注入系统 API 失败, 不创建 tap, aggregate device 或真实 AudioQueue.
#import <AudioToolbox/AudioToolbox.h>
#import <Foundation/Foundation.h>
#include <assert.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

static int fail_step;
static int init_step;
static int live_allocations;
static int live_mutexes;
static int live_conditions;

static void *test_calloc(size_t count, size_t size) {
  if (++init_step == fail_step) return NULL;
  void *value = calloc(count, size);
  if (value != NULL) live_allocations++;
  return value;
}
static void test_free(void *value) {
  if (value != NULL) live_allocations--;
  free(value);
}
static int test_mutex_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attrs) {
  if (++init_step == fail_step) return ENOMEM;
  int result = pthread_mutex_init(mutex, attrs);
  if (result == 0) live_mutexes++;
  return result;
}
static int test_mutex_destroy(pthread_mutex_t *mutex) {
  int result = pthread_mutex_destroy(mutex);
  assert(result == 0);
  live_mutexes--;
  return result;
}
static int test_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attrs) {
  if (++init_step == fail_step) return ENOMEM;
  int result = pthread_cond_init(cond, attrs);
  if (result == 0) live_conditions++;
  return result;
}
static int test_cond_destroy(pthread_cond_t *cond) {
  int result = pthread_cond_destroy(cond);
  assert(result == 0);
  live_conditions--;
  return result;
}
static _Thread_local atomic_bool *wait_entered;
static int test_cond_timedwait(pthread_cond_t *cond, pthread_mutex_t *mutex, const struct timespec *deadline) {
  if (wait_entered != NULL) atomic_store(wait_entered, true);
  // 标记时仍持有 ring mutex, 故障线程获取该锁前必须先完成原子 wait.
  return pthread_cond_timedwait_relative_np(cond, mutex, deadline);
}

static OSStatus enqueue_result;
static unsigned enqueue_calls;
static OSStatus test_enqueue(AudioQueueRef queue, AudioQueueBufferRef buffer, UInt32 count,
                             const AudioStreamPacketDescription *descriptions) {
  (void) queue; (void) buffer; (void) count; (void) descriptions;
  enqueue_calls++;
  return enqueue_result;
}
static OSStatus conversion_result;
static OSStatus test_convert(AudioConverterRef converter, AudioConverterComplexInputDataProc input,
                             void *context, UInt32 *frames, AudioBufferList *data,
                             AudioStreamPacketDescription *descriptions) {
  (void) converter; (void) input; (void) context; (void) descriptions;
  *frames = 0;
  data->mBuffers[0].mDataByteSize = 0;
  return conversion_result;
}

#define calloc test_calloc
#define free test_free
#define pthread_mutex_init test_mutex_init
#define pthread_mutex_destroy test_mutex_destroy
#define pthread_cond_init test_cond_init
#define pthread_cond_destroy test_cond_destroy
#define pthread_cond_timedwait_relative_np test_cond_timedwait
#define AudioQueueEnqueueBuffer test_enqueue
#define AudioConverterFillComplexBuffer test_convert
#include "../macos_audio.m"
#undef calloc
#undef free
#undef pthread_mutex_init
#undef pthread_mutex_destroy
#undef pthread_cond_init
#undef pthread_cond_destroy

static void assert_resources_released(void) {
  assert(live_allocations == 0);
  assert(live_mutexes == 0);
  assert(live_conditions == 0);
}

static void test_partial_initialization(void) {
  ARFloatRing empty = {0};
  ar_ring_close(&empty);
  ar_ring_free(&empty);
  for (fail_step = 1; fail_step <= 4; fail_step++) {
    init_step = 0;
    ARFloatRing ring;
    assert(!ar_ring_init(&ring, 8));
    ar_ring_close(&ring);
    ar_ring_free(&ring);
    assert_resources_released();
  }
  fail_step = 0;
  ARFloatRing ring;
  assert(ar_ring_init(&ring, 8));
  ar_ring_close(&ring);
  ar_ring_free(&ring);
  ar_ring_free(&ring);
  assert_resources_released();
}

static void *error_thread(void *context) {
  (void) context;
  ar_set_error("worker-error");
  char output[32];
  ar_macos_copy_error(output, sizeof(output));
  assert(strcmp(output, "worker-error") == 0);
  return NULL;
}

static void test_error_ownership(void) {
  ar_set_error("caller-error");
  pthread_t worker;
  assert(pthread_create(&worker, NULL, error_thread, NULL) == 0);
  assert(pthread_join(worker, NULL) == 0);
  char output[32];
  ar_macos_copy_error(output, sizeof(output));
  assert(strcmp(output, "caller-error") == 0);
  char bounded[3] = {1, 1, 42};
  ar_macos_copy_error(bounded, 2);
  assert(bounded[1] == 0 && bounded[2] == 42);
  ar_macos_copy_error(NULL, 0);
}

typedef struct {
  ARFloatRing *ring;
  bool writing;
  int result;
  atomic_bool entered;
} Waiter;

static void *wait_for_failure(void *context) {
  Waiter *waiter = context;
  float sample = 0;
  wait_entered = &waiter->entered;
  waiter->result = waiter->writing
      ? ar_ring_write_wait(waiter->ring, &sample, 1, 1000)
      : ar_ring_read(waiter->ring, &sample, 1, 1000);
  assert(waiter->result == -1);
  // 验证回调原始 OSStatus 在等待线程被保留, 不依赖错误文案.
  assert(waiter->ring->failure == -1234);
  char error[512];
  ar_macos_copy_error(error, sizeof(error));
  assert(strstr(error, "-1234") != NULL);
  wait_entered = NULL;
  return NULL;
}

static void test_failure_wakes_consumers(void) {
  for (int writing = 0; writing < 2; writing++) {
    ARFloatRing ring;
    assert(ar_ring_init(&ring, 1));
    float sample = 1;
    if (writing) assert(ar_ring_write_wait(&ring, &sample, 1, 0) == 0);
    Waiter waiter = {.ring = &ring, .writing = writing};
    atomic_init(&waiter.entered, false);
    pthread_t worker;
    assert(pthread_create(&worker, NULL, wait_for_failure, &waiter) == 0);
    while (!atomic_load(&waiter.entered)) sched_yield();
    ar_ring_fail(&ring, -1234, "injected");
    ar_ring_fail(&ring, -5678, "later");
    assert(pthread_join(worker, NULL) == 0);
    assert(ring.failure == -1234);
    uint32_t length = ring.len;
    ar_ring_write_overwrite(&ring, &sample, 1);
    assert(ring.len == length);
    ar_ring_free(&ring);
    assert_resources_released();
  }
}

static void test_converter_allocation_failures(void) {
  AudioStreamBasicDescription source = ar_pcm_format(44100, 2);
  for (int step = 1; step <= 2; step++) {
    init_step = 0;
    fail_step = step;
    ARPcmConverter converter;
    assert(ar_pcm_init(&converter, &source, 48000, 2, 512) == kAudio_MemFullError);
    assert(converter.handle == NULL && converter.output == NULL && converter.silence == NULL);
    ar_pcm_destroy(&converter);
    assert_resources_released();
  }
  fail_step = 0;
}

static void test_callback_failures(void) {
  ARPlaybackEngine playback = {0};
  assert(ar_ring_init(&playback.ring, 8));
  playback.buffer_samples = 4;
  float samples[4] = {0};
  AudioQueueBuffer buffer = {.mAudioData = samples, .mAudioDataBytesCapacity = sizeof(samples)};
  enqueue_result = -2345;
  enqueue_calls = 0;
  ar_output_callback(&playback, NULL, &buffer);
  assert(playback.ring.failure == enqueue_result);
  assert(ar_macos_playback_submit(&playback, samples, 4, 0) == -1);
  ar_output_callback(&playback, NULL, &buffer);
  assert(enqueue_calls == 1);
  ar_ring_free(&playback.ring);

  @autoreleasepool {
    ARSystemAudioCapture *capture = [[ARSystemAudioCapture alloc] init];
    assert(ar_capture_ring_init(&capture->state.ring, 48000, 2, 2, 2) == 0);
    capture->state.converter.source = ar_pcm_format(44100, 2);
    capture->state.converter.target = ar_pcm_format(48000, 2);
    capture->state.converter.input_frames = 2;
    capture->state.converter.output_frames = 2;
    capture->state.converter.callback_output_frames = 4;
    capture->state.converter.output = test_calloc(4, sizeof(float));
    capture->state.converter.handle = (AudioConverterRef) (uintptr_t) 1;
    AudioBufferList input = {.mNumberBuffers = 1};
    input.mBuffers[0] = (AudioBuffer) {.mNumberChannels = 2, .mData = samples, .mDataByteSize = sizeof(samples)};
    conversion_result = AR_CONVERTER_NEEDS_INPUT;
    ar_system_audio_io_proc(0, NULL, &input, NULL, NULL, NULL, &capture->state);
    assert(atomic_load(&capture->state.ring.write_cursor) == 0);
    conversion_result = -3456;
    ar_system_audio_io_proc(0, NULL, &input, NULL, NULL, NULL, &capture->state);
    assert(atomic_load(&capture->state.ring.failure) == conversion_result);
    assert([capture readSamples:samples sampleCount:4 timeoutMs:0] == -1);
    // 假 converter 不交给系统释放, 其余资源通过实际 dealloc 回收.
    capture->state.converter.handle = NULL;
  }
  assert_resources_released();
}

static void test_capture_budget_and_alignment(void) {
  ARFloatRing ring;
  assert(!ar_ring_init_audio(&ring, 0, 2, 240, false));
  assert(!ar_ring_init_audio(&ring, 48000, UINT32_MAX, 240, false));
  assert(!ar_ring_init_audio(&ring, 48000, 2, UINT32_MAX, false));
  assert(ar_ring_init_audio(&ring, 48000, 2, 240, false));
  assert(ring.capacity == 2880);
  float samples[3840];
  for (unsigned i = 0; i < 3840; i++) samples[i] = (float) i;
  ar_ring_write_overwrite(&ring, samples, 3840);
  float output[480];
  assert(ar_ring_read(&ring, output, 480, 0) == 0);
  assert(output[0] == 960 && output[479] == 1439);
  ar_ring_write_overwrite(&ring, samples, 960);
  assert(ar_ring_read(&ring, output, 480, 0) == 0);
  assert(output[0] == 1920 && output[479] == 2399);
  uint64_t dropped;
  uint32_t high_water;
  ar_ring_copy_stats(&ring, &dropped, &high_water);
  assert(dropped == 1440 && high_water == 2880);
  ar_ring_write_overwrite(&ring, samples, 3);
  assert(ring.failure == kAudio_ParamError);
  ar_ring_free(&ring);
  assert(ar_ring_init_audio(&ring, 48000, 2, 2880, false));
  assert(ar_ring_reserve_capture_chunk(&ring, 512));
  assert(ring.capacity == 6784);
  float chunk[1024];
  float long_output[5760];
  for (unsigned packet = 0; packet < 6; packet++) {
    for (unsigned i = 0; i < 1024; i++) chunk[i] = (float) (packet * 1024 + i);
    ar_ring_write_overwrite(&ring, chunk, 1024);
    if (packet < 5) assert(ar_ring_read(&ring, long_output, 5760, 0) == 1);
  }
  assert(ar_ring_read(&ring, long_output, 5760, 0) == 0);
  for (unsigned i = 0; i < 5760; i++) assert(long_output[i] == (float) i);
  assert(ring.len == 384 && ring.dropped_samples == 0);
  assert(ar_ring_read_partial_zero_fill(&ring, chunk, 384) == 384);
  assert(chunk[0] == 5760 && chunk[383] == 6143);
  ar_ring_free(&ring);
  assert_resources_released();
}

static void test_playback_watermark(void) {
  ARFloatRing ring;
  float samples[5760] = {0};
  assert(ar_ring_init_audio(&ring, 48000, 2, 240, true));
  for (int i = 0; i < 11; i++) assert(ar_ring_write_wait(&ring, samples, 480, 0) == 0);
  assert(ring.len == 5280 && ring.high_water_samples == 5280);
  assert(ar_ring_write_wait(&ring, samples, 480, 0) == -1);
  assert(ring.len == 5280);
  assert(ar_ring_read_partial_zero_fill(&ring, samples, 480) == 480);
  assert(ar_ring_write_wait(&ring, samples, 480, 0) == 0);
  assert(ar_ring_write_wait(&ring, samples, 2, 0) == -1);
  ar_ring_free(&ring);
  assert(ar_ring_init_audio(&ring, 48000, 2, 2880, true));
  assert(ar_ring_write_wait(&ring, samples, 5760, 0) == 0);
  // 仍有 50 ms 容量, 但已有 60 ms 数据, 水位规则必须阻止再提交.
  assert(ring.capacity - ring.len == 4800);
  assert(ar_ring_write_wait(&ring, samples, 5760, 0) == -1);
  assert(ar_ring_read_partial_zero_fill(&ring, samples, 960) == 960);
  assert(ar_ring_write_wait(&ring, samples, 5760, 0) == 0);
  assert(ring.len == 10560);
  ar_ring_free(&ring);
  assert_resources_released();
}

static void *wait_for_playback_space(void *context) {
  Waiter *waiter = context;
  float samples[480] = {0};
  wait_entered = &waiter->entered;
  waiter->result = ar_ring_write_wait(waiter->ring, samples, 480, 1000);
  wait_entered = NULL;
  return NULL;
}

static void test_playback_consumption_wakes_submit(void) {
  ARFloatRing ring;
  assert(ar_ring_init_audio(&ring, 48000, 2, 240, true));
  float samples[480] = {0};
  for (int i = 0; i < 11; i++) assert(ar_ring_write_wait(&ring, samples, 480, 0) == 0);
  Waiter waiter = {.ring = &ring};
  atomic_init(&waiter.entered, false);
  pthread_t worker;
  assert(pthread_create(&worker, NULL, wait_for_playback_space, &waiter) == 0);
  while (!atomic_load(&waiter.entered)) sched_yield();
  assert(ar_ring_read_partial_zero_fill(&ring, samples, 480) == 480);
  assert(pthread_join(worker, NULL) == 0);
  assert(waiter.result == 0 && ring.len == 5280);
  // 无消费者时必须在 100 ms 预算到期后退出, 不使用调用方的 10 秒等待.
  struct timespec start, end;
  clock_gettime(CLOCK_MONOTONIC, &start);
  assert(ar_ring_write_wait(&ring, samples, 480, 10000) == -1);
  clock_gettime(CLOCK_MONOTONIC, &end);
  double elapsed = end.tv_sec - start.tv_sec + (end.tv_nsec - start.tv_nsec) / 1e9;
  assert(elapsed >= 0.09 && elapsed < 2.0);
  ar_ring_free(&ring);
  assert_resources_released();
}

int main(void) {
  puts("[1/7] macOS 音频 ring 逐步初始化失败回收");
  test_partial_initialization();
  puts("[2/7] 线程错误归属和有界复制");
  test_error_ownership();
  puts("[3/7] 异步故障唤醒读写并保留首个错误");
  test_failure_wakes_consumers();
  puts("[4/7] 转换和播放回调故障传播");
  test_callback_failures();
  test_converter_allocation_failures();
  puts("[5/7] 捕获时间预算, 声道对齐和溢出统计");
  test_capture_budget_and_alignment();
  puts("[6/7] 播放水位与 60 ms 整帧边界");
  test_playback_watermark();
  puts("[7/7] 消费唤醒提交和 100 ms 超时预算");
  test_playback_consumption_wakes_submit();
  puts("macOS 原生音频故障与缓冲测试通过");
  return 0;
}
