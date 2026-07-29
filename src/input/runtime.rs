use super::channel::{InputChannelOffer, InputHostChannel};
use super::platform::{self, NativeEvent};
use super::protocol::{InputMessage, read_message, write_message};
use super::{Hotkey, InputMode, KeySnapshot, LocalInputRole, ScreenEdge};
use anyhow::{Context, Result, bail};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::{self, Instant, MissedTickBehavior};
use tokio_rustls::TlsStream;

const MOTION_INTERVAL: Duration = Duration::from_micros(8_333);
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(1);
const RETURN_COOLDOWN: Duration = Duration::from_millis(300);
const EDGE_INSET: i32 = 8;
const JUMP_ZONE_SIZE: i32 = 1;
const OVERFLOW_POLL: Duration = Duration::from_millis(50);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(20);
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct InputRuntimeOptions {
    pub mode: InputMode,
    pub edge: ScreenEdge,
    pub hotkey: Hotkey,
}

pub enum InputSessionContext {
    Host {
        channel: InputHostChannel,
        sockets: mpsc::Receiver<TcpStream>,
    },
    Client {
        offer: InputChannelOffer,
        remote_addr: SocketAddr,
    },
}

impl InputSessionContext {
    pub fn host(channel: InputHostChannel, sockets: mpsc::Receiver<TcpStream>) -> Self {
        Self::Host { channel, sockets }
    }

    pub fn client(offer: InputChannelOffer, remote_addr: SocketAddr) -> Self {
        Self::Client { offer, remote_addr }
    }
}

pub async fn run_input_session(
    context: InputSessionContext,
    master_secret: [u8; 32],
    local_role: LocalInputRole,
    options: InputRuntimeOptions,
) -> Result<()> {
    let mut platform = platform::start(options.mode, options.hotkey)?;
    tracing::info!(role = ?local_role, "输入同步运行时已启动");
    match context {
        InputSessionContext::Host {
            channel,
            mut sockets,
        } => {
            loop {
                let socket = sockets
                    .recv()
                    .await
                    .context("输入辅助连接等待期间主会话已结束")?;
                match time::timeout(AUTH_TIMEOUT, channel.accept(socket, &master_secret)).await {
                    Ok(Ok(stream)) => {
                        if let Err(err) = run_established(
                            stream,
                            local_role,
                            options.edge,
                            &mut platform,
                        )
                        .await
                        {
                            cleanup_platform(&platform);
                            if platform_is_terminal(&platform) {
                                return Err(err);
                            }
                            tracing::warn!(error = %err, "输入辅助连接已断开, 等待重连");
                        }
                    }
                    Ok(Err(err)) => {
                        cleanup_platform(&platform);
                        tracing::warn!(error = %err, "输入辅助连接认证失败");
                    }
                    Err(_) => {
                        cleanup_platform(&platform);
                        tracing::warn!("输入辅助连接认证超时");
                    }
                }
            }
        }
        InputSessionContext::Client { offer, remote_addr } => {
            let mut delay = RECONNECT_MIN;
            loop {
                let established = match time::timeout(
                    AUTH_TIMEOUT,
                    super::channel::connect(remote_addr, &offer, &master_secret),
                )
                .await
                {
                    Ok(Ok(stream)) => {
                        delay = RECONNECT_MIN;
                        run_established(stream, local_role, options.edge, &mut platform).await
                    }
                    Ok(Err(err)) => Err(err),
                    Err(_) => Err(anyhow::anyhow!("输入辅助连接认证超时")),
                };
                cleanup_platform(&platform);
                if let Err(err) = established {
                    if platform_is_terminal(&platform) {
                        return Err(err);
                    }
                    tracing::warn!(error = %err, retry_secs = delay.as_secs(), "输入辅助连接将在退避后重连");
                    time::sleep(delay).await;
                    delay = Duration::from_secs(
                        delay.as_secs().saturating_mul(2).min(RECONNECT_MAX.as_secs()),
                    );
                }
            }
        }
    }
}

