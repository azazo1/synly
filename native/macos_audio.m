#import <AudioToolbox/AudioConverter.h>
#import <AudioToolbox/AudioToolbox.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#import <CoreAudio/CoreAudio.h>
#import <Foundation/Foundation.h>
#include <errno.h>
#include <math.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "macos_audio_conversion.h"
#include "macos_capture_ring.h"

// FFI 错误只在调用线程读取. 回调错误经各自 ring 传递到读写线程,
// 避免多个捕获/播放实例并发覆盖共享字符缓冲区.
static _Thread_local char g_last_error[512] = "macOS audio backend error";

static void ar_set_error(const char *fmt, ...) {
  va_list args;
  va_start(args, fmt);
  vsnprintf(g_last_error, sizeof(g_last_error), fmt, args);
  va_end(args);
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
  float *data;
  uint32_t capacity;
  uint32_t read_pos;
  uint32_t write_pos;
  uint32_t len;
  uint32_t channels;
  uint32_t frame_samples;
  uint32_t write_watermark;
  uint32_t high_water_samples;
  uint64_t dropped_samples;
  bool closed;
  bool initialized;
  OSStatus failure;
  const char *failure_operation;
  pthread_mutex_t mutex;
  pthread_cond_t cond_read;
  pthread_cond_t cond_write;
} ARFloatRing;

static void ar_deadline_from_now(struct timespec *ts, uint32_t timeout_ms) {
  clock_gettime(CLOCK_MONOTONIC, ts);
  ts->tv_sec += timeout_ms / 1000;
  long nanos = ts->tv_nsec + (long) (timeout_ms % 1000) * 1000000L;
  ts->tv_sec += nanos / 1000000000L;
  ts->tv_nsec = nanos % 1000000000L;
}

// macOS 条件变量相对等待配合单调截止时间, 系统时钟变化不延长背压预算.
static int ar_ring_wait(pthread_cond_t *condition, pthread_mutex_t *mutex, const struct timespec *deadline) {
  struct timespec now;
  clock_gettime(CLOCK_MONOTONIC, &now);
  struct timespec remaining = {deadline->tv_sec - now.tv_sec, deadline->tv_nsec - now.tv_nsec};
  if (remaining.tv_nsec < 0) {
    remaining.tv_sec--;
    remaining.tv_nsec += 1000000000L;
  }
  if (remaining.tv_sec < 0) return ETIMEDOUT;
  return pthread_cond_timedwait_relative_np(condition, mutex, &remaining);
}

static bool ar_ring_init(ARFloatRing *ring, uint32_t capacity) {
  memset(ring, 0, sizeof(*ring));
  ring->data = (float *) calloc(capacity, sizeof(float));
  if (ring->data == NULL) {
    ar_set_error("failed to allocate ring buffer");
    return false;
  }
  int status = pthread_mutex_init(&ring->mutex, NULL);
  if (status != 0) {
    ar_set_error("初始化音频环形缓冲区 mutex 失败: %d", status);
    goto fail_data;
  }
  status = pthread_cond_init(&ring->cond_read, NULL);
  if (status != 0) {
    ar_set_error("初始化音频环形缓冲区读取条件变量失败: %d", status);
    goto fail_mutex;
  }
  status = pthread_cond_init(&ring->cond_write, NULL);
  if (status != 0) {
    ar_set_error("初始化音频环形缓冲区写入条件变量失败: %d", status);
    goto fail_cond_read;
  }

  ring->capacity = capacity;
  ring->channels = 1;
  ring->write_watermark = capacity;
  ring->initialized = true;
  return true;

fail_cond_read:
  pthread_cond_destroy(&ring->cond_read);
fail_mutex:
  pthread_mutex_destroy(&ring->mutex);
fail_data:
  free(ring->data);
  ring->data = NULL;
  return false;
}

