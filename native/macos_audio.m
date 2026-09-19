#import <AudioToolbox/AudioConverter.h>
#import <AudioToolbox/AudioToolbox.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#import <CoreAudio/CoreAudio.h>
#import <Foundation/Foundation.h>
#include <stdarg.h>
#include <stdio.h>
#include <pthread.h>
#include <assert.h>
#include "macos_audio_conversion.h"
#include "macos_capture_ring.h"
#include "macos_capture_health.h"
#include "macos_audio_changes.h"
#include "macos_playback_ring.h"

// FFI 错误只在调用线程读取. 回调错误经各自 ring 传递到读写线程,
// 避免多个捕获/播放实例并发覆盖共享字符缓冲区.
static _Thread_local char g_last_error[512] = "macOS audio backend error";

static void ar_set_error(const char *fmt, ...) {
  va_list args;
  va_start(args, fmt);
  vsnprintf(g_last_error, sizeof(g_last_error), fmt, args);
  va_end(args);
}

// 只序列化设备创建与销毁, 实时回调和正常读写从不取得此锁.
// 清理失败后保留原上下文, 禁止后续创建, 防止重试循环不断累积原生资源.
static pthread_mutex_t g_audio_lifecycle = PTHREAD_MUTEX_INITIALIZER;
static _Atomic int g_audio_cleanup_failure;
static void *g_quarantined_capture;
static void *g_quarantined_playback;

int ar_macos_audio_cleanup_failure(void) {
  return atomic_load_explicit(&g_audio_cleanup_failure, memory_order_acquire);
}

static bool ar_audio_creation_allowed(void) {
  int status = ar_macos_audio_cleanup_failure();
  if (status == 0) return true;
  ar_set_error("Core Audio 清理失败已隔离资源, 请重启应用后再试, status=%d", status);
  return false;
}

static void ar_audio_poison(OSStatus status) {
  int expected = 0;
  atomic_compare_exchange_strong(&g_audio_cleanup_failure, &expected, status);
}

int ar_macos_capture_supported(void) {
  // AudioHardwareCreateProcessTap 自 macOS 14.2 起提供, 不是 14.0.
  @autoreleasepool {
    NSOperatingSystemVersion minimum = {14, 2, 0};
    return [[NSProcessInfo processInfo] isOperatingSystemAtLeastVersion:minimum];
  }
}

void ar_macos_copy_error(char *out, uint32_t capacity) {
  if (out != NULL && capacity > 0) {
    snprintf(out, capacity, "%s", g_last_error);
  }
}

typedef struct {
  ARPcmConverter converter;
  ARAudioRing ring;
  ARCaptureHealth health;
  const ARAudioChangeState *changes;
  _Atomic bool callback_active;
} ARCaptureState;

// 自定义原生错误区分 IOProc 停滞与属性变化, 不与 PCM 转换/清理故障混淆.
static const OSStatus AR_CAPTURE_STALLED = 0x61727374;
static const OSStatus AR_CAPTURE_CHANGED = 0x61726368;

@interface ARSystemAudioCapture : NSObject {
@public
  AudioObjectID tapObjectID;
  AudioObjectID aggregateDeviceID;
  AudioDeviceIOProcID ioProcID;
  ARCaptureState state;
  ARAudioChanges *propertyChanges;
  bool startAttempted;
  void *quarantineNext;
}
- (bool)startWithSampleRate:(uint32_t)sampleRate
                          channels:(uint32_t)channels
                         frameSize:(uint32_t)frameSize;
- (OSStatus)shutdown;
- (int)readSamples:(float *)out sampleCount:(uint32_t)count timeoutMs:(uint32_t)timeoutMs;
@end

static void ar_capture_pcm(void *context, const float *samples, UInt32 count) {
  ar_capture_ring_write(context, samples, count);
}

