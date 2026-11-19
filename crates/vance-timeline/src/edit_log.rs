//! `EditOp` and `EditLog` — branching undo/redo for `Sequence` edits.
//!
//! ## Design
//!
//! - `EditOp` is a serializable enum over every edit operation in
//!   `crate::ops`. It captures all arguments verbatim, including any
//!   freshly allocated IDs (e.g. `AddTrack { track_id, ... }`), so a
//!   logged op replays to byte-identical output.
//! - `EditLog` is a TREE of `Sequence` snapshots ("Git for edits"). Each
//!   node holds the sequence reached by one op. `undo` walks to the
//!   parent; `redo` walks to the *active child* (the branch the user last
//!   descended). Editing after an undo FORKS a new child branch rather
//!   than discarding the redo branch, so alternate cuts stay reachable via
//!   [`EditLog::goto`] / [`EditLog::branch_tips`]. Snapshots share most
//!   structure via `Arc`, so undo/redo hit the `<1ms` target; total
//!   retained history is bounded by `MAX_HISTORY_NODES`.

use serde::{Deserialize, Serialize};
use vance_core::RationalTime;

use crate::bus::{BusOutput, Send};
use crate::clip::Clip;
use crate::effect::{Effect, EffectValue};
use crate::error::Result;
use crate::id::{BusId, ClipId, EffectId, TrackId, TransitionId};
use crate::ops;
use crate::sequence::Sequence;
use crate::track::TrackKind;
use crate::transition::TransitionKind;

