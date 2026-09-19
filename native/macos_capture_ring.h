#ifndef SYNLY_MACOS_CAPTURE_RING_H
#define SYNLY_MACOS_CAPTURE_RING_H

#include <mach/mach.h>
#include <mach/semaphore.h>
#include <mach/sync_policy.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

// Sunshine TPCircularBuffer 的 SPSC 所有权: 生产者从不覆盖消费者仍可能读取的内存.
// 按完整声道帧计数, 游标在 [0, 2 * capacity) 循环, 容量不必扩大为二的幂.
typedef struct {
  float *data;
  uint32_t capacity_frames;
  uint32_t channels;
  uint32_t frame_samples;
  _Atomic uint32_t read_cursor;
  _Atomic uint32_t write_cursor;
  _Atomic uint64_t dropped_samples;
  _Atomic uint32_t high_water_samples;
  _Atomic int failure;
  _Atomic bool closed;
  _Atomic bool notified;
  semaphore_t ready;
  bool initialized;
} ARCaptureRing;

static uint32_t ar_capture_distance(const ARCaptureRing *ring, uint32_t write, uint32_t read) {
  return write >= read ? write - read : write + ring->capacity_frames * 2 - read;
}

static int ar_capture_ring_init(ARCaptureRing *ring, uint32_t rate, uint32_t channels,
                                uint32_t frame_size, uint32_t callback_frames) {
  memset(ring, 0, sizeof(*ring));
  if (rate < 8000 || rate > 48000 || channels == 0 || channels > 8 || frame_size == 0 ||
      (uint64_t)frame_size * 1000 > (uint64_t)rate * 60 || callback_frames == 0) return KERN_INVALID_ARGUMENT;
  uint64_t capacity = (uint64_t)frame_size + callback_frames;
  uint64_t floor = ((uint64_t)rate * 30 + 999) / 1000;
  if (capacity < floor) capacity = floor;
  if (capacity > UINT32_MAX / sizeof(float) / channels) return KERN_INVALID_ARGUMENT;
  atomic_init(&ring->read_cursor, 0);
  atomic_init(&ring->write_cursor, 0);
  atomic_init(&ring->dropped_samples, 0);
  atomic_init(&ring->high_water_samples, 0);
  atomic_init(&ring->failure, 0);
  atomic_init(&ring->closed, false);
  atomic_init(&ring->notified, false);
  if (!atomic_is_lock_free(&ring->read_cursor) || !atomic_is_lock_free(&ring->write_cursor) ||
      !atomic_is_lock_free(&ring->dropped_samples) || !atomic_is_lock_free(&ring->high_water_samples) ||
      !atomic_is_lock_free(&ring->failure) || !atomic_is_lock_free(&ring->closed) ||
      !atomic_is_lock_free(&ring->notified)) return KERN_NOT_SUPPORTED;
  ring->data = calloc((size_t)capacity * channels, sizeof(float));
  if (ring->data == NULL) return KERN_RESOURCE_SHORTAGE;
  int status = semaphore_create(mach_task_self(), &ring->ready, SYNC_POLICY_FIFO, 0);
  if (status != KERN_SUCCESS) {
    free(ring->data);
    ring->data = NULL;
    return status;
  }
  ring->capacity_frames = (uint32_t)capacity;
  ring->channels = channels;
  ring->frame_samples = frame_size * channels;
  ring->initialized = true;
  return 0;
}

static void ar_capture_notify(ARCaptureRing *ring) {
  // 至多一个未消费的通知, 连续回调不会积累无界 semaphore 计数.
  if (!atomic_exchange_explicit(&ring->notified, true, memory_order_acq_rel)) {
    int status = semaphore_signal(ring->ready);
    if (status != KERN_SUCCESS) {
      int expected = 0;
      atomic_compare_exchange_strong(&ring->failure, &expected, status);
      atomic_store_explicit(&ring->closed, true, memory_order_release);
    }
  }
}

static void ar_capture_ring_close(ARCaptureRing *ring) {
  if (!ring->initialized) return;
  atomic_store_explicit(&ring->closed, true, memory_order_release);
  ar_capture_notify(ring);
}

static void ar_capture_ring_fail(ARCaptureRing *ring, int failure) {
  int expected = 0;
  atomic_compare_exchange_strong(&ring->failure, &expected, failure);
  ar_capture_ring_close(ring);
}

// 只能在生产者与消费者都停止后释放. 支持尚未完成初始化的清理.
static void ar_capture_ring_free(ARCaptureRing *ring) {
  if (!ring->initialized) return;
  semaphore_destroy(mach_task_self(), ring->ready);
  free(ring->data);
  memset(ring, 0, sizeof(*ring));
}

