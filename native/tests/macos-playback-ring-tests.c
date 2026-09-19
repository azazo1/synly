// 无设备的播放背压和实时消费测试, 使用真实 Mach 通知和 pthread 并发.
#include <mach/mach.h>
#include <mach/semaphore.h>
#include <assert.h>
#include <stdatomic.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>

static _Atomic bool waiting;
static _Atomic unsigned signals;
static _Thread_local bool callback_active;
static kern_return_t checked_wait(semaphore_t semaphore, mach_timespec_t duration) {
  assert(!callback_active);
  atomic_store(&waiting, true);
  return semaphore_timedwait(semaphore, duration);
}
static kern_return_t checked_signal(semaphore_t semaphore) {
  atomic_fetch_add(&signals, 1);
  return semaphore_signal(semaphore);
}
#define semaphore_timedwait checked_wait
#define semaphore_signal checked_signal
#include "../macos_playback_ring.h"
#undef semaphore_timedwait
#undef semaphore_signal

static void fill(ARPlaybackRing *ring, float *out, uint32_t count) {
  callback_active = true;
  assert(ar_playback_ring_fill(ring, out, count) == 0);
  callback_active = false;
}
static void test_budget_and_timeouts(void) {
  ARPlaybackRing ring;
  assert(ar_playback_ring_init(&ring, 0, 2, 240) != 0);
  assert(ar_playback_ring_init(&ring, 48000, 0, 240) != 0);
  assert(ar_playback_ring_init(&ring, 48000, 2, UINT32_MAX) != 0);
  assert(ar_playback_ring_init(&ring, 48000, 2, 240) == 0);
  assert(ring.watermark_frames == 2400 && ring.samples.capacity_frames == 2640);
  float frame[5760] = {0};
  for (unsigned i = 0; i < 11; i++) assert(ar_playback_ring_submit(&ring, frame, 480, 0) == 0);
  assert(ar_audio_ring_producer_used(&ring.samples) == 2640);
  assert(ar_playback_ring_submit(&ring, frame, 479, 0) == -1);
  uint64_t start = ar_audio_monotonic_ns();
  assert(ar_playback_ring_submit(&ring, frame, 480, 8) == 1);
  assert(ar_audio_monotonic_ns() - start >= 8000000);
  assert(ar_audio_ring_producer_used(&ring.samples) == 2640);
  start = ar_audio_monotonic_ns();
  assert(ar_playback_ring_submit(&ring, frame, 480, 5000) == 1);
  uint64_t elapsed = ar_audio_monotonic_ns() - start;
  assert(elapsed >= 100000000 && elapsed < 2000000000ULL);
  fill(&ring, frame, 480);
  assert(ar_playback_ring_submit(&ring, frame, 480, 0) == 0);
  assert(atomic_load(&ring.samples.high_water_samples) == 5280);
  ar_audio_ring_free(&ring.samples);

  assert(ar_playback_ring_init(&ring, 48000, 2, 2880) == 0);
  assert(ring.samples.capacity_frames == 5280);
  assert(ar_playback_ring_submit(&ring, frame, 5760, 0) == 0);
  assert(ar_playback_ring_submit(&ring, frame, 5760, 0) == 1);
  fill(&ring, frame, 960);
  assert(ar_audio_ring_producer_used(&ring.samples) == 2400);
  assert(ar_playback_ring_submit(&ring, frame, 5760, 0) == 0);
  assert(ar_audio_ring_producer_used(&ring.samples) == 5280);
  ar_audio_ring_free(&ring.samples);
}

static void test_zero_fill_and_wrap(void) {
  ARPlaybackRing ring;
  assert(ar_playback_ring_init(&ring, 8000, 2, 40) == 0);
  float frame[80], out[160];
  for (unsigned i = 0; i < 80; i++) frame[i] = (float)(i + 1);
  atomic_store(&ring.samples.read_cursor, ring.samples.capacity_frames * 2 - 7);
  atomic_store(&ring.samples.write_cursor, ring.samples.capacity_frames * 2 - 7);
  atomic_store(&signals, 0);
  assert(ar_playback_ring_submit(&ring, frame, 80, 0) == 0);
  assert(atomic_load(&signals) == 0);
  fill(&ring, out, 160);
  for (unsigned i = 0; i < 160; i++) assert(out[i] == (i < 80 ? frame[i] : 0));
  assert(ar_audio_ring_producer_used(&ring.samples) == 0);
  for (unsigned i = 0; i < 10000; i++) {
    assert(ar_playback_ring_submit(&ring, frame, 80, 0) == 0);
    fill(&ring, out, 80);
    assert(memcmp(out, frame, sizeof(frame)) == 0);
  }
  assert(atomic_load(&signals) == 1);
  fill(&ring, out, 160);
  for (unsigned i = 0; i < 160; i++) assert(out[i] == 0);
  assert(atomic_load(&signals) == 1);
  ar_audio_ring_close(&ring.samples);
  assert(ar_playback_ring_submit(&ring, frame, 80, 0) == -1);
  assert(ar_playback_ring_fill(&ring, out, 80) == -1);
  ar_audio_ring_free(&ring.samples);
}

