use synly_core::transport::logical::{LogicalSession, SessionKeys, CandidateExporter, BoundLink};
use synly_core::transport::routing::TransportKind;
use synly_core::transport::frames::FrameSender;
use synly_core::transport::clipboard_route::{ClipboardRoute, RouteContext as ClipboardRouteContext};

mod audio_path;
pub(crate) mod bluetooth;
mod clipboard_delivery;
mod input_route;
pub(crate) mod lan_admission;

use crate::audio::{self, AudioChannelDirection};
use crate::clipboard::{ClipboardSync, ClipboardWatcherHandle};
use crate::config::{DeviceConfig, SynlyConfig, TrustedDeviceConfig};
use crate::crypto;
use crate::discovery::{self, Advertisement, DiscoveredPeer, format_display_name};
use crate::host::clipboard_hub::ClipboardHubHandle;
use crate::host::session::InputRouteRegistry;
use crate::host::{
    ActiveSlotReserver, SessionCapabilityProfile, SlotReservation, runtime_options_for_profile,
};
use crate::input::{
    self, InputHostChannel, InputMode, InputRuntimeOptions, InputSessionContext,
    InputSocketConnection, InputSocketInbox, LocalInputRole, negotiate_input,
};
use crate::protocol::{
    AudioLayout as ProtocolAudioLayout, CapabilityEpoch, ClipboardPayload, ControlMessage,
    DeviceIdentity, Frame, FrameReader, FrameWriter, PROTOCOL_VERSION,
    PairAuthMethod, PairRequestPayload, RuntimeCapabilities, SessionAgreement, TransferLimits,
};
use crate::reconnect::{AttemptVerdict, ReconnectPolicy, run_auto_reconnect};
use crate::runtime_control::{
    InteractionRequest, InteractionResponse, RuntimeCommand, RuntimeControl, RuntimeEvent,
    RuntimeLifecycle, RuntimePeerSummary, RuntimeTuning,
};
use crate::runtime_options::{
    PairingRuntimeOptions, RuntimeOptions, normalize_pin, require_peer_query,
};
use crate::session::CapabilityState;
use crate::settings::{AudioMode, ClipboardMode, ConnectionPreference};
use crate::system_notification::{
    ConnectionEvent, NotificationPeer, SessionNotifier, SystemNotifier,
};
use synly_core::transport::stream::ByteStream;
use anyhow::{Context, Result, anyhow, bail};
use socket2::{SockRef, TcpKeepalive};
use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Instant};
use tokio_rustls::{TlsStream, client::TlsStream as ClientTlsStream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const PAIRING_TIMEOUT: Duration = Duration::from_secs(90);
const TLS_UPGRADE_TIMEOUT: Duration = Duration::from_secs(15);
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const PAIRING_FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const PAIRING_COOLDOWN: Duration = Duration::from_secs(3 * 60);
const PAIRING_MAX_FAILURES: u32 = 5;
const PAIRING_BACKOFF_BASE_MS: u64 = 1_000;
const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(2);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(20);
// 快速直连最多连续重试 3 次, 之后转入发现和正常退避.
const FAST_DIRECT_RETRIES: u32 = 3;
const CAPABILITY_ACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub(crate) enum SessionRole {
    Host,
    Client,
}

pub(crate) struct AuthenticatedSession {
    pub(crate) role: SessionRole,
    // TLS 认证和 exporter 提取完成后, 会话只依赖承载字节流.
    pub(crate) stream: ByteStream,
    pub(crate) require_existing_session: bool,
    pub(crate) trusted_reconnect: bool,
    pub(crate) transport: TransportKind,
    pub(crate) logical: LogicalSession,
    pub(crate) candidate_exporter: CandidateExporter,
    pub(crate) secondary_inbox: Option<mpsc::Receiver<BoundLink>>,
    pub(crate) remote: DeviceIdentity,
    pub(crate) remote_capabilities: RuntimeCapabilities,
    pub(crate) remote_socket_addr: Option<SocketAddr>,
    pub(crate) audio_master_secret: [u8; 32],
    pub(crate) input_master_secret: [u8; 32],
    pub(crate) capability_profile: SessionCapabilityProfile,
}

struct PairDecisionParams<'a> {
    exporter: &'a [u8],
    request_id: &'a str,
    accepted: bool,
    message: String,
    device: &'a DeviceConfig,
    instance_name: Option<&'a str>,
    clipboard_mode: ClipboardMode,
    audio_mode: AudioMode,
    input_mode: InputMode,
    clipboard_agreement: &'a SessionAgreement,
    auth_method: PairAuthMethod,
    pin: Option<&'a str>,
    server_trusts_client: bool,
    trust_established: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalAudioRole {
    Send,
    Receive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AudioPlan {
    role: LocalAudioRole,
    direction: AudioChannelDirection,
}

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

#[derive(Default)]
pub(crate) struct PairingThrottle {
    peers: HashMap<String, PairingPeerState>,
}

struct PairingPeerState {
    failures: u32,
    window_started_at: Instant,
    blocked_until: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PeerTarget {
    Discovered(DiscoveredPeer),
    Bluetooth { address: String, device_id: Option<Uuid> },
    Direct(SocketAddrV4),
}

impl PeerTarget {
    fn reconnect_query(&self) -> String {
        match self {
            Self::Discovered(peer) => preferred_peer_query(peer),
            Self::Direct(address) => address.to_string(),
            Self::Bluetooth { address, device_id } => device_id.map_or_else(|| format!("bluetooth:{address}"), |id| format!("bluetooth:{address}/{id}")),
        }
    }
}

pub async fn run(
    config: SynlyConfig,
    options: RuntimeOptions,
    commands: mpsc::UnboundedReceiver<RuntimeCommand>,
) -> Result<()> {
    match options.connection {
        ConnectionPreference::Host => {
            crate::host::run_host_runtime(config, options, commands).await
        }
        ConnectionPreference::Join => run_client(config, options).await,
    }
}

fn should_auto_accept_request(
    pairing: &PairingRuntimeOptions,
    auth_method: PairAuthMethod,
) -> bool {
    pairing.accept || auth_method == PairAuthMethod::TrustedDevice
}

fn accept_policy_label(pairing: &PairingRuntimeOptions) -> &'static str {
    if pairing.accept {
        "认证通过后自动接受"
    } else {
        "可信设备自动接受；未受信任设备认证通过后仍需本机确认"
    }
}

pub(crate) async fn run_advertisement_updates(
    mut advertisement: Advertisement,
    mut discovery: crate::config::DiscoveryConfig,
    mut capabilities: watch::Receiver<RuntimeCapabilities>,
    mut tuning: watch::Receiver<RuntimeTuning>,
    mut registration: discovery::DiscoveryRegistration,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let mut capabilities_closed = false;
    let mut tuning_closed = false;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            changed = capabilities.changed(), if !capabilities_closed => {
                match changed {
                    Err(_) => capabilities_closed = true,
                    Ok(()) => {
                        let next = *capabilities.borrow_and_update();
                        if advertisement.clipboard_mode == next.clipboard_mode
                            && advertisement.audio_mode == next.audio_mode
                            && advertisement.input_mode == next.input_mode
                        {
                            continue;
                        }
                        registration.stop().await;
                        advertisement.clipboard_mode = next.clipboard_mode;
                        advertisement.audio_mode = next.audio_mode;
                        advertisement.input_mode = next.input_mode;
                        registration = discovery::advertise(&advertisement, &discovery).await?;
                        tracing::info!(
                            clipboard = %next.clipboard_mode.label(),
                            audio = %next.audio_mode.label(),
                            input = %next.input_mode.label(),
                            "发现广播能力已更新"
                        );
                    }
                }
            }
            changed = tuning.changed(), if !tuning_closed => {
                match changed {
                    Err(_) => tuning_closed = true,
                    Ok(()) => {
                        let next = tuning.borrow_and_update().clone();
                        if advertisement.device.device_name == next.device_name
                            && advertisement.instance_name == next.instance_name
                            && discovery == next.discovery
                        {
                            continue;
                        }
                        registration.stop().await;
                        advertisement.device.device_name = next.device_name;
                        advertisement.instance_name = next.instance_name;
                        discovery = next.discovery;
                        registration = discovery::advertise(&advertisement, &discovery).await?;
                        tracing::info!(
                            device_name = %advertisement.device.device_name,
                            instance_name = ?advertisement.instance_name,
                            mdns_enabled = discovery.mdns_enabled,
                            lnd_enabled = discovery.lnd.is_some(),
                            "发现广播设置已更新"
                        );
                    }
                }
            }
        }
    }
    registration.stop().await;
    Ok(())
}

pub(crate) async fn run_client(mut config: SynlyConfig, mut options: RuntimeOptions) -> Result<()> {
    let discovery_timeout = Duration::from_secs(options.pairing.discovery_secs);
    let mut reconnect_query = options.pairing.peer_query.clone();
    let notifier = SystemNotifier::new(options.control.tuning(), options.control.input_activity());
    let shutdown = options.control.shutdown().clone();
    let mut runtime_capabilities = options.control.capabilities();
    let mut runtime_tuning = options.control.tuning();
    let initial_lifecycle = if options.pairing.known_peer.is_some() {
        RuntimeLifecycle::Connecting
    } else {
        RuntimeLifecycle::Discovering
    };
    options
        .control
        .report(RuntimeEvent::Lifecycle(initial_lifecycle));
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    let policy = ReconnectPolicy::new(RECONNECT_BASE_DELAY, RECONNECT_MAX_DELAY);
    let drive_shutdown = shutdown.clone();
    let mut attempt = PeerReconnectAttempt {
        config: &mut config,
        options: &mut options,
        reconnect_query: &mut reconnect_query,
        runtime_capabilities: &mut runtime_capabilities,
        runtime_tuning: &mut runtime_tuning,
        notifier: &notifier,
        discovery_timeout,
        direct_target: None,
        fast_retries_left: FAST_DIRECT_RETRIES,
    };
    tokio::select! {
        result = run_auto_reconnect(policy, drive_shutdown, &mut attempt) => result,
        signal_result = &mut ctrl_c => finish_ctrl_c(signal_result),
        _ = shutdown.cancelled() => Ok(()),
    }
}

struct PeerReconnectAttempt<'a> {
    config: &'a mut SynlyConfig,
    options: &'a mut RuntimeOptions,
    reconnect_query: &'a mut Option<String>,
    runtime_capabilities: &'a mut watch::Receiver<RuntimeCapabilities>,
    runtime_tuning: &'a mut watch::Receiver<RuntimeTuning>,
    notifier: &'a SystemNotifier,
    discovery_timeout: Duration,
    direct_target: Option<PeerTarget>,
    fast_retries_left: u32,
}

impl crate::reconnect::ReconnectAttempt for PeerReconnectAttempt<'_> {
    fn attempt(
        &mut self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = AttemptVerdict> + Send + '_>> {
        Box::pin(attempt_peer_connection(
            self.config,
            self.options,
            self.reconnect_query,
            self.runtime_capabilities,
            self.runtime_tuning,
            self.notifier,
            self.discovery_timeout,
            &mut self.direct_target,
            &mut self.fast_retries_left,
        ))
    }
}

async fn connect_and_run_session(
    peer_target: &PeerTarget,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
    notifier: &SystemNotifier,
    direct_target: &mut Option<PeerTarget>,
) -> Result<()> {
    let mut session = connect_to_peer(peer_target, config, options).await?;
    let _candidate_task = if options.bluetooth_enabled && session.transport == TransportKind::Lan {
        match synly_core::bluetooth::candidate::spawn_bluetooth(bluetooth::auth_config(config, options), session.logical.clone(), None, Some(options.control.input_activity())) {
            Ok((inbox, task)) => { session.secondary_inbox = Some(inbox); Some(task) },
            Err(error) => { tracing::warn!(error = %error, "蓝牙副承载初始化失败, LAN 主会话继续运行"); None },
        }
    } else { None };
    let _lan_candidate_task = if session.transport == TransportKind::Bluetooth {
        match synly_core::transport::lan_candidate::spawn_lan(bluetooth::auth_config(config, options), session.logical.clone(), options.discovery.clone(), options.transfer_limits, Some(options.control.input_activity())) {
            Ok((inbox, task)) => { session.secondary_inbox = Some(inbox); Some(task) },
            Err(error) => { tracing::warn!(error = %error, "LAN 副承载初始化失败, 蓝牙主会话继续运行"); None },
        }
    } else { None };
    *direct_target = match peer_target {
        PeerTarget::Bluetooth { address, .. } => Some(PeerTarget::Bluetooth { address: address.clone(), device_id: Some(session.remote.device_id) }),
        _ => session.remote_socket_addr.and_then(|address| match address { SocketAddr::V4(address) => Some(PeerTarget::Direct(address)), _ => None }),
    };
    let remote_label = format!(
        "{} ({})",
        identity_display_name(&session.remote),
        short_uuid(&session.remote.device_id)
    );
    let peer_summary = RuntimePeerSummary {
        device_id: session.remote.device_id,
        display_name: identity_display_name(&session.remote),
    };
    let peer = notification_peer(&session.remote);
    if let Err(err) = run_with_session_notifications(
        notifier,
        peer,
        run_sync_session(
            session,
            SyncSessionOptions {
                clipboard_mode: options.clipboard_mode,
                audio_mode: options.audio_mode,
                audio_layout: options.audio_layout,
                input_mode: options.input_mode,
                input_options: options.input.clone(),
                input_inbox: None,
                input_session_id: None,
                input_socket_tx: None,
                input_routes: None,
                clipboard_options: &options.clipboard,
                transfer_limits: options.transfer_limits,
                control: options.control.clone(),
                clipboard_hub: None,
                capability_profile: SessionCapabilityProfile::Full,
                session_shutdown: None,
            },
        ),
    )
    .await
    {
        tracing::warn!(peer = %remote_label, error = %err, "同步会话中断");
    } else {
        tracing::info!(peer = %remote_label, "连接已断开");
    }
    options
        .control
        .report(RuntimeEvent::Disconnected(peer_summary));
    options
        .control
        .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Discovering));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn attempt_peer_connection(
    config: &mut SynlyConfig,
    options: &mut RuntimeOptions,
    reconnect_query: &mut Option<String>,
    runtime_capabilities: &mut watch::Receiver<RuntimeCapabilities>,
    runtime_tuning: &mut watch::Receiver<RuntimeTuning>,
    notifier: &SystemNotifier,
    discovery_timeout: Duration,
    direct_target: &mut Option<PeerTarget>,
    fast_retries_left: &mut u32,
) -> AttemptVerdict {
    refresh_runtime_options(config, options, runtime_capabilities, runtime_tuning);
    let local_capabilities = RuntimeCapabilities {
        clipboard_mode: options.clipboard_mode,
        audio_mode: options.audio_mode,
        input_mode: options.input_mode,
    };

    if let Some(peer_target) = direct_target.clone() {
        if matches!(peer_target, PeerTarget::Bluetooth { .. }) { *reconnect_query = Some(peer_target.reconnect_query()); }
        let address = peer_target.reconnect_query();
        tracing::info!(address = %address, "使用上次地址快速重连");
        options
            .control
            .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Connecting));
        match connect_and_run_session(&peer_target, config, options, notifier, direct_target).await
        {
            Ok(()) => {
                *fast_retries_left = FAST_DIRECT_RETRIES;
                return AttemptVerdict::RetryImmediately;
            }
            Err(err) if err.downcast_ref::<PairingTerminal>().is_some() => {
                tracing::warn!(error = %err, "直连配对流程已终止, 不再自动重连");
                return AttemptVerdict::Terminal(err);
            }
            Err(err) => {
                tracing::warn!(address = %address, error = %err, "快速直连失败");
                if *fast_retries_left > 0 {
                    *fast_retries_left -= 1;
                    return AttemptVerdict::RetryImmediately;
                }
                *direct_target = None;
                tracing::info!("快速直连次数已用完, 转为重新发现");
            }
        }
    } else {
        *direct_target = None;
    }

    if let Some(known_peer) = options.pairing.known_peer.take() {
        match discovered_peer_target(known_peer, &local_capabilities) {
            Ok(peer_target) => {
                if let PeerTarget::Discovered(peer) = &peer_target {
                    tracing::info!(
                        peer = %peer.display_name(),
                        device_id = %&peer.device_id[..8.min(peer.device_id.len())],
                        port = peer.port,
                        addresses = ?peer.addresses,
                        "使用已发现地址直连, 跳过重新发现"
                    );
                }
                options
                    .control
                    .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Connecting));
                *reconnect_query = Some(peer_target.reconnect_query());
                match connect_and_run_session(
                    &peer_target,
                    config,
                    options,
                    notifier,
                    direct_target,
                )
                .await
                {
                    Ok(()) => {
                        *fast_retries_left = FAST_DIRECT_RETRIES;
                        return AttemptVerdict::RetryImmediately;
                    }
                    Err(err) if err.downcast_ref::<PairingTerminal>().is_some() => {
                        tracing::warn!(error = %err, "已发现地址配对流程已终止, 不再自动重连");
                        return AttemptVerdict::Terminal(err);
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "已发现地址直连失败, 转为重新发现");
                    }
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "已发现记录不可用, 转为重新发现");
            }
        }
        options
            .control
            .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Discovering));
    }

    let peer_target = match choose_peer(
        reconnect_query.as_deref(),
        discovery_timeout,
        options.pairing.headless,
        &local_capabilities,
        &options.discovery,
    )
    .await
    {
        Ok(peer) => peer,
        Err(err) => {
            if reconnect_query.is_some() {
                tracing::warn!(error = %err, "等待目标设备重新出现");
                return AttemptVerdict::Failed;
            }
            return AttemptVerdict::Terminal(err);
        }
    };
    if let PeerTarget::Discovered(peer) = &peer_target {
        tracing::info!(
            peer = %peer.display_name(),
            device_id = %&peer.device_id[..8.min(peer.device_id.len())],
            source = peer.source.label(),
            "已发现目标设备"
        );
    }
    *reconnect_query = Some(peer_target.reconnect_query());
    options
        .control
        .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Connecting));

    match connect_and_run_session(&peer_target, config, options, notifier, direct_target).await {
        Ok(()) => {
            *fast_retries_left = FAST_DIRECT_RETRIES;
            AttemptVerdict::RetryImmediately
        }
        Err(err) if err.downcast_ref::<PairingTerminal>().is_some() => {
            tracing::warn!(error = %err, "配对流程已终止, 不再自动重连");
            AttemptVerdict::Terminal(err)
        }
        Err(err) => {
            tracing::warn!(error = %err, "连接失败");
            options
                .control
                .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Discovering));
            AttemptVerdict::Failed
        }
    }
}