/// A serializable edit. Each variant maps 1:1 to a function in
/// `crate::ops`; see those for argument semantics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EditOp {
    /// Append a new (empty) track with the supplied ID.
    AddTrack {
        /// The ID the new track will have. Allocated by the caller so
        /// undo/redo replay produces the same ID.
        track_id: TrackId,
        /// Track kind (video / audio).
        kind: TrackKind,
        /// Display name.
        name: String,
    },
    /// Remove a track by ID.
    RemoveTrack {
        /// ID of the track to remove.
        track_id: TrackId,
    },
    /// Insert a clip at its `timeline_in`; rejects overlap.
    InsertClip {
        /// Track to insert into.
        track_id: TrackId,
        /// Clip to insert.
        clip: Clip,
    },
    /// Insert a clip and ripple later clips on the track right.
    RippleInsertClip {
        /// Track to insert into.
        track_id: TrackId,
        /// Clip to insert.
        clip: Clip,
    },
    /// Insert a clip; trim/remove anything it overlaps.
    OverwriteClip {
        /// Track to overwrite into.
        track_id: TrackId,
        /// Clip to insert.
        clip: Clip,
    },
    /// Remove a clip; leave a gap.
    RemoveClip {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to remove.
        clip_id: ClipId,
    },
    /// Remove a clip; close the gap by shifting later clips left.
    RippleRemoveClip {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to remove.
        clip_id: ClipId,
    },
    /// Slip — change source range only.
    Slip {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to slip.
        clip_id: ClipId,
        /// Delta added to both `source_in` and `source_out`.
        delta: RationalTime,
    },
    /// Slide — change timeline position only.
    Slide {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to slide.
        clip_id: ClipId,
        /// Delta added to `timeline_in`.
        delta: RationalTime,
    },
    /// Trim the in-point.
    TrimIn {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to trim.
        clip_id: ClipId,
        /// New `source_in`.
        new_source_in: RationalTime,
    },
    /// Trim the out-point.
    TrimOut {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to trim.
        clip_id: ClipId,
        /// New `source_out`.
        new_source_out: RationalTime,
    },
    /// Split a clip at a timeline position.
    Split {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to split.
        clip_id: ClipId,
        /// Timeline time at which to split.
        at_timeline: RationalTime,
    },
    /// Attach an effect to a clip.
    AddEffectToClip {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to attach the effect to.
        clip_id: ClipId,
        /// Effect to attach.
        effect: Effect,
    },
    /// Remove an effect from a clip by its ID.
    RemoveEffectFromClip {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip the effect is attached to.
        clip_id: ClipId,
        /// Effect ID to remove.
        effect_id: EffectId,
    },
    /// Update one parameter on an effect attached to a clip.
    UpdateEffectParam {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip the effect is attached to.
        clip_id: ClipId,
        /// Effect to mutate.
        effect_id: EffectId,
        /// Parameter key.
        key: String,
        /// New value (number / bool / string).
        value: EffectValue,
    },
    /// Add a transition at the cut following `after_clip_id` (Phase 5.4).
    AddTransitionAtCut {
        /// Track to attach the transition to.
        track_id: TrackId,
        /// Pre-allocated transition ID so replay produces stable IDs.
        transition_id: TransitionId,
        /// Clip whose `timeline_out` anchors the transition's center.
        after_clip_id: ClipId,
        /// Kind of transition (Dissolve, ...).
        kind: TransitionKind,
        /// Duration of the transition window.
        duration: RationalTime,
    },
    /// Remove a transition by ID.
    RemoveTransition {
        /// Track holding the transition.
        track_id: TrackId,
        /// Transition to remove.
        transition_id: TransitionId,
    },
    /// Set one parameter on a transition (WASM transitions). Replaces the
    /// stored value for `key`; recorded so undo/redo + replay are faithful.
    SetTransitionParam {
        /// Track holding the transition.
        track_id: TrackId,
        /// Transition to mutate.
        transition_id: TransitionId,
        /// Manifest param key.
        key: String,
        /// New value.
        value: EffectValue,
    },
    /// Move an effect to a new index in a clip's effects list
    /// (re-sequences the serial chain — used by the color page to
    /// reorder grade nodes).
    MoveEffectOnClip {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip holding the effect.
        clip_id: ClipId,
        /// Effect to move.
        effect_id: EffectId,
        /// Destination index (clamped into range).
        new_index: usize,
    },
    /// Toggle an effect's `enabled` (bypass) flag.
    SetEffectEnabled {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip holding the effect.
        clip_id: ClipId,
        /// Effect to toggle.
        effect_id: EffectId,
        /// New enabled state.
        enabled: bool,
    },
    /// Set the mixer mute on a track (Phase 7.3). Distinct from
    /// `enabled`: `enabled` removes the track from the render graph;
    /// `mute` silences its audible contribution without affecting
    /// layout. Permitted on locked tracks — mute/solo are
    /// non-destructive playback controls.
    SetTrackMute {
        /// Track to mutate.
        track_id: TrackId,
        /// New mute state.
        mute: bool,
    },
    /// Set the solo flag on a track (Phase 7.3). When any track in the
    /// sequence has `solo == true`, only soloed tracks contribute to
    /// the mix. Permitted on locked tracks.
    SetTrackSolo {
        /// Track to mutate.
        track_id: TrackId,
        /// New solo state.
        solo: bool,
    },
    /// Set the mixer gain in dB on a track (Phase 7.3). 0.0 = unity.
    /// Respects track lock — locked tracks reject gain changes
    /// because gain edits are destructive in the way mute/solo are
    /// not (a gain ramp committed during automation could be
    /// permanent).
    SetTrackGainDb {
        /// Track to mutate.
        track_id: TrackId,
        /// New gain in dB.
        gain_db: f32,
    },
    /// Set the per-clip mute on an audio (or video) clip (Phase 7.3).
    /// Routes through `Clip.enabled`, which is the existing per-clip
    /// mute used elsewhere in the engine — the EditOp gives mute its
    /// own name in the audit log so the UI's "mute" intent is
    /// distinguishable from "disable clip in render".
    SetClipMute {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to mutate.
        clip_id: ClipId,
        /// New mute (== !enabled).
        mute: bool,
    },
    /// Set the per-clip mixer gain in dB (Phase 7.3). 0.0 = unity.
    /// Composes multiplicatively with the parent track's gain.
    /// Respects track lock.
    SetClipGainDb {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to mutate.
        clip_id: ClipId,
        /// New gain in dB.
        gain_db: f32,
    },
    /// Set the master bus gain in dB (Phase 7.4). 0.0 = unity. Applied
    /// uniformly to every audio source after track/clip composition.
    /// Sequence-level — there's no track or clip lock to respect.
    SetMasterGainDb {
        /// New master gain in dB.
        gain_db: f32,
    },
    /// Set a track's stereo pan position (Phase 7.6). Range `[-1, +1]`,
    /// clamped on apply. Audio tracks only; video tracks accept the
    /// edit but the mixer ignores it. Respects track lock (pan edits
    /// are persistent — symmetric with `SetTrackGainDb`).
    SetTrackPan {
        /// Track to mutate.
        track_id: TrackId,
        /// New pan. `-1` = full left, `0` = center, `+1` = full right.
        pan: f32,
    },
    /// Set a clip's stereo pan position (Phase 7.6). Composes
    /// additively with the parent track's pan at mix time. Respects
    /// track lock.
    SetClipPan {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to mutate.
        clip_id: ClipId,
        /// New pan. `-1` = full left, `0` = center, `+1` = full right.
        pan: f32,
    },
    /// Append an audio effect to the end of a track's chain (Phase 7.7).
    /// The new effect's `enabled` flag carries through verbatim. Respects
    /// track lock.
    AddAudioEffect {
        /// Track to mutate.
        track_id: TrackId,
        /// Effect to append. Use [`crate::audio_effect::AudioEffectSpec::default_eq6`],
        /// `default_compressor`, or `default_limiter` for the conventional
        /// "Add EQ" / "Add Compressor" / "Add Limiter" button calls.
        effect: crate::audio_effect::AudioEffectSpec,
    },
    /// Remove the audio effect at the given index from a track's chain
    /// (Phase 7.7). No-op if the index is out of range.
    RemoveAudioEffect {
        /// Track to mutate.
        track_id: TrackId,
        /// Zero-based index into `Track.audio_effects`.
        index: usize,
    },
    /// Reorder one audio effect within a track's chain (Phase 7.7). No-op
    /// if either index is out of range.
    MoveAudioEffect {
        /// Track to mutate.
        track_id: TrackId,
        /// Where the effect currently lives.
        from_index: usize,
        /// Where it should land (0..=chain_len-1).
        to_index: usize,
    },
    /// Replace an audio effect's full spec (Phase 7.7). Used for every
    /// param-knob update: the engine pushes a fresh spec rather than
    /// mutating in-place so undo stays trivial.
    UpdateAudioEffect {
        /// Track to mutate.
        track_id: TrackId,
        /// Zero-based index into `Track.audio_effects`.
        index: usize,
        /// New spec — replaces the entry verbatim.
        effect: crate::audio_effect::AudioEffectSpec,
    },
    /// Replace a track's gain automation lane (Phase 7.4). Empty
    /// vector = no automation; the static `Track.gain_db` then drives
    /// the mixer. Respects track lock (gain edits are
    /// destructive / persistent, mirroring `SetTrackGainDb`).
    SetTrackGainKeyframes {
        /// Track to mutate.
        track_id: TrackId,
        /// New lane. Empty = revert to static.
        keyframes: Vec<crate::effect::Keyframe>,
    },
    /// Replace a clip's gain automation lane (Phase 7.4). Keyframe
    /// times are CLIP-RELATIVE seconds. Respects track lock.
    SetClipGainKeyframes {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to mutate.
        clip_id: ClipId,
        /// New lane (clip-relative). Empty = revert to static gain_db.
        keyframes: Vec<crate::effect::Keyframe>,
    },
    /// Set the clip's fade-in (Phase 7.4 M3). `duration_seconds == 0.0`
    /// removes the fade. Fade is multiplicative with the manual
    /// `gain_keyframes` lane — fades do not clobber automation.
    /// Respects track lock.
    SetClipFadeIn {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to mutate.
        clip_id: ClipId,
        /// Fade length in seconds, clip-relative. 0.0 = remove.
        duration_seconds: f64,
        /// Curve shape. Only `Linear` is implemented in 7.4; the
        /// other variants are persisted for forward-compat with 7.5.
        curve: crate::fade::FadeCurve,
    },
    /// Set the clip's fade-out (Phase 7.4 M3). Mirror of
    /// [`EditOp::SetClipFadeIn`]; the fade region starts at
    /// `clip_duration - duration_seconds`.
    SetClipFadeOut {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip to mutate.
        clip_id: ClipId,
        /// Fade length in seconds, clip-relative. 0.0 = remove.
        duration_seconds: f64,
        /// Curve shape (see [`EditOp::SetClipFadeIn`]).
        curve: crate::fade::FadeCurve,
    },
    /// Update several effect params in one undoable step (color-page
    /// wheel commit writes R/G/B/master together).
    UpdateEffectParams {
        /// Track holding the clip.
        track_id: TrackId,
        /// Clip holding the effect.
        clip_id: ClipId,
        /// Effect to mutate.
        effect_id: EffectId,
        /// `(key, value)` pairs applied together.
        params: Vec<(String, EffectValue)>,
    },
    /// Apply a batch of sub-ops atomically as a SINGLE undo step.
    /// Apply folds left over `ops`; if any child fails the whole
    /// compound returns Err and is NEVER recorded in the log, so the
    /// snapshot-based undo gives atomic rollback for free. The `label`
    /// flows through to the UI's undo entry (e.g. an AI tool-call
    /// turn shows as one labeled step). Phase 6.13.
    Compound {
        /// Sub-ops in apply order. Empty is a no-op.
        ops: Vec<EditOp>,
        /// Human-readable label shown in undo history.
        label: String,
    },
    /// GOAT-4 — add a mixer submix bus (routed to master) with a stable id.
    AddBus {
        /// Pre-allocated bus id so replay produces stable ids.
        bus_id: BusId,
        /// Display name.
        name: String,
    },
    /// GOAT-4 — remove a bus; dangling routes fall back to master.
    RemoveBus {
        /// Bus to remove.
        bus_id: BusId,
    },
    /// GOAT-4 — set a bus's fader gain in dB.
    SetBusGainDb {
        /// Bus to mutate.
        bus_id: BusId,
        /// New gain in dB.
        gain_db: f32,
    },
    /// GOAT-4 — set a bus's stereo pan `[-1, 1]`.
    SetBusPan {
        /// Bus to mutate.
        bus_id: BusId,
        /// New pan.
        pan: f32,
    },
    /// GOAT-4 — mute / unmute a bus.
    SetBusMute {
        /// Bus to mutate.
        bus_id: BusId,
        /// New mute state.
        mute: bool,
    },
    /// GOAT-4 — route a bus's main output (master or another bus).
    SetBusOutput {
        /// Bus to mutate.
        bus_id: BusId,
        /// New output target.
        output: BusOutput,
    },
    /// GOAT-4 — route a track's main output (master or a bus).
    RouteTrackToBus {
        /// Track to mutate.
        track_id: TrackId,
        /// New output target.
        output: BusOutput,
    },
    /// GOAT-4 — add an aux/reverb send from a track to a bus.
    AddTrackSend {
        /// Source track.
        track_id: TrackId,
        /// The send (target bus + level + pre/post fader).
        send: Send,
    },
    /// GOAT-4 — remove a track's send at `index` (no-op if out of range).
    RemoveTrackSend {
        /// Source track.
        track_id: TrackId,
        /// Zero-based index into `Track.sends`.
        index: usize,
    },
    /// GOAT-4 — append an insert effect to a bus's chain.
    AddBusInsert {
        /// Bus to mutate.
        bus_id: BusId,
        /// Effect spec to append.
        effect: crate::audio_effect::AudioEffectSpec,
    },
    /// GOAT-4 — remove a bus insert at `index` (no-op if out of range).
    RemoveBusInsert {
        /// Bus to mutate.
        bus_id: BusId,
        /// Zero-based index into `AudioBus.inserts`.
        index: usize,
    },
    /// GOAT-4 — reorder a bus insert within its chain.
    MoveBusInsert {
        /// Bus to mutate.
        bus_id: BusId,
        /// Source index.
        from_index: usize,
        /// Destination index.
        to_index: usize,
    },
    /// GOAT-4 — replace a bus insert's spec at `index` (coarse param update).
    UpdateBusInsert {
        /// Bus to mutate.
        bus_id: BusId,
        /// Zero-based index into `AudioBus.inserts`.
        index: usize,
        /// Full replacement spec.
        effect: crate::audio_effect::AudioEffectSpec,
    },
    /// GOAT-4 — add an aux send from a bus to another bus.
    AddBusSend {
        /// Source bus.
        bus_id: BusId,
        /// The send (target bus + level + pre/post fader).
        send: Send,
    },
    /// GOAT-4 — remove a bus's send at `index` (no-op if out of range).
    RemoveBusSend {
        /// Source bus.
        bus_id: BusId,
        /// Zero-based index into `AudioBus.sends`.
        index: usize,
    },
    /// GOAT-6 — connect `connected` to `anchor` (FCPX-style attachment).
    ConnectClip {
        /// The driving clip.
        anchor: ClipId,
        /// The clip that follows it.
        connected: ClipId,
    },
    /// GOAT-6 — remove the connection whose `connected` clip is this one.
    DisconnectClip {
        /// The follower clip to detach.
        connected: ClipId,
    },
    /// GOAT-6 — toggle the sequence's magnetic-timeline mode.
    SetMagnetic {
        /// New mode.
        magnetic: bool,
    },
    /// GOAT-6 — move a clip + its connected subtree together.
    MagneticMoveClip {
        /// Track of the dragged clip.
        track_id: TrackId,
        /// The dragged clip.
        clip_id: ClipId,
        /// New timeline-in for the dragged clip (the group shifts by the
        /// same delta).
        new_timeline_in: RationalTime,
    },
    /// GOAT-6 (Cut page) — append a clip at the track's gapless end.
    AppendClip {
        /// Destination track.
        track_id: TrackId,
        /// The clip to append (its `timeline_in` is ignored).
        clip: Clip,
    },
    /// GOAT-6 — ripple-delete a clip, closing the gap across all tracks.
    MagneticRippleDelete {
        /// Track of the clip.
        track_id: TrackId,
        /// The clip to delete.
        clip_id: ClipId,
    },
    /// GOAT-6 — ripple-insert a clip, opening space across all tracks.
    MagneticRippleInsert {
        /// Destination track.
        track_id: TrackId,
        /// The clip (placed at its `timeline_in`).
        clip: Clip,
    },
    /// GOAT-7 — add a caption/subtitle cue.
    AddCaption {
        /// The cue (placed by start time).
        cue: vance_captions::Cue,
    },
    /// GOAT-7 — remove a caption cue.
    RemoveCaption {
        /// Cue id.
        cue_id: vance_captions::CueId,
    },
    /// GOAT-7 — edit a caption cue's text.
    SetCaptionText {
        /// Cue id.
        cue_id: vance_captions::CueId,
        /// New text.
        text: String,
    },
    /// GOAT-7 — set a caption cue's `[start_ms, end_ms)`.
    SetCaptionTiming {
        /// Cue id.
        cue_id: vance_captions::CueId,
        /// New start (ms).
        start_ms: i64,
        /// New end (ms).
        end_ms: i64,
    },
    /// GOAT-7 — split a caption cue at `at_ms`.
    SplitCaption {
        /// Cue id.
        cue_id: vance_captions::CueId,
        /// Split time (ms).
        at_ms: i64,
    },
    /// GOAT-7 — merge a caption cue with the cue that follows it.
    MergeCaptionWithNext {
        /// Cue id.
        cue_id: vance_captions::CueId,
    },
    /// GOAT-7 — shift every caption cue by `delta_ms`.
    ShiftAllCaptions {
        /// Signed shift (ms).
        delta_ms: i64,
    },
    /// GOAT-7 — replace the whole caption list (import / transcription).
    SetCaptions {
        /// New cue list (will be sorted).
        cues: Vec<vance_captions::Cue>,
    },
    /// GOAT-gated (collaboration) — add a review comment.
    AddReviewComment {
        /// The comment (placed by timeline position).
        comment: crate::review::ReviewComment,
    },
    /// Remove a review comment.
    RemoveReviewComment {
        /// Comment id.
        comment_id: crate::id::CommentId,
    },
    /// Set a review comment's resolved flag.
    ResolveReviewComment {
        /// Comment id.
        comment_id: crate::id::CommentId,
        /// Resolved state.
        resolved: bool,
    },
    /// Edit a review comment's body text.
    SetReviewCommentText {
        /// Comment id.
        comment_id: crate::id::CommentId,
        /// New text.
        text: String,
    },
    /// Append a reply to a review comment's thread.
    ReplyReviewComment {
        /// Comment id.
        comment_id: crate::id::CommentId,
        /// The reply.
        reply: crate::review::ReviewReply,
    },
}