typedef struct { ARPlaybackRing *ring; int result; } Waiter;
static void *submit_waiter(void *context) {
  Waiter *waiter = context;
  float frame[80] = {0};
  waiter->result = ar_playback_ring_submit(waiter->ring, frame, 80, 100);
  return NULL;
}
static void test_wake_blocked_submit(void) {
  for (unsigned mode = 0; mode < 3; mode++) {
    ARPlaybackRing ring;
    assert(ar_playback_ring_init(&ring, 8000, 2, 40) == 0);
    float frame[80] = {0};
    for (unsigned i = 0; i < 11; i++) assert(ar_playback_ring_submit(&ring, frame, 80, 0) == 0);
    atomic_store(&waiting, false);
    Waiter waiter = {&ring, 99};
    pthread_t thread;
    assert(pthread_create(&thread, NULL, submit_waiter, &waiter) == 0);
    uint64_t deadline = ar_audio_monotonic_ns() + 1000000000ULL;
    while (!atomic_load(&waiting)) {
      assert(ar_audio_monotonic_ns() < deadline);
      sched_yield();
    }
    if (mode == 0) fill(&ring, frame, 80);
    else if (mode == 1) ar_audio_ring_close(&ring.samples);
    else {
      ar_audio_ring_fail(&ring.samples, -3456);
      ar_audio_ring_fail(&ring.samples, -4567);
    }
    assert(pthread_join(thread, NULL) == 0);
    assert(waiter.result == (mode == 0 ? 0 : -1));
    if (mode == 2) assert(atomic_load(&ring.samples.failure) == -3456);
    ar_audio_ring_free(&ring.samples);
  }
}

typedef struct { ARPlaybackRing ring; _Atomic bool done; } Stress;
static void *produce(void *context) {
  Stress *stress = context;
  float frame[80];
  for (unsigned block = 0; block < 10000; block++) {
    for (unsigned i = 0; i < 40; i++) {
      frame[i * 2] = (float)(block * 40 + i + 1);
      frame[i * 2 + 1] = -frame[i * 2];
    }
    assert(ar_playback_ring_submit(&stress->ring, frame, 80, 100) == 0);
    if (block % 7 == 0) sched_yield();
  }
  atomic_store(&stress->done, true);
  return NULL;
}
static void test_concurrent_order(void) {
  Stress stress = {0};
  assert(ar_playback_ring_init(&stress.ring, 8000, 2, 40) == 0);
  pthread_t thread;
  assert(pthread_create(&thread, NULL, produce, &stress) == 0);
  unsigned received = 0;
  uint64_t deadline = ar_audio_monotonic_ns() + 15000000000ULL;
  while (received < 400000) {
    float out[34];
    fill(&stress.ring, out, 34);
    for (unsigned i = 0; i < 17; i++) {
      if (out[i * 2] == 0) {
        assert(out[i * 2 + 1] == 0);
      } else {
        received++;
        assert(out[i * 2] == (float)received && out[i * 2 + 1] == -(float)received);
      }
    }
    assert(ar_audio_monotonic_ns() < deadline);
    if (received % 97 == 0) sched_yield();
  }
  assert(pthread_join(thread, NULL) == 0);
  assert(atomic_load(&stress.done));
  assert(ar_audio_ring_producer_used(&stress.ring.samples) == 0);
  assert(atomic_load(&stress.ring.samples.dropped_samples) == 0);
  assert(atomic_load(&stress.ring.samples.high_water_samples) <= 880);
  ar_audio_ring_free(&stress.ring.samples);
}
int main(void) {
  puts("[1/4] 播放 50 ms 水位, 60 ms 整帧和超时上限");
  test_budget_and_timeouts();
  puts("[2/4] 部分消费补零, 游标回绕和通知合并");
  test_zero_fill_and_wrap();
  puts("[3/4] 消费, 关闭和故障唤醒阻塞提交");
  test_wake_blocked_submit();
  puts("[4/4] 40 万帧双线程顺序及背压验证");
  test_concurrent_order();
  puts("macOS 播放 SPSC 测试通过");
  return 0;
}
