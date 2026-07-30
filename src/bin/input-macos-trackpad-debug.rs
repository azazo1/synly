#[cfg(target_os = "macos")]
use anyhow::Result;
#[cfg(target_os = "macos")]
use clap::Parser;
#[cfg(target_os = "macos")]
use synly::input::trackpad_debug::run_trackpad_debug;

#[cfg(target_os = "macos")]
#[derive(Debug, Parser)]
#[command(
    name = "input-macos-trackpad-debug",
    about = "完全捕获 macOS trackpad 输入并输出控制日志"
)]
struct Cli {}

#[cfg(target_os = "macos")]
fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(true)
        .init();
    let _cli = Cli::parse();
    run_trackpad_debug()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    tracing_subscriber::fmt().with_target(true).init();
    tracing::error!("input-macos-trackpad-debug 只支持 macOS");
}
