use crate::capabilities::CapabilityState;
use crate::crypto;
use crate::device::{DeviceConfig, TrustedDeviceConfig};
use crate::input::InputMode;
use crate::protocol::{
    ClipboardPayload, ControlMessage, DeviceIdentity, Frame, FrameReader, FrameWriter,
    PairAuthMethod, PairRequestPayload, PROTOCOL_VERSION, RuntimeCapabilities, SessionAgreement,
    TransferLimits,
};
use crate::settings::{AudioMode, ClipboardMode, FileSyncMode};
use crate::workspace::WorkspaceSummary;
use anyhow::{Context, Result, anyhow, bail};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream;
use uuid::Uuid;

const PAIRING_TIMEOUT: Duration = Duration::from_secs(90);
const TLS_UPGRADE_TIMEOUT: Duration = Duration::from_secs(15);
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(2);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(20);

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub device: DeviceConfig,
    pub trusted_devices: Vec<TrustedDeviceConfig>,
    pub transfer_limits: TransferLimits,
    pub clipboard_mode: ClipboardMode,
    pub instance_name: Option<String>,
    pub request_trust: bool,
}

#[derive(Clone, Debug)]
pub struct ClientTarget {
    pub addresses: Vec<Ipv4Addr>,
    pub port: u16,
    pub peer_device_id: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientState {
    Connecting,
    Pairing,
    Connected,
    Reconnecting,
}

#[derive(Clone, Debug)]
pub enum ClientEvent {
    StateChanged(ClientState),
    PinRequired {
        request_id: String,
        bootstrap_short: String,
        bootstrap_randomart: String,
        session_short: String,
        session_randomart: String,
    },
    PairingFailed {
        message: String,
    },
    Connected {
        remote: DeviceIdentity,
        agreement: SessionAgreement,
        remote_workspace: WorkspaceSummary,
    },
    ClipboardReceived(ClipboardPayload),
    Disconnected {
        message: String,
    },
    TrustEstablished(DeviceIdentity),
}

pub trait ClientListener: Send + Sync + 'static {
    fn on_event(&self, event: ClientEvent);
}

#[derive(Clone, Debug)]
pub enum ClientCommand {
    SubmitPin(String),
    CancelPin,
    SendClipboard(ClipboardPayload),
    SetClipboardMode(ClipboardMode),
    UpdateTrustedDevices(Vec<TrustedDeviceConfig>),
    Stop,
}

#[derive(Clone)]
pub struct ClientHandle {
    commands: mpsc::UnboundedSender<ClientCommand>,
    state: Arc<std::sync::Mutex<ClientState>>,
}

impl ClientHandle {
    pub fn submit_pin(&self, pin: &str) -> Result<()> {
        self.send(ClientCommand::SubmitPin(normalize_pin(pin)?))
    }

    pub fn cancel_pin(&self) -> Result<()> {
        self.send(ClientCommand::CancelPin)
    }

    pub fn send_clipboard(&self, payload: ClipboardPayload) -> Result<()> {
        self.send(ClientCommand::SendClipboard(payload))
    }

    pub fn set_clipboard_mode(&self, mode: ClipboardMode) -> Result<()> {
        self.send(ClientCommand::SetClipboardMode(mode))
    }

    pub fn update_trusted_devices(&self, devices: Vec<TrustedDeviceConfig>) -> Result<()> {
        self.send(ClientCommand::UpdateTrustedDevices(devices))
    }

    pub fn state(&self) -> ClientState {
        *self.state.lock().expect("client state poisoned")
    }

    pub fn stop(&self) -> Result<()> {
        self.send(ClientCommand::Stop)
    }

    fn send(&self, command: ClientCommand) -> Result<()> {
        self.commands
            .send(command)
            .map_err(|_| anyhow!("客户端任务已退出"))
    }
}

