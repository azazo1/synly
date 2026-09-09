use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

const APP_DIR_NAME: &str = if cfg!(synly_fake_dist) {
    "synly-fake"
} else {
    "synly"
};

/// 应用数据目录, 配置, 单实例锁和更新缓存都落在这里.
///
/// `SYNLY_DATA_DIR` 可覆盖, 供 `just debug` 隔离调试实例.
pub fn data_dir() -> Result<PathBuf> {
    if let Some(path) = env_path("SYNLY_DATA_DIR") {
        return Ok(path);
    }
    let home = crate::path_expand::home_dir().context("unable to determine home directory")?;
    Ok(Path::new(&home).join(".config").join(APP_DIR_NAME))
}

/// 配置目录与数据目录相同.
pub fn config_dir() -> Result<PathBuf> {
    data_dir()
}

/// 默认日志目录. 设置 `SYNLY_LOG_FILE` 时使用其父目录.
pub fn log_dir() -> Result<PathBuf> {
    if let Some(path) = env_path("SYNLY_LOG_FILE") {
        return Ok(path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf());
    }
    if env_path("SYNLY_DATA_DIR").is_some() {
        return Ok(data_dir()?.join("logs"));
    }
    Ok(dirs::data_local_dir()
        .context("unable to determine local data directory")?
        .join(APP_DIR_NAME)
        .join("logs"))
}

/// 主日志文件路径. `SYNLY_LOG_FILE` 可覆盖.
pub fn log_file_path() -> Result<PathBuf> {
    if let Some(path) = env_path("SYNLY_LOG_FILE") {
        return Ok(path);
    }
    Ok(log_dir()?.join("synly.log"))
}

/// 自动更新下载与安装脚本目录.
pub fn update_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("update"))
}

/// 剪贴板缓存根目录. 数据目录被覆盖时跟随隔离.
pub fn cache_dir() -> Result<PathBuf> {
    if env_path("SYNLY_DATA_DIR").is_some() {
        return Ok(data_dir()?.join("cache"));
    }
    dirs::cache_dir()
        .map(|dir| dir.join(APP_DIR_NAME))
        .context("unable to determine platform cache directory")
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
