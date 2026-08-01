use super::identity::{generate_keypair, validate_keypair};
use super::schema::{
    ClipboardConfig, DeviceConfig, DiscoveryConfig, InputConfig, NotificationConfig,
    RuntimeConfig, SynlyConfig, TransferConfig, TrustedDeviceConfig, UiConfig,
};
use crate::path_expand::home_dir;
use crate::settings::{
    AudioMode, ClipboardMode, ConnectionPreference, FileSyncMode, InitialSyncMode,
};
use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

const CONFIG_FILE_NAME: &str = "config.toml";
const IDENTITY_FILE_NAME: &str = "identity.toml";
const TRUSTED_DEVICES_FILE_NAME: &str = "trusted-devices.toml";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MainConfigFile {
    device: DeviceFileConfig,
    clipboard: ClipboardConfig,
    transfer: TransferConfig,
    notifications: NotificationConfig,
    discovery: DiscoveryConfig,
    ui: UiConfig,
    runtime: RuntimeFileConfig,
    input: InputConfig,
    /// 首选活跃设备, 旧配置缺失时按无处理.
    #[serde(default)]
    preferred_active: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceFileConfig {
    device_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeFileConfig {
    connection: Option<ConnectionPreference>,
    instance_name: String,
    peer_query: String,
    port: Option<u16>,
    file_sync_mode: FileSyncMode,
    paths: Vec<PathBuf>,
    initial: Option<InitialSyncMode>,
    sync_delete: bool,
    clipboard_mode: ClipboardMode,
    audio_mode: AudioMode,
    interval_secs: u64,
    max_folder_depth: Option<usize>,
    accept: bool,
    trust_device: bool,
    trusted_only: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityFile {
    device_id: Uuid,
    private_key: String,
    public_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedDevicesFile {
    devices: Vec<TrustedDeviceConfig>,
}

pub(super) fn load_or_create() -> Result<SynlyConfig> {
    load_or_create_in_dir(&config_dir()?)
}

pub(super) fn save_settings(config: &SynlyConfig) -> Result<()> {
    validate_main_config(config)?;
    write_toml_atomic(
        &config_dir()?.join(CONFIG_FILE_NAME),
        &MainConfigFile::from(config),
    )
}

pub(super) fn save_trusted_devices(config: &SynlyConfig) -> Result<()> {
    validate_trusted_devices(&config.trusted_devices)?;
    write_toml_atomic(
        &config_dir()?.join(TRUSTED_DEVICES_FILE_NAME),
        &TrustedDevicesFile {
            devices: config.trusted_devices.clone(),
        },
    )
}

fn load_or_create_in_dir(dir: &Path) -> Result<SynlyConfig> {
    let main_path = dir.join(CONFIG_FILE_NAME);
    let identity_path = dir.join(IDENTITY_FILE_NAME);
    let trusted_path = dir.join(TRUSTED_DEVICES_FILE_NAME);

    // 先解析所有现存文件, 避免旧格式或损坏文件导致部分新文件落盘.
    let existing_main = read_optional_toml::<MainConfigFile>(&main_path)?;
    let existing_identity = read_optional_toml::<IdentityFile>(&identity_path)?;
    let existing_trusted = read_optional_toml::<TrustedDevicesFile>(&trusted_path)?;
    if let Some(identity) = &existing_identity {
        validate_keypair(&identity.private_key, &identity.public_key)
            .with_context(|| format!("invalid identity at {}", identity_path.display()))?;
    }
    if let Some(trusted) = &existing_trusted {
        validate_trusted_devices(&trusted.devices)
            .with_context(|| format!("invalid trusted devices at {}", trusted_path.display()))?;
    }

    let identity = match existing_identity {
        Some(identity) => identity,
        None => {
            let (private_key, public_key) = generate_keypair()?;
            IdentityFile {
                device_id: Uuid::new_v4(),
                private_key,
                public_key,
            }
        }
    };
    let main = existing_main.unwrap_or_else(|| MainConfigFile::new(identity.device_id));
    validate_main_file(&main)?;
    let trusted = existing_trusted.unwrap_or(TrustedDevicesFile {
        devices: Vec::new(),
    });

    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create config dir {}", dir.display()))?;
    if !main_path.exists() {
        write_toml_atomic(&main_path, &main)?;
    }
    if !identity_path.exists() {
        write_toml_atomic(&identity_path, &identity)?;
    }
    if !trusted_path.exists() {
        write_toml_atomic(&trusted_path, &trusted)?;
    }

    Ok(main.into_runtime(identity, trusted.devices))
}

impl MainConfigFile {
    fn new(device_id: Uuid) -> Self {
        Self {
            device: DeviceFileConfig {
                device_name: detect_device_name(device_id),
            },
            clipboard: ClipboardConfig::default(),
            transfer: TransferConfig::default(),
            notifications: NotificationConfig::default(),
            discovery: DiscoveryConfig::default(),
            ui: UiConfig::default(),
            runtime: RuntimeFileConfig::default(),
            input: InputConfig::default(),
            preferred_active: None,
        }
    }

    fn into_runtime(
        self,
        identity: IdentityFile,
        trusted_devices: Vec<TrustedDeviceConfig>,
    ) -> SynlyConfig {
        SynlyConfig {
            device: DeviceConfig {
                device_id: identity.device_id,
                device_name: self.device.device_name,
                identity_private_key: identity.private_key,
                identity_public_key: identity.public_key,
            },
            clipboard: self.clipboard,
            transfer: self.transfer,
            notifications: self.notifications,
            discovery: self.discovery,
            ui: self.ui,
            runtime: self.runtime.into_runtime(self.input),
            trusted_devices,
            preferred_active: self.preferred_active,
        }
    }
}

impl From<&SynlyConfig> for MainConfigFile {
    fn from(config: &SynlyConfig) -> Self {
        Self {
            device: DeviceFileConfig {
                device_name: config.device.device_name.clone(),
            },
            clipboard: config.clipboard.clone(),
            transfer: config.transfer.clone(),
            notifications: config.notifications.clone(),
            discovery: config.discovery.clone(),
            ui: config.ui.clone(),
            runtime: RuntimeFileConfig::from(&config.runtime),
            input: config.runtime.input.clone(),
            preferred_active: config.preferred_active,
        }
    }
}

impl Default for RuntimeFileConfig {
    fn default() -> Self {
        let runtime = RuntimeConfig::default();
        Self::from(&runtime)
    }
}

impl RuntimeFileConfig {
    fn into_runtime(self, input: InputConfig) -> RuntimeConfig {
        RuntimeConfig {
            connection: self.connection,
            instance_name: self.instance_name,
            peer_query: self.peer_query,
            port: self.port,
            file_sync_mode: self.file_sync_mode,
            paths: self.paths,
            initial: self.initial,
            sync_delete: self.sync_delete,
            clipboard_mode: self.clipboard_mode,
            audio_mode: self.audio_mode,
            input,
            interval_secs: self.interval_secs,
            max_folder_depth: self.max_folder_depth,
            accept: self.accept,
            trust_device: self.trust_device,
            trusted_only: self.trusted_only,
        }
    }
}

impl From<&RuntimeConfig> for RuntimeFileConfig {
    fn from(runtime: &RuntimeConfig) -> Self {
        Self {
            connection: runtime.connection,
            instance_name: runtime.instance_name.clone(),
            peer_query: runtime.peer_query.clone(),
            port: runtime.port,
            file_sync_mode: runtime.file_sync_mode,
            paths: runtime.paths.clone(),
            initial: runtime.initial,
            sync_delete: runtime.sync_delete,
            clipboard_mode: runtime.clipboard_mode,
            audio_mode: runtime.audio_mode,
            interval_secs: runtime.interval_secs,
            max_folder_depth: runtime.max_folder_depth,
            accept: runtime.accept,
            trust_device: runtime.trust_device,
            trusted_only: runtime.trusted_only,
        }
    }
}

fn read_optional_toml<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    toml::from_str(&raw)
        .with_context(|| format!("failed to parse config at {}", path.display()))
        .map(Some)
}

fn validate_main_config(config: &SynlyConfig) -> Result<()> {
    if config.device.device_name.trim().is_empty() {
        anyhow::bail!("device name cannot be empty");
    }
    crate::input::validate_key_mapping(&config.runtime.input.key_mapping)?;
    config.runtime.input.hotkey.parse::<crate::input::Hotkey>()?;
    Ok(())
}

fn validate_main_file(config: &MainConfigFile) -> Result<()> {
    if config.device.device_name.trim().is_empty() {
        anyhow::bail!("device name cannot be empty");
    }
    crate::input::validate_key_mapping(&config.input.key_mapping)?;
    config.input.hotkey.parse::<crate::input::Hotkey>()?;
    Ok(())
}

fn validate_trusted_devices(devices: &[TrustedDeviceConfig]) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    for device in devices {
        if !ids.insert(device.device_id) {
            anyhow::bail!("trusted devices contain duplicate device ID {}", device.device_id);
        }
        if device.device_name.trim().is_empty() {
            anyhow::bail!("trusted device {} has an empty name", device.device_id);
        }
        if device.public_key.trim().is_empty() {
            anyhow::bail!("trusted device {} has an empty public key", device.device_id);
        }
        if device.tls_root_certificate.trim().is_empty() {
            anyhow::bail!("trusted device {} has an empty TLS root certificate", device.device_id);
        }
    }
    Ok(())
}

fn write_toml_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("invalid config path {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create config dir {}", parent.display()))?;
    let pretty = toml::to_string_pretty(value).context("failed to serialize config")?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|name| name.to_str()).unwrap_or("synly"),
        Uuid::new_v4()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .with_context(|| format!("failed to create temporary config {}", temp.display()))?;
        file.write_all(pretty.as_bytes())?;
        file.sync_all()?;
        replace_file(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.with_context(|| format!("failed to write config at {}", path.display()))
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn config_dir() -> Result<PathBuf> {
    let home = home_dir().context("unable to determine home directory")?;
    Ok(Path::new(&home).join(".config").join("synly"))
}

fn detect_device_name(device_id: Uuid) -> String {
    for key in ["SYNLY_DEVICE_NAME", "HOSTNAME", "COMPUTERNAME"] {
        if let Ok(value) = std::env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    if let Ok(user) = std::env::var("USER").or_else(|_| std::env::var("USERNAME")) {
        let trimmed = user.trim();
        if !trimmed.is_empty() {
            return format!("{trimmed}-{}", device_id.to_string().chars().take(8).collect::<String>());
        }
    }
    format!("synly-{}", device_id.to_string().chars().take(8).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn first_start_creates_three_strict_files() {
        let dir = unique_test_dir("first-start");
        let config = load_or_create_in_dir(&dir).unwrap();
        assert!(dir.join(CONFIG_FILE_NAME).exists());
        assert!(dir.join(IDENTITY_FILE_NAME).exists());
        assert!(dir.join(TRUSTED_DEVICES_FILE_NAME).exists());
        assert_eq!(
            config.runtime.input.key_mapping,
            crate::input::KeyMappingConfig::default()
        );
        assert!(!config.runtime.input.elevate_on_start);
        assert!(!config.runtime.input.block_switch_on_press);
        let reloaded = load_or_create_in_dir(&dir).unwrap();
        assert_eq!(reloaded.device.device_id, config.device.device_id);
        assert_eq!(reloaded.runtime, config.runtime);
        cleanup_dir(&dir);
    }

    #[test]
    fn block_switch_on_press_round_trips_and_missing_field_defaults_to_false() {
        let dir = unique_test_dir("block-switch-on-press");
        let mut config = load_or_create_in_dir(&dir).unwrap();
        assert!(!config.runtime.input.block_switch_on_press);
        config.runtime.input.block_switch_on_press = true;
        write_toml_atomic(
            &dir.join(CONFIG_FILE_NAME),
            &MainConfigFile::from(&config),
        )
        .unwrap();
        let path = dir.join(CONFIG_FILE_NAME);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("block_switch_on_press = true"));
        let without_field = text
            .lines()
            .filter(|line| !line.contains("block_switch_on_press"))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, without_field).unwrap();
        let reloaded = load_or_create_in_dir(&dir).unwrap();
        assert!(!reloaded.runtime.input.block_switch_on_press);
        cleanup_dir(&dir);
    }

    #[test]
    fn legacy_monolithic_config_is_rejected_without_writes() {
        let dir = unique_test_dir("legacy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(CONFIG_FILE_NAME),
            "[device]\ndevice_id = \"00000000-0000-0000-0000-000000000000\"\ndevice_name = \"old\"\n",
        )
        .unwrap();
        assert!(load_or_create_in_dir(&dir).is_err());
        assert!(!dir.join(IDENTITY_FILE_NAME).exists());
        assert!(!dir.join(TRUSTED_DEVICES_FILE_NAME).exists());
        cleanup_dir(&dir);
    }

    #[test]
    fn identity_mismatch_is_rejected_without_repair() {
        let dir = unique_test_dir("identity-mismatch");
        let config = load_or_create_in_dir(&dir).unwrap();
        let original = fs::read_to_string(dir.join(IDENTITY_FILE_NAME)).unwrap();
        let mut identity: IdentityFile = toml::from_str(&original).unwrap();
        identity.public_key = "invalid".to_string();
        fs::write(
            dir.join(IDENTITY_FILE_NAME),
            toml::to_string_pretty(&identity).unwrap(),
        )
        .unwrap();
        assert!(load_or_create_in_dir(&dir).is_err());
        assert_eq!(config.device.device_id, identity.device_id);
        assert_eq!(
            fs::read_to_string(dir.join(IDENTITY_FILE_NAME)).unwrap(),
            toml::to_string_pretty(&identity).unwrap()
        );
        cleanup_dir(&dir);
    }

    #[test]
    fn settings_and_trust_files_are_isolated() {
        let dir = unique_test_dir("isolated-save");
        let mut config = load_or_create_in_dir(&dir).unwrap();
        let identity = fs::read_to_string(dir.join(IDENTITY_FILE_NAME)).unwrap();
        let trusted = fs::read_to_string(dir.join(TRUSTED_DEVICES_FILE_NAME)).unwrap();
        config.device.device_name = "renamed".to_string();
        write_toml_atomic(&dir.join(CONFIG_FILE_NAME), &MainConfigFile::from(&config)).unwrap();
        assert_eq!(fs::read_to_string(dir.join(IDENTITY_FILE_NAME)).unwrap(), identity);
        assert_eq!(fs::read_to_string(dir.join(TRUSTED_DEVICES_FILE_NAME)).unwrap(), trusted);

        let settings = fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap();
        config.trusted_devices.push(TrustedDeviceConfig {
            device_id: Uuid::new_v4(),
            device_name: "peer".to_string(),
            public_key: "public".to_string(),
            tls_root_certificate: "certificate".to_string(),
            trusted_at_ms: 1,
            last_seen_ms: 2,
            successful_sessions: 3,
        });
        write_toml_atomic(
            &dir.join(TRUSTED_DEVICES_FILE_NAME),
            &TrustedDevicesFile {
                devices: config.trusted_devices,
            },
        )
        .unwrap();
        assert_eq!(fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap(), settings);
        assert_eq!(fs::read_to_string(dir.join(IDENTITY_FILE_NAME)).unwrap(), identity);
        cleanup_dir(&dir);
    }

    #[test]
    fn preferred_active_survives_settings_round_trip() {
        let dir = unique_test_dir("preferred-active");
        let mut config = load_or_create_in_dir(&dir).unwrap();
        let preferred = Uuid::new_v4();
        config.preferred_active = Some(preferred);
        write_toml_atomic(
            &dir.join(CONFIG_FILE_NAME),
            &MainConfigFile::from(&config),
        )
        .unwrap();
        let reloaded = load_or_create_in_dir(&dir).unwrap();
        assert_eq!(reloaded.preferred_active, Some(preferred));
        cleanup_dir(&dir);
    }

    #[test]
    fn unknown_and_missing_fields_are_rejected() {
        let mut main = MainConfigFile::new(Uuid::new_v4());
        main.input.key_mapping.macos_to_windows = BTreeMap::new();
        let raw = toml::to_string_pretty(&main).unwrap();
        let unknown = format!("{raw}\nunknown = true\n");
        assert!(toml::from_str::<MainConfigFile>(&unknown).is_err());
        let missing = raw.replace("elevate_on_start = false\n", "");
        assert!(toml::from_str::<MainConfigFile>(&missing).is_err());
    }

    #[test]
    fn duplicate_or_incomplete_trusted_devices_are_rejected() {
        let device_id = Uuid::new_v4();
        let device = TrustedDeviceConfig {
            device_id,
            device_name: "peer".to_string(),
            public_key: "public".to_string(),
            tls_root_certificate: "certificate".to_string(),
            trusted_at_ms: 1,
            last_seen_ms: 2,
            successful_sessions: 3,
        };
        assert!(validate_trusted_devices(&[device.clone(), device.clone()]).is_err());
        let mut incomplete = device;
        incomplete.public_key.clear();
        assert!(validate_trusted_devices(&[incomplete]).is_err());
    }

    #[test]
    fn example_main_config_is_valid() {
        let config: MainConfigFile =
            toml::from_str(include_str!("../../config.toml.example")).unwrap();
        assert_eq!(config.input.key_mapping, crate::input::KeyMappingConfig::default());
        assert!(!config.input.elevate_on_start);
        assert!(!config.input.block_switch_on_press);
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("synly-config-test-{label}-{}", Uuid::new_v4()))
    }

    fn cleanup_dir(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}
