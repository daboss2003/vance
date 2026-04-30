//! `vance-project` — on-disk project bundle.
//!
//! A Vance project lives in a directory with the `.vance` extension:
//!
//! ```text
//! myproject.vance/
//!   project.ron       # the [`Project`] (RON, with a `version` header)
//!   media/            # links to source media (Phase 4+: hash-based relink)
//!   cache/            # thumbnails, waveforms, proxies
//!   autosave/         # ring of last N autosaves (Phase 4+)
//! ```
//!
//! ## Atomicity
//!
//! `save` writes `project.ron` to a sibling temp file, fsyncs it, and
//! renames it onto the canonical name. POSIX `rename(2)` is atomic
//! within a directory, so a power loss either leaves the *old*
//! `project.ron` intact or leaves the *new* one in place — never a
//! half-written file.
//!
//! Phase 3.4 scope: round-trip a `Project` through `save` and `open`.
//! Versioning + migrations land in 3.5; the on-disk layout already
//! carries a `format_version` field so 3.5 has somewhere to hook in.

#![warn(missing_docs)]

mod bundle;
mod error;
pub mod migrations;

pub use bundle::{open, save, ProjectFile, FORMAT_VERSION};
pub use error::{ProjectError, Result};
pub use migrations::{migrate_to_current, MIN_SUPPORTED_VERSION};
