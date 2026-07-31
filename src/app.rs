use crate::audio::{self, AudioChannelDirection};
use crate::input::{
    self, InputHostChannel, InputMode, InputRuntimeOptions, InputSessionContext,
    InputSocketConnection, InputSocketInbox, LocalInputRole, negotiate_input,
};
use crate::clipboard::{ClipboardSync, ClipboardWatcherHandle};
use crate::config::{DeviceConfig, SynlyConfig, TrustedDeviceConfig};
use crate::crypto;
use crate::discovery::{self, Advertisement, DiscoveredPeer, format_display_name};
use crate::host::clipboard_hub::ClipboardHubHandle;
use crate::host::session::InputRouteRegistry;
use crate::host::{
    ActiveSlotReserver, SessionCapabilityProfile, SlotReservation, runtime_options_for_profile,
};
use crate::protocol::{
    CapabilityEpoch, ClipboardPayload, ControlMessage, DeviceIdentity, FileChunkHeader, Frame,
    FrameReader, FrameWriter, PROTOCOL_VERSION, PairAuthMethod, PairRequestPayload,
    RuntimeCapabilities, SessionAgreement, TransferLimits, frame_size_limit_message,
};
use crate::runtime_control::{
    InteractionRequest, InteractionResponse, RuntimeCommand, RuntimeControl, RuntimeEvent,
    RuntimeLifecycle, RuntimePeerSummary, RuntimeTuning,
};
use crate::runtime_options::{
    PairingRuntimeOptions, RuntimeOptions, normalize_pin, require_peer_query, sync_delete_label,
};
use crate::session::CapabilityState;
use crate::settings::{
    AudioMode, ClipboardMode, ConnectionPreference, FileSyncMode, InitialSyncMode,
};
use crate::sync::{
    DeletePolicy, EntryKind, ManifestEntry, ManifestSnapshot, TimestampComparisonContext,
    WorkspaceSpec, apply_file_metadata, build_apply_plan_with_time, build_incoming_snapshot,
    build_snapshot, delete_paths_best_effort, ensure_directories, filter_snapshot_by_folder_depth,
    filter_snapshot_for_incoming_root, resolve_incoming_path, resolve_outgoing_path, watch_targets,
};
use crate::system_notification::{
    ConnectionEvent, NotificationPeer, SessionNotifier, SystemNotifier,
};
use anyhow::{Context, Result, anyhow, bail};
use notify::{Config as NotifyConfig, Event, RecommendedWatcher, RecursiveMode, Watcher};
use rand::RngExt;
use sha2::{Digest, Sha256};
use socket2::{SockRef, TcpKeepalive};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::time::{self, Instant};
use tokio_rustls::{TlsStream, client::TlsStream as ClientTlsStream};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const FILE_STREAM_CHUNK_SIZE: usize = 256 * 1024;
const PAIRING_TIMEOUT: Duration = Duration::from_secs(90);
const TLS_UPGRADE_TIMEOUT: Duration = Duration::from_secs(15);
const TCP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const PAIRING_FAILURE_WINDOW: Duration = Duration::from_secs(5 * 60);
const PAIRING_COOLDOWN: Duration = Duration::from_secs(3 * 60);
const PAIRING_MAX_FAILURES: u32 = 5;
const PAIRING_BACKOFF_BASE_MS: u64 = 1_000;
const RECONNECT_BASE_DELAY: Duration = Duration::from_secs(2);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(20);
const TIMESTAMP_SKEW_TOLERANCE_MS: u64 = 10_000;
const FUTURE_TIMESTAMP_GUARD_MS: u64 = 10 * 60 * 1_000;
const CLOCK_SKEW_WARNING_MS: u64 = 60_000;
const REMOTE_ECHO_SUPPRESSION_TTL: Duration = Duration::from_secs(10);
const ADVERTISED_SNAPSHOT_CACHE_LIMIT: usize = 8;
const CAPABILITY_ACK_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
pub(crate) enum SessionRole {
    Host,
    Client,
}

#[derive(Debug)]
pub(crate) struct AuthenticatedSession {
    pub(crate) role: SessionRole,
    pub(crate) stream: TlsStream<TcpStream>,
    pub(crate) remote: DeviceIdentity,
    pub(crate) agreement: SessionAgreement,
    pub(crate) remote_workspace: crate::sync::WorkspaceSummary,
    pub(crate) remote_socket_addr: SocketAddr,
    pub(crate) audio_master_secret: [u8; 32],
    pub(crate) input_master_secret: [u8; 32],
    pub(crate) capability_profile: SessionCapabilityProfile,
}

#[derive(Debug)]
struct PendingRevision {
    requested_files: usize,
    remaining_files: BTreeSet<String>,
    failed_files: BTreeSet<String>,
    expected_files: BTreeMap<String, ManifestEntry>,
    delete_paths: Vec<String>,
    skipped_newer_count: usize,
    transfer_done: bool,
}

struct IncomingFileState {
    file: File,
    temp_path: PathBuf,
    final_path: PathBuf,
    expected_entry: ManifestEntry,
    hasher: Sha256,
    written: u64,
}

#[derive(Clone, Debug)]
struct AdvertisedSnapshot {
    revision: u64,
    snapshot: ManifestSnapshot,
}

#[derive(Clone, Debug)]
enum SnapshotLoopControl {
    ExpectRemoteChanges {
        expectations: Vec<RemoteEchoExpectation>,
    },
    AdoptCurrentSnapshotAsBaselineAndEnable,
    ForcePublish,
}

#[derive(Clone, Debug)]
struct RemoteEchoExpectation {
    wire_path: String,
    expected: SnapshotPathExpectation,
}

#[derive(Clone, Debug)]
struct PendingRemoteEchoExpectation {
    expected: SnapshotPathExpectation,
    expires_at: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SnapshotPathExpectation {
    Exact(ManifestEntry),
    DirExists,
    Missing,
}

#[derive(Default)]
struct SnapshotEchoSuppressions {
    paths: BTreeMap<String, PendingRemoteEchoExpectation>,
}

struct PairDecisionParams<'a> {
    exporter: &'a [u8],
    request_id: &'a str,
    accepted: bool,
    message: String,
    device: &'a DeviceConfig,
    instance_name: Option<&'a str>,
    workspace: &'a WorkspaceSpec,
    clipboard_mode: ClipboardMode,
    audio_mode: AudioMode,
    input_mode: InputMode,
    agreement: &'a SessionAgreement,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InitialSnapshotPolicy {
    PublishImmediately,
    WaitForRemoteSeed,
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
    Direct(SocketAddrV4),
}

impl PeerTarget {
    fn reconnect_query(&self) -> String {
        match self {
            Self::Discovered(peer) => preferred_peer_query(peer),
            Self::Direct(address) => address.to_string(),
        }
    }
}

pub async fn run(
    config: SynlyConfig,
    options: RuntimeOptions,
    commands: mpsc::UnboundedReceiver<RuntimeCommand>,
) -> Result<()> {
    match options.connection {
        ConnectionPreference::Host => crate::host::run_host_runtime(config, options, commands).await,
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
    let mut reconnect_delay = RECONNECT_BASE_DELAY;
    let notifier = SystemNotifier::new(options.control.tuning());
    let shutdown = options.control.shutdown().clone();
    let mut runtime_capabilities = options.control.capabilities();
    let mut runtime_tuning = options.control.tuning();
    options
        .control
        .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Discovering));
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    tokio::select! {
        result = async {
            loop {
                refresh_runtime_options(
                    &mut config,
                    &mut options,
                    &mut runtime_capabilities,
                    &mut runtime_tuning,
                );
                let local_workspace_summary = options.workspace.session_summary(
                    options.clipboard_mode,
                    options.audio_mode,
                    options.input_mode,
                );
                let peer_target = match choose_peer(
                    reconnect_query.as_deref(),
                    discovery_timeout,
                    options.pairing.headless,
                    &local_workspace_summary,
                    &options.discovery,
                )
                .await
                {
                    Ok(peer) => peer,
                    Err(err) => {
                        if reconnect_query.is_some() {
                            tracing::warn!(error = %err, "等待目标设备重新出现");
                            sleep_before_reconnect(reconnect_delay).await;
                            reconnect_delay = next_reconnect_delay(reconnect_delay);
                            continue;
                        }
                        return Err(err);
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
                reconnect_query = Some(peer_target.reconnect_query());
                options
                    .control
                    .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Connecting));

                match connect_to_peer(&peer_target, &mut config, &options).await {
                    Ok(session) => {
                        let remote_label = format!(
                            "{} ({})",
                            identity_display_name(&session.remote),
                            short_uuid(&session.remote.device_id)
                        );
                        let peer_summary = RuntimePeerSummary {
                            device_id: session.remote.device_id,
                            display_name: identity_display_name(&session.remote),
                        };
                        reconnect_delay = RECONNECT_BASE_DELAY;
                        let peer = notification_peer(&session.remote);
                        if let Err(err) = run_with_session_notifications(
                            &notifier,
                            peer,
                            run_sync_session(
                                session,
                                &options.workspace,
                                SyncSessionOptions {
                                    clipboard_mode: options.clipboard_mode,
                                    audio_mode: options.audio_mode,
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
                        options.control.report(RuntimeEvent::Disconnected(peer_summary));
                        options
                            .control
                            .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Discovering));
                        sleep_before_reconnect(reconnect_delay).await;
                        reconnect_delay = next_reconnect_delay(reconnect_delay);
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "连接失败");
                        options
                            .control
                            .report(RuntimeEvent::Lifecycle(RuntimeLifecycle::Discovering));
                        sleep_before_reconnect(reconnect_delay).await;
                        reconnect_delay = next_reconnect_delay(reconnect_delay);
                    }
                }
            }
        } => result,
        signal_result = &mut ctrl_c => finish_ctrl_c(signal_result),
        _ = shutdown.cancelled() => Ok(()),
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
    options.interval_secs = tuning.interval_secs;
    options.sync_delete = tuning.sync_delete;
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
) -> Result<Option<(AuthenticatedSession, SlotReservation)>> {
    let mut first_byte = [0u8; 1];
    let peeked = socket.peek(&mut first_byte).await?;
    if peeked == 0 {
        return Ok(None);
    }

    if first_byte[0] == 0x16 {
        handle_trusted_incoming_connection(socket, remote_addr, config, options, reserver).await
    } else {
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
                    connect_to_trusted_peer(
                        socket,
                        &device,
                        trusted_device,
                        config,
                        options,
                    )
                    .await
                }
                None => connect_to_untrusted_peer(socket, &device, config, options).await,
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
            connect_to_untrusted_peer(socket, &device, config, options).await
        }
    }
}

