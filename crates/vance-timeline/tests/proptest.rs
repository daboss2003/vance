//! Integration-level property tests for the timeline data model.
//!
//! Lives in `tests/` rather than `src/` so we can pull in `proptest`
//! as a dev-dep without it leaking into release builds.
//!
//! Properties exercised:
//! - Track invariants (sorted-by-timeline-in, no overlaps) hold after
//!   any successful sequence of inserts and removes.
//! - `insert_clip` and `remove_clip` invert each other on a fresh
//!   sequence.
//! - `EditLog::apply` × N then `undo` × N restores the base sequence.
//! - `apply` then `undo` then `redo` matches a single `apply`.

use proptest::prelude::*;
use std::sync::Arc;
use vance_core::{FrameRate, RationalTime};
use vance_timeline::{
    ops, Clip, ClipId, EditLog, EditOp, MediaItemId, Sequence, Track, TrackId, TrackKind,
};

// ---- helpers ---------------------------------------------------------------

fn t(num: i64) -> RationalTime {
    RationalTime::new(num, 1).unwrap()
}

/// One clip with `timeline_in = start`, `duration = duration`. Source
/// range [0, duration).
fn make_clip(media: MediaItemId, start: i64, duration: i64) -> Clip {
    Clip::new(media, "c", t(start), t(0), t(duration)).unwrap()
}

/// Track invariant: clips are sorted by timeline_in and pairwise
/// non-overlapping.
fn track_well_formed(track: &Track) -> bool {
    for w in track.clips.windows(2) {
        let a = &w[0];
        let b = &w[1];
        if a.timeline_in > b.timeline_in {
            return false;
        }
        if a.timeline_out() > b.timeline_in {
            return false;
        }
    }
    true
}

fn seed_seq() -> (Sequence, TrackId, MediaItemId) {
    let seq = Sequence::new("Seq", FrameRate::FPS_24);
    let seq = ops::add_track(&seq, TrackKind::Video, "V1");
    let track_id = seq.tracks[0].id;
    (seq, track_id, MediaItemId::new())
}

// ---- generators ------------------------------------------------------------

/// Vector of (start, duration) pairs that don't overlap on a single track.
/// Both numbers are nonzero positive; durations are bounded so RON output
/// stays small in shrunken cases.
fn non_overlapping_clips(max: usize) -> impl Strategy<Value = Vec<(i64, i64)>> {
    prop::collection::vec((1i64..50, 1i64..20), 0..=max).prop_map(|raw| {
        // Lay out clips end-to-end to guarantee no overlap regardless of
        // shrink choices. (start, duration) → cumulative start.
        let mut out = Vec::with_capacity(raw.len());
        let mut cursor = 0i64;
        for (gap, dur) in raw {
            cursor += gap;
            out.push((cursor, dur));
            cursor += dur;
        }
        out
    })
}

// ---- properties ------------------------------------------------------------

proptest! {
    /// Inserting any non-overlapping sequence of clips produces a
    /// well-formed track.
    #[test]
    fn inserts_keep_track_well_formed(spec in non_overlapping_clips(40)) {
        let (mut seq, tid, media) = seed_seq();
        for (start, dur) in spec {
            let c = make_clip(media, start, dur);
            seq = ops::insert_clip(&seq, tid, c).expect("non-overlapping by construction");
        }
        prop_assert!(track_well_formed(&seq.tracks[0]));
    }

    /// `insert_clip` then `remove_clip` returns to the original
    /// per-clip count and leaves the track empty.
    #[test]
    fn insert_remove_inverts(spec in non_overlapping_clips(20)) {
        let (mut seq, tid, media) = seed_seq();
        let mut clip_ids: Vec<ClipId> = Vec::new();
        for (start, dur) in spec {
            let c = make_clip(media, start, dur);
            clip_ids.push(c.id);
            seq = ops::insert_clip(&seq, tid, c).unwrap();
        }
        for id in clip_ids {
            seq = ops::remove_clip(&seq, tid, id).unwrap();
        }
        prop_assert_eq!(seq.tracks[0].clips.len(), 0);
    }

    /// Apply a chain of inserts through `EditLog`, then undo the chain
    /// — the resulting sequence equals the base.
    #[test]
    fn applies_undo_to_base(spec in non_overlapping_clips(15)) {
        let (base, tid, media) = seed_seq();
        let mut log = EditLog::new(base.clone());
        for (start, dur) in &spec {
            let c = make_clip(media, *start, *dur);
            log.apply(EditOp::InsertClip { track_id: tid, clip: c }).unwrap();
        }
        for _ in &spec {
            log.undo();
        }
        // Pointer-equal Arc<Track> on the only track means we restored the
        // exact pre-edit Arc, not a deep clone.
        prop_assert!(Arc::ptr_eq(&log.current().tracks[0], &base.tracks[0]));
    }

    /// `apply` then `undo` then `redo` lands on the same sequence as a
    /// single `apply`.
    #[test]
    fn apply_undo_redo_matches_apply(spec in non_overlapping_clips(8)) {
        let (base, tid, media) = seed_seq();
        let mut log_a = EditLog::new(base.clone());
        let mut log_b = EditLog::new(base.clone());
        for (start, dur) in &spec {
            let c = make_clip(media, *start, *dur);
            let op = EditOp::InsertClip { track_id: tid, clip: c.clone() };
            log_a.apply(op.clone()).unwrap();
            log_b.apply(op).unwrap();
        }
        // Round-trip log_b through undo/redo for every op.
        for _ in &spec { log_b.undo(); }
        for _ in &spec { log_b.redo(); }
        prop_assert_eq!(log_a.current(), log_b.current());
    }
}
