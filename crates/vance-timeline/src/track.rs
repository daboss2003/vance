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
use crate::transition::Transition;

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
    /// Display name. `V<n>` on a video track / `A<n>` on an audio track is
    /// an automatic name that follows the track's position (renumbered by
    /// the track ops); anything else is a custom name, kept as-is.
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
    /// Transitions anchored at cut points on this track. Each
    /// transition's `position` is the cut between two adjacent
    /// clips; the transition's window extends `duration / 2` on each
    /// side. Stored as `Arc` so edit ops can share unaffected
    /// transitions across `Track` rewrites.
    #[serde(default)]
    pub transitions: Vec<Arc<Transition>>,
    /// Mixer mute (Phase 7.3). Distinct from `enabled` — `enabled`
    /// is the project-level on/off (track disappears from the render
    /// graph entirely); `mute` is the mix-bus state that silences
    /// audible output without removing the track from layout. For
    /// audio tracks: contributes audio iff `enabled && !mute && (no
    /// other track has solo || self.solo)`. For video tracks: kept
    /// for symmetry but ignored by the render pipeline.
    #[serde(default)]
    pub mute: bool,
    /// Solo (Phase 7.3). If any track in the sequence has `solo`,
    /// only soloed tracks contribute to the mix. Symmetric with
    /// `mute`: defined on both kinds, used by the audio mixer.
    #[serde(default)]
    pub solo: bool,
    /// Mixer gain in decibels (Phase 7.3). 0.0 = unity. Audio tracks
    /// apply this as `linear = 10^(gain_db / 20)` immediately before
    /// summing into the master bus. Video tracks ignore it. When
    /// `gain_keyframes` is non-empty, this static value is IGNORED in
    /// favor of the animation lane.
    #[serde(default)]
    pub gain_db: f32,
    /// Phase 7.4: time-varying gain automation lane. Empty = use the
    /// static `gain_db` (the "no automation" path). Non-empty =
    /// evaluate at the playhead's timeline-seconds. `Keyframe.value`
    /// is in dB; the mixer precomputes `db_to_linear` at snapshot
    /// build time so the audio fold thread never touches `powf`.
    /// `#[serde(default)]` so pre-7.4 bundles deserialize empty.
    ///
    /// IMPORTANT: empty-lane semantics in the mixer are
    /// "no automation, use static gain", NOT the
    /// `eval_keyframes(&[]) = 0.0` footgun. The snapshot-build path
    /// must check `is_empty()` before invoking the evaluator.
    #[serde(default)]
    pub gain_keyframes: Vec<crate::effect::Keyframe>,
    /// Phase 7.6: stereo pan position, range `[-1, +1]` (−1 = full
    /// left, 0 = center, +1 = full right). Audio tracks only —
    /// video tracks ignore it. The mixer applies an equal-power pan
    /// law (`L = cos(θ)`, `R = sin(θ)` where `θ = (pan+1) · π/4`) so
    /// the total power across the stereo field stays constant as
    /// the user sweeps the knob. `#[serde(default)]` so pre-7.6
    /// bundles deserialize as 0.0 (center).
    #[serde(default)]
    pub pan: f32,
    /// Phase 7.7: ordered insert chain of built-in audio effects
    /// (parametric EQ + compressor + limiter). The mixer applies
    /// these per-track AFTER clip composition (gain + pan + fade +
    /// keyframe lane) and BEFORE master gain — the conventional
    /// channel-strip topology. Empty (the default) = pass-through.
    /// `#[serde(default)]` so pre-7.7 bundles deserialize empty.
    #[serde(default)]
    pub audio_effects: Vec<crate::audio_effect::AudioEffectSpec>,
    /// GOAT-4: where this track's main output routes — the master (default)
    /// or a submix bus. `#[serde(default)]` → `Master` for older bundles.
    #[serde(default)]
    pub output: crate::bus::BusOutput,
    /// GOAT-4: aux/reverb sends from this track to buses.
    #[serde(default)]
    pub sends: Vec<crate::bus::Send>,
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
            transitions: Vec::new(),
            mute: false,
            solo: false,
            gain_db: 0.0,
            gain_keyframes: Vec::new(),
            pan: 0.0,
            audio_effects: Vec::new(),
            output: crate::bus::BusOutput::Master,
            sends: Vec::new(),
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
            transitions: Vec::new(),
            mute: false,
            solo: false,
            gain_db: 0.0,
            gain_keyframes: Vec::new(),
            pan: 0.0,
            audio_effects: Vec::new(),
            output: crate::bus::BusOutput::Master,
            sends: Vec::new(),
        };
        assert_eq!(tr.duration(), t(7));
    }
}
