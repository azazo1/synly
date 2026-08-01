//! 从 git 仓库状态生成构建版本号的共享逻辑.
//!
//! 桌面端与 Android 端共用, 保证所有平台展示的版本号格式一致.

use std::path::{Path, PathBuf};
use std::process::Command;

/// 根据 git 状态生成构建版本号, 无 git 信息时回退到 Cargo 包版本.
///
/// 规则:
/// - 精确 tag: 直接显示该 tag, 例如 v1.2.3.
/// - 非 tag commit: 在最近 tag 后追加 - 和 6 位短 hash, 例如 v1.2.3-a1b2c3.
/// - 脏工作区: 分隔符改为 ^, 例如 v1.2.3^a1b2c3.
/// - 无可用 tag: 使用 Cargo 包版本.
pub fn build_version_string() -> String {
    let fallback = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());
    let Some(repo_root) = find_repo_root() else {
        return fallback;
    };
    let Some(describe) =
        git_output(&repo_root, &["describe", "--tags", "--always", "--abbrev=6"])
    else {
        return fallback;
    };
    let dirty = git_is_dirty(&repo_root);
    let head = git_output(&repo_root, &["rev-parse", "--short=6", "HEAD"]);
    format_version(Some(&describe), dirty, head.as_deref(), &fallback)
}

/// 纯函数形式的版本格式拼接, 便于单元测试.
pub fn format_version(
    describe: Option<&str>,
    dirty: bool,
    head: Option<&str>,
    fallback: &str,
) -> String {
    let Some(describe) = describe else {
        return fallback.to_string();
    };
    let separator = if dirty { "^" } else { "-" };
    if let Some((base, hash)) = split_describe_offset(describe) {
        return format!("{base}{separator}{hash}");
    }
    if is_hex_hash(describe) {
        let hash = &describe[..describe.len().min(6)];
        return format!("{fallback}{separator}{hash}");
    }
    if dirty
        && let Some(head) = head
    {
        return format!("{describe}^{head}");
    }
    describe.to_string()
}

/// 输出让 cargo 在 git 状态变化时重跑构建脚本的 rerun-if-changed 指令.
pub fn emit_git_rerun_if_changed() {
    let Some(repo_root) = find_repo_root() else {
        return;
    };
    let git_dir = repo_root.join(".git");
    if git_dir.is_dir() {
        for name in ["HEAD", "index", "refs"] {
            println!("cargo:rerun-if-changed={}", git_dir.join(name).display());
        }
    } else if git_dir.is_file() {
        println!("cargo:rerun-if-changed={}", git_dir.display());
    }
}

/// 从 CARGO_MANIFEST_DIR 向上查找包含 .git 的仓库根目录.
fn find_repo_root() -> Option<PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let mut dir = PathBuf::from(manifest_dir);
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// 在仓库根目录执行 git 命令并返回 trim 后的 stdout.
pub fn git_output(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_string())
}

/// 检测工作区是否有未提交改动.
pub fn git_is_dirty(repo_root: &Path) -> bool {
    match Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo_root)
        .output()
    {
        Ok(output) => !output.stdout.is_empty(),
        Err(_) => false,
    }
}

/// 从 git describe 输出解析非 tag 描述, 形如 <tag>-<N>-g<hash>.
pub fn split_describe_offset(describe: &str) -> Option<(&str, &str)> {
    let (before, hash) = describe.rsplit_once("-g")?;
    if hash.len() < 6 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let (base, count) = before.rsplit_once('-')?;
    if count.is_empty() || !count.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((base, hash))
}

fn is_hex_hash(value: &str) -> bool {
    value.len() >= 6 && value.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_tag_keeps_tag() {
        assert_eq!(format_version(Some("v1.2.3"), false, None, "0.1.0"), "v1.2.3");
    }

    #[test]
    fn non_tag_commit_appends_short_hash() {
        assert_eq!(
            format_version(Some("v1.2.3-5-ga1b2c3"), false, None, "0.1.0"),
            "v1.2.3-a1b2c3"
        );
    }

    #[test]
    fn dirty_head_uses_caret_separator() {
        assert_eq!(
            format_version(Some("v1.2.3-5-ga1b2c3"), true, None, "0.1.0"),
            "v1.2.3^a1b2c3"
        );
    }

    #[test]
    fn dirty_exact_tag_appends_head() {
        assert_eq!(
            format_version(Some("v1.2.3"), true, Some("deadbe"), "0.1.0"),
            "v1.2.3^deadbe"
        );
    }

    #[test]
    fn missing_describe_falls_back_to_package_version() {
        assert_eq!(format_version(None, false, None, "0.2.0"), "0.2.0");
    }

    #[test]
    fn bare_hash_describe_falls_back_with_hash() {
        assert_eq!(
            format_version(Some("296198"), false, None, "0.2.0"),
            "0.2.0-296198"
        );
    }
}
