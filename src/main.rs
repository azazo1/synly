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
    let mut config = config::SynlyConfig::load_or_create()?;
    let _tracing_guard = tracing_utils::init_tracing(config.ui.log_level.as_filter())?;
    if cli.headless {
        let options = cli::collect_runtime_options(cli, &config)?;
        input::ensure_platform_supported(options.input_mode)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("synly-headless")
            .build()?;
        return runtime.block_on(app::run(&mut config, options));
    }
    gui::run(config)
}
