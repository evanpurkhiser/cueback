use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use clap::{Parser, ValueHint};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Parser)]
#[command(
    name = "cueback",
    version,
    about = "The flight recorder for your DJ sets"
)]
pub struct Cli {
    /// TOML configuration file.
    #[arg(
        short,
        long,
        default_value = "cueback.toml",
        value_hint = ValueHint::FilePath
    )]
    pub config: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub storage: StorageConfig,

    #[serde(default)]
    pub device: DeviceConfig,

    #[serde(default)]
    pub recording: RecordingConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub recordings_dir: PathBuf,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceConfig {
    pub device_address: Option<Ipv4Addr>,
    pub announcement_bind: SocketAddr,

    #[serde(with = "humantime_serde")]
    pub announcement_timeout: Duration,

    pub pcm_port: u16,

    #[serde(with = "humantime_serde")]
    pub reconnect_delay: Duration,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            device_address: None,
            announcement_bind: "0.0.0.0:50000"
                .parse()
                .expect("default announcement address is valid"),
            announcement_timeout: Duration::from_secs(10),
            pcm_port: 7355,
            reconnect_delay: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingConfig {
    #[serde(with = "humantime_serde")]
    pub silence_timeout: Duration,

    pub silence_threshold: u16,
    pub ffmpeg: PathBuf,
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            silence_timeout: Duration::from_secs(300),
            silence_threshold: 0,
            ffmpeg: PathBuf::from("ffmpeg"),
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration from {path}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to parse configuration from {path}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },

    #[error("device.pcm_port must be greater than zero")]
    InvalidPcmPort,

    #[error("device.announcement_timeout must be greater than zero")]
    InvalidAnnouncementTimeout,

    #[error("device.reconnect_delay must be greater than zero")]
    InvalidReconnectDelay,

    #[error("recording.silence_timeout must be greater than zero")]
    InvalidSilenceTimeout,

    #[error("recording.silence_threshold cannot exceed {maximum}")]
    InvalidSilenceThreshold { maximum: u16 },
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let source = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));

        Self::parse(&source, path, base_dir)
    }

    fn parse(source: &str, path: &Path, base_dir: &Path) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(source).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;

        config.storage.recordings_dir = resolve_path(base_dir, config.storage.recordings_dir);
        config.validate()?;

        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.device.pcm_port == 0 {
            return Err(ConfigError::InvalidPcmPort);
        }
        if self.device.announcement_timeout.is_zero() {
            return Err(ConfigError::InvalidAnnouncementTimeout);
        }
        if self.device.reconnect_delay.is_zero() {
            return Err(ConfigError::InvalidReconnectDelay);
        }
        if self.recording.silence_timeout.is_zero() {
            return Err(ConfigError::InvalidSilenceTimeout);
        }
        if self.recording.silence_threshold > i16::MAX as u16 {
            return Err(ConfigError::InvalidSilenceThreshold {
                maximum: i16::MAX as u16,
            });
        }

        Ok(())
    }
}

fn resolve_path(base_dir: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }

    base_dir.join(path)
}

#[cfg(test)]
mod tests {
    use std::{
        net::Ipv4Addr,
        path::{Path, PathBuf},
        time::Duration,
    };

    use clap::Parser;

    use super::{Cli, Config, ConfigError};

    const MINIMAL: &str = r#"
        [storage]
        recordings_dir = "recordings"
    "#;

    #[test]
    fn cli_accepts_a_config_path() {
        let cli = Cli::try_parse_from(["cueback", "--config", "/etc/cueback.toml"]).unwrap();

        assert_eq!(cli.config, PathBuf::from("/etc/cueback.toml"));
    }

    #[test]
    fn parses_the_example_configuration() {
        Config::parse(
            include_str!("../cueback.example.toml"),
            Path::new("cueback.example.toml"),
            Path::new("."),
        )
        .unwrap();
    }

    #[test]
    fn applies_defaults_and_resolves_the_recordings_path_from_the_config() {
        let config =
            Config::parse(MINIMAL, Path::new("config.toml"), Path::new("/srv/cueback")).unwrap();

        assert_eq!(
            config.storage.recordings_dir,
            PathBuf::from("/srv/cueback/recordings")
        );
        assert_eq!(config.device.device_address, None);
        assert_eq!(config.device.pcm_port, 7355);
        assert_eq!(config.recording.silence_timeout, Duration::from_secs(300));
    }

    #[test]
    fn parses_device_and_recording_settings() {
        let config = Config::parse(
            r#"
                [storage]
                recordings_dir = "/var/lib/cueback/recordings"

                [device]
                device_address = "10.0.0.42"
                announcement_timeout = "15s"

                [recording]
                silence_timeout = "2m 30s"
                silence_threshold = 8
            "#,
            Path::new("config.toml"),
            Path::new("."),
        )
        .unwrap();

        assert_eq!(
            config.device.device_address,
            Some(Ipv4Addr::new(10, 0, 0, 42))
        );
        assert_eq!(config.device.announcement_timeout, Duration::from_secs(15));
        assert_eq!(config.recording.silence_timeout, Duration::from_secs(150));
        assert_eq!(config.recording.silence_threshold, 8);
    }

    #[test]
    fn rejects_unknown_fields() {
        let source = format!("{MINIMAL}\n[recording]\nsilence_timout = \"5m\"");

        assert!(matches!(
            Config::parse(&source, Path::new("config.toml"), Path::new(".")),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn rejects_thresholds_outside_signed_pcm() {
        let source = format!("{MINIMAL}\n[recording]\nsilence_threshold = 32768");

        assert!(matches!(
            Config::parse(&source, Path::new("config.toml"), Path::new(".")),
            Err(ConfigError::InvalidSilenceThreshold { .. })
        ));
    }
}