/// Apply a single op to a sequence, returning the new sequence.
pub fn apply(seq: &Sequence, op: &EditOp) -> Result<Sequence> {
    match op {
        EditOp::AddTrack {
            track_id,
            kind,
            name,
        } => Ok(ops::add_track_with_id(seq, *track_id, *kind, name.clone())),
        EditOp::RemoveTrack { track_id } => ops::remove_track(seq, *track_id),
        EditOp::InsertClip { track_id, clip } => ops::insert_clip(seq, *track_id, clip.clone()),
        EditOp::RippleInsertClip { track_id, clip } => {
            ops::ripple_insert_clip(seq, *track_id, clip.clone())
        }
        EditOp::OverwriteClip { track_id, clip } => {
            ops::overwrite_clip(seq, *track_id, clip.clone())
        }
        EditOp::RemoveClip { track_id, clip_id } => ops::remove_clip(seq, *track_id, *clip_id),
        EditOp::RippleRemoveClip { track_id, clip_id } => {
            ops::ripple_remove_clip(seq, *track_id, *clip_id)
        }
        EditOp::Slip {
            track_id,
            clip_id,
            delta,
        } => ops::slip(seq, *track_id, *clip_id, *delta),
        EditOp::Slide {
            track_id,
            clip_id,
            delta,
        } => ops::slide(seq, *track_id, *clip_id, *delta),
        EditOp::TrimIn {
            track_id,
            clip_id,
            new_source_in,
        } => ops::trim_in(seq, *track_id, *clip_id, *new_source_in),
        EditOp::TrimOut {
            track_id,
            clip_id,
            new_source_out,
        } => ops::trim_out(seq, *track_id, *clip_id, *new_source_out),
        EditOp::Split {
            track_id,
            clip_id,
            at_timeline,
        } => ops::split(seq, *track_id, *clip_id, *at_timeline),
        EditOp::AddEffectToClip {
            track_id,
            clip_id,
            effect,
        } => ops::add_effect_to_clip(seq, *track_id, *clip_id, effect.clone()),
        EditOp::RemoveEffectFromClip {
            track_id,
            clip_id,
            effect_id,
        } => ops::remove_effect_from_clip(seq, *track_id, *clip_id, *effect_id),
        EditOp::UpdateEffectParam {
            track_id,
            clip_id,
            effect_id,
            key,
            value,
        } => ops::update_effect_param(
            seq,
            *track_id,
            *clip_id,
            *effect_id,
            key.clone(),
            value.clone(),
        ),
        EditOp::AddTransitionAtCut {
            track_id,
            transition_id,
            after_clip_id,
            kind,
            duration,
        } => ops::add_transition_at_cut_with_id(
            seq,
            *track_id,
            *transition_id,
            *after_clip_id,
            kind.clone(),
            *duration,
        ),
        EditOp::RemoveTransition {
            track_id,
            transition_id,
        } => ops::remove_transition(seq, *track_id, *transition_id),
        EditOp::SetTransitionParam {
            track_id,
            transition_id,
            key,
            value,
        } => ops::set_transition_param(seq, *track_id, *transition_id, key.clone(), value.clone()),
        EditOp::MoveEffectOnClip {
            track_id,
            clip_id,
            effect_id,
            new_index,
        } => ops::move_effect_on_clip(seq, *track_id, *clip_id, *effect_id, *new_index),
        EditOp::SetEffectEnabled {
            track_id,
            clip_id,
            effect_id,
            enabled,
        } => ops::set_effect_enabled(seq, *track_id, *clip_id, *effect_id, *enabled),
        EditOp::SetTrackMute { track_id, mute } => ops::set_track_mute(seq, *track_id, *mute),
        EditOp::SetTrackSolo { track_id, solo } => ops::set_track_solo(seq, *track_id, *solo),
        EditOp::SetTrackGainDb { track_id, gain_db } => {
            ops::set_track_gain_db(seq, *track_id, *gain_db)
        }
        EditOp::SetClipMute {
            track_id,
            clip_id,
            mute,
        } => ops::set_clip_mute(seq, *track_id, *clip_id, *mute),
        EditOp::SetClipGainDb {
            track_id,
            clip_id,
            gain_db,
        } => ops::set_clip_gain_db(seq, *track_id, *clip_id, *gain_db),
        EditOp::SetMasterGainDb { gain_db } => ops::set_master_gain_db(seq, *gain_db),
        EditOp::SetTrackPan { track_id, pan } => ops::set_track_pan(seq, *track_id, *pan),
        EditOp::SetClipPan {
            track_id,
            clip_id,
            pan,
        } => ops::set_clip_pan(seq, *track_id, *clip_id, *pan),
        EditOp::AddAudioEffect { track_id, effect } => {
            ops::add_audio_effect(seq, *track_id, effect.clone())
        }
        EditOp::RemoveAudioEffect { track_id, index } => {
            ops::remove_audio_effect(seq, *track_id, *index)
        }
        EditOp::MoveAudioEffect {
            track_id,
            from_index,
            to_index,
        } => ops::move_audio_effect(seq, *track_id, *from_index, *to_index),
        EditOp::UpdateAudioEffect {
            track_id,
            index,
            effect,
        } => ops::update_audio_effect(seq, *track_id, *index, effect.clone()),
        EditOp::SetTrackGainKeyframes {
            track_id,
            keyframes,
        } => ops::set_track_gain_keyframes(seq, *track_id, keyframes.clone()),
        EditOp::SetClipGainKeyframes {
            track_id,
            clip_id,
            keyframes,
        } => ops::set_clip_gain_keyframes(seq, *track_id, *clip_id, keyframes.clone()),
        EditOp::SetClipFadeIn {
            track_id,
            clip_id,
            duration_seconds,
            curve,
        } => ops::set_clip_fade_in(seq, *track_id, *clip_id, *duration_seconds, *curve),
        EditOp::SetClipFadeOut {
            track_id,
            clip_id,
            duration_seconds,
            curve,
        } => ops::set_clip_fade_out(seq, *track_id, *clip_id, *duration_seconds, *curve),
        EditOp::UpdateEffectParams {
            track_id,
            clip_id,
            effect_id,
            params,
        } => ops::update_effect_params(seq, *track_id, *clip_id, *effect_id, params),
        EditOp::Compound { ops: children, .. } => {
            // Fold-left over children. If any child fails the whole
            // compound returns Err; the EditLog never records it, so
            // the original snapshot remains current = atomic rollback.
            let mut cur = seq.clone();
            for child in children {
                cur = apply(&cur, child)?;
            }
            Ok(cur)
        }
        EditOp::AddBus { bus_id, name } => Ok(ops::add_bus_with_id(seq, *bus_id, name.clone())),
        EditOp::RemoveBus { bus_id } => ops::remove_bus(seq, *bus_id),
        EditOp::SetBusGainDb { bus_id, gain_db } => ops::set_bus_gain_db(seq, *bus_id, *gain_db),
        EditOp::SetBusPan { bus_id, pan } => ops::set_bus_pan(seq, *bus_id, *pan),
        EditOp::SetBusMute { bus_id, mute } => ops::set_bus_mute(seq, *bus_id, *mute),
        EditOp::SetBusOutput { bus_id, output } => ops::set_bus_output(seq, *bus_id, *output),
        EditOp::RouteTrackToBus { track_id, output } => {
            ops::route_track_to_bus(seq, *track_id, *output)
        }
        EditOp::AddTrackSend { track_id, send } => ops::add_track_send(seq, *track_id, *send),
        EditOp::RemoveTrackSend { track_id, index } => {
            ops::remove_track_send(seq, *track_id, *index)
        }
        EditOp::AddBusInsert { bus_id, effect } => {
            ops::add_bus_insert(seq, *bus_id, effect.clone())
        }
        EditOp::RemoveBusInsert { bus_id, index } => ops::remove_bus_insert(seq, *bus_id, *index),
        EditOp::MoveBusInsert {
            bus_id,
            from_index,
            to_index,
        } => ops::move_bus_insert(seq, *bus_id, *from_index, *to_index),
        EditOp::UpdateBusInsert {
            bus_id,
            index,
            effect,
        } => ops::update_bus_insert(seq, *bus_id, *index, effect.clone()),
        EditOp::AddBusSend { bus_id, send } => ops::add_bus_send(seq, *bus_id, *send),
        EditOp::RemoveBusSend { bus_id, index } => ops::remove_bus_send(seq, *bus_id, *index),
        EditOp::ConnectClip { anchor, connected } => ops::connect_clip(seq, *anchor, *connected),
        EditOp::DisconnectClip { connected } => ops::disconnect_clip(seq, *connected),
        EditOp::SetMagnetic { magnetic } => Ok(ops::set_magnetic(seq, *magnetic)),
        EditOp::MagneticMoveClip {
            track_id,
            clip_id,
            new_timeline_in,
        } => ops::magnetic_move_clip(seq, *track_id, *clip_id, *new_timeline_in),
        EditOp::AppendClip { track_id, clip } => ops::append_clip(seq, *track_id, clip.clone()),
        EditOp::MagneticRippleDelete { track_id, clip_id } => {
            ops::magnetic_ripple_delete(seq, *track_id, *clip_id)
        }
        EditOp::MagneticRippleInsert { track_id, clip } => {
            ops::magnetic_ripple_insert(seq, *track_id, clip.clone())
        }
        EditOp::AddCaption { cue } => Ok(ops::add_caption(seq, cue.clone())),
        EditOp::RemoveCaption { cue_id } => Ok(ops::remove_caption(seq, *cue_id)),
        EditOp::SetCaptionText { cue_id, text } => {
            Ok(ops::set_caption_text(seq, *cue_id, text.clone()))
        }
        EditOp::SetCaptionTiming {
            cue_id,
            start_ms,
            end_ms,
        } => Ok(ops::set_caption_timing(seq, *cue_id, *start_ms, *end_ms)),
        EditOp::SplitCaption { cue_id, at_ms } => Ok(ops::split_caption(seq, *cue_id, *at_ms)),
        EditOp::MergeCaptionWithNext { cue_id } => Ok(ops::merge_caption_with_next(seq, *cue_id)),
        EditOp::ShiftAllCaptions { delta_ms } => Ok(ops::shift_all_captions(seq, *delta_ms)),
        EditOp::SetCaptions { cues } => Ok(ops::set_captions(seq, cues.clone())),
        EditOp::AddReviewComment { comment } => Ok(ops::add_review_comment(seq, comment.clone())),
        EditOp::RemoveReviewComment { comment_id } => {
            Ok(ops::remove_review_comment(seq, *comment_id))
        }
        EditOp::ResolveReviewComment {
            comment_id,
            resolved,
        } => Ok(ops::resolve_review_comment(seq, *comment_id, *resolved)),
        EditOp::SetReviewCommentText { comment_id, text } => {
            Ok(ops::set_review_comment_text(seq, *comment_id, text.clone()))
        }
        EditOp::ReplyReviewComment { comment_id, reply } => {
            Ok(ops::reply_review_comment(seq, *comment_id, reply.clone()))
        }
    }
}

