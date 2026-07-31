use crate::config::{
    ClipboardConfig, DiscoveryConfig, RuntimeConfig, SynlyConfig, TransferConfig,
    TrustedDeviceConfig, UiConfig,
};
use crate::runtime_control::{InteractionRequest, RuntimePeerSummary};
use crate::protocol::{CapabilityEpoch, RuntimeCapabilities};
use crate::settings::{AudioMode, ClipboardMode};
use crate::input::InputMode;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AppLifecycle {
    #[default]
    Idle,
    Hosting,
    Discovering,
    Connecting,
    Pairing,
    Connected,
    Reconfiguring,
    Error,
    Stopping,
}

impl AppLifecycle {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "空闲",
            Self::Hosting => "等待连接",
            Self::Discovering => "发现设备",
            Self::Connecting => "正在连接",
            Self::Pairing => "等待配对",
            Self::Connected => "已连接",
            Self::Reconfiguring => "正在应用设置",
            Self::Error => "发生错误",
            Self::Stopping => "正在停止",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredPeerView {
    pub device_id: String,
    pub display_name: String,
    pub addresses: Vec<String>,
    pub source: String,
    pub protocol_version: u16,
    pub compatible: bool,
    pub trusted: bool,
    pub file_mode: String,
    pub clipboard_mode: String,
    pub audio_mode: String,
    pub input_mode: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingInteraction {
    pub request: InteractionRequest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppSettings {
    pub device_name: String,
    pub clipboard: ClipboardConfig,
    pub transfer: TransferConfig,
    pub discovery: DiscoveryConfig,
    pub ui: UiConfig,
    pub notifications_enabled: bool,
}

impl AppSettings {
    pub fn from_config(config: &SynlyConfig) -> Self {
        Self {
            device_name: config.device.device_name.clone(),
            clipboard: config.clipboard.clone(),
            transfer: config.transfer.clone(),
            discovery: config.discovery.clone(),
            ui: config.ui.clone(),
            notifications_enabled: config.notifications.enabled,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AppSnapshot {
    pub lifecycle: AppLifecycle,
    pub desired: RuntimeConfig,
    pub pending: Option<RuntimeConfig>,
    pub applied: Option<RuntimeConfig>,
    pub settings: AppSettings,
    pub current_peer: Option<RuntimePeerSummary>,
    pub discovered_peers: Vec<DiscoveredPeerView>,
    pub trusted_devices: Vec<TrustedDeviceConfig>,
    pub interaction: Option<PendingInteraction>,
    pub last_error: Option<String>,
    pub input_elevation_ready: bool,
    pub actual_capabilities: Option<RuntimeCapabilities>,
    pub remote_capabilities: Option<RuntimeCapabilities>,
    pub capability_epoch: Option<CapabilityEpoch>,
    pub capabilities_acknowledged: bool,
}

impl AppSnapshot {
    pub fn idle(runtime: RuntimeConfig, settings: AppSettings) -> Self {
        Self {
            lifecycle: AppLifecycle::Idle,
            desired: runtime,
            pending: None,
            applied: None,
            settings,
            current_peer: None,
            discovered_peers: Vec::new(),
            trusted_devices: Vec::new(),
            interaction: None,
            last_error: None,
            input_elevation_ready: initial_input_elevation_ready(),
            actual_capabilities: None,
            remote_capabilities: None,
            capability_epoch: None,
            capabilities_acknowledged: true,
        }
    }
}

fn initial_input_elevation_ready() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::input::is_accessibility_trusted()
    }
    #[cfg(windows)]
    {
        false
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        true
    }
}

#[allow(dead_code)]
pub enum AppCommand {
    ApplySettings {
        runtime: RuntimeConfig,
        settings: Box<AppSettings>,
        session_pin: Option<String>,
    },
    Start,
    StartHosting,
    RefreshDiscovery,
    ConnectPeer(String),
    SetClipboardMode(ClipboardMode),
    SetAudioMode(AudioMode),
    SetInputMode(InputMode),
    SelectPaths(Vec<PathBuf>),
    Disconnect,
    RespondInteraction {
        request_id: Uuid,
        response: crate::runtime_control::InteractionResponse,
    },
    RevokeTrust(Uuid),
    RequestInputElevation,
    SaveWindowState {
        width: u32,
        height: u32,
    },
    RefreshInputPermission,
    Shutdown,
}
