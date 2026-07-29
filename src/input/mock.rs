use super::platform;
use super::protocol::InputMessage;
use super::{DesktopLayout, DisplayRect, Hotkey, InputMode, Point, ScreenEdge};
use anyhow::{Context, Result, bail};
use std::collections::BTreeSet;
use std::ffi::CString;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};

const GUI_INTERVAL: Duration = Duration::from_micros(8_333);
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
const EDGE_INSET: i32 = 8;

#[derive(Clone, Debug)]
pub struct MacosMockOptions {
    pub edge: ScreenEdge,
    pub hotkey: Hotkey,
    pub width: i32,
    pub height: i32,
}

pub fn run_macos_mock(options: MacosMockOptions) -> Result<()> {
    if options.width <= EDGE_INSET * 2 || options.height <= EDGE_INSET * 2 {
        bail!("mock 虚拟屏幕尺寸过小")
    }
    let edge = CString::new(options.edge.as_arg())?;
    let prepared = unsafe {
        synly_input_mock_gui_prepare(options.width, options.height, edge.as_ptr())
    };
    if prepared != 0 {
        bail!("无法创建 macOS mock GUI")
    }

    let stop = Arc::new(AtomicBool::new(false));
    spawn_signal_monitor(Arc::clone(&stop))?;
    let worker_stop = Arc::clone(&stop);
    let worker = std::thread::Builder::new()
        .name("synly-input-macos-mock".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("无法创建 macOS 输入 mock 运行时")?;
            runtime.block_on(run_mock_worker(options, worker_stop))
        })
        .context("无法启动 macOS 输入 mock 线程")?;

    unsafe { synly_input_mock_gui_run() };
    stop.store(true, Ordering::Release);
    match worker.join() {
        Ok(result) => result,
        Err(_) => bail!("macOS 输入 mock 线程异常退出"),
    }
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
                    tracing::error!(error = %error, "无法创建 macOS mock 信号运行时");
                    return;
                }
            };
            match runtime.block_on(tokio::signal::ctrl_c()) {
                Ok(()) => {
                    tracing::info!("收到 Ctrl-C, 正在恢复 macOS 光标");
                    stop.store(true, Ordering::Release);
                    unsafe { synly_input_mock_gui_stop() };
                }
                Err(error) => {
                    tracing::error!(error = %error, "macOS mock 无法监听 Ctrl-C");
                }
            }
        })
        .context("无法启动 macOS mock 信号线程")?;
    Ok(())
}

