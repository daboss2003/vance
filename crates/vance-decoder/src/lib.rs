//! `vance-decoder` — sidecar process + client library for decoding video.
//!
//! Per ADR 0004 the decoder runs out-of-process. This crate provides both
//! sides:
//!
//! - The wire protocol (`DecoderRequest` / `DecoderResponse` in `protocol`).
//! - The client (`DecoderClient`), used by `vance-engine` to drive a sidecar.
//! - The sidecar binary (`src/bin/main.rs`), which embeds FFmpeg and serves
//!   the protocol over stdin/stdout.
//!
//! Milestone 1.2 scope: open / info / seek / decode-at-PTS / close. Frames
//! are returned inline in the IPC response for now; shared memory lands
//! when 4K performance becomes a measurable problem (Milestone 1.5+).

#![warn(missing_docs)]

pub mod client;
pub mod protocol;

pub use client::DecoderClient;
pub use protocol::{
    DecoderRequest, DecoderResponse, FrameData, OpenError, PixelFormat, StreamInfo,
};
