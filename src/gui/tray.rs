use super::{AppWindow, save_window_state, send_command, show_main_window};
use crate::core::{AppCommand, AppSnapshot, AppSupervisorHandle};
use crate::input::InputMode;
use crate::settings::{AudioMode, ClipboardMode};
use anyhow::{Context, Result};
use slint::{ComponentHandle, Timer, TimerMode};
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const OPEN_ID: &str = "synly.open";
const CONNECT_ID: &str = "synly.connect";
const CLIPBOARD_ID: &str = "synly.clipboard";
const AUDIO_ID: &str = "synly.audio";
const INPUT_ID: &str = "synly.input";
const QUIT_ID: &str = "synly.quit";

#[derive(Clone)]
pub struct TrayController {
    inner: Rc<RefCell<ControllerInner>>,
    start_timer: Rc<Timer>,
}

#[derive(Clone)]
pub struct TrayStateSink(Arc<Mutex<TrayState>>);

struct ControllerInner {
    window: slint::Weak<AppWindow>,
    commands: tokio::sync::mpsc::Sender<AppCommand>,
    snapshots: tokio::sync::watch::Receiver<AppSnapshot>,
    shared_state: Arc<Mutex<TrayState>>,
    tray: Option<NativeTray>,
}

#[derive(Clone, PartialEq)]
struct TrayState {
    status_text: String,
    connected: bool,
    clipboard_enabled: bool,
    audio_enabled: bool,
    input_enabled: bool,
}

struct NativeTray {
    tray_icon: TrayIcon,
    status_item: MenuItem,
    connect_item: MenuItem,
    clipboard_item: CheckMenuItem,
    audio_item: CheckMenuItem,
    input_item: CheckMenuItem,
    state: TrayState,
    _poll_timer: Timer,
}

impl TrayController {
    pub fn new(window: &AppWindow, handle: &AppSupervisorHandle) -> Self {
        let snapshots = handle.snapshots();
        let state = TrayState::from_snapshot(&snapshots.borrow());
        let shared_state = Arc::new(Mutex::new(state));
        Self {
            inner: Rc::new(RefCell::new(ControllerInner {
                window: window.as_weak(),
                commands: handle.commands(),
                snapshots,
                shared_state,
                tray: None,
            })),
            start_timer: Rc::new(Timer::default()),
        }
    }

    pub fn start(&self) {
        let inner = Rc::downgrade(&self.inner);
        self.start_timer.start(
            TimerMode::SingleShot,
            Duration::ZERO,
            move || start_native_tray(inner.clone()),
        );
    }

    pub fn state_sink(&self) -> TrayStateSink {
        TrayStateSink(self.inner.borrow().shared_state.clone())
    }
}

impl TrayStateSink {
    pub fn apply_snapshot(&self, snapshot: &AppSnapshot) {
        if let Ok(mut state) = self.0.lock() {
            *state = TrayState::from_snapshot(snapshot);
        }
    }
}

impl TrayState {
    fn from_snapshot(snapshot: &AppSnapshot) -> Self {
        Self {
            status_text: format!("Synly {}", snapshot.lifecycle.label()),
            connected: snapshot.applied.is_some(),
            clipboard_enabled: snapshot.desired.clipboard_mode != ClipboardMode::Off,
            audio_enabled: snapshot.desired.audio_mode != AudioMode::Off,
            input_enabled: snapshot.desired.input_mode != InputMode::Off,
        }
    }
}

impl NativeTray {
    fn new(inner: Weak<RefCell<ControllerInner>>, state: &TrayState) -> Result<Self> {
        let menu = Menu::new();
        let open_item = MenuItem::with_id(OPEN_ID, "打开 Synly", true, None);
        let status_item = MenuItem::new(&state.status_text, false, None);
        let separator_one = PredefinedMenuItem::separator();
        let connect_item = MenuItem::with_id(
            CONNECT_ID,
            if state.connected { "断开" } else { "开始" },
            true,
            None,
        );
        let clipboard_item = CheckMenuItem::with_id(
            CLIPBOARD_ID,
            "剪贴板",
            true,
            state.clipboard_enabled,
            None,
        );
        let audio_item = CheckMenuItem::with_id(
            AUDIO_ID,
            "音频",
            true,
            state.audio_enabled,
            None,
        );
        let input_item = CheckMenuItem::with_id(
            INPUT_ID,
            "输入",
            true,
            state.input_enabled,
            None,
        );
        let separator_two = PredefinedMenuItem::separator();
        let quit_item = MenuItem::with_id(QUIT_ID, "退出", true, None);
        menu.append_items(&[
            &open_item,
            &status_item,
            &separator_one,
            &connect_item,
            &clipboard_item,
            &audio_item,
            &input_item,
            &separator_two,
            &quit_item,
        ])
        .context("无法创建系统托盘菜单")?;

        let tray_icon = TrayIconBuilder::new()
            .with_icon(make_template_icon()?)
            .with_icon_as_template(true)
            .with_tooltip(&state.status_text)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .build()
            .context("无法创建系统托盘图标")?;

        let poll_timer = Timer::default();
        poll_timer.start(
            TimerMode::Repeated,
            Duration::from_millis(80),
            move || poll_events(&inner),
        );

        Ok(Self {
            tray_icon,
            status_item,
            connect_item,
            clipboard_item,
            audio_item,
            input_item,
            state: state.clone(),
            _poll_timer: poll_timer,
        })
    }

