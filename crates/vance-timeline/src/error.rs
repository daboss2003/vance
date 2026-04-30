//! Errors returned by the timeline data model.
//!
//! Construction-time validation only at this milestone. Edit-operation
//! errors land alongside the operations themselves in 3.2.

use thiserror::Error;

/// Result alias for timeline-level operations.
pub type Result<T, E = TimelineError> = core::result::Result<T, E>;

/// Errors surfaced by the timeline crate.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum TimelineError {
    /// A constructor argument was out of range or otherwise invalid.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Wraps `vance_core::Error` so callers don't have to import both.
    #[error(transparent)]
    Core(#[from] vance_core::Error),
}

impl TimelineError {
    /// Convenience for `InvalidArgument` with a `String` message.
    pub fn invalid<S: Into<String>>(msg: S) -> Self {
        TimelineError::InvalidArgument(msg.into())
    }
}
