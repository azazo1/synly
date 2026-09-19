#ifndef SYNLY_MACOS_CAPTURE_HEALTH_H
#define SYNLY_MACOS_CAPTURE_HEALTH_H
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>

// Sunshine microphone.mm 的无数据等待上限为 5 秒. 此处只判定 IOProc 完全停滞,
// 不根据样本幅值或暂缺 PCM 判定离线. 时钟由调用方传入, 便于边界与竞争验证.
#define AR_CAPTURE_STALL_NS UINT64_C(5000000000)
#define AR_CAPTURE_EXPIRED UINT64_MAX

typedef struct {
  // UINT64_MAX 是终止哨兵. CAS 将回调心跳与超时判定排成同一个原子顺序.
  _Atomic uint64_t last_callback_ns;
} ARCaptureHealth;

static bool ar_capture_health_init(ARCaptureHealth *health, uint64_t now) {
  atomic_init(&health->last_callback_ns, now);
  return now != AR_CAPTURE_EXPIRED && atomic_is_lock_free(&health->last_callback_ns);
}

// 只允许一个 IOProc 调用. 不自旋或等待: 唯一竞争者是读取线程的超时终止 CAS.
static bool ar_capture_health_pulse(ARCaptureHealth *health, uint64_t now) {
  uint64_t previous = atomic_load_explicit(&health->last_callback_ns, memory_order_acquire);
  if (previous == AR_CAPTURE_EXPIRED || now == AR_CAPTURE_EXPIRED) return false;
  // 防御时钟异常, 旧时间不能缩短健康窗口.
  if (now < previous) now = previous;
  return atomic_compare_exchange_strong_explicit(&health->last_callback_ns, &previous, now,
                                                 memory_order_acq_rel, memory_order_acquire);
}

// 读取线程调用. 返回真后不会被迟到回调重新激活, 必须新建设备实例.
static bool ar_capture_health_expired(ARCaptureHealth *health, uint64_t now) {
  uint64_t previous = atomic_load_explicit(&health->last_callback_ns, memory_order_acquire);
  if (previous == AR_CAPTURE_EXPIRED) return true;
  if (now < previous || now - previous < AR_CAPTURE_STALL_NS) return false;
  return atomic_compare_exchange_strong_explicit(&health->last_callback_ns, &previous, AR_CAPTURE_EXPIRED,
                                                 memory_order_acq_rel, memory_order_acquire);
}

// 将单次阻塞读取限制在当前无回调期限内, 即使 FFI 调用方请求很长的等待也能检测.
static uint32_t ar_capture_health_wait_ms(ARCaptureHealth *health, uint64_t now, uint32_t requested_ms) {
  uint64_t previous = atomic_load_explicit(&health->last_callback_ns, memory_order_acquire);
  if (previous == AR_CAPTURE_EXPIRED) return 0;
  uint64_t elapsed = now >= previous ? now - previous : 0;
  if (elapsed >= AR_CAPTURE_STALL_NS) return 0;
  uint64_t remaining_ms = (AR_CAPTURE_STALL_NS - elapsed + 999999) / 1000000;
  return remaining_ms < requested_ms ? (uint32_t)remaining_ms : requested_ms;
}
#endif
