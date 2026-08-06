use super::platform;
use super::protocol::InputMessage;
use super::runtime;
use super::{
    CursorMode, DesktopLayout, Hotkey, InputMode, InputPlatform, InputRuntimeOptions,
    KeyMappingConfig, KeySnapshot, ModifierMask, ScreenEdge,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{self, Instant, MissedTickBehavior};

const MAX_MOCK_FRAME_LEN: usize = 64 * 1024;
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(3);
const FINISH_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone, Debug)]
pub struct ReceiverMockOptions {
    pub listen: SocketAddr,
    pub hotkey: Hotkey,
    pub cursor_mode: CursorMode,
    pub elevated: bool,
}

#[derive(Clone, Debug)]
pub struct ControllerMockOptions {
    pub address: SocketAddr,
    pub edge: ScreenEdge,
    pub motion_steps: u16,
    pub step_delay: Duration,
    pub inject_click: bool,
    pub inject_keyboard: bool,
    pub inject_wheel: bool,
}

#[derive(Clone, Debug)]
pub struct InteractiveControllerOptions {
    pub address: SocketAddr,
    pub edge: ScreenEdge,
    pub motion_step: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
enum MockFrame {
    Input(InputMessage),
    Finish,
}

struct ControllerChannel {
    outgoing: mpsc::Sender<MockFrame>,
    incoming: mpsc::Receiver<Result<MockFrame>>,
    writer_task: tokio::task::JoinHandle<Result<()>>,
    reader_task: tokio::task::JoinHandle<()>,
}

const ESC_USAGE: u16 = 0x29;
const DIRECTION_UP_USAGE: u16 = 0x52;
const DIRECTION_DOWN_USAGE: u16 = 0x51;
const DIRECTION_LEFT_USAGE: u16 = 0x50;
const DIRECTION_RIGHT_USAGE: u16 = 0x4f;

/// 方向键 usage 到相对位移的映射, 非方向键返回 None.
fn direction_motion(usage: u16, step: u32) -> Option<(i32, i32)> {
    let step = step.min(i32::MAX as u32) as i32;
    match usage {
        DIRECTION_UP_USAGE => Some((0, -step)),
        DIRECTION_DOWN_USAGE => Some((0, step)),
        DIRECTION_LEFT_USAGE => Some((-step, 0)),
        DIRECTION_RIGHT_USAGE => Some((step, 0)),
        _ => None,
    }
}

async fn open_controller_channel(address: SocketAddr) -> Result<ControllerChannel> {
    let stream = TcpStream::connect(address)
        .await
        .with_context(|| format!("无法连接真实输入被控端 {address}"))?;
    stream.set_nodelay(true)?;
    let (mut reader, mut writer) = stream.into_split();
    let mock_layout = DesktopLayout::new(vec![super::DisplayRect {
        x: 0,
        y: 0,
        width: 1440,
        height: 900,
    }])?;
    write_mock_frame(
        &mut writer,
        &MockFrame::Input(InputMessage::Hello {
            platform: InputPlatform::current(),
            layout: mock_layout,
        }),
    )
    .await?;
    let remote_layout = match read_mock_frame(&mut reader).await? {
        MockFrame::Input(InputMessage::Hello { layout, .. }) => layout,
        _ => bail!("真实输入被控端未先发送桌面布局"),
    };
    tracing::info!(
        address = %address,
        displays = ?remote_layout.displays,
        "mock 控制端布局交换完成"
    );
    let (outgoing, mut outgoing_rx) = mpsc::channel::<MockFrame>(256);
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = outgoing_rx.recv().await {
            write_mock_frame(&mut writer, &frame).await?;
        }
        Result::<()>::Ok(())
    });
    let (incoming, reader_task) = spawn_controller_reader(reader);
    Ok(ControllerChannel {
        outgoing,
        incoming,
        writer_task,
        reader_task,
    })
}