pub fn start_client(
    config: ClientConfig,
    target: ClientTarget,
    listener: Arc<dyn ClientListener>,
) -> Result<ClientHandle> {
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let state = Arc::new(std::sync::Mutex::new(ClientState::Connecting));
    let handle = ClientHandle {
        commands: command_tx,
        state: Arc::clone(&state),
    };
    let worker_state = Arc::clone(&state);
    tokio::spawn(async move {
        run_client_loop(config, target, listener, command_rx, worker_state).await;
    });
    Ok(handle)
}

async fn run_client_loop(
    mut config: ClientConfig,
    target: ClientTarget,
    listener: Arc<dyn ClientListener>,
    mut commands: mpsc::UnboundedReceiver<ClientCommand>,
    state: Arc<std::sync::Mutex<ClientState>>,
) {
    let mut reconnect_delay = RECONNECT_BASE_DELAY;
    loop {
        let result = connect_and_run(&mut config, &target, &listener, &mut commands, &state).await;
        if let Err(err) = result {
            listener.on_event(ClientEvent::Disconnected {
                message: format!("{err:#}"),
            });
        } else {
            listener.on_event(ClientEvent::Disconnected {
                message: "连接已关闭".to_string(),
            });
        }
        set_state(&state, ClientState::Reconnecting);
        listener.on_event(ClientEvent::StateChanged(ClientState::Reconnecting));

        let mut stopped = false;
        while !stopped {
            tokio::select! {
                _ = tokio::time::sleep(reconnect_delay) => break,
                command = commands.recv() => {
                    match command {
                        Some(ClientCommand::Stop) | None => {
                            stopped = true;
                        }
                        Some(ClientCommand::UpdateTrustedDevices(devices)) => {
                            config.trusted_devices = devices;
                        }
                        Some(ClientCommand::SetClipboardMode(mode)) => {
                            config.clipboard_mode = mode;
                        }
                        Some(command) => {
                            tracing::debug!(?command, "重连等待期间忽略非控制命令");
                        }
                    }
                }
            }
        }
        if stopped {
            break;
        }
        reconnect_delay = next_reconnect_delay(reconnect_delay);
    }
}

fn next_reconnect_delay(current: Duration) -> Duration {
    let next = current.as_secs().saturating_mul(2);
    Duration::from_secs(next.min(RECONNECT_MAX_DELAY.as_secs()))
}

fn set_state(state: &Arc<std::sync::Mutex<ClientState>>, next: ClientState) {
    *state.lock().expect("client state poisoned") = next;
}

async fn connect_and_run(
    config: &mut ClientConfig,
    target: &ClientTarget,
    listener: &Arc<dyn ClientListener>,
    commands: &mut mpsc::UnboundedReceiver<ClientCommand>,
    state: &Arc<std::sync::Mutex<ClientState>>,
) -> Result<()> {
    set_state(state, ClientState::Connecting);
    listener.on_event(ClientEvent::StateChanged(ClientState::Connecting));
    if let Some(trusted) = trusted_device_for_target(config, target) {
        let Some(socket) = connect_any(&target.addresses, target.port).await? else {
            bail!("目标设备没有可用地址");
        };
        match connect_trusted(config, socket, trusted).await {
            Ok(session) => {
                return run_session(config, listener, commands, state, session).await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "可信 mTLS 连接失败, 回退到 bootstrap 配对");
            }
        }
    }

    let Some(socket) = connect_any(&target.addresses, target.port).await? else {
        bail!("目标设备没有可用地址");
    };
    let session = connect_bootstrap(config, socket, target, listener, commands).await?;
    run_session(config, listener, commands, state, session).await
}

fn trusted_device_for_target<'a>(
    config: &'a ClientConfig,
    target: &ClientTarget,
) -> Option<&'a TrustedDeviceConfig> {
    target
        .peer_device_id
        .as_ref()
        .and_then(|device_id| {
            config
                .trusted_devices
                .iter()
                .find(|device| &device.device_id == device_id)
        })
        .or_else(|| {
            if target.peer_device_id.is_none() && config.trusted_devices.len() == 1 {
                config.trusted_devices.first()
            } else {
                None
            }
        })
}

