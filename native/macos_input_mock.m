#import <AppKit/AppKit.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef struct {
  bool active;
  int32_t x;
  int32_t y;
  int32_t dx;
  int32_t dy;
  uint32_t key_count;
  uint32_t button_mask;
  int32_t wheel_x;
  int32_t wheel_y;
  char event[256];
} SynlyInputMockState;

static pthread_mutex_t g_state_mutex = PTHREAD_MUTEX_INITIALIZER;
static SynlyInputMockState g_state;
static bool g_update_scheduled = false;
static atomic_bool g_running = false;

@interface SynlyInputMockView : NSView
@property(nonatomic) int32_t virtualWidth;
@property(nonatomic) int32_t virtualHeight;
@property(nonatomic) BOOL active;
@property(nonatomic) int32_t cursorX;
@property(nonatomic) int32_t cursorY;
@property(nonatomic) int32_t deltaX;
@property(nonatomic) int32_t deltaY;
@property(nonatomic) uint32_t keyCount;
@property(nonatomic) uint32_t buttonMask;
@property(nonatomic) int32_t wheelX;
@property(nonatomic) int32_t wheelY;
@property(nonatomic, copy) NSString *sourceEdge;
@property(nonatomic, copy) NSString *lastEvent;
@property(nonatomic, strong) NSMutableArray<NSValue *> *trail;
@end

@implementation SynlyInputMockView

- (BOOL)isFlipped {
  return YES;
}

- (void)applyState:(SynlyInputMockState)state {
  BOOL wasActive = self.active;
  self.active = state.active;
  self.cursorX = state.x;
  self.cursorY = state.y;
  self.deltaX = state.dx;
  self.deltaY = state.dy;
  self.keyCount = state.key_count;
  self.buttonMask = state.button_mask;
  self.wheelX = state.wheel_x;
  self.wheelY = state.wheel_y;
  self.lastEvent = [NSString stringWithUTF8String:state.event];

  if (!wasActive && self.active) {
    [self.trail removeAllObjects];
  }
  if (self.active) {
    NSPoint point = NSMakePoint(self.cursorX, self.cursorY);
    NSValue *last = self.trail.lastObject;
    if (last == nil || !NSEqualPoints(last.pointValue, point)) {
      [self.trail addObject:[NSValue valueWithPoint:point]];
      if (self.trail.count > 180) {
        [self.trail removeObjectAtIndex:0];
      }
    }
  }
  [self setNeedsDisplay:YES];
}

- (NSDictionary<NSAttributedStringKey, id> *)textStyleWithSize:(CGFloat)size
                                                         color:(NSColor *)color
                                                        weight:(NSFontWeight)weight {
  return @{
    NSFontAttributeName: [NSFont systemFontOfSize:size weight:weight],
    NSForegroundColorAttributeName: color,
  };
}

