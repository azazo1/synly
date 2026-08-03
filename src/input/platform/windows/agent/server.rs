use super::pipe::{NativePipe, PipeDirection};
use super::protocol::{
    AgentRequest, AgentResponse, AgentToGuiPacket, GuiToAgentPacket, is_timeout_error, read_packet,
    write_packet,
};
use super::security::{
    current_process_is_system, process_session_id, validate_parent_process, validate_pipe_server,
};
use super::{AGENT_HEARTBEAT_TIMEOUT, CONNECT_TIMEOUT, REQUEST_DELIVERY_TIMEOUT};
use super::super::super::{CaptureContext, InputBackend, MotionAccumulator};
use crate::input::{Hotkey, InputMode, Point};
use anyhow::{Context, Result, anyhow, bail};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::{self, MissedTickBehavior};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;

#[derive(Clone, Copy, Debug)]
pub(super) struct AgentMotion {
    pub(super) dx: i32,
    pub(super) dy: i32,
    pub(super) position: Option<Point>,
    pub(super) position_updated: bool,
}

#[derive(Clone)]
pub(super) struct AgentOutput {
    pub(super) reliable: std::sync::mpsc::SyncSender<AgentToGuiPacket>,
    pub(super) motion: Arc<AgentMotionSlot>,
}

pub(super) struct AgentMotionSlot {
    pub(super) latest: Mutex<Option<AgentMotion>>,
    pub(super) changed: AtomicBool,
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

pub(super) struct AgentHeartbeat {
    last_seen: Instant,
}

impl AgentHeartbeat {
    pub(super) fn new(now: Instant) -> Self {
        Self { last_seen: now }
    }

    fn observe(&mut self, now: Instant) {
        self.last_seen = now;
    }

    pub(super) fn expired(&self, now: Instant) -> bool {
        now.duration_since(self.last_seen) > AGENT_HEARTBEAT_TIMEOUT
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
    pub(super) fn send_reliable(&self, packet: AgentToGuiPacket) -> Result<()> {
        self.reliable
            .send(packet)
            .map_err(|_| anyhow!("Windows input agent event queue closed"))
    }

    pub(super) fn store_motion(&self, motion: AgentMotion) {
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

fn start_agent_transport(
    command_pipe_name: String,
    event_pipe_name: String,
    token: String,
    parent_pid: u32,
    agent_is_system: bool,
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
                agent_is_system,
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
        let GuiToAgentPacket::HelloAck { session_id } = ack
        else {
            bail!("Windows input agent received an invalid handshake response");
        };
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
    agent_is_system: bool,
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
            token,
            agent_pid: unsafe { GetCurrentProcessId() },
            parent_pid,
            agent_path,
            agent_is_system,
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
    let agent_is_system = current_process_is_system()?;
    tracing::info!(
        pid = unsafe { GetCurrentProcessId() },
        agent_is_system,
        "Windows 输入代理进程已启动"
    );
    let transport = start_agent_transport(
        command_pipe_name,
        event_pipe_name,
        token,
        parent_pid,
        agent_is_system,
    )?;
    let result = run_agent_loop(
        transport.requests,
        transport.output.clone(),
        Arc::clone(&transport.alive),
        agent_is_system,
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
    let combined = result.and(command_result).and(event_result);
    if let Err(error) = &combined {
        tracing::error!(error = %error, "Windows 输入代理已退出");
    } else {
        tracing::info!("Windows 输入代理已退出");
    }
    combined
}

fn agent_completion_packet(id: u64, response: Result<AgentResponse>) -> AgentToGuiPacket {
    AgentToGuiPacket::Response {
        id,
        response: response
            .unwrap_or_else(|error| AgentResponse::Error(format!("{error:#}"))),
    }
}

async fn run_agent_loop(
    mut packets: mpsc::Receiver<GuiToAgentPacket>,
    output: AgentOutput,
    alive: Arc<AtomicBool>,
    agent_is_system: bool,
) -> Result<()> {
    let mut runtime = None;
    let mut heartbeat = AgentHeartbeat::new(Instant::now());
    let mut heartbeat_stalled = false;
    let mut secure_desktop = false;
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
                    let request_name = request.name();
                    tracing::trace!(request = request_name, "Windows 输入代理收到请求");
                    let started = Instant::now();
                    let response = handle_agent_request(request, &mut runtime, output.clone()).await;
                    let completion = agent_completion_packet(id, response);
                    output.send_reliable(completion)?;
                    tracing::trace!(
                        request = request_name,
                        elapsed_ms = started.elapsed().as_millis(),
                        "Windows 输入代理请求处理完成"
                    );
                    heartbeat.observe(Instant::now());
                }
                _ = desktop_tick.tick() => {
                    if heartbeat.expired(Instant::now()) {
                        if !heartbeat_stalled {
                            heartbeat_stalled = true;
                            tracing::warn!(
                                "Windows 输入代理 GUI 心跳超时, 保持连接等待恢复"
                            );
                        }
                    } else if heartbeat_stalled {
                        heartbeat_stalled = false;
                        tracing::info!("Windows 输入代理 GUI 心跳已恢复");
                    }
                    let current_secure = !super::super::desktop::current_input_desktop_is_default();
                    if current_secure != secure_desktop {
                        secure_desktop = current_secure;
                        super::super::desktop::set_input_desktop_secure(secure_desktop);
                        if secure_desktop
                            && let Some(runtime) = runtime.as_ref()
                        {
                            let _ = runtime.backend.release_all();
                            if !agent_is_system {
                                let _ = runtime.backend.set_capture(false);
                            }
                        }
                        let primary = if secure_desktop {
                            runtime
                                .as_ref()
                                .and_then(|runtime| runtime.backend.primary_rect())
                        } else {
                            None
                        };
                        output.send_reliable(AgentToGuiPacket::SecureDesktopChanged {
                            secure: secure_desktop,
                            primary,
                        })?;
                    }
                }
            }
        }
    }
    .await;
    alive.store(false, Ordering::Release);
    if let Err(error) = &result {
        tracing::error!(error = %error, "Windows 输入代理主循环已退出");
    }
    if let Some(runtime) = runtime {
        runtime.stop().await;
    }
    result
}

pub(super) fn agent_event_writer_loop(
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
            let secure_desktop =
                !super::super::desktop::current_input_desktop_is_default();
            let primary = if secure_desktop {
                agent_backend(runtime)?.primary_rect()
            } else {
                None
            };
            Ok(AgentResponse::Started {
                layout,
                secure_desktop,
                primary,
            })
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
        AgentRequest::InjectMotion { dx, dy } => {
            agent_backend(runtime)?.inject_motion(dx, dy)?;
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
    let backend = super::super::native::start(context)?;
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
