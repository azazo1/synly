use super::platform::{CaptureContext, InputBackend, MotionAccumulator, NativeEvent};
use super::windows_ipc::{NativePipe, PipeDirection};
use super::{DesktopLayout, Hotkey, InputMode, KeySnapshot, ModifierMask, Point};
use anyhow::{Context, Result, anyhow, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};
use uuid::Uuid;
use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows_sys::Win32::Security::WinTrust::{
    WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_FILE_INFO,
    WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE, WinVerifyTrust,
};
use windows_sys::Win32::System::Pipes::{
    GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

const IPC_VERSION: u16 = 7;
const IPC_MAX_FRAME: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_DELIVERY_TIMEOUT: Duration = Duration::from_secs(4);
const CLIENT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
const AGENT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);

static AGENT: OnceLock<Mutex<Option<Arc<AgentClient>>>> = OnceLock::new();
static ELEVATION_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug, Serialize, Deserialize)]
enum AgentRequest {
    Start { mode: InputMode, hotkey: Hotkey },
    Stop,
    Health,
    CursorPosition,
    Snapshot,
    SetCapture(bool),
    WarpCursor(Point),
    InjectKey {
        usage: u16,
        modifiers: ModifierMask,
        down: bool,
        repeat: bool,
    },
    InjectButton { button: u8, down: bool },
    InjectCursor(Point),
    InjectWheel { x: i32, y: i32 },
    ReleaseAll,
}