- (void)drawRect:(NSRect)dirtyRect {
  (void) dirtyRect;
  [[NSColor colorWithRed:0.075 green:0.082 blue:0.09 alpha:1.0] setFill];
  NSRectFill(self.bounds);

  NSDictionary *titleStyle = [self textStyleWithSize:20
                                               color:NSColor.whiteColor
                                              weight:NSFontWeightSemibold];
  NSDictionary *labelStyle = [self textStyleWithSize:13
                                               color:[NSColor colorWithWhite:0.78 alpha:1.0]
                                              weight:NSFontWeightRegular];
  NSDictionary *monoStyle = @{
    NSFontAttributeName: [NSFont monospacedDigitSystemFontOfSize:13 weight:NSFontWeightMedium],
    NSForegroundColorAttributeName: [NSColor colorWithWhite:0.9 alpha:1.0],
  };

  [@"Synly macOS input mock" drawAtPoint:NSMakePoint(24, 18) withAttributes:titleStyle];
  NSString *stateText = self.active ? @"状态: 已接入 mock, 本机事件正在被消费"
                                    : [NSString stringWithFormat:@"状态: 等待从 %@ 边缘接入", self.sourceEdge];
  NSColor *stateColor = self.active
      ? [NSColor colorWithRed:0.25 green:0.88 blue:0.55 alpha:1.0]
      : [NSColor colorWithRed:0.98 green:0.73 blue:0.25 alpha:1.0];
  [stateText drawAtPoint:NSMakePoint(24, 52)
          withAttributes:[self textStyleWithSize:14 color:stateColor weight:NSFontWeightMedium]];

  NSRect screen = NSInsetRect(self.bounds, 24, 24);
  screen.origin.y = 92;
  screen.size.height -= 184;
  [[NSColor colorWithRed:0.11 green:0.125 blue:0.14 alpha:1.0] setFill];
  NSBezierPath *screenPath = [NSBezierPath bezierPathWithRoundedRect:screen xRadius:6 yRadius:6];
  [screenPath fill];
  [[NSColor colorWithWhite:0.42 alpha:1.0] setStroke];
  screenPath.lineWidth = 1;
  [screenPath stroke];

  CGFloat scaleX = screen.size.width / MAX(self.virtualWidth, 1);
  CGFloat scaleY = screen.size.height / MAX(self.virtualHeight, 1);
  if (self.trail.count > 1) {
    NSBezierPath *trailPath = [NSBezierPath bezierPath];
    BOOL first = YES;
    for (NSValue *value in self.trail) {
      NSPoint raw = value.pointValue;
      NSPoint mapped = NSMakePoint(screen.origin.x + raw.x * scaleX,
                                   screen.origin.y + raw.y * scaleY);
      if (first) {
        [trailPath moveToPoint:mapped];
        first = NO;
      } else {
        [trailPath lineToPoint:mapped];
      }
    }
    [[NSColor colorWithRed:0.22 green:0.68 blue:0.98 alpha:0.45] setStroke];
    trailPath.lineWidth = 2;
    [trailPath stroke];
  }

  if (self.active) {
    NSPoint cursor = NSMakePoint(screen.origin.x + self.cursorX * scaleX,
                                 screen.origin.y + self.cursorY * scaleY);
    [[NSColor colorWithRed:0.98 green:0.34 blue:0.28 alpha:1.0] setFill];
    [[NSBezierPath bezierPathWithOvalInRect:NSMakeRect(cursor.x - 7, cursor.y - 7, 14, 14)] fill];
    [[NSColor whiteColor] setStroke];
    NSBezierPath *cross = [NSBezierPath bezierPath];
    [cross moveToPoint:NSMakePoint(cursor.x - 11, cursor.y)];
    [cross lineToPoint:NSMakePoint(cursor.x + 11, cursor.y)];
    [cross moveToPoint:NSMakePoint(cursor.x, cursor.y - 11)];
    [cross lineToPoint:NSMakePoint(cursor.x, cursor.y + 11)];
    cross.lineWidth = 1;
    [cross stroke];
  }

  CGFloat footerY = NSMaxY(screen) + 14;
  NSString *position = [NSString stringWithFormat:@"虚拟屏幕: %d x %d    光标: x=%d, y=%d    delta: %+d, %+d",
                                                   self.virtualWidth, self.virtualHeight,
                                                   self.cursorX, self.cursorY,
                                                   self.deltaX, self.deltaY];
  [position drawAtPoint:NSMakePoint(24, footerY) withAttributes:monoStyle];
  NSString *inputs = [NSString stringWithFormat:@"按键: %u    按钮掩码: 0x%02x    滚轮累计: x=%d, y=%d",
                                                 self.keyCount, self.buttonMask,
                                                 self.wheelX, self.wheelY];
  [inputs drawAtPoint:NSMakePoint(24, footerY + 24) withAttributes:monoStyle];
  NSString *event = [NSString stringWithFormat:@"最后事件: %@", self.lastEvent ?: @"无"];
  [event drawAtPoint:NSMakePoint(24, footerY + 48) withAttributes:labelStyle];
  [@"关闭窗口或在终端按 Ctrl-C 可立即恢复本机光标" drawAtPoint:NSMakePoint(24, footerY + 72)
                                                   withAttributes:labelStyle];
}

