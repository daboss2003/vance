//! IPC error types.

use thiserror::Error;

/// Result alias for `vance-ipc`.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors crossing the IPC boundary.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum Error {
    /// Underlying I/O failure (pipe closed, write failed, etc.).
    #[error("ipc i/o: {0}")]
    Io(#[from] std::io::Error),

    /// Bincode encode failed.
    #[error("ipc encode: {0}")]
    Encode(#[from] bincode::error::EncodeError),

    /// Bincode decode failed.
    #[error("ipc decode: {0}")]
    Decode(#[from] bincode::error::DecodeError),

    /// Frame size header was implausible (likely corrupt stream).
    #[error("ipc frame too large: {size} bytes (max {})", crate::MAX_FRAME_BYTES)]
    FrameTooLarge {
        /// Reported (or actual) frame size in bytes.
        size: u32,
    },

    /// Read returned EOF before a complete frame was assembled.
    #[error("ipc unexpected eof while reading frame")]
    UnexpectedEof,

    /// Child sidecar process exited unexpectedly.
    #[error("sidecar exited: {0}")]
    SidecarExited(String),

    /// Sidecar binary could not be spawned.
    #[error("failed to spawn sidecar {program:?}: {source}")]
    SpawnFailed {
        /// Path to the binary we tried to launch.
        program: String,
        /// Underlying OS error.
        source: std::io::Error,
    },
}
