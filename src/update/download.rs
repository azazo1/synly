use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_LENGTH, RANGE};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};

pub struct DownloadProgress {
    pub received: u64,
    pub total: Option<u64>,
}

pub async fn download_sha256sums(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .header("Accept", "application/octet-stream")
        .send()
        .await
        .context("下载 SHA256SUMS 失败")?;
    let status = response.status();
    if status.as_u16() == 403 || status.as_u16() == 429 {
        bail!("GitHub API 速率限制, 请稍后再试");
    }
    if !status.is_success() {
        bail!("下载 SHA256SUMS 失败: HTTP {status}");
    }
    response.text().await.context("读取 SHA256SUMS 失败")
}

pub fn expected_sha256(sums: &str, file_name: &str) -> Result<[u8; 32]> {
    for raw_line in sums.split('\n') {
        let line = raw_line.trim().trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (hash, name) = split_sum_line(line)
            .with_context(|| format!("无法解析 SHA256SUMS 行: {line}"))?;
        if name == file_name {
            return parse_sha256(hash);
        }
    }
    bail!("SHA256SUMS 中没有 {file_name}");
}

fn split_sum_line(line: &str) -> Option<(&str, &str)> {
    let (hash, rest) = line.split_once(char::is_whitespace)?;
    let name = rest.trim().trim_start_matches('*').trim();
    if hash.is_empty() || name.is_empty() {
        return None;
    }
    Some((hash, name))
}

fn parse_sha256(value: &str) -> Result<[u8; 32]> {
    if value.len() != 64 || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("无效的 SHA256 摘要");
    }
    let mut out = [0u8; 32];
    for (index, chunk) in value.as_bytes().chunks(2).enumerate() {
        out[index] = u8::from_str_radix(std::str::from_utf8(chunk)?, 16)?;
    }
    Ok(out)
}

pub async fn download_archive(
    client: &reqwest::Client,
    url: &str,
    part_path: &Path,
    expected: [u8; 32],
    cancel: Arc<AtomicBool>,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<()> {
    if let Some(parent) = part_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut existing = if part_path.exists() {
        fs::metadata(part_path).await?.len()
    } else {
        0
    };
    let mut request = client.get(url).header("Accept", "application/octet-stream");
    if existing > 0 {
        request = request.header(RANGE, format!("bytes={existing}-"));
    }
    let mut response = request.send().await.context("下载更新包失败")?;
    let status = response.status();
    if status.as_u16() == 403 || status.as_u16() == 429 {
        bail!("GitHub API 速率限制, 请稍后再试");
    }
    let resume = existing > 0 && status.as_u16() == 206;
    if existing > 0 && !resume {
        if part_path.exists() {
            fs::remove_file(part_path).await.ok();
        }
        existing = 0;
        response = client
            .get(url)
            .header("Accept", "application/octet-stream")
            .send()
            .await
            .context("重新下载更新包失败")?;
        if !response.status().is_success() {
            bail!("下载更新包失败: HTTP {}", response.status());
        }
    } else if !resume && !status.is_success() {
        bail!("下载更新包失败: HTTP {status}");
    }
    let total = response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(|len| existing + len);
    let file = OpenOptions::new()
        .create(true)
        .append(existing > 0)
        .write(true)
        .truncate(existing == 0)
        .open(part_path)
        .await
        .with_context(|| format!("无法写入 {}", part_path.display()))?;
    let mut writer = BufWriter::new(file);
    let mut received = existing;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        if cancel.load(Ordering::Acquire) {
            writer.flush().await.ok();
            bail!("已取消下载");
        }
        let chunk = chunk.context("读取更新包分片失败")?;
        writer.write_all(&chunk).await?;
        received += chunk.len() as u64;
        on_progress(DownloadProgress {
            received,
            total,
        });
    }
    writer.flush().await?;
    drop(writer);
    let digest = sha256_file(part_path).await?;
    if !digest.eq_ignore_ascii_case(&hex_encode(&expected)) {
        fs::remove_file(part_path).await.ok();
        bail!("更新包 SHA256 校验失败");
    }
    Ok(())
}

pub async fn sha256_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)
        .await
        .with_context(|| format!("读取 {} 失败", path.display()))?;
    Ok(hex_encode(&Sha256::digest(bytes)))
}

pub fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_marker_and_crlf() {
        let sums = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef *synly-0.8.0-linux-x86_64.tar.gz\r\n";
        let hash = expected_sha256(sums, "synly-0.8.0-linux-x86_64.tar.gz").unwrap();
        assert_eq!(hex_encode(&hash), "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    }
}