fn cleanup_platform(platform: &platform::PlatformHandle) {
    let _ = platform.backend.set_capture(false);
    let _ = platform.backend.release_all();
}

fn platform_is_terminal(platform: &platform::PlatformHandle) -> bool {
    platform.failed.load(std::sync::atomic::Ordering::Acquire)
        || platform
            .overflowed
            .load(std::sync::atomic::Ordering::Acquire)
}

async fn run_established(
    stream: TlsStream<TcpStream>,
    local_role: LocalInputRole,
    source_edge: ScreenEdge,
    platform: &mut platform::PlatformHandle,
) -> Result<()> {
    let local_layout = platform.backend.layout()?;
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<InputMessage>(256);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            write_message(&mut writer, &message).await?;
        }
        Result::<()>::Ok(())
    });
    let _writer_abort = AbortOnDrop(writer_task.abort_handle());
    tx.send(InputMessage::Layout(local_layout.clone())).await?;
    let remote_layout = match read_message(&mut reader).await? {
        InputMessage::Layout(layout) => layout,
        InputMessage::Proof { .. } => bail!("输入通道认证完成后收到了重复证明"),
        _ => bail!("输入通道在布局交换前收到了事件"),
    };
    tracing::info!(
        local_displays = local_layout.displays.len(),
        remote_displays = remote_layout.displays.len(),
        "输入通道布局交换完成"
    );

    // 读取帧必须由单独任务连续完成, 避免 select 取消半包读取后破坏帧边界.
    let (mut incoming, reader_task) = spawn_input_reader(reader);
    let _reader_abort = AbortOnDrop(reader_task.abort_handle());

    let session = match local_role {
        LocalInputRole::Send => run_sender(
            &mut incoming,
            &tx,
            platform,
            local_layout,
            remote_layout,
            source_edge,
        )
        .await,
        LocalInputRole::Receive => {
            run_receiver(&mut incoming, &tx, platform, local_layout).await
        }
    };
    reader_task.abort();
    let _ = reader_task.await;
    drop(tx);
    if session.is_err() {
        writer_task.abort();
        let _ = writer_task.await;
        return session;
    }
    match writer_task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) if session.is_err() => {
            tracing::debug!(error = %err, "输入通道写入任务随会话错误结束");
        }
        Ok(Err(err)) => return Err(err),
        Err(err) => return Err(err.into()),
    }
    session
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn spawn_input_reader<R>(
    mut reader: R,
) -> (
    mpsc::Receiver<Result<InputMessage>>,
    tokio::task::JoinHandle<()>,
)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(256);
    let task = tokio::spawn(async move {
        loop {
            match read_message(&mut reader).await {
                Ok(message) => {
                    if tx.send(Ok(message)).await.is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            }
        }
    });
    (rx, task)
}

