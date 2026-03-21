use crate::protocol::{ClipboardFile, ClipboardImage, ClipboardPayload};
use anyhow::{Context, Result, anyhow};
use clipboard_rs::{
    Clipboard, ClipboardContent, ClipboardContext, ClipboardHandler, ClipboardWatcher,
    ClipboardWatcherContext, ContentFormat, WatcherShutdown, common::RustImage,
};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use url::Url;

const CLIPBOARD_CACHE_BATCH_PREFIX: &str = "batch-";

#[derive(Clone)]
pub struct ClipboardSync {
    state: Arc<Mutex<ClipboardSyncState>>,
    max_file_bytes: u64,
    max_cache_bytes: Option<u64>,
    cache_dir: PathBuf,
}

pub struct ClipboardWatcherHandle {
    shutdown: Option<WatcherShutdown>,
    thread: Option<thread::JoinHandle<()>>,
}

#[derive(Default)]
struct ClipboardSyncState {
    last_sent_signature: Option<String>,
    suppress_next_signature: Option<String>,
}

struct LocalClipboardHandler {
    ctx: ClipboardContext,
    state: Arc<Mutex<ClipboardSyncState>>,
    tx: mpsc::UnboundedSender<ClipboardPayload>,
    max_file_bytes: u64,
}

struct CapturedClipboard {
    payload: Option<ClipboardPayload>,
    warnings: Vec<String>,
}

impl ClipboardSync {
    pub fn new(max_file_bytes: u64, max_cache_bytes: Option<u64>, cache_dir: PathBuf) -> Self {
        Self {
            state: Arc::new(Mutex::new(ClipboardSyncState::default())),
            max_file_bytes,
            max_cache_bytes,
            cache_dir,
        }
    }

    pub fn start_local_watcher(
        &self,
        tx: mpsc::UnboundedSender<ClipboardPayload>,
    ) -> Result<ClipboardWatcherHandle> {
        let handler = LocalClipboardHandler::new(self.state.clone(), tx, self.max_file_bytes)?;
        let mut watcher: ClipboardWatcherContext<LocalClipboardHandler> =
            ClipboardWatcherContext::new().map_err(clipboard_error)?;
        let shutdown = watcher.add_handler(handler).get_shutdown_channel();
        let thread = thread::Builder::new()
            .name("synly-clipboard-watch".to_string())
            .spawn(move || watcher.start_watch())
            .context("failed to spawn clipboard watcher thread")?;

        Ok(ClipboardWatcherHandle {
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }

    pub async fn publish_initial_payload(
        &self,
        tx: &mpsc::UnboundedSender<ClipboardPayload>,
    ) -> Result<()> {
        let Some(payload) = self.read_local_payload().await? else {
            return Ok(());
        };

        if self.note_local_payload(&payload)? {
            let _ = tx.send(payload);
        }
        Ok(())
    }

    pub async fn apply_remote_payload(&self, payload: ClipboardPayload) -> Result<()> {
        let max_file_bytes = self.max_file_bytes;
        let max_cache_bytes = self.max_cache_bytes;
        let cache_dir = self.cache_dir.clone();
        let state = self.state.clone();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut warnings = Vec::new();
            let payload = sanitize_remote_payload(payload, max_file_bytes, &mut warnings);
            emit_warnings(&warnings);

            if payload.is_empty() {
                return Ok(());
            }

            let ctx = ClipboardContext::new().map_err(clipboard_error)?;
            let already_matches =
                capture_clipboard(&ctx, max_file_bytes).payload.as_ref() == Some(&payload);

            if !already_matches {
                apply_payload_to_clipboard(&ctx, &payload, &cache_dir, max_cache_bytes)?;
            }

            let signature = payload_signature(&payload);
            let mut guard = state
                .lock()
                .map_err(|_| anyhow!("clipboard sync state poisoned"))?;
            guard.note_remote_payload(signature, !already_matches);
            Ok(())
        })
        .await
        .context("clipboard apply task failed")??;

        Ok(())
    }

