use crate::input::{CursorMode, InputMode, KeyMappingConfig, ScreenEdge};
use crate::path_expand::expand_config_path_string;
use crate::protocol::TransferLimits;
use crate::settings::{
    AudioMode, ClipboardMode, ConnectionPreference, FileSyncMode, InitialSyncMode, LogLevel,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub use synly_core::device::{
    DeviceConfig, DiscoveryConfig, LndDiscoveryConfig, TrustedDeviceConfig,
};

const CLIPBOARD_CACHE_DIR_NAME: &str = "clipboard-cache";
const DEFAULT_CLIPBOARD_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct SynlyConfig {
    pub device: DeviceConfig,
    pub clipboard: ClipboardConfig,
    pub transfer: TransferConfig,
    pub notifications: NotificationConfig,
    pub discovery: DiscoveryConfig,
    pub ui: UiConfig,
    pub gui_state: GuiState,
    pub runtime: RuntimeConfig,
    pub trusted_devices: Vec<TrustedDeviceConfig>,
    /// 首选活跃设备, 跨 host 重启保留.
    pub preferred_active: Option<Uuid>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub connection: Option<ConnectionPreference>,
    pub instance_name: String,
    pub peer_query: String,
    pub port: Option<u16>,
    pub file_sync_mode: FileSyncMode,
    pub paths: Vec<PathBuf>,
    pub initial: Option<InitialSyncMode>,
    pub sync_delete: bool,
    pub clipboard_mode: ClipboardMode,
    pub audio_mode: AudioMode,
    pub input: InputConfig,
    pub interval_secs: u64,
    pub max_folder_depth: Option<usize>,
    pub accept: bool,
    pub trust_device: bool,
    pub trusted_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputConfig {
    pub mode: InputMode,
    pub edge: ScreenEdge,
    pub hotkey: String,
    pub elevate_on_start: bool,
    pub reverse_mouse_wheel: bool,
    pub reverse_trackpad: bool,
    pub native_scroll_macos_to_windows: bool,
    pub native_scroll_windows_to_macos: bool,
    #[serde(default)]
    pub block_switch_on_press: bool,
    pub key_mapping: KeyMappingConfig,
    pub cursor_mode: CursorMode,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    pub start_hidden: bool,
    pub close_to_tray: bool,
    pub launch_at_login: bool,
    pub resume_last_session: bool,
    pub log_level: LogLevel,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GuiState {
    pub first_run_completed: bool,
    pub window_width: u32,
    pub window_height: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClipboardConfig {
    pub max_file_bytes: u64,
    pub max_cache_bytes: Option<u64>,
    pub cache_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TransferConfig {
    pub max_meta_bytes: u64,
    pub max_frame_data_bytes: u64,
    pub max_clipboard_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NotificationConfig {
    pub enabled: bool,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            mode: InputMode::Off,
            edge: ScreenEdge::Right,
            hotkey: crate::input::Hotkey::DEFAULT.to_string(),
            elevate_on_start: false,
            reverse_mouse_wheel: false,
            reverse_trackpad: false,
            native_scroll_macos_to_windows: false,
            native_scroll_windows_to_macos: false,
            block_switch_on_press: false,
            key_mapping: KeyMappingConfig::default(),
            cursor_mode: CursorMode::default(),
        }
    }
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: DEFAULT_CLIPBOARD_MAX_FILE_BYTES,
            max_cache_bytes: None,
            cache_dir: None,
        }
    }
}

impl Default for TransferConfig {
    fn default() -> Self {
        let limits = TransferLimits::default();
        Self {
            max_meta_bytes: limits.max_meta_len as u64,
            max_frame_data_bytes: limits.max_frame_data_len as u64,
            max_clipboard_bytes: limits.max_clipboard_binary_len as u64,
        }
    }
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            start_hidden: false,
            close_to_tray: true,
            launch_at_login: false,
            resume_last_session: false,
            log_level: LogLevel::Info,
        }
    }
}

impl Default for GuiState {
    fn default() -> Self {
        Self {
            first_run_completed: false,
            window_width: 1080,
            window_height: 720,
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            connection: None,
            instance_name: String::new(),
            peer_query: String::new(),
            port: None,
            file_sync_mode: FileSyncMode::Off,
            paths: Vec::new(),
            initial: None,
            sync_delete: false,
            clipboard_mode: ClipboardMode::Off,
            audio_mode: AudioMode::Off,
            input: InputConfig::default(),
            interval_secs: 3,
            max_folder_depth: None,
            accept: false,
            trust_device: false,
            trusted_only: false,
        }
    }
}