// Sunshine 捕获预算 30 ms; Moonlight 播放队列水位 50 ms.
// 水位在提交前检查, 容量另加一整帧, 允许合法的 60 ms 帧.
static bool ar_ring_init_audio(ARFloatRing *ring, uint32_t rate, uint32_t channels,
                               uint32_t frame_size, bool playback) {
  if (rate < 8000 || rate > 48000 || channels == 0 || channels > 8 ||
      frame_size == 0 || (uint64_t) frame_size * 1000 > (uint64_t) rate * 60) {
    ar_set_error("无效的原生音频缓冲参数");
    return false;
  }
  uint64_t budget_frames = ((uint64_t) rate * (playback ? 50 : 30) + 999) / 1000;
  uint64_t capacity_frames = playback ? budget_frames + frame_size
      : (budget_frames > frame_size ? budget_frames : frame_size);
  uint64_t capacity = capacity_frames * channels;
  if (capacity > UINT32_MAX / sizeof(float)) {
    ar_set_error("原生音频缓冲大小溢出");
    return false;
  }
  if (!ar_ring_init(ring, (uint32_t) capacity)) return false;
  ring->channels = channels;
  ring->frame_samples = frame_size * channels;
  ring->write_watermark = playback ? (uint32_t) budget_frames * channels : ring->capacity;
  return true;
}

// 仅在 IOProc 启动前调用. 一帧尚未凑满时仍需接纳完整设备回调,
// 因而实际捕获容量下限为读取帧加一次回调的最大输出.
static bool ar_ring_reserve_capture_chunk(ARFloatRing *ring, uint32_t chunk_frames) {
  uint64_t required = (uint64_t) ring->frame_samples + (uint64_t) chunk_frames * ring->channels;
  if (required <= ring->capacity) return true;
  if (required > UINT32_MAX / sizeof(float)) {
    ar_set_error("捕获回调缓冲大小溢出");
    return false;
  }
  float *data = calloc((size_t) required, sizeof(float));
  if (data == NULL) {
    ar_set_error("无法分配捕获拼帧缓冲");
    return false;
  }
  free(ring->data);
  ring->data = data;
  ring->capacity = (uint32_t) required;
  ring->write_watermark = ring->capacity;
  ring->read_pos = ring->write_pos = ring->len = 0;
  return true;
}

static void ar_ring_record_drop(ARFloatRing *ring, uint32_t count) {
  ring->dropped_samples = UINT64_MAX - ring->dropped_samples < count
      ? UINT64_MAX : ring->dropped_samples + count;
}

static void ar_ring_record_high_water(ARFloatRing *ring) {
  if (ring->len > ring->high_water_samples) ring->high_water_samples = ring->len;
}

static void ar_ring_copy_stats(ARFloatRing *ring, uint64_t *dropped, uint32_t *high_water) {
  pthread_mutex_lock(&ring->mutex);
  *dropped = ring->dropped_samples;
  *high_water = ring->high_water_samples;
  pthread_mutex_unlock(&ring->mutex);
}

static void ar_ring_close(ARFloatRing *ring) {
  if (!ring->initialized) {
    return;
  }
  pthread_mutex_lock(&ring->mutex);
  ring->closed = true;
  pthread_cond_broadcast(&ring->cond_read);
  pthread_cond_broadcast(&ring->cond_write);
  pthread_mutex_unlock(&ring->mutex);
}

// 回调只保存原始错误和静态操作名, 字符串格式化留给阻塞读写线程.
// 首个故障保持不变, 直到整个设备实例销毁.
static void ar_ring_fail(ARFloatRing *ring, OSStatus status, const char *operation) {
  pthread_mutex_lock(&ring->mutex);
  if (ring->failure == noErr) {
    ring->failure = status;
    ring->failure_operation = operation;
  }
  ring->closed = true;
  pthread_cond_broadcast(&ring->cond_read);
  pthread_cond_broadcast(&ring->cond_write);
  pthread_mutex_unlock(&ring->mutex);
}

static bool ar_ring_is_closed(ARFloatRing *ring) {
  pthread_mutex_lock(&ring->mutex);
  bool closed = ring->closed;
  pthread_mutex_unlock(&ring->mutex);
  return closed;
}

static void ar_ring_report_failure_locked(ARFloatRing *ring) {
  if (ring->failure != noErr) {
    ar_set_error("%s 失败, OSStatus=%d", ring->failure_operation, (int) ring->failure);
  } else {
    ar_set_error("音频设备已关闭");
  }
}

static void ar_ring_free(ARFloatRing *ring) {
  if (!ring->initialized) {
    return;
  }
  pthread_cond_destroy(&ring->cond_write);
  pthread_cond_destroy(&ring->cond_read);
  pthread_mutex_destroy(&ring->mutex);
  free(ring->data);
  memset(ring, 0, sizeof(*ring));
}

static void ar_ring_drop_oldest_locked(ARFloatRing *ring, uint32_t count) {
  if (count >= ring->len) {
    ring->read_pos = ring->write_pos;
    ring->len = 0;
    return;
  }

  ring->read_pos = (ring->read_pos + count) % ring->capacity;
  ring->len -= count;
}