pub(crate) fn refresh_runtime_options(
    config: &mut SynlyConfig,
    options: &mut RuntimeOptions,
    capabilities: &mut watch::Receiver<RuntimeCapabilities>,
    tuning: &mut watch::Receiver<RuntimeTuning>,
) {
    let capabilities = *capabilities.borrow_and_update();
    let tuning = tuning.borrow_and_update().clone();
    config.device.device_name = tuning.device_name;
    options.instance_name = tuning.instance_name;
    options.discovery = tuning.discovery;
    options.notifications_enabled = tuning.notifications_enabled;
    options.input = tuning.input;
    options.input.mode = capabilities.input_mode;
    options.clipboard = tuning.clipboard;
    options.clipboard_mode = capabilities.clipboard_mode;
    options.audio_mode = capabilities.audio_mode;
    options.input_mode = capabilities.input_mode;
}

pub(crate) fn finish_ctrl_c(signal_result: std::io::Result<()>) -> Result<()> {
    signal_result.context("failed to listen for Ctrl-C")?;
    tracing::info!("收到 Ctrl-C, 正在安全退出");
    Ok(())
}

pub(crate) fn notification_peer(identity: &DeviceIdentity) -> NotificationPeer {
    NotificationPeer {
        display_name: identity_display_name(identity),
        short_device_id: short_uuid(&identity.device_id),
        device_id: identity.device_id,
    }
}

fn bootstrap_peer_label(device_name: &str, remote_addr: SocketAddr) -> String {
    let device_name = device_name.trim();
    if device_name.is_empty() {
        remote_addr.to_string()
    } else {
        format!("{device_name} ({remote_addr})")
    }
}

fn bootstrap_device_name_matches(declared: &str, authenticated: &str) -> bool {
    declared.trim() == authenticated.trim()
}

pub(crate) async fn run_with_session_notifications<N, F, T>(
    notifier: &N,
    peer: NotificationPeer,
    session: F,
) -> Result<T>
where
    N: SessionNotifier,
    F: Future<Output = Result<T>>,
{
    notifier.notify(ConnectionEvent::Connected, &peer);
    let _guard = SessionNotificationGuard { notifier, peer };
    session.await
}

struct SessionNotificationGuard<'a, N: SessionNotifier> {
    notifier: &'a N,
    peer: NotificationPeer,
}

impl<N: SessionNotifier> Drop for SessionNotificationGuard<'_, N> {
    fn drop(&mut self) {
        self.notifier
            .notify(ConnectionEvent::Disconnected, &self.peer);
    }
}

pub(crate) async fn handle_incoming_connection(
    socket: TcpStream,
    remote_addr: SocketAddr,
    pairing_throttle: &mut PairingThrottle,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
    reserver: &ActiveSlotReserver,
    admission: &lan_admission::LanAdmission,
) -> Result<Option<(AuthenticatedSession, SlotReservation)>> {
    let mut first_byte = [0u8; 1];
    let peeked = socket.peek(&mut first_byte).await?;
    if peeked == 0 {
        return Ok(None);
    }

    if first_byte[0] == 0x16 {
        handle_trusted_incoming_connection(socket, remote_addr, config, options, reserver, admission).await
    } else {
        if admission.full { bail!("host 会话已满, 不能启动新的 PIN 配对"); }
        handle_bootstrap_incoming_connection(
            socket,
            remote_addr,
            pairing_throttle,
            config,
            options,
            reserver,
        )
        .await
    }
}

async fn connect_to_peer(
    peer: &PeerTarget,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
) -> Result<AuthenticatedSession> {
    let device = config.device.clone();
    match peer {
        PeerTarget::Bluetooth { address, device_id } => bluetooth::connect(address, *device_id, config, options).await,
        PeerTarget::Discovered(peer) => {
            let trusted_transport = trusted_transport_for_peer(config, peer)?;
            if options.pairing.trusted_only && trusted_transport.is_none() {
                bail!(
                    "目标设备尚未建立完整的可信 mTLS 信任, 请先在 GUI 中完成一次 PIN 配对并启用 trust_device"
                );
            }
            let socket = connect_to_discovered_peer(peer).await?;
            match trusted_transport.as_ref() {
                Some(trusted_device) => {
                    connect_to_trusted_peer(socket, &device, trusted_device, config, options).await
                }
                None => connect_to_untrusted_peer(socket, &device, config, options)
                    .await
                    .map_err(|err| anyhow!(PairingTerminal(err))),
            }
        }
        PeerTarget::Direct(address) => {
            if should_try_direct_trusted(config, &options.pairing) {
                match connect_to_direct_trusted_peer(
                    *address.ip(),
                    address.port(),
                    &device,
                    config,
                    options,
                )
                .await
                {
                    Ok(session) => return Ok(session),
                    Err(err) if !options.pairing.trusted_only => {
                        tracing::warn!(error = %err, "直连 trusted mTLS 失败, 回退到 bootstrap/PIN");
                    }
                    Err(err) => return Err(err),
                }
            }
            let socket = connect_tcp(*address.ip(), address.port()).await?;
            connect_to_untrusted_peer(socket, &device, config, options)
                .await
                .map_err(|err| anyhow!(PairingTerminal(err)))
        }
    }
}

async fn connect_to_discovered_peer(peer: &DiscoveredPeer) -> Result<TcpStream> {
    if peer.addresses.is_empty() {
        bail!("peer advertised no IPv4 address");
    }
    let mut failures = Vec::new();
    if let Some(socket) =
        race_peer_addresses("候选地址", &peer.addresses, peer.port, &mut failures).await
    {
        return Ok(socket);
    }
    bail!("无法连接目标设备, 已尝试地址: {}", failures.join("; "))
}

async fn race_peer_addresses(
    group_label: &str,
    addresses: &[Ipv4Addr],
    port: u16,
    failures: &mut Vec<String>,
) -> Option<TcpStream> {
    if addresses.is_empty() {
        return None;
    }
    let endpoints = addresses
        .iter()
        .map(|address| format!("{address}:{port}"))
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!(group = group_label, endpoints = %endpoints, "开始并发连接");

    let mut attempts = tokio::task::JoinSet::new();
    for address in addresses.iter().copied() {
        attempts.spawn(async move { (address, connect_tcp(address, port).await) });
    }
    while let Some(result) = attempts.join_next().await {
        match result {
            Ok((address, Ok(socket))) => {
                attempts.abort_all();
                tracing::info!(%address, port, "TCP 连接成功");
                return Some(socket);
            }
            Ok((address, Err(err))) => {
                tracing::debug!(%address, port, error = %err, "TCP 连接失败");
                failures.push(format!("{address}:{port}: {err:#}"));
            }
            Err(err) => {
                tracing::warn!(error = %err, "TCP 连接任务异常结束");
                failures.push(format!("连接任务异常结束: {err}"));
            }
        }
    }
    None
}

async fn connect_tcp(address: Ipv4Addr, port: u16) -> Result<TcpStream> {
    match time::timeout(TCP_CONNECT_TIMEOUT, TcpStream::connect((address, port))).await {
        Ok(result) => {
            let socket =
                result.with_context(|| format!("failed to connect to {address}:{port}"))?;
            configure_session_socket(&socket)?;
            Ok(socket)
        }
        Err(_) => bail!(
            "连接 {address}:{port} 超过 {} 秒",
            TCP_CONNECT_TIMEOUT.as_secs()
        ),
    }
}

pub(crate) fn configure_session_socket(socket: &TcpStream) -> Result<()> {
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(3))
        .with_interval(Duration::from_secs(1))
        .with_retries(3);
    SockRef::from(socket)
        .set_tcp_keepalive(&keepalive)
        .context("无法配置同步会话 TCP keepalive")
}

async fn handle_trusted_incoming_connection(
    socket: TcpStream,
    remote_addr: SocketAddr,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
    reserver: &ActiveSlotReserver,
    admission: &lan_admission::LanAdmission,
) -> Result<Option<(AuthenticatedSession, SlotReservation)>> {
    let transfer_limits = TransferLimits { max_meta_len: options.transfer_limits.max_meta_len.min(16 * 1024), max_frame_data_len: 0, ..options.transfer_limits };
    let device = config.device.clone();
    let remote_label = remote_addr.to_string();
    let trusted = admission.trust_devices(&config.trusted_devices);
    if trusted.is_empty() { bail!("LAN TLS 连接没有持久信任或在线主会话授权"); }
    let acceptor = crypto::build_server_acceptor(&device, &trusted)?;
    let mut server_stream = acceptor.accept(socket).await?;
    let frame = read_frame(&mut server_stream, transfer_limits).await?;
    let (request_id, payload, trusted_proof) = match frame {
        Frame::Control(ControlMessage::PairRequest {
            request_id,
            payload,
            trusted_proof,
        }) => (request_id, payload, trusted_proof),
        _ => {
            write_frame(
                &mut server_stream,
                transfer_limits,
                Frame::Control(ControlMessage::Error {
                    message: "连接建立了，但请求格式不正确".to_string(),
                }),
            )
            .await?;
            return Ok(None);
        }
    };

    if payload.protocol_version != PROTOCOL_VERSION {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: format!("不支持的协议版本: {}", payload.protocol_version),
            }),
        )
        .await?;
        return Ok(None);
    }

    if let Err(err) = crypto::verify_device_identity_material(&payload.client) {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: format!("对端提供的设备身份材料无效: {err:#}"),
            }),
        )
        .await?;
        return Ok(None);
    }

    let (trusted_device, require_existing_session) = match admission.resolve(&payload.client, &config.trusted_devices) {
        Ok(resolved) => resolved,
        Err(error) => {
            write_frame(&mut server_stream, transfer_limits, Frame::Control(ControlMessage::Error { message: format!("LAN 可信身份准入失败: {error:#}") })).await?;
            return Ok(None);
        }
    };
    let Some(trusted_proof) = trusted_proof.as_deref() else {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: "可信设备已建立 mTLS，但缺少应用层身份签名。".to_string(),
            }),
        )
        .await?;
        return Ok(None);
    };

    let exporter = crypto::export_keying_material_from_server(&server_stream, &request_id)?;
    let audio_master_secret =
        crypto::export_audio_master_secret_from_server(&server_stream, &request_id)?;
    let input_master_secret =
        crypto::export_input_master_secret_from_server(&server_stream, &request_id)?;
    if let Err(err) = crypto::verify_device_identity(&payload.client, &trusted_device.public_key)
        .and_then(|_| {
            crypto::verify_trusted_pair_auth(
                &exporter,
                &trusted_device.public_key,
                &request_id,
                &payload,
                trusted_proof,
            )
        })
    {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: format!("可信设备身份绑定失败，已拒绝本次连接: {err:#}"),
            }),
        )
        .await?;
        return Ok(None);
    }

    let reservation = reserver.reserve(payload.client.device_id);
    let session_options = runtime_options_for_profile(options, reservation.profile());
    let clipboard_agreement = negotiate_clipboard(
        session_options.clipboard_mode,
        payload.capabilities.clipboard_mode,
    );
    let audio_compatible =
        audio_modes_compatible(session_options.audio_mode, payload.capabilities.audio_mode);
    let input_compatible =
        negotiate_input(session_options.input_mode, payload.capabilities.input_mode).is_some();
    print_pair_request_overview(&payload, &session_options, &remote_label)?;
    if !require_existing_session && !clipboard_agreement.any_direction()
        && !audio_compatible
        && !input_compatible
    {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: "剪贴板, 音频和输入方向都不兼容, 本次请求无法建立同步.".to_string(),
            }),
        )
        .await?;
        return Ok(None);
    }

    tracing::info!("可信设备 mTLS 与身份签名校验通过");
    let accepted = should_auto_accept_request(&options.pairing, PairAuthMethod::TrustedDevice);
    let message = if accepted {
        "服务端已接受同步请求。".to_string()
    } else {
        "服务端拒绝了本次同步请求。".to_string()
    };
    let control = signed_pair_decision(PairDecisionParams {
        exporter: &exporter,
        request_id: &request_id,
        accepted,
        message,
        device: &device,
        instance_name: session_options.instance_name.as_deref(),
        clipboard_mode: session_options.clipboard_mode,
        audio_mode: session_options.audio_mode,
        input_mode: session_options.input_mode,
        clipboard_agreement: &clipboard_agreement,
        auth_method: PairAuthMethod::TrustedDevice,
        pin: None,
        server_trusts_client: true,
        trust_established: false,
    })?;
    write_frame(&mut server_stream, transfer_limits, Frame::Control(control)).await?;

    if !accepted {
        return Ok(None);
    }

    if !require_existing_session {
        config.note_trusted_device_session(payload.client.device_id, &payload.client.device_name);
        config.save_trusted_devices()?;
    }

    let keys = SessionKeys::server(&server_stream, &request_id)?;
    let logical = keys.logical(device_identity(&config.device, options.instance_name.as_deref()), payload.client.clone(), TransportKind::Lan)?;
    let candidate_exporter = keys.candidate_exporter();
    let tls_stream: TlsStream<TcpStream> = server_stream.into();
    Ok(Some((
        AuthenticatedSession {
            role: SessionRole::Host,
            stream: ByteStream::new(tls_stream),
            require_existing_session,
            trusted_reconnect: true,
            transport: TransportKind::Lan,
            logical,
            candidate_exporter,
            secondary_inbox: None,
            remote: payload.client,
            remote_capabilities: payload.capabilities,
            remote_socket_addr: Some(remote_addr),
            audio_master_secret,
            input_master_secret,
            capability_profile: reservation.profile(),
        },
        reservation,
    )))
}

