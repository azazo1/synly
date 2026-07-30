#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
use anyhow::{Context, Result};
#[cfg(windows)]
use clap::Parser;

#[cfg(windows)]
#[derive(Parser)]
struct AgentCli {
    #[arg(long)]
    command_pipe: String,
    #[arg(long)]
    event_pipe: String,
    #[arg(long)]
    token: String,
    #[arg(long)]
    parent_pid: u32,
}

#[cfg(windows)]
fn main() -> Result<()> {
    let _tracing_guard = synly::input::init_windows_agent_tracing()?;
    let cli = AgentCli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("synly-input-agent")
        .enable_all()
        .build()
        .context("failed to create input agent runtime")?;
    runtime.block_on(synly::input::run_agent(
        cli.command_pipe,
        cli.event_pipe,
        cli.token,
        cli.parent_pid,
    ))
}

#[cfg(not(windows))]
fn main() {
    tracing::error!("synly-input-agent 只支持 Windows");
}
