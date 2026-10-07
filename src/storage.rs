use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use tokio::fs;
use uuid::Uuid;

/// Paths for the live, completed, and promoted artifacts of one session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionPaths {
    /// Stable directory name shared by every session artifact.
    pub name: String,

    /// FLAC being written by FFmpeg.
    pub audio_part: PathBuf,

    /// Completed FLAC awaiting session promotion.
    pub audio_completed: PathBuf,

    /// JSONL timeline being written during capture.
    pub timeline_part: PathBuf,

    /// Completed timeline awaiting session promotion.
    pub timeline_completed: PathBuf,

    /// Final session directory.
    pub session_dir: PathBuf,

    /// Published lossless master recording.
    pub master: PathBuf,

    /// Published control timeline.
    pub timeline: PathBuf,

    live_dir: PathBuf,
}

impl SessionPaths {
    /// Create paths using the session's local start time and a random identity.
    pub fn create(recordings_dir: &Path, started_at: DateTime<Local>, id: Uuid) -> Self {
        Self::new(recordings_dir, &session_name(started_at, id))
    }

    /// Ensure the hidden live workspace exists before opening either recorder.
    pub async fn prepare(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.live_dir).await
    }

    /// Publish both completed artifacts into the final session directory.
    ///
    /// If the second move fails, the first is moved back into the live workspace
    /// so a partially published session is not left behind.
    pub async fn promote(&self) -> std::io::Result<PathBuf> {
        fs::create_dir(&self.session_dir).await?;

        if let Err(error) = fs::rename(&self.audio_completed, &self.master).await {
            let _ = fs::remove_dir(&self.session_dir).await;
            return Err(error);
        }

        if let Err(error) = fs::rename(&self.timeline_completed, &self.timeline).await {
            let _ = fs::rename(&self.master, &self.audio_completed).await;
            let _ = fs::remove_dir(&self.session_dir).await;
            return Err(error);
        }

        Ok(self.session_dir.clone())
    }

    fn new(recordings_dir: &Path, name: &str) -> Self {
        let live_dir = recordings_dir.join(".live");
        let session_dir = recordings_dir.join(name);

        Self {
            name: name.to_owned(),
            audio_part: live_dir.join(format!("{name}.flac.part")),
            audio_completed: live_dir.join(format!("{name}.flac")),
            timeline_part: live_dir.join(format!("{name}.timeline.jsonl.part")),
            timeline_completed: live_dir.join(format!("{name}.timeline.jsonl")),
            master: session_dir.join("master.flac"),
            timeline: session_dir.join("timeline.jsonl"),
            session_dir,
            live_dir,
        }
    }
}

fn session_name(started_at: DateTime<Local>, id: Uuid) -> String {
    let timestamp = started_at.format("%Y-%m-%d-%H%M");
    let id = id.simple().to_string();

    format!("session-{timestamp}-{}", &id[..8])
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use chrono::{Local, TimeZone};
    use uuid::Uuid;

    use super::SessionPaths;

    #[test]
    fn assigns_companion_live_and_promoted_paths() {
        let started_at = Local
            .with_ymd_and_hms(2026, 10, 6, 21, 32, 45)
            .single()
            .unwrap();
        let id = Uuid::parse_str("a1b2c3d4-0000-0000-0000-000000000000").unwrap();
        let paths = SessionPaths::create(Path::new("/recordings"), started_at, id);

        assert_eq!(paths.name, "session-2026-10-06-2132-a1b2c3d4");
        assert_eq!(
            paths.audio_part,
            PathBuf::from("/recordings/.live/session-2026-10-06-2132-a1b2c3d4.flac.part")
        );
        assert_eq!(
            paths.timeline_part,
            PathBuf::from("/recordings/.live/session-2026-10-06-2132-a1b2c3d4.timeline.jsonl.part")
        );
        assert_eq!(
            paths.master,
            PathBuf::from("/recordings/session-2026-10-06-2132-a1b2c3d4/master.flac")
        );
        assert_eq!(
            paths.timeline,
            PathBuf::from("/recordings/session-2026-10-06-2132-a1b2c3d4/timeline.jsonl")
        );
    }
}
