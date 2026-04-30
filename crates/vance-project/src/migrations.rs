//! Migration framework for the on-disk project file.
//!
//! ## Strategy
//!
//! Every breaking schema change bumps [`FORMAT_VERSION`] and ships a
//! `migrate_vN_to_v(N+1)` step plus a typed snapshot struct
//! `ProjectFileVN` that can deserialize the *old* shape. `open` picks
//! the right typed deserializer based on the file's header version,
//! then runs the chain forward to today's `ProjectFile`.
//!
//! ## Why not serde rename + defaults?
//!
//! Field-level renames and `#[serde(default)]` cover trivial schema
//! drift without any framework at all. The migration chain is for
//! changes those tools can't express — types being merged or split,
//! enum variants reshuffled, semantic redefinitions of existing fields.
//! Day one (FORMAT_VERSION = 1) has no migrations because there is no
//! prior version. The framework's job here is to fail closed: an
//! envelope with `format_version` outside the supported range returns
//! `UnsupportedFormatVersion` instead of silently mis-decoding.
//!
//! ## Adding a migration
//!
//! 1. Bump [`FORMAT_VERSION`] in `bundle.rs` to N+1.
//! 2. In a child module here, declare a `ProjectFileVN` struct that
//!    matches the **previous** schema (copy the old field set
//!    verbatim).
//! 3. Add `fn migrate_vN_to_v(N+1)(prev: ProjectFileVN) -> ProjectFile`.
//! 4. In `bundle::open`, peek the version header, dispatch to the
//!    right typed deserializer, then call the chain.
//!
//! The chain itself is `for v in from..FORMAT_VERSION { run step v→v+1 }`.

use crate::bundle::{ProjectFile, FORMAT_VERSION};
use crate::error::{ProjectError, Result};

/// Minimum on-disk version this build still knows how to read.
///
/// Files older than this are rejected at load time with a clear error
/// rather than silently mis-decoded. Set to 1 for now; bump when a
/// historical migration is dropped.
pub const MIN_SUPPORTED_VERSION: u32 = 1;

/// Bring an envelope at any version in `[MIN_SUPPORTED_VERSION,
/// FORMAT_VERSION]` up to the current schema.
///
/// Today this is effectively the identity for v1 envelopes and a hard
/// reject otherwise; once the chain has steps, this is where they get
/// stitched together.
pub fn migrate_to_current(envelope: ProjectFile) -> Result<ProjectFile> {
    if envelope.format_version > FORMAT_VERSION {
        return Err(ProjectError::UnsupportedFormatVersion {
            found: envelope.format_version,
            supported: FORMAT_VERSION,
        });
    }
    if envelope.format_version < MIN_SUPPORTED_VERSION {
        // Bundle is from before the earliest version this build still
        // supports. Surface the same error variant — the user-facing
        // remedy ("open the file in an older Vance and re-save") is the
        // same for both ends of the version window.
        return Err(ProjectError::UnsupportedFormatVersion {
            found: envelope.format_version,
            supported: FORMAT_VERSION,
        });
    }
    // No migration steps yet (FORMAT_VERSION = 1 = MIN_SUPPORTED_VERSION).
    // When schema changes, dispatch each step here:
    //
    //     let mut env = envelope;
    //     while env.format_version < FORMAT_VERSION {
    //         env = match env.format_version {
    //             1 => migrate_v1_to_v2(env)?,
    //             _ => unreachable!("guarded above"),
    //         };
    //     }
    //     Ok(env)
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vance_timeline::Project;

    #[test]
    fn current_version_passes_through() {
        let env = ProjectFile {
            format_version: FORMAT_VERSION,
            project: Project::new("p"),
        };
        assert_eq!(
            migrate_to_current(env.clone()).unwrap().format_version,
            FORMAT_VERSION
        );
    }

    #[test]
    fn future_version_is_rejected() {
        let env = ProjectFile {
            format_version: FORMAT_VERSION + 1,
            project: Project::new("p"),
        };
        match migrate_to_current(env).unwrap_err() {
            ProjectError::UnsupportedFormatVersion { found, supported } => {
                assert_eq!(found, FORMAT_VERSION + 1);
                assert_eq!(supported, FORMAT_VERSION);
            }
            e => panic!("expected UnsupportedFormatVersion, got {e:?}"),
        }
    }

    #[test]
    fn pre_min_version_is_rejected() {
        // Synthesize a "v0" envelope. Today this is rejected; once a
        // v0→v1 migration ships, MIN_SUPPORTED_VERSION drops and this
        // test gets retired (or flipped).
        let env = ProjectFile {
            format_version: 0,
            project: Project::new("p"),
        };
        assert!(matches!(
            migrate_to_current(env),
            Err(ProjectError::UnsupportedFormatVersion { .. })
        ));
    }
}
