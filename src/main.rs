use anyhow::Result;
use clap::Parser;
use cueback::{cli::Cli, config::Config};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    cueback::app::run(config).await
}
