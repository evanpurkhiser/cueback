use crate::audio::{PcmBlock, StreamFormat};

#[derive(Debug)]
pub enum Event {
    AudioConnected {
        generation: u64,
        format: StreamFormat,
    },
    Pcm(PcmBlock),
    AudioDisconnected {
        generation: u64,
    },
}