pub async fn run_receiver_mock(options: ReceiverMockOptions) -> Result<()> {
    super::ensure_platform_supported(InputMode::Receive)?;
    if options.elevated {
        #[cfg(windows)]
        {
            tracing::info!("--elevated 已启用, 请求 Windows 输入管理员代理");
            super::request_windows_input_elevation()?;
        }
        #[cfg(not(windows))]
        bail!("--elevated 目前只支持 Windows 提权输入代理");
    }

    let mut platform = platform::start(InputMode::Receive, options.hotkey)?;
    let local_layout = platform.backend.layout()?;
    let listener = TcpListener::bind(options.listen)
        .await
        .with_context(|| format!("无法监听被控端 mock 地址 {}", options.listen))?;
    tracing::info!(
        listen = %options.listen,
        displays = ?local_layout.displays,
        "真实输入被控端已就绪"
    );

    let (stream, remote_addr) = listener.accept().await?;
    stream.set_nodelay(true)?;
    tracing::info!(%remote_addr, "mock 控制端已连接");
    let (mut reader, mut writer) = stream.into_split();
    write_mock_frame(
        &mut writer,
        &MockFrame::Input(InputMessage::Hello {
            platform: InputPlatform::current(),
            layout: local_layout.clone(),
        }),
    )
    .await?;
    let remote_layout = match read_mock_frame(&mut reader).await? {
        MockFrame::Input(InputMessage::Hello { layout, .. }) => layout,
        _ => bail!("mock 控制端未先发送桌面布局"),
    };
    tracing::info!(
        local_displays = local_layout.displays.len(),
        remote_displays = remote_layout.displays.len(),
        "被控端 mock 布局交换完成"
    );

    let (outgoing, mut outgoing_rx) = mpsc::channel::<InputMessage>(256);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = outgoing_rx.recv().await {
            write_mock_frame(&mut writer, &MockFrame::Input(message)).await?;
        }
        Result::<()>::Ok(())
    });
    let (mut incoming, incoming_motion, finish, reader_task) = spawn_receiver_reader(reader);
    let mut finish = Box::pin(finish);
    let input_options = InputRuntimeOptions {
        mode: InputMode::Receive,
        edge: ScreenEdge::Left,
        hotkey: options.hotkey,
        reverse_mouse_wheel: false,
        reverse_trackpad: false,
        native_scroll_macos_to_windows: false,
        native_scroll_windows_to_macos: false,
        block_switch_on_press: false,
        filter_app_events: false,
        key_mapping: KeyMappingConfig::default(),
        cursor_mode: options.cursor_mode,
    };
    let result = {
        let session = runtime::run_receiver(
            &mut incoming,
            &incoming_motion,
            &outgoing,
            &mut platform,
            local_layout,
            &input_options,
            platform::foreground_cursor_captured,
        );
        tokio::pin!(session);
        tokio::select! {
            result = &mut session => result,
            result = &mut finish => {
                result.context("mock 控制端未正常结束测试")?;
                Ok(())
            }
        }
    };

    let final_cursor = platform.backend.cursor_position();
    let _ = platform.backend.release_all();
    reader_task.abort();
    let _ = reader_task.await;
    drop(outgoing);
    writer_task.abort();
    let _ = writer_task.await;
    match final_cursor {
        Ok(cursor) => tracing::info!(?cursor, "真实输入被控端测试完成"),
        Err(error) => tracing::warn!(error = %error, "被控端完成测试但无法读取最终光标"),
    }
    result
}

