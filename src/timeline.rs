use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use serde::Serialize;
use thiserror::Error;
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncWriteExt, BufWriter},
    time::Instant,
};

use crate::{
    device::{ControlEvent, ControlStreamEvent, ControlStreamInfo},
    session::{CompletedSession, SessionEndReason},
    storage::SessionPaths,
};

const BUFFER_CAPACITY: usize = 64 * 1024;
const FLUSH_BYTES: usize = 64 * 1024;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Failure while writing or completing a session timeline.
#[derive(Debug, Error)]
pub enum TimelineError {
    /// The timeline file could not be opened, written, synchronized, or renamed.
    #[error("failed to manage the timeline file")]
    Io(#[from] std::io::Error),

    /// A timeline record could not be encoded as JSON.
    #[error("failed to encode a timeline record")]
    Serialize(#[from] serde_json::Error),
}

/// Buffered append-only journal for one live session.
pub struct TimelineRecorder {
    writer: BufWriter<File>,
    part_path: std::path::PathBuf,
    completed_path: std::path::PathBuf,
    unflushed_bytes: usize,
    last_flush: Instant,
}

impl TimelineRecorder {
    /// Open a new timeline and write its session header and control pre-roll.
    pub async fn start(
        paths: &SessionPaths,
        audio_started_at_frame: u64,
        remote: Option<&ControlStreamInfo>,
        pre_roll: impl IntoIterator<Item = ControlEvent>,
    ) -> Result<Self, TimelineError> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&paths.timeline_part)
            .await?;
        let mut recorder = Self {
            writer: BufWriter::with_capacity(BUFFER_CAPACITY, file),
            part_path: paths.timeline_part.clone(),
            completed_path: paths.timeline_completed.clone(),
            unflushed_bytes: 0,
            last_flush: Instant::now(),
        };

        recorder
            .write_record(&TimelineRecord::Started {
                version: 1,
                recorded_at: now(),
                audio_started_at_frame,
                remote: remote.map(RemoteMetadata::from),
            })
            .await?;
        for event in pre_roll {
            recorder.write_control(&event).await?;
        }

        Ok(recorder)
    }

    /// Append one control-stream lifecycle event or control callback.
    pub async fn write(&mut self, event: &ControlStreamEvent) -> Result<(), TimelineError> {
        match event {
            ControlStreamEvent::Connected(info) => {
                self.write_record(&TimelineRecord::RemoteConnected {
                    observed_at: now(),
                    remote: RemoteMetadata::from(info),
                })
                .await
            }
            ControlStreamEvent::Control(event) => self.write_control(event).await,
            ControlStreamEvent::Disconnected => {
                self.write_record(&TimelineRecord::RemoteDisconnected { observed_at: now() })
                    .await
            }
        }
    }

    /// Flush, synchronize, and complete the timeline after its session ends.
    pub async fn finish(mut self, completed: CompletedSession) -> Result<(), TimelineError> {
        self.write_record(&TimelineRecord::Ended {
            recorded_at: now(),
            audio_ended_at_frame: completed.audio_ended_at_frame,
            recording_ended_at_frame: completed.recording_ended_at_frame,
            reason: end_reason(completed.reason),
        })
        .await?;
        self.flush().await?;
        drop(self.writer);
        fs::rename(&self.part_path, &self.completed_path).await?;

        Ok(())
    }

    async fn write_control(&mut self, event: &ControlEvent) -> Result<(), TimelineError> {
        self.write_record(&TimelineRecord::Control {
            device_timestamp_us: event.timestamp_us,
            source: event.source,
            flags: event.flags,
            key_code: event.key_code,
            operation: event.operation,
            channel: event.channel,
            value: event.value,
            float_bits: event.float_bits,
            auxiliary: event.auxiliary,
        })
        .await
    }