static OSStatus ar_system_audio_io_proc(
    AudioObjectID inDevice,
    const AudioTimeStamp *inNow,
    const AudioBufferList *inInputData,
    const AudioTimeStamp *inInputTime,
    AudioBufferList *outOutputData,
    const AudioTimeStamp *inOutputTime,
    void *inClientData) {
  (void) inDevice;
  (void) inNow;
  (void) inInputTime;
  (void) outOutputData;
  (void) inOutputTime;

  ARCaptureState *capture = inClientData;
  if (atomic_load_explicit(&capture->ring.closed, memory_order_acquire)) {
    return noErr;
  }
  if (ar_audio_change_reasons(capture->changes) != 0) {
    ar_audio_ring_fail(&capture->ring, AR_CAPTURE_CHANGED);
    return noErr;
  }
  if (atomic_exchange_explicit(&capture->callback_active, true, memory_order_acq_rel)) {
    ar_audio_ring_fail(&capture->ring, kAudio_ParamError);
    return noErr;
  }
  if (ar_capture_health_pulse(&capture->health, ar_audio_monotonic_ns())) {
    OSStatus status = ar_pcm_push(&capture->converter, inInputData, ar_capture_pcm, &capture->ring);
    if (status != noErr) ar_audio_ring_fail(&capture->ring, status);
  }
  atomic_store_explicit(&capture->callback_active, false, memory_order_release);
  return noErr;
}

@implementation ARSystemAudioCapture

