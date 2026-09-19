#ifndef SYNLY_MACOS_AUDIO_RING_H
#define SYNLY_MACOS_AUDIO_RING_H

#include <mach/mach.h>
#include <mach/semaphore.h>
#include <mach/sync_policy.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

// 单生产者拥有 write_cursor, 单消费者拥有 read_cursor. 两侧不得覆盖对方仍持有的样本.
// 游标以完整声道帧为单位在 [0, 2 * capacity) 循环, 不要求容量为二的幂.
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
} ARAudioRing;

static bool ar_audio_ring_format_valid(uint32_t rate, uint32_t channels, uint32_t frame_size) {
  return rate >= 8000 && rate <= 48000 && channels > 0 && channels <= 8 && frame_size > 0 &&
      (uint64_t)frame_size * 1000 <= (uint64_t)rate * 60;
}

static uint32_t ar_audio_ring_distance(const ARAudioRing *ring, uint32_t write, uint32_t read) {
  return write >= read ? write - read : write + ring->capacity_frames * 2 - read;
}

static int ar_audio_ring_init(ARAudioRing *ring, uint64_t capacity, uint32_t channels, uint32_t frame_size) {
  memset(ring, 0, sizeof(*ring));
  if (channels == 0 || channels > 8 || frame_size == 0 || capacity < frame_size ||
      capacity > UINT32_MAX / sizeof(float) / channels) return KERN_INVALID_ARGUMENT;
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

static void ar_audio_ring_notify(ARAudioRing *ring) {
  // 一个方向只允许一个等待者. 捕获通知数据到达, 播放通知空间释放.
  if (!atomic_exchange_explicit(&ring->notified, true, memory_order_acq_rel)) {
    int status = semaphore_signal(ring->ready);
    if (status != KERN_SUCCESS) {
      int expected = 0;
      atomic_compare_exchange_strong(&ring->failure, &expected, status);
      atomic_store_explicit(&ring->closed, true, memory_order_release);
    }
  }
}

static void ar_audio_ring_close(ARAudioRing *ring) {
  if (!ring->initialized) return;
  atomic_store_explicit(&ring->closed, true, memory_order_release);
  ar_audio_ring_notify(ring);
}

static void ar_audio_ring_fail(ARAudioRing *ring, int failure) {
  int expected = 0;
  atomic_compare_exchange_strong(&ring->failure, &expected, failure);
  ar_audio_ring_close(ring);
}

// 必须先停止生产者和消费者. 允许清理尚未完成初始化的实例.
static void ar_audio_ring_free(ARAudioRing *ring) {
  if (!ring->initialized) return;
  semaphore_destroy(mach_task_self(), ring->ready);
  free(ring->data);
  memset(ring, 0, sizeof(*ring));
}

// 仅生产者查询, 避免两个无归属快照跨多轮读写产生虚假的占用量.
static uint32_t ar_audio_ring_producer_used(ARAudioRing *ring) {
  uint32_t write = atomic_load_explicit(&ring->write_cursor, memory_order_relaxed);
  uint32_t read = atomic_load_explicit(&ring->read_cursor, memory_order_acquire);
  return ar_audio_ring_distance(ring, write, read);
}

// 0=成功, 1=空间不足, -1=参数错误或已关闭. 此层不通知, 由方向策略选择唤醒点.
static int ar_audio_ring_push(ARAudioRing *ring, const float *samples, uint32_t count) {
  if (atomic_load_explicit(&ring->closed, memory_order_acquire)) return -1;
  if (samples == NULL || count == 0 || count % ring->channels != 0) return -1;
  uint32_t frames = count / ring->channels;
  uint32_t write = atomic_load_explicit(&ring->write_cursor, memory_order_relaxed);
  uint32_t read = atomic_load_explicit(&ring->read_cursor, memory_order_acquire);
  uint32_t used = ar_audio_ring_distance(ring, write, read);
  if (frames > ring->capacity_frames - used) return 1;
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
  return atomic_load_explicit(&ring->closed, memory_order_acquire) ? -1 : 0;
}

// partial=false 时不足整帧不消费, partial=true 时允许读取现有部分帧.
static int ar_audio_ring_take(ARAudioRing *ring, float *out, uint32_t count, bool partial, uint32_t *copied) {
  *copied = 0;
  if (out == NULL || count == 0 || count % ring->channels != 0 || count / ring->channels > ring->capacity_frames) return -1;
  if (atomic_load_explicit(&ring->closed, memory_order_acquire)) return -1;
  uint32_t read = atomic_load_explicit(&ring->read_cursor, memory_order_relaxed);
  uint32_t write = atomic_load_explicit(&ring->write_cursor, memory_order_acquire);
  uint32_t frames = count / ring->channels;
  uint32_t available = ar_audio_ring_distance(ring, write, read);
  if (available < frames) {
    if (!partial) return 1;
    frames = available;
  }
  if (frames != 0) {
    uint32_t offset = read % ring->capacity_frames;
    uint32_t first = ring->capacity_frames - offset;
    if (first > frames) first = frames;
    memcpy(out, ring->data + (size_t)offset * ring->channels, (size_t)first * ring->channels * sizeof(float));
    memcpy(out + (size_t)first * ring->channels, ring->data, (size_t)(frames - first) * ring->channels * sizeof(float));
    atomic_store_explicit(&ring->read_cursor, (read + frames) % (ring->capacity_frames * 2), memory_order_release);
  }
  *copied = frames * ring->channels;
  return atomic_load_explicit(&ring->closed, memory_order_acquire) ? -1 : 0;
}

static uint64_t ar_audio_monotonic_ns(void) {
  struct timespec time;
  clock_gettime(CLOCK_MONOTONIC, &time);
  return (uint64_t)time.tv_sec * 1000000000 + (uint64_t)time.tv_nsec;
}

// 0=可以重查状态, 1=已到截止时间, -1=系统等待失败. 只从阻塞工作线程调用.
static int ar_audio_ring_wait(ARAudioRing *ring, uint64_t deadline) {
  uint64_t now = ar_audio_monotonic_ns();
  if (now >= deadline) return 1;
  uint64_t remaining = deadline - now;
  mach_timespec_t duration = {(unsigned int)(remaining / 1000000000), (clock_res_t)(remaining % 1000000000)};
  int status = semaphore_timedwait(ring->ready, duration);
  if (status == KERN_SUCCESS) {
    // acquire 同步通知合并期间生产者/消费者发布的最新游标, 清标志后必须重查条件.
    atomic_exchange_explicit(&ring->notified, false, memory_order_acq_rel);
  } else if (status != KERN_OPERATION_TIMED_OUT && status != KERN_ABORTED) {
    ar_audio_ring_fail(ring, status);
    return -1;
  }
  return 0;
}
#endif
