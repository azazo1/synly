use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::reload;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const GUI_LOG_CAPACITY: usize = 500;
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

pub fn init_tracing(default_filter: &str) -> Result<WorkerGuard> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let (filter, filter_handle) = reload::Layer::new(filter);
    FILTER_HANDLE
        .set(filter_handle)
        .map_err(|_| anyhow::anyhow!("tracing filter is already initialized"))?;
    let log_dir = dirs::data_local_dir()
        .context("unable to determine local data directory")?
        .join("synly")
        .join("logs");
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create log directory {}", log_dir.display()))?;
    let file_appender = tracing_appender::rolling::daily(log_dir, "synly.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
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
