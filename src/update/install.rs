//! 安装落地: 把下载好的安装包交接给脱离进程的平台安装器.
//!
//! 安装版一律由安装器整目录落地, 应用自己只做交接: 新版新增的 dll 与资源由安装器补齐,
//! 新版移除的文件由安装器清理. 交接前写下 `apply-update.pending`, 落地结果由下次启动
//! 回显, 见 `super::pending`.

use super::form::{self, DistributionForm};
use super::pending::{self, Handoff};
use super::state::InstallOutcome;
use anyhow::{Context, Result};
#[cfg(not(target_os = "macos"))]
use anyhow::bail;
use std::fs;
use std::path::Path;
#[cfg(not(any(windows, target_os = "macos")))]
use std::path::PathBuf;
#[cfg(not(target_os = "macos"))]
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// 把安装包交接给平台安装器.
pub fn apply_installer(archive: &Path, version: &str) -> Result<InstallOutcome> {
    let update_dir = crate::paths::update_dir()?;
    fs::create_dir_all(&update_dir)
        .with_context(|| format!("无法创建更新目录 {}", update_dir.display()))?;
    let log_path = pending::log_path(&update_dir);
    match form::effective_form() {
        DistributionForm::Installer => {}
        DistributionForm::Portable => return handoff_portable(archive),
    }
    write_pending(archive, version, &update_dir, &log_path)?;
    #[cfg(windows)]
    {
        handoff_windows(archive, &log_path, &update_dir)?;
    }
    #[cfg(target_os = "macos")]
    {
        let exe = std::env::current_exe().context("无法确定当前可执行文件")?;
        let bundle = super::macos::bundle_root(&exe).context("当前程序不在 app bundle 内")?;
        super::macos::handoff_replace(archive, &bundle, std::process::id())?;
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        handoff_linux(archive, version, &update_dir, &log_path)?;
    }
    Ok(InstallOutcome::HandedOff)
}

/// 交接前写下本次落地记录, 供下次启动判断结果.
fn write_pending(
    archive: &Path,
    version: &str,
    update_dir: &Path,
    log_path: &Path,
) -> Result<()> {
    let package = archive
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let time_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default();
    let handoff = Handoff {
        version: version.to_string(),
        form: form::effective_form().as_str().to_string(),
        package,
        time_unix,
        log_path: log_path.to_path_buf(),
    };
    handoff.write(update_dir)?;
    tracing::info!(
        version,
        form = handoff.form,
        package = handoff.package,
        log = %log_path.display(),
        "已写下更新交接标记"
    );
    Ok(())
}

/// 非安装位置运行的副本只能手动更新.
#[allow(unused_variables)]
fn handoff_portable(archive: &Path) -> Result<InstallOutcome> {
    #[cfg(target_os = "macos")]
    {
        // 直接运行二进制时没有可替换的 bundle, 退回引导用户手动拖拽安装.
        super::macos::open_dmg(archive)?;
        Ok(InstallOutcome::DmgOpened)
    }
    #[cfg(not(target_os = "macos"))]
    {
        tracing::warn!("当前程序不在安装位置, 无法就地升级");
        bail!(
            "当前程序不在安装位置, 自动更新无法替换程序文件. 请从 Release 页下载安装包重新安装."
        );
    }
}

#[cfg(windows)]
fn handoff_windows(archive: &Path, log_path: &Path, update_dir: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // /SP- 不能省: /VERYSILENT 不会去掉安装器开头的确认提示.
    // 不加 /FORCECLOSEAPPLICATIONS, /RESTARTAPPLICATIONS 与 /DIR, 重新拉起应用由安装器
    // 的 postinstall 项负责.
    let log_arg = format!("/LOG={}", log_path.display());
    let mut command = Command::new(archive);
    command
        .arg("/SP-")
        .arg("/VERYSILENT")
        .arg("/SUPPRESSMSGBOXES")
        .arg("/NORESTART")
        .arg("/CLOSEAPPLICATIONS")
        .arg(log_arg)
        .current_dir(update_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    let child = command
        .spawn()
        .with_context(|| format!("无法启动安装程序 {}", archive.display()))?;
    tracing::info!(pid = child.id(), installer = %archive.display(), "已启动 Windows 安装程序");
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn handoff_linux(
    archive: &Path,
    version: &str,
    update_dir: &Path,
    log_path: &Path,
) -> Result<()> {
    let staging = update_dir.join(format!("staging-{}", version.trim_start_matches('v')));
    if staging.exists() {
        fs::remove_dir_all(&staging)
            .with_context(|| format!("无法清理旧的暂存目录 {}", staging.display()))?;
    }
    fs::create_dir_all(&staging)
        .with_context(|| format!("无法创建暂存目录 {}", staging.display()))?;
    unpack_tar_gz(archive, &staging)?;
    let script = staging.join("install.sh");
    if !script.is_file() {
        bail!("更新包缺少 install.sh: {}", archive.display());
    }
    if !staging.join("payload").is_dir() {
        bail!("更新包缺少 payload 目录: {}", archive.display());
    }
    let home = crate::path_expand::home_dir().context("无法确定用户主目录")?;
    let prefix = Path::new(&home).join(".local");
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .with_context(|| format!("无法打开更新日志 {}", log_path.display()))?;
    let error_file = log_file.try_clone().context("无法复制更新日志句柄")?;
    let mut command = Command::new("bash");
    command
        .arg(&script)
        .arg("--silent")
        .arg("--wait-pid")
        .arg(std::process::id().to_string())
        .arg("--prefix")
        .arg(&prefix)
        .arg("--log")
        .arg(log_path)
        .arg("--result-file")
        .arg(pending::result_path(update_dir))
        .current_dir(update_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(error_file));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command
        .spawn()
        .with_context(|| format!("无法启动安装脚本 {}", script.display()))?;
    tracing::info!(pid = child.id(), script = %script.display(), "已启动 Linux 安装脚本");
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn unpack_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(dest)
        .with_context(|| format!("解压 {} 失败", archive.display()))
}

/// 清理上次更新遗留的让位文件, 返回是否仍有文件被占用.
///
/// 安装器覆盖程序文件前会把被占用的旧文件改成 `<name>.old` 让位; 只要删不掉这些让位
/// 文件, 就说明仍有进程映射着更新前的映像, Windows 上通常是 SYSTEM 输入服务. 每次调用
/// 都会顺带清理能删掉的残留.
pub fn cleanup_stale_artifacts() -> bool {
    let mut in_use = false;
    if let Some(root) = form::installer_root()
        && let Ok(entries) = fs::read_dir(&root)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_displaced_name(&name) {
                continue;
            }
            if fs::remove_file(&path).is_err() {
                tracing::debug!(file = %path.display(), "旧程序文件仍被占用, 留待下次清理");
                in_use = true;
            }
        }
    }
    #[cfg(target_os = "macos")]
    super::macos::cleanup_stale();
    in_use
}

/// 判断文件名是否是安装器让位时留下的备份.
fn is_displaced_name(name: &str) -> bool {
    match name.rsplit_once(".old") {
        Some((_, suffix)) => suffix.is_empty() || suffix.starts_with('.'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_displaced_backup_names() {
        assert!(is_displaced_name("synly.exe.old"));
        assert!(is_displaced_name("SDL2.dll.old.1234"));
        assert!(!is_displaced_name("synly.exe"));
        assert!(!is_displaced_name("old"));
    }
}