    fn note_local_payload(&self, payload: &ClipboardPayload) -> Result<bool> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| anyhow!("clipboard sync state poisoned"))?;
        Ok(guard.note_local_payload(payload))
    }

    async fn read_local_payload(&self) -> Result<Option<ClipboardPayload>> {
        let max_file_bytes = self.max_file_bytes;
        tokio::task::spawn_blocking(move || -> Result<Option<ClipboardPayload>> {
            let ctx = ClipboardContext::new().map_err(clipboard_error)?;
            let captured = capture_clipboard(&ctx, max_file_bytes);
            emit_warnings(&captured.warnings);
            Ok(captured.payload)
        })
        .await
        .context("clipboard read task failed")?
    }
}

impl ClipboardWatcherHandle {
    pub fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.stop();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl ClipboardSyncState {
    fn note_local_payload(&mut self, payload: &ClipboardPayload) -> bool {
        let signature = payload_signature(payload);
        if self.suppress_next_signature.as_ref() == Some(&signature) {
            self.suppress_next_signature = None;
            self.last_sent_signature = Some(signature);
            return false;
        }

        if self.last_sent_signature.as_ref() == Some(&signature) {
            return false;
        }

        self.last_sent_signature = Some(signature);
        true
    }

    fn note_remote_payload(&mut self, signature: String, applied_to_local_clipboard: bool) {
        self.last_sent_signature = Some(signature.clone());
        self.suppress_next_signature = applied_to_local_clipboard.then_some(signature);
    }
}

impl LocalClipboardHandler {
    fn new(
        state: Arc<Mutex<ClipboardSyncState>>,
        tx: mpsc::UnboundedSender<ClipboardPayload>,
        max_file_bytes: u64,
    ) -> Result<Self> {
        let ctx = ClipboardContext::new().map_err(clipboard_error)?;
        Ok(Self {
            ctx,
            state,
            tx,
            max_file_bytes,
        })
    }
}

impl ClipboardHandler for LocalClipboardHandler {
    fn on_clipboard_change(&mut self) {
        let captured = capture_clipboard(&self.ctx, self.max_file_bytes);
        emit_warnings(&captured.warnings);

        let Some(payload) = captured.payload else {
            return;
        };

        let should_send = match self.state.lock() {
            Ok(mut state) => state.note_local_payload(&payload),
            Err(_) => false,
        };
        if should_send {
            let _ = self.tx.send(payload);
        }
    }
}

fn capture_clipboard(ctx: &ClipboardContext, max_file_bytes: u64) -> CapturedClipboard {
    let mut warnings = Vec::new();
    let text = read_text_content(ctx, &mut warnings);
    let rich_text = read_rich_text_content(ctx, &mut warnings);
    let html = read_html_content(ctx, &mut warnings);
    let image = read_image_content(ctx, &mut warnings);
    let files = read_file_content(ctx, max_file_bytes, &mut warnings);

    let payload = ClipboardPayload {
        text,
        rich_text,
        html,
        image,
        files,
    };

    CapturedClipboard {
        payload: (!payload.is_empty()).then_some(payload),
        warnings,
    }
}

fn read_text_content(ctx: &ClipboardContext, warnings: &mut Vec<String>) -> Option<String> {
    if !ctx.has(ContentFormat::Text) {
        return None;
    }

    match ctx.get_text() {
        Ok(text) => Some(text),
        Err(err) => {
            warnings.push(format!("无法读取剪贴板文本: {}", err));
            None
        }
    }
}

fn read_rich_text_content(ctx: &ClipboardContext, warnings: &mut Vec<String>) -> Option<String> {
    if !ctx.has(ContentFormat::Rtf) {
        return None;
    }

    match ctx.get_rich_text() {
        Ok(text) => Some(text),
        Err(err) => {
            warnings.push(format!("无法读取剪贴板富文本: {}", err));
            None
        }
    }
}

fn read_html_content(ctx: &ClipboardContext, warnings: &mut Vec<String>) -> Option<String> {
    if !ctx.has(ContentFormat::Html) {
        return None;
    }

    match ctx.get_html() {
        Ok(html) => Some(html),
        Err(err) => {
            warnings.push(format!("无法读取剪贴板 HTML: {}", err));
            None
        }
    }
}

fn read_image_content(
    ctx: &ClipboardContext,
    warnings: &mut Vec<String>,
) -> Option<ClipboardImage> {
    if !ctx.has(ContentFormat::Image) {
        return None;
    }

    match ctx.get_image() {
        Ok(image) => match image.to_png() {
            Ok(buffer) => Some(ClipboardImage {
                png_bytes: buffer.get_bytes().to_vec(),
            }),
            Err(err) => {
                warnings.push(format!("无法将剪贴板图片编码为 PNG: {}", err));
                None
            }
        },
        Err(err) => {
            warnings.push(format!("无法读取剪贴板图片: {}", err));
            None
        }
    }
}

fn read_file_content(
    ctx: &ClipboardContext,
    max_file_bytes: u64,
    warnings: &mut Vec<String>,
) -> Vec<ClipboardFile> {
    if !ctx.has(ContentFormat::Files) {
        return Vec::new();
    }

    let raw_files = match ctx.get_files() {
        Ok(files) => files,
        Err(err) => {
            warnings.push(format!("无法读取剪贴板文件列表: {}", err));
            return Vec::new();
        }
    };

    capture_files_from_paths(raw_files, max_file_bytes, warnings)
}

fn capture_files_from_paths(
    raw_files: Vec<String>,
    max_file_bytes: u64,
    warnings: &mut Vec<String>,
) -> Vec<ClipboardFile> {
    let mut files = Vec::new();

    for raw_file in raw_files {
        let path = match parse_clipboard_path(&raw_file) {
            Ok(path) => path,
            Err(err) => {
                warnings.push(format!(
                    "已跳过剪贴板文件 `{}`: 无法解析为本地路径: {}",
                    raw_file, err
                ));
                continue;
            }
        };

        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(err) => {
                warnings.push(format!(
                    "已跳过剪贴板文件 `{}`: 无法读取元数据: {}",
                    path.display(),
                    err
                ));
                continue;
            }
        };

        if !metadata.is_file() {
            let kind = if metadata.is_dir() {
                "目录"
            } else {
                "非普通文件"
            };
            warnings.push(format!(
                "已跳过剪贴板条目 `{}`: 目前只支持同步普通文件，当前是{}",
                path.display(),
                kind
            ));
            continue;
        }

        if metadata.len() > max_file_bytes {
            warnings.push(format!(
                "已跳过剪贴板文件 `{}`: 大小 {} 超过配置上限 {}",
                path.display(),
                format_bytes(metadata.len()),
                format_bytes(max_file_bytes)
            ));
            continue;
        }

        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => {
                warnings.push(format!(
                    "已跳过剪贴板文件 `{}`: 无法读取文件内容: {}",
                    path.display(),
                    err
                ));
                continue;
            }
        };

