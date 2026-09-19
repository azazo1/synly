#ifndef SYNLY_MACOS_AUDIO_CONVERSION_H
#define SYNLY_MACOS_AUDIO_CONVERSION_H

#include <AudioToolbox/AudioConverter.h>
#include <math.h>
#include <stdint.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>

// Sunshine av_audio.mm 的输入提供与转换路径. 暂缺数据不等同于 EOF,
// 输出容量不足时继续排空转换器, 不在下一次 IOProc 覆盖其仍持有的输入.
static const OSStatus AR_CONVERTER_NEEDS_INPUT = 0x61727774;

typedef struct {
  const float *data;
  UInt32 frames;
  UInt32 provided;
  UInt32 channels;
} ARConverterInput;

typedef struct {
  AudioStreamBasicDescription source;
  AudioStreamBasicDescription target;
  AudioConverterRef handle;
  float *output;
  float *silence;
  UInt32 input_frames;
  UInt32 output_frames;
  UInt32 callback_output_frames;
} ARPcmConverter;

static AudioStreamBasicDescription ar_pcm_format(Float64 rate, UInt32 channels) {
  AudioStreamBasicDescription format = {0};
  format.mSampleRate = rate;
  format.mFormatID = kAudioFormatLinearPCM;
  format.mFormatFlags = kAudioFormatFlagsNativeFloatPacked;
  format.mBytesPerPacket = format.mBytesPerFrame = channels * sizeof(float);
  format.mFramesPerPacket = 1;
  format.mChannelsPerFrame = channels;
  format.mBitsPerChannel = 32;
  return format;
}

static bool ar_pcm_format_supported(const AudioStreamBasicDescription *format) {
  return isfinite(format->mSampleRate) && format->mSampleRate >= 8000 && format->mSampleRate <= 384000 &&
      format->mFormatID == kAudioFormatLinearPCM &&
      (format->mFormatFlags & ~kAudioFormatFlagIsNonMixable) == kAudioFormatFlagsNativeFloatPacked &&
      format->mChannelsPerFrame >= 1 && format->mChannelsPerFrame <= 8 &&
      format->mBitsPerChannel == 32 && format->mFramesPerPacket == 1 &&
      format->mBytesPerFrame == format->mChannelsPerFrame * sizeof(float) &&
      format->mBytesPerPacket == format->mBytesPerFrame;
}

static void ar_pcm_destroy(ARPcmConverter *converter) {
  if (converter->handle != NULL) AudioConverterDispose(converter->handle);
  free(converter->output);
  free(converter->silence);
  memset(converter, 0, sizeof(*converter));
}

static OSStatus ar_pcm_init(ARPcmConverter *converter, const AudioStreamBasicDescription *source,
                            UInt32 target_rate, UInt32 target_channels, UInt32 input_frames) {
  memset(converter, 0, sizeof(*converter));
  converter->source = *source;
  converter->target = ar_pcm_format(target_rate, target_channels);
  if (!ar_pcm_format_supported(source) || !ar_pcm_format_supported(&converter->target) ||
      input_frames == 0 || input_frames > UINT32_MAX / source->mBytesPerFrame) return kAudio_ParamError;
  converter->input_frames = input_frames;
  OSStatus status = noErr;
  UInt32 estimated_bytes = input_frames * source->mBytesPerFrame;
  AudioConverterPrimeInfo prime = {0};
  if (source->mSampleRate != target_rate || source->mChannelsPerFrame != target_channels) {
    status = AudioConverterNew(source, &converter->target, &converter->handle);
    if (status != noErr) goto failed;
    UInt32 size = sizeof(estimated_bytes);
    status = AudioConverterGetProperty(converter->handle, kAudioConverterPropertyCalculateOutputBufferSize,
                                       &size, &estimated_bytes);
    if (status != noErr) goto failed;
    size = sizeof(prime);
    status = AudioConverterGetProperty(converter->handle, kAudioConverterPrimeInfo, &size, &prime);
    if (status != noErr) goto failed;
  }
  // 属性估算和完整输入块的时长取较大值. 多保留一个采样帧覆盖分数相位.
  double frames = ceil(((double) input_frames + prime.leadingFrames + prime.trailingFrames) *
                       target_rate / source->mSampleRate) + 1;
  double estimated_frames = ceil((double) estimated_bytes / converter->target.mBytesPerFrame);
  if (estimated_frames > frames) frames = estimated_frames;
  // 每次回调最多接受两个输出块, 超过此显式预算即报告设备格式/转换异常.
  // 与设备输入缓冲一起限定 ring 预分配, 不把任何单次属性估算当成绝对上界.
  if (!isfinite(frames) || frames > UINT32_MAX / converter->target.mBytesPerFrame / 2) {
    status = kAudio_ParamError;
    goto failed;
  }
  converter->output_frames = (UInt32) frames;
  converter->callback_output_frames = converter->handle != NULL ? converter->output_frames * 2 : input_frames;
  converter->output = calloc(converter->output_frames, converter->target.mBytesPerFrame);
  converter->silence = calloc(input_frames, source->mBytesPerFrame);
  if (converter->output == NULL || converter->silence == NULL) {
    status = kAudio_MemFullError;
    goto failed;
  }
  return noErr;
failed:
  ar_pcm_destroy(converter);
  return status;
}