async fn connect_to_discovered_peer(peer: &DiscoveredPeer) -> Result<TcpStream> {
    if peer.addresses.is_empty() {
        bail!("peer advertised no IPv4 address");
    }
    let groups = match discovery::group_peer_addresses(&peer.addresses) {
        Ok(groups) => groups,
        Err(err) => {
            tracing::warn!(error = %err, "无法读取本机网卡子网, 将尝试全部广播地址");
            discovery::PeerAddressGroups {
                same_subnet: Vec::new(),
                fallback: peer.addresses.clone(),
            }
        }
    };
    let mut failures = Vec::new();
    if let Some(socket) = race_peer_addresses(
        "同子网地址",
        &groups.same_subnet,
        peer.port,
        &mut failures,
    )
    .await
    {
        return Ok(socket);
    }
    if !groups.same_subnet.is_empty() && !groups.fallback.is_empty() {
        tracing::info!("同子网地址均无法连接, 正在尝试其余地址");
    }
    if let Some(socket) = race_peer_addresses(
        "其余地址",
        &groups.fallback,
        peer.port,
        &mut failures,
    )
    .await
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
            let socket = result.with_context(|| format!("failed to connect to {address}:{port}"))?;
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
) -> Result<Option<(AuthenticatedSession, SlotReservation)>> {
    let transfer_limits = options.transfer_limits;
    let device = config.device.clone();
    let remote_label = remote_addr.to_string();
    if !has_trusted_transport(config) {
        bail!("收到 TLS 连接，但本机尚未保存任何可信设备根证书；未信任设备必须先走 bootstrap/PIN");
    }

    let acceptor = crypto::build_server_acceptor(&device, &config.trusted_devices)?;
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

    let trusted_device = match config.trusted_device(&payload.client.device_id).cloned() {
        Some(trusted_device) => trusted_device,
        None => {
            write_frame(
                &mut server_stream,
                transfer_limits,
                Frame::Control(ControlMessage::Error {
                    message: "该设备未处于可信状态，不能走免 PIN 的 mTLS 直连。".to_string(),
                }),
            )
            .await?;
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
    let agreement =
        negotiate_file_sync_modes(session_options.file_sync_mode, payload.workspace.file_sync_mode);
    let clipboard_agreement =
        negotiate_clipboard(session_options.clipboard_mode, payload.workspace.clipboard_mode);
    let audio_compatible =
        audio_modes_compatible(session_options.audio_mode, payload.workspace.audio_mode);
    let input_compatible =
        negotiate_input(session_options.input_mode, payload.workspace.input_mode).is_some();
    print_pair_request_overview(&payload, &session_options, &agreement, &remote_label)?;
    if !agreement.any_direction() && !clipboard_agreement.any_direction() && !audio_compatible && !input_compatible {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: "文件、剪贴板和音频方向都不兼容，本次请求无法建立同步。".to_string(),
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
        workspace: &session_options.workspace,
        clipboard_mode: session_options.clipboard_mode,
        audio_mode: session_options.audio_mode,
        input_mode: session_options.input_mode,
        agreement: &agreement,
        auth_method: PairAuthMethod::TrustedDevice,
        pin: None,
        server_trusts_client: true,
        trust_established: false,
    })?;
    write_frame(&mut server_stream, transfer_limits, Frame::Control(control)).await?;

    if !accepted {
        return Ok(None);
    }

    config.note_trusted_device_session(payload.client.device_id, &payload.client.device_name);
    config.save_trusted_devices()?;

    let tls_stream: TlsStream<TcpStream> = server_stream.into();
    Ok(Some((
        AuthenticatedSession {
            role: SessionRole::Host,
            stream: tls_stream,
            remote: payload.client,
            agreement,
            remote_workspace: payload.workspace,
            remote_socket_addr: remote_addr,
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
    options
        .control
        .notify_interaction(InteractionRequest::ShowHostPin {
            request_id: gui_pairing_id,
            remote_label: remote_label.clone(),
            bootstrap_short: client_display.short.clone(),
            bootstrap_randomart: client_display.randomart.clone(),
            session_short: session_display.short.clone(),
            session_randomart: session_display.randomart.clone(),
            pin: pin.clone(),
            fixed_pin: options.pairing.pin.is_some(),
        });

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

    let pake_frame =
        match read_frame_with_timeout(&mut socket, PAIRING_TIMEOUT, transfer_limits).await {
            Ok(frame) => frame,
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
    let mut server_stream = time::timeout(TLS_UPGRADE_TIMEOUT, acceptor.accept(socket))
        .await
        .map_err(|_| anyhow!("等待客户端切换到临时 mTLS 超时"))??;
    let frame =
        read_frame_with_timeout(&mut server_stream, PAIRING_TIMEOUT, transfer_limits).await?;
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
    let agreement =
        negotiate_file_sync_modes(session_options.file_sync_mode, payload.workspace.file_sync_mode);
    let clipboard_agreement =
        negotiate_clipboard(session_options.clipboard_mode, payload.workspace.clipboard_mode);
    let audio_compatible =
        audio_modes_compatible(session_options.audio_mode, payload.workspace.audio_mode);
    let input_compatible =
        negotiate_input(session_options.input_mode, payload.workspace.input_mode).is_some();
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
    print_pair_request_overview(&payload, &session_options, &agreement, &remote_addr_text)?;
    if !agreement.any_direction() && !clipboard_agreement.any_direction() && !audio_compatible && !input_compatible {
        write_frame(
            &mut server_stream,
            transfer_limits,
            Frame::Control(ControlMessage::Error {
                message: "文件、剪贴板和音频方向都不兼容，本次请求无法建立同步。".to_string(),
            }),
        )
        .await?;
        return Ok(None);
    }

    tracing::info!("已建立基于 PIN 的临时 mTLS, 设备元数据处于加密保护中");
    let (accepted, remember_trusted_device) =
        if should_auto_accept_request(&options.pairing, PairAuthMethod::Pin) {
            (true, options.pairing.trust_device)
        } else if options.pairing.headless {
            tracing::warn!("headless 模式拒绝未信任设备");
            (false, false)
        } else {
            let interaction_id = Uuid::new_v4();
            let mut summary = payload.workspace.summary_lines();
            summary.push(format!("剪贴板: {}", payload.workspace.clipboard_mode.label()));
            summary.push(format!("音频: {}", payload.workspace.audio_mode.label()));
            summary.push(format!("输入: {}", payload.workspace.input_mode.label()));
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
        workspace: &session_options.workspace,
        clipboard_mode: session_options.clipboard_mode,
        audio_mode: session_options.audio_mode,
        input_mode: session_options.input_mode,
        agreement: &agreement,
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

    let tls_stream: TlsStream<TcpStream> = server_stream.into();
    Ok(Some((
        AuthenticatedSession {
            role: SessionRole::Host,
            stream: tls_stream,
            remote: payload.client,
            agreement,
            remote_workspace: payload.workspace,
            remote_socket_addr: remote_addr,
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
        workspace: options
            .workspace
            .session_summary(options.clipboard_mode, options.audio_mode, options.input_mode),
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
    let (remote, remote_workspace, agreement) = match reply {
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
                bail!("peer replied to trusted mTLS with an unexpected auth method");
            }
            let trusted_device = resolve_trusted_device(config, &server)?;
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
            (server, workspace, agreement)
        }
        ControlMessage::Error { message } => bail!("{}", message),
        other => bail!("unexpected trusted pairing response: {other:?}"),
    };

    config.note_trusted_device_session(remote.device_id, &remote.device_name);
    config.save_trusted_devices()?;

    let tls_stream: TlsStream<TcpStream> = client_stream.into();
    print_connected_peer(&remote, &agreement, &remote_workspace, options.input_mode)?;
    Ok(AuthenticatedSession {
        role: SessionRole::Client,
        stream: tls_stream,
        remote,
        agreement,
        remote_workspace,
        remote_socket_addr,
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
        workspace: options
            .workspace
            .session_summary(options.clipboard_mode, options.audio_mode, options.input_mode),
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
    let (remote, remote_workspace, agreement) = match &reply {
        ControlMessage::PairDecision {
            accepted,
            message,
            server,
            workspace,
            agreement,
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
            (server.clone(), workspace.clone(), agreement.clone())
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

    let tls_stream: TlsStream<TcpStream> = client_stream.into();
    print_connected_peer(&remote, &agreement, &remote_workspace, options.input_mode)?;
    Ok(AuthenticatedSession {
        role: SessionRole::Client,
        stream: tls_stream,
        remote,
        agreement,
        remote_workspace,
        remote_socket_addr,
        audio_master_secret,
        input_master_secret,
        capability_profile: SessionCapabilityProfile::Full,
    })
}

pub(crate) struct SyncSessionOptions<'a> {
    pub(crate) clipboard_mode: ClipboardMode,
    pub(crate) audio_mode: AudioMode,
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
    input_task: Option<tokio::task::JoinHandle<()>>,
    input_epoch: Option<CapabilityEpoch>,
    input_role: Option<LocalInputRole>,
}

impl CapabilityTaskRuntime {
    fn new(clipboard_options: &crate::clipboard::ClipboardRuntimeOptions) -> Self {
        Self {
            clipboard: ClipboardCapabilityRuntime::new(clipboard_options),
            audio_task: None,
            audio_epoch: None,
            audio_plan: None,
            input_task: None,
            input_epoch: None,
            input_role: None,
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
    }

    async fn stop_input(
        &mut self,
        input_session_id: Option<&watch::Sender<Option<Uuid>>>,
        input_routes: Option<&Arc<InputRouteRegistry>>,
    ) {
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
    pub(crate) remote_socket_addr: SocketAddr,
    pub(crate) audio_master_secret: [u8; 32],
    pub(crate) input_master_secret: [u8; 32],
    pub(crate) input_options: &'a InputRuntimeOptions,
    pub(crate) input_inbox: Option<&'a InputSocketInbox>,
    pub(crate) input_session_id: Option<&'a watch::Sender<Option<Uuid>>>,
    pub(crate) input_socket_tx: Option<&'a mpsc::Sender<InputSocketConnection>>,
    pub(crate) input_routes: Option<&'a Arc<InputRouteRegistry>>,
    pub(crate) clipboard_hub: Option<&'a ClipboardHubHandle>,
    pub(crate) tx: &'a mpsc::Sender<Frame>,
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
    if let Some(hub) = context.clipboard_hub {
        hub.set_receive_enabled(context.peer_device_id, clipboard_can_receive);
    } else {
        if runtime.clipboard.can_send && !clipboard_can_send {
            runtime.clipboard.stop_sender();
        }
        if !runtime.clipboard.can_send && clipboard_can_send {
            let (clipboard_tx, clipboard_rx) = mpsc::unbounded_channel();
            let watcher = match runtime.clipboard.sync.start_local_watcher(clipboard_tx.clone()) {
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
                let task = tokio::spawn(clipboard_sender_loop(
                    clipboard_rx,
                    context.tx.clone(),
                ));
                tasks.track(&task);
                runtime.clipboard.sender_task = Some(task);
                runtime.clipboard.watcher = watcher;
                runtime.clipboard.can_send = true;
            }
        }
    }
    runtime.clipboard.can_receive = clipboard_can_receive;

    let epoch = state.epoch();
    let audio_plan = state.audio_ready().then(|| {
        resolve_audio_plan(
            context.session_role,
            local.audio_mode,
            remote.audio_mode,
        )
    }).flatten();
    if runtime.audio_epoch != Some(epoch) || runtime.audio_plan != audio_plan {
        runtime.stop_audio().await;
        runtime.audio_epoch = Some(epoch);
        runtime.audio_plan = audio_plan;
        match audio_plan {
            Some(AudioPlan {
                role: LocalAudioRole::Receive,
                direction,
            }) => match audio::bind_and_spawn_receiver(
                context.audio_master_secret,
                direction,
                context.remote_socket_addr.ip(),
            ) {
                Ok((task, port)) => {
                    context
                        .tx
                        .send(Frame::Control(ControlMessage::AudioUdpReady { epoch, port }))
                        .await?;
                    runtime.audio_task = Some(task);
                }
                Err(err) => {
                    tracing::warn!(error = %err, "无法启动音频接收通道");
                }
            },
            Some(AudioPlan {
                role: LocalAudioRole::Send,
                ..
            }) => tracing::info!(?epoch, "音频发送端等待对侧接收端口"),
            None => {}
        }
    }

    let input_role = state
        .is_local_acknowledged()
        .then(|| negotiate_input(local.input_mode, remote.input_mode))
        .flatten();
    if runtime.input_epoch != Some(epoch) || runtime.input_role != input_role {
        runtime
            .stop_input(context.input_session_id, context.input_routes)
            .await;
        runtime.input_epoch = Some(epoch);
        runtime.input_role = input_role;
        if let Some(local_role) = input_role
            && matches!(context.session_role, SessionRole::Host)
        {
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
            let task = tokio::spawn(async move {
                if let Err(err) = input::run_input_session(
                    InputSessionContext::host(channel, inbox),
                    input_master_secret,
                    local_role,
                    input_options,
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
    previous.edge != next.edge
        || previous.hotkey != next.hotkey
        || previous.reverse_mouse_wheel != next.reverse_mouse_wheel
        || previous.reverse_trackpad != next.reverse_trackpad
        || previous.block_switch_on_press != next.block_switch_on_press
        || previous.key_mapping != next.key_mapping
        || previous_backend_generation != next_backend_generation
}

fn spawn_frame_reader<R>(
    reader: R,
    transfer_limits: TransferLimits,
) -> (mpsc::Receiver<Result<Frame>>, tokio::task::JoinHandle<()>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(64);
    let task = tokio::spawn(async move {
        let mut reader = FrameReader::with_limits(reader, transfer_limits);
        loop {
            match reader.read_frame().await {
                Ok(frame) => {
                    if tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = tx.send(Err(err)).await;
                    break;
                }
            }
        }
    });
    (rx, task)
}

pub(crate) async fn run_sync_session(
    session: AuthenticatedSession,
    workspace: &WorkspaceSpec,
    options: SyncSessionOptions<'_>,
) -> Result<()> {
    tracing::info!(
        peer = %identity_display_name(&session.remote),
        device_id = %short_uuid(&session.remote.device_id),
        remote_workspace = %session.remote_workspace.summary_lines().join(" | "),
        "同步会话已开始"
    );

    let local_can_send = allows_local_send(session.role, &session.agreement);
    let local_can_receive = allows_local_receive(session.role, &session.agreement);
    let file_can_send = local_can_send
        && workspace.can_send_files()
        && session.remote_workspace.can_receive_files();
    let file_can_receive = local_can_receive
        && workspace.can_receive_files()
        && session.remote_workspace.can_send_files();
    let initial_snapshot_policy = resolve_initial_snapshot_policy(
        session.role,
        workspace,
        &session.remote_workspace,
        file_can_send,
        file_can_receive,
    )?;
    let initial_local_capabilities = options
        .capability_profile
        .apply(RuntimeCapabilities {
            clipboard_mode: options.clipboard_mode,
            audio_mode: options.audio_mode,
            input_mode: options.input_mode,
        });
    let initial_remote_capabilities = RuntimeCapabilities {
        clipboard_mode: session.remote_workspace.clipboard_mode,
        audio_mode: session.remote_workspace.audio_mode,
        input_mode: session.remote_workspace.input_mode,
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
    let mut input_options = current_tuning.input;
    let mut input_backend_generation = current_tuning.input_backend_generation;
    let mut sync_delete = current_tuning.sync_delete;
    let shutdown = options.control.shutdown().clone();
    let session_shutdown = options.session_shutdown.clone();
    let mut capability_ack_deadline = None;
    let mut capabilities_open = true;
    let mut tuning_open = true;
    let remote_socket_addr = session.remote_socket_addr;
    let audio_master_secret = session.audio_master_secret;
    let input_master_secret = session.input_master_secret;

    tracing::info!(
        clipboard = %clipboard_summary_line(
            session.role,
            options.clipboard_mode,
            session.remote_workspace.clipboard_mode,
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

    let (read_half, write_half) = tokio::io::split(session.stream);
    let (tx, rx) = mpsc::channel::<Frame>(64);
    let mut session_tasks = SessionTaskAbortGuard::default();
    let writer_task = tokio::spawn(writer_loop(write_half, rx, options.transfer_limits));
    session_tasks.track(&writer_task);
    let (mut incoming_frames, reader_task) =
        spawn_frame_reader(read_half, options.transfer_limits);
    session_tasks.track(&reader_task);
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

    let (snapshot_control_tx, snapshot_control_rx) = mpsc::unbounded_channel();
    let (advertised_snapshot_tx, mut advertised_snapshot_rx) =
        mpsc::unbounded_channel::<AdvertisedSnapshot>();
    let snapshot_task = if file_can_send {
        let outgoing = workspace
            .outgoing
            .clone()
            .context("session negotiated sending, but local workspace has no outgoing selection")?;
        let sender = tx.clone();
        let task = tokio::spawn(snapshot_loop(
            outgoing,
            sender,
            tuning.clone(),
            snapshot_control_rx,
            matches!(
                initial_snapshot_policy,
                InitialSnapshotPolicy::PublishImmediately
            ),
            advertised_snapshot_tx,
        ));
        session_tasks.track(&task);
        Some(task)
    } else {
        None
    };
    let mut capability_runtime = CapabilityTaskRuntime::new(options.clipboard_options);
    refresh_capability_tasks(
        &capability_state,
        &mut capability_runtime,
        &mut session_tasks,
        CapabilityRefreshContext {
            session_role: session.role,
            peer_device_id: session.remote.device_id,
            remote_socket_addr,
            audio_master_secret,
            input_master_secret,
            input_options: &input_options,
            input_inbox: options.input_inbox.as_ref(),
            input_session_id: options.input_session_id.as_ref(),
            input_socket_tx: options.input_socket_tx.as_ref(),
            input_routes: options.input_routes.as_ref(),
            clipboard_hub: options.clipboard_hub.as_ref(),
            tx: &tx,
        },
    )
    .await?;
    let peer_summary = RuntimePeerSummary {
        device_id: session.remote.device_id,
        display_name: identity_display_name(&session.remote),
    };
    report_capability_state(&options.control, &peer_summary, &capability_state);
    let current_capabilities = *capabilities.borrow_and_update();
    let initial_update = capability_state
        .set_local(options.capability_profile.apply(current_capabilities))
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
                remote_socket_addr,
                audio_master_secret,
                input_master_secret,
                input_options: &input_options,
                input_inbox: options.input_inbox.as_ref(),
                input_session_id: options.input_session_id.as_ref(),
                input_socket_tx: options.input_socket_tx.as_ref(),
                input_routes: options.input_routes.as_ref(),
                clipboard_hub: options.clipboard_hub.as_ref(),
                tx: &tx,
            },
        )
        .await?;
        report_capability_state(&options.control, &peer_summary, &capability_state);
    }

    let incoming_root = workspace.incoming_root.clone();
    let outgoing_spec = workspace.outgoing.clone();
    let mut pending_revisions = BTreeMap::<u64, PendingRevision>::new();
    let mut incoming_files = HashMap::<(u64, String), IncomingFileState>::new();
    let mut advertised_snapshots = BTreeMap::<u64, ManifestSnapshot>::new();
    let mut last_reported_clock_skew_bucket = None;
    let waiting_for_initial_remote_seed = matches!(
        initial_snapshot_policy,
        InitialSnapshotPolicy::WaitForRemoteSeed
    );
    let mut pending_initial_remote_revision = None;
    options.control.report(RuntimeEvent::Connected(RuntimePeerSummary {
        device_id: session.remote.device_id,
        display_name: identity_display_name(&session.remote),
    }));
    let disconnected = loop {
        let frame = tokio::select! {
            biased;
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
                    .set_local(options.capability_profile.apply(next))
                {
                    refresh_capability_tasks(
                        &capability_state,
                        &mut capability_runtime,
                        &mut session_tasks,
                        CapabilityRefreshContext {
                            session_role: session.role,
                            peer_device_id: session.remote.device_id,
                            remote_socket_addr,
                            audio_master_secret,
                            input_master_secret,
                            input_options: &input_options,
                            input_inbox: options.input_inbox.as_ref(),
                            input_session_id: options.input_session_id.as_ref(),
                            input_socket_tx: options.input_socket_tx.as_ref(),
                            input_routes: options.input_routes.as_ref(),
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
                let enable_delete = !sync_delete && next.sync_delete;
                if let Some(hub) = &options.clipboard_hub {
                    hub.update_options(next.clipboard.clone());
                } else {
                    capability_runtime
                        .clipboard
                        .sync
                        .update_options(next.clipboard)?;
                }
                input_options = next.input;
                input_backend_generation = next.input_backend_generation;
                sync_delete = next.sync_delete;
                if enable_delete {
                    tx.send(Frame::Control(ControlMessage::SnapshotRescanRequest)).await?;
                }
                if input_changed {
                    let (generation, capabilities) = capability_state.bump_local();
                    refresh_capability_tasks(
                        &capability_state,
                        &mut capability_runtime,
                        &mut session_tasks,
                        CapabilityRefreshContext {
                            session_role: session.role,
                            peer_device_id: session.remote.device_id,
                            remote_socket_addr,
                            audio_master_secret,
                            input_master_secret,
                            input_options: &input_options,
                            input_inbox: options.input_inbox.as_ref(),
                            input_session_id: options.input_session_id.as_ref(),
                            input_socket_tx: options.input_socket_tx.as_ref(),
                            input_routes: options.input_routes.as_ref(),
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
            incoming = incoming_frames.recv() => {
                match incoming {
                    Some(Ok(frame)) => frame,
                    Some(Err(err)) if is_connection_shutdown_error(&err) => break true,
                    Some(Err(err)) => return Err(err),
                    None => break true,
                }
            }
        };
        drain_advertised_snapshots(&mut advertised_snapshot_rx, &mut advertised_snapshots);

        match frame {
            Frame::Control(ControlMessage::CapabilitiesUpdate {
                generation,
                capabilities,
            }) => {
                let changed = capability_state.apply_remote(generation, capabilities)?;
                tx.send(Frame::Control(ControlMessage::CapabilitiesAck { generation }))
                    .await?;
                if changed {
                    refresh_capability_tasks(
                        &capability_state,
                        &mut capability_runtime,
                        &mut session_tasks,
                        CapabilityRefreshContext {
                            session_role: session.role,
                            peer_device_id: session.remote.device_id,
                            remote_socket_addr,
                            audio_master_secret,
                            input_master_secret,
                            input_options: &input_options,
                            input_inbox: options.input_inbox.as_ref(),
                            input_session_id: options.input_session_id.as_ref(),
                            input_socket_tx: options.input_socket_tx.as_ref(),
                            input_routes: options.input_routes.as_ref(),
                            clipboard_hub: options.clipboard_hub.as_ref(),
                            tx: &tx,
                        },
                    )
                    .await?;
                    report_capability_state(&options.control, &peer_summary, &capability_state);
                }
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
                            remote_socket_addr,
                            audio_master_secret,
                            input_master_secret,
                            input_options: &input_options,
                            input_inbox: options.input_inbox.as_ref(),
                            input_session_id: options.input_session_id.as_ref(),
                            input_socket_tx: options.input_socket_tx.as_ref(),
                            input_routes: options.input_routes.as_ref(),
                            clipboard_hub: options.clipboard_hub.as_ref(),
                            tx: &tx,
                        },
                    )
                    .await?;
                    report_capability_state(&options.control, &peer_summary, &capability_state);
                }
            }
            Frame::Control(ControlMessage::InputChannelOffer { epoch, offer }) => {
                if !capability_state.current_epoch(epoch) {
                    tracing::debug!(?epoch, current = ?capability_state.epoch(), "忽略过期输入辅助通道");
                    continue;
                }
                if matches!(session.role, SessionRole::Host) {
                    bail!("host 输入会话收到对侧辅助通道 offer");
                }
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
                let task = tokio::spawn(async move {
                    if let Err(err) = input::run_input_session(
                        InputSessionContext::client(offer, remote_socket_addr),
                        input_master_secret,
                        local_role,
                        task_input_options,
                    )
                    .await
                    {
                        tracing::error!(error = %err, ?epoch, "输入辅助会话失败");
                    }
                });
                session_tasks.track(&task);
                capability_runtime.input_task = Some(task);
            }
            Frame::Control(ControlMessage::AudioUdpReady { epoch, port }) => {
                if !capability_state.current_epoch(epoch) {
                    tracing::debug!(?epoch, current = ?capability_state.epoch(), "忽略过期音频接收端口");
                    continue;
                }
                if let Some(AudioPlan {
                    role: LocalAudioRole::Send,
                    direction,
                }) = capability_runtime.audio_plan
                    && capability_runtime.audio_task.is_none()
                {
                    let remote_audio_addr = SocketAddr::new(remote_socket_addr.ip(), port);
                    match audio::spawn_sender(audio_master_secret, direction, remote_audio_addr) {
                        Ok(task) => {
                            capability_runtime.audio_task = Some(task);
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "无法启动音频发送通道");
                        }
                    }
                }
            }
            Frame::Control(ControlMessage::SnapshotRescanRequest) => {
                if file_can_send {
                    let _ = snapshot_control_tx.send(SnapshotLoopControl::ForcePublish);
                }
            }
            Frame::Control(ControlMessage::SnapshotAdvert {
                revision,
                snapshot,
                sender_time_ms,
            }) => {
                if !file_can_receive {
                    continue;
                }
                discard_superseded_revisions(&mut pending_revisions, &mut incoming_files, revision)
                    .await?;
                if waiting_for_initial_remote_seed {
                    pending_initial_remote_revision = Some(revision);
                }
                let root = incoming_root.as_ref().context(
                    "session negotiated receiving, but local workspace has no destination",
                )?;
                let snapshot = filter_snapshot_for_incoming_root(root, &snapshot)?;
                let local_snapshot = filter_snapshot_by_folder_depth(
                    &build_incoming_snapshot(root)?,
                    snapshot.layout,
                    snapshot.max_folder_depth,
                );
                let local_now_ms = current_unix_ms();
                let remote_clock_delta_ms = sender_time_ms as i64 - local_now_ms as i64;
                maybe_report_clock_skew(
                    remote_clock_delta_ms,
                    &mut last_reported_clock_skew_bucket,
                );
                let time_context = TimestampComparisonContext {
                    remote_clock_delta_ms,
                    local_now_ms: Some(local_now_ms),
                    remote_now_ms: Some(sender_time_ms),
                    skew_tolerance_ms: TIMESTAMP_SKEW_TOLERANCE_MS,
                    future_guard_ms: FUTURE_TIMESTAMP_GUARD_MS,
                };
                let skipped_delete_count = if !sync_delete {
                    let preview_policy = delete_policy(snapshot.layout, true);
                    build_apply_plan_with_time(
                        &snapshot,
                        &local_snapshot,
                        preview_policy,
                        time_context,
                    )
                    .delete_paths
                    .len()
                } else {
                    0
                };
                let delete_policy = delete_policy(snapshot.layout, sync_delete);
                let mut plan = build_apply_plan_with_time(
                    &snapshot,
                    &local_snapshot,
                    delete_policy,
                    time_context,
                );
                if waiting_for_initial_remote_seed
                    && pending_initial_remote_revision == Some(revision)
                {
                    plan.file_requests.append(&mut plan.skipped_newer_paths);
                }
                if file_can_send {
                    note_remote_snapshot_expectations(
                        &snapshot_control_tx,
                        &snapshot,
                        &local_snapshot,
                        &plan,
                    );
                }
                ensure_directories(root, &snapshot)?;

                if skipped_delete_count > 0 {
                    tracing::info!(
                        skipped_delete_count,
                        "检测到对端删除项, 本机未开启删除同步"
                    );
                }

                if !plan.skipped_newer_paths.is_empty() {
                    print_local_newer_paths(revision, &plan.skipped_newer_paths);
                    tx.send(Frame::Control(ControlMessage::OverwritePaused {
                        revision,
                        paths: plan.skipped_newer_paths.clone(),
                    }))
                    .await?;
                }

                if !plan.unreliable_timestamp_paths.is_empty() {
                    print_unreliable_timestamp_paths(revision, &plan.unreliable_timestamp_paths);
                }

                if plan.file_requests.is_empty() {
                    let delete_report = delete_paths_best_effort(root, &plan.delete_paths);
                    print_delete_failures(&delete_report);
                    print_standalone_delete_result(&delete_report, plan.skipped_newer_paths.len());
                    maybe_activate_initial_sender(
                        &snapshot_control_tx,
                        &mut pending_initial_remote_revision,
                        revision,
                    );
                } else {
                    let expected_files = expected_file_entries(&snapshot, &plan.file_requests)?;
                    pending_revisions.insert(
                        revision,
                        PendingRevision {
                            requested_files: plan.file_requests.len(),
                            remaining_files: plan.file_requests.iter().cloned().collect(),
                            failed_files: BTreeSet::new(),
                            expected_files,
                            delete_paths: plan.delete_paths,
                            skipped_newer_count: plan.skipped_newer_paths.len(),
                            transfer_done: false,
                        },
                    );
                    tx.send(Frame::Control(ControlMessage::FileRequest {
                        revision,
                        paths: plan.file_requests,
                    }))
                    .await?;
                }
            }
            Frame::Control(ControlMessage::FileRequest { revision, paths }) => {
                if !file_can_send {
                    continue;
                }
                let sender = tx.clone();
                let outgoing = outgoing_spec
                    .clone()
                    .context("no outgoing spec available for file request")?;
                let Some(advertised_snapshot) = advertised_snapshots.get(&revision).cloned() else {
                    let message = format!("收到未知或已过期的修订版 {revision} 文件请求");
                    tracing::warn!(revision, %message, "拒绝文件请求");
                    tx.send(Frame::Control(ControlMessage::TransferAborted {
                        revision,
                        message,
                    }))
                    .await?;
                    continue;
                };
                tokio::spawn(async move {
                    if let Err(err) = send_requested_files(
                        sender.clone(),
                        outgoing,
                        advertised_snapshot,
                        revision,
                        paths,
                    )
                    .await
                    {
                        let message = format!("发送修订版 {revision} 失败: {err:#}");
                        tracing::warn!(revision, error = %err, "发送修订版失败");
                        let _ = sender
                            .send(Frame::Control(ControlMessage::TransferAborted {
                                revision,
                                message,
                            }))
                            .await;
                    }
                });
            }
            Frame::Control(ControlMessage::OverwritePaused { revision, paths }) => {
                print_remote_overwrite_paused(revision, &paths);
            }
            Frame::Control(ControlMessage::TransferDone { revision }) => {
                if let Some(pending) = pending_revisions.get_mut(&revision) {
                    pending.transfer_done = true;
                }
                if maybe_finalize_revision(&incoming_root, &mut pending_revisions, revision) {
                    maybe_activate_initial_sender(
                        &snapshot_control_tx,
                        &mut pending_initial_remote_revision,
                        revision,
                    );
                }
            }
            Frame::Control(ControlMessage::TransferAborted { revision, message }) => {
                tracing::warn!(revision, %message, "对端中止修订版传输");
                abort_revision(&mut pending_revisions, &mut incoming_files, revision).await?;
                maybe_activate_initial_sender(
                    &snapshot_control_tx,
                    &mut pending_initial_remote_revision,
                    revision,
                );
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
            Frame::FileChunk(header, data) => {
                if !file_can_receive {
                    continue;
                }
                if !pending_revisions.contains_key(&header.revision) {
                    continue;
                }
                let root = incoming_root
                    .as_ref()
                    .context("received file data without a local destination")?;
                handle_file_chunk(
                    root,
                    &mut incoming_files,
                    &mut pending_revisions,
                    header,
                    data,
                )
                .await?;
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
    if let Some(task) = snapshot_task {
        task.abort();
    }
    reader_task.abort();
    let _ = reader_task.await;
    drop(tx);
    match writer_task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) if disconnected && is_connection_shutdown_error(&err) => {}
        Ok(Err(err)) => return Err(err),
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

async fn writer_loop<W>(
    writer: W,
    mut rx: mpsc::Receiver<Frame>,
    transfer_limits: TransferLimits,
) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut writer = FrameWriter::with_limits(writer, transfer_limits);
    while let Some(frame) = rx.recv().await {
        match writer.write_frame(frame).await {
            Ok(()) => {}
            Err(err) => {
                if let Some(message) = frame_size_limit_message(&err) {
                    tracing::warn!(%message, "已跳过超出大小限制的内容");
                    continue;
                }
                return Err(err);
            }
        }
    }
    Ok(())
}

async fn clipboard_sender_loop(
    mut rx: mpsc::UnboundedReceiver<ClipboardPayload>,
    tx: mpsc::Sender<Frame>,
) -> Result<()> {
    while let Some(payload) = rx.recv().await {
        tx.send(Frame::Clipboard(payload)).await?;
    }
    Ok(())
}

async fn snapshot_loop(
    outgoing: crate::sync::OutgoingSpec,
    tx: mpsc::Sender<Frame>,
    mut tuning: watch::Receiver<RuntimeTuning>,
    mut control_rx: mpsc::UnboundedReceiver<SnapshotLoopControl>,
    publish_initial_snapshot: bool,
    advertised_snapshot_tx: mpsc::UnboundedSender<AdvertisedSnapshot>,
) -> Result<()> {
    let (watch_tx, mut watch_rx) = mpsc::unbounded_channel::<notify::Result<Event>>();
    let mut watcher = RecommendedWatcher::new(
        move |event| {
            let _ = watch_tx.send(event);
        },
        NotifyConfig::default(),
    )
    .context("failed to start filesystem watcher")?;

    for target in watch_targets(&outgoing)? {
        let mode = if target.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher
            .watch(&target.path, mode)
            .with_context(|| format!("failed to watch shared path {}", target.path.display()))?;
    }

    let mut tuning_open = true;
    let mut ticker = time::interval(Duration::from_secs(
        tuning.borrow().interval_secs.max(1),
    ));
    let mut last_snapshot = None;
    let mut revision = 1u64;
    let debounce = Duration::from_millis(300);
    let mut echo_suppressions = SnapshotEchoSuppressions::default();
    let mut publishing_enabled = publish_initial_snapshot;

    if publishing_enabled {
        publish_snapshot_if_changed(
            &outgoing,
            &tx,
            &mut last_snapshot,
            &mut revision,
            &mut echo_suppressions,
            &advertised_snapshot_tx,
        )
        .await?;
    }
    ticker.tick().await;

    loop {
        tokio::select! {
            changed = tuning.changed(), if tuning_open => {
                if changed.is_err() {
                    tuning_open = false;
                    continue;
                }
                let interval_secs = tuning.borrow_and_update().interval_secs.max(1);
                ticker = time::interval(Duration::from_secs(interval_secs));
                ticker.tick().await;
                tracing::info!(interval_secs, "文件扫描间隔已更新");
            }
            maybe_control = control_rx.recv() => {
                let Some(control) = maybe_control else {
                    bail!("snapshot control channel closed unexpectedly");
                };
                match control {
                    SnapshotLoopControl::ExpectRemoteChanges { expectations } => {
                        echo_suppressions.note_remote_expectations(expectations);
                    }
                    SnapshotLoopControl::AdoptCurrentSnapshotAsBaselineAndEnable => {
                        last_snapshot = Some(build_snapshot(&outgoing)?);
                        publishing_enabled = true;
                    }
                    SnapshotLoopControl::ForcePublish => {
                        if publishing_enabled {
                            last_snapshot = None;
                            publish_snapshot_if_changed(
                                &outgoing,
                                &tx,
                                &mut last_snapshot,
                                &mut revision,
                                &mut echo_suppressions,
                                &advertised_snapshot_tx,
                            )
                            .await?;
                        }
                    }
                }
            }
            maybe_event = watch_rx.recv() => {
                let event = match maybe_event {
                    Some(event) => event,
                    None => bail!("filesystem watcher channel closed unexpectedly"),
                };

                if let Err(err) = event {
                    tracing::warn!(error = %err, "文件监视出错, 等待下一次重扫");
                    continue;
                }

                drain_watch_events(&mut watch_rx, debounce).await;
                if !publishing_enabled {
                    continue;
                }
                publish_snapshot_if_changed(
                    &outgoing,
                    &tx,
                    &mut last_snapshot,
                    &mut revision,
                    &mut echo_suppressions,
                    &advertised_snapshot_tx,
                )
                .await?;
            }
            _ = ticker.tick() => {
                if !publishing_enabled {
                    continue;
                }
                publish_snapshot_if_changed(
                    &outgoing,
                    &tx,
                    &mut last_snapshot,
                    &mut revision,
                    &mut echo_suppressions,
                    &advertised_snapshot_tx,
                )
                .await?;
            }
        }
    }
}

async fn publish_snapshot_if_changed(
    outgoing: &crate::sync::OutgoingSpec,
    tx: &mpsc::Sender<Frame>,
    last_snapshot: &mut Option<crate::sync::ManifestSnapshot>,
    revision: &mut u64,
    echo_suppressions: &mut SnapshotEchoSuppressions,
    advertised_snapshot_tx: &mpsc::UnboundedSender<AdvertisedSnapshot>,
) -> Result<()> {
    let snapshot = build_snapshot(outgoing)?;
    if last_snapshot.as_ref() == Some(&snapshot) {
        return Ok(());
    }

    if echo_suppressions.matches_only_remote_changes(last_snapshot.as_ref(), &snapshot) {
        *last_snapshot = Some(snapshot);
        return Ok(());
    }

    tx.send(Frame::Control(ControlMessage::SnapshotAdvert {
        revision: *revision,
        snapshot: snapshot.clone(),
        sender_time_ms: current_unix_ms(),
    }))
    .await?;
    let _ = advertised_snapshot_tx.send(AdvertisedSnapshot {
        revision: *revision,
        snapshot: snapshot.clone(),
    });
    *last_snapshot = Some(snapshot);
    *revision += 1;
    Ok(())
}

async fn drain_watch_events(
    watch_rx: &mut mpsc::UnboundedReceiver<notify::Result<Event>>,
    debounce: Duration,
) {
    let sleep = time::sleep(debounce);
    tokio::pin!(sleep);

    loop {
        tokio::select! {
            _ = &mut sleep => break,
            maybe_event = watch_rx.recv() => match maybe_event {
                Some(Ok(_)) => {
                    sleep.as_mut().reset(Instant::now() + debounce);
                }
                Some(Err(err)) => {
                    tracing::warn!(error = %err, "文件监视出错, 继续等待变更稳定");
                    sleep.as_mut().reset(Instant::now() + debounce);
                }
                None => break,
            }
        }
    }
}

impl SnapshotEchoSuppressions {
    fn note_remote_expectations(&mut self, expectations: Vec<RemoteEchoExpectation>) {
        let expires_at = Instant::now() + REMOTE_ECHO_SUPPRESSION_TTL;
        for expectation in expectations {
            self.paths.insert(
                expectation.wire_path,
                PendingRemoteEchoExpectation {
                    expected: expectation.expected,
                    expires_at,
                },
            );
        }
    }

    fn matches_only_remote_changes(
        &mut self,
        previous: Option<&ManifestSnapshot>,
        current: &ManifestSnapshot,
    ) -> bool {
        let Some(previous) = previous else {
            return false;
        };

        self.prune_expired();
        let changed_paths = snapshot_changed_paths(previous, current);
        if changed_paths.is_empty() {
            return false;
        }

        for path in &changed_paths {
            let Some(expectation) = self.paths.get(path) else {
                return false;
            };
            if !expectation_matches(&expectation.expected, current.entries.get(path)) {
                return false;
            }
        }

        for path in changed_paths {
            self.paths.remove(&path);
        }
        true
    }

    fn prune_expired(&mut self) {
        let now = Instant::now();
        self.paths
            .retain(|_, expectation| expectation.expires_at > now);
    }
}

fn note_remote_snapshot_expectations(
    snapshot_control_tx: &mpsc::UnboundedSender<SnapshotLoopControl>,
    remote_snapshot: &ManifestSnapshot,
    local_snapshot: &ManifestSnapshot,
    plan: &crate::sync::ApplyPlan,
) {
    let expectations = build_remote_echo_expectations(remote_snapshot, local_snapshot, plan);
    if expectations.is_empty() {
        return;
    }

    let _ = snapshot_control_tx.send(SnapshotLoopControl::ExpectRemoteChanges { expectations });
}

fn build_remote_echo_expectations(
    remote_snapshot: &ManifestSnapshot,
    local_snapshot: &ManifestSnapshot,
    plan: &crate::sync::ApplyPlan,
) -> Vec<RemoteEchoExpectation> {
    let mut expectations = BTreeMap::<String, SnapshotPathExpectation>::new();

    for path in &plan.file_requests {
        let Some(remote_entry) = remote_snapshot.entries.get(path) else {
            continue;
        };
        expectations.insert(
            path.clone(),
            SnapshotPathExpectation::Exact(remote_entry.clone()),
        );
        insert_ancestor_dir_expectations(&mut expectations, remote_snapshot, path);
    }

    for path in &plan.delete_paths {
        expectations.insert(path.clone(), SnapshotPathExpectation::Missing);
        insert_ancestor_dir_expectations(&mut expectations, remote_snapshot, path);
    }

    for (path, remote_entry) in &remote_snapshot.entries {
        if remote_entry.kind != EntryKind::Dir {
            continue;
        }

        if local_snapshot
            .entries
            .get(path)
            .is_none_or(|local_entry| local_entry.kind != EntryKind::Dir)
        {
            expectations
                .entry(path.clone())
                .or_insert(SnapshotPathExpectation::DirExists);
            insert_ancestor_dir_expectations(&mut expectations, remote_snapshot, path);
        }
    }

    expectations
        .into_iter()
        .map(|(wire_path, expected)| RemoteEchoExpectation {
            wire_path,
            expected,
        })
        .collect()
}

fn insert_ancestor_dir_expectations(
    expectations: &mut BTreeMap<String, SnapshotPathExpectation>,
    remote_snapshot: &ManifestSnapshot,
    wire_path: &str,
) {
    for ancestor in wire_path_ancestors(wire_path) {
        if remote_snapshot
            .entries
            .get(&ancestor)
            .is_some_and(|entry| entry.kind == EntryKind::Dir)
        {
            expectations
                .entry(ancestor)
                .or_insert(SnapshotPathExpectation::DirExists);
        }
    }
}

fn wire_path_ancestors(wire_path: &str) -> Vec<String> {
    let mut ancestors = Vec::new();
    let mut components = wire_path.split('/').collect::<Vec<_>>();
    while components.len() > 1 {
        components.pop();
        ancestors.push(components.join("/"));
    }
    ancestors
}

fn snapshot_changed_paths(previous: &ManifestSnapshot, current: &ManifestSnapshot) -> Vec<String> {
    let changed_paths = previous
        .entries
        .keys()
        .chain(current.entries.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|path| previous.entries.get(*path) != current.entries.get(*path))
        .cloned()
        .collect::<Vec<_>>();

    let mut collapsed = Vec::<String>::new();
    for path in changed_paths {
        if collapsed.iter().any(|ancestor| {
            !current.entries.contains_key(ancestor) && is_wire_path_ancestor(ancestor, &path)
        }) {
            continue;
        }
        collapsed.push(path);
    }

    collapsed
}

fn is_wire_path_ancestor(ancestor: &str, path: &str) -> bool {
    path != ancestor
        && path
            .strip_prefix(ancestor)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn expectation_matches(
    expectation: &SnapshotPathExpectation,
    current_entry: Option<&ManifestEntry>,
) -> bool {
    match expectation {
        SnapshotPathExpectation::Exact(entry) => current_entry == Some(entry),
        SnapshotPathExpectation::DirExists => {
            current_entry.is_some_and(|entry| entry.kind == EntryKind::Dir)
        }
        SnapshotPathExpectation::Missing => current_entry.is_none(),
    }
}

async fn send_requested_files(
    tx: mpsc::Sender<Frame>,
    outgoing: crate::sync::OutgoingSpec,
    advertised_snapshot: ManifestSnapshot,
    revision: u64,
    paths: Vec<String>,
) -> Result<()> {
    for path in paths {
        let advertised_entry = advertised_snapshot.entries.get(&path).with_context(|| {
            format!("requested path `{path}` is not part of revision {revision}")
        })?;
        if advertised_entry.kind != EntryKind::File {
            bail!("requested path `{path}` is not a file in revision {revision}");
        }
        send_one_file(&tx, &outgoing, revision, &path, advertised_entry).await?;
    }

    tx.send(Frame::Control(ControlMessage::TransferDone { revision }))
        .await?;
    Ok(())
}

async fn send_one_file(
    tx: &mpsc::Sender<Frame>,
    outgoing: &crate::sync::OutgoingSpec,
    revision: u64,
    wire_path: &str,
    advertised_entry: &ManifestEntry,
) -> Result<()> {
    if advertised_entry.kind != EntryKind::File {
        bail!("requested path {wire_path} is not a file in the advertised snapshot");
    }
    let path = resolve_outgoing_path(outgoing, wire_path)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("failed to inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!("requested path {} is not a regular file", path.display());
    }

    let expected_hash = advertised_entry
        .hash
        .as_deref()
        .context("advertised file entry is missing a content hash")?;

    let mut file = File::open(&path)
        .await
        .with_context(|| format!("failed to open {}", path.display()))?;
    let mut offset = 0u64;
    let mut buffer = vec![0u8; FILE_STREAM_CHUNK_SIZE];
    let mut hasher = Sha256::new();

    if advertised_entry.size == 0 {
        let actual_hash = format!("{:x}", Sha256::digest([]));
        if actual_hash != expected_hash {
            bail!(
                "共享文件 {} 在修订版 {} 发送前已变化：期望哈希 {}，当前空内容哈希 {}",
                path.display(),
                revision,
                expected_hash,
                actual_hash
            );
        }
        tx.send(Frame::FileChunk(
            FileChunkHeader {
                revision,
                path: wire_path.to_string(),
                offset: 0,
                total_size: 0,
                modified_ms: advertised_entry.modified_ms,
                executable: advertised_entry.executable,
                final_chunk: true,
            },
            Vec::new(),
        ))
        .await?;
        return Ok(());
    }

    while offset < advertised_entry.size {
        let remaining = advertised_entry.size - offset;
        let read_limit = usize::try_from(remaining.min(FILE_STREAM_CHUNK_SIZE as u64))
            .expect("chunk size bound should fit usize");
        let read = file.read(&mut buffer[..read_limit]).await?;
        if read == 0 {
            bail!(
                "共享文件 {} 在修订版 {} 发送时长度发生变化：期望 {} 字节，实际只读到 {} 字节",
                path.display(),
                revision,
                advertised_entry.size,
                offset
            );
        }
        hasher.update(&buffer[..read]);
        let next_offset = offset + read as u64;
        let final_chunk = next_offset >= advertised_entry.size;
        tx.send(Frame::FileChunk(
            FileChunkHeader {
                revision,
                path: wire_path.to_string(),
                offset,
                total_size: advertised_entry.size,
                modified_ms: advertised_entry.modified_ms,
                executable: advertised_entry.executable,
                final_chunk,
            },
            buffer[..read].to_vec(),
        ))
        .await?;
        offset = next_offset;
    }

    let actual_hash = format!("{:x}", hasher.finalize());
    if actual_hash != expected_hash {
        bail!(
            "共享文件 {} 在修订版 {} 发送时内容发生变化：期望哈希 {}，实际发送哈希 {}",
            path.display(),
            revision,
            expected_hash,
            actual_hash
        );
    }

    Ok(())
}

async fn handle_file_chunk(
    root: &Path,
    incoming_files: &mut HashMap<(u64, String), IncomingFileState>,
    pending_revisions: &mut BTreeMap<u64, PendingRevision>,
    header: FileChunkHeader,
    data: Vec<u8>,
) -> Result<()> {
    if pending_revisions
        .get(&header.revision)
        .is_some_and(|pending| pending.failed_files.contains(&header.path))
    {
        return Ok(());
    }

    let key = (header.revision, header.path.clone());
    if header.offset == 0 {
        let expected_entry = match pending_revisions
            .get(&header.revision)
            .and_then(|pending| pending.expected_files.get(&header.path))
            .cloned()
        {
            Some(entry) => entry,
            None => {
                report_incoming_file_failure(
                    root,
                    incoming_files,
                    pending_revisions,
                    header.revision,
                    &header.path,
                    None,
                    None,
                    anyhow!("missing advertised file metadata for {}", header.path),
                )
                .await;
                return Ok(());
            }
        };
        match begin_incoming_file(root, &header, expected_entry).await {
            Ok(state) => {
                incoming_files.insert(key.clone(), state);
            }
            Err((final_path, err)) => {
                report_incoming_file_failure(
                    root,
                    incoming_files,
                    pending_revisions,
                    header.revision,
                    &header.path,
                    final_path,
                    None,
                    err,
                )
                .await;
                return Ok(());
            }
        }
    }

    let write_result = {
        let state = match incoming_files.get_mut(&key) {
            Some(state) => state,
            None => {
                report_incoming_file_failure(
                    root,
                    incoming_files,
                    pending_revisions,
                    header.revision,
                    &header.path,
                    None,
                    None,
                    anyhow!("missing transfer state for {}", header.path),
                )
                .await;
                return Ok(());
            }
        };

        if state.written != header.offset {
            Err((
                Some(state.final_path.clone()),
                Some(state.temp_path.clone()),
                anyhow!("incoming chunk metadata mismatch for {}", header.path,),
            ))
        } else if header.total_size != state.expected_entry.size
            || header.modified_ms != state.expected_entry.modified_ms
            || header.executable != state.expected_entry.executable
        {
            Err((
                Some(state.final_path.clone()),
                Some(state.temp_path.clone()),
                anyhow!(
                    "incoming chunk metadata drifted for {}: expected size={}, modified_ms={}, executable={}, got size={}, modified_ms={}, executable={}",
                    header.path,
                    state.expected_entry.size,
                    state.expected_entry.modified_ms,
                    state.expected_entry.executable,
                    header.total_size,
                    header.modified_ms,
                    header.executable
                ),
            ))
        } else if let Err(err) = state.file.write_all(&data).await {
            Err((
                Some(state.final_path.clone()),
                Some(state.temp_path.clone()),
                err.into(),
            ))
        } else {
            state.hasher.update(&data);
            state.written += data.len() as u64;
            Ok(())
        }
    };

    if let Err((final_path, temp_path, err)) = write_result {
        report_incoming_file_failure(
            root,
            incoming_files,
            pending_revisions,
            header.revision,
            &header.path,
            final_path,
            temp_path,
            err,
        )
        .await;
        return Ok(());
    }

    if header.final_chunk {
        let state = match incoming_files.remove(&key) {
            Some(state) => state,
            None => {
                report_incoming_file_failure(
                    root,
                    incoming_files,
                    pending_revisions,
                    header.revision,
                    &header.path,
                    None,
                    None,
                    anyhow!("missing final transfer state for {}", header.path),
                )
                .await;
                return Ok(());
            }
        };
        let final_path = Some(state.final_path.clone());
        let temp_path = Some(state.temp_path.clone());

        if let Err(err) = finalize_incoming_file(state).await {
            report_incoming_file_failure(
                root,
                incoming_files,
                pending_revisions,
                header.revision,
                &header.path,
                final_path,
                temp_path,
                err,
            )
            .await;
            return Ok(());
        }

        if let Some(pending) = pending_revisions.get_mut(&header.revision) {
            pending.remaining_files.remove(&header.path);
            pending.expected_files.remove(&header.path);
        }
        let _ = maybe_finalize_revision(
            &Some(root.to_path_buf()),
            pending_revisions,
            header.revision,
        );
    }

    Ok(())
}

fn drain_advertised_snapshots(
    advertised_snapshot_rx: &mut mpsc::UnboundedReceiver<AdvertisedSnapshot>,
    advertised_snapshots: &mut BTreeMap<u64, ManifestSnapshot>,
) {
    while let Ok(advertised) = advertised_snapshot_rx.try_recv() {
        advertised_snapshots.insert(advertised.revision, advertised.snapshot);
    }

    while advertised_snapshots.len() > ADVERTISED_SNAPSHOT_CACHE_LIMIT {
        let Some(oldest_revision) = advertised_snapshots.keys().next().copied() else {
            break;
        };
        advertised_snapshots.remove(&oldest_revision);
    }
}

fn expected_file_entries(
    snapshot: &ManifestSnapshot,
    paths: &[String],
) -> Result<BTreeMap<String, ManifestEntry>> {
    paths
        .iter()
        .map(|path| {
            let entry = snapshot
                .entries
                .get(path)
                .with_context(|| format!("snapshot is missing requested path `{path}`"))?;
            if entry.kind != EntryKind::File {
                bail!("snapshot path `{path}` is not a file");
            }
            Ok((path.clone(), entry.clone()))
        })
        .collect()
}

async fn begin_incoming_file(
    root: &Path,
    header: &FileChunkHeader,
    expected_entry: ManifestEntry,
) -> std::result::Result<IncomingFileState, (Option<PathBuf>, anyhow::Error)> {
    if expected_entry.kind != EntryKind::File {
        return Err((
            None,
            anyhow!("advertised path {} is not a file", header.path),
        ));
    }
    if header.total_size != expected_entry.size
        || header.modified_ms != expected_entry.modified_ms
        || header.executable != expected_entry.executable
    {
        return Err((
            None,
            anyhow!(
                "incoming file metadata mismatch for {}: expected size={}, modified_ms={}, executable={}, got size={}, modified_ms={}, executable={}",
                header.path,
                expected_entry.size,
                expected_entry.modified_ms,
                expected_entry.executable,
                header.total_size,
                header.modified_ms,
                header.executable
            ),
        ));
    }

    let final_path = match resolve_incoming_path(root, &header.path) {
        Ok(path) => path,
        Err(err) => return Err((None, err)),
    };

    if let Some(parent) = final_path.parent()
        && let Err(err) = tokio::fs::create_dir_all(parent).await
    {
        return Err((Some(final_path), err.into()));
    }

    let temp_path = temp_file_path(&final_path);
    let _ = tokio::fs::remove_file(&temp_path).await;
    let file = match File::create(&temp_path).await {
        Ok(file) => file,
        Err(err) => return Err((Some(final_path), err.into())),
    };

    Ok(IncomingFileState {
        file,
        temp_path,
        final_path,
        expected_entry,
        hasher: Sha256::new(),
        written: 0,
    })
}

async fn finalize_incoming_file(state: IncomingFileState) -> Result<()> {
    let IncomingFileState {
        mut file,
        temp_path,
        final_path,
        expected_entry,
        hasher,
        written,
    } = state;

    file.flush().await?;
    drop(file);

    if written != expected_entry.size {
        bail!(
            "received size mismatch for {}: expected {}, got {}",
            final_path.display(),
            expected_entry.size,
            written
        );
    }

    let expected_hash = expected_entry
        .hash
        .as_deref()
        .context("advertised file entry is missing a content hash")?;
    let actual_hash = format!("{:x}", hasher.finalize());
    if actual_hash != expected_hash {
        bail!(
            "received hash mismatch for {}: expected {}, got {}",
            final_path.display(),
            expected_hash,
            actual_hash
        );
    }

    replace_destination(&final_path, &temp_path).await?;
    apply_file_metadata(
        &final_path,
        expected_entry.modified_ms,
        expected_entry.executable,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn report_incoming_file_failure(
    root: &Path,
    incoming_files: &mut HashMap<(u64, String), IncomingFileState>,
    pending_revisions: &mut BTreeMap<u64, PendingRevision>,
    revision: u64,
    wire_path: &str,
    final_path: Option<PathBuf>,
    temp_path: Option<PathBuf>,
    err: anyhow::Error,
) {
    let key = (revision, wire_path.to_string());
    let mut final_path = final_path;
    let mut temp_path = temp_path;

    if let Some(state) = incoming_files.remove(&key) {
        if final_path.is_none() {
            final_path = Some(state.final_path);
        }
        if temp_path.is_none() {
            temp_path = Some(state.temp_path);
        }
    }

    if let Some(temp_path) = temp_path {
        let _ = tokio::fs::remove_file(temp_path).await;
    }

    if let Some(pending) = pending_revisions.get_mut(&revision) {
        pending.remaining_files.remove(wire_path);
        pending.failed_files.insert(wire_path.to_string());
        pending.expected_files.remove(wire_path);
    }

    let target = final_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| wire_path.to_string());
    tracing::warn!(%target, revision, error = %err, "无法更新文件");

    let _ = maybe_finalize_revision(&Some(root.to_path_buf()), pending_revisions, revision);
}

fn maybe_finalize_revision(
    incoming_root: &Option<PathBuf>,
    pending_revisions: &mut BTreeMap<u64, PendingRevision>,
    revision: u64,
) -> bool {
    let should_finalize = pending_revisions
        .get(&revision)
        .is_some_and(|pending| pending.transfer_done && pending.remaining_files.is_empty());

    if should_finalize
        && let Some(pending) = pending_revisions.remove(&revision)
        && let Some(root) = incoming_root
    {
        let delete_report = delete_paths_best_effort(root, &pending.delete_paths);
        print_delete_failures(&delete_report);
        let updated_files = pending
            .requested_files
            .saturating_sub(pending.failed_files.len());
        print_revision_result(
            updated_files,
            pending.failed_files.len(),
            pending.skipped_newer_count,
            &delete_report,
        );
        return true;
    }

    false
}

async fn discard_superseded_revisions(
    pending_revisions: &mut BTreeMap<u64, PendingRevision>,
    incoming_files: &mut HashMap<(u64, String), IncomingFileState>,
    keep_revision: u64,
) -> Result<()> {
    let stale_revisions = pending_revisions
        .keys()
        .copied()
        .filter(|revision| *revision < keep_revision)
        .collect::<Vec<_>>();

    for revision in stale_revisions {
        abort_revision(pending_revisions, incoming_files, revision).await?;
    }

    Ok(())
}

async fn abort_revision(
    pending_revisions: &mut BTreeMap<u64, PendingRevision>,
    incoming_files: &mut HashMap<(u64, String), IncomingFileState>,
    revision: u64,
) -> Result<()> {
    pending_revisions.remove(&revision);

    let stale_files = incoming_files
        .keys()
        .filter(|(file_revision, _)| *file_revision == revision)
        .cloned()
        .collect::<Vec<_>>();

    for key in stale_files {
        if let Some(state) = incoming_files.remove(&key) {
            let _ = tokio::fs::remove_file(&state.temp_path).await;
        }
    }

    Ok(())
}

async fn replace_destination(destination: &Path, temp_path: &Path) -> Result<()> {
    if let Ok(metadata) = tokio::fs::symlink_metadata(destination).await {
        if metadata.file_type().is_symlink() || metadata.is_file() {
            tokio::fs::remove_file(destination).await?;
        } else if metadata.is_dir() {
            tokio::fs::remove_dir_all(destination).await?;
        }
    }
    tokio::fs::rename(temp_path, destination).await?;
    Ok(())
}

fn print_delete_failures(report: &crate::sync::DeleteReport) {
    for failure in &report.failures {
        let target = failure
            .local_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| failure.wire_path.clone());
        tracing::warn!(%target, reason = %failure.reason, "无法归档删除项");
    }
}

fn print_local_newer_paths(revision: u64, paths: &[String]) {
    tracing::info!(revision, count = paths.len(), paths = %paths.join(" | "), "本地文件较新, 已暂停覆盖");
}

fn print_unreliable_timestamp_paths(revision: u64, paths: &[String]) {
    tracing::warn!(revision, count = paths.len(), paths = %paths.join(" | "), "文件时间戳异常, 已忽略时间戳保护");
}

fn print_remote_overwrite_paused(revision: u64, paths: &[String]) {
    tracing::info!(revision, count = paths.len(), paths = %paths.join(" | "), "对端保留了本地较新的文件");
}

fn print_standalone_delete_result(report: &crate::sync::DeleteReport, skipped_newer: usize) {
    tracing::info!(
        skipped_newer,
        archived = report.archived_count,
        delete_failures = report.failures.len(),
        "同步删除结果已应用"
    );
}

fn print_revision_result(
    updated_files: usize,
    failed_updates: usize,
    skipped_newer: usize,
    delete_report: &crate::sync::DeleteReport,
) {
    tracing::info!(
        updated_files,
        skipped_newer,
        failed_updates,
        archived = delete_report.archived_count,
        delete_failures = delete_report.failures.len(),
        "同步修订版处理完成"
    );
}

fn maybe_report_clock_skew(
    remote_clock_delta_ms: i64,
    last_reported_clock_skew_bucket: &mut Option<i64>,
) {
    let bucket = clock_skew_bucket(remote_clock_delta_ms);
    if bucket == *last_reported_clock_skew_bucket {
        return;
    }

    *last_reported_clock_skew_bucket = bucket;
    if bucket.is_some() {
        tracing::warn!(
            remote_clock_delta_ms,
            delta = %format_clock_delta(remote_clock_delta_ms),
            "检测到两端系统时间偏差, 将修正时间戳比较"
        );
    }
}

fn clock_skew_bucket(remote_clock_delta_ms: i64) -> Option<i64> {
    let abs_delta_ms = remote_clock_delta_ms.unsigned_abs();
    if abs_delta_ms < CLOCK_SKEW_WARNING_MS {
        None
    } else {
        Some(remote_clock_delta_ms / CLOCK_SKEW_WARNING_MS as i64)
    }
}

fn format_clock_delta(remote_clock_delta_ms: i64) -> String {
    let abs_delta_ms = remote_clock_delta_ms.unsigned_abs();
    let sign = if remote_clock_delta_ms >= 0 {
        "快"
    } else {
        "慢"
    };

    if abs_delta_ms >= 60 * 60 * 1_000 {
        format!(
            "{} {:.1} 小时",
            sign,
            abs_delta_ms as f64 / (60.0 * 60.0 * 1_000.0)
        )
    } else if abs_delta_ms >= 60 * 1_000 {
        format!(
            "{} {:.1} 分钟",
            sign,
            abs_delta_ms as f64 / (60.0 * 1_000.0)
        )
    } else if abs_delta_ms >= 1_000 {
        format!("{} {:.1} 秒", sign, abs_delta_ms as f64 / 1_000.0)
    } else {
        format!("{} {} 毫秒", sign, abs_delta_ms)
    }
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

async fn choose_peer(
    peer_query: Option<&str>,
    timeout: Duration,
    _headless: bool,
    local_workspace: &crate::sync::WorkspaceSummary,
    discovery_config: &crate::config::DiscoveryConfig,
) -> Result<PeerTarget> {
    let query = require_peer_query(peer_query)?;
    if let Some(address) = parse_direct_peer_addr(query) {
        return Ok(PeerTarget::Direct(address));
    }
    let peers = discovery::browse(timeout, discovery_config).await?;
    let peer = select_peer_from_query(&peers, query)?;
    if peer.protocol_version != PROTOCOL_VERSION {
        bail!(
            "设备协议版本不兼容: 本机 {}, 对侧 {}",
            PROTOCOL_VERSION,
            peer.protocol_version
        );
    }
    ensure_discovered_peer_modes_match(&peer, local_workspace)?;
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
    local_workspace: &crate::sync::WorkspaceSummary,
) -> Option<String> {
    let file_agreement =
        negotiate_file_sync_modes(peer.file_sync_mode, local_workspace.file_sync_mode);
    let clipboard_agreement =
        negotiate_clipboard(peer.clipboard_mode, local_workspace.clipboard_mode);
    let audio_compatible = audio_modes_compatible(peer.audio_mode, local_workspace.audio_mode);
    let input_compatible = negotiate_input(peer.input_mode, local_workspace.input_mode).is_some();
    if file_agreement.any_direction()
        || clipboard_agreement.any_direction()
        || audio_compatible
        || input_compatible
    {
        return None;
    }

    Some(format!(
        "找到设备 {}, 但同步模式不匹配: 对端广播为 文件:{} / 剪贴板:{} / 音频:{} / 输入:{}; 本机为 文件:{} / 剪贴板:{} / 音频:{} / 输入:{}. 当前没有任何可用同步方向.",
        peer.display_name(),
        peer.file_sync_mode.label(),
        peer.clipboard_mode.label(),
        peer.audio_mode.label(),
        peer.input_mode.label(),
        local_workspace.file_sync_mode.label(),
        local_workspace.clipboard_mode.label(),
        local_workspace.audio_mode.label(),
        local_workspace.input_mode.label(),
    ))
}

fn ensure_discovered_peer_modes_match(
    peer: &DiscoveredPeer,
    local_workspace: &crate::sync::WorkspaceSummary,
) -> Result<()> {
    if let Some(message) = discovered_peer_mode_mismatch_message(peer, local_workspace) {
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

async fn sleep_before_reconnect(delay: Duration) {
    if delay.is_zero() {
        return;
    }

    tracing::info!(delay_secs = delay.as_secs(), "等待后重试连接");
    time::sleep(delay).await;
}

fn next_reconnect_delay(current: Duration) -> Duration {
    let next = current.as_secs().saturating_mul(2).max(1);
    Duration::from_secs(next.min(RECONNECT_MAX_DELAY.as_secs()))
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
    agreement: &SessionAgreement,
    remote_addr: &str,
) -> Result<()> {
    let remote_summary = payload.workspace.summary_lines().join(" | ");
    let mut local_summary = options
        .workspace
        .local_summary_lines_with_input(options.clipboard_mode, options.audio_mode, options.input_mode)
        .join(" | ");
    if options.workspace.incoming_root.is_some() {
        local_summary.push_str(" | 删除同步: ");
        local_summary.push_str(sync_delete_label(options.sync_delete));
    }
    tracing::info!(
        peer = %identity_display_name(&payload.client),
        device_id = %short_uuid(&payload.client.device_id),
        fingerprint = %crypto::short_identity_fingerprint(&payload.client.identity_public_key)?,
        remote_addr,
        remote_summary = %remote_summary,
        local_summary = %local_summary,
        file = file_sync_agreement_label(SessionRole::Host, agreement),
        clipboard = %clipboard_summary_line(
            SessionRole::Host,
            options.clipboard_mode,
            payload.workspace.clipboard_mode,
        ),
        input = %input_summary_line(options.input_mode, payload.workspace.input_mode),
        "收到同步请求"
    );
    Ok(())
}

fn print_connected_peer(
    remote: &DeviceIdentity,
    agreement: &SessionAgreement,
    remote_workspace: &crate::sync::WorkspaceSummary,
    local_input_mode: InputMode,
) -> Result<()> {
    tracing::info!(
        peer = %identity_display_name(remote),
        device_id = %short_uuid(&remote.device_id),
        fingerprint = %crypto::short_identity_fingerprint(&remote.identity_public_key)?,
        file = file_sync_agreement_label(SessionRole::Client, agreement),
        audio = remote_workspace.audio_mode.label(),
        input = %input_summary_line(local_input_mode, remote_workspace.input_mode),
        "连接已建立"
    );
    Ok(())
}

fn negotiate_file_sync_modes(
    host_mode: FileSyncMode,
    client_mode: FileSyncMode,
) -> SessionAgreement {
    negotiate_sync_directions(
        host_mode.can_send(),
        host_mode.can_receive(),
        client_mode.can_send(),
        client_mode.can_receive(),
    )
}

fn delete_policy(layout: crate::sync::SnapshotLayout, sync_delete: bool) -> DeletePolicy {
    if !sync_delete {
        return DeletePolicy::Never;
    }

    match layout {
        crate::sync::SnapshotLayout::RootContents => DeletePolicy::MirrorAll,
        crate::sync::SnapshotLayout::SelectedItems => DeletePolicy::MirrorSelectedItems,
    }
}

fn resolve_initial_snapshot_policy(
    _role: SessionRole,
    workspace: &WorkspaceSpec,
    remote_workspace: &crate::sync::WorkspaceSummary,
    file_can_send: bool,
    file_can_receive: bool,
) -> Result<InitialSnapshotPolicy> {
    if !file_can_send {
        return Ok(InitialSnapshotPolicy::PublishImmediately);
    }

    if !file_can_receive {
        return Ok(InitialSnapshotPolicy::PublishImmediately);
    }

    let local_initial = workspace
        .initial_sync
        .context("本机双向文件同步缺少初始状态来源配置")?;
    let remote_initial = remote_workspace
        .initial_sync
        .context("对端双向文件同步缺少初始状态来源配置")?;

    match (local_initial, remote_initial) {
        (InitialSyncMode::This, InitialSyncMode::Other) => {
            Ok(InitialSnapshotPolicy::PublishImmediately)
        }
        (InitialSyncMode::Other, InitialSyncMode::This) => {
            Ok(InitialSnapshotPolicy::WaitForRemoteSeed)
        }
        (InitialSyncMode::This, InitialSyncMode::This) => bail!(
            "双向初始状态冲突: 本机和对端都配置了 initial = this, 请让一端配置 this, 另一端配置 other"
        ),
        (InitialSyncMode::Other, InitialSyncMode::Other) => bail!(
            "双向初始状态冲突: 本机和对端都配置了 initial = other, 请让一端配置 this, 另一端配置 other"
        ),
    }
}

fn maybe_activate_initial_sender(
    snapshot_control_tx: &mpsc::UnboundedSender<SnapshotLoopControl>,
    pending_initial_remote_revision: &mut Option<u64>,
    revision: u64,
) {
    if *pending_initial_remote_revision != Some(revision) {
        return;
    }

    *pending_initial_remote_revision = None;
    let _ = snapshot_control_tx.send(SnapshotLoopControl::AdoptCurrentSnapshotAsBaselineAndEnable);
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
    let summary = params
        .workspace
        .session_summary(params.clipboard_mode, params.audio_mode, params.input_mode);
    let server = device_identity(params.device, params.instance_name);
    let proof = match params.auth_method {
        PairAuthMethod::Pin => crypto::sign_pair_decision(
            params.exporter,
            params.request_id,
            params.pin.context("missing PIN for pair decision")?,
            params.accepted,
            &params.message,
            &server,
            params.agreement,
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
            params.agreement,
            &summary,
            params.server_trusts_client,
            params.trust_established,
        )?,
    };
    Ok(ControlMessage::PairDecision {
        accepted: params.accepted,
        message: params.message,
        server,
        workspace: summary,
        agreement: params.agreement.clone(),
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
    let mut local_summary = options
        .workspace
        .local_summary_lines_with_input(options.clipboard_mode, options.audio_mode, options.input_mode)
        .join(" | ");
    if options.workspace.incoming_root.is_some() {
        local_summary.push_str(" | 删除同步: ");
        local_summary.push_str(sync_delete_label(options.sync_delete));
    }
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

fn file_sync_agreement_label(role: SessionRole, agreement: &SessionAgreement) -> &'static str {
    direction_label(role, agreement)
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

fn temp_file_path(destination: &Path) -> PathBuf {
    let suffix = rand::rng().random_range(1000..9999);
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("synly");
    destination.with_file_name(format!(".{}.{}.synly.part", file_name, suffix))
}

fn short_uuid(id: &Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        Advertisement, FILE_STREAM_CHUNK_SIZE, InitialSnapshotPolicy, SessionRole,
        SessionTaskAbortGuard, SnapshotEchoSuppressions, SnapshotPathExpectation,
        accept_policy_label, bootstrap_device_name_matches, bootstrap_peer_label,
        build_remote_echo_expectations, choose_peer, delete_policy, handle_file_chunk,
        identity_display_name, input_task_restart_required, is_connection_shutdown_error,
        next_reconnect_delay,
        parse_direct_peer_addr, peer_matches_query, preferred_peer_query, race_peer_addresses,
        resolve_audio_plan, resolve_initial_snapshot_policy, run_with_session_notifications,
        run_advertisement_updates, select_peer_from_query, send_one_file,
        should_auto_accept_request, should_try_direct_trusted,
        trusted_transport_for_device, trusted_transport_for_identity,
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
        DeviceIdentity, FileChunkHeader, Frame, PairAuthMethod, PROTOCOL_VERSION,
        RuntimeCapabilities,
    };
    use crate::runtime_control::{RuntimeControl, RuntimeTuning};
    use crate::runtime_options::PairingRuntimeOptions;
    use crate::settings::{AudioMode, ClipboardMode, FileSyncMode, InitialSyncMode};
    use crate::sync::{
        ApplyPlan, DeletePolicy, EntryKind, ManifestEntry, ManifestSnapshot, OutgoingSpec,
        SnapshotLayout, WorkspaceSpec, build_snapshot,
    };
    use crate::system_notification::{ConnectionEvent, NotificationPeer, SessionNotifier};
    use sha2::{Digest, Sha256};
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::env;
    use std::fs;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    #[test]
    fn input_backend_generation_restarts_the_input_task() {
        let input = InputRuntimeOptions {
            mode: InputMode::Receive,
            edge: ScreenEdge::Right,
            hotkey: Hotkey::DEFAULT.parse().unwrap(),
            reverse_mouse_wheel: false,
            reverse_trackpad: false,
            block_switch_on_press: false,
            key_mapping: crate::input::KeyMappingConfig::default(),
        };

        assert!(!input_task_restart_required(&input, &input, 3, 3));
        assert!(input_task_restart_required(&input, &input, 3, 4));
    }

    #[test]
    fn delete_policy_stays_disabled_when_sync_delete_is_off() {
        assert!(matches!(
            delete_policy(SnapshotLayout::RootContents, false),
            DeletePolicy::Never
        ));
        assert!(matches!(
            delete_policy(SnapshotLayout::SelectedItems, false),
            DeletePolicy::Never
        ));
    }

    #[test]
    fn delete_policy_mirrors_root_contents_when_enabled() {
        assert!(matches!(
            delete_policy(SnapshotLayout::RootContents, true),
            DeletePolicy::MirrorAll
        ));
    }

    #[test]
    fn delete_policy_limits_selected_items_when_enabled() {
        assert!(matches!(
            delete_policy(SnapshotLayout::SelectedItems, true),
            DeletePolicy::MirrorSelectedItems
        ));
    }

    #[test]
    fn initial_snapshot_policy_publishes_when_local_is_initial_source() {
        let local = WorkspaceSpec::for_both(PathBuf::from("/tmp/local"))
            .unwrap()
            .with_initial_sync(Some(InitialSyncMode::This));
        let remote = WorkspaceSpec::for_both(PathBuf::from("/tmp/remote"))
            .unwrap()
            .with_initial_sync(Some(InitialSyncMode::Other))
            .session_summary(ClipboardMode::Off, AudioMode::Off, InputMode::Off);

        let policy =
            resolve_initial_snapshot_policy(SessionRole::Host, &local, &remote, true, true)
                .unwrap();

        assert_eq!(policy, InitialSnapshotPolicy::PublishImmediately);
    }

    #[test]
    fn initial_snapshot_policy_waits_when_remote_is_initial_source() {
        let local = WorkspaceSpec::for_both(PathBuf::from("/tmp/local"))
            .unwrap()
            .with_initial_sync(Some(InitialSyncMode::Other));
        let remote = WorkspaceSpec::for_both(PathBuf::from("/tmp/remote"))
            .unwrap()
            .with_initial_sync(Some(InitialSyncMode::This))
            .session_summary(ClipboardMode::Off, AudioMode::Off, InputMode::Off);

        let policy =
            resolve_initial_snapshot_policy(SessionRole::Client, &local, &remote, true, true)
                .unwrap();

        assert_eq!(policy, InitialSnapshotPolicy::WaitForRemoteSeed);
    }

    #[test]
    fn initial_snapshot_policy_rejects_conflicting_initial_sources() {
        let local = WorkspaceSpec::for_both(PathBuf::from("/tmp/local"))
            .unwrap()
            .with_initial_sync(Some(InitialSyncMode::This));
        let remote = WorkspaceSpec::for_both(PathBuf::from("/tmp/remote"))
            .unwrap()
            .with_initial_sync(Some(InitialSyncMode::This))
            .session_summary(ClipboardMode::Off, AudioMode::Off, InputMode::Off);

        let err = resolve_initial_snapshot_policy(SessionRole::Host, &local, &remote, true, true)
            .unwrap_err()
            .to_string();

        assert!(err.contains("双向初始状态冲突"));
        assert!(err.contains("initial = this"));
    }

    #[test]
    fn reconnect_backoff_doubles_until_cap() {
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(2)),
            Duration::from_secs(4)
        );
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(10)),
            Duration::from_secs(20)
        );
        assert_eq!(
            next_reconnect_delay(Duration::from_secs(20)),
            Duration::from_secs(20)
        );
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
    fn remote_echo_expectations_cover_files_dirs_and_deletes() {
        let remote = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([
                ("docs".to_string(), dir_entry()),
                (
                    "docs/readme.txt".to_string(),
                    file_entry("remote-readme", 10),
                ),
                ("empty".to_string(), dir_entry()),
            ]),
        };
        let local = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([("old.txt".to_string(), file_entry("old", 5))]),
        };
        let plan = ApplyPlan {
            file_requests: vec!["docs/readme.txt".to_string()],
            delete_paths: vec!["old.txt".to_string()],
            skipped_newer_paths: Vec::new(),
            unreliable_timestamp_paths: Vec::new(),
        };

        let expectations = build_remote_echo_expectations(&remote, &local, &plan)
            .into_iter()
            .map(|expectation| (expectation.wire_path, expectation.expected))
            .collect::<BTreeMap<_, _>>();

        assert_eq!(
            expectations.get("docs/readme.txt"),
            Some(&SnapshotPathExpectation::Exact(file_entry(
                "remote-readme",
                10
            )))
        );
        assert_eq!(
            expectations.get("docs"),
            Some(&SnapshotPathExpectation::DirExists)
        );
        assert_eq!(
            expectations.get("empty"),
            Some(&SnapshotPathExpectation::DirExists)
        );
        assert_eq!(
            expectations.get("old.txt"),
            Some(&SnapshotPathExpectation::Missing)
        );
    }

    #[test]
    fn snapshot_echo_suppression_consumes_matching_remote_diff() {
        let previous = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([("docs".to_string(), dir_entry())]),
        };
        let current = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([
                ("docs".to_string(), dir_entry()),
                (
                    "docs/readme.txt".to_string(),
                    file_entry("remote-readme", 10),
                ),
            ]),
        };
        let mut suppressions = SnapshotEchoSuppressions::default();
        suppressions.note_remote_expectations(vec![super::RemoteEchoExpectation {
            wire_path: "docs/readme.txt".to_string(),
            expected: SnapshotPathExpectation::Exact(file_entry("remote-readme", 10)),
        }]);

        assert!(suppressions.matches_only_remote_changes(Some(&previous), &current));
        assert!(suppressions.paths.is_empty());
    }

    #[test]
    fn snapshot_echo_suppression_does_not_hide_unrelated_local_diff() {
        let previous = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::new(),
        };
        let current = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([("notes.txt".to_string(), file_entry("local", 5))]),
        };
        let mut suppressions = SnapshotEchoSuppressions::default();
        suppressions.note_remote_expectations(vec![super::RemoteEchoExpectation {
            wire_path: "docs/readme.txt".to_string(),
            expected: SnapshotPathExpectation::Exact(file_entry("remote-readme", 10)),
        }]);

        assert!(!suppressions.matches_only_remote_changes(Some(&previous), &current));
    }

    #[test]
    fn snapshot_echo_suppression_requires_expected_entry_match() {
        let previous = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::new(),
        };
        let current = ManifestSnapshot {
            layout: SnapshotLayout::RootContents,
            max_folder_depth: None,
            entries: BTreeMap::from([("docs/readme.txt".to_string(), file_entry("local", 5))]),
        };
        let mut suppressions = SnapshotEchoSuppressions::default();
        suppressions.note_remote_expectations(vec![super::RemoteEchoExpectation {
            wire_path: "docs/readme.txt".to_string(),
            expected: SnapshotPathExpectation::Exact(file_entry("remote-readme", 10)),
        }]);

        assert!(!suppressions.matches_only_remote_changes(Some(&previous), &current));
    }

    #[tokio::test]
    async fn send_one_file_streams_large_files_in_multiple_chunks() {
        let dir = test_dir("file-streaming");
        fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("large.bin");
        let expected = vec![0x5au8; FILE_STREAM_CHUNK_SIZE * 2 + 37];
        fs::write(&file_path, &expected).unwrap();

        let outgoing = OutgoingSpec::RootContents {
            root: dir.clone(),
            max_folder_depth: None,
        };
        let advertised_snapshot = build_snapshot(&outgoing).unwrap();
        let advertised_entry = advertised_snapshot
            .entries
            .get("large.bin")
            .unwrap()
            .clone();
        let (tx, mut rx) = mpsc::channel(16);

        send_one_file(&tx, &outgoing, 1, "large.bin", &advertised_entry)
            .await
            .unwrap();
        drop(tx);

        let mut chunk_count = 0usize;
        let mut assembled = Vec::new();
        while let Some(frame) = rx.recv().await {
            match frame {
                Frame::FileChunk(header, data) => {
                    assert_eq!(header.path, "large.bin");
                    assert_eq!(header.offset, assembled.len() as u64);
                    assembled.extend_from_slice(&data);
                    chunk_count += 1;
                }
                other => panic!("expected file chunk frame, got {other:?}"),
            }
        }

        assert_eq!(assembled, expected);
        assert!(chunk_count >= 3);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn send_one_file_rejects_drift_from_advertised_snapshot() {
        let dir = test_dir("file-drift");
        fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("note.txt");
        fs::write(&file_path, b"before").unwrap();

        let outgoing = OutgoingSpec::RootContents {
            root: dir.clone(),
            max_folder_depth: None,
        };
        let advertised_snapshot = build_snapshot(&outgoing).unwrap();
        let advertised_entry = advertised_snapshot.entries.get("note.txt").unwrap().clone();

        fs::write(&file_path, b"after!").unwrap();

        let (tx, _rx) = mpsc::channel(16);
        let err = send_one_file(&tx, &outgoing, 7, "note.txt", &advertised_entry)
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("内容发生变化"));

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn handle_file_chunk_rejects_hash_mismatch_against_advertised_entry() {
        let root = test_dir("incoming-hash-mismatch");
        fs::create_dir_all(&root).unwrap();
        let expected_entry = file_entry_for_bytes(b"good", 1234);
        let wire_path = "demo.txt".to_string();
        let mut incoming_files = HashMap::new();
        let mut pending_revisions = BTreeMap::from([(
            1,
            super::PendingRevision {
                requested_files: 1,
                remaining_files: BTreeSet::from([wire_path.clone()]),
                failed_files: BTreeSet::new(),
                expected_files: BTreeMap::from([(wire_path.clone(), expected_entry.clone())]),
                delete_paths: Vec::new(),
                skipped_newer_count: 0,
                transfer_done: true,
            },
        )]);

        handle_file_chunk(
            &root,
            &mut incoming_files,
            &mut pending_revisions,
            FileChunkHeader {
                revision: 1,
                path: wire_path.clone(),
                offset: 0,
                total_size: expected_entry.size,
                modified_ms: expected_entry.modified_ms,
                executable: expected_entry.executable,
                final_chunk: true,
            },
            b"oops".to_vec(),
        )
        .await
        .unwrap();

        assert!(!root.join(&wire_path).exists());
        assert!(incoming_files.is_empty());
        assert!(pending_revisions.is_empty());

        let _ = fs::remove_dir_all(root);
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
            file_sync_mode: FileSyncMode::Auto,
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
            &sample_local_workspace_summary(),
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
            &sample_local_workspace_summary(),
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
        };

        run_with_session_notifications(&notifier, peer.clone(), async { Ok(()) })
            .await
            .unwrap();
        let error_result: anyhow::Result<()> = run_with_session_notifications(
            &notifier,
            peer,
            async { anyhow::bail!("session failed") },
        )
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
        assert!(bootstrap_device_name_matches(" demo-device ", "demo-device"));
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
                interval_secs: 3,
                sync_delete: false,
                notifications_enabled: true,
                input_backend_generation: 0,
                device_name: "test-device".to_string(),
                instance_name: None,
                discovery: DiscoveryConfig::default(),
                input: InputRuntimeOptions {
                    mode: InputMode::Off,
                    edge: ScreenEdge::Right,
                    hotkey: Hotkey::DEFAULT.parse().unwrap(),
                    reverse_mouse_wheel: false,
                    reverse_trackpad: false,
                    block_switch_on_press: false,
                    key_mapping: crate::input::KeyMappingConfig::default(),
                },
                clipboard: ClipboardRuntimeOptions {
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
                file_sync_mode: FileSyncMode::Off,
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
            file_sync_mode: FileSyncMode::Both,
            clipboard_mode: ClipboardMode::Off,
            audio_mode: AudioMode::Off,
            input_mode: crate::input::InputMode::Off,
            source: crate::discovery::DiscoverySource::Mdns,
            port: 8080,
            addresses: vec![Ipv4Addr::new(192, 168, 1, 20)],
        }
    }

    fn sample_local_workspace_summary() -> crate::sync::WorkspaceSummary {
        crate::sync::WorkspaceSummary {
            file_sync_mode: FileSyncMode::Both,
            send_description: None,
            send_layout: None,
            send_items: Vec::new(),
            receive_root: None,
            initial_sync: Some(InitialSyncMode::This),
            max_folder_depth: None,
            clipboard_mode: ClipboardMode::Off,
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
            runtime: crate::config::RuntimeConfig::default(),
            trusted_devices,
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

    fn test_dir(prefix: &str) -> PathBuf {
        env::temp_dir().join(format!("synly-app-{prefix}-{}", Uuid::new_v4()))
    }

    fn file_entry(hash: &str, modified_ms: u64) -> ManifestEntry {
        ManifestEntry {
            kind: EntryKind::File,
            size: 1,
            modified_ms,
            hash: Some(hash.to_string()),
            executable: false,
        }
    }

    fn file_entry_for_bytes(bytes: &[u8], modified_ms: u64) -> ManifestEntry {
        ManifestEntry {
            kind: EntryKind::File,
            size: bytes.len() as u64,
            modified_ms,
            hash: Some(format!("{:x}", Sha256::digest(bytes))),
            executable: false,
        }
    }

    fn dir_entry() -> ManifestEntry {
        ManifestEntry {
            kind: EntryKind::Dir,
            size: 0,
            modified_ms: 1,
            hash: None,
            executable: false,
        }
    }
}
