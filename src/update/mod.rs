mod check;
mod download;
mod install;
#[cfg(target_os = "macos")]
mod macos;
mod state;

use crate::config::UpdateConfig;
use anyhow::Result;
use check::release_page_url;
use state::{AvailableRelease, InstallOutcome, RestartAction};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use state::{RestartAction as UpdateRestartAction, UpdatePhase, UpdateSnapshot};

const SILENT_CHECK_DELAY_SECS: u64 = 5;

#[derive(Clone)]
pub struct UpdateHandle {
    inner: Arc<Mutex<Inner>>,
    snapshots: watch::Receiver<UpdateSnapshot>,
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
    cancel_download: Arc<AtomicBool>,
    restart: Option<RestartAction>,
    client: reqwest::Client,
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
        let handle = self.clone();
        inner.check_task = Some(tokio::spawn(async move {
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
        inner.cancel_download.store(false, Ordering::Release);
        inner.snapshot.phase = UpdatePhase::Downloading;
        inner.snapshot.error_text.clear();
        inner.snapshot.received_bytes = 0;
        inner.snapshot.total_bytes = None;
        inner.publish();
        tracing::info!("开始下载更新包");
        let handle = self.clone();
        inner.download_task = Some(tokio::spawn(async move {
            handle.run_download().await;
        }));
    }

    pub fn cancel_download(&self) {
        let inner = self.lock();
        if inner.snapshot.phase == UpdatePhase::Downloading {
            tracing::info!("取消更新下载");
            inner.cancel_download.store(true, Ordering::Release);
        }
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

    pub fn install(&self) -> Option<RestartAction> {
        let inner = self.lock();
        match inner.snapshot.phase {
            UpdatePhase::ReadyToRestart => inner.restart.clone(),
            UpdatePhase::HandedOff => Some(RestartAction::QuitOnly),
            _ => None,
        }
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
        let (client, current, skipped) = {
            let inner = self.lock();
            (
                inner.client.clone(),
                inner.current_version.clone(),
                inner.skipped_version.clone(),
            )
        };
        let result = check::fetch_latest(&client, &current).await;
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

    async fn run_download(&self) {
        let (client, available, cancel) = {
            let inner = self.lock();
            let Some(available) = inner.available.clone() else {
                return;
            };
            (inner.client.clone(), available, inner.cancel_download.clone())
        };
        let update_dir = match crate::paths::update_dir() {
            Ok(dir) => dir,
            Err(error) => {
                self.fail(error.to_string());
                return;
            }
        };
        let part_path = update_dir.join(format!("{}.part", available.archive_name));
        let final_path = update_dir.join(&available.archive_name);
        let sums = match download::download_sha256sums(&client, &available.checksums_url).await {
            Ok(text) => text,
            Err(error) => {
                self.fail(error.to_string());
                return;
            }
        };
        let expected = match download::expected_sha256(&sums, &available.archive_name) {
            Ok(value) => value,
            Err(error) => {
                self.fail(error.to_string());
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
                inner.snapshot.received_bytes = progress.received;
                inner.snapshot.total_bytes = progress.total;
                inner.publish();
            },
        )
        .await;
        if cancel.load(Ordering::Acquire) {
            let mut inner = self.lock();
            inner.snapshot.phase = UpdatePhase::Available;
            inner.publish();
            return;
        }
        if let Err(error) = download {
            if cancel.load(Ordering::Acquire) {
                let mut inner = self.lock();
                inner.snapshot.phase = UpdatePhase::Available;
                inner.publish();
                return;
            }
            self.fail(error.to_string());
            return;
        }
        if let Err(error) = tokio::fs::rename(&part_path, &final_path).await {
            self.fail(format!("无法保存更新包: {error}"));
            return;
        }
        match install::apply_archive(&final_path) {
            Ok(InstallOutcome::ReadyToRestart { exe }) => {
                let mut inner = self.lock();
                inner.snapshot.phase = UpdatePhase::ReadyToRestart;
                inner.restart = Some(RestartAction::Relaunch { exe });
                inner.publish();
                tracing::info!("更新已就绪, 等待重启");
            }
            Ok(InstallOutcome::HandedOff) => {
                let mut inner = self.lock();
                inner.snapshot.phase = UpdatePhase::HandedOff;
                inner.snapshot.apply_message = "正在退出并替换, 请勿手动关闭进程".to_string();
                inner.restart = Some(RestartAction::QuitOnly);
                inner.publish();
                tracing::info!("已交接 macOS 替换脚本");
            }
            Ok(InstallOutcome::DmgOpened) => {
                let mut inner = self.lock();
                inner.snapshot.phase = UpdatePhase::DmgOpened;
                inner.snapshot.apply_message = "已打开 dmg, 请拖拽安装后重启应用".to_string();
                inner.publish();
            }
            Err(error) => self.fail(error.to_string()),
        }
    }

    fn fail(&self, message: String) {
        let mut inner = self.lock();
        inner.snapshot.phase = UpdatePhase::Failed;
        inner.snapshot.error_text = message;
        inner.publish();
    }
}

impl Inner {
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
    install::cleanup_old_binary();
    let client = reqwest::Client::builder()
        .user_agent("synly")
        .build()
        .expect("reqwest client");
    let mut snapshot = UpdateSnapshot::idle(current_version.clone(), config.auto_check);
    #[cfg(target_os = "macos")]
    if let Some(message) = macos::take_apply_result() {
        snapshot.phase = UpdatePhase::Failed;
        snapshot.error_text = message;
        snapshot.apply_message = snapshot.error_text.clone();
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
            cancel_download: Arc::new(AtomicBool::new(false)),
            restart: None,
            client,
        })),
        snapshots,
    };
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

pub fn relaunch(exe: PathBuf) -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::Command::new(exe).args(args).spawn()?;
    Ok(())
}
