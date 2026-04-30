//! A `Track` is an ordered list of clips of a single kind (video or
//! audio). Tracks compose in render order: video tracks blend
//! top-to-bottom, audio tracks sum.
//!
//! Clips on a track are stored as `Arc<Clip>` so edit operations that
//! "rewrite" a track can share the unaffected clips with the original.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use vance_core::RationalTime;

use crate::clip::Clip;
use crate::id::TrackId;

/// What kind of media this track holds. Drives compositing rules: video
/// tracks blend top-to-bottom; audio tracks sum.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum TrackKind {
    /// Video / image clips. Blended.
    Video,
    /// Audio clips. Summed.
    Audio,
}

/// One track on a sequence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// Stable ID; survives save/load.
    pub id: TrackId,
    /// What kind of clips this track accepts.
    pub kind: TrackKind,
    /// Display name. Convention: V1, V2, ... and A1, A2, ...
    pub name: String,
    /// When `false`, the track contributes nothing to the render but
    /// stays in the project. Cheap on/off.
    pub enabled: bool,
    /// When `true`, edit operations refuse to modify this track until
    /// it's unlocked.
    pub locked: bool,
    /// Clips on this track, in timeline order. Edit operations are
    /// expected to maintain this invariant; we don't enforce it on
    /// construction so callers can build freely from disk.
    pub clips: Vec<Arc<Clip>>,
}

impl Track {
    /// New track with a fresh ID, no clips, enabled, unlocked.
    pub fn new(kind: TrackKind, name: impl Into<String>) -> Self {
        Self {
            id: TrackId::new(),
            kind,
            name: name.into(),
            enabled: true,
            locked: false,
            clips: Vec::new(),
        }
    }

    /// Largest `clip.timeline_out()` on this track, or `RationalTime::ZERO`
    /// if empty. Used by `Sequence::duration()`.
    pub fn duration(&self) -> RationalTime {
        self.clips
            .iter()
            .map(|c| c.timeline_out())
            .max()
            .unwrap_or(RationalTime::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Clip;
    use crate::id::MediaItemId;

    fn t(num: i64) -> RationalTime {
        RationalTime::new(num, 1).unwrap()
    }

    #[test]
    fn empty_track_has_zero_duration() {
        let tr = Track::new(TrackKind::Video, "V1");
        assert_eq!(tr.duration(), RationalTime::ZERO);
    }

    #[test]
    fn track_duration_is_max_timeline_out() {
        let media = MediaItemId::new();
        let c1 = Arc::new(Clip::new(media, "a", t(0), t(0), t(3)).unwrap());
        let c2 = Arc::new(Clip::new(media, "b", t(5), t(0), t(2)).unwrap());
        let tr = Track {
            id: TrackId::new(),
            kind: TrackKind::Video,
            name: "V1".into(),
            enabled: true,
            locked: false,
            clips: vec![c1, c2],
        };
        assert_eq!(tr.duration(), t(7));
    }
}