static OSStatus ar_converter_input_proc(AudioConverterRef converter, UInt32 *packets,
    AudioBufferList *data, AudioStreamPacketDescription **descriptions, void *context) {
  (void) converter;
  if (descriptions != NULL) *descriptions = NULL;
  ARConverterInput *input = context;
  if (input->provided == input->frames) {
    *packets = 0;
    data->mNumberBuffers = 1;
    data->mBuffers[0] = (AudioBuffer) { .mNumberChannels = input->channels };
    return AR_CONVERTER_NEEDS_INPUT;
  }
  UInt32 frames = input->frames - input->provided;
  if (frames > *packets) frames = *packets;
  data->mNumberBuffers = 1;
  data->mBuffers[0] = (AudioBuffer) {
    .mNumberChannels = input->channels,
    .mDataByteSize = frames * input->channels * sizeof(float),
    .mData = (void *) (input->data + (size_t) input->provided * input->channels),
  };
  input->provided += frames;
  *packets = frames;
  return noErr;
}

typedef void (*ARPcmConsumer)(void *context, const float *samples, UInt32 count);

static OSStatus ar_pcm_push(ARPcmConverter *converter, const AudioBufferList *input,
                            ARPcmConsumer consume, void *context) {
  if (input == NULL || input->mNumberBuffers == 0) return noErr;
  if (input->mNumberBuffers != 1) return kAudio_ParamError;
  const AudioBuffer *buffer = &input->mBuffers[0];
  if (buffer->mNumberChannels != converter->source.mChannelsPerFrame ||
      buffer->mDataByteSize % converter->source.mBytesPerFrame != 0) return kAudio_ParamError;
  UInt32 frames = buffer->mDataByteSize / converter->source.mBytesPerFrame;
  if (frames > converter->input_frames) return kAudio_ParamError;
  if (frames == 0) return noErr;
  // NULL 数据但有帧数代表等长静音, 不插入与设备时间无关的一整编码帧.
  const float *samples = buffer->mData != NULL ? buffer->mData : converter->silence;
  if (converter->handle == NULL) {
    consume(context, samples, frames * converter->target.mChannelsPerFrame);
    return noErr;
  }
  ARConverterInput source = { .data = samples, .frames = frames,
                             .channels = converter->source.mChannelsPerFrame };
  UInt32 total = 0;
  for (;;) {
    AudioBufferList output = { .mNumberBuffers = 1 };
    output.mBuffers[0] = (AudioBuffer) { .mNumberChannels = converter->target.mChannelsPerFrame,
      .mDataByteSize = converter->output_frames * converter->target.mBytesPerFrame, .mData = converter->output };
    UInt32 count = converter->output_frames;
    OSStatus status = AudioConverterFillComplexBuffer(converter->handle, ar_converter_input_proc,
                                                       &source, &count, &output, NULL);
    if (status != noErr && status != AR_CONVERTER_NEEDS_INPUT) return status;
    if (count > converter->output_frames || count > converter->callback_output_frames - total ||
        output.mBuffers[0].mDataByteSize != count * converter->target.mBytesPerFrame) return kAudio_ParamError;
    if (count > 0) consume(context, converter->output, count * converter->target.mChannelsPerFrame);
    total += count;
    // 此状态保证转换器已经再次索要输入, 不再借用当前设备回调的内存.
    if (status == AR_CONVERTER_NEEDS_INPUT) return noErr;
    if (count == 0 || total == converter->callback_output_frames) return kAudio_ParamError;
  }
}
#endif
