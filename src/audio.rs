use thiserror::Error;

pub const SAMPLE_WIDTH: usize = size_of::<i16>();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub max_frames_per_block: u32,
}

impl StreamFormat {
    pub fn frame_bytes(self) -> usize {
        usize::from(self.channels) * SAMPLE_WIDTH
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PcmBlock {
    pub sequence: u32,
    pub start_frame: u64,
    pub dropped_frames: u32,
    pub pcm: Vec<u8>,
}

impl PcmBlock {
    pub fn frame_count(&self, format: StreamFormat) -> u64 {
        (self.pcm.len() / format.frame_bytes()) as u64
    }

    pub fn end_frame(&self, format: StreamFormat) -> u64 {
        self.start_frame.saturating_add(self.frame_count(format))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioObservation {
    pub start_frame: u64,
    pub end_frame: u64,
    pub first_audible_frame: Option<u64>,
    pub last_audible_frame: Option<u64>,
}

#[derive(Debug, Error)]
pub enum AnalyzeError {
    #[error("PCM block has {actual} bytes, which is not aligned to {frame_bytes}-byte frames")]
    Misaligned { actual: usize, frame_bytes: usize },
}

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
    let first_audible_frame = audible_frames.next();
    let last_audible_frame = audible_frames.next_back().or(first_audible_frame);

    Ok(AudioObservation {
        start_frame: block.start_frame,
        end_frame: block.end_frame(format),
        first_audible_frame,
        last_audible_frame,
    })
}

#[cfg(test)]
mod tests {
    use super::{PcmBlock, StreamFormat, analyze};

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

        assert_eq!(observation.start_frame, 100);
        assert_eq!(observation.end_frame, 104);
        assert_eq!(observation.first_audible_frame, Some(101));
        assert_eq!(observation.last_audible_frame, Some(102));
    }

    #[test]
    fn treats_samples_at_the_threshold_as_silent() {
        let observation = analyze(&block(&[(10, -10)]), FORMAT, 10).unwrap();

        assert_eq!(observation.first_audible_frame, None);
        assert_eq!(observation.last_audible_frame, None);
    }

    #[test]
    fn handles_the_full_negative_sample_range() {
        let observation = analyze(&block(&[(i16::MIN, 0)]), FORMAT, i16::MAX as u16).unwrap();

        assert_eq!(observation.first_audible_frame, Some(100));
    }

    #[test]
    fn rejects_partial_frames() {
        let mut block = block(&[(0, 0)]);
        block.pcm.pop();

        assert!(analyze(&block, FORMAT, 0).is_err());
    }
}
