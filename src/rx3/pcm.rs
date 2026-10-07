//! Decoder and connection supervisor for the RX3A PCM stream exposed by the
//! `rx3-toolkit` firmware addition.
//!
//! Each TCP connection starts with a configuration message and then carries
//! timestamped blocks of interleaved signed 16-bit little-endian PCM. Integer
//! fields in the RX3A envelope and configuration payload use network byte
//! order.

use std::{io::ErrorKind, net::SocketAddr, time::Duration};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    net::TcpStream,
    sync::{mpsc, watch},
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::{
    audio::{PcmBlock, StreamFormat},
    device::{Device, Event},
};

use super::{wait_for_device, wait_to_retry};

const MAGIC: &[u8; 4] = b"RX3A";
const VERSION: u8 = 1;
const CONFIG: u8 = 1;
const PCM: u8 = 2;
const S16_LE: u16 = 1;
const HEADER_SIZE: usize = 28;
const CONFIG_SIZE: usize = 12;
const MAX_PAYLOAD: usize = 1024 * 1024;

/// Failure to decode or receive an RX3A PCM stream.
#[derive(Debug, Error)]
pub enum PcmError {
    /// The underlying TCP stream failed.
    #[error("PCM stream I/O failed")]
    Io(#[from] std::io::Error),

    /// A message does not begin with the RX3A protocol marker.
    #[error("message does not start with RX3A")]
    InvalidMagic,

    /// The sender uses an RX3A protocol version this decoder does not support.
    #[error("unsupported RX3A version {0}")]
    UnsupportedVersion(u8),

    /// The message type is neither stream configuration nor PCM.
    #[error("unsupported RX3A message type {0}")]
    UnsupportedMessage(u8),

    /// The declared payload exceeds the allocation safety limit.
    #[error("PCM payload is too large: {0} bytes")]
    PayloadTooLarge(usize),

    /// A stream configuration payload has an unexpected byte length.
    #[error("invalid stream config length {0}")]
    InvalidConfigLength(usize),

    /// The announced sample rate is outside the accepted audio range.
    #[error("invalid sample rate {0}")]
    InvalidSampleRate(u32),

    /// The announced channel count is outside the accepted audio range.
    #[error("invalid channel count {0}")]
    InvalidChannels(u16),

    /// The announced sample encoding is not signed 16-bit little-endian PCM.
    #[error("unsupported sample format {0}")]
    UnsupportedSampleFormat(u16),

    /// The announced maximum block size is zero or implausibly large.
    #[error("invalid maximum frames per block {0}")]
    InvalidMaxFrames(u32),

    /// The connection sent audio before declaring its stream format.
    #[error("PCM arrived before the stream config")]
    MissingConfig,

    /// A repeated configuration message disagrees with the initial handshake.
    #[error("stream config changed during a connection")]
    ConfigChanged,

    /// No complete PCM block arrived before the connection watchdog expired.
    #[error("PCM stream was idle for {0:?}")]
    IdleTimeout(Duration),

    /// PCM bytes do not contain only complete interleaved frames.
    #[error("PCM payload has {actual} bytes, expected complete {frame_bytes}-byte frames")]
    MisalignedPcm {
        /// Bytes present in the payload.
        actual: usize,

        /// Bytes required for each complete frame.
        frame_bytes: usize,
    },

    /// A PCM message exceeds the block size promised during the handshake.
    #[error("PCM payload has {actual} frames, exceeding the configured maximum {maximum}")]
    TooManyFrames {
        /// Frames present in the payload.
        actual: usize,

        /// Maximum frames declared by the sender.
        maximum: u32,
    },
}

/// One decoded RX3A envelope before message-specific validation.
#[derive(Debug)]
struct Message {
    /// RX3A message type.
    kind: u8,

    /// Sender-assigned message sequence number.
    sequence: u32,

    /// Absolute start position on the sender's audio clock.
    timestamp_frames: u64,

    /// Frames omitted by the sender immediately before this message.
    dropped_frames: u32,

    /// Message body with the envelope removed.
    payload: Vec<u8>,
}

/// A handshaken RX3A stream with an invariant PCM format.
pub struct PcmStream<R> {
    reader: R,
    format: StreamFormat,
}

impl<R: AsyncRead + Unpin> PcmStream<R> {
    /// Read and validate the mandatory configuration message that opens a stream.
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

    /// Return the PCM format established by the opening configuration message.
    pub fn format(&self) -> StreamFormat {
        self.format
    }

