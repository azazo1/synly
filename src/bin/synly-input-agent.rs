#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use anyhow::{Context, Result};
#[cfg(windows)]
use clap::Parser;

#[cfg(windows)]
#[derive(Parser)]
struct AgentCli {
    #[arg(long)]
    pipe: String,
    #[arg(long)]
    token: String,
    #[arg(long)]
    parent_pid: u32,
}

#[cfg(windows)]
fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(true)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize input agent tracing: {error}"))?;
    let cli = AgentCli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("synly-input-agent")
        .enable_all()
        .build()
        .context("failed to create input agent runtime")?;
    runtime.block_on(synly::input::run_agent(
        cli.pipe,
        cli.token,
        cli.parent_pid,
    ))
}

#[cfg(not(windows))]
fn main() {
    tracing::error!("synly-input-agent 只支持 Windows");
}
