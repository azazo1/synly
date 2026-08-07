use super::platform;
use super::protocol::InputMessage;
use super::{
    DesktopLayout, DisplayRect, Hotkey, InputMode, InputPlatform, InputRuntimeOptions,
    KeyMappingConfig, Point, ScreenEdge,
};
use anyhow::{Context, Result, bail};
use slint::{CloseRequestResponse, ComponentHandle};
use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};

slint::include_modules!();

const GUI_INTERVAL: Duration = Duration::from_millis(16);
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
const EDGE_INSET: i32 = 8;
const GRID_SCROLL_STEP: f32 = 8.0;
const GRID_PATTERN_SIZE: f32 = 192.0;
const GRID_SMOOTH_FACTOR: f32 = 0.18;

#[derive(Clone, Debug)]
pub struct ScreenMockOptions {
    pub edge: ScreenEdge,
    pub hotkey: Hotkey,
    pub width: i32,
    pub height: i32,
}

impl Default for ScreenMockOptions {
    fn default() -> Self {
        Self {
            edge: ScreenEdge::Right,
            hotkey: Hotkey::DEFAULT
                .parse()
                .expect("默认紧急热键必须有效"),
            width: 1280,
            height: 720,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MockSettings {
    edge: ScreenEdge,
    hotkey: Hotkey,
    width: i32,
    height: i32,
    native_macos_to_windows: bool,
    native_windows_to_macos: bool,
    reverse_mouse_wheel: bool,
    reverse_trackpad: bool,
    filter_app_events: bool,
}

impl MockSettings {
    fn from_window(window: &InputScreenMockWindow) -> Result<Self> {
        let hotkey = Hotkey::from_str(window.get_hotkey_text().trim())?;
        Ok(Self {
            edge: edge_from_index(window.get_edge_index()),
            hotkey,
            width: window.get_virtual_width(),
            height: window.get_virtual_height(),
            native_macos_to_windows: window.get_native_scroll_macos_to_windows(),
            native_windows_to_macos: window.get_native_scroll_windows_to_macos(),
            reverse_mouse_wheel: window.get_reverse_mouse_wheel(),
            reverse_trackpad: window.get_reverse_trackpad(),
            filter_app_events: window.get_filter_app_events(),
        })
    }
}

pub fn run_screen_mock(options: ScreenMockOptions) -> Result<()> {
    validate_options(&options)?;
    let local_platform = InputPlatform::current();
    let remote_platform = opposite_platform(local_platform);
    let window = InputScreenMockWindow::new().context("无法创建输入虚拟屏幕窗口")?;
    window.set_route_text(
        format!(
            "{} -> {} mock",
            platform_label(local_platform),
            platform_label(remote_platform),
        )
        .into(),
    );
    window.set_source_edge(options.edge.as_arg().into());
    window.set_edge_index(edge_index(options.edge));
    window.set_hotkey_text(options.hotkey.to_string().into());
    window.set_virtual_width(options.width);
    window.set_virtual_height(options.height);
    window.set_virtual_aspect_ratio(options.width as f32 / options.height as f32);
    window.set_event_text(format!("等待从 {} 边缘接入", options.edge.as_arg()).into());
    window.show().context("无法显示输入虚拟屏幕窗口")?;

    let stop = Arc::new(AtomicBool::new(false));
    wire_window_shutdown(&window, Arc::clone(&stop));
    spawn_signal_monitor(Arc::clone(&stop))?;
    let (settings_tx, settings_rx) =
        tokio::sync::watch::channel(MockSettings::from_window(&window)?);
    let settings_weak = window.as_weak();
    window.on_settings_changed(move || {
        let Some(window) = settings_weak.upgrade() else {
            return;
        };
        match MockSettings::from_window(&window) {
            Ok(settings) => {
                window.set_source_edge(settings.edge.as_arg().into());
                window
                    .set_virtual_aspect_ratio(settings.width as f32 / settings.height as f32);
                let _ = settings_tx.send(settings);
            }
            Err(error) => {
                window.set_event_text(format!("设置无效: {error}").into());
            }
        }
    });
    let restore_weak = window.as_weak();
    window.on_restore_defaults(move || {
        let Some(window) = restore_weak.upgrade() else {
            return;
        };
        window.set_edge_index(edge_index(ScreenEdge::Right));
        window.set_virtual_width(1280);
        window.set_virtual_height(720);
        window.set_hotkey_text(Hotkey::DEFAULT.into());
        window.set_native_scroll_macos_to_windows(false);
        window.set_native_scroll_windows_to_macos(false);
        window.set_reverse_mouse_wheel(false);
        window.set_reverse_trackpad(false);
        window.set_smooth_scroll(false);
        window.set_filter_app_events(true);
        window.set_event_text("已恢复 mock 默认设置".into());
        window.invoke_settings_changed();
    });

    let presenter = GuiPresenter::new(window.as_weak(), options.width, options.height);

    let worker_stop = Arc::clone(&stop);
    let worker_presenter = presenter.clone();
    let mut worker_settings = settings_rx;
    let worker = std::thread::Builder::new()
        .name("synly-input-screen-mock".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("无法创建输入虚拟屏幕运行时")?;
            runtime.block_on(async {
                loop {
                    match run_mock_worker(
                        remote_platform,
                        worker_stop.clone(),
                        worker_presenter.clone(),
                        &mut worker_settings,
                    )
                    .await
                    {
                        Err(error) if error.downcast_ref::<MockRestart>().is_some() => {
                            tracing::info!("mock 输入设置已变化, 正在重启模拟");
                        }
                        result => return result,
                    }
                }
            })
        })
        .context("无法启动输入虚拟屏幕线程")?;

    let ui_result = slint::run_event_loop_until_quit().context("输入虚拟屏幕事件循环失败");
    stop.store(true, Ordering::Release);
    let worker_result = match worker.join() {
        Ok(result) => result,
        Err(_) => bail!("输入虚拟屏幕线程异常退出"),
    };
    ui_result?;
    worker_result
}

fn validate_options(options: &ScreenMockOptions) -> Result<()> {
    if options.width <= EDGE_INSET * 2 || options.height <= EDGE_INSET * 2 {
        bail!("mock 虚拟屏幕尺寸过小")
    }
    Ok(())
}

fn wire_window_shutdown(window: &InputScreenMockWindow, stop: Arc<AtomicBool>) {
    let callback_stop = Arc::clone(&stop);
    let weak = window.as_weak();
    window.on_quit_requested(move || {
        callback_stop.store(true, Ordering::Release);
        if let Some(window) = weak.upgrade() {
            let _ = window.hide();
        }
        let _ = slint::quit_event_loop();
    });

    window.window().on_close_requested(move || {
        stop.store(true, Ordering::Release);
        let _ = slint::quit_event_loop();
        CloseRequestResponse::HideWindow
    });
}

fn spawn_signal_monitor(stop: Arc<AtomicBool>) -> Result<()> {
    std::thread::Builder::new()
        .name("synly-input-mock-signal".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(error = %error, "无法创建输入 mock 信号运行时");
                    return;
                }
            };
            match runtime.block_on(tokio::signal::ctrl_c()) {
                Ok(()) => {
                    tracing::info!("收到 Ctrl-C, 正在恢复本机光标");
                    stop.store(true, Ordering::Release);
                    if let Err(error) = slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    }) {
                        tracing::debug!(error = %error, "输入 mock 事件循环已经停止");
                    }
                }
                Err(error) => {
                    tracing::error!(error = %error, "输入 mock 无法监听 Ctrl-C");
                }
            }
        })
        .context("无法启动输入 mock 信号线程")?;
    Ok(())
}