async fn handle_bootstrap_incoming_connection(
    mut socket: TcpStream,
    remote_addr: SocketAddr,
    pairing_throttle: &mut PairingThrottle,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
    reserver: &ActiveSlotReserver,
) -> Result<Option<(AuthenticatedSession, SlotReservation)>> {
    let transfer_limits = options.transfer_limits;
    let remote_addr_text = remote_addr.to_string();
    let remote_peer_key = remote_addr.ip().to_string();
    if options.pairing.trusted_only {
        write_frame(
            &mut socket,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: "当前 host 只允许已建立长期信任的设备通过 mTLS 直连。".to_string(),
            }),
        )
        .await?;
        return Ok(None);
    }

    if let Some(remaining) = pairing_throttle.blocked_remaining(&remote_peer_key) {
        write_frame(
            &mut socket,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: format!(
                    "该地址近期配对失败过多，请等待 {} 秒后再试。",
                    remaining.as_secs().max(1)
                ),
            }),
        )
        .await?;
        return Ok(None);
    }

    let bootstrap_hello =
        match read_frame_with_timeout(&mut socket, PAIRING_TIMEOUT, transfer_limits).await {
            Ok(frame) => frame,
            Err(err) => {
                register_pairing_failure(pairing_throttle, &remote_peer_key).await;
                return Err(err);
            }
        };
    let bootstrap_hello = match bootstrap_hello {
        Frame::Control(ControlMessage::BootstrapHello {
            protocol_version,
            client_bootstrap_public_key,
            device_name,
        }) => (protocol_version, client_bootstrap_public_key, device_name),
        _ => {
            write_frame(
                &mut socket,
                transfer_limits,
                Frame::Control(ControlMessage::Error {
                    message: "未信任设备必须先发送最小 bootstrap 请求。".to_string(),
                }),
            )
            .await?;
            return Ok(None);
        }
    };
    let (protocol_version, client_bootstrap_public_key, client_device_name) = bootstrap_hello;
    if protocol_version != PROTOCOL_VERSION {
        write_frame(
            &mut socket,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: format!("不支持的协议版本: {protocol_version}"),
            }),
        )
        .await?;
        return Ok(None);
    }

    let client_display = crypto::bootstrap_public_key_display(&client_bootstrap_public_key)?;
    let remote_label = bootstrap_peer_label(&client_device_name, remote_addr);
    let request_id = Uuid::new_v4().to_string();
    let server_bootstrap_key = crypto::generate_bootstrap_key_material()?;
    let server_bootstrap_public_key = server_bootstrap_key.public_key_encoded();
    let session_display = crypto::bootstrap_session_display(
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    )?;
    let pin = options
        .pairing
        .pin
        .clone()
        .unwrap_or_else(crypto::random_pin);

    tracing::info!(
        remote = %remote_label,
        bootstrap = %client_display.short,
        session = %session_display.short,
        fixed_pin = options.pairing.pin.is_some(),
        "收到未信任设备的最小配对请求"
    );
    tracing::debug!(
        bootstrap_randomart = %client_display.randomart,
        session_randomart = %session_display.randomart,
        "配对核对图已生成"
    );
    let gui_pairing_id = Uuid::new_v4();
    let (mut host_pin, mut cancel_rx) = HostPinPrompt::watch(
        &options.control,
        InteractionRequest::ShowHostPin {
            request_id: gui_pairing_id,
            remote_label: remote_label.clone(),
            bootstrap_short: client_display.short.clone(),
            bootstrap_randomart: client_display.randomart.clone(),
            session_short: session_display.short.clone(),
            session_randomart: session_display.randomart.clone(),
            pin: pin.clone(),
            fixed_pin: options.pairing.pin.is_some(),
        },
    )?;

    let (pake_state, server_pake_message) = crypto::start_bootstrap_pake_server(
        &pin,
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    )?;

    write_frame(
        &mut socket,
        transfer_limits,
        Frame::Control(ControlMessage::BootstrapChallenge {
            request_id: request_id.clone(),
            server_bootstrap_public_key: server_bootstrap_public_key.clone(),
            server_pake_message,
        }),
    )
    .await?;

    let pake_frame = match wait_or_cancel_pairing(
        read_frame_with_timeout(&mut socket, PAIRING_TIMEOUT, transfer_limits),
        &mut cancel_rx,
    )
    .await
    {
        Ok(Some(frame)) => frame,
        Ok(None) => return Ok(None),
        Err(err) => {
            register_pairing_failure(pairing_throttle, &remote_peer_key).await;
            return Err(err);
        }
    };
    let (client_pake_message, client_confirm) = match pake_frame {
        Frame::Control(ControlMessage::BootstrapPake {
            request_id: incoming_request_id,
            client_pake_message,
            client_confirm,
        }) if incoming_request_id == request_id => (client_pake_message, client_confirm),
        Frame::Control(ControlMessage::BootstrapPake { .. }) => {
            register_pairing_failure(pairing_throttle, &remote_peer_key).await;
            write_frame(
                &mut socket,
                transfer_limits,
                Frame::Control(ControlMessage::Error {
                    message: "收到的 PAKE 请求标识与当前连接不匹配。".to_string(),
                }),
            )
            .await?;
            return Ok(None);
        }
        _ => {
            register_pairing_failure(pairing_throttle, &remote_peer_key).await;
            write_frame(
                &mut socket,
                transfer_limits,
                Frame::Control(ControlMessage::Error {
                    message: "客户端没有按预期完成 PAKE 认证。".to_string(),
                }),
            )
            .await?;
            return Ok(None);
        }
    };

    let pake_key =
        match crypto::finish_bootstrap_pake(pake_state, &client_pake_message).and_then(|pake_key| {
            crypto::verify_client_pake_confirm(
                &pake_key,
                &request_id,
                &client_bootstrap_public_key,
                &server_bootstrap_public_key,
                &client_confirm,
            )?;
            Ok(pake_key)
        }) {
            Ok(pake_key) => pake_key,
            Err(err) => {
                register_pairing_failure(pairing_throttle, &remote_peer_key).await;
                write_frame(
                    &mut socket,
                    transfer_limits,
                    Frame::Control(ControlMessage::Error {
                        message: format!("PIN 或 PAKE 认证失败：{err:#}"),
                    }),
                )
                .await?;
                return Ok(None);
            }
        };

    pairing_throttle.note_success(&remote_peer_key);
    let server_confirm = crypto::server_pake_confirm(
        &pake_key,
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    );
    write_frame(
        &mut socket,
        transfer_limits,
        Frame::Control(ControlMessage::BootstrapAck {
            request_id: request_id.clone(),
            server_confirm,
        }),
    )
    .await?;

    let acceptor = crypto::build_bootstrap_server_acceptor(
        &request_id,
        &pake_key,
        server_bootstrap_key,
        &client_bootstrap_public_key,
    )?;
    let device = config.device.clone();
    let Some(mut server_stream) = wait_or_cancel_pairing(
        async {
            Ok(time::timeout(TLS_UPGRADE_TIMEOUT, acceptor.accept(socket))
                .await
                .map_err(|_| anyhow!("等待客户端切换到临时 mTLS 超时"))??)
        },
        &mut cancel_rx,
    )
    .await?
    else {
        return Ok(None);
    };
    let Some(frame) = wait_or_cancel_pairing(
        read_frame_with_timeout(&mut server_stream, PAIRING_TIMEOUT, transfer_limits),
        &mut cancel_rx,
    )
    .await?
    else {
        return Ok(None);
    };
    let (incoming_request_id, payload, trusted_proof) = match frame {
        Frame::Control(ControlMessage::PairRequest {
            request_id,
            payload,
            trusted_proof,
        }) => (request_id, payload, trusted_proof),
        _ => {
            write_frame(
                &mut server_stream,
                transfer_limits,
                Frame::Control(ControlMessage::Error {
                    message: "临时 mTLS 已建立，但请求格式不正确。".to_string(),
                }),
            )
            .await?;
            return Ok(None);
        }
    };
    if incoming_request_id != request_id {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: "收到的请求标识与当前 bootstrap 会话不匹配。".to_string(),
            }),
        )
        .await?;
        return Ok(None);
    }
    if trusted_proof.is_some() {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: "bootstrap 配对阶段不接受 trusted-device 签名。".to_string(),
            }),
        )
        .await?;
        return Ok(None);
    }
    if payload.protocol_version != PROTOCOL_VERSION {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: format!("不支持的协议版本: {}", payload.protocol_version),
            }),
        )
        .await?;
        return Ok(None);
    }
    if let Err(err) = crypto::verify_device_identity_material(&payload.client) {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: format!("对端提供的设备身份材料无效: {err:#}"),
            }),
        )
        .await?;
        return Ok(None);
    }

    let exporter = crypto::export_keying_material_from_server(&server_stream, &request_id)?;
    let audio_master_secret =
        crypto::export_audio_master_secret_from_server(&server_stream, &request_id)?;
    let input_master_secret =
        crypto::export_input_master_secret_from_server(&server_stream, &request_id)?;
    let reservation = reserver.reserve(payload.client.device_id);
    let session_options = runtime_options_for_profile(options, reservation.profile());
    let clipboard_agreement = negotiate_clipboard(
        session_options.clipboard_mode,
        payload.capabilities.clipboard_mode,
    );
    let audio_compatible =
        audio_modes_compatible(session_options.audio_mode, payload.capabilities.audio_mode);
    let input_compatible =
        negotiate_input(session_options.input_mode, payload.capabilities.input_mode).is_some();
    if !bootstrap_device_name_matches(&client_device_name, &payload.client.device_name) {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: "bootstrap 设备名与认证身份不一致".to_string(),
            }),
        )
        .await?;
        return Ok(None);
    }
    print_pair_request_overview(&payload, &session_options, &remote_addr_text)?;
    if !clipboard_agreement.any_direction()
        && !audio_compatible
        && !input_compatible
    {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: "剪贴板, 音频和输入方向都不兼容, 本次请求无法建立同步.".to_string(),
            }),
        )
        .await?;
        return Ok(None);
    }

    tracing::info!("已建立基于 PIN 的临时 mTLS, 设备元数据处于加密保护中");
    if pairing_cancel_received(&mut cancel_rx) {
        return Ok(None);
    }
    let (accepted, remember_trusted_device) =
        if should_auto_accept_request(&options.pairing, PairAuthMethod::Pin) {
            (true, options.pairing.trust_device)
        } else if options.pairing.headless {
            tracing::warn!("headless 模式拒绝未信任设备");
            (false, false)
        } else {
            host_pin.persist();
            let interaction_id = Uuid::new_v4();
            let summary = payload.capabilities.summary_lines();
            match options
                .control
                .request_interaction(InteractionRequest::AcceptPeer {
                    request_id: interaction_id,
                    display_name: identity_display_name(&payload.client),
                    device_id: payload.client.device_id,
                    summary,
                    default_trust: options.pairing.trust_device,
                })
                .await?
            {
                InteractionResponse::Decision { accepted, trust } => (accepted, trust),
                InteractionResponse::Cancel => (false, false),
                _ => bail!("GUI 返回了无效的配对决定"),
            }
        };
    let server_trusts_client = accepted && remember_trusted_device;
    let trust_established = accepted && remember_trusted_device && payload.request_trust;
    let message = if accepted {
        "服务端已接受同步请求。".to_string()
    } else {
        "服务端拒绝了本次同步请求。".to_string()
    };
    let control = signed_pair_decision(PairDecisionParams {
        exporter: &exporter,
        request_id: &request_id,
        accepted,
        message,
        device: &device,
        instance_name: session_options.instance_name.as_deref(),
        clipboard_mode: session_options.clipboard_mode,
        audio_mode: session_options.audio_mode,
        input_mode: session_options.input_mode,
        clipboard_agreement: &clipboard_agreement,
        auth_method: PairAuthMethod::Pin,
        pin: Some(&pin),
        server_trusts_client,
        trust_established,
    })?;
    write_frame(&mut server_stream, transfer_limits, Frame::Control(control)).await?;

    if accepted && remember_trusted_device {
        config.remember_trusted_device(
            payload.client.device_id,
            payload.client.device_name.clone(),
            payload.client.identity_public_key.clone(),
            payload.client.tls_root_certificate.clone(),
        );
        config.save_trusted_devices()?;
        if trust_established {
            tracing::info!("已保存对侧身份和 TLS 根证书, 后续连接将使用长期 mTLS");
        } else {
            tracing::info!("已保存对侧身份, 对侧本次未请求建立双向信任");
        }
    }

    if !accepted {
        return Ok(None);
    }

    let keys = SessionKeys::server(&server_stream, &request_id)?;
    let logical = keys.logical(device_identity(&config.device, options.instance_name.as_deref()), payload.client.clone(), TransportKind::Lan)?;
    let candidate_exporter = keys.candidate_exporter();
    let tls_stream: TlsStream<TcpStream> = server_stream.into();
    Ok(Some((
        AuthenticatedSession {
            role: SessionRole::Host,
            stream: ByteStream::new(tls_stream),
            require_existing_session: false,
            trusted_reconnect: false,
            transport: TransportKind::Lan,
            logical,
            candidate_exporter,
            secondary_inbox: None,
            remote: payload.client,
            remote_capabilities: payload.capabilities,
            remote_socket_addr: Some(remote_addr),
            audio_master_secret,
            input_master_secret,
            capability_profile: reservation.profile(),
        },
        reservation,
    )))
}

async fn connect_to_trusted_peer(
    socket: TcpStream,
    device: &DeviceConfig,
    trusted_device: &TrustedDeviceConfig,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
) -> Result<AuthenticatedSession> {
    let remote_socket_addr = socket.peer_addr()?;
    let connector =
        crypto::build_client_connector(device, trusted_device.tls_root_certificate.as_str())?;
    let client_stream = connector.connect(crypto::server_name()?, socket).await?;

    complete_trusted_client_pairing(
        client_stream,
        remote_socket_addr,
        device,
        config,
        options,
        {
            let trusted_device = trusted_device.clone();
            move |_config, _remote| Ok(trusted_device.clone())
        },
    )
    .await
}

async fn connect_to_direct_trusted_peer(
    address: std::net::Ipv4Addr,
    port: u16,
    device: &DeviceConfig,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
) -> Result<AuthenticatedSession> {
    if !has_trusted_transport(config) {
        bail!(
            "本机尚未保存可用于长期 mTLS 的可信设备根证书, 请先在 GUI 中完成一次 PIN 配对并启用 trust_device"
        );
    }

    let socket = connect_tcp(address, port).await?;
    let remote_socket_addr = socket.peer_addr()?;
    let connector =
        crypto::build_client_connector_for_trusted_devices(device, &config.trusted_devices)
            .with_context(|| format!("无法为直连目标 {}:{} 构建可信 mTLS 客户端", address, port))?;
    let client_stream = connector.connect(crypto::server_name()?, socket).await?;

    complete_trusted_client_pairing(
        client_stream,
        remote_socket_addr,
        device,
        config,
        options,
        trusted_transport_for_identity,
    )
    .await
}

async fn complete_trusted_client_pairing<F>(
    mut client_stream: ClientTlsStream<TcpStream>,
    remote_socket_addr: SocketAddr,
    device: &DeviceConfig,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
    resolve_trusted_device: F,
) -> Result<AuthenticatedSession>
where
    F: Fn(&SynlyConfig, &DeviceIdentity) -> Result<TrustedDeviceConfig>,
{
    let transfer_limits = options.transfer_limits;
    let request_id = Uuid::new_v4().to_string();
    let exporter = crypto::export_keying_material_from_client(&client_stream, &request_id)?;
    let audio_master_secret =
        crypto::export_audio_master_secret_from_client(&client_stream, &request_id)?;
    let input_master_secret =
        crypto::export_input_master_secret_from_client(&client_stream, &request_id)?;
    let payload = PairRequestPayload {
        protocol_version: PROTOCOL_VERSION,
        client: device_identity(device, options.instance_name.as_deref()),
        capabilities: RuntimeCapabilities {
            clipboard_mode: options.clipboard_mode,
            audio_mode: options.audio_mode,
            input_mode: options.input_mode,
        },
        request_trust: options.pairing.trust_device,
    };
    let trusted_proof = crypto::sign_trusted_pair_auth(
        &exporter,
        device.identity_private_key()?,
        &request_id,
        &payload,
    )?;
    write_frame(
        &mut client_stream,
        transfer_limits,
        Frame::Control(ControlMessage::PairRequest {
            request_id: request_id.clone(),
            payload: payload.clone(),
            trusted_proof: Some(trusted_proof),
        }),
    )
    .await?;

    let reply = match read_frame(&mut client_stream, transfer_limits).await? {
        Frame::Control(message) => message,
        _ => bail!("peer sent a non-control response during trusted pairing"),
    };
    let (remote, remote_capabilities, _clipboard_agreement) = match reply {
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
                bail!("peer replied to trusted mTLS with an unexpected auth method");
            }
            let trusted_device = resolve_trusted_device(config, &server)?;
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
            crypto::verify_device_identity_material(&server)?;
            crypto::verify_device_identity(&server, &trusted_device.public_key)?;
            crypto::verify_trusted_pair_decision(
                &decision,
                &exporter,
                &request_id,
                &trusted_device.public_key,
            )?;
            if !accepted {
                bail!("{}", message);
            }
            (server, capabilities, clipboard_agreement)
        }
        ControlMessage::Error { message } => bail!("{}", message),
        other => bail!("unexpected trusted pairing response: {other:?}"),
    };

    config.note_trusted_device_session(remote.device_id, &remote.device_name);
    config.save_trusted_devices()?;

    let keys = SessionKeys::client(&client_stream, &request_id)?;
    let logical = keys.logical(device_identity(device, options.instance_name.as_deref()), remote.clone(), TransportKind::Lan)?;
    let candidate_exporter = keys.candidate_exporter();
    let tls_stream: TlsStream<TcpStream> = client_stream.into();
    print_connected_peer(&remote, &remote_capabilities, options.input_mode)?;
    Ok(AuthenticatedSession {
        role: SessionRole::Client,
        stream: ByteStream::new(tls_stream),
        require_existing_session: false,
        trusted_reconnect: true,
        transport: TransportKind::Lan,
        logical,
        candidate_exporter,
        secondary_inbox: None,
        remote,
        remote_capabilities,
        remote_socket_addr: Some(remote_socket_addr),
        audio_master_secret,
        input_master_secret,
        capability_profile: SessionCapabilityProfile::Full,
    })
}

async fn connect_to_untrusted_peer(
    mut socket: TcpStream,
    device: &DeviceConfig,
    config: &mut SynlyConfig,
    options: &RuntimeOptions,
) -> Result<AuthenticatedSession> {
    let transfer_limits = options.transfer_limits;
    let remote_socket_addr = socket.peer_addr()?;
    let client_bootstrap_key = crypto::generate_bootstrap_key_material()?;
    let client_bootstrap_public_key = client_bootstrap_key.public_key_encoded();
    let client_display = crypto::bootstrap_public_key_display(&client_bootstrap_public_key)?;

    tracing::info!(bootstrap = %client_display.short, "发起最小配对请求");
    tracing::debug!(bootstrap_randomart = %client_display.randomart, "本机 bootstrap 核对图已生成");

    write_frame(
        &mut socket,
        transfer_limits,
        Frame::Control(ControlMessage::BootstrapHello {
            protocol_version: PROTOCOL_VERSION,
            client_bootstrap_public_key: client_bootstrap_public_key.clone(),
            device_name: device.device_name.clone(),
        }),
    )
    .await?;

    let (request_id, server_bootstrap_public_key, server_pake_message) =
        match read_frame_with_timeout(&mut socket, PAIRING_TIMEOUT, transfer_limits).await? {
            Frame::Control(ControlMessage::BootstrapChallenge {
                request_id,
                server_bootstrap_public_key,
                server_pake_message,
            }) => (request_id, server_bootstrap_public_key, server_pake_message),
            Frame::Control(ControlMessage::Error { message }) => bail!("{}", message),
            other => bail!("unexpected bootstrap response: {other:?}"),
        };
    let session_display = crypto::bootstrap_session_display(
        &request_id,
        &client_bootstrap_public_key,
        &server_bootstrap_public_key,
    )?;

    tracing::info!(session = %session_display.short, "收到配对会话核对图");
    tracing::debug!(session_randomart = %session_display.randomart, "配对会话核对图已生成");
    let pin = match options.pairing.pin.as_deref() {
        Some(pin) => normalize_pin(pin)?,
        None if options.pairing.headless => {
            bail!("headless 模式不允许 PIN 配对, 请先建立长期信任并配置 trusted_only = true")
        }
        None => {
            let response = options
                .control
                .request_interaction(InteractionRequest::EnterPin {
                    request_id: Uuid::new_v4(),
                    bootstrap_short: client_display.short.clone(),
                    bootstrap_randomart: client_display.randomart.clone(),
                    session_short: session_display.short.clone(),
                    session_randomart: session_display.randomart.clone(),
                })
                .await?;
            match response {
                InteractionResponse::Pin(pin) => normalize_pin(&pin)?,
                InteractionResponse::Cancel => bail!("用户取消了 PIN 配对"),
                _ => bail!("GUI 返回了无效的 PIN 响应"),
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
        transfer_limits,
        Frame::Control(ControlMessage::BootstrapPake {
            request_id: request_id.clone(),
            client_pake_message,
            client_confirm,
        }),
    )
    .await?;

    match read_frame_with_timeout(&mut socket, PAIRING_TIMEOUT, transfer_limits).await? {
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
            bail!("peer returned a mismatched bootstrap acknowledgment");
        }
        Frame::Control(ControlMessage::Error { message }) => bail!("{}", message),
        other => bail!("unexpected PAKE response: {other:?}"),
    }

    let connector = crypto::build_bootstrap_client_connector(
        &request_id,
        &pake_key,
        client_bootstrap_key,
        &server_bootstrap_public_key,
    )?;
    let mut client_stream = time::timeout(
        TLS_UPGRADE_TIMEOUT,
        connector.connect(crypto::server_name()?, socket),
    )
    .await
    .map_err(|_| anyhow!("等待服务端切换到临时 mTLS 超时"))??;
    let exporter = crypto::export_keying_material_from_client(&client_stream, &request_id)?;
    let audio_master_secret =
        crypto::export_audio_master_secret_from_client(&client_stream, &request_id)?;
    let input_master_secret =
        crypto::export_input_master_secret_from_client(&client_stream, &request_id)?;
    let payload = PairRequestPayload {
        protocol_version: PROTOCOL_VERSION,
        client: device_identity(device, options.instance_name.as_deref()),
        capabilities: RuntimeCapabilities {
            clipboard_mode: options.clipboard_mode,
            audio_mode: options.audio_mode,
            input_mode: options.input_mode,
        },
        request_trust: options.pairing.trust_device,
    };
    write_frame(
        &mut client_stream,
        transfer_limits,
        Frame::Control(ControlMessage::PairRequest {
            request_id: request_id.clone(),
            payload: payload.clone(),
            trusted_proof: None,
        }),
    )
    .await?;

    let reply = match read_frame_with_timeout(&mut client_stream, PAIRING_TIMEOUT, transfer_limits)
        .await?
    {
        Frame::Control(message) => message,
        _ => bail!("peer sent a non-control response during bootstrap pairing"),
    };
    let (remote, remote_capabilities) = match &reply {
        ControlMessage::PairDecision {
            accepted,
            message,
            server,
            capabilities,
            auth_method,
            ..
        } => {
            if *auth_method != PairAuthMethod::Pin {
                bail!("bootstrap pairing expected a PIN-bound decision");
            }
            crypto::verify_device_identity_material(server)?;
            crypto::verify_pair_decision(&reply, &exporter, &request_id, &pin)?;
            if !accepted {
                bail!("{}", message);
            }
            (server.clone(), *capabilities)
        }
        ControlMessage::Error { message } => bail!("{}", message),
        other => bail!("unexpected bootstrap pairing response: {other:?}"),
    };

    let (server_trusts_client, trust_established) = match &reply {
        ControlMessage::PairDecision {
            server_trusts_client,
            trust_established,
            ..
        } => (*server_trusts_client, *trust_established),
        _ => (false, false),
    };
    if server_trusts_client && !has_trusted_transport_for_device(config, &remote.device_id) {
        let remember_server = if options.pairing.trust_device {
            true
        } else if options.pairing.headless {
            tracing::info!("headless 模式未请求信任服务端");
            false
        } else {
            match options
                .control
                .request_interaction(InteractionRequest::ConfirmTrust {
                    request_id: Uuid::new_v4(),
                    display_name: identity_display_name(&remote),
                    device_id: remote.device_id,
                })
                .await?
            {
                InteractionResponse::Confirm(remember) => remember,
                InteractionResponse::Cancel => false,
                _ => bail!("GUI 返回了无效的信任响应"),
            }
        };
        if remember_server {
            config.remember_trusted_device(
                remote.device_id,
                remote.device_name.clone(),
                remote.identity_public_key.clone(),
                remote.tls_root_certificate.clone(),
            );
            config.save_trusted_devices()?;
            if options.pairing.trust_device {
                tracing::info!("服务端已信任本机, 已按 trust_device 配置保存对侧身份");
            } else if trust_established {
                tracing::info!("双方已保存彼此身份, 后续连接将优先使用长期 mTLS");
            } else {
                tracing::info!("已保存服务端身份和 TLS 根证书, 后续连接将优先使用长期 mTLS");
            }
        } else {
            tracing::info!("本机未保存服务端身份, 下次连接仍使用 bootstrap/PIN/PAKE");
        }
    }

    let keys = SessionKeys::client(&client_stream, &request_id)?;
    let logical = keys.logical(device_identity(device, options.instance_name.as_deref()), remote.clone(), TransportKind::Lan)?;
    let candidate_exporter = keys.candidate_exporter();
    let tls_stream: TlsStream<TcpStream> = client_stream.into();
    print_connected_peer(&remote, &remote_capabilities, options.input_mode)?;
    Ok(AuthenticatedSession {
        role: SessionRole::Client,
        stream: ByteStream::new(tls_stream),
        require_existing_session: false,
        trusted_reconnect: false,
        transport: TransportKind::Lan,
        logical,
        candidate_exporter,
        secondary_inbox: None,
        remote,
        remote_capabilities,
        remote_socket_addr: Some(remote_socket_addr),
        audio_master_secret,
        input_master_secret,
        capability_profile: SessionCapabilityProfile::Full,
    })
}

pub(crate) struct SyncSessionOptions<'a> {
    pub(crate) clipboard_mode: ClipboardMode,
    pub(crate) audio_mode: AudioMode,
    pub(crate) audio_layout: audio::AudioLayout,
    pub(crate) input_mode: InputMode,
    pub(crate) input_options: InputRuntimeOptions,
    pub(crate) input_inbox: Option<InputSocketInbox>,
    pub(crate) input_session_id: Option<watch::Sender<Option<Uuid>>>,
    pub(crate) input_socket_tx: Option<mpsc::Sender<InputSocketConnection>>,
    pub(crate) input_routes: Option<Arc<InputRouteRegistry>>,
    pub(crate) clipboard_options: &'a crate::clipboard::ClipboardRuntimeOptions,
    pub(crate) transfer_limits: TransferLimits,
    pub(crate) control: RuntimeControl,
    pub(crate) clipboard_hub: Option<ClipboardHubHandle>,
    pub(crate) capability_profile: SessionCapabilityProfile,
    pub(crate) session_shutdown: Option<CancellationToken>,
}

