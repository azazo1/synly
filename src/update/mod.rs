mod check;
mod download;
mod form;
mod install;
#[cfg(target_os = "macos")]
mod macos;
mod pending;
mod state;

use crate::config::UpdateConfig;
use anyhow::{Context, Result};
use check::release_page_url;
use pending::ApplyOutcome;
use state::{AvailableRelease, InstallOutcome, UpdatePhase as Phase};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use form::DistributionForm;
pub use state::{UpdatePhase, UpdateSnapshot};

const SILENT_CHECK_DELAY_SECS: u64 = 5;

/// 上次下载留下的校验和文件, 用于确认本地安装包仍然可信.
const CHECKSUMS_FILE: &str = "SHA256SUMS";

/// 本次启动是不是安装器落地更新后的首次启动.
///
/// 这种启动下, 正在运行的 Windows 输入服务必然还映射着更新前的映像: 安装器只换了磁盘上
/// 的文件, SYSTEM 服务要等重启才会加载新版本. 其它情况退回用让位文件是否删得掉来推断.
#[cfg(windows)]
static UPDATE_LANDED_BEFORE_START: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
pub struct UpdateHandle {
    inner: Arc<Mutex<Inner>>,
    snapshots: watch::Receiver<UpdateSnapshot>,
    runtime: tokio::runtime::Handle,
}

struct Inner {
    current_version: String,
    auto_check: bool,
    skipped_version: String,
    snapshot: UpdateSnapshot,
    available: Option<AvailableRelease>,
    snapshot_tx: watch::Sender<UpdateSnapshot>,
    persist: Arc<dyn Fn(UpdateConfig) + Send + Sync>,
    check_task: Option<JoinHandle<()>>,
    download_task: Option<JoinHandle<()>>,
    cancel_download: CancellationToken,
    download_generation: u64,
    applying: bool,
    /// 下载完成、等待用户点击重启并更新的安装包.
    ready_package: Option<PathBuf>,
}

impl UpdateHandle {
    pub fn subscribe(&self) -> watch::Receiver<UpdateSnapshot> {
        self.snapshots.clone()
    }

    pub fn snapshot(&self) -> UpdateSnapshot {
        self.snapshots.borrow().clone()
    }

    pub fn set_auto_check(&self, enabled: bool) {
        let mut inner = self.lock();
        if inner.auto_check == enabled {
            return;
        }
        inner.auto_check = enabled;
        inner.snapshot.auto_check = enabled;
        inner.publish();
        inner.persist();
        tracing::info!(enabled, "已更新启动时自动检查更新");
    }

    pub fn check(&self, manual: bool) {
        let mut inner = self.lock();
        if matches!(inner.snapshot.phase, UpdatePhase::Checking | UpdatePhase::Downloading) {
            return;
        }
        if let Some(task) = inner.check_task.take() {
            task.abort();
        }
        inner.snapshot.phase = UpdatePhase::Checking;
        inner.snapshot.error_text.clear();
        if manual {
            inner.snapshot.apply_message.clear();
        }
        inner.publish();
        tracing::info!(manual, "开始检查更新");
        let handle = self.clone();
        inner.check_task = Some(self.runtime.spawn(async move {
            handle.run_check(manual).await;
        }));
    }

    pub fn download(&self) {
        let mut inner = self.lock();
        if inner.available.is_none() {
            return;
        }
        if matches!(inner.snapshot.phase, UpdatePhase::Downloading) {
            return;
        }
        inner.cancel_download = CancellationToken::new();
        inner.download_generation = inner.download_generation.wrapping_add(1);
        inner.applying = false;
        inner.snapshot.phase = UpdatePhase::Downloading;
        inner.snapshot.cancellable = true;
        inner.snapshot.error_text.clear();
        inner.snapshot.received_bytes = 0;
        inner.snapshot.total_bytes = None;
        inner.publish();
        tracing::info!("开始下载更新包");
        let handle = self.clone();
        let generation = inner.download_generation;
        inner.download_task = Some(self.runtime.spawn(async move {
            handle.run_download(generation).await;
        }));
    }

