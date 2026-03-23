#import <AudioToolbox/AudioConverter.h>
#import <AudioToolbox/AudioToolbox.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#import <CoreAudio/CoreAudio.h>
#import <Foundation/Foundation.h>
#include <errno.h>
#include <pthread.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

static pthread_mutex_t g_error_mutex = PTHREAD_MUTEX_INITIALIZER;
static char g_last_error[512] = "macOS audio backend error";

static void ar_set_error(const char *fmt, ...) {
  va_list args;
  va_start(args, fmt);
  pthread_mutex_lock(&g_error_mutex);
  vsnprintf(g_last_error, sizeof(g_last_error), fmt, args);
  pthread_mutex_unlock(&g_error_mutex);
  va_end(args);
}

const char *ar_macos_last_error(void) {
  return g_last_error;
}

typedef struct {
  float *data;
  uint32_t capacity;
  uint32_t read_pos;
  uint32_t write_pos;
  uint32_t len;
  bool closed;
  pthread_mutex_t mutex;
  pthread_cond_t cond_read;
  pthread_cond_t cond_write;
} ARFloatRing;

typedef struct {
  float *input_data;
  UInt32 input_frames;
  UInt32 frames_provided;
  UInt32 device_channels;
} ARConverterInput;

static void ar_deadline_from_now(struct timespec *ts, uint32_t timeout_ms) {
  clock_gettime(CLOCK_REALTIME, ts);
  ts->tv_sec += timeout_ms / 1000;
  long nanos = ts->tv_nsec + (long) (timeout_ms % 1000) * 1000000L;
  ts->tv_sec += nanos / 1000000000L;
  ts->tv_nsec = nanos % 1000000000L;
}

static bool ar_ring_init(ARFloatRing *ring, uint32_t capacity) {
  memset(ring, 0, sizeof(*ring));
  ring->data = (float *) calloc(capacity, sizeof(float));
  if (ring->data == NULL) {
    ar_set_error("failed to allocate ring buffer");
    return false;
  }
  ring->capacity = capacity;
  pthread_mutex_init(&ring->mutex, NULL);
  pthread_cond_init(&ring->cond_read, NULL);
  pthread_cond_init(&ring->cond_write, NULL);
  return true;
}

static void ar_ring_close(ARFloatRing *ring) {
  pthread_mutex_lock(&ring->mutex);
  ring->closed = true;
  pthread_cond_broadcast(&ring->cond_read);
  pthread_cond_broadcast(&ring->cond_write);
  pthread_mutex_unlock(&ring->mutex);
}

static void ar_ring_free(ARFloatRing *ring) {
  if (ring->data != NULL) {
    free(ring->data);
    ring->data = NULL;
  }
  pthread_mutex_destroy(&ring->mutex);
  pthread_cond_destroy(&ring->cond_read);
  pthread_cond_destroy(&ring->cond_write);
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

  if (count > ring->capacity) {
    samples += count - ring->capacity;
    count = ring->capacity;
  }

  uint32_t free_slots = ring->capacity - ring->len;
  if (count > free_slots) {
    ar_ring_drop_oldest_locked(ring, count - free_slots);
  }

  for (uint32_t i = 0; i < count; ++i) {
    ring->data[ring->write_pos] = samples[i];
    ring->write_pos = (ring->write_pos + 1) % ring->capacity;
  }
  ring->len += count;

  pthread_cond_signal(&ring->cond_read);
  pthread_mutex_unlock(&ring->mutex);
}