- (bool)startWithSampleRate:(uint32_t)sampleRate
                          channels:(uint32_t)channels
                         frameSize:(uint32_t)frameSize {
  // 对象先由 FFI 持有, 初始化失败也通过显式 shutdown 决定释放还是隔离.

  if (channels != 2) {
    ar_set_error("macOS system audio capture currently supports stereo only");
    return false;
  }

  if (!ar_macos_capture_supported()) {
    ar_set_error("macOS 系统音频捕获需要 14.2 或更新版本");
    return false;
  }

  tapObjectID = kAudioObjectUnknown;
  aggregateDeviceID = kAudioObjectUnknown;
  ioProcID = NULL;
  memset(&state, 0, sizeof(state));
  propertyChanges = [[ARAudioChanges alloc] init];
  if (propertyChanges == nil) {
    ar_set_error("无法创建 lock-free 捕获属性通知状态");
    return false;
  }
  state.changes = &propertyChanges->signal->state;
  // 在 tap 创建前监听默认输出变化, 避免创建期间切换设备却沿用旧捕获状态.
  OSStatus status = [propertyChanges addObject:kAudioObjectSystemObject
      selector:kAudioHardwarePropertyDefaultOutputDevice scope:kAudioObjectPropertyScopeGlobal];
  if (status != noErr) {
    ar_set_error("注册默认输出设备变化通知失败, OSStatus=%d", (int)status);
    return false;
  }

  CATapDescription *tapDescription = [[CATapDescription alloc] initStereoGlobalTapButExcludeProcesses:@[]];
  if (tapDescription == nil) {
    ar_set_error("failed to create macOS system audio tap description");
    return false;
  }

  tapDescription.name = [NSString stringWithFormat:@"synly-tap-%p", self];
  tapDescription.UUID = [NSUUID UUID];
  [tapDescription setPrivate:YES];
  tapDescription.muteBehavior = CATapUnmuted;

  status = AudioHardwareCreateProcessTap(tapDescription, &tapObjectID);
  if (status != noErr) {
    ar_set_error("AudioHardwareCreateProcessTap failed with status %d", (int) status);
    return false;
  }

  NSString *tapUID = [[tapDescription UUID] UUIDString];
  if (tapUID == nil) {
    ar_set_error("failed to obtain system audio tap UUID");
    return false;
  }

  NSDictionary *subTap = @{
    @kAudioSubTapUIDKey: tapUID,
    @kAudioSubTapDriftCompensationKey: @YES,
  };
  NSDictionary *aggregate = @{
    @kAudioAggregateDeviceNameKey: [NSString stringWithFormat:@"synly-aggregate-%p", self],
    @kAudioAggregateDeviceUIDKey: [NSString stringWithFormat:@"dev.synly.aggregate-%p", self],
    @kAudioAggregateDeviceTapListKey: @[subTap],
    @kAudioAggregateDeviceTapAutoStartKey: @NO,
    @kAudioAggregateDeviceIsPrivateKey: @YES,
  };

  status = AudioHardwareCreateAggregateDevice((__bridge CFDictionaryRef) aggregate, &aggregateDeviceID);
  if (status != noErr && status != 'ExtA') {
    ar_set_error("AudioHardwareCreateAggregateDevice failed with status %d", (int) status);
    return false;
  }

  AudioObjectPropertyAddress sampleRateAddr = {
    .mSelector = kAudioDevicePropertyNominalSampleRate,
    .mScope = kAudioObjectPropertyScopeGlobal,
    .mElement = kAudioObjectPropertyElementMain,
  };
  Float64 requestedRate = sampleRate;
  UInt32 sampleRateSize = sizeof(requestedRate);
  AudioObjectSetPropertyData(aggregateDeviceID, &sampleRateAddr, 0, NULL, sampleRateSize, &requestedRate);

  AudioObjectPropertyAddress bufferSizeAddr = {
    .mSelector = kAudioDevicePropertyBufferFrameSize,
    .mScope = kAudioObjectPropertyScopeGlobal,
    .mElement = kAudioObjectPropertyElementMain,
  };
  UInt32 requestedFrameSize = frameSize;
  UInt32 frameSizeSize = sizeof(requestedFrameSize);
  AudioObjectSetPropertyData(aggregateDeviceID, &bufferSizeAddr, 0, NULL, frameSizeSize, &requestedFrameSize);

  const AudioObjectPropertySelector selectors[] = {
    kAudioDevicePropertyStreams, kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyDeviceIsAlive,
  };
  for (UInt32 index = 0; index < sizeof(selectors) / sizeof(selectors[0]); index++) {
    AudioObjectPropertyScope scope = selectors[index] == kAudioDevicePropertyStreams
        ? kAudioObjectPropertyScopeInput : kAudioObjectPropertyScopeGlobal;
    status = [propertyChanges addObject:aggregateDeviceID selector:selectors[index] scope:scope];
    if (status != noErr) {
      ar_set_error("注册捕获设备属性变化通知失败, OSStatus=%d", (int)status);
      return false;
    }
  }

  // 从实际输入 stream 查询虚拟格式, 不能用 nominal rate 和声道数猜 ASBD.
  AudioObjectPropertyAddress streamsAddr = {
    .mSelector = kAudioDevicePropertyStreams, .mScope = kAudioDevicePropertyScopeInput,
    .mElement = kAudioObjectPropertyElementMain,
  };
  UInt32 streamsSize = 0;
  status = AudioObjectGetPropertyDataSize(aggregateDeviceID, &streamsAddr, 0, NULL, &streamsSize);
  if (status != noErr || streamsSize != sizeof(AudioStreamID)) {
    ar_set_error("捕获输入必须为单个音频 stream, OSStatus=%d", (int) status);
    return false;
  }
  AudioStreamID streamID = kAudioObjectUnknown;
  status = AudioObjectGetPropertyData(aggregateDeviceID, &streamsAddr, 0, NULL, &streamsSize, &streamID);
  if (status != noErr) {
    ar_set_error("查询捕获输入 stream 失败, OSStatus=%d", (int) status);
    return false;
  }
  status = [propertyChanges addObject:streamID selector:kAudioStreamPropertyVirtualFormat scope:kAudioObjectPropertyScopeGlobal];
  if (status != noErr) {
    ar_set_error("注册捕获 stream 格式变化通知失败, OSStatus=%d", (int)status);
    return false;
  }
  AudioObjectPropertyAddress formatAddr = {
    .mSelector = kAudioStreamPropertyVirtualFormat, .mScope = kAudioObjectPropertyScopeGlobal,
    .mElement = kAudioObjectPropertyElementMain,
  };
  AudioStreamBasicDescription sourceFormat = {0};
  UInt32 formatSize = sizeof(sourceFormat);
  status = AudioObjectGetPropertyData(streamID, &formatAddr, 0, NULL, &formatSize, &sourceFormat);
  if (status != noErr || formatSize != sizeof(sourceFormat) || !ar_pcm_format_supported(&sourceFormat)) {
    ar_set_error("捕获输入格式不是支持的 native packed Float32 PCM, OSStatus=%d", (int) status);
    return false;
  }

  UInt32 deviceBufferFrames = 0;
  UInt32 deviceBufferSize = sizeof(deviceBufferFrames);
  status = AudioObjectGetPropertyData(aggregateDeviceID, &bufferSizeAddr, 0, NULL,
                                      &deviceBufferSize, &deviceBufferFrames);
  if (status != noErr || deviceBufferSize != sizeof(deviceBufferFrames) || deviceBufferFrames == 0) {
    ar_set_error("无法查询实际捕获缓冲, OSStatus=%d", (int) status);
    return false;
  }
  status = ar_pcm_init(&state.converter, &sourceFormat, sampleRate, channels, deviceBufferFrames);
  if (status != noErr) {
    ar_set_error("初始化捕获 PCM 转换失败, OSStatus=%d", (int) status);
    return false;
  }
  int ringStatus = ar_capture_ring_init(&state.ring, sampleRate, channels, frameSize, state.converter.callback_output_frames);
  if (ringStatus != 0) {
    ar_set_error("初始化捕获 SPSC 缓冲失败, status=%d", ringStatus);
    return false;
  }

  status = AudioDeviceCreateIOProcID(aggregateDeviceID, ar_system_audio_io_proc, &state, &ioProcID);
  if (status != noErr) {
    ar_set_error("AudioDeviceCreateIOProcID failed with status %d", (int) status);
    return false;
  }

  atomic_init(&state.callback_active, false);
  if (!atomic_is_lock_free(&state.callback_active) ||
      !ar_capture_health_init(&state.health, ar_audio_monotonic_ns())) {
    ar_set_error("捕获健康状态原子类型不支持 lock-free");
    return false;
  }
  if (ar_audio_change_reasons(state.changes) != 0) {
    ar_set_error("初始化期间捕获设备属性发生变化, 需要重新查询设备");
    return false;
  }
  startAttempted = true;
  status = AudioDeviceStart(aggregateDeviceID, ioProcID);
  if (status != noErr) {
    ar_set_error("AudioDeviceStart failed with status %d", (int) status);
    return false;
  }

  return true;
}

