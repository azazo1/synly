use crate::path_expand::{expand_config_path_string, home_dir};
use crate::protocol::TransferLimits;
use crate::input::{InputMode, ScreenEdge};
use crate::settings::{
    AudioMode, ClipboardMode, ConnectionPreference, FileSyncMode, InitialSyncMode, LogLevel,
};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const CONFIG_FILE_NAME: &str = "config.toml";
const LEGACY_DEVICE_CONFIG_FILE_NAME: &str = "device.json";
const CLIPBOARD_CACHE_DIR_NAME: &str = "clipboard-cache";
const DEFAULT_CLIPBOARD_MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SynlyConfig {
    pub device: DeviceConfig,
    #[serde(default)]
    pub clipboard: ClipboardConfig,
    #[serde(default)]
    pub transfer: TransferConfig,
    #[serde(default)]
    pub notifications: NotificationConfig,
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub trusted_devices: Vec<TrustedDeviceConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiConfig {
    #[serde(default)]
    pub first_run_completed: bool,
    #[serde(default)]
    pub start_hidden: bool,
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    #[serde(default)]
    pub launch_at_login: bool,
    #[serde(default)]
    pub resume_last_session: bool,
    #[serde(default)]
    pub log_level: LogLevel,
    #[serde(default = "default_window_width")]
    pub window_width: u32,
    #[serde(default = "default_window_height")]
    pub window_height: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<ConnectionPreference>,
    #[serde(default)]
    pub instance_name: String,
    #[serde(default)]
    pub peer_query: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default = "default_file_sync_mode")]
    pub file_sync_mode: FileSyncMode,
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<InitialSyncMode>,
    #[serde(default)]
    pub sync_delete: bool,
    #[serde(default)]
    pub clipboard_mode: ClipboardMode,
    #[serde(default)]
    pub audio_mode: AudioMode,
    #[serde(default)]
    pub input_mode: InputMode,
    #[serde(default = "default_input_edge")]
    pub input_edge: ScreenEdge,
    #[serde(default = "default_input_hotkey")]
    pub input_hotkey: String,
    #[serde(default = "default_interval_secs")]
    pub interval_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_folder_depth: Option<usize>,
    #[serde(default)]
    pub accept: bool,
    #[serde(default)]
    pub trust_device: bool,
    #[serde(default)]
    pub trusted_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub device_id: Uuid,
    pub device_name: String,
    #[serde(default)]
    pub identity_private_key: Option<String>,
    #[serde(default)]
    pub identity_public_key: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardConfig {
    #[serde(default = "default_clipboard_max_file_bytes")]
    pub max_file_bytes: u64,
    #[serde(default)]
    pub max_cache_bytes: Option<u64>,
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransferConfig {
    #[serde(default = "default_transfer_max_meta_bytes")]
    pub max_meta_bytes: u64,
    #[serde(default = "default_transfer_max_frame_data_bytes")]
    pub max_frame_data_bytes: u64,
    #[serde(default = "default_transfer_max_clipboard_bytes")]
    pub max_clipboard_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct NotificationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveryConfig {
    #[serde(default = "default_true")]
    pub mdns_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lnd: Option<LndDiscoveryConfig>,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LndDiscoveryConfig {
    pub server_url: String,
    #[serde(default)]
    pub bearer_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_domain: Option<String>,
}

impl std::fmt::Debug for LndDiscoveryConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LndDiscoveryConfig")
            .field("server_url", &self.server_url)
            .field("bearer_token", &"<redacted>")
            .field("discovery_domain", &self.discovery_domain)
            .finish()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustedDeviceConfig {
    pub device_id: Uuid,
    pub device_name: String,
    #[serde(default)]
    pub public_key: String,
    #[serde(default)]
    pub tls_root_certificate: String,
    #[serde(default)]
    pub trusted_at_ms: u64,
    #[serde(default)]
    pub last_seen_ms: u64,
    #[serde(default)]
    pub successful_sessions: u64,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            max_file_bytes: default_clipboard_max_file_bytes(),
            max_cache_bytes: None,
            cache_dir: None,
        }
    }
}

impl Default for TransferConfig {
    fn default() -> Self {
        Self {
            max_meta_bytes: default_transfer_max_meta_bytes(),
            max_frame_data_bytes: default_transfer_max_frame_data_bytes(),
            max_clipboard_bytes: default_transfer_max_clipboard_bytes(),
        }
    }
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for DiscoveryConfig {
    fn default() -> Self {
        Self {
            mdns_enabled: true,
            lnd: None,
        }
    }
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            first_run_completed: false,
            start_hidden: false,
            close_to_tray: true,
            launch_at_login: false,
            resume_last_session: false,
            log_level: LogLevel::Info,
            window_width: default_window_width(),
            window_height: default_window_height(),
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
            input_mode: InputMode::Off,
            input_edge: ScreenEdge::Right,
            input_hotkey: default_input_hotkey(),
            interval_secs: default_interval_secs(),
            max_folder_depth: None,
            accept: false,
            trust_device: false,
            trusted_only: false,
        }
    }
}

