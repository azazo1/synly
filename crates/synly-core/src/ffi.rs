use crate::client;
use crate::device::{DeviceConfig, DiscoveryConfig, LndDiscoveryConfig, TrustedDeviceConfig};
use crate::protocol::{ClipboardImage, ClipboardPayload, DeviceIdentity, TransferLimits};
use crate::settings::ClipboardMode;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;
use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::prelude::*;
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
            self.fields.push((field.name().to_string(), value.to_string()));
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

/// 当前构建版本号, 由 build.rs 根据最近版本 tag 与工作区状态自动生成.
#[uniffi::export]
pub fn build_version() -> String {
    env!("SYNLY_BUILD_VERSION").to_string()
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
    pub instance_name: Option<String>,
    pub request_trust: bool,
    pub discovery: Option<FfiDiscoveryConfig>,
}

#[derive(uniffi::Record)]
pub struct FfiClientTarget {
    pub addresses: Vec<String>,
    pub port: u16,
    pub peer_device_id: Option<String>,
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
            addresses: peer.addresses.into_iter().map(|address| address.to_string()).collect(),
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
    PairingFailed {
        message: String,
    },
    Connected {
        remote: FfiDeviceIdentity,
        client_to_host: bool,
        host_to_client: bool,
        remote_workspace_summary: String,
    },
    ClipboardReceived {
        text: Option<String>,
        html: Option<String>,
        image_png: Option<Vec<u8>>,
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
            client::ClientEvent::PairingFailed { message } => Self::PairingFailed { message },
            client::ClientEvent::Connected {
                remote,
                agreement: _,
                clipboard_agreement,
                remote_workspace,
            } => Self::Connected {
                remote: remote.into(),
                client_to_host: clipboard_agreement.client_to_host,
                host_to_client: clipboard_agreement.host_to_client,
                remote_workspace_summary: remote_workspace.summary_lines().join(" | "),
            },
            client::ClientEvent::ClipboardReceived(payload) => Self::ClipboardReceived {
                text: payload.text,
                html: payload.html,
                image_png: payload.image.map(|image| image.png_bytes),
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
    Failed {
        message: String,
    },
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
    ) -> Result<(), FfiError> {
        let payload = ClipboardPayload {
            text,
            rich_text: None,
            html,
            image: image_png.map(|png_bytes| ClipboardImage { png_bytes }),
            files: Vec::new(),
        };
        self.inner.send_clipboard(payload).map_err(Into::into)
    }

    pub fn set_clipboard_mode(&self, mode: FfiClipboardMode) -> Result<(), FfiError> {
        self.inner.set_clipboard_mode(mode.into()).map_err(Into::into)
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
        runtime().block_on(self.inner.stop_and_wait()).map_err(Into::into)
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
            instance_name: config.instance_name,
            request_trust: config.request_trust,
            discovery,
        },
        client::ClientTarget {
            addresses,
            port: target.port,
            peer_device_id,
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
    let peers = runtime()
        .block_on(crate::discovery::browse(Duration::from_millis(timeout_ms), &discovery))?;
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
