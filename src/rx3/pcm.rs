use std::{io::ErrorKind, net::SocketAddr, time::Duration};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::TcpStream,
    sync::{mpsc, watch},
};
use tokio_util::sync::CancellationToken;

use crate::{
    audio::{PcmBlock, StreamFormat},
    device::Event,
};

use super::announcement::Device;

const MAGIC: &[u8; 4] = b"RX3A";
const VERSION: u8 = 1;
const CONFIG: u8 = 1;
const PCM: u8 = 2;
const S16_LE: u16 = 1;
const HEADER_SIZE: usize = 28;
const CONFIG_SIZE: usize = 12;
const MAX_PAYLOAD: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum PcmError {
    #[error("PCM stream I/O failed")]
    Io(#[from] std::io::Error),

    #[error("message does not start with RX3A")]
    InvalidMagic,

    #[error("unsupported RX3A version {0}")]
    UnsupportedVersion(u8),

    #[error("unsupported RX3A message type {0}")]
    UnsupportedMessage(u8),

    #[error("PCM payload is too large: {0} bytes")]
    PayloadTooLarge(usize),

    #[error("invalid stream config length {0}")]
    InvalidConfigLength(usize),

    #[error("invalid sample rate {0}")]
    InvalidSampleRate(u32),

    #[error("invalid channel count {0}")]
    InvalidChannels(u16),

    #[error("unsupported sample format {0}")]
    UnsupportedSampleFormat(u16),

    #[error("invalid maximum frames per block {0}")]
    InvalidMaxFrames(u32),

    #[error("PCM arrived before the stream config")]
    MissingConfig,

    #[error("stream config changed during a connection")]
    ConfigChanged,

    #[error("PCM payload has {actual} bytes, expected complete {frame_bytes}-byte frames")]
    MisalignedPcm { actual: usize, frame_bytes: usize },

    #[error("PCM payload has {actual} frames, exceeding the configured maximum {maximum}")]
    TooManyFrames { actual: usize, maximum: u32 },
}

#[derive(Debug)]
struct Message {
    kind: u8,
    sequence: u32,
    timestamp_frames: u64,
    dropped_frames: u32,
    payload: Vec<u8>,
}

pub struct PcmStream<R> {
    reader: R,
    format: StreamFormat,
}

impl<R: AsyncRead + Unpin> PcmStream<R> {
    pub async fn handshake(mut reader: R) -> Result<Self, PcmError> {
        let message = read_message(&mut reader).await?.ok_or_else(|| {
            PcmError::Io(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "stream ended before config",
            ))
        })?;

        match message.kind {
            CONFIG => Ok(Self {
                reader,
                format: decode_config(&message.payload)?,
            }),
            PCM => Err(PcmError::MissingConfig),
            _ => unreachable!("message kind validated by read_message"),
        }
    }

    pub fn format(&self) -> StreamFormat {
        self.format
    }

    pub async fn next_block(&mut self) -> Result<Option<PcmBlock>, PcmError> {
        loop {
            let Some(message) = read_message(&mut self.reader).await? else {
                return Ok(None);
            };

            if message.kind == CONFIG {
                if decode_config(&message.payload)? != self.format {
                    return Err(PcmError::ConfigChanged);
                }

                continue;
            }

            validate_pcm(&message.payload, self.format)?;

            return Ok(Some(PcmBlock {
                sequence: message.sequence,
                start_frame: message.timestamp_frames,
                dropped_frames: message.dropped_frames,
                pcm: message.payload,
            }));
        }
    }
}

pub async fn supervise(
    mut device_rx: watch::Receiver<Option<Device>>,
    port: u16,
    reconnect_delay: Duration,
    event_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
) -> Result<(), PcmError> {
    let mut generation = 0;

    loop {
        if shutdown.is_cancelled() {
            return Ok(());
        }

        let device = loop {
            if let Some(device) = device_rx.borrow().clone() {
                break device;
            }

            tokio::select! {
                changed = device_rx.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                }
                _ = shutdown.cancelled() => return Ok(()),
            }
        };

        let address = SocketAddr::from((device.ip_address, port));
        let connected = tokio::select! {
            connected = TcpStream::connect(address) => connected,
            _ = shutdown.cancelled() => return Ok(()),
        };
        let stream = match connected {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("failed to connect to RX3 PCM stream at {address}: {error}");
                wait_to_retry(reconnect_delay, &shutdown).await;
                continue;
            }
        };
        let handshake = tokio::select! {
            handshake = PcmStream::handshake(stream) => handshake,
            _ = shutdown.cancelled() => return Ok(()),
        };
        let mut stream = match handshake {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("RX3 PCM handshake failed: {error}");
                wait_to_retry(reconnect_delay, &shutdown).await;
                continue;
            }
        };

        generation += 1;
        if event_tx
            .send(Event::AudioConnected {
                generation,
                format: stream.format(),
            })
            .await
            .is_err()
        {
            return Ok(());
        }

        loop {
            let block = tokio::select! {
                block = stream.next_block() => block,
                _ = shutdown.cancelled() => return Ok(()),
            };

            match block {
                Ok(Some(block)) => {
                    if event_tx.send(Event::Pcm(block)).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    eprintln!("RX3 PCM stream disconnected: {error}");
                    break;
                }
            }
        }

        if event_tx
            .send(Event::AudioDisconnected { generation })
            .await
            .is_err()
        {
            return Ok(());
        }
        wait_to_retry(reconnect_delay, &shutdown).await;
    }
}

async fn wait_to_retry(delay: Duration, shutdown: &CancellationToken) {
    tokio::select! {
        _ = tokio::time::sleep(delay) => {}
        _ = shutdown.cancelled() => {}
    }
}

