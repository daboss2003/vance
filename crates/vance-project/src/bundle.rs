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
    let mut project = envelope.project;
    // Phase 7.2 invariant: every MediaKind::Audio item carries audio
    // by definition. Legacy bundles (pre-7.2) saved before the field
    // existed deserialize with has_audio_track=false, so force it
    // true here for Audio items. Video items stay at whatever the
    // bundle recorded; the project-load backfill handles legacy
    // videos.
    let mut updated: Vec<vance_timeline::MediaItemId> = Vec::new();
    for (id, m) in project.media.iter() {
        if m.kind == vance_timeline::MediaKind::Audio && !m.has_audio_track {
            updated.push(*id);
        }
    }
    for id in updated {
        if let Some(arc) = project.media.get(&id).cloned() {
            let mut m = (*arc).clone();
            m.has_audio_track = true;
            project.media.insert(id, std::sync::Arc::new(m));
        }
    }
    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use vance_core::{FrameRate, RationalTime};
    use vance_timeline::{
        Clip, EditLog, EditOp, MediaItem, MediaKind, Project, Sequence, TrackId, TrackKind,
    };

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
        log.apply(EditOp::InsertClip { track_id, clip }).unwrap();

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
    fn round_trip_preserves_keyframed_and_expr_params() {
        // Regression: a clip carrying a KEYFRAMED effect param (motion
        // tracking / colour curves) plus an EXPRESSION param must survive
        // save → open. The untagged EffectValue used to drop keyframed
        // params on RON reload (unopenable project).
        use vance_timeline::{Effect, EffectValue, Keyframe, KeyframeInterp};
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("Anim.vance");

        let mut p = Project::new("Anim");
        let media = MediaItem::new(
            PathBuf::from("/tmp/c.mov"),
            "c.mov",
            MediaKind::Video,
            t(30),
        )
        .with_frame_rate(FrameRate::FPS_24);
        let media_id = media.id;
        p.media.insert(media_id, Arc::new(media));

        let mut log = EditLog::new(Sequence::new("S", FrameRate::FPS_24));
        let track_id = TrackId::new();
        log.apply(EditOp::AddTrack {
            track_id,
            kind: TrackKind::Video,
            name: "V1".into(),
        })
        .unwrap();
        let clip = Clip::new(media_id, "c.mov", t(0), t(0), t(5)).unwrap();
        let clip_id = clip.id;
        log.apply(EditOp::InsertClip { track_id, clip }).unwrap();

        let mut effect = Effect::new("transform");
        effect.params.insert(
            "opacity".into(),
            EffectValue::Keyframes {
                keyframes: vec![
                    Keyframe {
                        time_seconds: 0.0,
                        value: 0.0,
                        interp: KeyframeInterp::Linear,
                    },
                    Keyframe {
                        time_seconds: 1.0,
                        value: 1.0,
                        interp: KeyframeInterp::Bezier,
                    },
                ],
            },
        );
        effect.params.insert(
            "scale".into(),
            EffectValue::Expr {
                expr: "100.0 + 5.0 * noise(time)".into(),
            },
        );
        log.apply(EditOp::AddEffectToClip {
            track_id,
            clip_id,
            effect,
        })
        .unwrap();
        p.sequences.push(Arc::new(log.current().clone()));

        save(&p, &bundle).unwrap();
        let reopened = open(&bundle).unwrap();
        assert_eq!(
            reopened, p,
            "project with keyframed + expression params must round-trip"
        );
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

    // ---- Phase 7.3 mixer persistence ----------------------------------

    fn build_project_with_audio_mixer_state() -> Project {
        let mut p = Project::new("Mixer Test");
        let media = MediaItem::new(
            PathBuf::from("/tmp/clip.wav"),
            "clip.wav",
            MediaKind::Audio,
            t(10),
        );
        let media_id = media.id;
        p.media.insert(media_id, Arc::new(media));

        let mut log = EditLog::new(Sequence::new("Seq 01", FrameRate::FPS_24));
        let track_id = TrackId::new();
        log.apply(EditOp::AddTrack {
            track_id,
            kind: TrackKind::Audio,
            name: "A1".into(),
        })
        .unwrap();
        let clip = Clip::new(media_id, "clip.wav", t(0), t(0), t(5)).unwrap();
        let clip_id = clip.id;
        log.apply(EditOp::InsertClip { track_id, clip }).unwrap();

        // Set non-default mixer state on both the track AND the clip.
        log.apply(EditOp::SetTrackMute {
            track_id,
            mute: true,
        })
        .unwrap();
        log.apply(EditOp::SetTrackSolo {
            track_id,
            solo: true,
        })
        .unwrap();
        log.apply(EditOp::SetTrackGainDb {
            track_id,
            gain_db: -3.0,
        })
        .unwrap();
        log.apply(EditOp::SetClipGainDb {
            track_id,
            clip_id,
            gain_db: -6.0,
        })
        .unwrap();

        p.sequences.push(Arc::new(log.current().clone()));
        p
    }

    #[test]
    fn mixer_state_round_trips_through_save_and_open() {
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("Mixer.vance");
        let project = build_project_with_audio_mixer_state();
        save(&project, &bundle).unwrap();
        let reopened = open(&bundle).unwrap();
        let seq = &reopened.sequences[0];
        let track = &seq.tracks[0];
        assert!(track.mute, "mute survives round-trip");
        assert!(track.solo, "solo survives round-trip");
        assert!(
            (track.gain_db - (-3.0)).abs() < 1e-6,
            "track gain survives round-trip"
        );
        let clip = &track.clips[0];
        assert!(
            (clip.gain_db - (-6.0)).abs() < 1e-6,
            "clip gain survives round-trip"
        );
    }

    #[test]
    fn bus_inserts_and_sends_round_trip_through_save_and_open() {
        // GOAT-4 — a project with bus inserts + sends must reopen intact.
        // (Buses persist via RON; the EffectValue bug taught us to lock the
        // serialization of every new audio-graph field at the bundle level.)
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("Buses.vance");

        let mut p = Project::new("Buses");
        let mut seq = Sequence::new("Seq 01", FrameRate::FPS_24);
        let music = vance_timeline::BusId::new();
        let reverb = vance_timeline::BusId::new();
        let mut mbus = vance_timeline::AudioBus::new("Music");
        mbus.id = music;
        mbus.gain_db = -4.0;
        mbus.pan = -0.5;
        mbus.inserts
            .push(vance_timeline::AudioEffectSpec::default_compressor());
        mbus.inserts
            .push(vance_timeline::AudioEffectSpec::default_limiter());
        mbus.sends.push(vance_timeline::Send {
            target: reverb,
            level_db: -6.0,
            pre_fader: true,
        });
        let mut rbus = vance_timeline::AudioBus::new("Reverb");
        rbus.id = reverb;
        seq.buses.push(mbus);
        seq.buses.push(rbus);
        p.sequences.push(Arc::new(seq));

        save(&p, &bundle).unwrap();
        let reopened = open(&bundle).unwrap();
        let seq = &reopened.sequences[0];
        assert_eq!(seq.buses.len(), 2, "both buses survive");
        let m = seq.buses.iter().find(|b| b.id == music).unwrap();
        assert!((m.gain_db - (-4.0)).abs() < 1e-6);
        assert!((m.pan - (-0.5)).abs() < 1e-6);
        assert_eq!(m.inserts.len(), 2, "bus inserts survive RON round-trip");
        assert!(matches!(
            m.inserts[0],
            vance_timeline::AudioEffectSpec::Compressor { .. }
        ));
        assert!(matches!(
            m.inserts[1],
            vance_timeline::AudioEffectSpec::Limiter { .. }
        ));
        assert_eq!(m.sends.len(), 1, "bus send survives RON round-trip");
        assert_eq!(m.sends[0].target, reverb);
        assert!(m.sends[0].pre_fader);
        assert!((m.sends[0].level_db - (-6.0)).abs() < 1e-6);
    }

    #[test]
    fn legacy_bundle_loads_with_unity_mixer_defaults() {
        // Build a project using ONLY the pre-7.3 surface (no SetTrack*
        // / SetClip* ops, no field literals on the new fields). After
        // a save/open round-trip the new fields take their
        // serde-default values: mute=false, solo=false, gain_db=0.0,
        // and Audio-kind media items get has_audio_track forced true
        // by the bundle-load sweep.
        let tmp = TempDir::new().unwrap();
        let bundle = tmp.path().join("Legacy.vance");
        let mut p = Project::new("Legacy");
        let media = MediaItem::new(
            PathBuf::from("/tmp/legacy.wav"),
            "legacy.wav",
            MediaKind::Audio,
            t(10),
        );
        // Simulate legacy bundle: the deserialize path normally sets
        // has_audio_track=false for Audio items missing the field; we
        // emulate by zeroing it here so the load sweep's job is
        // verifiable.
        let mut media_inner = (*Arc::new(media)).clone();
        media_inner.has_audio_track = false;
        let media_id = media_inner.id;
        p.media.insert(media_id, Arc::new(media_inner));

        let mut log = EditLog::new(Sequence::new("Seq 01", FrameRate::FPS_24));
        let track_id = TrackId::new();
        log.apply(EditOp::AddTrack {
            track_id,
            kind: TrackKind::Audio,
            name: "A1".into(),
        })
        .unwrap();
        let clip = Clip::new(media_id, "legacy", t(0), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip { track_id, clip }).unwrap();
        p.sequences.push(Arc::new(log.current().clone()));

        save(&p, &bundle).unwrap();
        let reopened = open(&bundle).unwrap();
        let seq = &reopened.sequences[0];
        let track = &seq.tracks[0];
        assert!(!track.mute);
        assert!(!track.solo);
        assert_eq!(track.gain_db, 0.0);
        assert_eq!(track.clips[0].gain_db, 0.0);
        // Bundle-load sweep forces has_audio_track=true on Audio media.
        let media = reopened.media.get(&media_id).unwrap();
        assert!(
            media.has_audio_track,
            "Audio-kind item's has_audio_track forced true by load sweep"
        );
    }
}
