//! A `Clip` places a portion of a `MediaItem` onto a `Track` at some
//! position on the sequence timeline.
//!
//! Three time spaces meet on a clip:
//! - **Source time**: position within the underlying media file.
//!   `source_in` and `source_out` bound the visible range.
//! - **Timeline time**: position on the sequence's master timeline.
//!   `timeline_in` is where the clip's `source_in` lands.
//! - **Duration**: derived as `source_out - source_in`. We don't store
//!   it because that would invite the source range and the duration to
//!   disagree.

use serde::{Deserialize, Serialize};
use vance_core::RationalTime;

use crate::effect::Effect;
use crate::error::{Result, TimelineError};
use crate::id::{ClipId, MediaItemId};

/// One clip on a track.
///
/// Effect lists are usually short, so a plain `Vec` is the right shape.
/// The clip itself is held inside a `Track` as `Arc<Clip>`, which is
/// what gives us the structural sharing across edit operations.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    /// Stable ID; survives save/load and undo/redo.
    pub id: ClipId,
    /// Reference into `Project.media` library.
    pub media: MediaItemId,
    /// Display name. Defaults to the media item's display name on
    /// initial drop, but the user can rename per-clip.
    pub name: String,
    /// Where this clip's `source_in` lands on the sequence timeline.
    pub timeline_in: RationalTime,
    /// First visible time within the source media (inclusive).
    pub source_in: RationalTime,
    /// One past the last visible time within the source media (exclusive).
    /// Required to be `>= source_in`; equality means a zero-length clip,
    /// which the editor permits as a transient state during trims.
    pub source_out: RationalTime,
    /// When `false`, the clip is muted/skipped at render time but stays
    /// in the project so a single click un-mutes it. For audio clips
    /// this is also the per-clip mute used by the mixer (Phase 7.3).
    pub enabled: bool,
    /// Effects applied to this clip. Empty by default.
    pub effects: Vec<Effect>,
    /// Per-clip mixer gain in decibels (Phase 7.3). 0.0 = unity.
    /// Audio clips: `clip_linear = 10^(gain_db / 20)`. Composed
    /// multiplicatively with the parent track's gain at mix time.
    /// Video clips: ignored. When `gain_keyframes` is non-empty,
    /// this static value is IGNORED in favor of the animation lane.
    #[serde(default)]
    pub gain_db: f32,
    /// Phase 7.4: time-varying gain automation lane. Keyframes are
    /// CLIP-RELATIVE seconds (`t = 0` is the clip's `timeline_in`,
    /// not the sequence origin). Empty = use the static `gain_db`.
    /// `Keyframe.value` is in dB. See the matching note on
    /// [`Track.gain_keyframes`] about the
    /// `eval_keyframes(&[]) = 0.0` footgun — the snapshot-build path
    /// must check `is_empty()` before invoking the evaluator.
    #[serde(default)]
    pub gain_keyframes: Vec<crate::effect::Keyframe>,
    /// Phase 7.4 M3: optional fade-in. Composed multiplicatively
    /// with `gain_keyframes` and `gain_db` at mix time — fades do NOT
    /// clobber the automation lane. `None` = no fade-in.
    #[serde(default)]
    pub fade_in: Option<crate::fade::Fade>,
    /// Phase 7.4 M3: optional fade-out. Same compositional semantics
    /// as `fade_in`. The fade-out region starts at
    /// `clip_duration - fade_out.duration_seconds`.
    #[serde(default)]
    pub fade_out: Option<crate::fade::Fade>,
    /// Phase 7.6: per-clip stereo pan position, range `[-1, +1]`. See
    /// [`Track.pan`] for the equal-power law. Composes additively
    /// with the track's pan: the mixer applies both at fold time so
    /// "track pan +0.5 + clip pan -0.2 → effective pan +0.3" (clamped
    /// to `[-1, +1]` at evaluation time). Pre-7.6 bundles deserialize
    /// as 0.0 via `#[serde(default)]`.
    #[serde(default)]
    pub pan: f32,
}

impl Clip {
    /// Construct a new clip with a fresh ID. Returns an error if
    /// `source_out < source_in`.
    pub fn new(
        media: MediaItemId,
        name: impl Into<String>,
        timeline_in: RationalTime,
        source_in: RationalTime,
        source_out: RationalTime,
    ) -> Result<Self> {
        if source_out < source_in {
            return Err(TimelineError::invalid("source_out must be >= source_in"));
        }
        Ok(Self {
            id: ClipId::new(),
            media,
            name: name.into(),
            timeline_in,
            source_in,
            source_out,
            enabled: true,
            effects: Vec::new(),
            gain_db: 0.0,
            gain_keyframes: Vec::new(),
            fade_in: None,
            fade_out: None,
            pan: 0.0,
        })
    }

    /// Source-domain duration, `source_out - source_in`. Same as the
    /// timeline-domain duration in v1 (no speed changes yet).
    pub fn duration(&self) -> RationalTime {
        // RationalTime doesn't expose subtraction directly yet; we go
        // through the inner Rational, which does.
        RationalTime(self.source_out.0 - self.source_in.0)
    }

    /// Where the clip ends on the sequence timeline (exclusive).
    pub fn timeline_out(&self) -> RationalTime {
        RationalTime(self.timeline_in.0 + self.duration().0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(num: i64) -> RationalTime {
        RationalTime::new(num, 1).unwrap()
    }

    #[test]
    fn new_clip_validates_source_range() {
        let media = MediaItemId::new();
        assert!(Clip::new(media, "ok", t(0), t(0), t(5)).is_ok());
        assert!(Clip::new(media, "bad", t(0), t(5), t(0)).is_err());
    }

    #[test]
    fn duration_and_timeline_out() {
        let media = MediaItemId::new();
        let c = Clip::new(media, "x", t(10), t(2), t(7)).unwrap();
        assert_eq!(c.duration(), t(5));
        assert_eq!(c.timeline_out(), t(15));
    }
}
