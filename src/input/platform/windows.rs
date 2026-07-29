use super::{CaptureContext, InputBackend, NativeEvent};
use crate::input::{DesktopLayout, DisplayRect, InputMode, KeySnapshot, ModifierMask, Point};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::mem::{size_of, zeroed};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

type HHook = isize;
type HInstance = isize;
type HMonitor = isize;
type Hdc = isize;
type LParam = isize;
type WParam = usize;
type LResult = isize;

const WH_KEYBOARD_LL: i32 = 13;
const WH_MOUSE_LL: i32 = 14;
const HC_ACTION: i32 = 0;
const WM_QUIT: u32 = 0x0012;
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
const LLMHF_INJECTED: u32 = 0x00000001;
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
const WHEEL_DELTA: i32 = 120;
const INPUT_MOUSE: u32 = 0;
const INPUT_KEYBOARD: u32 = 1;
const EVENT_TAG: usize = 0x5359_4e4c_5949_4e50;
const SM_XVIRTUALSCREEN: i32 = 76;
const SM_YVIRTUALSCREEN: i32 = 77;
const SM_CXVIRTUALSCREEN: i32 = 78;
const SM_CYVIRTUALSCREEN: i32 = 79;
const VK_CONTROL: i32 = 0x11;
const VK_SHIFT: i32 = 0x10;
const VK_MENU: i32 = 0x12;
const VK_LWIN: i32 = 0x5b;
const VK_RWIN: i32 = 0x5c;

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
    fn GetCurrentThreadId() -> u32;
    fn GetCursorPos(point: *mut PointRaw) -> i32;
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn ShowCursor(show: i32) -> i32;
    fn SendInput(count: u32, inputs: *const InputRaw, size: i32) -> u32;
    fn GetAsyncKeyState(vk: i32) -> i16;
    fn GetSystemMetrics(index: i32) -> i32;
    fn EnumDisplayMonitors(
        dc: Hdc,
        clip: *const RectRaw,
        callback: Option<unsafe extern "system" fn(HMonitor, Hdc, *mut RectRaw, LParam) -> i32>,
        data: LParam,
    ) -> i32;
    fn GetMonitorInfoW(monitor: HMonitor, info: *mut MonitorInfo) -> i32;
}

struct WindowsState {
    context: CaptureContext,
    physical_pressed: Mutex<BTreeSet<u16>>,
    physical_buttons: Mutex<BTreeSet<u8>>,
    injected_pressed: Mutex<BTreeSet<u16>>,
    injected_buttons: Mutex<BTreeSet<u8>>,
    last_point: Mutex<Option<Point>>,
    thread_id: AtomicU32,
    stop: AtomicBool,
    hooks: Mutex<(HHook, HHook)>,
}

struct WindowsBackend {
    state: Arc<WindowsState>,
}

pub fn ensure_permissions(_mode: InputMode) -> Result<()> {
    Ok(())
}

