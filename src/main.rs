mod app;
mod cli;
mod clipboard;
mod config;
mod crypto;
mod discovery;
mod protocol;
mod sync;

use anyhow::Result;
use clap::Parser;
use console::style;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let device = config::DeviceConfig::load_or_create()?;
    let options = cli::collect_runtime_options(cli, &device)?;
    println!();
    println!("{}", style("本次同步确认").bold());
    for line in options.workspace.local_human_lines(options.sync_clipboard) {
        println!("{line}");
    }
    if options.workspace.incoming_root.is_some() {
        println!("删除同步: {}", cli::sync_delete_label(options.sync_delete));
    }
    app::run(device, options).await
}