pub async fn run_controller_mock(options: ControllerMockOptions) -> Result<()> {
    if options.motion_steps == 0 {
        bail!("motion_steps 必须大于 0");
    }
    let mut channel = open_controller_channel(options.address).await?;
    let generation = Arc::new(AtomicU64::new(1));
    let heartbeat_task = spawn_controller_heartbeat(
        channel.outgoing.clone(),
        Arc::clone(&generation),
    );
    let started = Instant::now();

    send_input(
        &channel.outgoing,
        InputMessage::Activate {
            generation: 1,
            source_edge: options.edge,
            edge_position: 0.5,
            pressed: empty_snapshot(),
        },
    )
    .await?;
    let first_pressure_steps = wait_for_receiver_heartbeat_under_motion(
        &mut channel.incoming,
        &channel.outgoing,
        options.edge,
        1,
        options.step_delay,
    )
    .await?;
    tracing::info!(
        generation = 1,
        pressure_steps = first_pressure_steps,
        "真实被控端已确认第一次接管"
    );
    send_full_input_sequence(&channel.outgoing, &options, 1).await?;
    send_input(&channel.outgoing, InputMessage::Deactivate {
        generation: 1,
        edge_position: None,
    })
    .await?;

    generation.store(2, Ordering::Release);
    send_input(
        &channel.outgoing,
        InputMessage::Activate {
            generation: 2,
            source_edge: options.edge,
            edge_position: 0.5,
            pressed: empty_snapshot(),
        },
    )
    .await?;
    let second_pressure_steps = wait_for_receiver_heartbeat_under_motion(
        &mut channel.incoming,
        &channel.outgoing,
        options.edge,
        2,
        options.step_delay,
    )
    .await?;
    tracing::info!(
        generation = 2,
        pressure_steps = second_pressure_steps,
        "真实被控端已确认重新接管"
    );
    send_input(
        &channel.outgoing,
        motion_message(options.edge, 2, 4),
    )
    .await?;
    send_input(&channel.outgoing, InputMessage::Deactivate {
        generation: 2,
        edge_position: None,
    })
    .await?;
    time::sleep(FINISH_DELAY).await;

    heartbeat_task.abort();
    let _ = heartbeat_task.await;
    channel.outgoing.send(MockFrame::Finish).await?;
    drop(channel.outgoing);
    channel.writer_task.await??;
    channel.reader_task.abort();
    let _ = channel.reader_task.await;
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        motion_steps = options.motion_steps,
        click = options.inject_click,
        keyboard = options.inject_keyboard,
        wheel = options.inject_wheel,
        "mock 控制端完整输入序列已完成"
    );
    Ok(())
}

pub async fn run_controller_mock_interactive(options: InteractiveControllerOptions) -> Result<()> {
    if options.motion_step == 0 {
        bail!("motion_step 必须大于 0");
    }
    let mut platform = platform::start(InputMode::Send, Hotkey::DEFAULT.parse()?)?;
    platform.backend.set_keyboard_capture(true)?;
    let mut channel = open_controller_channel(options.address).await?;
    let generation = Arc::new(AtomicU64::new(1));
    let heartbeat_task = spawn_controller_heartbeat(
        channel.outgoing.clone(),
        Arc::clone(&generation),
    );
    let mut active = true;
    send_input(
        &channel.outgoing,
        InputMessage::Activate {
            generation: 1,
            source_edge: options.edge,
            edge_position: 0.5,
            pressed: empty_snapshot(),
        },
    )
    .await?;
    let started = Instant::now();

    // 等待被控端确认接管, 期间若立即请求返回则批准并等待方向键重新接管.
    time::timeout(CONFIRM_TIMEOUT, async {
        loop {
            match channel.incoming.recv().await.context("被控端读取任务已停止")?? {
                MockFrame::Input(InputMessage::Heartbeat {
                    generation: 1,
                }) => break,
                MockFrame::Input(InputMessage::ReturnRequest {
                    generation: 1,
                    edge_position,
                }) => {
                    send_input(
                        &channel.outgoing,
                        InputMessage::Return {
                            generation: 1,
                            edge_position,
                        },
                    )
                    .await?;
                    active = false;
                    break;
                }
                MockFrame::Finish => bail!("被控端提前结束测试"),
                MockFrame::Input(_) => {}
            }
        }
        Result::<()>::Ok(())
    })
    .await
    .context("等待被控端确认接管超时")??;
    tracing::info!(
        edge = ?options.edge,
        step = options.motion_step,
        "交互控制已启动: 方向键移动被控端光标, Esc 退出"
    );

    let result: Result<()> = async {
        loop {
            tokio::select! {
                event = platform.events.recv() => {
                    let Some(event) = event else {
                        bail!("本机按键捕获事件流已结束");
                    };
                    match event {
                        platform::NativeEvent::Key { usage, down, repeat, .. } => {
                            if let Some((dx, dy)) = direction_motion(usage, options.motion_step) {
                                if down || repeat {
                                    let current = ensure_active(
                                        &channel.outgoing,
                                        &generation,
                                        &mut active,
                                        options.edge,
                                    )
                                    .await?;
                                    send_input(
                                        &channel.outgoing,
                                        InputMessage::Motion {
                                            generation: current,
                                            dx,
                                            dy,
                                        },
                                    )
                                    .await?;
                                }
                            } else if usage == ESC_USAGE && down {
                                tracing::info!("Esc 已按下, 结束交互控制");
                                return Ok(());
                            }
                        }
                        platform::NativeEvent::Emergency => {
                            tracing::info!("紧急热键已触发, 结束交互控制");
                            return Ok(());
                        }
                        platform::NativeEvent::Failed(message) => {
                            bail!("本机按键捕获失败: {message}")
                        }
                        platform::NativeEvent::ReliableQueueOverflow => {
                            bail!("本机按键捕获事件队列已满")
                        }
                        _ => {}
                    }
                }
                frame = channel.incoming.recv() => {
                    match frame.context("被控端读取任务已停止")?? {
                        MockFrame::Input(InputMessage::ReturnRequest {
                            generation: incoming_generation,
                            edge_position,
                        }) => {
                            if active && incoming_generation == generation.load(Ordering::Acquire) {
                                send_input(
                                    &channel.outgoing,
                                    InputMessage::Return {
                                        generation: incoming_generation,
                                        edge_position,
                                    },
                                )
                                .await?;
                                active = false;
                                tracing::info!(
                                    generation = incoming_generation,
                                    "被控端请求边缘返回, 已批准; 按方向键重新接管"
                                );
                            }
                        }
                        MockFrame::Input(InputMessage::Return { .. }) => {
                            active = false;
                        }
                        MockFrame::Input(InputMessage::Deactivate {
                            generation: incoming_generation,
                            ..
                        }) => {
                            if incoming_generation == generation.load(Ordering::Acquire) {
                                active = false;
                                tracing::info!(
                                    generation = incoming_generation,
                                    "被控端已释放控制, 按方向键重新接管"
                                );
                            }
                        }
                        MockFrame::Finish => bail!("被控端提前结束测试"),
                        MockFrame::Input(_) => {}
                    }
                }
            }
        }
    }
    .await;

    if active {
        let _ = send_input(
            &channel.outgoing,
            InputMessage::Deactivate {
                generation: generation.load(Ordering::Acquire),
                edge_position: None,
            },
        )
        .await;
    }
    heartbeat_task.abort();
    let _ = heartbeat_task.await;
    let _ = channel.outgoing.send(MockFrame::Finish).await;
    drop(channel.outgoing);
    let _ = channel.writer_task.await;
    channel.reader_task.abort();
    let _ = channel.reader_task.await;
    let _ = platform.backend.set_keyboard_capture(false);
    let _ = platform.backend.release_all();
    tracing::info!(
        elapsed_ms = started.elapsed().as_millis(),
        "交互控制已结束"
    );
    result
}