/// One node in the edit-history tree. Holds the `Sequence` snapshot
/// reached by applying `label`'s op to its parent. Snapshots are
/// Arc-shared internally, so keeping *every* node — including abandoned
/// branches — is cheap.
#[derive(Clone, Debug)]
struct HistoryNode {
    /// The sequence at this point in history.
    snapshot: Sequence,
    /// Parent node index. `None` only for the root.
    parent: Option<usize>,
    /// Child node indices in creation order. Older siblings stay reachable
    /// as alternate branches via `goto`.
    children: Vec<usize>,
    /// The child `redo()` follows from this node — the branch the user most
    /// recently descended (updated by apply/undo/goto), NOT just the
    /// newest child. Keeps redo-after-undo returning to the branch the user
    /// is actually on, even after `goto`-ing onto an older branch.
    active_child: Option<usize>,
    /// Human-readable label of the op that produced this node ("" = root).
    label: String,
    /// Optional user-assigned checkpoint / branch name.
    name: Option<String>,
}

/// A read-only view of one history node, for the UI's history/branch panel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HistoryNodeView {
    /// Stable node id (index). Pass to [`EditLog::goto`] to jump there.
    pub id: usize,
    /// Parent node id, or `None` for the root.
    pub parent: Option<usize>,
    /// Child node ids (>1 means this node is a branch point).
    pub children: Vec<usize>,
    /// Op label that produced this node.
    pub label: String,
    /// User checkpoint/branch name, if assigned.
    pub name: Option<String>,
    /// Whether this is the node currently being edited.
    pub is_current: bool,
}

