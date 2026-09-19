// 直接测试生产回调, 不创建 tap, aggregate device 或真实 AudioQueue.
#import <AudioToolbox/AudioToolbox.h>
#import <Foundation/Foundation.h>
#include <assert.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

static int fail_step;
static int init_step;
static int live_allocations;
static _Thread_local bool in_realtime_callback;

static void *test_calloc(size_t count, size_t size) {
  assert(!in_realtime_callback);
  if (++init_step == fail_step) return NULL;
  void *value = calloc(count, size);
  if (value != NULL) live_allocations++;
  return value;
}
static void test_free(void *value) {
  assert(!in_realtime_callback);
  if (value != NULL) live_allocations--;
  free(value);
}
static int checked_mutex_lock(pthread_mutex_t *mutex) {
  assert(!in_realtime_callback);
  return pthread_mutex_lock(mutex);
}
static OSStatus enqueue_result;
static unsigned enqueue_calls;
static OSStatus test_enqueue(AudioQueueRef queue, AudioQueueBufferRef buffer, UInt32 count,
                             const AudioStreamPacketDescription *descriptions) {
  (void)queue; (void)buffer; (void)count; (void)descriptions;
  enqueue_calls++;
  return enqueue_result;
}
static OSStatus conversion_result;
static OSStatus test_convert(AudioConverterRef converter, AudioConverterComplexInputDataProc input,
                             void *context, UInt32 *frames, AudioBufferList *data,
                             AudioStreamPacketDescription *descriptions) {
  (void)converter; (void)input; (void)context; (void)descriptions;
  *frames = 0;
  data->mBuffers[0].mDataByteSize = 0;
  return conversion_result;
}
#define calloc test_calloc
#define free test_free
#define pthread_mutex_lock checked_mutex_lock
#define AudioQueueEnqueueBuffer test_enqueue
#define AudioConverterFillComplexBuffer test_convert
#include "../macos_audio.m"
#undef calloc
#undef free
#undef pthread_mutex_lock