    pub fn cancel_download(&self) {
        let mut inner = self.lock();
        if inner.snapshot.phase != UpdatePhase::Downloading {
            return;
        }
        if inner.applying {
            tracing::debug!("更新已进入安装阶段, 无法取消");
            return;
        }
        tracing::info!("取消更新下载");
        inner.cancel_download.cancel();
        if let Some(task) = inner.download_task.take() {
            task.abort();
        }
        inner.snapshot.phase = UpdatePhase::Available;
        inner.snapshot.cancellable = false;
        inner.snapshot.error_text.clear();
        inner.publish();
    }

    pub fn skip_current(&self) {
        let mut inner = self.lock();
        if let Some(available) = inner.available.clone() {
            let tag = available.tag;
            inner.skipped_version = tag.clone();
            inner.persist();
            inner.snapshot.phase = UpdatePhase::UpToDate;
            inner.publish();
            tracing::info!(tag = %tag, "已跳过此版本");
        }
    }

    /// 用户点击 "重启并更新" 后把安装包交接给平台安装器.
    ///
    /// 下载完成本身只进入 `ReadyToRestart`, 不会自动交接, 也不会自动退出.
    pub fn install(&self) {
        let mut inner = self.lock();
        if inner.snapshot.phase != Phase::ReadyToRestart {
            return;
        }
        let Some(package) = inner.ready_package.clone() else {
            tracing::warn!("更新包已不在本地, 无法交接安装程序");
            return;
        };
        let version = inner
            .snapshot
            .latest_tag
            .clone()
            .unwrap_or_else(|| inner.current_version.clone());
        inner.snapshot.phase = Phase::Applying;
        inner.snapshot.cancellable = false;
        inner.snapshot.apply_message.clear();
        inner.snapshot.error_text.clear();
        inner.publish();
        tracing::info!(package = %package.display(), "开始交接安装程序");
        let handle = self.clone();
        inner.download_task = Some(self.runtime.spawn(async move {
            handle.run_apply(package, version).await;
        }));
    }