async fn run_mock_worker(
    remote_platform: InputPlatform,
    stop: Arc<AtomicBool>,
    presenter: GuiPresenter,
    settings: &mut tokio::sync::watch::Receiver<MockSettings>,
) -> Result<()> {
    let mock_settings = (*settings.borrow_and_update()).clone();
    presenter.set_size(mock_settings.width, mock_settings.height);
    let mut platform = match platform::start_with_filter(
        InputMode::Send,
        mock_settings.hotkey,
        mock_settings.filter_app_events,
    ) {
        Ok(platform) => platform,
        Err(error) => {
            tracing::error!(error = %error, "输入虚拟屏幕 mock 启动失败");
            presenter.publish(GuiUpdate::idle(format!("启动失败: {error}")));
            wait_for_shutdown(&stop).await;
            return Err(error);
        }
    };
    let local_layout = platform.backend.layout()?;
    let mock_layout = DesktopLayout::new(vec![DisplayRect {
        x: 0,
        y: 0,
        width: mock_settings.width,
        height: mock_settings.height,
    }])?;
    tracing::info!(
        local_platform = platform_label(InputPlatform::current()),
        remote_platform = platform_label(remote_platform),
        edge = mock_settings.edge.as_arg(),
        "输入虚拟屏幕 mock 已启动"
    );

    let observed_motion = Arc::clone(&platform.motion);
    let local_backend = Arc::clone(&platform.backend);
    let (incoming_tx, mut incoming_rx) = mpsc::channel(256);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(256);
    let runtime_options = InputRuntimeOptions {
        mode: InputMode::Send,
        edge: mock_settings.edge,
        hotkey: mock_settings.hotkey,
        reverse_mouse_wheel: mock_settings.reverse_mouse_wheel,
        reverse_trackpad: mock_settings.reverse_trackpad,
        native_scroll_macos_to_windows: mock_settings.native_macos_to_windows,
        native_scroll_windows_to_macos: mock_settings.native_windows_to_macos,
        block_switch_on_press: false,
        filter_app_events: mock_settings.filter_app_events,
        key_mapping: KeyMappingConfig::default(),
        cursor_mode: crate::input::CursorMode::Desktop,
    };
    let sender = super::runtime::run_sender(
        &mut incoming_rx,
        &outgoing_tx,
        &mut platform,
        local_layout,
        mock_settings.edge,
        remote_platform,
        &runtime_options,
    );
    tokio::pin!(sender);
    let peer = run_mock_peer(
        mock_layout,
        observed_motion,
        local_backend,
        incoming_tx,
        outgoing_rx,
        remote_platform,
        &stop,
        &presenter,
    );
    tokio::pin!(peer);
    let result = tokio::select! {
        result = &mut sender => result,
        result = &mut peer => result,
        _ = settings.changed() => Err(anyhow::anyhow!(MockRestart)),
    };
    if let Err(error) = &result
        && error.downcast_ref::<MockRestart>().is_none()
    {
        tracing::error!(error = %error, "输入虚拟屏幕 mock 已停止");
        presenter.publish(GuiUpdate::idle(format!("运行失败: {error}")));
        wait_for_shutdown(&stop).await;
    }
    result
}