        files.push(ClipboardFile {
            name: infer_clipboard_file_name(&path),
            bytes,
        });
    }

    files
}

fn apply_payload_to_clipboard(
    ctx: &ClipboardContext,
    payload: &ClipboardPayload,
    cache_dir: &Path,
    max_cache_bytes: Option<u64>,
) -> Result<()> {
    let mut contents = Vec::new();

    if let Some(text) = &payload.text {
        contents.push(ClipboardContent::Text(text.clone()));
    }
    if let Some(rich_text) = &payload.rich_text {
        contents.push(ClipboardContent::Rtf(rich_text.clone()));
    }
    if let Some(html) = &payload.html {
        contents.push(ClipboardContent::Html(html.clone()));
    }
    if let Some(image) = &payload.image {
        let image =
            clipboard_rs::RustImageData::from_bytes(&image.png_bytes).map_err(clipboard_error)?;
        contents.push(ClipboardContent::Image(image));
    }

    let mut file_warnings = Vec::new();
    let file_paths = write_clipboard_files_to_cache(
        cache_dir,
        &payload.files,
        max_cache_bytes,
        &mut file_warnings,
    )?;
    emit_warnings(&file_warnings);
    if !file_paths.is_empty() {
        contents.push(ClipboardContent::Files(
            file_paths
                .iter()
                .map(|path| path.to_string_lossy().to_string())
                .collect(),
        ));
    }

    if contents.is_empty() {
        return Ok(());
    }

    ctx.set(contents).map_err(clipboard_error)
}

