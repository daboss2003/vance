//! Workspace-wide error type.
//!
//! `vance-core` defines the *categories* of error every layer can produce.
//! Specific crates wrap their own errors in their own `thiserror` enums and
//! convert at the boundary.

use thiserror::Error;

/// Standard `Result` alias used throughout Vance.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Coarse error categories. Concrete causes live in inner crates.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum Error {
    /// I/O failure (filesystem, network, IPC).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Invalid input from outside the engine (project file, user data, IPC).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Internal invariant violated. Indicates a bug in Vance itself.
    #[error("invariant violated: {0}")]
    Invariant(String),

    /// Operation would be valid but is not yet implemented.
    #[error("not yet implemented: {0}")]
    Unimplemented(&'static str),

    /// Wrapped error from another crate that we don't classify further.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Construct an `InvalidInput` error from anything `Display`-able.
    pub fn invalid_input(msg: impl Into<String>) -> Self {
        Self::InvalidInput(msg.into())
    }

    /// Construct an `Invariant` error from anything `Display`-able.
    pub fn invariant(msg: impl Into<String>) -> Self {
        Self::Invariant(msg.into())
    }

    /// Construct an `Other` error.
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_render() {
        assert_eq!(
            format!("{}", Error::invalid_input("foo")),
            "invalid input: foo"
        );
        assert_eq!(
            format!("{}", Error::invariant("bar")),
            "invariant violated: bar"
        );
        assert_eq!(
            format!("{}", Error::Unimplemented("baz")),
            "not yet implemented: baz"
        );
        assert_eq!(format!("{}", Error::other("qux")), "qux");
    }

    #[test]
    fn io_error_converts() {
        let io: std::io::Error = std::io::Error::new(std::io::ErrorKind::NotFound, "x");
        let e: Error = io.into();
        assert!(matches!(e, Error::Io(_)));
    }
}