pub fn start(context: CaptureContext) -> Result<Arc<dyn InputBackend>> {
    let state = Arc::new(WindowsState {
        context,
        physical_pressed: Mutex::new(BTreeSet::new()),
        physical_buttons: Mutex::new(BTreeSet::new()),
        injected_pressed: Mutex::new(BTreeSet::new()),
        injected_buttons: Mutex::new(BTreeSet::new()),
        last_point: Mutex::new(None),
        thread_id: AtomicU32::new(0),
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
    let thread_id = unsafe { GetCurrentThreadId() };
    state.thread_id.store(thread_id, Ordering::Release);
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_callback), 0, 0) };
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_callback), 0, 0) };
    if keyboard == 0 || mouse == 0 {
        if keyboard != 0 {
            unsafe { UnhookWindowsHookEx(keyboard) };
        }
        if mouse != 0 {
            unsafe { UnhookWindowsHookEx(mouse) };
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
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    unsafe {
        UnhookWindowsHookEx(keyboard);
        UnhookWindowsHookEx(mouse);
    }
    *state.hooks.lock().unwrap() = (0, 0);
}

unsafe extern "system" fn keyboard_callback(code: i32, w_param: WParam, l_param: LParam) -> LResult {
    if code < HC_ACTION {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let state = unsafe { &*(l_param as *const KeyboardLl) };
    if state.extra_info == EVENT_TAG
        || state.flags & (LLKHF_INJECTED | LLKHF_LOWER_IL_INJECTED) != 0
    {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let context = unsafe { &*HOOK_STATE.load(Ordering::Acquire) };
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
    if context.context.capture_active.load(Ordering::Acquire) {
        context.context.emit_reliable(NativeEvent::Key { usage, modifiers, down, repeat });
        return 1;
    }
    unsafe { CallNextHookEx(0, code, w_param, l_param) }
}

unsafe extern "system" fn mouse_callback(code: i32, w_param: WParam, l_param: LParam) -> LResult {
    if code < HC_ACTION {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let event = unsafe { &*(l_param as *const MouseLl) };
    if event.extra_info == EVENT_TAG || event.flags & LLMHF_INJECTED != 0 {
        return unsafe { CallNextHookEx(0, code, w_param, l_param) };
    }
    let context = unsafe { &*HOOK_STATE.load(Ordering::Acquire) };
    let point = Point { x: event.pt.x, y: event.pt.y };
    let previous = context.last_point.lock().unwrap().replace(point);
    let active = context.context.capture_active.load(Ordering::Acquire);
    match w_param as u32 {
        WM_MOUSEMOVE => {
            if let Some(previous) = previous {
                context.context.motion.add(point.x - previous.x, point.y - previous.y);
            }
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let (button, down) = match w_param as u32 {
                WM_LBUTTONDOWN => (1, true),
                WM_LBUTTONUP => (1, false),
                WM_RBUTTONDOWN => (3, true),
                WM_RBUTTONUP => (3, false),
                WM_MBUTTONDOWN => (2, true),
                WM_MBUTTONUP => (2, false),
                WM_XBUTTONDOWN => (if event.mouse_data >> 16 == 1 { 4 } else { 5 }, true),
                _ => (if event.mouse_data >> 16 == 1 { 4 } else { 5 }, false),
            };
            update_set(&context.physical_buttons, button, down);
            if active {
                context.context.emit_reliable(NativeEvent::Button { button, down });
            }
        }
        WM_MOUSEWHEEL => {
            if active {
                context.context.emit_reliable(NativeEvent::Wheel {
                    x: 0,
                    y: (event.mouse_data >> 16) as i16 as i32 / WHEEL_DELTA,
                });
            }
        }
        WM_MOUSEHWHEEL => {
            if active {
                context.context.emit_reliable(NativeEvent::Wheel {
                    x: (event.mouse_data >> 16) as i16 as i32 / WHEEL_DELTA,
                    y: 0,
                });
            }
        }
        _ => {}
    }
    if active { 1 } else { unsafe { CallNextHookEx(0, code, w_param, l_param) } }
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
        let mut displays = Vec::new();
        let data = &mut displays as *mut Vec<DisplayRect> as isize;
        let ok = unsafe { EnumDisplayMonitors(0, ptr::null(), Some(monitor_callback), data) };
        if ok == 0 || displays.is_empty() {
            let x = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
            let y = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
            let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
            let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
            return DesktopLayout::new(vec![DisplayRect { x, y, width, height }]);
        }
        DesktopLayout::new(displays)
    }

    fn cursor_position(&self) -> Result<Point> {
        let mut point = PointRaw::default();
        if unsafe { GetCursorPos(&mut point) } == 0 {
            bail!("无法读取 Windows 光标位置");
        }
        Ok(Point { x: point.x, y: point.y })
    }

    fn snapshot(&self) -> KeySnapshot {
        KeySnapshot {
            usages: self.state.physical_pressed.lock().unwrap().iter().copied().collect(),
            modifiers: current_modifiers(),
            buttons: self.state.physical_buttons.lock().unwrap().iter().copied().collect(),
        }
    }

    fn set_capture(&self, active: bool) -> Result<()> {
        let previous = self.state.context.capture_active.swap(active, Ordering::AcqRel);
        if previous != active {
            unsafe { ShowCursor(if active { 0 } else { 1 }) };
        }
        Ok(())
    }

    fn warp_cursor(&self, point: Point) -> Result<()> {
        if unsafe { SetCursorPos(point.x, point.y) } == 0 {
            bail!("无法移动 Windows 光标");
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

    fn inject_motion(&self, dx: i32, dy: i32) -> Result<()> {
        send_mouse(dx, dy, 0, MOUSEEVENTF_MOVE)
    }

    fn inject_wheel(&self, x: i32, y: i32) -> Result<()> {
        if y != 0 {
            send_mouse(0, 0, (y * WHEEL_DELTA) as u32, MOUSEEVENTF_WHEEL)?;
        }
        if x != 0 {
            send_mouse(0, 0, (x * WHEEL_DELTA) as u32, MOUSEEVENTF_HWHEEL)?;
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

impl Drop for WindowsBackend {
    fn drop(&mut self) {
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
    let displays = unsafe { &mut *(data as *mut Vec<DisplayRect>) };
    let mut info: MonitorInfo = unsafe { zeroed() };
    info.cb_size = size_of::<MonitorInfo>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info) } != 0 {
        let rect = info.rc_monitor;
        displays.push(DisplayRect {
            x: rect.left,
            y: rect.top,
            width: rect.right - rect.left,
            height: rect.bottom - rect.top,
        });
    }
    1
}

fn current_modifiers() -> ModifierMask {
    let mut bits = 0u8;
    if key_down(VK_CONTROL) { bits |= ModifierMask::CTRL.bits(); }
    if key_down(VK_MENU) { bits |= ModifierMask::ALT.bits(); }
    if key_down(VK_SHIFT) { bits |= ModifierMask::SHIFT.bits(); }
    if key_down(VK_LWIN) || key_down(VK_RWIN) { bits |= ModifierMask::META.bits(); }
    ModifierMask::from_bits(bits)
}

fn key_down(vk: i32) -> bool {
    unsafe { GetAsyncKeyState(vk) } < 0
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
    if unsafe { SendInput(1, &input, size_of::<InputRaw>() as i32) } != 1 {
        bail!("Windows SendInput 键盘注入失败");
    }
    Ok(())
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
    if unsafe { SendInput(1, &input, size_of::<InputRaw>() as i32) } != 1 {
        bail!("Windows SendInput 鼠标注入失败");
    }
    Ok(())
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
        0x70..=0x7b => 0x3a + u16::from(vk - 0x70),
        0x30..=0x39 => if vk == 0x30 { 0x27 } else { 0x1e + u16::from(vk - 0x31) },
        0x41..=0x5a => 0x04 + u16::from(vk - 0x41),
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
