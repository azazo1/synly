//! 更新交接标记与落地结果回显.
//!
//! 把落地工作交给平台安装器之后, 当前进程马上就要退出, 安装器成功与否无法在本进程里
//! 观察到. 交接前写下的 `apply-update.pending` 是判断 "更新到底落地了没有" 的唯一依据:
//! 下次启动时当前版本已经达到目标版本, 说明安装器把新版本装上了; 版本没变, 则把安装器
//! 日志或安装脚本写下的结果文件回显给用户, 并保留安装包供重试.

use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const PENDING_FILE: &str = "apply-update.pending";
const RESULT_FILE: &str = "apply-update-result.txt";

const LOG_FILE: &str = "apply-update.log";

/// 交接给安装器时写下的记录.
#[derive(Clone, Debug)]
pub struct Handoff {
    pub version: String,
    pub form: String,
    pub package: String,
    pub time_unix: u64,
    pub log_path: PathBuf,
}

impl Handoff {
    pub fn write(&self, update_dir: &Path) -> Result<()> {
        fs::create_dir_all(update_dir)
            .with_context(|| format!("无法创建更新目录 {}", update_dir.display()))?;
        let body = format!(
            "version={}\nform={}\npackage={}\ntime_unix={}\nlog={}\n",
            self.version,
            self.form,
            self.package,
            self.time_unix,
            self.log_path.display()
        );
        let path = pending_path(update_dir);
        fs::write(&path, body).with_context(|| format!("无法写入 {}", path.display()))
    }
}

/// 上次交接的落地结果.
#[derive(Clone, Debug)]
pub enum ApplyOutcome {
    /// 安装器已经把程序换成了目标版本.
    Applied { version: String },
    /// 版本没有变化, 安装器没有跑完或没有把程序换掉.
    Failed { message: String },
}

/// 更新目录下的安装器日志路径.
pub fn log_path(update_dir: &Path) -> PathBuf {
    update_dir.join(LOG_FILE)
}

/// 安装脚本写入落地结果的路径.
pub fn result_path(update_dir: &Path) -> PathBuf {
    update_dir.join(RESULT_FILE)
}

/// 读取并删除上次交接记录, 判断它是否落地成功.
pub fn take_outcome(update_dir: &Path, current_version: &str) -> Option<ApplyOutcome> {
    let path = pending_path(update_dir);
    let text = fs::read_to_string(&path).ok()?;
    let record = parse(&text);
    let _ = fs::remove_file(&path);
    let version = record.version.clone()?;
    if !super::check::is_newer(current_version, &version) {
        return Some(ApplyOutcome::Applied { version });
    }
    let message = failure_message(update_dir, &record);
    Some(ApplyOutcome::Failed { message })
}

fn failure_message(update_dir: &Path, record: &Record) -> String {
    if let Ok(text) = fs::read_to_string(result_path(update_dir)) {
        let _ = fs::remove_file(result_path(update_dir));
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let log_path = record
        .log_path
        .clone()
        .unwrap_or_else(|| log_path(update_dir));
    let tail = read_log_tail(&log_path);
    if tail.is_empty() {
        format!(
            "安装程序没有完成本次更新, 安装包仍在更新目录, 可稍后重试 (日志: {})",
            log_path.display()
        )
    } else {
        format!("安装程序没有完成本次更新: {tail}")
    }
}

fn read_log_tail(path: &Path) -> String {
    let Ok(bytes) = fs::read(path) else {
        return String::new();
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.len() > 3 {
        lines = lines.split_off(lines.len() - 3);
    }
    lines.join("; ")
}

fn pending_path(update_dir: &Path) -> PathBuf {
    update_dir.join(PENDING_FILE)
}

#[derive(Default)]
struct Record {
    version: Option<String>,
    log_path: Option<PathBuf>,
}

fn parse(text: &str) -> Record {
    let mut record = Record::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "version" => record.version = Some(value.to_string()),
            "log" if !value.is_empty() => record.log_path = Some(PathBuf::from(value)),
            _ => {}
        }
    }
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pending_record_fields() {
        let record = parse("version=v1.2.3\nform=installer\npackage=synly-1.2.3-windows-x86_64-setup.exe\ntime_unix=42\nlog=C:\\tmp\\apply.log\n");
        assert_eq!(record.version.as_deref(), Some("v1.2.3"));
        assert_eq!(record.log_path.as_deref(), Some(Path::new(r"C:\tmp\apply.log")));
    }

    #[test]
    fn missing_log_field_falls_back_to_update_dir() {
        let record = parse("version=v1.2.3\n");
        assert!(record.log_path.is_none());
        assert_eq!(record.version.as_deref(), Some("v1.2.3"));
    }
}