/// Hard cap on retained history nodes (across all branches). On overflow
/// the log collapses to a fresh single-node history seeded at the current
/// snapshot — the user keeps their current state but loses undo history.
/// Generous enough that a normal session never hits it; a safety valve
/// against unbounded memory growth over a marathon branchy session.
const MAX_HISTORY_NODES: usize = 4096;

/// Branching undo/redo history for one `Sequence` — "Git for edits".
///
/// Internally a tree of `Sequence` snapshots. Linear undo/redo walk the
/// parent / active-child edge (so they behave exactly like a flat
/// history), but a new edit made after undoing forks a NEW branch
/// instead of discarding the redo branch — alternate edit paths are kept
/// and reachable via [`EditLog::goto`] / [`EditLog::branch_tips`].
/// Snapshots share most structure via `Arc`, but each node still owns a
/// fresh `tracks`/`markers` Vec, so retained history grows with edit count
/// (bounded by [`MAX_HISTORY_NODES`]).
///
/// NOT PERSISTED: the log is session-only — `vance-project::save` only
/// writes the current `Project` (which contains the current `Sequence`),
/// and `Engine::load_project` reseeds a fresh `EditLog` from that
/// baseline. Users cannot undo across a project reload. Persisting the
/// history is a separable migrations follow-up.
#[derive(Clone, Debug)]
pub struct EditLog {
    nodes: Vec<HistoryNode>,
    current: usize,
}

impl EditLog {
    /// Start a fresh log around `initial` (becomes the root node).
    pub fn new(initial: Sequence) -> Self {
        Self {
            nodes: vec![HistoryNode {
                snapshot: initial,
                parent: None,
                children: Vec::new(),
                active_child: None,
                label: String::new(),
                name: None,
            }],
            current: 0,
        }
    }

    /// Borrow the current sequence.
    pub fn current(&self) -> &Sequence {
        &self.nodes[self.current].snapshot
    }

