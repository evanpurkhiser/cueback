use std::path::PathBuf;

use clap::{Parser, ValueHint};

/// Command-line arguments accepted by Cueback.
#[derive(Debug, Parser)]
#[command(
    name = "cueback",
    version,
    about = "The flight recorder for your DJ sets"
)]
pub struct Cli {
    /// Read configuration from this TOML file.
    #[arg(
        short,
        long,
        default_value = "cueback.toml",
        value_hint = ValueHint::FilePath
    )]
    pub config: PathBuf,
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::Cli;

    #[test]
    fn accepts_a_config_path() {
        let cli = Cli::try_parse_from(["cueback", "--config", "/etc/cueback.toml"]).unwrap();

        assert_eq!(cli.config, PathBuf::from("/etc/cueback.toml"));
    }
}
