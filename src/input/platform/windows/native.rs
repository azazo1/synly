use super::super::{CaptureContext, InputBackend, NativeEvent, ScrollSource};
use super::cursor_capture::{
    CapturePhase, CursorCaptureTracker, CursorMove, select_capture_anchor,
};
use crate::input::{DesktopLayout, DisplayRect, KeySnapshot, ModifierMask, Point};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicPtr, AtomicU8, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

type HHook = isize;
type HInstance = isize;
type HCursor = isize;
type HWnd = isize;
type HMonitor = isize;
type Hdc = isize;
type LParam = isize;
type WParam = usize;
type LResult = isize;

const WH_KEYBOARD_LL: i32 = 13;
const WH_MOUSE_LL: i32 = 14;
const MAPVK_VSC_TO_VK: u32 = 1;
const MAPVK_VSC_TO_VK_EX: u32 = 3;
const HC_ACTION: i32 = 0;
const WM_QUIT: u32 = 0x0012;
const WM_DISPLAYCHANGE: u32 = 0x007e;
const WM_SYNLY_CAPTURE: u32 = 0x8001;
const WM_SYNLY_WARP: u32 = 0x8002;
const WM_SYNLY_MOUSE_MOVE: u32 = 0x8003;
const WM_SYNLY_PRE_WARP: u32 = 0x8004;
const WM_SYNLY_POST_WARP: u32 = 0x8005;
const WM_SYNLY_MOUSE_BUTTON: u32 = 0x8006;
const WM_SYNLY_MOUSE_WHEEL: u32 = 0x8007;
const WM_SYNLY_MOUSE_TRACKPAD_WHEEL: u32 = 0x8008;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_XBUTTONDOWN: u32 = 0x020b;
const WM_XBUTTONUP: u32 = 0x020c;
const WM_MOUSEWHEEL: u32 = 0x020a;
const WM_MOUSEHWHEEL: u32 = 0x020e;
const LLKHF_INJECTED: u32 = 0x10;
const LLKHF_LOWER_IL_INJECTED: u32 = 0x02;
const LLMHF_INJECTED: u32 = 0x0001;
const LLMHF_LOWER_IL_INJECTED: u32 = 0x0002;
const MOUSEEVENTF_FROMTOUCH: usize = 0xff51_5700;
const MOUSEEVENTF_MASK: usize = 0xffff_ff00;
const KEYEVENTF_EXTENDEDKEY: u32 = 0x0001;
const KEYEVENTF_KEYUP: u32 = 0x0002;
const KEYEVENTF_SCANCODE: u32 = 0x0008;
const MOUSEEVENTF_MOVE: u32 = 0x0001;
const MOUSEEVENTF_LEFTDOWN: u32 = 0x0002;
const MOUSEEVENTF_LEFTUP: u32 = 0x0004;
const MOUSEEVENTF_RIGHTDOWN: u32 = 0x0008;
const MOUSEEVENTF_RIGHTUP: u32 = 0x0010;
const MOUSEEVENTF_MIDDLEDOWN: u32 = 0x0020;
const MOUSEEVENTF_MIDDLEUP: u32 = 0x0040;
const MOUSEEVENTF_XDOWN: u32 = 0x0080;
const MOUSEEVENTF_XUP: u32 = 0x0100;
const MOUSEEVENTF_WHEEL: u32 = 0x0800;
const MOUSEEVENTF_HWHEEL: u32 = 0x1000;
const MOUSEEVENTF_VIRTUALDESK: u32 = 0x4000;
const MOUSEEVENTF_ABSOLUTE: u32 = 0x8000;
const WHEEL_DELTA: i32 = 120;
const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;
const EVENT_TAG: usize = 0x5359_4e4c_5949_4e50;
const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;
const SM_CXCURSOR: i32 = 13;
const SM_CYCURSOR: i32 = 14;
const CURSOR_SHOWING: u32 = 0x0001;
const VK_CONTROL: i32 = 0x11;
const VK_SHIFT: i32 = 0x10;
const VK_MENU: i32 = 0x12;
const VK_LWIN: i32 = 0x5b;
const VK_RWIN: i32 = 0x5c;
const CS_DBLCLKS: u32 = 0x0008;
const CS_NOCLOSE: u32 = 0x0200;
const WS_EX_TRANSPARENT: u32 = 0x00000020;
const WS_EX_TOOLWINDOW: u32 = 0x00000080;
const WS_POPUP: u32 = 0x80000000;
const HWND_TOP: HWnd = 0;
const HWND_BOTTOM: HWnd = 1;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOACTIVATE: u32 = 0x0010;
const SWP_SHOWWINDOW: u32 = 0x0040;
const SWP_HIDEWINDOW: u32 = 0x0080;
const SMTO_BLOCK: u32 = 0x0001;
const SMTO_ABORTIFHUNG: u32 = 0x0002;
const CAPTURE_MESSAGE_TIMEOUT_MS: u32 = 1_000;
const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;
const MONITORINFOF_PRIMARY: u32 = 0x00000001;
const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PointRaw {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RectRaw {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MonitorInfo {
    cb_size: u32,
    rc_monitor: RectRaw,
    rc_work: RectRaw,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CursorInfo {
    cb_size: u32,
    flags: u32,
    cursor: HCursor,
    point: PointRaw,
}

#[derive(Clone, Copy)]
struct MonitorDisplay {
    rect: DisplayRect,
    primary: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyboardLl {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MouseLl {
    pt: PointRaw,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct MouseInput {
    dx: i32,
    dy: i32,
    mouse_data: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyboardInput {
    vk: u16,
    scan: u16,
    flags: u32,
    time: u32,
    extra_info: usize,
}

#[repr(C)]
union InputUnion {
    mouse: MouseInput,
    keyboard: KeyboardInput,
}

#[repr(C)]
struct InputRaw {
    input_type: u32,
    data: InputUnion,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Msg {
    hwnd: isize,
    message: u32,
    w_param: usize,
    l_param: isize,
    time: u32,
    point: PointRaw,
    private: u32,
}

#[repr(C)]
struct WindowClassExW {
    cb_size: u32,
    style: u32,
    window_proc: Option<unsafe extern "system" fn(HWnd, u32, WParam, LParam) -> LResult>,
    cls_extra: i32,
    window_extra: i32,
    instance: HInstance,
    icon: isize,
    cursor: HCursor,
    background: isize,
    menu_name: *const u16,
    class_name: *const u16,
    small_icon: isize,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowsHookExW(
        id_hook: i32,
        callback: Option<unsafe extern "system" fn(i32, WParam, LParam) -> LResult>,
        module: HInstance,
        thread_id: u32,
    ) -> HHook;
    fn UnhookWindowsHookEx(hook: HHook) -> i32;
    fn CallNextHookEx(hook: HHook, code: i32, w_param: WParam, l_param: LParam) -> LResult;
    fn GetMessageW(message: *mut Msg, window: isize, min: u32, max: u32) -> i32;
    fn TranslateMessage(message: *const Msg) -> i32;
    fn DispatchMessageW(message: *const Msg) -> LResult;
    fn PostThreadMessageW(thread_id: u32, message: u32, w_param: WParam, l_param: LParam) -> i32;
    fn SendMessageTimeoutW(
        window: HWnd,
        message: u32,
        w_param: WParam,
        l_param: LParam,
        flags: u32,
        timeout_ms: u32,
        result: *mut usize,
    ) -> LResult;
    fn GetCurrentThreadId() -> u32;
    fn GetCursorPos(point: *mut PointRaw) -> i32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn ShowCursor(show: i32) -> i32;
    fn CreateCursor(
        instance: HInstance,
        x_hot_spot: i32,
        y_hot_spot: i32,
        width: i32,
        height: i32,
        and_plane: *const c_void,
        xor_plane: *const c_void,
    ) -> HCursor;
    fn DestroyCursor(cursor: HCursor) -> i32;
    fn RegisterClassExW(class: *const WindowClassExW) -> u16;
    fn UnregisterClassW(class_name: *const u16, instance: HInstance) -> i32;
    fn CreateWindowExW(
        extended_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: HWnd,
        menu: isize,
        instance: HInstance,
        parameter: *const c_void,
    ) -> HWnd;
    fn DestroyWindow(window: HWnd) -> i32;
    fn DefWindowProcW(window: HWnd, message: u32, w_param: WParam, l_param: LParam) -> LResult;
    fn SetWindowPos(
        window: HWnd,
        insert_after: HWnd,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> i32;
    fn SetThreadDpiAwarenessContext(context: isize) -> isize;
    fn SendInput(count: u32, inputs: *const InputRaw, size: i32) -> u32;
    fn GetAsyncKeyState(vk: i32) -> i16;
    fn MapVirtualKeyW(code: u32, map_type: u32) -> u32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn EnumDisplayMonitors(
        dc: Hdc,
        clip: *const RectRaw,
        callback: Option<unsafe extern "system" fn(HMonitor, Hdc, *mut RectRaw, LParam) -> i32>,
        data: LParam,
    ) -> i32;
    fn GetMonitorInfoW(monitor: HMonitor, info: *mut MonitorInfo) -> i32;
    fn GetForegroundWindow() -> HWnd;
    fn GetWindowThreadProcessId(window: HWnd, process_id: *mut u32) -> u32;
    fn GetClipCursor(rect: *mut RectRaw) -> i32;
    fn GetCursorInfo(info: *mut CursorInfo) -> i32;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetModuleHandleW(module_name: *const u16) -> HInstance;
    fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> isize;
    fn CloseHandle(handle: isize) -> i32;
    fn QueryFullProcessImageNameW(
        process: isize,
        flags: u32,
        buffer: *mut u16,
        size: *mut u32,
    ) -> i32;
}

struct WindowsState {
    context: CaptureContext,
    layout: Mutex<Option<DesktopLayout>>,
    primary: Mutex<Option<DisplayRect>>,
    cursor: Mutex<Option<CursorCaptureTracker>>,
    physical_pressed: Mutex<BTreeSet<u16>>,
    physical_buttons: Mutex<BTreeSet<u8>>,
    injected_pressed: Mutex<BTreeSet<u16>>,
    injected_buttons: Mutex<BTreeSet<u8>>,
    capture_phase: AtomicU8,
    keyboard_capture: AtomicBool,
    thread_id: AtomicU32,
    hider_window: AtomicIsize,
    cursor_hidden: AtomicBool,
    stop: AtomicBool,
    hooks: Mutex<(HHook, HHook)>,
}

struct WindowsBackend {
    state: Arc<WindowsState>,
}

impl WindowsBackend {
    /// 主显示器矩形, 用于安全桌面期间的光标钳制; 未知时返回 None.
    fn primary_rect(&self) -> Option<DisplayRect> {
        self.state
            .primary
            .lock()
            .ok()
            .and_then(|primary| *primary)
    }
}

pub(super) fn start(context: CaptureContext) -> Result<Arc<dyn InputBackend>> {
    let state = Arc::new(WindowsState {
        context,
        layout: Mutex::new(None),
        primary: Mutex::new(None),
        cursor: Mutex::new(None),
        physical_pressed: Mutex::new(BTreeSet::new()),
        physical_buttons: Mutex::new(BTreeSet::new()),
        injected_pressed: Mutex::new(BTreeSet::new()),
        injected_buttons: Mutex::new(BTreeSet::new()),
        capture_phase: AtomicU8::new(CapturePhase::Observing as u8),
        keyboard_capture: AtomicBool::new(false),
        thread_id: AtomicU32::new(0),
        hider_window: AtomicIsize::new(0),
        cursor_hidden: AtomicBool::new(false),
        stop: AtomicBool::new(false),
        hooks: Mutex::new((0, 0)),
    });
    HOOK_STATE.store(Arc::as_ptr(&state) as *mut WindowsState, Ordering::Release);
    let thread_state = Arc::clone(&state);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("synly-input-windows".to_string())
        .spawn(move || run_message_loop(thread_state, ready_tx))
        .context("无法启动 Windows 输入消息线程")?;
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(3))
        .context("等待 Windows 输入钩子启动超时")??;
    Ok(Arc::new(WindowsBackend { state }))
}

fn run_message_loop(state: Arc<WindowsState>, ready: std::sync::mpsc::SyncSender<Result<()>>) {
    let previous_dpi_context = unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
    };
    if previous_dpi_context == 0 {
        tracing::warn!("Windows 输入线程无法启用 per-monitor DPI awareness");
    }
    let (layout, anchor, primary) = match collect_display_snapshot() {
        Ok(snapshot) => snapshot,
        Err(error) => {
            if previous_dpi_context != 0 {
                unsafe { SetThreadDpiAwarenessContext(previous_dpi_context) };
            }
            let _ = ready.send(Err(error.context("无法读取 Windows 显示器布局")));
            return;
        }
    };
    let initial_point = read_cursor_position().ok();
    *state.layout.lock().unwrap() = Some(layout.clone());
    *state.primary.lock().unwrap() = primary;
    *state.cursor.lock().unwrap() = Some(CursorCaptureTracker::new(
        layout.clone(),
        anchor,
        initial_point,
    ));
    tracing::info!(
        displays = ?layout.displays,
        anchor = ?anchor,
        initial_point = ?initial_point,
        "Windows 光标捕获布局已初始化"
    );
    let thread_id = unsafe { GetCurrentThreadId() };
    state.thread_id.store(thread_id, Ordering::Release);
    let hider = match create_cursor_hider(thread_id) {
        Ok(hider) => hider,
        Err(error) => {
            if previous_dpi_context != 0 {
                unsafe { SetThreadDpiAwarenessContext(previous_dpi_context) };
            }
            let _ = ready.send(Err(error));
            return;
        }
    };
    state.hider_window.store(hider.window, Ordering::Release);
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_callback), 0, 0) };
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_callback), 0, 0) };
    if keyboard == 0 || mouse == 0 {
        if keyboard != 0 {
            unsafe { UnhookWindowsHookEx(keyboard) };
        }
        if mouse != 0 {
            unsafe { UnhookWindowsHookEx(mouse) };
        }
        state.hider_window.store(0, Ordering::Release);
        destroy_cursor_hider(hider);
        if previous_dpi_context != 0 {
            unsafe { SetThreadDpiAwarenessContext(previous_dpi_context) };
        }
        let _ = ready.send(Err(anyhow::anyhow!(
            "无法安装 Windows 全局输入钩子, 请检查当前进程完整性级别"
        )));
        return;
    }
    *state.hooks.lock().unwrap() = (keyboard, mouse);
    let _ = ready.send(Ok(()));
    let mut message = Msg::default();
    while !state.stop.load(Ordering::Acquire) {
        let result = unsafe { GetMessageW(&mut message, 0, 0, 0) };
        if result <= 0 {
            break;
        }
        if handle_thread_message(&state, &message) {
            continue;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    sync_cursor_capture(&state, false);
    unsafe {
        UnhookWindowsHookEx(keyboard);
        UnhookWindowsHookEx(mouse);
    }
    *state.hooks.lock().unwrap() = (0, 0);
    HOOK_STATE.store(ptr::null_mut(), Ordering::Release);
    state.hider_window.store(0, Ordering::Release);
    destroy_cursor_hider(hider);
    if previous_dpi_context != 0 {
        unsafe { SetThreadDpiAwarenessContext(previous_dpi_context) };
    }
}

struct CursorHider {
    window: HWnd,
    cursor: HCursor,
    instance: HInstance,
    class_name: Vec<u16>,
}

fn create_cursor_hider(thread_id: u32) -> Result<CursorHider> {
    let instance = unsafe { GetModuleHandleW(ptr::null()) };
    if instance == 0 {
        bail!("无法读取 Windows 进程模块句柄");
    }
    let cursor = create_blank_cursor(instance)?;
    let class_name = format!("SynlyInputCapture-{thread_id}\0")
        .encode_utf16()
        .collect::<Vec<_>>();
    let class = WindowClassExW {
        cb_size: size_of::<WindowClassExW>() as u32,
        style: CS_DBLCLKS | CS_NOCLOSE,
        window_proc: Some(cursor_hider_window_proc),
        cls_extra: 0,
        window_extra: 0,
        instance,
        icon: 0,
        cursor,
        background: 0,
        menu_name: ptr::null(),
        class_name: class_name.as_ptr(),
        small_icon: 0,
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        unsafe { DestroyCursor(cursor) };
        bail!("无法注册 Windows 光标隐藏窗口类");
    }
    let window_name = "SynlyInputCapture\0".encode_utf16().collect::<Vec<_>>();
    let window = unsafe {
        CreateWindowExW(
            WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW,
            class_name.as_ptr(),
            window_name.as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            0,
            0,
            instance,
            ptr::null(),
        )
    };
    if window == 0 {
        unsafe {
            UnregisterClassW(class_name.as_ptr(), instance);
            DestroyCursor(cursor);
        }
        bail!("无法创建 Windows 光标隐藏窗口");
    }
    Ok(CursorHider { window, cursor, instance, class_name })
}

fn create_blank_cursor(instance: HInstance) -> Result<HCursor> {
    let width = unsafe { GetSystemMetrics(SM_CXCURSOR) }.max(1);
    let height = unsafe { GetSystemMetrics(SM_CYCURSOR) }.max(1);
    let row_bytes = (width as usize).div_ceil(32) * 4;
    let plane_size = row_bytes.saturating_mul(height as usize);
    let and_plane = vec![0xffu8; plane_size];
    let xor_plane = vec![0u8; plane_size];
    let cursor = unsafe {
        CreateCursor(
            instance,
            0,
            0,
            width,
            height,
            and_plane.as_ptr().cast(),
            xor_plane.as_ptr().cast(),
        )
    };
    if cursor == 0 {
        bail!("无法创建 Windows 透明光标");
    }
    Ok(cursor)
}

fn destroy_cursor_hider(hider: CursorHider) {
    unsafe {
        DestroyWindow(hider.window);
        UnregisterClassW(hider.class_name.as_ptr(), hider.instance);
        DestroyCursor(hider.cursor);
    }
}

unsafe extern "system" fn cursor_hider_window_proc(
    window: HWnd,
    message: u32,
    w_param: WParam,
    l_param: LParam,
) -> LResult {
    let state = HOOK_STATE.load(Ordering::Acquire);
    if !state.is_null() {
        let state = unsafe { &*state };
        match message {
            WM_SYNLY_CAPTURE => {
                return isize::from(sync_cursor_capture(state, w_param != 0));
            }
            WM_SYNLY_WARP => {
                let target = decode_point(w_param, l_param);
                return isize::from(warp_cursor_in_message_thread(state, target));
            }
            WM_DISPLAYCHANGE => {
                if let Err(error) = refresh_display_layout(state) {
                    fail_capture(state, format!("Windows 显示器布局刷新失败: {error:#}"));
                }
                return 0;
            }
            _ => {}
        }
    }
    unsafe { DefWindowProcW(window, message, w_param, l_param) }
}

unsafe extern "system" fn keyboard_callback(code: i32, w_param: WParam, l_param: LParam) -> LResult {
    if code < HC_ACTION {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let state = unsafe { &*(l_param as *const KeyboardLl) };
    if state.extra_info == EVENT_TAG {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let context = HOOK_STATE.load(Ordering::Acquire);
    if context.is_null() {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let context = unsafe { &*context };
    if context.context.filter_app_events
        && state.flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) != 0
    {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let usage = windows_scan_to_hid(state.scan_code as u16, state.vk_code as u16);
    let Some(usage) = usage else {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    };
    let modifiers = current_modifiers();
    let down = matches!(w_param as u32, 0x0100 | 0x0104);
    let repeat = down && context.physical_pressed.lock().unwrap().contains(&usage);
    update_set(&context.physical_pressed, usage, down);
    if context.context.hotkey.matches(usage, modifiers) {
        if down && !repeat {
            context.context.emit_reliable(NativeEvent::Emergency);
        }
        return 1;
    }
    let phase = capture_phase(context);
    let capturing =
        phase == CapturePhase::Relaying || context.keyboard_capture.load(Ordering::Acquire);
    let suppressing = phase.suppresses_local_input();
    if capturing {
        context.context.emit_reliable(NativeEvent::Key { usage, modifiers, down, repeat });
    }
    if suppressing && !usage_is_modifier(usage) {
        return 1;
    }
    unsafe { CallNextHookEx(0, code, w_param, l_param) }
}

unsafe extern "system" fn mouse_callback(code: i32, w_param: WParam, l_param: LParam) -> LResult {
    if code < HC_ACTION {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let event = unsafe { &*(l_param as *const MouseLl) };
    if event.extra_info == EVENT_TAG {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let context = HOOK_STATE.load(Ordering::Acquire);
    if context.is_null() {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let context = unsafe { &*context };
    if context.context.filter_app_events
        && event.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0
    {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let suppressing = capture_phase(context).suppresses_local_input();
    let thread_id = context.thread_id.load(Ordering::Acquire);
    let mut posted = true;
    match w_param as u32 {
        WM_MOUSEMOVE => {
            posted = post_thread_point(thread_id, WM_SYNLY_MOUSE_MOVE, Point {
                x: event.pt.x,
                y: event.pt.y,
            });
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let (button, down): (u8, bool) = match w_param as u32 {
                WM_LBUTTONDOWN => (1, true),
                WM_LBUTTONUP => (1, false),
                WM_RBUTTONDOWN => (3, true),
                WM_RBUTTONUP => (3, false),
                WM_MBUTTONDOWN => (2, true),
                WM_MBUTTONUP => (2, false),
                WM_XBUTTONDOWN => (if event.mouse_data >> 16 == 1 { 4 } else { 5 }, true),
                _ => (if event.mouse_data >> 16 == 1 { 4 } else { 5 }, false),
            };
            posted = unsafe {
                PostThreadMessageW(
                    thread_id,
                    WM_SYNLY_MOUSE_BUTTON,
                    usize::from(button),
                    isize::from(down),
                )
            } != 0;
        }
        WM_MOUSEWHEEL => {
            let y = (event.mouse_data >> 16) as i16 as i32 / WHEEL_DELTA;
            let message = wheel_message(scroll_source_from_extra_info(event.extra_info));
            posted = post_thread_point(thread_id, message, Point { x: 0, y });
        }
        WM_MOUSEHWHEEL => {
            let x = (event.mouse_data >> 16) as i16 as i32 / WHEEL_DELTA;
            let message = wheel_message(scroll_source_from_extra_info(event.extra_info));
            posted = post_thread_point(thread_id, message, Point { x, y: 0 });
        }
        _ => {}
    }
    if !posted {
        fail_capture(context, "Windows 输入消息线程无法接收鼠标事件".to_string());
    }
    if suppressing {
        1
    } else {
        unsafe { CallNextHookEx(0, code, w_param, l_param) }
    }
}

fn scroll_source_from_extra_info(extra_info: usize) -> ScrollSource {
    if extra_info & MOUSEEVENTF_MASK == MOUSEEVENTF_FROMTOUCH {
        ScrollSource::Trackpad
    } else {
        ScrollSource::MouseWheel
    }
}

fn wheel_message(source: ScrollSource) -> u32 {
    match source {
        ScrollSource::MouseWheel => WM_SYNLY_MOUSE_WHEEL,
        ScrollSource::Trackpad => WM_SYNLY_MOUSE_TRACKPAD_WHEEL,
    }
}

fn capture_phase(state: &WindowsState) -> CapturePhase {
    CapturePhase::from_raw(state.capture_phase.load(Ordering::Acquire))
}

fn post_thread_point(thread_id: u32, message: u32, point: Point) -> bool {
    thread_id != 0
        && unsafe {
            PostThreadMessageW(
                thread_id,
                message,
                encode_coordinate(point.x),
                encode_coordinate(point.y) as isize,
            )
        } != 0
}

fn encode_coordinate(value: i32) -> usize {
    value as u32 as usize
}

fn decode_coordinate(value: usize) -> i32 {
    value as u32 as i32
}

fn decode_point(w_param: WParam, l_param: LParam) -> Point {
    Point {
        x: decode_coordinate(w_param),
        y: decode_coordinate(l_param as usize),
    }
}

fn handle_thread_message(state: &WindowsState, message: &Msg) -> bool {
    match message.message {
        WM_SYNLY_MOUSE_MOVE => {
            handle_mouse_move(state, decode_point(message.w_param, message.l_param));
            true
        }
        WM_SYNLY_MOUSE_BUTTON => {
            handle_mouse_button(state, message.w_param as u8, message.l_param != 0);
            true
        }
        WM_SYNLY_MOUSE_WHEEL => {
            handle_mouse_wheel(
                state,
                decode_point(message.w_param, message.l_param),
                ScrollSource::MouseWheel,
            );
            true
        }
        WM_SYNLY_MOUSE_TRACKPAD_WHEEL => {
            handle_mouse_wheel(
                state,
                decode_point(message.w_param, message.l_param),
                ScrollSource::Trackpad,
            );
            true
        }
        WM_SYNLY_PRE_WARP => {
            handle_pre_warp(state, decode_point(message.w_param, message.l_param));
            true
        }
        WM_SYNLY_POST_WARP => {
            tracing::warn!("Windows 光标捕获收到未匹配的 POST_WARP");
            true
        }
        _ => false,
    }
}

fn handle_mouse_move(state: &WindowsState, point: Point) {
    let phase = capture_phase(state);
    let (movement, anchor) = {
        let mut cursor = state.cursor.lock().unwrap();
        let Some(cursor) = cursor.as_mut() else {
            return;
        };
        (cursor.handle_move(phase, point), cursor.anchor())
    };
    match movement {
        CursorMove::Observed { point, dx, dy } => {
            state.context.motion.add_at(dx, dy, point);
        }
        CursorMove::Relayed { dx, dy, bogus } => {
            if dx == 0 && dy == 0 {
                return;
            }
            if bogus {
                tracing::debug!(dx, dy, anchor = ?anchor, "Windows 光标捕获已丢弃异常回正位移");
            } else {
                state.context.motion.add(dx, dy);
            }
            if !warp_cursor_in_message_thread(state, anchor) {
                let _ = sync_cursor_capture(state, false);
                fail_capture(state, "Windows 捕获期间无法回正本机光标".to_string());
            }
        }
        CursorMove::Ignored => {}
    }
}

fn handle_mouse_button(state: &WindowsState, button: u8, down: bool) {
    update_set(&state.physical_buttons, button, down);
    if capture_phase(state) == CapturePhase::Relaying {
        state.context.emit_reliable(NativeEvent::Button { button, down });
    }
}

fn handle_mouse_wheel(state: &WindowsState, delta: Point, source: ScrollSource) {
    if capture_phase(state) == CapturePhase::Relaying {
        state.context.emit_reliable(NativeEvent::Wheel {
            x: delta.x,
            y: delta.y,
            source,
        });
    }
}

fn handle_pre_warp(state: &WindowsState, target: Point) {
    if let Some(cursor) = state.cursor.lock().unwrap().as_mut() {
        cursor.begin_warp(target);
    }
    let mut pending = Msg::default();
    let matched = loop {
        let result = unsafe {
            GetMessageW(
                &mut pending,
                0,
                WM_SYNLY_MOUSE_MOVE,
                WM_SYNLY_POST_WARP,
            )
        };
        if result <= 0 {
            break false;
        }
        if pending.message == WM_SYNLY_POST_WARP {
            break true;
        }
    };
    if let Some(cursor) = state.cursor.lock().unwrap().as_mut() {
        cursor.end_warp(target);
    }
    if !matched {
        fail_capture(state, "Windows 光标 warp marker 未完整匹配".to_string());
    }
}

fn fail_capture(state: &WindowsState, message: String) {
    state.context.failed.store(true, Ordering::Release);
    state.context.emit_reliable(NativeEvent::Failed(message.clone()));
    tracing::error!(error = %message, "Windows 光标捕获失败");
}

static HOOK_STATE: AtomicPtr<WindowsState> = AtomicPtr::new(ptr::null_mut());

fn update_set<T: Ord + Copy>(set: &Mutex<BTreeSet<T>>, value: T, down: bool) {
    let mut set = set.lock().unwrap();
    if down {
        set.insert(value);
    } else {
        set.remove(&value);
    }
}

impl InputBackend for WindowsBackend {
    fn layout(&self) -> Result<DesktopLayout> {
        self.state
            .layout
            .lock()
            .map_err(|_| anyhow::anyhow!("Windows display layout cache poisoned"))?
            .clone()
            .context("Windows display layout cache is empty")
    }

    fn secure_desktop_state(&self) -> (bool, Option<DisplayRect>) {
        (
            super::desktop::input_desktop_is_secure(),
            self.primary_rect(),
        )
    }

    fn cursor_position(&self) -> Result<Point> {
        with_per_monitor_dpi(read_cursor_position)
    }

    fn snapshot(&self) -> KeySnapshot {
        KeySnapshot {
            usages: self.state.physical_pressed.lock().unwrap().iter().copied().collect(),
            modifiers: current_modifiers(),
            buttons: self.state.physical_buttons.lock().unwrap().iter().copied().collect(),
        }
    }

    fn refresh_pressed_state(&self) -> Result<KeySnapshot> {
        refresh_windows_pressed_state(&self.state);
        Ok(self.snapshot())
    }

    fn set_capture(&self, active: bool) -> Result<()> {
        let phase = capture_phase(&self.state);
        if (active && phase == CapturePhase::Relaying)
            || (!active && phase == CapturePhase::Observing)
        {
            return Ok(());
        }
        let window = self.state.hider_window.load(Ordering::Acquire);
        if window == 0 || !send_capture_message(window, active) {
            bail!("无法切换 Windows 光标捕获状态")
        }
        Ok(())
    }

    fn set_keyboard_capture(&self, active: bool) -> Result<()> {
        self.state.keyboard_capture.store(active, Ordering::Release);
        if active {
            tracing::info!("Windows 键盘监听捕获已开启, 光标状态不受影响");
        }
        Ok(())
    }

    fn warp_cursor(&self, point: Point) -> Result<()> {
        let window = self.state.hider_window.load(Ordering::Acquire);
        if window == 0 || !send_warp_message(window, point) {
            bail!("无法移动 Windows 光标")
        }
        Ok(())
    }

    fn inject_key(&self, usage: u16, _modifiers: ModifierMask, down: bool, _repeat: bool) -> Result<()> {
        let (scan, extended) = hid_to_windows_scan(usage)
            .with_context(|| format!("Windows 不支持 USB HID usage 0x{usage:04x}"))?;
        let mut flags = KEYEVENTF_SCANCODE;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        if !down {
            flags |= KEYEVENTF_KEYUP;
        }
        send_keyboard(scan, flags)?;
        update_set(&self.state.injected_pressed, usage, down);
        Ok(())
    }

    fn inject_button(&self, button: u8, down: bool) -> Result<()> {
        let (flags, data) = match button {
            1 => (if down { MOUSEEVENTF_LEFTDOWN } else { MOUSEEVENTF_LEFTUP }, 0),
            2 => (if down { MOUSEEVENTF_MIDDLEDOWN } else { MOUSEEVENTF_MIDDLEUP }, 0),
            3 => (if down { MOUSEEVENTF_RIGHTDOWN } else { MOUSEEVENTF_RIGHTUP }, 0),
            4 | 5 => (if down { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, if button == 4 { 1 } else { 2 }),
            _ => return Ok(()),
        };
        send_mouse(0, 0, data, flags)?;
        update_set(&self.state.injected_buttons, button, down);
        Ok(())
    }

    fn inject_cursor(&self, point: Point) -> Result<()> {
        let layout = self.layout()?;
        let mut bounded = layout.move_within_layout(point, 0, 0);
        if super::desktop::input_desktop_is_secure()
            && let Some(primary) = *self.state.primary.lock().unwrap()
        {
            bounded = Point {
                x: bounded.x.clamp(primary.x, primary.right().saturating_sub(1)),
                y: bounded.y.clamp(primary.y, primary.bottom().saturating_sub(1)),
            };
        }
        if bounded != point {
            tracing::warn!(requested = ?point, bounded = ?bounded, "Windows 远端光标坐标超出显示器布局, 已裁剪");
        }
        with_per_monitor_dpi(|| send_absolute_mouse(bounded))
    }

    fn inject_motion(&self, dx: i32, dy: i32) -> Result<()> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        send_mouse(dx, dy, 0, MOUSEEVENTF_MOVE)
    }

    fn inject_wheel(&self, x: i32, y: i32) -> Result<()> {
        if y != 0 {
            send_mouse(
                0,
                0,
                y.saturating_mul(WHEEL_DELTA) as u32,
                MOUSEEVENTF_WHEEL,
            )?;
        }
        if x != 0 {
            send_mouse(
                0,
                0,
                x.saturating_mul(WHEEL_DELTA) as u32,
                MOUSEEVENTF_HWHEEL,
            )?;
        }
        Ok(())
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

fn with_per_monitor_dpi<T>(operation: impl FnOnce() -> T) -> T {
    let previous = unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
    };
    let result = operation();
    if previous != 0 {
        unsafe { SetThreadDpiAwarenessContext(previous) };
    }
    result
}

fn send_capture_message(window: HWnd, active: bool) -> bool {
    let mut result = 0usize;
    (unsafe {
        SendMessageTimeoutW(
            window,
            WM_SYNLY_CAPTURE,
            usize::from(active),
            0,
            SMTO_BLOCK | SMTO_ABORTIFHUNG,
            CAPTURE_MESSAGE_TIMEOUT_MS,
            &mut result,
        )
    }) != 0
        && result != 0
}

fn send_warp_message(window: HWnd, point: Point) -> bool {
    let mut result = 0usize;
    (unsafe {
        SendMessageTimeoutW(
            window,
            WM_SYNLY_WARP,
            encode_coordinate(point.x),
            encode_coordinate(point.y) as isize,
            SMTO_BLOCK | SMTO_ABORTIFHUNG,
            CAPTURE_MESSAGE_TIMEOUT_MS,
            &mut result,
        )
    }) != 0
        && result != 0
}

fn sync_cursor_capture(state: &WindowsState, active: bool) -> bool {
    let phase = capture_phase(state);
    if (active && phase == CapturePhase::Relaying)
        || (!active && phase == CapturePhase::Observing)
    {
        return true;
    }
    let window = state.hider_window.load(Ordering::Acquire);
    if window == 0 {
        return false;
    }
    let succeeded = if active {
        state.capture_phase.store(CapturePhase::Arming as u8, Ordering::Release);
        let anchor = state
            .cursor
            .lock()
            .unwrap()
            .as_ref()
            .map(CursorCaptureTracker::anchor);
        let Some(anchor) = anchor else {
            state.capture_phase.store(CapturePhase::Observing as u8, Ordering::Release);
            return false;
        };
        let visibility_ok = set_cursor_visibility(false);
        let window_ok = visibility_ok
            && unsafe {
                SetWindowPos(
                    window,
                    HWND_TOP,
                    anchor.x,
                    anchor.y,
                    1,
                    1,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                )
            } != 0;
        let position_ok = window_ok && warp_cursor_in_message_thread(state, anchor);
        if position_ok {
            state.cursor_hidden.store(true, Ordering::Release);
            state.context.capture_active.store(true, Ordering::Release);
            state.capture_phase.store(CapturePhase::Relaying as u8, Ordering::Release);
            true
        } else {
            unsafe {
                SetWindowPos(
                    window,
                    HWND_BOTTOM,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_HIDEWINDOW,
                );
            }
            let _ = set_cursor_visibility(true);
            state.cursor_hidden.store(false, Ordering::Release);
            state.context.capture_active.store(false, Ordering::Release);
            state.capture_phase.store(CapturePhase::Observing as u8, Ordering::Release);
            false
        }
    } else {
        state.capture_phase.store(CapturePhase::Disarming as u8, Ordering::Release);
        let visibility_ok = set_cursor_visibility(true);
        let window_ok = unsafe {
            SetWindowPos(
                window,
                HWND_BOTTOM,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_HIDEWINDOW,
            )
        } != 0;
        if visibility_ok && window_ok {
            state.cursor_hidden.store(false, Ordering::Release);
            state.context.capture_active.store(false, Ordering::Release);
            state.capture_phase.store(CapturePhase::Observing as u8, Ordering::Release);
            true
        } else {
            state.cursor_hidden.store(true, Ordering::Release);
            state.context.capture_active.store(true, Ordering::Release);
            state.capture_phase.store(CapturePhase::Relaying as u8, Ordering::Release);
            false
        }
    };
    if succeeded {
        tracing::info!(active, phase = ?capture_phase(state), "Windows 光标捕获状态已切换");
    } else {
        let message = if active {
            "Windows 无法隐藏并固定本机光标"
        } else {
            "Windows 无法恢复本机光标"
        };
        fail_capture(state, message.to_string());
    }
    succeeded
}

fn warp_cursor_in_message_thread(state: &WindowsState, target: Point) -> bool {
    let thread_id = state.thread_id.load(Ordering::Acquire);
    if !post_thread_point(thread_id, WM_SYNLY_PRE_WARP, target) {
        return false;
    }
    let moved = set_cursor_position_verified(target);
    let marker_posted = unsafe {
        PostThreadMessageW(thread_id, WM_SYNLY_POST_WARP, 0, 0)
    } != 0;
    if marker_posted {
        handle_pre_warp(state, target);
    }
    moved && marker_posted
}

fn set_cursor_position_verified(target: Point) -> bool {
    for attempt in 1..=2 {
        if unsafe { SetCursorPos(target.x, target.y) } != 0
            && read_cursor_position().is_ok_and(|point| point == target)
        {
            return true;
        }
        tracing::debug!(attempt, target = ?target, "Windows 光标 warp 校验失败, 正在重试");
    }
    false
}

fn read_cursor_position() -> Result<Point> {
    let mut point = PointRaw::default();
    if unsafe { GetCursorPos(&mut point) } == 0 {
        bail!("无法读取 Windows 光标位置");
    }
    Ok(Point { x: point.x, y: point.y })
}

fn set_cursor_visibility(visible: bool) -> bool {
    for _ in 0..10 {
        let counter = unsafe { ShowCursor(if visible { 1 } else { 0 }) };
        if (visible && counter >= 0) || (!visible && counter < 0) {
            return true;
        }
    }
    false
}

impl Drop for WindowsBackend {
    fn drop(&mut self) {
        let _ = self.set_capture(false);
        let _ = self.release_all();
        self.state.stop.store(true, Ordering::Release);
        let thread_id = self.state.thread_id.load(Ordering::Acquire);
        if thread_id != 0 {
            unsafe { PostThreadMessageW(thread_id, WM_QUIT, 0, 0) };
        }
    }
}

unsafe extern "system" fn monitor_callback(
    monitor: HMonitor,
    _dc: Hdc,
    _clip: *mut RectRaw,
    data: LParam,
) -> i32 {
    let displays = unsafe { &mut *(data as *mut Vec<MonitorDisplay>) };
    let mut info: MonitorInfo = unsafe { zeroed() };
    info.cb_size = size_of::<MonitorInfo>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
        let rect = info.rc_monitor;
        displays.push(MonitorDisplay {
            rect: DisplayRect {
                x: rect.left,
                y: rect.top,
                width: rect.right - rect.left,
                height: rect.bottom - rect.top,
            },
            primary: info.flags & MONITORINFOF_PRIMARY != 0,
        });
    }
    1
}

fn collect_display_snapshot() -> Result<(DesktopLayout, Point, Option<DisplayRect>)> {
    let mut displays = Vec::new();
    let data = &mut displays as *mut Vec<MonitorDisplay> as isize;
    let ok = unsafe { EnumDisplayMonitors(0, ptr::null(), Some(monitor_callback), data) };
    if ok == 0 || displays.is_empty() {
        let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
        let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
        let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
        let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
        displays.push(MonitorDisplay {
            rect: DisplayRect { x, y, width, height },
            primary: true,
        });
    }
    let rects = displays.iter().map(|display| display.rect).collect::<Vec<_>>();
    let primary = displays
        .iter()
        .find(|display| display.primary)
        .map(|display| display.rect);
    let anchor = select_capture_anchor(primary, &rects)
        .context("Windows 显示器布局缺少光标捕获锚点")?;
    Ok((DesktopLayout::new(rects)?, anchor, primary))
}

fn refresh_display_layout(state: &WindowsState) -> Result<()> {
    let (layout, anchor, primary) = collect_display_snapshot()?;
    let changed = {
        let mut cursor = state.cursor.lock().unwrap();
        let cursor = cursor
            .as_mut()
            .context("Windows 光标捕获状态尚未初始化")?;
        cursor.update_layout(layout.clone(), anchor)
    };
    *state.layout.lock().unwrap() = Some(layout.clone());
    *state.primary.lock().unwrap() = primary;
    if !changed {
        return Ok(());
    }
    tracing::info!(displays = ?layout.displays, anchor = ?anchor, "Windows 显示器布局已刷新");
    if capture_phase(state) == CapturePhase::Relaying
        && !warp_cursor_in_message_thread(state, anchor)
    {
        let _ = sync_cursor_capture(state, false);
        bail!("Windows 显示器变化后无法回正本机光标");
    }
    Ok(())
}

fn current_modifiers() -> ModifierMask {
    let mut bits = 0u8;
    if key_down(VK_CONTROL) { bits |= ModifierMask::CTRL.bits(); }
    if key_down(VK_MENU) { bits |= ModifierMask::ALT.bits(); }
    if key_down(VK_SHIFT) { bits |= ModifierMask::SHIFT.bits(); }
    if key_down(VK_LWIN) || key_down(VK_RWIN) { bits |= ModifierMask::META.bits(); }
    ModifierMask::from_bits(bits)
}

fn usage_is_modifier(usage: u16) -> bool {
    matches!(usage, 0xe0..=0xe7)
}

fn key_down(vk: i32) -> bool {
    (unsafe { GetAsyncKeyState(vk) }) < 0
}

fn mouse_button_down(button: u8) -> bool {
    let vk = match button {
        1 => 0x01,
        2 => 0x04,
        3 => 0x02,
        4 => 0x05,
        5 => 0x06,
        _ => return true,
    };
    key_down(vk)
}

fn hid_to_windows_vk(usage: u16) -> Option<i32> {
    let direct = (0x01..=0xfeu16)
        .find(|vk| vk_to_hid(*vk) == usage)
        .map(i32::from);
    let mapped = hid_to_windows_scan(usage).and_then(|(scan, extended)| {
        let map_type = if extended {
            MAPVK_VSC_TO_VK_EX
        } else {
            MAPVK_VSC_TO_VK
        };
        let vk = unsafe { MapVirtualKeyW(u32::from(scan), map_type) } as i32;
        (vk != 0).then_some(vk)
    });
    direct.or(mapped)
}

fn refresh_windows_pressed_state(state: &WindowsState) {
    let mut pressed = state.physical_pressed.lock().unwrap();
    let mut next = BTreeSet::new();
    for usage in 0x04..=0xe7u16 {
        if let Some(vk) = hid_to_windows_vk(usage)
            && key_down(vk)
        {
            next.insert(usage);
        }
    }
    for usage in pressed.iter().copied() {
        if hid_to_windows_vk(usage).is_none() {
            next.insert(usage);
        }
    }
    *pressed = next;
    drop(pressed);

    let mut buttons = state.physical_buttons.lock().unwrap();
    let mut next_buttons = BTreeSet::new();
    for button in 1..=5u8 {
        if mouse_button_down(button) {
            next_buttons.insert(button);
        }
    }
    *buttons = next_buttons;
}

fn send_keyboard(scan: u16, flags: u32) -> Result<()> {
    let input = InputRaw {
        input_type: INPUT_KEYBOARD,
        data: InputUnion {
            keyboard: KeyboardInput {
                vk: 0,
                scan,
                flags,
                time: 0,
                extra_info: EVENT_TAG,
            },
        },
    };
    send_input_with_desktop_sync(&input)
}

fn send_mouse(dx: i32, dy: i32, mouse_data: u32, flags: u32) -> Result<()> {
    let input = InputRaw {
        input_type: INPUT_MOUSE,
        data: InputUnion {
            mouse: MouseInput {
                dx,
                dy,
                mouse_data,
                flags,
                time: 0,
                extra_info: EVENT_TAG,
            },
        },
    };
    send_input_with_desktop_sync(&input)
}

/// 参考 Sunshine 的 send_input: 先直接注入, 失败时把当前线程切换到输入桌面
/// (UAC/锁屏期间是 Winlogon 安全桌面) 后重试一次.
fn send_input_with_desktop_sync(input: &InputRaw) -> Result<()> {
    if unsafe { SendInput(1, input, size_of::<InputRaw>() as i32) } == 1 {
        return Ok(());
    }
    super::desktop::sync_thread_input_desktop()?;
    if unsafe { SendInput(1, input, size_of::<InputRaw>() as i32) } != 1 {
        bail!("Windows SendInput 注入失败");
    }
    Ok(())
}

fn send_absolute_mouse(point: Point) -> Result<()> {
    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) }.max(1);
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) }.max(1);
    send_mouse(
        normalize_absolute_coordinate(point.x, x, width),
        normalize_absolute_coordinate(point.y, y, height),
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )
}

pub(super) fn foreground_cursor_captured() -> bool {
    let window = unsafe { GetForegroundWindow() };
    if window == 0 || foreground_process_is_explorer(window) {
        return false;
    }
    // ClipCursor 生效时返回的矩形会小于虚拟屏幕, 这是游戏锁定光标的直接信号.
    let mut clip = RectRaw::default();
    if unsafe { GetClipCursor(&mut clip) } != 0 {
        let screen = virtual_screen_rect();
        if clip.left != screen.left
            || clip.top != screen.top
            || clip.right != screen.right
            || clip.bottom != screen.bottom
        {
            return true;
        }
    }
    // 部分游戏只隐藏系统光标并读取相对移动(例如 MC 的 3D 光标), 不裁剪范围.
    let mut info = CursorInfo {
        cb_size: size_of::<CursorInfo>() as u32,
        ..CursorInfo::default()
    };
    (unsafe { GetCursorInfo(&mut info) }) != 0 && info.flags & CURSOR_SHOWING == 0
}

fn virtual_screen_rect() -> RectRaw {
    let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    RectRaw {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    }
}

fn foreground_process_is_explorer(window: HWnd) -> bool {
    let mut process_id = 0u32;
    unsafe { GetWindowThreadProcessId(window, &mut process_id) };
    if process_id == 0 {
        return false;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if handle == 0 {
        return false;
    }
    let mut buffer = [0u16; 260];
    let mut size = buffer.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return false;
    }
    let path = String::from_utf16_lossy(&buffer[..size as usize]);
    path.rsplit('\\')
        .next()
        .is_some_and(|file| file.eq_ignore_ascii_case("explorer.exe"))
}

fn normalize_absolute_coordinate(coordinate: i32, origin: i32, extent: i32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    let maximum = i64::from(extent - 1);
    let relative = i64::from(coordinate)
        .saturating_sub(i64::from(origin))
        .clamp(0, maximum);
    relative
        .saturating_mul(65_535)
        .saturating_add(maximum / 2)
        .checked_div(maximum)
        .unwrap_or(0) as i32
}

fn windows_scan_to_hid(scan: u16, vk: u16) -> Option<u16> {
    let usage = vk_to_hid(vk);
    (usage != 0).then_some(usage).or_else(|| scan_to_hid(scan))
}

fn vk_to_hid(vk: u16) -> u16 {
    match vk {
        0x1b => 0x29,
        0x0d => 0x28,
        0x09 => 0x2b,
        0x20 => 0x2c,
        0x08 => 0x2a,
        0x2e => 0x4c,
        0x25 => 0x50,
        0x26 => 0x52,
        0x27 => 0x4f,
        0x28 => 0x51,
        0x70..=0x7b => 0x3a + (vk - 0x70),
        0x7c..=0x87 => 0x68 + (vk - 0x7c),
        0x30..=0x39 => if vk == 0x30 { 0x27 } else { 0x1e + (vk - 0x31) },
        0x41..=0x5a => 0x04 + (vk - 0x41),
        0xa2 => 0xe0,
        0xa0 => 0xe1,
        0xa4 => 0xe2,
        0x5b => 0xe3,
        0xa3 => 0xe4,
        0xa1 => 0xe5,
        0xa5 => 0xe6,
        0x5c => 0xe7,
        _ => 0,
    }
}

fn scan_to_hid(scan: u16) -> Option<u16> {
    for usage in 0x04..=0xe7 {
        if let Some((candidate, _)) = hid_to_windows_scan(usage)
            && candidate == scan
        {
            return Some(usage);
        }
    }
    None
}

fn hid_to_windows_scan(usage: u16) -> Option<(u16, bool)> {
    let value = match usage {
        0x04 => (0x1e, false),
        0x05 => (0x30, false),
        0x06 => (0x2e, false),
        0x07 => (0x20, false),
        0x08 => (0x12, false),
        0x09 => (0x21, false),
        0x0a => (0x22, false),
        0x0b => (0x23, false),
        0x0c => (0x17, false),
        0x0d => (0x24, false),
        0x0e => (0x25, false),
        0x0f => (0x26, false),
        0x10 => (0x32, false),
        0x11 => (0x31, false),
        0x12 => (0x18, false),
        0x13 => (0x19, false),
        0x14 => (0x10, false),
        0x15 => (0x13, false),
        0x16 => (0x1f, false),
        0x17 => (0x14, false),
        0x18 => (0x16, false),
        0x19 => (0x2f, false),
        0x1a => (0x11, false),
        0x1b => (0x2d, false),
        0x1c => (0x15, false),
        0x1d => (0x2c, false),
        0x1e..=0x27 => (0x02 + usage - 0x1e, false),
        0x28 => (0x1c, false),
        0x29 => (0x01, false),
        0x2a => (0x0e, false),
        0x2b => (0x0f, false),
        0x2c => (0x39, false),
        0x2d => (0x0c, false),
        0x2e => (0x0d, false),
        0x2f => (0x1a, false),
        0x30 => (0x1b, false),
        0x31 => (0x2b, false),
        0x33 => (0x27, false),
        0x34 => (0x28, false),
        0x35 => (0x29, false),
        0x36 => (0x33, false),
        0x37 => (0x34, false),
        0x38 => (0x35, false),
        0x39 => (0x3a, false),
        0x3a..=0x43 => (0x3b + usage - 0x3a, false),
        0x44 => (0x57, false),
        0x45 => (0x58, false),
        0x68 => (0x64, false),
        0x69 => (0x65, false),
        0x6a => (0x66, false),
        0x6b => (0x67, false),
        0x6c => (0x68, false),
        0x6d => (0x69, false),
        0x6e => (0x6a, false),
        0x6f => (0x6b, false),
        0x70 => (0x6c, false),
        0x71 => (0x6d, false),
        0x72 => (0x6e, false),
        0x73 => (0x76, false),
        0x49 => (0x52, true),
        0x4a => (0x47, true),
        0x4b => (0x49, true),
        0x4c => (0x53, true),
        0x4d => (0x4f, true),
        0x4e => (0x51, true),
        0x4f => (0x4d, true),
        0x50 => (0x4b, true),
        0x51 => (0x50, true),
        0x52 => (0x48, true),
        0xe0 => (0x1d, false),
        0xe1 => (0x2a, false),
        0xe2 => (0x38, false),
        0xe3 => (0x5b, true),
        0xe4 => (0x1d, true),
        0xe5 => (0x36, false),
        0xe6 => (0x38, true),
        0xe7 => (0x5c, true),
        _ => return None,
    };
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::super::{
        MOUSEEVENTF_FROMTOUCH, MOUSEEVENTF_MASK, ScrollSource, WM_SYNLY_MOUSE_TRACKPAD_WHEEL,
        WM_SYNLY_MOUSE_WHEEL, allow_native_fallback, hid_to_windows_scan,
        scroll_source_from_extra_info, vk_to_hid, wheel_message,
    };

    #[test]
    fn elevated_receiver_failure_does_not_fallback_to_native_input() {
        assert!(!allow_native_fallback(true));
        assert!(allow_native_fallback(false));
    }

    #[test]
    fn f13_to_f24_map_to_extended_hid_usages_and_scancodes() {
        for vk in 0x7c..=0x87u16 {
            let usage = vk_to_hid(vk);
            assert!((0x68..=0x73).contains(&usage));
            assert!(hid_to_windows_scan(usage).is_some());
        }
        assert_eq!(vk_to_hid(0x7c), 0x68);
        assert_eq!(vk_to_hid(0x87), 0x73);
    }

    #[test]
    fn touch_extra_info_is_classified_as_trackpad_scroll() {
        assert_eq!(
            scroll_source_from_extra_info(MOUSEEVENTF_FROMTOUCH),
            ScrollSource::Trackpad,
        );
        assert_eq!(
            scroll_source_from_extra_info(MOUSEEVENTF_FROMTOUCH | 0x1),
            ScrollSource::Trackpad,
        );
        assert_eq!(scroll_source_from_extra_info(0), ScrollSource::MouseWheel);
        assert_eq!(
            scroll_source_from_extra_info(MOUSEEVENTF_MASK),
            ScrollSource::MouseWheel,
        );
    }

    #[test]
    fn wheel_message_maps_source_to_message_id() {
        assert_eq!(wheel_message(ScrollSource::MouseWheel), WM_SYNLY_MOUSE_WHEEL);
        assert_eq!(
            wheel_message(ScrollSource::Trackpad),
            WM_SYNLY_MOUSE_TRACKPAD_WHEEL,
        );
    }
}
