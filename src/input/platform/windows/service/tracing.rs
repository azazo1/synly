use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;

pub fn init_tracing() -> Result<WorkerGuard> {
    std::panic::set_hook(Box::new(|info| {
        eprintln!("{info}");
        tracing::error!(panic = %info, "Synly 输入服务发生 panic");
    }));
    let log_dir = dirs::data_local_dir()
        .context("无法确定本地数据目录")?
        .join("synly")
        .join("logs")
        .join("service");
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("创建输入服务日志目录失败: {}", log_dir.display()))?;
    let file_appender = tracing_appender::rolling::daily(log_dir, "service.trace.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let console_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let trace_filter = EnvFilter::new(
        "info,synly_input_service=trace,synly::input::platform::windows::service=trace",
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
        .context("初始化输入服务日志失败")?;
    Ok(guard)
}