#[derive(Default)]
struct SessionTaskAbortGuard {
    handles: Vec<tokio::task::AbortHandle>,
}

impl SessionTaskAbortGuard {
    fn track<T>(&mut self, task: &tokio::task::JoinHandle<T>) {
        self.handles.push(task.abort_handle());
    }
}

impl Drop for SessionTaskAbortGuard {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}

struct ClipboardCapabilityRuntime {
    sync: ClipboardSync,
    watcher: Option<ClipboardWatcherHandle>,
    sender_task: Option<tokio::task::JoinHandle<Result<()>>>,
    can_send: bool,
    can_receive: bool,
}

impl ClipboardCapabilityRuntime {
    fn new(options: &crate::clipboard::ClipboardRuntimeOptions) -> Self {
        Self {
            sync: ClipboardSync::new(options),
            watcher: None,
            sender_task: None,
            can_send: false,
            can_receive: false,
        }
    }

    fn stop_sender(&mut self) {
        self.watcher.take();
        if let Some(task) = self.sender_task.take() {
            task.abort();
        }
        self.can_send = false;
    }
}

struct CapabilityTaskRuntime {
    clipboard: ClipboardCapabilityRuntime,
    audio_task: Option<audio::AudioTaskHandle>,
    audio_epoch: Option<CapabilityEpoch>,
    audio_plan: Option<AudioPlan>,
    audio_lan: Option<audio_path::LanAudioPath>,
    audio_blocked: Option<(CapabilityEpoch, Option<audio_path::LanAudioPath>)>,
    audio_deadline: Option<Instant>,
    input_task: Option<tokio::task::JoinHandle<()>>,
    input_epoch: Option<CapabilityEpoch>,
    pending_mux_input: Option<(Uuid, ByteStream, Instant)>,
    input_generation: Option<Uuid>,
    input_role: Option<LocalInputRole>,
    input_route: synly_core::transport::routing::ChannelRoute,
    input_uses_mux: bool,
    input_transport: Option<TransportKind>,
    input_requires_manual: bool,
    input_blocked: Option<(CapabilityEpoch, Option<TransportKind>)>,
}

impl CapabilityTaskRuntime {
    fn new(clipboard_options: &crate::clipboard::ClipboardRuntimeOptions, role: SessionRole) -> Self {
        Self {
            clipboard: ClipboardCapabilityRuntime::new(clipboard_options),
            audio_task: None,
            audio_epoch: None,
            audio_plan: None,
            audio_lan: None, audio_blocked: None, audio_deadline: None,
            input_task: None,
            input_epoch: None,
            pending_mux_input: None,
            input_generation: None,
            input_role: None,
            input_route: synly_core::transport::routing::ChannelRoute::new(synly_core::transport::routing::FunctionalChannel::Input, matches!(role, SessionRole::Host), synly_core::transport::routing::PathPolicy::PreferBluetooth),
            input_uses_mux: false, input_transport: None, input_requires_manual: false, input_blocked: None,
        }
    }

    async fn stop_audio(&mut self) {
        if let Some(task) = self.audio_task.take()
            && let Err(err) = task.stop().await
        {
            tracing::warn!(error = %err, "关闭音频 UDP 通道失败");
        }
        self.audio_epoch = None;
        self.audio_plan = None;
        self.audio_lan = None; self.audio_deadline = None;
    }

    async fn stop_input(
        &mut self,
        input_session_id: Option<&watch::Sender<Option<Uuid>>>,
        input_routes: Option<&Arc<InputRouteRegistry>>,
    ) {
        self.input_requires_manual |= self.input_task.is_some() || self.input_generation.is_some();
        self.input_route.cancel();
        if let Some(session_id) = input_session_id {
            if let Some(route_id) = *session_id.borrow()
                && let Some(routes) = input_routes
            {
                routes.remove(&route_id);
            }
            session_id.send_replace(None);
        }
        if let Some(task) = self.input_task.take() {
            task.abort();
            let _ = task.await;
        }
        self.pending_mux_input.take();
        self.input_generation = None;
        self.input_epoch = None;
        self.input_role = None;
    }

    async fn stop_all(
        &mut self,
        input_session_id: Option<&watch::Sender<Option<Uuid>>>,
        input_routes: Option<&Arc<InputRouteRegistry>>,
    ) {
        self.stop_input(input_session_id, input_routes).await;
        self.stop_audio().await;
        self.clipboard.stop_sender();
        self.clipboard.can_receive = false;
    }
}

struct CapabilityRefreshContext<'a> {
    pub(crate) session_role: SessionRole,
    pub(crate) peer_device_id: Uuid,
    pub(crate) input_mux: Option<&'a synly_core::transport::generation::GenerationLane>,
    pub(crate) input_transport: Option<TransportKind>,
    pub(crate) remote_socket_addr: Option<SocketAddr>,
    pub(crate) audio_master_secret: [u8; 32],
    pub(crate) audio_lan: Option<audio_path::LanAudioPath>,
    pub(crate) audio_layout: audio::AudioLayout,
    pub(crate) input_master_secret: [u8; 32],
    pub(crate) input_options: &'a InputRuntimeOptions,
    pub(crate) input_inbox: Option<&'a InputSocketInbox>,
    pub(crate) input_session_id: Option<&'a watch::Sender<Option<Uuid>>>,
    pub(crate) input_socket_tx: Option<&'a mpsc::Sender<InputSocketConnection>>,
    pub(crate) input_routes: Option<&'a Arc<InputRouteRegistry>>,
    pub(crate) input_activity: &'a Arc<AtomicBool>,
    pub(crate) clipboard_hub: Option<&'a ClipboardHubHandle>,
    pub(crate) tx: &'a FrameSender,
}

async fn refresh_capability_tasks(
    state: &CapabilityState,
    runtime: &mut CapabilityTaskRuntime,
    tasks: &mut SessionTaskAbortGuard,
    context: CapabilityRefreshContext<'_>,
) -> Result<()> {
    let local = state.effective_local();
    let remote = state.effective_remote();
    let clipboard_agreement = negotiate_clipboard_modes(
        context.session_role,
        local.clipboard_mode,
        remote.clipboard_mode,
    );
    let clipboard_can_send = allows_local_send(context.session_role, &clipboard_agreement);
    let clipboard_can_receive = allows_local_receive(context.session_role, &clipboard_agreement);
    context.tx.set_clipboard_enabled(clipboard_can_send);
    if let Some(hub) = context.clipboard_hub {
        hub.set_receive_enabled(context.peer_device_id, clipboard_can_receive);
    } else {
        if runtime.clipboard.can_send && !clipboard_can_send {
            runtime.clipboard.stop_sender();
        }
        if !runtime.clipboard.can_send && clipboard_can_send {
            let (clipboard_tx, clipboard_rx) = mpsc::unbounded_channel();
            let watcher = match runtime
                .clipboard
                .sync
                .start_local_watcher(clipboard_tx.clone())
            {
                Ok(watcher) => Some(watcher),
                Err(err) => {
                    tracing::warn!(error = %err, "无法启动剪贴板监听, 本次仅接收远端更新");
                    None
                }
            };
            if watcher.is_some()
                && let Err(err) = runtime
                    .clipboard
                    .sync
                    .publish_initial_payload(&clipboard_tx)
                    .await
            {
                tracing::warn!(error = %err, "无法读取当前剪贴板内容, 已跳过初始同步");
            }
            if watcher.is_some() {
                let task = tokio::spawn(clipboard_sender_loop(clipboard_rx, context.tx.clone()));
                tasks.track(&task);
                runtime.clipboard.sender_task = Some(task);
                runtime.clipboard.watcher = watcher;
                runtime.clipboard.can_send = true;
            }
        }
    }
    runtime.clipboard.can_receive = clipboard_can_receive;

    let epoch = state.epoch();
    audio_path::refresh(state, runtime, &context).await?;

    let input_role = state
        .is_local_acknowledged()
        .then(|| negotiate_input(local.input_mode, remote.input_mode))
        .flatten()
        .filter(|_| context.input_transport.is_some() && (context.remote_socket_addr.is_some() || context.input_mux.is_some()));
    if runtime.input_blocked == Some((epoch, context.input_transport)) { return Ok(()); }
    if runtime.input_epoch != Some(epoch) || runtime.input_role != input_role || runtime.input_uses_mux != context.input_mux.is_some() || runtime.input_transport != context.input_transport {
        runtime
            .stop_input(context.input_session_id, context.input_routes)
            .await;
        context.input_activity.store(false, Ordering::Release);
        runtime.input_epoch = Some(epoch);
        runtime.input_role = input_role;
        runtime.input_uses_mux = context.input_mux.is_some();
        runtime.input_transport = context.input_transport;
        runtime.input_blocked = None;
        if let Some(local_role) = input_role
            && matches!(context.session_role, SessionRole::Host)
        {
            if let Some(lane) = context.input_mux {
                let generation = Uuid::new_v4();
                runtime.pending_mux_input = Some((generation, lane.lease(generation)?, Instant::now() + Duration::from_secs(10)));
                runtime.input_generation = Some(generation);
                let route_epoch = runtime.input_route.begin(context.input_transport)?;
                let message = runtime.input_route.quiesced(route_epoch)?;
                context.tx.send(Frame::Control(ControlMessage::InputPath { epoch, generation, message })).await?;
                return Ok(());
            }
            let channel = InputHostChannel::create()?;
            let session_id = channel.offer().session_id;
            let offer = channel.offer().clone();
            let inbox = context
                .input_inbox
                .cloned()
                .context("输入协商成功但 host 未提供辅助连接队列")?;
            let session_id_tx = context
                .input_session_id
                .context("输入协商成功但 host 未提供会话路由")?;
            session_id_tx.send_replace(Some(session_id));
            if let (Some(routes), Some(socket_tx)) = (context.input_routes, context.input_socket_tx)
            {
                routes.insert(session_id, socket_tx.clone());
            }
            context
                .tx
                .send(Frame::Control(ControlMessage::InputChannelOffer {
                    epoch,
                    offer,
                }))
                .await?;
            let mut input_options = context.input_options.clone();
            input_options.mode = local.input_mode;
            let input_master_secret = context.input_master_secret;
            let activity = Arc::clone(context.input_activity);
            let require_manual = runtime.input_requires_manual;
            let task = tokio::spawn(async move {
                if let Err(err) = input::run_input_session_with_gate(
                    InputSessionContext::host(channel, inbox),
                    input_master_secret,
                    local_role,
                    input_options,
                    Some(activity),
                    require_manual,
                )
                .await
                {
                    tracing::error!(error = %err, ?epoch, "输入辅助会话失败");
                }
            });
            tasks.track(&task);
            runtime.input_task = Some(task);
        }
    }
    Ok(())
}

async fn wait_for_capability_ack(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => time::sleep_until(deadline).await,
        None => std::future::pending::<()>().await,
    }
}

fn report_capability_state(
    control: &RuntimeControl,
    peer: &RuntimePeerSummary,
    state: &CapabilityState,
) {
    control.report(RuntimeEvent::Capabilities {
        peer: peer.clone(),
        local: state.effective_local(),
        remote: state.effective_remote(),
        epoch: state.epoch(),
        acknowledged: state.is_local_acknowledged(),
    });
}

fn input_task_restart_required(
    previous: &InputRuntimeOptions,
    next: &InputRuntimeOptions,
    previous_backend_generation: u64,
    next_backend_generation: u64,
) -> bool {
    previous.path != next.path
        || previous.edge != next.edge
        || previous.hotkey != next.hotkey
        || previous.reverse_mouse_wheel != next.reverse_mouse_wheel
        || previous.reverse_trackpad != next.reverse_trackpad
        || previous.native_scroll_macos_to_windows != next.native_scroll_macos_to_windows
        || previous.native_scroll_windows_to_macos != next.native_scroll_windows_to_macos
        || previous.block_switch_on_press != next.block_switch_on_press
        || previous.filter_app_events != next.filter_app_events
        || previous.key_mapping != next.key_mapping
        || previous.cursor_mode != next.cursor_mode
        || previous_backend_generation != next_backend_generation
}