async fn connect_any(addresses: &[Ipv4Addr], port: u16) -> Result<Option<TcpStream>> {
    if addresses.is_empty() {
        return Ok(None);
    }
    let mut attempts = tokio::task::JoinSet::new();
    for address in addresses.iter().copied() {
        attempts.spawn(async move { (address, connect_tcp(address, port).await) });
    }
    let mut failures = Vec::new();
    while let Some(result) = attempts.join_next().await {
        match result {
            Ok((address, Ok(socket))) => {
                attempts.abort_all();
                tracing::info!(%address, port, "TCP 连接成功");
                return Ok(Some(socket));
            }
            Ok((address, Err(err))) => {
                failures.push(format!("{address}:{port}: {err:#}"));
            }
            Err(err) => {
                failures.push(format!("连接任务异常: {err}"));
            }
        }
    }
    bail!("无法连接目标设备, 已尝试: {}", failures.join("; "))
}

async fn connect_tcp(address: Ipv4Addr, port: u16) -> Result<TcpStream> {
    let socket = tokio::time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect((address, port)))
        .await
        .map_err(|_| anyhow!("连接 {address}:{port} 超时"))?
        .with_context(|| format!("连接 {address}:{port} 失败"))?;
    socket.set_nodelay(true)?;
    Ok(socket)
}

async fn connect_trusted(
    config: &ClientConfig,
    socket: TcpStream,
    trusted: &TrustedDeviceConfig,
) -> Result<AuthenticatedSession> {
    let connector = crypto::build_client_connector(&config.device, &trusted.tls_root_certificate)?;
    let stream = connector.connect(crypto::server_name()?, socket).await?;
    complete_trusted_pairing(config, stream, trusted).await
}

async fn complete_trusted_pairing(
    config: &ClientConfig,
    mut stream: TlsStream<TcpStream>,
    trusted: &TrustedDeviceConfig,
) -> Result<AuthenticatedSession> {
    let request_id = Uuid::new_v4().to_string();
    let exporter = crypto::export_keying_material_from_client(&stream, &request_id)?;
    let payload = client_pair_request(config);
    let trusted_proof = crypto::sign_trusted_pair_auth(
        &exporter,
        config.device.identity_private_key()?,
        &request_id,
        &payload,
    )?;
    write_frame(
        &mut stream,
        config.transfer_limits,
        Frame::Control(ControlMessage::PairRequest {
            request_id: request_id.clone(),
            payload: payload.clone(),
            trusted_proof: Some(trusted_proof),
        }),
    )
    .await?;

    let reply = match read_frame(&mut stream, config.transfer_limits).await? {
        Frame::Control(message) => message,
        _ => bail!("对端在可信配对中发送了非控制消息"),
    };
    let (remote, remote_workspace, agreement) = match reply.clone() {
        ControlMessage::PairDecision {
            accepted,
            message,
            server,
            workspace,
            agreement,
            auth_method,
            server_trusts_client,
            proof,
            trust_established,
        } => {
            if auth_method != PairAuthMethod::TrustedDevice {
                bail!("对端以意外的认证方式回复可信配对");
            }
            crypto::verify_device_identity_material(&server)?;
            crypto::verify_device_identity(&server, &trusted.public_key)?;
            let decision = ControlMessage::PairDecision {
                accepted,
                message: message.clone(),
                server: server.clone(),
                workspace: workspace.clone(),
                agreement: agreement.clone(),
                auth_method,
                server_trusts_client,
                proof,
                trust_established,
            };
            crypto::verify_trusted_pair_decision(
                &decision,
                &exporter,
                &request_id,
                &trusted.public_key,
            )?;
            if !accepted {
                bail!("{}", message);
            }
            (server, workspace, agreement)
        }
        ControlMessage::Error { message } => bail!("{}", message),
        other => bail!("意外的可信配对响应: {other:?}"),
    };
    Ok(AuthenticatedSession {
        stream,
        remote,
        agreement,
        remote_workspace,
    })
}

