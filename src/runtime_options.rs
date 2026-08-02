use crate::clipboard::ClipboardRuntimeOptions;
use crate::config::{DiscoveryConfig, RuntimeConfig, SynlyConfig};
use crate::input::{InputMode, InputRuntimeOptions};
use crate::path_expand::expand_path_string;
use crate::protocol::{RuntimeCapabilities, TransferLimits};
use crate::runtime_control::{RuntimeControl, RuntimeTuning};
use crate::settings::{AudioMode, ClipboardMode, ConnectionPreference, FileSyncMode};
use crate::sync::WorkspaceSpec;
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

const DEFAULT_DISCOVERY_SECS: u64 = 3;

#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    pub file_sync_mode: FileSyncMode,
    pub connection: ConnectionPreference,
    pub instance_name: Option<String>,
    pub workspace: WorkspaceSpec,
    pub sync_delete: bool,
    pub clipboard_mode: ClipboardMode,
    pub audio_mode: AudioMode,
    pub input_mode: InputMode,
    pub input: InputRuntimeOptions,
    pub notifications_enabled: bool,
    pub discovery: DiscoveryConfig,
    pub clipboard: ClipboardRuntimeOptions,
    pub transfer_limits: TransferLimits,
    pub interval_secs: u64,
    pub pairing: PairingRuntimeOptions,
    pub control: RuntimeControl,
}

#[derive(Clone, Debug)]
pub struct PairingRuntimeOptions {
    pub headless: bool,
    pub peer_query: Option<String>,
    pub port: Option<u16>,
    pub pin: Option<String>,
    pub accept: bool,
    pub trust_device: bool,
    pub trusted_only: bool,
    pub discovery_secs: u64,
}

pub fn runtime_options_from_config(
    config: &SynlyConfig,
    pin: Option<String>,
    headless: bool,
) -> Result<RuntimeOptions> {
    validate_runtime_config(&config.runtime, headless)?;
    let runtime = &config.runtime;
    let connection = runtime.connection.context("配置中缺少连接方式")?;
    let workspace = workspace_from_config(runtime)?;
    let sync_delete = workspace.incoming_root.is_some() && runtime.sync_delete;
    let pin = pin.as_deref().map(normalize_pin).transpose()?;
    let input = InputRuntimeOptions {
        mode: runtime.input.mode,
        edge: runtime.input.edge,
        hotkey: runtime.input.hotkey.parse()?,
        reverse_mouse_wheel: runtime.input.reverse_mouse_wheel,
        reverse_trackpad: runtime.input.reverse_trackpad,
        block_switch_on_press: runtime.input.block_switch_on_press,
        key_mapping: runtime.input.key_mapping.clone(),
        cursor_mode: runtime.input.cursor_mode,
    };
    let clipboard = ClipboardRuntimeOptions {
        max_file_bytes: config.clipboard.max_file_bytes,
        max_cache_bytes: config.clipboard.max_cache_bytes,
        cache_dir: config.clipboard_cache_dir()?,
    };
    let capabilities = RuntimeCapabilities {
        clipboard_mode: runtime.clipboard_mode,
        audio_mode: runtime.audio_mode,
        input_mode: runtime.input.mode,
    };
    let instance_name = normalize_optional_text(&runtime.instance_name);
    let tuning = RuntimeTuning {
        interval_secs: runtime.interval_secs.max(1),
        sync_delete,
        notifications_enabled: config.notifications.enabled,
        input_backend_generation: 0,
        device_name: config.device.device_name.clone(),
        instance_name: instance_name.clone(),
        discovery: config.discovery.clone(),
        input: input.clone(),
        clipboard: clipboard.clone(),
    };

    Ok(RuntimeOptions {
        file_sync_mode: runtime.file_sync_mode,
        connection,
        instance_name,
        workspace,
        sync_delete,
        clipboard_mode: runtime.clipboard_mode,
        audio_mode: runtime.audio_mode,
        input_mode: runtime.input.mode,
        input,
        notifications_enabled: config.notifications.enabled,
        discovery: config.discovery.clone(),
        clipboard,
        transfer_limits: config.transfer.to_limits()?,
        interval_secs: runtime.interval_secs.max(1),
        pairing: PairingRuntimeOptions {
            headless,
            peer_query: normalize_optional_text(&runtime.peer_query),
            port: runtime.port,
            pin,
            accept: runtime.accept,
            trust_device: runtime.trust_device,
            trusted_only: runtime.trusted_only,
            discovery_secs: DEFAULT_DISCOVERY_SECS,
        },
        control: RuntimeControl::detached(capabilities, tuning),
    })
}

