// 使用真实 AudioConverter, 不创建系统 tap 或打开音频设备.
#include <assert.h>
#include <stdio.h>
#include "../macos_audio_conversion.h"

typedef struct {
  float *samples;
  size_t capacity;
  size_t count;
} Collected;

static void collect(void *context, const float *samples, UInt32 count) {
  Collected *out = context;
  assert(out->count + count <= out->capacity);
  memcpy(out->samples + out->count, samples, count * sizeof(float));
  out->count += count;
}

// EOF 仅在测试中整个有限输入确实结束时使用, 生产 IOProc 始终是暂缺.
static OSStatus eof_input(AudioConverterRef converter, UInt32 *packets, AudioBufferList *data,
                          AudioStreamPacketDescription **descriptions, void *context) {
  (void) converter; (void) descriptions; (void) context;
  *packets = 0;
  data->mNumberBuffers = 1;
  data->mBuffers[0] = (AudioBuffer) {0};
  return noErr;
}

static Collected convert(Float64 input_rate, UInt32 channels, const float *samples, UInt32 frames,
                         const UInt32 *chunks, size_t chunk_count, UInt32 output_limit) {
  UInt32 max_chunk = 0;
  for (size_t i = 0; i < chunk_count; i++) if (chunks[i] > max_chunk) max_chunk = chunks[i];
  ARPcmConverter converter;
  AudioStreamBasicDescription format = ar_pcm_format(input_rate, channels);
  OSStatus status = ar_pcm_init(&converter, &format, 48000, 2, max_chunk);
  assert(status == noErr);
  // 故意限制单次输出为 7 帧, 验证必须多次 Fill 才能消费一个设备块.
  if (output_limit != 0 && converter.output_frames > output_limit) converter.output_frames = output_limit;
  Collected out = { .capacity = (size_t) ceil(frames * 48000.0 / input_rate) * 2 + 8192 };
  out.samples = calloc(out.capacity, sizeof(float));
  assert(out.samples != NULL);
  float *scratch = calloc(max_chunk, format.mBytesPerFrame);
  assert(scratch != NULL);
  size_t chunk = 0;
  UInt32 offset = 0;
  while (offset < frames) {
    UInt32 count = chunks[chunk++ % chunk_count];
    if (count > frames - offset) count = frames - offset;
    memcpy(scratch, samples + (size_t) offset * channels, count * format.mBytesPerFrame);
    AudioBufferList input = {.mNumberBuffers = 1};
    input.mBuffers[0] = (AudioBuffer) { .mNumberChannels = channels, .mData = scratch,
      .mDataByteSize = count * format.mBytesPerFrame };
    status = ar_pcm_push(&converter, &input, collect, &out);
    if (status != noErr) fprintf(stderr, "转换失败: rate=%g, offset=%u, chunk=%u, status=%d\n", input_rate, offset, count, (int)status);
    assert(status == noErr);
    // 立即破坏设备借出的输入, 下一块输出不得受影响.
    for (UInt32 i = 0; i < count * channels; i++) scratch[i] = NAN;
    offset += count;
  }
  if (converter.handle != NULL) {
    for (;;) {
      AudioBufferList output = {.mNumberBuffers = 1};
      output.mBuffers[0] = (AudioBuffer) { .mNumberChannels = 2, .mData = converter.output,
        .mDataByteSize = converter.output_frames * sizeof(float) * 2 };
      UInt32 count = converter.output_frames;
      assert(AudioConverterFillComplexBuffer(converter.handle, eof_input, NULL, &count, &output, NULL) == noErr);
      if (count == 0) break;
      collect(&out, converter.output, count * 2);
    }
  }
  ar_pcm_destroy(&converter);
  free(scratch);
  return out;
}

