use crate::audio::AudioObservation;

/// Whether the current audio stream has an active live session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    /// No audible PCM has started a session.
    Idle,

    /// A session is active and may currently be in its trailing silence window.
    Recording {
        /// First audible frame in the session.
        started_at_frame: u64,

        /// Most recent audible frame observed in the session.
        last_audible_frame: u64,
    },
}

/// Condition that completed a live session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionEndReason {
    /// The configured duration of continuous silence elapsed.
    Silence,

    /// The PCM connection ended or restarted.
    Discontinuity,

    /// Cueback was asked to shut down.
    Shutdown,
}

/// Audio-clock boundaries recorded when a live session completes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompletedSession {
    /// First audible frame in the session.
    pub started_at_frame: u64,

    /// Last audible frame before trailing silence.
    pub audio_ended_at_frame: u64,

    /// Frame through which PCM was retained in the recording.
    pub recording_ended_at_frame: u64,

    /// Condition that completed the session.
    pub reason: SessionEndReason,
}

/// Decision produced from one block-level audio observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionTransition {
    /// The existing session state continues.
    None,

    /// Audible PCM started a new session.
    Started {
        /// First audible frame in the new session.
        audio_started_at_frame: u64,
    },

    /// An active session reached an end condition.
    Ended(CompletedSession),
}

/// Stateful silence policy operating entirely on the device audio clock.
#[derive(Debug)]
pub struct SessionTracker {
    silence_frames: u64,
    state: SessionState,
}

impl SessionTracker {
    /// Create a tracker with a trailing-silence duration expressed in frames.
    pub fn new(silence_frames: u64) -> Self {
        Self {
            silence_frames,
            state: SessionState::Idle,
        }
    }

    /// Return the current live-session state.
    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Apply one contiguous audio observation and return any state transition.
    pub fn observe(&mut self, observation: AudioObservation) -> SessionTransition {
        match self.state {
            SessionState::Idle => self.start(observation),
            SessionState::Recording {
                started_at_frame,
                last_audible_frame,
            } => self.continue_recording(observation, started_at_frame, last_audible_frame),
        }
    }

    /// Complete the active session for an external end condition.
    pub fn finish(
        &mut self,
        recording_ended_at_frame: u64,
        reason: SessionEndReason,
    ) -> Option<CompletedSession> {
        let SessionState::Recording {
            started_at_frame,
            last_audible_frame,
        } = self.state
        else {
            return None;
        };

        self.state = SessionState::Idle;

        Some(CompletedSession {
            started_at_frame,
            audio_ended_at_frame: last_audible_frame,
            recording_ended_at_frame,
            reason,
        })
    }

    fn start(&mut self, observation: AudioObservation) -> SessionTransition {
        let Some(started_at_frame) = observation.first_audible_frame else {
            return SessionTransition::None;
        };
        let last_audible_frame = observation.last_audible_frame.unwrap_or(started_at_frame);

        self.state = SessionState::Recording {
            started_at_frame,
            last_audible_frame,
        };

        SessionTransition::Started {
            audio_started_at_frame: started_at_frame,
        }
    }

    fn continue_recording(
        &mut self,
        observation: AudioObservation,
        started_at_frame: u64,
        last_audible_frame: u64,
    ) -> SessionTransition {
        if let Some(last_audible_frame) = observation.last_audible_frame {
            self.state = SessionState::Recording {
                started_at_frame,
                last_audible_frame,
            };

            return SessionTransition::None;
        }

        let recording_ended_at_frame = last_audible_frame
            .saturating_add(1)
            .saturating_add(self.silence_frames);
        if observation.end_frame < recording_ended_at_frame {
            return SessionTransition::None;
        }

        let session = self
            .finish(recording_ended_at_frame, SessionEndReason::Silence)
            .expect("recording state was matched above");

        SessionTransition::Ended(session)
    }
}

#[cfg(test)]
mod tests {
    use crate::audio::AudioObservation;

    use super::{SessionEndReason, SessionState, SessionTracker, SessionTransition};

    fn observation(
        start_frame: u64,
        end_frame: u64,
        audible: Option<(u64, u64)>,
    ) -> AudioObservation {
        AudioObservation {
            start_frame,
            end_frame,
            first_audible_frame: audible.map(|range| range.0),
            last_audible_frame: audible.map(|range| range.1),
        }
    }

    #[test]
    fn silence_does_not_start_a_session() {
        let mut tracker = SessionTracker::new(100);

        assert_eq!(
            tracker.observe(observation(0, 50, None)),
            SessionTransition::None
        );
        assert_eq!(tracker.state(), SessionState::Idle);
    }

    #[test]
    fn first_audible_frame_starts_a_session() {
        let mut tracker = SessionTracker::new(100);

        assert_eq!(
            tracker.observe(observation(0, 50, Some((12, 34)))),
            SessionTransition::Started {
                audio_started_at_frame: 12
            }
        );
        assert_eq!(
            tracker.state(),
            SessionState::Recording {
                started_at_frame: 12,
                last_audible_frame: 34,
            }
        );
    }

    #[test]
    fn renewed_audio_resets_the_silence_window() {
        let mut tracker = SessionTracker::new(100);
        tracker.observe(observation(0, 50, Some((12, 34))));
        tracker.observe(observation(50, 100, Some((75, 80))));

        assert_eq!(
            tracker.observe(observation(100, 179, None)),
            SessionTransition::None
        );
        assert!(matches!(tracker.state(), SessionState::Recording { .. }));
    }

    #[test]
    fn continuous_silence_ends_at_the_frame_deadline() {
        let mut tracker = SessionTracker::new(100);
        tracker.observe(observation(0, 50, Some((12, 34))));

        assert_eq!(
            tracker.observe(observation(50, 134, None)),
            SessionTransition::None
        );
        assert_eq!(
            tracker.observe(observation(134, 135, None)),
            SessionTransition::Ended(super::CompletedSession {
                started_at_frame: 12,
                audio_ended_at_frame: 34,
                recording_ended_at_frame: 135,
                reason: SessionEndReason::Silence,
            })
        );
        assert_eq!(tracker.state(), SessionState::Idle);
    }

    #[test]
    fn a_discontinuity_finishes_an_active_session() {
        let mut tracker = SessionTracker::new(100);
        tracker.observe(observation(0, 50, Some((12, 34))));

        let completed = tracker.finish(50, SessionEndReason::Discontinuity).unwrap();

        assert_eq!(completed.audio_ended_at_frame, 34);
        assert_eq!(completed.recording_ended_at_frame, 50);
        assert_eq!(completed.reason, SessionEndReason::Discontinuity);
        assert_eq!(tracker.state(), SessionState::Idle);
    }
}
