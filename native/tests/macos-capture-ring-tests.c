// 无音频设备的真实 SPSC 和 Mach 通知回归.
#include <mach/mach.h>
#include <mach/semaphore.h>
#include <assert.h>
#include <stdatomic.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>

static int fail_allocation;
static int fail_semaphore;
static int allocations;
static int semaphores;
static _Atomic unsigned signals;
static _Atomic bool waiting;

static void *checked_calloc(size_t count, size_t size) {
  if (fail_allocation) return NULL;
  void *value = calloc(count, size);
  if (value != NULL) allocations++;
  return value;
}
static void checked_free(void *value) {
  if (value != NULL) allocations--;
  free(value);
}
static kern_return_t checked_create(task_t task, semaphore_t *semaphore, int policy, int count) {
  if (fail_semaphore) return KERN_RESOURCE_SHORTAGE;
  kern_return_t status = semaphore_create(task, semaphore, policy, count);
  if (status == KERN_SUCCESS) semaphores++;
  return status;
}
static kern_return_t checked_destroy(task_t task, semaphore_t semaphore) {
  kern_return_t status = semaphore_destroy(task, semaphore);
  assert(status == KERN_SUCCESS);
  semaphores--;
  return status;
}
static kern_return_t checked_signal(semaphore_t semaphore) {
  atomic_fetch_add(&signals, 1);
  return semaphore_signal(semaphore);
}
static kern_return_t checked_wait(semaphore_t semaphore, mach_timespec_t duration) {
  atomic_store(&waiting, true);
  return semaphore_timedwait(semaphore, duration);
}
#define calloc checked_calloc
#define free checked_free
#define semaphore_create checked_create
#define semaphore_destroy checked_destroy
#define semaphore_signal checked_signal
#define semaphore_timedwait checked_wait
#include "../macos_capture_ring.h"
#undef calloc
#undef free
#undef semaphore_create
#undef semaphore_destroy
#undef semaphore_signal
#undef semaphore_timedwait

static void test_init_and_overflow(void) {
  ARAudioRing ring = {0};
  ar_audio_ring_close(&ring);
  ar_audio_ring_free(&ring);
  assert(ar_capture_ring_init(&ring, 0, 2, 240, 512) != 0);
  assert(ar_capture_ring_init(&ring, 48000, 0, 240, 512) != 0);
  assert(ar_capture_ring_init(&ring, 48000, 2, 240, UINT32_MAX) != 0);
  fail_allocation = 1;
  assert(ar_capture_ring_init(&ring, 48000, 2, 240, 512) != 0);
  fail_allocation = 0;
  fail_semaphore = 1;
  assert(ar_capture_ring_init(&ring, 48000, 2, 240, 512) != 0);
  fail_semaphore = 0;
  ar_audio_ring_free(&ring);
  assert(allocations == 0 && semaphores == 0);
  assert(ar_capture_ring_init(&ring, 8000, 2, 1, 1) == 0);
  assert(ring.capacity_frames == 240);
  float block[480];
  for (unsigned i = 0; i < 480; i++) block[i] = (float)i;
  assert(ar_capture_ring_write(&ring, block, 480));
  float extra[] = {-1, -2};
  assert(!ar_capture_ring_write(&ring, extra, 2));
  assert(atomic_load(&ring.dropped_samples) == 2);
  assert(atomic_load(&ring.high_water_samples) == 480);
  atomic_store(&ring.dropped_samples, UINT64_MAX - 1);
  assert(!ar_capture_ring_write(&ring, extra, 2));
  assert(atomic_load(&ring.dropped_samples) == UINT64_MAX);
  for (unsigned i = 0; i < 240; i++) {
    float out[2];
    assert(ar_capture_ring_read(&ring, out, 2, 0) == 0);
    assert(out[0] == (float)(i * 2) && out[1] == (float)(i * 2 + 1));
  }
  assert(ar_capture_ring_write(&ring, extra, 2));
  float out[2];
  assert(ar_capture_ring_read(&ring, out, 2, 0) == 0);
  assert(out[0] == -1 && out[1] == -2);
  // 容量不为二的幂, 两个游标跨 2*capacity 边界后仍保持帧顺序.
  atomic_store(&ring.read_cursor, ring.capacity_frames * 2 - 1);
  atomic_store(&ring.write_cursor, ring.capacity_frames * 2 - 1);
  assert(ar_capture_ring_write(&ring, block, 8));
  for (unsigned i = 0; i < 4; i++) {
    assert(ar_capture_ring_read(&ring, out, 2, 0) == 0);
    assert(out[0] == (float)(i * 2) && out[1] == (float)(i * 2 + 1));
  }
  ar_audio_ring_fail(&ring, -1234);
  ar_audio_ring_fail(&ring, -2345);
  assert(atomic_load(&ring.failure) == -1234);
  assert(ar_capture_ring_read(&ring, out, 2, 0) == -1);
  ar_audio_ring_free(&ring);
  ar_audio_ring_free(&ring);
}

