use super::channel::{InputChannelOffer, InputHostChannel};
use super::mapping::KeyMapper;
use super::platform::{self, NativeEvent, ScrollSource};
use super::protocol::{InputMessage, read_message, write_message};
use super::{
    Hotkey, InputMode, InputPlatform, KeyMappingConfig, KeySnapshot, LocalInputRole, ScreenEdge,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
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
const GAME_MODE_POLL: Duration = Duration::from_millis(500);
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
    pub reverse_mouse_wheel: bool,
    pub reverse_trackpad: bool,
    pub block_switch_on_press: bool,
    pub key_mapping: KeyMappingConfig,
    pub cursor_mode: CursorMode,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum CursorMode {
    #[default]
    Desktop,
    Auto,
    Game,
}

impl CursorMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Desktop => "桌面光标",
            Self::Auto => "自动切换",
            Self::Game => "游戏光标",
        }
    }
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
                        if let Err(err) = run_established(
                            stream,
                            local_role,
                            &options,
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
                        run_established(stream, local_role, &options, &mut platform).await
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
    options: &InputRuntimeOptions,
    platform: &mut platform::PlatformHandle,
) -> Result<()> {
    let local_layout = platform.backend.layout()?;
    let local_platform = InputPlatform::current();
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<InputMessage>(256);
    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            write_message(&mut writer, &message).await?;
        }
        Result::<()>::Ok(())
    });
    let _writer_abort = AbortOnDrop(writer_task.abort_handle());
    tx.send(InputMessage::Hello {
        platform: local_platform,
        layout: local_layout.clone(),
    })
    .await?;
    let (remote_platform, remote_layout) = match read_message(&mut reader).await? {
        InputMessage::Hello { platform, layout } => (platform, layout),
        InputMessage::Proof { .. } => bail!("输入通道认证完成后收到了重复证明"),
        _ => bail!("输入通道在布局交换前收到了事件"),
    };
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
            options.edge,
            remote_platform,
            options,
        )
        .await,
        LocalInputRole::Receive => {
            run_receiver(
                &mut incoming,
                &incoming_motion,
                &tx,
                platform,
                local_layout,
                options,
                platform::foreground_cursor_captured,
            )
            .await
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
        Ok(Err(err)) => {
            return Err(err);
        }
        Err(err) => {
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
        loop {
            match read_message(&mut reader).await {
                Ok(InputMessage::Motion { generation, dx, dy }) => {
                    reader_motion.push(generation, dx, dy);
                }
                Ok(message) => {
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
                        break;
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "输入通道 reader 已停止");
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            }
        }
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

#[derive(Default)]
struct PressedState {
    keys: BTreeSet<u16>,
    buttons: BTreeSet<u8>,
}

impl PressedState {
    fn from_snapshot(snapshot: &KeySnapshot) -> Self {
        Self {
            keys: snapshot.usages.iter().copied().collect(),
            buttons: snapshot.buttons.iter().copied().collect(),
        }
    }

    fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    fn key(&mut self, usage: u16, down: bool, repeat: bool) {
        if down && !repeat {
            self.keys.insert(usage);
        } else if !down {
            self.keys.remove(&usage);
        }
    }

    fn button(&mut self, button: u8, down: bool) {
        if down {
            self.buttons.insert(button);
        } else {
            self.buttons.remove(&button);
        }
    }
}

struct SenderControl {
    generation: u64,
    active: bool,
    activation_confirmed: bool,
    recovery: SenderRecoveryGuard,
    cooldown_until: Instant,
    local_pressed: PressedState,
    pending_return_request: Option<(u64, f32)>,
}

impl SenderControl {
    fn new(
        platform: &platform::PlatformHandle,
        layout: super::DesktopLayout,
        edge: ScreenEdge,
    ) -> Self {
        Self {
            generation: 0,
            active: false,
            activation_confirmed: false,
            recovery: SenderRecoveryGuard::new(Arc::clone(&platform.backend), layout, edge),
            cooldown_until: Instant::now(),
            local_pressed: PressedState::default(),
            pending_return_request: None,
        }
    }

    fn deactivate(&mut self) {
        self.active = false;
        self.activation_confirmed = false;
        self.cooldown_until = Instant::now() + RETURN_COOLDOWN;
        self.pending_return_request = None;
    }
}

pub(super) async fn run_sender(
    incoming: &mut mpsc::Receiver<Result<InputMessage>>,
    tx: &mpsc::Sender<InputMessage>,
    platform: &mut platform::PlatformHandle,
    local_layout: super::DesktopLayout,
    source_edge: ScreenEdge,
    remote_platform: InputPlatform,
    options: &InputRuntimeOptions,
) -> Result<()> {
    let local_platform = InputPlatform::current();
    let mut key_mapper = KeyMapper::new(
        &options.key_mapping,
        local_platform,
        remote_platform,
    )?;
    let mut control = SenderControl::new(platform, local_layout.clone(), source_edge);
    let mut last_heartbeat = Instant::now();
    let mut motion_tick = time::interval(MOTION_INTERVAL);
    motion_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut heartbeat = time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut overflow_poll = time::interval(OVERFLOW_POLL);
    let mut timeout_tick = time::interval(HEARTBEAT_INTERVAL);
    let mut motion_logged = false;
    let mut last_activation_ready = None;
    let mut press_blocked = false;

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
                    InputMessage::ReturnRequest { generation: remote_generation, edge_position }
                        if control.active && remote_generation == control.generation => {
                            if options.block_switch_on_press && !control.local_pressed.is_empty() {
                                control.pending_return_request =
                                    Some((control.generation, edge_position));
                                tracing::info!(
                                    generation = control.generation,
                                    "对端请求返回但本机按键/鼠标处于按下状态, 等待松开"
                                );
                            } else {
                                sender_return_now(
                                    tx,
                                    platform,
                                    &local_layout,
                                    source_edge,
                                    edge_position,
                                    &mut control,
                                )?;
                            }
                        }
                    InputMessage::Deactivate { generation: remote_generation, edge_position }
                        if control.active && remote_generation == control.generation => {
                            control.recovery.recover_at(edge_position);
                            control.deactivate();
                            key_mapper.clear();
                        }
                    InputMessage::Heartbeat { generation: remote_generation }
                        if !control.active || remote_generation == control.generation => {
                        last_heartbeat = Instant::now();
                        if control.active
                            && remote_generation == control.generation
                            && !control.activation_confirmed
                        {
                            control.activation_confirmed = true;
                            tracing::info!(generation = control.generation, "对端已确认接管输入控制");
                        }
                    }
                    InputMessage::Heartbeat { .. }
                    | InputMessage::Deactivate { .. } => {}
                    InputMessage::Return { .. } => {}
                    InputMessage::Hello { .. } => {}
                    InputMessage::Proof { .. } => bail!("输入通道收到重复认证消息"),
                    _ => {}
                }
            }
            Some(event) = platform.events.recv() => {
                match event {
                    NativeEvent::Emergency => {
                        if control.active {
                            control.recovery.recover();
                            control.deactivate();
                            key_mapper.clear();
                            let _ = tx.try_send(InputMessage::Deactivate {
                                generation: control.generation,
                                edge_position: None,
                            });
                            tracing::info!(generation = control.generation, "紧急热键已收回本机控制");
                        }
                    }
                    NativeEvent::Key { usage, modifiers: _, down, repeat } if control.active => {
                        control.local_pressed.key(usage, down, repeat);
                        if let Some(mapped) = key_mapper.map_key(usage, down, repeat) {
                            enqueue_message(tx, InputMessage::Key {
                                generation: control.generation,
                                usage: mapped.usage,
                                modifiers: mapped.modifiers,
                                down: mapped.down,
                                repeat: mapped.repeat,
                            })?;
                        }
                        finish_pending_sender_return(
                            tx,
                            platform,
                            &local_layout,
                            source_edge,
                            &mut control,
                        )?;
                    }
                    NativeEvent::Button { button, down } if control.active => {
                        control.local_pressed.button(button, down);
                        enqueue_message(tx, InputMessage::Button {
                            generation: control.generation,
                            button,
                            down,
                        })?;
                        finish_pending_sender_return(
                            tx,
                            platform,
                            &local_layout,
                            source_edge,
                            &mut control,
                        )?;
                    }
                    NativeEvent::Wheel { x, y, source } if control.active => {
                        let (x, y) = transform_scroll(
                            x,
                            y,
                            source,
                            local_platform,
                            remote_platform,
                            options.reverse_mouse_wheel,
                            options.reverse_trackpad,
                        );
                        enqueue_message(tx, InputMessage::Wheel {
                            generation: control.generation,
                            x,
                            y,
                        })?;
                    }
                    NativeEvent::ReliableQueueOverflow => {
                        if control.active {
                            control.recovery.recover();
                            let _ = tx.try_send(InputMessage::Deactivate {
                                generation: control.generation,
                                edge_position: None,
                            });
                        }
                        bail!("本机输入可靠事件队列已满, 已停止远程控制");
                    }
                    NativeEvent::Failed(message) => {
                        bail!("本机输入捕获失败: {message}")
                    }
                    _ => {}
                }
            }
            _ = motion_tick.tick() => {
                let sample = platform.motion.take();
                if control.active {
                    if sample.dx == 0 && sample.dy == 0 {
                        continue;
                    }
                    enqueue_message(tx, InputMessage::Motion {
                        generation: control.generation,
                        dx: sample.dx,
                        dy: sample.dy,
                    })?;
                } else if Instant::now() >= control.cooldown_until {
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
                    if !activation_ready {
                        press_blocked = false;
                    }
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
                        if options.block_switch_on_press {
                            let snapshot = platform.backend.snapshot();
                            let blocked = !snapshot.usages.is_empty()
                                || !snapshot.buttons.is_empty();
                            if blocked && !press_blocked {
                                tracing::info!(
                                    point = ?point,
                                    edge = ?source_edge,
                                    "边缘已就绪但按键/鼠标处于按下状态, 等待松开后切换"
                                );
                            }
                            press_blocked = blocked;
                            if blocked {
                                continue;
                            }
                        }
                        control.generation = control.generation.wrapping_add(1).max(1);
                        let snapshot = platform.backend.snapshot();
                        let pressed = key_mapper.map_snapshot(&snapshot);
                        control.local_pressed = PressedState::from_snapshot(&snapshot);
                        control.pending_return_request = None;
                        enqueue_message(tx, InputMessage::Activate {
                            generation: control.generation,
                            source_edge,
                            edge_position,
                            pressed,
                        })?;
                        platform.backend.set_capture(true)?;
                        control.active = true;
                        control.activation_confirmed = false;
                        last_heartbeat = Instant::now();
                        control.recovery.arm(edge_position);
                        tracing::info!(
                            generation = control.generation,
                            edge = ?source_edge,
                            point = ?point,
                            edge_position,
                            "鼠标已进入对端桌面"
                        );
                    }
                }
            }
            _ = heartbeat.tick() => {
                enqueue_message(tx, InputMessage::Heartbeat { generation: control.generation })?;
            }
            _ = overflow_poll.tick() => {
                platform.backend.health_check()?;
                if platform.overflowed.load(std::sync::atomic::Ordering::Acquire) {
                    if control.active {
                        control.recovery.recover();
                        let _ = tx.try_send(InputMessage::Deactivate {
                            generation: control.generation,
                            edge_position: None,
                        });
                    }
                    bail!("本机输入可靠事件队列已满, 已停止远程控制");
                }
            }
            _ = timeout_tick.tick() => {
                if control.active
                    && Instant::now().duration_since(last_heartbeat)
                        >= sender_heartbeat_timeout(control.activation_confirmed)
                {
                    let _ = platform.backend.set_capture(false);
                    bail!("输入辅助通道心跳超时");
                }
            }
        }
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
        self.recover_at(None);
    }

    fn recover_at(&mut self, edge_position: Option<f32>) {
        let Some(edge_position) = edge_position.or(self.edge_position.take()) else {
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

fn sender_return_now(
    tx: &mpsc::Sender<InputMessage>,
    platform: &platform::PlatformHandle,
    layout: &super::DesktopLayout,
    edge: ScreenEdge,
    edge_position: f32,
    control: &mut SenderControl,
) -> Result<()> {
    enqueue_message(tx, InputMessage::Return {
        generation: control.generation,
        edge_position,
    })?;
    deactivate_sender(platform, layout, edge, edge_position)?;
    control.recovery.disarm();
    control.deactivate();
    tracing::info!(generation = control.generation, "控制已从对端返回本机");
    Ok(())
}

fn finish_pending_sender_return(
    tx: &mpsc::Sender<InputMessage>,
    platform: &platform::PlatformHandle,
    layout: &super::DesktopLayout,
    edge: ScreenEdge,
    control: &mut SenderControl,
) -> Result<()> {
    if control.local_pressed.is_empty()
        && let Some((_, edge_position)) = control.pending_return_request
    {
        sender_return_now(tx, platform, layout, edge, edge_position, control)?;
    }
    Ok(())
}

fn restore_sender(
    backend: &dyn platform::InputBackend,
    layout: &super::DesktopLayout,
    edge: ScreenEdge,
    edge_position: f32,
) -> Result<()> {
    let target = layout.point_inside_edge(edge, edge_position, EDGE_INSET);
    let warp_result = backend.warp_cursor(target);
    backend.set_capture(false)?;
    warp_result
}

fn enqueue_message(tx: &mpsc::Sender<InputMessage>, message: InputMessage) -> Result<()> {
    tx.try_send(message).map_err(|err| {
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
    options: &InputRuntimeOptions,
    foreground_captured: impl Fn() -> bool,
) -> Result<()> {
    let mut generation = 0u64;
    let mut active = false;
    let mut return_edge = ScreenEdge::Left;
    let mut cursor = None;
    let mut cursor_mode_active = match options.cursor_mode {
        CursorMode::Desktop => false,
        CursorMode::Auto => foreground_captured(),
        CursorMode::Game => true,
    };
    tracing::info!(
        game = cursor_mode_active,
        mode = ?options.cursor_mode,
        "接收端游戏光标模式初始状态"
    );
    let mut last_heartbeat = Instant::now();
    let mut timeout_tick = time::interval(HEARTBEAT_INTERVAL);
    timeout_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut overflow_poll = time::interval(OVERFLOW_POLL);
    let mut heartbeat = time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut motion_tick = time::interval(MOTION_INTERVAL);
    motion_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut monitor_tick = time::interval(GAME_MODE_POLL);
    monitor_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            _ = heartbeat.tick() => {
                enqueue_message(tx, InputMessage::Heartbeat { generation })?;
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
            _ = monitor_tick.tick() => {
                if options.cursor_mode == CursorMode::Game {
                    continue;
                }
                let captured = foreground_captured();
                let game = match options.cursor_mode {
                    CursorMode::Desktop => cursor_mode_active && captured,
                    CursorMode::Auto => captured,
                    CursorMode::Game => true,
                };
                if game == cursor_mode_active {
                    continue;
                }
                cursor_mode_active = game;
                tracing::info!(
                    game,
                    foreground_cursor_captured = captured,
                    "前台光标捕获状态变化, 游戏光标模式已切换"
                );
                if !game && active {
                    if cursor.is_none() {
                        cursor = Some(platform.backend.cursor_position()?);
                    }
                    tracing::info!(generation, "前台光标捕获状态结束, 已切回桌面光标模式");
                }
            }
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
                        if cursor_mode_active {
                            tracing::info!(
                                generation,
                                edge = ?return_edge,
                                "游戏光标模式接管, 跳过边缘定位"
                            );
                        } else {
                            let entry = local_layout.point_inside_edge(
                                return_edge,
                                edge_position,
                                EDGE_INSET,
                            );
                            match platform.backend.warp_cursor(entry) {
                                Ok(()) => cursor = Some(entry),
                                Err(error) if foreground_captured() => {
                                    cursor_mode_active = true;
                                    tracing::warn!(
                                        generation,
                                        error = %error,
                                        "前台游戏锁定光标, 无法定位到边缘, 已切换游戏光标模式"
                                    );
                                }
                                Err(error) => return Err(error),
                            }
                        }
                        apply_snapshot(&*platform.backend, &pressed)?;
                        enqueue_message(tx, InputMessage::Heartbeat { generation })?;
                        tracing::info!(generation, edge = ?return_edge, "开始接受对端控制");
                    }
                    InputMessage::Deactivate { generation: incoming_generation, .. }
                        if incoming_generation == generation => {
                        platform.backend.release_all()?;
                        active = false;
                        cursor = None;
                    }
                    InputMessage::Heartbeat { generation: incoming_generation } if incoming_generation == generation => {
                        last_heartbeat = Instant::now();
                    }
                    InputMessage::Heartbeat { .. } => {}
                    InputMessage::Return { generation: incoming_generation, .. }
                        if active && incoming_generation == generation => {
                            platform.backend.release_all()?;
                            active = false;
                            cursor = None;
                            tracing::info!(generation, "对端已批准返回, 控制已归还本机");
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
                            if cursor_mode_active {
                                platform.backend.inject_motion(dx, dy)?;
                            } else if let Some(edge_position) = apply_receiver_motion(
                                &*platform.backend,
                                &local_layout,
                                return_edge,
                                &mut cursor,
                                dx,
                                dy,
                            )? {
                                enqueue_message(tx, InputMessage::ReturnRequest { generation, edge_position })?;
                            }
                    }
                    InputMessage::Proof { .. } => bail!("输入通道收到重复认证消息"),
                    InputMessage::Hello { .. } => {}
                    _ => {}
                }
            }
            _ = motion_tick.tick() => {
                if let Some(motion) = incoming_motion.take()
                    && active
                    && motion.generation == generation
                {
                    if cursor_mode_active {
                        platform.backend.inject_motion(motion.dx, motion.dy)?;
                    } else if let Some(edge_position) = apply_receiver_motion(
                        &*platform.backend,
                        &local_layout,
                        return_edge,
                        &mut cursor,
                        motion.dx,
                        motion.dy,
                    )? {
                        enqueue_message(tx, InputMessage::ReturnRequest { generation, edge_position })?;
                    }
                }
            }
            Some(event) = platform.events.recv() => {
                match event {
                    NativeEvent::Emergency => {
                        if active {
                            platform.backend.release_all()?;
                            let edge_position =
                                cursor.map(|point| local_layout.edge_position(return_edge, point));
                            active = false;
                            cursor = None;
                            let _ = tx.try_send(InputMessage::Deactivate {
                                generation,
                                edge_position,
                            });
                            tracing::info!(generation, "接收端紧急热键已停止远程控制");
                        }
                    }
                    NativeEvent::ReliableQueueOverflow => {
                        bail!("本机紧急热键事件队列已满")
                    }
                    NativeEvent::Failed(message) => {
                        bail!("本机输入注入后端失败: {message}")
                    }
                    _ => {}
                }
            }
        }
    }
}

