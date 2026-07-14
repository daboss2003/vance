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

use crate::bus::{bus_graph_cycle, AudioBus, BusOutput, Send};
use crate::clip::Clip;
use crate::connection::{connected_subtree, would_cycle, Connection};
use crate::effect::{Effect, EffectValue};
use crate::error::{Result, TimelineError};
use crate::id::{BusId, ClipId, EffectId, TrackId, TransitionId};
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
        master_gain_db: seq.master_gain_db,
        buses: seq.buses.clone(),
        connections: seq.connections.clone(),
        magnetic: seq.magnetic,
        captions: seq.captions.clone(),
        review_comments: seq.review_comments.clone(),
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
        mute: false,
        solo: false,
        gain_db: 0.0,
        gain_keyframes: Vec::new(),
        pan: 0.0,
        audio_effects: Vec::new(),
        output: crate::bus::BusOutput::Master,
        sends: Vec::new(),
    };
    let mut tracks = seq.tracks.clone();
    tracks.push(Arc::new(new_track));
    Sequence {
        id: seq.id,
        name: seq.name.clone(),
        frame_rate: seq.frame_rate,
        tracks,
        markers: seq.markers.clone(),
        master_gain_db: seq.master_gain_db,
        buses: seq.buses.clone(),
        connections: seq.connections.clone(),
        magnetic: seq.magnetic,
        captions: seq.captions.clone(),
        review_comments: seq.review_comments.clone(),
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
        master_gain_db: seq.master_gain_db,
        buses: seq.buses.clone(),
        connections: seq.connections.clone(),
        magnetic: seq.magnetic,
        captions: seq.captions.clone(),
        review_comments: seq.review_comments.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    // Overwriting re-IDs trimmed survivors / removes covered clips, so any
    // connection to a vanished id is pruned (GOAT-6 integrity).
    Ok(prune_dangling_connections(replace_track(seq, idx, new_track)))
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(prune_dangling_connections(replace_track(seq, idx, new_track)))
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(prune_dangling_connections(replace_track(seq, idx, new_track)))
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

// ---- Phase 7.3: mixer property setters ----------------------------------
//
// Asymmetry vs. set_effect_enabled: mute and solo are non-destructive
// playback controls (the user often toggles them on a locked track to
// audition without unlocking), so they DO NOT call `ensure_unlocked`.
// `gain_db` is a destructive parameter change (an automation commit
// could be permanent), so the track and clip gain ops DO call
// `ensure_unlocked` — matching the policy `set_effect_enabled` uses
// for effect params.

/// Set `Track.mute` (Phase 7.3). Allowed on locked tracks.
pub fn set_track_mute(seq: &Sequence, track_id: TrackId, mute: bool) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    if track.mute == mute {
        return Ok(seq.clone());
    }
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions: track.transitions.clone(),
        mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Set `Track.solo` (Phase 7.3). Allowed on locked tracks.
pub fn set_track_solo(seq: &Sequence, track_id: TrackId, solo: bool) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    if track.solo == solo {
        return Ok(seq.clone());
    }
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions: track.transitions.clone(),
        mute: track.mute,
        solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Set `Track.gain_db` (Phase 7.3). Rejects locked tracks.
pub fn set_track_gain_db(
    seq: &Sequence,
    track_id: TrackId,
    gain_db: f32,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let sanitized = sanitize_gain_db(gain_db)?;
    if track.gain_db == sanitized {
        return Ok(seq.clone());
    }
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions: track.transitions.clone(),
        mute: track.mute,
        solo: track.solo,
        gain_db: sanitized,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Set `Clip.enabled` (per-clip mute, Phase 7.3). Routed through the
/// existing `enabled` field — `enabled == !mute`. Allowed on locked
/// tracks, mirroring `SetTrackMute`.
pub fn set_clip_mute(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    mute: bool,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;
    let new_enabled = !mute;
    if clip.enabled == new_enabled {
        return Ok(seq.clone());
    }
    let mut updated = (*clip).clone();
    updated.enabled = new_enabled;
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Set `Clip.gain_db` (Phase 7.3). Rejects locked tracks.
pub fn set_clip_gain_db(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    gain_db: f32,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;
    let sanitized = sanitize_gain_db(gain_db)?;
    if clip.gain_db == sanitized {
        return Ok(seq.clone());
    }
    let mut updated = (*clip).clone();
    updated.gain_db = sanitized;
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Reject non-finite gain inputs and clamp to a sane mixer range. The
/// audio sidecar's PCM is f32 in `[-1, 1]`; +60 dB push or -60 dB
/// attenuate covers every realistic mixer use without inviting
/// numeric blow-up.
fn sanitize_gain_db(gain_db: f32) -> Result<f32> {
    if !gain_db.is_finite() {
        return Err(TimelineError::invalid("gain_db must be finite"));
    }
    Ok(gain_db.clamp(-60.0, 60.0))
}

/// Reject non-finite pan inputs and clamp to `[-1, +1]`. Phase 7.6.
fn sanitize_pan(pan: f32) -> Result<f32> {
    if !pan.is_finite() {
        return Err(TimelineError::invalid("pan must be finite"));
    }
    Ok(pan.clamp(-1.0, 1.0))
}

/// Phase 7.7 — Append an audio effect to a track's chain. Respects
/// track lock (mixer chain edits are persistent like gain).
pub fn add_audio_effect(
    seq: &Sequence,
    track_id: TrackId,
    effect: crate::audio_effect::AudioEffectSpec,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let mut audio_effects = track.audio_effects.clone();
    audio_effects.push(effect);
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions: track.transitions.clone(),
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects,
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Phase 7.7 — Remove an audio effect at `index`. Returns the
/// unchanged sequence if `index` is out of range so chained
/// EditOps never panic on stale indices.
pub fn remove_audio_effect(
    seq: &Sequence,
    track_id: TrackId,
    index: usize,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    if index >= track.audio_effects.len() {
        return Ok(seq.clone());
    }
    let mut audio_effects = track.audio_effects.clone();
    audio_effects.remove(index);
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions: track.transitions.clone(),
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects,
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Phase 7.7 — Reorder an effect within a chain. Out-of-range indices
/// (or `from == to`) leave the chain untouched.
pub fn move_audio_effect(
    seq: &Sequence,
    track_id: TrackId,
    from_index: usize,
    to_index: usize,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let len = track.audio_effects.len();
    if from_index >= len || to_index >= len || from_index == to_index {
        return Ok(seq.clone());
    }
    let mut audio_effects = track.audio_effects.clone();
    let entry = audio_effects.remove(from_index);
    audio_effects.insert(to_index, entry);
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions: track.transitions.clone(),
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects,
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Phase 7.7 — Replace an effect's spec at the given index. The
/// caller (UI param-knob handler) is responsible for sending the FULL
/// new spec; this op is intentionally coarse so undo stays cheap.
pub fn update_audio_effect(
    seq: &Sequence,
    track_id: TrackId,
    index: usize,
    effect: crate::audio_effect::AudioEffectSpec,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    if index >= track.audio_effects.len() {
        return Ok(seq.clone());
    }
    if track.audio_effects[index] == effect {
        return Ok(seq.clone());
    }
    let mut audio_effects = track.audio_effects.clone();
    audio_effects[index] = effect;
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions: track.transitions.clone(),
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects,
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Set `Track.pan` (Phase 7.6). Rejects locked tracks (pan edits
/// persist, mirroring `SetTrackGainDb`).
pub fn set_track_pan(seq: &Sequence, track_id: TrackId, pan: f32) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let sanitized = sanitize_pan(pan)?;
    if track.pan == sanitized {
        return Ok(seq.clone());
    }
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions: track.transitions.clone(),
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: sanitized,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Set `Clip.pan` (Phase 7.6). Rejects locked tracks.
pub fn set_clip_pan(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    pan: f32,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;
    let sanitized = sanitize_pan(pan)?;
    if clip.pan == sanitized {
        return Ok(seq.clone());
    }
    let mut updated = (*clip).clone();
    updated.pan = sanitized;
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Validate a keyframe lane for the mixer: every keyframe must have a
/// finite `value` (dB) and a finite, non-negative `time_seconds`, and
/// the list must be sorted ascending by time. Empty lanes are valid
/// (they mean "no automation, use the static gain").
fn validate_gain_lane(keyframes: &[crate::effect::Keyframe]) -> Result<()> {
    let mut prev_t: f64 = f64::NEG_INFINITY;
    for k in keyframes {
        if !k.time_seconds.is_finite() || k.time_seconds < 0.0 {
            return Err(TimelineError::invalid(
                "gain keyframe time_seconds must be finite and ≥ 0",
            ));
        }
        if !k.value.is_finite() {
            return Err(TimelineError::invalid(
                "gain keyframe value (dB) must be finite",
            ));
        }
        if k.time_seconds < prev_t {
            return Err(TimelineError::invalid(
                "gain keyframes must be sorted ascending by time_seconds",
            ));
        }
        prev_t = k.time_seconds;
    }
    Ok(())
}

/// Replace `Track.gain_keyframes` (Phase 7.4). Empty lane = revert to
/// the static `Track.gain_db`. Validates ordering + finiteness;
/// respects track lock (gain edits are destructive).
pub fn set_track_gain_keyframes(
    seq: &Sequence,
    track_id: TrackId,
    keyframes: Vec<crate::effect::Keyframe>,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    validate_gain_lane(&keyframes)?;
    if track.gain_keyframes == keyframes {
        return Ok(seq.clone());
    }
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions: track.transitions.clone(),
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: keyframes,
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Replace `Clip.gain_keyframes` (Phase 7.4). Keyframe times are
/// CLIP-RELATIVE seconds (the mixer subtracts `clip.timeline_in` at
/// snapshot build time). Empty = revert to static `Clip.gain_db`.
/// Respects track lock.
pub fn set_clip_gain_keyframes(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    keyframes: Vec<crate::effect::Keyframe>,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;
    validate_gain_lane(&keyframes)?;
    if clip.gain_keyframes == keyframes {
        return Ok(seq.clone());
    }
    let mut updated = (*clip).clone();
    updated.gain_keyframes = keyframes;
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Set `Clip.fade_in` (Phase 7.4 M3). `duration_seconds == 0.0` clears
/// the fade. The fade composes multiplicatively with `gain_keyframes`
/// at mix time — setting a fade does NOT erase the automation lane.
/// Validates the duration is finite, non-negative, and `<= clip
/// duration`. Respects track lock.
pub fn set_clip_fade_in(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    duration_seconds: f64,
    curve: crate::fade::FadeCurve,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;
    let new_fade = sanitize_fade(duration_seconds, curve, &clip)?;
    if clip.fade_in == new_fade {
        return Ok(seq.clone());
    }
    let mut updated = (*clip).clone();
    updated.fade_in = new_fade;
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Set `Clip.fade_out` (Phase 7.4 M3). Mirror of [`set_clip_fade_in`];
/// the fade region runs from `clip_duration - duration_seconds` to
/// the clip end.
pub fn set_clip_fade_out(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    duration_seconds: f64,
    curve: crate::fade::FadeCurve,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (clip_idx, clip) = find_clip(&track, clip_id)?;
    let new_fade = sanitize_fade(duration_seconds, curve, &clip)?;
    if clip.fade_out == new_fade {
        return Ok(seq.clone());
    }
    let mut updated = (*clip).clone();
    updated.fade_out = new_fade;
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Validate a fade duration and produce the `Option<Fade>` to store.
/// `0.0` (or near-zero floats) collapses to `None` so the mixer can
/// trivially skip the fade region. Negative / non-finite / longer-
/// than-clip durations are rejected.
fn sanitize_fade(
    duration_seconds: f64,
    curve: crate::fade::FadeCurve,
    clip: &crate::clip::Clip,
) -> Result<Option<crate::fade::Fade>> {
    if !duration_seconds.is_finite() {
        return Err(TimelineError::invalid("fade duration must be finite"));
    }
    if duration_seconds < 0.0 {
        return Err(TimelineError::invalid(
            "fade duration must be non-negative",
        ));
    }
    let clip_dur = clip.duration().0.as_f64();
    if duration_seconds > clip_dur {
        return Err(TimelineError::invalid(
            "fade duration must be <= clip duration",
        ));
    }
    if duration_seconds == 0.0 {
        Ok(None)
    } else {
        Ok(Some(crate::fade::Fade {
            duration_seconds,
            curve,
        }))
    }
}

/// Set `Sequence.master_gain_db` (Phase 7.4). Master is sequence-level
/// — no per-track lock to check. Sanitized through the shared
/// [-60, +60] clamp + NaN reject.
pub fn set_master_gain_db(seq: &Sequence, gain_db: f32) -> Result<Sequence> {
    let sanitized = sanitize_gain_db(gain_db)?;
    if seq.master_gain_db == sanitized {
        return Ok(seq.clone());
    }
    Ok(Sequence {
        id: seq.id,
        name: seq.name.clone(),
        frame_rate: seq.frame_rate,
        tracks: seq.tracks.clone(),
        markers: seq.markers.clone(),
        master_gain_db: sanitized,
        buses: seq.buses.clone(),
        connections: seq.connections.clone(),
        magnetic: seq.magnetic,
        captions: seq.captions.clone(),
        review_comments: seq.review_comments.clone(),
    })
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
    let right_id = ClipId::new();
    right.id = right_id;
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    let mut out = replace_track(seq, idx, new_track);
    // GOAT-6: splitting a CONNECTED clip should keep BOTH halves attached to
    // their anchor (else the right half tears free under a magnetic move).
    // Duplicate every inbound connection onto the right half's new id.
    let inherited: Vec<Connection> = out
        .connections
        .iter()
        .filter(|c| c.connected == clip_id)
        .map(|c| Connection::new(c.anchor, right_id))
        .collect();
    out.connections.extend(inherited);
    Ok(out)
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
        params: std::collections::BTreeMap::new(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
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
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

/// Set one parameter on a transition (used for WASM transitions). The
/// stored value for `key` is replaced (or inserted). Built-in dissolve
/// transitions ignore params but the op is still applied uniformly.
pub fn set_transition_param(
    seq: &Sequence,
    track_id: TrackId,
    transition_id: TransitionId,
    key: String,
    value: crate::effect::EffectValue,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let mut found = false;
    let transitions = track
        .transitions
        .iter()
        .map(|t| {
            if t.id == transition_id {
                found = true;
                let mut next = (**t).clone();
                next.params.insert(key.clone(), value.clone());
                std::sync::Arc::new(next)
            } else {
                t.clone()
            }
        })
        .collect();
    if !found {
        return Err(TimelineError::invalid("no such transition"));
    }
    let new_track = Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips: track.clips.clone(),
        transitions,
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    };
    Ok(replace_track(seq, idx, new_track))
}

// ---- GOAT-4: mixer buses + sends --------------------------------------------

/// Clone `seq` with a new bus list.
fn with_buses(seq: &Sequence, buses: Vec<AudioBus>) -> Sequence {
    Sequence {
        id: seq.id,
        name: seq.name.clone(),
        frame_rate: seq.frame_rate,
        tracks: seq.tracks.clone(),
        markers: seq.markers.clone(),
        master_gain_db: seq.master_gain_db,
        buses,
        connections: seq.connections.clone(),
        magnetic: seq.magnetic,
        captions: seq.captions.clone(),
        review_comments: seq.review_comments.clone(),
    }
}

fn find_bus(seq: &Sequence, bus_id: BusId) -> Result<usize> {
    seq.buses
        .iter()
        .position(|b| b.id == bus_id)
        .ok_or_else(|| TimelineError::invalid("no such bus"))
}

/// Add a new submix bus (routed to master) with the supplied id.
pub fn add_bus_with_id(seq: &Sequence, bus_id: BusId, name: impl Into<String>) -> Sequence {
    let mut bus = AudioBus::new(name);
    bus.id = bus_id;
    let mut buses = seq.buses.clone();
    buses.push(bus);
    with_buses(seq, buses)
}

/// Remove a bus. Anything routed to it (track/bus main outputs) falls back
/// to master, and sends targeting it are dropped — so no dangling routes.
pub fn remove_bus(seq: &Sequence, bus_id: BusId) -> Result<Sequence> {
    find_bus(seq, bus_id)?;
    let buses: Vec<AudioBus> = seq
        .buses
        .iter()
        .filter(|b| b.id != bus_id)
        .map(|b| {
            let mut b = b.clone();
            if b.output == BusOutput::Bus(bus_id) {
                b.output = BusOutput::Master;
            }
            b.sends.retain(|s| s.target != bus_id);
            b
        })
        .collect();
    let mut out = with_buses(seq, buses);
    // Re-route any TRACK that pointed at the removed bus + drop its sends.
    let tracks: Vec<Arc<Track>> = out
        .tracks
        .iter()
        .map(|t| {
            if t.output == BusOutput::Bus(bus_id) || t.sends.iter().any(|s| s.target == bus_id) {
                let mut nt = (**t).clone();
                if nt.output == BusOutput::Bus(bus_id) {
                    nt.output = BusOutput::Master;
                }
                nt.sends.retain(|s| s.target != bus_id);
                Arc::new(nt)
            } else {
                t.clone()
            }
        })
        .collect();
    out.tracks = tracks;
    Ok(out)
}

/// Set a bus's fader gain in dB.
pub fn set_bus_gain_db(seq: &Sequence, bus_id: BusId, gain_db: f32) -> Result<Sequence> {
    let idx = find_bus(seq, bus_id)?;
    let g = sanitize_gain_db(gain_db)?;
    let mut buses = seq.buses.clone();
    buses[idx].gain_db = g;
    Ok(with_buses(seq, buses))
}

/// Set a bus's stereo pan, clamped to `[-1, 1]`.
pub fn set_bus_pan(seq: &Sequence, bus_id: BusId, pan: f32) -> Result<Sequence> {
    let idx = find_bus(seq, bus_id)?;
    let mut buses = seq.buses.clone();
    buses[idx].pan = pan.clamp(-1.0, 1.0);
    Ok(with_buses(seq, buses))
}

/// Mute / unmute a bus.
pub fn set_bus_mute(seq: &Sequence, bus_id: BusId, mute: bool) -> Result<Sequence> {
    let idx = find_bus(seq, bus_id)?;
    let mut buses = seq.buses.clone();
    buses[idx].mute = mute;
    Ok(with_buses(seq, buses))
}

/// Route a bus's main output (to master or another bus). Rejects a target
/// that doesn't exist or that would create a routing cycle.
pub fn set_bus_output(seq: &Sequence, bus_id: BusId, output: BusOutput) -> Result<Sequence> {
    let idx = find_bus(seq, bus_id)?;
    if let BusOutput::Bus(target) = output {
        if target == bus_id {
            return Err(TimelineError::invalid("a bus cannot route to itself"));
        }
        find_bus(seq, target)?;
    }
    let mut buses = seq.buses.clone();
    buses[idx].output = output;
    // Send-aware: an output change can close a loop through existing sends.
    if bus_graph_cycle(&buses).is_some() {
        return Err(TimelineError::invalid("bus routing would create a cycle"));
    }
    Ok(with_buses(seq, buses))
}

/// Route a track's main output (to master or a bus). Rejects a missing bus.
pub fn route_track_to_bus(seq: &Sequence, track_id: TrackId, output: BusOutput) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    if let BusOutput::Bus(target) = output {
        find_bus(seq, target)?;
    }
    let mut nt = (*track).clone();
    nt.output = output;
    Ok(replace_track(seq, idx, nt))
}

/// Add an aux send from a track to a bus. Rejects a missing target bus.
pub fn add_track_send(seq: &Sequence, track_id: TrackId, send: Send) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    find_bus(seq, send.target)?;
    let mut nt = (*track).clone();
    nt.sends.push(send);
    Ok(replace_track(seq, idx, nt))
}

/// Remove a track's send at `index`. No-op if the index is out of range.
pub fn remove_track_send(seq: &Sequence, track_id: TrackId, index: usize) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    if index >= track.sends.len() {
        return Ok(seq.clone());
    }
    let mut nt = (*track).clone();
    nt.sends.remove(index);
    Ok(replace_track(seq, idx, nt))
}

/// GOAT-4 — append an insert effect to a bus's chain.
pub fn add_bus_insert(
    seq: &Sequence,
    bus_id: BusId,
    effect: crate::audio_effect::AudioEffectSpec,
) -> Result<Sequence> {
    let idx = find_bus(seq, bus_id)?;
    let mut buses = seq.buses.clone();
    buses[idx].inserts.push(effect);
    Ok(with_buses(seq, buses))
}

/// Remove the bus insert at `index`. No-op if out of range.
pub fn remove_bus_insert(seq: &Sequence, bus_id: BusId, index: usize) -> Result<Sequence> {
    let idx = find_bus(seq, bus_id)?;
    if index >= seq.buses[idx].inserts.len() {
        return Ok(seq.clone());
    }
    let mut buses = seq.buses.clone();
    buses[idx].inserts.remove(index);
    Ok(with_buses(seq, buses))
}

/// Reorder a bus insert. Out-of-range / `from == to` is a no-op.
pub fn move_bus_insert(
    seq: &Sequence,
    bus_id: BusId,
    from_index: usize,
    to_index: usize,
) -> Result<Sequence> {
    let idx = find_bus(seq, bus_id)?;
    let len = seq.buses[idx].inserts.len();
    if from_index >= len || to_index >= len || from_index == to_index {
        return Ok(seq.clone());
    }
    let mut buses = seq.buses.clone();
    let entry = buses[idx].inserts.remove(from_index);
    buses[idx].inserts.insert(to_index, entry);
    Ok(with_buses(seq, buses))
}

/// Replace a bus insert's spec at `index` (the coarse param-update op,
/// mirroring [`update_audio_effect`]). No-op on out-of-range / unchanged.
pub fn update_bus_insert(
    seq: &Sequence,
    bus_id: BusId,
    index: usize,
    effect: crate::audio_effect::AudioEffectSpec,
) -> Result<Sequence> {
    let idx = find_bus(seq, bus_id)?;
    if index >= seq.buses[idx].inserts.len() || seq.buses[idx].inserts[index] == effect {
        return Ok(seq.clone());
    }
    let mut buses = seq.buses.clone();
    buses[idx].inserts[index] = effect;
    Ok(with_buses(seq, buses))
}

/// GOAT-4 — add an aux send from a bus to another bus. Rejects a missing
/// target, a self-send, or a send that would create a feedback cycle
/// (checked over the full output + send graph).
pub fn add_bus_send(seq: &Sequence, bus_id: BusId, send: Send) -> Result<Sequence> {
    let idx = find_bus(seq, bus_id)?;
    if send.target == bus_id {
        return Err(TimelineError::invalid("a bus cannot send to itself"));
    }
    find_bus(seq, send.target)?;
    let mut buses = seq.buses.clone();
    buses[idx].sends.push(send);
    if bus_graph_cycle(&buses).is_some() {
        return Err(TimelineError::invalid("bus send would create a feedback cycle"));
    }
    Ok(with_buses(seq, buses))
}

/// Remove a bus's send at `index`. No-op if out of range.
pub fn remove_bus_send(seq: &Sequence, bus_id: BusId, index: usize) -> Result<Sequence> {
    let idx = find_bus(seq, bus_id)?;
    if index >= seq.buses[idx].sends.len() {
        return Ok(seq.clone());
    }
    let mut buses = seq.buses.clone();
    buses[idx].sends.remove(index);
    Ok(with_buses(seq, buses))
}

// ---- GOAT-6: clip connections + magnetic timeline ---------------------------

/// Drop connections referencing a clip id that no longer exists anywhere
/// in the sequence (so removing / re-IDing a clip never leaves a dangling
/// FCPX attachment). Returns the sequence unchanged when nothing dangles.
fn prune_dangling_connections(mut seq: Sequence) -> Sequence {
    if seq.connections.is_empty() {
        return seq;
    }
    let live: std::collections::HashSet<ClipId> = seq
        .tracks
        .iter()
        .flat_map(|t| t.clips.iter().map(|c| c.id))
        .collect();
    seq.connections
        .retain(|c| live.contains(&c.anchor) && live.contains(&c.connected));
    seq
}

/// Does a clip list (any order) contain a pair of overlapping clips?
/// Used to validate cross-track ripple results against the no-overlap
/// invariant before committing.
fn track_has_overlap(clips: &[Arc<Clip>]) -> bool {
    let mut sorted: Vec<&Arc<Clip>> = clips.iter().collect();
    sorted.sort_by_key(|c| c.timeline_in);
    sorted
        .windows(2)
        .any(|w| w[0].timeline_out() > w[1].timeline_in)
}

/// Find a clip anywhere in the sequence → `(track_idx, clip_idx, clip)`.
fn find_clip_anywhere(seq: &Sequence, clip_id: ClipId) -> Result<(usize, usize, Arc<Clip>)> {
    for (ti, t) in seq.tracks.iter().enumerate() {
        if let Some((ci, c)) = t.clips.iter().enumerate().find(|(_, c)| c.id == clip_id) {
            return Ok((ti, ci, c.clone()));
        }
    }
    Err(TimelineError::invalid(format!(
        "clip {clip_id} not found in sequence"
    )))
}

/// Clone a track with a replacement clip list (all other fields shared).
fn rebuild_track(track: &Track, clips: Vec<Arc<Clip>>) -> Track {
    Track {
        id: track.id,
        kind: track.kind,
        name: track.name.clone(),
        enabled: track.enabled,
        locked: track.locked,
        clips,
        transitions: track.transitions.clone(),
        mute: track.mute,
        solo: track.solo,
        gain_db: track.gain_db,
        gain_keyframes: track.gain_keyframes.clone(),
        pan: track.pan,
        audio_effects: track.audio_effects.clone(),
        output: track.output,
        sends: track.sends.clone(),
    }
}

/// GOAT-6 — connect `connected` to `anchor` so it follows the anchor under
/// magnetic moves / ripples. Rejects missing clips, self-connection, and
/// cycles. A clip has at most one anchor — re-connecting replaces it.
pub fn connect_clip(seq: &Sequence, anchor: ClipId, connected: ClipId) -> Result<Sequence> {
    find_clip_anywhere(seq, anchor)?;
    find_clip_anywhere(seq, connected)?;
    if would_cycle(&seq.connections, anchor, connected) {
        return Err(TimelineError::invalid(
            "connection would create a cycle (or self-loop)",
        ));
    }
    let mut conns: Vec<Connection> = seq
        .connections
        .iter()
        .copied()
        .filter(|c| c.connected != connected)
        .collect();
    conns.push(Connection::new(anchor, connected));
    let mut out = seq.clone();
    out.connections = conns;
    Ok(out)
}

/// Remove the connection (if any) whose `connected` clip is `connected`.
pub fn disconnect_clip(seq: &Sequence, connected: ClipId) -> Result<Sequence> {
    if !seq.connections.iter().any(|c| c.connected == connected) {
        return Ok(seq.clone());
    }
    let mut out = seq.clone();
    out.connections.retain(|c| c.connected != connected);
    Ok(out)
}

/// Toggle the sequence's magnetic-timeline mode.
pub fn set_magnetic(seq: &Sequence, magnetic: bool) -> Sequence {
    let mut out = seq.clone();
    out.magnetic = magnetic;
    out
}

/// GOAT-6 — move a clip AND its connected subtree by one delta, keeping
/// the group's internal layout. Cross-track (connected clips on other
/// tracks move too). Clamps so no moved clip lands before t=0; rejects a
/// move that would overlap a non-moved clip, or that touches a locked track.
pub fn magnetic_move_clip(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
    new_timeline_in: RationalTime,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    let (_, clip) = find_clip(&track, clip_id)?;
    let _ = idx;
    let mut delta = sub(new_timeline_in, clip.timeline_in);

    let moved: std::collections::HashSet<ClipId> =
        connected_subtree(&seq.connections, clip_id).into_iter().collect();

    // Clamp delta so the earliest moved clip stays at or after t=0.
    let mut earliest = clip.timeline_in;
    for t in &seq.tracks {
        for c in &t.clips {
            if moved.contains(&c.id) && c.timeline_in < earliest {
                earliest = c.timeline_in;
            }
        }
    }
    if add(earliest, delta) < RationalTime::ZERO {
        delta = sub(RationalTime::ZERO, earliest);
    }
    if delta == RationalTime::ZERO {
        return Ok(seq.clone());
    }

    let mut new_tracks: Vec<Arc<Track>> = Vec::with_capacity(seq.tracks.len());
    for t in &seq.tracks {
        let touches = t.clips.iter().any(|c| moved.contains(&c.id));
        if !touches {
            new_tracks.push(t.clone());
            continue;
        }
        ensure_unlocked(t)?;
        let mut new_clips: Vec<Arc<Clip>> = Vec::with_capacity(t.clips.len());
        for c in &t.clips {
            if moved.contains(&c.id) {
                let mut nc = (**c).clone();
                nc.timeline_in = add(nc.timeline_in, delta);
                new_clips.push(Arc::new(nc));
            } else {
                new_clips.push(c.clone());
            }
        }
        // A moved clip must not collide with a non-moved clip on this track.
        for a in &new_clips {
            if !moved.contains(&a.id) {
                continue;
            }
            for b in &new_clips {
                if a.id == b.id || moved.contains(&b.id) {
                    continue;
                }
                if intervals_overlap(a.timeline_in, a.timeline_out(), b.timeline_in, b.timeline_out()) {
                    return Err(TimelineError::invalid(
                        "magnetic move would overlap an existing clip",
                    ));
                }
            }
        }
        new_clips.sort_by_key(|c| c.timeline_in);
        new_tracks.push(Arc::new(rebuild_track(t, new_clips)));
    }
    let mut out = seq.clone();
    out.tracks = new_tracks;
    Ok(out)
}

/// GOAT-6 (Cut page) — append `clip` at the track's current end (gapless),
/// ignoring the clip's own `timeline_in`. Errors on a locked track.
pub fn append_clip(seq: &Sequence, track_id: TrackId, clip: Clip) -> Result<Sequence> {
    let (_, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let end = track
        .clips
        .iter()
        .map(|c| c.timeline_out())
        .max()
        .unwrap_or(RationalTime::ZERO);
    let mut c = clip;
    c.timeline_in = end; // duration is source-derived, so it's preserved
    insert_clip(seq, track_id, c)
}

/// GOAT-6 — magnetic ripple delete: remove `clip_id` and close the gap
/// across ALL unlocked tracks (every clip starting at or after the clip's
/// position shifts left by its duration), keeping the timeline in sync.
/// Drops any connection that referenced the removed clip.
pub fn magnetic_ripple_delete(
    seq: &Sequence,
    track_id: TrackId,
    clip_id: ClipId,
) -> Result<Sequence> {
    let (ti, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let (ci, removed) = find_clip(&track, clip_id)?;
    let at = removed.timeline_in;
    let dur = removed.duration();

    let mut new_tracks: Vec<Arc<Track>> = Vec::with_capacity(seq.tracks.len());
    for (idx, t) in seq.tracks.iter().enumerate() {
        // Locked lanes (other than the edited one) don't ripple.
        if t.locked && idx != ti {
            new_tracks.push(t.clone());
            continue;
        }
        let mut new_clips: Vec<Arc<Clip>> = Vec::with_capacity(t.clips.len());
        for (j, c) in t.clips.iter().enumerate() {
            if idx == ti && j == ci {
                continue; // the removed clip
            }
            if c.timeline_in >= at {
                let mut nc = (**c).clone();
                let shifted = sub(nc.timeline_in, dur);
                nc.timeline_in = if shifted < RationalTime::ZERO {
                    RationalTime::ZERO
                } else {
                    shifted
                };
                new_clips.push(Arc::new(nc));
            } else {
                new_clips.push(c.clone());
            }
        }
        new_clips.sort_by_key(|c| c.timeline_in);
        // The cross-track left-shift can run a later clip onto an earlier
        // one that stayed put (the gap being closed isn't free on every
        // track). Reject rather than silently break the no-overlap invariant.
        if track_has_overlap(&new_clips) {
            return Err(TimelineError::invalid(
                "ripple delete would overlap clips on another track — align cuts or use plain delete",
            ));
        }
        new_tracks.push(Arc::new(rebuild_track(t, new_clips)));
    }
    let mut out = seq.clone();
    out.tracks = new_tracks;
    out.connections
        .retain(|cn| cn.anchor != clip_id && cn.connected != clip_id);
    Ok(out)
}

/// GOAT-6 — magnetic ripple insert: open `clip.duration()` of space across
/// ALL unlocked tracks at `clip.timeline_in` (later clips shift right),
/// then place `clip` on `track_id`. Errors if the open point falls inside
/// an existing clip on ANY unlocked track (pick a cut boundary so every
/// rippled lane stays in sync — matching the target-track requirement).
pub fn magnetic_ripple_insert(seq: &Sequence, track_id: TrackId, clip: Clip) -> Result<Sequence> {
    let (_, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let at = clip.timeline_in;
    let dur = clip.duration();

    // A clip that STRADDLES `at` on an unlocked track can't be rippled
    // without splitting it; shifting only its successors would desync it.
    // Require a cut boundary at `at` on every rippled lane.
    for t in &seq.tracks {
        if t.locked {
            continue;
        }
        if t
            .clips
            .iter()
            .any(|c| c.timeline_in < at && c.timeline_out() > at)
        {
            return Err(TimelineError::invalid(
                "ripple insert needs a cut at the insertion point on every track — split the spanning clip first",
            ));
        }
    }

    let mut new_tracks: Vec<Arc<Track>> = Vec::with_capacity(seq.tracks.len());
    for t in &seq.tracks {
        if t.locked {
            new_tracks.push(t.clone());
            continue;
        }
        let mut new_clips: Vec<Arc<Clip>> = Vec::with_capacity(t.clips.len());
        for c in &t.clips {
            if c.timeline_in >= at {
                let mut nc = (**c).clone();
                nc.timeline_in = add(nc.timeline_in, dur);
                new_clips.push(Arc::new(nc));
            } else {
                new_clips.push(c.clone());
            }
        }
        new_tracks.push(Arc::new(rebuild_track(t, new_clips)));
    }
    let mut out = seq.clone();
    out.tracks = new_tracks;
    insert_clip(&out, track_id, clip)
}

// ---- GOAT-gated: review comments (collaboration) ----------------------------

use crate::id::CommentId;
use crate::review::{ReviewComment, ReviewReply};

/// Clone `seq` after mutating its review-comment list via `f`.
fn with_review_comments(seq: &Sequence, f: impl FnOnce(&mut Vec<ReviewComment>)) -> Sequence {
    let mut out = seq.clone();
    f(&mut out.review_comments);
    out
}

/// Add a review comment, keeping the list sorted by timeline position.
pub fn add_review_comment(seq: &Sequence, comment: ReviewComment) -> Sequence {
    with_review_comments(seq, |cs| {
        crate::review::insert_sorted(cs, comment);
    })
}

/// Remove the comment with `id` (no-op if absent).
pub fn remove_review_comment(seq: &Sequence, id: CommentId) -> Sequence {
    with_review_comments(seq, |cs| {
        cs.retain(|c| c.id != id);
    })
}

/// Set a comment's resolved flag.
pub fn resolve_review_comment(seq: &Sequence, id: CommentId, resolved: bool) -> Sequence {
    with_review_comments(seq, |cs| {
        if let Some(c) = cs.iter_mut().find(|c| c.id == id) {
            c.resolved = resolved;
        }
    })
}

/// Replace a comment's body text.
pub fn set_review_comment_text(seq: &Sequence, id: CommentId, text: String) -> Sequence {
    with_review_comments(seq, |cs| {
        if let Some(c) = cs.iter_mut().find(|c| c.id == id) {
            c.text = text;
        }
    })
}

/// Append a reply to a comment's thread.
pub fn reply_review_comment(seq: &Sequence, id: CommentId, reply: ReviewReply) -> Sequence {
    with_review_comments(seq, |cs| {
        if let Some(c) = cs.iter_mut().find(|c| c.id == id) {
            c.replies.push(reply);
        }
    })
}

// ---- GOAT-7: captions / subtitles -------------------------------------------

/// Clone `seq` after mutating its caption list via `f`.
fn with_captions(seq: &Sequence, f: impl FnOnce(&mut Vec<vance_captions::Cue>)) -> Sequence {
    let mut out = seq.clone();
    f(&mut out.captions);
    out
}

/// GOAT-7 — add a caption cue, keeping the list sorted by start.
pub fn add_caption(seq: &Sequence, cue: vance_captions::Cue) -> Sequence {
    with_captions(seq, |cues| {
        vance_captions::ops::add_cue(cues, cue);
    })
}

/// Remove the caption cue with `cue_id`.
pub fn remove_caption(seq: &Sequence, cue_id: vance_captions::CueId) -> Sequence {
    with_captions(seq, |cues| {
        vance_captions::ops::remove_cue(cues, cue_id);
    })
}

/// Replace a caption cue's text.
pub fn set_caption_text(seq: &Sequence, cue_id: vance_captions::CueId, text: String) -> Sequence {
    with_captions(seq, |cues| {
        vance_captions::ops::set_text(cues, cue_id, text);
    })
}

/// Set a caption cue's `[start_ms, end_ms)`.
pub fn set_caption_timing(
    seq: &Sequence,
    cue_id: vance_captions::CueId,
    start_ms: i64,
    end_ms: i64,
) -> Sequence {
    with_captions(seq, |cues| {
        vance_captions::ops::set_timing(cues, cue_id, start_ms, end_ms);
    })
}

/// Split a caption cue at `at_ms`.
pub fn split_caption(seq: &Sequence, cue_id: vance_captions::CueId, at_ms: i64) -> Sequence {
    with_captions(seq, |cues| {
        vance_captions::ops::split_cue(cues, cue_id, at_ms);
    })
}

/// Merge a caption cue with the cue that follows it.
pub fn merge_caption_with_next(seq: &Sequence, cue_id: vance_captions::CueId) -> Sequence {
    with_captions(seq, |cues| {
        vance_captions::ops::merge_with_next(cues, cue_id);
    })
}

/// Shift every caption cue by `delta_ms` (re-sync an imported track).
pub fn shift_all_captions(seq: &Sequence, delta_ms: i64) -> Sequence {
    with_captions(seq, |cues| {
        vance_captions::ops::shift_all(cues, delta_ms);
    })
}

/// Replace the whole caption list (bulk import / transcription result).
pub fn set_captions(seq: &Sequence, mut cues: Vec<vance_captions::Cue>) -> Sequence {
    vance_captions::ops::sort_cues(&mut cues);
    let mut out = seq.clone();
    out.captions = cues;
    out
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
    fn bus_ops_route_send_and_cleanup() {
        use crate::bus::{BusOutput, Send};
        use crate::id::BusId;
        let (seq, tid, _media) = seed_seq();

        // Add a Music bus + a Reverb bus.
        let music = BusId::new();
        let reverb = BusId::new();
        let seq = add_bus_with_id(&seq, music, "Music");
        let seq = add_bus_with_id(&seq, reverb, "Reverb");
        assert_eq!(seq.buses.len(), 2);

        // Route the track to Music; add a reverb send.
        let seq = route_track_to_bus(&seq, tid, BusOutput::Bus(music)).unwrap();
        assert_eq!(seq.tracks[0].output, BusOutput::Bus(music));
        let seq = add_track_send(&seq, tid, Send::to(reverb)).unwrap();
        assert_eq!(seq.tracks[0].sends.len(), 1);

        // Music → Reverb (submix). Then a cycle (Reverb → Music) is rejected.
        let seq = set_bus_output(&seq, music, BusOutput::Bus(reverb)).unwrap();
        assert!(set_bus_output(&seq, reverb, BusOutput::Bus(music)).is_err());
        // Self-route + missing bus are rejected too.
        assert!(set_bus_output(&seq, music, BusOutput::Bus(music)).is_err());
        assert!(route_track_to_bus(&seq, tid, BusOutput::Bus(BusId::new())).is_err());

        // Removing Reverb falls the track output / Music output back, and
        // drops the reverb send — no dangling routes.
        let seq = remove_bus(&seq, reverb).unwrap();
        assert_eq!(seq.buses.len(), 1);
        assert_eq!(seq.tracks[0].sends.len(), 0, "send to removed bus dropped");
        let music_bus = seq.buses.iter().find(|b| b.id == music).unwrap();
        assert_eq!(music_bus.output, BusOutput::Master, "Music re-routed to master");
    }

    #[test]
    fn bus_edit_ops_undo_redo_round_trip() {
        use crate::bus::BusOutput;
        use crate::edit_log::{EditLog, EditOp};
        use crate::id::BusId;
        let seq = Sequence::new("S", FrameRate::FPS_24);
        let mut log = EditLog::new(seq);
        let tid = TrackId::new();
        log.apply(EditOp::AddTrack { track_id: tid, kind: TrackKind::Audio, name: "A1".into() })
            .unwrap();
        let bus = BusId::new();
        log.apply(EditOp::AddBus { bus_id: bus, name: "Music".into() }).unwrap();
        log.apply(EditOp::RouteTrackToBus { track_id: tid, output: BusOutput::Bus(bus) }).unwrap();
        assert_eq!(log.current().buses.len(), 1);
        assert_eq!(log.current().tracks[0].output, BusOutput::Bus(bus));
        // Undo the route, then the bus add.
        log.undo().unwrap();
        assert_eq!(log.current().tracks[0].output, BusOutput::Master);
        log.undo().unwrap();
        assert_eq!(log.current().buses.len(), 0);
        // Redo both.
        log.redo().unwrap();
        log.redo().unwrap();
        assert_eq!(log.current().tracks[0].output, BusOutput::Bus(bus));
    }

    #[test]
    fn bus_insert_and_send_ops_round_trip() {
        use crate::audio_effect::AudioEffectSpec;
        use crate::bus::Send;
        use crate::id::BusId;
        let seq = Sequence::new("S", FrameRate::FPS_24);
        let music = BusId::new();
        let reverb = BusId::new();
        let seq = add_bus_with_id(&seq, music, "Music");
        let seq = add_bus_with_id(&seq, reverb, "Reverb");

        // Bus inserts: add EQ then a compressor, reorder, update, remove.
        let seq = add_bus_insert(&seq, music, AudioEffectSpec::default_eq6()).unwrap();
        let seq = add_bus_insert(&seq, music, AudioEffectSpec::default_compressor()).unwrap();
        let m = seq.buses.iter().find(|b| b.id == music).unwrap();
        assert_eq!(m.inserts.len(), 2);
        let seq = move_bus_insert(&seq, music, 0, 1).unwrap();
        let m = seq.buses.iter().find(|b| b.id == music).unwrap();
        assert!(matches!(m.inserts[0], AudioEffectSpec::Compressor { .. }));
        let seq = update_bus_insert(&seq, music, 1, AudioEffectSpec::default_limiter()).unwrap();
        let m = seq.buses.iter().find(|b| b.id == music).unwrap();
        assert!(matches!(m.inserts[1], AudioEffectSpec::Limiter { .. }));
        let seq = remove_bus_insert(&seq, music, 0).unwrap();
        assert_eq!(seq.buses.iter().find(|b| b.id == music).unwrap().inserts.len(), 1);

        // Bus send: Music → Reverb is fine; self-send + feedback rejected.
        let seq = add_bus_send(&seq, music, Send::to(reverb)).unwrap();
        assert_eq!(seq.buses.iter().find(|b| b.id == music).unwrap().sends.len(), 1);
        assert!(add_bus_send(&seq, music, Send::to(music)).is_err(), "self-send");
        // Reverb → Music would close a loop (Music already sends to Reverb).
        assert!(add_bus_send(&seq, reverb, Send::to(music)).is_err(), "feedback");
        let seq = remove_bus_send(&seq, music, 0).unwrap();
        assert_eq!(seq.buses.iter().find(|b| b.id == music).unwrap().sends.len(), 0);
    }

    #[test]
    fn bus_insert_send_edit_ops_undo_redo() {
        use crate::audio_effect::AudioEffectSpec;
        use crate::bus::Send;
        use crate::edit_log::{EditLog, EditOp};
        use crate::id::BusId;
        let mut log = EditLog::new(Sequence::new("S", FrameRate::FPS_24));
        let bus = BusId::new();
        let rev = BusId::new();
        log.apply(EditOp::AddBus { bus_id: bus, name: "Music".into() }).unwrap();
        log.apply(EditOp::AddBus { bus_id: rev, name: "Reverb".into() }).unwrap();
        log.apply(EditOp::AddBusInsert { bus_id: bus, effect: AudioEffectSpec::default_eq6() })
            .unwrap();
        log.apply(EditOp::AddBusSend { bus_id: bus, send: Send::to(rev) }).unwrap();
        let cur = log.current();
        let m = cur.buses.iter().find(|b| b.id == bus).unwrap();
        assert_eq!(m.inserts.len(), 1);
        assert_eq!(m.sends.len(), 1);
        // Undo send + insert.
        log.undo().unwrap();
        log.undo().unwrap();
        let m = log.current();
        let m = m.buses.iter().find(|b| b.id == bus).unwrap();
        assert_eq!(m.inserts.len(), 0);
        assert_eq!(m.sends.len(), 0);
        // Redo both.
        log.redo().unwrap();
        log.redo().unwrap();
        let cur = log.current();
        let m = cur.buses.iter().find(|b| b.id == bus).unwrap();
        assert_eq!(m.inserts.len(), 1);
        assert_eq!(m.sends.len(), 1);
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

    // ---- Phase 7.3: mixer property setters --------------------------

    fn seed_audio_seq() -> (Sequence, TrackId, MediaItemId) {
        let seq = Sequence::new("Seq", FrameRate::FPS_24);
        let seq = add_track(&seq, TrackKind::Audio, "A1");
        let track_id = seq.tracks[0].id;
        let media = MediaItemId::new();
        (seq, track_id, media)
    }

    /// There's no `set_track_locked` op yet, but the field exists; we
    /// reach into the test sequence directly to flip the first track's
    /// lock. Used only for asserting the M/S vs gain locked-policy
    /// asymmetry.
    fn lock_first_track_for_test(seq: &Sequence) -> Sequence {
        let mut tracks = seq.tracks.clone();
        let t = &tracks[0];
        let locked_track = Track {
            id: t.id,
            kind: t.kind,
            name: t.name.clone(),
            enabled: t.enabled,
            locked: true,
            clips: t.clips.clone(),
            transitions: t.transitions.clone(),
            mute: t.mute,
            solo: t.solo,
            gain_db: t.gain_db,
            gain_keyframes: t.gain_keyframes.clone(),
            pan: t.pan,
            audio_effects: t.audio_effects.clone(),
            output: t.output,
            sends: t.sends.clone(),
        };
        tracks[0] = Arc::new(locked_track);
        Sequence {
            id: seq.id,
            name: seq.name.clone(),
            frame_rate: seq.frame_rate,
            tracks,
            markers: seq.markers.clone(),
        master_gain_db: seq.master_gain_db,
        buses: seq.buses.clone(),
        connections: seq.connections.clone(),
        magnetic: seq.magnetic,
        captions: seq.captions.clone(),
        review_comments: seq.review_comments.clone(),
        }
    }

    #[test]
    fn set_track_mute_toggles_state() {
        let (seq, tid, _) = seed_audio_seq();
        assert!(!seq.tracks[0].mute);
        let seq = set_track_mute(&seq, tid, true).unwrap();
        assert!(seq.tracks[0].mute);
        let seq = set_track_mute(&seq, tid, false).unwrap();
        assert!(!seq.tracks[0].mute);
    }

    #[test]
    fn set_track_mute_is_noop_when_already_at_target() {
        let (seq, tid, _) = seed_audio_seq();
        let before_arc = Arc::as_ptr(&seq.tracks[0]);
        let seq2 = set_track_mute(&seq, tid, false).unwrap();
        // No mutation, so the Arc should be the same Sequence reused.
        assert_eq!(before_arc, Arc::as_ptr(&seq2.tracks[0]));
    }

    #[test]
    fn set_track_mute_allowed_on_locked_tracks() {
        let (seq, tid, _) = seed_audio_seq();
        let seq = lock_first_track_for_test(&seq);
        let seq = set_track_mute(&seq, tid, true).unwrap();
        assert!(seq.tracks[0].mute);
    }

    #[test]
    fn set_track_solo_allowed_on_locked_tracks() {
        let (seq, tid, _) = seed_audio_seq();
        let seq = lock_first_track_for_test(&seq);
        let seq = set_track_solo(&seq, tid, true).unwrap();
        assert!(seq.tracks[0].solo);
    }

    #[test]
    fn set_track_gain_db_rejects_locked_track() {
        let (seq, tid, _) = seed_audio_seq();
        let seq = lock_first_track_for_test(&seq);
        assert!(set_track_gain_db(&seq, tid, -3.0).is_err());
    }

    #[test]
    fn set_track_gain_db_clamps_to_mixer_range() {
        let (seq, tid, _) = seed_audio_seq();
        let seq = set_track_gain_db(&seq, tid, 200.0).unwrap();
        assert_eq!(seq.tracks[0].gain_db, 60.0);
        let seq = set_track_gain_db(&seq, tid, -200.0).unwrap();
        assert_eq!(seq.tracks[0].gain_db, -60.0);
    }

    #[test]
    fn set_track_gain_db_rejects_nan() {
        let (seq, tid, _) = seed_audio_seq();
        assert!(set_track_gain_db(&seq, tid, f32::NAN).is_err());
        assert!(set_track_gain_db(&seq, tid, f32::INFINITY).is_err());
    }

    #[test]
    fn set_clip_mute_routes_through_enabled() {
        let (seq, tid, media) = seed_audio_seq();
        let clip = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        let seq = set_clip_mute(&seq, tid, cid, true).unwrap();
        assert!(!seq.tracks[0].clips[0].enabled);
        let seq = set_clip_mute(&seq, tid, cid, false).unwrap();
        assert!(seq.tracks[0].clips[0].enabled);
    }

    #[test]
    fn set_clip_gain_db_composes_with_track_gain() {
        // The composition itself lives in the mixer (M2); here we just
        // verify that both fields can hold independent values.
        let (seq, tid, media) = seed_audio_seq();
        let clip = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        let seq = set_track_gain_db(&seq, tid, -3.0).unwrap();
        let seq = set_clip_gain_db(&seq, tid, cid, -6.0).unwrap();
        assert_eq!(seq.tracks[0].gain_db, -3.0);
        assert_eq!(seq.tracks[0].clips[0].gain_db, -6.0);
    }

    fn kf(t: f64, v: f64) -> crate::effect::Keyframe {
        crate::effect::Keyframe {
            time_seconds: t,
            value: v,
            interp: crate::effect::KeyframeInterp::Linear,
        }
    }

    #[test]
    fn set_track_gain_keyframes_replaces_lane() {
        let (seq, tid, _) = seed_audio_seq();
        let lane = vec![kf(0.0, 0.0), kf(1.0, -6.0)];
        let seq = set_track_gain_keyframes(&seq, tid, lane.clone()).unwrap();
        assert_eq!(seq.tracks[0].gain_keyframes, lane);
        // Empty list reverts.
        let seq = set_track_gain_keyframes(&seq, tid, Vec::new()).unwrap();
        assert!(seq.tracks[0].gain_keyframes.is_empty());
    }

    #[test]
    fn set_track_gain_keyframes_rejects_unsorted() {
        let (seq, tid, _) = seed_audio_seq();
        let unsorted = vec![kf(1.0, 0.0), kf(0.5, 0.0)];
        assert!(set_track_gain_keyframes(&seq, tid, unsorted).is_err());
    }

    #[test]
    fn set_track_gain_keyframes_rejects_nan_and_negative_time() {
        let (seq, tid, _) = seed_audio_seq();
        assert!(set_track_gain_keyframes(&seq, tid, vec![kf(-0.1, 0.0)]).is_err());
        assert!(set_track_gain_keyframes(&seq, tid, vec![kf(0.0, f64::NAN)]).is_err());
        assert!(set_track_gain_keyframes(&seq, tid, vec![kf(0.0, f64::INFINITY)]).is_err());
        assert!(set_track_gain_keyframes(&seq, tid, vec![kf(f64::NAN, 0.0)]).is_err());
    }

    #[test]
    fn set_track_gain_keyframes_rejects_locked_track() {
        let (seq, tid, _) = seed_audio_seq();
        let seq = lock_first_track_for_test(&seq);
        assert!(set_track_gain_keyframes(&seq, tid, vec![kf(0.0, 0.0)]).is_err());
    }

    #[test]
    fn set_clip_gain_keyframes_replaces_lane() {
        let (seq, tid, media) = seed_audio_seq();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        let lane = vec![kf(0.0, -60.0), kf(0.5, 0.0)]; // fade-in shape
        let seq = set_clip_gain_keyframes(&seq, tid, cid, lane.clone()).unwrap();
        assert_eq!(seq.tracks[0].clips[0].gain_keyframes, lane);
    }

    #[test]
    fn set_clip_fade_in_writes_fade_and_zero_clears() {
        use crate::fade::FadeCurve;
        let (seq, tid, media) = seed_audio_seq();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        let seq = set_clip_fade_in(&seq, tid, cid, 0.5, FadeCurve::Linear).unwrap();
        let fade = seq.tracks[0].clips[0].fade_in.expect("fade-in set");
        assert!((fade.duration_seconds - 0.5).abs() < 1e-9);
        assert_eq!(fade.curve, FadeCurve::Linear);
        let seq = set_clip_fade_in(&seq, tid, cid, 0.0, FadeCurve::Linear).unwrap();
        assert!(seq.tracks[0].clips[0].fade_in.is_none());
    }

    #[test]
    fn set_clip_fade_out_writes_fade_and_zero_clears() {
        use crate::fade::FadeCurve;
        let (seq, tid, media) = seed_audio_seq();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        let seq = set_clip_fade_out(&seq, tid, cid, 1.0, FadeCurve::EqualPower).unwrap();
        let fade = seq.tracks[0].clips[0].fade_out.expect("fade-out set");
        assert!((fade.duration_seconds - 1.0).abs() < 1e-9);
        assert_eq!(fade.curve, FadeCurve::EqualPower);
    }

    #[test]
    fn set_clip_fade_in_rejects_invalid_duration() {
        use crate::fade::FadeCurve;
        let (seq, tid, media) = seed_audio_seq();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        assert!(set_clip_fade_in(&seq, tid, cid, -0.1, FadeCurve::Linear).is_err());
        assert!(set_clip_fade_in(&seq, tid, cid, f64::NAN, FadeCurve::Linear).is_err());
        assert!(set_clip_fade_in(&seq, tid, cid, f64::INFINITY, FadeCurve::Linear).is_err());
        // duration > clip duration (5s)
        assert!(set_clip_fade_in(&seq, tid, cid, 6.0, FadeCurve::Linear).is_err());
    }

    #[test]
    fn set_clip_fade_in_rejects_locked_track() {
        use crate::fade::FadeCurve;
        let (seq, tid, media) = seed_audio_seq();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        let seq = lock_first_track_for_test(&seq);
        assert!(set_clip_fade_in(&seq, tid, cid, 0.5, FadeCurve::Linear).is_err());
    }

    #[test]
    fn fades_coexist_with_gain_keyframes() {
        // Setting a fade must NOT clear the manual keyframe lane.
        use crate::fade::FadeCurve;
        let (seq, tid, media) = seed_audio_seq();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        let lane = vec![kf(0.0, -6.0), kf(1.0, 0.0)];
        let seq = set_clip_gain_keyframes(&seq, tid, cid, lane.clone()).unwrap();
        let seq = set_clip_fade_in(&seq, tid, cid, 0.5, FadeCurve::Linear).unwrap();
        assert_eq!(seq.tracks[0].clips[0].gain_keyframes, lane);
        assert!(seq.tracks[0].clips[0].fade_in.is_some());
    }

    // ====================================================
    // Phase 7.6 pan tests
    // ====================================================

    #[test]
    fn set_track_pan_round_trips_and_clamps() {
        let (seq, tid, _) = seed_audio_seq();
        assert_eq!(seq.tracks[0].pan, 0.0);
        let seq = set_track_pan(&seq, tid, -0.5).unwrap();
        assert!((seq.tracks[0].pan - (-0.5)).abs() < 1e-6);
        let seq = set_track_pan(&seq, tid, 5.0).unwrap();
        assert_eq!(seq.tracks[0].pan, 1.0);
        let seq = set_track_pan(&seq, tid, -5.0).unwrap();
        assert_eq!(seq.tracks[0].pan, -1.0);
    }

    #[test]
    fn set_track_pan_rejects_nan_and_inf() {
        let (seq, tid, _) = seed_audio_seq();
        assert!(set_track_pan(&seq, tid, f32::NAN).is_err());
        assert!(set_track_pan(&seq, tid, f32::INFINITY).is_err());
        assert!(set_track_pan(&seq, tid, f32::NEG_INFINITY).is_err());
    }

    #[test]
    fn set_track_pan_respects_lock() {
        let (seq, tid, _) = seed_audio_seq();
        let seq = lock_first_track_for_test(&seq);
        assert!(set_track_pan(&seq, tid, 0.5).is_err());
    }

    #[test]
    fn set_clip_pan_round_trips_and_clamps() {
        let (seq, tid, media) = seed_audio_seq();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        assert_eq!(seq.tracks[0].clips[0].pan, 0.0);
        let seq = set_clip_pan(&seq, tid, cid, 0.7).unwrap();
        assert!((seq.tracks[0].clips[0].pan - 0.7).abs() < 1e-6);
        let seq = set_clip_pan(&seq, tid, cid, 9.0).unwrap();
        assert_eq!(seq.tracks[0].clips[0].pan, 1.0);
    }

    #[test]
    fn set_master_gain_db_round_trips() {
        let (seq, _, _) = seed_audio_seq();
        assert_eq!(seq.master_gain_db, 0.0);
        let seq = set_master_gain_db(&seq, -6.0).unwrap();
        assert!((seq.master_gain_db - (-6.0)).abs() < 1e-6);
        let seq = set_master_gain_db(&seq, 0.0).unwrap();
        assert_eq!(seq.master_gain_db, 0.0);
    }

    #[test]
    fn set_master_gain_db_clamps_and_rejects_nan() {
        let (seq, _, _) = seed_audio_seq();
        let seq = set_master_gain_db(&seq, 999.0).unwrap();
        assert_eq!(seq.master_gain_db, 60.0);
        let seq = set_master_gain_db(&seq, -999.0).unwrap();
        assert_eq!(seq.master_gain_db, -60.0);
        assert!(set_master_gain_db(&seq, f32::NAN).is_err());
        assert!(set_master_gain_db(&seq, f32::INFINITY).is_err());
    }

    #[test]
    fn set_master_gain_db_is_noop_when_already_at_target() {
        let (seq, _, _) = seed_audio_seq();
        let before = Arc::as_ptr(&seq.tracks[0]);
        let seq2 = set_master_gain_db(&seq, 0.0).unwrap();
        // No mutation → tracks Arc reused.
        assert_eq!(before, Arc::as_ptr(&seq2.tracks[0]));
    }

    #[test]
    fn set_clip_gain_db_rejects_locked_track() {
        let (seq, tid, media) = seed_audio_seq();
        let clip = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        let seq = insert_clip(&seq, tid, clip).unwrap();
        let seq = lock_first_track_for_test(&seq);
        assert!(set_clip_gain_db(&seq, tid, cid, -3.0).is_err());
    }

    // ---- GOAT-6: magnetic timeline + connections ----------------------------

    /// A sequence with V1 + V2, one clip each: V1 "a" [0,5), V2 "t" [1,3).
    fn two_track_with_clips() -> (Sequence, TrackId, TrackId, ClipId, ClipId, MediaItemId) {
        let (seq, v1, media) = seed_seq();
        let seq = add_track(&seq, TrackKind::Video, "V2");
        let v2 = seq.tracks[1].id;
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let aid = a.id;
        let tt = Clip::new(media, "t", t(1), t(0), t(2)).unwrap();
        let tid = tt.id;
        let seq = insert_clip(&seq, v1, a).unwrap();
        let seq = insert_clip(&seq, v2, tt).unwrap();
        (seq, v1, v2, aid, tid, media)
    }

    #[test]
    fn connect_then_magnetic_move_drags_connected_clip() {
        let (seq, v1, _v2, aid, tid, _m) = two_track_with_clips();
        let seq = connect_clip(&seq, aid, tid).unwrap();
        // Move the anchor "a" from 0 → 4; "t" (connected) shifts +4 too.
        let seq = magnetic_move_clip(&seq, v1, aid, t(4)).unwrap();
        let a = seq.tracks[0].clips.iter().find(|c| c.id == aid).unwrap();
        let tclip = seq.tracks[1].clips.iter().find(|c| c.id == tid).unwrap();
        assert_eq!(a.timeline_in, t(4), "anchor moved");
        assert_eq!(tclip.timeline_in, t(5), "connected clip dragged by same delta");
    }

    #[test]
    fn connect_rejects_cycle_and_self() {
        let (seq, _v1, _v2, aid, tid, _m) = two_track_with_clips();
        assert!(connect_clip(&seq, aid, aid).is_err(), "self-connection");
        let seq = connect_clip(&seq, aid, tid).unwrap();
        // t already connected to a; a → t back-edge would cycle.
        assert!(connect_clip(&seq, tid, aid).is_err(), "cycle");
    }

    #[test]
    fn magnetic_move_rejects_overlap_with_unconnected_clip() {
        let (seq, v1, media) = seed_seq();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let aid = a.id;
        let b = Clip::new(media, "b", t(5), t(0), t(5)).unwrap();
        let seq = insert_clip(&seq, v1, a).unwrap();
        let seq = insert_clip(&seq, v1, b).unwrap();
        // Moving "a" right by 2 → [2,7) overlaps "b" [5,10).
        assert!(magnetic_move_clip(&seq, v1, aid, t(2)).is_err());
    }

    #[test]
    fn append_places_at_gapless_end() {
        let (seq, v1, media) = seed_seq();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let seq = insert_clip(&seq, v1, a).unwrap();
        // Append ignores the clip's own timeline_in (here 99).
        let b = Clip::new(media, "b", t(99), t(0), t(3)).unwrap();
        let bid = b.id;
        let seq = append_clip(&seq, v1, b).unwrap();
        let b = seq.tracks[0].clips.iter().find(|c| c.id == bid).unwrap();
        assert_eq!(b.timeline_in, t(5), "appended right after the last clip");
        assert_eq!(b.timeline_out(), t(8));
    }

    #[test]
    fn magnetic_ripple_delete_closes_gap_on_all_tracks() {
        let (seq, v1, v2, aid, tid, media) = two_track_with_clips();
        // Add a second V1 clip after "a": "a2" [5,9).
        let a2 = Clip::new(media, "a2", t(5), t(0), t(4)).unwrap();
        let a2id = a2.id;
        let seq = insert_clip(&seq, v1, a2).unwrap();
        // Delete "a" (dur 5) → a2 shifts left 5 → [0,4); V2 "t" (at 1 ≥ 0)
        // shifts left 5 but clamps to 0.
        let _ = v2;
        let seq = magnetic_ripple_delete(&seq, v1, aid).unwrap();
        assert!(seq.tracks[0].clips.iter().all(|c| c.id != aid), "a removed");
        let a2 = seq.tracks[0].clips.iter().find(|c| c.id == a2id).unwrap();
        assert_eq!(a2.timeline_in, t(0), "later V1 clip rippled left by 5");
        let tclip = seq.tracks[1].clips.iter().find(|c| c.id == tid).unwrap();
        assert_eq!(tclip.timeline_in, t(0), "V2 clip rippled (clamped to 0)");
    }

    #[test]
    fn magnetic_ripple_insert_opens_space_on_all_tracks() {
        let (seq, v1, v2, aid, tid, media) = two_track_with_clips();
        let _ = (aid, v2);
        // Insert a 3s clip at t=0 on V1: everything at/after 0 shifts +3.
        let ins = Clip::new(media, "ins", t(0), t(0), t(3)).unwrap();
        let seq = magnetic_ripple_insert(&seq, v1, ins).unwrap();
        // V1 "a" was [0,5) → now [3,8); V2 "t" was [1,3) → now [4,6).
        let a = seq.tracks[0].clips.iter().find(|c| c.id == aid).unwrap();
        assert_eq!(a.timeline_in, t(3), "V1 clip shifted right");
        let tclip = seq.tracks[1].clips.iter().find(|c| c.id == tid).unwrap();
        assert_eq!(tclip.timeline_in, t(4), "V2 clip shifted right too");
    }

    #[test]
    fn magnetic_ripple_delete_rejects_cross_track_overlap() {
        // GOAT-6 review fix: closing the gap on a non-target track must not
        // run a later clip onto an earlier one.
        let (seq, v1, media) = seed_seq();
        let seq = add_track(&seq, TrackKind::Video, "V2");
        let v2 = seq.tracks[1].id;
        let x = Clip::new(media, "x", t(0), t(0), t(2)).unwrap();
        let del = Clip::new(media, "del", t(2), t(0), t(8)).unwrap();
        let delid = del.id;
        let p = Clip::new(media, "p", t(1), t(0), t(2)).unwrap();
        let q = Clip::new(media, "q", t(9), t(0), t(3)).unwrap();
        let seq = insert_clip(&seq, v1, x).unwrap();
        let seq = insert_clip(&seq, v1, del).unwrap();
        let seq = insert_clip(&seq, v2, p).unwrap();
        let seq = insert_clip(&seq, v2, q).unwrap();
        // Deleting "del" (dur 8) would shift q [9,12)→[1,4) onto p [1,3).
        assert!(magnetic_ripple_delete(&seq, v1, delid).is_err());
    }

    #[test]
    fn magnetic_ripple_insert_rejects_straddling_clip() {
        let (seq, v1, media) = seed_seq();
        let seq = add_track(&seq, TrackKind::Video, "V2");
        let v2 = seq.tracks[1].id;
        let a = Clip::new(media, "a", t(0), t(0), t(2)).unwrap();
        let s = Clip::new(media, "s", t(0), t(0), t(10)).unwrap(); // spans at=5
        let seq = insert_clip(&seq, v1, a).unwrap();
        let seq = insert_clip(&seq, v2, s).unwrap();
        let ins = Clip::new(media, "ins", t(5), t(0), t(3)).unwrap();
        assert!(magnetic_ripple_insert(&seq, v1, ins).is_err());
    }

    #[test]
    fn classic_remove_prunes_dangling_connection() {
        let (seq, _v1, _v2, aid, tid, _m) = two_track_with_clips();
        let seq = connect_clip(&seq, aid, tid).unwrap();
        assert_eq!(seq.connections.len(), 1);
        // Classic remove of the connected clip (on V2) drops the connection.
        let v2 = seq.tracks[1].id;
        let seq = remove_clip(&seq, v2, tid).unwrap();
        assert!(seq.connections.is_empty(), "dangling connection pruned");
    }

    #[test]
    fn overwrite_prunes_connection_to_covered_clip() {
        let (seq, v1, media) = seed_seq();
        let seq = add_track(&seq, TrackKind::Video, "V2");
        let v2 = seq.tracks[1].id;
        let anchor = Clip::new(media, "anchor", t(0), t(0), t(5)).unwrap();
        let aid = anchor.id;
        let lower = Clip::new(media, "lower", t(1), t(0), t(2)).unwrap(); // [1,3)
        let lid = lower.id;
        let seq = insert_clip(&seq, v1, anchor).unwrap();
        let seq = insert_clip(&seq, v2, lower).unwrap();
        let seq = connect_clip(&seq, aid, lid).unwrap();
        // Overwrite the whole "lower" clip on V2 → its id vanishes.
        let x = Clip::new(media, "x", t(0), t(0), t(4)).unwrap();
        let seq = overwrite_clip(&seq, v2, x).unwrap();
        assert!(
            seq.connections.is_empty(),
            "connection to the overwritten clip is pruned"
        );
    }

    #[test]
    fn split_keeps_both_halves_of_connected_clip_attached() {
        // GOAT-6 review fix: splitting a connected clip attaches BOTH halves
        // to the anchor (no tearing under magnetic move).
        let (seq, _v1, v2, aid, tid, _m) = two_track_with_clips();
        // "t" is [1,3) on V2, connected to anchor "a".
        let seq = connect_clip(&seq, aid, tid).unwrap();
        let seq = split(&seq, v2, tid, t(2)).unwrap();
        // Both halves of "t" must be connected to "a".
        let v2_clips = &seq.tracks[1].clips;
        assert_eq!(v2_clips.len(), 2, "split produced two halves");
        let connected: std::collections::HashSet<ClipId> =
            seq.connections.iter().map(|c| c.connected).collect();
        for c in v2_clips {
            assert!(connected.contains(&c.id), "each half is connected to the anchor");
        }
    }

    #[test]
    fn magnetic_edit_ops_undo_redo() {
        use crate::edit_log::{EditLog, EditOp};
        let (seq, v1, _v2, aid, tid, _m) = two_track_with_clips();
        let mut log = EditLog::new(seq);
        log.apply(EditOp::ConnectClip { anchor: aid, connected: tid }).unwrap();
        log.apply(EditOp::MagneticMoveClip {
            track_id: v1,
            clip_id: aid,
            new_timeline_in: t(4),
        })
        .unwrap();
        assert_eq!(
            log.current().tracks[1].clips[0].timeline_in,
            t(5),
            "connected clip moved with anchor"
        );
        log.undo().unwrap();
        assert_eq!(log.current().tracks[1].clips[0].timeline_in, t(1), "move undone");
        log.undo().unwrap();
        assert!(log.current().connections.is_empty(), "connect undone");
        log.redo().unwrap();
        log.redo().unwrap();
        assert_eq!(log.current().tracks[1].clips[0].timeline_in, t(5), "redo");
    }

    #[test]
    fn caption_edit_ops_undo_redo() {
        use crate::edit_log::{EditLog, EditOp};
        let mut log = EditLog::new(Sequence::new("S", FrameRate::FPS_24));
        let cue = vance_captions::Cue::new(1000, 3000, "hello");
        let cid = cue.id;
        log.apply(EditOp::AddCaption { cue }).unwrap();
        assert_eq!(log.current().captions.len(), 1);
        log.apply(EditOp::SetCaptionText {
            cue_id: cid,
            text: "hello world".into(),
        })
        .unwrap();
        assert_eq!(log.current().captions[0].text, "hello world");
        log.apply(EditOp::SplitCaption { cue_id: cid, at_ms: 2000 }).unwrap();
        assert_eq!(log.current().captions.len(), 2, "split");
        // Undo split + edit + add.
        log.undo().unwrap();
        assert_eq!(log.current().captions.len(), 1);
        log.undo().unwrap();
        assert_eq!(log.current().captions[0].text, "hello");
        log.undo().unwrap();
        assert!(log.current().captions.is_empty());
        log.redo().unwrap();
        assert_eq!(log.current().captions.len(), 1, "redo add");
    }
}
