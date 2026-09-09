use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::reload;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const GUI_LOG_CAPACITY: usize = 200;
const MAX_LOG_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_LOG_FILES: usize = 14;
static GUI_LOGS: OnceLock<Arc<Mutex<VecDeque<String>>>> = OnceLock::new();
static FILTER_HANDLE: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

#[derive(Clone)]
struct GuiLogMakeWriter {
    logs: Arc<Mutex<VecDeque<String>>>,
}

struct GuiLogWriter {
    logs: Arc<Mutex<VecDeque<String>>>,
    buffer: Vec<u8>,
}

impl<'a> MakeWriter<'a> for GuiLogMakeWriter {
    type Writer = GuiLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        GuiLogWriter {
            logs: Arc::clone(&self.logs),
            buffer: Vec::new(),
        }
    }
}

impl Write for GuiLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for GuiLogWriter {
    fn drop(&mut self) {
        let line = String::from_utf8_lossy(&self.buffer).trim().to_string();
        if line.is_empty() {
            return;
        }
        let Ok(mut logs) = self.logs.lock() else {
            return;
        };
        logs.push_back(line);
        while logs.len() > GUI_LOG_CAPACITY {
            logs.pop_front();
        }
    }
}

struct RotatingFile {
    path: PathBuf,
    file: File,
    written: u64,
    date: String,
}

impl RotatingFile {
    fn open(path: PathBuf) -> io::Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            written,
            date: current_date(),
        })
    }

    fn rotate_if_needed(&mut self, incoming: u64) -> io::Result<()> {
        let date = current_date();
        if self.date == date && self.written + incoming <= MAX_LOG_FILE_BYTES {
            return Ok(());
        }
        self.file.flush()?;
        if self.written > 0 {
            let archived = archive_name(&self.path, &self.date);
            fs::rename(&self.path, &archived)?;
        }
        self.file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        self.written = 0;
        self.date = date;
        prune_old_logs(&self.path);
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed(bytes.len() as u64)?;
        let written = self.file.write(bytes)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn current_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(mut z: i64) -> (i32, u32, u32) {
    z += 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m as u32, d as u32)
}

fn archive_name(path: &Path, date: &str) -> PathBuf {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("synly");
    let ext = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("log");
    let dir = path.parent().unwrap_or(Path::new("."));
    for index in 1..1_000 {
        let candidate = dir.join(format!("{stem}.{date}.{index}.{ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}.{date}.overflow.{ext}"))
}

fn prune_old_logs(path: &Path) {
    let Some(dir) = path.parent() else {
        return;
    };
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("synly");
    let mut files = match fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|candidate| {
                candidate
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(stem) && name != path.file_name().and_then(|value| value.to_str()).unwrap_or_default())
            })
            .collect::<Vec<_>>(),
        Err(_) => return,
    };
    files.sort();
    while files.len() >= MAX_LOG_FILES {
        if let Some(old) = files.first() {
            let _ = fs::remove_file(old);
            files.remove(0);
        } else {
            break;
        }
    }
}

pub fn init_tracing(default_filter: &str) -> Result<WorkerGuard> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let (filter, filter_handle) = reload::Layer::new(filter);
    FILTER_HANDLE
        .set(filter_handle)
        .map_err(|_| anyhow::anyhow!("tracing filter is already initialized"))?;
    let log_path = crate::paths::log_file_path()?;
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create log directory {}", parent.display()))?;
    }
    let rotating = RotatingFile::open(log_path.clone())
        .with_context(|| format!("failed to open log file {}", log_path.display()))?;
    let (file_writer, guard) = tracing_appender::non_blocking(rotating);
    let gui_logs = GUI_LOGS
        .get_or_init(|| Arc::new(Mutex::new(VecDeque::new())))
        .clone();
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(true),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(file_writer),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(GuiLogMakeWriter { logs: gui_logs }),
        )
        .try_init()
        .context("failed to initialize tracing")?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!(panic = %info, "未捕获 panic");
        default_hook(info);
    }));
    Ok(guard)
}

pub fn set_log_level(filter: &str) -> Result<()> {
    let filter = EnvFilter::try_new(filter).context("invalid tracing filter")?;
    FILTER_HANDLE
        .get()
        .context("tracing filter is not initialized")?
        .reload(filter)
        .context("failed to reload tracing filter")
}

pub fn recent_logs() -> String {
    GUI_LOGS
        .get()
        .and_then(|logs| logs.lock().ok())
        .map(|logs| logs.iter().cloned().collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

pub fn clear_logs() {
    let Some(logs) = GUI_LOGS.get() else {
        return;
    };
    let Ok(mut logs) = logs.lock() else {
        return;
    };
    logs.clear();
}