    fn apply_state(&mut self, state: &TrayState) {
        if self.state == *state {
            return;
        }
        self.status_item.set_text(&state.status_text);
        self.connect_item
            .set_text(if state.connected { "断开" } else { "开始" });
        self.clipboard_item.set_checked(state.clipboard_enabled);
        self.audio_item.set_checked(state.audio_enabled);
        self.input_item.set_checked(state.input_enabled);
        if let Err(error) = self.tray_icon.set_tooltip(Some(&state.status_text)) {
            tracing::warn!(error = %error, "无法更新系统托盘提示");
        }
        self.state = state.clone();
    }
}

fn start_native_tray(inner: Weak<RefCell<ControllerInner>>) {
    let Some(controller) = inner.upgrade() else { return };
    if let Err(error) = initialize_platform() {
        tracing::error!(error = %error, "系统托盘平台初始化失败");
        return;
    }
    let shared_state = controller.borrow().shared_state.clone();
    let state = match shared_state.lock() {
        Ok(state) => state.clone(),
        Err(error) => {
            tracing::error!(error = %error, "无法读取系统托盘状态");
            return;
        }
    };
    match NativeTray::new(Rc::downgrade(&controller), &state) {
        Ok(tray) => {
            controller.borrow_mut().tray = Some(tray);
            tracing::info!("系统托盘已启动");
        }
        Err(error) => tracing::error!(error = %error, "系统托盘启动失败"),
    }
}

fn poll_events(inner: &Weak<RefCell<ControllerInner>>) {
    poll_platform_events();
    if let Some(inner) = inner.upgrade() {
        let shared_state = inner.borrow().shared_state.clone();
        if let Ok(state) = shared_state.lock()
            && let Some(tray) = inner.borrow_mut().tray.as_mut()
        {
            tray.apply_state(&state);
        }
    }
    while let Ok(event) = TrayIconEvent::receiver().try_recv() {
        if matches!(
            event,
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
        ) {
            handle_action(inner, OPEN_ID);
        }
    }
    while let Ok(event) = MenuEvent::receiver().try_recv() {
        handle_action(inner, event.id.0.as_str());
    }
}

fn handle_action(inner: &Weak<RefCell<ControllerInner>>, action: &str) {
    let Some(inner) = inner.upgrade() else { return };
    match action {
        OPEN_ID => {
            let window = inner.borrow().window.clone();
            if let Some(window) = window.upgrade() {
                tracing::info!("从系统托盘打开主窗口");
                if let Err(error) = show_main_window(&window) {
                    tracing::warn!(error = %error, "无法从系统托盘打开主窗口");
                }
            }
        }
        CONNECT_ID => {
            let inner = inner.borrow();
            let command = if inner.snapshots.borrow().applied.is_some() {
                AppCommand::Disconnect
            } else {
                AppCommand::Start
            };
            send_command(&inner.commands, command);
        }
        CLIPBOARD_ID => {
            let inner = inner.borrow();
            let mode = if inner.snapshots.borrow().desired.clipboard_mode == ClipboardMode::Off {
                ClipboardMode::Both
            } else {
                ClipboardMode::Off
            };
            send_command(&inner.commands, AppCommand::SetClipboardMode(mode));
        }
        AUDIO_ID => {
            let inner = inner.borrow();
            let mode = if inner.snapshots.borrow().desired.audio_mode == AudioMode::Off {
                AudioMode::Receive
            } else {
                AudioMode::Off
            };
            send_command(&inner.commands, AppCommand::SetAudioMode(mode));
        }
        INPUT_ID => {
            let inner = inner.borrow();
            let mode = if inner.snapshots.borrow().desired.input_mode == InputMode::Off {
                InputMode::Receive
            } else {
                InputMode::Off
            };
            send_command(&inner.commands, AppCommand::SetInputMode(mode));
        }
        QUIT_ID => {
            let inner = inner.borrow();
            if let Some(window) = inner.window.upgrade() {
                save_window_state(&window, &inner.commands);
            }
            send_command(&inner.commands, AppCommand::Shutdown);
            let _ = slint::quit_event_loop();
        }
        _ => {}
    }
}

fn make_template_icon() -> Result<Icon> {
    const SIZE: u32 = 32;
    let mut rgba = vec![0; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let upper_shaft = (4..=21).contains(&x) && (8..=12).contains(&y);
            let upper_head = (18..=27).contains(&x)
                && (y as i32 - 10).abs() * 2 <= (27 - x) as i32;
            let lower_shaft = (10..=27).contains(&x) && (20..=24).contains(&y);
            let lower_head = (4..=13).contains(&x)
                && (y as i32 - 22).abs() * 2 <= (x - 4) as i32;
            if upper_shaft || upper_head || lower_shaft || lower_head {
                let offset = ((y * SIZE + x) * 4) as usize;
                rgba[offset] = 66;
                rgba[offset + 1] = 156;
                rgba[offset + 2] = 118;
                rgba[offset + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).context("无法生成系统托盘图标")
}

#[cfg(target_os = "linux")]
fn initialize_platform() -> Result<()> {
    gtk::init().context("无法初始化 GTK 托盘后端")
}

#[cfg(not(target_os = "linux"))]
fn initialize_platform() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn poll_platform_events() {
    let context = gtk::glib::MainContext::default();
    while context.pending() {
        context.iteration(false);
    }
}

#[cfg(not(target_os = "linux"))]
fn poll_platform_events() {}