pub(crate) async fn run_sync_session(
    session: AuthenticatedSession,
    options: SyncSessionOptions<'_>,
) -> Result<()> {
    tracing::info!(
        peer = %identity_display_name(&session.remote),
        device_id = %short_uuid(&session.remote.device_id),
        remote_capabilities = %session.remote_capabilities.summary_lines().join(" | "),
        "同步会话已开始"
    );

    let _logical_owner = session.logical.owner()?;
    let logical = session.logical.clone();
    let mut secondary_inbox = session.secondary_inbox;
    let mut secondary = None;
    let (stream, channels) = synly_core::transport::bluetooth::open(session.stream);
    let mut bluetooth_channels = Some(channels);
    let primary_input_mux = bluetooth_channels.as_ref().map(|channels| channels.input.clone());
    let mut input_mux = primary_input_mux.clone();
    let mut input_transport = None;
    let mut remote_input_policy = synly_core::transport::routing::PathPolicy::PreferBluetooth;
    let mut remote_transport_state = input_route::available(session.transport, None);
    let mut remote_transport_generation = 0;
    let mut advertised_transport_state = None;
    let mut reported_transport_status = None;
    // 输入重建后"需按热键确认"的上一轮状态, 用于只在门槛上升沿提醒一次, 而不是每轮重复提醒.
    let mut input_reconfirm_announced = false;
    let mut transport_generation = 0u64;
    let primary_clipboard_lane = bluetooth_channels.as_mut().expect("已创建主承载").enable_clipboard_routes()?;
    let mut clipboard_route = ClipboardRoute::new(matches!(session.role, SessionRole::Host), options.transfer_limits);
    let mut remote_clipboard_policy = synly_core::transport::routing::PathPolicy::Auto;
    let capability_profile = options.capability_profile;
    let has_lan = session.remote_socket_addr.is_some();
    let has_mux_input = input_mux.is_some();
    let apply_capabilities = |capabilities| {
        let mut capabilities = capability_profile.apply(capabilities);
        if !has_lan { capabilities.audio_mode = AudioMode::Off; }
        if !has_lan && !has_mux_input { capabilities.input_mode = InputMode::Off; }
        capabilities
    };
    let initial_local_capabilities = apply_capabilities(RuntimeCapabilities {
        clipboard_mode: options.clipboard_mode,
        audio_mode: options.audio_mode,
        input_mode: options.input_mode,
    });
    let initial_remote_capabilities = RuntimeCapabilities {
        clipboard_mode: session.remote_capabilities.clipboard_mode,
        audio_mode: session.remote_capabilities.audio_mode,
        input_mode: session.remote_capabilities.input_mode,
    };
    let mut capability_state = CapabilityState::new(
        matches!(session.role, SessionRole::Host),
        initial_local_capabilities,
        initial_remote_capabilities,
    );
    let mut capabilities = options.control.capabilities();
    let mut tuning = options.control.tuning();
    let current_tuning = tuning.borrow_and_update().clone();
    let initial_input_tuning_changed = input_task_restart_required(
        &current_tuning.input,
        &options.input_options,
        current_tuning.input_backend_generation,
        current_tuning.input_backend_generation,
    );
    let mut clipboard_policy = current_tuning.clipboard.path;
    let mut input_options = current_tuning.input;
    let mut input_backend_generation = current_tuning.input_backend_generation;
    let shutdown = options.control.shutdown().clone();
    let session_shutdown = options.session_shutdown.clone();
    let mut capability_ack_deadline = None;
    let mut capabilities_open = true;
    let mut tuning_open = true;
    let remote_socket_addr = session.remote_socket_addr;
    let audio_master_secret = session.audio_master_secret;
    let input_master_secret = session.input_master_secret;
    let input_activity = options.control.input_activity();

    tracing::info!(
        clipboard = %clipboard_summary_line(
            session.role,
            options.clipboard_mode,
            session.remote_capabilities.clipboard_mode,
        ),
        audio = %audio_summary_line(options.audio_mode, initial_remote_capabilities.audio_mode),
        input = negotiate_input(options.input_mode, initial_remote_capabilities.input_mode)
            .map(|role| match role {
                LocalInputRole::Send => "本机发送控制",
                LocalInputRole::Receive => "本机接受控制",
            })
            .unwrap_or("未建立输入通道"),
        "运行时能力协商完成"
    );

    let (tx, mut incoming_frames, frame_io) = synly_core::transport::frames::open_control(stream, options.transfer_limits);
    let (tx, clipboard_sender_guard) = tx.routed_clipboard();
    let mut session_tasks = SessionTaskAbortGuard::default();
    if let Some(hub) = options.clipboard_hub.clone() {
        let rx = hub.subscribe(session.remote.device_id);
        let forward_tx = tx.clone();
        let task = tokio::spawn(async move {
            let mut rx = rx;
            while let Some(payload) = rx.recv().await {
                if forward_tx.send(Frame::Clipboard(payload)).await.is_err() {
                    break;
                }
            }
        });
        session_tasks.track(&task);
    }

    let mut audio_lan = None;
    let mut capability_runtime = CapabilityTaskRuntime::new(options.clipboard_options, session.role);
    refresh_capability_tasks(
        &capability_state,
        &mut capability_runtime,
        &mut session_tasks,
        CapabilityRefreshContext {
            session_role: session.role,
            peer_device_id: session.remote.device_id,
            input_mux: input_mux.as_ref(), input_transport,
            remote_socket_addr,
            audio_master_secret,
            audio_lan,
            audio_layout: options.audio_layout,
            input_master_secret,
            input_options: &input_options,
            input_inbox: options.input_inbox.as_ref(),
            input_session_id: options.input_session_id.as_ref(),
            input_socket_tx: options.input_socket_tx.as_ref(),
            input_routes: options.input_routes.as_ref(),
            input_activity: &input_activity,
            clipboard_hub: options.clipboard_hub.as_ref(),
            tx: &tx,
        },
    )
    .await?;
    let peer_summary = RuntimePeerSummary {
        device_id: session.remote.device_id,
        display_name: identity_display_name(&session.remote),
    };
    options
        .control
        .report(RuntimeEvent::Connected(RuntimePeerSummary {
            device_id: session.remote.device_id,
            display_name: identity_display_name(&session.remote),
        }));
    report_capability_state(&options.control, &peer_summary, &capability_state);
    let current_capabilities = *capabilities.borrow_and_update();
    let initial_update = capability_state
        .set_local(apply_capabilities(current_capabilities))
        .or_else(|| initial_input_tuning_changed.then(|| capability_state.bump_local()));
    if let Some((generation, capabilities)) = initial_update {
        tx.send(Frame::Control(ControlMessage::CapabilitiesUpdate {
            generation,
            capabilities,
        }))
        .await?;
        capability_ack_deadline = Some(Instant::now() + CAPABILITY_ACK_TIMEOUT);
        refresh_capability_tasks(
            &capability_state,
            &mut capability_runtime,
            &mut session_tasks,
            CapabilityRefreshContext {
                session_role: session.role,
                peer_device_id: session.remote.device_id,
                input_mux: input_mux.as_ref(), input_transport,
            remote_socket_addr,
                audio_master_secret,
                audio_lan,
                audio_layout: options.audio_layout,
                input_master_secret,
                input_options: &input_options,
                input_inbox: options.input_inbox.as_ref(),
                input_session_id: options.input_session_id.as_ref(),
                input_socket_tx: options.input_socket_tx.as_ref(),
                input_routes: options.input_routes.as_ref(),
                input_activity: &input_activity,
                clipboard_hub: options.clipboard_hub.as_ref(),
                tx: &tx,
            },
        )
        .await?;
        report_capability_state(&options.control, &peer_summary, &capability_state);
    }

    let mut clipboard_delivery = clipboard_delivery::Delivery::default();
    let disconnected = loop {
        let available = input_route::available(session.transport, secondary.as_ref());
        if advertised_transport_state != Some((available, input_options.path, clipboard_policy)) {
            transport_generation = transport_generation.checked_add(1).context("传输状态代次已耗尽")?;
            tx.send(Frame::Control(ControlMessage::TransportState { generation: transport_generation, available, input_policy: input_options.path, clipboard_policy })).await?;
            advertised_transport_state = Some((available, input_options.path, clipboard_policy));
        }
        capability_runtime.input_route.update_policy(input_options.path);
        let current = capability_runtime.input_role.and(capability_runtime.input_transport);
        // 正在控制对端时不因偏好变化切换承载: 切换会拆掉输入子流并强制重新热键确认.
        let switch_allowed = !input_activity.load(Ordering::Acquire);
        let (choice, lane) = input_route::select(session.transport, primary_input_mux.as_ref(), secondary.as_ref(), remote_transport_state, input_options.path, remote_input_policy, current, switch_allowed);
        input_transport = (remote_transport_generation > 0).then(|| choice.transport()).flatten();
        input_mux = if input_transport.is_some() { lane } else { None };
        audio_lan = audio_path::select(session.transport, remote_socket_addr, session.logical.id(), secondary.as_ref(), if remote_transport_generation > 0 { remote_transport_state } else { Default::default() });
        refresh_capability_tasks(&capability_state, &mut capability_runtime, &mut session_tasks, CapabilityRefreshContext {
            session_role: session.role, peer_device_id: session.remote.device_id, input_mux: input_mux.as_ref(), input_transport, remote_socket_addr,
            audio_master_secret, audio_lan, audio_layout: options.audio_layout, input_master_secret, input_options: &input_options,
            input_inbox: options.input_inbox.as_ref(), input_session_id: options.input_session_id.as_ref(),
            input_socket_tx: options.input_socket_tx.as_ref(), input_routes: options.input_routes.as_ref(),
            input_activity: &input_activity, clipboard_hub: options.clipboard_hub.as_ref(), tx: &tx,
        }).await?;
        if remote_transport_generation > 0 {
            clipboard_route.reconcile(ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links: remote_transport_state, policy: clipboard_policy, remote_policy: remote_clipboard_policy, tx: &tx }).await?;
        }
        let clipboard_deadline = clipboard_route.deadline();
        let audio_deadline = capability_runtime.audio_deadline;
        // 手动激活门槛只约束正在遥控对端的那台机器: 被控端从不读取这个门槛, 也没有热键要求,
        // 因此被控端既不提醒, 也不在状态栏显示确认提示, 避免给出无法照做的指引.
        let input_reconfirm_required = matches!(capability_runtime.input_role, Some(LocalInputRole::Send)) && capability_runtime.input_requires_manual;
        let status = crate::runtime_control::TransportStatus {
            primary: session.transport, available, input: choice,
            input_running: capability_runtime.input_task.is_some(),
            clipboard: clipboard_route.transport(),
            clipboard_choice: clipboard_route.choice(ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links: remote_transport_state, policy: clipboard_policy, remote_policy: remote_clipboard_policy, tx: &tx }),
            clipboard_switching: clipboard_route.switching(), clipboard_failed: clipboard_route.failed(),
            audio: capability_runtime.audio_task.as_ref().map(|_| TransportKind::Lan), audio_waiting: capability_runtime.audio_deadline.is_some(),
            audio_failed: capability_runtime.audio_blocked == Some((capability_state.epoch(), audio_lan)),
            audio_unavailable: audio_lan.is_none() && resolve_audio_plan(session.role, capability_state.effective_local().audio_mode, capability_state.effective_remote().audio_mode).is_some(),
            switching: capability_runtime.pending_mux_input.is_some(),
            failed: capability_runtime.input_blocked.is_some(),
            requires_manual_activation: input_reconfirm_required,
            // 运行时选项里保存的是已解析的 Hotkey, 界面需要可读文本.
            input_hotkey: input_options.hotkey.to_string(),
        };
        // 按上升沿提醒: 门槛每次从无到有都提示一次, 因为同一会话内可能因链路重建或改设置再次要求确认.
        // 若只在会话内提醒一次, 之后热键被改了也不会再告知, 用户就会以为提示里的键才是当前生效的键.
        if input_reconfirm_required && !input_reconfirm_announced {
            let hotkey = input_options.hotkey.to_string();
            let notifications_enabled = options.control.tuning().borrow().notifications_enabled;
            crate::system_notification::notify_input_reconfirmation(notifications_enabled, &hotkey);
            tracing::info!(hotkey = %hotkey, "输入路径已重建, 已提示用户按热键确认");
        }
        input_reconfirm_announced = input_reconfirm_required;
        // TransportStatus 含 String 已不再是 Copy, 因此这里一共只有两处按值使用, 都要照顾到:
        // 先克隆一份交给上报, 再把原值存进已上报状态, 否则第二处会用到已移动的值.
        if reported_transport_status.as_ref() != Some(&status) {
            options.control.report(RuntimeEvent::Transport { peer: peer_summary.clone(), status: status.clone() });
            reported_transport_status = Some(status);
        }
        let frame = tokio::select! {
            biased;
            result = audio_path::finish(&mut capability_runtime.audio_task) => {
                if let Err(error) = result { tracing::warn!(error = %error, "音频 LAN 任务结束"); }
                if let Some(message) = audio_path::fail(&mut capability_runtime).await { tx.send(Frame::Control(message)).await?; } continue;
            }
            _ = async { match audio_deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => {
                if let Some(message) = audio_path::fail(&mut capability_runtime).await { tx.send(Frame::Control(message)).await?; } continue;
            }
            receipt = clipboard_delivery.receipt() => { tx.send(Frame::Control(receipt)).await?; continue; }
            payload = clipboard_route.incoming() => {
                match payload {
                    Ok(transfer) => { if let Some(receipt) = clipboard_delivery.begin(transfer, capability_runtime.clipboard.can_receive, &capability_runtime.clipboard.sync, options.clipboard_hub.as_ref(), session.remote.device_id)? { tx.send(Frame::Control(receipt)).await?; } }
                    Err(error) => {
                        tracing::warn!(error = %error, "剪贴板子流失败");
                        clipboard_route.fail(ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links: remote_transport_state, policy: clipboard_policy, remote_policy: remote_clipboard_policy, tx: &tx }, true).await?;
                    }
                }
                continue;
            }
            _ = async { match clipboard_deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => {
                clipboard_route.fail(ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links: remote_transport_state, policy: clipboard_policy, remote_policy: remote_clipboard_policy, tx: &tx }, true).await?; continue;
            }
            _ = shutdown.cancelled() => {
                capability_runtime
                    .stop_all(
                        options.input_session_id.as_ref(),
                        options.input_routes.as_ref(),
                    )
                    .await;
                tx.send(Frame::Control(ControlMessage::Goodbye)).await?;
                break false;
            }
            _ = async {
                if let Some(token) = &session_shutdown {
                    token.cancelled().await;
                }
            }, if session_shutdown.is_some() => {
                capability_runtime
                    .stop_all(
                        options.input_session_id.as_ref(),
                        options.input_routes.as_ref(),
                    )
                    .await;
                tx.send(Frame::Control(ControlMessage::Goodbye)).await?;
                break false;
            }
            _ = wait_for_capability_ack(capability_ack_deadline), if capability_ack_deadline.is_some() => {
                bail!(
                    "capability generation {} ack timed out after {} seconds",
                    capability_state.local_generation(),
                    CAPABILITY_ACK_TIMEOUT.as_secs()
                );
            }
            changed = capabilities.changed(), if capabilities_open => {
                if changed.is_err() {
                    capabilities_open = false;
                    continue;
                }
                let next = *capabilities.borrow_and_update();
                if let Some((generation, capabilities)) = capability_state
                    .set_local(apply_capabilities(next))
                {
                    refresh_capability_tasks(
                        &capability_state,
                        &mut capability_runtime,
                        &mut session_tasks,
                        CapabilityRefreshContext {
                            session_role: session.role,
                            peer_device_id: session.remote.device_id,
                            input_mux: input_mux.as_ref(), input_transport,
            remote_socket_addr,
                            audio_master_secret,
                            audio_lan,
                            audio_layout: options.audio_layout,
                            input_master_secret,
                            input_options: &input_options,
                            input_inbox: options.input_inbox.as_ref(),
                            input_session_id: options.input_session_id.as_ref(),
                            input_socket_tx: options.input_socket_tx.as_ref(),
                            input_routes: options.input_routes.as_ref(),
                            input_activity: &input_activity,
                            clipboard_hub: options.clipboard_hub.as_ref(),
                            tx: &tx,
                        },
                    )
                    .await?;
                    report_capability_state(&options.control, &peer_summary, &capability_state);
                    tx.send(Frame::Control(ControlMessage::CapabilitiesUpdate {
                        generation,
                        capabilities,
                    }))
                    .await?;
                    capability_ack_deadline = Some(Instant::now() + CAPABILITY_ACK_TIMEOUT);
                }
                continue;
            }
            changed = tuning.changed(), if tuning_open => {
                if changed.is_err() {
                    tuning_open = false;
                    continue;
                }
                let next = tuning.borrow_and_update().clone();
                let input_changed = input_task_restart_required(
                    &input_options,
                    &next.input,
                    input_backend_generation,
                    next.input_backend_generation,
                );
                clipboard_policy = next.clipboard.path;
                if let Some(hub) = &options.clipboard_hub {
                    hub.update_options(next.clipboard.clone());
                } else {
                    capability_runtime
                        .clipboard
                        .sync
                        .update_options(next.clipboard)?;
                }
                input_options = next.input;
                if input_changed { capability_runtime.input_blocked = None; }
                input_backend_generation = next.input_backend_generation;
                if input_changed {
                    let (generation, capabilities) = capability_state.bump_local();
                    refresh_capability_tasks(
                        &capability_state,
                        &mut capability_runtime,
                        &mut session_tasks,
                        CapabilityRefreshContext {
                            session_role: session.role,
                            peer_device_id: session.remote.device_id,
                            input_mux: input_mux.as_ref(), input_transport,
            remote_socket_addr,
                            audio_master_secret,
                            audio_lan,
                            audio_layout: options.audio_layout,
                            input_master_secret,
                            input_options: &input_options,
                            input_inbox: options.input_inbox.as_ref(),
                            input_session_id: options.input_session_id.as_ref(),
                            input_socket_tx: options.input_socket_tx.as_ref(),
                            input_routes: options.input_routes.as_ref(),
                            input_activity: &input_activity,
                            clipboard_hub: options.clipboard_hub.as_ref(),
                            tx: &tx,
                        },
                    )
                    .await?;
                    report_capability_state(&options.control, &peer_summary, &capability_state);
                    tx.send(Frame::Control(ControlMessage::CapabilitiesUpdate {
                        generation,
                        capabilities,
                    }))
                    .await?;
                    capability_ack_deadline = Some(Instant::now() + CAPABILITY_ACK_TIMEOUT);
                }
                continue;
            }
            _ = wait_for_capability_ack(capability_runtime.pending_mux_input.as_ref().map(|pending| pending.2)) => {
                input_route::fail(&mut capability_runtime, &capability_state, CapabilityRefreshContext {
                    session_role: session.role, peer_device_id: session.remote.device_id, input_mux: input_mux.as_ref(), input_transport, remote_socket_addr,
                    audio_master_secret, audio_lan, audio_layout: options.audio_layout, input_master_secret, input_options: &input_options,
                    input_inbox: options.input_inbox.as_ref(), input_session_id: options.input_session_id.as_ref(),
                    input_socket_tx: options.input_socket_tx.as_ref(), input_routes: options.input_routes.as_ref(),
                    input_activity: &input_activity, clipboard_hub: options.clipboard_hub.as_ref(), tx: &tx,
                }, true).await?;
                continue;
            }
            _ = input_route::task_finished(&mut capability_runtime.input_task) => {
                input_route::fail(&mut capability_runtime, &capability_state, CapabilityRefreshContext {
                    session_role: session.role, peer_device_id: session.remote.device_id, input_mux: input_mux.as_ref(), input_transport, remote_socket_addr,
                    audio_master_secret, audio_lan, audio_layout: options.audio_layout, input_master_secret, input_options: &input_options,
                    input_inbox: options.input_inbox.as_ref(), input_session_id: options.input_session_id.as_ref(),
                    input_socket_tx: options.input_socket_tx.as_ref(), input_routes: options.input_routes.as_ref(),
                    input_activity: &input_activity, clipboard_hub: options.clipboard_hub.as_ref(), tx: &tx,
                }, true).await?;
                continue;
            }
            candidate = synly_core::transport::logical::receive_secondary(&mut secondary_inbox) => {
                match candidate {
                    Some(candidate) => {
                        let transport = candidate.guard.transport();
                        if !logical.has(transport) || transport == logical.primary() || secondary.is_some() { bail!("候选链路未绑定或已存在副承载"); }
                        let mut tunnel = candidate.multiplex(); tunnel.channels.enable_clipboard_routes()?;
                        secondary = Some(tunnel);
                        tracing::info!(?transport, session = %logical.id(), "副承载已加入当前逻辑会话, 控制路径保持不变");
                    }
                    None => secondary_inbox = None,
                }
                continue;
            }
            error = synly_core::transport::logical::secondary_failure(&mut secondary) => {
                let transport = secondary.as_ref().map(|secondary| secondary.transport());
                secondary.take();
                tracing::warn!(?transport, %error, "副承载已经移除, 主会话继续运行");
                continue;
            }
            error = synly_core::transport::bluetooth::wait_failure(&mut bluetooth_channels) => {
                bail!("主承载复用失败: {error}");
            }
            incoming = incoming_frames.recv() => {
                match incoming {
                    Some(Ok(frame)) => frame,
                    Some(Err(err)) if is_connection_shutdown_error(&err) => break true,
                    Some(Err(err)) => return Err(err),
                    None => break true,
                }
            }
        };

        match frame {
            Frame::Control(ControlMessage::CapabilitiesUpdate {
                generation,
                capabilities,
            }) => {
                let changed = capability_state.apply_remote(generation, capabilities)?;
                tx.send(Frame::Control(ControlMessage::CapabilitiesAck {
                    generation,
                }))
                .await?;
                if changed {
                    refresh_capability_tasks(
                        &capability_state,
                        &mut capability_runtime,
                        &mut session_tasks,
                        CapabilityRefreshContext {
                            session_role: session.role,
                            peer_device_id: session.remote.device_id,
                            input_mux: input_mux.as_ref(), input_transport,
            remote_socket_addr,
                            audio_master_secret,
                            audio_lan,
                            audio_layout: options.audio_layout,
                            input_master_secret,
                            input_options: &input_options,
                            input_inbox: options.input_inbox.as_ref(),
                            input_session_id: options.input_session_id.as_ref(),
                            input_socket_tx: options.input_socket_tx.as_ref(),
                            input_routes: options.input_routes.as_ref(),
                            input_activity: &input_activity,
                            clipboard_hub: options.clipboard_hub.as_ref(),
                            tx: &tx,
                        },
                    )
                    .await?;
                }
                report_capability_state(&options.control, &peer_summary, &capability_state);
            }
            Frame::Control(ControlMessage::CapabilitiesAck { generation }) => {
                if capability_state.apply_ack(generation)? {
                    capability_ack_deadline = None;
                    refresh_capability_tasks(
                        &capability_state,
                        &mut capability_runtime,
                        &mut session_tasks,
                        CapabilityRefreshContext {
                            session_role: session.role,
                            peer_device_id: session.remote.device_id,
                            input_mux: input_mux.as_ref(), input_transport,
            remote_socket_addr,
                            audio_master_secret,
                            audio_lan,
                            audio_layout: options.audio_layout,
                            input_master_secret,
                            input_options: &input_options,
                            input_inbox: options.input_inbox.as_ref(),
                            input_session_id: options.input_session_id.as_ref(),
                            input_socket_tx: options.input_socket_tx.as_ref(),
                            input_routes: options.input_routes.as_ref(),
                            input_activity: &input_activity,
                            clipboard_hub: options.clipboard_hub.as_ref(),
                            tx: &tx,
                        },
                    )
                    .await?;
                    report_capability_state(&options.control, &peer_summary, &capability_state);
                }
            }
            Frame::Control(ControlMessage::TransportState { generation, available, input_policy, clipboard_policy }) => {
                if generation == 0 || !available.contains(session.transport) { bail!("对端传输状态代次或主承载无效"); }
                if generation == remote_transport_generation && (available != remote_transport_state || input_policy != remote_input_policy || clipboard_policy != remote_clipboard_policy) { bail!("同一传输代次收到冲突状态"); }
                if generation > remote_transport_generation {
                    if available != remote_transport_state || input_policy != remote_input_policy { capability_runtime.input_blocked = None; }
                    remote_transport_generation = generation; remote_transport_state = available; remote_input_policy = input_policy; remote_clipboard_policy = clipboard_policy;
                }
            }
            Frame::Control(ControlMessage::InputPath { epoch, generation, message }) => {
                input_route::receive(epoch, generation, message, &capability_state, &mut capability_runtime, &mut session_tasks, CapabilityRefreshContext {
                    session_role: session.role, peer_device_id: session.remote.device_id, input_mux: input_mux.as_ref(), input_transport, remote_socket_addr,
                    audio_master_secret, audio_lan, audio_layout: options.audio_layout, input_master_secret, input_options: &input_options,
                    input_inbox: options.input_inbox.as_ref(), input_session_id: options.input_session_id.as_ref(),
                    input_socket_tx: options.input_socket_tx.as_ref(), input_routes: options.input_routes.as_ref(),
                    input_activity: &input_activity, clipboard_hub: options.clipboard_hub.as_ref(), tx: &tx,
                }).await?;
            }
            Frame::Control(ControlMessage::InputPathFailed { epoch, generation }) => {
                if capability_state.current_epoch(epoch) && capability_runtime.input_generation == Some(generation) {
                    input_route::fail(&mut capability_runtime, &capability_state, CapabilityRefreshContext {
                        session_role: session.role, peer_device_id: session.remote.device_id, input_mux: input_mux.as_ref(), input_transport, remote_socket_addr,
                        audio_master_secret, audio_lan, audio_layout: options.audio_layout, input_master_secret, input_options: &input_options,
                        input_inbox: options.input_inbox.as_ref(), input_session_id: options.input_session_id.as_ref(),
                        input_socket_tx: options.input_socket_tx.as_ref(), input_routes: options.input_routes.as_ref(),
                        input_activity: &input_activity, clipboard_hub: options.clipboard_hub.as_ref(), tx: &tx,
                    }, false).await?;
                }
            }
            Frame::Control(ControlMessage::InputMuxOffer { .. } | ControlMessage::InputMuxReady { .. }) => { bail!("不再接受未经四阶段提交的输入复用协商"); }
            Frame::Control(ControlMessage::InputChannelOffer { epoch, offer }) => {
                if input_transport != Some(TransportKind::Lan) || input_mux.is_some() { continue; }
                if !capability_state.current_epoch(epoch) {
                    tracing::debug!(?epoch, current = ?capability_state.epoch(), "忽略过期输入辅助通道");
                    continue;
                }
                if matches!(session.role, SessionRole::Host) {
                    bail!("host 输入会话收到对侧辅助通道 offer");
                }
                let Some(input_address) = remote_socket_addr else {
                    tracing::warn!("蓝牙会话拒绝 TCP 输入辅助通道 offer");
                    continue;
                };
                let local = capability_state.effective_local();
                let remote = capability_state.effective_remote();
                let Some(local_role) = negotiate_input(local.input_mode, remote.input_mode) else {
                    tracing::debug!(?epoch, "忽略未协商的输入辅助通道");
                    continue;
                };
                if capability_runtime.input_task.is_some() {
                    bail!("当前 capability epoch 收到重复输入辅助通道 offer");
                }
                let mut task_input_options = input_options.clone();
                task_input_options.mode = local.input_mode;
                let activity_for_input = Arc::clone(&input_activity);
                let require_manual = capability_runtime.input_requires_manual;
                let task = tokio::spawn(async move {
                    if let Err(err) = input::run_input_session_with_gate(
                        InputSessionContext::client(offer, input_address),
                        input_master_secret,
                        local_role,
                        task_input_options,
                        Some(activity_for_input),
                        require_manual,
                    )
                    .await
                    {
                        tracing::error!(error = %err, ?epoch, "输入辅助会话失败");
                    }
                });
                session_tasks.track(&task);
                capability_runtime.input_task = Some(task);
            }
            Frame::Control(ControlMessage::AudioPathFailed { epoch, path_id }) => {
                if capability_state.current_epoch(epoch) && capability_runtime.audio_lan.is_some_and(|path| path.binding_id == path_id) {
                    let _ = audio_path::fail(&mut capability_runtime).await;
                }
            }
            Frame::Control(ControlMessage::AudioUdpReady { epoch, path_id, port, layout, channel_id }) => {
                audio_path::receive(audio_path::Ready { epoch, path_id, port, layout, channel_id }, &capability_state, &mut capability_runtime, audio_master_secret)?;
            }
            Frame::Control(ControlMessage::Error { message }) => {
                tracing::warn!(%message, "对端报告错误");
            }
            Frame::Control(ControlMessage::Goodbye) => {
                break true;
            }
            Frame::Control(ControlMessage::BootstrapHello { .. })
            | Frame::Control(ControlMessage::BootstrapChallenge { .. })
            | Frame::Control(ControlMessage::BootstrapPake { .. })
            | Frame::Control(ControlMessage::BootstrapAck { .. })
            | Frame::Control(ControlMessage::PairRequest { .. })
            | Frame::Control(ControlMessage::PinChallenge { .. })
            | Frame::Control(ControlMessage::PairAuth { .. })
            | Frame::Control(ControlMessage::PairDecision { .. }) => {
                bail!("received an unexpected pairing message after session start")
            }
            Frame::Control(ControlMessage::ClipboardPath { epoch, generation, message }) => {
                clipboard_route.receive(epoch, generation, message, ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links: remote_transport_state, policy: clipboard_policy, remote_policy: remote_clipboard_policy, tx: &tx }).await?;
            }
            Frame::Control(ControlMessage::ClipboardPathFailed { epoch, generation }) => {
                clipboard_route.remote_failed(epoch, generation, ClipboardRouteContext { capabilities: &capability_state, primary: session.transport, primary_lane: &primary_clipboard_lane, secondary: secondary.as_ref().and_then(ClipboardRouteContext::secondary), remote_links: remote_transport_state, policy: clipboard_policy, remote_policy: remote_clipboard_policy, tx: &tx }).await?;
            }
            Frame::Control(ControlMessage::ClipboardApplied { stamp }) => tx.clipboard_receipt(stamp, true),
            Frame::Control(ControlMessage::ClipboardRejected { stamp }) => tx.clipboard_receipt(stamp, false),
            Frame::ClipboardTransfer(_) => bail!("剪贴板载荷不能通过主控制子流发送"),
            Frame::Clipboard(payload) => {
                if !capability_runtime.clipboard.can_receive {
                    continue;
                }
                if let Some(hub) = &options.clipboard_hub {
                    hub.ingest(session.remote.device_id, payload);
                } else if let Err(err) = capability_runtime
                    .clipboard
                    .sync
                    .apply_remote_payload(payload)
                    .await
                {
                    tracing::warn!(error = %err, "无法应用远端剪贴板内容");
                }
            }
        }
    };

    if let Some(hub) = &options.clipboard_hub {
        hub.set_receive_enabled(session.remote.device_id, false);
        hub.unsubscribe(session.remote.device_id);
    }
    capability_runtime
        .stop_all(
            options.input_session_id.as_ref(),
            options.input_routes.as_ref(),
        )
        .await;
    drop(clipboard_sender_guard);
    drop(tx);
    match frame_io.finish().await {
        Ok(()) => {},
        Err(error) if disconnected && is_connection_shutdown_error(&error) => {},
        Err(error) => return Err(error),
    }
    Ok(())
}

