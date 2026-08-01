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
mod host;
mod path_expand;
mod protocol;
mod reconnect;
mod runtime_control;
mod runtime_options;
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

/// 当前构建版本, 由 build.rs 根据最近版本 tag 与工作区状态生成.
const BUILD_VERSION: &str = env!("SYNLY_BUILD_VERSION");

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    if let Some(command) = &cli.command
        && matches!(command, cli::Command::InputAgent { .. })
    {
        return run_internal_command(command);
    }
    let mut config = config::SynlyConfig::load_or_create()?;
    let _tracing_guard = tracing_utils::init_tracing(config.ui.log_level.as_filter())?;
    let session_override = cli.session_override();
    if let Some(session) = &session_override {
        session.apply_to(&mut config.runtime, cli.headless);
    }
    if cli.headless {
        let options = runtime_options::runtime_options_from_config(&config, None, true)?;
        #[cfg(windows)]
        if config.runtime.input.elevate_on_start {
            windows_input_agent::request_startup_elevation()?;
        }
        input::ensure_platform_supported(options.input_mode)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .thread_name("synly-headless")
            .build()?;
        let (_, commands) = tokio::sync::mpsc::unbounded_channel();
        return runtime.block_on(app::run(config, options, commands));
    }
    gui::run(config, session_override.is_some())
}

fn run_internal_command(command: &cli::Command) -> Result<()> {
    match command {
        cli::Command::InputAgent {
            command_pipe,
            event_pipe,
            token,
            parent_pid,
        } => {
            #[cfg(windows)]
            {
                let _tracing_guard = input::init_windows_agent_tracing()?;
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .thread_name("synly-input-agent")
                    .enable_all()
                    .build()?;
                runtime.block_on(input::run_agent(
                    command_pipe.clone(),
                    event_pipe.clone(),
                    token.clone(),
                    *parent_pid,
                ))
            }
            #[cfg(not(windows))]
            {
                let _ = (command_pipe, event_pipe, token, parent_pid);
                anyhow::bail!("Windows input agent internal command is only available on Windows");
            }
        }
        cli::Command::Host | cli::Command::Join { .. } => {
            anyhow::bail!("host/join 子命令不是内部命令")
        }
    }
}
