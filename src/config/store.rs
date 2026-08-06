use super::identity::{generate_keypair, validate_keypair};
use super::migrations;
use super::schema::{
    ClipboardConfig, DeviceConfig, DiscoveryConfig, GuiState, InputConfig, NotificationConfig,
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
const GUI_STATE_FILE_NAME: &str = "gui-state.toml";
const IDENTITY_FILE_NAME: &str = "identity.toml";
const TRUSTED_DEVICES_FILE_NAME: &str = "trusted-devices.toml";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MainConfigFile {
    version: u32,
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
    version: u32,
    device_id: Uuid,
    private_key: String,
    public_key: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedDevicesFile {
    version: u32,
    devices: Vec<TrustedDeviceConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GuiStateFile {
    version: u32,
    first_run_completed: bool,
    window_width: u32,
    window_height: u32,
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
            version: migrations::TRUSTED_DEVICES_VERSION,
            devices: config.trusted_devices.clone(),
        },
    )
}

pub(super) fn save_gui_state(config: &SynlyConfig) -> Result<()> {
    write_toml_atomic(
        &config_dir()?.join(GUI_STATE_FILE_NAME),
        &GuiStateFile::from(&config.gui_state),
    )
}

fn load_or_create_in_dir(dir: &Path) -> Result<SynlyConfig> {
    let main_path = dir.join(CONFIG_FILE_NAME);
    let gui_state_path = dir.join(GUI_STATE_FILE_NAME);
    let identity_path = dir.join(IDENTITY_FILE_NAME);
    let trusted_path = dir.join(TRUSTED_DEVICES_FILE_NAME);

    let existing_main = read_optional_main(&main_path)?;
    let existing_gui_state =
        read_optional_toml::<GuiStateFile>(&gui_state_path, migrations::migrate_gui_state)?;
    let existing_identity =
        read_optional_toml::<IdentityFile>(&identity_path, migrations::migrate_identity)?;
    let existing_trusted = read_optional_toml::<TrustedDevicesFile>(
        &trusted_path,
        migrations::migrate_trusted_devices,
    )?;
    let main_missing = existing_main.is_none();
    let main_migrated = existing_main
        .as_ref()
        .is_some_and(|loaded| loaded.migrated);
    let identity_missing = existing_identity.is_none();
    let identity_migrated = existing_identity
        .as_ref()
        .is_some_and(|loaded| loaded.migrated);
    let trusted_missing = existing_trusted.is_none();
    let trusted_migrated = existing_trusted
        .as_ref()
        .is_some_and(|loaded| loaded.migrated);
    let gui_state_missing = existing_gui_state.is_none();
    let legacy_gui_state = existing_main
        .as_ref()
        .and_then(|loaded| loaded.legacy_gui_state.clone());
    if let Some(identity) = &existing_identity {
        validate_keypair(&identity.value.private_key, &identity.value.public_key)
            .with_context(|| format!("invalid identity at {}", identity_path.display()))?;
    }
    if let Some(trusted) = &existing_trusted {
        validate_trusted_devices(&trusted.value.devices)
            .with_context(|| format!("invalid trusted devices at {}", trusted_path.display()))?;
    }

    let identity = match existing_identity {
        Some(identity) => identity.value,
        None => {
            let (private_key, public_key) = generate_keypair()?;
            IdentityFile {
                version: migrations::IDENTITY_VERSION,
                device_id: Uuid::new_v4(),
                private_key,
                public_key,
            }
        }
    };
    let main = existing_main
        .map(|loaded| loaded.value)
        .unwrap_or_else(|| MainConfigFile::new(identity.device_id));
    validate_main_file(&main)?;
    let trusted = existing_trusted
        .map(|loaded| loaded.value)
        .unwrap_or(TrustedDevicesFile {
            version: migrations::TRUSTED_DEVICES_VERSION,
            devices: Vec::new(),
        });
    let gui_state = existing_gui_state
        .map(|loaded| loaded.value.into_runtime())
        .or(legacy_gui_state)
        .unwrap_or_default();
    let config = main.into_runtime(
        identity.clone(),
        trusted.devices.clone(),
        gui_state,
    );

    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create config dir {}", dir.display()))?;
    if gui_state_missing {
        write_toml_atomic(&gui_state_path, &GuiStateFile::from(&config.gui_state))?;
    }
    if identity_missing || identity_migrated {
        write_toml_atomic(&identity_path, &identity)?;
    }
    if trusted_missing || trusted_migrated {
        write_toml_atomic(&trusted_path, &trusted)?;
    }
    if main_missing || main_migrated {
        write_toml_atomic(&main_path, &MainConfigFile::from(&config))?;
    }

    Ok(config)
}

impl MainConfigFile {
    fn new(device_id: Uuid) -> Self {
        Self {
            version: migrations::MAIN_CONFIG_VERSION,
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
        gui_state: GuiState,
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
            gui_state,
            runtime: self.runtime.into_runtime(self.input),
            trusted_devices,
            preferred_active: self.preferred_active,
        }
    }
}

impl From<&SynlyConfig> for MainConfigFile {
    fn from(config: &SynlyConfig) -> Self {
        Self {
            version: migrations::MAIN_CONFIG_VERSION,
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

impl From<&GuiState> for GuiStateFile {
    fn from(gui_state: &GuiState) -> Self {
        Self {
            version: migrations::GUI_STATE_VERSION,
            first_run_completed: gui_state.first_run_completed,
            window_width: gui_state.window_width,
            window_height: gui_state.window_height,
        }
    }
}

impl GuiStateFile {
    fn into_runtime(self) -> GuiState {
        GuiState {
            first_run_completed: self.first_run_completed,
            window_width: self.window_width,
            window_height: self.window_height,
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

struct LoadedDocument<T> {
    value: T,
    migrated: bool,
}

struct LoadedMainConfig {
    value: MainConfigFile,
    migrated: bool,
    legacy_gui_state: Option<GuiState>,
}

fn read_optional_main(path: &Path) -> Result<Option<LoadedMainConfig>> {
    let Some(raw) = read_optional_raw(path)? else {
        return Ok(None);
    };
    let migration = migrations::migrate_main_config(&raw)
        .with_context(|| format!("failed to migrate config at {}", path.display()))?;
    let value = migration
        .document
        .try_into()
        .with_context(|| format!("failed to parse migrated config at {}", path.display()))?;
    Ok(Some(LoadedMainConfig {
        value,
        migrated: migration.migrated,
        legacy_gui_state: migration.legacy_gui_state,
    }))
}

fn read_optional_toml<T: DeserializeOwned>(
    path: &Path,
    migrate: fn(&str) -> Result<migrations::MigrationDocument>,
) -> Result<Option<LoadedDocument<T>>> {
    let Some(raw) = read_optional_raw(path)? else {
        return Ok(None);
    };
    let migration = migrate(&raw)
        .with_context(|| format!("failed to migrate config at {}", path.display()))?;
    let value = migration
        .document
        .try_into()
        .with_context(|| format!("failed to parse migrated config at {}", path.display()))?;
    Ok(Some(LoadedDocument {
        value,
        migrated: migration.migrated,
    }))
}

fn read_optional_raw(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))
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
    fn first_start_creates_four_strict_files() {
        let dir = unique_test_dir("first-start");
        let config = load_or_create_in_dir(&dir).unwrap();
        assert!(dir.join(CONFIG_FILE_NAME).exists());
        assert!(dir.join(GUI_STATE_FILE_NAME).exists());
        assert!(dir.join(IDENTITY_FILE_NAME).exists());
        assert!(dir.join(TRUSTED_DEVICES_FILE_NAME).exists());
        assert_eq!(
            config.runtime.input.key_mapping,
            crate::input::KeyMappingConfig::default()
        );
        assert!(!config.runtime.input.elevate_on_start);
        assert!(!config.runtime.input.block_switch_on_press);
        assert!(!config.runtime.input.filter_app_events);
        let reloaded = load_or_create_in_dir(&dir).unwrap();
        assert_eq!(reloaded.device.device_id, config.device.device_id);
        assert_eq!(reloaded.runtime, config.runtime);
        assert_eq!(reloaded.gui_state, config.gui_state);
        assert!(fs::read_to_string(dir.join(CONFIG_FILE_NAME))
            .unwrap()
            .contains("version = 3"));
        assert!(fs::read_to_string(dir.join(GUI_STATE_FILE_NAME))
            .unwrap()
            .contains("version = 1"));
        assert!(fs::read_to_string(dir.join(IDENTITY_FILE_NAME))
            .unwrap()
            .contains("version = 1"));
        assert!(fs::read_to_string(dir.join(TRUSTED_DEVICES_FILE_NAME))
            .unwrap()
            .contains("version = 1"));
        cleanup_dir(&dir);
    }

    #[test]
    fn legacy_files_migrate_to_version_one_and_split_gui_state() {
        let dir = unique_test_dir("legacy-migration");
        let config = load_or_create_in_dir(&dir).unwrap();
        let device_id = config.device.device_id;

        let mut main: toml::Value =
            toml::from_str(&fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap()).unwrap();
        let main_table = main.as_table_mut().unwrap();
        main_table.remove("version");
        let ui = main_table
            .get_mut("ui")
            .and_then(toml::Value::as_table_mut)
            .unwrap();
        ui.insert("first_run_completed".to_string(), toml::Value::Boolean(true));
        ui.insert("window_width".to_string(), toml::Value::Integer(900));
        ui.insert("window_height".to_string(), toml::Value::Integer(600));
        fs::write(
            dir.join(CONFIG_FILE_NAME),
            toml::to_string_pretty(&main).unwrap(),
        )
        .unwrap();
        fs::remove_file(dir.join(GUI_STATE_FILE_NAME)).unwrap();

        for file_name in [IDENTITY_FILE_NAME, TRUSTED_DEVICES_FILE_NAME] {
            let path = dir.join(file_name);
            let mut document: toml::Value =
                toml::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            document.as_table_mut().unwrap().remove("version");
            fs::write(&path, toml::to_string_pretty(&document).unwrap()).unwrap();
        }

        let migrated = load_or_create_in_dir(&dir).unwrap();
        assert_eq!(migrated.device.device_id, device_id);
        assert_eq!(
            migrated.gui_state,
            GuiState {
                first_run_completed: true,
                window_width: 900,
                window_height: 600,
            }
        );

        let main: toml::Value =
            toml::from_str(&fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(main["version"].as_integer(), Some(3));
        assert!(!main["ui"].as_table().unwrap().contains_key("first_run_completed"));
        assert!(!main["ui"].as_table().unwrap().contains_key("window_width"));
        assert!(!main["ui"].as_table().unwrap().contains_key("window_height"));
        let gui_state: GuiStateFile =
            toml::from_str(&fs::read_to_string(dir.join(GUI_STATE_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(gui_state.version, migrations::GUI_STATE_VERSION);
        assert_eq!(gui_state.window_width, 900);
        assert_eq!(gui_state.window_height, 600);
        assert_eq!(
            toml::from_str::<IdentityFile>(
                &fs::read_to_string(dir.join(IDENTITY_FILE_NAME)).unwrap()
            )
            .unwrap()
            .version,
            migrations::IDENTITY_VERSION
        );
        assert_eq!(
            toml::from_str::<TrustedDevicesFile>(
                &fs::read_to_string(dir.join(TRUSTED_DEVICES_FILE_NAME)).unwrap()
            )
            .unwrap()
            .version,
            migrations::TRUSTED_DEVICES_VERSION
        );

        let main_after = fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap();
        let gui_after = fs::read_to_string(dir.join(GUI_STATE_FILE_NAME)).unwrap();
        let reloaded = load_or_create_in_dir(&dir).unwrap();
        assert_eq!(reloaded.gui_state, migrated.gui_state);
        assert_eq!(fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap(), main_after);
        assert_eq!(
            fs::read_to_string(dir.join(GUI_STATE_FILE_NAME)).unwrap(),
            gui_after
        );
        cleanup_dir(&dir);
    }

    #[test]
    fn existing_gui_state_wins_over_legacy_main_fields() {
        let dir = unique_test_dir("gui-state-precedence");
        load_or_create_in_dir(&dir).unwrap();
        let mut main: toml::Value =
            toml::from_str(&fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap()).unwrap();
        let main_table = main.as_table_mut().unwrap();
        main_table.remove("version");
        let ui = main_table
            .get_mut("ui")
            .and_then(toml::Value::as_table_mut)
            .unwrap();
        ui.insert("first_run_completed".to_string(), toml::Value::Boolean(false));
        ui.insert("window_width".to_string(), toml::Value::Integer(900));
        ui.insert("window_height".to_string(), toml::Value::Integer(600));
        fs::write(
            dir.join(CONFIG_FILE_NAME),
            toml::to_string_pretty(&main).unwrap(),
        )
        .unwrap();
        let gui_state = GuiStateFile {
            version: migrations::GUI_STATE_VERSION,
            first_run_completed: true,
            window_width: 1000,
            window_height: 700,
        };
        write_toml_atomic(&dir.join(GUI_STATE_FILE_NAME), &gui_state).unwrap();

        let loaded = load_or_create_in_dir(&dir).unwrap();
        assert_eq!(loaded.gui_state.window_width, 1000);
        assert_eq!(loaded.gui_state.window_height, 700);
        let main: toml::Value =
            toml::from_str(&fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(main["version"].as_integer(), Some(3));
        assert!(!main["ui"].as_table().unwrap().contains_key("window_width"));
        cleanup_dir(&dir);
    }

    #[test]
    fn gui_state_serialization_does_not_change_main_config() {
        let dir = unique_test_dir("gui-state-isolation");
        let mut config = load_or_create_in_dir(&dir).unwrap();
        let main_before = fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap();
        config.gui_state.window_width = 1000;
        config.gui_state.window_height = 700;
        write_toml_atomic(
            &dir.join(GUI_STATE_FILE_NAME),
            &GuiStateFile::from(&config.gui_state),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(CONFIG_FILE_NAME)).unwrap(),
            main_before
        );
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
    fn filter_app_events_round_trips() {
        let dir = unique_test_dir("filter-app-events");
        let mut config = load_or_create_in_dir(&dir).unwrap();
        assert!(!config.runtime.input.filter_app_events);
        config.runtime.input.filter_app_events = true;
        write_toml_atomic(
            &dir.join(CONFIG_FILE_NAME),
            &MainConfigFile::from(&config),
        )
        .unwrap();
        let reloaded = load_or_create_in_dir(&dir).unwrap();
        assert!(reloaded.runtime.input.filter_app_events);
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
                version: migrations::TRUSTED_DEVICES_VERSION,
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
        assert!(!config.input.native_scroll_macos_to_windows);
        assert!(!config.input.native_scroll_windows_to_macos);
        assert!(!config.input.filter_app_events);
        assert_eq!(config.input.cursor_mode, crate::input::CursorMode::Auto);
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("synly-config-test-{label}-{}", Uuid::new_v4()))
    }

    fn cleanup_dir(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}
