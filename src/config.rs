use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use figment::{
    Figment,
    providers::{Format, Toml},
};
use serde::Deserialize;
use thiserror::Error;

/// Runtime configuration loaded from the Cueback TOML file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Recording storage paths.
    pub storage: StorageConfig,

    /// Device discovery and PCM connection behavior.
    #[serde(default)]
    pub device: DeviceConfig,

    /// Session detection and encoding behavior.
    #[serde(default)]
    pub recording: RecordingConfig,
}

/// Locations used while recording and promoting completed sessions.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    /// Root directory containing `.live` and completed session directories.
    pub recordings_dir: PathBuf,
}

/// Network settings for discovering and connecting to the audio source.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceConfig {
    /// Fixed device address, bypassing PRO DJ LINK discovery when set.
    pub device_address: Option<Ipv4Addr>,

    /// Local address on which to receive PRO DJ LINK announcements.
    pub announcement_bind: SocketAddr,

    /// Time after the last announcement before the device is considered offline.
    #[serde(with = "humantime_serde")]
    pub announcement_timeout: Duration,

    /// TCP port exposed by the device's PCM streaming service.
    pub pcm_port: u16,

    /// Delay before reconnecting after a PCM connection failure.
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

/// Session detection and FFmpeg settings.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingConfig {
    /// Continuous silence required to complete an active recording.
    #[serde(with = "humantime_serde")]
    pub silence_timeout: Duration,

    /// Absolute sample amplitude above which a PCM frame is audible.
    pub silence_threshold: u16,

    /// FFmpeg executable used to encode the PCM stream as FLAC.
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

/// Failure to load or validate Cueback's configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Figment could not read or deserialize the TOML file.
    #[error("failed to load configuration from {path}")]
    Load {
        /// Configuration path selected by the command line.
        path: PathBuf,

        /// Detailed provider or deserialization failure.
        source: Box<figment::Error>,
    },

    /// The configured PCM port is zero.
    #[error("device.pcm_port must be greater than zero")]
    InvalidPcmPort,

    /// The announcement timeout is zero.
    #[error("device.announcement_timeout must be greater than zero")]
    InvalidAnnouncementTimeout,

    /// The reconnect delay is zero.
    #[error("device.reconnect_delay must be greater than zero")]
    InvalidReconnectDelay,

    /// The silence timeout is zero.
    #[error("recording.silence_timeout must be greater than zero")]
    InvalidSilenceTimeout,

    /// The silence threshold cannot be represented as signed 16-bit PCM.
    #[error("recording.silence_threshold cannot exceed {maximum}")]
    InvalidSilenceThreshold {
        /// Largest valid absolute threshold.
        maximum: u16,
    },
}

impl Config {
    /// Load and validate configuration from a TOML file.
    ///
    /// Relative recording paths are resolved from the configuration file's
    /// directory rather than the process working directory.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let base_dir = path.parent().unwrap_or_else(|| Path::new("."));

        Self::extract(Figment::new().merge(Toml::file(path)), path, base_dir)
    }

    #[cfg(test)]
    fn parse(source: &str, path: &Path, base_dir: &Path) -> Result<Self, ConfigError> {
        Self::extract(Figment::new().merge(Toml::string(source)), path, base_dir)
    }

    fn extract(figment: Figment, path: &Path, base_dir: &Path) -> Result<Self, ConfigError> {
        let mut config: Self = figment.extract().map_err(|source| ConfigError::Load {
            path: path.to_owned(),
            source: Box::new(source),
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

    use super::{Config, ConfigError};

    const MINIMAL: &str = r#"
        [storage]
        recordings_dir = "recordings"
    "#;

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
            Err(ConfigError::Load { .. })
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
