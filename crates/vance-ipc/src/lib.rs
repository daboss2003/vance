//! `vance-ipc` — process-boundary IPC primitives.
//!
//! Vance's process model (ADR 0004) puts decoders, plugins, and the AI
//! runtime in sidecar processes. This crate provides the framing,
//! transport, and supervision they all share.
//!
//! Milestone 1.1 scope:
//! - Length-prefixed bincode framing over any `Read`/`Write`.
//! - A `Sidecar<Req, Resp>` supervisor that spawns a child binary and
//!   exchanges typed request/response messages over its stdin/stdout.
//! - A child-side `serve` helper that runs a request handler loop.
//!
//! Shared-memory frame transport lands in Milestone 1.2 when `vance-decoder`
//! actually has frames to ship.
//!
//! ## Why stdin/stdout, not Unix sockets?
//!
//! Stdin/stdout pipes are the same on every OS (no `\\.\pipe\` vs
//! `/tmp/something.sock`), require no cleanup, and are inherited cleanly
//! by the child. The cost is half-duplex per stream — but with two streams
//! (parent's stdout = child's stdin for commands; child's stdout = parent's
//! stdin for responses) we get a clean full-duplex channel. Good enough
//! for command/response; Milestone 1.2 adds shared memory for bulk frame
//! data so the IPC channel never carries pixel buffers.

#![warn(missing_docs)]

mod error;
mod frame;
mod sidecar;

pub use error::{Error, Result};
pub use frame::{read_frame, write_frame, MAX_FRAME_BYTES};
pub use sidecar::{serve, Sidecar};
