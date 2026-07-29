use super::platform::{CaptureContext, InputBackend, MotionAccumulator, NativeEvent};
use super::{DesktopLayout, Hotkey, InputMode, KeySnapshot, ModifierMask, Point};
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const IPC_VERSION: u16 = 1;
const IPC_MAX_FRAME: usize = 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(3);
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(500);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(2);

static AGENT: OnceLock<Mutex<Option<Arc<AgentClient>>>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize)]
enum AgentRequest {
    Start { mode: InputMode, hotkey: Hotkey },
    Stop,
    Health,
    Layout,
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
    Ping,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum AgentResponse {
    Ok,
    Pong,
    Layout(DesktopLayout),
    Point(Point),
    Snapshot(KeySnapshot),
    Error(String),
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
    Request {
        id: u64,
        request: AgentRequest,
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

struct ClientCommand {
    request: AgentRequest,
    response: std::sync::mpsc::SyncSender<Result<AgentResponse, String>>,
}

struct AgentClient {
    commands: mpsc::Sender<ClientCommand>,
    context: Arc<Mutex<Option<CaptureContext>>>,
    alive: Arc<AtomicBool>,
}

struct AgentBackend {
    client: Arc<AgentClient>,
}

struct NativeAgentRuntime {
    backend: Arc<dyn InputBackend>,
    task: tokio::task::JoinHandle<()>,
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

pub fn request_elevation() -> Result<()> {
    if let Some(client) = current_client()
        && client.request(AgentRequest::Health).is_ok()
    {
        return Ok(());
    }

    let pipe_name = format!(r"\\.\pipe\synly-input-{}", Uuid::new_v4());
    let token = Uuid::new_v4().to_string();
    let parent_pid = unsafe { GetCurrentProcessId() };
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

    let executable = agent_executable()?;
    validate_binary_signature(&std::env::current_exe()?)?;
    validate_binary_signature(&executable)?;
    launch_elevated(&executable, &pipe_name, &token, parent_pid)?;

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("synly-input-agent-client".to_string())
        .spawn(move || {
            let error_ready = ready_tx.clone();
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| {
                    runtime.block_on(connect_agent(server, token, parent_pid, ready_tx))
                });
            if let Err(error) = result {
                let _ = error_ready.send(Err(anyhow!(format!("{error:#}"))));
                tracing::error!(error = %error, "Windows 输入代理连接线程结束");
            }
        })
        .context("failed to start Windows input agent client thread")?;

    let client = ready_rx
        .recv_timeout(CONNECT_TIMEOUT)
        .context("等待 Windows 输入代理授权和连接超时")??;
    let slot = AGENT.get_or_init(|| Mutex::new(None));
    *slot.lock().map_err(|_| anyhow!("Windows input agent state poisoned"))? = Some(client);
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

pub(in crate::input) fn start_client(context: CaptureContext) -> Result<Arc<dyn InputBackend>> {
    ensure_ready(context.mode)?;
    let client = current_client().context("Windows 输入代理连接不可用")?;
    client.request(AgentRequest::Start {
        mode: context.mode,
        hotkey: context.hotkey,
    })?;
    *client
        .context
        .lock()
        .map_err(|_| anyhow!("Windows input agent context poisoned"))? = Some(context);
    Ok(Arc::new(AgentBackend { client }))
}

pub async fn run_agent(pipe_name: String, token: String, parent_pid: u32) -> Result<()> {
    validate_parent_process(parent_pid)?;
    let mut client = connect_pipe(&pipe_name).await?;
    validate_pipe_server(&client, parent_pid)?;
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
    let ack = read_packet(&mut client).await?;
    let AgentPacket::HelloAck {
        version,
        session_id,
    } = ack
    else {
        bail!("Windows input agent received an invalid handshake response");
    };
    if version != IPC_VERSION || session_id != process_session_id(parent_pid)? {
        bail!("Windows input agent handshake validation failed");
    }
    run_agent_loop(client).await
}

impl AgentClient {
    fn request(&self, request: AgentRequest) -> Result<AgentResponse> {
        if !self.alive.load(Ordering::Acquire) {
            bail!("Windows input agent connection is closed");
        }
        let (response_tx, response_rx) = std::sync::mpsc::sync_channel(1);
        self.commands
            .try_send(ClientCommand {
                request,
                response: response_tx,
            })
            .map_err(|error| anyhow!("Windows input agent command queue unavailable: {error}"))?;
        match response_rx.recv_timeout(REQUEST_TIMEOUT) {
            Ok(Ok(AgentResponse::Error(message))) => Err(anyhow!(message)),
            Ok(Ok(response)) => Ok(response),
            Ok(Err(message)) => Err(anyhow!(message)),
            Err(error) => Err(error).context("Windows input agent request timed out"),
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

impl InputBackend for AgentBackend {
    fn health_check(&self) -> Result<()> {
        self.client.request(AgentRequest::Health)?;
        Ok(())
    }

    fn layout(&self) -> Result<DesktopLayout> {
        match self.client.request(AgentRequest::Layout)? {
            AgentResponse::Layout(layout) => Ok(layout),
            _ => bail!("Windows input agent returned an invalid layout response"),
        }
    }

    fn cursor_position(&self) -> Result<Point> {
        match self.client.request(AgentRequest::CursorPosition)? {
            AgentResponse::Point(point) => Ok(point),
            _ => bail!("Windows input agent returned an invalid cursor response"),
        }
    }

    fn snapshot(&self) -> KeySnapshot {
        match self.client.request(AgentRequest::Snapshot) {
            Ok(AgentResponse::Snapshot(snapshot)) => snapshot,
            Ok(_) => KeySnapshot {
                usages: Vec::new(),
                modifiers: ModifierMask::default(),
                buttons: Vec::new(),
            },
            Err(error) => {
                self.client.emit_failure(&error);
                KeySnapshot {
                    usages: Vec::new(),
                    modifiers: ModifierMask::default(),
                    buttons: Vec::new(),
                }
            }
        }
    }

    fn set_capture(&self, active: bool) -> Result<()> {
        self.client.request(AgentRequest::SetCapture(active))?;
        Ok(())
    }

    fn warp_cursor(&self, point: Point) -> Result<()> {
        self.client.request(AgentRequest::WarpCursor(point))?;
        Ok(())
    }

    fn inject_key(
        &self,
        usage: u16,
        modifiers: ModifierMask,
        down: bool,
        repeat: bool,
    ) -> Result<()> {
        self.client.request(AgentRequest::InjectKey {
            usage,
            modifiers,
            down,
            repeat,
        })?;
        Ok(())
    }

    fn inject_button(&self, button: u8, down: bool) -> Result<()> {
        self.client
            .request(AgentRequest::InjectButton { button, down })?;
        Ok(())
    }

    fn inject_cursor(&self, point: Point) -> Result<()> {
        self.client.request(AgentRequest::InjectCursor(point))?;
        Ok(())
    }

    fn inject_wheel(&self, x: i32, y: i32) -> Result<()> {
        self.client.request(AgentRequest::InjectWheel { x, y })?;
        Ok(())
    }

    fn release_all(&self) -> Result<()> {
        self.client.request(AgentRequest::ReleaseAll)?;
        Ok(())
    }
}

impl Drop for AgentBackend {
    fn drop(&mut self) {
        let _ = self.client.request(AgentRequest::SetCapture(false));
        let _ = self.client.request(AgentRequest::ReleaseAll);
        let _ = self.client.request(AgentRequest::Stop);
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
    time::timeout(CONNECT_TIMEOUT, server.connect())
        .await
        .context("Windows input agent did not connect to the named pipe")??;
    let hello = read_packet(&mut server).await?;
    let AgentPacket::Hello {
        version,
        token: incoming_token,
        agent_pid,
        parent_pid: incoming_parent,
        agent_path,
    } = hello
    else {
        bail!("Windows input agent sent an invalid handshake");
    };
    if version != IPC_VERSION || incoming_token != token || incoming_parent != parent_pid {
        bail!("Windows input agent handshake token or version mismatch");
    }
    validate_pipe_client(&server, agent_pid, &agent_path)?;
    let session_id = process_session_id(parent_pid)?;
    write_packet(
        &mut server,
        &AgentPacket::HelloAck {
            version: IPC_VERSION,
            session_id,
        },
    )
    .await?;

    let (commands, command_rx) = mpsc::channel(64);
    let context = Arc::new(Mutex::new(None));
    let alive = Arc::new(AtomicBool::new(true));
    let client = Arc::new(AgentClient {
        commands,
        context: Arc::clone(&context),
        alive: Arc::clone(&alive),
    });
    ready
        .send(Ok(Arc::clone(&client)))
        .map_err(|_| anyhow!("Windows input agent readiness receiver closed"))?;
    client_loop(server, command_rx, context, alive).await
}

async fn client_loop(
    server: NamedPipeServer,
    mut commands: mpsc::Receiver<ClientCommand>,
    context: Arc<Mutex<Option<CaptureContext>>>,
    alive: Arc<AtomicBool>,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(server);
    let mut next_id = 1u64;
    let mut pending = HashMap::<
        u64,
        std::sync::mpsc::SyncSender<Result<AgentResponse, String>>,
    >::new();
    let mut heartbeat = time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_pong = Instant::now();
    let result = loop {
        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break Ok(()) };
                let id = next_id;
                next_id = next_id.saturating_add(1);
                write_packet(
                    &mut writer,
                    &AgentPacket::Request {
                        id,
                        request: command.request,
                    },
                )
                .await?;
                pending.insert(id, command.response);
            }
            packet = read_packet(&mut reader) => {
                match packet? {
                    AgentPacket::Response { id, response } => {
                        if id == 0 && matches!(&response, AgentResponse::Pong) {
                            last_pong = Instant::now();
                        } else if let Some(sender) = pending.remove(&id) {
                            let _ = sender.send(Ok(response));
                        }
                    }
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
                    _ => bail!("Windows input agent sent an unexpected packet"),
                }
            }
            _ = heartbeat.tick() => {
                if last_pong.elapsed() > HEARTBEAT_TIMEOUT {
                    break Err(anyhow!("Windows input agent heartbeat timed out"));
                }
                write_packet(
                    &mut writer,
                    &AgentPacket::Request {
                        id: 0,
                        request: AgentRequest::Ping,
                    },
                )
                .await?;
            }
        }
    };
    alive.store(false, Ordering::Release);
    for (_, sender) in pending {
        let _ = sender.send(Err("Windows input agent connection closed".to_string()));
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

async fn run_agent_loop(client: tokio::net::windows::named_pipe::NamedPipeClient) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(client);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<AgentPacket>(256);
    let mut runtime = None;
    let mut heartbeat = Instant::now();
    let mut paused = false;
    let mut desktop_tick = time::interval(Duration::from_millis(250));
    desktop_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            packet = read_packet(&mut reader) => {
                let AgentPacket::Request { id, request } = packet? else {
                    bail!("Windows input agent received an unexpected packet");
                };
                heartbeat = Instant::now();
                let response = handle_agent_request(request, &mut runtime, outgoing_tx.clone()).await;
                write_packet(
                    &mut writer,
                    &AgentPacket::Response {
                        id,
                        response: response.unwrap_or_else(|error| AgentResponse::Error(format!("{error:#}"))),
                    },
                )
                .await?;
            }
            outgoing = outgoing_rx.recv() => {
                let Some(outgoing) = outgoing else { break };
                write_packet(&mut writer, &outgoing).await?;
            }
            _ = desktop_tick.tick() => {
                if heartbeat.elapsed() > HEARTBEAT_TIMEOUT {
                    tracing::warn!("Windows 输入代理心跳超时");
                    break;
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
                    outgoing_tx
                        .send(AgentPacket::SecureDesktopPaused(paused))
                        .await?;
                }
            }
        }
    }
    if let Some(runtime) = runtime {
        runtime.stop().await;
    }
    Ok(())
}

async fn handle_agent_request(
    request: AgentRequest,
    runtime: &mut Option<NativeAgentRuntime>,
    outgoing: mpsc::Sender<AgentPacket>,
) -> Result<AgentResponse> {
    match request {
        AgentRequest::Start { mode, hotkey } => {
            if let Some(previous) = runtime.take() {
                previous.stop().await;
            }
            *runtime = Some(start_native_runtime(mode, hotkey, outgoing)?);
            Ok(AgentResponse::Ok)
        }
        AgentRequest::Stop => {
            if let Some(previous) = runtime.take() {
                previous.stop().await;
            }
            Ok(AgentResponse::Ok)
        }
        AgentRequest::Health | AgentRequest::Ping => Ok(AgentResponse::Pong),
        AgentRequest::Layout => Ok(AgentResponse::Layout(agent_backend(runtime)?.layout()?)),
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
    outgoing: mpsc::Sender<AgentPacket>,
) -> Result<NativeAgentRuntime> {
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
                    if outgoing.send(AgentPacket::Event(event)).await.is_err() {
                        break;
                    }
                }
                _ = motion_tick.tick() => {
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
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match ClientOptions::new().read(true).write(true).open(pipe_name) {
            Ok(client) => return Ok(client),
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(error = %error, "等待 GUI 命名管道就绪");
                time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error).context("failed to connect Windows input agent pipe"),
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
    let path = current.with_file_name("synly-input-agent.exe");
    if !path.is_file() {
        bail!("Windows input agent is missing: {}", path.display());
    }
    Ok(path)
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
        "--pipe \"{pipe_name}\" --token \"{token}\" --parent-pid {parent_pid}"
    ));
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            executable.as_ptr(),
            parameters.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
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
    let bytes = bincode::serialize(packet).context("failed to encode Windows input agent packet")?;
    if bytes.is_empty() || bytes.len() > IPC_MAX_FRAME {
        bail!("Windows input agent packet length is invalid");
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_packet<R>(reader: &mut R) -> Result<AgentPacket>
where
    R: AsyncRead + Unpin,
{
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > IPC_MAX_FRAME {
        bail!("Windows input agent packet length is invalid: {length}");
    }
    let mut bytes = vec![0u8; length];
    reader.read_exact(&mut bytes).await?;
    bincode::deserialize(&bytes).context("failed to decode Windows input agent packet")
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
