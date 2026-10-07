use std::{net::Ipv4Addr, time::Duration};

use thiserror::Error;
use tokio::{
    net::UdpSocket,
    sync::watch,
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

use crate::device::{Device, SupportedDevice, UnsupportedDeviceError};

const ANNOUNCEMENT_TYPE: u8 = 0x06;
const HEADER: [u8; 10] = [0x51, 0x73, 0x70, 0x74, 0x31, 0x57, 0x6d, 0x4a, 0x4f, 0x4c];
const MIN_PACKET_SIZE: usize = 0x35;

/// Failure to decode or receive a PRO DJ LINK announcement.
#[derive(Debug, Error)]
pub enum AnnouncementError {
    /// The datagram ends before all required announcement fields.
    #[error("announcement packet is too short: {0} bytes")]
    TooShort(usize),

    /// The datagram does not carry the PRO DJ LINK magic bytes.
    #[error("announcement packet has an invalid PRO DJ LINK header")]
    InvalidHeader,

    /// The fixed-width device name is not valid UTF-8.
    #[error("device name is not valid UTF-8")]
    InvalidName(#[from] std::str::Utf8Error),

    /// The announced model has no Cueback audio adapter.
    #[error(transparent)]
    UnsupportedDevice(#[from] UnsupportedDeviceError),

    /// The announcement socket failed.
    #[error("failed to receive announcements")]
    Io(#[from] std::io::Error),
}

/// Decode a PRO DJ LINK device announcement.
///
/// Other PRO DJ LINK packet types return `Ok(None)` so callers can share a
/// socket with the rest of the protocol traffic.
pub fn parse(packet: &[u8]) -> Result<Option<Device>, AnnouncementError> {
    if packet.len() < HEADER.len() + 1 {
        return Err(AnnouncementError::TooShort(packet.len()));
    }
    if packet[..HEADER.len()] != HEADER {
        return Err(AnnouncementError::InvalidHeader);
    }
    if packet[0x0a] != ANNOUNCEMENT_TYPE {
        return Ok(None);
    }
    if packet.len() < MIN_PACKET_SIZE {
        return Err(AnnouncementError::TooShort(packet.len()));
    }

    let name_bytes = &packet[0x0c..0x20];
    let name_length = name_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name_bytes.len());
    let name = std::str::from_utf8(&name_bytes[..name_length])?;

    Ok(Some(Device {
        model: SupportedDevice::try_from(name)?,
        id: packet[0x24],
        mac_address: packet[0x26..0x2c]
            .try_into()
            .expect("packet length checked above"),
        ip_address: Ipv4Addr::new(packet[0x2c], packet[0x2d], packet[0x2e], packet[0x2f]),
        kind: packet[0x34],
    }))
}

/// Track RX3 availability from periodic PRO DJ LINK announcements.
pub async fn watch_rx3(
    socket: UdpSocket,
    timeout: Duration,
    device_tx: watch::Sender<Option<Device>>,
    shutdown: CancellationToken,
) -> Result<(), AnnouncementError> {
    let mut buffer = [0; 2048];
    let mut last_seen = None;
    let mut expiration = tokio::time::interval(Duration::from_secs(1));
    expiration.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            received = socket.recv_from(&mut buffer) => {
                let (length, _) = received?;
                let Ok(Some(device)) = parse(&buffer[..length]) else {
                    continue;
                };
                last_seen = Some(Instant::now());
                device_tx.send_replace(Some(device));
            }
            _ = expiration.tick() => {
                if last_seen.is_some_and(|seen| seen.elapsed() >= timeout) {
                    last_seen = None;
                    device_tx.send_replace(None);
                }
            }
            _ = shutdown.cancelled() => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use crate::device::{Device, SupportedDevice};

    use super::{AnnouncementError, HEADER, parse};

    fn announcement(name: &str) -> Vec<u8> {
        let mut packet = vec![0; 0x36];
        packet[..HEADER.len()].copy_from_slice(&HEADER);
        packet[0x0a] = 0x06;
        packet[0x0c..0x0c + name.len()].copy_from_slice(name.as_bytes());
        packet[0x24] = 1;
        packet[0x26..0x2c].copy_from_slice(&[0, 1, 2, 3, 4, 5]);
        packet[0x2c..0x30].copy_from_slice(&[10, 0, 0, 42]);
        packet[0x34] = 7;
        packet
    }

    #[test]
    fn parses_a_device_announcement() {
        assert_eq!(
            parse(&announcement("XDJ-RX3")).unwrap(),
            Some(Device {
                model: SupportedDevice::XdjRx3,
                id: 1,
                kind: 7,
                mac_address: [0, 1, 2, 3, 4, 5],
                ip_address: Ipv4Addr::new(10, 0, 0, 42),
            })
        );
    }

    #[test]
    fn ignores_other_pro_dj_link_packet_types() {
        let mut packet = announcement("XDJ-RX3");
        packet[0x0a] = 0x0a;

        assert_eq!(parse(&packet).unwrap(), None);
    }

    #[test]
    fn rejects_truncated_announcements() {
        assert!(parse(&announcement("XDJ-RX3")[..20]).is_err());
    }

    #[test]
    fn rejects_unsupported_devices() {
        assert!(matches!(
            parse(&announcement("CDJ-3000")),
            Err(AnnouncementError::UnsupportedDevice(_))
        ));
    }
}