static void ar_ring_write_overwrite(ARFloatRing *ring, const float *samples, uint32_t count) {
  pthread_mutex_lock(&ring->mutex);
  if (ring->closed) {
    pthread_mutex_unlock(&ring->mutex);
    return;
  }

  // 上游回调必须提供完整的 interleaved sample-frame, 不能错位截断声道.
  if (count % ring->channels != 0) {
    pthread_mutex_unlock(&ring->mutex);
    ar_ring_fail(ring, kAudio_ParamError, "捕获 PCM 声道对齐");
    return;
  }
  if (count > ring->capacity) {
    ar_ring_record_drop(ring, count - ring->capacity);
    samples += count - ring->capacity;
    count = ring->capacity;
  }

  uint32_t free_slots = ring->capacity - ring->len;
  if (count > free_slots) {
    ar_ring_record_drop(ring, count - free_slots);
    ar_ring_drop_oldest_locked(ring, count - free_slots);
  }

  for (uint32_t i = 0; i < count; ++i) {
    ring->data[ring->write_pos] = samples[i];
    ring->write_pos = (ring->write_pos + 1) % ring->capacity;
  }
  ring->len += count;
  ar_ring_record_high_water(ring);

  pthread_cond_signal(&ring->cond_read);
  pthread_mutex_unlock(&ring->mutex);
}

static int ar_ring_read(ARFloatRing *ring, float *out, uint32_t count, uint32_t timeout_ms) {
  if (count == 0 || count > ring->capacity || count % ring->channels != 0 ||
      (ring->frame_samples != 0 && count != ring->frame_samples)) {
    ar_set_error("音频读取长度超出缓冲区范围");
    return -1;
  }
  pthread_mutex_lock(&ring->mutex);
  struct timespec deadline;
  ar_deadline_from_now(&deadline, timeout_ms);

  while (!ring->closed && ring->len < count) {
    if (ar_ring_wait(&ring->cond_read, &ring->mutex, &deadline) == ETIMEDOUT) {
      pthread_mutex_unlock(&ring->mutex);
      return 1;
    }
  }

  if (ring->closed) {
    ar_ring_report_failure_locked(ring);
    pthread_mutex_unlock(&ring->mutex);
    return -1;
  }

  for (uint32_t i = 0; i < count; ++i) {
    out[i] = ring->data[ring->read_pos];
    ring->read_pos = (ring->read_pos + 1) % ring->capacity;
  }
  ring->len -= count;

  pthread_cond_signal(&ring->cond_write);
  pthread_mutex_unlock(&ring->mutex);
  return 0;
}

static uint32_t ar_ring_read_partial_zero_fill(ARFloatRing *ring, float *out, uint32_t count) {
  pthread_mutex_lock(&ring->mutex);
  uint32_t to_copy = ring->len < count ? ring->len : count;
  for (uint32_t i = 0; i < to_copy; ++i) {
    out[i] = ring->data[ring->read_pos];
    ring->read_pos = (ring->read_pos + 1) % ring->capacity;
  }
  ring->len -= to_copy;
  pthread_cond_signal(&ring->cond_write);
  pthread_mutex_unlock(&ring->mutex);

  if (to_copy < count) {
    memset(out + to_copy, 0, (count - to_copy) * sizeof(float));
  }
  return to_copy;
}

static int ar_ring_write_wait(ARFloatRing *ring, const float *samples, uint32_t count, uint32_t timeout_ms) {
  if (count == 0 || count > ring->capacity || count % ring->channels != 0 ||
      (ring->frame_samples != 0 && count != ring->frame_samples)) {
    ar_set_error("音频写入长度超出缓冲区范围");
    return -1;
  }
  pthread_mutex_lock(&ring->mutex);
  struct timespec deadline;
  ar_deadline_from_now(&deadline, timeout_ms < 100 ? timeout_ms : 100);

  while (!ring->closed && (ring->len > ring->write_watermark || ring->capacity - ring->len < count)) {
    int status = ar_ring_wait(&ring->cond_write, &ring->mutex, &deadline);
    if (status != 0 && status != ETIMEDOUT) {
      pthread_mutex_unlock(&ring->mutex);
      ar_set_error("等待音频播放缓冲失败: %d", status);
      return -1;
    }
    if (status == ETIMEDOUT && !ring->closed &&
        (ring->len > ring->write_watermark || ring->capacity - ring->len < count)) {
      pthread_mutex_unlock(&ring->mutex);
      ar_set_error("音频播放队列超过水位且等待超时");
      return -1;
    }
  }

  if (ring->closed) {
    ar_ring_report_failure_locked(ring);
    pthread_mutex_unlock(&ring->mutex);
    return -1;
  }

  for (uint32_t i = 0; i < count; ++i) {
    ring->data[ring->write_pos] = samples[i];
    ring->write_pos = (ring->write_pos + 1) % ring->capacity;
  }
  ring->len += count;
  ar_ring_record_high_water(ring);

  pthread_cond_signal(&ring->cond_read);
  pthread_mutex_unlock(&ring->mutex);
  return 0;
}