    async fn write_record(&mut self, record: &TimelineRecord) -> Result<(), TimelineError> {
        let mut encoded = serde_json::to_vec(record)?;
        encoded.push(b'\n');
        self.writer.write_all(&encoded).await?;
        self.unflushed_bytes += encoded.len();

        if self.unflushed_bytes >= FLUSH_BYTES || self.last_flush.elapsed() >= FLUSH_INTERVAL {
            self.flush().await?;
        }

        Ok(())
    }

    async fn flush(&mut self) -> Result<(), TimelineError> {
        self.writer.flush().await?;
        self.writer.get_ref().sync_data().await?;
        self.unflushed_bytes = 0;
        self.last_flush = Instant::now();

        Ok(())
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum TimelineRecord {
    Started {
        version: u32,
        recorded_at: String,
        audio_started_at_frame: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        remote: Option<RemoteMetadata>,
    },
    RemoteConnected {
        observed_at: String,
        remote: RemoteMetadata,
    },
    RemoteDisconnected {
        observed_at: String,
    },
    Control {
        device_timestamp_us: u64,
        source: u8,
        flags: u8,
        key_code: u16,
        operation: u8,
        channel: u8,
        value: i32,
        float_bits: u32,
        auxiliary: i32,
    },
    Ended {
        recorded_at: String,
        audio_ended_at_frame: u64,
        recording_ended_at_frame: u64,
        reason: &'static str,
    },
}

#[derive(Serialize)]
struct RemoteMetadata {
    rbp_build_prefix: String,
    firmware_major: u16,
    firmware_minor: u16,
    schema_revision: u32,
    schema_crc32: u32,
    control_count: u32,
}

impl From<&ControlStreamInfo> for RemoteMetadata {
    fn from(info: &ControlStreamInfo) -> Self {
        Self {
            rbp_build_prefix: info
                .rbp_build_prefix
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            firmware_major: info.firmware_major,
            firmware_minor: info.firmware_minor,
            schema_revision: info.schema_revision,
            schema_crc32: info.schema_crc32,
            control_count: info.control_count,
        }
    }
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn end_reason(reason: SessionEndReason) -> &'static str {
    match reason {
        SessionEndReason::Silence => "silence",
        SessionEndReason::Discontinuity => "discontinuity",
        SessionEndReason::Shutdown => "shutdown",
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use chrono::Local;
    use serde_json::Value;
    use tokio::fs;
    use uuid::Uuid;

    use crate::{
        device::ControlEvent,
        session::{CompletedSession, SessionEndReason},
        storage::SessionPaths,
    };

    use super::TimelineRecorder;

    #[tokio::test]
    async fn completes_a_json_lines_timeline_with_raw_control_values() {
        let root = std::env::temp_dir().join(format!("cueback-timeline-{}", Uuid::new_v4()));
        let paths = SessionPaths::create(&root, Local::now(), Uuid::new_v4());
        paths.prepare().await.unwrap();
        let event = ControlEvent {
            key_code: 0x4101,
            operation: 2,
            channel: 1,
            value: -42,
            float_bits: 1.0_f32.to_bits(),
            auxiliary: 7,
            source: 1,
            flags: 2,
            timestamp_us: 123_456,
        };

        let recorder = TimelineRecorder::start(&paths, 512, None, [event])
            .await
            .unwrap();
        recorder
            .finish(CompletedSession {
                started_at_frame: 512,
                audio_ended_at_frame: 1024,
                recording_ended_at_frame: 2048,
                reason: SessionEndReason::Silence,
            })
            .await
            .unwrap();

        assert!(!Path::new(&paths.timeline_part).exists());
        let contents = fs::read_to_string(&paths.timeline_completed).await.unwrap();
        let records = contents
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["type"], "started");
        assert_eq!(records[0]["audio_started_at_frame"], 512);
        assert_eq!(records[1]["type"], "control");
        assert_eq!(records[1]["device_timestamp_us"], 123_456);
        assert_eq!(records[1]["value"], -42);
        assert_eq!(records[2]["type"], "ended");
        assert_eq!(records[2]["reason"], "silence");

        fs::remove_dir_all(root).await.unwrap();
    }
}
