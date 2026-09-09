use super::state::InstallOutcome;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
#[cfg(not(target_os = "macos"))]
use std::path::PathBuf;

pub fn apply_archive(archive: &Path) -> Result<InstallOutcome> {
    let exe = std::env::current_exe().context("无法确定当前可执行文件")?;
    #[cfg(target_os = "macos")]
    {
        if let Some(bundle) = super::macos::bundle_root(&exe) {
            super::macos::handoff_replace(archive, &bundle, std::process::id())?;
            Ok(InstallOutcome::HandedOff)
        } else {
            super::macos::open_dmg(archive)?;
            Ok(InstallOutcome::DmgOpened)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let unpacked = unpack(archive)?;
        let new_binary = find_binary(&unpacked)?;
        replace_executable(&exe, &new_binary)?;
        let _ = fs::remove_dir_all(unpacked);
        Ok(InstallOutcome::ReadyToRestart { exe })
    }
}

/// 清理上次更新遗留的旧可执行文件, 返回它是否仍被占用.
///
/// 更新时旧映像被改名为 `<exe>.old`; 只要删不掉它, 就说明还有进程映射着旧映像,
/// 通常是 SYSTEM 输入服务仍在运行更新前的版本.
pub fn cleanup_old_binary() -> bool {
    let mut in_use = false;
    if let Ok(exe) = std::env::current_exe() {
        let prefix = backup_name_prefix(&exe).to_string_lossy().into_owned();
        let unique_prefix = format!("{prefix}.");
        if let Some(parent) = exe.parent()
            && let Ok(entries) = fs::read_dir(parent)
        {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name != prefix && !name.starts_with(&unique_prefix) {
                    continue;
                }
                if fs::remove_file(entry.path()).is_err() {
                    tracing::debug!(file = %entry.path().display(), "旧可执行文件仍被占用, 留待下次清理");
                    in_use = true;
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    super::macos::cleanup_stale();
    in_use
}

#[cfg(not(target_os = "macos"))]
fn unpack(archive: &Path) -> Result<PathBuf> {
    let parent = archive.parent().unwrap_or(Path::new("."));
    let unpack_dir = parent.join("extract");
    if unpack_dir.exists() {
        fs::remove_dir_all(&unpack_dir)?;
    }
    fs::create_dir_all(&unpack_dir)?;
    let name = archive.file_name().and_then(|value| value.to_str()).unwrap_or("");
    if name.ends_with(".tar.gz") {
        unpack_tar_gz(archive, &unpack_dir)?;
    } else if name.ends_with(".zip") {
        unpack_zip(archive, &unpack_dir)?;
    } else {
        anyhow::bail!("不支持的更新包格式: {name}");
    }
    Ok(unpack_dir)
}

#[cfg(not(target_os = "macos"))]
fn unpack_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    tar.unpack(dest)
        .with_context(|| format!("解压 {} 失败", archive.display()))
}

#[cfg(not(target_os = "macos"))]
fn unpack_zip(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("打开 {} 失败", archive.display()))?;
    zip.extract(dest)
        .with_context(|| format!("解压 {} 失败", archive.display()))
}

#[cfg(not(target_os = "macos"))]
fn find_binary(root: &Path) -> Result<PathBuf> {
    let expected = if cfg!(windows) { "synly.exe" } else { "synly" };
    if root.join(expected).is_file() {
        return Ok(root.join(expected));
    }
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry?;
        if entry.file_type().is_file() && entry.file_name() == expected {
            return Ok(entry.path().to_path_buf());
        }
    }
    anyhow::bail!("更新包中没有 {expected}");
}

#[cfg(not(target_os = "macos"))]
fn replace_executable(current: &Path, new_binary: &Path) -> Result<()> {
    let backup = backup_path(current);
    if backup.exists() {
        let _ = fs::remove_file(&backup);
    }
    // 旧备份可能仍被上一版进程占用 (例如 SYSTEM 输入服务还映射着旧映像),
    // 这时换一个唯一名字让位, 避免本次更新因为无法备份而失败.
    let backup = match rename_or_copy(current, &backup) {
        Ok(()) => backup,
        Err(_) => {
            let fallback = unique_backup_path(current);
            rename_or_copy(current, &fallback).context("无法备份当前程序")?;
            fallback
        }
    };
    if let Err(error) = rename_or_copy(new_binary, current) {
        let _ = rename_or_copy(&backup, current);
        return Err(error).context("无法安装新程序");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(current, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn backup_name_prefix(exe: &Path) -> std::ffi::OsString {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    name
}

#[cfg(not(target_os = "macos"))]
fn backup_path(exe: &Path) -> PathBuf {
    exe.with_file_name(backup_name_prefix(exe))
}

#[cfg(not(target_os = "macos"))]
fn unique_backup_path(exe: &Path) -> PathBuf {
    let mut name = backup_name_prefix(exe);
    name.push(format!(".{}", std::process::id()));
    exe.with_file_name(name)
}

#[cfg(not(target_os = "macos"))]
fn rename_or_copy(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(from, to)?;
            fs::remove_file(from)?;
            Ok(())
        }
    }
}