    pub fn open_release_page(&self) {
        let inner = self.lock();
        let url = inner
            .snapshot
            .release_url
            .clone()
            .or_else(|| inner.available.as_ref().map(|item| item.html_url.clone()))
            .unwrap_or_else(|| release_page_url("latest"));
        drop(inner);
        if let Err(error) = open_url(&url) {
            tracing::warn!(error = %error, "无法打开 Release 页");
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn run_check(&self, manual: bool) {
        let (current, skipped) = {
            let inner = self.lock();
            (inner.current_version.clone(), inner.skipped_version.clone())
        };
        let result = match build_client() {
            Ok(client) => check::fetch_latest(&client, &current).await,
            Err(error) => Err(error),
        };
        let mut inner = self.lock();
        match result {
            Ok(Some(available)) => {
                let skipped_hit = !skipped.is_empty() && skipped == available.tag;
                let tag = available.tag.clone();
                let display = available.display.clone();
                inner.available = Some(available.clone());
                inner.snapshot.latest_tag = Some(tag.clone());
                inner.snapshot.latest_display = Some(display.clone());
                inner.snapshot.release_notes = available.notes.clone();
                inner.snapshot.release_url = Some(available.html_url.clone());
                inner.snapshot.error_text.clear();
                if skipped_hit && !manual {
                    inner.snapshot.phase = UpdatePhase::UpToDate;
                    inner.publish();
                    return;
                }
                inner.snapshot.phase = UpdatePhase::Available;
                inner.publish();
                tracing::info!(tag = %tag, "发现新版本");
                if !manual {
                    drop(inner);
                    notify_update_available(&display);
                }
            }
            Ok(None) => {
                inner.available = None;
                inner.snapshot.phase = UpdatePhase::UpToDate;
                inner.snapshot.error_text.clear();
                inner.publish();
                if manual {
                    tracing::info!("当前已是最新版本");
                }
            }
            Err(error) => {
                inner.snapshot.phase = UpdatePhase::Failed;
                inner.snapshot.error_text = error.to_string();
                inner.publish();
                if manual {
                    tracing::warn!(error = %error, "手动检查更新失败");
                } else {
                    tracing::debug!(error = %error, "静默检查更新失败");
                }
            }
        }
    }

    async fn run_download(&self, generation: u64) {
        let (available, cancel) = {
            let inner = self.lock();
            if !inner.is_active_download(generation) {
                return;
            }
            let Some(available) = inner.available.clone() else {
                return;
            };
            (available, inner.cancel_download.clone())
        };
        let client = match build_client() {
            Ok(client) => client,
            Err(error) => {
                self.fail(generation, error.to_string());
                return;
            }
        };
        let update_dir = match crate::paths::update_dir() {
            Ok(dir) => dir,
            Err(error) => {
                self.fail(generation, error.to_string());
                return;
            }
        };
        let part_path = update_dir.join(format!("{}.part", available.archive_name));
        let final_path = update_dir.join(&available.archive_name);
        let sums = tokio::select! {
            _ = cancel.cancelled() => return,
            result = download::download_sha256sums(&client, &available.checksums_url) => match result {
                Ok(text) => text,
                Err(error) => {
                    self.fail(generation, error.to_string());
                    return;
                }
            },
        };
        let expected = match download::expected_sha256(&sums, &available.archive_name) {
            Ok(value) => value,
            Err(error) => {
                self.fail(generation, error.to_string());
                return;
            }
        };
        let handle = self.clone();
        let download = download::download_archive(
            &client,
            &available.archive_url,
            &part_path,
            expected,
            cancel.clone(),
            move |progress| {
                let mut inner = handle.lock();
                if !inner.is_active_download(generation) {
                    return;
                }
                inner.snapshot.received_bytes = progress.received;
                inner.snapshot.total_bytes = progress.total;
                inner.publish();
            },
        )
        .await;
        if cancel.is_cancelled() || !self.is_active_download(generation) {
            return;
        }
        if let Err(error) = download {
            self.fail(generation, error.to_string());
            return;
        }
        let renamed = tokio::select! {
            _ = cancel.cancelled() => return,
            result = tokio::fs::rename(&part_path, &final_path) => result,
        };
        if let Err(error) = renamed {
            self.fail(generation, format!("无法保存更新包: {error}"));
            return;
        }
        if let Err(error) = persist_checksums(&update_dir, &sums) {
            tracing::warn!(error = %error, "无法保存 SHA256SUMS, 下次启动时需要重新下载更新包");
        }
        self.finish_download(generation, |inner| {
            inner.ready_package = Some(final_path.clone());
            inner.snapshot.phase = Phase::ReadyToRestart;
            inner.snapshot.cancellable = false;
            inner.snapshot.apply_message = "新版本已下载, 点击重启并更新后由安装程序完成替换".to_string();
            tracing::info!(package = %final_path.display(), "更新包已就绪, 等待用户重启安装");
        });
    }

    /// 把安装包交接给平台安装器, 成功后本进程随即退出.
    async fn run_apply(&self, package: PathBuf, version: String) {
        let result = tokio::task::spawn_blocking(move || install::apply_installer(&package, &version))
            .await
            .unwrap_or_else(|error| Err(anyhow::anyhow!("安装任务异常终止: {error}")));
        let mut inner = self.lock();
        if inner.snapshot.phase != Phase::Applying {
            return;
        }
        match result {
            Ok(InstallOutcome::HandedOff) => {
                inner.snapshot.phase = Phase::HandedOff;
                inner.snapshot.cancellable = false;
                inner.snapshot.apply_message = "正在退出并运行安装程序, 请勿手动关闭进程".to_string();
                inner.publish();
                tracing::info!("已交接安装程序, 准备退出");
            }
            Ok(InstallOutcome::DmgOpened) => {
                inner.snapshot.phase = Phase::DmgOpened;
                inner.snapshot.cancellable = false;
                inner.snapshot.apply_message = "已打开安装镜像, 请拖拽安装后重启".to_string();
                inner.publish();
                tracing::info!("已打开 dmg, 等待用户手动安装");
            }
            Err(error) => {
                inner.snapshot.phase = Phase::Failed;
                inner.snapshot.cancellable = false;
                inner.snapshot.error_text = error.to_string();
                inner.snapshot.apply_message.clear();
                inner.publish();
                tracing::warn!(error = %format!("{error:#}"), "交接安装程序失败");
            }
        }
    }

    fn is_active_download(&self, generation: u64) -> bool {
        self.lock().is_active_download(generation)
    }

    /// 启动时复用上次已下载并校验通过的安装包, 避免让用户重新下载.
    async fn restore_local_package(&self, update_dir: &Path) {
        let Some((package, version)) = locate_local_package(update_dir) else {
            return;
        };
        let current = {
            let inner = self.lock();
            if inner.ready_package.is_some() {
                return;
            }
            inner.current_version.clone()
        };
        if !check::is_newer(&current, &version) {
            return;
        }
        let Some(name) = package.file_name().map(|value| value.to_string_lossy().into_owned())
        else {
            return;
        };
        let Some(expected) = fs::read_to_string(update_dir.join(CHECKSUMS_FILE))
            .ok()
            .and_then(|text| download::expected_sha256(&text, &name).ok())
        else {
            return;
        };
        let Ok(actual) = download::sha256_file(&package).await else {
            return;
        };
        if !actual.eq_ignore_ascii_case(&download::hex_encode(&expected)) {
            tracing::warn!(package = %package.display(), "本地更新包校验失败, 需要重新下载");
            return;
        }
        let mut inner = self.lock();
        if inner.ready_package.is_some()
            || !matches!(inner.snapshot.phase, Phase::Idle | Phase::UpToDate)
        {
            return;
        }
        inner.ready_package = Some(package.clone());
        inner.snapshot.phase = Phase::ReadyToRestart;
        inner.snapshot.latest_tag = Some(version.clone());
        inner.snapshot.latest_display = Some(version.clone());
        inner.snapshot.apply_message =
            "新版本已下载, 点击重启并更新后由安装程序完成替换".to_string();
        inner.publish();
        tracing::info!(version, package = %package.display(), "复用上次下载好的更新包");
    }

    fn finish_download(&self, generation: u64, update: impl FnOnce(&mut Inner)) {
        let mut inner = self.lock();
        if inner.download_generation != generation {
            return;
        }
        if inner.snapshot.phase != UpdatePhase::Downloading {
            return;
        }
        update(&mut inner);
        inner.applying = false;
        inner.publish();
    }

    fn fail(&self, generation: u64, message: String) {
        self.finish_download(generation, |inner| {
            inner.snapshot.phase = UpdatePhase::Failed;
            inner.snapshot.cancellable = false;
            inner.snapshot.error_text = message;
        });
    }
}

impl Inner {
    fn is_active_download(&self, generation: u64) -> bool {
        self.download_generation == generation
            && self.snapshot.phase == UpdatePhase::Downloading
            && !self.cancel_download.is_cancelled()
    }

    fn publish(&self) {
        self.snapshot_tx.send_replace(self.snapshot.clone());
    }

    fn persist(&self) {
        (self.persist)(UpdateConfig {
            auto_check: self.auto_check,
            skipped_version: self.skipped_version.clone(),
        });
    }
}

pub fn start(
    runtime: &tokio::runtime::Runtime,
    current_version: String,
    config: UpdateConfig,
    persist: Arc<dyn Fn(UpdateConfig) + Send + Sync>,
) -> Result<UpdateHandle> {
    let _ = install::cleanup_stale_artifacts();
    let mut snapshot = UpdateSnapshot::idle(current_version.clone(), config.auto_check);
    let update_dir = crate::paths::update_dir().ok();
    if let Some(dir) = &update_dir {
        apply_pending_outcome(dir, &current_version, &mut snapshot);
    }
    let (snapshot_tx, snapshots) = watch::channel(snapshot.clone());
    let handle = UpdateHandle {
        inner: Arc::new(Mutex::new(Inner {
            current_version,
            auto_check: config.auto_check,
            skipped_version: config.skipped_version,
            snapshot,
            available: None,
            snapshot_tx,
            persist,
            check_task: None,
            download_task: None,
            cancel_download: CancellationToken::new(),
            download_generation: 0,
            applying: false,
            ready_package: None,
        })),
        snapshots,
        runtime: runtime.handle().clone(),
    };
    if let Some(dir) = update_dir {
        let restored = handle.clone();
        runtime.spawn(async move {
            restored.restore_local_package(&dir).await;
        });
    }
    if config.auto_check {
        let delayed = handle.clone();
        runtime.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(SILENT_CHECK_DELAY_SECS)).await;
            if delayed.lock().auto_check {
                delayed.check(false);
            }
        });
    }
    Ok(handle)
}

