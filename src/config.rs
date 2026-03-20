use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceConfig {
    pub device_id: Uuid,
    pub device_name: String,
}

impl DeviceConfig {
    pub fn load_or_create() -> Result<Self> {
        let path = device_config_path()?;

        if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read device config at {}", path.display()))?;
            let config: DeviceConfig = serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse device config at {}", path.display()))?;
            return Ok(config);
        }

        let device_id = Uuid::new_v4();
        let device_name = detect_device_name(device_id);
        let config = DeviceConfig {
            device_id,
            device_name,
        };

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config dir {}", parent.display()))?;
        } else {
            bail!("invalid config path {}", path.display());
        }

        let pretty = serde_json::to_string_pretty(&config)?;
        fs::write(&path, pretty)
            .with_context(|| format!("failed to write device config at {}", path.display()))?;

        Ok(config)
    }

    pub fn short_id(&self) -> String {
        self.device_id.to_string().chars().take(8).collect()
    }
}

fn device_config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("device.json"))
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
