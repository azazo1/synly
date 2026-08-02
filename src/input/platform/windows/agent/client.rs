use super::pipe::{NativePipe, PipeDirection};
use super::protocol::{
    AgentRequest, AgentResponse, AgentToGuiPacket, GuiToAgentPacket, is_timeout_error, read_packet,
    write_packet,
};
use super::security::{
    PipeSecurity, current_process_is_elevated, process_session_id, validate_pipe_client, wide,
};
use super::{
    AGENT_HEARTBEAT_TIMEOUT, CLIENT_HEARTBEAT_INTERVAL, CONNECT_TIMEOUT, DISPATCH_TIMEOUT,
    REQUEST_DELIVERY_TIMEOUT,
};
use super::super::super::{CaptureContext, InputBackend, NativeEvent};
use crate::input::{DesktopLayout, InputMode, KeySnapshot, ModifierMask, Point};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, GetCurrentProcessId};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

static AGENT: OnceLock<Mutex<Option<Arc<AgentClient>>>> = OnceLock::new();
static ELEVATION_REQUESTED: AtomicBool = AtomicBool::new(false);

pub(super) struct ClientCommand {
    pub(super) request: AgentRequest,
    pub(super) dispatched: Option<std::sync::mpsc::SyncSender<()>>,
    pub(super) response: Option<std::sync::mpsc::SyncSender<Result<AgentResponse, String>>>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct CursorUpdate {
    pub(super) point: Point,
}

pub(super) type PendingResponses = Arc<Mutex<HashMap<u64, PendingResponse>>>;

pub(super) struct PendingResponse {
    request: &'static str,
    caller: Option<std::sync::mpsc::SyncSender<Result<AgentResponse, String>>>,
    completion: std::sync::mpsc::SyncSender<Result<(), String>>,
}

pub(super) enum ClientQueueItem {
    Command(ClientCommand),
    Cursor,
}

pub(super) struct ClientCursorState {
    pub(super) latest: Mutex<Option<CursorUpdate>>,
    pub(super) queued: AtomicBool,
}

#[derive(Clone)]
struct AgentHello {
    agent_pid: u32,
    agent_path: PathBuf,
}

pub(super) struct AgentClient {
    pub(super) commands: std::sync::mpsc::SyncSender<ClientQueueItem>,
    pub(super) cursor: Arc<ClientCursorState>,
    pub(super) context: Arc<Mutex<Option<CaptureContext>>>,
    pub(super) alive: Arc<AtomicBool>,
    pub(super) lifecycle: Mutex<()>,
    pub(super) next_lease: AtomicU64,
    pub(super) active_lease: AtomicU64,
}

struct GuiTransportStart {
    created: std::sync::mpsc::Receiver<Result<(), String>>,
    ready: std::sync::mpsc::Receiver<Result<Arc<AgentClient>, String>>,
}

impl GuiTransportStart {
    fn wait_until_created(&self) -> Result<()> {
        for _ in 0..2 {
            match self.created.recv_timeout(CONNECT_TIMEOUT) {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(anyhow!(error)),
                Err(error) => {
                    return Err(error).context("等待 Windows 输入代理命名管道创建超时");
                }
            }
        }
        Ok(())
    }

    fn wait_until_ready(self) -> Result<Arc<AgentClient>> {
        match self.ready.recv_timeout(CONNECT_TIMEOUT) {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(error)) => Err(anyhow!(error)),
            Err(error) => Err(error).context("等待 Windows 输入代理授权和连接超时"),
        }
    }
}

pub(super) struct AgentBackend {
    pub(super) client: Arc<AgentClient>,
    pub(super) lease: u64,
    pub(super) layout: DesktopLayout,
}

