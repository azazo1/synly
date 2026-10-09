pub mod bluetooth;

use crate::client;
use crate::device::{DeviceConfig, DiscoveryConfig, LndDiscoveryConfig, TrustedDeviceConfig};
use crate::protocol::{
    ClipboardFile, ClipboardImage, ClipboardPayload, DeviceIdentity, TransferLimits,
};
use crate::settings::ClipboardMode;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use uuid::Uuid;

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("synly-android")
            .build()
            .expect("failed to create synly android runtime")
    })
}

#[uniffi::export(callback_interface)]
pub trait FfiLogListener: Send + Sync {
    fn log(&self, level: String, target: String, message: String);
}

#[derive(Default)]
struct MessageVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.fields
                .push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = Some(format!("{value:?}"));
        } else {
            self.fields
                .push((field.name().to_string(), format!("{value:?}")));
        }
    }
}

struct AndroidLogLayer {
    bridge: Box<dyn FfiLogListener>,
}

impl<S> Layer<S> for AndroidLogLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let message = visitor.message.unwrap_or_default();
        let fields = visitor
            .fields
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        let line = if fields.is_empty() {
            message
        } else {
            format!("{message} {fields}")
        };
        self.bridge.log(
            event.metadata().level().as_str().to_string(),
            event.metadata().target().to_string(),
            line,
        );
    }
}

#[uniffi::export]
pub fn init_tracing(listener: Box<dyn FfiLogListener>) -> Result<(), FfiError> {
    let layer = AndroidLogLayer { bridge: listener };
    tracing_subscriber::registry()
        .with(layer)
        .with(tracing_subscriber::EnvFilter::new("info"))
        .try_init()
        .map_err(|err| FfiError::Failed {
            message: err.to_string(),
        })
}

/// 当前构建版本号. 日常为 `dev-build`, 发布构建由 `scripts/build-version.sh` 或 CI 注入.
#[uniffi::export]
pub fn build_version() -> String {
    env!("SYNLY_BUILD_VERSION").to_string()
}

#[derive(uniffi::Enum)]
pub enum FfiPathPolicy { Auto, PreferBluetooth, LanOnly, BluetoothOnly }
impl From<FfiPathPolicy> for crate::transport::routing::PathPolicy {
    fn from(policy: FfiPathPolicy) -> Self { match policy { FfiPathPolicy::Auto => Self::Auto, FfiPathPolicy::PreferBluetooth => Self::PreferBluetooth, FfiPathPolicy::LanOnly => Self::LanOnly, FfiPathPolicy::BluetoothOnly => Self::BluetoothOnly } }
}

#[derive(uniffi::Enum)]
pub enum FfiClipboardMode {
    Off,
    Send,
    Receive,
    Both,
}

impl From<ClipboardMode> for FfiClipboardMode {
    fn from(mode: ClipboardMode) -> Self {
        match mode {
            ClipboardMode::Off => Self::Off,
            ClipboardMode::Send => Self::Send,
            ClipboardMode::Receive => Self::Receive,
            ClipboardMode::Both => Self::Both,
        }
    }
}

impl From<FfiClipboardMode> for ClipboardMode {
    fn from(mode: FfiClipboardMode) -> Self {
        match mode {
            FfiClipboardMode::Off => Self::Off,
            FfiClipboardMode::Send => Self::Send,
            FfiClipboardMode::Receive => Self::Receive,
            FfiClipboardMode::Both => Self::Both,
        }
    }
}

#[derive(uniffi::Enum)]
pub enum FfiClientState {
    Connecting,
    Pairing,
    Connected,
    Reconnecting,
}

impl From<client::ClientState> for FfiClientState {
    fn from(state: client::ClientState) -> Self {
        match state {
            client::ClientState::Connecting => Self::Connecting,
            client::ClientState::Pairing => Self::Pairing,
            client::ClientState::Connected => Self::Connected,
            client::ClientState::Reconnecting => Self::Reconnecting,
        }
    }
}

#[derive(uniffi::Record)]
pub struct FfiDeviceIdentity {
    pub device_id: String,
    pub device_name: String,
    pub instance_name: Option<String>,
    pub identity_public_key: String,
    pub tls_root_certificate: String,
}

#[derive(uniffi::Record)]
pub struct FfiClipboardFile {
    pub name: String,
    pub bytes: Vec<u8>,
}

