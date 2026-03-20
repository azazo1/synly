mod app;
mod cli;
mod config;
mod crypto;
mod discovery;
mod protocol;
mod sync;

use anyhow::Result;
use clap::Parser;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    let device = config::DeviceConfig::load_or_create()?;
    let options = cli::collect_runtime_options(cli, &device)?;
    app::run(device, options).await
}
