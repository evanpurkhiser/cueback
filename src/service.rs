use std::{path::PathBuf, time::Duration};

use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{
    audio::{AnalyzeError, StreamFormat, analyze},
    device::Event,
    recorder::{FlacRecorder, RecorderError},
    session::{SessionEndReason, SessionTracker, SessionTransition},
};

/// Failure while translating device events into a recording.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// PCM analysis failed.
    #[error(transparent)]
    Analyze(#[from] AnalyzeError),

    /// FLAC recording or promotion failed.
    #[error(transparent)]
    Recorder(#[from] RecorderError),

    /// PCM arrived before its connection established a stream format.
    #[error("PCM arrived without an active audio connection")]
    MissingFormat,
}

/// Owns session detection and the active recorder for one device event stream.
pub struct CaptureService {
    /// Encoder executable used for each live recording.
    ffmpeg: PathBuf,

    /// Root containing live artifacts and promoted session directories.
    recordings_dir: PathBuf,

    /// Continuous silence required to finish an active session.
    silence_timeout: Duration,

    /// Absolute signed-sample amplitude considered audible.
    silence_threshold: u16,

    /// PCM format negotiated for the current device connection.
    format: Option<StreamFormat>,

    /// Session state machine clocked by the current PCM stream.
    tracker: Option<SessionTracker>,

    /// FFmpeg process receiving the active session, when recording.
    recorder: Option<FlacRecorder>,

    /// Exclusive audio-clock frame through which PCM has reached FFmpeg.
    written_through_frame: u64,
}

impl CaptureService {
    /// Build a capture service from storage, encoder, and silence policy settings.
    pub fn new(
        ffmpeg: PathBuf,
        recordings_dir: PathBuf,
        silence_timeout: Duration,
        silence_threshold: u16,
    ) -> Self {
        Self {
            ffmpeg,
            recordings_dir,
            silence_timeout,
            silence_threshold,
            format: None,
            tracker: None,
            recorder: None,
            written_through_frame: 0,
        }
    }

    /// Consume device events until cancellation, finalizing any active recording.
    pub async fn run(
        mut self,
        mut events: mpsc::Receiver<Event>,
        shutdown: CancellationToken,
    ) -> Result<(), ServiceError> {
        loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    self.handle(event).await?;
                }
                _ = shutdown.cancelled() => break,
            }
        }

        self.finish(SessionEndReason::Shutdown).await
    }

    async fn handle(&mut self, event: Event) -> Result<(), ServiceError> {
        match event {
            Event::AudioConnected { format, .. } => {
                let silence_frames = duration_to_frames(self.silence_timeout, format.sample_rate);
                self.format = Some(format);
                self.tracker = Some(SessionTracker::new(silence_frames));
            }
            Event::Pcm(block) => {
                let format = self.format.ok_or(ServiceError::MissingFormat)?;
                let observation = analyze(&block, format, self.silence_threshold)?;
                let transition = self
                    .tracker
                    .as_mut()
                    .ok_or(ServiceError::MissingFormat)?
                    .observe(observation);

                if let SessionTransition::Started {
                    audio_started_at_frame,
                } = transition
                {
                    self.recorder = Some(
                        FlacRecorder::start(&self.ffmpeg, &self.recordings_dir, format).await?,
                    );
                    eprintln!("recording started at audio frame {audio_started_at_frame}");
                }

                if let Some(recorder) = &mut self.recorder {
                    if let Err(write_error) = recorder.write(&block.pcm).await {
                        let recorder = self
                            .recorder
                            .take()
                            .expect("recorder was matched immediately above");

                        return match recorder.finish().await {
                            Ok(_) => Err(write_error.into()),
                            Err(encoder_error) => Err(encoder_error.into()),
                        };
                    }
                    self.written_through_frame = observation.end_frame;
                }

                if let SessionTransition::Ended(session) = transition {
                    self.finish_recorder().await?;
                    eprintln!(
                        "recording ended at frame {} after audio stopped at frame {}",
                        session.recording_ended_at_frame, session.audio_ended_at_frame
                    );
                }
            }
            Event::AudioDisconnected { .. } => {
                self.finish(SessionEndReason::Discontinuity).await?;
                self.format = None;
                self.tracker = None;
            }
        }

        Ok(())
    }

    async fn finish(&mut self, reason: SessionEndReason) -> Result<(), ServiceError> {
        if let Some(tracker) = &mut self.tracker {
            tracker.finish(self.written_through_frame, reason);
        }
        self.finish_recorder().await
    }

    async fn finish_recorder(&mut self) -> Result<(), ServiceError> {
        let Some(recorder) = self.recorder.take() else {
            return Ok(());
        };
        let path = recorder.finish().await?;
        eprintln!("saved {}", path.display());

        Ok(())
    }
}

fn duration_to_frames(duration: Duration, sample_rate: u32) -> u64 {
    duration
        .as_secs()
        .saturating_mul(u64::from(sample_rate))
        .saturating_add(u64::from(duration.subsec_nanos()) * u64::from(sample_rate) / 1_000_000_000)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::{CaptureService, duration_to_frames};

    #[test]
    fn converts_the_timeout_to_the_audio_clock() {
        assert_eq!(
            duration_to_frames(Duration::from_secs(300), 44_100),
            13_230_000
        );
        assert_eq!(
            duration_to_frames(Duration::from_millis(500), 44_100),
            22_050
        );
    }

    #[tokio::test]
    async fn cancellation_stops_an_idle_service() {
        let (_event_tx, event_rx) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let service = CaptureService::new(
            PathBuf::from("ffmpeg"),
            PathBuf::from("recordings"),
            Duration::from_secs(300),
            0,
        );

        service.run(event_rx, shutdown).await.unwrap();
    }
}