impl RuntimeConfig {
    pub fn normalize_file_sync_options(&mut self) {
        if !matches!(self.file_sync_mode, FileSyncMode::Both | FileSyncMode::Auto) {
            self.initial = None;
        }
    }
}

impl SynlyConfig {
    pub fn load_or_create() -> Result<Self> {
        super::store::load_or_create()
    }

    pub fn clipboard_cache_dir(&self) -> Result<PathBuf> {
        let base_dir = clipboard_cache_base_dir()?;
        match &self.clipboard.cache_dir {
            Some(path) => resolve_configured_path(path, &base_dir),
            None => Ok(base_dir.join(CLIPBOARD_CACHE_DIR_NAME)),
        }
    }

    pub fn save_settings(&self) -> Result<()> {
        super::store::save_settings(self)
    }

    pub fn save_trusted_devices(&self) -> Result<()> {
        super::store::save_trusted_devices(self)
    }

    pub fn save_gui_state(&self) -> Result<()> {
        super::store::save_gui_state(self)
    }

    pub fn trusted_device(&self, device_id: &Uuid) -> Option<&TrustedDeviceConfig> {
        self.trusted_devices
            .iter()
            .find(|device| device.device_id == *device_id && !device.public_key.trim().is_empty())
    }

    pub fn remember_trusted_device(
        &mut self,
        device_id: Uuid,
        device_name: String,
        public_key: String,
        tls_root_certificate: String,
    ) {
        let now = unix_time_ms();
        if let Some(device) = self
            .trusted_devices
            .iter_mut()
            .find(|device| device.device_id == device_id)
        {
            device.device_name = device_name;
            device.public_key = public_key;
            device.tls_root_certificate = tls_root_certificate;
            if device.trusted_at_ms == 0 {
                device.trusted_at_ms = now;
            }
            device.last_seen_ms = now;
            device.successful_sessions = device.successful_sessions.saturating_add(1);
        } else {
            self.trusted_devices.push(TrustedDeviceConfig {
                device_id,
                device_name,
                public_key,
                tls_root_certificate,
                trusted_at_ms: now,
                last_seen_ms: now,
                successful_sessions: 1,
            });
            self.trusted_devices.sort_by_key(|device| device.device_id);
        }
    }

    pub fn note_trusted_device_session(&mut self, device_id: Uuid, device_name: &str) {
        let now = unix_time_ms();
        if let Some(device) = self
            .trusted_devices
            .iter_mut()
            .find(|device| device.device_id == device_id)
        {
            device.device_name = device_name.to_string();
            device.last_seen_ms = now;
            device.successful_sessions = device.successful_sessions.saturating_add(1);
        }
    }

    pub fn revoke_trusted_device(&mut self, device_id: Uuid) -> bool {
        let previous_len = self.trusted_devices.len();
        self.trusted_devices
            .retain(|device| device.device_id != device_id);
        self.trusted_devices.len() != previous_len
    }
}

impl TransferConfig {
    pub fn to_limits(&self) -> Result<TransferLimits> {
        let max_meta_len = usize::try_from(self.max_meta_bytes)
            .context("transfer.max_meta_bytes exceeds this platform's supported size")?;
        let max_frame_data_len = usize::try_from(self.max_frame_data_bytes)
            .context("transfer.max_frame_data_bytes exceeds this platform's supported size")?;
        let max_clipboard_binary_len = usize::try_from(self.max_clipboard_bytes)
            .context("transfer.max_clipboard_bytes exceeds this platform's supported size")?;
        if max_meta_len == 0 {
            bail!("transfer.max_meta_bytes must be greater than 0");
        }
        if max_frame_data_len == 0 {
            bail!("transfer.max_frame_data_bytes must be greater than 0");
        }
        if max_clipboard_binary_len == 0 {
            bail!("transfer.max_clipboard_bytes must be greater than 0");
        }
        Ok(TransferLimits {
            max_meta_len,
            max_frame_data_len,
            max_clipboard_binary_len,
        })
    }
}

pub(super) fn resolve_configured_path(path: &Path, base_dir: &Path) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    let path = expand_config_path_string(&raw).context("configured path cannot be empty")?;
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(base_dir.join(path))
    }
}

fn clipboard_cache_base_dir() -> Result<PathBuf> {
    dirs::cache_dir()
        .map(|dir| dir.join("synly"))
        .context("unable to determine platform cache directory")
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}
