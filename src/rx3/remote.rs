//! Decoder and connection supervisor for the RX3R remote-control stream
//! exposed by the `rx3-toolkit` firmware addition.

use std::{io::ErrorKind, net::SocketAddr, time::Duration};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, watch},
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

use crate::device::{ControlEvent, ControlStreamEvent, ControlStreamInfo, Device};

use super::{wait_for_device, wait_to_retry};

const MAGIC: &[u8; 4] = b"RX3R";
const VERSION: u8 = 1;
const HELLO: u8 = 1;
const SCHEMA: u8 = 2;
const SUBSCRIBE: u8 = 3;
const EVENT: u8 = 4;
const ERROR: u8 = 7;
const PING: u8 = 8;
const PONG: u8 = 9;
const CAP_EVENTS: u32 = 1;
const EVENT_CONTROLS: u32 = 1;
const HEADER_SIZE: usize = 16;
const HELLO_SIZE: usize = 16;
const SCHEMA_SIZE: usize = 8;
const CONTROL_EVENT_SIZE: usize = 28;
const ERROR_SIZE: usize = 8;
const KEEPALIVE_SIZE: usize = 8;
const MAX_PAYLOAD: usize = 4096;

/// Failure to decode or communicate with an RX3R remote-control stream.
#[derive(Debug, Error)]
pub enum RemoteError {
    /// The underlying TCP stream failed.
    #[error("remote-control stream I/O failed")]
    Io(#[from] std::io::Error),

    /// A message does not begin with the RX3R protocol marker.
    #[error("message does not start with RX3R")]
    InvalidMagic,

    /// The sender uses an RX3R protocol version this decoder does not support.
    #[error("unsupported RX3R version {0}")]
    UnsupportedVersion(u8),

    /// The message type is not part of the supported RX3R protocol.
    #[error("unsupported RX3R message type {0}")]
    UnsupportedMessage(u8),

    /// The declared payload exceeds the protocol allocation limit.
    #[error("remote-control payload is too large: {0} bytes")]
    PayloadTooLarge(usize),

    /// A protocol payload has an unexpected byte length.
    #[error("invalid {kind} payload length {actual}; expected {expected}")]
    InvalidPayloadLength {
        kind: &'static str,
        actual: usize,
        expected: usize,
    },

    /// The connection did not begin with the expected handshake messages.
    #[error("invalid remote-control handshake")]
    InvalidHandshake,

    /// The server cannot publish control events.
    #[error("remote-control server does not advertise event support")]
    EventsUnsupported,

    /// An unsolicited message used a request identifier.
    #[error("unsolicited remote-control message has request ID {0}")]
    UnexpectedRequestId(u32),

    /// The device rejected a protocol operation.
    #[error("remote-control server returned error {code} with detail {detail}")]
    Peer { code: u32, detail: u32 },

    /// A keepalive response did not arrive before the next deadline.
    #[error("remote-control keepalive timed out after {0:?}")]
    KeepaliveTimeout(Duration),
}

#[derive(Debug)]
struct Message {
    kind: u8,
    request_id: u32,
    payload: Vec<u8>,
}

/// A handshaken RX3R stream subscribed to control events.
struct RemoteStream<S> {
    io: S,
    info: ControlStreamInfo,
}

impl<S: AsyncRead + AsyncWrite + Unpin> RemoteStream<S> {
    async fn handshake(mut io: S) -> Result<Self, RemoteError> {
        let hello = read_message(&mut io)
            .await?
            .ok_or_else(unexpected_handshake_eof)?;
        if hello.kind != HELLO || hello.request_id != 0 {
            return Err(RemoteError::InvalidHandshake);
        }
        expect_length("hello", &hello.payload, HELLO_SIZE)?;
        let capabilities = u32::from_be_bytes(hello.payload[12..16].try_into().unwrap());
        if capabilities & CAP_EVENTS == 0 {
            return Err(RemoteError::EventsUnsupported);
        }

        let schema = read_message(&mut io)
            .await?
            .ok_or_else(unexpected_handshake_eof)?;
        if schema.kind != SCHEMA || schema.request_id != 0 {
            return Err(RemoteError::InvalidHandshake);
        }
        expect_length("schema", &schema.payload, SCHEMA_SIZE)?;

        write_message(&mut io, SUBSCRIBE, 0, &EVENT_CONTROLS.to_be_bytes()).await?;

        Ok(Self {
            io,
            info: ControlStreamInfo {
                rbp_build_prefix: hello.payload[0..4].try_into().unwrap(),
                firmware_major: u16::from_be_bytes(hello.payload[4..6].try_into().unwrap()),
                firmware_minor: u16::from_be_bytes(hello.payload[6..8].try_into().unwrap()),
                schema_revision: u32::from_be_bytes(hello.payload[8..12].try_into().unwrap()),
                schema_crc32: u32::from_be_bytes(schema.payload[0..4].try_into().unwrap()),
                control_count: u32::from_be_bytes(schema.payload[4..8].try_into().unwrap()),
            },
        })
    }