- (OSStatus)shutdown {
  ar_audio_ring_close(&state.ring);
  OSStatus status = [propertyChanges removeAll];
  if (status != noErr) return status;
  if (ioProcID != NULL) {
    if (startAttempted) {
      status = AudioDeviceStop(aggregateDeviceID, ioProcID);
      if (status != noErr) return status;
      startAttempted = false;
    }
    status = AudioDeviceDestroyIOProcID(aggregateDeviceID, ioProcID);
    if (status != noErr) return status;
    ioProcID = NULL;
  }
  if (aggregateDeviceID != kAudioObjectUnknown) {
    status = AudioHardwareDestroyAggregateDevice(aggregateDeviceID);
    if (status != noErr) return status;
    aggregateDeviceID = kAudioObjectUnknown;
  }
  if (tapObjectID != kAudioObjectUnknown) {
    status = AudioHardwareDestroyProcessTap(tapObjectID);
    if (status != noErr) return status;
    tapObjectID = kAudioObjectUnknown;
  }
  return noErr;
}

- (void)dealloc {
  // 系统回调注册只能由 shutdown 注销. 失败对象保持 FFI 强引用, 不进入 dealloc.
  assert(ioProcID == NULL && aggregateDeviceID == kAudioObjectUnknown && tapObjectID == kAudioObjectUnknown);
  ar_pcm_destroy(&state.converter);
  ar_audio_ring_free(&state.ring);
}

