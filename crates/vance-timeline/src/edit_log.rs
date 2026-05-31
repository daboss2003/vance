//! `EditOp` and `EditLog` — undo/redo for `Sequence` edits.
//!
//! ## Design
//!
//! - `EditOp` is a serializable enum over every edit operation in
//!   `crate::ops`. It captures all arguments verbatim, including any
//!   freshly allocated IDs (e.g. `AddTrack { track_id, ... }`), so a
//!   logged op replays to byte-identical output.
//! - `EditLog` keeps the active `Sequence` *plus* a flat `Vec` of past
//!   edits. Each `Entry` stores both `before` and `after` sequences; an
//!   undo is a pointer-swap to `before`, a redo a pointer-swap to
//!   `after`. Cloning a `Sequence` is cheap because all internal
//!   structure is `Arc`-shared, so this trades modest memory for the
//!   `<1ms` undo/redo target the plan calls for.
//! - Applying a new op while the cursor is mid-history truncates the
//!   redo branch — the standard NLE behavior.

use serde::{Deserialize, Serialize};
use vance_core::RationalTime;

use crate::clip::Clip;
use crate::effect::{Effect, EffectValue};
use crate::error::Result;
use crate::id::{ClipId, EffectId, TrackId, TransitionId};
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
            *kind,
            *duration,
        ),
        EditOp::RemoveTransition {
            track_id,
            transition_id,
        } => ops::remove_transition(seq, *track_id, *transition_id),
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
    }
}

/// One entry in an `EditLog`. Holds the op and the sequences it sat
/// between, both kept Arc-shared for cheap undo/redo swaps.
#[derive(Clone, Debug)]
struct Entry {
    /// Retained for future logging / status-bar display, even though
    /// the swap-based undo/redo doesn't need to inspect it.
    #[allow(dead_code)]
    op: EditOp,
    before: Sequence,
    after: Sequence,
}

/// Linear undo/redo history for one `Sequence`.
///
/// `current` is the sequence the caller is editing. `entries[..cursor]`
/// have been applied; `entries[cursor..]` are available for redo until
/// a fresh `apply` truncates them.
///
/// NOT PERSISTED: the log is session-only — `vance-project::save` only
/// writes the current `Project` (which contains the current `Sequence`),
/// and `Engine::load_project` reseeds a fresh `EditLog` from that
/// baseline. Users cannot undo across a project reload. Persisting the
/// history (with a Compound-aware serialization) is a separable Phase
/// 3.5 / migrations follow-up.
#[derive(Clone, Debug)]
pub struct EditLog {
    current: Sequence,
    entries: Vec<Entry>,
    cursor: usize,
}

impl EditLog {
    /// Start a fresh log around `initial`.
    pub fn new(initial: Sequence) -> Self {
        Self {
            current: initial,
            entries: Vec::new(),
            cursor: 0,
        }
    }

    /// Borrow the current sequence.
    pub fn current(&self) -> &Sequence {
        &self.current
    }

    /// Apply `op`, advance the cursor, and drop any redo branch.
    pub fn apply(&mut self, op: EditOp) -> Result<&Sequence> {
        let after = apply(&self.current, &op)?;
        // Truncate the redo branch — once the user makes a new edit
        // mid-history, the unapplied future is gone.
        self.entries.truncate(self.cursor);
        let before = std::mem::replace(&mut self.current, after.clone());
        self.entries.push(Entry { op, before, after });
        self.cursor = self.entries.len();
        Ok(&self.current)
    }

    /// Step back one edit. Returns the new current sequence, or `None`
    /// if there's nothing to undo.
    pub fn undo(&mut self) -> Option<&Sequence> {
        if self.cursor == 0 {
            return None;
        }
        self.cursor -= 1;
        self.current = self.entries[self.cursor].before.clone();
        Some(&self.current)
    }

    /// Step forward one edit. Returns the new current sequence, or
    /// `None` if there's nothing to redo.
    pub fn redo(&mut self) -> Option<&Sequence> {
        if self.cursor >= self.entries.len() {
            return None;
        }
        self.current = self.entries[self.cursor].after.clone();
        self.cursor += 1;
        Some(&self.current)
    }

    /// `true` if `undo()` would have an effect.
    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    /// `true` if `redo()` would have an effect.
    pub fn can_redo(&self) -> bool {
        self.cursor < self.entries.len()
    }

    /// Number of edits currently in the log (applied + redo branch).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// `true` if no edit has been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
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
    fn new_apply_truncates_redo_branch() {
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
        assert!(!log.can_redo()); // redo branch truncated
        assert_eq!(log.current().tracks[0].clips.len(), 1);
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
        log.apply(EditOp::InsertClip { track_id: tid, clip }).unwrap();
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
                EditOp::InsertClip { track_id: tid, clip: c0 },
                EditOp::InsertClip { track_id: tid, clip: c1 },
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
                EditOp::InsertClip { track_id: tid, clip: c0 },
                // This one fails: track doesn't exist.
                EditOp::RemoveTrack { track_id: bogus_tid },
            ],
        });
        assert!(result.is_err(), "compound with failing child must error");
        assert_eq!(log.len(), before_len, "no entry recorded on failure");
        assert_eq!(log.current().tracks[0].clips.len(), before_clip_count, "state unchanged");
    }

    #[test]
    fn empty_compound_is_a_noop() {
        let (mut log, _tid, _media) = seed_log();
        let before = log.current().clone();
        log.apply(EditOp::Compound { label: "empty".into(), ops: Vec::new() })
            .unwrap();
        assert_eq!(log.current(), &before, "empty compound must not change state");
    }
}
