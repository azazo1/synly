use anyhow::{Result, anyhow};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;
use synly::input::receiver_mock::{
    ControllerMockOptions, ReceiverMockOptions, run_controller_mock, run_receiver_mock,
};
use synly::input::{Hotkey, ScreenEdge};

#[derive(Debug, Parser)]
#[command(
    name = "input-receiver-mock",
    about = "使用 mock 控制端验证真实系统输入接收端"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Receive {
        #[arg(long, default_value = "0.0.0.0:59679")]
        listen: SocketAddr,
        #[arg(long, default_value = Hotkey::DEFAULT)]
        hotkey: String,
    },
    Control {
        address: SocketAddr,
        #[arg(long, value_enum, default_value = "right")]
        edge: ScreenEdge,
        #[arg(long, default_value_t = 60)]
        motion_steps: u16,
        #[arg(long, default_value_t = 8)]
        step_delay_ms: u64,
        #[arg(long)]
        skip_click: bool,
        #[arg(long)]
        skip_keyboard: bool,
        #[arg(long)]
        skip_wheel: bool,
    },
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(true)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .map_err(|error| anyhow!("无法初始化输入接收 mock 日志: {error}"))?;

    match Cli::parse().command {
        Command::Receive { listen, hotkey } => {
            run_receiver_mock(ReceiverMockOptions {
                listen,
                hotkey: Hotkey::from_str(&hotkey)?,
            })
            .await
        }
        Command::Control {
            address,
            edge,
            motion_steps,
            step_delay_ms,
            skip_click,
            skip_keyboard,
            skip_wheel,
        } => {
            run_controller_mock(ControllerMockOptions {
                address,
                edge,
                motion_steps,
                step_delay: Duration::from_millis(step_delay_ms),
                inject_click: !skip_click,
                inject_keyboard: !skip_keyboard,
                inject_wheel: !skip_wheel,
            })
            .await
        }
    }
}
