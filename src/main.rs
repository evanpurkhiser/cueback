use anyhow::Result;
use clap::Parser;
use cueback::config::{Cli, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    cueback::app::run(config).await
}
