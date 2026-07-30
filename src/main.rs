mod app;
mod autostart;
mod audio;
mod cli;
mod clipboard;
mod config;
mod core;
mod crypto;
mod discovery;
mod gui;
mod path_expand;
mod protocol;
mod runtime_control;
mod settings;
mod session;
mod system_notification;
mod tracing_utils;
mod sync;

#[cfg(windows)]
mod windows_input_agent;

use anyhow::Result;
use clap::Parser;
use synly::input;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    if let Some(command) = cli.internal_command.as_ref() {
        return run_internal_command(command);
    }
    let mut config = config::SynlyConfig::load_or_create()?;
    let _tracing_guard = tracing_utils::init_tracing(config.ui.log_level.as_filter())?;
    if cli.headless {
        let options = cli::collect_runtime_options(cli, &config)?;
        input::ensure_platform_supported(options.input_mode)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("synly-headless")
            .build()?;
        return runtime.block_on(app::run(&mut config, options));
    }
    gui::run(config)
}

fn run_internal_command(command: &cli::InternalCommand) -> Result<()> {
    match command {
        cli::InternalCommand::InputAgent {
            pipe,
            token,
            parent_pid,
        } => {
            #[cfg(windows)]
            {
                tracing_subscriber::fmt()
                    .with_target(true)
                    .with_ansi(false)
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                    )
                    .try_init()
                    .map_err(|error| {
                        anyhow::anyhow!("failed to initialize input agent tracing: {error}")
                    })?;
                tracing::trace!(%pipe, parent_pid, "Windows 输入代理内部子进程入口已启动"); // to remove
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("synly-input-agent")
                    .enable_all()
                    .build()?;
                return runtime.block_on(input::run_agent(
                    pipe.clone(),
                    token.clone(),
                    *parent_pid,
                ));
            }
            #[cfg(not(windows))]
            {
                let _ = (pipe, token, parent_pid);
                anyhow::bail!("Windows input agent internal command is only available on Windows");
            }
        }
    }
}
