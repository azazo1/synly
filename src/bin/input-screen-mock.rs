#[cfg(any(target_os = "macos", target_os = "windows"))]
use anyhow::Result;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use clap::Parser;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use synly::input::mock::{ScreenMockOptions, run_screen_mock};

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug, Parser)]
#[command(
    name = "input-screen-mock",
    about = "使用 Slint 无连接虚拟屏幕验证本机输入捕获和返回, 设置可在窗口内调整"
)]
struct Cli {}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(true)
        .init();
    let _cli = Cli::parse();
    run_screen_mock(ScreenMockOptions::default())
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("input-screen-mock 只支持 macOS 和 Windows")
}