fn validate_runtime_config(runtime: &RuntimeConfig, headless: bool) -> Result<()> {
    if runtime.connection.is_none() {
        bail!("配置中缺少连接方式")
    }
    if runtime.port == Some(0) {
        bail!("配置中的监听端口必须大于 0")
    }
    if headless && !runtime.trusted_only {
        bail!("headless 模式要求配置 trusted_only = true")
    }
    if headless
        && runtime.connection == Some(ConnectionPreference::Join)
        && runtime.peer_query.trim().is_empty()
    {
        bail!("headless join 模式要求配置非空 peer_query")
    }
    Ok(())
}

pub fn sync_delete_label(enabled: bool) -> &'static str {
    if enabled { "开启" } else { "关闭" }
}

pub fn normalize_pin(pin: &str) -> Result<String> {
    let trimmed = pin.trim();
    if trimmed.len() != 6 || !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("PIN 必须是 6 位数字")
    }
    Ok(trimmed.to_string())
}

pub fn require_peer_query(peer_query: Option<&str>) -> Result<&str> {
    match peer_query {
        Some(query) if !query.trim().is_empty() => Ok(query.trim()),
        _ => bail!(
            "join 模式要求配置 peer_query, 可使用实例名, 设备名, 设备 ID 前缀, IPv4 地址或完整 IPv4:端口"
        ),
    }
}

fn workspace_from_config(runtime: &RuntimeConfig) -> Result<WorkspaceSpec> {
    let paths = runtime.paths.clone();
    let initial = runtime.initial;
    let workspace = match runtime.file_sync_mode {
        FileSyncMode::Off => {
            if initial.is_some() {
                bail!("file_sync_mode = off 时 initial 必须为空")
            }
            if !paths.is_empty() {
                bail!("file_sync_mode = off 时 paths 必须为空")
            }
            WorkspaceSpec::for_off()
        }
        FileSyncMode::Send => {
            if initial.is_some() {
                bail!("file_sync_mode = send 时 initial 必须为空")
            }
            if paths.is_empty() {
                bail!("file_sync_mode = send 时 paths 至少需要 1 个路径")
            }
            WorkspaceSpec::for_send(expand_path_list(paths)?)?
        }
        FileSyncMode::Receive => {
            if initial.is_some() {
                bail!("file_sync_mode = receive 时 initial 必须为空")
            }
            WorkspaceSpec::for_receive(expand_single_path(paths, "receive")?)?
        }
        FileSyncMode::Both => WorkspaceSpec::for_both(expand_single_path(paths, "both")?)?
            .with_initial_sync(Some(initial.context(
                "file_sync_mode = both 时必须配置 initial = this 或 other",
            )?)),
        FileSyncMode::Auto => WorkspaceSpec::for_auto(expand_single_path(paths, "auto")?)?
            .with_initial_sync(Some(initial.context(
                "file_sync_mode = auto 时必须配置 initial = this 或 other",
            )?)),
    };
    Ok(workspace.with_max_folder_depth(runtime.max_folder_depth))
}

fn expand_path_list(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    paths.into_iter().map(expand_pathbuf).collect()
}

fn expand_single_path(paths: Vec<PathBuf>, mode_name: &str) -> Result<PathBuf> {
    match paths.len() {
        0 => bail!("file_sync_mode = {mode_name} 时需要 1 个目录路径"),
        1 => expand_pathbuf(paths.into_iter().next().expect("path length checked")),
        _ => bail!("file_sync_mode = {mode_name} 时只能配置 1 个目录路径"),
    }
}

