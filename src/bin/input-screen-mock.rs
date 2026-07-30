#[cfg(any(target_os = "macos", target_os = "windows"))]
use anyhow::Result;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use clap::Parser;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use std::str::FromStr;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use synly::input::mock::{ScreenMockOptions, run_screen_mock};
#[cfg(any(target_os = "macos", target_os = "windows"))]
use synly::input::{Hotkey, ScreenEdge};

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug, Parser)]
#[command(
    name = "input-screen-mock",
    about = "使用 Slint 无连接虚拟屏幕验证本机输入捕获和返回"
)]
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

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(true)
        .init();
    let cli = Cli::parse();
    run_screen_mock(ScreenMockOptions {
        edge: cli.edge,
        hotkey: Hotkey::from_str(&cli.hotkey)?,
        width: cli.width,
        height: cli.height,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("input-screen-mock 只支持 macOS 和 Windows")
}