@end

@interface SynlyInputMockDelegate : NSObject <NSApplicationDelegate, NSWindowDelegate>
@end

@implementation SynlyInputMockDelegate
- (BOOL)applicationShouldTerminateAfterLastWindowClosed:(NSApplication *)sender {
  (void) sender;
  return YES;
}

- (void)applicationWillTerminate:(NSNotification *)notification {
  (void) notification;
  atomic_store(&g_running, false);
}

- (void)windowWillClose:(NSNotification *)notification {
  (void) notification;
  atomic_store(&g_running, false);
}
@end

static NSWindow *g_window;
static SynlyInputMockView *g_view;
static SynlyInputMockDelegate *g_delegate;

int synly_input_mock_gui_prepare(int32_t width, int32_t height, const char *source_edge) {
  @autoreleasepool {
    NSApplication *app = NSApplication.sharedApplication;
    app.activationPolicy = NSApplicationActivationPolicyRegular;
    g_delegate = [[SynlyInputMockDelegate alloc] init];
    app.delegate = g_delegate;

    NSRect frame = NSMakeRect(0, 0, 940, 650);
    NSWindowStyleMask style = NSWindowStyleMaskTitled | NSWindowStyleMaskClosable |
                              NSWindowStyleMaskMiniaturizable | NSWindowStyleMaskResizable;
    g_window = [[NSWindow alloc] initWithContentRect:frame
                                           styleMask:style
                                             backing:NSBackingStoreBuffered
                                               defer:NO];
    g_window.title = @"Synly macOS input mock";
    g_window.minSize = NSMakeSize(720, 520);
    g_window.delegate = g_delegate;

    g_view = [[SynlyInputMockView alloc] initWithFrame:frame];
    g_view.virtualWidth = MAX(width, 1);
    g_view.virtualHeight = MAX(height, 1);
    g_view.sourceEdge = [NSString stringWithUTF8String:source_edge ?: "right"];
    g_view.lastEvent = @"等待输入";
    g_view.trail = [NSMutableArray array];
    g_window.contentView = g_view;
    [g_window center];
    [g_window makeKeyAndOrderFront:nil];
    [app activateIgnoringOtherApps:YES];
    atomic_store(&g_running, true);
    return 0;
  }
}

void synly_input_mock_gui_run(void) {
  @autoreleasepool {
    [NSApplication.sharedApplication run];
    atomic_store(&g_running, false);
  }
}

bool synly_input_mock_gui_is_running(void) {
  return atomic_load(&g_running);
}

void synly_input_mock_gui_stop(void) {
  atomic_store(&g_running, false);
  dispatch_async(dispatch_get_main_queue(), ^{
    [NSApplication.sharedApplication terminate:nil];
  });
}

void synly_input_mock_gui_update(
    bool active,
    int32_t x,
    int32_t y,
    int32_t dx,
    int32_t dy,
    uint32_t key_count,
    uint32_t button_mask,
    int32_t wheel_x,
    int32_t wheel_y,
    const char *event) {
  pthread_mutex_lock(&g_state_mutex);
  g_state.active = active;
  g_state.x = x;
  g_state.y = y;
  g_state.dx = dx;
  g_state.dy = dy;
  g_state.key_count = key_count;
  g_state.button_mask = button_mask;
  g_state.wheel_x = wheel_x;
  g_state.wheel_y = wheel_y;
  snprintf(g_state.event, sizeof(g_state.event), "%s", event ?: "");
  bool should_schedule = !g_update_scheduled;
  g_update_scheduled = true;
  pthread_mutex_unlock(&g_state_mutex);

  if (!should_schedule) {
    return;
  }
  dispatch_async(dispatch_get_main_queue(), ^{
    SynlyInputMockState state;
    pthread_mutex_lock(&g_state_mutex);
    state = g_state;
    g_update_scheduled = false;
    pthread_mutex_unlock(&g_state_mutex);
    [g_view applyState:state];
  });
}
