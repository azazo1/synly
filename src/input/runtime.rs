use super::channel::{InputChannelOffer, InputHostChannel};
use super::platform::{self, NativeEvent};
use super::protocol::{InputMessage, read_message, write_message};
use super::{Hotkey, InputMode, KeySnapshot, LocalInputRole, ScreenEdge};
use anyhow::{Context, Result, bail};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::{self, Instant, MissedTickBehavior};
use tokio_rustls::TlsStream;

const MOTION_INTERVAL: Duration = Duration::from_micros(8_333);
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(1);
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);
const RETURN_COOLDOWN: Duration = Duration::from_millis(300);
const EDGE_INSET: i32 = 8;
const JUMP_ZONE_SIZE: i32 = 1;
const OVERFLOW_POLL: Duration = Duration::from_millis(50);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(20);
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputRuntimeOptions {
    pub mode: InputMode,
    pub edge: ScreenEdge,
    pub hotkey: Hotkey,
}

#[derive(Clone)]
pub struct InputSocketInbox {
    sockets: Arc<Mutex<mpsc::Receiver<InputSocketConnection>>>,
}

impl InputSocketInbox {
    pub fn new(sockets: mpsc::Receiver<InputSocketConnection>) -> Self {
        Self {
            sockets: Arc::new(Mutex::new(sockets)),
        }
    }

    async fn recv(&self) -> Option<InputSocketConnection> {
        self.sockets.lock().await.recv().await
    }
}

pub struct InputSocketConnection {
    pub session_id: uuid::Uuid,
    pub socket: TcpStream,
}

impl InputSocketConnection {
    pub fn new(session_id: uuid::Uuid, socket: TcpStream) -> Self {
        Self { session_id, socket }
    }
}

pub enum InputSessionContext {
    Host {
        channel: InputHostChannel,
        sockets: InputSocketInbox,
    },
    Client {
        offer: InputChannelOffer,
        remote_addr: SocketAddr,
    },
}

