use thiserror::Error;

/// Bytes occupied by one signed 16-bit PCM sample.
pub const SAMPLE_WIDTH: usize = size_of::<i16>();

/// PCM properties negotiated when the audio stream connects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamFormat {
    /// Frames per second for each channel.
    pub sample_rate: u32,

    /// Number of interleaved channels in each frame.
    pub channels: u16,

    /// Largest block the sender promised to transmit.
    pub max_frames_per_block: u32,
}

impl StreamFormat {
    /// Number of bytes occupied by one interleaved audio frame.
    pub fn frame_bytes(self) -> usize {
        usize::from(self.channels) * SAMPLE_WIDTH
    }
}

/// A timestamped block of PCM from one device connection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcmBlock {
    /// Sender-assigned sequence number.
    pub sequence: u32,

    /// Absolute audio-clock frame at which this block begins.
    pub start_frame: u64,

    /// Frames the sender dropped immediately before this block.
    pub dropped_frames: u32,

    /// Interleaved signed 16-bit little-endian samples.
    pub pcm: Vec<u8>,
}

impl PcmBlock {
    /// Number of complete audio frames in this block.
    pub fn frame_count(&self, format: StreamFormat) -> u64 {
        (self.pcm.len() / format.frame_bytes()) as u64
    }

    /// Exclusive end position on the device audio clock.
    pub fn end_frame(&self, format: StreamFormat) -> u64 {
        self.start_frame.saturating_add(self.frame_count(format))
    }
}

/// Inclusive audible frame range found within one PCM block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudibleRange {
    /// First frame containing a sample above the configured threshold.
    pub first_frame: u64,

    /// Last frame containing a sample above the configured threshold.
    pub last_frame: u64,
}

/// Audible content observed within one PCM block.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioObservation {
    /// Exclusive end frame covered by the block.
    pub end_frame: u64,

    /// Range containing audible samples, or `None` when the block is silent.
    pub audible: Option<AudibleRange>,
}

/// Failure to interpret a PCM block as complete frames.
#[derive(Debug, Error)]
pub enum AnalyzeError {
    /// The byte count does not contain only complete interleaved frames.
    #[error("PCM block has {actual} bytes, which is not aligned to {frame_bytes}-byte frames")]
    Misaligned {
        /// Bytes present in the block.
        actual: usize,

        /// Bytes required for each complete frame.
        frame_bytes: usize,
    },
}

/// Locate the audible frame range in a signed 16-bit PCM block.
pub fn analyze(
    block: &PcmBlock,
    format: StreamFormat,
    threshold: u16,
) -> Result<AudioObservation, AnalyzeError> {
    let frame_bytes = format.frame_bytes();
    if !block.pcm.len().is_multiple_of(frame_bytes) {
        return Err(AnalyzeError::Misaligned {
            actual: block.pcm.len(),
            frame_bytes,
        });
    }

    let mut audible_frames =
        block
            .pcm
            .chunks_exact(frame_bytes)
            .enumerate()
            .filter_map(|(index, frame)| {
                frame
                    .chunks_exact(SAMPLE_WIDTH)
                    .any(|sample| {
                        i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs() > threshold
                    })
                    .then_some(block.start_frame + index as u64)
            });
    let audible = audible_frames.next().map(|first_frame| AudibleRange {
        first_frame,
        last_frame: audible_frames.next_back().unwrap_or(first_frame),
    });

    Ok(AudioObservation {
        end_frame: block.end_frame(format),
        audible,
    })
}

#[cfg(test)]
mod tests {
    use super::{AudibleRange, PcmBlock, StreamFormat, analyze};

    const FORMAT: StreamFormat = StreamFormat {
        sample_rate: 44_100,
        channels: 2,
        max_frames_per_block: 512,
    };

    fn block(samples: &[(i16, i16)]) -> PcmBlock {
        let pcm = samples
            .iter()
            .flat_map(|(left, right)| [left.to_le_bytes(), right.to_le_bytes()])
            .flatten()
            .collect();

        PcmBlock {
            sequence: 1,
            start_frame: 100,
            dropped_frames: 0,
            pcm,
        }
    }

    #[test]
    fn finds_the_first_and_last_audible_stereo_frames() {
        let observation =
            analyze(&block(&[(0, 0), (0, 11), (-12, 0), (0, 0)]), FORMAT, 10).unwrap();

        assert_eq!(observation.end_frame, 104);
        assert_eq!(
            observation.audible,
            Some(AudibleRange {
                first_frame: 101,
                last_frame: 102,
            })
        );
    }

    #[test]
    fn treats_samples_at_the_threshold_as_silent() {
        let observation = analyze(&block(&[(10, -10)]), FORMAT, 10).unwrap();

        assert_eq!(observation.audible, None);
    }

    #[test]
    fn handles_the_full_negative_sample_range() {
        let observation = analyze(&block(&[(i16::MIN, 0)]), FORMAT, i16::MAX as u16).unwrap();

        assert_eq!(
            observation.audible,
            Some(AudibleRange {
                first_frame: 100,
                last_frame: 100,
            })
        );
    }

    #[test]
    fn rejects_partial_frames() {
        let mut block = block(&[(0, 0)]);
        block.pcm.pop();

        assert!(analyze(&block, FORMAT, 0).is_err());
    }
}