async fn connect_bootstrap(
    config: &mut ClientConfig,
    mut socket: TcpStream,
    _target: &ClientTarget,
    listener: &Arc<dyn ClientListener>,
    commands: &mut mpsc::UnboundedReceiver<ClientCommand>,
) -> Result<AuthenticatedSession> {
    let client_bootstrap_key = crypto::generate_bootstrap_key_material()?;
    let client_bootstrap_public_key = client_bootstrap_key.public_key_encoded();
    let client_display = crypto::bootstrap_public_key_display(&client_bootstrap_public_key)?;
    tracing::info!(bootstrap = %client_display.short, "发起最小配对请求");

    write_frame(
        &mut socket,
        config.transfer_limits,
        Frame::Control(ControlMessage::BootstrapHello {
            protocol_version: PROTOCOL_VERSION,
            client_bootstrap_public_key: client_bootstrap_public_key.clone(),
            device_name: config.device.device_name.clone(),
        }),
    )
    .await?;

    let (request_id, server_bootstrap_public_key, server_pake_message) =
        match read_frame_with_timeout(&mut socket, PAIRING_TIMEOUT, config.transfer_limits).await? {
            Frame::Control(ControlMessage::BootstrapChallenge {
                request_id,
                server_bootstrap_public_key,
                server_pake_message,
            }) => (request_id, server_bootstrap_public_key, server_pake_message),
            Frame::Control(ControlMessage::Error { message }) => bail!("{}", message),
            other => bail!("意外的配对响应: {other:?}"),
        };
    let session_display = crypto::bootstrap_session_display(
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    )?;
    tracing::info!(session = %session_display.short, "收到配对会话核对图");

    listener.on_event(ClientEvent::StateChanged(ClientState::Pairing));
    listener.on_event(ClientEvent::PinRequired {
        request_id: request_id.clone(),
        bootstrap_short: client_display.short.clone(),
        bootstrap_randomart: client_display.randomart.clone(),
        session_short: session_display.short.clone(),
        session_randomart: session_display.randomart.clone(),
    });
    let pin = loop {
        match commands.recv().await {
            Some(ClientCommand::SubmitPin(pin)) => break normalize_pin(&pin)?,
            Some(ClientCommand::CancelPin) => bail!("用户取消了 PIN 配对"),
            Some(ClientCommand::Stop) | None => bail!("客户端已停止"),
            Some(ClientCommand::UpdateTrustedDevices(devices)) => {
                config.trusted_devices = devices;
            }
            Some(ClientCommand::SetClipboardMode(mode)) => {
                config.clipboard_mode = mode;
            }
            Some(command) => {
                tracing::debug!(?command, "配对阶段忽略剪贴板命令");
            }
        }
    };

    let (pake_state, client_pake_message) = crypto::start_bootstrap_pake_client(
        &pin,
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    )?;
    let pake_key = crypto::finish_bootstrap_pake(pake_state, &server_pake_message)?;
    let client_confirm = crypto::client_pake_confirm(
        &pake_key,
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    );
    write_frame(
        &mut socket,
        config.transfer_limits,
        Frame::Control(ControlMessage::BootstrapPake {
            request_id: request_id.clone(),
            client_pake_message,
            client_confirm,
        }),
    )
    .await?;

    match read_frame_with_timeout(&mut socket, PAIRING_TIMEOUT, config.transfer_limits).await? {
        Frame::Control(ControlMessage::BootstrapAck {
            request_id: incoming_request_id,
            server_confirm,
        }) if incoming_request_id == request_id => {
            crypto::verify_server_pake_confirm(
                &pake_key,
                &request_id,
                &client_bootstrap_public_key,
                &server_bootstrap_public_key,
                &server_confirm,
            )?;
        }
        Frame::Control(ControlMessage::BootstrapAck { .. }) => {
            bail!("对端返回了不匹配的配对确认");
        }
        Frame::Control(ControlMessage::Error { message }) => bail!("{}", message),
        other => bail!("意外的 PAKE 响应: {other:?}"),
    }

    let connector = crypto::build_bootstrap_client_connector(
        &request_id,
        &pake_key,
        client_bootstrap_key,
        &server_bootstrap_public_key,
    )?;
    let mut stream = tokio::time::timeout(
        TLS_UPGRADE_TIMEOUT,
        connector.connect(crypto::server_name()?, socket),
    )
    .await
    .map_err(|_| anyhow!("等待服务端切换到临时 mTLS 超时"))??;
    let exporter = crypto::export_keying_material_from_client(&stream, &request_id)?;
    let payload = client_pair_request(config);
    write_frame(
        &mut stream,
        config.transfer_limits,
        Frame::Control(ControlMessage::PairRequest {
            request_id: request_id.clone(),
            payload: payload.clone(),
            trusted_proof: None,
        }),
    )
    .await?;

    let reply = match read_frame_with_timeout(&mut stream, PAIRING_TIMEOUT, config.transfer_limits)
        .await?
    {
        Frame::Control(message) => message,
        _ => bail!("对端在配对阶段发送了非控制消息"),
    };
    let (remote, remote_workspace, agreement, server_trusts_client) = match &reply {
        ControlMessage::PairDecision {
            accepted,
            message,
            server,
            workspace,
            agreement,
            auth_method,
            server_trusts_client,
            ..
        } => {
            if *auth_method != PairAuthMethod::Pin {
                bail!("配对决策使用了非 PIN 认证方式");
            }
            crypto::verify_device_identity_material(server)?;
            crypto::verify_pair_decision(&reply, &exporter, &request_id, &pin)?;
            if !accepted {
                bail!("{}", message);
            }
            (
                server.clone(),
                workspace.clone(),
                agreement.clone(),
                *server_trusts_client,
            )
        }
        ControlMessage::Error { message } => bail!("{}", message),
        other => bail!("意外的配对响应: {other:?}"),
    };

    if server_trusts_client
        && !config
            .trusted_devices
            .iter()
            .any(|device| device.device_id == remote.device_id)
    {
        config.trusted_devices.push(TrustedDeviceConfig {
            device_id: remote.device_id,
            device_name: remote.device_name.clone(),
            public_key: remote.identity_public_key.clone(),
            tls_root_certificate: remote.tls_root_certificate.clone(),
            trusted_at_ms: unix_time_ms(),
            last_seen_ms: unix_time_ms(),
            successful_sessions: 1,
        });
        config
            .trusted_devices
            .sort_by_key(|device| device.device_id);
        listener.on_event(ClientEvent::TrustEstablished(remote.clone()));
    }

    Ok(AuthenticatedSession {
        stream,
        remote,
        agreement,
        remote_workspace,
    })
}