#[derive(Debug)]
struct MockRestart;

impl std::fmt::Display for MockRestart {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("mock input settings changed")
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_mock_peer(
    mock_layout: DesktopLayout,
    motion: Arc<platform::MotionAccumulator>,
    local_backend: Arc<dyn platform::InputBackend>,
    incoming: mpsc::Sender<Result<InputMessage>>,
    mut outgoing: mpsc::Receiver<InputMessage>,
    remote_platform: InputPlatform,
    stop: &AtomicBool,
    presenter: &GuiPresenter,
) -> Result<()> {
    let mut gui_tick = time::interval(GUI_INTERVAL);
    gui_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut heartbeat = time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut generation = 0u64;
    let mut active = false;
    let mut return_edge = ScreenEdge::Left;
    let mut remote_cursor = Point::default();
    let mut last_local_position = None;
    let mut last_delta = Point::default();
    let mut keys = BTreeSet::new();
    let mut buttons = BTreeSet::new();
    let mut wheel = Point::default();
    let mut grid_offset = GridOffset::default();
    let mut last_event = "等待输入".to_string();

    loop {
        tokio::select! {
            message = outgoing.recv() => {
                let message = message.context("正式输入发送端已停止")?;
                match message {
                    InputMessage::Activate {
                        generation: incoming_generation,
                        source_edge,
                        edge_position,
                        pressed,
                    } => {
                        generation = incoming_generation;
                        active = true;
                        return_edge = source_edge.opposite();
                        remote_cursor = mock_layout.point_inside_edge(
                            return_edge,
                            edge_position,
                            EDGE_INSET,
                        );
                        keys = pressed.usages.into_iter().collect();
                        buttons = pressed.buttons.into_iter().collect();
                        last_event = format!(
                            "正式 sender 已接入 mock, entry={edge_position:.3}, x={}, y={}",
                            remote_cursor.x,
                            remote_cursor.y,
                        );
                        tracing::info!(
                            generation,
                            source_edge = ?source_edge,
                            edge_position,
                            cursor = ?remote_cursor,
                            "mock 收到正式 sender 的 Activate"
                        );
                    }
                    InputMessage::Deactivate { generation: incoming_generation, .. }
                        if incoming_generation == generation => {
                        active = false;
                        keys.clear();
                        buttons.clear();
                        last_event = "正式 sender 已恢复本机".to_string();
                        tracing::info!(generation, "mock 收到正式 sender 的 Deactivate");
                    }
                    InputMessage::Motion {
                        generation: incoming_generation,
                        dx,
                        dy,
                    } if active && incoming_generation == generation => {
                        last_delta = Point { x: dx, y: dy };
                        if let Some(edge_position) = mock_layout.crossed_outer_edge_position(
                            return_edge,
                            remote_cursor,
                            dx,
                            dy,
                        ) {
                            remote_cursor = mock_layout.move_within_layout(remote_cursor, dx, dy);
                            incoming
                                .send(Ok(InputMessage::ReturnRequest {
                                    generation,
                                    edge_position,
                                }))
                                .await
                                .context("无法向正式 sender 请求返回控制")?;
                            last_event = format!(
                                "mock 已向正式 sender 请求返回, position={edge_position:.3}"
                            );
                            tracing::info!(
                                generation,
                                edge_position,
                                "mock 向正式 sender 请求返回控制"
                            );
                        } else {
                            remote_cursor = mock_layout.move_within_layout(remote_cursor, dx, dy);
                        }
                    }
                    InputMessage::Return {
                        generation: incoming_generation,
                        edge_position,
                    } if active && incoming_generation == generation => {
                        active = false;
                        keys.clear();
                        buttons.clear();
                        last_event = format!(
                            "正式 sender 已批准 mock 返回, position={edge_position:.3}"
                        );
                        tracing::info!(generation, edge_position, "mock 收到正式 sender 的 Return");
                    }
                    InputMessage::Key {
                        generation: incoming_generation,
                        usage,
                        modifiers,
                        down,
                        repeat,
                    } if active && incoming_generation == generation => {
                        if down {
                            keys.insert(usage);
                        } else {
                            keys.remove(&usage);
                        }
                        last_event = format!(
                            "按键 hid=0x{usage:04x}, down={down}, repeat={repeat}, modifiers=0x{:02x}",
                            modifiers.bits(),
                        );
                    }
                    InputMessage::Button {
                        generation: incoming_generation,
                        button,
                        down,
                    } if active && incoming_generation == generation => {
                        if down {
                            buttons.insert(button);
                        } else {
                            buttons.remove(&button);
                        }
                        last_event = format!("鼠标按钮 button={button}, down={down}");
                    }
                    InputMessage::Wheel {
                        generation: incoming_generation,
                        x,
                        y,
                    } if active && incoming_generation == generation => {
                        wheel.x = wheel.x.saturating_add(x);
                        wheel.y = wheel.y.saturating_add(y);
                        grid_offset.add_wheel(x, y, remote_platform);
                        last_event = format!("滚轮 x={x}, y={y}");
                    }
                    InputMessage::Heartbeat { .. } => {}
                    InputMessage::SecureDesktop { active } => {
                        last_event = if active {
                            "对端进入安全桌面".to_string()
                        } else {
                            "对端离开安全桌面".to_string()
                        };
                        tracing::info!(active, "mock 收到对端安全桌面状态");
                    }
                    InputMessage::Hello { .. }
                    | InputMessage::Proof { .. }
                    | InputMessage::ReturnRequest { .. }
                    | InputMessage::Return { .. }
                    | InputMessage::Deactivate { .. }
                    | InputMessage::Key { .. }
                    | InputMessage::Button { .. }
                    | InputMessage::Motion { .. }
                    | InputMessage::Wheel { .. } => {}
                }
            }
            _ = heartbeat.tick() => {
                incoming
                    .send(Ok(InputMessage::Heartbeat { generation }))
                    .await
                    .context("无法向正式 sender 发送 mock 心跳")?;
            }
            _ = gui_tick.tick() => {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if !active {
                    let sample = motion.take_observed();
                    let delta = Point { x: sample.dx, y: sample.dy };
                    if delta != last_delta || sample.position != last_local_position {
                        last_delta = delta;
                        last_local_position = sample.position;
                        if let Some(point) = sample.position {
                            last_event = format!(
                                "本机采样 x={}, y={}, delta={:+}, {:+}",
                                point.x,
                                point.y,
                                sample.dx,
                                sample.dy,
                            );
                            tracing::trace!(
                                point = ?point,
                                dx = sample.dx,
                                dy = sample.dy,
                                position_updated = sample.position_updated,
                                "mock 收到本机位置采样"
                            );
                        }
                    }
                }
                let local_snapshot = local_backend.snapshot();
                let local_keys: BTreeSet<u16> =
                    local_snapshot.usages.iter().copied().collect();
                let local_buttons: BTreeSet<u8> =
                    local_snapshot.buttons.iter().copied().collect();
                let local_keys_text = pressed_names(&local_keys);
                let local_buttons_text = pressed_button_names(&local_buttons);
                presenter.publish(GuiUpdate {
                    active,
                    cursor: remote_cursor,
                    delta: last_delta,
                    key_count: keys.len() as u32,
                    button_mask: button_mask(&buttons),
                    wheel,
                    grid_offset_x: grid_offset.x,
                    grid_offset_y: grid_offset.y,
                    event: last_event.clone(),
                    host_keys: local_keys_text,
                    host_buttons: local_buttons_text,
                    remote_keys: pressed_names(&keys),
                    remote_buttons: pressed_button_names(&buttons),
                });
            }
        }
    }
    Ok(())
}

async fn wait_for_shutdown(stop: &AtomicBool) {
    let mut tick = time::interval(GUI_INTERVAL);
    loop {
        tick.tick().await;
        if stop.load(Ordering::Acquire) {
            break;
        }
    }
}

fn opposite_platform(platform: InputPlatform) -> InputPlatform {
    match platform {
        InputPlatform::Macos => InputPlatform::Windows,
        InputPlatform::Windows => InputPlatform::Macos,
    }
}

fn platform_label(platform: InputPlatform) -> &'static str {
    match platform {
        InputPlatform::Macos => "macOS",
        InputPlatform::Windows => "Windows",
    }
}

