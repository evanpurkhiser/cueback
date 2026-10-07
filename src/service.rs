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

    /// PCM arrived without an active audio connection.
    #[error("PCM arrived without an active audio connection")]
    MissingConnection,
}

/// Storage, encoder, and silence policy settings for live capture.
#[derive(Debug)]
pub struct CaptureSettings {
    /// Encoder executable used for each live recording.
    pub ffmpeg: PathBuf,

    /// Root containing live artifacts and promoted session directories.
    pub recordings_dir: PathBuf,

    /// Continuous silence required to finish an active session.
    pub silence_timeout: Duration,

    /// Absolute signed-sample amplitude considered audible.
    pub silence_threshold: u16,
}

#[derive(Debug)]
struct ConnectedStream {
    /// PCM format negotiated for this connection.
    format: StreamFormat,

    /// Session state machine clocked by this connection.
    tracker: SessionTracker,

    /// Exclusive audio-clock frame through which PCM has reached FFmpeg.
    written_through_frame: u64,
}

/// Owns session detection and the active recorder for one device event stream.
pub struct CaptureService {
    /// Capture policy and output locations.
    settings: CaptureSettings,

    /// FFmpeg process receiving the active session, when recording.
    recorder: Option<FlacRecorder>,

    /// State owned by the current PCM connection.
    stream: Option<ConnectedStream>,
}

impl CaptureService {
    /// Build a capture service from storage, encoder, and silence policy settings.
    pub fn new(settings: CaptureSettings) -> Self {
        Self {
            settings,
            recorder: None,
            stream: None,
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
            Event::AudioConnected { format } => {
                let silence_frames =
                    duration_to_frames(self.settings.silence_timeout, format.sample_rate);
                self.stream = Some(ConnectedStream {
                    format,
                    tracker: SessionTracker::new(silence_frames),
                    written_through_frame: 0,
                });
            }
            Event::Pcm(block) => {
                let stream = self
                    .stream
                    .as_mut()
                    .ok_or(ServiceError::MissingConnection)?;
                let observation = analyze(&block, stream.format, self.settings.silence_threshold)?;
                let transition = stream.tracker.observe(observation);

                if let SessionTransition::Started {
                    audio_started_at_frame,
                } = transition
                {
                    self.recorder = Some(
                        FlacRecorder::start(
                            &self.settings.ffmpeg,
                            &self.settings.recordings_dir,
                            stream.format,
                        )
                        .await?,
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
                    stream.written_through_frame = observation.end_frame;
                }

                if let SessionTransition::Ended(session) = transition {
                    self.finish_recorder().await?;
                    eprintln!(
                        "recording ended at frame {} after audio stopped at frame {}",
                        session.recording_ended_at_frame, session.audio_ended_at_frame
                    );
                }
            }
            Event::AudioDisconnected => {
                self.finish(SessionEndReason::Discontinuity).await?;
                self.stream = None;
            }
        }

        Ok(())
    }

    async fn finish(&mut self, reason: SessionEndReason) -> Result<(), ServiceError> {
        if let Some(stream) = &mut self.stream {
            stream.tracker.finish(stream.written_through_frame, reason);
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

    use crate::{
        audio::{PcmBlock, StreamFormat},
        device::Event,
    };

    use super::{CaptureService, CaptureSettings, ServiceError, duration_to_frames};

    const FORMAT: StreamFormat = StreamFormat {
        sample_rate: 44_100,
        channels: 2,
        max_frames_per_block: 512,
    };

    fn service() -> CaptureService {
        CaptureService::new(CaptureSettings {
            ffmpeg: PathBuf::from("ffmpeg"),
            recordings_dir: PathBuf::from("recordings"),
            silence_timeout: Duration::from_secs(300),
            silence_threshold: 0,
        })
    }

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
        let service = service();

        service.run(event_rx, shutdown).await.unwrap();
    }

    #[tokio::test]
    async fn pcm_requires_an_active_connection() {
        let error = service()
            .handle(Event::Pcm(PcmBlock {
                sequence: 1,
                start_frame: 0,
                dropped_frames: 0,
                pcm: vec![0; 4],
            }))
            .await
            .unwrap_err();

        assert!(matches!(error, ServiceError::MissingConnection));
    }

    #[tokio::test]
    async fn disconnect_clears_connection_state() {
        let mut service = service();
        service
            .handle(Event::AudioConnected { format: FORMAT })
            .await
            .unwrap();

        service.handle(Event::AudioDisconnected).await.unwrap();

        assert!(service.stream.is_none());
    }
}