typedef struct {
  ARPcmConverter converter;
  ARCaptureRing ring;
} ARCaptureState;

@interface ARSystemAudioCapture : NSObject {
@public
  AudioObjectID tapObjectID;
  AudioObjectID aggregateDeviceID;
  AudioDeviceIOProcID ioProcID;
  ARCaptureState state;
}
- (instancetype)initWithSampleRate:(uint32_t)sampleRate
                          channels:(uint32_t)channels
                         frameSize:(uint32_t)frameSize;
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
  OSStatus status = ar_pcm_push(&capture->converter, inInputData, ar_capture_pcm, &capture->ring);
  if (status != noErr) ar_capture_ring_fail(&capture->ring, status);
  return noErr;
}

@implementation ARSystemAudioCapture

- (instancetype)initWithSampleRate:(uint32_t)sampleRate
                          channels:(uint32_t)channels
                         frameSize:(uint32_t)frameSize {
  self = [super init];
  if (self == nil) {
    ar_set_error("failed to allocate system audio capture");
    return nil;
  }

  if (channels != 2) {
    ar_set_error("macOS system audio capture currently supports stereo only");
    return nil;
  }

  if (!ar_macos_capture_supported()) {
    ar_set_error("macOS 系统音频捕获需要 14.2 或更新版本");
    return nil;
  }

  tapObjectID = kAudioObjectUnknown;
  aggregateDeviceID = kAudioObjectUnknown;
  ioProcID = NULL;
  memset(&state, 0, sizeof(state));

  CATapDescription *tapDescription = [[CATapDescription alloc] initStereoGlobalTapButExcludeProcesses:@[]];
  if (tapDescription == nil) {
    ar_set_error("failed to create macOS system audio tap description");
    return nil;
  }

  tapDescription.name = [NSString stringWithFormat:@"synly-tap-%p", self];
  tapDescription.UUID = [NSUUID UUID];
  [tapDescription setPrivate:YES];
  tapDescription.muteBehavior = CATapUnmuted;

  OSStatus status = AudioHardwareCreateProcessTap(tapDescription, &tapObjectID);
  if (status != noErr) {
    ar_set_error("AudioHardwareCreateProcessTap failed with status %d", (int) status);
    return nil;
  }

  NSString *tapUID = [[tapDescription UUID] UUIDString];
  if (tapUID == nil) {
    ar_set_error("failed to obtain system audio tap UUID");
    return nil;
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
    return nil;
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

  // 从实际输入 stream 查询虚拟格式, 不能用 nominal rate 和声道数猜 ASBD.
  AudioObjectPropertyAddress streamsAddr = {
    .mSelector = kAudioDevicePropertyStreams, .mScope = kAudioDevicePropertyScopeInput,
    .mElement = kAudioObjectPropertyElementMain,
  };
  UInt32 streamsSize = 0;
  status = AudioObjectGetPropertyDataSize(aggregateDeviceID, &streamsAddr, 0, NULL, &streamsSize);
  if (status != noErr || streamsSize != sizeof(AudioStreamID)) {
    ar_set_error("捕获输入必须为单个音频 stream, OSStatus=%d", (int) status);
    return nil;
  }
  AudioStreamID streamID = kAudioObjectUnknown;
  status = AudioObjectGetPropertyData(aggregateDeviceID, &streamsAddr, 0, NULL, &streamsSize, &streamID);
  if (status != noErr) {
    ar_set_error("查询捕获输入 stream 失败, OSStatus=%d", (int) status);
    return nil;
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
    return nil;
  }

  UInt32 deviceBufferFrames = 0;
  UInt32 deviceBufferSize = sizeof(deviceBufferFrames);
  status = AudioObjectGetPropertyData(aggregateDeviceID, &bufferSizeAddr, 0, NULL,
                                      &deviceBufferSize, &deviceBufferFrames);
  if (status != noErr || deviceBufferSize != sizeof(deviceBufferFrames) || deviceBufferFrames == 0) {
    ar_set_error("无法查询实际捕获缓冲, OSStatus=%d", (int) status);
    return nil;
  }
  status = ar_pcm_init(&state.converter, &sourceFormat, sampleRate, channels, deviceBufferFrames);
  if (status != noErr) {
    ar_set_error("初始化捕获 PCM 转换失败, OSStatus=%d", (int) status);
    return nil;
  }
  int ringStatus = ar_capture_ring_init(&state.ring, sampleRate, channels, frameSize, state.converter.callback_output_frames);
  if (ringStatus != 0) {
    ar_set_error("初始化捕获 SPSC 缓冲失败, status=%d", ringStatus);
    return nil;
  }

  status = AudioDeviceCreateIOProcID(aggregateDeviceID, ar_system_audio_io_proc, &state, &ioProcID);
  if (status != noErr) {
    ar_set_error("AudioDeviceCreateIOProcID failed with status %d", (int) status);
    return nil;
  }

  status = AudioDeviceStart(aggregateDeviceID, ioProcID);
  if (status != noErr) {
    ar_set_error("AudioDeviceStart failed with status %d", (int) status);
    return nil;
  }

  return self;
}

- (void)dealloc {
  ar_capture_ring_close(&state.ring);

  if (ioProcID != NULL && aggregateDeviceID != kAudioObjectUnknown) {
    AudioDeviceStop(aggregateDeviceID, ioProcID);
    AudioDeviceDestroyIOProcID(aggregateDeviceID, ioProcID);
    ioProcID = NULL;
  }
  if (aggregateDeviceID != kAudioObjectUnknown) {
    AudioHardwareDestroyAggregateDevice(aggregateDeviceID);
    aggregateDeviceID = kAudioObjectUnknown;
  }
  if (tapObjectID != kAudioObjectUnknown) {
    AudioHardwareDestroyProcessTap(tapObjectID);
    tapObjectID = kAudioObjectUnknown;
  }
  ar_pcm_destroy(&state.converter);
  ar_capture_ring_free(&state.ring);
}

- (int)readSamples:(float *)out sampleCount:(uint32_t)count timeoutMs:(uint32_t)timeoutMs {
  int result = ar_capture_ring_read(&state.ring, out, count, timeoutMs);
  if (result < 0) {
    ar_set_error("捕获 SPSC 读取失败或已关闭, status=%d", atomic_load(&state.ring.failure));
  }
  return result;
}

@end

typedef struct {
  AudioQueueRef queue;
  AudioStreamBasicDescription format;
  AudioQueueBufferRef buffers[3];
  ARFloatRing ring;
  uint32_t buffer_samples;
} ARPlaybackEngine;

static void ar_output_callback(void *inUserData, AudioQueueRef inAQ, AudioQueueBufferRef inBuffer) {
  ARPlaybackEngine *engine = (ARPlaybackEngine *) inUserData;
  if (ar_ring_is_closed(&engine->ring)) {
    return;
  }
  uint32_t sample_count = engine->buffer_samples;
  ar_ring_read_partial_zero_fill(&engine->ring, (float *) inBuffer->mAudioData, sample_count);
  inBuffer->mAudioDataByteSize = sample_count * sizeof(float);
  OSStatus status = AudioQueueEnqueueBuffer(inAQ, inBuffer, 0, NULL);
  if (status != noErr) {
    ar_ring_fail(&engine->ring, status, "AudioQueueEnqueueBuffer");
  }
}

void *ar_macos_capture_create(const char *device_name, uint32_t sample_rate, uint32_t channels, uint32_t frame_size) {
  if (device_name != NULL && device_name[0] != '\0') {
    ar_set_error("macOS system audio capture does not support selecting a specific device yet");
    return NULL;
  }

  @autoreleasepool {
    ARSystemAudioCapture *capture =
        [[ARSystemAudioCapture alloc] initWithSampleRate:sample_rate channels:channels frameSize:frame_size];
    if (capture == nil) {
      return NULL;
    }
    return (__bridge_retained void *) capture;
  }
}

void ar_macos_capture_stats(void *handle, uint64_t *dropped, uint32_t *high_water) {
  ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *) handle;
  *dropped = atomic_load_explicit(&capture->state.ring.dropped_samples, memory_order_relaxed);
  *high_water = atomic_load_explicit(&capture->state.ring.high_water_samples, memory_order_relaxed);
}