fn client_pair_request(config: &ClientConfig) -> PairRequestPayload {
    PairRequestPayload {
        protocol_version: PROTOCOL_VERSION,
        client: client_identity(config),
        workspace: client_workspace_summary(config.clipboard_mode),
        request_trust: config.request_trust,
    }
}

pub fn client_identity(config: &ClientConfig) -> DeviceIdentity {
    DeviceIdentity {
        device_id: config.device.device_id,
        device_name: config.device.device_name.clone(),
        instance_name: config.instance_name.clone(),
        identity_public_key: config
            .device
            .identity_public_key()
            .expect("device identity public key is missing")
            .to_string(),
        tls_root_certificate: crypto::device_tls_root_certificate(&config.device)
            .expect("device TLS root certificate generation failed"),
    }
}

pub fn client_workspace_summary(clipboard_mode: ClipboardMode) -> WorkspaceSummary {
    WorkspaceSummary {
        file_sync_mode: FileSyncMode::Off,
        send_description: None,
        send_layout: None,
        send_items: Vec::new(),
        receive_root: None,
        initial_sync: None,
        max_folder_depth: None,
        clipboard_mode,
        audio_mode: AudioMode::Off,
        input_mode: InputMode::Off,
    }
}

struct AuthenticatedSession {
    stream: TlsStream<TcpStream>,
    remote: DeviceIdentity,
    agreement: SessionAgreement,
    remote_workspace: WorkspaceSummary,
}