    async fn next_message(&mut self) -> Result<Option<Message>, RemoteError> {
        read_message(&mut self.io).await
    }

    async fn send(&mut self, kind: u8, request_id: u32, payload: &[u8]) -> Result<(), RemoteError> {
        write_message(&mut self.io, kind, request_id, payload).await
    }
}

/// Maintain the remote-control connection for the currently announced device.
pub async fn supervise(
    mut device_rx: watch::Receiver<Option<Device>>,
    port: u16,
    keepalive_interval: Duration,
    reconnect_delay: Duration,
    event_tx: mpsc::Sender<ControlStreamEvent>,
    shutdown: CancellationToken,
) -> Result<(), RemoteError> {
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
                eprintln!("failed to connect to RX3 remote-control stream at {address}: {error}");
                wait_to_retry(reconnect_delay, &shutdown).await;
                continue;
            }
        };
        let handshake = tokio::select! {
            handshake = RemoteStream::handshake(stream) => handshake,
            _ = shutdown.cancelled() => return Ok(()),
        };
        let stream = match handshake {
            Ok(stream) => stream,
            Err(error) => {
                eprintln!("RX3 remote-control handshake failed: {error}");
                wait_to_retry(reconnect_delay, &shutdown).await;
                continue;
            }
        };

        if relay_connection(stream, keepalive_interval, &event_tx, &shutdown).await
            == RelayOutcome::Stop
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

async fn relay_connection<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: RemoteStream<S>,
    keepalive_interval: Duration,
    event_tx: &mpsc::Sender<ControlStreamEvent>,
    shutdown: &CancellationToken,
) -> RelayOutcome {
    if !send_event(
        event_tx,
        ControlStreamEvent::Connected(stream.info.clone()),
        shutdown,
    )
    .await
    {
        return RelayOutcome::Stop;
    }

    let mut request_id = 1_u32;
    let mut awaiting_pong = None;
    let mut deadline = Instant::now() + keepalive_interval;

    loop {
        let message = tokio::select! {
            message = stream.next_message() => Some(message),
            _ = sleep_until(deadline) => None,
            _ = shutdown.cancelled() => return RelayOutcome::Stop,
        };

        let Some(message) = message else {
            if awaiting_pong.is_some() {
                eprintln!(
                    "RX3 remote-control stream disconnected: {}",
                    RemoteError::KeepaliveTimeout(keepalive_interval)
                );
                break;
            }

            let nonce = request_id.to_be_bytes();
            let mut payload = [0; KEEPALIVE_SIZE];
            payload[4..].copy_from_slice(&nonce);
            if let Err(error) = stream.send(PING, request_id, &payload).await {
                eprintln!("RX3 remote-control stream disconnected: {error}");
                break;
            }
            awaiting_pong = Some((request_id, payload));
            request_id = request_id.wrapping_add(1).max(1);
            deadline = Instant::now() + keepalive_interval;
            continue;
        };

        let message = match message {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                eprintln!("RX3 remote-control stream disconnected: {error}");
                break;
            }
        };

        match message.kind {
            EVENT => match decode_control_event(message) {
                Ok(event) => {
                    if !send_event(event_tx, ControlStreamEvent::Control(event), shutdown).await {
                        return RelayOutcome::Stop;
                    }
                }
                Err(error) => {
                    eprintln!("RX3 remote-control stream disconnected: {error}");
                    break;
                }
            },
            PING => {
                if let Err(error) = stream
                    .send(PONG, message.request_id, &message.payload)
                    .await
                {
                    eprintln!("RX3 remote-control stream disconnected: {error}");
                    break;
                }
            }
            PONG => {
                let Some((expected_id, expected_payload)) = awaiting_pong else {
                    eprintln!("RX3 remote-control stream sent an unexpected pong");
                    break;
                };
                if message.request_id != expected_id || message.payload != expected_payload {
                    eprintln!("RX3 remote-control stream sent a mismatched pong");
                    break;
                }
                awaiting_pong = None;
            }
            ERROR => match decode_peer_error(&message.payload) {
                Ok(error) => {
                    eprintln!("RX3 remote-control stream disconnected: {error}");
                    break;
                }
                Err(error) => {
                    eprintln!("RX3 remote-control stream disconnected: {error}");
                    break;
                }
            },
            kind => {
                eprintln!(
                    "RX3 remote-control stream disconnected: {}",
                    RemoteError::UnsupportedMessage(kind)
                );
                break;
            }
        }

        if awaiting_pong.is_none() {
            deadline = Instant::now() + keepalive_interval;
        }
    }

    if !send_event(event_tx, ControlStreamEvent::Disconnected, shutdown).await {
        return RelayOutcome::Stop;
    }

    RelayOutcome::Reconnect
}