async fn run_sender(
    incoming: &mut mpsc::Receiver<Result<InputMessage>>,
    tx: &mpsc::Sender<InputMessage>,
    platform: &mut platform::PlatformHandle,
    local_layout: super::DesktopLayout,
    _remote_layout: super::DesktopLayout,
    source_edge: ScreenEdge,
) -> Result<()> {
    let mut generation = 0u64;
    let mut active = false;
    let mut recovery = SenderRecoveryGuard::new(
        Arc::clone(&platform.backend),
        local_layout.clone(),
        source_edge,
    );
    let mut cooldown_until = Instant::now();
    let mut last_heartbeat = Instant::now();
    let mut motion_tick = time::interval(MOTION_INTERVAL);
    motion_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut heartbeat = time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut overflow_poll = time::interval(OVERFLOW_POLL);
    let mut timeout_tick = time::interval(HEARTBEAT_INTERVAL);

    loop {
        tokio::select! {
            message = incoming.recv() => {
                let message = message.context("输入辅助读取任务已停止")??;
                match message {
                    InputMessage::Return { generation: remote_generation, edge_position }
                        if active && remote_generation == generation => {
                            deactivate_sender(platform, &local_layout, source_edge, edge_position)?;
                            active = false;
                            recovery.disarm();
                            cooldown_until = Instant::now() + RETURN_COOLDOWN;
                            tracing::info!(generation, "控制已从对端返回本机");
                        }
                    InputMessage::Deactivate { generation: remote_generation }
                        if active && remote_generation == generation => {
                            recovery.recover();
                            active = false;
                            cooldown_until = Instant::now() + RETURN_COOLDOWN;
                        }
                    InputMessage::Heartbeat { generation: remote_generation }
                        if !active || remote_generation == generation => {
                        last_heartbeat = Instant::now();
                    }
                    InputMessage::Layout(_) => {}
                    InputMessage::Proof { .. } => bail!("输入通道收到重复认证消息"),
                    _ => {}
                }
            }
            Some(event) = platform.events.recv() => {
                match event {
                    NativeEvent::Emergency => {
                        if active {
                            recovery.recover();
                            active = false;
                            cooldown_until = Instant::now() + RETURN_COOLDOWN;
                            let _ = tx.try_send(InputMessage::Deactivate { generation });
                            tracing::info!(generation, "紧急热键已收回本机控制");
                        }
                    }
                    NativeEvent::Key { usage, modifiers, down, repeat } if active => {
                        enqueue_message(tx, InputMessage::Key { generation, usage, modifiers, down, repeat })?;
                    }
                    NativeEvent::Button { button, down } if active => {
                        enqueue_message(tx, InputMessage::Button { generation, button, down })?;
                    }
                    NativeEvent::Wheel { x, y } if active => {
                        enqueue_message(tx, InputMessage::Wheel { generation, x, y })?;
                    }
                    NativeEvent::ReliableQueueOverflow => {
                        if active {
                            recovery.recover();
                            let _ = tx.try_send(InputMessage::Deactivate { generation });
                        }
                        bail!("本机输入可靠事件队列已满, 已停止远程控制");
                    }
                    NativeEvent::Failed(message) => bail!("本机输入捕获失败: {message}"),
                    _ => {}
                }
            }
            _ = motion_tick.tick() => {
                let (dx, dy) = platform.motion.take();
                if dx == 0 && dy == 0 {
                    continue;
                }
                if active {
                    enqueue_message(tx, InputMessage::Motion { generation, dx, dy })?;
                } else if Instant::now() >= cooldown_until {
                    let point = platform.backend.cursor_position()?;
                    if local_layout.is_jump_zone_point(source_edge, point, JUMP_ZONE_SIZE) {
                        generation = generation.wrapping_add(1).max(1);
                        let edge_position = local_layout.normalized_edge_position(source_edge, point);
                        let pressed = platform.backend.snapshot();
                        enqueue_message(tx, InputMessage::Activate { generation, source_edge, edge_position, pressed })?;
                        platform.backend.set_capture(true)?;
                        active = true;
                        recovery.arm(edge_position);
                        tracing::info!(generation, edge = ?source_edge, "鼠标已进入对端桌面");
                    }
                }
            }
            _ = heartbeat.tick() => {
                enqueue_message(tx, InputMessage::Heartbeat { generation })?;
            }
            _ = overflow_poll.tick() => {
                platform.backend.health_check()?;
                if platform.overflowed.load(std::sync::atomic::Ordering::Acquire) {
                    if active {
                        recovery.recover();
                        let _ = tx.try_send(InputMessage::Deactivate { generation });
                    }
                    bail!("本机输入可靠事件队列已满, 已停止远程控制");
                }
            }
            _ = timeout_tick.tick() => {
                if active && Instant::now().duration_since(last_heartbeat) >= HEARTBEAT_TIMEOUT {
                    let _ = platform.backend.set_capture(false);
                    bail!("输入辅助通道心跳超时");
                }
            }
        }
    }
}

struct SenderRecoveryGuard {
    backend: Arc<dyn platform::InputBackend>,
    layout: super::DesktopLayout,
    edge: ScreenEdge,
    edge_position: Option<f32>,
}