    /// Decode the next PCM block, validating repeated configuration messages.
    ///
    /// A clean TCP close before the next envelope returns `Ok(None)`. A close
    /// partway through an envelope or payload is reported as an I/O error.
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

/// Maintain the PCM connection for the currently announced device.
///
/// Events for each connection are emitted in strict order: `AudioConnected`,
/// zero or more `Pcm` blocks, then `AudioDisconnected`. An established stream
/// remains authoritative while it delivers data even if announcement packets
/// temporarily stop. Cancellation ends the task without a disconnect event;
/// the capture service handles that path as application shutdown.
pub async fn supervise(
    mut device_rx: watch::Receiver<Option<Device>>,
    port: u16,
    idle_timeout: Duration,
    reconnect_delay: Duration,
    event_tx: mpsc::Sender<Event>,
    shutdown: CancellationToken,
) -> Result<(), PcmError> {
    loop {
        let Some(device) = wait_for_device(&mut device_rx, &shutdown).await else {
            return Ok(());
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
        let stream = match handshake {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("RX3 PCM handshake failed: {error}");
                wait_to_retry(reconnect_delay, &shutdown).await;
                continue;
            }
        };

        if relay_connection(stream, idle_timeout, &event_tx, &shutdown).await == RelayOutcome::Stop
        {
            return Ok(());
        }
        wait_to_retry(reconnect_delay, &shutdown).await;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayOutcome {
    Reconnect,
    Stop,
}

/// Forward one handshaken connection into the device event stream.
async fn relay_connection<R: AsyncRead + Unpin>(
    mut stream: PcmStream<R>,
    idle_timeout: Duration,
    event_tx: &mpsc::Sender<Event>,
    shutdown: &CancellationToken,
) -> RelayOutcome {
    if !send_event(
        event_tx,
        Event::AudioConnected {
            format: stream.format(),
        },
        shutdown,
    )
    .await
    {
        return RelayOutcome::Stop;
    }

    loop {
        let block = tokio::select! {
            block = next_block_with_timeout(&mut stream, idle_timeout) => block,
            _ = shutdown.cancelled() => return RelayOutcome::Stop,
        };

        match block {
            Ok(Some(block)) => {
                if !send_event(event_tx, Event::Pcm(block), shutdown).await {
                    return RelayOutcome::Stop;
                }
            }
            Ok(None) => break,
            Err(error) => {
                eprintln!("RX3 PCM stream disconnected: {error}");
                break;
            }
        }
    }

    if !send_event(event_tx, Event::AudioDisconnected, shutdown).await {
        return RelayOutcome::Stop;
    }

    RelayOutcome::Reconnect
}

async fn send_event(
    event_tx: &mpsc::Sender<Event>,
    event: Event,
    shutdown: &CancellationToken,
) -> bool {
    tokio::select! {
        result = event_tx.send(event) => result.is_ok(),
        _ = shutdown.cancelled() => false,
    }
}

/// Read the next PCM block before the connection-level idle deadline.
///
/// Silent PCM is still a block and therefore keeps the connection alive. The
/// timeout only detects a peer that has stopped delivering RX3A messages.
async fn next_block_with_timeout<R: AsyncRead + Unpin>(
    stream: &mut PcmStream<R>,
    idle_timeout: Duration,
) -> Result<Option<PcmBlock>, PcmError> {
    timeout(idle_timeout, stream.next_block())
        .await
        .map_err(|_| PcmError::IdleTimeout(idle_timeout))?
}

/// Read one length-prefixed RX3A message from the byte stream.
///
/// The fixed header is laid out as magic (bytes 0–3), version (4), message type
/// (5), reserved bytes (6–7), sequence (8–11), payload size (12–15), audio
/// timestamp (16–23), and dropped-frame count (24–27).
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

/// Decode and constrain the connection-opening configuration payload.
///
/// The payload contains sample rate (bytes 0–3), channel count (4–5), sample
/// format (6–7), and maximum frames per block (8–11).
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

/// Check a PCM payload against the format negotiated during the handshake.
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
    use std::time::Duration;

    use tokio::{
        io::{AsyncWriteExt, duplex},
        sync::mpsc,
    };
    use tokio_util::sync::CancellationToken;

    use super::{
        CONFIG, PCM, PcmError, PcmStream, RelayOutcome, next_block_with_timeout, relay_connection,
    };
    use crate::{audio::StreamFormat, device::Event};

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

    #[tokio::test(start_paused = true)]
    async fn times_out_when_a_connected_stream_stops_delivering_blocks() {
        let (mut writer, reader) = duplex(128);
        writer.write_all(&config()).await.unwrap();
        let mut stream = PcmStream::handshake(reader).await.unwrap();
        let idle_timeout = Duration::from_secs(5);

        assert!(matches!(
            next_block_with_timeout(&mut stream, idle_timeout).await,
            Err(PcmError::IdleTimeout(timeout)) if timeout == idle_timeout
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_connection_emits_a_disconnect_event() {
        let (mut writer, reader) = duplex(128);
        writer.write_all(&config()).await.unwrap();
        let stream = PcmStream::handshake(reader).await.unwrap();
        let shutdown = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let relay_shutdown = shutdown.child_token();
        let relay = tokio::spawn(async move {
            relay_connection(stream, Duration::from_secs(5), &event_tx, &relay_shutdown).await
        });

        assert!(matches!(
            event_rx.recv().await,
            Some(Event::AudioConnected { .. })
        ));
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::AudioDisconnected)
        ));

        assert_eq!(relay.await.unwrap(), RelayOutcome::Reconnect);
        drop(writer);
    }
}
