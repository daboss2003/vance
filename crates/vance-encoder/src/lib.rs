//! `vance-encoder` — sidecar-based FFmpeg encoder.
//!
//! Mirrors `vance-decoder`: a parent process spawns one or more
//! sidecar binaries, each handling a single in-progress encode. The
//! parent feeds frames + audio over IPC and the sidecar writes the
//! container file.
//!
//! Public API:
//! - [`protocol`] — wire types shared between parent and sidecar.
//! - [`EncoderClient`] — parent-side wrapper around one sidecar.

#![warn(missing_docs)]

pub mod client;
pub mod protocol;

pub use client::{EncoderClient, EncoderClientError};
pub use protocol::{
    AudioCodec, AudioConfig, AudioSampleFormat, ColorSignaling, EncoderError, EncoderPreset,
    EncoderRequest, EncoderResponse, HdrMetadata, MasteringDisplay, VideoConfig, VideoPixelFormat,
    PROTOCOL_VERSION,
};