impl AgentRequest {
    fn name(&self) -> &'static str {
        match self {
            Self::Start { .. } => "Start",
            Self::Stop => "Stop",
            Self::Health => "Health",
            Self::CursorPosition => "CursorPosition",
            Self::Snapshot => "Snapshot",
            Self::SetCapture(_) => "SetCapture",
            Self::WarpCursor(_) => "WarpCursor",
            Self::InjectKey { .. } => "InjectKey",
            Self::InjectButton { .. } => "InjectButton",
            Self::InjectCursor(_) => "InjectCursor",
            Self::InjectWheel { .. } => "InjectWheel",
            Self::ReleaseAll => "ReleaseAll",
        }
    }

    fn requires_cursor_ordering(&self) -> bool {
        matches!(self, Self::InjectButton { .. } | Self::InjectWheel { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum AgentResponse {
    Ok,
    Pong,
    Started { layout: DesktopLayout },
    Point(Point),
    Snapshot(KeySnapshot),
    Error(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum GuiToAgentPacket {
    HelloAck {
        version: u16,
        session_id: u32,
    },
    Request {
        id: u64,
        request: AgentRequest,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum AgentToGuiPacket {
    Hello {
        version: u16,
        token: String,
        agent_pid: u32,
        parent_pid: u32,
        agent_path: PathBuf,
    },
    Ready,
    StartupError {
        error: String,
    },
    Response {
        id: u64,
        response: AgentResponse,
    },
    Event(NativeEvent),
    Motion {
        dx: i32,
        dy: i32,
        position: Option<Point>,
        position_updated: bool,
    },
    SecureDesktopPaused(bool),
}

#[derive(Clone, Copy, Debug)]
struct AgentMotion {
    dx: i32,
    dy: i32,
    position: Option<Point>,
    position_updated: bool,
}

#[derive(Clone)]
struct AgentOutput {
    reliable: std::sync::mpsc::SyncSender<AgentToGuiPacket>,
    motion: Arc<AgentMotionSlot>,
}

struct ClientCommand {
    request: AgentRequest,
    dispatched: Option<std::sync::mpsc::SyncSender<()>>,
    response: Option<std::sync::mpsc::SyncSender<Result<AgentResponse, String>>>,
}

#[derive(Clone, Copy, Debug)]
struct CursorUpdate {
    point: Point,
}

type PendingResponses = Arc<Mutex<HashMap<
    u64,
    PendingResponse,
>>>;

struct PendingResponse {
    request: &'static str,
    caller: Option<std::sync::mpsc::SyncSender<Result<AgentResponse, String>>>,
    completion: std::sync::mpsc::SyncSender<Result<(), String>>,
}

enum ClientQueueItem {
    Command(ClientCommand),
    Cursor,
}

struct ClientCursorState {
    latest: Mutex<Option<CursorUpdate>>,
    queued: AtomicBool,
}

struct AgentMotionSlot {
    latest: Mutex<Option<AgentMotion>>,
    changed: AtomicBool,
}

#[derive(Clone)]
struct AgentHello {
    agent_pid: u32,
    agent_path: PathBuf,
}

struct AgentClient {
    commands: std::sync::mpsc::SyncSender<ClientQueueItem>,
    cursor: Arc<ClientCursorState>,
    context: Arc<Mutex<Option<CaptureContext>>>,
    alive: Arc<AtomicBool>,
    lifecycle: Mutex<()>,
    next_lease: AtomicU64,
    active_lease: AtomicU64,
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

struct AgentBackend {
    client: Arc<AgentClient>,
    lease: u64,
    layout: DesktopLayout,
}

struct NativeAgentRuntime {
    backend: Arc<dyn InputBackend>,
    task: tokio::task::JoinHandle<()>,
}

struct AgentTransport {
    requests: mpsc::Receiver<GuiToAgentPacket>,
    output: AgentOutput,
    alive: Arc<AtomicBool>,
    command_thread: std::thread::JoinHandle<Result<()>>,
    event_thread: std::thread::JoinHandle<Result<()>>,
}

struct AgentHeartbeat {
    last_seen: Instant,
}

impl AgentHeartbeat {
    fn new(now: Instant) -> Self {
        Self { last_seen: now }
    }

    fn observe(&mut self, now: Instant) {
        self.last_seen = now;
    }

    fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.last_seen) > AGENT_HEARTBEAT_TIMEOUT
    }
}

struct PipeSecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

impl PipeSecurity {
    fn for_current_user() -> Result<Self> {
        let user_sid = current_user_sid_string()?;
        let sddl = wide(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{user_sid})"));
        let mut descriptor = std::ptr::null_mut();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error())
                .context("failed to build Windows input agent pipe DACL");
        }
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
        })
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

impl NativeAgentRuntime {
    async fn stop(self) {
        let _ = self.backend.set_capture(false);
        let _ = self.backend.release_all();
        self.task.abort();
        let _ = self.task.await;
    }
}

impl AgentOutput {
    fn send_reliable(&self, packet: AgentToGuiPacket) -> Result<()> {
        self.reliable
            .send(packet)
            .map_err(|_| anyhow!("Windows input agent event queue closed"))
    }

    fn store_motion(&self, motion: AgentMotion) {
        if let Ok(mut latest) = self.motion.latest.lock() {
            match latest.as_mut() {
                Some(current) => {
                    current.dx = current.dx.saturating_add(motion.dx);
                    current.dy = current.dy.saturating_add(motion.dy);
                    if motion.position_updated {
                        current.position = motion.position;
                        current.position_updated = true;
                    }
                }
                None => *latest = Some(motion),
            }
            self.motion.changed.store(true, Ordering::Release);
        }
    }
}

impl AgentMotionSlot {
    fn take(&self) -> Option<AgentMotion> {
        if !self.changed.swap(false, Ordering::AcqRel) {
            return None;
        }
        self.latest.lock().ok().and_then(|mut latest| latest.take())
    }
}

pub fn request_elevation() -> Result<()> {
    if let Some(client) = current_client()
        && client.request(AgentRequest::Health).is_ok()
    {
        return Ok(());
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
    validate_binary_signature(&executable)?;
    launch_elevated(
        &executable,
        &command_pipe_name,
        &event_pipe_name,
        &token,
        parent_pid,
    )?;

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
    client.request(AgentRequest::Health)?;
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

fn start_agent_transport(
    command_pipe_name: String,
    event_pipe_name: String,
    token: String,
    parent_pid: u32,
) -> Result<AgentTransport> {
    let (request_tx, request_rx) = mpsc::channel(256);
    let (reliable_tx, reliable_rx) = std::sync::mpsc::sync_channel(256);
    let motion = Arc::new(AgentMotionSlot {
        latest: Mutex::new(None),
        changed: AtomicBool::new(false),
    });
    let output = AgentOutput {
        reliable: reliable_tx,
        motion: Arc::clone(&motion),
    };
    let alive = Arc::new(AtomicBool::new(true));
    let (startup_tx, startup_rx) = std::sync::mpsc::sync_channel(1);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

    let command_alive = Arc::clone(&alive);
    let command_startup = startup_tx.clone();
    let command_thread = std::thread::Builder::new()
        .name("synly-agent-command".to_string())
        .spawn(move || {
            let result = agent_command_owner(
                command_pipe_name,
                parent_pid,
                request_tx,
                startup_tx,
                Arc::clone(&command_alive),
            );
            if let Err(error) = &result {
                command_alive.store(false, Ordering::Release);
                let _ = command_startup.send(Err(format!("{error:#}")));
            }
            result
        })
        .context("failed to start Windows input agent command reader")?;

    let event_alive = Arc::clone(&alive);
    let event_thread = std::thread::Builder::new()
        .name("synly-agent-event".to_string())
        .spawn(move || {
            let result = agent_event_owner(
                event_pipe_name,
                token,
                parent_pid,
                reliable_rx,
                motion,
                startup_rx,
                ready_tx.clone(),
                Arc::clone(&event_alive),
            );
            if let Err(error) = &result {
                event_alive.store(false, Ordering::Release);
                let _ = ready_tx.send(Err(format!("{error:#}")));
            }
            result
        })
        .context("failed to start Windows input agent event writer")?;

    match ready_rx.recv_timeout(CONNECT_TIMEOUT) {
        Ok(Ok(())) => Ok(AgentTransport {
            requests: request_rx,
            output,
            alive,
            command_thread,
            event_thread,
        }),
        Ok(Err(error)) => {
            alive.store(false, Ordering::Release);
            Err(anyhow!(error))
        }
        Err(error) => {
            alive.store(false, Ordering::Release);
            Err(error).context("Windows input agent transport startup timed out")
        }
    }
}

fn agent_command_owner(
    pipe_name: String,
    parent_pid: u32,
    requests: mpsc::Sender<GuiToAgentPacket>,
    startup: std::sync::mpsc::SyncSender<Result<(), String>>,
    alive: Arc<AtomicBool>,
) -> Result<()> {
    let mut pipe = NativePipe::connect_client(
        &pipe_name,
        PipeDirection::ServerToClient,
        CONNECT_TIMEOUT,
    )?;
    validate_pipe_server(&pipe, parent_pid)?;
    let ack: GuiToAgentPacket = read_packet(&mut pipe, CONNECT_TIMEOUT)?;
    let startup_result = (|| -> Result<()> {
        let GuiToAgentPacket::HelloAck {
            version,
            session_id,
        } = ack
        else {
            bail!("Windows input agent received an invalid handshake response");
        };
        if version != IPC_VERSION {
            bail!(
                "Windows input agent handshake version mismatch: agent={}, GUI={version}",
                IPC_VERSION
            );
        }
        let expected_session_id = process_session_id(parent_pid)?;
        if session_id != expected_session_id {
            bail!(
                "Windows input agent handshake session mismatch: agent={expected_session_id}, GUI={session_id}"
            );
        }
        validate_parent_process(parent_pid)?;
        Ok(())
    })();
    startup
        .send(startup_result.as_ref().map(|_| ()).map_err(|error| format!("{error:#}")))
        .map_err(|_| anyhow!("Windows input agent event owner stopped during startup"))?;
    startup_result?;

    while alive.load(Ordering::Acquire) {
        let packet = match read_packet::<GuiToAgentPacket>(&mut pipe, Duration::from_secs(1)) {
            Ok(packet) => packet,
            Err(error) if is_timeout_error(&error) => continue,
            Err(_error) if !alive.load(Ordering::Acquire) => return Ok(()),
            Err(error) => return Err(error),
        };
        let GuiToAgentPacket::Request { .. } = packet else {
            bail!("Windows input agent received an unexpected packet");
        };
        requests
            .blocking_send(packet)
            .map_err(|_| anyhow!("Windows input agent backend request queue closed"))?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn agent_event_owner(
    pipe_name: String,
    token: String,
    parent_pid: u32,
    reliable: std::sync::mpsc::Receiver<AgentToGuiPacket>,
    motion: Arc<AgentMotionSlot>,
    startup: std::sync::mpsc::Receiver<Result<(), String>>,
    ready: std::sync::mpsc::SyncSender<Result<(), String>>,
    alive: Arc<AtomicBool>,
) -> Result<()> {
    let mut pipe = NativePipe::connect_client(
        &pipe_name,
        PipeDirection::ClientToServer,
        CONNECT_TIMEOUT,
    )?;
    validate_pipe_server(&pipe, parent_pid)?;
    let agent_path = std::env::current_exe().context("failed to locate input agent executable")?;
    write_packet(
        &mut pipe,
        &AgentToGuiPacket::Hello {
            version: IPC_VERSION,
            token,
            agent_pid: unsafe { GetCurrentProcessId() },
            parent_pid,
            agent_path,
        },
        REQUEST_DELIVERY_TIMEOUT,
    )?;
    match startup.recv_timeout(CONNECT_TIMEOUT) {
        Ok(Ok(())) => {
            write_packet(
                &mut pipe,
                &AgentToGuiPacket::Ready,
                REQUEST_DELIVERY_TIMEOUT,
            )?;
        }
        Ok(Err(error)) => {
            let _ = write_packet(
                &mut pipe,
                &AgentToGuiPacket::StartupError {
                    error: error.clone(),
                },
                REQUEST_DELIVERY_TIMEOUT,
            );
            return Err(anyhow!(error));
        }
        Err(error) => return Err(error).context("Windows input agent startup result timed out"),
    }
    ready
        .send(Ok(()))
        .map_err(|_| anyhow!("Windows input agent startup receiver closed"))?;
    agent_event_writer_loop(pipe, reliable, motion, alive)
}

pub async fn run_agent(
    command_pipe_name: String,
    event_pipe_name: String,
    token: String,
    parent_pid: u32,
) -> Result<()> {
    let transport = start_agent_transport(
        command_pipe_name,
        event_pipe_name,
        token,
        parent_pid,
    )?;
    let result = run_agent_loop(
        transport.requests,
        transport.output.clone(),
        Arc::clone(&transport.alive),
    )
    .await;
    transport.alive.store(false, Ordering::Release);
    drop(transport.output);
    let command_result = transport
        .command_thread
        .join()
        .map_err(|_| anyhow!("Windows input agent command owner panicked"))?;
    let event_result = transport
        .event_thread
        .join()
        .map_err(|_| anyhow!("Windows input agent event owner panicked"))?;
    result.and(command_result).and(event_result)
}

impl AgentClient {
    fn request(&self, request: AgentRequest) -> Result<AgentResponse> {
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
            &self.alive,
            request_name,
            dispatch_rx,
            response_rx,
            DISPATCH_TIMEOUT,
        )
    }

    fn notify(&self, request: AgentRequest) -> Result<()> {
        if !self.alive.load(Ordering::Acquire) {
            bail!("Windows input agent connection is closed");
        }
        let request = match request {
            AgentRequest::InjectCursor(point) => {
                if let Ok(mut latest) = self.cursor.latest.lock() {
                    *latest = Some(CursorUpdate {
                        point,
                    });
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

fn wait_for_agent_response(
    alive: &AtomicBool,
    request_name: &str,
    dispatch_rx: std::sync::mpsc::Receiver<()>,
    response_rx: std::sync::mpsc::Receiver<Result<AgentResponse, String>>,
    dispatch_timeout: Duration,
) -> Result<AgentResponse> {
    match dispatch_rx.recv_timeout(dispatch_timeout) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            mark_agent_unavailable(alive, request_name, "dispatch-timeout");
            bail!("Windows input agent {request_name} dispatch timed out");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            mark_agent_unavailable(alive, request_name, "dispatch-disconnected");
            bail!("Windows input agent {request_name} dispatch failed before pipe write");
        }
    }
    match response_rx.recv() {
        Ok(Ok(AgentResponse::Error(message))) => Err(anyhow!(message)),
        Ok(Ok(response)) => Ok(response),
        Ok(Err(message)) => Err(anyhow!(message)),
        Err(error) => {
            mark_agent_unavailable(alive, request_name, "response-disconnected");
            Err(error).with_context(|| format!("Windows input agent {request_name} response channel closed"))
        }
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
        &GuiToAgentPacket::HelloAck {
            version: IPC_VERSION,
            session_id,
        },
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
        version,
        token: incoming_token,
        agent_pid,
        parent_pid: incoming_parent,
        agent_path,
    } = hello
    else {
        bail!("Windows input agent sent an invalid handshake");
    };
    if version != IPC_VERSION {
        bail!(
            "Windows input agent handshake version mismatch: agent={version}, GUI={}",
            IPC_VERSION
        );
    }
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

fn client_event_reader_loop(
    mut pipe: NativePipe,
    pending: PendingResponses,
    context: Arc<Mutex<Option<CaptureContext>>>,
    alive: Arc<AtomicBool>,
) -> Result<()> {
    while alive.load(Ordering::Acquire) {
        let packet = match read_packet::<AgentToGuiPacket>(&mut pipe, AGENT_HEARTBEAT_TIMEOUT) {
            Ok(packet) => packet,
            Err(_) if !alive.load(Ordering::Acquire) => return Ok(()),
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
                if pending_response.request == "InjectCursor" {
                    if let AgentResponse::Error(message) = &response
                        && let Ok(context) = context.lock()
                        && let Some(context) = context.as_ref()
                    {
                        context.emit_reliable(NativeEvent::Failed(format!(
                            "Windows input agent cursor injection failed: {message}"
                        )));
                    }
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

fn agent_completion_packet(id: u64, response: Result<AgentResponse>) -> AgentToGuiPacket {
    AgentToGuiPacket::Response {
        id,
        response: response
            .unwrap_or_else(|error| AgentResponse::Error(format!("{error:#}"))),
    }
}

fn command_writer_loop(
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
                    flush_latest_cursor(
                        &mut pipe,
                        &cursor,
                        &pending,
                        &alive,
                        &mut next_id,
                    )?;
                }
                write_client_command(
                    &mut pipe,
                    command,
                    &pending,
                    &alive,
                    &mut next_id,
                )?;
                next_heartbeat = Instant::now() + CLIENT_HEARTBEAT_INTERVAL;
            }
            Ok(ClientQueueItem::Cursor) => {
                flush_latest_cursor(
                    &mut pipe,
                    &cursor,
                    &pending,
                    &alive,
                    &mut next_id,
                )?;
                next_heartbeat = Instant::now() + CLIENT_HEARTBEAT_INTERVAL;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                flush_latest_cursor(
                    &mut pipe,
                    &cursor,
                    &pending,
                    &alive,
                    &mut next_id,
                )?;
                if Instant::now() >= next_heartbeat {
                    write_client_command(
                        &mut pipe,
                        ClientCommand {
                            request: AgentRequest::Health,
                            dispatched: None,
                            response: None,
                        },
                        &pending,
                        &alive,
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
    alive: &Arc<AtomicBool>,
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
        write_client_command(
            pipe,
            ClientCommand {
                request: AgentRequest::InjectCursor(update.point),
                dispatched: None,
                response: None,
            },
            pending,
            alive,
            next_id,
        )?;
    }
    Ok(())
}

fn write_client_command(
    pipe: &mut NativePipe,
    command: ClientCommand,
    pending: &PendingResponses,
    alive: &Arc<AtomicBool>,
    next_id: &mut u64,
) -> Result<()> {
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
        &GuiToAgentPacket::Request {
            id,
            request,
        },
        REQUEST_DELIVERY_TIMEOUT,
    )
    {
        complete_pending_with_error(pending, id, format!("{error:#}"));
        return Err(error);
    }
    if let Some(dispatched) = dispatched {
        let _ = dispatched.send(());
    }
    match completion_rx.recv_timeout(REQUEST_DELIVERY_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => Err(anyhow!(message)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let message = format!("Windows input agent {request_name} response timed out");
            complete_pending_with_error(pending, id, message.clone());
            mark_agent_unavailable(alive, request_name, "response-timeout");
            Err(anyhow!(message))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let message = format!("Windows input agent {request_name} response channel closed");
            complete_pending_with_error(pending, id, message.clone());
            mark_agent_unavailable(alive, request_name, "response-disconnected");
            Err(anyhow!(message))
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

async fn run_agent_loop(
    mut packets: mpsc::Receiver<GuiToAgentPacket>,
    output: AgentOutput,
    alive: Arc<AtomicBool>,
) -> Result<()> {
    let mut runtime = None;
    let mut heartbeat = AgentHeartbeat::new(Instant::now());
    let mut paused = false;
    let mut desktop_tick = time::interval(Duration::from_millis(250));
    desktop_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let result: Result<()> = async {
        loop {
            tokio::select! {
                packet = packets.recv() => {
                    let packet = packet.context("Windows input agent request reader stopped")?;
                    let GuiToAgentPacket::Request { id, request } = packet else {
                        bail!("Windows input agent received an unexpected packet");
                    };
                    heartbeat.observe(Instant::now());
                    let response = handle_agent_request(request, &mut runtime, output.clone()).await;
                    let completion = agent_completion_packet(id, response);
                    output.send_reliable(completion)?;
                    heartbeat.observe(Instant::now());
                }
                _ = desktop_tick.tick() => {
                    if heartbeat.expired(Instant::now()) {
                        let message = "Windows input agent GUI heartbeat timed out";
                        break Err(anyhow!(message));
                    }
                    let current_paused = !is_default_input_desktop();
                    if current_paused != paused {
                        paused = current_paused;
                        if paused
                            && let Some(runtime) = runtime.as_ref()
                        {
                            let _ = runtime.backend.set_capture(false);
                            let _ = runtime.backend.release_all();
                        }
                        output.send_reliable(AgentToGuiPacket::SecureDesktopPaused(paused))?;
                    }
                }
            }
        }
    }
    .await;
    alive.store(false, Ordering::Release);
    if let Some(runtime) = runtime {
        runtime.stop().await;
    }
    result
}

fn agent_event_writer_loop(
    mut pipe: NativePipe,
    reliable: std::sync::mpsc::Receiver<AgentToGuiPacket>,
    motion: Arc<AgentMotionSlot>,
    alive: Arc<AtomicBool>,
) -> Result<()> {
    while alive.load(Ordering::Acquire) {
        match reliable.recv_timeout(Duration::from_millis(20)) {
            Ok(packet) => {
                write_agent_event_packet(&mut pipe, packet)?;
                while let Ok(packet) = reliable.try_recv() {
                    write_agent_event_packet(&mut pipe, packet)?;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(motion) = motion.take() {
                    write_motion_packet(&mut pipe, motion)?;
                }
                break;
            }
        }
        if let Some(motion) = motion.take() {
            write_motion_packet(&mut pipe, motion)?;
        }
    }
    Ok(())
}

fn write_motion_packet(pipe: &mut NativePipe, motion: AgentMotion) -> Result<()> {
    write_packet(
        pipe,
        &AgentToGuiPacket::Motion {
            dx: motion.dx,
            dy: motion.dy,
            position: motion.position,
            position_updated: motion.position_updated,
        },
        REQUEST_DELIVERY_TIMEOUT,
    )?;
    Ok(())
}

fn write_agent_event_packet(pipe: &mut NativePipe, packet: AgentToGuiPacket) -> Result<()> {
    write_packet(pipe, &packet, REQUEST_DELIVERY_TIMEOUT)
}

async fn handle_agent_request(
    request: AgentRequest,
    runtime: &mut Option<NativeAgentRuntime>,
    outgoing: AgentOutput,
) -> Result<AgentResponse> {
    match request {
        AgentRequest::Start { mode, hotkey } => {
            if let Some(previous) = runtime.take() {
                previous.stop().await;
            }
            *runtime = Some(start_native_runtime(mode, hotkey, outgoing)?);
            let layout = agent_backend(runtime)?.layout()?;
            Ok(AgentResponse::Started { layout })
        }
        AgentRequest::Stop => {
            if let Some(previous) = runtime.take() {
                previous.stop().await;
            }
            Ok(AgentResponse::Ok)
        }
        AgentRequest::Health => Ok(AgentResponse::Pong),
        AgentRequest::CursorPosition => {
            Ok(AgentResponse::Point(agent_backend(runtime)?.cursor_position()?))
        }
        AgentRequest::Snapshot => Ok(AgentResponse::Snapshot(agent_backend(runtime)?.snapshot())),
        AgentRequest::SetCapture(active) => {
            agent_backend(runtime)?.set_capture(active)?;
            Ok(AgentResponse::Ok)
        }
        AgentRequest::WarpCursor(point) => {
            agent_backend(runtime)?.warp_cursor(point)?;
            Ok(AgentResponse::Ok)
        }
        AgentRequest::InjectKey {
            usage,
            modifiers,
            down,
            repeat,
        } => {
            agent_backend(runtime)?.inject_key(usage, modifiers, down, repeat)?;
            Ok(AgentResponse::Ok)
        }
        AgentRequest::InjectButton { button, down } => {
            agent_backend(runtime)?.inject_button(button, down)?;
            Ok(AgentResponse::Ok)
        }
        AgentRequest::InjectCursor(point) => {
            agent_backend(runtime)?.inject_cursor(point)?;
            Ok(AgentResponse::Ok)
        }
        AgentRequest::InjectWheel { x, y } => {
            agent_backend(runtime)?.inject_wheel(x, y)?;
            Ok(AgentResponse::Ok)
        }
        AgentRequest::ReleaseAll => {
            agent_backend(runtime)?.release_all()?;
            Ok(AgentResponse::Ok)
        }
    }
}

fn start_native_runtime(
    mode: InputMode,
    hotkey: Hotkey,
    outgoing: AgentOutput,
) -> Result<NativeAgentRuntime> {
    let send_motion = mode == InputMode::Send;
    let (events_tx, mut events_rx) = mpsc::channel(256);
    let motion = Arc::new(MotionAccumulator::default());
    let context = CaptureContext {
        mode,
        hotkey,
        events: events_tx,
        motion: Arc::clone(&motion),
        capture_active: Arc::new(AtomicBool::new(false)),
        overflowed: Arc::new(AtomicBool::new(false)),
        failed: Arc::new(AtomicBool::new(false)),
    };
    let backend = super::platform::windows::start_native(context)?;
    let task = tokio::spawn(async move {
        let mut motion_tick = time::interval(Duration::from_millis(8));
        motion_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                event = events_rx.recv() => {
                    let Some(event) = event else { break };
                    if outgoing.send_reliable(AgentToGuiPacket::Event(event)).is_err() {
                        break;
                    }
                }
                _ = motion_tick.tick(), if send_motion => {
                    let sample = motion.take_observed();
                    if sample.dx != 0 || sample.dy != 0 || sample.position_updated {
                        outgoing.store_motion(AgentMotion {
                                dx: sample.dx,
                                dy: sample.dy,
                                position: sample.position,
                                position_updated: sample.position_updated,
                            });
                    }
                }
            }
        }
    });
    Ok(NativeAgentRuntime { backend, task })
}

fn agent_backend(runtime: &Option<NativeAgentRuntime>) -> Result<&Arc<dyn InputBackend>> {
    runtime
        .as_ref()
        .map(|runtime| &runtime.backend)
        .context("Windows input agent runtime is not started")
}

fn current_client() -> Option<Arc<AgentClient>> {
    AGENT
        .get()
        .and_then(|slot| slot.lock().ok())
        .and_then(|slot| slot.as_ref().cloned())
        .filter(|client| client.alive.load(Ordering::Acquire))
}

fn agent_executable() -> Result<PathBuf> {
    let current = std::env::current_exe().context("failed to locate Synly executable")?;
    if !current.is_file() {
        bail!("Windows input agent host is missing: {}", current.display());
    }
    Ok(current)
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

fn validate_parent_process(parent_pid: u32) -> Result<()> {
    if process_session_id(parent_pid)? != process_session_id(unsafe { GetCurrentProcessId() })? {
        bail!("Windows input agent and GUI are not in the same session");
    }
    let parent_path = process_image_path(parent_pid)?;
    let agent_path = std::env::current_exe()?;
    validate_install_directory(&parent_path, &agent_path)?;
    validate_binary_signature(&parent_path)?;
    validate_binary_signature(&agent_path)
}

fn validate_pipe_server(
    client: &NativePipe,
    expected_pid: u32,
) -> Result<()> {
    let mut actual_pid = 0u32;
    let ok = unsafe { GetNamedPipeServerProcessId(client.raw_handle(), &mut actual_pid) };
    if ok == 0 || actual_pid != expected_pid {
        bail!("Windows input agent named pipe server PID validation failed");
    }
    Ok(())
}

fn validate_pipe_client(
    server: &NativePipe,
    expected_pid: u32,
    reported_path: &Path,
) -> Result<()> {
    let mut actual_pid = 0u32;
    let ok = unsafe { GetNamedPipeClientProcessId(server.raw_handle(), &mut actual_pid) };
    if ok == 0 || actual_pid != expected_pid {
        bail!("Windows input agent named pipe client PID validation failed");
    }
    let actual_path = process_image_path(actual_pid)?;
    if normalize_path(&actual_path) != normalize_path(reported_path) {
        bail!("Windows input agent image path validation failed");
    }
    let gui_path = std::env::current_exe()?;
    validate_install_directory(&gui_path, &actual_path)?;
    if process_session_id(actual_pid)? != process_session_id(unsafe { GetCurrentProcessId() })? {
        bail!("Windows input agent session validation failed");
    }
    Ok(())
}

fn validate_install_directory(left: &Path, right: &Path) -> Result<()> {
    if normalize_path(left.parent().unwrap_or(left)) != normalize_path(right.parent().unwrap_or(right)) {
        bail!("Windows input agent and GUI are not installed in the same directory");
    }
    Ok(())
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn validate_binary_signature(path: &Path) -> Result<()> {
    match verify_authenticode(path) {
        Ok(()) => Ok(()),
        Err(error) if cfg!(debug_assertions) => {
            tracing::warn!(path = %path.display(), error = %error, "debug 构建允许未签名的 Windows 输入组件");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn verify_authenticode(path: &Path) -> Result<()> {
    let path = wide(&path.to_string_lossy());
    let mut file = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: path.as_ptr(),
        hFile: std::ptr::null_mut(),
        pgKnownSubject: std::ptr::null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        pPolicyCallbackData: std::ptr::null_mut(),
        pSIPClientData: std::ptr::null_mut(),
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: windows_sys::Win32::Security::WinTrust::WINTRUST_DATA_0 {
            pFile: &mut file,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        hWVTStateData: std::ptr::null_mut(),
        pwszURLReference: std::ptr::null_mut(),
        dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL,
        dwUIContext: 0,
        pSignatureSettings: std::ptr::null_mut(),
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe {
        WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            std::ptr::null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
    }
    if status != 0 {
        bail!("Windows Authenticode validation failed with status 0x{status:08X}");
    }
    Ok(())
}

fn process_session_id(process_id: u32) -> Result<u32> {
    let mut session_id = 0u32;
    if unsafe { ProcessIdToSessionId(process_id, &mut session_id) } == 0 {
        bail!("failed to resolve Windows process session ID");
    }
    Ok(session_id)
}

fn process_image_path(process_id: u32) -> Result<PathBuf> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        bail!("failed to open Windows process {process_id} for image validation");
    }
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(process, 0, buffer.as_mut_ptr(), &mut length)
    };
    unsafe {
        CloseHandle(process);
    }
    if ok == 0 {
        bail!("failed to query Windows process image path");
    }
    Ok(PathBuf::from(String::from_utf16_lossy(
        &buffer[..length as usize],
    )))
}

fn current_user_sid_string() -> Result<String> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to open current Windows process token");
    }

    let mut required = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut required,
        );
    }
    if required == 0 {
        unsafe {
            CloseHandle(token);
        }
        return Err(std::io::Error::last_os_error())
            .context("failed to size current Windows user SID");
    }

    let word_count = (required as usize).div_ceil(std::mem::size_of::<usize>());
    let mut buffer = vec![0usize; word_count];
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    };
    unsafe {
        CloseHandle(token);
    }
    if ok == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to read current Windows user SID");
    }

    let token_user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
    let mut sid_text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_text) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("failed to format current Windows user SID");
    }
    let mut length = 0usize;
    unsafe {
        while *sid_text.add(length) != 0 {
            length += 1;
        }
    }
    let sid = unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, length)) };
    unsafe {
        LocalFree(sid_text.cast());
    }
    Ok(sid)
}

fn is_default_input_desktop() -> bool {
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
    };

    let desktop = unsafe { OpenInputDesktop(0, 0, DESKTOP_READOBJECTS) };
    if desktop.is_null() {
        return false;
    }
    let mut needed = 0u32;
    unsafe {
        GetUserObjectInformationW(desktop, UOI_NAME, std::ptr::null_mut(), 0, &mut needed);
    }
    let mut buffer = vec![0u16; (needed as usize / 2).max(1)];
    let ok = unsafe {
        GetUserObjectInformationW(
            desktop,
            UOI_NAME,
            buffer.as_mut_ptr().cast::<c_void>(),
            needed,
            &mut needed,
        )
    };
    unsafe {
        CloseDesktop(desktop);
    }
    if ok == 0 {
        return false;
    }
    let end = buffer.iter().position(|value| *value == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end]).eq_ignore_ascii_case("Default")
}

impl AgentToGuiPacket {
    fn packet_name(&self) -> &'static str {
        match self {
            Self::Hello { .. } => "Hello",
            Self::Ready => "Ready",
            Self::StartupError { .. } => "StartupError",
            Self::Response { .. } => "Response",
            Self::Event(_) => "Event",
            Self::Motion { .. } => "Motion",
            Self::SecureDesktopPaused(_) => "SecureDesktopPaused",
        }
    }
}

fn write_packet<P>(pipe: &mut NativePipe, packet: &P, timeout: Duration) -> Result<()>
where
    P: Serialize,
{
    let bytes = bincode::serialize(packet).context("failed to encode Windows input agent packet")?;
    if bytes.is_empty() || bytes.len() > IPC_MAX_FRAME {
        bail!("Windows input agent packet length is invalid");
    }
    let mut frame = Vec::with_capacity(4 + bytes.len());
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&bytes);
    pipe.write_all(&frame, timeout).map_err(Into::into)
}

fn read_packet<P>(pipe: &mut NativePipe, timeout: Duration) -> Result<P>
where
    P: DeserializeOwned,
{
    let started = Instant::now();
    let mut length_bytes = [0u8; 4];
    pipe.read_exact(&mut length_bytes, timeout)?;
    let length = u32::from_be_bytes(length_bytes) as usize;
    if length == 0 || length > IPC_MAX_FRAME {
        bail!("Windows input agent packet length is invalid: {length}");
    }
    let mut bytes = vec![0u8; length];
    let remaining = timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        bail!("Windows input agent packet read timed out");
    }
    pipe.read_exact(&mut bytes, remaining)?;
    bincode::deserialize(&bytes)
        .context("failed to decode Windows input agent packet")
}

fn is_timeout_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
            || cause.to_string().contains("timed out")
    })
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::DisplayRect;

    fn test_layout() -> DesktopLayout {
        DesktopLayout::new(vec![DisplayRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }])
        .unwrap()
    }

    fn test_cursor() -> Arc<ClientCursorState> {
        Arc::new(ClientCursorState {
            latest: Mutex::new(None),
            queued: AtomicBool::new(false),
        })
    }

    fn test_client(
        commands: std::sync::mpsc::SyncSender<ClientQueueItem>,
        alive: Arc<AtomicBool>,
    ) -> AgentClient {
        AgentClient {
            commands,
            cursor: test_cursor(),
            context: Arc::new(Mutex::new(None)),
            alive,
            lifecycle: Mutex::new(()),
            next_lease: AtomicU64::new(1),
            active_lease: AtomicU64::new(0),
        }
    }

    fn create_test_server(name: &str, direction: PipeDirection) -> NativePipe {
        let security = PipeSecurity::for_current_user().unwrap();
        NativePipe::create_server(name, direction, &security.attributes).unwrap()
    }

    #[test]
    fn closed_command_queue_marks_agent_unavailable() {
        let (commands, receiver) = std::sync::mpsc::sync_channel(1);
        drop(receiver);
        let alive = Arc::new(AtomicBool::new(true));
        let client = test_client(commands, Arc::clone(&alive));

        assert!(client.request(AgentRequest::Health).is_err());
        assert!(!alive.load(Ordering::Acquire));
    }

    #[test]
    fn stale_backend_drop_does_not_stop_current_lease() {
        let (commands, receiver) = std::sync::mpsc::sync_channel(4);
        let client = Arc::new(test_client(commands, Arc::new(AtomicBool::new(true))));
        client.next_lease.store(3, Ordering::Release);
        client.active_lease.store(2, Ordering::Release);
        drop(AgentBackend {
            client: Arc::clone(&client),
            lease: 1,
            layout: test_layout(),
        });

        assert_eq!(client.active_lease.load(Ordering::Acquire), 2);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn current_backend_drop_stops_runtime_without_closing_reusable_agent() {
        let (commands, receiver) = std::sync::mpsc::sync_channel(4);
        let client = Arc::new(test_client(commands, Arc::new(AtomicBool::new(true))));
        client.next_lease.store(2, Ordering::Release);
        client.active_lease.store(1, Ordering::Release);
        drop(AgentBackend {
            client: Arc::clone(&client),
            lease: 1,
            layout: test_layout(),
        });

        let item = receiver.try_recv().unwrap();
        let ClientQueueItem::Command(command) = item else {
            panic!("expected Stop command");
        };
        assert!(matches!(command.request, AgentRequest::Stop));
        assert!(command.response.is_none());
        assert_eq!(client.active_lease.load(Ordering::Acquire), 0);
        assert!(client.alive.load(Ordering::Acquire));
    }

    #[test]
    fn cursor_notifications_keep_only_the_latest_value() {
        let (commands, receiver) = std::sync::mpsc::sync_channel(1);
        let client = test_client(commands, Arc::new(AtomicBool::new(true)));

        for x in 0..20_000 {
            client
                .notify(AgentRequest::InjectCursor(Point { x, y: 10 }))
                .unwrap();
        }

        assert!(matches!(receiver.try_recv().unwrap(), ClientQueueItem::Cursor));
        assert!(receiver.try_recv().is_err());
        let latest = client.cursor.latest.lock().unwrap().unwrap();
        assert_eq!(latest.point, Point { x: 19_999, y: 10 });
    }

    #[test]
    fn native_dual_pipe_transport_survives_cursor_lifecycle_and_event_pressure() {
        const CYCLES: usize = 1000;
        const EXPECTED_REQUESTS: usize = 1 + CYCLES * 2 + 2;

        let connection_id = Uuid::new_v4();
        let command_name = format!(r"\\.\pipe\synly-test-command-{connection_id}");
        let event_name = format!(r"\\.\pipe\synly-test-event-{connection_id}");
        let alive = Arc::new(AtomicBool::new(true));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let context = Arc::new(Mutex::new(None));
        let cursor = test_cursor();
        let (commands, command_rx) = std::sync::mpsc::sync_channel(64);
        let (created_tx, created_rx) = std::sync::mpsc::sync_channel(2);

        let command_pending = Arc::clone(&pending);
        let command_alive = Arc::clone(&alive);
        let command_cursor = Arc::clone(&cursor);
        let command_created = created_tx.clone();
        let command_server_name = command_name.clone();
        let gui_command = std::thread::spawn(move || -> Result<()> {
            let mut pipe = create_test_server(
                &command_server_name,
                PipeDirection::ServerToClient,
            );
            command_created.send(()).unwrap();
            pipe.connect_server(CONNECT_TIMEOUT)?;
            command_writer_loop(
                pipe,
                command_rx,
                command_cursor,
                command_pending,
                command_alive,
            )
        });

        let event_pending = Arc::clone(&pending);
        let event_alive = Arc::clone(&alive);
        let event_context = Arc::clone(&context);
        let event_created = created_tx;
        let event_server_name = event_name.clone();
        let gui_event = std::thread::spawn(move || -> Result<()> {
            let mut pipe = create_test_server(
                &event_server_name,
                PipeDirection::ClientToServer,
            );
            event_created.send(()).unwrap();
            pipe.connect_server(CONNECT_TIMEOUT)?;
            client_event_reader_loop(pipe, event_pending, event_context, event_alive)
        });

        created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();
        created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();

        let (reliable_tx, reliable_rx) = std::sync::mpsc::sync_channel(256);
        let motion = Arc::new(AgentMotionSlot {
            latest: Mutex::new(None),
            changed: AtomicBool::new(false),
        });
        let output = AgentOutput {
            reliable: reliable_tx,
            motion: Arc::clone(&motion),
        };
        let agent_event_alive = Arc::clone(&alive);
        let agent_event_name = event_name;
        let agent_event = std::thread::spawn(move || -> Result<()> {
            let pipe = NativePipe::connect_client(
                &agent_event_name,
                PipeDirection::ClientToServer,
                CONNECT_TIMEOUT,
            )?;
            agent_event_writer_loop(pipe, reliable_rx, motion, agent_event_alive)
        });

        let agent_output = output.clone();
        let agent_command_name = command_name;
        let agent_command = std::thread::spawn(move || -> Result<Vec<u64>> {
            let mut pipe = NativePipe::connect_client(
                &agent_command_name,
                PipeDirection::ServerToClient,
                CONNECT_TIMEOUT,
            )?;
            let mut ids = Vec::with_capacity(EXPECTED_REQUESTS);
            for expected_id in 1..=u64::try_from(EXPECTED_REQUESTS).unwrap() {
                let packet = read_packet::<GuiToAgentPacket>(
                    &mut pipe,
                    REQUEST_DELIVERY_TIMEOUT,
                )?;
                let GuiToAgentPacket::Request { id, request } = packet else {
                    bail!("expected request packet");
                };
                assert_eq!(id, expected_id);
                if expected_id == 1 {
                    assert!(matches!(
                        request,
                        AgentRequest::InjectCursor(Point { x: 19_999, y: 720 })
                    ));
                } else if expected_id == u64::try_from(EXPECTED_REQUESTS - 1).unwrap() {
                    assert!(matches!(
                        request,
                        AgentRequest::InjectButton {
                            button: 1,
                            down: true,
                        }
                    ));
                } else if expected_id == u64::try_from(EXPECTED_REQUESTS).unwrap() {
                    assert!(matches!(
                        request,
                        AgentRequest::InjectWheel { x: 0, y: 120 }
                    ));
                }
                ids.push(id);
                agent_output.send_reliable(AgentToGuiPacket::Event(
                    NativeEvent::Button {
                        button: 1,
                        down: false,
                    },
                ))?;
                agent_output.store_motion(AgentMotion {
                    dx: 1,
                    dy: -1,
                    position: Some(Point { x: 10, y: 20 }),
                    position_updated: true,
                });
                agent_output.send_reliable(AgentToGuiPacket::Response {
                    id,
                    response: AgentResponse::Ok,
                })?;
            }
            Ok(ids)
        });

        let producer_commands = commands.clone();
        let producer_cursor = Arc::clone(&cursor);
        let producer = std::thread::spawn(move || {
            for x in 0..20_000 {
                *producer_cursor.latest.lock().unwrap() = Some(CursorUpdate {
                    point: Point { x, y: 720 },
                });
            }
            producer_cursor.queued.store(true, Ordering::Release);
            producer_commands.send(ClientQueueItem::Cursor).unwrap();
            for cycle in 0..CYCLES {
                producer_commands
                    .send(ClientQueueItem::Command(ClientCommand {
                        request: AgentRequest::ReleaseAll,
                        dispatched: None,
                        response: None,
                    }))
                    .unwrap();
                producer_commands
                    .send(ClientQueueItem::Command(ClientCommand {
                        request: AgentRequest::WarpCursor(Point {
                            x: 2551,
                            y: i32::try_from(cycle % 1440).unwrap(),
                        }),
                        dispatched: None,
                        response: None,
                    }))
                    .unwrap();
            }
            producer_commands
                .send(ClientQueueItem::Command(ClientCommand {
                    request: AgentRequest::InjectButton {
                        button: 1,
                        down: true,
                    },
                    dispatched: None,
                    response: None,
                }))
                .unwrap();
            let (dispatch_tx, dispatch_rx) = std::sync::mpsc::sync_channel(1);
            let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
            producer_commands
                .send(ClientQueueItem::Command(ClientCommand {
                    request: AgentRequest::InjectWheel { x: 0, y: 120 },
                    dispatched: Some(dispatch_tx),
                    response: Some(response_tx),
                }))
                .unwrap();
            (dispatch_rx, response_rx)
        });

        let (dispatch_rx, response_rx) = producer.join().unwrap();
        assert!(matches!(
            wait_for_agent_response(
                &alive,
                "InjectWheel",
                dispatch_rx,
                response_rx,
                Duration::from_secs(30),
            )
            .unwrap(),
            AgentResponse::Ok
        ));
        let ids = agent_command.join().unwrap().unwrap();
        assert_eq!(ids.len(), EXPECTED_REQUESTS);
        assert!(ids.windows(2).all(|pair| pair[1] == pair[0] + 1));

        alive.store(false, Ordering::Release);
        drop(commands);
        drop(output);
        gui_command.join().unwrap().unwrap();
        agent_event.join().unwrap().unwrap();
        gui_event.join().unwrap().unwrap();
    }

    #[test]
    fn native_pipe_rejects_invalid_frame_length() {
        let name = format!(r"\\.\pipe\synly-test-length-{}", Uuid::new_v4());
        let (created_tx, created_rx) = std::sync::mpsc::sync_channel(1);
        let server_name = name.clone();
        let server = std::thread::spawn(move || {
            let mut pipe = create_test_server(
                &server_name,
                PipeDirection::ClientToServer,
            );
            created_tx.send(()).unwrap();
            pipe.connect_server(CONNECT_TIMEOUT).unwrap();
            read_packet::<GuiToAgentPacket>(&mut pipe, Duration::from_secs(1))
        });
        created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();
        let mut client = NativePipe::connect_client(
            &name,
            PipeDirection::ClientToServer,
            CONNECT_TIMEOUT,
        )
        .unwrap();
        client
            .write_all(
                &(u32::try_from(IPC_MAX_FRAME).unwrap() + 1).to_be_bytes(),
                Duration::from_secs(1),
            )
            .unwrap();

        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn native_pipe_reports_half_frame_when_peer_exits() {
        let name = format!(r"\\.\pipe\synly-test-half-frame-{}", Uuid::new_v4());
        let (created_tx, created_rx) = std::sync::mpsc::sync_channel(1);
        let server_name = name.clone();
        let server = std::thread::spawn(move || {
            let mut pipe = create_test_server(
                &server_name,
                PipeDirection::ClientToServer,
            );
            created_tx.send(()).unwrap();
            pipe.connect_server(CONNECT_TIMEOUT).unwrap();
            read_packet::<GuiToAgentPacket>(&mut pipe, Duration::from_secs(1))
        });
        created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();
        let mut client = NativePipe::connect_client(
            &name,
            PipeDirection::ClientToServer,
            CONNECT_TIMEOUT,
        )
        .unwrap();
        client
            .write_all(&16u32.to_be_bytes(), Duration::from_secs(1))
            .unwrap();
        client
            .write_all(&[1, 2, 3], Duration::from_secs(1))
            .unwrap();
        drop(client);

        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn native_pipe_read_timeout_is_cancelled() {
        let name = format!(r"\\.\pipe\synly-test-timeout-{}", Uuid::new_v4());
        let (created_tx, created_rx) = std::sync::mpsc::sync_channel(1);
        let server_name = name.clone();
        let server = std::thread::spawn(move || {
            let mut pipe = create_test_server(
                &server_name,
                PipeDirection::ClientToServer,
            );
            created_tx.send(()).unwrap();
            pipe.connect_server(CONNECT_TIMEOUT).unwrap();
            let mut byte = [0u8; 1];
            pipe.read_exact(&mut byte, Duration::from_millis(50))
        });
        created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();
        let _client = NativePipe::connect_client(
            &name,
            PipeDirection::ClientToServer,
            CONNECT_TIMEOUT,
        )
        .unwrap();

        let error = server.join().unwrap().unwrap_err();
        assert!(is_timeout_error(&error));
    }

    #[test]
    fn reliable_request_timeout_marks_transport_unavailable() {
        let name = format!(r"\\.\pipe\synly-test-response-timeout-{}", Uuid::new_v4());
        let (created_tx, created_rx) = std::sync::mpsc::sync_channel(1);
        let alive = Arc::new(AtomicBool::new(true));
        let server_alive = Arc::clone(&alive);
        let server_name = name.clone();
        let server = std::thread::spawn(move || {
            let mut pipe = create_test_server(
                &server_name,
                PipeDirection::ServerToClient,
            );
            created_tx.send(()).unwrap();
            pipe.connect_server(CONNECT_TIMEOUT).unwrap();
            let pending = Arc::new(Mutex::new(HashMap::new()));
            let mut next_id = 1;
            write_client_command(
                &mut pipe,
                ClientCommand {
                    request: AgentRequest::Stop,
                    dispatched: None,
                    response: None,
                },
                &pending,
                &server_alive,
                &mut next_id,
            )
        });
        created_rx.recv_timeout(CONNECT_TIMEOUT).unwrap();
        let mut client = NativePipe::connect_client(
            &name,
            PipeDirection::ServerToClient,
            CONNECT_TIMEOUT,
        )
        .unwrap();
        let packet = read_packet::<GuiToAgentPacket>(
            &mut client,
            REQUEST_DELIVERY_TIMEOUT,
        )
        .unwrap();
        assert!(matches!(
            packet,
            GuiToAgentPacket::Request {
                id: 1,
                request: AgentRequest::Stop,
            }
        ));

        assert!(server.join().unwrap().is_err());
        assert!(!alive.load(Ordering::Acquire));
    }

    #[test]
    fn agent_heartbeat_only_expires_after_the_full_timeout() {
        let started = Instant::now();
        let heartbeat = AgentHeartbeat::new(started);

        assert!(!heartbeat.expired(started + AGENT_HEARTBEAT_TIMEOUT));
        assert!(heartbeat.expired(
            started + AGENT_HEARTBEAT_TIMEOUT + Duration::from_millis(1)
        ));
    }

    #[test]
    #[ignore = "requires interactive UAC approval and a real Windows desktop"]
    fn elevated_agent_process_is_reused_across_requests() {
        request_elevation().unwrap();
        let first = current_client().unwrap();
        request_elevation().unwrap();
        let second = current_client().unwrap();

        assert!(Arc::ptr_eq(&first, &second));
    }
}
