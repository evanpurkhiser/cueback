use std::net::Ipv4Addr;

use thiserror::Error;

use crate::audio::{PcmBlock, StreamFormat};

/// Device models whose audio stream Cueback knows how to consume.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupportedDevice {
    /// Pioneer DJ XDJ-RX3 all-in-one DJ system.
    XdjRx3,
}

impl TryFrom<&str> for SupportedDevice {
    type Error = UnsupportedDeviceError;

    fn try_from(name: &str) -> Result<Self, Self::Error> {
        match name {
            "XDJ-RX3" => Ok(Self::XdjRx3),
            _ => Err(UnsupportedDeviceError {
                name: name.to_owned(),
            }),
        }
    }
}

/// A PRO DJ LINK device model that Cueback cannot record.
#[derive(Debug, Error, Eq, PartialEq)]
#[error("unsupported device {name}")]
pub struct UnsupportedDeviceError {
    name: String,
}

/// Identity and network location learned from a device announcement.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Device {
    /// Supported hardware model.
    pub model: SupportedDevice,

    /// PRO DJ LINK player number.
    pub id: u8,

    /// Device type byte reported by PRO DJ LINK.
    pub kind: u8,

    /// Hardware MAC address.
    pub mac_address: [u8; 6],

    /// Address used to open the PCM stream.
    pub ip_address: Ipv4Addr,
}

/// Changes and PCM data emitted by a device audio adapter.
#[derive(Debug)]
pub enum Event {
    /// A new PCM connection completed its format handshake.
    AudioConnected {
        /// Format of PCM blocks for this connection.
        format: StreamFormat,
    },

    /// One timestamped block of interleaved PCM samples.
    Pcm(PcmBlock),

    /// The active PCM connection ended.
    AudioDisconnected,
}