static int ar_ring_read(ARFloatRing *ring, float *out, uint32_t count, uint32_t timeout_ms) {
  pthread_mutex_lock(&ring->mutex);
  struct timespec deadline;
  ar_deadline_from_now(&deadline, timeout_ms);

  while (!ring->closed && ring->len < count) {
    if (pthread_cond_timedwait(&ring->cond_read, &ring->mutex, &deadline) == ETIMEDOUT) {
      pthread_mutex_unlock(&ring->mutex);
      return 1;
    }
  }

  if (ring->closed) {
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
  pthread_mutex_lock(&ring->mutex);
  struct timespec deadline;
  ar_deadline_from_now(&deadline, timeout_ms);

  while (!ring->closed && ring->capacity - ring->len < count) {
    if (pthread_cond_timedwait(&ring->cond_write, &ring->mutex, &deadline) == ETIMEDOUT) {
      pthread_mutex_unlock(&ring->mutex);
      ar_set_error("audio output ring buffer timed out");
      return -1;
    }
  }

  if (ring->closed) {
    pthread_mutex_unlock(&ring->mutex);
    ar_set_error("audio output backend has been closed");
    return -1;
  }

  for (uint32_t i = 0; i < count; ++i) {
    ring->data[ring->write_pos] = samples[i];
    ring->write_pos = (ring->write_pos + 1) % ring->capacity;
  }
  ring->len += count;

  pthread_cond_signal(&ring->cond_read);
  pthread_mutex_unlock(&ring->mutex);
  return 0;
}

static UInt32 ar_min_u32(UInt32 lhs, UInt32 rhs) {
  return lhs < rhs ? lhs : rhs;
}

static OSStatus ar_converter_input_proc(
    AudioConverterRef inAudioConverter,
    UInt32 *ioNumberDataPackets,
    AudioBufferList *ioData,
    AudioStreamPacketDescription **outDataPacketDescription,
    void *inUserData) {
  (void) inAudioConverter;
  (void) outDataPacketDescription;

  ARConverterInput *input = (ARConverterInput *) inUserData;
  if (input->frames_provided >= input->input_frames) {
    *ioNumberDataPackets = 0;
    return noErr;
  }

  UInt32 frames = ar_min_u32(*ioNumberDataPackets, input->input_frames - input->frames_provided);
  ioData->mNumberBuffers = 1;
  ioData->mBuffers[0].mNumberChannels = input->device_channels;
  ioData->mBuffers[0].mDataByteSize = frames * input->device_channels * sizeof(float);
  ioData->mBuffers[0].mData = input->input_data + (input->frames_provided * input->device_channels);
  input->frames_provided += frames;
  *ioNumberDataPackets = frames;
  return noErr;
}

@interface ARSystemAudioCapture : NSObject {
@public
  AudioObjectID tapObjectID;
  AudioObjectID aggregateDeviceID;
  AudioDeviceIOProcID ioProcID;
  AudioConverterRef audioConverter;
  float *conversionBuffer;
  UInt32 conversionBufferSize;
  UInt32 clientSampleRate;
  UInt32 clientChannels;
  UInt32 clientFrameSize;
  Float64 deviceSampleRate;
  UInt32 deviceChannels;
  ARFloatRing ring;
}
- (instancetype)initWithSampleRate:(uint32_t)sampleRate
                          channels:(uint32_t)channels
                         frameSize:(uint32_t)frameSize;
- (int)readSamples:(float *)out sampleCount:(uint32_t)count timeoutMs:(uint32_t)timeoutMs;
@end

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

  ARSystemAudioCapture *capture = (__bridge ARSystemAudioCapture *) inClientData;
  bool wrote = false;

  if (inInputData != NULL && inInputData->mNumberBuffers > 0) {
    AudioBuffer input = inInputData->mBuffers[0];
    if (input.mData != NULL && input.mDataByteSize > 0 && capture->deviceChannels > 0) {
      if (capture->audioConverter != NULL) {
        UInt32 inputFrames = input.mDataByteSize / (capture->deviceChannels * sizeof(float));
        ARConverterInput converterInput;
        converterInput.input_data = (float *) input.mData;
        converterInput.input_frames = inputFrames;
        converterInput.frames_provided = 0;
        converterInput.device_channels = capture->deviceChannels;

        AudioBufferList output;
        memset(&output, 0, sizeof(output));
        output.mNumberBuffers = 1;
        output.mBuffers[0].mNumberChannels = capture->clientChannels;
        output.mBuffers[0].mDataByteSize = capture->conversionBufferSize;
        output.mBuffers[0].mData = capture->conversionBuffer;

        UInt32 outputFrames = capture->conversionBufferSize / (capture->clientChannels * sizeof(float));
        OSStatus status = AudioConverterFillComplexBuffer(
            capture->audioConverter,
            ar_converter_input_proc,
            &converterInput,
            &outputFrames,
            &output,
            NULL);

        if (status == noErr && outputFrames > 0) {
          ar_ring_write_overwrite(&capture->ring, capture->conversionBuffer, outputFrames * capture->clientChannels);
          wrote = true;
        }
      } else {
        ar_ring_write_overwrite(&capture->ring, (const float *) input.mData, input.mDataByteSize / sizeof(float));
        wrote = true;
      }
    }
  }

  if (!wrote) {
    UInt32 silenceSamples = capture->clientFrameSize * capture->clientChannels;
    UInt32 availableSamples = capture->conversionBufferSize / sizeof(float);
    if (silenceSamples > availableSamples) {
      silenceSamples = availableSamples;
    }
    if (silenceSamples > 0) {
      memset(capture->conversionBuffer, 0, silenceSamples * sizeof(float));
      ar_ring_write_overwrite(&capture->ring, capture->conversionBuffer, silenceSamples);
    }
  }

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

  NSOperatingSystemVersion minimum = {14, 0, 0};
  if (![[NSProcessInfo processInfo] isOperatingSystemAtLeastVersion:minimum]) {
    ar_set_error("macOS system audio capture requires macOS 14.0 or newer");
    return nil;
  }

  uint32_t ringCapacity = sampleRate * channels * 2;
  if (!ar_ring_init(&ring, ringCapacity)) {
    return nil;
  }

  tapObjectID = kAudioObjectUnknown;
  aggregateDeviceID = kAudioObjectUnknown;
  ioProcID = NULL;
  audioConverter = NULL;
  conversionBuffer = NULL;
  conversionBufferSize = 0;
  clientSampleRate = sampleRate;
  clientChannels = channels;
  clientFrameSize = frameSize;
  deviceSampleRate = sampleRate;
  deviceChannels = channels;

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

  UInt32 queryRateSize = sizeof(deviceSampleRate);
  status = AudioObjectGetPropertyData(aggregateDeviceID, &sampleRateAddr, 0, NULL, &queryRateSize, &deviceSampleRate);
  if (status != noErr || deviceSampleRate <= 0.0) {
    deviceSampleRate = sampleRate;
  }

  AudioObjectPropertyAddress streamConfigAddr = {
    .mSelector = kAudioDevicePropertyStreamConfiguration,
    .mScope = kAudioDevicePropertyScopeInput,
    .mElement = kAudioObjectPropertyElementMain,
  };
  UInt32 streamConfigSize = 0;
  status = AudioObjectGetPropertyDataSize(aggregateDeviceID, &streamConfigAddr, 0, NULL, &streamConfigSize);
  if (status == noErr && streamConfigSize > 0) {
    AudioBufferList *streamConfig = (AudioBufferList *) malloc(streamConfigSize);
    if (streamConfig != NULL) {
      status = AudioObjectGetPropertyData(aggregateDeviceID, &streamConfigAddr, 0, NULL, &streamConfigSize, streamConfig);
      if (status == noErr && streamConfig->mNumberBuffers > 0) {
        deviceChannels = streamConfig->mBuffers[0].mNumberChannels;
      }
      free(streamConfig);
    }
  }
  if (deviceChannels == 0) {
    deviceChannels = channels;
  }

  uint32_t roundedDeviceRate = (uint32_t) (deviceSampleRate + 0.5);
  if (roundedDeviceRate != sampleRate || deviceChannels != channels) {
    AudioStreamBasicDescription sourceFormat;
    memset(&sourceFormat, 0, sizeof(sourceFormat));
    sourceFormat.mSampleRate = deviceSampleRate;
    sourceFormat.mFormatID = kAudioFormatLinearPCM;
    sourceFormat.mFormatFlags = kLinearPCMFormatFlagIsFloat | kLinearPCMFormatFlagIsPacked;
    sourceFormat.mBitsPerChannel = 32;
    sourceFormat.mChannelsPerFrame = deviceChannels;
    sourceFormat.mFramesPerPacket = 1;
    sourceFormat.mBytesPerFrame = deviceChannels * sizeof(float);
    sourceFormat.mBytesPerPacket = sourceFormat.mBytesPerFrame;

    AudioStreamBasicDescription targetFormat;
    memset(&targetFormat, 0, sizeof(targetFormat));
    targetFormat.mSampleRate = sampleRate;
    targetFormat.mFormatID = kAudioFormatLinearPCM;
    targetFormat.mFormatFlags = kLinearPCMFormatFlagIsFloat | kLinearPCMFormatFlagIsPacked;
    targetFormat.mBitsPerChannel = 32;
    targetFormat.mChannelsPerFrame = channels;
    targetFormat.mFramesPerPacket = 1;
    targetFormat.mBytesPerFrame = channels * sizeof(float);
    targetFormat.mBytesPerPacket = targetFormat.mBytesPerFrame;

    status = AudioConverterNew(&sourceFormat, &targetFormat, &audioConverter);
    if (status != noErr) {
      ar_set_error("AudioConverterNew failed with status %d", (int) status);
      return nil;
    }
  }

  conversionBufferSize = frameSize * channels * sizeof(float) * 8;
  conversionBuffer = (float *) calloc(conversionBufferSize, 1);
  if (conversionBuffer == NULL) {
    ar_set_error("failed to allocate system audio conversion buffer");
    return nil;
  }

  status = AudioDeviceCreateIOProcID(aggregateDeviceID, ar_system_audio_io_proc, (__bridge void *) self, &ioProcID);
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
  ar_ring_close(&ring);

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
  if (audioConverter != NULL) {
    AudioConverterDispose(audioConverter);
    audioConverter = NULL;
  }
  if (conversionBuffer != NULL) {
    free(conversionBuffer);
    conversionBuffer = NULL;
  }

  ar_ring_free(&ring);
}

- (int)readSamples:(float *)out sampleCount:(uint32_t)count timeoutMs:(uint32_t)timeoutMs {
  return ar_ring_read(&ring, out, count, timeoutMs);
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
  uint32_t sample_count = engine->buffer_samples;
  ar_ring_read_partial_zero_fill(&engine->ring, (float *) inBuffer->mAudioData, sample_count);
  inBuffer->mAudioDataByteSize = sample_count * sizeof(float);
  AudioQueueEnqueueBuffer(inAQ, inBuffer, 0, NULL);
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

  uint32_t ringCapacity = sample_rate * channels;
  if (!ar_ring_init(&engine->ring, ringCapacity)) {
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
    AudioQueueEnqueueBuffer(engine->queue, engine->buffers[i], 0, NULL);
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
