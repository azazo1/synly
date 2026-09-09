#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

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
mod paths;
mod protocol;
mod reconnect;
mod runtime_control;
mod runtime_options;
mod settings;
mod session;
mod system_notification;
mod tracing_utils;
mod sync;
mod update;

#[cfg(windows)]
mod windows_input_agent;

use anyhow::Result;
use clap::Parser;
use synly::input;

/// 当前构建版本. 日常开发为 `dev-build`, 发布构建由 `scripts/build-version.sh` 或 CI 注入.
const BUILD_VERSION: &str = env!("SYNLY_BUILD_VERSION");

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    if let Some(command) = &cli.command
        && matches!(
            command,
            cli::Command::InputAgent { .. }
                | cli::Command::Service { .. }
                | cli::Command::ServiceEntry
        )
    {
        return run_internal_command(command);
    }
    // 先建立日志, 否则配置读取失败时既没有日志也没有可见的错误输出.
    let _tracing_guard = tracing_utils::init_tracing(tracing_utils::BOOTSTRAP_FILTER)?;
    match paths::log_file_path() {
        Ok(log_path) => {
            tracing::info!(version = BUILD_VERSION, log = %log_path.display(), "Synly 启动")
        }
        Err(_) => tracing::info!(version = BUILD_VERSION, "Synly 启动"),
    }
    let mut config = match config::SynlyConfig::load_or_create() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!("加载配置失败, 无法启动: {error:#}");
            return Err(error);
        }
    };
    let configured_filter = config.ui.log_level.as_filter();
    if let Err(error) = tracing_utils::apply_configured_level(configured_filter) {
        tracing::warn!("无法应用日志等级 {configured_filter}, 继续使用默认等级: {error:#}");
    }
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
    match gui::run(config, session_override.is_some())? {
        gui::GuiExit::Quit => Ok(()),
        gui::GuiExit::Restart { exe } => {
            drop(_tracing_guard);
            crate::update::relaunch(exe)
        }
    }
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
        cli::Command::Service { action } => {
            #[cfg(windows)]
            {
                match action {
                    cli::ServiceAction::Install => {
                        input::install_windows_input_service()?;
                        println!("Synly 输入服务已安装并启动");
                        Ok(())
                    }
                    cli::ServiceAction::Uninstall => {
                        input::uninstall_windows_input_service()?;
                        println!("Synly 输入服务已卸载");
                        Ok(())
                    }
                    cli::ServiceAction::Status => {
                        let status = input::windows_input_service_status()?;
                        println!("Synly 输入服务状态: {}", status.label());
                        Ok(())
                    }
                }
            }
            #[cfg(not(windows))]
            {
                let _ = action;
                anyhow::bail!("输入服务管理命令仅支持 Windows")
            }
        }
        cli::Command::ServiceEntry => {
            #[cfg(windows)]
            {
                let _tracing_guard = input::init_windows_service_tracing()?;
                input::run_windows_input_service()
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("输入服务入口仅支持 Windows")
            }
        }
    }
}