pub fn request_elevation() -> Result<()> {
    if let Some(client) = current_client() {
        if client.request(AgentRequest::Health).is_ok() {
            return Ok(());
        }
        mark_agent_unavailable(&client.alive, "Health", "elevation-recheck");
    }

    let connection_id = Uuid::new_v4();
    let command_pipe_name = format!(r"\\.\pipe\synly-input-command-{connection_id}");
    let event_pipe_name = format!(r"\\.\pipe\synly-input-event-{connection_id}");
    let token = Uuid::new_v4().to_string();
    let parent_pid = unsafe { GetCurrentProcessId() };
    let transport = start_gui_transport(
        command_pipe_name.clone(),
        event_pipe_name.clone(),
        token.clone(),
        parent_pid,
    )?;
    transport.wait_until_created()?;

    let executable = agent_executable()?;
    if current_process_is_elevated()? {
        tracing::info!("当前 Synly 进程已提升, 直接启动隐藏输入代理");
        launch_with_current_token(
            &executable,
            &command_pipe_name,
            &event_pipe_name,
            &token,
            parent_pid,
        )?;
    } else {
        tracing::info!("当前 Synly 进程未提升, 请求 UAC 启动输入代理");
        launch_elevated(
            &executable,
            &command_pipe_name,
            &event_pipe_name,
            &token,
            parent_pid,
        )?;
    }

    let client = transport.wait_until_ready()?;
    let slot = AGENT.get_or_init(|| Mutex::new(None));
    *slot.lock().map_err(|_| anyhow!("Windows input agent state poisoned"))? = Some(client);
    ELEVATION_REQUESTED.store(true, Ordering::Release);
    Ok(())
}

pub(in crate::input) fn ensure_ready(mode: InputMode) -> Result<()> {
    if mode == InputMode::Off {
        return Ok(());
    }
    let client = current_client().context("Windows 输入代理尚未获得本机管理员授权")?;
    if let Err(error) = client.request(AgentRequest::Health) {
        mark_agent_unavailable(&client.alive, "Health", "startup-recheck");
        return Err(error);
    }
    Ok(())
}

pub(in crate::input) fn is_ready() -> bool {
    current_client().is_some()
}

pub(in crate::input) fn elevation_requested() -> bool {
    ELEVATION_REQUESTED.load(Ordering::Acquire)
}

pub(in crate::input) fn start_client(context: CaptureContext) -> Result<Arc<dyn InputBackend>> {
    ensure_ready(context.mode)?;
    let client = current_client().context("Windows 输入代理连接不可用")?;
    let lifecycle = client
        .lifecycle
        .lock()
        .map_err(|_| anyhow!("Windows input agent lifecycle poisoned"))?;
    let layout = match client.request(AgentRequest::Start {
        mode: context.mode,
        hotkey: context.hotkey,
    })? {
        AgentResponse::Started { layout } => layout,
        _ => bail!("Windows input agent returned an invalid Start response"),
    };
    let lease = client.next_lease.fetch_add(1, Ordering::AcqRel);
    client.active_lease.store(lease, Ordering::Release);
    *client
        .context
        .lock()
        .map_err(|_| anyhow!("Windows input agent context poisoned"))? = Some(context);
    drop(lifecycle);
    Ok(Arc::new(AgentBackend {
        client,
        lease,
        layout,
    }))
}

impl AgentClient {
    pub(super) fn request(&self, request: AgentRequest) -> Result<AgentResponse> {
        if !self.alive.load(Ordering::Acquire) {
            bail!("Windows input agent connection is closed");
        }
        let request_name = request.name();
        let (dispatch_tx, dispatch_rx) = std::sync::mpsc::sync_channel(1);
        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        self.commands
            .try_send(ClientQueueItem::Command(ClientCommand {
                request,
                dispatched: Some(dispatch_tx),
                response: Some(response_tx),
            }))
            .map_err(|error| {
                if matches!(&error, std::sync::mpsc::TrySendError::Disconnected(_)) {
                    mark_agent_unavailable(&self.alive, request_name, "command-queue-closed");
                }
                anyhow!("Windows input agent {request_name} command queue unavailable: {error}")
            })?;
        wait_for_agent_response(
            request_name,
            dispatch_rx,
            response_rx,
            DISPATCH_TIMEOUT,
        )
    }

