#import <AppKit/AppKit.h>
#import <CoreServices/CoreServices.h>
#import <Foundation/Foundation.h>
#include <stdbool.h>

static void (*g_show_callback)(void) = NULL;
static BOOL g_follow_window = YES;
static NSTimeInterval g_ignore_activate_until = 0;
static id g_helper = nil;

@interface SynlyDockHelper : NSObject
@end

@implementation SynlyDockHelper

- (void)applicationDidBecomeActive:(NSNotification *)notification {
  (void)notification;
  [self requestShowIfAllowed];
}

- (void)handleReopenEvent:(NSAppleEventDescriptor *)event
           withReplyEvent:(NSAppleEventDescriptor *)reply {
  (void)event;
  (void)reply;
  [self requestShowIfAllowed];
}

- (void)requestShowIfAllowed {
  if ([NSDate date].timeIntervalSinceReferenceDate < g_ignore_activate_until) {
    return;
  }
  if (g_show_callback) {
    g_show_callback();
  }
}

@end

static void synly_dock_install_helper(void) {
  static dispatch_once_t once;
  dispatch_once(&once, ^{
    // Do not replace NSApplication.delegate. winit panics in sendEvent if the
    // current delegate is not WinitApplicationDelegate.
    SynlyDockHelper *helper = [SynlyDockHelper new];
    NSApplication *app = [NSApplication sharedApplication];
    [[NSNotificationCenter defaultCenter]
        addObserver:helper
           selector:@selector(applicationDidBecomeActive:)
               name:NSApplicationDidBecomeActiveNotification
             object:app];
    [[NSAppleEventManager sharedAppleEventManager]
        setEventHandler:helper
            andSelector:@selector(handleReopenEvent:withReplyEvent:)
          forEventClass:kCoreEventClass
             andEventID:kAEReopenApplication];
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
  NSApplication *app = [NSApplication sharedApplication];
  if (visible) {
    [app setActivationPolicy:NSApplicationActivationPolicyRegular];
    if (!app.isActive) {
      [app activateIgnoringOtherApps:YES];
    }
  } else {
    synly_dock_note_hidden();
    if (g_follow_window) {
      [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
    }
  }
}
