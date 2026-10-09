use crate::capabilities::CapabilityState;
use crate::crypto;
use crate::device::{DeviceConfig, DiscoveryConfig, TrustedDeviceConfig};
use crate::discovery::{self, DiscoveredPeer};
use crate::input::InputMode;
use crate::protocol::{
    ClipboardPayload, ControlMessage, DeviceIdentity, Frame, FrameReader, FrameWriter,
    PROTOCOL_VERSION, PairAuthMethod, PairRequestPayload, RuntimeCapabilities, SessionAgreement,
    TransferLimits,
};
use crate::reconnect::{self, AttemptVerdict, ReconnectPolicy};
use crate::settings::{AudioMode, ClipboardMode};
use crate::transport::stream::{AsyncByteStream, ByteStream};
use crate::transport::clipboard_route::{ClipboardRoute, RouteContext as ClipboardRouteContext};
use crate::transport::routing::{AvailableLinks, PathPolicy};
use anyhow::{Context, Result, anyhow, bail};
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_rustls::client::TlsStream;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const PAIRING_TIMEOUT: Duration = Duration::from_secs(90);
const TLS_UPGRADE_TIMEOUT: Duration = Duration::from_secs(15);
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(2);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(20);
const REDISCOVER_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
struct PairingTerminal(anyhow::Error);

impl std::fmt::Display for PairingTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for PairingTerminal {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub device: DeviceConfig,
    pub trusted_devices: Vec<TrustedDeviceConfig>,
    pub transfer_limits: TransferLimits,
    pub clipboard_mode: ClipboardMode,
    pub clipboard_path: PathPolicy,
    pub instance_name: Option<String>,
    pub request_trust: bool,
    pub bluetooth_enabled: bool,
    pub discovery: Option<DiscoveryConfig>,
}