impl SynlyConfig {
    pub fn load_or_create() -> Result<Self> {
        Self::load_or_create_in_dir(&config_dir()?)
    }

    pub fn clipboard_cache_dir(&self) -> Result<PathBuf> {
        let base_dir = clipboard_cache_base_dir()?;
        match &self.clipboard.cache_dir {
            Some(path) => resolve_configured_path(path, &base_dir),
            None => Ok(base_dir.join(CLIPBOARD_CACHE_DIR_NAME)),
        }
    }

    pub fn save(&self) -> Result<()> {
        write_config_to_path(&config_path_in(&config_dir()?), self)
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

    fn load_or_create_in_dir(dir: &Path) -> Result<Self> {
        let path = config_path_in(dir);
        if path.exists() {
            return load_config_from_path(&path);
        }

        let config = if let Some(device) = load_legacy_device_from_dir(dir)? {
            Self {
                device,
                clipboard: ClipboardConfig::default(),
                transfer: TransferConfig::default(),
                notifications: NotificationConfig::default(),
                discovery: DiscoveryConfig::default(),
                ui: UiConfig::default(),
                runtime: RuntimeConfig::default(),
                trusted_devices: Vec::new(),
            }
        } else {
            Self::new_generated()
        };

        let mut config = config;
        config.ensure_device_identity()?;
        write_config_to_path(&path, &config)?;
        Ok(config)
    }

    fn new_generated() -> Self {
        let device_id = Uuid::new_v4();
        let (identity_private_key, identity_public_key) =
            generate_identity_keypair().expect("failed to generate device identity");
        Self {
            device: DeviceConfig {
                device_id,
                device_name: detect_device_name(device_id),
                identity_private_key: Some(identity_private_key),
                identity_public_key: Some(identity_public_key),
            },
            clipboard: ClipboardConfig::default(),
            transfer: TransferConfig::default(),
            notifications: NotificationConfig::default(),
            discovery: DiscoveryConfig::default(),
            ui: UiConfig::default(),
            runtime: RuntimeConfig::default(),
            trusted_devices: Vec::new(),
        }
    }

    fn ensure_device_identity(&mut self) -> Result<()> {
        self.device.ensure_identity_keypair()
    }
}

impl DeviceConfig {
    pub fn short_id(&self) -> String {
        self.device_id.to_string().chars().take(8).collect()
    }

    pub fn identity_public_key(&self) -> Result<&str> {
        self.identity_public_key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .context("device identity public key is missing")
    }

    pub fn identity_private_key(&self) -> Result<&str> {
        self.identity_private_key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .context("device identity private key is missing")
    }