/// 回显上次交接的落地结果.
fn apply_pending_outcome(update_dir: &Path, current_version: &str, snapshot: &mut UpdateSnapshot) {
    match pending::take_outcome(update_dir, current_version) {
        Some(ApplyOutcome::Applied { version }) => {
            #[cfg(windows)]
            note_update_landed();
            snapshot.apply_message = format!("已更新到 {version}");
            tracing::info!(version, "上次交接的安装程序已完成替换");
        }
        Some(ApplyOutcome::Failed { message }) => {
            tracing::warn!(message, "上次交接的安装程序没有完成替换");
            snapshot.phase = Phase::Failed;
            snapshot.error_text = message.clone();
            snapshot.apply_message = message;
        }
        None => {}
    }
}

/// 记录本次启动是安装器落地更新后的首次启动.
#[cfg(windows)]
fn note_update_landed() {
    UPDATE_LANDED_BEFORE_START.store(true, Ordering::Release);
}

/// 本次启动是否紧接着一次已落地的更新.
///
/// 这种情况下正在运行的 Windows 输入服务必然还映射着更新前的映像, 需要重启它.
#[cfg(windows)]
pub fn update_landed_before_start() -> bool {
    UPDATE_LANDED_BEFORE_START.load(Ordering::Acquire)
}