async fn ensure_active(
    outgoing: &mpsc::Sender<MockFrame>,
    generation: &AtomicU64,
    active: &mut bool,
    edge: ScreenEdge,
) -> Result<u64> {
    if *active {
        return Ok(generation.load(Ordering::Acquire));
    }
    let next = generation.fetch_add(1, Ordering::AcqRel) + 1;
    send_input(
        outgoing,
        InputMessage::Activate {
            generation: next,
            source_edge: edge,
            edge_position: 0.5,
            pressed: empty_snapshot(),
        },
    )
    .await?;
    *active = true;
    tracing::info!(generation = next, "按方向键重新接管被控端");
    Ok(next)
}

async fn send_full_input_sequence(
    outgoing: &mpsc::Sender<MockFrame>,
    options: &ControllerMockOptions,
    generation: u64,
) -> Result<()> {
    for step in 0..options.motion_steps {
        send_input(
            outgoing,
            motion_message(options.edge, generation, 4),
        )
        .await?;
        time::sleep(options.step_delay).await;
        if (step + 1) % 500 == 0 {
            tracing::info!(
                generation,
                completed_steps = step + 1,
                total_steps = options.motion_steps,
                "mock 控制端持续运动测试进度"
            );
        }
    }
    if options.inject_keyboard {
        send_input(
            outgoing,
            InputMessage::Key {
                generation,
                usage: 0xe1,
                modifiers: ModifierMask::SHIFT,
                down: true,
                repeat: false,
            },
        )
        .await?;
        send_input(
            outgoing,
            InputMessage::Key {
                generation,
                usage: 0xe1,
                modifiers: ModifierMask::SHIFT,
                down: false,
                repeat: false,
            },
        )
        .await?;
    }
    if options.inject_click {
        send_input(
            outgoing,
            InputMessage::Button {
                generation,
                button: 1,
                down: true,
            },
        )
        .await?;
        send_input(
            outgoing,
            InputMessage::Button {
                generation,
                button: 1,
                down: false,
            },
        )
        .await?;
    }
    if options.inject_wheel {
        send_input(
            outgoing,
            InputMessage::Wheel {
                generation,
                x: 0,
                y: 120,
            },
        )
        .await?;
    }
    Ok(())
}

