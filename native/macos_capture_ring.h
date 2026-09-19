#ifndef SYNLY_MACOS_CAPTURE_RING_H
#define SYNLY_MACOS_CAPTURE_RING_H
#include "macos_audio_ring.h"

static int ar_capture_ring_init(ARAudioRing *ring, uint32_t rate, uint32_t channels,
                                uint32_t frame_size, uint32_t callback_frames) {
  memset(ring, 0, sizeof(*ring));
  if (!ar_audio_ring_format_valid(rate, channels, frame_size) || callback_frames == 0) return KERN_INVALID_ARGUMENT;
  uint64_t capacity = (uint64_t)frame_size + callback_frames;
  uint64_t floor = ((uint64_t)rate * 30 + 999) / 1000;
  if (capacity < floor) capacity = floor;
  return ar_audio_ring_init(ring, capacity, channels, frame_size);
}

static bool ar_capture_ring_write(ARAudioRing *ring, const float *samples, uint32_t count) {
  if (atomic_load_explicit(&ring->closed, memory_order_acquire)) return false;
  if (samples == NULL || count == 0 || count % ring->channels != 0) {
    ar_audio_ring_fail(ring, KERN_INVALID_ARGUMENT);
    return false;
  }
  int status = ar_audio_ring_push(ring, samples, count);
  if (status == 1) {
    // 只有生产者更新统计. 饱和计数不需要 CAS 重试, 超容量时拒绝整个新块.
    uint64_t dropped = atomic_load_explicit(&ring->dropped_samples, memory_order_relaxed);
    atomic_store_explicit(&ring->dropped_samples, UINT64_MAX - dropped < count ? UINT64_MAX : dropped + count, memory_order_relaxed);
  } else if (status == 0) ar_audio_ring_notify(ring);
  return status == 0;
}

static int ar_capture_ring_try_read(ARAudioRing *ring, float *out, uint32_t count) {
  if (count != ring->frame_samples) return -1;
  uint32_t copied;
  return ar_audio_ring_take(ring, out, count, false, &copied);
}

static int ar_capture_ring_read(ARAudioRing *ring, float *out, uint32_t count, uint32_t timeout_ms) {
  uint64_t deadline = ar_audio_monotonic_ns() + (uint64_t)timeout_ms * 1000000;
  for (;;) {
    int result = ar_capture_ring_try_read(ring, out, count);
    if (result != 1) return result;
    result = ar_audio_ring_wait(ring, deadline);
    if (result != 0) return result < 0 ? -1 : ar_capture_ring_try_read(ring, out, count);
  }
}
#endif