    pub(super) fn notify(&self, request: AgentRequest) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            bail!("Windows input agent connection is closed");
        }
        let request = match request {
            AgentRequest::InjectCursor(point) => {
                if let Ok(mut latest) = self.cursor.latest.lock() {
                    *latest = Some(CursorUpdate { point });
                }
                if !self.cursor.queued.swap(true, Ordering::AcqRel) {
                    match self.commands.try_send(ClientQueueItem::Cursor) {
                        Ok(()) => {}
                        Err(std::sync::mpsc::TrySendError::Full(_)) => {
                            self.cursor.queued.store(false, Ordering::Release);
                        }
                        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                            self.cursor.queued.store(false, Ordering::Release);
                            mark_agent_unavailable(
                                &self.alive,
                                "InjectCursor",
                                "cursor-channel-closed",
                            );
                        }
                    }
                }
                return Ok(());
            }
            request => request,
        };
        let request_name = request.name();
        self.commands
            .try_send(ClientQueueItem::Command(ClientCommand {
                request,
                dispatched: None,
                response: None,
            }))
            .map_err(|error| {
                if matches!(&error, std::sync::mpsc::TrySendError::Disconnected(_)) {
                    mark_agent_unavailable(&self.alive, request_name, "notify-queue-closed");
                }
                anyhow!("Windows input agent {request_name} command queue unavailable: {error}")
            })?;
        Ok(())
    }

    fn emit_failure(&self, error: &anyhow::Error) {
        if let Ok(context) = self.context.lock()
            && let Some(context) = context.as_ref()
        {
            context.emit_reliable(NativeEvent::Failed(format!("{error:#}")));
        }
    }
}

fn mark_agent_unavailable(alive: &AtomicBool, request_name: &str, phase: &str) {
    if alive.swap(false, Ordering::AcqRel) {
        tracing::error!(
            target: "synly_input_agent",
            request = request_name,
            %phase,
            "Windows 输入代理连接已标记为不可用"
        );
    }
}

pub(super) fn wait_for_agent_response(
    request_name: &str,
    dispatch_rx: std::sync::mpsc::Receiver<()>,
    response_rx: std::sync::mpsc::Receiver<Result<AgentResponse, String>>,
    dispatch_timeout: Duration,
) -> Result<AgentResponse> {
    match dispatch_rx.recv_timeout(dispatch_timeout) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            bail!("Windows input agent {request_name} dispatch timed out");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            bail!("Windows input agent {request_name} dispatch failed before pipe write");
        }
    }
    match response_rx.recv() {
        Ok(Ok(AgentResponse::Error(message))) => Err(anyhow!(message)),
        Ok(Ok(response)) => Ok(response),
        Ok(Err(message)) => Err(anyhow!(message)),
        Err(error) => Err(error).with_context(|| {
            format!("Windows input agent {request_name} response channel closed")
        }),
    }
}

impl InputBackend for AgentBackend {
    fn health_check(&self) -> Result<()> {
        if !self.is_current() {
            bail!("Windows input agent backend was superseded by a newer session");
        }
        if !self.client.alive.load(Ordering::Acquire) {
            bail!("Windows input agent connection is closed");
        }
        Ok(())
    }

    fn layout(&self) -> Result<DesktopLayout> {
        Ok(self.layout.clone())
    }

    fn cursor_position(&self) -> Result<Point> {
        match self.request(AgentRequest::CursorPosition)? {
            AgentResponse::Point(point) => Ok(point),
            _ => bail!("Windows input agent returned an invalid cursor response"),
        }
    }

    fn snapshot(&self) -> KeySnapshot {
        match self.request(AgentRequest::Snapshot) {
            Ok(AgentResponse::Snapshot(snapshot)) => snapshot,
            Ok(_) => KeySnapshot {
                usages: Vec::new(),
                modifiers: ModifierMask::default(),
                buttons: Vec::new(),
            },
            Err(error) => {
                if self.is_current() {
                    self.client.emit_failure(&error);
                }
                KeySnapshot {
                    usages: Vec::new(),
                    modifiers: ModifierMask::default(),
                    buttons: Vec::new(),
                }
            }
        }
    }

    fn set_capture(&self, active: bool) -> Result<()> {
        self.request(AgentRequest::SetCapture(active))?;
        Ok(())
    }

    fn warp_cursor(&self, point: Point) -> Result<()> {
        self.request(AgentRequest::WarpCursor(point))?;
        Ok(())
    }

    fn inject_key(
        &self,
        usage: u16,
        modifiers: ModifierMask,
        down: bool,
        repeat: bool,
    ) -> Result<()> {
        self.request(AgentRequest::InjectKey {
            usage,
            modifiers,
            down,
            repeat,
        })?;
        Ok(())
    }

