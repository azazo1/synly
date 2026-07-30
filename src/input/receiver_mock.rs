use super::platform;
use super::protocol::InputMessage;
use super::runtime;
use super::{
    DesktopLayout, Hotkey, InputMode, KeySnapshot, ModifierMask, ScreenEdge,
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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
enum MockFrame {
    Input(InputMessage),
    Finish,
}

pub async fn run_receiver_mock(options: ReceiverMockOptions) -> Result<()> {
    #[cfg(windows)]
    super::windows_agent::request_elevation()?;
    super::ensure_platform_supported(InputMode::Receive)?;

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
        &MockFrame::Input(InputMessage::Layout(local_layout.clone())),
    )
    .await?;
    let remote_layout = match read_mock_frame(&mut reader).await? {
        MockFrame::Input(InputMessage::Layout(layout)) => layout,
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
    let (mut incoming, finish, reader_task) = spawn_receiver_reader(reader);
    let mut finish = Box::pin(finish);
    let result = {
        let session = runtime::run_receiver(
            &mut incoming,
            &outgoing,
            &mut platform,
            local_layout,
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
    let stream = TcpStream::connect(options.address)
        .await
        .with_context(|| format!("无法连接真实输入被控端 {}", options.address))?;
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
        &MockFrame::Input(InputMessage::Layout(mock_layout)),
    )
    .await?;
    let remote_layout = match read_mock_frame(&mut reader).await? {
        MockFrame::Input(InputMessage::Layout(layout)) => layout,
        _ => bail!("真实输入被控端未先发送桌面布局"),
    };
    tracing::info!(
        address = %options.address,
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
    let (mut incoming, reader_task) = spawn_controller_reader(reader);
    let generation = Arc::new(AtomicU64::new(1));
    let heartbeat_task = spawn_controller_heartbeat(
        outgoing.clone(),
        Arc::clone(&generation),
    );
    let started = Instant::now();

    send_input(
        &outgoing,
        InputMessage::Activate {
            generation: 1,
            source_edge: options.edge,
            edge_position: 0.5,
            pressed: empty_snapshot(),
        },
    )
    .await?;
    wait_for_receiver_heartbeat(&mut incoming, 1).await?;
    tracing::info!(generation = 1, "真实被控端已确认第一次接管");
    send_full_input_sequence(&outgoing, &options, 1).await?;
    send_input(&outgoing, InputMessage::Deactivate { generation: 1 }).await?;

    generation.store(2, Ordering::Release);
    send_input(
        &outgoing,
        InputMessage::Activate {
            generation: 2,
            source_edge: options.edge,
            edge_position: 0.5,
            pressed: empty_snapshot(),
        },
    )
    .await?;
    wait_for_receiver_heartbeat(&mut incoming, 2).await?;
    tracing::info!(generation = 2, "真实被控端已确认重新接管");
    send_input(
        &outgoing,
        motion_message(options.edge, 2, 4),
    )
    .await?;
    send_input(&outgoing, InputMessage::Deactivate { generation: 2 }).await?;
    time::sleep(FINISH_DELAY).await;

    heartbeat_task.abort();
    let _ = heartbeat_task.await;
    outgoing.send(MockFrame::Finish).await?;
    drop(outgoing);
    writer_task.await??;
    reader_task.abort();
    let _ = reader_task.await;
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

async fn send_full_input_sequence(
    outgoing: &mpsc::Sender<MockFrame>,
    options: &ControllerMockOptions,
    generation: u64,
) -> Result<()> {
    for _ in 0..options.motion_steps {
        send_input(
            outgoing,
            motion_message(options.edge, generation, 4),
        )
        .await?;
        time::sleep(options.step_delay).await;
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

async fn wait_for_receiver_heartbeat(
    incoming: &mut mpsc::Receiver<Result<MockFrame>>,
    generation: u64,
) -> Result<()> {
    time::timeout(CONFIRM_TIMEOUT, async {
        loop {
            match incoming.recv().await.context("真实被控端读取任务已停止")?? {
                MockFrame::Input(InputMessage::Heartbeat {
                    generation: incoming_generation,
                }) if incoming_generation == generation => break Ok(()),
                MockFrame::Input(InputMessage::Return { .. }) => {
                    break Err(anyhow::anyhow!("真实被控端提前返回了控制权"));
                }
                MockFrame::Finish => {
                    break Err(anyhow::anyhow!("真实被控端提前结束了测试"));
                }
                _ => {}
            }
        }
    })
    .await
    .context("等待真实被控端确认接管超时")?
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
    oneshot::Receiver<()>,
    tokio::task::JoinHandle<()>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (incoming_tx, incoming_rx) = mpsc::channel(256);
    let (finish_tx, finish_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        loop {
            match read_mock_frame(&mut reader).await {
                Ok(MockFrame::Input(message)) => {
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
    (incoming_rx, finish_rx, task)
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
    use super::{MockFrame, inward_motion, read_mock_frame, write_mock_frame};
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