static void test_long_frame_and_notifications(void) {
  ARAudioRing ring;
  assert(ar_capture_ring_init(&ring, 48000, 2, 2880, 512) == 0);
  assert(ring.capacity_frames == 3392);
  float chunk[1024], out[5760];
  for (unsigned packet = 0; packet < 6; packet++) {
    for (unsigned i = 0; i < 1024; i++) chunk[i] = (float)(packet * 1024 + i);
    assert(ar_capture_ring_write(&ring, chunk, 1024));
    if (packet < 5) assert(ar_capture_ring_read(&ring, out, 5760, 0) == 1);
  }
  assert(ar_capture_ring_read(&ring, out, 5760, 0) == 0);
  for (unsigned i = 0; i < 5760; i++) assert(out[i] == (float)i);
  assert(ar_audio_ring_distance(&ring, atomic_load(&ring.write_cursor), atomic_load(&ring.read_cursor)) == 192);
  assert(atomic_load(&ring.dropped_samples) == 0);
  ar_audio_ring_free(&ring);

  assert(ar_capture_ring_init(&ring, 8000, 2, 1, 1) == 0);
  atomic_store(&signals, 0);
  float frame[2] = {1, 2};
  for (unsigned i = 0; i < 10000; i++) {
    assert(ar_capture_ring_write(&ring, frame, 2));
    assert(ar_capture_ring_read(&ring, frame, 2, 0) == 0);
  }
  assert(atomic_load(&signals) == 1);
  uint64_t start = ar_audio_monotonic_ns();
  assert(ar_capture_ring_read(&ring, frame, 2, 20) == 1);
  assert(ar_audio_monotonic_ns() - start >= 20000000);
  assert(!atomic_load(&ring.notified));
  assert(ar_capture_ring_write(&ring, frame, 2));
  assert(atomic_load(&signals) == 2);
  ar_audio_ring_close(&ring);
  assert(ar_capture_ring_read(&ring, frame, 2, 0) == -1);
  assert(!ar_capture_ring_write(&ring, frame, 2));
  ar_audio_ring_free(&ring);
}

typedef struct { ARAudioRing *ring; int result; } Reader;
static void *waiter(void *context) {
  Reader *reader = context;
  float out[2];
  reader->result = ar_capture_ring_read(reader->ring, out, 2, 2000);
  if (reader->result == 0) assert(out[0] == 1 && out[1] == 2);
  return NULL;
}
static void test_wakeup(void) {
  for (int mode = 0; mode < 3; mode++) {
    ARAudioRing ring;
    assert(ar_capture_ring_init(&ring, 8000, 2, 1, 1) == 0);
    atomic_store(&waiting, false);
    Reader reader = {&ring, 99};
    pthread_t thread;
    assert(pthread_create(&thread, NULL, waiter, &reader) == 0);
    while (!atomic_load(&waiting)) sched_yield();
    if (mode == 0) {
      float frame[] = {1, 2};
      assert(ar_capture_ring_write(&ring, frame, 2));
    } else if (mode == 1) ar_audio_ring_close(&ring);
    else ar_audio_ring_fail(&ring, -3456);
    assert(pthread_join(thread, NULL) == 0);
    assert(reader.result == (mode == 0 ? 0 : -1));
    ar_audio_ring_free(&ring);
  }
}