fn transform_scroll(
    x: i32,
    y: i32,
    source: ScrollSource,
    local_platform: InputPlatform,
    remote_platform: InputPlatform,
    reverse_mouse_wheel: bool,
    reverse_trackpad: bool,
) -> (i32, i32) {
    // macOS 和 Windows 的水平滚动 API 符号相反, 同平台传输保持原生值.
    let x = if local_platform == remote_platform {
        x
    } else {
        x.saturating_neg()
    };
    let reverse = match source {
        ScrollSource::MouseWheel => reverse_mouse_wheel,
        ScrollSource::Trackpad => reverse_trackpad,
    };
    if reverse {
        (x.saturating_neg(), y.saturating_neg())
    } else {
        (x, y)
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
            let target = layout.move_within_layout(point, dx, dy);
            backend.inject_cursor(target)?;
            *cursor = Some(target);
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
        IncomingMotion, PressedState, SenderRecoveryGuard, enqueue_message, receiver_motion,
        run_receiver, run_sender, sender_activation_edge_position, sender_heartbeat_timeout,
        spawn_input_reader, transform_scroll,
    };
    use crate::input::platform::{InputBackend, MotionAccumulator, NativeEvent, PlatformHandle};
    use crate::input::platform::ScrollSource;
    use crate::input::protocol::{InputMessage, write_message};
    use crate::input::{
        DesktopLayout, DisplayRect, Hotkey, InputMode, InputPlatform, InputRuntimeOptions,
        KeyMappingConfig, KeySnapshot, ModifierMask, Point, ScreenEdge,
    };
    use anyhow::Result;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;
    use tokio::io::AsyncWriteExt;
    use tokio::time::{Duration, sleep, timeout};

    #[derive(Default)]
    struct FakeBackend {
        capture: Mutex<bool>,
        warped: Mutex<Option<Point>>,
        warp_fail: AtomicBool,
        recovery_actions: Mutex<Vec<&'static str>>,
        pressed: Mutex<Option<KeySnapshot>>,
        motions: Mutex<Vec<(i32, i32)>>,
    }

    impl InputBackend for FakeBackend {
        fn layout(&self) -> Result<DesktopLayout> {
            DesktopLayout::new(vec![DisplayRect { x: 0, y: 0, width: 100, height: 100 }])
        }

        fn cursor_position(&self) -> Result<Point> {
            Ok(Point { x: 99, y: 50 })
        }

        fn snapshot(&self) -> KeySnapshot {
            self.pressed
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| KeySnapshot {
                    usages: Vec::new(),
                    modifiers: ModifierMask::default(),
                    buttons: Vec::new(),
                })
        }

        fn set_capture(&self, active: bool) -> Result<()> {
            *self.capture.lock().unwrap() = active;
            self.recovery_actions.lock().unwrap().push("capture");
            Ok(())
        }

        fn warp_cursor(&self, point: Point) -> Result<()> {
            if self.warp_fail.load(Ordering::Acquire) {
                anyhow::bail!("无法移动光标");
            }
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

        fn inject_motion(&self, dx: i32, dy: i32) -> Result<()> {
            self.motions.lock().unwrap().push((dx, dy));
            Ok(())
        }

        fn inject_wheel(&self, _x: i32, _y: i32) -> Result<()> {
            Ok(())
        }

        fn release_all(&self) -> Result<()> {
            self.recovery_actions.lock().unwrap().push("release");
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
        let input_options = test_input_options(ScreenEdge::Left);

        let task = tokio::spawn(async move {
            run_sender(
                &mut incoming,
                &outgoing,
                &mut platform,
                layout.clone(),
                ScreenEdge::Left,
                InputPlatform::current(),
                &input_options,
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
    async fn sender_blocks_activation_while_pressed_when_option_enabled() {
        let backend = Arc::new(FakeBackend::default());
        *backend.pressed.lock().unwrap() = Some(KeySnapshot {
            usages: Vec::new(),
            modifiers: ModifierMask::default(),
            buttons: vec![1],
        });
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
        let mut input_options = test_input_options(ScreenEdge::Right);
        input_options.block_switch_on_press = true;
        motion.add_at(4, 0, Point { x: 97, y: 50 });

        let task = tokio::spawn(async move {
            run_sender(
                &mut incoming,
                &outgoing,
                &mut platform,
                layout.clone(),
                ScreenEdge::Right,
                InputPlatform::current(),
                &input_options,
            )
            .await
        });

        // 按住鼠标按钮到达边缘: 不应激活.
        let blocked = timeout(Duration::from_millis(200), async {
            loop {
                let message = messages.recv().await.expect("sender 不应提前停止");
                assert!(
                    !matches!(message, InputMessage::Activate { .. }),
                    "按住状态下不应激活"
                );
            }
        })
        .await;
        assert!(blocked.is_err(), "按住状态下不应激活");
        assert!(!*backend.capture.lock().unwrap());

        // 松开后继续运动: 应正常激活.
        *backend.pressed.lock().unwrap() = None;
        motion.add_at(2, 0, Point { x: 99, y: 50 });
        let edge_position = timeout(Duration::from_secs(1), async {
            loop {
                if let InputMessage::Activate { edge_position, .. } =
                    messages.recv().await.expect("sender 不应提前停止")
                {
                    break edge_position;
                }
            }
        })
        .await
        .expect("松开后应正常激活");
        assert_eq!(edge_position, 0.5);
        assert!(*backend.capture.lock().unwrap());
        task.abort();
        let _ = task.await;
    }

    #[test]
    fn scroll_reversal_is_selected_by_source() {
        assert_eq!(
            transform_scroll(
                4,
                -7,
                ScrollSource::MouseWheel,
                InputPlatform::Macos,
                InputPlatform::Macos,
                true,
                false,
            ),
            (-4, 7)
        );
        assert_eq!(
            transform_scroll(
                4,
                -7,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Macos,
                true,
                false,
            ),
            (4, -7)
        );
        assert_eq!(
            transform_scroll(
                i32::MIN,
                i32::MAX,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Macos,
                false,
                true,
            ),
            (i32::MAX, -i32::MAX)
        );
    }

    #[test]
    fn cross_platform_scroll_flips_only_horizontal_axis() {
        assert_eq!(
            transform_scroll(
                4,
                -7,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Macos,
                false,
                false,
            ),
            (4, -7)
        );
        assert_eq!(
            transform_scroll(
                4,
                -7,
                ScrollSource::Trackpad,
                InputPlatform::Macos,
                InputPlatform::Windows,
                false,
                false,
            ),
            (-4, -7)
        );
        assert_eq!(
            transform_scroll(
                4,
                -7,
                ScrollSource::Trackpad,
                InputPlatform::Windows,
                InputPlatform::Macos,
                false,
                false,
            ),
            (-4, -7)
        );
        assert_eq!(
            transform_scroll(
                4,
                -7,
                ScrollSource::Trackpad,
                InputPlatform::Windows,
                InputPlatform::Windows,
                false,
                false,
            ),
            (4, -7)
        );
    }

    fn test_input_options(edge: ScreenEdge) -> InputRuntimeOptions {
        InputRuntimeOptions {
            mode: InputMode::Send,
            edge,
            hotkey: Hotkey::DEFAULT.parse().unwrap(),
            reverse_mouse_wheel: false,
            reverse_trackpad: false,
            block_switch_on_press: false,
            key_mapping: KeyMappingConfig::default(),
            cursor_mode: super::CursorMode::Desktop,
        }
    }

    #[test]
    fn pressed_state_tracks_down_up_and_ignores_repeats() {
        let mut pressed = PressedState::default();
        pressed.key(0x04, true, true);
        assert!(pressed.is_empty());
        pressed.key(0x04, true, false);
        assert!(!pressed.is_empty());
        pressed.key(0x04, false, false);
        assert!(pressed.is_empty());
        pressed.button(1, true);
        assert!(!pressed.is_empty());
        pressed.button(1, false);
        assert!(pressed.is_empty());
        let from_snapshot = PressedState::from_snapshot(&KeySnapshot {
            usages: vec![0x04],
            modifiers: ModifierMask::default(),
            buttons: vec![1],
        });
        assert!(!from_snapshot.is_empty());
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
                &test_input_options(ScreenEdge::Left),
                || false,
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
    async fn receiver_requests_return_at_edge_and_waits_for_approval() {
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

        let incoming_motion_for_task = Arc::clone(&incoming_motion);
        let task = tokio::spawn(async move {
            run_receiver(
                &mut incoming,
                &incoming_motion_for_task,
                &outgoing,
                &mut platform,
                layout,
                &test_input_options(ScreenEdge::Left),
                || false,
            )
            .await
        });

        // 等待接收端确认激活并安置光标.
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
        .expect("接收端应确认激活");

        // 到达返回边缘: 应发送 ReturnRequest 而非自行返回.
        for _ in 0..200 {
            incoming_motion.push(7, -1, 0);
        }
        let edge_position = timeout(Duration::from_secs(1), async {
            loop {
                match messages.recv().await {
                    Some(InputMessage::ReturnRequest { generation: 7, edge_position }) => {
                        break edge_position
                    }
                    Some(InputMessage::Return { .. }) => panic!("接收端不应自行返回"),
                    Some(_) => {}
                    None => panic!("接收端不应提前停止"),
                }
            }
        })
        .await
        .expect("接收端应请求返回");
        assert_eq!(edge_position, 0.5);

        // 未获批准前保持接受控制: 不应出现 Return.
        for _ in 0..200 {
            incoming_motion.push(7, -1, 0);
        }
        let not_returned = timeout(Duration::from_millis(300), async {
            loop {
                match messages.recv().await {
                    Some(InputMessage::Return { .. }) => panic!("未获批准前不应返回"),
                    Some(_) => {}
                    None => panic!("接收端不应提前停止"),
                }
            }
        })
        .await;
        assert!(not_returned.is_err(), "未获批准前应保持接受控制");

        // 对端批准返回后释放控制, 不再处理对端运动.
        incoming_tx
            .send(Ok(InputMessage::Return {
                generation: 7,
                edge_position: 0.5,
            }))
            .await
            .unwrap();
        for _ in 0..200 {
            incoming_motion.push(7, -1, 0);
        }
        let released = timeout(Duration::from_millis(300), async {
            loop {
                match messages.recv().await {
                    Some(InputMessage::ReturnRequest { .. }) => {
                        panic!("返回后不应再处理对端运动")
                    }
                    Some(_) => {}
                    None => panic!("接收端不应提前停止"),
                }
            }
        })
        .await;
        assert!(released.is_err(), "返回后应停止接受控制");

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn receiver_game_mode_injects_relative_motion_without_edge_return() {
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
        let mut options = test_input_options(ScreenEdge::Left);
        options.cursor_mode = super::CursorMode::Game;
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

        let incoming_motion_for_task = Arc::clone(&incoming_motion);
        let task = tokio::spawn(async move {
            run_receiver(
                &mut incoming,
                &incoming_motion_for_task,
                &outgoing,
                &mut platform,
                layout,
                &options,
                || false,
            )
            .await
        });

        // 游戏模式下连续运动应直接相对注入, 不维护绝对光标, 也不产生边缘返回请求.
        for _ in 0..500 {
            incoming_motion.push(7, -3, 2);
        }
        let no_return = timeout(Duration::from_millis(300), async {
            loop {
                match messages.recv().await {
                    Some(InputMessage::ReturnRequest { .. }) => {
                        panic!("游戏模式不应请求边缘返回")
                    }
                    Some(_) => {}
                    None => panic!("接收端不应提前停止"),
                }
            }
        })
        .await;
        assert!(no_return.is_err(), "游戏模式应持续接受控制");
        assert!(
            !backend.motions.lock().unwrap().is_empty(),
            "游戏模式应注入相对移动"
        );
        assert!(
            backend.warped.lock().unwrap().is_none(),
            "游戏模式接管不应 warp 光标"
        );

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn receiver_falls_back_to_game_mode_when_warp_blocked_by_cursor_capture() {
        let backend = Arc::new(FakeBackend::default());
        backend.warp_fail.store(true, Ordering::Release);
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
        let options = test_input_options(ScreenEdge::Left);
        let captured = Arc::new(AtomicBool::new(true));
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

        let incoming_motion_for_task = Arc::clone(&incoming_motion);
        let captured_for_task = Arc::clone(&captured);
        let task = tokio::spawn(async move {
            run_receiver(
                &mut incoming,
                &incoming_motion_for_task,
                &outgoing,
                &mut platform,
                layout,
                &options,
                move || captured_for_task.load(Ordering::Acquire),
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
        .expect("接收端应确认激活");

        // warp 被游戏锁定后应降级为游戏光标模式, 运动直接相对注入.
        for _ in 0..500 {
            incoming_motion.push(7, -3, 2);
        }
        let no_return = timeout(Duration::from_millis(300), async {
            loop {
                match messages.recv().await {
                    Some(InputMessage::ReturnRequest { .. }) => {
                        panic!("游戏模式不应请求边缘返回")
                    }
                    Some(_) => {}
                    None => panic!("接收端不应提前停止"),
                }
            }
        })
        .await;
        assert!(no_return.is_err(), "warp 失败后应保持游戏光标模式");
        assert!(
            !backend.motions.lock().unwrap().is_empty(),
            "warp 失败后应注入相对移动"
        );

        // Desktop 配置下 warp 被锁只是临时切到游戏模式, 捕获结束后应恢复桌面模式并保持控制.
        incoming_tx
            .send(Ok(InputMessage::Heartbeat { generation: 7 }))
            .await
            .unwrap();
        captured.store(false, Ordering::Release);
        sleep(Duration::from_millis(550)).await;
        incoming_tx
            .send(Ok(InputMessage::Heartbeat { generation: 7 }))
            .await
            .unwrap();
        incoming_motion.push(7, -100, 0);
        let edge_position = timeout(Duration::from_millis(800), async {
            loop {
                match messages.recv().await {
                    Some(InputMessage::ReturnRequest {
                        generation: 7,
                        edge_position,
                    }) => break edge_position,
                    Some(InputMessage::Deactivate { .. }) => {
                        panic!("捕获结束后不应释放控制")
                    }
                    Some(_) => {}
                    None => panic!("接收端不应提前停止"),
                }
            }
        })
        .await
        .expect("捕获结束后应恢复桌面光标模式");
        assert_eq!(edge_position, 0.5);

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn receiver_switches_to_desktop_when_cursor_capture_lost() {
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
        let mut options = test_input_options(ScreenEdge::Left);
        options.cursor_mode = super::CursorMode::Auto;
        let captured = Arc::new(AtomicBool::new(true));
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

        let incoming_motion_for_task = Arc::clone(&incoming_motion);
        let captured_for_task = Arc::clone(&captured);
        let task = tokio::spawn(async move {
            run_receiver(
                &mut incoming,
                &incoming_motion_for_task,
                &outgoing,
                &mut platform,
                layout,
                &options,
                move || captured_for_task.load(Ordering::Acquire),
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
        .expect("接收端应确认激活");

        // 前台光标捕获状态结束: 应切回桌面光标模式并保持远端控制, 不应释放.
        incoming_tx
            .send(Ok(InputMessage::Heartbeat { generation: 7 }))
            .await
            .unwrap();
        captured.store(false, Ordering::Release);
        sleep(Duration::from_millis(550)).await;
        incoming_tx
            .send(Ok(InputMessage::Heartbeat { generation: 7 }))
            .await
            .unwrap();

        // 切回桌面模式后, 绝对移动仍应继续, 光标越过返回边时正常请求返回.
        incoming_motion.push(7, -100, 0);
        let edge_position = timeout(Duration::from_millis(800), async {
            loop {
                match messages.recv().await {
                    Some(InputMessage::ReturnRequest {
                        generation: 7,
                        edge_position,
                    }) => break edge_position,
                    Some(InputMessage::Deactivate { .. }) => {
                        panic!("捕获状态结束后不应发送 Deactivate")
                    }
                    Some(_) => {}
                    None => panic!("接收端不应提前停止"),
                }
            }
        })
        .await
        .expect("桌面模式应继续处理绝对移动");
        assert_eq!(edge_position, 0.5);

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn sender_returns_immediately_when_requested_and_option_disabled() {
        let backend = Arc::new(FakeBackend::default());
        let layout = backend.layout().unwrap();
        let motion = Arc::new(MotionAccumulator::default());
        let (_events_tx, events) = mpsc::channel(8);
        let mut platform = PlatformHandle {
            backend: Arc::clone(&backend) as Arc<dyn InputBackend>,
            events,
            motion: Arc::clone(&motion),
            overflowed: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let (incoming_tx, mut incoming) = mpsc::channel(8);
        let (outgoing, mut messages) = mpsc::channel(16);
        motion.add_at(4, 0, Point { x: 97, y: 50 });
        let input_options = test_input_options(ScreenEdge::Right);

        let task = tokio::spawn(async move {
            run_sender(
                &mut incoming,
                &outgoing,
                &mut platform,
                layout.clone(),
                ScreenEdge::Right,
                InputPlatform::current(),
                &input_options,
            )
            .await
        });

        timeout(Duration::from_secs(1), async {
            loop {
                if let InputMessage::Activate { generation, .. } =
                    messages.recv().await.expect("sender 不应提前停止")
                {
                    assert_eq!(generation, 1);
                    break;
                }
            }
        })
        .await
        .expect("sender 应激活");

        // 对端请求返回: 选项关闭时应立即批准并恢复本机.
        incoming_tx
            .send(Ok(InputMessage::ReturnRequest {
                generation: 1,
                edge_position: 0.5,
            }))
            .await
            .unwrap();
        let edge_position = timeout(Duration::from_secs(1), async {
            loop {
                if let InputMessage::Return { edge_position, .. } =
                    messages.recv().await.expect("sender 不应提前停止")
                {
                    break edge_position;
                }
            }
        })
        .await
        .expect("应批准对端返回");
        assert_eq!(edge_position, 0.5);
        assert!(!*backend.capture.lock().unwrap());

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn sender_restores_at_receiver_reported_edge_position_on_deactivate() {
        let backend = Arc::new(FakeBackend::default());
        let layout = backend.layout().unwrap();
        let motion = Arc::new(MotionAccumulator::default());
        let (_events_tx, events) = mpsc::channel(8);
        let mut platform = PlatformHandle {
            backend: Arc::clone(&backend) as Arc<dyn InputBackend>,
            events,
            motion: Arc::clone(&motion),
            overflowed: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let (incoming_tx, mut incoming) = mpsc::channel(8);
        let (outgoing, mut messages) = mpsc::channel(16);
        motion.add_at(4, 0, Point { x: 97, y: 50 });
        let input_options = test_input_options(ScreenEdge::Right);

        let task = tokio::spawn(async move {
            run_sender(
                &mut incoming,
                &outgoing,
                &mut platform,
                layout.clone(),
                ScreenEdge::Right,
                InputPlatform::current(),
                &input_options,
            )
            .await
        });

        timeout(Duration::from_secs(1), async {
            loop {
                if let InputMessage::Activate { generation, .. } =
                    messages.recv().await.expect("sender 不应提前停止")
                {
                    assert_eq!(generation, 1);
                    break;
                }
            }
        })
        .await
        .expect("sender 应激活");

        incoming_tx
            .send(Ok(InputMessage::Deactivate {
                generation: 1,
                edge_position: Some(0.75),
            }))
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            loop {
                if !*backend.capture.lock().unwrap()
                    && backend.warped.lock().unwrap().is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("应使用接收端报告的位置恢复本机");
        assert_eq!(
            *backend.warped.lock().unwrap(),
            Some(Point { x: 91, y: 75 })
        );
        assert!(!*backend.capture.lock().unwrap());

        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn sender_blocks_return_until_local_release_when_option_enabled() {
        let backend = Arc::new(FakeBackend::default());
        let layout = backend.layout().unwrap();
        let motion = Arc::new(MotionAccumulator::default());
        let (events_tx, events) = mpsc::channel(8);
        let mut platform = PlatformHandle {
            backend: Arc::clone(&backend) as Arc<dyn InputBackend>,
            events,
            motion: Arc::clone(&motion),
            overflowed: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
        };
        let (incoming_tx, mut incoming) = mpsc::channel(8);
        let (outgoing, mut messages) = mpsc::channel(16);
        motion.add_at(4, 0, Point { x: 97, y: 50 });
        let mut input_options = test_input_options(ScreenEdge::Right);
        input_options.block_switch_on_press = true;

        let task = tokio::spawn(async move {
            run_sender(
                &mut incoming,
                &outgoing,
                &mut platform,
                layout.clone(),
                ScreenEdge::Right,
                InputPlatform::current(),
                &input_options,
            )
            .await
        });

        // 快照为空, 允许激活.
        timeout(Duration::from_secs(1), async {
            loop {
                if let InputMessage::Activate { generation, .. } =
                    messages.recv().await.expect("sender 不应提前停止")
                {
                    assert_eq!(generation, 1);
                    break;
                }
            }
        })
        .await
        .expect("sender 应激活");

        // 本机按下按钮, 对端请求返回: 应被拦截.
        events_tx
            .send(NativeEvent::Button { button: 1, down: true })
            .await
            .unwrap();
        timeout(Duration::from_secs(1), async {
            loop {
                if let InputMessage::Button { button: 1, down: true, .. } =
                    messages.recv().await.expect("sender 不应提前停止")
                {
                    break;
                }
            }
        })
        .await
        .expect("sender 应转发本机按钮按下");
        incoming_tx
            .send(Ok(InputMessage::ReturnRequest {
                generation: 1,
                edge_position: 0.5,
            }))
            .await
            .unwrap();
        let blocked = timeout(Duration::from_millis(300), async {
            loop {
                match messages.recv().await {
                    Some(InputMessage::Return { .. }) => panic!("按住状态下不应批准返回"),
                    Some(_) => {}
                    None => panic!("sender 不应提前停止"),
                }
            }
        })
        .await;
        assert!(blocked.is_err(), "按住状态下不应批准返回");
        assert!(*backend.capture.lock().unwrap());

        // 松开按钮: 应批准返回并恢复本机.
        events_tx
            .send(NativeEvent::Button { button: 1, down: false })
            .await
            .unwrap();
        let edge_position = timeout(Duration::from_secs(1), async {
            loop {
                if let InputMessage::Return { edge_position, .. } =
                    messages.recv().await.expect("sender 不应提前停止")
                {
                    break edge_position;
                }
            }
        })
        .await
        .expect("松开后应批准返回");
        assert_eq!(edge_position, 0.5);
        assert!(!*backend.capture.lock().unwrap());

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