void ar_macos_capture_destroy(void *handle) {
  if (handle != NULL) {
    @autoreleasepool {
      CFBridgingRelease(handle);
    }
  }
}

int ar_macos_capture_read(void *handle, float *out_samples, uint32_t sample_count, uint32_t timeout_ms) {
  @autoreleasepool {
    ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *) handle;
    return [capture readSamples:out_samples sampleCount:sample_count timeoutMs:timeout_ms];
  }
}

void *ar_macos_playback_create(uint32_t sample_rate, uint32_t channels, uint32_t frame_size) {
  ARPlaybackEngine *engine = (ARPlaybackEngine *) calloc(1, sizeof(ARPlaybackEngine));
  if (engine == NULL) {
    ar_set_error("failed to allocate playback engine");
    return NULL;
  }

  if (!ar_ring_init_audio(&engine->ring, sample_rate, channels, frame_size, true)) {
    free(engine);
    return NULL;
  }

  engine->format.mSampleRate = sample_rate;
  engine->format.mFormatID = kAudioFormatLinearPCM;
  engine->format.mFormatFlags = kLinearPCMFormatFlagIsFloat | kLinearPCMFormatFlagIsPacked;
  engine->format.mBitsPerChannel = 32;
  engine->format.mChannelsPerFrame = channels;
  engine->format.mFramesPerPacket = 1;
  engine->format.mBytesPerFrame = channels * sizeof(float);
  engine->format.mBytesPerPacket = engine->format.mBytesPerFrame;
  engine->buffer_samples = frame_size * channels;

  OSStatus status = AudioQueueNewOutput(&engine->format, ar_output_callback, engine, NULL, NULL, 0, &engine->queue);
  if (status != noErr) {
    ar_set_error("AudioQueueNewOutput failed with status %d", (int) status);
    ar_ring_free(&engine->ring);
    free(engine);
    return NULL;
  }

  uint32_t bufferBytes = engine->buffer_samples * sizeof(float);
  for (int i = 0; i < 3; ++i) {
    status = AudioQueueAllocateBuffer(engine->queue, bufferBytes, &engine->buffers[i]);
    if (status != noErr) {
      ar_set_error("AudioQueueAllocateBuffer failed with status %d", (int) status);
      AudioQueueDispose(engine->queue, true);
      ar_ring_free(&engine->ring);
      free(engine);
      return NULL;
    }
    memset(engine->buffers[i]->mAudioData, 0, bufferBytes);
    engine->buffers[i]->mAudioDataByteSize = bufferBytes;
    status = AudioQueueEnqueueBuffer(engine->queue, engine->buffers[i], 0, NULL);
    if (status != noErr) {
      ar_set_error("AudioQueueEnqueueBuffer 初始化失败, OSStatus=%d", (int) status);
      AudioQueueDispose(engine->queue, true);
      ar_ring_free(&engine->ring);
      free(engine);
      return NULL;
    }
  }

  status = AudioQueueStart(engine->queue, NULL);
  if (status != noErr) {
    ar_set_error("AudioQueueStart failed with status %d", (int) status);
    AudioQueueDispose(engine->queue, true);
    ar_ring_free(&engine->ring);
    free(engine);
    return NULL;
  }

  return engine;
}

void ar_macos_playback_stats(void *handle, uint64_t *dropped, uint32_t *high_water) {
  ARPlaybackEngine *engine = handle;
  ar_ring_copy_stats(&engine->ring, dropped, high_water);
}

void ar_macos_playback_destroy(void *handle) {
  if (handle == NULL) {
    return;
  }

  ARPlaybackEngine *engine = (ARPlaybackEngine *) handle;
  ar_ring_close(&engine->ring);
  if (engine->queue != NULL) {
    AudioQueueStop(engine->queue, true);
    AudioQueueDispose(engine->queue, true);
  }
  ar_ring_free(&engine->ring);
  free(engine);
}

int ar_macos_playback_submit(void *handle, const float *samples, uint32_t sample_count, uint32_t timeout_ms) {
  ARPlaybackEngine *engine = (ARPlaybackEngine *) handle;
  return ar_ring_write_wait(&engine->ring, samples, sample_count, timeout_ms);
}