async fn run_session(
    config: &mut ClientConfig,
    listener: &Arc<dyn ClientListener>,
    commands: &mut mpsc::UnboundedReceiver<ClientCommand>,
    state: &Arc<std::sync::Mutex<ClientState>>,
    session: AuthenticatedSession,
) -> Result<()> {
    set_state(state, ClientState::Connected);
    listener.on_event(ClientEvent::Connected {
        remote: session.remote.clone(),
        agreement: session.agreement.clone(),
        remote_workspace: session.remote_workspace.clone(),
    });
    tracing::info!(
        peer = %session.remote.device_name,
        "同步会话已开始"
    );

    let local_capabilities = RuntimeCapabilities {
        clipboard_mode: config.clipboard_mode,
        audio_mode: AudioMode::Off,
        input_mode: InputMode::Off,
    };
    let remote_capabilities = RuntimeCapabilities {
        clipboard_mode: session.remote_workspace.clipboard_mode,
        audio_mode: session.remote_workspace.audio_mode,
        input_mode: session.remote_workspace.input_mode,
    };
    let mut capability_state = CapabilityState::new(false, local_capabilities, remote_capabilities);
    let can_send = session.agreement.client_to_host;
    let can_receive = session.agreement.host_to_client;
    if can_send || can_receive {
        tracing::info!(
            send = can_send,
            receive = can_receive,
            "剪贴板同步方向已协商"
        );
    }

    let (read_half, write_half) = tokio::io::split(session.stream);
    let (frame_tx, frame_rx) = mpsc::channel::<Frame>(64);
    let mut writer_task = tokio::spawn(writer_loop(write_half, frame_rx, config.transfer_limits));
    let (incoming_tx, mut incoming_rx) = mpsc::unbounded_channel::<Frame>();
    let mut reader_task = tokio::spawn(reader_loop(read_half, incoming_tx, config.transfer_limits));

    let mut initial_update = capability_state.set_local(local_capabilities);
    if initial_update.is_none() {
        initial_update = Some((0, local_capabilities));
    }
    if let Some((generation, capabilities)) = initial_update {
        frame_tx
            .send(Frame::Control(ControlMessage::CapabilitiesUpdate {
                generation,
                capabilities,
            }))
            .await?;
    }

    let mut running = true;
    while running {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(ClientCommand::SendClipboard(payload)) => {
                        if can_send && !payload.is_empty() {
                            frame_tx.send(Frame::Clipboard(payload)).await?;
                        } else if !payload.is_empty() {
                            tracing::debug!("当前会话不允许发送剪贴板");
                        }
                    }
                    Some(ClientCommand::SetClipboardMode(mode)) => {
                        config.clipboard_mode = mode;
                        if let Some((generation, capabilities)) =
                            capability_state.set_local(RuntimeCapabilities {
                                clipboard_mode: mode,
                                audio_mode: AudioMode::Off,
                                input_mode: InputMode::Off,
                            })
                        {
                            frame_tx
                                .send(Frame::Control(ControlMessage::CapabilitiesUpdate {
                                    generation,
                                    capabilities,
                                }))
                                .await?;
                        }
                    }
                    Some(ClientCommand::UpdateTrustedDevices(devices)) => {
                        config.trusted_devices = devices;
                    }
                    Some(ClientCommand::Stop) | None => {
                        let _ = frame_tx.send(Frame::Control(ControlMessage::Goodbye)).await;
                        running = false;
                    }
                    Some(command) => {
                        tracing::debug!(?command, "会话阶段忽略不适用命令");
                    }
                }
            }
            frame = incoming_rx.recv() => {
                let Some(frame) = frame else {
                    bail!("与对端的连接已关闭");
                };
                match frame {
                    Frame::Control(ControlMessage::CapabilitiesUpdate { generation, capabilities }) => {
                        match capability_state.apply_remote(generation, capabilities) {
                            Ok(true) => {
                                frame_tx
                                    .send(Frame::Control(ControlMessage::CapabilitiesAck { generation }))
                                    .await?;
                            }
                            Ok(false) => {}
                            Err(err) => {
                                tracing::warn!(error = %err, "忽略对端能力更新");
                            }
                        }
                    }
                    Frame::Control(ControlMessage::CapabilitiesAck { generation }) => {
                        if let Err(err) = capability_state.apply_ack(generation) {
                            tracing::warn!(error = %err, "能力确认序号无效");
                        }
                    }
                    Frame::Clipboard(payload) => {
                        if can_receive {
                            listener.on_event(ClientEvent::ClipboardReceived(payload));
                        } else {
                            tracing::debug!("当前会话不允许接收剪贴板");
                        }
                    }
                    Frame::Control(ControlMessage::Error { message }) => {
                        bail!("对端报告错误: {message}");
                    }
                    Frame::Control(ControlMessage::Goodbye) => {
                        tracing::info!("对端已优雅关闭会话");
                        running = false;
                    }
                    Frame::Control(_) => {
                        tracing::debug!("会话阶段忽略其他控制消息");
                    }
                    Frame::FileChunk(_, _) => {
                        tracing::debug!("会话阶段忽略文件块");
                    }
                }
            }
            _ = &mut reader_task, if reader_task.is_finished() => {
                bail!("读取对端消息的任务已结束");
            }
            _ = &mut writer_task, if writer_task.is_finished() => {
                bail!("发送消息的任务已结束");
            }
        }
    }
    Ok(())
}

