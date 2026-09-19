// 不打开设备, 使用注入时间验证健康窗口, 使用真实线程验证心跳/超时竞争.
#include <assert.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <time.h>
#include "../macos_capture_health.h"

static uint64_t real_now(void) {
  struct timespec time;
  assert(clock_gettime(CLOCK_MONOTONIC, &time) == 0);
  return (uint64_t)time.tv_sec * 1000000000 + (uint64_t)time.tv_nsec;
}
static void test_deadlines(void) {
  ARCaptureHealth health;
  uint64_t start = 1000000000;
  assert(ar_capture_health_init(&health, start));
  assert(!ar_capture_health_expired(&health, start - 1));
  assert(!ar_capture_health_expired(&health, start + AR_CAPTURE_STALL_NS - 1));
  assert(ar_capture_health_wait_ms(&health, start, UINT32_MAX) == 5000);
  assert(ar_capture_health_wait_ms(&health, start, 200) == 200);
  assert(ar_capture_health_wait_ms(&health, start, 0) == 0);
  assert(ar_capture_health_wait_ms(&health, start + AR_CAPTURE_STALL_NS - 1, 200) == 1);
  assert(ar_capture_health_wait_ms(&health, start + AR_CAPTURE_STALL_NS, 200) == 0);
  assert(ar_capture_health_expired(&health, start + AR_CAPTURE_STALL_NS));
  assert(!ar_capture_health_pulse(&health, start + AR_CAPTURE_STALL_NS + 1));
  assert(ar_capture_health_expired(&health, start));
  assert(ar_capture_health_wait_ms(&health, start, UINT32_MAX) == 0);

  assert(ar_capture_health_init(&health, start));
  // 心跳先到达, 即使旧窗口刚到期, 后续检测必须使用新窗口而不是错误关闭.
  assert(ar_capture_health_pulse(&health, start + AR_CAPTURE_STALL_NS));
  assert(!ar_capture_health_expired(&health, start + AR_CAPTURE_STALL_NS));
  assert(!ar_capture_health_expired(&health, start + AR_CAPTURE_STALL_NS * 2 - 1));
  assert(ar_capture_health_expired(&health, start + AR_CAPTURE_STALL_NS * 2));
  assert(!ar_capture_health_init(&health, AR_CAPTURE_EXPIRED));
}
static void test_silence_and_clock_edges(void) {
  ARCaptureHealth health;
  assert(ar_capture_health_init(&health, 0));
  for (uint64_t second = 1; second <= 3600; second++) {
    uint64_t time = second * 1000000000;
    // 有回调但输出静音/零 PCM 时仍发送心跳, 不是只在写入 ring 时更新.
    assert(ar_capture_health_pulse(&health, time));
    assert(!ar_capture_health_expired(&health, time + 4999999999));
  }
  uint64_t last = atomic_load(&health.last_callback_ns);
  assert(ar_capture_health_pulse(&health, last - 100));
  assert(atomic_load(&health.last_callback_ns) == last);
  assert(ar_capture_health_wait_ms(&health, last - 1, UINT32_MAX) == 5000);
  assert(ar_capture_health_init(&health, AR_CAPTURE_EXPIRED - 2));
  assert(!ar_capture_health_pulse(&health, AR_CAPTURE_EXPIRED));
  assert(!ar_capture_health_expired(&health, 0));
}

typedef struct {
  ARCaptureHealth health;
  _Atomic unsigned epoch;
  _Atomic unsigned done;
  bool pulsed;
} Race;
#define ROUNDS 20000
static void *pulse_thread(void *context) {
  Race *race = context;
  uint64_t deadline = real_now() + 10000000000ULL;
  for (unsigned index = 1; index <= ROUNDS; index++) {
    while (atomic_load_explicit(&race->epoch, memory_order_acquire) != index) {
      assert(real_now() < deadline);
      sched_yield();
    }
    race->pulsed = ar_capture_health_pulse(&race->health, AR_CAPTURE_STALL_NS * 2);
    atomic_store_explicit(&race->done, index, memory_order_release);
  }
  return NULL;
}
static void test_expiry_races_pulse(void) {
  Race race = {0};
  pthread_t thread;
  assert(pthread_create(&thread, NULL, pulse_thread, &race) == 0);
  uint64_t deadline = real_now() + 10000000000ULL;
  for (unsigned index = 1; index <= ROUNDS; index++) {
    assert(ar_capture_health_init(&race.health, 0));
    atomic_store_explicit(&race.epoch, index, memory_order_release);
    if (index % 2 == 0) sched_yield();
    bool expired = ar_capture_health_expired(&race.health, AR_CAPTURE_STALL_NS * 2);
    while (atomic_load_explicit(&race.done, memory_order_acquire) != index) {
      assert(real_now() < deadline);
      sched_yield();
    }
    // 必须恰有一方成功. 超时胜出后回调不能复活, 心跳胜出后检测不能误报.
    assert(expired != race.pulsed);
    assert(atomic_load(&race.health.last_callback_ns) == (expired ? AR_CAPTURE_EXPIRED : AR_CAPTURE_STALL_NS * 2));
  }
  assert(pthread_join(thread, NULL) == 0);
}
int main(void) {
  puts("[1/3] 无首回调, 5 秒边界和等待预算");
  test_deadlines();
  puts("[2/3] 持续静音心跳与时钟边界");
  test_silence_and_clock_edges();
  puts("[3/3] 2 万次真实线程心跳/超时竞争");
  test_expiry_races_pulse();
  puts("macOS 捕获停滞检测测试通过");
  return 0;
}