fn motion_message(edge: ScreenEdge, generation: u64, distance: i32) -> InputMessage {
    let (dx, dy) = inward_motion(edge, distance);
    InputMessage::Motion {
        generation,
        dx,
        dy,
    }
}

fn inward_motion(edge: ScreenEdge, distance: i32) -> (i32, i32) {
    match edge {
        ScreenEdge::Right => (distance, 0),
        ScreenEdge::Left => (-distance, 0),
        ScreenEdge::Top => (0, -distance),
        ScreenEdge::Bottom => (0, distance),
    }
}

fn empty_snapshot() -> KeySnapshot {
    KeySnapshot {
        usages: Vec::new(),
        modifiers: ModifierMask::default(),
        buttons: Vec::new(),
    }
}

async fn send_input(
    outgoing: &mpsc::Sender<MockFrame>,
    message: InputMessage,
) -> Result<()> {
    outgoing
        .send(MockFrame::Input(message))
        .await
        .context("mock 控制端写入队列已关闭")
}

async fn wait_for_receiver_heartbeat_under_motion(
    incoming: &mut mpsc::Receiver<Result<MockFrame>>,
    outgoing: &mpsc::Sender<MockFrame>,
    edge: ScreenEdge,
    generation: u64,
    step_delay: Duration,
) -> Result<u64> {
    let mut pressure_steps = 0u64;
    let mut pressure = time::interval(step_delay.max(Duration::from_millis(1)));
    pressure.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let deadline = time::sleep(CONFIRM_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            frame = incoming.recv() => {
                match frame.context("真实被控端读取任务已停止")?? {
                    MockFrame::Input(InputMessage::Heartbeat {
                        generation: incoming_generation,
                    }) if incoming_generation == generation => break Ok(pressure_steps),
                    MockFrame::Input(InputMessage::Return { .. }) => {
                        break Err(anyhow::anyhow!("真实被控端提前返回了控制权"));
                    }
                    MockFrame::Finish => {
                        break Err(anyhow::anyhow!("真实被控端提前结束了测试"));
                    }
                    _ => {}
                }
            }
            _ = pressure.tick() => {
                send_input(outgoing, motion_message(edge, generation, 4)).await?;
                pressure_steps = pressure_steps.saturating_add(1);
            }
            _ = &mut deadline => break Err(anyhow::anyhow!("等待真实被控端确认接管超时")),
        }
    }
}

fn spawn_controller_heartbeat(
    outgoing: mpsc::Sender<MockFrame>,
    generation: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut heartbeat = time::interval(HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            heartbeat.tick().await;
            let message = InputMessage::Heartbeat {
                generation: generation.load(Ordering::Acquire),
            };
            if outgoing.send(MockFrame::Input(message)).await.is_err() {
                break;
            }
        }
    })
}

fn spawn_receiver_reader<R>(
    mut reader: R,
) -> (
    mpsc::Receiver<Result<InputMessage>>,
    Arc<runtime::IncomingMotion>,
    oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (incoming_tx, incoming_rx) = mpsc::channel(256);
    let motion = Arc::new(runtime::IncomingMotion::default());
    let reader_motion = Arc::clone(&motion);
    let (finish_tx, finish_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            match read_mock_frame(&mut reader).await {
                Ok(MockFrame::Input(InputMessage::Motion { generation, dx, dy })) => {
                    reader_motion.push(generation, dx, dy);
                }
                Ok(MockFrame::Input(message)) => {
                    if !matches!(message, InputMessage::Heartbeat { .. })
                        && let Some(motion) = reader_motion.take()
                        && incoming_tx
                            .send(Ok(InputMessage::Motion {
                                generation: motion.generation,
                                dx: motion.dx,
                                dy: motion.dy,
                            }))
                            .await
                            .is_err()
                    {
                        break;
                    }
                    if incoming_tx.send(Ok(message)).await.is_err() {
                        break;
                    }
                }
                Ok(MockFrame::Finish) => {
                    let _ = finish_tx.send(());
                    break;
                }
                Err(error) => {
                    let _ = incoming_tx.send(Err(error)).await;
                    break;
                }
            }
        }
    });
    (incoming_rx, motion, finish_rx, task)
}