    fn ensure_identity_keypair(&mut self) -> Result<()> {
        match (
            self.identity_private_key.as_deref(),
            self.identity_public_key.as_deref(),
        ) {
            (Some(private_key), Some(public_key)) if !private_key.trim().is_empty() => {
                let derived_public_key = public_key_from_private_key(private_key)?;
                if derived_public_key != public_key {
                    self.identity_public_key = Some(derived_public_key);
                }
            }
            (Some(private_key), _) if !private_key.trim().is_empty() => {
                self.identity_public_key = Some(public_key_from_private_key(private_key)?);
            }
            _ => {
                let (private_key, public_key) = generate_identity_keypair()?;
                self.identity_private_key = Some(private_key);
                self.identity_public_key = Some(public_key);
            }
        }
        Ok(())
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

fn config_path_in(dir: &Path) -> PathBuf {
    dir.join(CONFIG_FILE_NAME)
}

fn legacy_device_config_path_in(dir: &Path) -> PathBuf {
    dir.join(LEGACY_DEVICE_CONFIG_FILE_NAME)
}

fn load_config_from_path(path: &Path) -> Result<SynlyConfig> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let mut config: SynlyConfig = toml::from_str(&raw)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;
    let before_private = config.device.identity_private_key.clone();
    let before_public = config.device.identity_public_key.clone();
    config.ensure_device_identity()?;
    if config.device.identity_private_key != before_private
        || config.device.identity_public_key != before_public
    {
        write_config_to_path(path, &config)?;
    }
    Ok(config)
}

fn write_config_to_path(path: &Path, config: &SynlyConfig) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("invalid config path {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    let pretty = toml::to_string_pretty(config).context("failed to serialize config")?;
    fs::write(path, pretty)
        .with_context(|| format!("failed to write config at {}", path.display()))?;
    Ok(())
}

fn load_legacy_device_from_dir(dir: &Path) -> Result<Option<DeviceConfig>> {
    let legacy_path = legacy_device_config_path_in(dir);
    if !legacy_path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(&legacy_path).with_context(|| {
        format!(
            "failed to read legacy device config at {}",
            legacy_path.display()
        )
    })?;
    let device = serde_json::from_str(&raw).with_context(|| {
        format!(
            "failed to parse legacy device config at {}",
            legacy_path.display()
        )
    })?;
    Ok(Some(device))
}

fn default_clipboard_max_file_bytes() -> u64 {
    DEFAULT_CLIPBOARD_MAX_FILE_BYTES
}

fn default_true() -> bool {
    true
}

fn default_window_width() -> u32 {
    1080
}

fn default_window_height() -> u32 {
    720
}

fn default_file_sync_mode() -> FileSyncMode {
    FileSyncMode::Off
}

fn default_input_edge() -> ScreenEdge {
    ScreenEdge::Right
}

fn default_input_hotkey() -> String {
    crate::input::Hotkey::DEFAULT.to_string()
}

fn default_interval_secs() -> u64 {
    3
}

fn default_transfer_max_meta_bytes() -> u64 {
    TransferLimits::default().max_meta_len as u64
}

fn default_transfer_max_frame_data_bytes() -> u64 {
    TransferLimits::default().max_frame_data_len as u64
}

fn default_transfer_max_clipboard_bytes() -> u64 {
    TransferLimits::default().max_clipboard_binary_len as u64
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn generate_identity_keypair() -> Result<(String, String)> {
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| anyhow!("failed to generate identity key"))?;
    let private_key = STANDARD_NO_PAD.encode(pkcs8.as_ref());
    let public_key = public_key_from_private_key(&private_key)?;
    Ok((private_key, public_key))
}

fn public_key_from_private_key(private_key: &str) -> Result<String> {
    let pkcs8 = STANDARD_NO_PAD
        .decode(private_key.trim().as_bytes())
        .context("failed to decode device identity private key")?;
    let key_pair = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| anyhow!("failed to parse device identity private key"))?;
    Ok(STANDARD_NO_PAD.encode(key_pair.public_key().as_ref()))
}

fn resolve_configured_path(path: &Path, base_dir: &Path) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    let path = expand_config_path_string(&raw).context("configured path cannot be empty")?;
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(base_dir.join(path))
    }
}

fn config_dir() -> Result<PathBuf> {
    let home = home_dir().context("unable to determine home directory")?;
    Ok(Path::new(&home).join(".config").join("synly"))
}

fn clipboard_cache_base_dir() -> Result<PathBuf> {
    dirs::cache_dir()
        .map(|dir| dir.join("synly"))
        .context("unable to determine platform cache directory")
}

