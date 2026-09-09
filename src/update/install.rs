use super::state::InstallOutcome;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

pub fn apply_archive(archive: &Path) -> Result<InstallOutcome> {
    let exe = std::env::current_exe().context("无法确定当前可执行文件")?;
    #[cfg(target_os = "macos")]
    {
        if let Some(bundle) = super::macos::bundle_root(&exe) {
            super::macos::handoff_replace(archive, &bundle, std::process::id())?;
            return Ok(InstallOutcome::HandedOff);
        }
        super::macos::open_dmg(archive)?;
        return Ok(InstallOutcome::DmgOpened);
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

pub fn cleanup_old_binary() {
    if let Ok(exe) = std::env::current_exe() {
        let backup = backup_path(&exe);
        let _ = fs::remove_file(&backup);
    }
    #[cfg(target_os = "macos")]
    super::macos::cleanup_stale();
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
    rename_or_copy(current, &backup).context("无法备份当前程序")?;
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

fn backup_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
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