#[derive(Clone, Debug)]
pub struct ClientTarget {
    pub addresses: Vec<Ipv4Addr>,
    pub port: u16,
    pub peer_device_id: Option<Uuid>,
    /// 系统地址只是接入线索, 应用身份仍必须经过 TLS 签名与授权.
    pub bluetooth_address: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientState {
    Connecting,
    Pairing,
    Connected,
    Reconnecting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientTransportStatus {
    pub primary: crate::transport::routing::TransportKind, pub available: AvailableLinks,
    pub clipboard: Option<crate::transport::routing::TransportKind>, pub clipboard_choice: crate::transport::routing::RouteChoice,
    pub switching: bool, pub failed: bool,
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
    BluetoothAuthorizationRequired {
        request_id: String,
        request: crate::bluetooth::session::AuthorizationRequest,
    },
    PairingFailed {
        message: String,
    },
    Connected {
        remote: DeviceIdentity,
        clipboard_agreement: SessionAgreement,
        remote_capabilities: RuntimeCapabilities,
        remote_address: Option<Ipv4Addr>,
        remote_port: Option<u16>,
    },
    TransportChanged(ClientTransportStatus),
    ClipboardReceived(ClipboardPayload),
    ClipboardDelivery { delivery_id: Uuid, payload: ClipboardPayload },
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
    AuthorizeBluetooth { request_id: String, accepted: bool, remember: bool },
    SubmitPin(String),
    CancelPin,
    SendClipboard(ClipboardPayload),
    ConfirmClipboard { delivery_id: Uuid, success: bool },
    SetClipboardMode(ClipboardMode),
    SetClipboardPath(PathPolicy),
    UpdateTrustedDevices(Vec<TrustedDeviceConfig>),
    Stop,
}

#[derive(Clone)]
pub struct ClientHandle {
    commands: mpsc::UnboundedSender<ClientCommand>,
    state: Arc<std::sync::Mutex<ClientState>>,
    completion: CancellationToken,
    cancellation: CancellationToken,
}

impl ClientHandle {
    pub fn authorize_bluetooth(&self, request_id: String, accepted: bool, remember: bool) -> Result<()> {
        self.send(ClientCommand::AuthorizeBluetooth { request_id, accepted, remember })
    }

    pub fn submit_pin(&self, pin: &str) -> Result<()> {
        self.send(ClientCommand::SubmitPin(normalize_pin(pin)?))
    }

    pub fn cancel_pin(&self) -> Result<()> {
        self.send(ClientCommand::CancelPin)
    }

    pub fn send_clipboard(&self, payload: ClipboardPayload) -> Result<()> {
        self.send(ClientCommand::SendClipboard(payload))
    }

    /// 应用层实际写入成功后确认, 回调发出本身不代表应用成功.
    pub fn confirm_clipboard(&self, delivery_id: Uuid, success: bool) -> Result<()> { self.send(ClientCommand::ConfirmClipboard { delivery_id, success }) }

    pub fn set_clipboard_mode(&self, mode: ClipboardMode) -> Result<()> {
        self.send(ClientCommand::SetClipboardMode(mode))
    }

    pub fn set_clipboard_path(&self, policy: PathPolicy) -> Result<()> { self.send(ClientCommand::SetClipboardPath(policy)) }

    pub fn update_trusted_devices(&self, devices: Vec<TrustedDeviceConfig>) -> Result<()> {
        self.send(ClientCommand::UpdateTrustedDevices(devices))
    }

    pub fn state(&self) -> ClientState {
        *self.state.lock().expect("client state poisoned")
    }

    pub fn stop(&self) -> Result<()> {
        self.cancellation.cancel();
        self.send(ClientCommand::Stop)
    }

    pub async fn stop_and_wait(&self) -> Result<()> {
        self.cancellation.cancel();
        let _ = self.send(ClientCommand::Stop);
        // 队列关闭不等于资源清理完成. 持久信号支持多个等待者, 也不会漏掉先发生的完成.
        self.completion.cancelled().await;
        Ok(())
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
    let cancellation = CancellationToken::new();
    let handle = ClientHandle {
        commands: command_tx,
        state: Arc::clone(&state),
        completion: CancellationToken::new(),
        cancellation: cancellation.clone(),
    };
    let worker_state = Arc::clone(&state);
    // 在 spawn 前创建 guard, 工作任务未首次轮询就被丢弃时也必须通知完成.
    let completion_guard = handle.completion.clone().drop_guard();
    tokio::spawn(async move {
        let _completion_guard = completion_guard;
        run_client_loop(
            config,
            target,
            listener,
            command_rx,
            worker_state,
            cancellation,
        )
        .await;
    });
    Ok(handle)
}

async fn run_client_loop(
    mut config: ClientConfig,
    mut target: ClientTarget,
    listener: Arc<dyn ClientListener>,
    mut commands: mpsc::UnboundedReceiver<ClientCommand>,
    state: Arc<std::sync::Mutex<ClientState>>,
    cancellation: CancellationToken,
) {
    let policy = ReconnectPolicy::new(RECONNECT_BASE_DELAY, RECONNECT_MAX_DELAY);
    let shutdown = cancellation.clone();
    let mut attempt = ClientReconnectAttempt {
        config: &mut config,
        target: &mut target,
        listener: &listener,
        commands: &mut commands,
        state: &state,
        cancellation: &cancellation,
    };
    let result = reconnect::run_auto_reconnect(policy, shutdown, &mut attempt).await;
    if let Err(err) = result {
        tracing::debug!(error = %err, "客户端重连循环退出");
    }
}

struct ClientReconnectAttempt<'a> {
    config: &'a mut ClientConfig,
    target: &'a mut ClientTarget,
    listener: &'a Arc<dyn ClientListener>,
    commands: &'a mut mpsc::UnboundedReceiver<ClientCommand>,
    state: &'a Arc<std::sync::Mutex<ClientState>>,
    cancellation: &'a CancellationToken,
}

impl reconnect::ReconnectAttempt for ClientReconnectAttempt<'_> {
    fn attempt(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = reconnect::AttemptVerdict> + Send + '_>>
    {
        Box::pin(attempt_connect_once(
            self.config,
            self.target,
            self.listener,
            self.commands,
            self.state,
            self.cancellation,
        ))
    }
}

async fn attempt_connect_once(
    config: &mut ClientConfig,
    target: &mut ClientTarget,
    listener: &Arc<dyn ClientListener>,
    commands: &mut mpsc::UnboundedReceiver<ClientCommand>,
    state: &Arc<std::sync::Mutex<ClientState>>,
    cancellation: &CancellationToken,
) -> AttemptVerdict {
    let result = connect_and_run(config, target, listener, commands, state, cancellation).await;
    match result {
        Err(err) if err.downcast_ref::<PairingTerminal>().is_some() => {
            let message = format!("{err:#}");
            tracing::warn!(error = %message, "配对流程已终止, 不再自动重连");
            listener.on_event(ClientEvent::Disconnected {
                message: message.clone(),
            });
            listener.on_event(ClientEvent::PairingFailed { message });
            AttemptVerdict::Terminal(err)
        }
        Err(err) => {
            listener.on_event(ClientEvent::Disconnected {
                message: format!("{err:#}"),
            });
            set_state(state, ClientState::Reconnecting);
            listener.on_event(ClientEvent::StateChanged(ClientState::Reconnecting));
            AttemptVerdict::Failed
        }
        Ok(()) => {
            listener.on_event(ClientEvent::Disconnected {
                message: "连接已关闭".to_string(),
            });
            set_state(state, ClientState::Reconnecting);
            listener.on_event(ClientEvent::StateChanged(ClientState::Reconnecting));
            AttemptVerdict::Disconnected
        }
    }
}

fn set_state(state: &Arc<std::sync::Mutex<ClientState>>, next: ClientState) {
    *state.lock().expect("client state poisoned") = next;
}

async fn connect_and_run(
    config: &mut ClientConfig,
    target: &mut ClientTarget,
    listener: &Arc<dyn ClientListener>,
    commands: &mut mpsc::UnboundedReceiver<ClientCommand>,
    state: &Arc<std::sync::Mutex<ClientState>>,
    cancellation: &CancellationToken,
) -> Result<()> {
    set_state(state, ClientState::Connecting);
    listener.on_event(ClientEvent::StateChanged(ClientState::Connecting));
    if let Some(address) = target.bluetooth_address.clone() {
        let session = connect_bluetooth(config, target, &address, listener, commands, state, cancellation).await?;
        return run_session(config, listener, commands, state, session, cancellation).await;
    }
    if let Some(trusted) = trusted_device_for_target(config, target) {
        let Some(socket) = connect_with_rediscovery(config, target).await? else {
            bail!("目标设备没有可用地址");
        };
        match connect_trusted(config, socket, trusted).await {
            Ok(session) => {
                return run_session(config, listener, commands, state, session, cancellation).await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "可信 mTLS 连接失败, 回退到 bootstrap 配对");
            }
        }
    }

    let Some(socket) = connect_with_rediscovery(config, target).await? else {
        bail!("目标设备没有可用地址");
    };
    let session =
        connect_bootstrap(config, socket, target, listener, commands, cancellation).await?;
    run_session(config, listener, commands, state, session, cancellation).await
}

async fn connect_bluetooth(
    config: &mut ClientConfig, target: &mut ClientTarget, address: &str,
    listener: &Arc<dyn ClientListener>, commands: &mut mpsc::UnboundedReceiver<ClientCommand>,
    state: &Arc<std::sync::Mutex<ClientState>>, cancellation: &CancellationToken,
) -> Result<AuthenticatedSession> {
    use crate::bluetooth::{self, session::{self, AuthorizationDecision}};
    let connection = bluetooth::connect(address).await?;
    let expected = target.peer_device_id.and_then(|id| config.trusted_devices.iter().find(|peer| peer.device_id == id)).cloned();
    let expectation = expected.as_ref().map(|device| session::TrustedExpectation::One(device)).unwrap_or(session::TrustedExpectation::Interactive);
    let auth = session::AuthConfig { device: config.device.clone(), instance_name: config.instance_name.clone(),
        capabilities: client_capabilities(config.clipboard_mode), policies: Default::default(), trusted_devices: config.trusted_devices.clone(),
        request_trust: config.request_trust, trusted_only: false };
    let requested_peer = target.peer_device_id;
    let authorization_config = &mut *config;
    let request_commands = &mut *commands;
    let authenticated = session::connect(connection, &auth, expectation, move |request| async move {
        if requested_peer.is_some_and(|id| id != request.peer.device_id) { bail!("蓝牙应用身份与目标设备 ID 不一致"); }
        let request_id = Uuid::new_v4().to_string();
        set_state(state, ClientState::Pairing);
        listener.on_event(ClientEvent::StateChanged(ClientState::Pairing));
        listener.on_event(ClientEvent::BluetoothAuthorizationRequired { request_id: request_id.clone(), request });
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => bail!("客户端已停止"),
                command = request_commands.recv() => match command {
                    Some(ClientCommand::AuthorizeBluetooth { request_id: incoming, accepted, remember }) if incoming == request_id => return Ok(AuthorizationDecision { accepted, remember }),
                    Some(ClientCommand::Stop) | None => bail!("客户端已停止"),
                    Some(ClientCommand::UpdateTrustedDevices(devices)) => authorization_config.trusted_devices = devices,
                    Some(ClientCommand::SetClipboardPath(policy)) => authorization_config.clipboard_path = policy,
                    Some(ClientCommand::SetClipboardMode(mode)) => authorization_config.clipboard_mode = mode,
                    Some(_) => tracing::debug!("蓝牙授权期间忽略不匹配或过期命令"),
                }
            }
        }
    }).await.map_err(|error| anyhow!(PairingTerminal(error)))?;
    let remote = authenticated.remote;
    target.peer_device_id = Some(remote.device_id);
    if authenticated.remember_peer {
        config.trusted_devices.retain(|peer| peer.device_id != remote.device_id);
        config.trusted_devices.push(TrustedDeviceConfig { device_id: remote.device_id, device_name: remote.device_name.clone(), public_key: remote.identity_public_key.clone(),
            tls_root_certificate: remote.tls_root_certificate.clone(), trusted_at_ms: unix_time_ms(), last_seen_ms: unix_time_ms(), successful_sessions: 1 });
        listener.on_event(ClientEvent::TrustEstablished(remote.clone()));
    }
    let clipboard_agreement = SessionAgreement {
        client_to_host: config.clipboard_mode.can_send() && authenticated.capabilities.clipboard_mode.can_receive(),
        host_to_client: config.clipboard_mode.can_receive() && authenticated.capabilities.clipboard_mode.can_send(),
    };
    let keys = crate::transport::logical::SessionKeys::bluetooth(&authenticated.stream, authenticated.session_id, authenticated.link_master_secret)?;
    let logical = keys.logical(client_identity(config), remote.clone(), crate::transport::routing::TransportKind::Bluetooth)?;
    Ok(AuthenticatedSession { stream: ByteStream::new(authenticated.stream), transport: crate::transport::routing::TransportKind::Bluetooth, logical, remote_socket: None, remote, clipboard_agreement, remote_capabilities: authenticated.capabilities })
}