static bool ar_capture_ring_write(ARCaptureRing *ring, const float *samples, uint32_t count) {
  if (atomic_load_explicit(&ring->closed, memory_order_acquire)) return false;
  if (samples == NULL || count % ring->channels != 0) {
    ar_capture_ring_fail(ring, KERN_INVALID_ARGUMENT);
    return false;
  }
  uint32_t frames = count / ring->channels;
  uint32_t write = atomic_load_explicit(&ring->write_cursor, memory_order_relaxed);
  uint32_t read = atomic_load_explicit(&ring->read_cursor, memory_order_acquire);
  uint32_t used = ar_capture_distance(ring, write, read);
  if (frames > ring->capacity_frames - used) {
    // 只有生产者更新统计, 因此饱和计数无需 CAS 重试循环.
    uint64_t dropped = atomic_load_explicit(&ring->dropped_samples, memory_order_relaxed);
    atomic_store_explicit(&ring->dropped_samples, UINT64_MAX - dropped < count ? UINT64_MAX : dropped + count, memory_order_relaxed);
    return false;
  }
  uint32_t offset = write % ring->capacity_frames;
  uint32_t first = ring->capacity_frames - offset;
  if (first > frames) first = frames;
  memcpy(ring->data + (size_t)offset * ring->channels, samples, (size_t)first * ring->channels * sizeof(float));
  memcpy(ring->data, samples + (size_t)first * ring->channels, (size_t)(frames - first) * ring->channels * sizeof(float));
  uint32_t high_water = (used + frames) * ring->channels;
  if (high_water > atomic_load_explicit(&ring->high_water_samples, memory_order_relaxed)) {
    atomic_store_explicit(&ring->high_water_samples, high_water, memory_order_relaxed);
  }
  atomic_store_explicit(&ring->write_cursor, (write + frames) % (ring->capacity_frames * 2), memory_order_release);
  ar_capture_notify(ring);
  return true;
}

// 0=完整一帧, 1=暂缺, -1=关闭或故障. 只允许一个读取线程调用.
static int ar_capture_ring_try_read(ARCaptureRing *ring, float *out, uint32_t count) {
  if (out == NULL || count != ring->frame_samples) return -1;
  if (atomic_load_explicit(&ring->closed, memory_order_acquire)) return -1;
  uint32_t read = atomic_load_explicit(&ring->read_cursor, memory_order_relaxed);
  uint32_t write = atomic_load_explicit(&ring->write_cursor, memory_order_acquire);
  uint32_t frames = count / ring->channels;
  if (ar_capture_distance(ring, write, read) < frames) return 1;
  uint32_t offset = read % ring->capacity_frames;
  uint32_t first = ring->capacity_frames - offset;
  if (first > frames) first = frames;
  memcpy(out, ring->data + (size_t)offset * ring->channels, (size_t)first * ring->channels * sizeof(float));
  memcpy(out + (size_t)first * ring->channels, ring->data, (size_t)(frames - first) * ring->channels * sizeof(float));
  atomic_store_explicit(&ring->read_cursor, (read + frames) % (ring->capacity_frames * 2), memory_order_release);
  return atomic_load_explicit(&ring->closed, memory_order_acquire) ? -1 : 0;
}

static uint64_t ar_capture_monotonic_ns(void) {
  struct timespec time;
  clock_gettime(CLOCK_MONOTONIC, &time);
  return (uint64_t)time.tv_sec * 1000000000 + (uint64_t)time.tv_nsec;
}

static int ar_capture_ring_read(ARCaptureRing *ring, float *out, uint32_t count, uint32_t timeout_ms) {
  uint64_t deadline = ar_capture_monotonic_ns() + (uint64_t)timeout_ms * 1000000;
  for (;;) {
    int result = ar_capture_ring_try_read(ring, out, count);
    if (result != 1) return result;
    uint64_t now = ar_capture_monotonic_ns();
    if (now >= deadline) return 1;
    uint64_t remaining = deadline - now;
    mach_timespec_t duration = {(unsigned int)(remaining / 1000000000), (clock_res_t)(remaining % 1000000000)};
    int status = semaphore_timedwait(ring->ready, duration);
    if (status == KERN_SUCCESS) {
      // 先消费通知再清标志, 然后重查数据. 清标志之前到达的数据也不会丢失唤醒.
      atomic_exchange_explicit(&ring->notified, false, memory_order_acq_rel);
    } else if (status != KERN_OPERATION_TIMED_OUT && status != KERN_ABORTED) {
      ar_capture_ring_fail(ring, status);
      return -1;
    }
  }
}
#endif