static void *error_thread(void *context) {
  (void)context;
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
static void test_converter_allocation_failures(void) {
  AudioStreamBasicDescription source = ar_pcm_format(44100, 2);
  for (int step = 1; step <= 2; step++) {
    init_step = 0;
    fail_step = step;
    ARPcmConverter converter;
    assert(ar_pcm_init(&converter, &source, 48000, 2, 512) == kAudio_MemFullError);
    assert(converter.handle == NULL && converter.output == NULL && converter.silence == NULL);
    ar_pcm_destroy(&converter);
    assert(live_allocations == 0);
  }
  fail_step = 0;
}

static void capture_callback(ARCaptureState *state, const AudioBufferList *input) {
  in_realtime_callback = true;
  ar_system_audio_io_proc(0, NULL, input, NULL, NULL, NULL, state);
  in_realtime_callback = false;
}
static void output_callback(ARPlaybackEngine *engine, AudioQueueBuffer *buffer) {
  in_realtime_callback = true;
  ar_output_callback(engine, NULL, buffer);
  in_realtime_callback = false;
}
static void test_callback_boundaries(void) {
  ARPlaybackEngine playback = {0};
  assert(ar_playback_ring_init(&playback.ring, 48000, 2, 2) == 0);
  playback.buffer_samples = 4;
  float samples[4] = {9, 9, 9, 9};
  AudioQueueBuffer buffer = {.mAudioData = samples, .mAudioDataBytesCapacity = sizeof(samples)};
  enqueue_result = noErr;
  enqueue_calls = 0;
  output_callback(&playback, &buffer);
  assert(enqueue_calls == 1 && buffer.mAudioDataByteSize == sizeof(samples));
  for (unsigned i = 0; i < 4; i++) assert(samples[i] == 0);
  float source[] = {1, 2, 3, 4};
  assert(ar_macos_playback_submit(&playback, source, 4, 0) == 0);
  enqueue_result = -2345;
  output_callback(&playback, &buffer);
  assert(memcmp(samples, source, sizeof(source)) == 0);
  assert(atomic_load(&playback.ring.samples.failure) == enqueue_result);
  assert(ar_macos_playback_submit(&playback, source, 4, 0) == -1);
  output_callback(&playback, &buffer);
  assert(enqueue_calls == 2);
  ar_audio_ring_free(&playback.ring.samples);

  assert(ar_playback_ring_init(&playback.ring, 48000, 2, 2) == 0);
  AudioQueueBuffer undersized = {.mAudioData = samples, .mAudioDataBytesCapacity = sizeof(float)};
  output_callback(&playback, &undersized);
  assert(enqueue_calls == 2);
  assert(atomic_load(&playback.ring.samples.failure) == kAudio_ParamError);
  assert(!atomic_load(&playback.callback_active));
  ar_audio_ring_free(&playback.ring.samples);

  assert(ar_playback_ring_init(&playback.ring, 48000, 2, 2) == 0);
  atomic_store(&playback.callback_active, true);
  output_callback(&playback, &buffer);
  assert(atomic_load(&playback.ring.samples.failure) == kAudio_ParamError);
  assert(atomic_load(&playback.callback_active));
  assert(enqueue_calls == 2);
  atomic_store(&playback.callback_active, false);
  ar_audio_ring_free(&playback.ring.samples);

  ARCaptureState direct = {0};
  AudioStreamBasicDescription format = ar_pcm_format(48000, 2);
  assert(ar_pcm_init(&direct.converter, &format, 48000, 2, 2) == noErr);
  assert(ar_capture_ring_init(&direct.ring, 48000, 2, 2, 2) == 0);
  assert(ar_capture_health_init(&direct.health, ar_audio_monotonic_ns()));
  AudioBufferList input = {.mNumberBuffers = 1};
  input.mBuffers[0] = (AudioBuffer){.mNumberChannels = 2, .mData = source, .mDataByteSize = sizeof(source)};
  capture_callback(&direct, &input);
  assert(ar_capture_ring_read(&direct.ring, samples, 4, 0) == 0);
  assert(memcmp(source, samples, sizeof(source)) == 0);
  atomic_store(&direct.callback_active, true);
  capture_callback(&direct, &input);
  assert(atomic_load(&direct.ring.failure) == kAudio_ParamError);
  assert(atomic_load(&direct.callback_active));
  atomic_store(&direct.callback_active, false);
  ar_audio_ring_close(&direct.ring);
  capture_callback(&direct, &input);
  ar_pcm_destroy(&direct.converter);
  ar_audio_ring_free(&direct.ring);

  @autoreleasepool {
    ARSystemAudioCapture *capture = [[ARSystemAudioCapture alloc] init];
    assert(ar_capture_ring_init(&capture->state.ring, 48000, 2, 2, 2) == 0);
    assert(ar_capture_health_init(&capture->state.health, ar_audio_monotonic_ns()));
    capture->state.converter.source = ar_pcm_format(44100, 2);
    capture->state.converter.target = ar_pcm_format(48000, 2);
    capture->state.converter.input_frames = 2;
    capture->state.converter.output_frames = 2;
    capture->state.converter.callback_output_frames = 4;
    capture->state.converter.output = test_calloc(4, sizeof(float));
    capture->state.converter.handle = (AudioConverterRef)(uintptr_t)1;
    conversion_result = AR_CONVERTER_NEEDS_INPUT;
    capture_callback(&capture->state, &input);
    assert(atomic_load(&capture->state.ring.write_cursor) == 0);
    conversion_result = -3456;
    capture_callback(&capture->state, &input);
    assert(atomic_load(&capture->state.ring.failure) == conversion_result);
    assert([capture readSamples:samples sampleCount:4 timeoutMs:0] == -1);
    // 假 converter 不交给系统释放, 其余资源通过实际 dealloc 回收.
    capture->state.converter.handle = NULL;
  }
  @autoreleasepool {
    ARAudioChanges *changes = [[ARAudioChanges alloc] init];
    assert(changes != nil);
    const AudioObjectPropertyAddress addresses[] = {
      {.mSelector = kAudioHardwarePropertyDefaultOutputDevice},
      {.mSelector = kAudioDevicePropertyStreams},
      {.mSelector = kAudioStreamPropertyVirtualFormat},
      {.mSelector = kAudioDevicePropertyBufferFrameSize},
      {.mSelector = kAudioDevicePropertyDeviceIsAlive},
    };
    ARCaptureState changed = {0};
    assert(ar_capture_ring_init(&changed.ring, 48000, 2, 2, 2) == 0);
    changed.changes = &changes->signal->state;
    in_realtime_callback = true;
    changes->listener(5, addresses);
    in_realtime_callback = false;
    assert(ar_audio_change_reasons(changed.changes) == 31);
    capture_callback(&changed, &input);
    assert(atomic_load(&changed.ring.failure) == AR_CAPTURE_CHANGED);
    ar_audio_ring_free(&changed.ring);
    assert(ar_playback_ring_init(&playback.ring, 48000, 2, 2) == 0);
    playback.changes = &changes->signal->state;
    unsigned enqueues_before = enqueue_calls;
    output_callback(&playback, &buffer);
    assert(atomic_load(&playback.ring.samples.failure) == AR_PLAYBACK_CHANGED);
    assert(enqueue_calls == enqueues_before);
    ar_audio_ring_free(&playback.ring.samples);
  }
  assert(live_allocations == 0);
}
int main(void) {
  puts("[1/3] 线程错误归属和有界复制");
  test_error_ownership();
  puts("[2/3] 转换缓冲分配失败回收");
  test_converter_allocation_failures();
  puts("[3/3] 双向回调无互斥, 缺样补零和故障传播");
  test_callback_boundaries();
  puts("macOS 原生音频回调测试通过");
  return 0;
}
