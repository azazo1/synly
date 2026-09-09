#import <AppKit/AppKit.h>
#import <Foundation/Foundation.h>
#include <stdbool.h>

static void (*g_show_callback)(void) = NULL;
static BOOL g_follow_window = YES;
static NSTimeInterval g_ignore_activate_until = 0;
static id g_helper = nil;

@interface SynlyDockHelper : NSObject <NSApplicationDelegate>
@property(nonatomic, strong) id originalDelegate;
@end

@implementation SynlyDockHelper

- (BOOL)respondsToSelector:(SEL)selector {
  return [super respondsToSelector:selector] ||
         [self.originalDelegate respondsToSelector:selector];
}

- (id)forwardingTargetForSelector:(SEL)selector {
  if ([self.originalDelegate respondsToSelector:selector]) {
    return self.originalDelegate;
  }
  return [super forwardingTargetForSelector:selector];
}

- (BOOL)applicationShouldHandleReopen:(NSApplication *)sender
                    hasVisibleWindows:(BOOL)flag {
  if ([NSDate date].timeIntervalSinceReferenceDate < g_ignore_activate_until) {
    return NO;
  }
  if (g_show_callback) {
    g_show_callback();
  }
  return YES;
}

- (void)applicationDidBecomeActive:(NSNotification *)notification {
  if ([self.originalDelegate respondsToSelector:@selector(applicationDidBecomeActive:)]) {
    [self.originalDelegate applicationDidBecomeActive:notification];
  }
  if ([NSDate date].timeIntervalSinceReferenceDate < g_ignore_activate_until) {
    return;
  }
}

@end

static void synly_dock_install_helper(void) {
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    NSApplication *app = [NSApplication sharedApplication];
    SynlyDockHelper *helper = [SynlyDockHelper new];
    helper.originalDelegate = app.delegate;
    app.delegate = helper;
    g_helper = helper;
  });
}

void synly_dock_set_show_callback(void (*callback)(void)) {
  g_show_callback = callback;
  synly_dock_install_helper();
}

void synly_dock_set_follow_window(bool follow) {
  g_follow_window = follow ? YES : NO;
}

void synly_dock_note_hidden(void) {
  g_ignore_activate_until = [NSDate date].timeIntervalSinceReferenceDate + 0.6;
}

void synly_dock_set_visible(bool visible) {
  synly_dock_install_helper();
  NSApplication *app = [NSApplication sharedApplication];
  if (visible) {
    [app setActivationPolicy:NSApplicationActivationPolicyRegular];
    [app activateIgnoringOtherApps:YES];
  } else {
    synly_dock_note_hidden();
    if (g_follow_window) {
      [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
    }
  }
}
