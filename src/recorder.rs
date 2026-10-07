use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use chrono::{DateTime, Local};
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    task::JoinHandle,
};
use uuid::Uuid;

use crate::audio::StreamFormat;

/// Failure while encoding or promoting a live recording.
#[derive(Debug, Error)]
pub enum RecorderError {
    #[error("failed to manage the recording file")]
    Io(#[from] std::io::Error),

    #[error("FFmpeg did not expose its standard input")]
    MissingStdin,

    #[error("FFmpeg did not expose its standard error")]
    MissingStderr,

    #[error("FFmpeg diagnostics task failed")]
    Diagnostics(#[from] tokio::task::JoinError),

    #[error("FFmpeg failed with {status}: {diagnostics}")]
    Ffmpeg {
        status: std::process::ExitStatus,
        diagnostics: String,
    },
}

/// One managed FFmpeg process receiving PCM and writing a live FLAC file.
pub struct FlacRecorder {
    child: Child,
    stdin: ChildStdin,
    diagnostics: JoinHandle<Result<Vec<u8>, std::io::Error>>,
    paths: RecordingPaths,
}

#[derive(Debug, Eq, PartialEq)]
struct RecordingPaths {
    temporary: PathBuf,
    completed: PathBuf,
    session_dir: PathBuf,
    master: PathBuf,
}

impl RecordingPaths {
    fn new(recordings_dir: &Path, session_name: &str) -> Self {
        let live_dir = recordings_dir.join(".live");
        let session_dir = recordings_dir.join(session_name);

        Self {
            temporary: live_dir.join(format!("{session_name}.flac.part")),
            completed: live_dir.join(format!("{session_name}.flac")),
            master: session_dir.join("master.flac"),
            session_dir,
        }
    }
}

impl FlacRecorder {
    /// Spawn FFmpeg and prepare a hidden live recording for the PCM format.
    pub async fn start(
        ffmpeg: &Path,
        recordings_dir: &Path,
        format: StreamFormat,
    ) -> Result<Self, RecorderError> {
        let paths =
            RecordingPaths::new(recordings_dir, &session_name(Local::now(), Uuid::new_v4()));
        fs::create_dir_all(
            paths
                .temporary
                .parent()
                .expect("recording path always has a parent"),
        )
        .await?;

        let mut command = Command::new(ffmpeg);
        command
            .args(ffmpeg_args(format))
            .arg(&paths.temporary)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or(RecorderError::MissingStdin)?;
        let mut stderr = child.stderr.take().ok_or(RecorderError::MissingStderr)?;
        let diagnostics = tokio::spawn(async move { diagnostic_tail(&mut stderr).await });

        Ok(Self {
            child,
            stdin,
            diagnostics,
            paths,
        })
    }

    /// Append PCM bytes to the active encoder.
    pub async fn write(&mut self, pcm: &[u8]) -> Result<(), RecorderError> {
        self.stdin.write_all(pcm).await?;
        Ok(())
    }

    /// Drain FFmpeg and atomically promote its completed FLAC into a session directory.
    pub async fn finish(mut self) -> Result<PathBuf, RecorderError> {
        if let Err(error) = self.stdin.shutdown().await
            && error.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(error.into());
        }
        drop(self.stdin);

        let status = self.child.wait().await?;
        let diagnostics = self.diagnostics.await??;
        if !status.success() {
            return Err(RecorderError::Ffmpeg {
                status,
                diagnostics: String::from_utf8_lossy(&diagnostics).trim().to_owned(),
            });
        }

        fs::rename(&self.paths.temporary, &self.paths.completed).await?;

        // Future capture processing runs here while the completed source still
        // lives under `.live`, before its results are published as a session.
        fs::create_dir(&self.paths.session_dir).await?;
        fs::rename(&self.paths.completed, &self.paths.master).await?;

        Ok(self.paths.master)
    }
}

fn session_name(started_at: DateTime<Local>, id: Uuid) -> String {
    let timestamp = started_at.format("%Y-%m-%d-%H%M");
    let id = id.simple().to_string();

    format!("session-{timestamp}-{}", &id[..8])
}

async fn diagnostic_tail(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>, std::io::Error> {
    const LIMIT: usize = 64 * 1024;

    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(output);
        }

        output.extend_from_slice(&buffer[..read]);
        if output.len() > LIMIT {
            output.drain(..output.len() - LIMIT);
        }
    }
}

fn ffmpeg_args(format: StreamFormat) -> Vec<String> {
    [
        "-nostdin".to_owned(),
        "-hide_banner".to_owned(),
        "-loglevel".to_owned(),
        "warning".to_owned(),
        "-f".to_owned(),
        "s16le".to_owned(),
        "-ar".to_owned(),
        format.sample_rate.to_string(),
        "-ac".to_owned(),
        format.channels.to_string(),
        "-i".to_owned(),
        "pipe:0".to_owned(),
        "-map".to_owned(),
        "0:a:0".to_owned(),
        "-c:a".to_owned(),
        "flac".to_owned(),
        "-compression_level".to_owned(),
        "5".to_owned(),
        "-f".to_owned(),
        "flac".to_owned(),
        "-n".to_owned(),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use chrono::{Local, TimeZone};
    use uuid::Uuid;

    use crate::audio::StreamFormat;

    use super::{RecordingPaths, ffmpeg_args, session_name};

    #[test]
    fn keeps_live_files_hidden_until_promotion() {
        let paths =
            RecordingPaths::new(Path::new("/recordings"), "session-2026-10-06-2132-a1b2c3d4");

        assert_eq!(
            paths.temporary,
            PathBuf::from("/recordings/.live/session-2026-10-06-2132-a1b2c3d4.flac.part")
        );
        assert_eq!(
            paths.completed,
            PathBuf::from("/recordings/.live/session-2026-10-06-2132-a1b2c3d4.flac")
        );
        assert_eq!(
            paths.master,
            PathBuf::from("/recordings/session-2026-10-06-2132-a1b2c3d4/master.flac")
        );
    }

    #[test]
    fn names_sessions_with_their_local_start_time() {
        let started_at = Local
            .with_ymd_and_hms(2026, 10, 6, 21, 32, 45)
            .single()
            .unwrap();
        let id = Uuid::parse_str("a1b2c3d4-0000-0000-0000-000000000000").unwrap();

        assert_eq!(
            session_name(started_at, id),
            "session-2026-10-06-2132-a1b2c3d4"
        );
    }

    #[test]
    fn configures_ffmpeg_for_the_announced_pcm_format() {
        let args = ffmpeg_args(StreamFormat {
            sample_rate: 44_100,
            channels: 2,
            max_frames_per_block: 512,
        });

        assert!(args.windows(2).any(|args| args == ["-ar", "44100"]));
        assert!(args.windows(2).any(|args| args == ["-ac", "2"]));
        assert!(args.windows(2).any(|args| args == ["-c:a", "flac"]));
    }
}