async fn read_message<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<Message>, PcmError> {
    let mut header = [0; HEADER_SIZE];
    match reader.read_exact(&mut header[..1]).await {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    reader.read_exact(&mut header[1..]).await?;

    if &header[..4] != MAGIC {
        return Err(PcmError::InvalidMagic);
    }
    if header[4] != VERSION {
        return Err(PcmError::UnsupportedVersion(header[4]));
    }
    if !matches!(header[5], CONFIG | PCM) {
        return Err(PcmError::UnsupportedMessage(header[5]));
    }

    let payload_size = u32::from_be_bytes(header[12..16].try_into().unwrap()) as usize;
    if payload_size > MAX_PAYLOAD {
        return Err(PcmError::PayloadTooLarge(payload_size));
    }

    let mut payload = vec![0; payload_size];
    reader.read_exact(&mut payload).await?;

    Ok(Some(Message {
        kind: header[5],
        sequence: u32::from_be_bytes(header[8..12].try_into().unwrap()),
        timestamp_frames: u64::from_be_bytes(header[16..24].try_into().unwrap()),
        dropped_frames: u32::from_be_bytes(header[24..28].try_into().unwrap()),
        payload,
    }))
}

fn decode_config(payload: &[u8]) -> Result<StreamFormat, PcmError> {
    if payload.len() != CONFIG_SIZE {
        return Err(PcmError::InvalidConfigLength(payload.len()));
    }

    let sample_rate = u32::from_be_bytes(payload[0..4].try_into().unwrap());
    let channels = u16::from_be_bytes(payload[4..6].try_into().unwrap());
    let sample_format = u16::from_be_bytes(payload[6..8].try_into().unwrap());
    let max_frames_per_block = u32::from_be_bytes(payload[8..12].try_into().unwrap());

    if !(8_000..=384_000).contains(&sample_rate) {
        return Err(PcmError::InvalidSampleRate(sample_rate));
    }
    if !(1..=32).contains(&channels) {
        return Err(PcmError::InvalidChannels(channels));
    }
    if sample_format != S16_LE {
        return Err(PcmError::UnsupportedSampleFormat(sample_format));
    }
    if !(1..=sample_rate).contains(&max_frames_per_block) {
        return Err(PcmError::InvalidMaxFrames(max_frames_per_block));
    }

    Ok(StreamFormat {
        sample_rate,
        channels,
        max_frames_per_block,
    })
}

fn validate_pcm(payload: &[u8], format: StreamFormat) -> Result<(), PcmError> {
    let frame_bytes = format.frame_bytes();
    if !payload.len().is_multiple_of(frame_bytes) {
        return Err(PcmError::MisalignedPcm {
            actual: payload.len(),
            frame_bytes,
        });
    }

    let frames = payload.len() / frame_bytes;
    if frames > format.max_frames_per_block as usize {
        return Err(PcmError::TooManyFrames {
            actual: frames,
            maximum: format.max_frames_per_block,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, duplex};

    use super::{CONFIG, PCM, PcmError, PcmStream};
    use crate::audio::StreamFormat;

    fn message(kind: u8, sequence: u32, timestamp: u64, payload: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(b"RX3A");
        encoded.push(1);
        encoded.push(kind);
        encoded.extend_from_slice(&0_u16.to_be_bytes());
        encoded.extend_from_slice(&sequence.to_be_bytes());
        encoded.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        encoded.extend_from_slice(&timestamp.to_be_bytes());
        encoded.extend_from_slice(&3_u32.to_be_bytes());
        encoded.extend_from_slice(payload);
        encoded
    }

    fn config() -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&44_100_u32.to_be_bytes());
        payload.extend_from_slice(&2_u16.to_be_bytes());
        payload.extend_from_slice(&1_u16.to_be_bytes());
        payload.extend_from_slice(&512_u32.to_be_bytes());
        message(CONFIG, 0, 0, &payload)
    }

    #[tokio::test]
    async fn decodes_config_and_pcm_from_a_chunked_stream() {
        let (mut writer, reader) = duplex(128);
        let mut bytes = config();
        bytes.extend_from_slice(&message(PCM, 1, 42, &[1, 0, 2, 0]));

        tokio::spawn(async move {
            for chunk in bytes.chunks(3) {
                writer.write_all(chunk).await.unwrap();
            }
        });

        let mut stream = PcmStream::handshake(reader).await.unwrap();
        assert_eq!(
            stream.format(),
            StreamFormat {
                sample_rate: 44_100,
                channels: 2,
                max_frames_per_block: 512,
            }
        );

        let block = stream.next_block().await.unwrap().unwrap();
        assert_eq!(block.sequence, 1);
        assert_eq!(block.start_frame, 42);
        assert_eq!(block.dropped_frames, 3);
        assert_eq!(block.pcm, [1, 0, 2, 0]);
    }

    #[tokio::test]
    async fn rejects_pcm_before_config() {
        let (mut writer, reader) = duplex(64);
        writer
            .write_all(&message(PCM, 1, 0, &[0, 0, 0, 0]))
            .await
            .unwrap();

        assert!(matches!(
            PcmStream::handshake(reader).await,
            Err(PcmError::MissingConfig)
        ));
    }

    #[tokio::test]
    async fn rejects_partial_sample_frames() {
        let (mut writer, reader) = duplex(128);
        let mut bytes = config();
        bytes.extend_from_slice(&message(PCM, 1, 0, &[0, 0, 0]));
        writer.write_all(&bytes).await.unwrap();

        let mut stream = PcmStream::handshake(reader).await.unwrap();

        assert!(matches!(
            stream.next_block().await,
            Err(PcmError::MisalignedPcm { .. })
        ));
    }
}
