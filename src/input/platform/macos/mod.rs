use super::{CaptureContext, InputBackend, NativeEvent, ScrollSource};
use crate::input::{DesktopLayout, DisplayRect, InputMode, KeySnapshot, ModifierMask, Point};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::ffi::{c_double, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub mod permissions;

pub(super) fn foreground_cursor_captured() -> bool {
    permissions::foreground_cursor_captured()
}

type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFStringRef = *const c_void;
type CFTypeRef = *const c_void;
type CGDirectDisplayID = u32;
type CGEventType = u32;
type CGEventFlags = u64;
type CGKeyCode = u16;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: c_double,
    y: c_double,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: c_double,
    height: c_double,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

const EVENT_LEFT_DOWN: CGEventType = 1;
const EVENT_LEFT_UP: CGEventType = 2;
const EVENT_RIGHT_DOWN: CGEventType = 3;
const EVENT_RIGHT_UP: CGEventType = 4;
const EVENT_MOUSE_MOVED: CGEventType = 5;
const EVENT_LEFT_DRAGGED: CGEventType = 6;
const EVENT_RIGHT_DRAGGED: CGEventType = 7;
const EVENT_KEY_DOWN: CGEventType = 10;
const EVENT_KEY_UP: CGEventType = 11;
const EVENT_FLAGS_CHANGED: CGEventType = 12;
const EVENT_SCROLL: CGEventType = 22;
const EVENT_OTHER_DOWN: CGEventType = 25;
const EVENT_OTHER_UP: CGEventType = 26;
const EVENT_OTHER_DRAGGED: CGEventType = 27;
const EVENT_GESTURE: CGEventType = 29;
const EVENT_DOCK_SWIPE: CGEventType = 30;
const EVENT_NAVIGATION_SWIPE: CGEventType = 31;
const EVENT_TAP_DISABLED_TIMEOUT: CGEventType = u32::MAX - 1;
const EVENT_TAP_DISABLED_USER_INPUT: CGEventType = u32::MAX;
const FIELD_MOUSE_BUTTON: u32 = 3;
const FIELD_MOUSE_DELTA_X: u32 = 4;
const FIELD_MOUSE_DELTA_Y: u32 = 5;
const FIELD_SCROLL_DELTA_Y: u32 = 11;
const FIELD_SCROLL_DELTA_X: u32 = 12;
const FIELD_SCROLL_IS_CONTINUOUS: u32 = 88;
const FIELD_KEY_AUTOREPEAT: u32 = 8;
const FIELD_KEY_CODE: u32 = 9;
const FIELD_SOURCE_USER_DATA: u32 = 42;
const FLAG_SHIFT: u64 = 1 << 17;
const FLAG_CONTROL: u64 = 1 << 18;
const FLAG_ALT: u64 = 1 << 19;
const FLAG_META: u64 = 1 << 20;
const EVENT_TAG: i64 = 0x5359_4e4c_5949_4e50;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: Option<unsafe extern "C" fn(CGEventTapProxy, CGEventType, CGEventRef, *mut c_void) -> CGEventRef>,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventGetFlags(event: CGEventRef) -> CGEventFlags;
    fn CGEventSetFlags(event: CGEventRef, flags: CGEventFlags);
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventCreate(source: *const c_void) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(source: *const c_void, key: CGKeyCode, down: bool) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: *const c_void,
        event_type: CGEventType,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent(
        source: *const c_void,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGGetActiveDisplayList(max_displays: u32, displays: *mut CGDirectDisplayID, count: *mut u32) -> i32;
    fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    fn CGMainDisplayID() -> CGDirectDisplayID;
    fn CGDisplayHideCursor(display: CGDirectDisplayID) -> i32;
    fn CGDisplayShowCursor(display: CGDirectDisplayID) -> i32;
    fn CGWarpMouseCursorPosition(position: CGPoint) -> i32;
    fn CGAssociateMouseAndMouseCursorPosition(connected: bool) -> i32;
    fn CGSetLocalEventsSuppressionInterval(interval: c_double);
    fn CGSSetConnectionProperty(
        connection: i32,
        target_connection: i32,
        key: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    fn _CGSDefaultConnection() -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopCommonModes: CFStringRef;
    static kCFBooleanTrue: CFTypeRef;
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        value: *const i8,
        encoding: u32,
    ) -> CFStringRef;
    fn CFMachPortCreateRunLoopSource(
        allocator: *const c_void,
        port: CFMachPortRef,
        order: isize,
    ) -> CFRunLoopSourceRef;
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(run_loop: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRun();
    fn CFRunLoopStop(run_loop: CFRunLoopRef);
    fn CFRelease(value: *const c_void);
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

struct MacState {
    context: CaptureContext,
    physical_pressed: Mutex<BTreeSet<u16>>,
    physical_buttons: Mutex<BTreeSet<u8>>,
    injected_pressed: Mutex<BTreeSet<u16>>,
    injected_buttons: Mutex<BTreeSet<u8>>,
    injected_cursor: Mutex<Option<Point>>,
    keyboard_capture: AtomicBool,
    tap: Mutex<Option<usize>>,
    run_loop: Mutex<Option<usize>>,
}

struct MacBackend {
    state: Arc<MacState>,
}

impl Drop for MacBackend {
    fn drop(&mut self) {
        let _ = self.set_capture(false);
        let _ = self.release_all();
        if let Some(tap) = *self.state.tap.lock().unwrap() {
            unsafe { CGEventTapEnable(tap as CFMachPortRef, false) };
        }
        if let Some(run_loop) = *self.state.run_loop.lock().unwrap() {
            unsafe { CFRunLoopStop(run_loop as CFRunLoopRef) };
        }
    }
}

pub fn ensure_permissions(_mode: InputMode) -> Result<()> {
    if !permissions::is_accessibility_trusted() {
        bail!("鼠标键盘同步需要在系统设置中授予 Synly 辅助功能权限")
    }
    if unsafe { IsSecureEventInputEnabled() } {
        bail!("macOS Secure Input 已启用, 无法安全启动鼠标键盘同步")
    }
    Ok(())
}

pub fn start(context: CaptureContext) -> Result<Arc<dyn InputBackend>> {
    ensure_permissions(context.mode)?;
    let state = Arc::new(MacState {
        context,
        physical_pressed: Mutex::new(BTreeSet::new()),
        physical_buttons: Mutex::new(BTreeSet::new()),
        injected_pressed: Mutex::new(BTreeSet::new()),
        injected_buttons: Mutex::new(BTreeSet::new()),
        injected_cursor: Mutex::new(None),
        keyboard_capture: AtomicBool::new(false),
        tap: Mutex::new(None),
        run_loop: Mutex::new(None),
    });
    let thread_state = Arc::clone(&state);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("synly-input-macos".to_string())
        .spawn(move || run_event_tap(thread_state, ready_tx))
        .context("无法启动 macOS 输入捕获线程")?;
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .context("等待 macOS 输入捕获后端启动超时")??;
    Ok(Arc::new(MacBackend { state }))
}

fn run_event_tap(state: Arc<MacState>, ready: std::sync::mpsc::SyncSender<Result<()>>) {
    let mask = [
        EVENT_LEFT_DOWN,
        EVENT_LEFT_UP,
        EVENT_RIGHT_DOWN,
        EVENT_RIGHT_UP,
        EVENT_MOUSE_MOVED,
        EVENT_LEFT_DRAGGED,
        EVENT_RIGHT_DRAGGED,
        EVENT_KEY_DOWN,
        EVENT_KEY_UP,
        EVENT_FLAGS_CHANGED,
        EVENT_SCROLL,
        EVENT_OTHER_DOWN,
        EVENT_OTHER_UP,
        EVENT_OTHER_DRAGGED,
        EVENT_GESTURE,
        EVENT_DOCK_SWIPE,
        EVENT_NAVIGATION_SWIPE,
    ]
    .into_iter()
    .fold(0u64, |mask, event| mask | (1u64 << event));
    let context = Arc::into_raw(Arc::clone(&state)) as *mut c_void;
    let tap = unsafe { CGEventTapCreate(0, 0, 0, mask, Some(event_callback), context) };
    if tap.is_null() {
        unsafe { drop(Arc::from_raw(context.cast::<MacState>())) };
        let _ = ready.send(Err(anyhow::anyhow!(
            "无法创建 Quartz event tap, 请检查辅助功能和输入监控权限"
        )));
        return;
    }
    *state.tap.lock().unwrap() = Some(tap as usize);
    let source = unsafe { CFMachPortCreateRunLoopSource(ptr::null(), tap, 0) };
    if source.is_null() {
        unsafe {
            CGEventTapEnable(tap, false);
            CFRelease(tap);
            drop(Arc::from_raw(context.cast::<MacState>()));
        }
        let _ = ready.send(Err(anyhow::anyhow!("无法创建 Quartz event tap run loop source")));
        return;
    }
    unsafe {
        *state.run_loop.lock().unwrap() = Some(CFRunLoopGetCurrent() as usize);
        CFRunLoopAddSource(CFRunLoopGetCurrent(), source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
    }
    let _ = ready.send(Ok(()));
    unsafe { CFRunLoopRun() };
    *state.tap.lock().unwrap() = None;
    *state.run_loop.lock().unwrap() = None;
    unsafe {
        CFRelease(source);
        CFRelease(tap);
        drop(Arc::from_raw(context.cast::<MacState>()));
    }
}

unsafe extern "C" fn event_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let state = unsafe { &*user_info.cast::<MacState>() };
    if event_type == EVENT_TAP_DISABLED_TIMEOUT {
        if let Some(tap) = *state.tap.lock().unwrap() {
            unsafe { CGEventTapEnable(tap as CFMachPortRef, false) };
        }
        state.context.emit_reliable(NativeEvent::Failed(
            "Quartz event tap 响应超时, 已停止输入捕获".to_string(),
        ));
        return event;
    }
    if event_type == EVENT_TAP_DISABLED_USER_INPUT {
        state.context.emit_reliable(NativeEvent::Failed(
            "Quartz event tap 被系统禁用".to_string(),
        ));
        return event;
    }
    if unsafe { CGEventGetIntegerValueField(event, FIELD_SOURCE_USER_DATA) } == EVENT_TAG {
        return event;
    }

    let active = state.context.capture_active.load(Ordering::Acquire);
    match event_type {
        EVENT_MOUSE_MOVED | EVENT_LEFT_DRAGGED | EVENT_RIGHT_DRAGGED | EVENT_OTHER_DRAGGED => {
            let dx = unsafe { CGEventGetIntegerValueField(event, FIELD_MOUSE_DELTA_X) } as i32;
            let dy = unsafe { CGEventGetIntegerValueField(event, FIELD_MOUSE_DELTA_Y) } as i32;
            if !active {
                let position_event = unsafe { CGEventCreate(ptr::null()) };
                let position = if !position_event.is_null() {
                    let position = unsafe { CGEventGetLocation(position_event) };
                    unsafe { CFRelease(position_event) };
                    position
                } else {
                    unsafe { CGEventGetLocation(event) }
                };
                state.context.motion.add_at(
                    dx,
                    dy,
                    Point {
                        x: position.x.round() as i32,
                        y: position.y.round() as i32,
                    },
                );
            } else {
                state.context.motion.add(dx, dy);
            }
            if active { ptr::null_mut() } else { event }
        }
        EVENT_LEFT_DOWN | EVENT_LEFT_UP | EVENT_RIGHT_DOWN | EVENT_RIGHT_UP | EVENT_OTHER_DOWN
        | EVENT_OTHER_UP => {
            let raw_button = unsafe { CGEventGetIntegerValueField(event, FIELD_MOUSE_BUTTON) } as u8;
            let button = match raw_button {
                0 => 1,
                1 => 3,
                2 => 2,
                value => value.saturating_add(1),
            };
            let down = matches!(event_type, EVENT_LEFT_DOWN | EVENT_RIGHT_DOWN | EVENT_OTHER_DOWN);
            update_set(&state.physical_buttons, button, down);
            if active {
                state.context.emit_reliable(NativeEvent::Button { button, down });
                ptr::null_mut()
            } else {
                event
            }
        }
        EVENT_SCROLL => {
            let x = unsafe { CGEventGetIntegerValueField(event, FIELD_SCROLL_DELTA_X) } as i32;
            let y = unsafe { CGEventGetIntegerValueField(event, FIELD_SCROLL_DELTA_Y) } as i32;
            let source = scroll_source(unsafe {
                CGEventGetIntegerValueField(event, FIELD_SCROLL_IS_CONTINUOUS)
            });
            if active {
                state.context.emit_reliable(NativeEvent::Wheel { x, y, source });
                ptr::null_mut()
            } else {
                event
            }
        }
        EVENT_GESTURE | EVENT_DOCK_SWIPE | EVENT_NAVIGATION_SWIPE => {
            if suppress_local_gesture(event_type, active) { ptr::null_mut() } else { event }
        }
        EVENT_KEY_DOWN | EVENT_KEY_UP | EVENT_FLAGS_CHANGED => {
            let keycode = unsafe { CGEventGetIntegerValueField(event, FIELD_KEY_CODE) } as u16;
            let Some(usage) = mac_keycode_to_hid(keycode) else {
                return if active { ptr::null_mut() } else { event };
            };
            let flags = unsafe { CGEventGetFlags(event) };
            let modifiers = modifiers_from_flags(flags);
            let down = if event_type == EVENT_FLAGS_CHANGED {
                modifier_usage_is_down(usage, flags)
            } else {
                event_type == EVENT_KEY_DOWN
            };
            let repeat = event_type == EVENT_KEY_DOWN
                && unsafe { CGEventGetIntegerValueField(event, FIELD_KEY_AUTOREPEAT) } != 0;
            update_set(&state.physical_pressed, usage, down);
            if state.context.hotkey.matches(usage, modifiers) {
                if down && !repeat {
                    state.context.emit_reliable(NativeEvent::Emergency);
                }
                return ptr::null_mut();
            }
            let listening =
                active || state.keyboard_capture.load(Ordering::Acquire);
            if listening && event_type == EVENT_FLAGS_CHANGED && usage_is_modifier(usage) {
                state.context.emit_reliable(NativeEvent::Key {
                    usage,
                    modifiers,
                    down,
                    repeat,
                });
                return event;
            }
            if listening {
                state.context.emit_reliable(NativeEvent::Key {
                    usage,
                    modifiers,
                    down,
                    repeat,
                });
                if active {
                    return ptr::null_mut();
                }
            }
            event
        }
        _ => event,
    }
}

fn suppress_local_gesture(event_type: CGEventType, capture_active: bool) -> bool {
    capture_active
        && matches!(
            event_type,
            EVENT_GESTURE | EVENT_DOCK_SWIPE | EVENT_NAVIGATION_SWIPE
        )
}

fn scroll_source(is_continuous: i64) -> ScrollSource {
    if is_continuous != 0 {
        ScrollSource::Trackpad
    } else {
        ScrollSource::MouseWheel
    }
}

fn usage_is_modifier(usage: u16) -> bool {
    matches!(usage, 0xe0..=0xe7)
}

fn update_set<T: Ord + Copy>(set: &Mutex<BTreeSet<T>>, value: T, down: bool) {
    let mut set = set.lock().unwrap();
    if down {
        set.insert(value);
    } else {
        set.remove(&value);
    }
}

fn mac_mouse_button(button: u8) -> u32 {
    match button {
        1 => 0,
        2 => 2,
        3 => 1,
        other => u32::from(other.saturating_sub(1)),
    }
}

fn enable_background_cursor_updates() {
    const PROPERTY: &[u8] = b"SetsCursorInBackground\0";
    const CF_STRING_ENCODING_MAC_ROMAN: u32 = 0;

    let property = unsafe {
        CFStringCreateWithCString(
            ptr::null(),
            PROPERTY.as_ptr().cast(),
            CF_STRING_ENCODING_MAC_ROMAN,
        )
    };
    if property.is_null() {
        tracing::warn!("无法创建 macOS 后台光标属性名, 光标隐藏可能不稳定");
        return;
    }
    let connection = unsafe { _CGSDefaultConnection() };
    let result = unsafe {
        CGSSetConnectionProperty(connection, connection, property, kCFBooleanTrue)
    };
    unsafe { CFRelease(property) };
    if result != 0 {
        tracing::warn!(error_code = result, "设置 macOS 后台光标属性失败, 光标隐藏可能不稳定");
    } else {
        tracing::debug!("已设置 macOS 后台光标属性");
    }
}

impl InputBackend for MacBackend {
    fn health_check(&self) -> Result<()> {
        let result = if !permissions::is_accessibility_trusted() {
            Err(anyhow::anyhow!("macOS 辅助功能权限已撤销"))
        } else if unsafe { IsSecureEventInputEnabled() } {
            Err(anyhow::anyhow!("macOS Secure Input 已启用"))
        } else {
            Ok(())
        };
        if result.is_err() {
            self.state.context.failed.store(true, Ordering::Release);
        }
        result
    }

    fn layout(&self) -> Result<DesktopLayout> {
        let mut count = 0u32;
        let result = unsafe { CGGetActiveDisplayList(0, ptr::null_mut(), &mut count) };
        if result != 0 || count == 0 {
            bail!("无法读取 macOS 显示器列表, 错误码 {result}");
        }
        let mut displays = vec![0u32; count as usize];
        let result = unsafe { CGGetActiveDisplayList(count, displays.as_mut_ptr(), &mut count) };
        if result != 0 {
            bail!("无法读取 macOS 显示器布局, 错误码 {result}");
        }
        DesktopLayout::new(
            displays
                .into_iter()
                .take(count as usize)
                .map(|display| unsafe { CGDisplayBounds(display) })
                .map(|bounds| DisplayRect {
                    x: bounds.origin.x.round() as i32,
                    y: bounds.origin.y.round() as i32,
                    width: bounds.size.width.round() as i32,
                    height: bounds.size.height.round() as i32,
                })
                .collect(),
        )
    }

    fn cursor_position(&self) -> Result<Point> {
        let event = unsafe { CGEventCreate(ptr::null()) };
        if event.is_null() {
            bail!("无法读取 macOS 光标位置");
        }
        let point = unsafe { CGEventGetLocation(event) };
        unsafe { CFRelease(event) };
        Ok(Point { x: point.x.round() as i32, y: point.y.round() as i32 })
    }

    fn snapshot(&self) -> KeySnapshot {
        let (usages, modifiers) = {
            let pressed = self.state.physical_pressed.lock().unwrap();
            (
                pressed.iter().copied().collect(),
                current_modifiers(&pressed),
            )
        };
        let buttons = self.state.physical_buttons.lock().unwrap().iter().copied().collect();
        KeySnapshot { usages, modifiers, buttons }
    }

    fn set_capture(&self, active: bool) -> Result<()> {
        let previous = self.state.context.capture_active.swap(active, Ordering::AcqRel);
        if previous == active {
            return Ok(());
        }
        let display = unsafe { CGMainDisplayID() };
        enable_background_cursor_updates();
        let visibility_result = if active {
            unsafe { CGDisplayHideCursor(display) }
        } else {
            unsafe { CGDisplayShowCursor(display) }
        };
        let association_result = if active {
            let recouple_result = unsafe { CGAssociateMouseAndMouseCursorPosition(true) };
            unsafe { CGSetLocalEventsSuppressionInterval(0.0001) };
            let decouple_result = unsafe { CGAssociateMouseAndMouseCursorPosition(false) };
            if recouple_result != 0 { recouple_result } else { decouple_result }
        } else {
            unsafe { CGSetLocalEventsSuppressionInterval(0.0) };
            unsafe { CGAssociateMouseAndMouseCursorPosition(true) }
        };
        if visibility_result != 0 || association_result != 0 {
            if active {
                unsafe {
                    CGAssociateMouseAndMouseCursorPosition(true);
                    CGSetLocalEventsSuppressionInterval(0.0);
                    CGDisplayShowCursor(display);
                }
                self.state
                    .context
                    .capture_active
                    .store(false, Ordering::Release);
            }
            bail!(
                "切换 macOS 光标捕获状态失败, visibility={visibility_result}, association={association_result}"
            );
        }
        Ok(())
    }

    fn set_keyboard_capture(&self, active: bool) -> Result<()> {
        self.state.keyboard_capture.store(active, Ordering::Release);
        if active {
            tracing::info!("macOS 键盘监听捕获已开启, 光标状态不受影响");
        }
        Ok(())
    }

    fn warp_cursor(&self, point: Point) -> Result<()> {
        let result = unsafe { CGWarpMouseCursorPosition(CGPoint { x: point.x as f64, y: point.y as f64 }) };
        if result != 0 {
            bail!("移动 macOS 光标失败, 错误码 {result}");
        }
        *self.state.injected_cursor.lock().unwrap() = Some(point);
        Ok(())
    }

    fn inject_key(
        &self,
        usage: u16,
        modifiers: ModifierMask,
        down: bool,
        _repeat: bool,
    ) -> Result<()> {
        let keycode = hid_to_mac_keycode(usage)
            .with_context(|| format!("macOS 不支持 USB HID usage 0x{usage:04x}"))?;
        let event = unsafe { CGEventCreateKeyboardEvent(ptr::null(), keycode, down) };
        post_event(event, flags_from_modifiers(modifiers))?;
        update_set(&self.state.injected_pressed, usage, down);
        Ok(())
    }

    fn inject_button(&self, button: u8, down: bool) -> Result<()> {
        let point = match *self.state.injected_cursor.lock().unwrap() {
            Some(point) => point,
            None => self.cursor_position()?,
        };
        let event_type = match (button, down) {
            (1, true) => EVENT_LEFT_DOWN,
            (1, false) => EVENT_LEFT_UP,
            (3, true) => EVENT_RIGHT_DOWN,
            (3, false) => EVENT_RIGHT_UP,
            (_, true) => EVENT_OTHER_DOWN,
            (_, false) => EVENT_OTHER_UP,
        };
        let mouse_button = mac_mouse_button(button);
        let event = unsafe {
            CGEventCreateMouseEvent(
                ptr::null(),
                event_type,
                CGPoint { x: point.x as f64, y: point.y as f64 },
                mouse_button,
            )
        };
        post_event(event, 0)?;
        update_set(&self.state.injected_buttons, button, down);
        Ok(())
    }

    fn inject_cursor(&self, point: Point) -> Result<()> {
        let pressed_button = self
            .state
            .injected_buttons
            .lock()
            .unwrap()
            .iter()
            .next()
            .copied();
        let (mouse_button, event_type) = match pressed_button {
            Some(1) => (0, EVENT_LEFT_DRAGGED),
            Some(3) => (1, EVENT_RIGHT_DRAGGED),
            Some(button) => (mac_mouse_button(button), EVENT_OTHER_DRAGGED),
            None => (0, EVENT_MOUSE_MOVED),
        };
        let event = unsafe {
            CGEventCreateMouseEvent(
                ptr::null(),
                event_type,
                CGPoint { x: point.x as f64, y: point.y as f64 },
                mouse_button,
            )
        };
        if event.is_null() {
            bail!("无法创建 macOS 鼠标移动事件");
        }
        if let Some(previous) = *self.state.injected_cursor.lock().unwrap() {
            unsafe {
                CGEventSetIntegerValueField(
                    event,
                    FIELD_MOUSE_DELTA_X,
                    i64::from(point.x.saturating_sub(previous.x)),
                );
                CGEventSetIntegerValueField(
                    event,
                    FIELD_MOUSE_DELTA_Y,
                    i64::from(point.y.saturating_sub(previous.y)),
                );
            }
        }
        let modifiers = current_modifiers(&self.state.injected_pressed.lock().unwrap());
        post_event(event, flags_from_modifiers(modifiers))?;
        *self.state.injected_cursor.lock().unwrap() = Some(point);
        Ok(())
    }

    fn inject_motion(&self, dx: i32, dy: i32) -> Result<()> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        let point = match *self.state.injected_cursor.lock().unwrap() {
            Some(point) => point,
            None => self.cursor_position()?,
        };
        let event = unsafe {
            CGEventCreateMouseEvent(
                ptr::null(),
                EVENT_MOUSE_MOVED,
                CGPoint { x: point.x as f64, y: point.y as f64 },
                0,
            )
        };
        if event.is_null() {
            bail!("无法创建 macOS 相对鼠标移动事件");
        }
        unsafe {
            CGEventSetIntegerValueField(event, FIELD_MOUSE_DELTA_X, i64::from(dx));
            CGEventSetIntegerValueField(event, FIELD_MOUSE_DELTA_Y, i64::from(dy));
        }
        let modifiers = current_modifiers(&self.state.injected_pressed.lock().unwrap());
        post_event(event, flags_from_modifiers(modifiers))?;
        Ok(())
    }

    fn inject_wheel(&self, x: i32, y: i32) -> Result<()> {
        let event = unsafe { CGEventCreateScrollWheelEvent(ptr::null(), 1, 2, y, x) };
        post_event(event, 0)
    }

    fn release_all(&self) -> Result<()> {
        let keys = std::mem::take(&mut *self.state.injected_pressed.lock().unwrap());
        let buttons = std::mem::take(&mut *self.state.injected_buttons.lock().unwrap());
        for usage in keys {
            let _ = self.inject_key(usage, ModifierMask::default(), false, false);
        }
        for button in buttons {
            let _ = self.inject_button(button, false);
        }
        Ok(())
    }
}

fn post_event(event: CGEventRef, flags: u64) -> Result<()> {
    if event.is_null() {
        bail!("无法创建 macOS 输入事件");
    }
    unsafe {
        CGEventSetIntegerValueField(event, FIELD_SOURCE_USER_DATA, EVENT_TAG);
        if flags != 0 {
            CGEventSetFlags(event, flags);
        }
        CGEventPost(0, event);
        CFRelease(event);
    }
    Ok(())
}

fn modifiers_from_flags(flags: u64) -> ModifierMask {
    let mut bits = 0u8;
    if flags & FLAG_CONTROL != 0 { bits |= ModifierMask::CTRL.bits(); }
    if flags & FLAG_ALT != 0 { bits |= ModifierMask::ALT.bits(); }
    if flags & FLAG_SHIFT != 0 { bits |= ModifierMask::SHIFT.bits(); }
    if flags & FLAG_META != 0 { bits |= ModifierMask::META.bits(); }
    ModifierMask::from_bits(bits)
}

fn flags_from_modifiers(modifiers: ModifierMask) -> u64 {
    let mut flags = 0;
    if modifiers.contains(ModifierMask::CTRL) { flags |= FLAG_CONTROL; }
    if modifiers.contains(ModifierMask::ALT) { flags |= FLAG_ALT; }
    if modifiers.contains(ModifierMask::SHIFT) { flags |= FLAG_SHIFT; }
    if modifiers.contains(ModifierMask::META) { flags |= FLAG_META; }
    flags
}

fn modifier_usage_is_down(usage: u16, flags: u64) -> bool {
    match usage {
        0xe0 | 0xe4 => flags & FLAG_CONTROL != 0,
        0xe1 | 0xe5 => flags & FLAG_SHIFT != 0,
        0xe2 | 0xe6 => flags & FLAG_ALT != 0,
        0xe3 | 0xe7 => flags & FLAG_META != 0,
        _ => false,
    }
}

fn current_modifiers(keys: &BTreeSet<u16>) -> ModifierMask {
    let mut bits = 0;
    if keys.contains(&0xe0) || keys.contains(&0xe4) { bits |= ModifierMask::CTRL.bits(); }
    if keys.contains(&0xe1) || keys.contains(&0xe5) { bits |= ModifierMask::SHIFT.bits(); }
    if keys.contains(&0xe2) || keys.contains(&0xe6) { bits |= ModifierMask::ALT.bits(); }
    if keys.contains(&0xe3) || keys.contains(&0xe7) { bits |= ModifierMask::META.bits(); }
    ModifierMask::from_bits(bits)
}

fn mac_keycode_to_hid(code: u16) -> Option<u16> {
    Some(match code {
        0 => 0x04, 11 => 0x05, 8 => 0x06, 2 => 0x07, 14 => 0x08, 3 => 0x09,
        5 => 0x0a, 4 => 0x0b, 34 => 0x0c, 38 => 0x0d, 40 => 0x0e, 37 => 0x0f,
        46 => 0x10, 45 => 0x11, 31 => 0x12, 35 => 0x13, 12 => 0x14, 15 => 0x15,
        1 => 0x16, 17 => 0x17, 32 => 0x18, 9 => 0x19, 13 => 0x1a, 7 => 0x1b,
        16 => 0x1c, 6 => 0x1d, 18 => 0x1e, 19 => 0x1f, 20 => 0x20, 21 => 0x21,
        23 => 0x22, 22 => 0x23, 26 => 0x24, 28 => 0x25, 25 => 0x26, 29 => 0x27,
        36 => 0x28, 53 => 0x29, 51 => 0x2a, 48 => 0x2b, 49 => 0x2c, 24 => 0x2e,
        27 => 0x2d, 33 => 0x2f, 30 => 0x30, 42 => 0x31, 41 => 0x33, 39 => 0x34,
        43 => 0x36, 47 => 0x37, 44 => 0x38, 57 => 0x39, 122 => 0x3a, 120 => 0x3b,
        99 => 0x3c, 118 => 0x3d, 96 => 0x3e, 97 => 0x3f, 98 => 0x40, 100 => 0x41,
        101 => 0x42, 109 => 0x43, 103 => 0x44, 111 => 0x45, 105 => 0x46, 107 => 0x47,
        113 => 0x48, 114 => 0x49, 115 => 0x4a, 116 => 0x4b, 117 => 0x4c, 119 => 0x4d,
        121 => 0x4e, 124 => 0x4f, 123 => 0x50, 125 => 0x51, 126 => 0x52,
        59 => 0xe0, 56 => 0xe1, 58 => 0xe2, 55 => 0xe3, 62 => 0xe4, 60 => 0xe5,
        61 => 0xe6, 54 => 0xe7,
        _ => return None,
    })
}

fn hid_to_mac_keycode(usage: u16) -> Option<u16> {
    (0..=127).find(|code| mac_keycode_to_hid(*code) == Some(usage))
}

#[cfg(test)]
mod tests {
    use super::{
        CaptureContext, EVENT_DOCK_SWIPE, EVENT_GESTURE, EVENT_NAVIGATION_SWIPE, EVENT_SCROLL,
        MacBackend, MacState, NativeEvent, mac_mouse_button, scroll_source, suppress_local_gesture,
    };
    use crate::input::platform::{InputBackend, MotionAccumulator, ScrollSource};
    use crate::input::{Hotkey, InputMode, ModifierMask};
    use std::collections::BTreeSet;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[test]
    fn protocol_buttons_map_to_quartz_button_numbers() {
        assert_eq!(mac_mouse_button(1), 0);
        assert_eq!(mac_mouse_button(2), 2);
        assert_eq!(mac_mouse_button(3), 1);
        assert_eq!(mac_mouse_button(4), 3);
    }

    #[test]
    fn active_capture_suppresses_gestures_but_not_scroll_events() {
        assert!(suppress_local_gesture(EVENT_GESTURE, true));
        assert!(suppress_local_gesture(EVENT_DOCK_SWIPE, true));
        assert!(suppress_local_gesture(EVENT_NAVIGATION_SWIPE, true));
        assert!(!suppress_local_gesture(EVENT_GESTURE, false));
        assert!(!suppress_local_gesture(EVENT_DOCK_SWIPE, false));
        assert!(!suppress_local_gesture(EVENT_NAVIGATION_SWIPE, false));
        assert!(!suppress_local_gesture(EVENT_SCROLL, true));
    }

    #[test]
    fn continuous_scroll_events_are_classified_as_trackpad_input() {
        assert_eq!(scroll_source(0), ScrollSource::MouseWheel);
        assert_eq!(scroll_source(1), ScrollSource::Trackpad);
    }

    #[test]
    fn snapshot_releases_pressed_lock_before_reading_modifiers() {
        let (events, _receiver) = mpsc::channel::<NativeEvent>(1);
        let state = Arc::new(MacState {
            context: CaptureContext {
                mode: InputMode::Send,
                hotkey: Hotkey::DEFAULT.parse().unwrap(),
                events,
                motion: Arc::new(MotionAccumulator::default()),
                capture_active: Arc::new(AtomicBool::new(false)),
                overflowed: Arc::new(AtomicBool::new(false)),
                failed: Arc::new(AtomicBool::new(false)),
            },
            physical_pressed: Mutex::new(BTreeSet::from([0xe0])),
            physical_buttons: Mutex::new(BTreeSet::from([1])),
            injected_pressed: Mutex::new(BTreeSet::new()),
            injected_buttons: Mutex::new(BTreeSet::new()),
            injected_cursor: Mutex::new(None),
            keyboard_capture: AtomicBool::new(false),
            tap: Mutex::new(None),
            run_loop: Mutex::new(None),
        });
        let backend = MacBackend { state };
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::spawn(move || {
            result_tx.send(backend.snapshot()).unwrap();
        });
        let snapshot = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("snapshot 不应因重复锁定 physical_pressed 而阻塞");
        assert_eq!(snapshot.modifiers, ModifierMask::CTRL);
        assert_eq!(snapshot.usages, vec![0xe0]);
        assert_eq!(snapshot.buttons, vec![1]);
    }
}