typedef struct {
  ARAudioRing ring;
  _Atomic bool done;
  uint32_t accepted;
  uint32_t consumed;
} Stress;
static void *produce(void *context) {
  Stress *stress = context;
  float samples[34];
  for (unsigned packet = 0; packet < 20000; packet++) {
    for (unsigned i = 0; i < 17; i++) {
      samples[i * 2] = (float)(packet * 17 + i + 1);
      samples[i * 2 + 1] = -samples[i * 2];
    }
    if (ar_capture_ring_write(&stress->ring, samples, 34)) stress->accepted += 17;
    if (packet % 13 == 0) sched_yield();
  }
  atomic_store(&stress->done, true);
  return NULL;
}
static void test_concurrent_samples(void) {
  Stress stress = {0};
  assert(ar_capture_ring_init(&stress.ring, 8000, 2, 1, 17) == 0);
  pthread_t thread;
  assert(pthread_create(&thread, NULL, produce, &stress) == 0);
  float previous = 0;
  while (true) {
    float out[2];
    int result = ar_capture_ring_read(&stress.ring, out, 2, 10);
    assert(result >= 0);
    if (result == 0) {
      assert(out[0] > previous && out[1] == -out[0]);
      previous = out[0];
      stress.consumed++;
    } else if (atomic_load(&stress.done)) break;
  }
  assert(pthread_join(thread, NULL) == 0);
  assert(stress.consumed == stress.accepted);
  assert((uint64_t)stress.accepted * 2 + atomic_load(&stress.ring.dropped_samples) == 20000 * 34);
  assert(atomic_load(&stress.ring.high_water_samples) <= stress.ring.capacity_frames * 2);
  ar_audio_ring_free(&stress.ring);
}

typedef struct {
  ARAudioRing ring;
  _Atomic unsigned acknowledged;
} Handoff;
static void *handoff_producer(void *context) {
  Handoff *handoff = context;
  uint64_t deadline = ar_audio_monotonic_ns() + 10000000000ULL;
  for (unsigned index = 1; index <= 2000; index++) {
    while (atomic_load(&handoff->acknowledged) != index - 1) {
      assert(ar_audio_monotonic_ns() < deadline);
      sched_yield();
    }
    float frame[2] = {(float)index, -(float)index};
    assert(ar_capture_ring_write(&handoff->ring, frame, 2));
    if (index % 3 == 0) sched_yield();
  }
  return NULL;
}
static void test_repeated_handoff(void) {
  Handoff handoff = {0};
  assert(ar_capture_ring_init(&handoff.ring, 8000, 2, 1, 1) == 0);
  pthread_t thread;
  assert(pthread_create(&thread, NULL, handoff_producer, &handoff) == 0);
  for (unsigned index = 1; index <= 2000; index++) {
    float frame[2];
    assert(ar_capture_ring_read(&handoff.ring, frame, 2, 1000) == 0);
    assert(frame[0] == (float)index && frame[1] == -(float)index);
    atomic_store(&handoff.acknowledged, index);
    if (index % 5 == 0) sched_yield();
  }
  assert(pthread_join(thread, NULL) == 0);
  assert(atomic_load(&handoff.ring.dropped_samples) == 0);
  ar_audio_ring_free(&handoff.ring);
}

int main(void) {
  puts("[1/4] 捕获 SPSC 初始化回收, 溢出与游标回绕");
  test_init_and_overflow();
  puts("[2/4] 60 ms 拼帧与通知合并");
  test_long_frame_and_notifications();
  puts("[3/4] 数据, 关闭和故障唤醒");
  test_wakeup();
  puts("[4/4] 34 万帧并发传输和 2000 次交接唤醒");
  test_concurrent_samples();
  test_repeated_handoff();
  assert(allocations == 0 && semaphores == 0);
  puts("macOS 捕获 SPSC 测试通过");
  return 0;
}
