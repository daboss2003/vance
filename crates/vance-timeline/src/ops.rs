//! Pure edit operations on a `Sequence`.
//!
//! Every operation takes `&Sequence` and returns either a brand-new
//! `Sequence` with the change applied or a `TimelineError`. The tracks
//! the operation didn't touch are shared with the input sequence via
//! `Arc`, so a 100-track sequence's edit cost is dominated by the one
//! track that changed — not by deep-cloning the world.
//!
//! ## Conventions
//!
//! - "ripple" variants close the gap left behind by shifting later
//!   clips on the same track left by the removed/changed range.
//!   Non-ripple variants leave a gap.
//! - Operations refuse to modify a locked track (`TimelineError::Locked`).
//! - Operations validate their geometry: zero-or-negative durations,
//!   timeline overlap, negative source positions all reject up-front.
//! - Slip changes the source range, leaving the timeline alone.
//!   Slide changes the timeline position, leaving the source range alone.

use std::sync::Arc;
use vance_core::RationalTime;

use crate::clip::Clip;
use crate::effect::{Effect, EffectValue};
use crate::error::{Result, TimelineError};
use crate::id::{ClipId, EffectId, TrackId, TransitionId};
use crate::sequence::Sequence;
use crate::track::{Track, TrackKind};
use crate::transition::{Transition, TransitionKind};

// ---- time helpers -----------------------------------------------------------

#[inline]
fn add(a: RationalTime, b: RationalTime) -> RationalTime {
    RationalTime(a.0 + b.0)
}

#[inline]
fn sub(a: RationalTime, b: RationalTime) -> RationalTime {
    RationalTime(a.0 - b.0)
}

// ---- track-finding helpers --------------------------------------------------

/// Find a track by ID; returns its index in `seq.tracks` and an Arc clone.
fn find_track(seq: &Sequence, track_id: TrackId) -> Result<(usize, Arc<Track>)> {
    seq.tracks
        .iter()
        .enumerate()
        .find(|(_, t)| t.id == track_id)
        .map(|(i, t)| (i, t.clone()))
        .ok_or_else(|| TimelineError::invalid(format!("track {track_id} not found")))
}

/// Find a clip on a track by ID; returns its index in `track.clips` and an Arc clone.
fn find_clip(track: &Track, clip_id: ClipId) -> Result<(usize, Arc<Clip>)> {
    track
        .clips
        .iter()
        .enumerate()
        .find(|(_, c)| c.id == clip_id)
        .map(|(i, c)| (i, c.clone()))
        .ok_or_else(|| TimelineError::invalid(format!("clip {clip_id} not found on track")))
}

fn ensure_unlocked(track: &Track) -> Result<()> {
    if track.locked {
        Err(TimelineError::invalid(format!(
            "track {} is locked",
            track.id
        )))
    } else {
        Ok(())
    }
}

/// Replace track `idx` in `seq.tracks` with a new Arc<Track>; clones the
/// outer Vec but leaves all other Arc<Track> entries shared.
fn replace_track(seq: &Sequence, idx: usize, new_track: Track) -> Sequence {
    let mut tracks = seq.tracks.clone();
    tracks[idx] = Arc::new(new_track);
    Sequence {
        id: seq.id,
        name: seq.name.clone(),
        frame_rate: seq.frame_rate,
        tracks,
        markers: seq.markers.clone(),
    }
}

// ---- overlap check ----------------------------------------------------------

/// Half-open intervals `[a_in, a_out)` and `[b_in, b_out)` overlap?
fn intervals_overlap(
    a_in: RationalTime,
    a_out: RationalTime,
    b_in: RationalTime,
    b_out: RationalTime,
) -> bool {
    a_in < b_out && b_in < a_out
}

/// Does any clip in `clips` (other than `skip_idx` if Some) overlap the
/// half-open range `[range_in, range_out)`?
fn range_overlaps_existing(
    clips: &[Arc<Clip>],
    range_in: RationalTime,
    range_out: RationalTime,
    skip_idx: Option<usize>,
) -> bool {
    for (i, c) in clips.iter().enumerate() {
        if Some(i) == skip_idx {
            continue;
        }
        if intervals_overlap(c.timeline_in, c.timeline_out(), range_in, range_out) {
            return true;
        }
    }
    false
}