async fn clipboard_sender_loop(
    mut rx: mpsc::UnboundedReceiver<ClipboardPayload>,
    tx: FrameSender,
) -> Result<()> {
    while let Some(payload) = rx.recv().await {
        tx.send(Frame::Clipboard(payload)).await?;
    }
    Ok(())
}

async fn choose_peer(
    peer_query: Option<&str>,
    timeout: Duration,
    _headless: bool,
    local_capabilities: &RuntimeCapabilities,
    discovery_config: &crate::config::DiscoveryConfig,
) -> Result<PeerTarget> {
    let query = require_peer_query(peer_query)?;
    if let Some((address, device_id)) = bluetooth::parse_target(query)? {
        return Ok(PeerTarget::Bluetooth { address, device_id });
    }
    if let Some(address) = parse_direct_peer_addr(query) {
        return Ok(PeerTarget::Direct(address));
    }
    let peers = discovery::browse(timeout, discovery_config).await?;
    let peer = select_peer_from_query(&peers, query)?;
    discovered_peer_target(peer, local_capabilities)
}

pub(crate) fn known_peer_for_query(
    peers: &[DiscoveredPeer],
    query: &str,
) -> Option<DiscoveredPeer> {
    let query = query.trim();
    if query.is_empty() || parse_direct_peer_addr(query).is_some() {
        return None;
    }
    select_peer_from_query(peers, query)
        .ok()
        .filter(|peer| !peer.addresses.is_empty())
}

fn discovered_peer_target(
    peer: DiscoveredPeer,
    local_capabilities: &RuntimeCapabilities,
) -> Result<PeerTarget> {
    if peer.protocol_version != PROTOCOL_VERSION {
        bail!(
            "设备协议版本不兼容: 本机 {}, 对侧 {}",
            PROTOCOL_VERSION,
            peer.protocol_version
        );
    }
    ensure_discovered_peer_modes_match(&peer, local_capabilities)?;
    Ok(PeerTarget::Discovered(peer))
}

fn parse_direct_peer_addr(query: &str) -> Option<SocketAddrV4> {
    query.trim().parse().ok()
}

fn select_peer_from_query(peers: &[DiscoveredPeer], query: &str) -> Result<DiscoveredPeer> {
    let mut logical_matches = BTreeMap::<String, DiscoveredPeer>::new();
    for peer in peers
        .iter()
        .filter(|peer| peer_matches_query(peer, query))
        .cloned()
    {
        match logical_matches.get_mut(&peer.device_id) {
            Some(current) if current.port == peer.port => {
                current.addresses.extend(peer.addresses);
                current.addresses.sort();
                current.addresses.dedup();
                if discovery_source_priority(peer.source)
                    > discovery_source_priority(current.source)
                {
                    current.source = peer.source;
                }
            }
            Some(current)
                if discovery_source_priority(peer.source)
                    > discovery_source_priority(current.source) =>
            {
                *current = peer;
            }
            Some(_) => {}
            None => {
                logical_matches.insert(peer.device_id.clone(), peer);
            }
        }
    }
    let matches = logical_matches.into_values().collect::<Vec<_>>();

    match matches.len() {
        0 => bail!("没有找到匹配 `{query}` 的设备"),
        1 => Ok(matches[0].clone()),
        _ => {
            let labels = matches
                .iter()
                .map(DiscoveredPeer::label)
                .collect::<Vec<_>>()
                .join("\n");
            bail!(
                "`{query}` 匹配到多个设备，请改用更精确的实例名、设备名、设备 ID 前缀或 IPv4 地址:\n{labels}"
            )
        }
    }
}

fn discovery_source_priority(source: discovery::DiscoverySource) -> u8 {
    match source {
        discovery::DiscoverySource::Lnd => 0,
        discovery::DiscoverySource::Mdns => 1,
        discovery::DiscoverySource::MdnsAndLnd => 2,
    }
}

fn peer_matches_query(peer: &DiscoveredPeer, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return false;
    }

    peer.instance_name
        .as_deref()
        .is_some_and(|instance_name| instance_name.eq_ignore_ascii_case(query))
        || peer.device_name.eq_ignore_ascii_case(query)
        || peer.device_id.eq_ignore_ascii_case(query)
        || peer
            .device_id
            .to_ascii_lowercase()
            .starts_with(&query.to_ascii_lowercase())
        || peer.addresses.iter().any(|address| {
            address.to_string() == query || format!("{address}:{}", peer.port) == query
        })
}

fn preferred_peer_query(peer: &DiscoveredPeer) -> String {
    peer.instance_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case(&peer.device_name))
        .unwrap_or(&peer.device_id)
        .to_string()
}

fn discovered_peer_mode_mismatch_message(
    peer: &DiscoveredPeer,
    local_capabilities: &RuntimeCapabilities,
) -> Option<String> {
    let clipboard_agreement =
        negotiate_clipboard(peer.clipboard_mode, local_capabilities.clipboard_mode);
    let audio_compatible = audio_modes_compatible(peer.audio_mode, local_capabilities.audio_mode);
    let input_compatible = negotiate_input(peer.input_mode, local_capabilities.input_mode).is_some();
    if clipboard_agreement.any_direction()
        || audio_compatible
        || input_compatible
    {
        return None;
    }

    Some(format!(
        "找到设备 {}, 但同步模式不匹配: 对端广播为 剪贴板:{} / 音频:{} / 输入:{}; 本机为 剪贴板:{} / 音频:{} / 输入:{}. 当前没有任何可用同步方向.",
        peer.display_name(),
        peer.clipboard_mode.label(),
        peer.audio_mode.label(),
        peer.input_mode.label(),
        local_capabilities.clipboard_mode.label(),
        local_capabilities.audio_mode.label(),
        local_capabilities.input_mode.label(),
    ))
}

fn ensure_discovered_peer_modes_match(
    peer: &DiscoveredPeer,
    local_capabilities: &RuntimeCapabilities,
) -> Result<()> {
    if let Some(message) = discovered_peer_mode_mismatch_message(peer, local_capabilities) {
        bail!("{message}");
    }
    Ok(())
}

pub(crate) fn identity_display_name(identity: &DeviceIdentity) -> String {
    format_display_name(identity.instance_name.as_deref(), &identity.device_name)
}

fn trusted_transport_for_peer(
    config: &SynlyConfig,
    peer: &DiscoveredPeer,
) -> Result<Option<TrustedDeviceConfig>> {
    let device_id = Uuid::parse_str(&peer.device_id)
        .with_context(|| format!("peer advertised an invalid device id: {}", peer.device_id))?;
    Ok(trusted_transport_for_device(config, &device_id))
}

fn trusted_transport_for_identity(
    config: &SynlyConfig,
    identity: &DeviceIdentity,
) -> Result<TrustedDeviceConfig> {
    if let Some(trusted_device) = trusted_transport_for_device(config, &identity.device_id) {
        return Ok(trusted_device);
    }
    if config.trusted_device(&identity.device_id).is_some() {
        bail!(
            "设备 `{}` 已记录身份，但尚未具备完整的长期 mTLS 信任材料",
            identity_display_name(identity)
        );
    }
    bail!(
        "设备 `{}` 尚未被本机信任, 不能使用 trusted_only 直连",
        identity_display_name(identity)
    );
}

fn trusted_transport_for_device(
    config: &SynlyConfig,
    device_id: &Uuid,
) -> Option<TrustedDeviceConfig> {
    config.trusted_devices.iter().find_map(|device| {
        (device.device_id == *device_id
            && !device.public_key.trim().is_empty()
            && !device.tls_root_certificate.trim().is_empty())
        .then(|| device.clone())
    })
}

async fn read_frame<R>(reader: &mut R, transfer_limits: TransferLimits) -> Result<Frame>
where
    R: AsyncRead + Unpin,
{
    FrameReader::with_limits(reader, transfer_limits)
        .read_frame()
        .await
}

async fn write_frame<W>(writer: &mut W, transfer_limits: TransferLimits, frame: Frame) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    FrameWriter::with_limits(writer, transfer_limits)
        .write_frame(frame)
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
    time::timeout(timeout, read_frame(reader, transfer_limits))
        .await
        .map_err(|_| anyhow!("等待对端响应超时"))?
}

struct HostPinPrompt {
    control: RuntimeControl,
    clear_on_drop: bool,
}

impl HostPinPrompt {
    fn watch(
        control: &RuntimeControl,
        request: InteractionRequest,
    ) -> Result<(Self, oneshot::Receiver<InteractionResponse>)> {
        let cancel_rx = control.watch_interaction(request)?;
        Ok((
            Self {
                control: control.clone(),
                clear_on_drop: true,
            },
            cancel_rx,
        ))
    }

    fn persist(&mut self) {
        self.clear_on_drop = false;
    }
}

impl Drop for HostPinPrompt {
    fn drop(&mut self) {
        if self.clear_on_drop {
            self.control.notify_interaction(InteractionRequest::Clear {
                request_id: Uuid::new_v4(),
            });
        }
    }
}

async fn wait_or_cancel_pairing<T>(
    work: impl Future<Output = Result<T>>,
    cancel_rx: &mut oneshot::Receiver<InteractionResponse>,
) -> Result<Option<T>> {
    tokio::select! {
        result = work => result.map(Some),
        resp = cancel_rx => {
            match resp {
                Ok(InteractionResponse::Cancel) | Err(_) => {
                    tracing::info!("用户取消了配对");
                    Ok(None)
                }
                Ok(other) => {
                    tracing::debug!(?other, "配对等待收到非取消响应");
                    Ok(None)
                }
            }
        }
    }
}

fn pairing_cancel_received(cancel_rx: &mut oneshot::Receiver<InteractionResponse>) -> bool {
    match cancel_rx.try_recv() {
        Ok(InteractionResponse::Cancel) | Err(oneshot::error::TryRecvError::Closed) => {
            tracing::info!("用户取消了配对");
            true
        }
        Ok(_) | Err(oneshot::error::TryRecvError::Empty) => false,
    }
}

