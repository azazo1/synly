use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub fn bundle_root(exe: &Path) -> Option<PathBuf> {
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()?.to_str()? != "MacOS" {
        return None;
    }
    let contents = macos_dir.parent()?;
    if contents.file_name()?.to_str()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?;
    if bundle.extension()?.to_str()? != "app" {
        return None;
    }
    Some(bundle.to_path_buf())
}

pub fn handoff_replace(dmg: &Path, bundle: &Path, pid: u32) -> Result<()> {
    let update_dir = crate::paths::update_dir()?;
    fs::create_dir_all(&update_dir)?;
    let script_path = update_dir.join("apply-update.sh");
    let log_path = update_dir.join("apply-update.log");
    let result_path = update_dir.join("apply-update-result.txt");
    let script = render_script(pid, bundle, dmg, &result_path, &log_path);
    fs::write(&script_path, script)
        .with_context(|| format!("无法写入 {}", script_path.display()))?;
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))?;
    let mut command = Command::new(&script_path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    command
        .spawn()
        .with_context(|| format!("无法启动替换脚本 {}", script_path.display()))?;
    Ok(())
}

pub fn open_dmg(dmg: &Path) -> Result<()> {
    let status = Command::new("open")
        .arg(dmg)
        .status()
        .context("无法打开 dmg")?;
    if !status.success() {
        bail!("打开 dmg 失败");
    }
    Ok(())
}

pub fn take_apply_result() -> Option<String> {
    let path = crate::paths::update_dir().ok()?.join("apply-update-result.txt");
    let text = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub fn cleanup_stale() {
    let Ok(update_dir) = crate::paths::update_dir() else {
        return;
    };
    let _ = fs::remove_file(update_dir.join("apply-update.sh"));
    if let Ok(entries) = fs::read_dir(&update_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|value| value.to_str()).unwrap_or("");
            if name.starts_with("mount-") {
                let _ = Command::new("hdiutil").args(["detach", &path.to_string_lossy()]).status();
                let _ = fs::remove_dir_all(&path);
            }
        }
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(bundle) = bundle_root(&exe)
    {
        let backup = bundle.with_extension("app.old");
        let _ = fs::remove_dir_all(backup);
    }
}

fn render_script(pid: u32, bundle: &Path, dmg: &Path, result: &Path, log: &Path) -> String {
    format!(
        r#"#!/bin/bash
set -euo pipefail
trap '' HUP
exec >>{log} 2>&1
echo "[apply-update] start pid={pid}"
old_pid={pid}
bundle={bundle}
dmg={dmg}
result={result}
fail() {{
  printf '%s\n' "$1" > "$result"
  if [[ -d "$bundle" ]]; then
    open "$bundle" >/dev/null 2>&1 || true
  fi
  exit 1
}}
alive=1
for _ in $(seq 1 60); do
  if ! kill -0 "$old_pid" >/dev/null 2>&1; then
    alive=0
    break
  fi
  sleep 1
done
if [[ "$alive" -eq 1 ]]; then
  fail '旧进程未按时退出, 请稍后重新检查更新'
fi
parent="$(dirname "$bundle")"
if [[ ! -w "$parent" ]]; then
  fail '应用目录不可写, 请手动打开 dmg 拖拽安装'
fi
mount_point="$(mktemp -d "$parent/mount-XXXXXX")"
cleanup_mount() {{
  hdiutil detach "$mount_point" >/dev/null 2>&1 || true
  rm -rf "$mount_point"
}}
trap cleanup_mount EXIT
hdiutil attach -nobrowse -readonly -mountpoint "$mount_point" "$dmg" >/dev/null || fail '无法挂载更新镜像, 请手动打开 dmg 拖拽安装'
app_source="$(find "$mount_point" -maxdepth 1 -name '*.app' -type d | head -n 1)"
if [[ -z "$app_source" ]]; then
  fail '更新镜像中没有应用包, 请手动打开 dmg 拖拽安装'
fi
staging="$parent/$(basename "$bundle").new"
rm -rf "$staging"
ditto "$app_source" "$staging" || fail '复制新应用失败, 请手动打开 dmg 拖拽安装'
xattr -dr com.apple.quarantine "$staging" >/dev/null 2>&1 || true
backup="${{bundle}}.old"
rm -rf "$backup"
mv "$bundle" "$backup" || fail '无法让出当前应用, 请手动打开 dmg 拖拽安装'
if ! mv "$staging" "$bundle"; then
  mv "$backup" "$bundle" || true
  fail '替换应用失败, 请手动打开 dmg 拖拽安装'
fi
rm -rf "$backup"
trap - EXIT
cleanup_mount
open "$bundle" >/dev/null 2>&1 || true
echo "[apply-update] completed"
"#
        ,
        pid = pid,
        bundle = shell_quote(bundle),
        dmg = shell_quote(dmg),
        result = shell_quote(result),
        log = shell_quote(log),
    )
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
