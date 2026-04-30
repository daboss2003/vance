//! Errors returned by `save` / `open`.

use std::path::PathBuf;
use thiserror::Error;

/// Result alias for project-bundle operations.
pub type Result<T, E = ProjectError> = core::result::Result<T, E>;

/// Errors surfaced when reading or writing a `.vance/` bundle.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ProjectError {
    /// The given path doesn't exist or isn't a directory.
    #[error("bundle path is not a directory: {0}")]
    NotABundle(PathBuf),

    /// `project.ron` is missing inside the bundle.
    #[error("bundle is missing project.ron: {0}")]
    MissingProjectFile(PathBuf),

    /// On-disk format version is newer than this build understands.
    /// Migrations (3.5) will downgrade `Unsupported` to a normal load
    /// path for known older versions.
    #[error("project format version {found} is newer than supported ({supported})")]
    UnsupportedFormatVersion {
        /// The version found in the file on disk.
        found: u32,
        /// The maximum version this build can read.
        supported: u32,
    },

    /// I/O error from the filesystem.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// RON parse error on load.
    #[error("ron parse: {0}")]
    RonParse(#[from] ron::de::SpannedError),

    /// RON serialization error on save.
    #[error("ron emit: {0}")]
    RonEmit(#[from] ron::Error),
}