fn write_clipboard_files_to_cache(
    cache_dir: &Path,
    files: &[ClipboardFile],
    max_cache_bytes: Option<u64>,
    warnings: &mut Vec<String>,
) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(cache_dir).with_context(|| {
        format!(
            "failed to create clipboard cache root {}",
            cache_dir.display()
        )
    })?;

    if files.is_empty() {
        prune_clipboard_cache(cache_dir, max_cache_bytes, None, warnings)?;
        return Ok(Vec::new());
    }

    let batch_dir = allocate_cache_batch_dir(cache_dir)?;
    fs::create_dir_all(&batch_dir).with_context(|| {
        format!(
            "failed to create clipboard cache directory {}",
            batch_dir.display()
        )
    })?;

    let mut written = Vec::new();
    for file in files {
        let file_name = sanitize_clipboard_file_name(&file.name);
        let path = unique_cache_path(&batch_dir, &file_name);
        match fs::write(&path, &file.bytes) {
            Ok(()) => written.push(path),
            Err(err) => warnings.push(format!(
                "无法写入剪贴板缓存文件 `{}`: {}",
                path.display(),
                err
            )),
        }
    }

    if written.is_empty() {
        let _ = fs::remove_dir_all(&batch_dir);
        prune_clipboard_cache(cache_dir, max_cache_bytes, None, warnings)?;
        return Ok(Vec::new());
    }

    prune_clipboard_cache(cache_dir, max_cache_bytes, Some(&batch_dir), warnings)?;
    Ok(written)
}

fn sanitize_remote_payload(
    mut payload: ClipboardPayload,
    max_file_bytes: u64,
    warnings: &mut Vec<String>,
) -> ClipboardPayload {
    payload.files = payload
        .files
        .into_iter()
        .filter_map(|file| {
            if u64::try_from(file.bytes.len()).ok().unwrap_or(u64::MAX) > max_file_bytes {
                warnings.push(format!(
                    "已跳过远端剪贴板文件 `{}`: 大小 {} 超过本机配置上限 {}",
                    file.name,
                    format_bytes(file.bytes.len() as u64),
                    format_bytes(max_file_bytes)
                ));
                None
            } else {
                Some(ClipboardFile {
                    name: sanitize_clipboard_file_name(&file.name),
                    bytes: file.bytes,
                })
            }
        })
        .collect();
    payload
}

fn parse_clipboard_path(raw: &str) -> Result<PathBuf> {
    if raw.starts_with("file://") {
        let url = Url::parse(raw).with_context(|| format!("invalid file URL `{raw}`"))?;
        return url
            .to_file_path()
            .map_err(|_| anyhow!("file URL `{raw}` does not map to a local path"));
    }

    Ok(PathBuf::from(raw))
}

fn infer_clipboard_file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_else(|| OsStr::new("clipboard-file"))
        .to_string_lossy()
        .to_string()
}

fn sanitize_clipboard_file_name(name: &str) -> String {
    let candidate = Path::new(name)
        .file_name()
        .unwrap_or_else(|| OsStr::new("clipboard-file"))
        .to_string_lossy()
        .trim()
        .to_string();
    if candidate.is_empty() {
        "clipboard-file".to_string()
    } else {
        candidate
    }
}

