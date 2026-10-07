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

/// Remote-control protocol identity reported when a control stream connects.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlStreamInfo {
    /// Prefix of the firmware executable build identity.
    pub rbp_build_prefix: [u8; 4],

    /// Firmware major version.
    pub firmware_major: u16,

    /// Firmware minor version.
    pub firmware_minor: u16,

    /// Revision of the remote-control schema.
    pub schema_revision: u32,

    /// CRC-32 of the control catalog.
    pub schema_crc32: u32,

    /// Number of controls declared by the catalog.
    pub control_count: u32,
}

/// One source-timestamped control callback from the device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlEvent {
    /// Firmware control identifier.
    pub key_code: u16,

    /// Firmware operation code.
    pub operation: u8,

    /// Deck or mixer channel, or zero for global controls.
    pub channel: u8,

    /// Integer control value.
    pub value: i32,

    /// Raw IEEE-754 bits supplied by the firmware.
    pub float_bits: u32,

    /// Additional firmware-specific integer value.
    pub auxiliary: i32,

    /// Firmware source code distinguishing physical and remote input.
    pub source: u8,

    /// Firmware event flags, including queue loss before this event.
    pub flags: u8,

    /// Device `CLOCK_MONOTONIC` time captured at the input callback.
    pub timestamp_us: u64,
}

/// Lifecycle and control events emitted by a device control adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlStreamEvent {
    /// A remote-control stream completed its handshake and subscription.
    Connected(ControlStreamInfo),

    /// A physical or remotely injected control callback.
    Control(ControlEvent),

    /// The active remote-control stream ended.
    Disconnected,
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
