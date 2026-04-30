//! Tells the playback engine what to render at each timeline position.
//!
//! This module is pure: no I/O, no decoder calls, just `Sequence` →
//! "which clip's frame should be on screen, and at what source PTS?".
//! The engine layer (`vance-app::engine`) is what actually pulls that
//! frame from the decoder; keeping the lookup pure means we can unit-
//! test sequence semantics without spinning up FFmpeg.
//!
//! Phase 4.1 scope: video only, no compositing yet — the topmost video
//! track holding a clip at the queried time wins. Audio scene plans
//! and proper layered compositing land in later 4.x milestones.

use vance_core::{Rational, RationalTime};

use crate::clip::Clip;
use crate::id::{ClipId, MediaItemId};
use crate::sequence::Sequence;
use crate::track::{Track, TrackKind};

/// What's on screen at a given timeline position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SceneAt {
    /// No video clip is active at this time on any enabled track.
    Empty,
    /// A single clip is the visible source. (No layering yet — multi-
    /// track compositing in v1 lands later in Phase 4 / Phase 5.)
    Clip(ActiveClip),
}

/// The clip currently providing the on-screen frame, plus where to read
/// from inside its source media.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ActiveClip {
    /// Which clip is active.
    pub clip_id: ClipId,
    /// The source media this clip references; the engine pulls frames
    /// from here.
    pub media: MediaItemId,
    /// Position inside the source media to read from. This is
    /// `source_in + (timeline_t - timeline_in)` (no speed changes in
    /// v1; that lives behind a future `rate` field on `Clip`).
    pub source_pts: RationalTime,
}

/// Resolve the active clip at `timeline_t` on `seq`.
///
/// "Topmost video track wins" — we walk video tracks from the *end* of
/// `seq.tracks` (highest in the UI: V2 above V1) toward the start and
/// return the first hit. Disabled tracks are skipped. Out-of-bounds
/// queries return `Empty`.
pub fn scene_at(seq: &Sequence, timeline_t: RationalTime) -> SceneAt {
    if timeline_t.0 < Rational::ZERO {
        return SceneAt::Empty;
    }
    for track in seq.tracks.iter().rev() {
        if track.kind != TrackKind::Video || !track.enabled {
            continue;
        }
        if let Some(c) = clip_at(track, timeline_t) {
            if !c.enabled {
                continue;
            }
            let into_clip = RationalTime(c.timeline_in.0).0; // borrow guard
            let _ = into_clip; // silence unused-binding lint in case of future edits
            let source_pts =
                RationalTime(c.source_in.0 + (timeline_t.0 - c.timeline_in.0));
            return SceneAt::Clip(ActiveClip {
                clip_id: c.id,
                media: c.media,
                source_pts,
            });
        }
    }
    SceneAt::Empty
}

/// First clip on `track` whose half-open `[timeline_in, timeline_out)`
/// range contains `t`.
fn clip_at(track: &Track, t: RationalTime) -> Option<&Clip> {
    track
        .clips
        .iter()
        .find(|c| c.timeline_in <= t && t < c.timeline_out())
        .map(|arc| arc.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Clip;
    use crate::ops;
    use crate::track::TrackKind;
    use std::sync::Arc;
    use vance_core::FrameRate;

    fn t(num: i64) -> RationalTime {
        RationalTime::new(num, 1).unwrap()
    }

    fn seq_with_clip(start: i64, dur: i64) -> (Sequence, MediaItemId, ClipId) {
        let media = MediaItemId::new();
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        let tid = seq.tracks[0].id;
        let clip = Clip::new(media, "c", t(start), t(2), t(2 + dur)).unwrap();
        let cid = clip.id;
        seq = ops::insert_clip(&seq, tid, clip).unwrap();
        (seq, media, cid)
    }

    #[test]
    fn empty_outside_clip_range() {
        let (seq, _media, _cid) = seq_with_clip(0, 5); // [0,5)
        assert_eq!(scene_at(&seq, t(5)), SceneAt::Empty);
        assert_eq!(scene_at(&seq, RationalTime::new(-1, 1).unwrap()), SceneAt::Empty);
    }

    #[test]
    fn finds_active_clip_with_correct_source_pts() {
        let (seq, media, cid) = seq_with_clip(0, 5); // timeline [0,5), source [2,7)
        match scene_at(&seq, t(0)) {
            SceneAt::Clip(a) => {
                assert_eq!(a.clip_id, cid);
                assert_eq!(a.media, media);
                assert_eq!(a.source_pts, t(2));
            }
            other => panic!("expected Clip, got {other:?}"),
        }
        match scene_at(&seq, t(3)) {
            SceneAt::Clip(a) => assert_eq!(a.source_pts, t(5)),
            other => panic!("expected Clip at t=3, got {other:?}"),
        }
    }

    #[test]
    fn topmost_track_wins() {
        let media = MediaItemId::new();
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        seq = ops::add_track(&seq, TrackKind::Video, "V2");
        let v1 = seq.tracks[0].id;
        let v2 = seq.tracks[1].id;
        let lower = Clip::new(media, "v1", t(0), t(0), t(10)).unwrap();
        let upper_id = ClipId::new();
        let upper = Clip {
            id: upper_id,
            ..Clip::new(media, "v2", t(0), t(20), t(30)).unwrap()
        };
        seq = ops::insert_clip(&seq, v1, lower).unwrap();
        seq = ops::insert_clip(&seq, v2, upper).unwrap();
        match scene_at(&seq, t(2)) {
            SceneAt::Clip(a) => {
                assert_eq!(a.clip_id, upper_id, "V2 should win over V1");
                assert_eq!(a.source_pts, t(22));
            }
            other => panic!("expected V2 clip, got {other:?}"),
        }
    }

    #[test]
    fn disabled_track_is_ignored() {
        let (mut seq, _media, _cid) = seq_with_clip(0, 5);
        // Disable the only track.
        let mut tr = (*seq.tracks[0]).clone();
        tr.enabled = false;
        let mut tracks = seq.tracks.clone();
        tracks[0] = Arc::new(tr);
        seq = Sequence { tracks, ..seq };
        assert_eq!(scene_at(&seq, t(2)), SceneAt::Empty);
    }

    #[test]
    fn audio_track_does_not_supply_video_scene() {
        let media = MediaItemId::new();
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Audio, "A1");
        let tid = seq.tracks[0].id;
        let clip = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        seq = ops::insert_clip(&seq, tid, clip).unwrap();
        // No video track present — scene_at should be Empty.
        assert_eq!(scene_at(&seq, t(2)), SceneAt::Empty);
    }
}
