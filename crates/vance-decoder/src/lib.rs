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
//! Scope: open / info / seek / decode-at-PTS / streaming / close. Pixels
//! cross the process boundary as a raw blob after each `Frame` reply, or in
//! a shared-memory slot once one is attached (ADR 0006).

#![warn(missing_docs)]

pub mod client;
pub mod frame;
pub mod hwaccel;
pub mod protocol;

pub use client::{thread_budget, ClientOptions, DecoderClient, ThreadBudget, TransportStats};
pub use frame::{FrameBytes, FrameData};
pub use protocol::{
    DecoderEnvelope, DecoderRequest, DecoderResponse, FrameMeta, FramePayload, OpenError,
    PixelFormat, StreamInfo,
};
