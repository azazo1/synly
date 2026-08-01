use super::platform;
use super::protocol::InputMessage;
use super::{
    DesktopLayout, DisplayRect, Hotkey, InputMode, InputPlatform, InputRuntimeOptions,
    KeyMappingConfig, Point, ScreenEdge,
};
use anyhow::{Context, Result, bail};
use slint::{CloseRequestResponse, ComponentHandle};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};

slint::include_modules!();

const GUI_INTERVAL: Duration = Duration::from_millis(16);
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
const EDGE_INSET: i32 = 8;

#[derive(Clone, Debug)]
pub struct ScreenMockOptions {
    pub edge: ScreenEdge,
    pub hotkey: Hotkey,
    pub width: i32,
    pub height: i32,
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
    window.set_virtual_width(options.width);
    window.set_virtual_height(options.height);
    window.set_virtual_aspect_ratio(options.width as f32 / options.height as f32);
    window.set_event_text(format!("等待从 {} 边缘接入", options.edge.as_arg()).into());
    window.show().context("无法显示输入虚拟屏幕窗口")?;

    let stop = Arc::new(AtomicBool::new(false));
    wire_window_shutdown(&window, Arc::clone(&stop));
    spawn_signal_monitor(Arc::clone(&stop))?;

    let presenter = GuiPresenter::new(window.as_weak(), options.width, options.height);

    let worker_stop = Arc::clone(&stop);
    let worker_presenter = presenter.clone();
    let worker = std::thread::Builder::new()
        .name("synly-input-screen-mock".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("无法创建输入虚拟屏幕运行时")?;
            runtime.block_on(run_mock_worker(
                options,
                remote_platform,
                worker_stop,
                worker_presenter,
            ))
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
    options: ScreenMockOptions,
    remote_platform: InputPlatform,
    stop: Arc<AtomicBool>,
    presenter: GuiPresenter,
) -> Result<()> {
    let mut platform = match platform::start(InputMode::Send, options.hotkey) {
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
        width: options.width,
        height: options.height,
    }])?;
    tracing::info!(
        local_platform = platform_label(InputPlatform::current()),
        remote_platform = platform_label(remote_platform),
        edge = options.edge.as_arg(),
        "输入虚拟屏幕 mock 已启动"
    );

    let observed_motion = Arc::clone(&platform.motion);
    let (incoming_tx, mut incoming_rx) = mpsc::channel(256);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(256);
    let runtime_options = InputRuntimeOptions {
        mode: InputMode::Send,
        edge: options.edge,
        hotkey: options.hotkey,
        reverse_mouse_wheel: false,
        reverse_trackpad: false,
        block_switch_on_press: false,
        key_mapping: KeyMappingConfig::default(),
        cursor_mode: crate::input::CursorMode::Desktop,
        auto_game_cursor: false,
    };
    let sender = super::runtime::run_sender(
        &mut incoming_rx,
        &outgoing_tx,
        &mut platform,
        local_layout,
        options.edge,
        remote_platform,
        &runtime_options,
    );
    tokio::pin!(sender);
    let peer = run_mock_peer(
        mock_layout,
        observed_motion,
        incoming_tx,
        outgoing_rx,
        &stop,
        &presenter,
    );
    tokio::pin!(peer);
    let result = tokio::select! {
        result = &mut sender => result,
        result = &mut peer => result,
    };
    if let Err(error) = &result {
        tracing::error!(error = %error, "输入虚拟屏幕 mock 已停止");
        presenter.publish(GuiUpdate::idle(format!("运行失败: {error}")));
        wait_for_shutdown(&stop).await;
    }
    result
}

async fn run_mock_peer(
    mock_layout: DesktopLayout,
    motion: Arc<platform::MotionAccumulator>,
    incoming: mpsc::Sender<Result<InputMessage>>,
    mut outgoing: mpsc::Receiver<InputMessage>,
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
                    InputMessage::Deactivate { generation: incoming_generation }
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
                        last_event = format!("滚轮 x={x}, y={y}");
                    }
                    InputMessage::Heartbeat { .. } => {}
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
                            tracing::debug!(
                                point = ?point,
                                dx = sample.dx,
                                dy = sample.dy,
                                position_updated = sample.position_updated,
                                "mock 收到本机位置采样"
                            );
                        }
                    }
                }
                presenter.publish(GuiUpdate {
                    active,
                    cursor: remote_cursor,
                    delta: last_delta,
                    key_count: keys.len() as u32,
                    button_mask: button_mask(&buttons),
                    wheel,
                    event: last_event.clone(),
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

#[derive(Clone)]
struct GuiPresenter {
    window: slint::Weak<InputScreenMockWindow>,
    latest: Arc<Mutex<GuiUpdate>>,
    scheduled: Arc<AtomicBool>,
    width: i32,
    height: i32,
}

impl GuiPresenter {
    fn new(window: slint::Weak<InputScreenMockWindow>, width: i32, height: i32) -> Self {
        Self {
            window,
            latest: Arc::new(Mutex::new(GuiUpdate::idle("正在启动".to_string()))),
            scheduled: Arc::new(AtomicBool::new(false)),
            width,
            height,
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
        let width = self.width;
        let height = self.height;
        if let Err(error) = slint::invoke_from_event_loop(move || {
            scheduled.store(false, Ordering::Release);
            let update = match latest.lock() {
                Ok(update) => update.clone(),
                Err(_) => return,
            };
            if let Some(window) = window.upgrade() {
                apply_gui_update(&window, &update, width, height);
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
    event: String,
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
            event,
        }
    }
}

fn apply_gui_update(
    window: &InputScreenMockWindow,
    update: &GuiUpdate,
    width: i32,
    height: i32,
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
    use super::{button_mask, normalized_coordinate, opposite_platform};
    use crate::input::InputPlatform;
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
}