fn unique_cache_path(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .unwrap_or_else(|| OsStr::new("clipboard-file"))
        .to_string_lossy();
    let extension = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();

    for index in 2.. {
        let candidate = dir.join(format!("{stem}-{index}{extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }

    unreachable!("finite loop over candidate file names unexpectedly exhausted")
}

fn allocate_cache_batch_dir(cache_dir: &Path) -> Result<PathBuf> {
    let now_ms = unix_time_ms();
    for index in 0..10_000u32 {
        let candidate = cache_dir.join(format!(
            "{CLIPBOARD_CACHE_BATCH_PREFIX}{now_ms:020}-{index:04}"
        ));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    Err(anyhow!(
        "failed to allocate a unique clipboard cache directory under {}",
        cache_dir.display()
    ))
}

fn prune_clipboard_cache(
    cache_dir: &Path,
    max_cache_bytes: Option<u64>,
    active_dir: Option<&Path>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let Some(max_cache_bytes) = max_cache_bytes else {
        return Ok(());
    };
    if !cache_dir.exists() {
        return Ok(());
    }

    let mut batches = collect_cache_batches(cache_dir)?;
    let mut total_bytes = batches.iter().map(|batch| batch.size_bytes).sum::<u64>();
    batches.sort_by(|left, right| {
        left.created_ms
            .cmp(&right.created_ms)
            .then_with(|| left.path.cmp(&right.path))
    });

    while total_bytes > max_cache_bytes {
        let Some(index) = batches
            .iter()
            .position(|batch| active_dir != Some(batch.path.as_path()))
        else {
            warnings.push(format!(
                "剪贴板缓存当前占用 {}，仍超过配置上限 {}；已保留最新缓存。",
                format_bytes(total_bytes),
                format_bytes(max_cache_bytes)
            ));
            break;
        };

        let batch = batches.remove(index);
        fs::remove_dir_all(&batch.path).with_context(|| {
            format!(
                "failed to remove clipboard cache directory {}",
                batch.path.display()
            )
        })?;
        total_bytes = total_bytes.saturating_sub(batch.size_bytes);
        warnings.push(format!(
            "已清理较早的剪贴板缓存 `{}`，释放 {}。",
            batch.path.display(),
            format_bytes(batch.size_bytes)
        ));
    }

    Ok(())
}

#[derive(Clone, Debug)]
struct CacheBatch {
    path: PathBuf,
    created_ms: u64,
    size_bytes: u64,
}

fn collect_cache_batches(cache_dir: &Path) -> Result<Vec<CacheBatch>> {
    let mut batches = Vec::new();
    for entry in fs::read_dir(cache_dir).with_context(|| {
        format!(
            "failed to read clipboard cache root {}",
            cache_dir.display()
        )
    })? {
        let entry = entry.with_context(|| {
            format!(
                "failed to read an entry under clipboard cache root {}",
                cache_dir.display()
            )
        })?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if !metadata.is_dir() {
            continue;
        }

        let file_name = entry.file_name().to_string_lossy().to_string();
        if !file_name.starts_with(CLIPBOARD_CACHE_BATCH_PREFIX) {
            continue;
        }

        batches.push(CacheBatch {
            created_ms: metadata_modified_ms(&metadata),
            size_bytes: directory_size(&path)?,
            path,
        });
    }
    Ok(batches)
}

fn directory_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path).with_context(|| {
        format!(
            "failed to read clipboard cache directory {}",
            path.display()
        )
    })? {
        let entry =
            entry.with_context(|| format!("failed to read an entry under {}", path.display()))?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.is_dir() {
            total = total.saturating_add(directory_size(&path)?);
        } else if metadata.is_file() {
            total = total.saturating_add(metadata.len());
        }
    }
    Ok(total)
}

fn metadata_modified_ms(metadata: &fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn emit_warnings(warnings: &[String]) {
    for warning in warnings {
        eprintln!("{warning}");
    }
}

fn clipboard_error(err: Box<dyn std::error::Error + Send + Sync + 'static>) -> anyhow::Error {
    anyhow!(err.to_string())
}

fn payload_signature(payload: &ClipboardPayload) -> String {
    let mut hasher = Sha256::new();
    hash_optional_string(&mut hasher, "text", payload.text.as_deref());
    hash_optional_string(&mut hasher, "rtf", payload.rich_text.as_deref());
    hash_optional_string(&mut hasher, "html", payload.html.as_deref());

    if let Some(image) = &payload.image {
        hash_part(&mut hasher, "image");
        hasher.update(image.png_bytes.len().to_le_bytes());
        hasher.update(&image.png_bytes);
    } else {
        hash_part(&mut hasher, "image:none");
    }

    hash_part(&mut hasher, "files");
    hasher.update(payload.files.len().to_le_bytes());
    for file in &payload.files {
        hash_part(&mut hasher, "file");
        hasher.update(file.name.as_bytes());
        hasher.update(file.bytes.len().to_le_bytes());
        hasher.update(&file.bytes);
    }

    format!("{:x}", hasher.finalize())
}

fn hash_optional_string(hasher: &mut Sha256, label: &str, value: Option<&str>) {
    hash_part(hasher, label);
    match value {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.as_bytes());
        }
        None => hasher.update([0]),
    }
}

fn hash_part(hasher: &mut Sha256, label: &str) {
    hasher.update(label.as_bytes());
    hasher.update([0]);
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;

    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClipboardFile, ClipboardSyncState, capture_files_from_paths, payload_signature,
        sanitize_remote_payload, write_clipboard_files_to_cache,
    };
    use crate::protocol::ClipboardPayload;
    use std::fs;
    use std::path::Path;
    use uuid::Uuid;

    #[test]
    fn remote_apply_is_suppressed_once() {
        let mut state = ClipboardSyncState::default();
        let payload = text_payload("hello");
        state.note_remote_payload(payload_signature(&payload), true);

        assert!(!state.note_local_payload(&payload));
        assert!(!state.note_local_payload(&payload));
    }

    #[test]
    fn remote_apply_without_local_change_does_not_block_future_same_payload() {
        let mut state = ClipboardSyncState::default();
        let hello = text_payload("hello");
        let world = text_payload("world");

        state.note_remote_payload(payload_signature(&hello), false);
        assert!(!state.note_local_payload(&hello));

        state.note_remote_payload(payload_signature(&world), false);
        assert!(state.note_local_payload(&hello));
    }

    #[test]
    fn oversized_clipboard_files_are_filtered() {
        let dir = unique_test_dir("files");
        fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small.txt");
        let large = dir.join("large.txt");
        fs::write(&small, b"ok").unwrap();
        fs::write(&large, vec![7u8; 8]).unwrap();

        let mut warnings = Vec::new();
        let files = capture_files_from_paths(
            vec![
                small.to_string_lossy().to_string(),
                large.to_string_lossy().to_string(),
            ],
            4,
            &mut warnings,
        );

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "small.txt");
        assert_eq!(files[0].bytes, b"ok");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("超过配置上限"));

        cleanup_dir(&dir);
    }

    #[test]
    fn remote_payload_is_filtered_against_local_limit() {
        let mut warnings = Vec::new();
        let payload = ClipboardPayload {
            text: None,
            rich_text: None,
            html: None,
            image: None,
            files: vec![
                ClipboardFile {
                    name: "keep.txt".to_string(),
                    bytes: vec![1, 2],
                },
                ClipboardFile {
                    name: "drop.txt".to_string(),
                    bytes: vec![3, 4, 5],
                },
            ],
        };

        let filtered = sanitize_remote_payload(payload, 2, &mut warnings);
        assert_eq!(filtered.files.len(), 1);
        assert_eq!(filtered.files[0].name, "keep.txt");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("drop.txt"));
    }

    #[test]
    fn clipboard_cache_prunes_oldest_batches_when_over_limit() {
        let dir = unique_test_dir("cache-prune");
        fs::create_dir_all(&dir).unwrap();

        let mut warnings = Vec::new();
        let first = write_clipboard_files_to_cache(
            &dir,
            &[ClipboardFile {
                name: "one.bin".to_string(),
                bytes: vec![1, 2, 3],
            }],
            Some(6),
            &mut warnings,
        )
        .unwrap();
        assert_eq!(first.len(), 1);

        let second = write_clipboard_files_to_cache(
            &dir,
            &[ClipboardFile {
                name: "two.bin".to_string(),
                bytes: vec![4, 5, 6, 7],
            }],
            Some(6),
            &mut warnings,
        )
        .unwrap();
        assert_eq!(second.len(), 1);

        let batch_dirs = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.metadata().map(|meta| meta.is_dir()).unwrap_or(false))
            .collect::<Vec<_>>();
        assert_eq!(batch_dirs.len(), 1);
        assert!(second[0].exists());
        assert!(!first[0].exists());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("已清理较早的剪贴板缓存"))
        );

        cleanup_dir(&dir);
    }

    #[test]
    fn oversized_latest_clipboard_cache_is_retained_with_warning() {
        let dir = unique_test_dir("cache-keep-latest");
        fs::create_dir_all(&dir).unwrap();

        let mut warnings = Vec::new();
        let paths = write_clipboard_files_to_cache(
            &dir,
            &[ClipboardFile {
                name: "large.bin".to_string(),
                bytes: vec![9; 8],
            }],
            Some(4),
            &mut warnings,
        )
        .unwrap();

        assert_eq!(paths.len(), 1);
        assert!(paths[0].exists());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("已保留最新缓存"))
        );

        cleanup_dir(&dir);
    }

    fn text_payload(text: &str) -> ClipboardPayload {
        ClipboardPayload {
            text: Some(text.to_string()),
            rich_text: None,
            html: None,
            image: None,
            files: vec![],
        }
    }

    fn unique_test_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("synly-clipboard-test-{label}-{}", Uuid::new_v4()))
    }

    fn cleanup_dir(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}