    fn inject_button(&self, button: u8, down: bool) -> Result<()> {
        self.request(AgentRequest::InjectButton { button, down })?;
        Ok(())
    }

    fn inject_cursor(&self, point: Point) -> Result<()> {
        self.notify(AgentRequest::InjectCursor(point))
    }

    fn inject_motion(&self, dx: i32, dy: i32) -> Result<()> {
        self.notify(AgentRequest::InjectMotion { dx, dy })
    }

    fn inject_wheel(&self, x: i32, y: i32) -> Result<()> {
        self.request(AgentRequest::InjectWheel { x, y })?;
        Ok(())
    }

    fn release_all(&self) -> Result<()> {
        self.request(AgentRequest::ReleaseAll)?;
        Ok(())
    }
}

impl AgentBackend {
    fn request(&self, request: AgentRequest) -> Result<AgentResponse> {
        let _lifecycle = self
            .client
            .lifecycle
            .lock()
            .map_err(|_| anyhow!("Windows input agent lifecycle poisoned"))?;
        if !self.is_current() {
            bail!("Windows input agent backend was superseded by a newer session");
        }
        let response = self.client.request(request);
        if let Err(error) = &response {
            self.client.emit_failure(error);
        }
        response
    }

    fn is_current(&self) -> bool {
        self.client.active_lease.load(Ordering::Acquire) == self.lease
    }

    fn notify(&self, request: AgentRequest) -> Result<()> {
        let _lifecycle = self
            .client
            .lifecycle
            .lock()
            .map_err(|_| anyhow!("Windows input agent lifecycle poisoned"))?;
        if !self.is_current() {
            bail!("Windows input agent backend was superseded by a newer session");
        }
        if !self.client.alive.load(Ordering::Acquire) {
            bail!("Windows input agent connection is closed");
        }
        self.client.notify(request)
    }
}

