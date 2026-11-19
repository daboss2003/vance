//! Phase 3 exit gate.
//!
//! From PLAN.md §7 / Phase 3:
//!
//! > **Exit gate:** Programmatically construct a 5-clip sequence, save
//! > it, reopen it, prove equality.
//!
//! This test is the literal demonstration of that gate. It exercises
//! every Phase-3 surface together — `Project`, `Sequence`, `Track`,
//! `Clip`, `MediaItem`, `EditLog`, `EditOp`, the bundle layout, RON
//! serialization, and the migration framework's no-op pass-through —
//! in a single round trip.

use std::path::PathBuf;
use std::sync::Arc;
use tempfile::TempDir;

use vance_core::{FrameRate, RationalTime};
use vance_project::{open, save};
use vance_timeline::{
    Clip, EditLog, EditOp, MediaItem, MediaKind, Project, Sequence, TrackId, TrackKind,
};

fn t(num: i64) -> RationalTime {
    RationalTime::new(num, 1).unwrap()
}

#[test]
fn phase_3_exit_gate_round_trip_five_clip_sequence() {
    // 1. Build a project with one media item and a sequence containing
    //    five non-overlapping clips on a single video track.
    let mut project = Project::new("Exit Gate Demo");

    let media = MediaItem::new(
        PathBuf::from("/tmp/source.mov"),
        "source.mov",
        MediaKind::Video,
        t(60),
    )
    .with_frame_rate(FrameRate::FPS_24);
    let media_id = media.id;
    project.media.insert(media_id, Arc::new(media));

    let mut log = EditLog::new(Sequence::new("Seq 01", FrameRate::FPS_24));
    let track_id = TrackId::new();
    log.apply(EditOp::AddTrack {
        track_id,
        kind: TrackKind::Video,
        name: "V1".into(),
    })
    .unwrap();

    for i in 0..5i64 {
        // Lay clips end-to-end at 5-second intervals, sourcing from
        // the same media: timeline [i*5, i*5+5), source [0, 5).
        let clip = Clip::new(media_id, format!("clip {}", i + 1), t(i * 5), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip { track_id, clip }).unwrap();
    }

    project.sequences.push(Arc::new(log.current().clone()));
    assert_eq!(project.sequences[0].clip_count(), 5);
    assert_eq!(project.sequences[0].duration(), t(25));

    // 2. Save the project to a fresh `.vance/` bundle.
    let tmp = TempDir::new().unwrap();
    let bundle = tmp.path().join("ExitGate.vance");
    save(&project, &bundle).expect("save");

    assert!(bundle.is_dir());
    assert!(bundle.join("project.ron").is_file());
    assert!(bundle.join("media").is_dir());
    assert!(bundle.join("cache").is_dir());
    assert!(bundle.join("autosave").is_dir());

    // 3. Reopen the bundle. The migration framework runs (no-op for
    //    the current FORMAT_VERSION) and the project deserializes back.
    let reopened = open(&bundle).expect("open");

    // 4. Prove equality, top to bottom.
    assert_eq!(
        reopened, project,
        "saved/reopened project must equal the in-memory original"
    );

    // Spot-check the parts that matter most for the Phase-4 UI work
    // that builds on this:
    let seq = &reopened.sequences[0];
    assert_eq!(seq.tracks.len(), 1);
    assert_eq!(seq.tracks[0].kind, TrackKind::Video);
    assert_eq!(seq.tracks[0].clips.len(), 5);
    for (i, clip) in seq.tracks[0].clips.iter().enumerate() {
        assert_eq!(clip.timeline_in, t(i as i64 * 5));
        assert_eq!(clip.duration(), t(5));
        assert_eq!(clip.media, media_id);
    }
}
