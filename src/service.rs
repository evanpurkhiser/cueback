use std::{collections::VecDeque, path::PathBuf, time::Duration};

use chrono::Local;
use thiserror::Error;
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    audio::{AnalyzeError, StreamFormat, analyze},
    device::{ControlEvent, ControlStreamEvent, ControlStreamInfo, Event},
    recorder::{FlacRecorder, RecorderError},
    session::{CompletedSession, SessionEndReason, SessionTracker, SessionTransition},
    storage::SessionPaths,
    timeline::{TimelineError, TimelineRecorder},
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

    /// Timeline recording failed.
    #[error(transparent)]
    Timeline(#[from] TimelineError),

    /// Session artifacts could not be prepared or promoted.
    #[error("failed to manage session artifacts")]
    Storage(#[from] std::io::Error),

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

    /// Control history retained before the first audible frame.
    pub timeline_pre_roll: Duration,
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

struct ActiveRecording {
    paths: SessionPaths,
    audio: FlacRecorder,
    timeline: TimelineRecorder,
}

impl ActiveRecording {
    async fn start(
        settings: &CaptureSettings,
        format: StreamFormat,
        audio_started_at_frame: u64,
        remote: Option<&ControlStreamInfo>,
        pre_roll: impl IntoIterator<Item = ControlEvent>,
    ) -> Result<Self, ServiceError> {
        let paths = SessionPaths::create(&settings.recordings_dir, Local::now(), Uuid::new_v4());
        paths.prepare().await?;
        let audio = FlacRecorder::start(&settings.ffmpeg, paths.clone(), format).await?;
        let timeline =
            TimelineRecorder::start(&paths, audio_started_at_frame, remote, pre_roll).await?;

        Ok(Self {
            paths,
            audio,
            timeline,
        })
    }

    async fn finish(self, completed: CompletedSession) -> Result<PathBuf, ServiceError> {
        let audio_result = self.audio.finish().await;
        let timeline_result = self.timeline.finish(completed).await;

        audio_result?;
        timeline_result?;

        Ok(self.paths.promote().await?)
    }
}

struct BufferedControl {
    received_at: Instant,
    event: ControlEvent,
}

/// Owns session detection and the active recorder for one device event stream.
pub struct CaptureService {
    /// Capture policy and output locations.
    settings: CaptureSettings,

    /// FFmpeg process receiving the active session, when recording.
    recording: Option<ActiveRecording>,

    /// State owned by the current PCM connection.
    stream: Option<ConnectedStream>,

    /// Most recent remote-control handshake, while connected.
    remote: Option<ControlStreamInfo>,

    /// Control callbacks retained so a session includes its initiating action.
    control_pre_roll: VecDeque<BufferedControl>,
}

impl CaptureService {
    /// Build a capture service from storage, encoder, and silence policy settings.
    pub fn new(settings: CaptureSettings) -> Self {
        Self {
            settings,
            recording: None,
            stream: None,
            remote: None,
            control_pre_roll: VecDeque::new(),
        }
    }

    /// Consume device events until cancellation, finalizing any active recording.
    pub async fn run(
        mut self,
        mut audio_events: mpsc::Receiver<Event>,
        mut control_events: mpsc::Receiver<ControlStreamEvent>,
        shutdown: CancellationToken,
    ) -> Result<(), ServiceError> {
        let mut controls_open = true;

        loop {
            tokio::select! {
                event = audio_events.recv() => {
                    let Some(event) = event else {
                        break;
                    };
                    self.handle_audio(event).await?;
                }
                event = control_events.recv(), if controls_open => {
                    match event {
                        Some(event) => self.handle_control(event).await?,
                        None => controls_open = false,
                    }
                }
                _ = shutdown.cancelled() => break,
            }
        }

        self.finish(SessionEndReason::Shutdown).await
    }

    async fn handle_audio(&mut self, event: Event) -> Result<(), ServiceError> {
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
                self.prune_pre_roll(Instant::now());
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
                    let pre_roll = self
                        .control_pre_roll
                        .drain(..)
                        .map(|buffered| buffered.event)
                        .collect::<Vec<_>>();
                    self.recording = Some(
                        ActiveRecording::start(
                            &self.settings,
                            stream.format,
                            audio_started_at_frame,
                            self.remote.as_ref(),
                            pre_roll,
                        )
                        .await?,
                    );
                    eprintln!("recording started at audio frame {audio_started_at_frame}");
                }

                if let Some(recording) = &mut self.recording {
                    if let Err(write_error) = recording.audio.write(&block.pcm).await {
                        let recording = self
                            .recording
                            .take()
                            .expect("recording was matched immediately above");
                        let completed = stream
                            .tracker
                            .finish(observation.end_frame, SessionEndReason::Discontinuity)
                            .expect("an active recorder has an active session");

                        return match recording.finish(completed).await {
                            Ok(_) => Err(write_error.into()),
                            Err(finish_error) => Err(finish_error),
                        };
                    }
                    stream.written_through_frame = observation.end_frame;
                }

                if let SessionTransition::Ended(session) = transition {
                    self.finish_recording(session).await?;
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

    async fn handle_control(&mut self, event: ControlStreamEvent) -> Result<(), ServiceError> {
        match &event {
            ControlStreamEvent::Connected(info) => self.remote = Some(info.clone()),
            ControlStreamEvent::Disconnected => self.remote = None,
            ControlStreamEvent::Control(control) if self.recording.is_none() => {
                let now = Instant::now();
                self.control_pre_roll.push_back(BufferedControl {
                    received_at: now,
                    event: control.clone(),
                });
                self.prune_pre_roll(now);

                return Ok(());
            }
            ControlStreamEvent::Control(_) => {}
        }

        if let Some(recording) = &mut self.recording {
            recording.timeline.write(&event).await?;
        }

        Ok(())
    }

    fn prune_pre_roll(&mut self, now: Instant) {
        while self.control_pre_roll.front().is_some_and(|event| {
            now.duration_since(event.received_at) > self.settings.timeline_pre_roll
        }) {
            self.control_pre_roll.pop_front();
        }
    }

    async fn finish(&mut self, reason: SessionEndReason) -> Result<(), ServiceError> {
        let completed = self
            .stream
            .as_mut()
            .and_then(|stream| stream.tracker.finish(stream.written_through_frame, reason));
        let Some(completed) = completed else {
            return Ok(());
        };

        self.finish_recording(completed).await
    }

    async fn finish_recording(&mut self, completed: CompletedSession) -> Result<(), ServiceError> {
        let Some(recording) = self.recording.take() else {
            return Ok(());
        };
        let path = recording.finish(completed).await?;
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

    use tokio::{sync::mpsc, time::advance};
    use tokio_util::sync::CancellationToken;

    use crate::{
        audio::{PcmBlock, StreamFormat},
        device::{ControlEvent, ControlStreamEvent, Event},
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
            timeline_pre_roll: Duration::from_secs(10),
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
        let (_audio_tx, audio_rx) = mpsc::channel(1);
        let (_control_tx, control_rx) = mpsc::channel(1);
        let shutdown = CancellationToken::new();
        shutdown.cancel();
        let service = service();

        service.run(audio_rx, control_rx, shutdown).await.unwrap();
    }

    #[tokio::test]
    async fn pcm_requires_an_active_connection() {
        let error = service()
            .handle_audio(Event::Pcm(PcmBlock {
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
            .handle_audio(Event::AudioConnected { format: FORMAT })
            .await
            .unwrap();

        service
            .handle_audio(Event::AudioDisconnected)
            .await
            .unwrap();

        assert!(service.stream.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn control_pre_roll_discards_events_older_than_its_window() {
        let mut service = service();
        let first = control_event(1);
        let second = control_event(2);

        service
            .handle_control(ControlStreamEvent::Control(first))
            .await
            .unwrap();
        advance(Duration::from_secs(11)).await;
        service
            .handle_control(ControlStreamEvent::Control(second.clone()))
            .await
            .unwrap();

        assert_eq!(service.control_pre_roll.len(), 1);
        assert_eq!(service.control_pre_roll[0].event, second);
    }

    fn control_event(timestamp_us: u64) -> ControlEvent {
        ControlEvent {
            key_code: 0x4101,
            operation: 0,
            channel: 1,
            value: 1,
            float_bits: 0,
            auxiliary: 0,
            source: 1,
            flags: 0,
            timestamp_us,
        }
    }
}