impl From<DeviceIdentity> for FfiDeviceIdentity {
    fn from(identity: DeviceIdentity) -> Self {
        Self {
            device_id: identity.device_id.to_string(),
            device_name: identity.device_name,
            instance_name: identity.instance_name,
            identity_public_key: identity.identity_public_key,
            tls_root_certificate: identity.tls_root_certificate,
        }
    }
}

impl TryFrom<FfiDeviceIdentity> for DeviceIdentity {
    type Error = FfiError;

    fn try_from(identity: FfiDeviceIdentity) -> Result<Self, Self::Error> {
        Ok(Self {
            device_id: Uuid::parse_str(&identity.device_id)?,
            device_name: identity.device_name,
            instance_name: identity.instance_name,
            identity_public_key: identity.identity_public_key,
            tls_root_certificate: identity.tls_root_certificate,
        })
    }
}

#[derive(uniffi::Record)]
pub struct FfiDeviceConfig {
    pub device_id: String,
    pub device_name: String,
    pub identity_private_key: String,
    pub identity_public_key: String,
}

impl TryFrom<FfiDeviceConfig> for DeviceConfig {
    type Error = FfiError;

    fn try_from(config: FfiDeviceConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            device_id: Uuid::parse_str(&config.device_id)?,
            device_name: config.device_name,
            identity_private_key: config.identity_private_key,
            identity_public_key: config.identity_public_key,
        })
    }
}

#[derive(uniffi::Record)]
pub struct FfiTrustedDeviceConfig {
    pub device_id: String,
    pub device_name: String,
    pub public_key: String,
    pub tls_root_certificate: String,
    pub trusted_at_ms: u64,
    pub last_seen_ms: u64,
    pub successful_sessions: u64,
}

impl From<TrustedDeviceConfig> for FfiTrustedDeviceConfig {
    fn from(device: TrustedDeviceConfig) -> Self {
        Self {
            device_id: device.device_id.to_string(),
            device_name: device.device_name,
            public_key: device.public_key,
            tls_root_certificate: device.tls_root_certificate,
            trusted_at_ms: device.trusted_at_ms,
            last_seen_ms: device.last_seen_ms,
            successful_sessions: device.successful_sessions,
        }
    }
}

impl TryFrom<FfiTrustedDeviceConfig> for TrustedDeviceConfig {
    type Error = FfiError;

    fn try_from(device: FfiTrustedDeviceConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            device_id: Uuid::parse_str(&device.device_id)?,
            device_name: device.device_name,
            public_key: device.public_key,
            tls_root_certificate: device.tls_root_certificate,
            trusted_at_ms: device.trusted_at_ms,
            last_seen_ms: device.last_seen_ms,
            successful_sessions: device.successful_sessions,
        })
    }
}

#[derive(uniffi::Record)]
pub struct FfiClientConfig {
    pub device: FfiDeviceConfig,
    pub trusted_devices: Vec<FfiTrustedDeviceConfig>,
    pub max_meta_len: u32,
    pub max_frame_data_len: u32,
    pub max_clipboard_binary_len: u32,
    pub clipboard_mode: FfiClipboardMode,
    pub clipboard_path: FfiPathPolicy,
    pub instance_name: Option<String>,
    pub request_trust: bool,
    pub bluetooth_enabled: bool,
    pub discovery: Option<FfiDiscoveryConfig>,
}

#[derive(uniffi::Record)]
pub struct FfiClientTarget {
    pub addresses: Vec<String>,
    pub port: u16,
    pub peer_device_id: Option<String>,
    pub bluetooth_address: Option<String>,
}

#[derive(uniffi::Record)]
pub struct FfiDiscoveryConfig {
    pub mdns_enabled: bool,
    pub lnd_server_url: Option<String>,
    pub lnd_bearer_token: Option<String>,
    pub lnd_discovery_domain: Option<String>,
}

#[derive(uniffi::Record)]
pub struct FfiDiscoveredPeer {
    pub device_name: String,
    pub instance_name: Option<String>,
    pub device_id: String,
    pub protocol_version: u16,
    pub clipboard_mode: FfiClipboardMode,
    pub port: u16,
    pub addresses: Vec<String>,
    pub source: FfiDiscoverySource,
}

#[derive(uniffi::Enum)]
pub enum FfiDiscoverySource {
    Mdns,
    Lnd,
    MdnsAndLnd,
}