impl SenderRecoveryGuard {
    fn new(
        backend: Arc<dyn platform::InputBackend>,
        layout: super::DesktopLayout,
        edge: ScreenEdge,
    ) -> Self {
        Self {
            backend,
            layout,
            edge,
            edge_position: None,
        }
    }

    fn arm(&mut self, edge_position: f32) {
        self.edge_position = Some(edge_position);
    }

    fn disarm(&mut self) {
        self.edge_position = None;
    }

    fn recover(&mut self) {
        let Some(edge_position) = self.edge_position.take() else {
            return;
        };
        let _ = restore_sender(
            &*self.backend,
            &self.layout,
            self.edge,
            edge_position,
        );
    }
}

impl Drop for SenderRecoveryGuard {
    fn drop(&mut self) {
        self.recover();
    }
}

fn deactivate_sender(
    platform: &platform::PlatformHandle,
    layout: &super::DesktopLayout,
    edge: ScreenEdge,
    edge_position: f32,
) -> Result<()> {
    restore_sender(&*platform.backend, layout, edge, edge_position)
}

fn restore_sender(
    backend: &dyn platform::InputBackend,
    layout: &super::DesktopLayout,
    edge: ScreenEdge,
    edge_position: f32,
) -> Result<()> {
    let warp_result = backend.warp_cursor(
        layout.point_inside_edge(edge, edge_position, EDGE_INSET),
    );
    let capture_result = backend.set_capture(false);
    capture_result?;
    warp_result
}

fn enqueue_message(tx: &mpsc::Sender<InputMessage>, message: InputMessage) -> Result<()> {
    tx.try_send(message).map_err(|err| match err {
        TrySendError::Full(_) => anyhow::anyhow!("输入辅助发送队列已满"),
        TrySendError::Closed(_) => anyhow::anyhow!("输入辅助发送队列已关闭"),
    })
}

