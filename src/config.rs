use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub device_id: Uuid,
    pub device_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClipboardConfig {
    #[serde(default = "default_clipboard_max_file_bytes")]
    pub max_file_bytes: u64,
    #[serde(default)]
    pub cache_dir: Option<PathBuf>,
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

    fn load_or_create_in_dir(dir: &Path) -> Result<Self> {
        let path = config_path_in(dir);
        if path.exists() {
            return load_config_from_path(&path);
        }

        let config = if let Some(device) = load_legacy_device_from_dir(dir)? {
            Self {
                device,
                clipboard: ClipboardConfig::default(),
            }
        } else {
            Self::new_generated()
        };

        write_config_to_path(&path, &config)?;
        Ok(config)
    }

    fn new_generated() -> Self {
        let device_id = Uuid::new_v4();
        Self {
            device: DeviceConfig {
                device_id,
                device_name: detect_device_name(device_id),
            },
            clipboard: ClipboardConfig::default(),
        }
    }
}

impl DeviceConfig {
    pub fn short_id(&self) -> String {
        self.device_id.to_string().chars().take(8).collect()
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
    toml::from_str(&raw).with_context(|| format!("failed to parse config at {}", path.display()))
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

fn resolve_configured_path(path: &Path, base_dir: &Path) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("configured path cannot be empty");
    }

    let with_env = expand_env_vars(trimmed)?;
    let expanded = expand_tilde(&with_env)?;
    let path = PathBuf::from(expanded);
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

fn expand_tilde(raw: &str) -> Result<String> {
    if !raw.starts_with('~') {
        return Ok(raw.to_string());
    }

    let rest = &raw[1..];
    if !rest.is_empty() && !rest.starts_with('/') && !rest.starts_with('\\') {
        return Ok(raw.to_string());
    }

    let home = home_dir().context("无法展开 `~`，因为当前环境没有可用的 home 目录")?;
    Ok(format!("{}{}", home.display(), rest))
}

fn expand_env_vars(raw: &str) -> Result<String> {
    let mut result = String::with_capacity(raw.len());
    let chars: Vec<char> = raw.chars().collect();
    let mut index = 0usize;

    while index < chars.len() {
        if chars[index] == '$' {
            if index + 1 < chars.len() && chars[index + 1] == '{' {
                let mut end = index + 2;
                while end < chars.len() && chars[end] != '}' {
                    end += 1;
                }
                if end >= chars.len() {
                    bail!("环境变量引用缺少右花括号: {}", raw);
                }
                let key = chars[index + 2..end].iter().collect::<String>();
                result.push_str(&env::var(&key).unwrap_or_default());
                index = end + 1;
                continue;
            }

            let mut end = index + 1;
            while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
                end += 1;
            }
            if end == index + 1 {
                result.push(chars[index]);
                index += 1;
                continue;
            }
            let key = chars[index + 1..end].iter().collect::<String>();
            result.push_str(&env::var(&key).unwrap_or_default());
            index = end;
            continue;
        }

        result.push(chars[index]);
        index += 1;
    }

    Ok(result)
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        if let Ok(profile) = env::var("USERPROFILE") {
            return Some(PathBuf::from(profile));
        }
    }

    env::var("HOME").ok().map(PathBuf::from)
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
        ClipboardConfig, DeviceConfig, SynlyConfig, config_path_in, legacy_device_config_path_in,
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
        assert_eq!(config.clipboard, ClipboardConfig::default());

        cleanup_dir(&dir);
    }

    #[test]
    fn load_or_create_in_dir_migrates_legacy_json() {
        let dir = unique_test_dir("migrate");
        fs::create_dir_all(&dir).unwrap();

        let legacy_device = DeviceConfig {
            device_id: Uuid::new_v4(),
            device_name: "legacy-device".to_string(),
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
        assert_eq!(config.clipboard, ClipboardConfig::default());
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
        assert_eq!(config.clipboard, ClipboardConfig::default());

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
        assert_eq!(config.clipboard.max_file_bytes, 42);
        assert_eq!(
            config.clipboard.cache_dir,
            Some(PathBuf::from("custom-cache"))
        );

        cleanup_dir(&dir);
    }

    #[test]
    fn resolve_configured_path_joins_relative_path() {
        let base = PathBuf::from("/tmp/synly-config-base");
        let resolved = resolve_configured_path(Path::new("cache-dir"), &base).unwrap();
        assert_eq!(resolved, base.join("cache-dir"));
    }

    fn unique_test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("synly-config-test-{label}-{}", Uuid::new_v4()))
    }

    fn cleanup_dir(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}
