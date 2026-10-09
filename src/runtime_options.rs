use crate::audio::AudioLayout;
use crate::clipboard::ClipboardRuntimeOptions;
use crate::config::{DiscoveryConfig, RuntimeConfig, SynlyConfig};
use crate::discovery::DiscoveredPeer;
use crate::input::{InputMode, InputRuntimeOptions};
use crate::protocol::{RuntimeCapabilities, TransferLimits};
use crate::runtime_control::{RuntimeControl, RuntimeTuning};
use crate::settings::{AudioMode, ClipboardMode, ConnectionPreference};
use anyhow::{Context, Result, bail};

const DEFAULT_DISCOVERY_SECS: u64 = 3;

#[derive(Clone, Debug)]
pub struct RuntimeOptions {
    pub connection: ConnectionPreference,
    pub bluetooth_enabled: bool,
    pub instance_name: Option<String>,
    pub clipboard_mode: ClipboardMode,
    pub audio_mode: AudioMode,
    pub audio_layout: AudioLayout,
    pub input_mode: InputMode,
    pub input: InputRuntimeOptions,
    pub notifications_enabled: bool,
    pub discovery: DiscoveryConfig,
    pub clipboard: ClipboardRuntimeOptions,
    pub transfer_limits: TransferLimits,
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
    /// GUI 当前发现结果, 仅用于本次 Join 的首次直连, 不写入配置.
    pub known_peer: Option<DiscoveredPeer>,
}

pub fn runtime_options_from_config(
    config: &SynlyConfig,
    pin: Option<String>,
    headless: bool,
) -> Result<RuntimeOptions> {
    validate_runtime_config(&config.runtime, headless)?;
    let runtime = &config.runtime;
    let connection = runtime.connection.context("配置中缺少连接方式")?;
    let pin = pin.as_deref().map(normalize_pin).transpose()?;
    let input = InputRuntimeOptions {
        mode: runtime.input.mode,
        path: runtime.input.path,
        edge: runtime.input.edge,
        hotkey: runtime.input.hotkey.parse()?,
        reverse_mouse_wheel: runtime.input.reverse_mouse_wheel,
        reverse_trackpad: runtime.input.reverse_trackpad,
        native_scroll_macos_to_windows: runtime.input.native_scroll_macos_to_windows,
        native_scroll_windows_to_macos: runtime.input.native_scroll_windows_to_macos,
        block_switch_on_press: runtime.input.block_switch_on_press,
        filter_app_events: runtime.input.filter_app_events,
        key_mapping: runtime.input.key_mapping.clone(),
        cursor_mode: runtime.input.cursor_mode,
    };
    let clipboard = ClipboardRuntimeOptions {
        path: config.clipboard.path,
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
        notifications_enabled: config.notifications.enabled,
        input_backend_generation: 0,
        device_name: config.device.device_name.clone(),
        instance_name: instance_name.clone(),
        discovery: config.discovery.clone(),
        input: input.clone(),
        clipboard: clipboard.clone(),
    };

    Ok(RuntimeOptions {
        connection,
        bluetooth_enabled: runtime.bluetooth_enabled,
        instance_name,
        clipboard_mode: runtime.clipboard_mode,
        audio_mode: runtime.audio_mode,
        audio_layout: runtime.audio_layout,
        input_mode: runtime.input.mode,
        input,
        notifications_enabled: config.notifications.enabled,
        discovery: config.discovery.clone(),
        clipboard,
        transfer_limits: config.transfer.to_limits()?,
        pairing: PairingRuntimeOptions {
            headless,
            peer_query: normalize_optional_text(&runtime.peer_query),
            port: runtime.port,
            pin,
            accept: runtime.accept,
            trust_device: runtime.trust_device,
            trusted_only: runtime.trusted_only,
            discovery_secs: DEFAULT_DISCOVERY_SECS,
            known_peer: None,
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
            "join 模式要求配置 peer_query, 可使用设备名, 设备 ID, IPv4:端口或 bluetooth:地址/设备UUID"
        ),
    }
}

fn normalize_optional_text(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ClipboardConfig, DeviceConfig, DiscoveryConfig, GuiState, NotificationConfig,
        TransferConfig, UiConfig,
    };
    use crate::input::ScreenEdge;
    use uuid::Uuid;

    #[test]
    fn runtime_options_map_complete_config() {
        let mut config = test_config();
        config.runtime.connection = Some(ConnectionPreference::Join);
        config.runtime.instance_name = " worker-a ".to_string();
        config.runtime.peer_query = " demo-device ".to_string();
        config.runtime.clipboard_mode = ClipboardMode::Receive;
        config.runtime.audio_mode = AudioMode::Send;
        config.runtime.input.mode = InputMode::Send;
        config.runtime.input.edge = ScreenEdge::Left;
        config.runtime.accept = true;
        config.runtime.trust_device = true;
        config.runtime.trusted_only = true;
        config.runtime.bluetooth_enabled = true;

        let options =
            runtime_options_from_config(&config, Some("123456".to_string()), false).unwrap();

        assert_eq!(options.connection, ConnectionPreference::Join);
        assert_eq!(options.instance_name.as_deref(), Some("worker-a"));
        assert_eq!(options.pairing.peer_query.as_deref(), Some("demo-device"));
        assert_eq!(options.pairing.pin.as_deref(), Some("123456"));
        assert_eq!(options.input.edge, ScreenEdge::Left);
        assert!(options.bluetooth_enabled);
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
    fn runtime_config_rejects_missing_role() {
        let config = test_config();
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
            update: crate::config::UpdateConfig::default(),
            gui_state: GuiState::default(),
            runtime: RuntimeConfig::default(),
            trusted_devices: Vec::new(),
            preferred_active: None,
        }
    }
}