impl From<crate::discovery::DiscoverySource> for FfiDiscoverySource {
    fn from(source: crate::discovery::DiscoverySource) -> Self {
        match source {
            crate::discovery::DiscoverySource::Mdns => Self::Mdns,
            crate::discovery::DiscoverySource::Lnd => Self::Lnd,
            crate::discovery::DiscoverySource::MdnsAndLnd => Self::MdnsAndLnd,
        }
    }
}

impl From<crate::discovery::DiscoveredPeer> for FfiDiscoveredPeer {
    fn from(peer: crate::discovery::DiscoveredPeer) -> Self {
        Self {
            device_name: peer.device_name,
            instance_name: peer.instance_name,
            device_id: peer.device_id,
            protocol_version: peer.protocol_version,
            clipboard_mode: peer.clipboard_mode.into(),
            port: peer.port,
            addresses: peer
                .addresses
                .into_iter()
                .map(|address| address.to_string())
                .collect(),
            source: peer.source.into(),
        }
    }
}

#[derive(uniffi::Enum)]
pub enum FfiClientEvent {
    StateChanged {
        state: FfiClientState,
    },
    PinRequired {
        request_id: String,
        bootstrap_short: String,
        bootstrap_randomart: String,
        session_short: String,
        session_randomart: String,
    },
    BluetoothAuthorizationRequired {
        request_id: String,
        remote: FfiDeviceIdentity,
        system_address: String,
        system_name: String,
        fingerprint: String,
        changed_identity: bool,
        request_trust: bool,
        capabilities_summary: String,
    },
    PairingFailed {
        message: String,
    },
    Connected {
        remote: FfiDeviceIdentity,
        client_to_host: bool,
        host_to_client: bool,
        remote_capabilities_summary: String,
        remote_address: Option<String>,
        remote_port: Option<u16>,
    },
    TransportChanged { primary: String, lan_available: bool, bluetooth_available: bool, clipboard_status: String },
    ClipboardReceived {
        delivery_id: Option<String>,
        text: Option<String>,
        html: Option<String>,
        image_png: Option<Vec<u8>>,
        files: Vec<FfiClipboardFile>,
    },
    Disconnected {
        message: String,
    },
    TrustEstablished {
        device: FfiDeviceIdentity,
    },
}