- (int)readSamples:(float *)out sampleCount:(uint32_t)count timeoutMs:(uint32_t)timeoutMs {
  uint64_t now = ar_audio_monotonic_ns();
  if (ar_audio_change_reasons(state.changes) != 0) ar_audio_ring_fail(&state.ring, AR_CAPTURE_CHANGED);
  if (!atomic_load_explicit(&state.ring.closed, memory_order_acquire) && ar_capture_health_expired(&state.health, now)) {
    ar_audio_ring_fail(&state.ring, AR_CAPTURE_STALLED);
  }
  uint32_t waitMs = ar_capture_health_wait_ms(&state.health, now, timeoutMs);
  int result = ar_capture_ring_read(&state.ring, out, count, waitMs);
  if (result >= 0 && ar_audio_change_reasons(state.changes) != 0) {
    ar_audio_ring_fail(&state.ring, AR_CAPTURE_CHANGED);
    result = -1;
  }
  // 即使缓冲仍有旧数据, 设备停滞也不能因短时间读取成功而不断延长恢复窗口.
  if (result >= 0 && ar_capture_health_expired(&state.health, ar_audio_monotonic_ns())) {
    ar_audio_ring_fail(&state.ring, AR_CAPTURE_STALLED);
    result = -1;
  }
  if (result < 0) {
    int failure = atomic_load(&state.ring.failure);
    if (failure == AR_CAPTURE_STALLED) {
      ar_set_error("系统音频捕获连续 5 秒没有 IOProc 回调, 需要重建设备");
    } else if (failure == AR_CAPTURE_CHANGED) {
      ar_set_error("捕获设备属性发生变化, 需要重建设备, reasons=0x%x", ar_audio_change_reasons(state.changes));
    } else {
      ar_set_error("捕获 SPSC 读取失败或已关闭, status=%d", failure);
    }
  }
  return result;
}

@end

typedef struct {
  AudioQueueRef queue;
  AudioStreamBasicDescription format;
  AudioQueueBufferRef buffers[3];
  ARPlaybackRing ring;
  uint32_t buffer_samples;
  _Atomic bool callback_active;
  const ARAudioChangeState *changes;
  void *changes_handle;
  void *quarantine_next;
} ARPlaybackEngine;

static const OSStatus AR_PLAYBACK_CHANGED = 0x61727063;

static void ar_output_callback(void *inUserData, AudioQueueRef inAQ, AudioQueueBufferRef inBuffer) {
  ARPlaybackEngine *engine = inUserData;
  if (atomic_load_explicit(&engine->ring.samples.closed, memory_order_acquire)) return;
  if (ar_audio_change_reasons(engine->changes) != 0) {
    ar_audio_ring_fail(&engine->ring.samples, AR_PLAYBACK_CHANGED);
    return;
  }
  // SPSC 只允许一个消费者. 若系统并发调用, 立即报告故障而不是等待或竞争游标.
  if (atomic_exchange_explicit(&engine->callback_active, true, memory_order_acq_rel)) {
    ar_audio_ring_fail(&engine->ring.samples, kAudio_ParamError);
    return;
  }
  uint32_t sample_count = engine->buffer_samples;
  if (inBuffer->mAudioData == NULL || (uint64_t)sample_count * sizeof(float) > inBuffer->mAudioDataBytesCapacity) {
    ar_audio_ring_fail(&engine->ring.samples, kAudio_ParamError);
  } else if (ar_playback_ring_fill(&engine->ring, inBuffer->mAudioData, sample_count) == 0) {
    inBuffer->mAudioDataByteSize = sample_count * sizeof(float);
    OSStatus status = AudioQueueEnqueueBuffer(inAQ, inBuffer, 0, NULL);
    if (status != noErr) ar_audio_ring_fail(&engine->ring.samples, status);
  }
  atomic_store_explicit(&engine->callback_active, false, memory_order_release);
}

// 返回后即消费外部句柄. 失败时由隔离链持有强引用, 不再暴露给运行时.
static OSStatus ar_capture_destroy_locked(void *handle) {
  if (handle == NULL) return noErr;
  ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *)handle;
  OSStatus status = [capture shutdown];
  if (status != noErr) {
    capture->quarantineNext = g_quarantined_capture;
    g_quarantined_capture = handle;
    ar_audio_poison(status);
  } else {
    CFBridgingRelease(handle);
  }
  return status;
}

static void *ar_capture_create_locked(const char *device_name, uint32_t sample_rate, uint32_t channels, uint32_t frame_size) {
  if (!ar_audio_creation_allowed()) return NULL;
  if (device_name != NULL && device_name[0] != '\0') {
    ar_set_error("macOS system audio capture does not support selecting a specific device yet");
    return NULL;
  }
  ARSystemAudioCapture *capture = [[ARSystemAudioCapture alloc] init];
  if (capture == nil) {
    ar_set_error("无法分配系统音频捕获对象");
    return NULL;
  }
  void *handle = (__bridge_retained void *)capture;
  if (![capture startWithSampleRate:sample_rate channels:channels frameSize:frame_size]) {
    OSStatus cleanup = ar_capture_destroy_locked(handle);
    if (cleanup != noErr) ar_set_error("捕获初始化后清理失败, 已隔离原生资源, status=%d", cleanup);
    return NULL;
  }
  return handle;
}