fn spawn_controller_reader<R>(
    mut reader: R,
) -> (
    mpsc::Receiver<Result<MockFrame>>,
    tokio::task::JoinHandle<()>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(256);
    let task = tokio::spawn(async move {
        loop {
            match read_mock_frame(&mut reader).await {
                Ok(frame) => {
                    if tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                    break;
                }
            }
        }
    });
    (rx, task)
}

async fn write_mock_frame<W>(writer: &mut W, frame: &MockFrame) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let bytes = bincode::serialize(frame).context("无法编码输入接收 mock 帧")?;
    if bytes.is_empty() || bytes.len() > MAX_MOCK_FRAME_LEN {
        bail!("输入接收 mock 帧长度无效: {}", bytes.len());
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_mock_frame<R>(reader: &mut R) -> Result<MockFrame>
where
    R: AsyncRead + Unpin,
{
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > MAX_MOCK_FRAME_LEN {
        bail!("输入接收 mock 帧长度无效: {length}");
    }
    let mut bytes = vec![0u8; length];
    reader.read_exact(&mut bytes).await?;
    bincode::deserialize(&bytes).context("无法解码输入接收 mock 帧")
}

#[cfg(test)]
mod tests {
    use super::{
        DIRECTION_DOWN_USAGE, DIRECTION_LEFT_USAGE, DIRECTION_RIGHT_USAGE, DIRECTION_UP_USAGE,
        ESC_USAGE, MockFrame, direction_motion, inward_motion, read_mock_frame, write_mock_frame,
    };
    use crate::input::protocol::InputMessage;
    use crate::input::ScreenEdge;
    use tokio::io::duplex;

    #[test]
    fn mock_motion_moves_inward_from_each_source_edge() {
        assert_eq!(inward_motion(ScreenEdge::Right, 4), (4, 0));
        assert_eq!(inward_motion(ScreenEdge::Left, 4), (-4, 0));
        assert_eq!(inward_motion(ScreenEdge::Top, 4), (0, -4));
        assert_eq!(inward_motion(ScreenEdge::Bottom, 4), (0, 4));
    }

    #[test]
    fn direction_motion_maps_arrow_keys_to_relative_deltas() {
        assert_eq!(
            direction_motion(DIRECTION_UP_USAGE, 4),
            Some((0, -4))
        );
        assert_eq!(
            direction_motion(DIRECTION_DOWN_USAGE, 4),
            Some((0, 4))
        );
        assert_eq!(
            direction_motion(DIRECTION_LEFT_USAGE, 4),
            Some((-4, 0))
        );
        assert_eq!(
            direction_motion(DIRECTION_RIGHT_USAGE, 4),
            Some((4, 0))
        );
        assert_eq!(direction_motion(ESC_USAGE, 4), None);
        assert_eq!(direction_motion(0x04, 4), None);
    }

    #[tokio::test]
    async fn mock_wire_preserves_input_and_finish_frames() {
        let (mut writer, mut reader) = duplex(1024);
        let task = tokio::spawn(async move {
            write_mock_frame(
                &mut writer,
                &MockFrame::Input(InputMessage::Heartbeat { generation: 7 }),
            )
            .await
            .unwrap();
            write_mock_frame(&mut writer, &MockFrame::Finish)
                .await
                .unwrap();
        });

        assert_eq!(
            read_mock_frame(&mut reader).await.unwrap(),
            MockFrame::Input(InputMessage::Heartbeat { generation: 7 })
        );
        assert_eq!(read_mock_frame(&mut reader).await.unwrap(), MockFrame::Finish);
        task.await.unwrap();
    }
}
