use crate::path_expand::expand_config_path_string;
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
const DEFAULT_CLIPBOARD_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SynlyConfig {
    pub device: DeviceConfig,
    #[serde(default)]
    pub clipboard: ClipboardConfig,
    #[serde(default)]
    pub trusted_devices: Vec<TrustedDeviceConfig>,
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
    pub cache_dir: Option<PathBuf>,
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
            cache_dir: None,
        }
    }
}

impl SynlyConfig {
    pub fn load_or_create() -> Result<Self> {
        Self::load_or_create_in_dir(&config_dir()?)
    }

    pub fn clipboard_cache_dir(&self) -> Result<PathBuf> {
        let base_dir = config_dir()?;
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

    fn load_or_create_in_dir(dir: &Path) -> Result<Self> {
        let path = config_path_in(dir);
        if path.exists() {
            return load_config_from_path(&path);
        }

        let config = if let Some(device) = load_legacy_device_from_dir(dir)? {
            Self {
                device,
                clipboard: ClipboardConfig::default(),
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
    #[cfg(windows)]
    {
        if let Ok(appdata) = env::var("APPDATA") {
            return Ok(Path::new(&appdata).join("synly"));
        }
        if let Ok(home) = env::var("USERPROFILE") {
            return Ok(Path::new(&home)
                .join("AppData")
                .join("Roaming")
                .join("synly"));
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = env::var("HOME") {
            return Ok(Path::new(&home)
                .join("Library")
                .join("Application Support")
                .join("synly"));
        }
    }

    if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
        return Ok(Path::new(&xdg).join("synly"));
    }

    if let Ok(home) = env::var("HOME") {
        return Ok(Path::new(&home).join(".config").join("synly"));
    }

    bail!("unable to determine config directory")
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
        ClipboardConfig, DeviceConfig, SynlyConfig, TrustedDeviceConfig, config_path_in,
        legacy_device_config_path_in, resolve_configured_path,
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
        assert_eq!(config.clipboard, ClipboardConfig::default());
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
        assert!(config.trusted_devices.is_empty());

        cleanup_dir(&dir);
    }

    #[test]
    fn custom_relative_cache_dir_is_resolved_under_config_dir() {
        let dir = unique_test_dir("cache-relative");
        fs::create_dir_all(&dir).unwrap();
        let path = config_path_in(&dir);
        let toml = format!(
            "[device]\ndevice_id = \"{}\"\ndevice_name = \"demo\"\n\n[clipboard]\nmax_file_bytes = 42\ncache_dir = \"custom-cache\"\n",
            Uuid::new_v4()
        );
        fs::write(&path, toml).unwrap();

        let config = SynlyConfig::load_or_create_in_dir(&dir).unwrap();
        assert!(config.device.identity_private_key.is_some());
        assert!(config.device.identity_public_key.is_some());
        assert_eq!(config.clipboard.max_file_bytes, 42);
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
    fn generated_config_contains_identity_keypair() {
        let config = SynlyConfig::new_generated();
        assert!(config.device.identity_private_key.is_some());
        assert!(config.device.identity_public_key.is_some());
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("synly-config-test-{label}-{}", Uuid::new_v4()))
    }

    fn cleanup_dir(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}