/// Insertion point that keeps `clips` sorted by `timeline_in`.
fn insertion_index(clips: &[Arc<Clip>], at: RationalTime) -> usize {
    clips
        .iter()
        .position(|c| c.timeline_in > at)
        .unwrap_or(clips.len())
}

// ---- track-level operations -------------------------------------------------

/// Append a new track to the sequence with a freshly allocated ID.
pub fn add_track(seq: &Sequence, kind: TrackKind, name: impl Into<String>) -> Sequence {
    add_track_with_id(seq, TrackId::new(), kind, name)
}

/// Append a new track using the supplied ID. The `EditLog` calls this
/// when re-applying a logged `AddTrack` so undo/redo round-trips are
/// faithful.
pub fn add_track_with_id(
    seq: &Sequence,
    track_id: TrackId,
    kind: TrackKind,
    name: impl Into<String>,
) -> Sequence {
    let new_track = Track {
        id: track_id,
        kind,
        name: name.into(),
        enabled: true,
        locked: false,
        clips: Vec::new(),
        transitions: Vec::new(),
    };
    let mut tracks = seq.tracks.clone();
    tracks.push(Arc::new(new_track));
    Sequence {
        id: seq.id,
        name: seq.name.clone(),
        frame_rate: seq.frame_rate,
        tracks,
        markers: seq.markers.clone(),
    }
}

/// Remove a track by ID.
pub fn remove_track(seq: &Sequence, track_id: TrackId) -> Result<Sequence> {
    let (idx, _) = find_track(seq, track_id)?;
    let mut tracks = seq.tracks.clone();
    tracks.remove(idx);
    Ok(Sequence {
        id: seq.id,
        name: seq.name.clone(),
        frame_rate: seq.frame_rate,
        tracks,
        markers: seq.markers.clone(),
    })
}

// ---- clip insertion ---------------------------------------------------------

/// Insert `clip` onto `track_id` at its current `timeline_in`.
///
/// Errors if the clip's timeline range overlaps an existing clip on the
/// track (callers wanting to push neighbors should call
/// `ripple_insert_clip`; callers wanting to replace overlap should call
/// `overwrite_clip`).
pub fn insert_clip(seq: &Sequence, track_id: TrackId, clip: Clip) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;

    let dur = clip.duration();
    if dur.0 < vance_core::Rational::ZERO {
        return Err(TimelineError::invalid("clip has negative duration"));
    }
    if range_overlaps_existing(&track.clips, clip.timeline_in, clip.timeline_out(), None) {
        return Err(TimelineError::invalid(
            "clip would overlap an existing clip on the track",
        ));
    }

    let pos = insertion_index(&track.clips, clip.timeline_in);
    let mut clips = track.clips.clone();
    clips.insert(pos, Arc::new(clip));

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Insert `clip` at its `timeline_in`, pushing every clip that starts at
/// or after `timeline_in` later by `clip.duration()`. Always succeeds
/// (no overlap check needed: we make space).
pub fn ripple_insert_clip(seq: &Sequence, track_id: TrackId, clip: Clip) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;

    let dur = clip.duration();
    if dur.0 < vance_core::Rational::ZERO {
        return Err(TimelineError::invalid("clip has negative duration"));
    }

    let at = clip.timeline_in;
    let mut clips: Vec<Arc<Clip>> = Vec::with_capacity(track.clips.len() + 1);
    let mut inserted = false;
    for c in track.clips.iter() {
        if !inserted && c.timeline_in >= at {
            clips.push(Arc::new(clip.clone()));
            inserted = true;
        }
        if c.timeline_in >= at {
            // Shift this and every later clip by the inserted clip's duration.
            let mut shifted = (**c).clone();
            shifted.timeline_in = add(shifted.timeline_in, dur);
            clips.push(Arc::new(shifted));
        } else {
            clips.push(c.clone());
        }
    }
    if !inserted {
        clips.push(Arc::new(clip));
    }

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Place `clip` on `track_id`, removing or trimming any clips it
/// overlaps. Whole-clip overlaps are removed; partial overlaps trim the
/// existing clip's source range so that its visible portion is the
/// non-overlapping piece.
pub fn overwrite_clip(seq: &Sequence, track_id: TrackId, clip: Clip) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;

    let dur = clip.duration();
    if dur.0 < vance_core::Rational::ZERO {
        return Err(TimelineError::invalid("clip has negative duration"));
    }

    let new_in = clip.timeline_in;
    let new_out = clip.timeline_out();

    let mut kept: Vec<Arc<Clip>> = Vec::with_capacity(track.clips.len() + 1);
    for c in track.clips.iter() {
        let c_in = c.timeline_in;
        let c_out = c.timeline_out();
        if !intervals_overlap(c_in, c_out, new_in, new_out) {
            kept.push(c.clone());
            continue;
        }
        // There is overlap. Decide what to keep of the existing clip.
        let left_kept = c_in < new_in;
        let right_kept = c_out > new_out;

        if left_kept {
            // Keep the part of `c` that ends at `new_in`.
            let kept_dur = sub(new_in, c_in);
            let mut left = (**c).clone();
            left.id = ClipId::new();
            left.source_out = add(c.source_in, kept_dur);
            kept.push(Arc::new(left));
        }
        if right_kept {
            // Keep the part of `c` that starts at `new_out`.
            let trimmed_off = sub(new_out, c_in);
            let mut right = (**c).clone();
            right.id = ClipId::new();
            right.source_in = add(c.source_in, trimmed_off);
            right.timeline_in = new_out;
            kept.push(Arc::new(right));
        }
    }
    let pos = insertion_index(&kept, new_in);
    kept.insert(pos, Arc::new(clip));

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: kept,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

