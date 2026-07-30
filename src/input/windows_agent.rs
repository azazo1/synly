use super::platform::{CaptureContext, InputBackend, MotionAccumulator, NativeEvent};
use super::{DesktopLayout, Hotkey, InputMode, KeySnapshot, ModifierMask, Point};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;
use tokio::time::{self, Instant, MissedTickBehavior};
use uuid::Uuid;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
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

const IPC_VERSION: u16 = 5;
const IPC_MAX_FRAME: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const DISPATCH_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const CLIENT_HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
const AGENT_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);

static AGENT: OnceLock<Mutex<Option<Arc<AgentClient>>>> = OnceLock::new();

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

    fn reports_diagnostic(&self) -> bool {
        matches!(
            self,
            Self::Start { .. }
                | Self::Stop
                | Self::SetCapture(_)
                | Self::WarpCursor(_)
                | Self::ReleaseAll
        )
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

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum AgentDiagnosticPhase {
    Started,
    Completed,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum AgentPacket {
    Hello {
        version: u16,
        token: String,
        agent_pid: u32,
        parent_pid: u32,
        agent_path: PathBuf,
    },
    HelloAck {
        version: u16,
        session_id: u32,
    },
    Ready,
    StartupError {
        error: String,
    },
    Request {
        id: u64,
        expects_response: bool,
        request: AgentRequest,
    },
    Response {
        id: u64,
        response: AgentResponse,
    },
    Diagnostic {
        request: String,
        phase: AgentDiagnosticPhase,
        elapsed_ms: u64,
        error: Option<String>,
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

struct ClientCommand {
    request: AgentRequest,
    queued_at: Instant,
    dispatched: Option<std::sync::mpsc::SyncSender<()>>,
    response: Option<std::sync::mpsc::SyncSender<Result<AgentResponse, String>>>,
}

type PendingResponses = Arc<Mutex<HashMap<
    u64,
    std::sync::mpsc::SyncSender<Result<AgentResponse, String>>,
>>>;

struct AgentClient {
    commands: mpsc::Sender<ClientCommand>,
    context: Arc<Mutex<Option<CaptureContext>>>,
    alive: Arc<AtomicBool>,
    lifecycle: Mutex<()>,
    next_lease: AtomicU64,
    active_lease: AtomicU64,
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
        tracing::trace!(target: "synly_input_agent", "Windows 输入代理 native runtime 开始停止"); // to remove
        let _ = self.backend.set_capture(false);
        let _ = self.backend.release_all();
        self.task.abort();
        let _ = self.task.await;
        tracing::trace!(target: "synly_input_agent", "Windows 输入代理 native runtime 已停止"); // to remove
    }
}

pub fn request_elevation() -> Result<()> {
    tracing::trace!("Windows 输入代理授权流程开始"); // to remove
    if let Some(client) = current_client()
        && client.request(AgentRequest::Health).is_ok()
    {
        tracing::trace!("Windows 输入代理已有可用连接, 跳过重新授权"); // to remove
        return Ok(());
    }

    let pipe_name = format!(r"\\.\pipe\synly-input-{}", Uuid::new_v4());
    let token = Uuid::new_v4().to_string();
    let parent_pid = unsafe { GetCurrentProcessId() };
    tracing::trace!(%pipe_name, parent_pid, "Windows 输入代理准备创建命名管道"); // to remove
    let mut options = ServerOptions::new();
    options
        .access_inbound(true)
        .access_outbound(true)
        .first_pipe_instance(true)
        .reject_remote_clients(true);
    let mut security = PipeSecurity::for_current_user()?;
    let server = unsafe {
        options.create_with_security_attributes_raw(
            &pipe_name,
            (&mut security.attributes as *mut SECURITY_ATTRIBUTES).cast(),
        )
    }
    .context("failed to create Windows input agent named pipe")?;
    tracing::trace!(%pipe_name, parent_pid, "Windows 输入代理命名管道已创建"); // to remove

    let executable = agent_executable()?;
    tracing::trace!(path = %executable.display(), "Windows 输入代理开始校验组件路径和签名"); // to remove
    validate_binary_signature(&executable)?;
    tracing::trace!(path = %executable.display(), "Windows 输入代理组件校验完成"); // to remove
    launch_elevated(&executable, &pipe_name, &token, parent_pid)?;
    tracing::trace!(path = %executable.display(), parent_pid, "Windows 输入代理 UAC 启动请求已提交"); // to remove

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("synly-input-agent-client".to_string())
        .spawn(move || {
            tracing::trace!(parent_pid, "Windows 输入代理 IPC 线程开始"); // to remove
            let error_ready = ready_tx.clone();
            let result = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("synly-input-agent-io")
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| {
                    runtime.block_on(connect_agent(server, token, parent_pid, ready_tx))
                });
            if let Err(error) = result {
                tracing::trace!(parent_pid, error = %error, "Windows 输入代理 IPC 线程返回错误"); // to remove
                let _ = error_ready.send(Err(anyhow!(format!("{error:#}"))));
                tracing::error!(error = %error, "Windows 输入代理连接线程结束");
            }
        })
        .context("failed to start Windows input agent client thread")?;

    let client = ready_rx
        .recv_timeout(CONNECT_TIMEOUT)
        .context("等待 Windows 输入代理授权和连接超时")??;
    tracing::trace!(parent_pid, "Windows 输入代理授权和 IPC 握手完成"); // to remove
    let slot = AGENT.get_or_init(|| Mutex::new(None));
    *slot.lock().map_err(|_| anyhow!("Windows input agent state poisoned"))? = Some(client);
    Ok(())
}

pub(in crate::input) fn ensure_ready(mode: InputMode) -> Result<()> {
    tracing::trace!(?mode, "Windows 输入代理 ensure_ready 开始"); // to remove
    if mode == InputMode::Off {
        tracing::trace!("Windows 输入代理 mode 为 Off, 跳过健康检查"); // to remove
        return Ok(());
    }
    let client = current_client().context("Windows 输入代理尚未获得本机管理员授权")?;
    client.request(AgentRequest::Health)?;
    tracing::trace!(?mode, "Windows 输入代理 ensure_ready 健康检查完成"); // to remove
    Ok(())
}

pub(in crate::input) fn is_ready() -> bool {
    current_client().is_some()
}

pub(in crate::input) fn start_client(context: CaptureContext) -> Result<Arc<dyn InputBackend>> {
    tracing::trace!(mode = ?context.mode, hotkey = ?context.hotkey, "Windows 输入代理 start_client 开始"); // to remove
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
    tracing::trace!(displays = ?layout.displays, "Windows 输入代理 Start response 已解析"); // to remove
    let lease = client.next_lease.fetch_add(1, Ordering::AcqRel);
    client.active_lease.store(lease, Ordering::Release);
    *client
        .context
        .lock()
        .map_err(|_| anyhow!("Windows input agent context poisoned"))? = Some(context);
    tracing::trace!(lease, "Windows 输入代理 capture context 已安装"); // to remove
    drop(lifecycle);
    Ok(Arc::new(AgentBackend {
        client,
        lease,
        layout,
    }))
}

pub async fn run_agent(pipe_name: String, token: String, parent_pid: u32) -> Result<()> {
    let mut client = connect_pipe(&pipe_name).await?;
    tracing::trace!(%pipe_name, parent_pid, "Windows 输入代理已连接 GUI 命名管道"); // to remove
    validate_pipe_server(&client, parent_pid)?;
    tracing::trace!(%pipe_name, parent_pid, "Windows 输入代理 pipe server PID 校验通过"); // to remove
    let agent_path = std::env::current_exe().context("failed to locate input agent executable")?;
    write_packet(
        &mut client,
        &AgentPacket::Hello {
            version: IPC_VERSION,
            token,
            agent_pid: unsafe { GetCurrentProcessId() },
            parent_pid,
            agent_path,
        },
    )
    .await?;
    tracing::trace!(parent_pid, "Windows 输入代理 Hello 已写入"); // to remove
    let ack = read_packet(&mut client).await?;
    let startup = (|| -> Result<()> {
        let AgentPacket::HelloAck {
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
        tracing::trace!(version, session_id, parent_pid, "Windows 输入代理 HelloAck 校验通过"); // to remove
        tracing::trace!(%pipe_name, parent_pid, "Windows 输入代理进程开始校验 GUI 父进程"); // to remove
        validate_parent_process(parent_pid)?;
        tracing::trace!(%pipe_name, parent_pid, "Windows 输入代理父进程校验通过"); // to remove
        Ok(())
    })();
    if let Err(error) = startup {
        let message = format!("{error:#}");
        tracing::trace!(parent_pid, error = %message, "Windows 输入代理初始化失败并回传 GUI"); // to remove
        let _ = write_packet(&mut client, &AgentPacket::StartupError { error: message }).await;
        return Err(error);
    }
    write_packet(&mut client, &AgentPacket::Ready).await?;
    tracing::trace!(parent_pid, "Windows 输入代理 Ready 已写入"); // to remove
    run_agent_loop(client).await
}

impl AgentClient {
    fn request(&self, request: AgentRequest) -> Result<AgentResponse> {
        if !self.alive.load(Ordering::Acquire) {
            tracing::trace!(request = request.name(), "Windows 输入代理同步请求发现连接已关闭"); // to remove
            bail!("Windows input agent connection is closed");
        }
        let request_name = request.name();
        tracing::trace!(request = request_name, "Windows 输入代理同步请求准备入队"); // to remove
        let (dispatch_tx, dispatch_rx) = std::sync::mpsc::sync_channel(1);
        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        self.commands
            .try_send(ClientCommand {
                request,
                queued_at: Instant::now(),
                dispatched: Some(dispatch_tx),
                response: Some(response_tx),
            })
            .map_err(|error| {
                tracing::trace!(request = request_name, error = %error, "Windows 输入代理同步请求入队失败"); // to remove
                if matches!(&error, mpsc::error::TrySendError::Closed(_)) {
                    mark_agent_unavailable(&self.alive, request_name, "command-queue-closed");
                }
                anyhow!("Windows input agent {request_name} command queue unavailable: {error}")
            })?;
        let response = wait_for_agent_response(
            &self.alive,
            request_name,
            dispatch_rx,
            response_rx,
            DISPATCH_TIMEOUT,
            REQUEST_TIMEOUT,
        );
        tracing::trace!(request = request_name, result = ?response.as_ref().err(), "Windows 输入代理同步请求等待结束"); // to remove
        response
    }

    fn notify(&self, request: AgentRequest) {
        if !self.alive.load(Ordering::Acquire) {
            tracing::trace!(request = request.name(), "Windows 输入代理通知因连接关闭被丢弃"); // to remove
            return;
        }
        let request_name = request.name();
        if let Err(error) = self.commands.try_send(ClientCommand {
            request,
            queued_at: Instant::now(),
            dispatched: None,
            response: None,
        }) && matches!(error, mpsc::error::TrySendError::Closed(_))
        {
            tracing::trace!(request = request_name, error = %error, "Windows 输入代理通知入队失败"); // to remove
            mark_agent_unavailable(&self.alive, request_name, "notify-queue-closed");
        }
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
    response_timeout: Duration,
) -> Result<AgentResponse> {
    tracing::trace!(request = request_name, dispatch_timeout_ms = dispatch_timeout.as_millis(), response_timeout_ms = response_timeout.as_millis(), "Windows 输入代理开始等待派发和响应"); // to remove
    match dispatch_rx.recv_timeout(dispatch_timeout) {
        Ok(()) => {
            tracing::trace!(request = request_name, "Windows 输入代理请求已确认写入 pipe"); // to remove
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            tracing::trace!(request = request_name, "Windows 输入代理请求等待 pipe 派发超时"); // to remove
            mark_agent_unavailable(alive, request_name, "dispatch-timeout");
            bail!("Windows input agent {request_name} dispatch timed out");
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            tracing::trace!(request = request_name, "Windows 输入代理请求在 pipe 派发前断开"); // to remove
            mark_agent_unavailable(alive, request_name, "dispatch-disconnected");
            bail!("Windows input agent {request_name} dispatch failed before pipe write");
        }
    }
    match response_rx.recv_timeout(response_timeout) {
        Ok(Ok(AgentResponse::Error(message))) => {
            tracing::trace!(request = request_name, error = %message, "Windows 输入代理返回业务错误"); // to remove
            Err(anyhow!(message))
        }
        Ok(Ok(response)) => {
            tracing::trace!(request = request_name, "Windows 输入代理响应已匹配"); // to remove
            Ok(response)
        }
        Ok(Err(message)) => {
            tracing::trace!(request = request_name, error = %message, "Windows 输入代理 pending response 返回连接错误"); // to remove
            Err(anyhow!(message))
        }
        Err(error) => {
            tracing::trace!(request = request_name, error = %error, "Windows 输入代理等待 response 超时或断开"); // to remove
            mark_agent_unavailable(alive, request_name, "response-timeout");
            Err(error).with_context(|| {
                format!("Windows input agent {request_name} request timed out")
            })
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
        let request_name = request.name();
        tracing::trace!(request = request_name, lease = self.lease, active_lease = self.client.active_lease.load(Ordering::Acquire), "Windows 输入代理 backend 同步调用开始"); // to remove
        let _lifecycle = self
            .client
            .lifecycle
            .lock()
            .map_err(|_| anyhow!("Windows input agent lifecycle poisoned"))?;
        if !self.is_current() {
            tracing::trace!(request = request_name, lease = self.lease, active_lease = self.client.active_lease.load(Ordering::Acquire), "Windows 输入代理 backend lease 已过期"); // to remove
            bail!("Windows input agent backend was superseded by a newer session");
        }
        let response = self.client.request(request);
        if let Err(error) = &response {
            tracing::trace!(request = request_name, lease = self.lease, error = %error, "Windows 输入代理 backend 同步调用失败"); // to remove
            self.client.emit_failure(error);
        } else {
            tracing::trace!(request = request_name, lease = self.lease, "Windows 输入代理 backend 同步调用完成"); // to remove
        }
        response
    }

    fn is_current(&self) -> bool {
        self.client.active_lease.load(Ordering::Acquire) == self.lease
    }

    fn notify(&self, request: AgentRequest) -> Result<()> {
        let request_name = request.name();
        let _lifecycle = self
            .client
            .lifecycle
            .lock()
            .map_err(|_| anyhow!("Windows input agent lifecycle poisoned"))?;
        if !self.is_current() {
            tracing::trace!(request = request_name, lease = self.lease, active_lease = self.client.active_lease.load(Ordering::Acquire), "Windows 输入代理 backend 通知 lease 已过期"); // to remove
            bail!("Windows input agent backend was superseded by a newer session");
        }
        if !self.client.alive.load(Ordering::Acquire) {
            tracing::trace!(request = request_name, lease = self.lease, "Windows 输入代理 backend 通知发现连接关闭"); // to remove
            bail!("Windows input agent connection is closed");
        }
        self.client.notify(request);
        Ok(())
    }
}

impl Drop for AgentBackend {
    fn drop(&mut self) {
        tracing::trace!(lease = self.lease, active_lease = self.client.active_lease.load(Ordering::Acquire), "Windows 输入代理 backend 开始 Drop"); // to remove
        let Ok(_lifecycle) = self.client.lifecycle.lock() else {
            tracing::trace!(lease = self.lease, "Windows 输入代理 backend Drop 无法获取 lifecycle lock"); // to remove
            return;
        };
        if self
            .client
            .active_lease
            .compare_exchange(self.lease, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            tracing::trace!(lease = self.lease, active_lease = self.client.active_lease.load(Ordering::Acquire), "Windows 输入代理 backend Drop 忽略过期 lease"); // to remove
            return;
        }
        tracing::trace!(lease = self.lease, "Windows 输入代理 backend Drop 已入队 Stop"); // to remove
        self.client.notify(AgentRequest::Stop);
        if let Ok(mut context) = self.client.context.lock() {
            *context = None;
        }
    }
}

async fn connect_agent(
    mut server: NamedPipeServer,
    token: String,
    parent_pid: u32,
    ready: std::sync::mpsc::SyncSender<Result<Arc<AgentClient>>>,
) -> Result<()> {
    tracing::trace!(parent_pid, "Windows 输入代理 GUI 端等待命名管道连接"); // to remove
    time::timeout(CONNECT_TIMEOUT, server.connect())
        .await
        .context("Windows input agent did not connect to the named pipe")??;
    tracing::trace!(parent_pid, "Windows 输入代理 GUI 端命名管道已连接"); // to remove
    let hello = read_packet(&mut server).await?;
    let AgentPacket::Hello {
        version,
        token: incoming_token,
        agent_pid,
        parent_pid: incoming_parent,
        agent_path,
    } = hello
    else {
        tracing::trace!(parent_pid, "Windows 输入代理 GUI 端收到非 Hello 握手包"); // to remove
        bail!("Windows input agent sent an invalid handshake");
    };
    tracing::trace!(version, agent_pid, incoming_parent, expected_parent = parent_pid, token_matches = incoming_token == token, "Windows 输入代理 GUI 端收到 Hello"); // to remove
    if version != IPC_VERSION {
        tracing::trace!(version, expected_version = IPC_VERSION, agent_pid, incoming_parent, expected_parent = parent_pid, token_matches = incoming_token == token, "Windows 输入代理 GUI 端 Hello 版本校验失败"); // to remove
        bail!(
            "Windows input agent handshake version mismatch: agent={version}, GUI={}",
            IPC_VERSION
        );
    }
    if incoming_token != token || incoming_parent != parent_pid {
        tracing::trace!(version, expected_version = IPC_VERSION, agent_pid, incoming_parent, expected_parent = parent_pid, token_matches = incoming_token == token, "Windows 输入代理 GUI 端 Hello 校验失败"); // to remove
        bail!("Windows input agent handshake token or parent mismatch");
    }
    validate_pipe_client(&server, agent_pid, &agent_path)?;
    tracing::trace!(agent_pid, path = %agent_path.display(), "Windows 输入代理 GUI 端 pipe client 校验通过"); // to remove
    let session_id = process_session_id(parent_pid)?;
    write_packet(
        &mut server,
        &AgentPacket::HelloAck {
            version: IPC_VERSION,
            session_id,
        },
    )
    .await?;
    tracing::trace!(agent_pid, parent_pid, session_id, "Windows 输入代理 GUI 端 HelloAck 已写入"); // to remove
    let startup = time::timeout(CONNECT_TIMEOUT, read_packet(&mut server))
        .await
        .context("Windows input agent startup confirmation timed out")??;
    match startup {
        AgentPacket::Ready => {
            tracing::trace!(agent_pid, parent_pid, "Windows 输入代理 GUI 端收到 Ready"); // to remove
        }
        AgentPacket::StartupError { error } => {
            tracing::trace!(agent_pid, parent_pid, error = %error, "Windows 输入代理 GUI 端收到初始化错误"); // to remove
            bail!("Windows input agent startup failed: {error}");
        }
        other => {
            bail!(
                "Windows input agent sent an invalid startup packet: {}",
                agent_packet_name(&other)
            );
        }
    }

    let (commands, command_rx) = mpsc::channel(64);
    let context = Arc::new(Mutex::new(None));
    let alive = Arc::new(AtomicBool::new(true));
    let client = Arc::new(AgentClient {
        commands,
        context: Arc::clone(&context),
        alive: Arc::clone(&alive),
        lifecycle: Mutex::new(()),
        next_lease: AtomicU64::new(1),
        active_lease: AtomicU64::new(0),
    });
    ready
        .send(Ok(Arc::clone(&client)))
        .map_err(|_| anyhow!("Windows input agent readiness receiver closed"))?;
    tracing::trace!(agent_pid, parent_pid, "Windows 输入代理 GUI 端 client 已发布为 ready"); // to remove
    let heartbeat_task = spawn_client_heartbeat(client.commands.clone(), Arc::clone(&alive));
    let result = client_loop(server, command_rx, context, alive).await;
    tracing::trace!(agent_pid, parent_pid, result = ?result.as_ref().err(), "Windows 输入代理 GUI 端 client loop 已返回"); // to remove
    heartbeat_task.abort();
    let _ = heartbeat_task.await;
    result
}

fn spawn_client_heartbeat(
    commands: mpsc::Sender<ClientCommand>,
    alive: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut heartbeat = time::interval(CLIENT_HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            heartbeat.tick().await;
            if !alive.load(Ordering::Acquire) {
                break;
            }
            match commands.try_send(ClientCommand {
                request: AgentRequest::Health,
                queued_at: Instant::now(),
                dispatched: None,
                response: None,
            }) {
                Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => {}
                Err(mpsc::error::TrySendError::Closed(_)) => break,
            }
        }
    })
}

async fn client_loop(
    server: NamedPipeServer,
    commands: mpsc::Receiver<ClientCommand>,
    context: Arc<Mutex<Option<CaptureContext>>>,
    alive: Arc<AtomicBool>,
) -> Result<()> {
    tracing::trace!("Windows 输入代理 GUI client loop 开始"); // to remove
    let (reader, writer) = tokio::io::split(server);
    let (mut packets, reader_task) = spawn_packet_reader(reader);
    let pending = Arc::new(Mutex::new(HashMap::new()));
    let heartbeat_probe = Arc::new(AtomicU64::new(0));
    let mut writer_task = tokio::spawn(command_writer_loop(
        writer,
        commands,
        Arc::clone(&pending),
        Arc::clone(&heartbeat_probe),
    ));
    let mut writer_finished = false;
    let mut unmatched_response = 0usize;
    let result: Result<()> = async {
        loop {
            tokio::select! {
            writer_result = &mut writer_task => {
                writer_finished = true;
                tracing::trace!(result = ?writer_result.as_ref().err(), "Windows 输入代理 GUI command writer 已结束"); // to remove
                break match writer_result {
                    Ok(result) => result,
                    Err(error) => Err(error.into()),
                };
            }
            packet = packets.recv() => {
                let packet = packet.context("Windows input agent packet reader stopped")??;
                match packet {
                    AgentPacket::Response { id, response } => {
                        let response_name = agent_response_name(&response);
                        let heartbeat_response = heartbeat_probe
                            .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok();
                        if heartbeat_response {
                            let roundtrip = id;
                            if roundtrip == 1 || roundtrip % 10 == 0 {
                                tracing::debug!(
                                    target: "synly_input_agent",
                                    request_id = id,
                                    "Windows 输入代理保活往返已确认"
                                );
                            }
                        }
                        let sender = pending
                            .lock()
                            .map_err(|_| anyhow!("Windows input agent pending response state poisoned"))?
                            .remove(&id);
                        if let Some(sender) = sender {
                            tracing::trace!(request_id = id, response = response_name, "Windows 输入代理 GUI response id 已匹配 pending"); // to remove
                            let _ = sender.try_send(Ok(response));
                        } else {
                            unmatched_response = unmatched_response.saturating_add(1);
                            if !heartbeat_response
                                && (unmatched_response == 1 || unmatched_response % 500 == 0)
                            {
                                tracing::trace!(count = unmatched_response, request_id = id, response = response_name, "Windows 输入代理 GUI 收到无 waiter response 汇总"); // to remove
                            }
                        }
                    }
                    AgentPacket::Diagnostic {
                        request,
                        phase,
                        elapsed_ms,
                        error,
                    } => match phase {
                        AgentDiagnosticPhase::Started => {
                            tracing::info!(
                                target: "synly_input_agent",
                                %request,
                                "Windows 输入代理开始处理请求"
                            );
                        }
                        AgentDiagnosticPhase::Completed => {
                            tracing::debug!(
                                target: "synly_input_agent",
                                %request,
                                elapsed_ms,
                                "Windows 输入代理完成请求"
                            );
                        }
                        AgentDiagnosticPhase::Failed => {
                            tracing::error!(
                                target: "synly_input_agent",
                                %request,
                                elapsed_ms,
                                error = error.as_deref().unwrap_or("unknown error"),
                                "Windows 输入代理请求失败"
                            );
                        }
                    },
                    AgentPacket::Event(event) => {
                        if let Ok(context) = context.lock()
                            && let Some(context) = context.as_ref()
                        {
                            context.emit_reliable(event);
                        }
                    }
                    AgentPacket::Motion {
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
                    AgentPacket::SecureDesktopPaused(paused) => {
                        if let Ok(context) = context.lock()
                            && let Some(context) = context.as_ref()
                            && paused
                        {
                            context.emit_reliable(NativeEvent::Emergency);
                        }
                        tracing::warn!(paused, "Windows 输入代理安全桌面状态变化");
                    }
                    _ => break Err(anyhow!("Windows input agent sent an unexpected packet")),
                }
            }
            }
        }
    }
    .await;
    tracing::trace!(writer_finished, unmatched_response, result = ?result.as_ref().err(), "Windows 输入代理 GUI client loop 准备清理"); // to remove
    if !writer_finished {
        writer_task.abort();
        let _ = writer_task.await;
    }
    reader_task.abort();
    let _ = reader_task.await;
    alive.store(false, Ordering::Release);
    let pending = pending
        .lock()
        .map(|mut pending| std::mem::take(&mut *pending))
        .unwrap_or_default();
    for (_, sender) in pending {
        let _ = sender.try_send(Err("Windows input agent connection closed".to_string()));
    }
    if let Ok(context) = context.lock()
        && let Some(context) = context.as_ref()
    {
        context.emit_reliable(NativeEvent::Failed(
            "Windows 输入代理连接已断开".to_string(),
        ));
    }
    result
}

fn agent_response_name(response: &AgentResponse) -> &'static str {
    match response {
        AgentResponse::Ok => "Ok",
        AgentResponse::Pong => "Pong",
        AgentResponse::Started { .. } => "Started",
        AgentResponse::Point(_) => "Point",
        AgentResponse::Snapshot(_) => "Snapshot",
        AgentResponse::Error(_) => "Error",
    }
}

fn agent_completion_packet(
    id: u64,
    expects_response: bool,
    response: Result<AgentResponse>,
) -> Option<AgentPacket> {
    if expects_response {
        return Some(AgentPacket::Response {
            id,
            response: response
                .unwrap_or_else(|error| AgentResponse::Error(format!("{error:#}"))),
        });
    }
    response.err().map(|error| {
        AgentPacket::Event(NativeEvent::Failed(format!(
            "Windows input agent notification failed: {error:#}"
        )))
    })
}

async fn command_writer_loop<W>(
    mut writer: W,
    mut commands: mpsc::Receiver<ClientCommand>,
    pending: PendingResponses,
    heartbeat_probe: Arc<AtomicU64>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut next_id = 1u64;
    let mut written_inject_cursor = 0usize;
    tracing::trace!("Windows 输入代理 GUI command writer 开始"); // to remove
    while let Some(command) = commands.recv().await {
        let ClientCommand {
            request,
            queued_at,
            dispatched,
            response,
        } = command;
        let id = next_id;
        next_id = next_id.saturating_add(1);
        let request_name = request.name();
        let report_diagnostic = request.reports_diagnostic();
        let is_inject_cursor = matches!(&request, AgentRequest::InjectCursor(_));
        let is_heartbeat_probe = matches!(&request, AgentRequest::Health)
            && response.is_none()
            && heartbeat_probe.load(Ordering::Acquire) == 0;
        let expects_response = response.is_some() || is_heartbeat_probe;
        if let Some(response) = response {
            pending
                .lock()
                .map_err(|_| anyhow!("Windows input agent pending response state poisoned"))?
                .insert(id, response);
            tracing::trace!(request = request_name, request_id = id, "Windows 输入代理 GUI 已登记 pending response"); // to remove
        }
        if !is_inject_cursor && !matches!(&request, AgentRequest::Health) {
            tracing::trace!(request = request_name, request_id = id, queue_ms = queued_at.elapsed().as_millis(), "Windows 输入代理 GUI 开始写入 IPC request"); // to remove
        }
        if let Err(error) = write_packet(
            &mut writer,
            &AgentPacket::Request {
                id,
                expects_response,
                request,
            },
        )
        .await
        {
            tracing::trace!(request = request_name, request_id = id, error = %error, "Windows 输入代理 GUI 写入 IPC request 失败"); // to remove
            if let Some(response) = pending
                .lock()
                .map_err(|_| anyhow!("Windows input agent pending response state poisoned"))?
                .remove(&id)
            {
                let _ = response.try_send(Err(format!("{error:#}")));
            }
            return Err(error);
        }
        if is_inject_cursor {
            written_inject_cursor = written_inject_cursor.saturating_add(1);
            if written_inject_cursor == 1 || written_inject_cursor % 500 == 0 {
                tracing::trace!(count = written_inject_cursor, request_id = id, "Windows 输入代理 GUI 已写入 InjectCursor 汇总"); // to remove
            }
        } else if request_name != "Health" {
            tracing::trace!(request = request_name, request_id = id, "Windows 输入代理 GUI IPC request 写入完成"); // to remove
        }
        if let Some(dispatched) = dispatched {
            let _ = dispatched.send(());
            tracing::trace!(request = request_name, request_id = id, "Windows 输入代理 GUI 已通知同步调用方 request 完成派发"); // to remove
        }
        if is_heartbeat_probe
            && heartbeat_probe
                .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            && (id == 1 || id % 10 == 0)
        {
            tracing::debug!(
                target: "synly_input_agent",
                request_id = id,
                "Windows 输入代理保活请求已写入"
            );
        }
        if report_diagnostic {
            tracing::debug!(
                target: "synly_input_agent",
                request = request_name,
                request_id = id,
                queue_ms = queued_at.elapsed().as_millis(),
                "Windows 输入代理 IPC 请求已写入"
            );
        }
    }
    tracing::trace!(inject_cursor_count = written_inject_cursor, "Windows 输入代理 GUI command writer 收到关闭信号"); // to remove
    Ok(())
}

async fn run_agent_loop(client: tokio::net::windows::named_pipe::NamedPipeClient) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(client);
    let (mut packets, reader_task) = spawn_packet_reader(reader);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<AgentPacket>(256);
    let mut runtime = None;
    let mut heartbeat = AgentHeartbeat::new(Instant::now());
    let mut paused = false;
    let mut desktop_tick = time::interval(Duration::from_millis(250));
    desktop_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut processed_inject_cursor = 0usize;
    let mut outgoing_motion = 0usize;
    tracing::trace!("Windows 输入代理进程 request loop 开始"); // to remove

    let result: Result<()> = async {
        loop {
            tokio::select! {
                packet = packets.recv() => {
                    let packet = packet.context("Windows input agent request reader stopped")??;
                    let AgentPacket::Request {
                        id,
                        expects_response,
                        request,
                    } = packet else {
                        bail!("Windows input agent received an unexpected packet");
                    };
                    heartbeat.observe(Instant::now());
                    let request_name = request.name().to_string();
                    let report_diagnostic = request.reports_diagnostic();
                    let is_inject_cursor = matches!(&request, AgentRequest::InjectCursor(_));
                    if is_inject_cursor {
                        processed_inject_cursor = processed_inject_cursor.saturating_add(1);
                        if processed_inject_cursor == 1 || processed_inject_cursor % 500 == 0 {
                            tracing::trace!(target: "synly_input_agent", count = processed_inject_cursor, request_id = id, "Windows 输入代理进程收到 InjectCursor 汇总"); // to remove
                        }
                    } else if request_name != "Health" {
                        tracing::trace!(target: "synly_input_agent", request = %request_name, request_id = id, runtime_started = runtime.is_some(), "Windows 输入代理进程收到 request"); // to remove
                    }
                    if report_diagnostic {
                        write_packet(
                            &mut writer,
                            &AgentPacket::Diagnostic {
                                request: request_name.clone(),
                                phase: AgentDiagnosticPhase::Started,
                                elapsed_ms: 0,
                                error: None,
                            },
                        )
                        .await?;
                    }
                    let started = Instant::now();
                    let response = handle_agent_request(request, &mut runtime, outgoing_tx.clone()).await;
                    if !is_inject_cursor && request_name != "Health" {
                        tracing::trace!(target: "synly_input_agent", request = %request_name, request_id = id, elapsed_ms = started.elapsed().as_millis(), result = ?response.as_ref().err(), "Windows 输入代理进程 backend 调用结束"); // to remove
                    }
                    if report_diagnostic {
                        let elapsed_ms = started
                            .elapsed()
                            .as_millis()
                            .try_into()
                            .unwrap_or(u64::MAX);
                        let (phase, error) = match &response {
                            Ok(_) => (AgentDiagnosticPhase::Completed, None),
                            Err(error) => (
                                AgentDiagnosticPhase::Failed,
                                Some(format!("{error:#}")),
                            ),
                        };
                        write_packet(
                            &mut writer,
                            &AgentPacket::Diagnostic {
                                request: request_name.clone(),
                                phase,
                                elapsed_ms,
                                error,
                            },
                        )
                        .await?;
                    }
                    if let Some(completion) =
                        agent_completion_packet(id, expects_response, response)
                    {
                        let completion_name = agent_packet_name(&completion);
                        write_packet(&mut writer, &completion).await?;
                        if !is_inject_cursor && request_name != "Health" {
                            tracing::trace!(target: "synly_input_agent", request = %request_name, request_id = id, packet = completion_name, "Windows 输入代理进程完成包已写入"); // to remove
                        }
                    }
                    heartbeat.observe(Instant::now());
                }
                outgoing = outgoing_rx.recv() => {
                    let Some(outgoing) = outgoing else { break Ok(()) };
                    if matches!(&outgoing, AgentPacket::Motion { .. }) {
                        outgoing_motion = outgoing_motion.saturating_add(1);
                        if outgoing_motion == 1 || outgoing_motion % 500 == 0 {
                            tracing::trace!(target: "synly_input_agent", count = outgoing_motion, "Windows 输入代理进程已写入 Motion 汇总"); // to remove
                        }
                    }
                    write_packet(&mut writer, &outgoing).await?;
                }
                _ = desktop_tick.tick() => {
                    if heartbeat.expired(Instant::now()) {
                        tracing::trace!(target: "synly_input_agent", elapsed_ms = heartbeat.last_seen.elapsed().as_millis(), "Windows 输入代理进程 GUI heartbeat 超时"); // to remove
                        let message = "Windows input agent GUI heartbeat timed out";
                        let _ = write_packet(
                            &mut writer,
                            &AgentPacket::Diagnostic {
                                request: "Heartbeat".to_string(),
                                phase: AgentDiagnosticPhase::Failed,
                                elapsed_ms: heartbeat.last_seen.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                                error: Some(message.to_string()),
                            },
                        )
                        .await;
                        break Err(anyhow!(message));
                    }
                    let current_paused = !is_default_input_desktop();
                    if current_paused != paused {
                        paused = current_paused;
                        tracing::trace!(target: "synly_input_agent", paused, "Windows 输入代理进程 desktop 状态变化"); // to remove
                        if paused
                            && let Some(runtime) = runtime.as_ref()
                        {
                            let _ = runtime.backend.set_capture(false);
                            let _ = runtime.backend.release_all();
                        }
                        outgoing_tx
                            .send(AgentPacket::SecureDesktopPaused(paused))
                            .await?;
                    }
                }
            }
        }
    }
    .await;
    tracing::trace!(target: "synly_input_agent", inject_cursor_count = processed_inject_cursor, outgoing_motion_count = outgoing_motion, result = ?result.as_ref().err(), "Windows 输入代理进程 request loop 已返回"); // to remove
    reader_task.abort();
    let _ = reader_task.await;
    if let Some(runtime) = runtime {
        runtime.stop().await;
    }
    result
}

async fn handle_agent_request(
    request: AgentRequest,
    runtime: &mut Option<NativeAgentRuntime>,
    outgoing: mpsc::Sender<AgentPacket>,
) -> Result<AgentResponse> {
    match request {
        AgentRequest::Start { mode, hotkey } => {
            tracing::trace!(target: "synly_input_agent", ?mode, ?hotkey, replacing_runtime = runtime.is_some(), "Windows 输入代理开始执行 Start"); // to remove
            if let Some(previous) = runtime.take() {
                previous.stop().await;
            }
            *runtime = Some(start_native_runtime(mode, hotkey, outgoing)?);
            let layout = agent_backend(runtime)?.layout()?;
            tracing::trace!(target: "synly_input_agent", displays = ?layout.displays, "Windows 输入代理 Start 已获取布局"); // to remove
            Ok(AgentResponse::Started { layout })
        }
        AgentRequest::Stop => {
            tracing::trace!(target: "synly_input_agent", runtime_started = runtime.is_some(), "Windows 输入代理开始执行 Stop"); // to remove
            if let Some(previous) = runtime.take() {
                previous.stop().await;
            }
            tracing::trace!(target: "synly_input_agent", "Windows 输入代理 Stop 完成"); // to remove
            Ok(AgentResponse::Ok)
        }
        AgentRequest::Health => Ok(AgentResponse::Pong),
        AgentRequest::CursorPosition => {
            Ok(AgentResponse::Point(agent_backend(runtime)?.cursor_position()?))
        }
        AgentRequest::Snapshot => Ok(AgentResponse::Snapshot(agent_backend(runtime)?.snapshot())),
        AgentRequest::SetCapture(active) => {
            tracing::trace!(target: "synly_input_agent", active, "Windows 输入代理开始执行 SetCapture"); // to remove
            agent_backend(runtime)?.set_capture(active)?;
            tracing::trace!(target: "synly_input_agent", active, "Windows 输入代理 SetCapture 完成"); // to remove
            Ok(AgentResponse::Ok)
        }
        AgentRequest::WarpCursor(point) => {
            tracing::trace!(target: "synly_input_agent", point = ?point, "Windows 输入代理开始执行 WarpCursor"); // to remove
            agent_backend(runtime)?.warp_cursor(point)?;
            tracing::trace!(target: "synly_input_agent", point = ?point, "Windows 输入代理 WarpCursor 完成"); // to remove
            Ok(AgentResponse::Ok)
        }
        AgentRequest::InjectKey {
            usage,
            modifiers,
            down,
            repeat,
        } => {
            tracing::trace!(target: "synly_input_agent", usage, down, repeat, "Windows 输入代理开始执行 InjectKey"); // to remove
            agent_backend(runtime)?.inject_key(usage, modifiers, down, repeat)?;
            Ok(AgentResponse::Ok)
        }
        AgentRequest::InjectButton { button, down } => {
            tracing::trace!(target: "synly_input_agent", button, down, "Windows 输入代理开始执行 InjectButton"); // to remove
            agent_backend(runtime)?.inject_button(button, down)?;
            Ok(AgentResponse::Ok)
        }
        AgentRequest::InjectCursor(point) => {
            agent_backend(runtime)?.inject_cursor(point)?;
            Ok(AgentResponse::Ok)
        }
        AgentRequest::InjectWheel { x, y } => {
            tracing::trace!(target: "synly_input_agent", x, y, "Windows 输入代理开始执行 InjectWheel"); // to remove
            agent_backend(runtime)?.inject_wheel(x, y)?;
            Ok(AgentResponse::Ok)
        }
        AgentRequest::ReleaseAll => {
            tracing::trace!(target: "synly_input_agent", "Windows 输入代理开始执行 ReleaseAll"); // to remove
            agent_backend(runtime)?.release_all()?;
            tracing::trace!(target: "synly_input_agent", "Windows 输入代理 ReleaseAll 完成"); // to remove
            Ok(AgentResponse::Ok)
        }
    }
}

fn start_native_runtime(
    mode: InputMode,
    hotkey: Hotkey,
    outgoing: mpsc::Sender<AgentPacket>,
) -> Result<NativeAgentRuntime> {
    tracing::trace!(target: "synly_input_agent", ?mode, ?hotkey, "Windows 输入代理开始创建 native runtime"); // to remove
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
    tracing::trace!(target: "synly_input_agent", ?mode, "Windows 输入代理 native backend 已启动"); // to remove
    let task = tokio::spawn(async move {
        let mut motion_tick = time::interval(Duration::from_millis(8));
        motion_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                event = events_rx.recv() => {
                    let Some(event) = event else { break };
                    if outgoing.send(AgentPacket::Event(event)).await.is_err() {
                        break;
                    }
                }
                _ = motion_tick.tick(), if send_motion => {
                    let sample = motion.take_observed();
                    if (sample.dx != 0 || sample.dy != 0 || sample.position_updated)
                        && outgoing
                            .send(AgentPacket::Motion {
                                dx: sample.dx,
                                dy: sample.dy,
                                position: sample.position,
                                position_updated: sample.position_updated,
                            })
                            .await
                            .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
    tracing::trace!(target: "synly_input_agent", ?mode, "Windows 输入代理 native runtime 已就绪"); // to remove
    Ok(NativeAgentRuntime { backend, task })
}

fn agent_backend(runtime: &Option<NativeAgentRuntime>) -> Result<&Arc<dyn InputBackend>> {
    runtime
        .as_ref()
        .map(|runtime| &runtime.backend)
        .context("Windows input agent runtime is not started")
}

async fn connect_pipe(
    pipe_name: &str,
) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
    tracing::trace!(%pipe_name, "Windows 输入代理进程开始连接 GUI pipe"); // to remove
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    let mut attempts = 0usize;
    loop {
        attempts = attempts.saturating_add(1);
        match ClientOptions::new().read(true).write(true).open(pipe_name) {
            Ok(client) => {
                tracing::trace!(%pipe_name, attempts, "Windows 输入代理进程连接 GUI pipe 成功"); // to remove
                return Ok(client);
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(error = %error, "等待 GUI 命名管道就绪");
                time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => {
                tracing::trace!(%pipe_name, attempts, error = %error, "Windows 输入代理进程连接 GUI pipe 失败"); // to remove
                return Err(error).context("failed to connect Windows input agent pipe");
            }
        }
    }
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
    pipe_name: &str,
    token: &str,
    parent_pid: u32,
) -> Result<()> {
    let verb = wide("runas");
    let executable = wide(&executable.to_string_lossy());
    let parameters = wide(&format!(
        "__input-agent --pipe \"{pipe_name}\" --token \"{token}\" --parent-pid {parent_pid}"
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
    client: &tokio::net::windows::named_pipe::NamedPipeClient,
    expected_pid: u32,
) -> Result<()> {
    let mut actual_pid = 0u32;
    let ok = unsafe {
        GetNamedPipeServerProcessId(client.as_raw_handle() as HANDLE, &mut actual_pid)
    };
    if ok == 0 || actual_pid != expected_pid {
        bail!("Windows input agent named pipe server PID validation failed");
    }
    Ok(())
}

fn validate_pipe_client(
    server: &NamedPipeServer,
    expected_pid: u32,
    reported_path: &Path,
) -> Result<()> {
    let mut actual_pid = 0u32;
    let ok = unsafe {
        GetNamedPipeClientProcessId(server.as_raw_handle() as HANDLE, &mut actual_pid)
    };
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

async fn write_packet<W>(writer: &mut W, packet: &AgentPacket) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let packet_name = agent_packet_name(packet);
    let bytes = bincode::serialize(packet).context("failed to encode Windows input agent packet")?;
    if bytes.is_empty() || bytes.len() > IPC_MAX_FRAME {
        tracing::trace!(target: "synly_input_agent", packet = packet_name, length = bytes.len(), "Windows 输入代理 IPC packet 长度校验失败"); // to remove
        bail!("Windows input agent packet length is invalid");
    }
    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .map_err(|error| {
            tracing::trace!(target: "synly_input_agent", packet = packet_name, error = %error, "Windows 输入代理 IPC packet 长度写入失败"); // to remove
            error
        })?;
    writer.write_all(&bytes).await.map_err(|error| {
        tracing::trace!(target: "synly_input_agent", packet = packet_name, error = %error, "Windows 输入代理 IPC packet body 写入失败"); // to remove
        error
    })?;
    writer.flush().await.map_err(|error| {
        tracing::trace!(target: "synly_input_agent", packet = packet_name, error = %error, "Windows 输入代理 IPC packet flush 失败"); // to remove
        error
    })?;
    Ok(())
}

async fn read_packet<R>(reader: &mut R) -> Result<AgentPacket>
where
    R: AsyncRead + Unpin,
{
    let length = reader.read_u32().await.map_err(|error| {
        tracing::trace!(target: "synly_input_agent", error = %error, "Windows 输入代理 IPC packet 长度读取失败"); // to remove
        error
    })? as usize;
    if length == 0 || length > IPC_MAX_FRAME {
        tracing::trace!(target: "synly_input_agent", length, "Windows 输入代理 IPC packet 长度校验失败"); // to remove
        bail!("Windows input agent packet length is invalid: {length}");
    }
    let mut bytes = vec![0u8; length];
    reader.read_exact(&mut bytes).await.map_err(|error| {
        tracing::trace!(target: "synly_input_agent", length, error = %error, "Windows 输入代理 IPC packet body 读取失败"); // to remove
        error
    })?;
    bincode::deserialize(&bytes)
        .inspect_err(|error| {
            tracing::trace!(target: "synly_input_agent", length, error = %error, "Windows 输入代理 IPC packet 解码失败"); // to remove
        })
        .context("failed to decode Windows input agent packet")
}

fn agent_packet_name(packet: &AgentPacket) -> &'static str {
    match packet {
        AgentPacket::Hello { .. } => "Hello",
        AgentPacket::HelloAck { .. } => "HelloAck",
        AgentPacket::Ready => "Ready",
        AgentPacket::StartupError { .. } => "StartupError",
        AgentPacket::Request { .. } => "Request",
        AgentPacket::Response { .. } => "Response",
        AgentPacket::Diagnostic { .. } => "Diagnostic",
        AgentPacket::Event(_) => "Event",
        AgentPacket::Motion { .. } => "Motion",
        AgentPacket::SecureDesktopPaused(_) => "SecureDesktopPaused",
    }
}

fn spawn_packet_reader<R>(
    mut reader: R,
) -> (
    mpsc::Receiver<Result<AgentPacket>>,
    tokio::task::JoinHandle<()>,
)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (packets, receiver) = mpsc::channel(256);
    let task = tokio::spawn(async move {
        loop {
            match read_packet(&mut reader).await {
                Ok(packet) => {
                    if packets.send(Ok(packet)).await.is_err() {
                        tracing::trace!(target: "synly_input_agent", "Windows 输入代理 IPC packet reader 下游已关闭"); // to remove
                        break;
                    }
                }
                Err(error) => {
                    tracing::trace!(target: "synly_input_agent", error = %error, "Windows 输入代理 IPC packet reader 返回错误"); // to remove
                    let _ = packets.send(Err(error)).await;
                    break;
                }
            }
        }
        tracing::trace!(target: "synly_input_agent", "Windows 输入代理 IPC packet reader task 已退出"); // to remove
    });
    (receiver, task)
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

    #[test]
    fn closed_command_queue_marks_agent_unavailable() {
        let (commands, receiver) = mpsc::channel(1);
        drop(receiver);
        let alive = Arc::new(AtomicBool::new(true));
        let client = AgentClient {
            commands,
            context: Arc::new(Mutex::new(None)),
            alive: Arc::clone(&alive),
            lifecycle: Mutex::new(()),
            next_lease: AtomicU64::new(1),
            active_lease: AtomicU64::new(0),
        };

        assert!(client.request(AgentRequest::Health).is_err());
        assert!(!alive.load(Ordering::Acquire));
    }

    #[test]
    fn stale_backend_drop_does_not_stop_current_lease() {
        let (commands, mut receiver) = mpsc::channel(4);
        let client = Arc::new(AgentClient {
            commands,
            context: Arc::new(Mutex::new(None)),
            alive: Arc::new(AtomicBool::new(true)),
            lifecycle: Mutex::new(()),
            next_lease: AtomicU64::new(3),
            active_lease: AtomicU64::new(2),
        });
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
        let (commands, mut receiver) = mpsc::channel(4);
        let client = Arc::new(AgentClient {
            commands,
            context: Arc::new(Mutex::new(None)),
            alive: Arc::new(AtomicBool::new(true)),
            lifecycle: Mutex::new(()),
            next_lease: AtomicU64::new(2),
            active_lease: AtomicU64::new(1),
        });
        drop(AgentBackend {
            client: Arc::clone(&client),
            lease: 1,
            layout: test_layout(),
        });

        let command = receiver.try_recv().unwrap();
        assert!(matches!(command.request, AgentRequest::Stop));
        assert!(command.response.is_none());
        assert_eq!(client.active_lease.load(Ordering::Acquire), 0);
        assert!(client.alive.load(Ordering::Acquire));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn backend_layout_and_health_use_local_state_without_queueing_requests() {
        let (commands, mut receiver) = mpsc::channel(1);
        let alive = Arc::new(AtomicBool::new(true));
        let client = Arc::new(AgentClient {
            commands,
            context: Arc::new(Mutex::new(None)),
            alive: Arc::clone(&alive),
            lifecycle: Mutex::new(()),
            next_lease: AtomicU64::new(2),
            active_lease: AtomicU64::new(1),
        });
        let backend = AgentBackend {
            client,
            lease: 1,
            layout: test_layout(),
        };

        assert!(backend.health_check().is_ok());
        assert_eq!(backend.layout().unwrap(), test_layout());
        assert!(receiver.try_recv().is_err());
        alive.store(false, Ordering::Release);
        assert!(backend.health_check().is_err());
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn client_heartbeat_uses_the_ordered_command_queue() {
        let (commands, mut receiver) = mpsc::channel(1);
        let alive = Arc::new(AtomicBool::new(true));
        let heartbeat_task = spawn_client_heartbeat(commands, Arc::clone(&alive));

        let command = time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(command.request, AgentRequest::Health));
        assert!(command.dispatched.is_none());
        assert!(command.response.is_none());

        alive.store(false, Ordering::Release);
        heartbeat_task.abort();
        let _ = heartbeat_task.await;
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
    fn agent_heartbeat_observation_extends_the_deadline() {
        let started = Instant::now();
        let observed = started + AGENT_HEARTBEAT_TIMEOUT;
        let mut heartbeat = AgentHeartbeat::new(started);
        heartbeat.observe(observed);

        assert!(!heartbeat.expired(observed + AGENT_HEARTBEAT_TIMEOUT));
    }

    #[test]
    fn response_timeout_begins_after_pipe_dispatch() {
        let alive = Arc::new(AtomicBool::new(true));
        let worker_alive = Arc::clone(&alive);
        let (dispatch_tx, dispatch_rx) = std::sync::mpsc::sync_channel(1);
        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            wait_for_agent_response(
                &worker_alive,
                "ReleaseAll",
                dispatch_rx,
                response_rx,
                Duration::from_secs(1),
                Duration::from_millis(80),
            )
        });

        std::thread::sleep(Duration::from_millis(120));
        dispatch_tx.send(()).unwrap();
        response_tx.send(Ok(AgentResponse::Ok)).unwrap();

        assert!(matches!(worker.join().unwrap().unwrap(), AgentResponse::Ok));
        assert!(alive.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn packet_reader_preserves_fragmented_frames() {
        let (mut writer, reader) = tokio::io::duplex(4096);
        let packets = [
            AgentPacket::Request {
                id: 11,
                expects_response: false,
                request: AgentRequest::Health,
            },
            AgentPacket::Request {
                id: 12,
                expects_response: true,
                request: AgentRequest::CursorPosition,
            },
        ];
        let writer_task = tokio::spawn(async move {
            for packet in packets {
                let bytes = bincode::serialize(&packet).unwrap();
                let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
                frame.extend_from_slice(&bytes);
                for byte in frame {
                    writer.write_all(&[byte]).await.unwrap();
                    tokio::task::yield_now().await;
                }
            }
        });
        let (mut packets, reader_task) = spawn_packet_reader(reader);

        assert!(matches!(
            packets.recv().await.unwrap().unwrap(),
            AgentPacket::Request {
                id: 11,
                expects_response: false,
                request: AgentRequest::Health,
            }
        ));
        assert!(matches!(
            packets.recv().await.unwrap().unwrap(),
            AgentPacket::Request {
                id: 12,
                expects_response: true,
                request: AgentRequest::CursorPosition,
            }
        ));

        writer_task.await.unwrap();
        reader_task.abort();
        let _ = reader_task.await;
    }

    #[test]
    fn fire_and_forget_success_does_not_create_response() {
        assert!(agent_completion_packet(7, false, Ok(AgentResponse::Ok)).is_none());
    }

    #[test]
    fn fire_and_forget_failure_is_forwarded_as_native_failure() {
        assert!(matches!(
            agent_completion_packet(8, false, Err(anyhow!("inject failed"))),
            Some(AgentPacket::Event(NativeEvent::Failed(message)))
                if message.contains("inject failed")
        ));
    }

    #[test]
    fn reliable_request_keeps_its_response_id() {
        assert!(matches!(
            agent_completion_packet(9, true, Ok(AgentResponse::Pong)),
            Some(AgentPacket::Response {
                id: 9,
                response: AgentResponse::Pong,
            })
        ));
    }
}