impl InputSessionContext {
    pub fn host(channel: InputHostChannel, sockets: InputSocketInbox) -> Self {
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
    tracing::trace!(role = ?local_role, mode = ?options.mode, edge = ?options.edge, "输入辅助会话任务开始"); // to remove
    let mut platform = platform::start(options.mode, options.hotkey)?;
    tracing::info!(role = ?local_role, "输入同步运行时已启动");
    match context {
        InputSessionContext::Host {
            channel,
            sockets,
        } => {
            loop {
                let connection = sockets
                    .recv()
                    .await
                    .context("输入辅助连接等待期间主会话已结束")?;
                tracing::trace!(session_id = %connection.session_id, "输入辅助 host 收到已路由 socket"); // to remove
                match time::timeout(
                    AUTH_TIMEOUT,
                    channel.accept_after_preamble(
                        connection.socket,
                        connection.session_id,
                        &master_secret,
                    ),
                )
                .await
                {
                    Ok(Ok(stream)) => {
                        tracing::trace!(session_id = %connection.session_id, "输入辅助 host 认证完成, 开始运行 established"); // to remove
                        if let Err(err) = run_established(
                            stream,
                            local_role,
                            options.edge,
                            &mut platform,
                        )
                        .await
                        {
                            tracing::trace!(session_id = %connection.session_id, error = %err, "输入辅助 host established 返回错误"); // to remove
                            cleanup_platform(&platform);
                            if platform_is_terminal(&platform) {
                                return Err(err);
                            }
                            tracing::warn!(error = %err, "输入辅助连接已断开, 等待重连");
                        }
                    }
                    Ok(Err(err)) => {
                        tracing::trace!(session_id = %connection.session_id, error = %err, "输入辅助 host 认证返回错误"); // to remove
                        cleanup_platform(&platform);
                        tracing::warn!(error = %err, "输入辅助连接认证失败");
                    }
                    Err(_) => {
                        tracing::trace!(session_id = %connection.session_id, "输入辅助 host 认证超时"); // to remove
                        cleanup_platform(&platform);
                        tracing::warn!("输入辅助连接认证超时");
                    }
                }
            }
        }
        InputSessionContext::Client { offer, remote_addr } => {
            let mut delay = RECONNECT_MIN;
            loop {
                tracing::trace!(%remote_addr, session_id = %offer.session_id, retry_secs = delay.as_secs(), "输入辅助 client 准备连接"); // to remove
                let established = match time::timeout(
                    AUTH_TIMEOUT,
                    super::channel::connect(remote_addr, &offer, &master_secret),
                )
                .await
                {
                    Ok(Ok(stream)) => {
                        tracing::trace!(%remote_addr, session_id = %offer.session_id, "输入辅助 client 认证完成, 开始运行 established"); // to remove
                        delay = RECONNECT_MIN;
                        run_established(stream, local_role, options.edge, &mut platform).await
                    }
                    Ok(Err(err)) => Err(err),
                    Err(_) => Err(anyhow::anyhow!("输入辅助连接认证超时")),
                };
                cleanup_platform(&platform);
                if let Err(err) = established {
                    tracing::trace!(%remote_addr, session_id = %offer.session_id, error = %err, "输入辅助 client established 返回错误"); // to remove
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
    tracing::trace!(role = ?local_role, source_edge = ?source_edge, "输入辅助 established 开始获取本地布局"); // to remove
    let local_layout = platform.backend.layout()?;
    tracing::trace!(role = ?local_role, displays = ?local_layout.displays, "输入辅助 established 已获取本地布局"); // to remove
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<InputMessage>(256);
    let writer_task = tokio::spawn(async move {
        let mut reported_heartbeat_generation = None;
        let mut written_motion = 0usize;
        while let Some(message) = rx.recv().await {
            let message_name = input_message_name(&message);
            if let Err(error) = write_message(&mut writer, &message).await {
                tracing::trace!(message = message_name, error = %error, "输入辅助 writer 写入失败"); // to remove
                return Err(error);
            }
            match message {
                InputMessage::Activate { generation, .. } => {
                    tracing::trace!(generation, "输入通道 Activate 已写入"); // to remove
                }
                InputMessage::Heartbeat { generation }
                    if reported_heartbeat_generation != Some(generation) =>
                {
                    reported_heartbeat_generation = Some(generation);
                    tracing::trace!(generation, "输入通道当前 generation 首个心跳已写入"); // to remove
                }
                InputMessage::Motion { .. } => {
                    written_motion = written_motion.saturating_add(1);
                    if written_motion == 1 || written_motion.is_multiple_of(500) {
                        tracing::trace!(count = written_motion, "输入辅助 writer 已写入运动汇总"); // to remove
                    }
                }
                InputMessage::Layout(_) => {
                    tracing::trace!("输入辅助 writer 已写入 Layout"); // to remove
                }
                InputMessage::Deactivate { generation } => {
                    tracing::trace!(generation, "输入辅助 writer 已写入 Deactivate"); // to remove
                }
                InputMessage::Return { generation, edge_position } => {
                    tracing::trace!(generation, edge_position, "输入辅助 writer 已写入 Return"); // to remove
                }
                InputMessage::Key { generation, usage, down, repeat, .. } => {
                    tracing::trace!(generation, usage, down, repeat, "输入辅助 writer 已写入 Key"); // to remove
                }
                InputMessage::Button { generation, button, down } => {
                    tracing::trace!(generation, button, down, "输入辅助 writer 已写入 Button"); // to remove
                }
                InputMessage::Wheel { generation, x, y } => {
                    tracing::trace!(generation, x, y, "输入辅助 writer 已写入 Wheel"); // to remove
                }
                _ => {}
            }
        }
        tracing::trace!(motion_count = written_motion, "输入辅助 writer 收到关闭信号"); // to remove
        Result::<()>::Ok(())
    });
    let _writer_abort = AbortOnDrop(writer_task.abort_handle());
    tracing::trace!("输入辅助 established 写入本地布局"); // to remove
    tx.send(InputMessage::Layout(local_layout.clone())).await?;
    let remote_layout = match read_message(&mut reader).await? {
        InputMessage::Layout(layout) => layout,
        InputMessage::Proof { .. } => bail!("输入通道认证完成后收到了重复证明"),
        _ => bail!("输入通道在布局交换前收到了事件"),
    };
    tracing::trace!(displays = ?remote_layout.displays, "输入辅助 established 读取对端布局"); // to remove
    tracing::info!(
        local_displays = local_layout.displays.len(),
        remote_displays = remote_layout.displays.len(),
        "输入通道布局交换完成"
    );

    // 读取帧必须由单独任务连续完成, 避免 select 取消半包读取后破坏帧边界.
    let (mut incoming, incoming_motion, reader_task) = spawn_input_reader(reader);
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
            run_receiver(
                &mut incoming,
                &incoming_motion,
                &tx,
                platform,
                local_layout,
            )
            .await
        }
    };
    tracing::trace!(role = ?local_role, result = ?session.as_ref().err(), "输入辅助 established role loop 已返回"); // to remove
    reader_task.abort();
    let _ = reader_task.await;
    drop(tx);
    if session.is_err() {
        tracing::trace!("输入辅助 established 中止 writer task"); // to remove
        writer_task.abort();
        let _ = writer_task.await;
        return session;
    }
    match writer_task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) if session.is_err() => {
            tracing::trace!(error = %err, "输入辅助 writer 与 session 错误同时结束"); // to remove
            tracing::debug!(error = %err, "输入通道写入任务随会话错误结束");
        }
        Ok(Err(err)) => {
            tracing::trace!(error = %err, "输入辅助 writer task 返回错误"); // to remove
            return Err(err);
        }
        Err(err) => {
            tracing::trace!(error = %err, "输入辅助 writer task join 失败"); // to remove
            return Err(err.into());
        }
    }
    session
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub(super) fn spawn_input_reader<R>(
    mut reader: R,
) -> (
    mpsc::Receiver<Result<InputMessage>>,
    Arc<IncomingMotion>,
    tokio::task::JoinHandle<()>,
)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(256);
    let motion = Arc::new(IncomingMotion::default());
    let reader_motion = Arc::clone(&motion);
    let task = tokio::spawn(async move {
        // let mut reported_heartbeat_generation = None;
        let mut received_motion = 0usize;
        loop {
            match read_message(&mut reader).await {
                Ok(InputMessage::Motion { generation, dx, dy }) => {
                    received_motion = received_motion.saturating_add(1);
                    if received_motion == 1 || received_motion.is_multiple_of(500) {
                        tracing::trace!(count = received_motion, generation, dx, dy, "输入辅助 reader 收到运动汇总"); // to remove
                    }
                    reader_motion.push(generation, dx, dy);
                }
                Ok(message) => {
                    // match &message {
                    //     InputMessage::Activate { generation, .. } => {
                    //         tracing::trace!(generation, "输入通道 Activate 已读取"); // to remove
                    //     }
                    //     InputMessage::Heartbeat { generation }
                    //         if reported_heartbeat_generation != Some(*generation) =>
                    //     {
                    //         reported_heartbeat_generation = Some(*generation);
                    //         tracing::trace!(generation, "输入通道当前 generation 首个心跳已读取"); // to remove
                    //     }
                    //     InputMessage::Layout(_) => {
                    //         tracing::trace!("输入辅助 reader 已读取 Layout"); // to remove
                    //     }
                    //     InputMessage::Deactivate { generation } => {
                    //         tracing::trace!(generation, "输入辅助 reader 已读取 Deactivate"); // to remove
                    //     }
                    //     InputMessage::Return { generation, edge_position } => {
                    //         tracing::trace!(generation, edge_position, "输入辅助 reader 已读取 Return"); // to remove
                    //     }
                    //     InputMessage::Key { generation, usage, down, repeat, .. } => {
                    //         tracing::trace!(generation, usage, down, repeat, "输入辅助 reader 已读取 Key"); // to remove
                    //     }
                    //     InputMessage::Button { generation, button, down } => {
                    //         tracing::trace!(generation, button, down, "输入辅助 reader 已读取 Button"); // to remove
                    //     }
                    //     InputMessage::Wheel { generation, x, y } => {
                    //         tracing::trace!(generation, x, y, "输入辅助 reader 已读取 Wheel"); // to remove
                    //     }
                    //     _ => {}
                    // }
                    if !matches!(message, InputMessage::Heartbeat { .. })
                        && let Some(motion) = reader_motion.take()
                        && tx
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
                    if tx.send(Ok(message)).await.is_err() {
                        tracing::trace!(motion_count = received_motion, "输入辅助 reader 下游已关闭"); // to remove
                        break;
                    }
                }
                Err(err) => {
                    tracing::trace!(motion_count = received_motion, error = %err, "输入辅助 reader 读取失败"); // to remove
                    tracing::warn!(error = %err, "输入通道 reader 已停止");
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            }
        }
        tracing::trace!(motion_count = received_motion, "输入辅助 reader task 已退出"); // to remove
    });
    (rx, motion, task)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CoalescedMotion {
    pub generation: u64,
    pub dx: i32,
    pub dy: i32,
}

#[derive(Default)]
pub(super) struct IncomingMotion {
    pending: StdMutex<Option<CoalescedMotion>>,
}

impl IncomingMotion {
    pub(super) fn push(&self, generation: u64, dx: i32, dy: i32) {
        let mut pending = self.pending.lock().unwrap_or_else(|error| error.into_inner());
        match pending.as_mut() {
            Some(motion) if motion.generation == generation => {
                motion.dx = motion.dx.saturating_add(dx);
                motion.dy = motion.dy.saturating_add(dy);
            }
            _ => {
                *pending = Some(CoalescedMotion { generation, dx, dy });
            }
        }
    }

    pub(super) fn take(&self) -> Option<CoalescedMotion> {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

pub(super) async fn run_sender(
    incoming: &mut mpsc::Receiver<Result<InputMessage>>,
    tx: &mpsc::Sender<InputMessage>,
    platform: &mut platform::PlatformHandle,
    local_layout: super::DesktopLayout,
    _remote_layout: super::DesktopLayout,
    source_edge: ScreenEdge,
) -> Result<()> {
    let mut generation = 0u64;
    let mut active = false;
    let mut activation_confirmed = false;
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
    let mut motion_logged = false;
    let mut last_activation_ready = None;
    let mut sent_motion = 0usize;

    tracing::info!(
        edge = ?source_edge,
        displays = ?local_layout.displays,
        "输入发送端已准备边缘切换"
    );

    loop {
        tokio::select! {
            biased;
            message = incoming.recv() => {
                let message = message.context("输入辅助读取任务已停止")??;
                match message {
                    InputMessage::Return { generation: remote_generation, edge_position }
                        if active && remote_generation == generation => {
                            tracing::trace!(generation, edge_position, "输入发送端收到 Return"); // to remove
                            deactivate_sender(platform, &local_layout, source_edge, edge_position)?;
                            active = false;
                            activation_confirmed = false;
                            recovery.disarm();
                            cooldown_until = Instant::now() + RETURN_COOLDOWN;
                            tracing::info!(generation, "控制已从对端返回本机");
                        }
                    InputMessage::Deactivate { generation: remote_generation }
                        if active && remote_generation == generation => {
                            tracing::trace!(generation, "输入发送端收到 Deactivate"); // to remove
                            recovery.recover();
                            active = false;
                            activation_confirmed = false;
                            cooldown_until = Instant::now() + RETURN_COOLDOWN;
                        }
                    InputMessage::Heartbeat { generation: remote_generation }
                        if !active || remote_generation == generation => {
                        last_heartbeat = Instant::now();
                        if active && remote_generation == generation && !activation_confirmed {
                            activation_confirmed = true;
                            tracing::info!(generation, "对端已确认接管输入控制");
                        }
                    }
                    InputMessage::Heartbeat { generation: remote_generation } => {
                        tracing::trace!(generation, remote_generation, "输入发送端忽略不匹配 heartbeat"); // to remove
                    }
                    InputMessage::Return { generation: remote_generation, .. } => {
                        tracing::trace!(generation, remote_generation, active, "输入发送端忽略不匹配 Return"); // to remove
                    }
                    InputMessage::Deactivate { generation: remote_generation } => {
                        tracing::trace!(generation, remote_generation, active, "输入发送端忽略不匹配 Deactivate"); // to remove
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
                            tracing::trace!(generation, "输入发送端收到 Emergency"); // to remove
                            recovery.recover();
                            active = false;
                            activation_confirmed = false;
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
                        tracing::trace!(generation, active, "输入发送端可靠事件队列溢出"); // to remove
                        if active {
                            recovery.recover();
                            let _ = tx.try_send(InputMessage::Deactivate { generation });
                        }
                        bail!("本机输入可靠事件队列已满, 已停止远程控制");
                    }
                    NativeEvent::Failed(message) => {
                        tracing::trace!(generation, error = %message, "输入发送端收到 backend Failed"); // to remove
                        bail!("本机输入捕获失败: {message}")
                    }
                    _ => {}
                }
            }
            _ = motion_tick.tick() => {
                let sample = platform.motion.take();
                if active {
                    if sample.dx == 0 && sample.dy == 0 {
                        continue;
                    }
                    enqueue_message(tx, InputMessage::Motion {
                        generation,
                        dx: sample.dx,
                        dy: sample.dy,
                    })?;
                    sent_motion = sent_motion.saturating_add(1);
                    if sent_motion == 1 || sent_motion.is_multiple_of(500) {
                        tracing::trace!(count = sent_motion, generation, "输入发送端已发送运动汇总"); // to remove
                    }
                } else if Instant::now() >= cooldown_until {
                    let point = match (sample.position_updated, sample.position) {
                        (true, Some(point)) => point,
                        _ if sample.dx != 0 || sample.dy != 0 => {
                            platform.backend.cursor_position()?
                        }
                        _ => continue,
                    };
                    let activation_edge_position = sender_activation_edge_position(
                        &local_layout,
                        source_edge,
                        point,
                        sample.dx,
                        sample.dy,
                    );
                    let activation_ready = activation_edge_position.is_some();
                    if last_activation_ready != Some(activation_ready) {
                        tracing::info!(
                            point = ?point,
                            edge = ?source_edge,
                            activation_ready,
                            displays = ?local_layout.displays,
                            "输入发送端边缘判定状态变化"
                        );
                        last_activation_ready = Some(activation_ready);
                    }
                    // tracing::debug!(
                    //     point = ?point,
                    //     dx = sample.dx,
                    //     dy = sample.dy,
                    //     position_updated = sample.position_updated,
                    //     edge = ?source_edge,
                    //     activation_ready,
                    //     displays = ?local_layout.displays,
                    //     "输入发送端检查本机边缘"
                    // );
                    if !motion_logged {
                        tracing::info!(
                            dx = sample.dx,
                            dy = sample.dy,
                            point = ?point,
                            edge = ?source_edge,
                            "输入发送端收到首次鼠标运动"
                        );
                        motion_logged = true;
                    }
                    if let Some(edge_position) = activation_edge_position {
                        generation = generation.wrapping_add(1).max(1);
                        tracing::trace!(generation, "输入发送端开始读取按键快照"); // to remove
                        let pressed = platform.backend.snapshot();
                        tracing::trace!(generation, pressed_keys = pressed.usages.len(), pressed_buttons = pressed.buttons.len(), "输入发送端按键快照读取完成"); // to remove
                        enqueue_message(tx, InputMessage::Activate { generation, source_edge, edge_position, pressed })?;
                        tracing::trace!(generation, "输入发送端 Activate 已入队"); // to remove
                        tracing::trace!(generation, "输入发送端开始启用 capture"); // to remove
                        platform.backend.set_capture(true)?;
                        tracing::trace!(generation, "输入发送端 capture 已启用"); // to remove
                        active = true;
                        activation_confirmed = false;
                        last_heartbeat = Instant::now();
                        recovery.arm(edge_position);
                        tracing::info!(
                            generation,
                            edge = ?source_edge,
                            point = ?point,
                            edge_position,
                            "鼠标已进入对端桌面"
                        );
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
                if active
                    && Instant::now().duration_since(last_heartbeat)
                        >= sender_heartbeat_timeout(activation_confirmed)
                {
                    tracing::trace!(generation, activation_confirmed, elapsed_ms = Instant::now().duration_since(last_heartbeat).as_millis(), "输入发送端 heartbeat 超时"); // to remove
                    let _ = platform.backend.set_capture(false);
                    bail!("输入辅助通道心跳超时");
                }
            }
        }
    }
}

fn input_message_name(message: &InputMessage) -> &'static str {
    match message {
        InputMessage::Proof { .. } => "Proof",
        InputMessage::Layout(_) => "Layout",
        InputMessage::Activate { .. } => "Activate",
        InputMessage::Deactivate { .. } => "Deactivate",
        InputMessage::Return { .. } => "Return",
        InputMessage::Heartbeat { .. } => "Heartbeat",
        InputMessage::Key { .. } => "Key",
        InputMessage::Button { .. } => "Button",
        InputMessage::Motion { .. } => "Motion",
        InputMessage::Wheel { .. } => "Wheel",
    }
}

fn sender_activation_edge_position(
    layout: &super::DesktopLayout,
    edge: ScreenEdge,
    point: super::Point,
    dx: i32,
    dy: i32,
) -> Option<f32> {
    if layout.is_jump_zone_point(edge, point, JUMP_ZONE_SIZE) {
        Some(layout.normalized_edge_position(edge, point))
    } else {
        layout.crossed_outer_edge_position(edge, point, dx, dy)
    }
}

fn sender_heartbeat_timeout(activation_confirmed: bool) -> Duration {
    if activation_confirmed {
        HEARTBEAT_TIMEOUT
    } else {
        ACTIVATION_TIMEOUT
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
    let target = layout.point_inside_edge(edge, edge_position, EDGE_INSET);
    tracing::trace!(edge = ?edge, edge_position, target = ?target, "输入发送端开始恢复本机光标"); // to remove
    let warp_result = backend.warp_cursor(target);
    if let Err(error) = &warp_result {
        tracing::trace!(error = %error, "输入发送端恢复光标失败"); // to remove
    }
    tracing::trace!(edge = ?edge, "输入发送端开始关闭 capture"); // to remove
    let capture_result = backend.set_capture(false);
    if let Err(error) = &capture_result {
        tracing::trace!(error = %error, "输入发送端关闭 capture 失败"); // to remove
    }
    capture_result?;
    warp_result
}

fn enqueue_message(tx: &mpsc::Sender<InputMessage>, message: InputMessage) -> Result<()> {
    let message_name = input_message_name(&message);
    tx.try_send(message).map_err(|err| {
        tracing::trace!(message = message_name, error = %err, "输入辅助发送队列写入失败"); // to remove
        match err {
            TrySendError::Full(_) => anyhow::anyhow!("输入辅助发送队列已满"),
            TrySendError::Closed(_) => anyhow::anyhow!("输入辅助发送队列已关闭"),
        }
    })
}

pub(super) async fn run_receiver(
    incoming: &mut mpsc::Receiver<Result<InputMessage>>,
    incoming_motion: &IncomingMotion,
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
    let mut motion_tick = time::interval(MOTION_INTERVAL);
    motion_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut applied_motion = 0usize;

    loop {
        tokio::select! {
            biased;
            _ = heartbeat.tick() => {
                enqueue_message(tx, InputMessage::Heartbeat { generation })?;
            }
            _ = timeout_tick.tick() => {
                if active && Instant::now().duration_since(last_heartbeat) >= HEARTBEAT_TIMEOUT {
                    tracing::trace!(generation, elapsed_ms = Instant::now().duration_since(last_heartbeat).as_millis(), "输入接收端 heartbeat 超时"); // to remove
                    tracing::trace!(generation, "输入接收端开始超时 release_all"); // to remove
                    platform.backend.release_all()?;
                    bail!("输入辅助通道心跳超时");
                }
            }
            _ = overflow_poll.tick() => {
                platform.backend.health_check()?;
                if platform.overflowed.load(std::sync::atomic::Ordering::Acquire) {
                    tracing::trace!(generation, "输入接收端可靠事件队列溢出"); // to remove
                    let _ = platform.backend.release_all();
                    bail!("本机输入可靠事件队列已满, 已停止远程控制");
                }
            }
            message = incoming.recv() => {
                let message = message.context("输入辅助读取任务已停止")??;
                match message {
                    InputMessage::Activate { generation: incoming_generation, source_edge, edge_position, pressed } => {
                        tracing::trace!(generation = incoming_generation, current_generation = generation, source_edge = ?source_edge, edge_position, "输入接收端收到 Activate"); // to remove
                        if incoming_generation <= generation {
                            tracing::trace!(generation = incoming_generation, current_generation = generation, "输入接收端忽略过期 Activate"); // to remove
                            continue;
                        }
                        tracing::trace!(generation = incoming_generation, "输入接收端开始 Activate 前 release_all"); // to remove
                        platform.backend.release_all()?;
                        generation = incoming_generation;
                        active = true;
                        return_edge = source_edge.opposite();
                        last_heartbeat = Instant::now();
                        let entry = local_layout.point_inside_edge(return_edge, edge_position, EDGE_INSET);
                        tracing::trace!(generation, return_edge = ?return_edge, edge_position, entry = ?entry, "输入接收端计算并写入进入点"); // to remove
                        platform.backend.warp_cursor(entry)?;
                        cursor = Some(entry);
                        tracing::trace!(generation, pressed_keys = pressed.usages.len(), pressed_buttons = pressed.buttons.len(), "输入接收端开始应用按键快照"); // to remove
                        apply_snapshot(&*platform.backend, &pressed)?;
                        tracing::trace!(generation, "输入接收端按键快照应用完成"); // to remove
                        enqueue_message(tx, InputMessage::Heartbeat { generation })?;
                        tracing::info!(generation, edge = ?return_edge, "开始接受对端控制");
                    }
                    InputMessage::Deactivate { generation: incoming_generation } if incoming_generation == generation => {
                        tracing::trace!(generation, "输入接收端收到 Deactivate, 开始 release_all"); // to remove
                        platform.backend.release_all()?;
                        active = false;
                        cursor = None;
                    }
                    InputMessage::Heartbeat { generation: incoming_generation } if incoming_generation == generation => {
                        last_heartbeat = Instant::now();
                    }
                    InputMessage::Heartbeat { generation: incoming_generation } => {
                        tracing::trace!(generation, incoming_generation, "输入接收端忽略不匹配 heartbeat"); // to remove
                    }
                    InputMessage::Key { generation: incoming_generation, usage, modifiers, down, repeat }
                        if active && incoming_generation == generation => {
                            tracing::trace!(generation, usage, down, repeat, "输入接收端开始注入 key"); // to remove
                            platform.backend.inject_key(usage, modifiers, down, repeat)?;
                    }
                    InputMessage::Button { generation: incoming_generation, button, down }
                        if active && incoming_generation == generation => {
                            tracing::trace!(generation, button, down, "输入接收端开始注入 button"); // to remove
                            platform.backend.inject_button(button, down)?;
                    }
                    InputMessage::Wheel { generation: incoming_generation, x, y }
                        if active && incoming_generation == generation => {
                            tracing::trace!(generation, x, y, "输入接收端开始注入 wheel"); // to remove
                            platform.backend.inject_wheel(x, y)?;
                    }
                    InputMessage::Motion { generation: incoming_generation, dx, dy }
                        if active && incoming_generation == generation => {
                            applied_motion = applied_motion.saturating_add(1);
                            if applied_motion == 1 || applied_motion.is_multiple_of(500) {
                                tracing::trace!(count = applied_motion, generation, "输入接收端已处理运动汇总"); // to remove
                            }
                            if let Some(edge_position) = apply_receiver_motion(
                                &*platform.backend,
                                &local_layout,
                                return_edge,
                                &mut cursor,
                                dx,
                                dy,
                            )? {
                                tracing::trace!(generation, edge_position, "输入接收端运动越过返回边界"); // to remove
                                active = false;
                                enqueue_message(tx, InputMessage::Return { generation, edge_position })?;
                            }
                    }
                    InputMessage::Proof { .. } => bail!("输入通道收到重复认证消息"),
                    _ => {}
                }
            }
            _ = motion_tick.tick() => {
                if let Some(motion) = incoming_motion.take() {
                    if active
                        && motion.generation == generation
                        && let Some(edge_position) = apply_receiver_motion(
                            &*platform.backend,
                            &local_layout,
                            return_edge,
                            &mut cursor,
                            motion.dx,
                            motion.dy,
                        )?
                    {
                        applied_motion = applied_motion.saturating_add(1);
                        tracing::trace!(generation = motion.generation, dx = motion.dx, dy = motion.dy, "输入接收端处理合并运动"); // to remove
                        active = false;
                        enqueue_message(tx, InputMessage::Return { generation, edge_position })?;
                    } else if active && motion.generation == generation {
                        applied_motion = applied_motion.saturating_add(1);
                        if applied_motion == 1 || applied_motion.is_multiple_of(500) {
                            tracing::trace!(count = applied_motion, generation, "输入接收端已处理合并运动汇总"); // to remove
                        }
                    } else {
                        tracing::trace!(generation = motion.generation, dx = motion.dx, dy = motion.dy, active, current_generation = generation, "输入接收端丢弃不适用合并运动"); // to remove
                    }
                }
            }
            Some(event) = platform.events.recv() => {
                match event {
                    NativeEvent::Emergency => {
                        if active {
                            tracing::trace!(generation, "输入接收端收到 Emergency"); // to remove
                            platform.backend.release_all()?;
                            active = false;
                            cursor = None;
                            let _ = tx.try_send(InputMessage::Deactivate { generation });
                            tracing::info!(generation, "接收端紧急热键已停止远程控制");
                        }
                    }
                    NativeEvent::ReliableQueueOverflow => {
                        tracing::trace!(generation, "输入接收端可靠事件队列溢出"); // to remove
                        bail!("本机紧急热键事件队列已满")
                    }
                    NativeEvent::Failed(message) => {
                        tracing::trace!(generation, error = %message, "输入接收端收到 backend Failed"); // to remove
                        bail!("本机输入注入后端失败: {message}")
                    }
                    _ => {}
                }
            }
        }
    }
}

fn apply_receiver_motion(
    backend: &dyn platform::InputBackend,
    layout: &super::DesktopLayout,
    return_edge: ScreenEdge,
    cursor: &mut Option<super::Point>,
    dx: i32,
    dy: i32,
) -> Result<Option<f32>> {
    let point = cursor.context("输入接收端缺少逻辑光标位置")?;
    match receiver_motion(layout, return_edge, point, dx, dy) {
        ReceiverMotion::Return(edge_position) => {
            tracing::trace!(edge_position, "输入接收端开始返回本机并 release_all"); // to remove
            backend.release_all()?;
            *cursor = None;
            Ok(Some(edge_position))
        }
        ReceiverMotion::Move(target) => {
            backend.inject_cursor(target)?;
            *cursor = Some(target);
            Ok(None)
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
        ACTIVATION_TIMEOUT, AbortOnDrop, EDGE_INSET, HEARTBEAT_TIMEOUT, ReceiverMotion,
        IncomingMotion, SenderRecoveryGuard, enqueue_message, receiver_motion, run_receiver, run_sender,
        sender_activation_edge_position, sender_heartbeat_timeout, spawn_input_reader,
    };
    use crate::input::platform::{InputBackend, MotionAccumulator, PlatformHandle};
    use crate::input::protocol::{InputMessage, write_message};
    use crate::input::{DesktopLayout, DisplayRect, KeySnapshot, ModifierMask, Point, ScreenEdge};
    use anyhow::Result;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;
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

    #[test]
    fn sender_activates_when_outward_motion_crosses_the_edge() {
        let layout = DesktopLayout::new(vec![DisplayRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }])
        .unwrap();

        assert_eq!(
            sender_activation_edge_position(
                &layout,
                ScreenEdge::Right,
                Point { x: 97, y: 50 },
                4,
                0,
            ),
            Some(0.5),
        );
        assert_eq!(
            sender_activation_edge_position(
                &layout,
                ScreenEdge::Right,
                Point { x: 97, y: 50 },
                2,
                0,
            ),
            None,
        );
    }

    #[test]
    fn sender_uses_a_longer_timeout_until_activation_is_confirmed() {
        assert_eq!(sender_heartbeat_timeout(false), ACTIVATION_TIMEOUT);
        assert_eq!(sender_heartbeat_timeout(true), HEARTBEAT_TIMEOUT);
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
    async fn sender_activates_from_a_zero_delta_edge_position_update() {
        let backend = Arc::new(FakeBackend::default());
        let layout = backend.layout().unwrap();
        let motion = Arc::new(MotionAccumulator::default());
        let (_events_tx, events) = mpsc::channel(1);
        let mut platform = PlatformHandle {
            backend: Arc::clone(&backend) as Arc<dyn InputBackend>,
            events,
            motion: Arc::clone(&motion),
            overflowed: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let (_incoming_tx, mut incoming) = mpsc::channel(1);
        let (outgoing, mut messages) = mpsc::channel(8);
        motion.add_at(0, 0, Point { x: 0, y: 50 });

        let task = tokio::spawn(async move {
            run_sender(
                &mut incoming,
                &outgoing,
                &mut platform,
                layout.clone(),
                layout,
                ScreenEdge::Left,
            )
            .await
        });
        let edge_position = timeout(Duration::from_secs(1), async {
            loop {
                if let InputMessage::Activate {
                    edge_position,
                    ..
                } =
                    messages.recv().await.expect("sender 不应提前停止")
                {
                    break edge_position;
                }
            }
        })
        .await
        .expect("sender 应处理零 delta 的边缘坐标更新");

        assert_eq!(edge_position, 0.5);
        assert!(*backend.capture.lock().unwrap());
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn receiver_acknowledges_activation_before_motion_pressure() {
        let backend = Arc::new(FakeBackend::default());
        let layout = backend.layout().unwrap();
        let motion = Arc::new(MotionAccumulator::default());
        let (_events_tx, events) = mpsc::channel(1);
        let mut platform = PlatformHandle {
            backend: Arc::clone(&backend) as Arc<dyn InputBackend>,
            events,
            motion,
            overflowed: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let (incoming_tx, mut incoming) = mpsc::channel(256);
        let incoming_motion = Arc::new(IncomingMotion::default());
        let (outgoing, mut messages) = mpsc::channel(16);
        incoming_tx
            .send(Ok(InputMessage::Activate {
                generation: 7,
                source_edge: ScreenEdge::Right,
                edge_position: 0.5,
                pressed: KeySnapshot {
                    usages: Vec::new(),
                    modifiers: ModifierMask::default(),
                    buttons: Vec::new(),
                },
            }))
            .await
            .unwrap();
        for _ in 0..200 {
            incoming_motion.push(7, 1, 0);
        }

        let task = tokio::spawn(async move {
            run_receiver(
                &mut incoming,
                &incoming_motion,
                &outgoing,
                &mut platform,
                layout,
            )
            .await
        });
        timeout(Duration::from_secs(1), async {
            loop {
                if matches!(
                    messages.recv().await,
                    Some(InputMessage::Heartbeat { generation: 7 })
                ) {
                    break;
                }
            }
        })
        .await
        .expect("接收端应在持续运动前确认新的 generation");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn input_reader_keeps_frame_alignment_across_select_cancellation() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let (mut incoming, _motion, reader_task) = spawn_input_reader(reader);
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

    #[tokio::test]
    async fn input_reader_prioritizes_heartbeat_over_motion_pressure() {
        let (mut writer, reader) = tokio::io::duplex(64 * 1024);
        let (mut incoming, motion, reader_task) = spawn_input_reader(reader);
        let write_task = tokio::spawn(async move {
            write_message(
                &mut writer,
                &InputMessage::Activate {
                    generation: 5,
                    source_edge: ScreenEdge::Right,
                    edge_position: 0.5,
                    pressed: KeySnapshot {
                        usages: Vec::new(),
                        modifiers: ModifierMask::default(),
                        buttons: Vec::new(),
                    },
                },
            )
            .await
            .unwrap();
            for _ in 0..400 {
                write_message(
                    &mut writer,
                    &InputMessage::Motion { generation: 5, dx: 1, dy: -1 },
                )
                .await
                .unwrap();
            }
            write_message(&mut writer, &InputMessage::Heartbeat { generation: 5 })
                .await
                .unwrap();
        });

        assert!(matches!(
            timeout(Duration::from_secs(1), incoming.recv()).await.unwrap(),
            Some(Ok(InputMessage::Activate { generation: 5, .. }))
        ));
        assert!(matches!(
            timeout(Duration::from_secs(1), incoming.recv()).await.unwrap(),
            Some(Ok(InputMessage::Heartbeat { generation: 5 }))
        ));
        assert_eq!(
            motion.take(),
            Some(super::CoalescedMotion { generation: 5, dx: 400, dy: -400 })
        );

        write_task.await.unwrap();
        reader_task.abort();
        let _ = reader_task.await;
    }

    #[tokio::test]
    async fn input_reader_flushes_motion_before_reliable_input() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let (mut incoming, motion, reader_task) = spawn_input_reader(reader);
        let write_task = tokio::spawn(async move {
            write_message(
                &mut writer,
                &InputMessage::Motion { generation: 3, dx: 7, dy: -2 },
            )
            .await
            .unwrap();
            write_message(
                &mut writer,
                &InputMessage::Button { generation: 3, button: 1, down: true },
            )
            .await
            .unwrap();
        });

        assert_eq!(
            timeout(Duration::from_secs(1), incoming.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            InputMessage::Motion { generation: 3, dx: 7, dy: -2 }
        );
        assert_eq!(
            timeout(Duration::from_secs(1), incoming.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            InputMessage::Button { generation: 3, button: 1, down: true }
        );
        assert_eq!(motion.take(), None);

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
