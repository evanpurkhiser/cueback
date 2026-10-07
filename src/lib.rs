//! Automatic capture and session detection for DJ audio streams.
#![warn(missing_docs)]

/// Runtime assembly and task supervision.
pub mod app;
/// PCM format types and audible-frame analysis.
pub mod audio;
/// Command-line interface definitions.
pub mod cli;
/// File-backed runtime configuration.
pub mod config;
/// Device-neutral identity and audio events.
pub mod device;
/// Managed FFmpeg FLAC recording.
pub mod recorder;
/// Recording workflow driven by device events.
pub mod service;
/// Pure live-session state transitions.
pub mod session;

mod rx3;