// ---- clip removal -----------------------------------------------------------

/// Remove `clip_id` from `track_id`, leaving a gap in its place.
pub fn remove_clip(seq: &Sequence, track_id: TrackId, clip_id: ClipId) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, _) = find_clip(&track, clip_id)?;

    let mut clips = track.clips.clone();
    clips.remove(clip_idx);

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Remove `clip_id` from `track_id` and shift every later clip on the
/// same track left by the removed clip's duration so the gap closes.
pub fn ripple_remove_clip(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, removed) = find_clip(&track, clip_id)?;

    let dur = removed.duration();
    let mut clips: Vec<Arc<Clip>> = Vec::with_capacity(track.clips.len() - 1);
    for (i, c) in track.clips.iter().enumerate() {
        if i == clip_idx {
            continue;
        }
        if i > clip_idx {
            let mut shifted = (**c).clone();
            shifted.timeline_in = sub(shifted.timeline_in, dur);
            clips.push(Arc::new(shifted));
        } else {
            clips.push(c.clone());
        }
    }

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

// ---- per-clip edits ---------------------------------------------------------

/// Slip a clip: change which portion of the source media is visible
/// without changing the clip's position or duration on the timeline.
/// Positive delta moves the visible window later within the source.
pub fn slip(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    delta: RationalTime,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, c) = find_clip(&track, clip_id)?;

    let mut updated = (*c).clone();
    updated.source_in = add(updated.source_in, delta);
    updated.source_out = add(updated.source_out, delta);
    if updated.source_in.0 < vance_core::Rational::ZERO {
        return Err(TimelineError::invalid("slip would move source_in negative"));
    }

    let mut clips = track.clips.clone();
    clips[clip_idx] = Arc::new(updated);

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Slide a clip: change its timeline position without changing the
/// source range. Errors if the new position would overlap a neighbor.
pub fn slide(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    delta: RationalTime,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, c) = find_clip(&track, clip_id)?;

    let new_in = add(c.timeline_in, delta);
    let new_out = add(c.timeline_out(), delta);
    if new_in.0 < vance_core::Rational::ZERO {
        return Err(TimelineError::invalid("slide would move timeline_in negative"));
    }
    if range_overlaps_existing(&track.clips, new_in, new_out, Some(clip_idx)) {
        return Err(TimelineError::invalid("slide would overlap a neighbor"));
    }

    let mut updated = (*c).clone();
    updated.timeline_in = new_in;
    let mut clips = track.clips.clone();
    clips.remove(clip_idx);
    let pos = insertion_index(&clips, new_in);
    clips.insert(pos, Arc::new(updated));

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Trim the in-point of a clip. `new_source_in` is the new visible
/// start within the source media. The clip's `timeline_in` shifts by
/// the same delta so the unaffected (right-hand) content keeps its
/// position on the timeline. The clip's `timeline_out` therefore stays
/// fixed.
pub fn trim_in(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    new_source_in: RationalTime,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, c) = find_clip(&track, clip_id)?;

    if new_source_in.0 < vance_core::Rational::ZERO {
        return Err(TimelineError::invalid("source_in cannot be negative"));
    }
    if new_source_in > c.source_out {
        return Err(TimelineError::invalid("trim_in past source_out"));
    }

    let delta = sub(new_source_in, c.source_in);
    let mut updated = (*c).clone();
    updated.source_in = new_source_in;
    updated.timeline_in = add(updated.timeline_in, delta);
    if range_overlaps_existing(
        &track.clips,
        updated.timeline_in,
        updated.timeline_out(),
        Some(clip_idx),
    ) {
        return Err(TimelineError::invalid("trim_in would overlap a neighbor"));
    }

    let mut clips = track.clips.clone();
    clips[clip_idx] = Arc::new(updated);

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Trim the out-point of a clip. `new_source_out` is the new visible
/// end within the source media. Timeline `timeline_in` is unchanged;
/// `timeline_out` slides to match the new duration.
pub fn trim_out(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    new_source_out: RationalTime,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, c) = find_clip(&track, clip_id)?;

    if new_source_out < c.source_in {
        return Err(TimelineError::invalid("trim_out before source_in"));
    }

    let mut updated = (*c).clone();
    updated.source_out = new_source_out;
    if range_overlaps_existing(
        &track.clips,
        updated.timeline_in,
        updated.timeline_out(),
        Some(clip_idx),
    ) {
        return Err(TimelineError::invalid("trim_out would overlap a neighbor"));
    }

    let mut clips = track.clips.clone();
    clips[clip_idx] = Arc::new(updated);

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Attach an `Effect` to a clip's effects list. Effects of the same
/// kind can be stacked; callers that want at-most-one-per-kind should
/// remove first. Effect IDs come from the caller so undo/redo replays
/// produce byte-identical results.
pub fn add_effect_to_clip(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    effect: Effect,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;

    let mut updated = (*clip).clone();
    updated.effects.push(effect);

    let mut clips = track.clips.clone();
    clips[clip_idx] = Arc::new(updated);

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Update (or insert) a single parameter on a clip's effect by ID.
/// Returns an error if the clip/effect can't be found. Each call is
/// its own EditLog entry — slider-style live updates will compound
/// into many undo steps; 5.5 (keyframes) will batch them.
pub fn update_effect_param(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    effect_id: EffectId,
    key: String,
    value: EffectValue,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;

    let mut updated = (*clip).clone();
    let target = updated
        .effects
        .iter_mut()
        .find(|e| e.id == effect_id)
        .ok_or_else(|| TimelineError::invalid(format!("effect {effect_id} not on clip")))?;
    target.params.insert(key, value);

    let mut clips = track.clips.clone();
    clips[clip_idx] = Arc::new(updated);
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Update (or insert) *several* parameters on a clip's effect in one
/// shot. Used by the color page so a wheel drag (which moves R, G, B
/// and master together) commits as a single undoable step instead of
/// N. Errors if the clip/effect can't be found.
pub fn update_effect_params(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    effect_id: EffectId,
    params: &[(String, EffectValue)],
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;

    let mut updated = (*clip).clone();
    let target = updated
        .effects
        .iter_mut()
        .find(|e| e.id == effect_id)
        .ok_or_else(|| TimelineError::invalid(format!("effect {effect_id} not on clip")))?;
    for (k, v) in params {
        target.params.insert(k.clone(), v.clone());
    }

    let mut clips = track.clips.clone();
    clips[clip_idx] = Arc::new(updated);
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Remove an effect (by ID) from a clip's effects list. No-op if
/// the effect isn't on the clip.
pub fn remove_effect_from_clip(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    effect_id: EffectId,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;

    let mut updated = (*clip).clone();
    updated.effects.retain(|e| e.id != effect_id);

    let mut clips = track.clips.clone();
    clips[clip_idx] = Arc::new(updated);

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Move an effect to a new index in the clip's effects list. The list
/// order is the serial render order (for color-grade nodes the first
/// entry is applied first), so reordering re-sequences the chain.
/// `new_index` is clamped into range. Errors if the effect isn't on
/// the clip. A no-op move (same resolved index) still returns a fresh
/// sequence so undo/redo stays uniform.
pub fn move_effect_on_clip(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    effect_id: EffectId,
    new_index: usize,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;

    let mut updated = (*clip).clone();
    let cur = updated
        .effects
        .iter()
        .position(|e| e.id == effect_id)
        .ok_or_else(|| TimelineError::invalid(format!("effect {effect_id} not on clip")))?;
    let dest = new_index.min(updated.effects.len().saturating_sub(1));
    if cur != dest {
        let e = updated.effects.remove(cur);
        updated.effects.insert(dest, e);
    }

    let mut clips = track.clips.clone();
    clips[clip_idx] = Arc::new(updated);
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Set the `enabled` (bypass) flag on a clip's effect by ID. Used for
/// per-node bypass on the color page — `enabled` is a struct field,
/// not a params key, so it needs its own op rather than routing
/// through `update_effect_param`. Errors if the effect isn't found.
pub fn set_effect_enabled(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    effect_id: EffectId,
    enabled: bool,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;

    let mut updated = (*clip).clone();
    let target = updated
        .effects
        .iter_mut()
        .find(|e| e.id == effect_id)
        .ok_or_else(|| TimelineError::invalid(format!("effect {effect_id} not on clip")))?;
    target.enabled = enabled;

    let mut clips = track.clips.clone();
    clips[clip_idx] = Arc::new(updated);
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Split a clip at `at_timeline` into two clips that together cover the
/// same range. The left half keeps the original ID; the right half gets
/// a fresh ID. Errors if `at_timeline` isn't strictly inside the clip's
/// timeline range.
pub fn split(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    at_timeline: RationalTime,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, c) = find_clip(&track, clip_id)?;

    if at_timeline <= c.timeline_in || at_timeline >= c.timeline_out() {
        return Err(TimelineError::invalid(
            "split point must lie strictly inside the clip",
        ));
    }
    let into_clip = sub(at_timeline, c.timeline_in);
    let split_source = add(c.source_in, into_clip);

    let mut left = (*c).clone();
    left.source_out = split_source;

    let mut right = (*c).clone();
    right.id = ClipId::new();
    right.source_in = split_source;
    right.timeline_in = at_timeline;

    let mut clips = track.clips.clone();
    clips[clip_idx] = Arc::new(left);
    clips.insert(clip_idx + 1, Arc::new(right));

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

// ---- Transitions (Phase 5.4) -----------------------------------------------

/// Add a transition with a pre-allocated ID. EditLog replay calls this
/// so undo/redo produces a byte-identical sequence; UI / engine calls
/// go through the wrapper that allocates an ID for them.
pub fn add_transition_at_cut_with_id(
    seq: &Sequence,
    track_id: TrackId,
    transition_id: TransitionId,
    after_clip_id: ClipId,
    kind: TransitionKind,
    duration: RationalTime,
) -> Result<Sequence> {
    if duration.0 <= vance_core::Rational::ZERO {
        return Err(TimelineError::invalid(
            "transition duration must be positive",
        ));
    }
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_pos, clip) = find_clip(&track, after_clip_id)?;
    if clip_pos + 1 >= track.clips.len() {
        return Err(TimelineError::invalid(
            "no clip after the anchor — transition needs two adjacent clips",
        ));
    }
    let next = &track.clips[clip_pos + 1];
    let cut = clip.timeline_out();
    if next.timeline_in < cut {
        return Err(TimelineError::invalid(
            "anchor clip overlaps its neighbor — split first",
        ));
    }

    let new_t = Transition {
        id: transition_id,
        kind,
        position: cut,
        duration,
    };
    for existing in track.transitions.iter() {
        if new_t.window_in() < existing.window_out()
            && existing.window_in() < new_t.window_out()
        {
            return Err(TimelineError::invalid(
                "transition window overlaps an existing transition",
            ));
        }
    }
    let mut transitions = track.transitions.clone();
    transitions.push(Arc::new(new_t));
    transitions.sort_by_key(|t| t.position);

    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions,
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Add a transition to a track at the cut after `after_clip_id`. The
/// transition is anchored at that cut (`after_clip.timeline_out()`)
/// and extends `duration / 2` to each side. Rejects when there is no
/// clip immediately after the anchor clip, when `duration` is zero,
/// or when the new transition would overlap an existing one. Returns
/// the freshly allocated `TransitionId` so callers can record an
/// EditOp that replays with the same ID.
pub fn add_transition_at_cut(
    seq: &Sequence,
    track_id: TrackId,
    after_clip_id: ClipId,
    kind: TransitionKind,
    duration: RationalTime,
) -> Result<(Sequence, TransitionId)> {
    let id = TransitionId::new();
    let new_seq = add_transition_at_cut_with_id(seq, track_id, id, after_clip_id, kind, duration)?;
    Ok((new_seq, id))
}

/// Remove a transition from a track by ID. No-op if not present.
pub fn remove_transition(
    seq: &Sequence,
    track_id: TrackId,
    transition_id: TransitionId,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let mut transitions = track.transitions.clone();
    transitions.retain(|t| t.id != transition_id);
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions,
    };
    Ok(replace_track(seq, idx, new_track))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::Clip;
    use crate::id::MediaItemId;
    use crate::sequence::Sequence;
    use vance_core::FrameRate;

    fn t(num: i64) -> RationalTime {
        RationalTime::new(num, 1).unwrap()
    }

    fn seed_seq() -> (Sequence, TrackId, MediaItemId) {
        let seq = Sequence::new("Seq", FrameRate::FPS_24);
        let seq = add_track(&seq, TrackKind::Video, "V1");
        let track_id = seq.tracks[0].id;
        let media = MediaItemId::new();
        (seq, track_id, media)
    }

    #[test]
    fn insert_then_remove_round_trip() {
        let (seq, tid, media) = seed_seq();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        assert_eq!(seq.tracks[0].clips.len(), 1);
        let seq = remove_clip(&seq, tid, cid).unwrap();
        assert_eq!(seq.tracks[0].clips.len(), 0);
    }

    #[test]
    fn insert_rejects_overlap() {
        let (seq, tid, media) = seed_seq();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let seq = insert_clip(&seq, tid, a).unwrap();
        let b = Clip::new(media, "b", t(3), t(0), t(5)).unwrap();
        assert!(insert_clip(&seq, tid, b).is_err());
    }

    #[test]
    fn ripple_insert_pushes_later_clips() {
        let (seq, tid, media) = seed_seq();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let b = Clip::new(media, "b", t(5), t(0), t(5)).unwrap(); // [5,10)
        let seq = insert_clip(&seq, tid, a).unwrap();
        let seq = insert_clip(&seq, tid, b).unwrap();
        // Insert a 2-second clip at t=0; existing clips should shift right by 2.
        let inserted = Clip::new(media, "i", t(0), t(0), t(2)).unwrap();
        let seq = ripple_insert_clip(&seq, tid, inserted).unwrap();
        assert_eq!(seq.tracks[0].clips.len(), 3);
        // Clip "i" at [0,2), then "a" at [2,7), then "b" at [7,12).
        assert_eq!(seq.tracks[0].clips[0].timeline_in, t(0));
        assert_eq!(seq.tracks[0].clips[1].timeline_in, t(2));
        assert_eq!(seq.tracks[0].clips[2].timeline_in, t(7));
    }

    #[test]
    fn overwrite_replaces_overlap_and_trims_partials() {
        let (seq, tid, media) = seed_seq();
        let a = Clip::new(media, "a", t(0), t(0), t(10)).unwrap(); // [0,10)
        let seq = insert_clip(&seq, tid, a).unwrap();
        let new_clip = Clip::new(media, "n", t(3), t(0), t(4)).unwrap(); // [3,7)
        let seq = overwrite_clip(&seq, tid, new_clip).unwrap();
        // We expect [0,3) (left), [3,7) (new), [7,10) (right).
        let clips = &seq.tracks[0].clips;
        assert_eq!(clips.len(), 3);
        assert_eq!(clips[0].timeline_in, t(0));
        assert_eq!(clips[0].timeline_out(), t(3));
        assert_eq!(clips[1].timeline_in, t(3));
        assert_eq!(clips[1].timeline_out(), t(7));
        assert_eq!(clips[2].timeline_in, t(7));
        assert_eq!(clips[2].timeline_out(), t(10));
    }

    #[test]
    fn ripple_remove_closes_gap() {
        let (seq, tid, media) = seed_seq();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let b = Clip::new(media, "b", t(5), t(0), t(5)).unwrap();
        let c = Clip::new(media, "c", t(10), t(0), t(5)).unwrap();
        let bid = b.id;
        let seq = insert_clip(&seq, tid, a).unwrap();
        let seq = insert_clip(&seq, tid, b).unwrap();
        let seq = insert_clip(&seq, tid, c).unwrap();
        let seq = ripple_remove_clip(&seq, tid, bid).unwrap();
        assert_eq!(seq.tracks[0].clips.len(), 2);
        assert_eq!(seq.tracks[0].clips[0].timeline_in, t(0));
        assert_eq!(seq.tracks[0].clips[1].timeline_in, t(5)); // c shifted in
    }

    #[test]
    fn slip_changes_source_only() {
        let (seq, tid, media) = seed_seq();
        let c = Clip::new(media, "c", t(0), t(2), t(7)).unwrap();
        let cid = c.id;
        let seq = insert_clip(&seq, tid, c).unwrap();
        let seq = slip(&seq, tid, cid, t(1)).unwrap();
        let updated = &seq.tracks[0].clips[0];
        assert_eq!(updated.source_in, t(3));
        assert_eq!(updated.source_out, t(8));
        assert_eq!(updated.timeline_in, t(0)); // unchanged
        assert_eq!(updated.duration(), t(5)); // unchanged
    }

    #[test]
    fn slide_rejects_overlap() {
        let (seq, tid, media) = seed_seq();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let b = Clip::new(media, "b", t(10), t(0), t(5)).unwrap();
        let bid = b.id;
        let seq = insert_clip(&seq, tid, a).unwrap();
        let seq = insert_clip(&seq, tid, b).unwrap();
        // sliding b left by 6 would put it at [4,9), overlapping a [0,5).
        assert!(slide(&seq, tid, bid, RationalTime::new(-6, 1).unwrap()).is_err());
    }

    #[test]
    fn trim_in_keeps_timeline_out_fixed() {
        let (seq, tid, media) = seed_seq();
        let c = Clip::new(media, "c", t(0), t(0), t(10)).unwrap(); // dur 10
        let cid = c.id;
        let seq = insert_clip(&seq, tid, c).unwrap();
        let original_out = seq.tracks[0].clips[0].timeline_out();
        let seq = trim_in(&seq, tid, cid, t(3)).unwrap();
        let updated = &seq.tracks[0].clips[0];
        assert_eq!(updated.source_in, t(3));
        assert_eq!(updated.timeline_in, t(3));
        assert_eq!(updated.timeline_out(), original_out);
    }

    #[test]
    fn split_yields_two_clips_summing_to_original() {
        let (seq, tid, media) = seed_seq();
        let c = Clip::new(media, "c", t(0), t(2), t(12)).unwrap(); // dur 10
        let cid = c.id;
        let seq = insert_clip(&seq, tid, c).unwrap();
        let seq = split(&seq, tid, cid, t(4)).unwrap();
        let clips = &seq.tracks[0].clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].timeline_in, t(0));
        assert_eq!(clips[0].timeline_out(), t(4));
        assert_eq!(clips[0].source_in, t(2));
        assert_eq!(clips[0].source_out, t(6));
        assert_eq!(clips[1].timeline_in, t(4));
        assert_eq!(clips[1].timeline_out(), t(10));
        assert_eq!(clips[1].source_in, t(6));
        assert_eq!(clips[1].source_out, t(12));
    }

    #[test]
    fn locked_track_rejects_edits() {
        let (seq, tid, media) = seed_seq();
        let mut t0 = (*seq.tracks[0]).clone();
        t0.locked = true;
        let mut tracks = seq.tracks.clone();
        tracks[0] = Arc::new(t0);
        let seq = Sequence {
            tracks,
            ..seq.clone()
        };
        let c = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        assert!(insert_clip(&seq, tid, c).is_err());
    }

    #[test]
    fn structural_sharing_other_tracks_remain_same_arc() {
        let mut seq = Sequence::new("Seq", FrameRate::FPS_24);
        seq = add_track(&seq, TrackKind::Video, "V1");
        seq = add_track(&seq, TrackKind::Video, "V2");
        let v1_id = seq.tracks[0].id;
        let v2_arc_before = seq.tracks[1].clone();

        let media = MediaItemId::new();
        let c = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let seq = insert_clip(&seq, v1_id, c).unwrap();

        // V2 wasn't touched; it must be the *same Arc* (pointer equality).
        assert!(Arc::ptr_eq(&seq.tracks[1], &v2_arc_before));
    }

    // ---- Grade-node ops (Phase 6.6) ----------------------------------------

    /// Seed a clip with three effects (a, b, c by kind) so we can test
    /// reorder / enable by ID.
    fn seed_clip_with_effects() -> (Sequence, TrackId, ClipId, [EffectId; 3]) {
        let (seq, tid, media) = seed_seq();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        let e0 = Effect::new("grade");
        let e1 = Effect::new("grade");
        let e2 = Effect::new("grade");
        let ids = [e0.id, e1.id, e2.id];
        let seq = add_effect_to_clip(&seq, tid, cid, e0).unwrap();
        let seq = add_effect_to_clip(&seq, tid, cid, e1).unwrap();
        let seq = add_effect_to_clip(&seq, tid, cid, e2).unwrap();
        (seq, tid, cid, ids)
    }

    fn effect_ids(seq: &Sequence, tid: TrackId, cid: ClipId) -> Vec<EffectId> {
        let track = seq.tracks.iter().find(|t| t.id == tid).unwrap();
        let clip = track.clips.iter().find(|c| c.id == cid).unwrap();
        clip.effects.iter().map(|e| e.id).collect()
    }

    #[test]
    fn move_effect_reorders_chain() {
        let (seq, tid, cid, ids) = seed_clip_with_effects();
        // Move the last (ids[2]) to the front.
        let seq = move_effect_on_clip(&seq, tid, cid, ids[2], 0).unwrap();
        assert_eq!(effect_ids(&seq, tid, cid), vec![ids[2], ids[0], ids[1]]);
    }

    #[test]
    fn move_effect_clamps_out_of_range_index() {
        let (seq, tid, cid, ids) = seed_clip_with_effects();
        // new_index way past the end → clamp to last slot.
        let seq = move_effect_on_clip(&seq, tid, cid, ids[0], 99).unwrap();
        assert_eq!(effect_ids(&seq, tid, cid), vec![ids[1], ids[2], ids[0]]);
    }

    #[test]
    fn move_effect_same_index_is_noop_order() {
        let (seq, tid, cid, ids) = seed_clip_with_effects();
        let seq = move_effect_on_clip(&seq, tid, cid, ids[1], 1).unwrap();
        assert_eq!(effect_ids(&seq, tid, cid), ids.to_vec());
    }

    #[test]
    fn move_effect_unknown_id_errors() {
        let (seq, tid, cid, _ids) = seed_clip_with_effects();
        assert!(move_effect_on_clip(&seq, tid, cid, EffectId::new(), 0).is_err());
    }

    #[test]
    fn set_effect_enabled_toggles() {
        let (seq, tid, cid, ids) = seed_clip_with_effects();
        let seq = set_effect_enabled(&seq, tid, cid, ids[1], false).unwrap();
        let track = seq.tracks.iter().find(|t| t.id == tid).unwrap();
        let clip = track.clips.iter().find(|c| c.id == cid).unwrap();
        let e = clip.effects.iter().find(|e| e.id == ids[1]).unwrap();
        assert!(!e.enabled);
        // Other effects untouched.
        assert!(clip.effects.iter().find(|e| e.id == ids[0]).unwrap().enabled);
    }

    #[test]
    fn set_effect_enabled_unknown_id_errors() {
        let (seq, tid, cid, _ids) = seed_clip_with_effects();
        assert!(set_effect_enabled(&seq, tid, cid, EffectId::new(), false).is_err());
    }

    #[test]
    fn update_effect_params_writes_all_keys() {
        let (seq, tid, cid, ids) = seed_clip_with_effects();
        let seq = update_effect_params(
            &seq,
            tid,
            cid,
            ids[0],
            &[
                ("gain_r".into(), EffectValue::Number(1.5)),
                ("gain_g".into(), EffectValue::Number(1.2)),
                ("gain_b".into(), EffectValue::Number(0.8)),
            ],
        )
        .unwrap();
        let track = seq.tracks.iter().find(|t| t.id == tid).unwrap();
        let clip = track.clips.iter().find(|c| c.id == cid).unwrap();
        let e = clip.effects.iter().find(|e| e.id == ids[0]).unwrap();
        assert_eq!(e.params.get("gain_r"), Some(&EffectValue::Number(1.5)));
        assert_eq!(e.params.get("gain_g"), Some(&EffectValue::Number(1.2)));
        assert_eq!(e.params.get("gain_b"), Some(&EffectValue::Number(0.8)));
    }

    #[test]
    fn update_effect_params_unknown_id_errors() {
        let (seq, tid, cid, _ids) = seed_clip_with_effects();
        assert!(update_effect_params(
            &seq,
            tid,
            cid,
            EffectId::new(),
            &[("gain_r".into(), EffectValue::Number(2.0))]
        )
        .is_err());
    }
}
