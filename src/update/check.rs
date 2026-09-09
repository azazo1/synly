use super::state::AvailableRelease;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const GITHUB_OWNER: &str = "azazo1";
pub const GITHUB_REPO: &str = "synly";
pub const APP_NAME: &str = "synly";

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
    body: Option<String>,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

pub fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

pub fn current_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "unknown"
    }
}

pub fn archive_extension() -> &'static str {
    if cfg!(target_os = "macos") {
        "dmg"
    } else if cfg!(windows) {
        "zip"
    } else {
        "tar.gz"
    }
}

pub fn archive_name_for(tag: &str, platform: &str, arch: &str, ext: &str) -> String {
    format!("{APP_NAME}-{}-{platform}-{arch}.{ext}", strip_v_prefix(tag))
}

pub fn normalize_semver(display: &str) -> Option<semver::Version> {
    let trimmed = display.trim();
    if trimmed.is_empty() || trimmed == "dev-build" {
        return None;
    }
    let mut value = trimmed.strip_prefix('v').unwrap_or(trimmed);
    if let Some((base, suffix)) = value.rsplit_once(['-', '^'])
        && suffix.len() == 7
        && suffix.chars().all(|c| c.is_ascii_hexdigit())
    {
        value = base;
    }
    semver::Version::parse(value).ok()
}

pub fn is_newer(current: &str, candidate_tag: &str) -> bool {
    match (normalize_semver(current), normalize_semver(candidate_tag)) {
        (Some(current), Some(candidate)) => candidate > current,
        _ => false,
    }
}

pub fn latest_url() -> String {
    format!("https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases/latest")
}

pub fn release_page_url(tag: &str) -> String {
    format!("https://github.com/{GITHUB_OWNER}/{GITHUB_REPO}/releases/tag/{tag}")
}

pub async fn fetch_latest(
    client: &reqwest::Client,
    current_version: &str,
) -> Result<Option<AvailableRelease>> {
    let response = client
        .get(latest_url())
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("查询 GitHub latest release 失败")?;
    let status = response.status();
    if status.as_u16() == 403 || status.as_u16() == 429 {
        bail!("GitHub API 速率限制, 请稍后再试");
    }
    if status.as_u16() == 404 {
        return Ok(None);
    }
    if !status.is_success() {
        bail!("查询更新失败: HTTP {status}");
    }
    let release: GithubRelease = response.json().await.context("解析 GitHub release 失败")?;
    if !is_newer(current_version, &release.tag_name) {
        return Ok(None);
    }
    let archive_name = archive_name_for(
        &release.tag_name,
        current_platform(),
        current_arch(),
        archive_extension(),
    );
    let archive = release
        .assets
        .iter()
        .find(|asset| asset.name == archive_name)
        .with_context(|| format!("当前平台没有匹配的更新包: {archive_name}"))?;
    let checksums = release
        .assets
        .iter()
        .find(|asset| asset.name == "SHA256SUMS")
        .context("当前 release 缺少 SHA256SUMS")?;
    Ok(Some(AvailableRelease {
        display: release.tag_name.clone(),
        tag: release.tag_name,
        notes: release.body.unwrap_or_default(),
        html_url: release.html_url,
        archive_name,
        archive_url: archive.browser_download_url.clone(),
        checksums_url: checksums.browser_download_url.clone(),
    }))
}

fn strip_v_prefix(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_build_never_updates() {
        assert!(!is_newer("dev-build", "v1.2.3"));
        assert!(normalize_semver("dev-build").is_none());
    }

    #[test]
    fn commit_suffix_does_not_count_as_newer() {
        assert!(!is_newer("v0.8.0-a1b2c3d", "v0.8.0"));
        assert!(is_newer("v0.8.0-a1b2c3d", "v0.8.1"));
        assert!(is_newer("v0.8.0^a1b2c3d", "v0.9.0"));
    }

    #[test]
    fn archive_name_strips_v_prefix() {
        assert_eq!(
            archive_name_for("v0.8.0", "macos", "aarch64", "dmg"),
            "synly-0.8.0-macos-aarch64.dmg"
        );
    }
}