fn is_connection_shutdown_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>().is_some_and(|io_err| {
        matches!(
            io_err.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::TimedOut
        )
    })
}

async fn register_pairing_failure(pairing_throttle: &mut PairingThrottle, peer_key: &str) {
    let backoff = pairing_throttle.note_failure(peer_key);
    if !backoff.is_zero() {
        time::sleep(backoff).await;
    }
}

impl PairingThrottle {
    fn blocked_remaining(&mut self, peer_key: &str) -> Option<Duration> {
        let now = Instant::now();
        let state = self.peers.get(peer_key)?;
        if now.duration_since(state.window_started_at) > PAIRING_FAILURE_WINDOW {
            self.peers.remove(peer_key);
            return None;
        }
        match state.blocked_until {
            Some(blocked_until) if blocked_until > now => Some(blocked_until.duration_since(now)),
            _ => None,
        }
    }

    fn note_failure(&mut self, peer_key: &str) -> Duration {
        let now = Instant::now();
        let state = self
            .peers
            .entry(peer_key.to_string())
            .or_insert(PairingPeerState {
                failures: 0,
                window_started_at: now,
                blocked_until: None,
            });
        if now.duration_since(state.window_started_at) > PAIRING_FAILURE_WINDOW {
            state.failures = 0;
            state.window_started_at = now;
            state.blocked_until = None;
        }
        state.failures = state.failures.saturating_add(1);
        if state.failures >= PAIRING_MAX_FAILURES {
            state.blocked_until = Some(now + PAIRING_COOLDOWN);
        }
        Duration::from_millis(
            PAIRING_BACKOFF_BASE_MS.saturating_mul(u64::from(state.failures.min(4))),
        )
    }

    fn note_success(&mut self, peer_key: &str) {
        self.peers.remove(peer_key);
    }
}

fn has_trusted_transport(config: &SynlyConfig) -> bool {
    config.trusted_devices.iter().any(|device| {
        !device.public_key.trim().is_empty() && !device.tls_root_certificate.trim().is_empty()
    })
}

fn should_try_direct_trusted(config: &SynlyConfig, pairing: &PairingRuntimeOptions) -> bool {
    pairing.trusted_only || has_trusted_transport(config)
}

fn has_trusted_transport_for_device(config: &SynlyConfig, device_id: &Uuid) -> bool {
    trusted_transport_for_device(config, device_id).is_some()
}

fn print_pair_request_overview(
    payload: &PairRequestPayload,
    options: &RuntimeOptions,
    remote_addr: &str,
) -> Result<()> {
    let remote_summary = payload.capabilities.summary_lines().join(" | ");
    let local_summary = RuntimeCapabilities {
        clipboard_mode: options.clipboard_mode,
        audio_mode: options.audio_mode,
        input_mode: options.input_mode,
    }.summary_lines().join(" | ");
    tracing::info!(
        peer = %identity_display_name(&payload.client),
        device_id = %short_uuid(&payload.client.device_id),
        fingerprint = %crypto::short_identity_fingerprint(&payload.client.identity_public_key)?,
        remote_addr,
        remote_summary = %remote_summary,
        local_summary = %local_summary,
        clipboard = %clipboard_summary_line(
            SessionRole::Host,
            options.clipboard_mode,
            payload.capabilities.clipboard_mode,
        ),
        input = %input_summary_line(options.input_mode, payload.capabilities.input_mode),
        "收到同步请求"
    );
    Ok(())
}

fn print_connected_peer(
    remote: &DeviceIdentity,
    remote_capabilities: &RuntimeCapabilities,
    local_input_mode: InputMode,
) -> Result<()> {
    tracing::info!(
        peer = %identity_display_name(remote),
        device_id = %short_uuid(&remote.device_id),
        fingerprint = %crypto::short_identity_fingerprint(&remote.identity_public_key)?,
        audio = remote_capabilities.audio_mode.label(),
        input = %input_summary_line(local_input_mode, remote_capabilities.input_mode),
        "连接已建立"
    );
    Ok(())
}

fn allows_local_send(role: SessionRole, agreement: &SessionAgreement) -> bool {
    match role {
        SessionRole::Host => agreement.host_to_client,
        SessionRole::Client => agreement.client_to_host,
    }
}

fn allows_local_receive(role: SessionRole, agreement: &SessionAgreement) -> bool {
    match role {
        SessionRole::Host => agreement.client_to_host,
        SessionRole::Client => agreement.host_to_client,
    }
}

fn signed_pair_decision(params: PairDecisionParams<'_>) -> Result<ControlMessage> {
    let summary = RuntimeCapabilities {
        clipboard_mode: params.clipboard_mode,
        audio_mode: params.audio_mode,
        input_mode: params.input_mode,
    };
    let server = device_identity(params.device, params.instance_name);
    let proof = match params.auth_method {
        PairAuthMethod::Pin => crypto::sign_pair_decision(
            params.exporter,
            params.request_id,
            params.pin.context("missing PIN for pair decision")?,
            params.accepted,
            &params.message,
            &server,
            params.clipboard_agreement,
            &summary,
            params.auth_method,
            params.server_trusts_client,
            params.trust_established,
        )?,
        PairAuthMethod::TrustedDevice => crypto::sign_trusted_pair_decision(
            params.device.identity_private_key()?,
            params.exporter,
            params.request_id,
            params.accepted,
            &params.message,
            &server,
            params.clipboard_agreement,
            &summary,
            params.server_trusts_client,
            params.trust_established,
        )?,
    };
    Ok(ControlMessage::PairDecision {
        accepted: params.accepted,
        message: params.message,
        server,
        capabilities: summary,
        clipboard_agreement: params.clipboard_agreement.clone(),
        auth_method: params.auth_method,
        server_trusts_client: params.server_trusts_client,
        proof,
        trust_established: params.trust_established,
    })
}

fn device_identity(device: &DeviceConfig, instance_name: Option<&str>) -> DeviceIdentity {
    DeviceIdentity {
        device_id: device.device_id,
        device_name: device.device_name.clone(),
        instance_name: instance_name.map(ToString::to_string),
        identity_public_key: device
            .identity_public_key()
            .expect("device identity public key is missing")
            .to_string(),
        tls_root_certificate: crypto::device_tls_root_certificate(device)
            .expect("device TLS root certificate generation failed"),
    }
}

pub(crate) fn print_host_ready(device: &DeviceConfig, options: &RuntimeOptions, port: u16) {
    let fingerprint = crypto::short_identity_fingerprint(
        device
            .identity_public_key()
            .expect("device identity public key is missing"),
    )
    .expect("device identity fingerprint is invalid");
    let local_summary = RuntimeCapabilities {
        clipboard_mode: options.clipboard_mode,
        audio_mode: options.audio_mode,
        input_mode: options.input_mode,
    }.summary_lines().join(" | ");
    let pairing_policy = if options.pairing.trusted_only {
        "仅可信设备"
    } else {
        "可信设备使用长期 mTLS, 未信任设备使用 bootstrap + PIN + 临时 mTLS"
    };
    tracing::info!(
        device = %device.device_name,
        device_id = %device.short_id(),
        instance = options.instance_name.as_deref().unwrap_or(""),
        %fingerprint,
        %local_summary,
        pairing_policy,
        accept_policy = accept_policy_label(&options.pairing),
        fixed_pin = options.pairing.pin.is_some(),
        port,
        fixed_port = options.pairing.port.is_some(),
        "Synly host 已就绪"
    );
}

fn direction_label(role: SessionRole, agreement: &SessionAgreement) -> &'static str {
    match (
        allows_local_send(role, agreement),
        allows_local_receive(role, agreement),
    ) {
        (true, true) => "双向同步",
        (true, false) => "本机 -> 对端",
        (false, true) => "对端 -> 本机",
        (false, false) => "无可用同步方向",
    }
}

fn negotiate_sync_directions(
    host_can_send: bool,
    host_can_receive: bool,
    client_can_send: bool,
    client_can_receive: bool,
) -> SessionAgreement {
    SessionAgreement {
        host_to_client: host_can_send && client_can_receive,
        client_to_host: client_can_send && host_can_receive,
    }
}

fn negotiate_clipboard(host_mode: ClipboardMode, client_mode: ClipboardMode) -> SessionAgreement {
    negotiate_sync_directions(
        host_mode.can_send(),
        host_mode.can_receive(),
        client_mode.can_send(),
        client_mode.can_receive(),
    )
}

fn negotiate_clipboard_modes(
    role: SessionRole,
    local_mode: ClipboardMode,
    remote_mode: ClipboardMode,
) -> SessionAgreement {
    match role {
        SessionRole::Host => negotiate_clipboard(local_mode, remote_mode),
        SessionRole::Client => negotiate_clipboard(remote_mode, local_mode),
    }
}

fn clipboard_summary_line(
    role: SessionRole,
    local_mode: ClipboardMode,
    remote_mode: ClipboardMode,
) -> String {
    let agreement = negotiate_clipboard_modes(role, local_mode, remote_mode);
    if agreement.any_direction() {
        return format!("本次剪贴板同步: {}", direction_label(role, &agreement));
    }

    match (local_mode, remote_mode) {
        (ClipboardMode::Off, ClipboardMode::Off) => "本次剪贴板同步: 关闭".to_string(),
        (ClipboardMode::Off, _) => "本次剪贴板同步: 本机未开启".to_string(),
        (_, ClipboardMode::Off) => "本次剪贴板同步: 对端未开启".to_string(),
        _ => "本次剪贴板同步: 方向不兼容，不会同步".to_string(),
    }
}

fn audio_modes_compatible(host_mode: AudioMode, client_mode: AudioMode) -> bool {
    matches!(
        (host_mode, client_mode),
        (AudioMode::Send, AudioMode::Receive) | (AudioMode::Receive, AudioMode::Send)
    )
}

fn resolve_audio_plan(
    role: SessionRole,
    local_audio_mode: AudioMode,
    remote_audio_mode: AudioMode,
) -> Option<AudioPlan> {
    match (local_audio_mode, remote_audio_mode, role) {
        (AudioMode::Send, AudioMode::Receive, SessionRole::Host) => Some(AudioPlan {
            role: LocalAudioRole::Send,
            direction: AudioChannelDirection::HostToClient,
        }),
        (AudioMode::Send, AudioMode::Receive, SessionRole::Client) => Some(AudioPlan {
            role: LocalAudioRole::Send,
            direction: AudioChannelDirection::ClientToHost,
        }),
        (AudioMode::Receive, AudioMode::Send, SessionRole::Host) => Some(AudioPlan {
            role: LocalAudioRole::Receive,
            direction: AudioChannelDirection::ClientToHost,
        }),
        (AudioMode::Receive, AudioMode::Send, SessionRole::Client) => Some(AudioPlan {
            role: LocalAudioRole::Receive,
            direction: AudioChannelDirection::HostToClient,
        }),
        _ => None,
    }
}

fn audio_summary_line(local_audio_mode: AudioMode, remote_audio_mode: AudioMode) -> String {
    match (local_audio_mode, remote_audio_mode) {
        (AudioMode::Off, AudioMode::Off) => "本次音频同步: 关闭".to_string(),
        (AudioMode::Off, _) => "本次音频同步: 本机未开启".to_string(),
        (_, AudioMode::Off) => "本次音频同步: 对端未开启".to_string(),
        (AudioMode::Send, AudioMode::Receive) => "本次音频同步: 本机 -> 对端".to_string(),
        (AudioMode::Receive, AudioMode::Send) => "本次音频同步: 对端 -> 本机".to_string(),
        (AudioMode::Send, AudioMode::Send) => {
            "本次音频同步: 双方都选了发送方，不会建立音频通道".to_string()
        }
        (AudioMode::Receive, AudioMode::Receive) => {
            "本次音频同步: 双方都选了接收方，不会建立音频通道".to_string()
        }
    }
}

fn input_summary_line(local_mode: InputMode, remote_mode: InputMode) -> String {
    match negotiate_input(local_mode, remote_mode) {
        Some(LocalInputRole::Send) => "本机 -> 对端".to_string(),
        Some(LocalInputRole::Receive) => "对端 -> 本机".to_string(),
        None => match (local_mode, remote_mode) {
            (InputMode::Off, InputMode::Off) => "关闭".to_string(),
            (InputMode::Off, _) => "本机未开启".to_string(),
            (_, InputMode::Off) => "对端未开启".to_string(),
            _ => "方向不兼容, 不会建立输入通道".to_string(),
        },
    }
}

