#ifndef SYNLY_MACOS_AUDIO_CHANGES_H
#define SYNLY_MACOS_AUDIO_CHANGES_H
#import <CoreAudio/CoreAudio.h>
#import <Foundation/Foundation.h>
#include <assert.h>
#include <stdatomic.h>

// 双向音频共用的通知状态. 只发布原因位, 不读取设备属性, 不接触 ring 或执行重建.
// HAL/dispatch 持有的 block 只强引用独立信号对象, 不引用捕获/播放对象或其 C 上下文.
typedef struct {
  _Atomic uint32_t reasons;
} ARAudioChangeState;

enum {
  AR_AUDIO_CHANGE_ROUTE = 1u << 0,
  AR_AUDIO_CHANGE_STREAMS = 1u << 1,
  AR_AUDIO_CHANGE_FORMAT = 1u << 2,
  AR_AUDIO_CHANGE_BUFFER = 1u << 3,
  AR_AUDIO_CHANGE_ALIVE = 1u << 4,
};

static uint32_t ar_audio_change_reasons(const ARAudioChangeState *state) {
  return state == NULL ? 0 : atomic_load_explicit(&state->reasons, memory_order_acquire);
}

static void ar_audio_change_notify(ARAudioChangeState *state, UInt32 count,
                                  const AudioObjectPropertyAddress *addresses) {
  uint32_t reasons = 0;
  for (UInt32 index = 0; index < count; index++) {
    switch (addresses[index].mSelector) {
      case kAudioHardwarePropertyDefaultOutputDevice: reasons |= AR_AUDIO_CHANGE_ROUTE; break;
      case kAudioDevicePropertyStreams: reasons |= AR_AUDIO_CHANGE_STREAMS; break;
      case kAudioStreamPropertyVirtualFormat: reasons |= AR_AUDIO_CHANGE_FORMAT; break;
      case kAudioDevicePropertyBufferFrameSize: reasons |= AR_AUDIO_CHANGE_BUFFER; break;
      case kAudioDevicePropertyDeviceIsAlive: reasons |= AR_AUDIO_CHANGE_ALIVE; break;
      default: break;
    }
  }
  if (reasons != 0) atomic_fetch_or_explicit(&state->reasons, reasons, memory_order_release);
}

@interface ARAudioChangeSignal : NSObject {
@public
  ARAudioChangeState state;
}
@end
@implementation ARAudioChangeSignal
- (instancetype)init {
  self = [super init];
  if (self != nil) {
    atomic_init(&state.reasons, 0);
    if (!atomic_is_lock_free(&state.reasons)) return nil;
  }
  return self;
}
@end

typedef struct {
  AudioObjectID object;
  AudioObjectPropertyAddress address;
} ARAudioPropertyWatch;

@interface ARAudioChanges : NSObject {
@public
  ARAudioChangeSignal *signal;
  AudioObjectPropertyListenerBlock listener;
  ARAudioPropertyWatch watches[5];
  UInt32 count;
}
- (OSStatus)addObject:(AudioObjectID)object selector:(AudioObjectPropertySelector)selector scope:(AudioObjectPropertyScope)scope;
- (OSStatus)removeAll;
@end

@implementation ARAudioChanges
- (instancetype)init {
  self = [super init];
  if (self == nil) return nil;
  signal = [[ARAudioChangeSignal alloc] init];
  if (signal == nil) return nil;
  ARAudioChangeSignal *target = signal;
  listener = [^(UInt32 addressCount, const AudioObjectPropertyAddress *addresses) {
    ar_audio_change_notify(&target->state, addressCount, addresses);
  } copy];
  return self;
}
- (OSStatus)addObject:(AudioObjectID)object selector:(AudioObjectPropertySelector)selector scope:(AudioObjectPropertyScope)scope {
  assert(count < sizeof(watches) / sizeof(watches[0]));
  ARAudioPropertyWatch watch = {
    .object = object,
    .address = {.mSelector = selector, .mScope = scope, .mElement = kAudioObjectPropertyElementMain},
  };
  // NULL queue 让 HAL 直接调用. 不假定 Remove 成功会等待所有在途 block 执行完毕.
  OSStatus status = AudioObjectAddPropertyListenerBlock(object, &watch.address, NULL, listener);
  if (status == noErr) watches[count++] = watch;
  return status;
}
- (OSStatus)removeAll {
  while (count > 0) {
    ARAudioPropertyWatch *watch = &watches[count - 1];
    OSStatus status = AudioObjectRemovePropertyListenerBlock(watch->object, &watch->address, NULL, listener);
    if (status != noErr) return status;
    count--;
  }
  return noErr;
}
- (void)dealloc {
  // 未成功注销的 owner 必须进入对应方向的隔离链, 不能默默丢弃注册记录.
  assert(count == 0);
}
@end
#endif
