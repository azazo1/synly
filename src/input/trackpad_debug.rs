use anyhow::{Context, Result, bail};
use std::ffi::{c_double, c_void};
use std::fmt::Write;
use std::ptr;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant};

type CGEventRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CFMachPortRef = *mut c_void;
type CFRunLoopSourceRef = *mut c_void;
type CFRunLoopRef = *mut c_void;
type CFStringRef = *const c_void;
type CGEventType = u32;

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
const FIELD_KEY_CODE: u32 = 9;
const FIELD_SCROLL_IS_CONTINUOUS: u32 = 88;
const FIELD_SCROLL_PHASE: u32 = 99;
const FIELD_SCROLL_MOMENTUM_PHASE: u32 = 123;
const ALL_EVENT_MASK: u64 = u64::MAX;
const EVENT_TYPE_COUNT: usize = 64;
const LOG_QUEUE_CAPACITY: usize = 1024;
const SUMMARY_INTERVAL: Duration = Duration::from_secs(1);

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: Option<
            unsafe extern "C" fn(
                CGEventTapProxy,
                CGEventType,
                CGEventRef,
                *mut c_void,
            ) -> CGEventRef,
        >,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetDoubleValueField(event: CGEventRef, field: u32) -> c_double;
    fn CGEventGetFlags(event: CGEventRef) -> u64;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFRunLoopCommonModes: CFStringRef;
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

#[derive(Debug)]
enum LogRecord {
    Event(String),
    Warning(String),
}

struct DebugState {
    tap: CFMachPortRef,
    run_loop: CFRunLoopRef,
    sender: SyncSender<LogRecord>,
    event_counts: [u64; EVENT_TYPE_COUNT],
    dropped_logs: u64,
    motion_x: i64,
    motion_y: i64,
    last_summary: Instant,
    stopping: bool,
}

impl DebugState {
    fn new(sender: SyncSender<LogRecord>) -> Self {
        Self {
            tap: ptr::null_mut(),
            run_loop: ptr::null_mut(),
            sender,
            event_counts: [0; EVENT_TYPE_COUNT],
            dropped_logs: 0,
            motion_x: 0,
            motion_y: 0,
            last_summary: Instant::now(),
            stopping: false,
        }
    }

    fn count(&mut self, event_type: CGEventType) {
        if let Some(count) = self.event_counts.get_mut(event_type as usize) {
            *count += 1;
        }
    }

    fn emit(&mut self, record: LogRecord) {
        if let Err(error) = self.sender.try_send(record)
            && matches!(error, TrySendError::Full(_))
        {
            self.dropped_logs += 1;
        }
    }

    fn observe(&mut self, event_type: CGEventType, event: CGEventRef) {
        self.count(event_type);
        match event_type {
            EVENT_MOUSE_MOVED
            | EVENT_LEFT_DRAGGED
            | EVENT_RIGHT_DRAGGED
            | EVENT_OTHER_DRAGGED => {
                self.motion_x += unsafe {
                    CGEventGetIntegerValueField(event, FIELD_MOUSE_DELTA_X)
                };
                self.motion_y += unsafe {
                    CGEventGetIntegerValueField(event, FIELD_MOUSE_DELTA_Y)
                };
            }
            EVENT_LEFT_DOWN
            | EVENT_LEFT_UP
            | EVENT_RIGHT_DOWN
            | EVENT_RIGHT_UP
            | EVENT_OTHER_DOWN
            | EVENT_OTHER_UP => {
                let button = unsafe { CGEventGetIntegerValueField(event, FIELD_MOUSE_BUTTON) };
                self.emit(LogRecord::Event(format!(
                    "pointer button: type={event_type}, button={button}"
                )));
            }
            EVENT_SCROLL => {
                let x = unsafe { CGEventGetIntegerValueField(event, FIELD_SCROLL_DELTA_X) };
                let y = unsafe { CGEventGetIntegerValueField(event, FIELD_SCROLL_DELTA_Y) };
                let continuous = unsafe {
                    CGEventGetIntegerValueField(event, FIELD_SCROLL_IS_CONTINUOUS)
                };
                let phase = unsafe { CGEventGetIntegerValueField(event, FIELD_SCROLL_PHASE) };
                let momentum = unsafe {
                    CGEventGetIntegerValueField(event, FIELD_SCROLL_MOMENTUM_PHASE)
                };
                self.emit(LogRecord::Event(format!(
                    "scroll: x={x}, y={y}, continuous={continuous}, phase={phase}, momentum={momentum}"
                )));
            }
            EVENT_GESTURE | EVENT_DOCK_SWIPE | EVENT_NAVIGATION_SWIPE => {
                self.emit(LogRecord::Event(format!(
                    "gesture: type={event_type}, fields={}",
                    nonzero_fields(event)
                )));
            }
            _ => {
                self.emit(LogRecord::Event(format!(
                    "raw event: type={event_type}, fields={}",
                    nonzero_fields(event)
                )));
            }
        }
        self.emit_periodic_summary();
    }

    fn emit_periodic_summary(&mut self) {
        if self.last_summary.elapsed() < SUMMARY_INTERVAL {
            return;
        }
        self.emit(LogRecord::Event(format!(
            "summary: events={}, motion_x={}, motion_y={}, dropped_logs={}",
            event_counts_text(&self.event_counts),
            self.motion_x,
            self.motion_y,
            self.dropped_logs
        )));
        self.motion_x = 0;
        self.motion_y = 0;
        self.last_summary = Instant::now();
    }