/// 清理安装器让位时留下的旧文件, 返回是否仍有文件无法删除.
///
/// 安装器覆盖程序文件前会把被占用的旧文件改成 `<name>.old`; 只要它还在, 就说明仍有进程
/// 运行着更新前的版本, Windows 上通常是 SYSTEM 输入服务. 每次调用都会顺带清理能删掉的
/// 残留.
#[cfg(windows)]
pub fn cleanup_old_binary_backups() -> bool {
    install::cleanup_stale_artifacts()
}

/// 每次检查或下载都新建 client, 让系统代理和代理环境变量变化实时生效.
///
/// reqwest 只在构建 client 时读取一次代理配置, 长期复用同一个 client 会一直沿用启动时的旧代理.
fn build_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("synly")
        .build()
        .context("无法创建更新用的 HTTP 客户端")
}

fn notify_update_available(version: &str) {
    let body = format!("新版本 {version} 可用");
    let _ = std::thread::Builder::new()
        .name("synly-update-notification".to_string())
        .spawn(move || {
            let _ = notify_rust::Notification::new()
                .appname("Synly")
                .summary("Synly 有新版本")
                .body(&body)
                .show();
        });
}

fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()?;
    }
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()?;
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

/// 把本次下载使用的 SHA256SUMS 留在更新目录, 供下次启动校验本地安装包.
fn persist_checksums(update_dir: &Path, sums: &str) -> Result<()> {
    fs::write(update_dir.join(CHECKSUMS_FILE), sums).context("无法保存 SHA256SUMS")
}

/// 在更新目录里寻找上次下载好的安装包.
fn locate_local_package(update_dir: &Path) -> Option<(PathBuf, String)> {
    let entries = fs::read_dir(update_dir).ok()?;
    let mut found: Option<(PathBuf, String)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(version) = parse_local_package_name(
            &name,
            check::current_platform(),
            check::current_arch(),
            form::effective_form(),
        ) else {
            continue;
        };
        let version = format!("v{version}");
        let newer = found
            .as_ref()
            .map(|(_, existing)| check::is_newer(existing, &version))
            .unwrap_or(true);
        if newer {
            found = Some((path, version));
        }
    }
    found
}

/// 从安装包文件名解析版本, 只接受当前平台与形态对应的命名.
fn parse_local_package_name(
    name: &str,
    platform: &str,
    arch: &str,
    form: DistributionForm,
) -> Option<String> {
    let suffix = match (platform, form) {
        ("windows", DistributionForm::Installer) => "-setup.exe",
        ("windows", DistributionForm::Portable) => "-portable.zip",
        ("linux", DistributionForm::Installer) => "-setup.tar.gz",
        ("linux", DistributionForm::Portable) => "-portable.tar.gz",
        _ => ".dmg",
    };
    let head = name.strip_suffix(suffix)?;
    let rest = head.strip_prefix("synly-")?;
    let version = rest.strip_suffix(&format!("-{platform}-{arch}"))?;
    (!version.is_empty()).then(|| version.to_string())
}
