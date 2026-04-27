//! Viewport errors.

use thiserror::Error;

/// Result alias for `vance-viewport`.
pub type Result<T> = std::result::Result<T, ViewportError>;

/// Errors during viewport install / render.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ViewportError {
    /// Caller passed a host handle of the wrong platform variant.
    #[error("host handle mismatch: this build expected {expected}")]
    WrongHandleKind {
        /// What the current platform's `Viewport::install` requires
        /// (e.g. "AppKit NSWindow").
        expected: &'static str,
    },

    /// We tried to talk to AppKit / Win32 / GTK from a non-main thread.
    #[error("viewport call must originate on the main thread")]
    NotMainThread,

    /// The host window had no content view / client area we could attach to.
    #[error("host window has no usable content view")]
    NoContentView,

    /// Failed to create a wgpu surface from the platform layer.
    #[error("wgpu surface create: {0}")]
    Surface(String),

    /// The platform path is not implemented in this build (Win/Linux pending
    /// the spike re-verification before Phase 1 ends).
    #[error("viewport not implemented on this platform yet — see ADR 0005")]
    Unimplemented,
}
