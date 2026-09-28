use super::{AppWindow, guard_callback, save_window_state, send_command, show_main_window};
use crate::core::{AppCommand, AppSnapshot, AppSupervisorHandle};
use crate::input::InputMode;
use crate::settings::{AudioMode, ClipboardMode};
use crate::update::UpdateHandle;
use anyhow::{Context, Result};
use slint::{ComponentHandle, Timer, TimerMode};
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

const OPEN_ID: &str = "synly.open";
const CONNECT_ID: &str = "synly.connect";
const CLIPBOARD_ID_PREFIX: &str = "synly.clipboard";
const AUDIO_ID_PREFIX: &str = "synly.audio";
const INPUT_ID_PREFIX: &str = "synly.input";
const CHECK_UPDATE_ID: &str = "synly.check-update";
const AUTO_CHECK_ID: &str = "synly.auto-check";
const QUIT_ID: &str = "synly.quit";

/// 方向菜单项: (模式, 菜单项 id 后缀, 文案).
/// 文案与主窗口快捷开关的下拉选项保持一致.
const CLIPBOARD_MODES: [(ClipboardMode, &str, &str); 4] = [
    (ClipboardMode::Off, "off", "关闭"),
    (ClipboardMode::Send, "send", "发送"),
    (ClipboardMode::Receive, "receive", "接收"),
    (ClipboardMode::Both, "both", "双向"),
];
const AUDIO_MODES: [(AudioMode, &str, &str); 3] = [
    (AudioMode::Off, "off", "关闭"),
    (AudioMode::Send, "send", "发送"),
    (AudioMode::Receive, "receive", "接收"),
];
const INPUT_MODES: [(InputMode, &str, &str); 3] = [
    (InputMode::Off, "off", "关闭"),
    (InputMode::Send, "send", "发送控制"),
    (InputMode::Receive, "receive", "接受控制"),
];

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
    update: UpdateHandle,
    shared_state: Arc<Mutex<TrayState>>,
    tray: Option<NativeTray>,
}

#[derive(Clone, PartialEq)]
struct TrayState {
    app_title: String,
    status_text: String,
    connected: bool,
    clipboard_mode: ClipboardMode,
    audio_mode: AudioMode,
    input_mode: InputMode,
    auto_check: bool,
}

/// 一组互斥的方向菜单项, 同一时刻只有当前方向被勾选.
struct ModeGroup<T> {
    items: Vec<(T, CheckMenuItem)>,
}

impl<T: Copy + PartialEq> ModeGroup<T> {
    fn build(id_prefix: &str, title: &str, modes: &[(T, &str, &str)]) -> Result<(Submenu, Self)> {
        let submenu = Submenu::new(title, true);
        let mut items = Vec::with_capacity(modes.len());
        for (mode, key, label) in modes {
            let item =
                CheckMenuItem::with_id(format!("{id_prefix}.{key}"), *label, true, false, None);
            submenu
                .append(&item)
                .with_context(|| format!("无法创建托盘菜单项 {title}/{label}"))?;
            items.push((*mode, item));
        }
        Ok((submenu, Self { items }))
    }

    fn sync(&self, current: T) {
        for (mode, item) in &self.items {
            item.set_checked(*mode == current);
        }
    }
}

/// 托盘触发的方向切换.
#[derive(Clone, Copy, PartialEq)]
enum TrayModeChange {
    Clipboard(ClipboardMode),
    Audio(AudioMode),
    Input(InputMode),
}

impl TrayModeChange {
    fn command(self) -> AppCommand {
        match self {
            Self::Clipboard(mode) => AppCommand::SetClipboardMode(mode),
            Self::Audio(mode) => AppCommand::SetAudioMode(mode),
            Self::Input(mode) => AppCommand::SetInputMode(mode),
        }
    }
}

struct NativeTray {
    tray_icon: TrayIcon,
    status_item: MenuItem,
    connect_item: MenuItem,
    clipboard_group: ModeGroup<ClipboardMode>,
    audio_group: ModeGroup<AudioMode>,
    input_group: ModeGroup<InputMode>,
    auto_check_item: CheckMenuItem,
    state: TrayState,
    _poll_timer: Timer,
}

impl TrayController {
    pub fn new(
        window: &AppWindow,
        handle: &AppSupervisorHandle,
        update: UpdateHandle,
        version: &str,
    ) -> Self {
        let snapshots = handle.snapshots();
        let state =
            TrayState::from_snapshot(&snapshots.borrow(), version, update.snapshot().auto_check);
        let shared_state = Arc::new(Mutex::new(state));
        Self {
            inner: Rc::new(RefCell::new(ControllerInner {
                window: window.as_weak(),
                commands: handle.commands(),
                snapshots,
                update,
                shared_state,
                tray: None,
            })),
            start_timer: Rc::new(Timer::default()),
        }
    }

