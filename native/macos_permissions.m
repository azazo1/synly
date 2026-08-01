#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>
#import <Foundation/Foundation.h>
#include <stdbool.h>

static void (*g_synly_accessibility_change_handler)(bool trusted) = NULL;
static bool g_synly_accessibility_trusted = false;

bool synly_permissions_is_accessibility_trusted(void) {
  return AXIsProcessTrusted();
}

void synly_permissions_request_accessibility(void) {
  NSDictionary *options = @{(__bridge id)kAXTrustedCheckOptionPrompt: @YES};
  AXIsProcessTrustedWithOptions((CFDictionaryRef)options);
  NSURL *url = [NSURL
      URLWithString:@"x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"];
  [[NSWorkspace sharedWorkspace] openURL:url];
}

void synly_permissions_set_change_handler(void (*handler)(bool trusted)) {
  g_synly_accessibility_change_handler = handler;
  g_synly_accessibility_trusted = AXIsProcessTrusted();
  NSDistributedNotificationCenter *center = [NSDistributedNotificationCenter defaultCenter];
  [center addObserverForName:@"com.apple.accessibility.api"
                      object:nil
                       queue:[NSOperationQueue mainQueue]
                  usingBlock:^(NSNotification *note) {
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(0.5 * NSEC_PER_SEC)),
                   dispatch_get_main_queue(), ^{
      bool trusted = AXIsProcessTrusted();
      if (trusted != g_synly_accessibility_trusted &&
          g_synly_accessibility_change_handler != NULL) {
        g_synly_accessibility_trusted = trusted;
        g_synly_accessibility_change_handler(trusted);
      }
    });
  }];
}

bool synly_foreground_cursor_captured(void) {
  if (CGCursorIsVisible()) {
    return false;
  }
  NSRunningApplication *front = [[NSWorkspace sharedWorkspace] frontmostApplication];
  if (front == nil) {
    return false;
  }
  NSString *bundleId = front.bundleIdentifier ?: @"";
  NSArray<NSString *> *excluded = @[
    @"com.apple.finder",
    @"com.apple.dock",
    @"com.apple.systemuiserver",
    @"com.apple.loginwindow",
    @"com.apple.Spotlight",
    @"com.apple.notificationcenterui",
  ];
  for (NSString *candidate in excluded) {
    if ([bundleId isEqualToString:candidate]) {
      return false;
    }
  }
  return true;
}