    /// Apply `op`, forking a new history node from the current one.
    ///
    /// If the current node already has children (the user undid and is now
    /// editing from mid-history), this creates a NEW branch rather than
    /// truncating the existing one — the old branch stays reachable.
    pub fn apply(&mut self, op: EditOp) -> Result<&Sequence> {
        let after = {
            let cur = &self.nodes[self.current];
            apply(&cur.snapshot, &op)?
        };
        // Safety valve: collapse to a fresh history at the current snapshot
        // before exceeding the node cap, bounding memory.
        if self.nodes.len() >= MAX_HISTORY_NODES {
            let snap = self.nodes[self.current].snapshot.clone();
            *self = EditLog::new(snap);
        }
        let new_idx = self.nodes.len();
        self.nodes.push(HistoryNode {
            snapshot: after,
            parent: Some(self.current),
            children: Vec::new(),
            active_child: None,
            label: op_label(&op),
            name: None,
        });
        self.nodes[self.current].children.push(new_idx);
        // The new edit is the branch this node now redoes into.
        self.nodes[self.current].active_child = Some(new_idx);
        self.current = new_idx;
        Ok(&self.nodes[self.current].snapshot)
    }

    /// Step back to the parent node. `None` at the root.
    pub fn undo(&mut self) -> Option<&Sequence> {
        let parent = self.nodes[self.current].parent?;
        // Remember the branch we came down so a later redo returns HERE,
        // not to whichever sibling happened to be created last.
        let from = self.current;
        self.nodes[parent].active_child = Some(from);
        self.current = parent;
        Some(&self.nodes[self.current].snapshot)
    }

    /// Step forward to the active child (the branch the user last
    /// descended). `None` at a leaf.
    pub fn redo(&mut self) -> Option<&Sequence> {
        let node = &self.nodes[self.current];
        let child = node
            .active_child
            .or_else(|| node.children.last().copied())?;
        self.current = child;
        Some(&self.nodes[self.current].snapshot)
    }

    /// `true` if `undo()` would have an effect.
    pub fn can_undo(&self) -> bool {
        self.nodes[self.current].parent.is_some()
    }

    /// `true` if `redo()` would have an effect.
    pub fn can_redo(&self) -> bool {
        !self.nodes[self.current].children.is_empty()
    }

    /// Number of edits recorded (every node except the root, across all
    /// branches).
    pub fn len(&self) -> usize {
        self.nodes.len() - 1
    }

    /// `true` if no edit has been recorded yet (only the root exists).
    pub fn is_empty(&self) -> bool {
        self.nodes.len() == 1
    }

    /// The id of the node currently being edited.
    pub fn current_id(&self) -> usize {
        self.current
    }

    /// Tag the current node with a checkpoint / branch name (e.g. "v2 —
    /// punchier open"). Replaces any existing name on that node.
    pub fn checkpoint(&mut self, name: impl Into<String>) {
        self.nodes[self.current].name = Some(name.into());
    }

    /// Jump to any node by id — the core "switch branch / restore
    /// version" operation. Returns the sequence there, or `None` if the
    /// id is out of range.
    pub fn goto(&mut self, id: usize) -> Option<&Sequence> {
        if id >= self.nodes.len() {
            return None;
        }
        self.current = id;
        // Mark the whole root→id path active, so undo/redo from here walk
        // the branch the user jumped onto rather than diverting to a
        // sibling that happened to be created later.
        let mut node = id;
        while let Some(parent) = self.nodes[node].parent {
            self.nodes[parent].active_child = Some(node);
            node = parent;
        }
        Some(&self.nodes[self.current].snapshot)
    }

    /// Snapshot of the whole history tree for the UI, in node-id order.
    pub fn history(&self) -> Vec<HistoryNodeView> {
        self.nodes
            .iter()
            .enumerate()
            .map(|(id, n)| HistoryNodeView {
                id,
                parent: n.parent,
                children: n.children.clone(),
                label: n.label.clone(),
                name: n.name.clone(),
                is_current: id == self.current,
            })
            .collect()
    }

    /// Leaf node ids — the distinct branch endings ("alternate cuts").
    pub fn branch_tips(&self) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.children.is_empty())
            .map(|(id, _)| id)
            .collect()
    }

    /// Named checkpoints, as `(node_id, name)`, in node-id order.
    pub fn checkpoints(&self) -> Vec<(usize, String)> {
        self.nodes
            .iter()
            .enumerate()
            .filter_map(|(id, n)| n.name.clone().map(|name| (id, name)))
            .collect()
    }
}