fn button_mask(buttons: &BTreeSet<u8>) -> u32 {
    buttons.iter().fold(0u32, |mask, button| {
        mask | 1u32
            .checked_shl(u32::from(button.saturating_sub(1)))
            .unwrap_or(0)
    })
}

fn edge_index(edge: ScreenEdge) -> i32 {
    match edge {
        ScreenEdge::Left => 0,
        ScreenEdge::Right => 1,
        ScreenEdge::Top => 2,
        ScreenEdge::Bottom => 3,
    }
}

fn edge_from_index(index: i32) -> ScreenEdge {
    match index {
        0 => ScreenEdge::Left,
        2 => ScreenEdge::Top,
        3 => ScreenEdge::Bottom,
        _ => ScreenEdge::Right,
    }
}

fn pressed_names(usages: &BTreeSet<u16>) -> String {
    if usages.is_empty() {
        return "无".to_string();
    }
    usages
        .iter()
        .map(|usage| super::hotkey::key_name(*usage))
        .collect::<Vec<_>>()
        .join(", ")
}

fn pressed_button_names(buttons: &BTreeSet<u8>) -> String {
    if buttons.is_empty() {
        return "无".to_string();
    }
    buttons
        .iter()
        .map(|button| match button {
            1 => "left".to_string(),
            2 => "middle".to_string(),
            3 => "right".to_string(),
            4 => "x1".to_string(),
            5 => "x2".to_string(),
            other => format!("button-{other}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn wheel_to_grid_delta(x: i32, y: i32, remote_platform: InputPlatform) -> (f32, f32) {
    let scale = grid_pixels_per_wheel_unit(remote_platform);
    (x as f32 * scale, y as f32 * scale)
}

fn grid_pixels_per_wheel_unit(remote_platform: InputPlatform) -> f32 {
    match remote_platform {
        InputPlatform::Macos | InputPlatform::Windows => GRID_SCROLL_STEP,
    }
}

fn grid_phase(value: f32) -> f32 {
    value.rem_euclid(GRID_PATTERN_SIZE)
}

#[derive(Clone, Copy, Default)]
struct GridOffset {
    x: f32,
    y: f32,
}

impl GridOffset {
    fn add_wheel(&mut self, x: i32, y: i32, remote_platform: InputPlatform) {
        let (delta_x, delta_y) = wheel_to_grid_delta(x, y, remote_platform);
        self.x += delta_x;
        self.y += delta_y;
    }
}

#[derive(Clone, Copy, Default)]
struct SmoothGridState {
    x: f32,
    y: f32,
}

impl SmoothGridState {
    fn advance(&mut self, target_x: f32, target_y: f32) -> (f32, f32) {
        self.x = smooth_toward(self.x, target_x);
        self.y = smooth_toward(self.y, target_y);
        (self.x, self.y)
    }

    fn snap(&mut self, target_x: f32, target_y: f32) {
        self.x = target_x;
        self.y = target_y;
    }
}

fn smooth_toward(current: f32, target: f32) -> f32 {
    let diff = target - current;
    if diff.abs() < 0.05 {
        target
    } else {
        current + diff * GRID_SMOOTH_FACTOR
    }
}

#[derive(Clone)]
struct GuiPresenter {
    window: slint::Weak<InputScreenMockWindow>,
    latest: Arc<Mutex<GuiUpdate>>,
    scheduled: Arc<AtomicBool>,
    smooth: Arc<Mutex<SmoothGridState>>,
    size: Arc<Mutex<(i32, i32)>>,
}

impl GuiPresenter {
    fn new(window: slint::Weak<InputScreenMockWindow>, width: i32, height: i32) -> Self {
        Self {
            window,
            latest: Arc::new(Mutex::new(GuiUpdate::idle("正在启动".to_string()))),
            scheduled: Arc::new(AtomicBool::new(false)),
            smooth: Arc::new(Mutex::new(SmoothGridState::default())),
            size: Arc::new(Mutex::new((width, height))),
        }
    }

    fn set_size(&self, width: i32, height: i32) {
        if let Ok(mut size) = self.size.lock() {
            *size = (width, height);
        } else {
            tracing::error!("输入 mock GUI 尺寸锁已损坏");
        }
    }

    fn publish(&self, update: GuiUpdate) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = update;
        } else {
            tracing::error!("输入 mock GUI 状态锁已损坏");
            return;
        }
        if self.scheduled.swap(true, Ordering::AcqRel) {
            return;
        }

        let window = self.window.clone();
        let latest = Arc::clone(&self.latest);
        let scheduled = Arc::clone(&self.scheduled);
        let smooth = Arc::clone(&self.smooth);
        let (width, height) = match self.size.lock() {
            Ok(size) => *size,
            Err(_) => {
                tracing::error!("输入 mock GUI 尺寸锁已损坏");
                (0, 0)
            }
        };
        if let Err(error) = slint::invoke_from_event_loop(move || {
            scheduled.store(false, Ordering::Release);
            let update = match latest.lock() {
                Ok(update) => update.clone(),
                Err(_) => return,
            };
            if let Some(window) = window.upgrade() {
                let mut smooth = smooth.lock().unwrap_or_else(|error| error.into_inner());
                apply_gui_update(&window, &update, width, height, &mut smooth);
            }
        }) {
            self.scheduled.store(false, Ordering::Release);
            tracing::debug!(error = %error, "无法调度输入 mock GUI 更新");
        }
    }
}

#[derive(Clone)]
struct GuiUpdate {
    active: bool,
    cursor: Point,
    delta: Point,
    key_count: u32,
    button_mask: u32,
    wheel: Point,
    grid_offset_x: f32,
    grid_offset_y: f32,
    event: String,
    host_keys: String,
    host_buttons: String,
    remote_keys: String,
    remote_buttons: String,
}

impl GuiUpdate {
    fn idle(event: String) -> Self {
        Self {
            active: false,
            cursor: Point::default(),
            delta: Point::default(),
            key_count: 0,
            button_mask: 0,
            wheel: Point::default(),
            grid_offset_x: 0.0,
            grid_offset_y: 0.0,
            event,
            host_keys: "无".to_string(),
            host_buttons: "无".to_string(),
            remote_keys: "无".to_string(),
            remote_buttons: "无".to_string(),
        }
    }
}

fn apply_gui_update(
    window: &InputScreenMockWindow,
    update: &GuiUpdate,
    width: i32,
    height: i32,
    smooth: &mut SmoothGridState,
) {
    window.set_active(update.active);
    window.set_cursor_x(update.cursor.x);
    window.set_cursor_y(update.cursor.y);
    window.set_cursor_ratio_x(normalized_coordinate(update.cursor.x, width));
    window.set_cursor_ratio_y(normalized_coordinate(update.cursor.y, height));
    window.set_delta_x(update.delta.x);
    window.set_delta_y(update.delta.y);
    window.set_key_count(update.key_count.min(i32::MAX as u32) as i32);
    window.set_button_summary(format!("0x{:08x}", update.button_mask).into());
    window.set_wheel_x(update.wheel.x);
    window.set_wheel_y(update.wheel.y);
    window.set_host_keys(update.host_keys.clone().into());
    window.set_host_buttons(update.host_buttons.clone().into());
    window.set_remote_keys(update.remote_keys.clone().into());
    window.set_remote_buttons(update.remote_buttons.clone().into());
    if window.get_smooth_scroll() {
        let (display_x, display_y) = smooth.advance(update.grid_offset_x, update.grid_offset_y);
        window.set_grid_offset_x(grid_phase(display_x));
        window.set_grid_offset_y(grid_phase(display_y));
    } else {
        smooth.snap(update.grid_offset_x, update.grid_offset_y);
        window.set_grid_offset_x(grid_phase(update.grid_offset_x));
        window.set_grid_offset_y(grid_phase(update.grid_offset_y));
    }
    window.set_event_text(update.event.clone().into());
}

fn normalized_coordinate(value: i32, extent: i32) -> f32 {
    if extent <= 1 {
        return 0.0;
    }
    (value as f32 / (extent - 1) as f32).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::{
        GRID_PATTERN_SIZE, GRID_SCROLL_STEP, SmoothGridState, button_mask, edge_from_index,
        edge_index, grid_phase, grid_pixels_per_wheel_unit, normalized_coordinate,
        opposite_platform, pressed_button_names, pressed_names,
    };
    use crate::input::{InputPlatform, ScreenEdge};
    use std::collections::BTreeSet;

    #[test]
    fn mock_uses_the_opposite_remote_platform() {
        assert_eq!(
            opposite_platform(InputPlatform::Macos),
            InputPlatform::Windows,
        );
        assert_eq!(
            opposite_platform(InputPlatform::Windows),
            InputPlatform::Macos,
        );
    }

    #[test]
    fn gui_values_are_bounded_and_buttons_use_stable_bits() {
        assert_eq!(normalized_coordinate(-4, 100), 0.0);
        assert_eq!(normalized_coordinate(99, 100), 1.0);
        assert_eq!(button_mask(&BTreeSet::from([1, 3])), 0b101);
    }

    #[test]
    fn mock_edge_index_roundtrips() {
        for edge in [
            ScreenEdge::Left,
            ScreenEdge::Right,
            ScreenEdge::Top,
            ScreenEdge::Bottom,
        ] {
            assert_eq!(edge_from_index(edge_index(edge)), edge);
        }
    }

    #[test]
    fn pressed_state_names_are_readable() {
        assert_eq!(pressed_names(&BTreeSet::from([0x04, 0x45])), "a, f12");
        assert_eq!(
            pressed_button_names(&BTreeSet::from([1, 3, 5])),
            "left, right, x2"
        );
        assert_eq!(pressed_names(&BTreeSet::new()), "无");
        assert_eq!(pressed_button_names(&BTreeSet::new()), "无");
    }

    #[test]
    fn grid_offset_uses_small_step_and_wraps_phase() {
        assert_eq!(grid_pixels_per_wheel_unit(InputPlatform::Macos), GRID_SCROLL_STEP);
        assert_eq!(grid_pixels_per_wheel_unit(InputPlatform::Windows), GRID_SCROLL_STEP);
        assert_eq!(grid_phase(-60.0), 132.0);
        assert_eq!(grid_phase(48.0), 48.0);
        assert_eq!(grid_phase(250.0), 58.0);
        assert_eq!(grid_phase(GRID_PATTERN_SIZE), 0.0);
    }

    #[test]
    fn smooth_grid_moves_toward_target_incrementally() {
        let mut smooth = SmoothGridState::default();
        let (first_x, _) = smooth.advance(8.0, 4.0);
        assert!(first_x > 0.0 && first_x < 8.0);
        for _ in 0..128 {
            smooth.advance(8.0, 4.0);
        }
        assert_eq!(smooth.x, 8.0);
        assert_eq!(smooth.y, 4.0);
    }
}