async fn writer_loop<W>(writer: W, mut rx: mpsc::Receiver<Frame>, transfer_limits: TransferLimits) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let mut writer = FrameWriter::with_limits(writer, transfer_limits);
    while let Some(frame) = rx.recv().await {
        writer.write_frame(frame).await?;
    }
    Ok(())
}

async fn reader_loop<R>(
    reader: R,
    tx: mpsc::UnboundedSender<Frame>,
    transfer_limits: TransferLimits,
) -> Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut reader = FrameReader::with_limits(reader, transfer_limits);
    loop {
        let frame = reader.read_frame().await?;
        if tx.send(frame).is_err() {
            return Ok(());
        }
    }
}

async fn read_frame<R>(reader: &mut R, transfer_limits: TransferLimits) -> Result<Frame>
where
    R: AsyncRead + Unpin,
{
    FrameReader::with_limits(reader, transfer_limits)
        .read_frame()
        .await
}

async fn read_frame_with_timeout<R>(
    reader: &mut R,
    timeout: Duration,
    transfer_limits: TransferLimits,
) -> Result<Frame>
where
    R: AsyncRead + Unpin,
{
    tokio::time::timeout(timeout, read_frame(reader, transfer_limits))
        .await
        .map_err(|_| anyhow!("等待对端响应超时"))?
}

async fn write_frame<W>(writer: &mut W, transfer_limits: TransferLimits, frame: Frame) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    FrameWriter::with_limits(writer, transfer_limits)
        .write_frame(frame)
        .await
}

pub fn normalize_pin(pin: &str) -> Result<String> {
    let trimmed = pin.trim();
    if trimmed.len() != 6 || !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("PIN 必须是 6 位数字");
    }
    Ok(trimmed.to_string())
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::normalize_pin;

    #[test]
    fn pin_must_be_six_digits() {
        assert!(normalize_pin("123456").is_ok());
        assert!(normalize_pin(" 123456 ").is_ok());
        assert!(normalize_pin("12345").is_err());
        assert!(normalize_pin("abcdef").is_err());
        assert!(normalize_pin("1234567").is_err());
    }
}
