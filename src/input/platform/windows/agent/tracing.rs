use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

pub fn init() -> Result<WorkerGuard> {
    let log_dir = dirs::data_local_dir()
        .context("unable to determine local data directory")?
        .join("synly")
        .join("logs")
        .join("input-agent");
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("failed to create input agent log directory {}", log_dir.display()))?;
    let file_appender = tracing_appender::rolling::daily(log_dir, "input-agent.trace.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let console_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let trace_filter = EnvFilter::new(
        "info,synly_input_agent=trace,synly::input::platform::windows::agent=trace",
    );
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_filter(console_filter),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(file_writer)
                .with_filter(trace_filter),
        )
        .try_init()
        .context("failed to initialize input agent tracing")?;
    Ok(guard)
}
