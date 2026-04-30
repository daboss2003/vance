//! Bundle layout, save, and open.
//!
//! The on-disk format is a `ProjectFile` envelope (versioned) wrapping
//! the in-memory `vance_timeline::Project`. Keeping the version on the
//! envelope (rather than inside `Project`) lets the project type evolve
//! independently of the file schema and gives migrations a clean entry
//! point.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use vance_timeline::Project;

use crate::error::{ProjectError, Result};
use crate::migrations::migrate_to_current;

/// Latest on-disk format version this build can write.
///
/// Bumped when the file schema changes in a way that requires a
/// migration (added/removed fields, changed semantics). The in-memory
/// `Project` type can evolve freely between bumps.
pub const FORMAT_VERSION: u32 = 1;

/// On-disk envelope around a `Project`. The version travels with the
/// file so future loaders can dispatch through the migration chain.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectFile {
    /// Schema version of this file. See [`FORMAT_VERSION`].
    pub format_version: u32,
    /// The actual project payload.
    pub project: Project,
}

impl ProjectFile {
    /// Wrap `project` in a `ProjectFile` stamped with the current format
    /// version. Use this when constructing a new file to write.
    pub fn current(project: Project) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            project,
        }
    }
}

/// Path to `<bundle>/project.ron`.
fn project_ron_path(bundle: &Path) -> PathBuf {
    bundle.join("project.ron")
}

/// Make sure the bundle's three sibling directories exist.
fn ensure_subdirs(bundle: &Path) -> Result<()> {
    for sub in ["media", "cache", "autosave"] {
        fs::create_dir_all(bundle.join(sub))?;
    }
    Ok(())
}

/// Save `project` into the `.vance/` bundle at `bundle_path`. Creates
/// the directory and required subdirectories if missing. The
/// `project.ron` write is atomic (temp file + rename + fsync).
pub fn save(project: &Project, bundle_path: impl AsRef<Path>) -> Result<()> {
    let bundle = bundle_path.as_ref();
    fs::create_dir_all(bundle)?;
    ensure_subdirs(bundle)?;

    let envelope = ProjectFile::current(project.clone());
    // Pretty RON for human-friendliness; keeps the file diffable.
    let pretty = ron::ser::PrettyConfig::new()
        .struct_names(true)
        .indentor("  ".to_string());
    let body = ron::ser::to_string_pretty(&envelope, pretty)?;

    // Atomic write: temp file in the same directory, fsync, rename.
    // Same-directory rename is atomic on POSIX and on NTFS via
    // ReplaceFileW (which std::fs::rename uses under the hood).
    let final_path = project_ron_path(bundle);
    let tmp_path = bundle.join(".project.ron.tmp");
    {
        let mut f = fs::File::create(&tmp_path)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

/// Load the project at `bundle_path`. Returns `Project` (the envelope
/// is unwrapped). 3.5 will run migrations here when the on-disk
/// `format_version` is older than [`FORMAT_VERSION`].
pub fn open(bundle_path: impl AsRef<Path>) -> Result<Project> {
    let bundle = bundle_path.as_ref();
    if !bundle.is_dir() {
        return Err(ProjectError::NotABundle(bundle.to_path_buf()));
    }
    let ron_path = project_ron_path(bundle);
    if !ron_path.is_file() {
        return Err(ProjectError::MissingProjectFile(ron_path));
    }

    let body = fs::read_to_string(&ron_path)?;
    let envelope: ProjectFile = ron::de::from_str(&body)?;

    // Run the migration chain (no-op on v1; rejects future / pre-min
    // versions with UnsupportedFormatVersion).
    let envelope = migrate_to_current(envelope)?;
    Ok(envelope.project)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use vance_timeline::{
        Clip, EditLog, EditOp, MediaItem, MediaKind, Project, Sequence, TrackId, TrackKind,
    };
    use vance_core::{FrameRate, RationalTime};

    fn t(num: i64) -> RationalTime {
        RationalTime::new(num, 1).unwrap()
    }

    fn build_project_with_one_clip() -> Project {
        let mut p = Project::new("Test Project");
        let media = MediaItem::new(
            PathBuf::from("/tmp/clip.mov"),
            "clip.mov",
            MediaKind::Video,
            t(30),
        )
        .with_frame_rate(FrameRate::FPS_24);
        let media_id = media.id;
        p.media.insert(media_id, Arc::new(media));

        let mut log = EditLog::new(Sequence::new("Seq 01", FrameRate::FPS_24));
        let track_id = TrackId::new();
        log.apply(EditOp::AddTrack {
            track_id,
            kind: TrackKind::Video,
            name: "V1".into(),
        })
        .unwrap();
        let clip = Clip::new(media_id, "clip.mov", t(0), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id,
            clip,
        })
        .unwrap();

        p.sequences.push(Arc::new(log.current().clone()));
        p
    }

    #[test]
    fn save_creates_expected_bundle_layout() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("My.vance");
        let project = build_project_with_one_clip();
        save(&project, &bundle).unwrap();

        assert!(bundle.is_dir(), "bundle directory exists");
        assert!(bundle.join("project.ron").is_file(), "project.ron written");
        assert!(bundle.join("media").is_dir(), "media/ created");
        assert!(bundle.join("cache").is_dir(), "cache/ created");
        assert!(bundle.join("autosave").is_dir(), "autosave/ created");
    }

    #[test]
    fn round_trip_preserves_project() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("My.vance");
        let project = build_project_with_one_clip();

        save(&project, &bundle).unwrap();
        let reopened = open(&bundle).unwrap();
        assert_eq!(reopened, project);
    }

    #[test]
    fn open_missing_directory_errors_clearly() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("nope.vance");
        match open(&bundle).unwrap_err() {
            ProjectError::NotABundle(p) => assert_eq!(p, bundle),
            e => panic!("expected NotABundle, got {e:?}"),
        }
    }

    #[test]
    fn open_missing_project_ron_errors_clearly() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("empty.vance");
        fs::create_dir_all(&bundle).unwrap();
        match open(&bundle).unwrap_err() {
            ProjectError::MissingProjectFile(_) => {}
            e => panic!("expected MissingProjectFile, got {e:?}"),
        }
    }

    #[test]
    fn open_rejects_future_format_version() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("future.vance");
        ensure_subdirs(&bundle).unwrap();
        // Write a forged file pretending to be a future format.
        let project = Project::new("p");
        let envelope = ProjectFile {
            format_version: FORMAT_VERSION + 99,
            project,
        };
        let body = ron::ser::to_string_pretty(&envelope, ron::ser::PrettyConfig::new()).unwrap();
        fs::write(bundle.join("project.ron"), body).unwrap();
        match open(&bundle).unwrap_err() {
            ProjectError::UnsupportedFormatVersion { found, supported } => {
                assert_eq!(found, FORMAT_VERSION + 99);
                assert_eq!(supported, FORMAT_VERSION);
            }
            e => panic!("expected UnsupportedFormatVersion, got {e:?}"),
        }
    }

    #[test]
    fn save_overwrites_existing_bundle() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("My.vance");
        let p1 = Project::new("first");
        let p2 = Project::new("second");
        save(&p1, &bundle).unwrap();
        save(&p2, &bundle).unwrap();
        let reopened = open(&bundle).unwrap();
        assert_eq!(reopened.name, "second");
    }
}