async fn connect_with_rediscovery(
    config: &ClientConfig,
    target: &mut ClientTarget,
) -> Result<Option<TcpStream>> {
    match connect_any(&target.addresses, target.port).await {
        Ok(Some(socket)) => {
            prioritize_connected_address(target, &socket);
            return Ok(Some(socket));
        }
        Ok(None) => {}
        Err(original) => {
            if refresh_target_addresses(target, &config.discovery).await {
                let socket = connect_any(&target.addresses, target.port).await?;
                if let Some(ref socket) = socket {
                    prioritize_connected_address(target, socket);
                }
                return Ok(socket);
            }
            return Err(original);
        }
    }
    if refresh_target_addresses(target, &config.discovery).await {
        let socket = connect_any(&target.addresses, target.port).await?;
        if let Some(ref socket) = socket {
            prioritize_connected_address(target, socket);
        }
        return Ok(socket);
    }
    Ok(None)
}

fn prioritize_connected_address(target: &mut ClientTarget, socket: &TcpStream) {
    let Ok(std::net::SocketAddr::V4(remote)) = socket.peer_addr() else {
        return;
    };
    let address = *remote.ip();
    if let Some(index) = target
        .addresses
        .iter()
        .position(|candidate| *candidate == address)
    {
        target.addresses.swap(0, index);
    } else {
        target.addresses.insert(0, address);
    }
}

async fn refresh_target_addresses(
    target: &mut ClientTarget,
    discovery: &Option<DiscoveryConfig>,
) -> bool {
    let Some(device_id) = target.peer_device_id else {
        return false;
    };
    let Some(config) = discovery.as_ref() else {
        return false;
    };
    let peers = match discovery::browse(REDISCOVER_TIMEOUT, config).await {
        Ok(peers) => peers,
        Err(err) => {
            tracing::warn!(error = %err, "按设备 ID 重新发现目标失败");
            return false;
        }
    };
    let Some(peer) = rediscovered_peer(&peers, device_id) else {
        tracing::info!(device_id = %device_id, "重新发现未找到目标设备");
        return false;
    };
    if peer.addresses.is_empty() {
        return false;
    }
    tracing::info!(
        device_id = %device_id,
        port = peer.port,
        addresses = ?peer.addresses,
        "已重新发现目标设备, 更新重连地址"
    );
    target.addresses.clone_from(&peer.addresses);
    target.port = peer.port;
    true
}

fn rediscovered_peer(peers: &[DiscoveredPeer], device_id: Uuid) -> Option<&DiscoveredPeer> {
    peers
        .iter()
        .find(|peer| Uuid::parse_str(&peer.device_id).ok() == Some(device_id))
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
    let remote_socket = socket.peer_addr().ok();
    let connector = crypto::build_client_connector(&config.device, &trusted.tls_root_certificate)?;
    let stream = connector.connect(crypto::server_name()?, socket).await?;
    complete_trusted_pairing(config, stream, trusted, remote_socket).await
}

async fn complete_trusted_pairing<T: AsyncByteStream + 'static>(
    config: &ClientConfig,
    mut stream: TlsStream<T>,
    trusted: &TrustedDeviceConfig,
    remote_socket: Option<std::net::SocketAddr>,
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
    let (remote, remote_capabilities, clipboard_agreement) = match reply.clone() {
        ControlMessage::PairDecision {
            accepted,
            message,
            server,
            capabilities,
            clipboard_agreement,
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
                capabilities,
                clipboard_agreement: clipboard_agreement.clone(),
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
            (server, capabilities, clipboard_agreement)
        }
        ControlMessage::Error { message } => bail!("{}", message),
        other => bail!("意外的可信配对响应: {other:?}"),
    };
    let keys = crate::transport::logical::SessionKeys::client(&stream, &request_id)?;
    let logical = keys.logical(client_identity(config), remote.clone(), crate::transport::routing::TransportKind::Lan)?;
    Ok(AuthenticatedSession {
        stream: ByteStream::new(stream),
        transport: crate::transport::routing::TransportKind::Lan,
        logical,
        remote_socket,
        remote,
        clipboard_agreement,
        remote_capabilities,
    })
}

async fn connect_bootstrap(
    config: &mut ClientConfig,
    socket: TcpStream,
    target: &ClientTarget,
    listener: &Arc<dyn ClientListener>,
    commands: &mut mpsc::UnboundedReceiver<ClientCommand>,
    cancellation: &CancellationToken,
) -> Result<AuthenticatedSession> {
    connect_bootstrap_inner(config, socket, target, listener, commands, cancellation)
        .await
        .map_err(|err| anyhow!(PairingTerminal(err)))
}