impl From<client::ClientEvent> for FfiClientEvent {
    fn from(event: client::ClientEvent) -> Self {
        match event {
            client::ClientEvent::StateChanged(state) => Self::StateChanged {
                state: state.into(),
            },
            client::ClientEvent::PinRequired {
                request_id,
                bootstrap_short,
                bootstrap_randomart,
                session_short,
                session_randomart,
            } => Self::PinRequired {
                request_id,
                bootstrap_short,
                bootstrap_randomart,
                session_short,
                session_randomart,
            },
            client::ClientEvent::BluetoothAuthorizationRequired { request_id, request } => Self::BluetoothAuthorizationRequired {
                request_id, remote: request.peer.into(), system_address: request.system_peer.address, system_name: request.system_peer.name,
                fingerprint: request.fingerprint, changed_identity: request.changed_identity, request_trust: request.request_trust,
                capabilities_summary: request.capabilities.summary_lines().join(" | "),
            },
            client::ClientEvent::PairingFailed { message } => Self::PairingFailed { message },
            client::ClientEvent::Connected {
                remote,
                clipboard_agreement,
                remote_capabilities,
                remote_address,
                remote_port,
            } => Self::Connected {
                remote: remote.into(),
                client_to_host: clipboard_agreement.client_to_host,
                host_to_client: clipboard_agreement.host_to_client,
                remote_capabilities_summary: remote_capabilities.summary_lines().join(" | "),
                remote_address: remote_address.map(|address| address.to_string()),
                remote_port,
            },
            client::ClientEvent::TransportChanged(status) => {
                use crate::transport::routing::{TransportKind, RouteChoice, PauseReason};
                let label = |kind| match kind { TransportKind::Lan => "局域网", TransportKind::Bluetooth => "蓝牙" };
                let clipboard_status = match status.clipboard_choice {
                    RouteChoice::Paused(PauseReason::PolicyConflict) => "双方策略冲突",
                    RouteChoice::Paused(PauseReason::TransportUnavailable) => "所需路径不可用",
                    RouteChoice::Paused(PauseReason::UnsupportedChannel) => "平台不支持",
                    RouteChoice::Selected(_) => if status.failed { "失败暂停" } else if status.switching { "切换确认中" } else { status.clipboard.map(label).unwrap_or("关闭或等待路径") },
                };
                Self::TransportChanged { primary: label(status.primary).to_owned(), lan_available: status.available.lan, bluetooth_available: status.available.bluetooth, clipboard_status: clipboard_status.to_owned() }
            },
            client::ClientEvent::ClipboardDelivery { delivery_id, payload } => Self::ClipboardReceived {
                delivery_id: Some(delivery_id.to_string()), text: payload.text, html: payload.html,
                image_png: payload.image.map(|image| image.png_bytes),
                files: payload.files.into_iter().map(|file| FfiClipboardFile { name: file.name, bytes: file.bytes }).collect(),
            },
            client::ClientEvent::ClipboardReceived(payload) => Self::ClipboardReceived {
                delivery_id: None,
                text: payload.text,
                html: payload.html,
                image_png: payload.image.map(|image| image.png_bytes),
                files: payload
                    .files
                    .into_iter()
                    .map(|file| FfiClipboardFile {
                        name: file.name,
                        bytes: file.bytes,
                    })
                    .collect(),
            },
            client::ClientEvent::Disconnected { message } => Self::Disconnected { message },
            client::ClientEvent::TrustEstablished(device) => Self::TrustEstablished {
                device: device.into(),
            },
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait FfiClientListener: Send + Sync {
    fn on_event(&self, event: FfiClientEvent);
}

struct ListenerBridge {
    inner: Box<dyn FfiClientListener>,
}

impl client::ClientListener for ListenerBridge {
    fn on_event(&self, event: client::ClientEvent) {
        self.inner.on_event(event.into());
    }
}

#[derive(Debug, uniffi::Error)]
#[uniffi(flat_error)]
pub enum FfiError {
    Failed { message: String },
}

impl std::fmt::Display for FfiError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed { message } => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for FfiError {}

impl From<anyhow::Error> for FfiError {
    fn from(err: anyhow::Error) -> Self {
        Self::Failed {
            message: format!("{err:#}"),
        }
    }
}

impl From<uuid::Error> for FfiError {
    fn from(err: uuid::Error) -> Self {
        Self::Failed {
            message: format!("{err}"),
        }
    }
}

impl From<std::net::AddrParseError> for FfiError {
    fn from(err: std::net::AddrParseError) -> Self {
        Self::Failed {
            message: format!("{err}"),
        }
    }
}

#[derive(uniffi::Object)]
pub struct FfiClientHandle {
    inner: client::ClientHandle,
}

#[uniffi::export]
impl FfiClientHandle {
    pub fn authorize_bluetooth(&self, request_id: String, accepted: bool, remember: bool) -> Result<(), FfiError> {
        self.inner.authorize_bluetooth(request_id, accepted, remember).map_err(Into::into)
    }

    pub fn submit_pin(&self, pin: String) -> Result<(), FfiError> {
        self.inner.submit_pin(&pin).map_err(Into::into)
    }

    pub fn cancel_pin(&self) -> Result<(), FfiError> {
        self.inner.cancel_pin().map_err(Into::into)
    }

    pub fn send_clipboard(
        &self,
        text: Option<String>,
        html: Option<String>,
        image_png: Option<Vec<u8>>,
        files: Vec<FfiClipboardFile>,
    ) -> Result<(), FfiError> {
        let payload = ClipboardPayload {
            text,
            rich_text: None,
            html,
            image: image_png.map(|png_bytes| ClipboardImage { png_bytes }),
            files: files
                .into_iter()
                .map(|file| ClipboardFile {
                    name: file.name,
                    bytes: file.bytes,
                })
                .collect(),
        };
        self.inner.send_clipboard(payload).map_err(Into::into)
    }

    pub fn confirm_clipboard(&self, delivery_id: String, success: bool) -> Result<(), FfiError> {
        let id = uuid::Uuid::parse_str(&delivery_id).map_err(|error| FfiError::from(anyhow::anyhow!("剪贴板交付 ID 无效: {error}")))?;
        self.inner.confirm_clipboard(id, success).map_err(Into::into)
    }

    pub fn set_clipboard_path(&self, policy: FfiPathPolicy) -> Result<(), FfiError> { self.inner.set_clipboard_path(policy.into()).map_err(Into::into) }

    pub fn set_clipboard_mode(&self, mode: FfiClipboardMode) -> Result<(), FfiError> {
        self.inner
            .set_clipboard_mode(mode.into())
            .map_err(Into::into)
    }

    pub fn update_trusted_devices(
        &self,
        devices: Vec<FfiTrustedDeviceConfig>,
    ) -> Result<(), FfiError> {
        let devices = devices
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>, _>>()?;
        self.inner
            .update_trusted_devices(devices)
            .map_err(Into::into)
    }

    pub fn state(&self) -> FfiClientState {
        self.inner.state().into()
    }

    pub fn stop(&self) -> Result<(), FfiError> {
        runtime()
            .block_on(self.inner.stop_and_wait())
            .map_err(Into::into)
    }
}

#[uniffi::export]
pub fn start_client(
    config: FfiClientConfig,
    target: FfiClientTarget,
    listener: Box<dyn FfiClientListener>,
) -> Result<Arc<FfiClientHandle>, FfiError> {
    let device = config.device.try_into()?;
    let trusted_devices = config
        .trusted_devices
        .into_iter()
        .map(TryInto::try_into)
        .collect::<Result<Vec<_>, _>>()?;
    let transfer_limits = TransferLimits {
        max_meta_len: config.max_meta_len as usize,
        max_frame_data_len: config.max_frame_data_len as usize,
        max_clipboard_binary_len: config.max_clipboard_binary_len as usize,
    };
    let clipboard_mode = config.clipboard_mode.into();
    let addresses = target
        .addresses
        .into_iter()
        .map(|address| address.parse::<Ipv4Addr>())
        .collect::<Result<Vec<_>, _>>()?;
    let peer_device_id = target
        .peer_device_id
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()?;
    let discovery = config.discovery.map(into_discovery_config);
    let bridge = ListenerBridge { inner: listener };
    let _guard = runtime().enter();
    let handle = client::start_client(
        client::ClientConfig {
            device,
            trusted_devices,
            transfer_limits,
            clipboard_mode,
            clipboard_path: config.clipboard_path.into(),
            instance_name: config.instance_name,
            request_trust: config.request_trust,
            bluetooth_enabled: config.bluetooth_enabled,
            discovery,
        },
        client::ClientTarget {
            addresses,
            port: target.port,
            peer_device_id,
            bluetooth_address: target.bluetooth_address,
        },
        Arc::new(bridge),
    )?;
    Ok(Arc::new(FfiClientHandle { inner: handle }))
}

#[uniffi::export]
pub fn normalize_pin(pin: String) -> Result<String, FfiError> {
    client::normalize_pin(&pin).map_err(Into::into)
}

#[uniffi::export]
pub fn parse_human_bytes(input: String) -> Result<u64, FfiError> {
    crate::size::parse_human_bytes(&input).map_err(Into::into)
}

#[uniffi::export]
pub fn format_human_bytes(bytes: u64) -> String {
    crate::size::format_human_bytes(bytes)
}

#[uniffi::export]
pub fn generate_device_config(device_name: String) -> Result<FfiDeviceConfig, FfiError> {
    let device = crate::identity::generate_device_config(device_name)?;
    Ok(FfiDeviceConfig {
        device_id: device.device_id.to_string(),
        device_name: device.device_name,
        identity_private_key: device.identity_private_key,
        identity_public_key: device.identity_public_key,
    })
}

#[uniffi::export]
pub fn browse_devices(
    config: FfiDiscoveryConfig,
    timeout_ms: u64,
) -> Result<Vec<FfiDiscoveredPeer>, FfiError> {
    let discovery = into_discovery_config(config);
    let peers = runtime().block_on(crate::discovery::browse(
        Duration::from_millis(timeout_ms),
        &discovery,
    ))?;
    Ok(peers.into_iter().map(Into::into).collect())
}

fn into_discovery_config(config: FfiDiscoveryConfig) -> DiscoveryConfig {
    DiscoveryConfig {
        mdns_enabled: config.mdns_enabled,
        lnd: match (config.lnd_server_url, config.lnd_bearer_token) {
            (Some(server_url), Some(bearer_token)) => Some(LndDiscoveryConfig {
                server_url,
                bearer_token,
                discovery_domain: config.lnd_discovery_domain,
            }),
            _ => None,
        },
    }
}