    pub fn start(&self) {
        let inner = Rc::downgrade(&self.inner);
        self.start_timer
            .start(TimerMode::SingleShot, Duration::ZERO, move || {
                guard_callback("tray_start", || start_native_tray(inner.clone()))
            });
    }

    pub fn state_sink(&self) -> TrayStateSink {
        TrayStateSink(self.inner.borrow().shared_state.clone())
    }
}

impl TrayStateSink {
    pub fn apply_snapshot(&self, snapshot: &AppSnapshot) {
        if let Ok(mut state) = self.0.lock() {
            state.apply_app_snapshot(snapshot);
        }
    }

    pub fn apply_auto_check(&self, auto_check: bool) {
        if let Ok(mut state) = self.0.lock() {
            state.auto_check = auto_check;
        }
    }
}

impl TrayState {
    fn from_snapshot(snapshot: &AppSnapshot, version: &str, auto_check: bool) -> Self {
        let mut state = Self {
            app_title: format!("Synly {version}"),
            status_text: String::new(),
            connected: false,
            clipboard_mode: ClipboardMode::Off,
            audio_mode: AudioMode::Off,
            input_mode: InputMode::Off,
            auto_check,
        };
        state.apply_app_snapshot(snapshot);
        state
    }

    fn apply_app_snapshot(&mut self, snapshot: &AppSnapshot) {
        self.status_text = format!("Synly {}", snapshot.lifecycle.label());
        self.connected = snapshot.applied.is_some();
        self.clipboard_mode = snapshot.desired.clipboard_mode;
        self.audio_mode = snapshot.desired.audio_mode;
        self.input_mode = snapshot.desired.input.mode;
    }

    fn tooltip(&self) -> String {
        format!("Synly\n{}", self.status_text)
    }
}

impl NativeTray {
    fn new(inner: Weak<RefCell<ControllerInner>>, state: &TrayState) -> Result<Self> {
        let menu = Menu::new();
        let title_item = MenuItem::new(&state.app_title, false, None);
        let open_item = MenuItem::with_id(OPEN_ID, "打开 Synly", true, None);
        let status_item = MenuItem::new(&state.status_text, false, None);
        let separator_one = PredefinedMenuItem::separator();
        let connect_item = MenuItem::with_id(
            CONNECT_ID,
            if state.connected { "断开" } else { "开始" },
            true,
            None,
        );
        let (clipboard_menu, clipboard_group) =
            ModeGroup::build(CLIPBOARD_ID_PREFIX, "剪贴板", &CLIPBOARD_MODES)?;
        let (audio_menu, audio_group) = ModeGroup::build(AUDIO_ID_PREFIX, "音频", &AUDIO_MODES)?;
        let (input_menu, input_group) = ModeGroup::build(INPUT_ID_PREFIX, "输入", &INPUT_MODES)?;
        let separator_two = PredefinedMenuItem::separator();
        let check_update_item = MenuItem::with_id(CHECK_UPDATE_ID, "检查更新", true, None);
        let auto_check_item = CheckMenuItem::with_id(
            AUTO_CHECK_ID,
            "启动时自动检查更新",
            true,
            state.auto_check,
            None,
        );
        let separator_three = PredefinedMenuItem::separator();
        let quit_item = MenuItem::with_id(QUIT_ID, "退出", true, None);
        menu.append_items(&[
            &title_item,
            &open_item,
            &status_item,
            &separator_one,
            &connect_item,
            &clipboard_menu,
            &audio_menu,
            &input_menu,
            &separator_two,
            &check_update_item,
            &auto_check_item,
            &separator_three,
            &quit_item,
        ])
        .context("无法创建系统托盘菜单")?;

        let tray_icon = TrayIconBuilder::new()
            .with_icon(make_template_icon()?)
            .with_icon_as_template(true)
            .with_tooltip(state.tooltip())
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .build()
            .context("无法创建系统托盘图标")?;

        let poll_timer = Timer::default();
        poll_timer.start(TimerMode::Repeated, Duration::from_millis(80), move || {
            poll_events(&inner)
        });

        let mut tray = Self {
            tray_icon,
            status_item,
            connect_item,
            clipboard_group,
            audio_group,
            input_group,
            auto_check_item,
            state: state.clone(),
            _poll_timer: poll_timer,
        };
        // 菜单项创建时一律不勾选, 这里按当前状态补一次勾选.
        tray.sync(state);
        Ok(tray)
    }