async fn connect_bootstrap_inner(
    config: &mut ClientConfig,
    mut socket: TcpStream,
    _target: &ClientTarget,
    listener: &Arc<dyn ClientListener>,
    commands: &mut mpsc::UnboundedReceiver<ClientCommand>,
    cancellation: &CancellationToken,
) -> Result<AuthenticatedSession> {
    let remote_socket = socket.peer_addr().ok();
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
    let pin = match tokio::time::timeout(PAIRING_TIMEOUT, async {
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => bail!("客户端已停止"),
                command = commands.recv() => {
                    match command {
                        Some(ClientCommand::SubmitPin(pin)) => return normalize_pin(&pin),
                        Some(ClientCommand::CancelPin) => bail!("用户取消了 PIN 配对"),
                        Some(ClientCommand::Stop) | None => bail!("客户端已停止"),
                        Some(ClientCommand::UpdateTrustedDevices(devices)) => {
                            config.trusted_devices = devices;
                        }
                        Some(ClientCommand::SetClipboardPath(policy)) => config.clipboard_path = policy,
                        Some(ClientCommand::SetClipboardMode(mode)) => {
                            config.clipboard_mode = mode;
                        }
                        Some(command) => {
                            tracing::debug!(?command, "配对阶段忽略剪贴板命令");
                        }
                    }
                }
            }
        }
    })
    .await
    {
        Ok(Ok(pin)) => pin,
        Ok(Err(err)) => return Err(err),
        Err(_) => bail!("等待用户输入 PIN 超时, 配对已终止"),
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
    let (remote, remote_capabilities, clipboard_agreement, server_trusts_client) =
        match &reply {
            ControlMessage::PairDecision {
                accepted,
                message,
                server,
                capabilities,
                clipboard_agreement,
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
                    *capabilities,
                    clipboard_agreement.clone(),
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

    let keys = crate::transport::logical::SessionKeys::client(&stream, &request_id)?;
    let logical = keys.logical(client_identity(config), remote.clone(), crate::transport::routing::TransportKind::Lan)?;
    Ok(AuthenticatedSession {
        stream: ByteStream::new(stream),
        transport: crate::transport::routing::TransportKind::Lan,
        logical,
        remote_socket,
        remote,
        clipboard_agreement,
        remote_capabilities,
    })
}

fn client_pair_request(config: &ClientConfig) -> PairRequestPayload {
    PairRequestPayload {
        protocol_version: PROTOCOL_VERSION,
        client: client_identity(config),
        capabilities: client_capabilities(config.clipboard_mode),
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

pub fn client_capabilities(clipboard_mode: ClipboardMode) -> RuntimeCapabilities {
    RuntimeCapabilities {
        clipboard_mode,
        audio_mode: AudioMode::Off,
        input_mode: InputMode::Off,
    }
}

fn active_identity_revoked(peer: &DeviceIdentity, previous: &[TrustedDeviceConfig], next: &[TrustedDeviceConfig]) -> bool {
    let matches = |known: &TrustedDeviceConfig| known.device_id == peer.device_id && crypto::public_keys_match(&known.public_key, &peer.identity_public_key);
    previous.iter().any(matches) && !next.iter().any(matches)
}

struct AuthenticatedSession {
    // 只在应用认证完成后擦除 TLS 承载类型, 业务会话不依赖 TCP.
    stream: ByteStream,
    transport: crate::transport::routing::TransportKind,
    logical: crate::transport::logical::LogicalSession,
    remote_socket: Option<std::net::SocketAddr>,
    remote: DeviceIdentity,
    clipboard_agreement: SessionAgreement,
    remote_capabilities: RuntimeCapabilities,
}

async fn run_session(
    config: &mut ClientConfig,
    listener: &Arc<dyn ClientListener>,
    commands: &mut mpsc::UnboundedReceiver<ClientCommand>,
    state: &Arc<std::sync::Mutex<ClientState>>,
    session: AuthenticatedSession,
    cancellation: &CancellationToken,
) -> Result<()> {
    set_state(state, ClientState::Connected);
    let remote_socket = session.remote_socket;
    let remote_address = remote_socket.and_then(|address| match address.ip() {
        std::net::IpAddr::V4(address) => Some(address),
        std::net::IpAddr::V6(_) => None,
    });
    let remote_port = remote_socket.map(|address| address.port());
    listener.on_event(ClientEvent::Connected {
        remote: session.remote.clone(),
        clipboard_agreement: session.clipboard_agreement.clone(),
        remote_capabilities: session.remote_capabilities,
        remote_address,
        remote_port,
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
        clipboard_mode: session.remote_capabilities.clipboard_mode,
        audio_mode: session.remote_capabilities.audio_mode,
        input_mode: session.remote_capabilities.input_mode,
    };
    let mut capability_state = CapabilityState::new(false, local_capabilities, remote_capabilities);
    let mut clipboard_delivery = crate::transport::clipboard::ClipboardInbox::default();
    let mut clipboard_pending = None;
    let can_send = session.clipboard_agreement.client_to_host;
    let can_receive = session.clipboard_agreement.host_to_client;
    if can_send || can_receive {
        tracing::info!(
            send = can_send,
            receive = can_receive,
            "剪贴板同步方向已协商"
        );
    }

    let _logical_owner = session.logical.owner()?;
    let mut secondary_inbox = None;
    let mut secondary: Option<crate::transport::logical::SecondaryTunnel> = None;
    let _candidate_task = if config.bluetooth_enabled && session.transport == crate::transport::routing::TransportKind::Lan {
        let auth = crate::bluetooth::session::AuthConfig { device: config.device.clone(), instance_name: config.instance_name.clone(), capabilities: local_capabilities,
            policies: Default::default(), trusted_devices: Vec::new(), request_trust: false, trusted_only: true };
        match crate::bluetooth::candidate::spawn_bluetooth(auth, session.logical.clone(), None, None) {
            Ok((inbox, task)) => { secondary_inbox = Some(inbox); Some(task) },
            Err(error) => { tracing::warn!(error = %error, "客户端蓝牙副承载初始化失败, 主会话继续运行"); None },
        }
    } else { None };
    let _lan_candidate_task = if session.transport == crate::transport::routing::TransportKind::Bluetooth {
        let auth = crate::bluetooth::session::AuthConfig { device: config.device.clone(), instance_name: config.instance_name.clone(), capabilities: local_capabilities,
            policies: Default::default(), trusted_devices: Vec::new(), request_trust: false, trusted_only: true };
        match crate::transport::lan_candidate::spawn_lan(auth, session.logical.clone(), config.discovery.clone().unwrap_or_default(), config.transfer_limits, None) {
            Ok((inbox, task)) => { secondary_inbox = Some(inbox); Some(task) },
            Err(error) => { tracing::warn!(error = %error, "客户端 LAN 副承载初始化失败, 蓝牙主会话继续运行"); None },
        }
    } else { None };
    let (stream, channels) = crate::transport::bluetooth::open(session.stream);
    let mut bluetooth_channels = Some(channels);
    let primary_clipboard_lane = bluetooth_channels.as_mut().expect("已创建主承载").enable_clipboard_routes()?;
    let (frame_tx, mut incoming_rx, frame_io) = crate::transport::frames::open_control(stream, config.transfer_limits);
    let (frame_tx, clipboard_sender_guard) = frame_tx.routed_clipboard();
    let mut clipboard_route = ClipboardRoute::new(false, config.transfer_limits);
    let mut remote_links = AvailableLinks::default(); let mut remote_transport_generation = 0;
    let mut remote_clipboard_policy = PathPolicy::Auto; let mut remote_input_policy = PathPolicy::LanOnly;

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
    let mut advertised_transport_state = None;
    let mut transport_generation = 0u64;
    let mut reported_transport_status = None;
    while running {
        let local_clipboard = capability_state.effective_local().clipboard_mode;
        let remote_clipboard = capability_state.effective_remote().clipboard_mode;
        frame_tx.set_clipboard_enabled(local_clipboard.can_send() && remote_clipboard.can_receive());
        let mut available = crate::transport::routing::AvailableLinks { lan: session.transport == crate::transport::routing::TransportKind::Lan, bluetooth: session.transport == crate::transport::routing::TransportKind::Bluetooth };
        if let Some(secondary) = &secondary { match secondary.transport() { crate::transport::routing::TransportKind::Lan => available.lan = true, crate::transport::routing::TransportKind::Bluetooth => available.bluetooth = true } }
        if advertised_transport_state != Some((available, config.clipboard_path)) {
            transport_generation = transport_generation.checked_add(1).context("传输状态代次已耗尽")?;
            frame_tx.send(Frame::Control(ControlMessage::TransportState { generation: transport_generation, available, input_policy: PathPolicy::LanOnly, clipboard_policy: config.clipboard_path })).await?;
            advertised_transport_state = Some((available, config.clipboard_path));
        }
        if remote_transport_generation > 0 {
            clipboard_route.reconcile(ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links, policy: config.clipboard_path, remote_policy: remote_clipboard_policy, tx: &frame_tx }).await?;
        }
        let status = ClientTransportStatus { primary: session.transport, available,
            clipboard: clipboard_route.transport(), clipboard_choice: clipboard_route.choice(ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links, policy: config.clipboard_path, remote_policy: remote_clipboard_policy, tx: &frame_tx }),
            switching: clipboard_route.switching(), failed: clipboard_route.failed() };
        if reported_transport_status != Some(status) { reported_transport_status = Some(status); listener.on_event(ClientEvent::TransportChanged(status)); }
        let clipboard_deadline = clipboard_route.deadline();
        tokio::select! {
            payload = clipboard_route.incoming() => {
                match payload {
                    Ok(transfer) => deliver_clipboard(transfer, &mut clipboard_delivery, &mut clipboard_pending, local_clipboard.can_receive() && remote_clipboard.can_send(), listener, &frame_tx).await?,
                    Err(error) => {
                        tracing::warn!(error = %error, "客户端剪贴板子流失败");
                        clipboard_route.fail(ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links, policy: config.clipboard_path, remote_policy: remote_clipboard_policy, tx: &frame_tx }, true).await?;
                    }
                }
            }
            _ = async { match clipboard_deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => {
                clipboard_route.fail(ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links, policy: config.clipboard_path, remote_policy: remote_clipboard_policy, tx: &frame_tx }, true).await?;
            }
            candidate = crate::transport::logical::receive_secondary(&mut secondary_inbox) => {
                match candidate {
                    Some(candidate) => {
                        if secondary.is_some() { bail!("客户端已存在副承载"); }
                        let mut tunnel = candidate.multiplex(); tunnel.channels.enable_clipboard_routes()?;
                        secondary = Some(tunnel);
                        tracing::info!(session = %session.logical.id(), "客户端副承载已加入, 控制路径保持主会话");
                    },
                    None => secondary_inbox = None,
                }
            }
            error = crate::transport::logical::secondary_failure(&mut secondary) => {
                secondary.take();
                tracing::warn!(%error, "客户端副承载已移除, 主会话继续运行");
            }
            command = commands.recv() => {
                match command {
                    Some(ClientCommand::SendClipboard(payload)) => {
                        if capability_state
                            .effective_local()
                            .clipboard_mode
                            .can_send()
                            && !payload.is_empty()
                        {
                            frame_tx.send(Frame::Clipboard(payload)).await?;
                        } else if !payload.is_empty() {
                            tracing::debug!("当前会话不允许发送剪贴板");
                        }
                    }
                    Some(ClientCommand::ConfirmClipboard { delivery_id, success }) => {
                        if let Some(stamp) = clipboard_pending.filter(|stamp: &crate::transport::clipboard::ClipboardStamp| stamp.id == delivery_id) {
                            clipboard_pending = None;
                            let receipt = if clipboard_delivery.finish(stamp, success) { ControlMessage::ClipboardApplied { stamp } } else { ControlMessage::ClipboardRejected { stamp } };
                            frame_tx.send(Frame::Control(receipt)).await?;
                        } else { tracing::debug!(%delivery_id, "忽略过期或重复的剪贴板应用回执"); }
                    }
                    Some(ClientCommand::SetClipboardPath(policy)) => config.clipboard_path = policy,
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
                        let revoked = active_identity_revoked(&session.remote, &config.trusted_devices, &devices);
                        config.trusted_devices = devices;
                        if revoked {
                            session.logical.close();
                            return Err(anyhow!(PairingTerminal(anyhow!("当前设备信任已撤销, 已关闭全部承载并停止自动重连"))));
                        }
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
                let frame = frame?;
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
                    Frame::Control(ControlMessage::TransportState { generation, available, input_policy, clipboard_policy }) => {
                        if generation == 0 || !available.contains(session.transport) { bail!("对端传输状态代次或主承载无效"); }
                        if generation == remote_transport_generation && (available != remote_links || input_policy != remote_input_policy || clipboard_policy != remote_clipboard_policy) { bail!("同一传输代次收到冲突状态"); }
                        if generation > remote_transport_generation { remote_transport_generation = generation; remote_links = available; remote_input_policy = input_policy; remote_clipboard_policy = clipboard_policy; }
                    }
                    Frame::Control(ControlMessage::ClipboardPath { epoch, generation, message }) => {
                        clipboard_route.receive(epoch, generation, message, ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links, policy: config.clipboard_path, remote_policy: remote_clipboard_policy, tx: &frame_tx }).await?;
                    }
                    Frame::Control(ControlMessage::ClipboardPathFailed { epoch, generation }) => {
                        clipboard_route.remote_failed(epoch, generation, ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links, policy: config.clipboard_path, remote_policy: remote_clipboard_policy, tx: &frame_tx }).await?;
                    }
                    Frame::Control(ControlMessage::ClipboardApplied { stamp }) => frame_tx.clipboard_receipt(stamp, true),
                    Frame::Control(ControlMessage::ClipboardRejected { stamp }) => frame_tx.clipboard_receipt(stamp, false),
                    Frame::ClipboardTransfer(_) => bail!("剪贴板载荷不能通过主控制子流发送"),
                    Frame::Clipboard(payload) => {
                        if capability_state
                            .effective_local()
                            .clipboard_mode
                            .can_receive()
                        {
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
                }
            }
            error = crate::transport::bluetooth::wait_failure(&mut bluetooth_channels) => {
                bail!("主承载复用失败: {error}");
            }
            _ = cancellation.cancelled() => {
                let _ = frame_tx.send(Frame::Control(ControlMessage::Goodbye)).await;
                running = false;
            }
        }
    }
    drop(clipboard_sender_guard);
    drop(frame_tx);
    frame_io.finish().await
}

async fn deliver_clipboard(transfer: crate::protocol::ClipboardTransfer, inbox: &mut crate::transport::clipboard::ClipboardInbox, pending: &mut Option<crate::transport::clipboard::ClipboardStamp>, allowed: bool, listener: &Arc<dyn ClientListener>, tx: &crate::transport::frames::FrameSender) -> Result<()> {
    use crate::transport::clipboard::ReceiveAction;
    transfer.validate()?; let stamp = transfer.stamp;
    let action = if allowed { inbox.begin(&transfer)? } else { ReceiveAction::Stale };
    match action {
        ReceiveAction::Apply => { *pending = Some(stamp); let payload = Arc::try_unwrap(transfer.payload).unwrap_or_else(|payload| (*payload).clone()); listener.on_event(ClientEvent::ClipboardDelivery { delivery_id: stamp.id, payload }); }
        ReceiveAction::Duplicate => tx.send(Frame::Control(ControlMessage::ClipboardApplied { stamp })).await?,
        ReceiveAction::InFlight => {},
        ReceiveAction::Busy | ReceiveAction::Stale => tx.send(Frame::Control(ControlMessage::ClipboardRejected { stamp })).await?,
    }
    Ok(())
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
    use super::{DiscoveredPeer, normalize_pin, rediscovered_peer};
    use crate::discovery::DiscoverySource;
    use crate::input::InputMode;
    use crate::settings::{AudioMode, ClipboardMode};
    use std::net::Ipv4Addr;
    use uuid::Uuid;

    #[tokio::test]
    async fn stop_waits_for_worker_exit_after_commands_close() {
        let (commands, receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(receiver);
        let completion = tokio_util::sync::CancellationToken::new();
        let handle = super::ClientHandle {
            commands, state: std::sync::Arc::new(std::sync::Mutex::new(super::ClientState::Connecting)),
            completion: completion.clone(), cancellation: tokio_util::sync::CancellationToken::new(),
        };
        let waiting = handle.stop_and_wait();
        tokio::pin!(waiting);
        // 队列接收端可能先于工作任务的资源清理被丢弃, 关闭队列不等于退出完成.
        tokio::select! { biased;
            result = &mut waiting => panic!("任务尚未完成却提前返回: {result:?}"),
            _ = std::future::ready(()) => {}
        }
        completion.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting).await.unwrap().unwrap();
    }

    fn lifecycle_client(listener: std::sync::Arc<dyn super::ClientListener>) -> super::ClientHandle {
        let config = super::ClientConfig {
            device: crate::identity::generate_device_config("退出测试".to_owned()).unwrap(),
            trusted_devices: Vec::new(), transfer_limits: crate::protocol::TransferLimits::default(),
            clipboard_mode: ClipboardMode::Both, clipboard_path: crate::transport::routing::PathPolicy::Auto,
            instance_name: None, request_trust: false, bluetooth_enabled: false, discovery: None,
        };
        super::start_client(config, super::ClientTarget { addresses: Vec::new(), port: 0, peer_device_id: None, bluetooth_address: None }, listener).unwrap()
    }

    struct ExitListener {
        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        cleanup: Option<tokio::sync::oneshot::Sender<()>>,
        panic: bool,
    }
    impl super::ClientListener for ExitListener {
        fn on_event(&self, _: super::ClientEvent) {
            if let Some(entered) = self.entered.lock().unwrap().take() { let _ = entered.send(()); }
            assert!(!self.panic, "模拟外部回调异常退出");
        }
    }
    impl Drop for ExitListener {
        fn drop(&mut self) { if let Some(cleanup) = self.cleanup.take() { let _ = cleanup.send(()); } }
    }

    #[tokio::test]
    async fn concurrent_and_late_stop_waiters_observe_completed_worker_cleanup() {
        let (cleanup, mut cleaned) = tokio::sync::oneshot::channel();
        let handle = lifecycle_client(std::sync::Arc::new(ExitListener { entered: std::sync::Mutex::new(None), cleanup: Some(cleanup), panic: false }));
        let second = handle.clone();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            let (first, other) = tokio::join!(handle.stop_and_wait(), second.stop_and_wait());
            first.unwrap(); other.unwrap();
            cleaned.try_recv().expect("完成信号必须发生在工作任务资源清理后");
            handle.stop_and_wait().await.unwrap();
        }).await.unwrap();
    }

    #[tokio::test]
    async fn stop_waiter_is_released_after_worker_callback_panics() {
        let (entered, started) = tokio::sync::oneshot::channel();
        let (cleanup, mut cleaned) = tokio::sync::oneshot::channel();
        let handle = lifecycle_client(std::sync::Arc::new(ExitListener { entered: std::sync::Mutex::new(Some(entered)), cleanup: Some(cleanup), panic: true }));
        tokio::time::timeout(std::time::Duration::from_secs(1), started).await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), handle.stop_and_wait()).await.unwrap().unwrap();
        cleaned.try_recv().expect("异常退出也必须先释放监听器资源");
    }

    #[test]
    fn removing_or_replacing_active_trust_revokes_only_that_identity() {
        use crate::{identity, crypto, protocol::DeviceIdentity, device::TrustedDeviceConfig};
        let device = identity::generate_device_config("peer".to_owned()).unwrap();
        let peer = DeviceIdentity { device_id: device.device_id, device_name: device.device_name.clone(), instance_name: None, identity_public_key: device.identity_public_key.clone(), tls_root_certificate: crypto::device_tls_root_certificate(&device).unwrap() };
        let known = TrustedDeviceConfig { device_id: peer.device_id, device_name: peer.device_name.clone(), public_key: peer.identity_public_key.clone(), tls_root_certificate: peer.tls_root_certificate.clone(), trusted_at_ms: 0, last_seen_ms: 0, successful_sessions: 0 };
        assert!(!super::active_identity_revoked(&peer, &[], &[]));
        assert!(!super::active_identity_revoked(&peer, std::slice::from_ref(&known), std::slice::from_ref(&known)));
        assert!(super::active_identity_revoked(&peer, std::slice::from_ref(&known), &[]));
        let mut changed = known.clone(); changed.public_key = identity::generate_device_config("changed".to_owned()).unwrap().identity_public_key;
        assert!(super::active_identity_revoked(&peer, &[known], &[changed]));
    }

    #[tokio::test]
    async fn trusted_pairing_supports_tls_over_a_non_tcp_stream_and_waits_for_application_receipt() {
        use crate::{crypto, identity, protocol::{ControlMessage, Frame, PairAuthMethod, SessionAgreement, TransferLimits}, transport::stream::ByteStream};
        use crate::device::TrustedDeviceConfig;
        let make_config = |name: &str| super::ClientConfig {
            device: identity::generate_device_config(name.to_owned()).unwrap(),
            trusted_devices: Vec::new(), transfer_limits: TransferLimits::default(),
            clipboard_mode: ClipboardMode::Both, clipboard_path: crate::transport::routing::PathPolicy::Auto, instance_name: None, request_trust: true, bluetooth_enabled: false, discovery: None,
        };
        let mut client = make_config("客户端"); let server = make_config("服务端");
        struct Listener(tokio::sync::mpsc::UnboundedSender<super::ClientEvent>);
        impl super::ClientListener for Listener { fn on_event(&self, event: super::ClientEvent) { let _ = self.0.send(event); } }
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let listener: std::sync::Arc<dyn super::ClientListener> = std::sync::Arc::new(Listener(events_tx));
        let (commands_tx, mut commands_rx) = tokio::sync::mpsc::unbounded_channel();
        let peer_commands_tx = commands_tx.clone();
        let server_identity = super::client_identity(&server);
        let trust = |config: &super::ClientConfig| TrustedDeviceConfig {
            device_id: config.device.device_id, device_name: config.device.device_name.clone(),
            public_key: config.device.identity_public_key.clone(), tls_root_certificate: crypto::device_tls_root_certificate(&config.device).unwrap(),
            trusted_at_ms: 0, last_seen_ms: 0, successful_sessions: 0,
        };
        let trusted_server = trust(&server);
        let acceptor = crypto::build_server_acceptor(&server.device, &[trust(&client)]).unwrap();
        let connector = crypto::build_client_connector(&client.device, &trusted_server.tls_root_certificate).unwrap();
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let mut stream = acceptor.accept(ByteStream::new(server_io)).await.unwrap();
            let Frame::Control(ControlMessage::PairRequest { request_id, payload, trusted_proof: Some(proof) }) = super::read_frame(&mut stream, server.transfer_limits).await.unwrap() else { panic!("缺少可信配对请求"); };
            let exporter = crypto::export_keying_material_from_server(&stream, &request_id).unwrap();
            crypto::verify_trusted_pair_auth(&exporter, &payload.client.identity_public_key, &request_id, &payload, &proof).unwrap();
            let agreement = SessionAgreement { host_to_client: true, client_to_host: true };
            let capabilities = super::client_capabilities(ClipboardMode::Both);
            let proof = crypto::sign_trusted_pair_decision(server.device.identity_private_key().unwrap(), &exporter, &request_id, true, "已授权", &server_identity, &agreement, &capabilities, true, false).unwrap();
            super::write_frame(&mut stream, server.transfer_limits, Frame::Control(ControlMessage::PairDecision { accepted: true, message: "已授权".to_owned(), server: server_identity, capabilities, clipboard_agreement: agreement, auth_method: PairAuthMethod::TrustedDevice, server_trusts_client: true, proof, trust_established: false })).await.unwrap();
            let (control, mut channels) = crate::transport::bluetooth::open(ByteStream::new(stream));
            let lane = channels.enable_clipboard_routes().unwrap();
            let (frame_tx, mut frames, _frame_io) = crate::transport::frames::open_control(control, server.transfer_limits);
            assert!(matches!(frames.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::CapabilitiesUpdate { generation: 0, .. })));
            assert!(matches!(frames.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::TransportState { .. })));
            use crate::{protocol::CapabilityEpoch, transport::routing::{AvailableLinks, TransportKind, RouteOffer, RouteMessage, FunctionalChannel, PathPolicy}};
            let epoch = CapabilityEpoch { host_generation: 0, client_generation: 0 }; let generation = Uuid::new_v4();
            let (clipboard_tx, mut clipboard_frames, _clipboard_io) = crate::transport::frames::open_clipboard(lane.lease(generation).unwrap(), server.transfer_limits);
            frame_tx.send_and_flush(Frame::Control(ControlMessage::TransportState { generation: 1, available: AvailableLinks { lan: true, bluetooth: false }, input_policy: PathPolicy::LanOnly, clipboard_policy: PathPolicy::Auto })).await.unwrap();
            frame_tx.send_and_flush(Frame::Control(ControlMessage::ClipboardPath { epoch, generation, message: RouteMessage::Offer(RouteOffer { channel: FunctionalChannel::Clipboard, epoch: 1, transport: Some(TransportKind::Lan) }) })).await.unwrap();
            assert!(matches!(frames.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::ClipboardPath { generation: actual, message: RouteMessage::Ready { .. }, .. }) if actual == generation));
            frame_tx.send_and_flush(Frame::Control(ControlMessage::ClipboardPath { epoch, generation, message: RouteMessage::Commit { channel: FunctionalChannel::Clipboard, epoch: 1 } })).await.unwrap();
            assert!(matches!(frames.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::ClipboardPath { generation: actual, message: RouteMessage::Committed { .. }, .. }) if actual == generation));
            let stamp = crate::transport::clipboard::ClipboardStamp { id: Uuid::new_v4(), sequence: 1 };
            let transfer = crate::protocol::ClipboardTransfer { stamp, route_epoch: 1, payload: std::sync::Arc::new(crate::protocol::ClipboardPayload { text: Some("应用前不能确认".to_owned()), rich_text: None, html: None, image: None, files: vec![] }) };
            clipboard_tx.send_and_flush(Frame::ClipboardTransfer(transfer.clone())).await.unwrap();
            loop { if let Some(super::ClientEvent::ClipboardDelivery { delivery_id, payload }) = events_rx.recv().await { assert_eq!(delivery_id, stamp.id); assert_eq!(payload, *transfer.payload); break; } }
            assert!(tokio::time::timeout(std::time::Duration::from_millis(20), frames.recv()).await.is_err());
            peer_commands_tx.send(super::ClientCommand::ConfirmClipboard { delivery_id: Uuid::new_v4(), success: true }).unwrap();
            assert!(tokio::time::timeout(std::time::Duration::from_millis(20), frames.recv()).await.is_err());
            peer_commands_tx.send(super::ClientCommand::ConfirmClipboard { delivery_id: stamp.id, success: true }).unwrap();
            assert!(matches!(frames.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::ClipboardApplied { stamp: got }) if got == stamp));
            // 对端应用确认丢失后的重试由核心去重, 不重复发出应用回调.
            clipboard_tx.send_and_flush(Frame::ClipboardTransfer(transfer)).await.unwrap();
            assert!(matches!(frames.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::ClipboardApplied { stamp: got }) if got == stamp));
            while let Ok(event) = events_rx.try_recv() { assert!(!matches!(event, super::ClientEvent::ClipboardDelivery { .. })); }
            let outbound = crate::protocol::ClipboardPayload { text: Some("客户端可靠发送".to_owned()), rich_text: None, html: None, image: None, files: vec![] };
            peer_commands_tx.send(super::ClientCommand::SendClipboard(outbound.clone())).unwrap();
            let Frame::ClipboardTransfer(sent) = clipboard_frames.recv().await.unwrap().unwrap() else { panic!("客户端应使用可靠发送入口") };
            assert_eq!(*sent.payload, outbound); assert_eq!(sent.route_epoch, 1); assert_eq!(sent.stamp.sequence, 1);
            frame_tx.send_and_flush(Frame::Control(ControlMessage::ClipboardApplied { stamp: sent.stamp })).await.unwrap();
            assert!(tokio::time::timeout(std::time::Duration::from_millis(20), frames.recv()).await.is_err());
            // 在线收紧策略仅撤销剪贴板 lease, 主控制身份和能力仍保持在线.
            peer_commands_tx.send(super::ClientCommand::SetClipboardPath(PathPolicy::BluetoothOnly)).unwrap();
            assert!(matches!(frames.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::TransportState { generation: 2, clipboard_policy: PathPolicy::BluetoothOnly, .. })));
            let retained = crate::protocol::ClipboardPayload { text: Some("策略暂停时保留最新载荷".to_owned()), rich_text: None, html: None, image: None, files: vec![] };
            peer_commands_tx.send(super::ClientCommand::SendClipboard(retained.clone())).unwrap();
            frame_tx.send_and_flush(Frame::Control(ControlMessage::CapabilitiesAck { generation: 0 })).await.unwrap();
            peer_commands_tx.send(super::ClientCommand::SetClipboardPath(PathPolicy::LanOnly)).unwrap();
            assert!(matches!(frames.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::TransportState { generation: 3, clipboard_policy: PathPolicy::LanOnly, .. })));
            drop(_clipboard_io); lane.revoke();
            let generation = Uuid::new_v4();
            let (_clipboard_tx, mut restored_frames, _restored_io) = crate::transport::frames::open_clipboard(lane.lease(generation).unwrap(), server.transfer_limits);
            frame_tx.send_and_flush(Frame::Control(ControlMessage::ClipboardPath { epoch, generation, message: RouteMessage::Offer(RouteOffer { channel: FunctionalChannel::Clipboard, epoch: 2, transport: Some(TransportKind::Lan) }) })).await.unwrap();
            assert!(matches!(frames.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::ClipboardPath { generation: actual, message: RouteMessage::Ready { .. }, .. }) if actual == generation));
            frame_tx.send_and_flush(Frame::Control(ControlMessage::ClipboardPath { epoch, generation, message: RouteMessage::Commit { channel: FunctionalChannel::Clipboard, epoch: 2 } })).await.unwrap();
            assert!(matches!(frames.recv().await.unwrap().unwrap(), Frame::Control(ControlMessage::ClipboardPath { generation: actual, message: RouteMessage::Committed { .. }, .. }) if actual == generation));
            let Frame::ClipboardTransfer(restored) = restored_frames.recv().await.unwrap().unwrap() else { panic!("新路径应发送暂停期间的最新载荷") };
            assert_eq!(*restored.payload, retained); assert_eq!(restored.route_epoch, 2); assert!(restored.stamp.sequence > sent.stamp.sequence);
            let mut paused = false; let mut committed = false; let mut observed = Vec::new();
            while let Ok(event) = events_rx.try_recv() {
                if let super::ClientEvent::TransportChanged(status) = event {
                    observed.push(status);
                    paused |= status.clipboard_choice == crate::transport::routing::RouteChoice::Paused(crate::transport::routing::PauseReason::TransportUnavailable);
                    committed |= status.clipboard == Some(TransportKind::Lan) && !status.switching;
                } else { assert!(!matches!(event, super::ClientEvent::Connected { .. })); }
            }
            // 发送任务与状态回调独立调度, 数据到达不代表状态事件已进入邮箱.
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while !paused || !committed {
                    let event = events_rx.recv().await.expect("状态回调仍在线");
                    if let super::ClientEvent::TransportChanged(status) = event {
                        observed.push(status);
                        paused |= status.clipboard_choice == crate::transport::routing::RouteChoice::Paused(crate::transport::routing::PauseReason::TransportUnavailable);
                        committed |= status.clipboard == Some(TransportKind::Lan) && !status.switching;
                    } else { assert!(!matches!(event, super::ClientEvent::Connected { .. })); }
                }
            }).await.unwrap_or_else(|_| panic!("核心应报告在线策略暂停与恢复后的实际路径: {observed:?}"));
            frame_tx.send_and_flush(Frame::Control(ControlMessage::Goodbye)).await.unwrap();
            // 复用子流 flush 不是 native/TLS 已交付屏障. 保持承载到客户端读到 Goodbye.
            finished_rx.await.unwrap();
        });
        let stream = connector.connect(crypto::server_name().unwrap(), ByteStream::new(client_io)).await.unwrap();
        let session = super::complete_trusted_pairing(&client, stream, &trusted_server, None).await.unwrap();
        assert_eq!(session.remote.device_id, trusted_server.device_id);
        assert!(session.remote_socket.is_none());
        let state = std::sync::Arc::new(std::sync::Mutex::new(super::ClientState::Connecting));
        tokio::time::timeout(std::time::Duration::from_secs(3), super::run_session(&mut client, &listener, &mut commands_rx, &state, session, &tokio_util::sync::CancellationToken::new())).await.unwrap().unwrap();
        let _ = finished_tx.send(());
        peer.await.unwrap();
        drop(commands_tx);
    }

    #[test]
    fn pin_must_be_six_digits() {
        assert!(normalize_pin("123456").is_ok());
        assert!(normalize_pin(" 123456 ").is_ok());
        assert!(normalize_pin("12345").is_err());
        assert!(normalize_pin("abcdef").is_err());
        assert!(normalize_pin("1234567").is_err());
    }

    #[test]
    fn rediscovered_peer_matches_full_device_id() {
        let device_id = Uuid::new_v4();
        let peers = vec![test_peer(device_id, vec![Ipv4Addr::LOCALHOST])];
        assert_eq!(
            rediscovered_peer(&peers, device_id).map(|peer| peer.device_id.as_str()),
            Some(device_id.to_string().as_str())
        );
    }

    #[test]
    fn rediscovered_peer_ignores_unrelated_devices() {
        let device_id = Uuid::new_v4();
        let peers = vec![test_peer(Uuid::new_v4(), vec![Ipv4Addr::LOCALHOST])];
        assert!(rediscovered_peer(&peers, device_id).is_none());
    }

    fn test_peer(device_id: Uuid, addresses: Vec<Ipv4Addr>) -> DiscoveredPeer {
        DiscoveredPeer {
            fullname: "test._synly._tcp.local.".to_string(),
            device_name: "测试设备".to_string(),
            instance_name: None,
            device_id: device_id.to_string(),
            protocol_version: 1,
            clipboard_mode: ClipboardMode::Both,
            audio_mode: AudioMode::Off,
            input_mode: InputMode::Off,
            source: DiscoverySource::Mdns,
            port: 42000,
            addresses,
        }
    }
}