void *ar_macos_capture_create(const char *device_name, uint32_t sample_rate, uint32_t channels, uint32_t frame_size) {
  @autoreleasepool {
    pthread_mutex_lock(&g_audio_lifecycle);
    void *handle = ar_capture_create_locked(device_name, sample_rate, channels, frame_size);
    pthread_mutex_unlock(&g_audio_lifecycle);
    return handle;
  }
}

void ar_macos_capture_stats(void *handle, uint64_t *dropped, uint32_t *high_water) {
  ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *) handle;
  *dropped = atomic_load_explicit(&capture->state.ring.dropped_samples, memory_order_relaxed);
  *high_water = atomic_load_explicit(&capture->state.ring.high_water_samples, memory_order_relaxed);
}

int ar_macos_capture_destroy(void *handle) {
  @autoreleasepool {
    pthread_mutex_lock(&g_audio_lifecycle);
    OSStatus status = ar_capture_destroy_locked(handle);
    pthread_mutex_unlock(&g_audio_lifecycle);
    return status;
  }
}

int ar_macos_capture_read(void *handle, float *out_samples, uint32_t sample_count, uint32_t timeout_ms) {
  @autoreleasepool {
    ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *) handle;
    return [capture readSamples:out_samples sampleCount:sample_count timeoutMs:timeout_ms];
  }
}

static OSStatus ar_playback_destroy_locked(ARPlaybackEngine *engine) {
  if (engine == NULL) return noErr;
  ar_audio_ring_close(&engine->ring.samples);
  ARAudioChanges *changes = (__bridge ARAudioChanges *)engine->changes_handle;
  OSStatus status = [changes removeAll];
  if (status == noErr && engine->queue != NULL) {
    // AudioQueueDispose(..., true) 从工作线程同步销毁并停止回调, 无需先单独 Stop.
    status = AudioQueueDispose(engine->queue, true);
  }
  if (status != noErr) {
    engine->quarantine_next = g_quarantined_playback;
    g_quarantined_playback = engine;
    ar_audio_poison(status);
    return status;
  }
  engine->queue = NULL;
  if (engine->changes_handle != NULL) CFBridgingRelease(engine->changes_handle);
  ar_audio_ring_free(&engine->ring.samples);
  free(engine);
  return noErr;
}

static void ar_playback_cleanup_after_failure(ARPlaybackEngine *engine) {
  OSStatus cleanup = ar_playback_destroy_locked(engine);
  if (cleanup != noErr) ar_set_error("播放初始化后清理失败, 已隔离回调上下文, status=%d", cleanup);
}

