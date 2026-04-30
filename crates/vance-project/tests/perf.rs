//! Phase 3.6 perf gates.
//!
//! These tests are `#[ignore]`-d by default because they generate a
//! 100k-clip project on disk, which is slow to bother with on every
//! `cargo test`. Run them on demand with:
//!
//! ```sh
//! cargo test -p vance-project --test perf --release -- --ignored --nocapture
//! ```
//!
//! In debug mode the assertions are skipped (we still print timings)
//! because `[profile.dev]` doesn't optimize our own code; the exit-gate
//! numbers in PLAN.md (100k clips load <2s, undo/redo <1ms) refer to
//! release builds.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tempfile::TempDir;

use vance_core::{FrameRate, RationalTime};
use vance_project::{open, save};
use vance_timeline::{
    ops, Clip, EditLog, EditOp, MediaItem, MediaItemId, MediaKind, Project, Sequence, TrackId,
    TrackKind,
};

const N_TRACKS: usize = 100;
const CLIPS_PER_TRACK: usize = 1_000;
const TOTAL_CLIPS: usize = N_TRACKS * CLIPS_PER_TRACK;

fn t(num: i64) -> RationalTime {
    RationalTime::new(num, 1).unwrap()
}

fn build_huge_project() -> Project {
    let mut p = Project::new("Huge");

    // One media item; clips share it. Avoids generating 100k unique paths.
    let media = MediaItem::new(
        PathBuf::from("/dev/null/source.mov"),
        "source",
        MediaKind::Video,
        t(1_000_000),
    );
    let media_id = media.id;
    p.media.insert(media_id, Arc::new(media));

    // Build 100 tracks × 1000 clips directly via the typed structs;
    // bypassing EditLog here is fine — this is timeline construction,
    // not user-driven editing.
    let mut seq = Sequence::new("Big Seq", FrameRate::FPS_24);
    for ti in 0..N_TRACKS {
        seq = ops::add_track(&seq, TrackKind::Video, format!("V{}", ti + 1));
    }
    // Layout each track end-to-end: clip i on every track is at
    // [i*5, i*5 + 4); guarantees no overlap on any single track.
    for tk in 0..N_TRACKS {
        let track_id = seq.tracks[tk].id;
        for i in 0..CLIPS_PER_TRACK {
            let c = Clip::new(
                media_id,
                "c",
                t(i as i64 * 5),
                t(0),
                t(4),
            )
            .unwrap();
            seq = ops::insert_clip(&seq, track_id, c).unwrap();
        }
    }

    assert_eq!(
        seq.clip_count(),
        TOTAL_CLIPS,
        "assembled the right shape"
    );
    p.sequences.push(Arc::new(seq));
    p
}

#[test]
#[ignore]
fn load_100k_clip_project_under_two_seconds() {
    let tmp = TempDir::new().unwrap();
    let bundle = tmp.path().join("Huge.vance");

    let project = build_huge_project();
    save(&project, &bundle).expect("save");

    let start = Instant::now();
    let reopened = open(&bundle).expect("open");
    let elapsed = start.elapsed();
    assert_eq!(reopened.sequences[0].clip_count(), TOTAL_CLIPS);

    eprintln!("open() of {TOTAL_CLIPS} clips: {elapsed:?}");
    if !cfg!(debug_assertions) {
        assert!(
            elapsed.as_secs_f64() < 2.0,
            "open() took {elapsed:?}, target <2s"
        );
    }
}

#[test]
#[ignore]
fn undo_redo_under_one_ms() {
    // Keep the log work small but the underlying sequence large. We
    // care about per-op latency for undo/redo, not throughput, and the
    // bottleneck is cloning the outer Vec<Arc<Track>> — so seed the
    // sequence with `N_TRACKS` tracks, then time a single op cycle.
    let mut seq = Sequence::new("S", FrameRate::FPS_24);
    for ti in 0..N_TRACKS {
        seq = ops::add_track(&seq, TrackKind::Video, format!("V{}", ti + 1));
    }
    let track_id = seq.tracks[0].id;
    let media = MediaItemId::new();

    let mut log = EditLog::new(seq);
    let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
    log.apply(EditOp::InsertClip {
        track_id,
        clip,
    })
    .unwrap();

    // Time a few hundred undo/redo cycles to average out OS scheduler
    // jitter, then check the per-op average.
    const ITERS: u32 = 1000;
    let start = Instant::now();
    for _ in 0..ITERS {
        log.undo();
        log.redo();
    }
    let total = start.elapsed();
    let per_op = total / (ITERS * 2);
    eprintln!(
        "{N_TRACKS} tracks, {ITERS} undo/redo round-trips ({} ops), per-op {per_op:?}",
        ITERS * 2
    );
    if !cfg!(debug_assertions) {
        assert!(
            per_op.as_micros() < 1_000,
            "undo/redo per-op {per_op:?}, target <1ms"
        );
    }
    // Sanity: the log shouldn't have grown.
    assert!(log.can_undo());
}

/// Basic integrity check on the huge-project builder so a regression in
/// `insert_clip` or `add_track` shows up cleanly here too. Runs by
/// default (no `#[ignore]`) but uses a much smaller shape.
#[test]
fn small_huge_project_builder_integrity() {
    let mut p = Project::new("small");
    let media = MediaItem::new(
        PathBuf::from("/tmp/x.mov"),
        "x",
        MediaKind::Video,
        t(1000),
    );
    let media_id = media.id;
    p.media.insert(media_id, Arc::new(media));

    let mut seq = Sequence::new("S", FrameRate::FPS_24);
    for _ in 0..3 {
        seq = ops::add_track(&seq, TrackKind::Video, "V");
    }
    for tk in 0..3 {
        let tid: TrackId = seq.tracks[tk].id;
        for i in 0..5 {
            let c = Clip::new(media_id, "c", t(i * 5), t(0), t(4)).unwrap();
            seq = ops::insert_clip(&seq, tid, c).unwrap();
        }
    }
    assert_eq!(seq.clip_count(), 15);
    p.sequences.push(Arc::new(seq));
}
