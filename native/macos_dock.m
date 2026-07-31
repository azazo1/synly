#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#include <stdbool.h>

// 控制 macOS 应用在 Dock 中的可见性.
// 窗口显示时使用 Regular(显示 Dock 图标), 窗口隐藏到托盘后切换为
// Accessory(隐藏 Dock 图标), 避免常驻托盘应用关闭窗口后残留图标.
// 必须在主线程调用.
void synly_dock_set_visible(bool visible) {
  NSApplication *app = [NSApplication sharedApplication];
  if (visible) {
    [app setActivationPolicy:NSApplicationActivationPolicyRegular];
    [app activateIgnoringOtherApps:YES];
  } else {
    [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
  }
}