fn detect_device_name(device_id: Uuid) -> String {
    for key in ["SYNLY_DEVICE_NAME", "HOSTNAME", "COMPUTERNAME"] {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    if let Ok(user) = env::var("USER").or_else(|_| env::var("USERNAME")) {
        let trimmed = user.trim();
        if !trimmed.is_empty() {
            return format!(
                "{}-{}",
                trimmed,
                device_id.to_string().chars().take(4).collect::<String>()
            );
        }
    }

    format!(
        "synly-{}",
        device_id.to_string().chars().take(6).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CLIPBOARD_CACHE_DIR_NAME, ClipboardConfig, DeviceConfig, DiscoveryConfig,
        LndDiscoveryConfig, NotificationConfig, SynlyConfig, TransferConfig, TrustedDeviceConfig,
        clipboard_cache_base_dir, config_dir, config_path_in, legacy_device_config_path_in,
        resolve_configured_path,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use uuid::Uuid;

    #[test]
    fn load_or_create_in_dir_creates_toml_config() {
        let dir = unique_test_dir("create");
        let config = SynlyConfig::load_or_create_in_dir(&dir).unwrap();

        let path = config_path_in(&dir);
        assert!(path.exists());

        let saved = fs::read_to_string(path).unwrap();
        assert!(saved.contains("[device]"));
        assert!(saved.contains("[clipboard]"));
        assert!(saved.contains("[transfer]"));
        assert_eq!(config.clipboard, ClipboardConfig::default());
        assert_eq!(config.transfer, TransferConfig::default());
        assert_eq!(config.notifications, NotificationConfig::default());
        assert_eq!(config.discovery, DiscoveryConfig::default());
        assert!(config.trusted_devices.is_empty());

        cleanup_dir(&dir);
    }

    #[test]
    fn load_or_create_in_dir_migrates_legacy_json() {
        let dir = unique_test_dir("migrate");
        fs::create_dir_all(&dir).unwrap();

        let legacy_device = DeviceConfig {
            device_id: Uuid::new_v4(),
            device_name: "legacy-device".to_string(),
            identity_private_key: None,
            identity_public_key: None,
        };
        let legacy_path = legacy_device_config_path_in(&dir);
        fs::write(
            &legacy_path,
            serde_json::to_string_pretty(&legacy_device).unwrap(),
        )
        .unwrap();

        let config = SynlyConfig::load_or_create_in_dir(&dir).unwrap();

        assert_eq!(config.device.device_id, legacy_device.device_id);
        assert_eq!(config.device.device_name, legacy_device.device_name);
        assert!(config.device.identity_private_key.is_some());
        assert!(config.device.identity_public_key.is_some());
        assert_eq!(config.clipboard, ClipboardConfig::default());
        assert_eq!(config.transfer, TransferConfig::default());
        assert_eq!(config.notifications, NotificationConfig::default());
        assert_eq!(config.discovery, DiscoveryConfig::default());
        assert!(config.trusted_devices.is_empty());
        assert!(config_path_in(&dir).exists());

        cleanup_dir(&dir);
    }

    #[test]
    fn parse_existing_toml_without_clipboard_section_uses_default() {
        let dir = unique_test_dir("default_clipboard");
        fs::create_dir_all(&dir).unwrap();
        let path = config_path_in(&dir);
        let legacy_style_toml = format!(
            "[device]\ndevice_id = \"{}\"\ndevice_name = \"demo\"\n",
            Uuid::new_v4()
        );
        fs::write(&path, legacy_style_toml).unwrap();

        let config = SynlyConfig::load_or_create_in_dir(&dir).unwrap();
        assert_eq!(config.device.device_name, "demo");
        assert!(config.device.identity_private_key.is_some());
        assert!(config.device.identity_public_key.is_some());
        assert_eq!(config.clipboard, ClipboardConfig::default());
        assert_eq!(config.transfer, TransferConfig::default());
        assert_eq!(config.notifications, NotificationConfig::default());
        assert_eq!(config.discovery, DiscoveryConfig::default());
        assert!(config.trusted_devices.is_empty());

        cleanup_dir(&dir);
    }

    #[test]
    fn custom_relative_cache_dir_is_loaded() {
        let dir = unique_test_dir("cache-relative");
        fs::create_dir_all(&dir).unwrap();
        let path = config_path_in(&dir);
        let toml = format!(
            "[device]\ndevice_id = \"{}\"\ndevice_name = \"demo\"\n\n[clipboard]\nmax_file_bytes = 42\nmax_cache_bytes = 99\ncache_dir = \"custom-cache\"\n",
            Uuid::new_v4()
        );
        fs::write(&path, toml).unwrap();

        let config = SynlyConfig::load_or_create_in_dir(&dir).unwrap();
        assert!(config.device.identity_private_key.is_some());
        assert!(config.device.identity_public_key.is_some());
        assert_eq!(config.clipboard.max_file_bytes, 42);
        assert_eq!(config.clipboard.max_cache_bytes, Some(99));
        assert_eq!(config.transfer, TransferConfig::default());
        assert_eq!(config.notifications, NotificationConfig::default());
        assert_eq!(config.discovery, DiscoveryConfig::default());
        assert_eq!(
            config.clipboard.cache_dir,
            Some(PathBuf::from("custom-cache"))
        );
        assert!(config.trusted_devices.is_empty());

        cleanup_dir(&dir);
    }

    #[test]
    fn remember_trusted_device_persists_in_config() {
        let dir = unique_test_dir("trusted-device");
        fs::create_dir_all(&dir).unwrap();

        let mut config = SynlyConfig::new_generated();
        super::write_config_to_path(&config_path_in(&dir), &config).unwrap();

        let remote_id = Uuid::new_v4();
        config.remember_trusted_device(
            remote_id,
            "remote".to_string(),
            "pubkey".to_string(),
            "rootcert".to_string(),
        );

        super::write_config_to_path(&config_path_in(&dir), &config).unwrap();
        let reloaded = SynlyConfig::load_or_create_in_dir(&dir).unwrap();
        assert_eq!(
            reloaded.trusted_devices,
            vec![TrustedDeviceConfig {
                device_id: remote_id,
                device_name: "remote".to_string(),
                public_key: "pubkey".to_string(),
                tls_root_certificate: "rootcert".to_string(),
                trusted_at_ms: reloaded.trusted_devices[0].trusted_at_ms,
                last_seen_ms: reloaded.trusted_devices[0].last_seen_ms,
                successful_sessions: 1,
            }]
        );

        cleanup_dir(&dir);
    }

    #[test]
    fn resolve_configured_path_joins_relative_path() {
        let base = PathBuf::from("/tmp/synly-config-base");
        let resolved = resolve_configured_path(Path::new("cache-dir"), &base).unwrap();
        assert_eq!(resolved, base.join("cache-dir"));
    }

    #[test]
    fn custom_transfer_limits_are_loaded() {
        let dir = unique_test_dir("transfer");
        fs::create_dir_all(&dir).unwrap();
        let path = config_path_in(&dir);
        let toml = format!(
            "[device]\ndevice_id = \"{}\"\ndevice_name = \"demo\"\n\n[transfer]\nmax_meta_bytes = 123\nmax_frame_data_bytes = 456\nmax_clipboard_bytes = 789\n",
            Uuid::new_v4()
        );
        fs::write(&path, toml).unwrap();

        let config = SynlyConfig::load_or_create_in_dir(&dir).unwrap();
        assert_eq!(
            config.transfer,
            TransferConfig {
                max_meta_bytes: 123,
                max_frame_data_bytes: 456,
                max_clipboard_bytes: 789,
            }
        );

        cleanup_dir(&dir);
    }

    #[test]
    fn notification_and_lnd_config_are_loaded() {
        let dir = unique_test_dir("notification-lnd");
        fs::create_dir_all(&dir).unwrap();
        let path = config_path_in(&dir);
        let toml = format!(
            "[device]\ndevice_id = \"{}\"\ndevice_name = \"demo\"\n\n[notifications]\nenabled = false\n\n[discovery.lnd]\nserver_url = \"https://example.com/lnd\"\nbearer_token = \"secret\"\ndiscovery_domain = \"office-a\"\n",
            Uuid::new_v4()
        );
        fs::write(&path, toml).unwrap();

        let config = SynlyConfig::load_or_create_in_dir(&dir).unwrap();

        assert!(!config.notifications.enabled);
        assert_eq!(
            config.discovery.lnd,
            Some(LndDiscoveryConfig {
                server_url: "https://example.com/lnd".to_string(),
                bearer_token: "secret".to_string(),
                discovery_domain: Some("office-a".to_string()),
            })
        );

        cleanup_dir(&dir);
    }

    #[test]
    fn invalid_transfer_limits_are_rejected() {
        let err = TransferConfig {
            max_meta_bytes: 0,
            max_frame_data_bytes: 1,
            max_clipboard_bytes: 1,
        }
        .to_limits()
        .unwrap_err();
        assert!(err.to_string().contains("max_meta_bytes"));
    }

    #[test]
    fn generated_config_contains_identity_keypair() {
        let config = SynlyConfig::new_generated();
        assert!(config.device.identity_private_key.is_some());
        assert!(config.device.identity_public_key.is_some());
    }

    #[test]
    fn config_dir_is_always_under_home_dot_config() {
        let dir = config_dir().unwrap();
        assert!(dir.ends_with(Path::new(".config").join("synly")));
    }

    #[test]
    fn clipboard_cache_uses_platform_cache_dir() {
        let mut config = SynlyConfig::new_generated();
        let cache_dir = clipboard_cache_base_dir().unwrap();
        assert_eq!(
            config.clipboard_cache_dir().unwrap(),
            cache_dir.join(CLIPBOARD_CACHE_DIR_NAME)
        );

        config.clipboard.cache_dir = Some(PathBuf::from("custom-cache"));
        assert_eq!(
            config.clipboard_cache_dir().unwrap(),
            cache_dir.join("custom-cache")
        );
    }

    #[test]
    fn example_config_is_valid() {
        let config: SynlyConfig = toml::from_str(include_str!("../config.toml.example")).unwrap();
        assert!(config.device.identity_private_key.is_none());
        assert!(config.device.identity_public_key.is_none());
        assert!(config.discovery.lnd.is_none());
        assert!(config.notifications.enabled);
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("synly-config-test-{label}-{}", Uuid::new_v4()))
    }

    fn cleanup_dir(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}
