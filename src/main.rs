mod app;
mod cli;
mod clipboard;
mod config;
mod crypto;
mod discovery;
mod path_expand;
mod protocol;
mod startup_tui;
mod sync;

use anyhow::Result;
use clap::Parser;
use console::style;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let mut config = config::SynlyConfig::load_or_create()?;
    let options = cli::collect_runtime_options(cli, &config)?;
    println!();
    println!("{}", style("本次同步确认").bold());
    for line in options.workspace.local_human_lines(options.sync_clipboard) {
        println!("{line}");
    }
    if options.workspace.incoming_root.is_some() {
        println!("删除同步: {}", cli::sync_delete_label(options.sync_delete));
    }
    app::run(&mut config, options).await
}