static void test_chunk_independence(Float64 rate, UInt32 channels) {
  UInt32 frames = (UInt32) rate / 2;
  float *input = calloc(frames * channels, sizeof(float));
  assert(input != NULL);
  for (UInt32 frame = 0; frame < frames; frame++) {
    for (UInt32 channel = 0; channel < channels; channel++) {
      input[frame * channels + channel] = 0.2 * sin(2 * M_PI * (channel ? 1200 : 440) * frame / rate);
    }
  }
  UInt32 large[] = {frames};
  UInt32 device[] = {512};
  UInt32 ragged[] = {1, 17, 441, 1024, 3, 512};
  Collected reference = convert(rate, channels, input, frames, large, 1, 0);
  for (int variant = 0; variant < 3; variant++) {
    Collected actual = convert(rate, channels, input, frames, variant == 0 ? device : ragged, variant == 0 ? 1 : 6, variant == 2 ? 7 : 0);
    fprintf(stderr, "采样率=%g, 声道=%u, 分块=%s, 参考帧=%zu, 实际帧=%zu\n", rate, channels,
            variant == 0 ? "512" : (variant == 1 ? "不等长" : "7 帧输出"), reference.count / 2, actual.count / 2);
    assert(actual.count == reference.count);
    double error = 0;
    for (size_t i = 0; i < actual.count; i++) {
      assert(isfinite(actual.samples[i]));
      double difference = fabs(actual.samples[i] - reference.samples[i]);
      if (difference > error) error = difference;
    }
    assert(error < 0.00001);
    free(actual.samples);
  }
  assert(llabs((long long) reference.count / 2 - 24000) <= 1);
  free(reference.samples);
  free(input);
}

static void test_format_and_callback_boundaries(void) {
  AudioStreamBasicDescription base = ar_pcm_format(48000, 2);
  for (int field = 0; field < 8; field++) {
    AudioStreamBasicDescription invalid = base;
    switch (field) {
      case 0: invalid.mSampleRate = NAN; break;
      case 1: invalid.mSampleRate = INFINITY; break;
      case 2: invalid.mFormatFlags |= kAudioFormatFlagIsNonInterleaved; break;
      case 3: invalid.mBitsPerChannel = 16; break;
      case 4: invalid.mBytesPerFrame = 4; break;
      case 5: invalid.mChannelsPerFrame = 0; break;
      case 6: invalid.mFramesPerPacket = 2; break;
      case 7: invalid.mFormatID = kAudioFormatMPEG4AAC; break;
    }
    ARPcmConverter converter;
    assert(ar_pcm_init(&converter, &invalid, 48000, 2, 512) != noErr);
    ar_pcm_destroy(&converter);
  }
  ARPcmConverter converter;
  assert(ar_pcm_init(&converter, &base, 48000, 2, UINT32_MAX) != noErr);
  assert(ar_pcm_init(&converter, &base, 48000, 2, 512) == noErr);
  float output[1024];
  Collected out = {.samples = output, .capacity = 1024};
  AudioBufferList input = {.mNumberBuffers = 1};
  input.mBuffers[0] = (AudioBuffer){ .mNumberChannels = 2, .mDataByteSize = 17 * 8, .mData = NULL };
  assert(ar_pcm_push(&converter, &input, collect, &out) == noErr);
  assert(out.count == 34);
  for (size_t i = 0; i < out.count; i++) assert(out.samples[i] == 0);
  out.count = 0;
  assert(ar_pcm_push(&converter, NULL, collect, &out) == noErr);
  input.mBuffers[0].mDataByteSize = 0;
  assert(ar_pcm_push(&converter, &input, collect, &out) == noErr);
  assert(out.count == 0);
  input.mBuffers[0].mDataByteSize = 513 * 8;
  assert(ar_pcm_push(&converter, &input, collect, &out) != noErr);
  input.mBuffers[0].mDataByteSize = 17;
  assert(ar_pcm_push(&converter, &input, collect, &out) != noErr);
  input.mBuffers[0].mDataByteSize = 16;
  input.mBuffers[0].mNumberChannels = 1;
  assert(ar_pcm_push(&converter, &input, collect, &out) != noErr);
  input.mNumberBuffers = 2;
  assert(ar_pcm_push(&converter, &input, collect, &out) != noErr);
  assert(out.count == 0);
  ar_pcm_destroy(&converter);
}

int main(void) {
  puts("[1/2] 真实 PCM 转换的分块连续性与输入内存生命周期");
  for (size_t i = 0; i < 5; i++) {
    Float64 rates[] = {8000, 44100, 48000, 96000, 192000};
    test_chunk_independence(rates[i], 2);
  }
  test_chunk_independence(44100, 1);
  puts("[2/2] ASBD, 字节边界和静音时间线");
  test_format_and_callback_boundaries();
  puts("macOS 真实 AudioConverter 测试通过");
  return 0;
}