static void *ar_playback_create_locked(uint32_t sample_rate, uint32_t channels, uint32_t frame_size) {
  if (!ar_audio_creation_allowed()) return NULL;
  ARPlaybackEngine *engine = calloc(1, sizeof(ARPlaybackEngine));
  if (engine == NULL) {
    ar_set_error("failed to allocate playback engine");
    return NULL;
  }
  atomic_init(&engine->callback_active, false);
  if (!atomic_is_lock_free(&engine->callback_active)) {
    ar_set_error("播放回调原子门控不支持 lock-free");
    free(engine);
    return NULL;
  }
  int ring_status = ar_playback_ring_init(&engine->ring, sample_rate, channels, frame_size);
  if (ring_status != 0) {
    ar_set_error("初始化播放 SPSC 缓冲失败, status=%d", ring_status);
    free(engine);
    return NULL;
  }
  ARAudioChanges *changes = [[ARAudioChanges alloc] init];
  if (changes == nil) {
    ar_set_error("无法创建 lock-free 播放设备通知状态");
    ar_playback_cleanup_after_failure(engine);
    return NULL;
  }
  engine->changes_handle = (__bridge_retained void *)changes;
  engine->changes = &changes->signal->state;
  OSStatus status = [changes addObject:kAudioObjectSystemObject
      selector:kAudioHardwarePropertyDefaultOutputDevice scope:kAudioObjectPropertyScopeGlobal];
  if (status != noErr) {
    ar_set_error("注册播放默认设备变化通知失败, OSStatus=%d", (int)status);
    ar_playback_cleanup_after_failure(engine);
    return NULL;
  }
  engine->format = ar_pcm_format(sample_rate, channels);
  engine->buffer_samples = frame_size * channels;
  status = AudioQueueNewOutput(&engine->format, ar_output_callback, engine, NULL, NULL, 0, &engine->queue);
  if (status != noErr) {
    ar_set_error("AudioQueueNewOutput failed with status %d", (int) status);
    ar_playback_cleanup_after_failure(engine);
    return NULL;
  }

  uint32_t bufferBytes = engine->buffer_samples * sizeof(float);
  for (int i = 0; i < 3; ++i) {
    status = AudioQueueAllocateBuffer(engine->queue, bufferBytes, &engine->buffers[i]);
    if (status != noErr) {
      ar_set_error("AudioQueueAllocateBuffer failed with status %d", (int) status);
      ar_playback_cleanup_after_failure(engine);
      return NULL;
    }
    memset(engine->buffers[i]->mAudioData, 0, bufferBytes);
    engine->buffers[i]->mAudioDataByteSize = bufferBytes;
    status = AudioQueueEnqueueBuffer(engine->queue, engine->buffers[i], 0, NULL);
    if (status != noErr) {
      ar_set_error("AudioQueueEnqueueBuffer 初始化失败, OSStatus=%d", (int) status);
      ar_playback_cleanup_after_failure(engine);
      return NULL;
    }
  }
  if (ar_audio_change_reasons(engine->changes) != 0) {
    ar_set_error("初始化期间默认播放设备发生变化, 需要重建输出");
    ar_playback_cleanup_after_failure(engine);
    return NULL;
  }
  status = AudioQueueStart(engine->queue, NULL);
  if (status != noErr) {
    ar_set_error("AudioQueueStart failed with status %d", (int) status);
    ar_playback_cleanup_after_failure(engine);
    return NULL;
  }
  return engine;
}

void *ar_macos_playback_create(uint32_t sample_rate, uint32_t channels, uint32_t frame_size) {
  @autoreleasepool {
    pthread_mutex_lock(&g_audio_lifecycle);
    void *handle = ar_playback_create_locked(sample_rate, channels, frame_size);
    pthread_mutex_unlock(&g_audio_lifecycle);
    return handle;
  }
}

void ar_macos_playback_stats(void *handle, uint64_t *dropped, uint32_t *high_water) {
  ARPlaybackEngine *engine = handle;
  *dropped = atomic_load_explicit(&engine->ring.samples.dropped_samples, memory_order_relaxed);
  *high_water = atomic_load_explicit(&engine->ring.samples.high_water_samples, memory_order_relaxed);
}

int ar_macos_playback_destroy(void *handle) {
  @autoreleasepool {
    pthread_mutex_lock(&g_audio_lifecycle);
    OSStatus status = ar_playback_destroy_locked(handle);
    pthread_mutex_unlock(&g_audio_lifecycle);
    return status;
  }
}

int ar_macos_playback_submit(void *handle, const float *samples, uint32_t sample_count, uint32_t timeout_ms) {
  ARPlaybackEngine *engine = handle;
  if (ar_audio_change_reasons(engine->changes) != 0) ar_audio_ring_fail(&engine->ring.samples, AR_PLAYBACK_CHANGED);
  int status = ar_playback_ring_submit(&engine->ring, samples, sample_count, timeout_ms);
  if (ar_audio_change_reasons(engine->changes) != 0) {
    ar_audio_ring_fail(&engine->ring.samples, AR_PLAYBACK_CHANGED);
    status = -1;
  }
  if (status != 0) {
    int failure = atomic_load(&engine->ring.samples.failure);
    if (failure == AR_PLAYBACK_CHANGED) {
      ar_set_error("默认播放设备发生变化, 需要重建输出");
    } else {
      ar_set_error("播放 SPSC 提交失败, result=%d, status=%d", status, failure);
    }
    return -1;
  }
  return 0;
}
