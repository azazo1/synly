#[cfg(target_os = "macos")]
use anyhow::Result;
#[cfg(target_os = "macos")]
use clap::Parser;
#[cfg(target_os = "macos")]
use std::str::FromStr;
#[cfg(target_os = "macos")]
use synly::input::mock::{MacosMockOptions, run_macos_mock};
#[cfg(target_os = "macos")]
use synly::input::{Hotkey, ScreenEdge};

#[cfg(target_os = "macos")]
#[derive(Debug, Parser)]
#[command(name = "input-macos-mock", about = "使用 GUI mock 验证 macOS 输入捕获和虚拟屏幕切换")]
struct Cli {
    #[arg(long, value_enum, default_value = "right", help = "从本机接入 mock 的屏幕边缘")]
    edge: ScreenEdge,
    #[arg(long, default_value = Hotkey::DEFAULT, help = "恢复本机控制的紧急热键")]
    hotkey: String,
    #[arg(long, default_value_t = 1280, help = "mock 虚拟屏幕宽度")]
    width: i32,
    #[arg(long, default_value_t = 720, help = "mock 虚拟屏幕高度")]
    height: i32,
}

#[cfg(target_os = "macos")]
fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(true)
        .init();
    let cli = Cli::parse();
    run_macos_mock(MacosMockOptions {
        edge: cli.edge,
        hotkey: Hotkey::from_str(&cli.hotkey)?,
        width: cli.width,
        height: cli.height,
    })
}

#[cfg(not(target_os = "macos"))]
fn main() {
    tracing::error!("input-macos-mock 只支持 macOS");
}