async fn run_mock_worker(options: MacosMockOptions, stop: Arc<AtomicBool>) -> Result<()> {
    let mut platform = match platform::start(InputMode::Send, options.hotkey) {
        Ok(platform) => platform,
        Err(error) => {
            tracing::error!(error = %error, "macOS 输入 mock 启动失败");
            update_gui(&GuiUpdate {
                active: false,
                cursor: Point::default(),
                delta: Point::default(),
                key_count: 0,
                button_mask: 0,
                wheel: Point::default(),
                event: &format!("启动失败: {error}"),
            });
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
    update_gui(&GuiUpdate {
        active: false,
        cursor: Point::default(),
        delta: Point::default(),
        key_count: 0,
        button_mask: 0,
        wheel: Point::default(),
        event: &format!("等待从 {} 边缘接入", options.edge.as_arg()),
    });
    tracing::info!(edge = options.edge.as_arg(), "macOS 输入 mock 已启动");

    let observed_motion = Arc::clone(&platform.motion);
    let (incoming_tx, mut incoming_rx) = mpsc::channel(256);
    let (outgoing_tx, outgoing_rx) = mpsc::channel(256);
    let sender = super::runtime::run_sender(
        &mut incoming_rx,
        &outgoing_tx,
        &mut platform,
        local_layout,
        mock_layout.clone(),
        options.edge,
    );
    tokio::pin!(sender);
    let peer = run_mock_peer(
        mock_layout,
        observed_motion,
        incoming_tx,
        outgoing_rx,
        &stop,
    );
    tokio::pin!(peer);
    let result = tokio::select! {
        result = &mut sender => result,
        result = &mut peer => result,
    };
    unsafe { synly_input_mock_gui_stop() };
    result
}

async fn run_mock_peer(
    mock_layout: DesktopLayout,
    motion: Arc<platform::MotionAccumulator>,
    incoming: mpsc::Sender<Result<InputMessage>>,
    mut outgoing: mpsc::Receiver<InputMessage>,
    stop: &AtomicBool,
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
                            active = false;
                            incoming
                                .send(Ok(InputMessage::Return { generation, edge_position }))
                                .await
                                .context("无法向正式 sender 返回控制")?;
                            last_event = format!(
                                "mock 已向正式 sender 返回控制, position={edge_position:.3}"
                            );
                            tracing::info!(
                                generation,
                                edge_position,
                                "mock 向正式 sender 返回控制"
                            );
                        } else {
                            remote_cursor = mock_layout.move_within_layout(remote_cursor, dx, dy);
                        }
                    }
                    InputMessage::Key { generation: incoming_generation, usage, modifiers, down, repeat }
                        if active && incoming_generation == generation => {
                        if down { keys.insert(usage); } else { keys.remove(&usage); }
                        last_event = format!(
                            "按键 hid=0x{usage:04x}, down={down}, repeat={repeat}, modifiers=0x{:02x}",
                            modifiers.bits(),
                        );
                    }
                    InputMessage::Button { generation: incoming_generation, button, down }
                        if active && incoming_generation == generation => {
                        if down { buttons.insert(button); } else { buttons.remove(&button); }
                        last_event = format!("鼠标按钮 button={button}, down={down}");
                    }
                    InputMessage::Wheel { generation: incoming_generation, x, y }
                        if active && incoming_generation == generation => {
                        wheel.x = wheel.x.saturating_add(x);
                        wheel.y = wheel.y.saturating_add(y);
                        last_event = format!("滚轮 x={x}, y={y}");
                    }
                    InputMessage::Heartbeat { .. } => {}
                    InputMessage::Layout(_) | InputMessage::Proof { .. } | InputMessage::Return { .. }
                    | InputMessage::Deactivate { .. }
                    | InputMessage::Key { .. } | InputMessage::Button { .. }
                    | InputMessage::Motion { .. } | InputMessage::Wheel { .. } => {}
                }
            }
            _ = heartbeat.tick() => {
                incoming
                    .send(Ok(InputMessage::Heartbeat { generation }))
                    .await
                    .context("无法向正式 sender 发送 mock 心跳")?;
            }
            _ = gui_tick.tick() => {
                if stop.load(Ordering::Acquire) || !unsafe { synly_input_mock_gui_is_running() } {
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
                update_gui(&GuiUpdate {
                    active,
                    cursor: remote_cursor,
                    delta: last_delta,
                    key_count: keys.len() as u32,
                    button_mask: button_mask(&buttons),
                    wheel,
                    event: &last_event,
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
        if stop.load(Ordering::Acquire) || !unsafe { synly_input_mock_gui_is_running() } {
            break;
        }
    }
}

fn button_mask(buttons: &BTreeSet<u8>) -> u32 {
    buttons.iter().fold(0u32, |mask, button| {
        mask | 1u32.checked_shl(u32::from(button.saturating_sub(1))).unwrap_or(0)
    })
}

struct GuiUpdate<'a> {
    active: bool,
    cursor: Point,
    delta: Point,
    key_count: u32,
    button_mask: u32,
    wheel: Point,
    event: &'a str,
}

fn update_gui(update: &GuiUpdate<'_>) {
    let event = CString::new(update.event.replace('\0', " "))
        .unwrap_or_else(|_| CString::new("无法显示事件").unwrap());
    unsafe {
        synly_input_mock_gui_update(
            update.active,
            update.cursor.x,
            update.cursor.y,
            update.delta.x,
            update.delta.y,
            update.key_count,
            update.button_mask,
            update.wheel.x,
            update.wheel.y,
            event.as_ptr(),
        );
    }
}

#[link(name = "macos_audio", kind = "static")]
unsafe extern "C" {
    fn synly_input_mock_gui_prepare(width: i32, height: i32, source_edge: *const std::ffi::c_char) -> i32;
    fn synly_input_mock_gui_run();
    fn synly_input_mock_gui_is_running() -> bool;
    fn synly_input_mock_gui_stop();
    fn synly_input_mock_gui_update(
        active: bool,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
        key_count: u32,
        button_mask: u32,
        wheel_x: i32,
        wheel_y: i32,
        event: *const std::ffi::c_char,
    );
}