async fn send_event(
    event_tx: &mpsc::Sender<ControlStreamEvent>,
    event: ControlStreamEvent,
    shutdown: &CancellationToken,
) -> bool {
    tokio::select! {
        result = event_tx.send(event) => result.is_ok(),
        _ = shutdown.cancelled() => false,
    }
}

async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<Message>, RemoteError> {
    let mut header = [0; HEADER_SIZE];
    match reader.read_exact(&mut header[..1]).await {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    reader.read_exact(&mut header[1..]).await?;

    if &header[..4] != MAGIC {
        return Err(RemoteError::InvalidMagic);
    }
    if header[4] != VERSION {
        return Err(RemoteError::UnsupportedVersion(header[4]));
    }
    if !matches!(header[5], HELLO | SCHEMA | EVENT | ERROR | PING | PONG) {
        return Err(RemoteError::UnsupportedMessage(header[5]));
    }

    let payload_size = u32::from_be_bytes(header[12..16].try_into().unwrap()) as usize;
    if payload_size > MAX_PAYLOAD {
        return Err(RemoteError::PayloadTooLarge(payload_size));
    }

    let mut payload = vec![0; payload_size];
    reader.read_exact(&mut payload).await?;

    Ok(Some(Message {
        kind: header[5],
        request_id: u32::from_be_bytes(header[8..12].try_into().unwrap()),
        payload,
    }))
}

async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    kind: u8,
    request_id: u32,
    payload: &[u8],
) -> Result<(), RemoteError> {
    if payload.len() > MAX_PAYLOAD {
        return Err(RemoteError::PayloadTooLarge(payload.len()));
    }

    let mut header = [0; HEADER_SIZE];
    header[..4].copy_from_slice(MAGIC);
    header[4] = VERSION;
    header[5] = kind;
    header[8..12].copy_from_slice(&request_id.to_be_bytes());
    header[12..16].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;

    Ok(())
}

fn decode_control_event(message: Message) -> Result<ControlEvent, RemoteError> {
    if message.request_id != 0 {
        return Err(RemoteError::UnexpectedRequestId(message.request_id));
    }
    expect_length("control event", &message.payload, CONTROL_EVENT_SIZE)?;

    Ok(ControlEvent {
        key_code: u16::from_be_bytes(message.payload[0..2].try_into().unwrap()),
        operation: message.payload[2],
        channel: message.payload[3],
        value: i32::from_be_bytes(message.payload[4..8].try_into().unwrap()),
        float_bits: u32::from_be_bytes(message.payload[8..12].try_into().unwrap()),
        auxiliary: i32::from_be_bytes(message.payload[12..16].try_into().unwrap()),
        source: message.payload[16],
        flags: message.payload[17],
        timestamp_us: u64::from_be_bytes(message.payload[20..28].try_into().unwrap()),
    })
}

fn decode_peer_error(payload: &[u8]) -> Result<RemoteError, RemoteError> {
    expect_length("error", payload, ERROR_SIZE)?;

    Ok(RemoteError::Peer {
        code: u32::from_be_bytes(payload[0..4].try_into().unwrap()),
        detail: u32::from_be_bytes(payload[4..8].try_into().unwrap()),
    })
}

fn expect_length(kind: &'static str, payload: &[u8], expected: usize) -> Result<(), RemoteError> {
    if payload.len() != expected {
        return Err(RemoteError::InvalidPayloadLength {
            kind,
            actual: payload.len(),
            expected,
        });
    }

    Ok(())
}