impl Drop for AgentBackend {
    fn drop(&mut self) {
        let Ok(_lifecycle) = self.client.lifecycle.lock() else {
            return;
        };
        if self
            .client
            .active_lease
            .compare_exchange(self.lease, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let _ = self.client.notify(AgentRequest::Stop);
        if let Ok(mut context) = self.client.context.lock() {
            *context = None;
        }
    }
}

fn start_gui_transport(
    command_pipe_name: String,
    event_pipe_name: String,
    token: String,
    parent_pid: u32,
) -> Result<GuiTransportStart> {
    let (commands, command_rx) = std::sync::mpsc::sync_channel(64);
    let cursor = Arc::new(ClientCursorState {
        latest: Mutex::new(None),
        queued: AtomicBool::new(false),
    });
    let context = Arc::new(Mutex::new(None));
    let alive = Arc::new(AtomicBool::new(true));
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let client = Arc::new(AgentClient {
        commands,
        cursor: Arc::clone(&cursor),
        context: Arc::clone(&context),
        alive: Arc::clone(&alive),
        lifecycle: Mutex::new(()),
        next_lease: AtomicU64::new(1),
        active_lease: AtomicU64::new(0),
    });
    let (created_tx, created_rx) = std::sync::mpsc::sync_channel(2);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(2);
    let (hello_tx, hello_rx) = std::sync::mpsc::sync_channel(1);

    let command_created = created_tx.clone();
    let command_ready = ready_tx.clone();
    let command_pending = Arc::clone(&pending);
    let command_alive = Arc::clone(&alive);
    let command_context = Arc::clone(&context);
    let command_cursor = Arc::clone(&cursor);
    std::thread::Builder::new()
        .name("synly-input-command".to_string())
        .spawn(move || {
            let result = gui_command_owner(
                command_pipe_name,
                parent_pid,
                command_created,
                hello_rx,
                command_rx,
                command_cursor,
                Arc::clone(&command_pending),
                Arc::clone(&command_alive),
            );
            if let Err(error) = result {
                let message = format!("{error:#}");
                let _ = command_ready.send(Err(message.clone()));
                fail_gui_transport(
                    &command_alive,
                    &command_pending,
                    &command_context,
                    &message,
                );
                tracing::error!(error = %message, "Windows 输入代理 command pipe 线程结束");
            }
        })
        .context("failed to start Windows input agent command owner")?;

    let event_client = Arc::clone(&client);
    let event_pending = Arc::clone(&pending);
    let event_alive = Arc::clone(&alive);
    let event_context = Arc::clone(&context);
    std::thread::Builder::new()
        .name("synly-input-event".to_string())
        .spawn(move || {
            let result = gui_event_owner(
                event_pipe_name,
                token,
                parent_pid,
                created_tx,
                hello_tx,
                Arc::clone(&event_client),
                Arc::clone(&event_pending),
                Arc::clone(&event_context),
                Arc::clone(&event_alive),
                ready_tx.clone(),
            );
            if let Err(error) = result {
                let message = format!("{error:#}");
                let _ = ready_tx.send(Err(message.clone()));
                fail_gui_transport(
                    &event_alive,
                    &event_pending,
                    &event_context,
                    &message,
                );
                tracing::error!(error = %message, "Windows 输入代理 event pipe 线程结束");
            }
        })
        .context("failed to start Windows input agent event owner")?;

    Ok(GuiTransportStart {
        created: created_rx,
        ready: ready_rx,
    })
}

#[allow(clippy::too_many_arguments)]
fn gui_command_owner(
    pipe_name: String,
    parent_pid: u32,
    created: std::sync::mpsc::SyncSender<Result<(), String>>,
    hello: std::sync::mpsc::Receiver<AgentHello>,
    commands: std::sync::mpsc::Receiver<ClientQueueItem>,
    cursor: Arc<ClientCursorState>,
    pending: PendingResponses,
    alive: Arc<AtomicBool>,
) -> Result<()> {
    let security = PipeSecurity::for_current_user()?;
    let mut pipe = match NativePipe::create_server(
        &pipe_name,
        PipeDirection::ServerToClient,
        &security.attributes,
    ) {
        Ok(pipe) => pipe,
        Err(error) => {
            let _ = created.send(Err(format!("{error:#}")));
            return Err(error);
        }
    };
    created
        .send(Ok(()))
        .map_err(|_| anyhow!("Windows input agent pipe creation receiver closed"))?;
    pipe.connect_server(CONNECT_TIMEOUT)?;
    let hello = hello
        .recv_timeout(CONNECT_TIMEOUT)
        .context("Windows input agent command owner did not receive agent identity")?;
    validate_pipe_client(&pipe, hello.agent_pid, &hello.agent_path)?;
    let session_id = process_session_id(parent_pid)?;
    write_packet(
        &mut pipe,
        &GuiToAgentPacket::HelloAck { session_id },
        REQUEST_DELIVERY_TIMEOUT,
    )?;
    command_writer_loop(pipe, commands, cursor, pending, alive)
}

#[allow(clippy::too_many_arguments)]
fn gui_event_owner(
    pipe_name: String,
    token: String,
    parent_pid: u32,
    created: std::sync::mpsc::SyncSender<Result<(), String>>,
    hello_sender: std::sync::mpsc::SyncSender<AgentHello>,
    client: Arc<AgentClient>,
    pending: PendingResponses,
    context: Arc<Mutex<Option<CaptureContext>>>,
    alive: Arc<AtomicBool>,
    ready: std::sync::mpsc::SyncSender<Result<Arc<AgentClient>, String>>,
) -> Result<()> {
    let security = PipeSecurity::for_current_user()?;
    let mut pipe = match NativePipe::create_server(
        &pipe_name,
        PipeDirection::ClientToServer,
        &security.attributes,
    ) {
        Ok(pipe) => pipe,
        Err(error) => {
            let _ = created.send(Err(format!("{error:#}")));
            return Err(error);
        }
    };
    created
        .send(Ok(()))
        .map_err(|_| anyhow!("Windows input agent pipe creation receiver closed"))?;
    pipe.connect_server(CONNECT_TIMEOUT)?;
    let hello: AgentToGuiPacket = read_packet(&mut pipe, CONNECT_TIMEOUT)?;
    let AgentToGuiPacket::Hello {
        token: incoming_token,
        agent_pid,
        parent_pid: incoming_parent,
        agent_path,
    } = hello
    else {
        bail!("Windows input agent sent an invalid handshake");
    };
    if incoming_token != token || incoming_parent != parent_pid {
        bail!("Windows input agent handshake token or parent mismatch");
    }
    validate_pipe_client(&pipe, agent_pid, &agent_path)?;
    hello_sender
        .send(AgentHello {
            agent_pid,
            agent_path: agent_path.clone(),
        })
        .map_err(|_| anyhow!("Windows input agent command owner stopped during handshake"))?;
    match read_packet::<AgentToGuiPacket>(&mut pipe, CONNECT_TIMEOUT)? {
        AgentToGuiPacket::Ready => {}
        AgentToGuiPacket::StartupError { error } => {
            bail!("Windows input agent startup failed: {error}");
        }
        other => {
            bail!(
                "Windows input agent sent an invalid startup packet: {}",
                other.packet_name()
            );
        }
    }
    ready
        .send(Ok(client))
        .map_err(|_| anyhow!("Windows input agent readiness receiver closed"))?;
    client_event_reader_loop(pipe, pending, context, alive)
}

pub(super) fn client_event_reader_loop(
    mut pipe: NativePipe,
    pending: PendingResponses,
    context: Arc<Mutex<Option<CaptureContext>>>,
    alive: Arc<AtomicBool>,
) -> Result<()> {
    while alive.load(Ordering::Acquire) {
        let packet = match read_packet::<AgentToGuiPacket>(&mut pipe, AGENT_HEARTBEAT_TIMEOUT) {
            Ok(packet) => packet,
            Err(_) if !alive.load(Ordering::Acquire) => return Ok(()),
            Err(error) if is_timeout_error(&error) => continue,
            Err(error) => return Err(error),
        };
        match packet {
            AgentToGuiPacket::Response { id, response } => {
                let pending_response = pending
                    .lock()
                    .map_err(|_| anyhow!("Windows input agent pending response state poisoned"))?
                    .remove(&id);
                let Some(pending_response) = pending_response else {
                    continue;
                };
                if pending_response.request == "InjectCursor"
                    && let AgentResponse::Error(message) = &response
                    && let Ok(context) = context.lock()
                    && let Some(context) = context.as_ref()
                {
                    context.emit_reliable(NativeEvent::Failed(format!(
                        "Windows input agent cursor injection failed: {message}"
                    )));
                }
                if let Some(caller) = pending_response.caller {
                    let _ = caller.send(Ok(response));
                }
                let _ = pending_response.completion.send(Ok(()));
            }
            AgentToGuiPacket::Event(event) => {
                if let Ok(context) = context.lock()
                    && let Some(context) = context.as_ref()
                {
                    context.emit_reliable(event);
                }
            }
            AgentToGuiPacket::Motion {
                dx,
                dy,
                position,
                position_updated,
            } => {
                if let Ok(context) = context.lock()
                    && let Some(context) = context.as_ref()
                {
                    if position_updated {
                        if let Some(position) = position {
                            context.motion.add_at(dx, dy, position);
                        }
                    } else {
                        context.motion.add(dx, dy);
                    }
                }
            }
            AgentToGuiPacket::SecureDesktopPaused(paused) => {
                if let Ok(context) = context.lock()
                    && let Some(context) = context.as_ref()
                    && paused
                {
                    context.emit_reliable(NativeEvent::Emergency);
                }
                tracing::warn!(paused, "Windows 输入代理安全桌面状态变化");
            }
            _ => bail!("Windows input agent sent an unexpected packet"),
        }
    }
    Ok(())
}

pub(super) fn command_writer_loop(
    mut pipe: NativePipe,
    commands: std::sync::mpsc::Receiver<ClientQueueItem>,
    cursor: Arc<ClientCursorState>,
    pending: PendingResponses,
    alive: Arc<AtomicBool>,
) -> Result<()> {
    let mut next_id = 1u64;
    let mut next_heartbeat = Instant::now() + CLIENT_HEARTBEAT_INTERVAL;
    while alive.load(Ordering::Acquire) {
        let heartbeat_wait = next_heartbeat.saturating_duration_since(Instant::now());
        let wait = heartbeat_wait.min(Duration::from_millis(50));
        match commands.recv_timeout(wait) {
            Ok(ClientQueueItem::Command(command)) => {
                if command.request.requires_cursor_ordering() {
                    flush_latest_cursor(&mut pipe, &cursor, &pending, &mut next_id)?;
                }
                let _ = write_client_command(&mut pipe, command, &pending, &mut next_id)?;
                next_heartbeat = Instant::now() + CLIENT_HEARTBEAT_INTERVAL;
            }
            Ok(ClientQueueItem::Cursor) => {
                flush_latest_cursor(&mut pipe, &cursor, &pending, &mut next_id)?;
                next_heartbeat = Instant::now() + CLIENT_HEARTBEAT_INTERVAL;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                flush_latest_cursor(&mut pipe, &cursor, &pending, &mut next_id)?;
                if Instant::now() >= next_heartbeat {
                    let _ = write_client_command(
                        &mut pipe,
                        ClientCommand {
                            request: AgentRequest::Health,
                            dispatched: None,
                            response: None,
                        },
                        &pending,
                        &mut next_id,
                    )?;
                    next_heartbeat = Instant::now() + CLIENT_HEARTBEAT_INTERVAL;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

fn flush_latest_cursor(
    pipe: &mut NativePipe,
    cursor: &ClientCursorState,
    pending: &PendingResponses,
    next_id: &mut u64,
) -> Result<()> {
    let update = {
        let mut latest = cursor
            .latest
            .lock()
            .map_err(|_| anyhow!("Windows input agent cursor state poisoned"))?;
        let update = latest.take();
        cursor.queued.store(false, Ordering::Release);
        update
    };
    if let Some(update) = update {
        let _ = write_client_command(
            pipe,
            ClientCommand {
                request: AgentRequest::InjectCursor(update.point),
                dispatched: None,
                response: None,
            },
            pending,
            next_id,
        )?;
    }
    Ok(())
}

pub(super) fn write_client_command(
    pipe: &mut NativePipe,
    command: ClientCommand,
    pending: &PendingResponses,
    next_id: &mut u64,
) -> Result<bool> {
    let ClientCommand {
        request,
        dispatched,
        response,
    } = command;
    let id = *next_id;
    *next_id = next_id.saturating_add(1);
    let request_name = request.name();
    let (completion_tx, completion_rx) = std::sync::mpsc::sync_channel(1);
    pending
        .lock()
        .map_err(|_| anyhow!("Windows input agent pending response state poisoned"))?
        .insert(
            id,
            PendingResponse {
                request: request_name,
                caller: response,
                completion: completion_tx,
            },
        );
    if let Err(error) = write_packet(
        pipe,
        &GuiToAgentPacket::Request { id, request },
        REQUEST_DELIVERY_TIMEOUT,
    ) {
        complete_pending_with_error(pending, id, format!("{error:#}"));
        return Err(error);
    }
    if let Some(dispatched) = dispatched {
        let _ = dispatched.send(());
    }
    match completion_rx.recv_timeout(REQUEST_DELIVERY_TIMEOUT) {
        Ok(Ok(())) => Ok(true),
        Ok(Err(message)) => {
            tracing::warn!(request = request_name, error = %message, "Windows 输入代理请求未完成, 保持连接");
            Ok(false)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let message = format!("Windows input agent {request_name} response timed out");
            complete_pending_with_error(pending, id, message.clone());
            tracing::warn!(request = request_name, "Windows 输入代理请求响应超时, 保持连接");
            Ok(false)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let message = format!("Windows input agent {request_name} response channel closed");
            complete_pending_with_error(pending, id, message.clone());
            tracing::warn!(request = request_name, "Windows 输入代理请求响应通道关闭, 保持连接");
            Ok(false)
        }
    }
}

fn complete_pending_with_error(pending: &PendingResponses, id: u64, message: String) {
    let pending_response = pending
        .lock()
        .ok()
        .and_then(|mut pending| pending.remove(&id));
    if let Some(pending_response) = pending_response {
        if let Some(caller) = pending_response.caller {
            let _ = caller.send(Err(message.clone()));
        }
        let _ = pending_response.completion.send(Err(message));
    }
}

fn fail_gui_transport(
    alive: &AtomicBool,
    pending: &PendingResponses,
    context: &Mutex<Option<CaptureContext>>,
    message: &str,
) {
    let first_failure = alive.swap(false, Ordering::AcqRel);
    let pending_responses = pending
        .lock()
        .map(|mut pending| std::mem::take(&mut *pending))
        .unwrap_or_default();
    for (_, pending_response) in pending_responses {
        if let Some(caller) = pending_response.caller {
            let _ = caller.send(Err(message.to_string()));
        }
        let _ = pending_response.completion.send(Err(message.to_string()));
    }
    if first_failure
        && let Ok(context) = context.lock()
        && let Some(context) = context.as_ref()
    {
        context.emit_reliable(NativeEvent::Failed(message.to_string()));
    }
}

pub(super) fn current_client() -> Option<Arc<AgentClient>> {
    AGENT
        .get()
        .and_then(|slot| slot.lock().ok())
        .and_then(|slot| slot.as_ref().cloned())
        .filter(|client| client.alive.load(Ordering::Acquire))
}

fn agent_executable() -> Result<PathBuf> {
    let current = std::env::current_exe().context("failed to locate Synly executable")?;
    let host = resolve_agent_host(&current);
    if !host.is_file() {
        bail!("Windows input agent host is missing: {}", host.display());
    }
    if host != current {
        tracing::info!(
            host = %host.display(),
            "Windows 输入代理宿主已解析为同目录主程序"
        );
    }
    Ok(host)
}

fn resolve_agent_host(current: &Path) -> PathBuf {
    resolve_agent_host_with(current, |path| path.is_file())
}

fn resolve_agent_host_with(current: &Path, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let is_host = current
        .file_stem()
        .map(|stem| stem.to_string_lossy().eq_ignore_ascii_case("synly"))
        .unwrap_or(false);
    if is_host {
        return current.to_path_buf();
    }
    let sibling = current.with_file_name("synly.exe");
    if exists(&sibling) {
        sibling
    } else {
        current.to_path_buf()
    }
}

fn launch_elevated(
    executable: &Path,
    command_pipe_name: &str,
    event_pipe_name: &str,
    token: &str,
    parent_pid: u32,
) -> Result<()> {
    let verb = wide("runas");
    let executable = wide(&executable.to_string_lossy());
    let parameters = wide(&format!(
        "__input-agent --command-pipe \"{command_pipe_name}\" --event-pipe \"{event_pipe_name}\" --token \"{token}\" --parent-pid {parent_pid}"
    ));
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            executable.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            SW_HIDE,
        )
    } as isize;
    if result <= 32 {
        bail!("Windows UAC elevation request failed or was rejected: {result}");
    }
    Ok(())
}

fn launch_with_current_token(
    executable: &Path,
    command_pipe_name: &str,
    event_pipe_name: &str,
    token: &str,
    parent_pid: u32,
) -> Result<()> {
    let mut command = Command::new(executable);
    command
        .arg("__input-agent")
        .arg("--command-pipe")
        .arg(command_pipe_name)
        .arg("--event-pipe")
        .arg(event_pipe_name)
        .arg("--token")
        .arg(token)
        .arg("--parent-pid")
        .arg(parent_pid.to_string())
        .creation_flags(CREATE_NO_WINDOW);
    command
        .spawn()
        .context("failed to start Windows input agent with the current elevated token")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_agent_host_with;
    use std::path::Path;

    #[test]
    fn auxiliary_binary_prefers_sibling_synly_as_agent_host() {
        let mock = Path::new(r"C:\tools\input-receiver-mock.exe");
        let main = Path::new(r"C:\tools\synly.exe");
        let exists = |path: &Path| path == main;
        assert_eq!(resolve_agent_host_with(mock, exists), main);
        assert_eq!(resolve_agent_host_with(main, exists), main);
    }

    #[test]
    fn auxiliary_binary_falls_back_to_itself_without_sibling_host() {
        let mock = Path::new(r"C:\tools\input-receiver-mock.exe");
        let main = Path::new(r"C:\tools\synly.exe");
        let standalone = Path::new(r"C:\tools\standalone.exe");
        assert_eq!(resolve_agent_host_with(mock, |_| false), mock);
        assert_eq!(resolve_agent_host_with(standalone, |_| false), standalone);
        assert_eq!(resolve_agent_host_with(main, |_| false), main);
    }
}
