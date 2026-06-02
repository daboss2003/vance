//! A `Sequence` is the editable timeline. It owns ordered tracks plus
//! sequence-level markers, and carries its own frame rate (which need
//! not match any source media's rate).

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use vance_core::{FrameRate, RationalTime};

use crate::id::SequenceId;
use crate::marker::Marker;
use crate::track::Track;

/// A timeline.
///
/// `tracks` holds video tracks first, then audio tracks. The order
/// within each kind matters for compositing (video) and channel
/// assignment (audio). `Arc<[Arc<Track>]>` lets edit operations share
/// untouched tracks across versions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    /// Stable ID.
    pub id: SequenceId,
    /// Display name (e.g. "Sequence 01").
    pub name: String,
    /// The sequence's master frame rate. Source media at other rates is
    /// resampled at render time.
    pub frame_rate: FrameRate,
    /// Tracks, in render order. Convention: video tracks first (V1, V2,
    /// ...), then audio (A1, A2, ...). Edit operations preserve this
    /// grouping.
    pub tracks: Vec<Arc<Track>>,
    /// Sequence-level markers (chapters, notes, range brackets).
    pub markers: Vec<Marker>,
}

impl Sequence {
    /// New empty sequence at the given frame rate.
    pub fn new(name: impl Into<String>, frame_rate: FrameRate) -> Self {
        Self {
            id: SequenceId::new(),
            name: name.into(),
            frame_rate,
            tracks: Vec::new(),
            markers: Vec::new(),
        }
    }

    /// Total duration of the sequence — the largest `track.duration()`
    /// over all tracks, or `ZERO` if empty.
    pub fn duration(&self) -> RationalTime {
        self.tracks
            .iter()
            .map(|t| t.duration())
            .max()
            .unwrap_or(RationalTime::ZERO)
    }

    /// Total clip count across all tracks. O(tracks).
    pub fn clip_count(&self) -> usize {
        self.tracks.iter().map(|t| t.clips.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Clip;
    use crate::id::{MediaItemId, TrackId};
    use crate::track::{Track, TrackKind};

    fn t(num: i64) -> RationalTime {
        RationalTime::new(num, 1).unwrap()
    }

    #[test]
    fn empty_sequence_zero_duration() {
        let s = Sequence::new("Seq 01", FrameRate::FPS_24);
        assert_eq!(s.duration(), RationalTime::ZERO);
        assert_eq!(s.clip_count(), 0);
    }

    #[test]
    fn duration_is_longest_track() {
        let media = MediaItemId::new();
        let c_short = Arc::new(Clip::new(media, "s", t(0), t(0), t(3)).unwrap());
        let c_long = Arc::new(Clip::new(media, "l", t(0), t(0), t(7)).unwrap());
        let v1 = Arc::new(Track {
            id: TrackId::new(),
            kind: TrackKind::Video,
            name: "V1".into(),
            enabled: true,
            locked: false,
            clips: vec![c_short],
            transitions: Vec::new(),
            mute: false,
            solo: false,
            gain_db: 0.0,
        });
        let v2 = Arc::new(Track {
            id: TrackId::new(),
            kind: TrackKind::Video,
            name: "V2".into(),
            enabled: true,
            locked: false,
            clips: vec![c_long],
            transitions: Vec::new(),
            mute: false,
            solo: false,
            gain_db: 0.0,
        });
        let seq = Sequence {
            id: SequenceId::new(),
            name: "Seq".into(),
            frame_rate: FrameRate::FPS_24,
            tracks: vec![v1, v2],
            markers: Vec::new(),
        };
        assert_eq!(seq.duration(), t(7));
        assert_eq!(seq.clip_count(), 2);
    }
}