    fn stop_for_key(&mut self, event_type: CGEventType, event: CGEventRef) {
        if self.stopping {
            return;
        }
        self.count(event_type);
        self.stopping = true;
        let keycode = unsafe { CGEventGetIntegerValueField(event, FIELD_KEY_CODE) };
        let flags = unsafe { CGEventGetFlags(event) };
        self.emit(LogRecord::Event(format!(
            "keyboard exit: type={event_type}, keycode={keycode}, flags=0x{flags:x}"
        )));
        unsafe { CFRunLoopStop(self.run_loop) };
    }
}

pub fn run_trackpad_debug() -> Result<()> {
    if !unsafe { AXIsProcessTrusted() } {
        bail!("trackpad 诊断需要在系统设置中授予终端或 Synly 辅助功能权限")
    }
    if unsafe { IsSecureEventInputEnabled() } {
        bail!("macOS Secure Input 已启用, 无法启动 trackpad 诊断")
    }

    let (log_tx, log_rx) = std::sync::mpsc::sync_channel(LOG_QUEUE_CAPACITY);
    let log_thread = std::thread::Builder::new()
        .name("synly-trackpad-debug-log".to_string())
        .spawn(move || write_logs(log_rx))
        .context("无法启动 trackpad 诊断日志线程")?;
    let mut state = DebugState::new(log_tx);
    let user_info = (&mut state as *mut DebugState).cast::<c_void>();
    let tap = unsafe {
        CGEventTapCreate(
            0,
            0,
            0,
            ALL_EVENT_MASK,
            Some(event_callback),
            user_info,
        )
    };
    if tap.is_null() {
        bail!("无法创建全事件 Quartz event tap, 请检查辅助功能和输入监控权限")
    }
    let source = unsafe { CFMachPortCreateRunLoopSource(ptr::null(), tap, 0) };
    if source.is_null() {
        unsafe { CFRelease(tap) };
        bail!("无法创建 trackpad 诊断 run loop source")
    }

    state.tap = tap;
    state.run_loop = unsafe { CFRunLoopGetCurrent() };
    unsafe {
        CFRunLoopAddSource(state.run_loop, source, kCFRunLoopCommonModes);
        CGEventTapEnable(tap, true);
    }
    tracing::info!(
        "trackpad 全事件捕获已启动, 所有输入将被阻止, 按任意键结束测试"
    );
    unsafe { CFRunLoopRun() };
    unsafe {
        CGEventTapEnable(tap, false);
        CFRelease(source);
        CFRelease(tap);
    }

    let final_counts = event_counts_text(&state.event_counts);
    let dropped_logs = state.dropped_logs;
    drop(state);
    log_thread
        .join()
        .map_err(|_| anyhow::anyhow!("trackpad 诊断日志线程异常退出"))?;
    tracing::info!(events = %final_counts, dropped_logs, "trackpad 全事件捕获已停止");
    Ok(())
}

unsafe extern "C" fn event_callback(
    _proxy: CGEventTapProxy,
    event_type: CGEventType,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef {
    let state = unsafe { &mut *user_info.cast::<DebugState>() };
    if event_type == EVENT_TAP_DISABLED_TIMEOUT || event_type == EVENT_TAP_DISABLED_USER_INPUT {
        state.emit(LogRecord::Warning(format!(
            "event tap disabled: type={event_type}, re-enabling"
        )));
        unsafe { CGEventTapEnable(state.tap, true) };
        return event;
    }
    if is_keyboard_event(event_type) {
        state.stop_for_key(event_type, event);
        return ptr::null_mut();
    }
    state.observe(event_type, event);
    ptr::null_mut()
}

fn is_keyboard_event(event_type: CGEventType) -> bool {
    matches!(event_type, EVENT_KEY_DOWN | EVENT_KEY_UP | EVENT_FLAGS_CHANGED)
}

fn nonzero_fields(event: CGEventRef) -> String {
    let mut fields = String::new();
    for field in 0..=255 {
        let integer = unsafe { CGEventGetIntegerValueField(event, field) };
        let double = unsafe { CGEventGetDoubleValueField(event, field) };
        if integer == 0 && double == 0.0 {
            continue;
        }
        if !fields.is_empty() {
            fields.push_str(", ");
        }
        let _ = write!(fields, "{field}=i:{integer}/f:{double:.6}");
    }
    if fields.is_empty() {
        "none".to_string()
    } else {
        fields
    }
}

fn event_counts_text(counts: &[u64; EVENT_TYPE_COUNT]) -> String {
    let values = counts
        .iter()
        .enumerate()
        .filter(|(_, count)| **count != 0)
        .map(|(event_type, count)| format!("{event_type}:{count}"))
        .collect::<Vec<_>>();
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(",")
    }
}

fn write_logs(receiver: Receiver<LogRecord>) {
    while let Ok(record) = receiver.recv() {
        match record {
            LogRecord::Event(message) => tracing::info!(target: "trackpad_debug", "{message}"),
            LogRecord::Warning(message) => {
                tracing::warn!(target: "trackpad_debug", "{message}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        EVENT_FLAGS_CHANGED, EVENT_KEY_DOWN, EVENT_KEY_UP, EVENT_SCROLL, EVENT_TYPE_COUNT,
        event_counts_text, is_keyboard_event,
    };

    #[test]
    fn any_keyboard_event_stops_the_debug_capture() {
        assert!(is_keyboard_event(EVENT_KEY_DOWN));
        assert!(is_keyboard_event(EVENT_KEY_UP));
        assert!(is_keyboard_event(EVENT_FLAGS_CHANGED));
        assert!(!is_keyboard_event(EVENT_SCROLL));
    }

    #[test]
    fn event_counts_only_include_observed_types() {
        let mut counts = [0; EVENT_TYPE_COUNT];
        counts[22] = 3;
        counts[30] = 1;
        assert_eq!(event_counts_text(&counts), "22:3,30:1");
    }
}