/// Short human label for an op, shown in the history panel.
fn op_label(op: &EditOp) -> String {
    match op {
        EditOp::AddTrack { .. } => "Add track".into(),
        EditOp::RemoveTrack { .. } => "Remove track".into(),
        EditOp::InsertClip { .. } => "Insert clip".into(),
        EditOp::RippleInsertClip { .. } => "Ripple insert".into(),
        EditOp::OverwriteClip { .. } => "Overwrite clip".into(),
        EditOp::RemoveClip { .. } => "Remove clip".into(),
        EditOp::RippleRemoveClip { .. } => "Ripple delete".into(),
        EditOp::Slip { .. } => "Slip".into(),
        EditOp::Slide { .. } => "Slide".into(),
        EditOp::TrimIn { .. } => "Trim in".into(),
        EditOp::TrimOut { .. } => "Trim out".into(),
        EditOp::Split { .. } => "Split".into(),
        EditOp::AddEffectToClip { .. } => "Add effect".into(),
        EditOp::RemoveEffectFromClip { .. } => "Remove effect".into(),
        EditOp::UpdateEffectParam { .. } | EditOp::UpdateEffectParams { .. } => {
            "Adjust effect".into()
        }
        EditOp::AddTransitionAtCut { .. } => "Add transition".into(),
        EditOp::RemoveTransition { .. } => "Remove transition".into(),
        EditOp::SetTransitionParam { .. } => "Adjust transition".into(),
        EditOp::MoveEffectOnClip { .. } => "Reorder effect".into(),
        EditOp::SetEffectEnabled { .. } => "Toggle effect".into(),
        EditOp::SetTrackMute { .. } => "Track mute".into(),
        EditOp::SetTrackSolo { .. } => "Track solo".into(),
        EditOp::SetTrackGainDb { .. } | EditOp::SetTrackGainKeyframes { .. } => "Track gain".into(),
        EditOp::SetClipMute { .. } => "Clip mute".into(),
        EditOp::SetClipGainDb { .. } | EditOp::SetClipGainKeyframes { .. } => "Clip gain".into(),
        EditOp::SetMasterGainDb { .. } => "Master gain".into(),
        EditOp::SetTrackPan { .. } => "Track pan".into(),
        EditOp::SetClipPan { .. } => "Clip pan".into(),
        EditOp::AddAudioEffect { .. } => "Add audio effect".into(),
        EditOp::RemoveAudioEffect { .. } => "Remove audio effect".into(),
        EditOp::MoveAudioEffect { .. } => "Reorder audio effect".into(),
        EditOp::UpdateAudioEffect { .. } => "Adjust audio effect".into(),
        EditOp::SetClipFadeIn { .. } => "Fade in".into(),
        EditOp::SetClipFadeOut { .. } => "Fade out".into(),
        EditOp::Compound { label, .. } => label.clone(),
        EditOp::AddBus { .. } => "Add bus".into(),
        EditOp::RemoveBus { .. } => "Remove bus".into(),
        EditOp::SetBusGainDb { .. } => "Bus gain".into(),
        EditOp::SetBusPan { .. } => "Bus pan".into(),
        EditOp::SetBusMute { .. } => "Bus mute".into(),
        EditOp::SetBusOutput { .. } | EditOp::RouteTrackToBus { .. } => "Reroute".into(),
        EditOp::AddTrackSend { .. } => "Add send".into(),
        EditOp::RemoveTrackSend { .. } => "Remove send".into(),
        EditOp::AddBusInsert { .. } => "Add bus insert".into(),
        EditOp::RemoveBusInsert { .. } => "Remove bus insert".into(),
        EditOp::MoveBusInsert { .. } => "Reorder bus insert".into(),
        EditOp::UpdateBusInsert { .. } => "Adjust bus insert".into(),
        EditOp::AddBusSend { .. } => "Add bus send".into(),
        EditOp::RemoveBusSend { .. } => "Remove bus send".into(),
        EditOp::ConnectClip { .. } => "Connect clip".into(),
        EditOp::DisconnectClip { .. } => "Disconnect clip".into(),
        EditOp::SetMagnetic { magnetic } => {
            if *magnetic {
                "Enable magnetic timeline".into()
            } else {
                "Disable magnetic timeline".into()
            }
        }
        EditOp::MagneticMoveClip { .. } => "Move (magnetic)".into(),
        EditOp::AppendClip { .. } => "Append clip".into(),
        EditOp::MagneticRippleDelete { .. } => "Ripple delete (all tracks)".into(),
        EditOp::MagneticRippleInsert { .. } => "Ripple insert (all tracks)".into(),
        EditOp::AddCaption { .. } => "Add caption".into(),
        EditOp::RemoveCaption { .. } => "Remove caption".into(),
        EditOp::SetCaptionText { .. } => "Edit caption".into(),
        EditOp::SetCaptionTiming { .. } => "Retime caption".into(),
        EditOp::SplitCaption { .. } => "Split caption".into(),
        EditOp::MergeCaptionWithNext { .. } => "Merge caption".into(),
        EditOp::ShiftAllCaptions { .. } => "Shift captions".into(),
        EditOp::SetCaptions { .. } => "Import captions".into(),
        EditOp::AddReviewComment { .. } => "Add review comment".into(),
        EditOp::RemoveReviewComment { .. } => "Remove review comment".into(),
        EditOp::ResolveReviewComment { .. } => "Resolve review comment".into(),
        EditOp::SetReviewCommentText { .. } => "Edit review comment".into(),
        EditOp::ReplyReviewComment { .. } => "Reply to review comment".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::MediaItemId;
    use crate::sequence::Sequence;
    use vance_core::FrameRate;

    fn t(num: i64) -> RationalTime {
        RationalTime::new(num, 1).unwrap()
    }

    fn seed_log() -> (EditLog, TrackId, MediaItemId) {
        let seq = Sequence::new("Seq", FrameRate::FPS_24);
        let mut log = EditLog::new(seq);
        let tid = TrackId::new();
        log.apply(EditOp::AddTrack {
            track_id: tid,
            kind: TrackKind::Video,
            name: "V1".into(),
        })
        .unwrap();
        let media = MediaItemId::new();
        (log, tid, media)
    }

    #[test]
    fn apply_advances_cursor_and_updates_current() {
        let (mut log, tid, media) = seed_log();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip,
        })
        .unwrap();
        assert_eq!(log.len(), 2);
        assert!(log.can_undo());
        assert!(!log.can_redo());
        assert_eq!(log.current().tracks[0].clips.len(), 1);
    }

    #[test]
    fn undo_restores_previous_state() {
        let (mut log, tid, media) = seed_log();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip,
        })
        .unwrap();
        let snap_after = log.current().clone();
        log.undo().expect("undo should succeed");
        assert_eq!(log.current().tracks[0].clips.len(), 0);
        assert!(log.can_redo());
        log.redo().expect("redo should succeed");
        // Pointer-equal Arc<Track> on redo means we restored the exact
        // post-edit state, not a reapplied clone.
        assert!(std::sync::Arc::ptr_eq(
            &log.current().tracks[0],
            &snap_after.tracks[0],
        ));
    }

    #[test]
    fn new_apply_mid_history_forks_instead_of_truncating() {
        let (mut log, tid, media) = seed_log();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let b = Clip::new(media, "b", t(5), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip: a,
        })
        .unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip: b,
        })
        .unwrap();
        let old_tip = log.current_id();
        // Step back twice, then apply a different op.
        log.undo();
        log.undo();
        assert!(log.can_redo());
        let c = Clip::new(media, "c", t(0), t(0), t(2)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip: c,
        })
        .unwrap();
        // The current leaf has no redo of its own...
        assert!(!log.can_redo());
        assert_eq!(log.current().tracks[0].clips.len(), 1);
        // ...but the old a→b branch is NOT discarded — it survives as a
        // reachable tip ("Git for edits").
        assert!(log.branch_tips().contains(&old_tip));
        log.goto(old_tip).unwrap();
        assert_eq!(log.current().tracks[0].clips.len(), 2);
    }

    #[test]
    fn fork_preserves_both_branches() {
        let (mut log, tid, media) = seed_log();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip: a,
        })
        .unwrap();
        let a_tip = log.current_id();
        assert_eq!(log.current().tracks[0].clips.len(), 1);

        // Undo to the track node, then take a different path (two inserts).
        log.undo().unwrap();
        let b1 = Clip::new(media, "b1", t(0), t(0), t(5)).unwrap();
        let b2 = Clip::new(media, "b2", t(10), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip: b1,
        })
        .unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip: b2,
        })
        .unwrap();
        let b_tip = log.current_id();
        assert_eq!(log.current().tracks[0].clips.len(), 2);

        let tips = log.branch_tips();
        assert!(tips.contains(&a_tip), "branch A tip lost");
        assert!(tips.contains(&b_tip), "branch B tip lost");

        // Jump back to branch A — its single-clip edit is intact...
        log.goto(a_tip).unwrap();
        assert_eq!(log.current().tracks[0].clips.len(), 1);
        // ...and forward to branch B again.
        log.goto(b_tip).unwrap();
        assert_eq!(log.current().tracks[0].clips.len(), 2);
    }

    #[test]
    fn redo_follows_most_recent_branch() {
        let (mut log, tid, media) = seed_log();
        let branch_point = log.current_id();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip: a,
        })
        .unwrap();
        log.undo().unwrap();
        let b = Clip::new(media, "b", t(0), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip: b,
        })
        .unwrap();
        let b_node = log.current_id();
        // From the branch point, linear redo follows the newest child (B).
        log.goto(branch_point).unwrap();
        assert!(log.can_redo());
        log.redo().unwrap();
        assert_eq!(log.current_id(), b_node);
    }

    #[test]
    fn redo_returns_to_the_branch_you_visited_not_newest_sibling() {
        // Regression: after goto-ing onto an OLDER branch and undoing,
        // redo must return to that branch, not divert to the sibling that
        // happened to be created last.
        let (mut log, tid, media) = seed_log();
        let branch_point = log.current_id();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip: a,
        })
        .unwrap();
        let a_node = log.current_id();
        log.undo().unwrap();
        let b = Clip::new(media, "b", t(0), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip: b,
        })
        .unwrap();
        let b_node = log.current_id();
        assert_ne!(a_node, b_node);

        // Visit branch A (the older one), then undo to the branch point.
        log.goto(a_node).unwrap();
        log.undo().unwrap();
        assert_eq!(log.current_id(), branch_point);
        // Redo must land on A (where the user just was), not newest B.
        log.redo().unwrap();
        assert_eq!(
            log.current_id(),
            a_node,
            "redo diverted to the newest sibling instead of the visited branch"
        );
    }

    #[test]
    fn checkpoint_names_node_and_is_listed() {
        let (mut log, tid, media) = seed_log();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip: a,
        })
        .unwrap();
        log.checkpoint("rough cut");
        let id = log.current_id();
        assert_eq!(log.checkpoints(), vec![(id, "rough cut".to_string())]);
        let h = log.history();
        let node = h.iter().find(|n| n.id == id).expect("node in history");
        assert_eq!(node.name.as_deref(), Some("rough cut"));
        assert!(node.is_current);
        assert!(node.parent.is_some(), "an edit node has a parent");
    }

    #[test]
    fn goto_out_of_range_is_none() {
        let (mut log, _tid, _media) = seed_log();
        assert!(log.goto(999).is_none());
    }

    #[test]
    fn undo_redo_round_trips_through_many_edits() {
        let (mut log, tid, media) = seed_log();
        for i in 0..5 {
            let c = Clip::new(media, format!("c{i}"), t(i * 5), t(0), t(5)).unwrap();
            log.apply(EditOp::InsertClip {
                track_id: tid,
                clip: c,
            })
            .unwrap();
        }
        assert_eq!(log.current().tracks[0].clips.len(), 5);
        for _ in 0..5 {
            assert!(log.undo().is_some());
        }
        assert_eq!(log.current().tracks[0].clips.len(), 0);
        for _ in 0..5 {
            assert!(log.redo().is_some());
        }
        assert_eq!(log.current().tracks[0].clips.len(), 5);
    }

    #[test]
    fn undo_at_base_returns_none() {
        let log_init = Sequence::new("Seq", FrameRate::FPS_24);
        let mut log = EditLog::new(log_init);
        assert!(!log.can_undo());
        assert!(log.undo().is_none());
    }

    #[test]
    fn grade_node_ops_undo_redo_round_trip() {
        use crate::effect::Effect;
        let (mut log, tid, media) = seed_log();
        let clip = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let cid = clip.id;
        log.apply(EditOp::InsertClip {
            track_id: tid,
            clip,
        })
        .unwrap();
        // Two grade nodes with caller-allocated IDs (byte-identical replay).
        let e0 = Effect::new("grade");
        let e1 = Effect::new("grade");
        let (id0, id1) = (e0.id, e1.id);
        log.apply(EditOp::AddEffectToClip {
            track_id: tid,
            clip_id: cid,
            effect: e0,
        })
        .unwrap();
        log.apply(EditOp::AddEffectToClip {
            track_id: tid,
            clip_id: cid,
            effect: e1,
        })
        .unwrap();
        // Reorder: move id1 to front, then bypass id0.
        log.apply(EditOp::MoveEffectOnClip {
            track_id: tid,
            clip_id: cid,
            effect_id: id1,
            new_index: 0,
        })
        .unwrap();
        log.apply(EditOp::SetEffectEnabled {
            track_id: tid,
            clip_id: cid,
            effect_id: id0,
            enabled: false,
        })
        .unwrap();

        let order = |log: &EditLog| -> Vec<crate::id::EffectId> {
            log.current().tracks[0].clips[0]
                .effects
                .iter()
                .map(|e| e.id)
                .collect()
        };
        let enabled0 = |log: &EditLog| -> bool {
            log.current().tracks[0].clips[0]
                .effects
                .iter()
                .find(|e| e.id == id0)
                .unwrap()
                .enabled
        };

        // Final state: id1 first, id0 bypassed.
        assert_eq!(order(&log), vec![id1, id0]);
        assert!(!enabled0(&log));

        // Undo the bypass → id0 enabled again, order unchanged.
        log.undo().unwrap();
        assert!(enabled0(&log));
        assert_eq!(order(&log), vec![id1, id0]);

        // Undo the reorder → original order.
        log.undo().unwrap();
        assert_eq!(order(&log), vec![id0, id1]);

        // Redo both → back to final state.
        log.redo().unwrap();
        log.redo().unwrap();
        assert_eq!(order(&log), vec![id1, id0]);
        assert!(!enabled0(&log));
    }

    #[test]
    fn compound_applies_as_one_undo_step() {
        // Two child inserts via a single Compound -> one EditLog entry,
        // one undo reverts the whole batch.
        let (mut log, tid, media) = seed_log();
        assert_eq!(log.len(), 1);
        let c0 = Clip::new(media, "c0", t(0), t(0), t(5)).unwrap();
        let c1 = Clip::new(media, "c1", t(10), t(0), t(5)).unwrap();
        log.apply(EditOp::Compound {
            label: "ai: insert two clips".into(),
            ops: vec![
                EditOp::InsertClip {
                    track_id: tid,
                    clip: c0,
                },
                EditOp::InsertClip {
                    track_id: tid,
                    clip: c1,
                },
            ],
        })
        .unwrap();
        // ONE new entry (not two).
        assert_eq!(log.len(), 2);
        assert_eq!(log.current().tracks[0].clips.len(), 2);
        log.undo().unwrap();
        assert_eq!(log.current().tracks[0].clips.len(), 0);
        log.redo().unwrap();
        assert_eq!(log.current().tracks[0].clips.len(), 2);
    }

    #[test]
    fn compound_failure_rolls_back_atomically() {
        // First child succeeds, second fails (bad track id). Compound
        // returns Err and is NEVER recorded — the snapshot stays at
        // pre-compound state.
        let (mut log, tid, media) = seed_log();
        let before_len = log.len();
        let before_clip_count = log.current().tracks[0].clips.len();
        let c0 = Clip::new(media, "c0", t(0), t(0), t(5)).unwrap();
        let bogus_tid = TrackId::new();
        let result = log.apply(EditOp::Compound {
            label: "should rollback".into(),
            ops: vec![
                EditOp::InsertClip {
                    track_id: tid,
                    clip: c0,
                },
                // This one fails: track doesn't exist.
                EditOp::RemoveTrack {
                    track_id: bogus_tid,
                },
            ],
        });
        assert!(result.is_err(), "compound with failing child must error");
        assert_eq!(log.len(), before_len, "no entry recorded on failure");
        assert_eq!(
            log.current().tracks[0].clips.len(),
            before_clip_count,
            "state unchanged"
        );
    }

    #[test]
    fn empty_compound_is_a_noop() {
        let (mut log, _tid, _media) = seed_log();
        let before = log.current().clone();
        log.apply(EditOp::Compound {
            label: "empty".into(),
            ops: Vec::new(),
        })
        .unwrap();
        assert_eq!(
            log.current(),
            &before,
            "empty compound must not change state"
        );
    }
}