fn expand_pathbuf(path: PathBuf) -> Result<PathBuf> {
    expand_path_string(&path.to_string_lossy())
}

fn normalize_optional_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ClipboardConfig, DeviceConfig, DiscoveryConfig, NotificationConfig, TransferConfig,
        UiConfig,
    };
    use crate::input::ScreenEdge;
    use crate::settings::InitialSyncMode;
    use uuid::Uuid;

    #[test]
    fn runtime_options_map_complete_config() {
        let mut config = test_config();
        config.runtime.connection = Some(ConnectionPreference::Join);
        config.runtime.instance_name = " worker-a ".to_string();
        config.runtime.peer_query = " demo-device ".to_string();
        config.runtime.file_sync_mode = FileSyncMode::Both;
        config.runtime.paths = vec![PathBuf::from(".")];
        config.runtime.initial = Some(InitialSyncMode::Other);
        config.runtime.sync_delete = true;
        config.runtime.clipboard_mode = ClipboardMode::Receive;
        config.runtime.audio_mode = AudioMode::Send;
        config.runtime.input.mode = InputMode::Send;
        config.runtime.input.edge = ScreenEdge::Left;
        config.runtime.interval_secs = 9;
        config.runtime.max_folder_depth = Some(4);
        config.runtime.accept = true;
        config.runtime.trust_device = true;
        config.runtime.trusted_only = true;

        let options = runtime_options_from_config(&config, Some("123456".to_string()), false)
            .unwrap();

        assert_eq!(options.connection, ConnectionPreference::Join);
        assert_eq!(options.instance_name.as_deref(), Some("worker-a"));
        assert_eq!(options.pairing.peer_query.as_deref(), Some("demo-device"));
        assert_eq!(options.pairing.pin.as_deref(), Some("123456"));
        assert_eq!(options.input.edge, ScreenEdge::Left);
        assert_eq!(options.interval_secs, 9);
        assert_eq!(
            options
                .workspace
                .session_summary(ClipboardMode::Receive, AudioMode::Send, InputMode::Send)
                .max_folder_depth,
            Some(4)
        );
    }

    #[test]
    fn headless_requires_trusted_configuration() {
        let mut config = test_config();
        config.runtime.connection = Some(ConnectionPreference::Host);
        assert!(runtime_options_from_config(&config, None, true).is_err());

        config.runtime.trusted_only = true;
        assert!(runtime_options_from_config(&config, None, true).is_ok());
    }

    #[test]
    fn headless_join_requires_peer_query() {
        let mut config = test_config();
        config.runtime.connection = Some(ConnectionPreference::Join);
        config.runtime.trusted_only = true;
        assert!(runtime_options_from_config(&config, None, true).is_err());

        config.runtime.peer_query = "peer-a".to_string();
        assert!(runtime_options_from_config(&config, None, true).is_ok());
    }

    #[test]
    fn runtime_config_rejects_missing_role_and_invalid_workspace() {
        let config = test_config();
        assert!(runtime_options_from_config(&config, None, false).is_err());

        let mut config = test_config();
        config.runtime.connection = Some(ConnectionPreference::Host);
        config.runtime.file_sync_mode = FileSyncMode::Receive;
        assert!(runtime_options_from_config(&config, None, false).is_err());
    }

    #[test]
    fn normalize_pin_requires_six_digits() {
        assert_eq!(normalize_pin("001234").unwrap(), "001234");
        assert!(normalize_pin("12345").is_err());
        assert!(normalize_pin("12ab56").is_err());
    }

    fn test_config() -> SynlyConfig {
        SynlyConfig {
            device: DeviceConfig {
                device_id: Uuid::nil(),
                device_name: "test-device".to_string(),
                identity_private_key: String::new(),
                identity_public_key: String::new(),
            },
            clipboard: ClipboardConfig::default(),
            transfer: TransferConfig::default(),
            notifications: NotificationConfig::default(),
            discovery: DiscoveryConfig::default(),
            ui: UiConfig::default(),
            runtime: RuntimeConfig::default(),
            trusted_devices: Vec::new(),
            preferred_active: None,
        }
    }
}
