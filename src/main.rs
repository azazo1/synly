mod app;
mod audio;
mod cli;
mod clipboard;
mod config;
mod crypto;
mod discovery;
mod input;
mod path_expand;
mod protocol;
mod startup_tui;
mod system_notification;
mod tracing_utils;
mod sync;

use anyhow::Result;
use clap::Parser;
use console::style;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let mut config = config::SynlyConfig::load_or_create()?;
    let options = cli::collect_runtime_options(cli, &config)?;
    input::ensure_platform_supported(options.input_mode)?;
    tracing_utils::init_tracing();
    println!();
    println!("{}", style("本次同步确认").bold());
    if let Some(instance_name) = options.instance_name.as_deref() {
        println!("当前实例: {instance_name}");
    }
    for line in options
        .workspace
        .local_summary_lines_with_input(
            options.clipboard_mode,
            options.audio_mode,
            options.input_mode,
        )
    {
        println!("{line}");
    }
    if options.workspace.incoming_root.is_some() {
        println!("删除同步: {}", cli::sync_delete_label(options.sync_delete));
    }
    app::run(&mut config, options).await
}