    /// 按最新状态刷新菜单, 状态未变化时直接跳过.
    fn apply_state(&mut self, state: &TrayState) {
        if self.state == *state {
            return;
        }
        self.sync(state);
    }

    /// 乐观地把方向切换结果先画到菜单上.
    /// 托盘菜单项被点击时平台可能自行翻转勾选, 这里覆盖回唯一勾选项,
    /// 后续真实状态到达时会再做一次校准.
    fn preview_mode_change(&mut self, change: TrayModeChange) {
        let mut state = self.state.clone();
        match change {
            TrayModeChange::Clipboard(mode) => state.clipboard_mode = mode,
            TrayModeChange::Audio(mode) => state.audio_mode = mode,
            TrayModeChange::Input(mode) => state.input_mode = mode,
        }
        self.sync(&state);
    }

    fn sync(&mut self, state: &TrayState) {
        self.status_item.set_text(&state.status_text);
        self.connect_item
            .set_text(if state.connected { "断开" } else { "开始" });
        self.clipboard_group.sync(state.clipboard_mode);
        self.audio_group.sync(state.audio_mode);
        self.input_group.sync(state.input_mode);
        self.auto_check_item.set_checked(state.auto_check);
        if let Err(error) = self.tray_icon.set_tooltip(Some(&state.tooltip())) {
            tracing::warn!(error = %error, "无法更新系统托盘提示");
        }
        self.state = state.clone();
    }
}

fn start_native_tray(inner: Weak<RefCell<ControllerInner>>) {
    let Some(controller) = inner.upgrade() else {
        return;
    };
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
    guard_callback("tray_poll", || poll_events_inner(inner));
}

fn poll_events_inner(inner: &Weak<RefCell<ControllerInner>>) {
    poll_platform_events();
    if let Some(inner) = inner.upgrade() {
        let auto_check = inner.borrow().update.snapshot().auto_check;
        let shared_state = inner.borrow().shared_state.clone();
        if let Ok(mut state) = shared_state.lock() {
            state.auto_check = auto_check;
            if let Some(tray) = inner.borrow_mut().tray.as_mut() {
                tray.apply_state(&state);
            }
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
    // 托盘事件同样跑在 winit 的事件循环线程上, panic 逃出去会直接 abort.
    guard_callback("tray_action", || handle_action_inner(inner, action));
}

/// 把方向菜单项的 id 解析成方向切换, 其它菜单项返回 None.
fn mode_change(action: &str) -> Option<TrayModeChange> {
    let (group, key) = action.strip_prefix("synly.")?.split_once('.')?;
    match group {
        "clipboard" => Some(TrayModeChange::Clipboard(lookup_mode(
            &CLIPBOARD_MODES,
            key,
        )?)),
        "audio" => Some(TrayModeChange::Audio(lookup_mode(&AUDIO_MODES, key)?)),
        "input" => Some(TrayModeChange::Input(lookup_mode(&INPUT_MODES, key)?)),
        _ => None,
    }
}

fn lookup_mode<T: Copy>(modes: &[(T, &str, &str)], key: &str) -> Option<T> {
    modes
        .iter()
        .find(|(_, item_key, _)| *item_key == key)
        .map(|(mode, _, _)| *mode)
}

fn handle_action_inner(inner: &Weak<RefCell<ControllerInner>>, action: &str) {
    let Some(inner) = inner.upgrade() else { return };
    if let Some(change) = mode_change(action) {
        let mut controller = inner.borrow_mut();
        if let Some(tray) = controller.tray.as_mut() {
            tray.preview_mode_change(change);
        }
        send_command(&controller.commands, change.command());
        return;
    }
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
        CHECK_UPDATE_ID => {
            let update = inner.borrow().update.clone();
            update.check(true);
            let window = inner.borrow().window.clone();
            if let Some(window) = window.upgrade() {
                tracing::info!("从系统托盘检查更新");
                window.set_update_window_visible(true);
                if let Err(error) = show_main_window(&window) {
                    tracing::warn!(error = %error, "无法从系统托盘打开更新窗口");
                }
            }
        }
        AUTO_CHECK_ID => {
            let update = inner.borrow().update.clone();
            let enabled = !update.snapshot().auto_check;
            update.set_auto_check(enabled);
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
            let upper_head = (18..=27).contains(&x) && (y as i32 - 10).abs() * 2 <= (27 - x) as i32;
            let lower_shaft = (10..=27).contains(&x) && (20..=24).contains(&y);
            let lower_head = (4..=13).contains(&x) && (y as i32 - 22).abs() * 2 <= (x - 4) as i32;
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