async fn run_receiver(
    incoming: &mut mpsc::Receiver<Result<InputMessage>>,
    tx: &mpsc::Sender<InputMessage>,
    platform: &mut platform::PlatformHandle,
    local_layout: super::DesktopLayout,
) -> Result<()> {
    let mut generation = 0u64;
    let mut active = false;
    let mut return_edge = ScreenEdge::Left;
    let mut cursor = None;
    let mut last_heartbeat = Instant::now();
    let mut timeout_tick = time::interval(HEARTBEAT_INTERVAL);
    timeout_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut overflow_poll = time::interval(OVERFLOW_POLL);
    let mut heartbeat = time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            message = incoming.recv() => {
                let message = message.context("输入辅助读取任务已停止")??;
                match message {
                    InputMessage::Activate { generation: incoming_generation, source_edge, edge_position, pressed } => {
                        if incoming_generation <= generation {
                            continue;
                        }
                        platform.backend.release_all()?;
                        generation = incoming_generation;
                        active = true;
                        return_edge = source_edge.opposite();
                        last_heartbeat = Instant::now();
                        let entry = local_layout.point_inside_edge(return_edge, edge_position, EDGE_INSET);
                        platform.backend.warp_cursor(entry)?;
                        cursor = Some(entry);
                        apply_snapshot(&*platform.backend, &pressed)?;
                        tracing::info!(generation, edge = ?return_edge, "开始接受对端控制");
                    }
                    InputMessage::Deactivate { generation: incoming_generation } if incoming_generation == generation => {
                        platform.backend.release_all()?;
                        active = false;
                        cursor = None;
                    }
                    InputMessage::Heartbeat { generation: incoming_generation } if incoming_generation == generation => {
                        last_heartbeat = Instant::now();
                    }
                    InputMessage::Key { generation: incoming_generation, usage, modifiers, down, repeat }
                        if active && incoming_generation == generation => {
                            platform.backend.inject_key(usage, modifiers, down, repeat)?;
                    }
                    InputMessage::Button { generation: incoming_generation, button, down }
                        if active && incoming_generation == generation => {
                            platform.backend.inject_button(button, down)?;
                    }
                    InputMessage::Wheel { generation: incoming_generation, x, y }
                        if active && incoming_generation == generation => {
                            platform.backend.inject_wheel(x, y)?;
                    }
                    InputMessage::Motion { generation: incoming_generation, dx, dy }
                        if active && incoming_generation == generation => {
                            let point = cursor.context("输入接收端缺少逻辑光标位置")?;
                            match receiver_motion(&local_layout, return_edge, point, dx, dy) {
                                ReceiverMotion::Return(edge_position) => {
                                    platform.backend.release_all()?;
                                    active = false;
                                    cursor = None;
                                    enqueue_message(tx, InputMessage::Return { generation, edge_position })?;
                                }
                                ReceiverMotion::Move(target) => {
                                    platform.backend.inject_cursor(target)?;
                                    cursor = Some(target);
                                }
                            }
                    }
                    InputMessage::Proof { .. } => bail!("输入通道收到重复认证消息"),
                    _ => {}
                }
            }
            Some(event) = platform.events.recv() => {
                match event {
                    NativeEvent::Emergency => {
                        if active {
                            platform.backend.release_all()?;
                            active = false;
                            cursor = None;
                            let _ = tx.try_send(InputMessage::Deactivate { generation });
                            tracing::info!(generation, "接收端紧急热键已停止远程控制");
                        }
                    }
                    NativeEvent::ReliableQueueOverflow => bail!("本机紧急热键事件队列已满"),
                    NativeEvent::Failed(message) => bail!("本机输入注入后端失败: {message}"),
                    _ => {}
                }
            }
            _ = timeout_tick.tick() => {
                if active && Instant::now().duration_since(last_heartbeat) >= HEARTBEAT_TIMEOUT {
                    platform.backend.release_all()?;
                    bail!("输入辅助通道心跳超时");
                }
            }
            _ = overflow_poll.tick() => {
                platform.backend.health_check()?;
                if platform.overflowed.load(std::sync::atomic::Ordering::Acquire) {
                    let _ = platform.backend.release_all();
                    bail!("本机输入可靠事件队列已满, 已停止远程控制");
                }
            }
            _ = heartbeat.tick() => {
                enqueue_message(tx, InputMessage::Heartbeat { generation })?;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ReceiverMotion {
    Move(super::Point),
    Return(f32),
}

fn receiver_motion(
    layout: &super::DesktopLayout,
    return_edge: ScreenEdge,
    point: super::Point,
    dx: i32,
    dy: i32,
) -> ReceiverMotion {
    if let Some(edge_position) = layout.crossed_outer_edge_position(return_edge, point, dx, dy) {
        ReceiverMotion::Return(edge_position)
    } else {
        ReceiverMotion::Move(layout.move_within_layout(point, dx, dy))
    }
}

fn apply_snapshot(backend: &dyn platform::InputBackend, snapshot: &KeySnapshot) -> Result<()> {
    for usage in &snapshot.usages {
        backend.inject_key(*usage, snapshot.modifiers, true, false)?;
    }
    for button in &snapshot.buttons {
        backend.inject_button(*button, true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AbortOnDrop, EDGE_INSET, ReceiverMotion, SenderRecoveryGuard, enqueue_message,
        receiver_motion, spawn_input_reader,
    };
    use crate::input::platform::InputBackend;
    use crate::input::{DesktopLayout, DisplayRect, KeySnapshot, ModifierMask, Point, ScreenEdge};
    use anyhow::Result;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncWriteExt;
    use tokio::time::{Duration, sleep, timeout};

    #[derive(Default)]
    struct FakeBackend {
        capture: Mutex<bool>,
        warped: Mutex<Option<Point>>,
        recovery_actions: Mutex<Vec<&'static str>>,
    }

    impl InputBackend for FakeBackend {
        fn layout(&self) -> Result<DesktopLayout> {
            DesktopLayout::new(vec![DisplayRect { x: 0, y: 0, width: 100, height: 100 }])
        }

        fn cursor_position(&self) -> Result<Point> {
            Ok(Point { x: 99, y: 50 })
        }

        fn snapshot(&self) -> KeySnapshot {
            KeySnapshot { usages: Vec::new(), modifiers: ModifierMask::default(), buttons: Vec::new() }
        }

        fn set_capture(&self, active: bool) -> Result<()> {
            *self.capture.lock().unwrap() = active;
            self.recovery_actions.lock().unwrap().push("capture");
            Ok(())
        }

        fn warp_cursor(&self, point: Point) -> Result<()> {
            *self.warped.lock().unwrap() = Some(point);
            self.recovery_actions.lock().unwrap().push("warp");
            Ok(())
        }

        fn inject_key(&self, _usage: u16, _modifiers: ModifierMask, _down: bool, _repeat: bool) -> Result<()> {
            Ok(())
        }

        fn inject_button(&self, _button: u8, _down: bool) -> Result<()> {
            Ok(())
        }

        fn inject_cursor(&self, _point: Point) -> Result<()> {
            Ok(())
        }

        fn inject_wheel(&self, _x: i32, _y: i32) -> Result<()> {
            Ok(())
        }

        fn release_all(&self) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn sender_recovery_guard_restores_cursor_on_drop() {
        let backend = Arc::new(FakeBackend::default());
        *backend.capture.lock().unwrap() = true;
        let layout = backend.layout().unwrap();
        let mut guard = SenderRecoveryGuard::new(
            Arc::clone(&backend) as Arc<dyn InputBackend>,
            layout,
            ScreenEdge::Right,
        );
        guard.arm(0.5);
        drop(guard);
        assert!(!*backend.capture.lock().unwrap());
        assert_eq!(
            *backend.warped.lock().unwrap(),
            Some(Point { x: 100 - EDGE_INSET - 1, y: 50 })
        );
        assert_eq!(
            *backend.recovery_actions.lock().unwrap(),
            vec!["warp", "capture"],
        );
    }

    #[tokio::test]
    async fn input_writer_queue_rejects_overflow_without_waiting() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        enqueue_message(&tx, crate::input::protocol::InputMessage::Heartbeat { generation: 1 })
            .unwrap();
        assert!(
            enqueue_message(
                &tx,
                crate::input::protocol::InputMessage::Heartbeat { generation: 1 },
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn input_reader_keeps_frame_alignment_across_select_cancellation() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let (mut incoming, reader_task) = spawn_input_reader(reader);
        let expected = crate::input::protocol::InputMessage::Heartbeat { generation: 9 };
        let bytes = bincode::serialize(&expected).unwrap();
        let write_task = tokio::spawn(async move {
            writer.write_u32(bytes.len() as u32).await.unwrap();
            writer.write_all(&bytes[..1]).await.unwrap();
            sleep(Duration::from_millis(20)).await;
            writer.write_all(&bytes[1..]).await.unwrap();
        });

        tokio::select! {
            _ = sleep(Duration::from_millis(1)) => {}
            _ = incoming.recv() => panic!("分片帧不应提前完成"),
        }
        let actual = timeout(Duration::from_secs(1), incoming.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(actual, expected);

        write_task.await.unwrap();
        reader_task.abort();
        let _ = reader_task.await;
    }

    #[test]
    fn receiver_uses_logical_cursor_across_consecutive_motion_frames() {
        let layout = DesktopLayout::new(vec![DisplayRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }])
        .unwrap();
        let first = receiver_motion(
            &layout,
            ScreenEdge::Left,
            Point { x: 8, y: 40 },
            30,
            5,
        );
        assert_eq!(first, ReceiverMotion::Move(Point { x: 38, y: 45 }));
        let ReceiverMotion::Move(point) = first else {
            panic!("第一次移动不应返回本机");
        };
        assert_eq!(
            receiver_motion(&layout, ScreenEdge::Left, point, 30, 5),
            ReceiverMotion::Move(Point { x: 68, y: 50 }),
        );
    }

    #[tokio::test]
    async fn input_io_task_is_aborted_when_session_is_dropped() {
        let task = tokio::spawn(std::future::pending::<()>());
        let guard = AbortOnDrop(task.abort_handle());
        drop(guard);
        assert!(task.await.unwrap_err().is_cancelled());
    }
}
