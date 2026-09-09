use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_LENGTH, RANGE};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio_util::sync::CancellationToken;

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
    cancel: CancellationToken,
    mut on_progress: impl FnMut(DownloadProgress),
) -> Result<()> {
    if cancel.is_cancelled() {
        bail!("已取消下载");
    }
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
    let mut response = send_cancellable(request, &cancel, "下载更新包失败").await?;
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
        response = send_cancellable(
            client.get(url).header("Accept", "application/octet-stream"),
            &cancel,
            "重新下载更新包失败",
        )
        .await?;
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
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                writer.flush().await.ok();
                bail!("已取消下载");
            }
            chunk = stream.next() => chunk,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.context("读取更新包分片失败")?;
        writer.write_all(&chunk).await?;
        received += chunk.len() as u64;
        on_progress(DownloadProgress {
            received,
            total,
        });
    }
    if cancel.is_cancelled() {
        writer.flush().await.ok();
        bail!("已取消下载");
    }
    writer.flush().await?;
    drop(writer);
    let digest = tokio::select! {
        _ = cancel.cancelled() => bail!("已取消下载"),
        result = sha256_file(part_path) => result?,
    };
    if !digest.eq_ignore_ascii_case(&hex_encode(&expected)) {
        fs::remove_file(part_path).await.ok();
        bail!("更新包 SHA256 校验失败");
    }
    Ok(())
}

async fn send_cancellable(
    request: reqwest::RequestBuilder,
    cancel: &CancellationToken,
    context: &'static str,
) -> Result<reqwest::Response> {
    tokio::select! {
        _ = cancel.cancelled() => bail!("已取消下载"),
        result = request.send() => result.context(context),
    }
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
    use tokio::io::AsyncWriteExt;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn parses_binary_marker_and_crlf() {
        let sums = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef *synly-0.8.0-linux-x86_64.tar.gz\r\n";
        let hash = expected_sha256(sums, "synly-0.8.0-linux-x86_64.tar.gz").unwrap();
        assert_eq!(hex_encode(&hash), "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
    }

    #[tokio::test]
    async fn cancel_stops_in_flight_download() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10485760\r\n\r\n")
                .await;
            let chunk = vec![0u8; 1024];
            loop {
                if socket.write_all(&chunk).await.is_err() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        });

        let dir = std::env::temp_dir().join(format!(
            "synly-update-cancel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let part_path = dir.join("archive.part");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let cancel = CancellationToken::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut started = Some(started_tx);
        let download = {
            let client = client.clone();
            let url = format!("http://{addr}/archive");
            let part_path = part_path.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                download_archive(
                    &client,
                    &url,
                    &part_path,
                    [0u8; 32],
                    cancel,
                    move |progress| {
                        if progress.received > 0
                            && let Some(tx) = started.take()
                        {
                            let _ = tx.send(());
                        }
                    },
                )
                .await
            })
        };

        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
            .await
            .expect("下载应开始写入")
            .expect("下载进度通道已关闭");
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), download)
            .await
            .expect("取消下载应该尽快返回")
            .expect("下载任务不应 panic");
        let error = result.expect_err("取消后应返回错误");
        assert!(
            error.to_string().contains("已取消下载"),
            "实际错误: {error}"
        );
        assert!(part_path.exists(), "取消后应保留 .part 以便续传");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
