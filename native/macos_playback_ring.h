#ifndef SYNLY_MACOS_PLAYBACK_RING_H
#define SYNLY_MACOS_PLAYBACK_RING_H
#include "macos_audio_ring.h"

typedef struct {
  ARAudioRing samples;
  uint32_t watermark_frames;
  // 仅消费者更新状态, 统计值供工作线程读取. 不在实时回调打印日志.
  bool consumed_audio;
  bool pending_gap;
  _Atomic uint32_t resumed_gaps;
} ARPlaybackRing;

static int ar_playback_ring_init(ARPlaybackRing *ring, uint32_t rate, uint32_t channels, uint32_t frame_size) {
  memset(ring, 0, sizeof(*ring));
  if (!ar_audio_ring_format_valid(rate, channels, frame_size)) return KERN_INVALID_ARGUMENT;
  atomic_init(&ring->resumed_gaps, 0);
  if (!atomic_is_lock_free(&ring->resumed_gaps)) return KERN_NOT_SUPPORTED;
  ring->watermark_frames = (uint32_t)(((uint64_t)rate * 50 + 999) / 1000);
  return ar_audio_ring_init(&ring->samples, (uint64_t)ring->watermark_frames + frame_size, channels, frame_size);
}

// 提交前 50 ms 水位, 容量另加一完整帧. 仅阻塞播放工作线程, 不阻塞 AudioQueue 回调.
// 0=成功, 1=超时, -1=关闭/故障/参数错误. 超时不把新帧放入缓冲.
static int ar_playback_ring_submit(ARPlaybackRing *ring, const float *samples, uint32_t count, uint32_t timeout_ms) {
  ARAudioRing *storage = &ring->samples;
  if (samples == NULL || count != storage->frame_samples) return -1;
  uint64_t deadline = ar_audio_monotonic_ns() + (uint64_t)(timeout_ms < 100 ? timeout_ms : 100) * 1000000;
  for (;;) {
    if (atomic_load_explicit(&storage->closed, memory_order_acquire)) return -1;
    uint32_t used = ar_audio_ring_producer_used(storage);
    if (used <= ring->watermark_frames && count / storage->channels <= storage->capacity_frames - used) {
      // 此后只有消费者能改变空闲空间, 因而不会发生检查后被其它生产者填满的竞争.
      return ar_audio_ring_push(storage, samples, count);
    }
    int status = ar_audio_ring_wait(storage, deadline);
    if (status != 0) {
      if (atomic_load_explicit(&storage->closed, memory_order_acquire)) return -1;
      return status;
    }
  }
}

// 不足设备请求长度时补零, 但不凭空消费样本. 函数中无等待或分配.
static int ar_playback_ring_fill(ARPlaybackRing *ring, float *out, uint32_t count) {
  uint32_t copied;
  int result = ar_audio_ring_take(&ring->samples, out, count, true, &copied);
  if (result != 0) return result;
  if (copied != 0) {
    if (ring->pending_gap) atomic_fetch_add_explicit(&ring->resumed_gaps, 1, memory_order_relaxed);
    ring->consumed_audio = true;
    ring->pending_gap = false;
  }
  if (copied < count && ring->consumed_audio) ring->pending_gap = true;
  memset(out + copied, 0, (size_t)(count - copied) * sizeof(float));
  if (copied != 0) ar_audio_ring_notify(&ring->samples);
  return atomic_load_explicit(&ring->samples.closed, memory_order_acquire) ? -1 : 0;
}
#endif