fn unexpected_handshake_eof() -> RemoteError {
    RemoteError::Io(std::io::Error::new(
        ErrorKind::UnexpectedEof,
        "stream ended during remote-control handshake",
    ))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::{
        CONTROL_EVENT_SIZE, EVENT, HELLO, PONG, RelayOutcome, RemoteStream, SCHEMA,
        decode_control_event, relay_connection,
    };
    use crate::device::{ControlEvent, ControlStreamEvent};

    fn message(kind: u8, request_id: u32, payload: &[u8]) -> Vec<u8> {
        let mut encoded = Vec::new();
        encoded.extend_from_slice(b"RX3R");
        encoded.push(1);
        encoded.push(kind);
        encoded.extend_from_slice(&0_u16.to_be_bytes());
        encoded.extend_from_slice(&request_id.to_be_bytes());
        encoded.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        encoded.extend_from_slice(payload);
        encoded
    }

    fn handshake() -> Vec<u8> {
        let mut hello = Vec::new();
        hello.extend_from_slice(&[0xcf, 0x30, 0x92, 0x38]);
        hello.extend_from_slice(&1_u16.to_be_bytes());
        hello.extend_from_slice(&19_u16.to_be_bytes());
        hello.extend_from_slice(&1_u32.to_be_bytes());
        hello.extend_from_slice(&1_u32.to_be_bytes());

        let mut schema = Vec::new();
        schema.extend_from_slice(&0x4257_e3cb_u32.to_be_bytes());
        schema.extend_from_slice(&89_u32.to_be_bytes());

        [message(HELLO, 0, &hello), message(SCHEMA, 0, &schema)].concat()
    }

    fn control_message() -> super::Message {
        let mut payload = vec![0; CONTROL_EVENT_SIZE];
        payload[0..2].copy_from_slice(&0x4101_u16.to_be_bytes());
        payload[2] = 0;
        payload[3] = 1;
        payload[4..8].copy_from_slice(&42_i32.to_be_bytes());
        payload[8..12].copy_from_slice(&1.0_f32.to_bits().to_be_bytes());
        payload[12..16].copy_from_slice(&(-1_i32).to_be_bytes());
        payload[16] = 1;
        payload[17] = 1;
        payload[20..28].copy_from_slice(&123_456_u64.to_be_bytes());

        super::Message {
            kind: EVENT,
            request_id: 0,
            payload,
        }
    }

    #[test]
    fn decodes_a_control_event_without_losing_raw_values() {
        assert_eq!(
            decode_control_event(control_message()).unwrap(),
            ControlEvent {
                key_code: 0x4101,
                operation: 0,
                channel: 1,
                value: 42,
                float_bits: 1.0_f32.to_bits(),
                auxiliary: -1,
                source: 1,
                flags: 1,
                timestamp_us: 123_456,
            }
        );
    }

    #[tokio::test]
    async fn handshakes_and_subscribes_to_control_events() {
        let (client, mut server) = duplex(256);
        server.write_all(&handshake()).await.unwrap();

        let stream = RemoteStream::handshake(client).await.unwrap();
        let mut subscription = [0; 20];
        server.read_exact(&mut subscription).await.unwrap();

        assert_eq!(stream.info.firmware_major, 1);
        assert_eq!(stream.info.firmware_minor, 19);
        assert_eq!(stream.info.control_count, 89);
        assert_eq!(&subscription[..4], b"RX3R");
        assert_eq!(subscription[5], super::SUBSCRIBE);
        assert_eq!(&subscription[16..20], &1_u32.to_be_bytes());
    }

    #[tokio::test(start_paused = true)]
    async fn a_half_open_connection_fails_its_keepalive() {
        let (client, mut server) = duplex(256);
        server.write_all(&handshake()).await.unwrap();
        let stream = RemoteStream::handshake(client).await.unwrap();
        let mut subscription = [0; 20];
        server.read_exact(&mut subscription).await.unwrap();

        let shutdown = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let relay_shutdown = shutdown.child_token();
        let relay = tokio::spawn(async move {
            relay_connection(stream, Duration::from_secs(5), &event_tx, &relay_shutdown).await
        });

        assert!(matches!(
            event_rx.recv().await,
            Some(ControlStreamEvent::Connected(_))
        ));
        assert!(matches!(
            event_rx.recv().await,
            Some(ControlStreamEvent::Disconnected)
        ));
        assert_eq!(relay.await.unwrap(), RelayOutcome::Reconnect);

        let mut ping = [0; 24];
        server.read_exact(&mut ping).await.unwrap();
        assert_eq!(ping[5], super::PING);
        assert_ne!(ping[5], PONG);
    }
}