fn short_uuid(id: &Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        Advertisement, SessionRole, SessionTaskAbortGuard, accept_policy_label,
        bootstrap_device_name_matches, bootstrap_peer_label, choose_peer,
        identity_display_name, input_task_restart_required, is_connection_shutdown_error,
        known_peer_for_query, parse_direct_peer_addr, peer_matches_query, preferred_peer_query,
        race_peer_addresses, resolve_audio_plan, run_advertisement_updates,
        run_with_session_notifications, select_peer_from_query, should_auto_accept_request,
        should_try_direct_trusted, trusted_transport_for_device, trusted_transport_for_identity,
    };
    use crate::audio::AudioChannelDirection;
    use crate::clipboard::ClipboardRuntimeOptions;
    use crate::config::{
        ClipboardConfig, DeviceConfig, DiscoveryConfig, NotificationConfig, SynlyConfig,
        TransferConfig, TrustedDeviceConfig,
    };
    use crate::discovery::DiscoveredPeer;
    use crate::input::{Hotkey, InputMode, InputRuntimeOptions, ScreenEdge};
    use crate::protocol::{
        DeviceIdentity, PROTOCOL_VERSION, PairAuthMethod,
        RuntimeCapabilities,
    };
    use crate::runtime_control::{RuntimeControl, RuntimeTuning};
    use crate::runtime_options::PairingRuntimeOptions;
    use crate::settings::{AudioMode, ClipboardMode};
    use crate::system_notification::{ConnectionEvent, NotificationPeer, SessionNotifier};
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::sync::Mutex;
    use std::time::Duration;
    use uuid::Uuid;

    #[tokio::test]
    async fn lan_candidate_authenticates_at_capacity_without_saving_trust_or_enabling_new_capabilities() {
        use super::{device_identity, handle_trusted_incoming_connection};
        use std::sync::Arc;
        use crate::protocol::{Frame, ControlMessage};
        use crate::host::{ActiveSlot, ActiveSlotReserver};
        use synly_core::transport::{logical::{LogicalSession, SessionKeys}, routing::TransportKind, stream::ByteStream};
        let client = synly_core::identity::generate_device_config("client".to_owned()).unwrap();
        let mut config = sample_config_with_trusted_devices(Vec::new());
        config.device = synly_core::identity::generate_device_config("host".to_owned()).unwrap();
        config.runtime.connection = Some(crate::settings::ConnectionPreference::Host);
        let options = crate::runtime_options::runtime_options_from_config(&config, None, false).unwrap();
        let host_peer = device_identity(&config.device, None); let client_peer = device_identity(&client, None);
        let id = Uuid::new_v4();
        let host_logical = LogicalSession::new(id, host_peer.clone(), client_peer.clone(), TransportKind::Bluetooth, [9; 32]).unwrap();
        let client_logical = LogicalSession::new(id, client_peer.clone(), host_peer.clone(), TransportKind::Bluetooth, [9; 32]).unwrap();
        let _host_owner = host_logical.owner().unwrap(); let _client_owner = client_logical.owner().unwrap();
        let admission = super::lan_admission::LanAdmission { active: vec![host_logical.clone()], full: true };
        let slot = Arc::new(Mutex::new(ActiveSlot::with_preferred(None))); let reserver = ActiveSlotReserver::new(slot.clone());
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let host = async {
            let (socket, address) = listener.accept().await.unwrap();
            let (session, reservation) = handle_trusted_incoming_connection(socket, address, &mut config, &options, &reserver, &admission).await.unwrap().unwrap();
            assert!(session.require_existing_session && session.trusted_reconnect);
            assert!(config.trusted_devices.is_empty());
            let bound = host_logical.accept(session.stream, &session.remote, TransportKind::Lan, session.candidate_exporter.value()).await.unwrap();
            drop(reservation); bound
        };
        let candidate = async {
            let socket = tokio::net::TcpStream::connect(address).await.unwrap();
            let connector = crate::crypto::build_client_connector(&client, &host_peer.tls_root_certificate).unwrap();
            let mut stream = connector.connect(crate::crypto::server_name().unwrap(), socket).await.unwrap();
            let request_id = Uuid::new_v4().to_string();
            let exporter = crate::crypto::export_keying_material_from_client(&stream, &request_id).unwrap();
            let payload = crate::protocol::PairRequestPayload { protocol_version: crate::protocol::PROTOCOL_VERSION, client: client_peer.clone(), capabilities: crate::protocol::RuntimeCapabilities { clipboard_mode: ClipboardMode::Off, audio_mode: AudioMode::Off, input_mode: InputMode::Off }, request_trust: false };
            let proof = crate::crypto::sign_trusted_pair_auth(&exporter, client.identity_private_key().unwrap(), &request_id, &payload).unwrap();
            crate::protocol::FrameWriter::new(&mut stream).write_frame(Frame::Control(ControlMessage::PairRequest { request_id: request_id.clone(), payload, trusted_proof: Some(proof) })).await.unwrap();
            let Frame::Control(decision) = crate::protocol::FrameReader::new(&mut stream).read_frame().await.unwrap() else { panic!("候选应收到可信决定") };
            assert!(matches!(decision, ControlMessage::PairDecision { accepted: true, trust_established: false, .. }));
            crate::crypto::verify_trusted_pair_decision(&decision, &exporter, &request_id, &host_peer.identity_public_key).unwrap();
            let keys = SessionKeys::client(&stream, &request_id).unwrap();
            client_logical.connect(ByteStream::new(stream), &host_peer, TransportKind::Lan, keys.candidate_exporter().value()).await.unwrap()
        };
        let (a, b) = tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(host, candidate) }).await.unwrap();
        assert!(host_logical.has(TransportKind::Lan) && client_logical.has(TransportKind::Lan));
        assert!(config.trusted_devices.is_empty() && slot.lock().unwrap().active().is_none());
        drop(a); drop(b); assert!(host_logical.is_open() && client_logical.is_open());
    }

    #[test]
    fn input_backend_generation_restarts_the_input_task() {
        let input = InputRuntimeOptions {
            mode: InputMode::Receive,
            path: synly_core::transport::routing::PathPolicy::PreferBluetooth,
            edge: ScreenEdge::Right,
            hotkey: Hotkey::DEFAULT.parse().unwrap(),
            reverse_mouse_wheel: false,
            reverse_trackpad: false,
            native_scroll_macos_to_windows: false,
            native_scroll_windows_to_macos: false,
            block_switch_on_press: false,
            filter_app_events: false,
            key_mapping: crate::input::KeyMappingConfig::default(),
            cursor_mode: crate::input::CursorMode::Desktop,
        };

        assert!(!input_task_restart_required(&input, &input, 3, 3));
        assert!(input_task_restart_required(&input, &input, 3, 4));
        let mut changed_path = input.clone();
        changed_path.path = synly_core::transport::routing::PathPolicy::LanOnly;
        assert!(input_task_restart_required(&input, &changed_path, 3, 3));
        let mut changed_mode = input.clone();
        changed_mode.cursor_mode = crate::input::CursorMode::Game;
        assert!(input_task_restart_required(&input, &changed_mode, 3, 3));
        let mut changed_native = input.clone();
        changed_native.native_scroll_macos_to_windows = true;
        assert!(input_task_restart_required(&input, &changed_native, 3, 3));
        let mut changed_reverse = input.clone();
        changed_reverse.native_scroll_windows_to_macos = true;
        assert!(input_task_restart_required(&input, &changed_reverse, 3, 3));
        let mut changed_filter = input.clone();
        changed_filter.filter_app_events = true;
        assert!(input_task_restart_required(&input, &changed_filter, 3, 3));
    }

    #[test]
    fn connection_shutdown_errors_are_recognized() {
        let err = anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ));
        assert!(is_connection_shutdown_error(&err));

        let err = anyhow::Error::from(std::io::Error::other("other"));
        assert!(!is_connection_shutdown_error(&err));
    }

    #[test]
    fn peer_query_matches_name_id_prefix_and_ip() {
        let peer = sample_peer();
        assert!(peer_matches_query(&peer, "demo-device"));
        assert!(peer_matches_query(&peer, "worker-a"));
        assert!(peer_matches_query(&peer, "abcd1234"));
        assert!(peer_matches_query(&peer, "192.168.1.20"));
        assert!(peer_matches_query(&peer, "192.168.1.20:8080"));
        assert!(!peer_matches_query(&peer, "unknown"));
    }

    #[test]
    fn select_peer_from_query_requires_unique_match() {
        let peer = sample_peer();
        let selected = select_peer_from_query(std::slice::from_ref(&peer), "demo-device").unwrap();
        assert_eq!(selected.device_id, peer.device_id);

        let duplicate = DiscoveredPeer {
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            fullname: "dup".to_string(),
            device_name: "demo-device".to_string(),
            instance_name: Some("worker-b".to_string()),
            device_id: "ffffeeee-dddd-cccc-bbbb-aaaaaaaaaaaa".to_string(),
            clipboard_mode: ClipboardMode::Off,
            audio_mode: AudioMode::Off,
            input_mode: crate::input::InputMode::Off,
            source: crate::discovery::DiscoverySource::Mdns,
            port: 9999,
            addresses: vec![Ipv4Addr::new(192, 168, 1, 21)],
        };
        assert!(select_peer_from_query(&[peer, duplicate], "demo-device").is_err());
    }

    #[test]
    fn known_peer_for_query_uses_unique_discovered_match() {
        let peer = sample_peer();
        let known = known_peer_for_query(std::slice::from_ref(&peer), &peer.device_id)
            .expect("unique discovered peer should seed the first connect");
        assert_eq!(known.device_id, peer.device_id);
        assert_eq!(known.addresses, peer.addresses);
        assert_eq!(known.port, peer.port);
    }

    #[test]
    fn known_peer_for_query_skips_direct_address_and_empty_candidates() {
        let peer = sample_peer();
        assert!(known_peer_for_query(std::slice::from_ref(&peer), "192.168.1.20:8080").is_none());

        let mut empty = sample_peer();
        empty.addresses.clear();
        assert!(known_peer_for_query(std::slice::from_ref(&empty), &empty.device_id).is_none());
    }

    #[test]
    fn known_peer_for_query_ignores_ambiguous_matches() {
        let peer = sample_peer();
        let duplicate = DiscoveredPeer {
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            fullname: "dup".to_string(),
            device_name: "demo-device".to_string(),
            instance_name: Some("worker-b".to_string()),
            device_id: "ffffeeee-dddd-cccc-bbbb-aaaaaaaaaaaa".to_string(),
            clipboard_mode: ClipboardMode::Off,
            audio_mode: AudioMode::Off,
            input_mode: crate::input::InputMode::Off,
            source: crate::discovery::DiscoverySource::Mdns,
            port: 9999,
            addresses: vec![Ipv4Addr::new(192, 168, 1, 21)],
        };
        assert!(known_peer_for_query(&[peer, duplicate], "demo-device").is_none());
    }

    #[test]
    fn select_peer_from_query_collapses_stale_ports_for_the_same_device() {
        let mut current = sample_peer();
        current.source = crate::discovery::DiscoverySource::Mdns;
        current.port = 49200;
        let mut stale = current.clone();
        stale.fullname = "stale-lnd".to_string();
        stale.source = crate::discovery::DiscoverySource::Lnd;
        stale.port = 49100;

        let selected = select_peer_from_query(&[stale, current], "demo-device").unwrap();

        assert_eq!(selected.port, 49200);
        assert_eq!(selected.source, crate::discovery::DiscoverySource::Mdns);
    }

    #[test]
    fn trusted_device_requests_auto_accept_without_accept_flag() {
        let pairing = sample_pairing_options();

        assert!(should_auto_accept_request(
            &pairing,
            PairAuthMethod::TrustedDevice
        ));
        assert!(!should_auto_accept_request(&pairing, PairAuthMethod::Pin));
    }

    #[test]
    fn accept_policy_label_reflects_trusted_device_default() {
        let pairing = sample_pairing_options();
        assert_eq!(
            accept_policy_label(&pairing),
            "可信设备自动接受；未受信任设备认证通过后仍需本机确认"
        );

        let mut pairing = pairing;
        pairing.accept = true;
        assert_eq!(accept_policy_label(&pairing), "认证通过后自动接受");
    }

    #[tokio::test]
    async fn choose_peer_requires_explicit_query_in_no_interact() {
        let err = choose_peer(
            None,
            Duration::from_millis(1),
            true,
            &sample_local_capabilities(),
            &DiscoveryConfig::default(),
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("peer_query"));
    }

    #[tokio::test]
    async fn full_ipv4_socket_addr_uses_direct_target() {
        let target = choose_peer(
            Some("192.168.1.20:8080"),
            Duration::from_millis(1),
            true,
            &sample_local_capabilities(),
            &DiscoveryConfig::default(),
        )
        .await
        .expect("full socket address should skip discovery");

        assert!(matches!(
            target,
            super::PeerTarget::Direct(address)
                if *address.ip() == Ipv4Addr::new(192, 168, 1, 20) && address.port() == 8080
        ));
    }

    #[tokio::test]
    async fn session_notifications_cover_success_and_error() {
        let notifier = RecordingNotifier::default();
        let peer = NotificationPeer {
            display_name: "demo".to_string(),
            short_device_id: "12345678".to_string(),
            device_id: Uuid::new_v4(),
        };

        run_with_session_notifications(&notifier, peer.clone(), async { Ok(()) })
            .await
            .unwrap();
        let error_result: anyhow::Result<()> =
            run_with_session_notifications(&notifier, peer, async {
                anyhow::bail!("session failed")
            })
            .await;

        assert!(error_result.is_err());
        assert_eq!(
            *notifier.events.lock().unwrap(),
            vec![
                ConnectionEvent::Connected,
                ConnectionEvent::Disconnected,
                ConnectionEvent::Connected,
                ConnectionEvent::Disconnected,
            ]
        );
    }

    #[tokio::test]
    async fn session_notifications_cover_cancellation() {
        let notifier = RecordingNotifier::default();
        let peer = NotificationPeer {
            display_name: "demo".to_string(),
            short_device_id: "12345678".to_string(),
            device_id: Uuid::new_v4(),
        };
        let mut session = Box::pin(run_with_session_notifications(
            &notifier,
            peer,
            std::future::pending::<anyhow::Result<()>>(),
        ));

        assert!(
            tokio::time::timeout(Duration::from_millis(1), session.as_mut())
                .await
                .is_err()
        );
        drop(session);

        assert_eq!(
            *notifier.events.lock().unwrap(),
            vec![ConnectionEvent::Connected, ConnectionEvent::Disconnected]
        );
    }

    #[tokio::test]
    async fn dropping_session_task_guard_aborts_tracked_tasks() {
        let task = tokio::spawn(std::future::pending::<()>());
        let mut guard = SessionTaskAbortGuard::default();
        guard.track(&task);

        drop(guard);

        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn peer_address_race_returns_the_first_successful_connection() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_task = tokio::spawn(async move { listener.accept().await.unwrap() });
        let mut failures = Vec::new();

        let socket = tokio::time::timeout(
            Duration::from_secs(1),
            race_peer_addresses(
                "测试地址",
                &[Ipv4Addr::new(192, 0, 2, 1), Ipv4Addr::LOCALHOST],
                port,
                &mut failures,
            ),
        )
        .await
        .unwrap()
        .unwrap();

        assert_eq!(socket.peer_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
        drop(socket);
        tokio::time::timeout(Duration::from_secs(1), accept_task)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn parse_direct_peer_addr_requires_host_and_port() {
        assert_eq!(
            parse_direct_peer_addr(" 192.168.1.20:8080 "),
            Some(SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 20), 8080))
        );
        assert_eq!(parse_direct_peer_addr("192.168.1.20"), None);
    }

    #[test]
    fn trusted_transport_for_device_requires_full_mtls_materials() {
        let device_id = Uuid::new_v4();
        let config = sample_config_with_trusted_devices(vec![TrustedDeviceConfig {
            device_id,
            device_name: "demo-device".to_string(),
            public_key: "pub".to_string(),
            tls_root_certificate: String::new(),
            trusted_at_ms: 0,
            last_seen_ms: 0,
            successful_sessions: 0,
        }]);

        assert!(trusted_transport_for_device(&config, &device_id).is_none());
    }

    #[test]
    fn trusted_transport_for_identity_accepts_fully_trusted_device() {
        let device_id = Uuid::new_v4();
        let config = sample_config_with_trusted_devices(vec![TrustedDeviceConfig {
            device_id,
            device_name: "demo-device".to_string(),
            public_key: "pub".to_string(),
            tls_root_certificate: "cert".to_string(),
            trusted_at_ms: 0,
            last_seen_ms: 0,
            successful_sessions: 0,
        }]);
        let identity = DeviceIdentity {
            device_id,
            device_name: "demo-device".to_string(),
            instance_name: Some("worker-a".to_string()),
            identity_public_key: "pub".to_string(),
            tls_root_certificate: "cert".to_string(),
        };

        let trusted = trusted_transport_for_identity(&config, &identity).unwrap();
        assert_eq!(trusted.device_id, device_id);
    }

    #[test]
    fn trusted_transport_for_identity_rejects_partial_trust() {
        let device_id = Uuid::new_v4();
        let config = sample_config_with_trusted_devices(vec![TrustedDeviceConfig {
            device_id,
            device_name: "demo-device".to_string(),
            public_key: "pub".to_string(),
            tls_root_certificate: String::new(),
            trusted_at_ms: 0,
            last_seen_ms: 0,
            successful_sessions: 0,
        }]);
        let identity = DeviceIdentity {
            device_id,
            device_name: "demo-device".to_string(),
            instance_name: None,
            identity_public_key: "pub".to_string(),
            tls_root_certificate: "cert".to_string(),
        };

        let err = trusted_transport_for_identity(&config, &identity)
            .unwrap_err()
            .to_string();
        assert!(err.contains("长期 mTLS"));
    }

    #[test]
    fn direct_ip_prefers_trusted_when_any_trusted_transport_exists() {
        let config = sample_config_with_trusted_devices(vec![TrustedDeviceConfig {
            device_id: Uuid::new_v4(),
            device_name: "demo-device".to_string(),
            public_key: "pub".to_string(),
            tls_root_certificate: "cert".to_string(),
            trusted_at_ms: 0,
            last_seen_ms: 0,
            successful_sessions: 0,
        }]);

        assert!(should_try_direct_trusted(
            &config,
            &sample_pairing_options()
        ));
    }

    #[test]
    fn direct_ip_tries_trusted_when_trusted_only_is_enabled() {
        let config = sample_config_with_trusted_devices(Vec::new());
        let mut pairing = sample_pairing_options();
        pairing.trusted_only = true;

        assert!(should_try_direct_trusted(&config, &pairing));
    }

    #[test]
    fn preferred_peer_query_uses_instance_name_when_present() {
        let peer = sample_peer();
        assert_eq!(preferred_peer_query(&peer), "worker-a");
    }

    #[test]
    fn identity_display_name_prefers_instance_name() {
        let identity = DeviceIdentity {
            device_id: Uuid::nil(),
            device_name: "demo-device".to_string(),
            instance_name: Some("worker-a".to_string()),
            identity_public_key: "pub".to_string(),
            tls_root_certificate: "cert".to_string(),
        };

        assert_eq!(identity_display_name(&identity), "worker-a @ demo-device");
    }

    #[test]
    fn bootstrap_peer_label_prefers_device_name_and_keeps_address() {
        let address = SocketAddr::from(([127, 0, 0, 1], 8080));

        assert_eq!(
            bootstrap_peer_label("  demo-device  ", address),
            "demo-device (127.0.0.1:8080)"
        );
        assert_eq!(bootstrap_peer_label(" ", address), "127.0.0.1:8080");
    }

    #[test]
    fn bootstrap_device_name_must_match_authenticated_identity() {
        assert!(bootstrap_device_name_matches(
            " demo-device ",
            "demo-device"
        ));
        assert!(!bootstrap_device_name_matches(
            "displayed-device",
            "authenticated-device"
        ));
    }

    #[test]
    fn resolve_audio_plan_assigns_sender_direction_for_host() {
        let plan = resolve_audio_plan(SessionRole::Host, AudioMode::Send, AudioMode::Receive)
            .expect("send/receive pair should enable audio");

        assert_eq!(plan.role, super::LocalAudioRole::Send);
        assert_eq!(plan.direction, AudioChannelDirection::HostToClient);
    }

    #[test]
    fn resolve_audio_plan_rejects_same_audio_roles() {
        assert!(
            resolve_audio_plan(SessionRole::Client, AudioMode::Send, AudioMode::Send).is_none()
        );
        assert!(
            resolve_audio_plan(SessionRole::Client, AudioMode::Receive, AudioMode::Receive)
                .is_none()
        );
    }

    #[tokio::test]
    async fn advertisement_updates_survive_detached_control_channels() {
        let control = RuntimeControl::detached(
            RuntimeCapabilities {
                clipboard_mode: ClipboardMode::Off,
                audio_mode: AudioMode::Off,
                input_mode: InputMode::Off,
            },
            RuntimeTuning {
                notifications_enabled: true,
                input_backend_generation: 0,
                device_name: "test-device".to_string(),
                instance_name: None,
                discovery: DiscoveryConfig::default(),
                input: InputRuntimeOptions {
                    mode: InputMode::Off,
                    path: synly_core::transport::routing::PathPolicy::PreferBluetooth,
                    edge: ScreenEdge::Right,
                    hotkey: Hotkey::DEFAULT.parse().unwrap(),
                    reverse_mouse_wheel: false,
                    reverse_trackpad: false,
                    native_scroll_macos_to_windows: false,
                    native_scroll_windows_to_macos: false,
                    block_switch_on_press: false,
                    filter_app_events: false,
                    key_mapping: crate::input::KeyMappingConfig::default(),
                    cursor_mode: crate::input::CursorMode::Desktop,
                },
                clipboard: ClipboardRuntimeOptions {
                    path: synly_core::transport::routing::PathPolicy::Auto,
                    max_file_bytes: 1,
                    max_cache_bytes: None,
                    cache_dir: std::path::PathBuf::from("."),
                },
            },
        );
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut task = tokio::spawn(run_advertisement_updates(
            Advertisement {
                protocol_version: PROTOCOL_VERSION,
                port: 0,
                device: DeviceConfig {
                    device_id: uuid::Uuid::nil(),
                    device_name: "test-device".to_string(),
                    identity_private_key: String::new(),
                    identity_public_key: String::new(),
                },
                clipboard_mode: ClipboardMode::Off,
                audio_mode: AudioMode::Off,
                input_mode: InputMode::Off,
                instance_name: None,
            },
            DiscoveryConfig::default(),
            control.capabilities(),
            control.tuning(),
            crate::discovery::DiscoveryRegistration::default(),
            shutdown.clone(),
        ));

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut task)
                .await
                .is_err(),
            "detached 控制通道关闭时广告更新任务不应提前结束"
        );
        shutdown.cancel();
        assert!(task.await.unwrap().is_ok());
    }

    fn sample_peer() -> DiscoveredPeer {
        DiscoveredPeer {
            fullname: "demo._synly._tcp.local.".to_string(),
            device_name: "demo-device".to_string(),
            instance_name: Some("worker-a".to_string()),
            device_id: "abcd1234-1111-2222-3333-444455556666".to_string(),
            protocol_version: crate::protocol::PROTOCOL_VERSION,
            clipboard_mode: ClipboardMode::Both,
            audio_mode: AudioMode::Off,
            input_mode: crate::input::InputMode::Off,
            source: crate::discovery::DiscoverySource::Mdns,
            port: 8080,
            addresses: vec![Ipv4Addr::new(192, 168, 1, 20)],
        }
    }

    fn sample_local_capabilities() -> RuntimeCapabilities {
        RuntimeCapabilities {
            clipboard_mode: ClipboardMode::Both,
            audio_mode: AudioMode::Off,
            input_mode: crate::input::InputMode::Off,
        }
    }

    fn sample_pairing_options() -> PairingRuntimeOptions {
        PairingRuntimeOptions {
            headless: false,
            peer_query: None,
            port: None,
            pin: None,
            accept: false,
            trust_device: false,
            trusted_only: false,
            discovery_secs: 3,
            known_peer: None,
        }
    }

    fn sample_config_with_trusted_devices(
        trusted_devices: Vec<TrustedDeviceConfig>,
    ) -> SynlyConfig {
        SynlyConfig {
            device: DeviceConfig {
                device_id: Uuid::nil(),
                device_name: "local-device".to_string(),
                identity_private_key: String::new(),
                identity_public_key: String::new(),
            },
            clipboard: ClipboardConfig::default(),
            transfer: TransferConfig::default(),
            notifications: NotificationConfig::default(),
            discovery: DiscoveryConfig::default(),
            ui: crate::config::UiConfig::default(),
            update: crate::config::UpdateConfig::default(),
            gui_state: crate::config::GuiState::default(),
            runtime: crate::config::RuntimeConfig::default(),
            trusted_devices,
            preferred_active: None,
        }
    }

    #[derive(Default)]
    struct RecordingNotifier {
        events: Mutex<Vec<ConnectionEvent>>,
    }

    impl SessionNotifier for RecordingNotifier {
        fn notify(&self, event: ConnectionEvent, _peer: &NotificationPeer) {
            self.events.lock().unwrap().push(event);
        }
    }

}
