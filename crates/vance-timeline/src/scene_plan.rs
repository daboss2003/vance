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

use crate::blend::BlendMode;
use crate::clip::Clip;
use crate::id::{ClipId, MediaItemId, TrackId, TransitionId};
use crate::sequence::Sequence;
use crate::track::{Track, TrackKind};
use crate::transition::{Transition, TransitionKind};

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
            let source_pts = RationalTime(c.source_in.0 + (timeline_t.0 - c.timeline_in.0));
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

/// Cross-dissolve resolution at `timeline_t`. When the time falls inside
/// a transition's window on an enabled video track, the outgoing clip
/// (A) is the one whose `timeline_out` equals the transition's
/// position, and the incoming clip (B) is the immediately following
/// clip on that same track.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveTransition {
    /// Which transition this is — used for UI selection bookkeeping.
    pub transition_id: TransitionId,
    /// The transition kind — drives the engine path (GPU dissolve vs a
    /// WASM `process_transition`).
    pub kind: TransitionKind,
    /// Outgoing clip (fading out) — the renderer's A side.
    pub from: ActiveClip,
    /// Incoming clip (fading in) — the renderer's B side.
    pub to: ActiveClip,
    /// Linear mix factor in `[0, 1]`. 0 = pure A; 1 = pure B. For a WASM
    /// transition this is fed to the plugin as its `mix`/`progress` param.
    pub mix: f32,
    /// Plugin param overrides (WASM transitions only; empty otherwise).
    pub params: std::collections::BTreeMap<String, crate::effect::EffectValue>,
}

/// First transition on an enabled video track whose window contains
/// `timeline_t`, paired with the surrounding clips' source PTSes.
/// Returns `None` when no track has a transition active at this time.
pub fn transition_at(seq: &Sequence, timeline_t: RationalTime) -> Option<ActiveTransition> {
    if timeline_t.0 < Rational::ZERO {
        return None;
    }
    // Walk topmost-track-first so transitions on V2 win over V1, same
    // as `scene_at`.
    for track in seq.tracks.iter().rev() {
        if track.kind != TrackKind::Video || !track.enabled {
            continue;
        }
        for tr in track.transitions.iter() {
            if !tr.contains(timeline_t) {
                continue;
            }
            // Find the outgoing/incoming clips around the transition's
            // position. The outgoing clip's `timeline_out` equals
            // `tr.position` (or close); the incoming clip's
            // `timeline_in` equals `tr.position` (or close).
            let from = track
                .clips
                .iter()
                .find(|c| c.timeline_out() == tr.position)
                .map(|arc| arc.as_ref())?;
            let to = track
                .clips
                .iter()
                .find(|c| c.timeline_in == tr.position)
                .map(|arc| arc.as_ref())?;
            if !from.enabled || !to.enabled {
                continue;
            }
            let mix = transition_mix(tr, timeline_t);
            // A's source_pts is wherever we'd be if A still played at
            // `timeline_t` (it may have ended; we virtually extend).
            let from_src = RationalTime(from.source_in.0 + (timeline_t.0 - from.timeline_in.0));
            // B's source_pts: similarly, virtually start B early so
            // it covers the pre-cut portion of the window.
            let to_src = RationalTime(to.source_in.0 + (timeline_t.0 - to.timeline_in.0));
            return Some(ActiveTransition {
                transition_id: tr.id,
                kind: tr.kind.clone(),
                from: ActiveClip {
                    clip_id: from.id,
                    media: from.media,
                    source_pts: from_src,
                },
                to: ActiveClip {
                    clip_id: to.id,
                    media: to.media,
                    source_pts: to_src,
                },
                mix,
                params: tr.params.clone(),
            });
        }
    }
    None
}

// ---- M1: layered scene plan (ADR 0007) ---------------------------------------

/// Every visible layer at one timeline position, for the compositor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScenePlan {
    /// Layers bottom → top (V1 first). Empty = black frame.
    pub layers: Vec<Layer>,
}

/// One video track's contribution to a [`ScenePlan`].
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    /// Track this layer comes from.
    pub track_id: TrackId,
    /// Index of the track among ALL the sequence's video tracks (0 = V1),
    /// counting disabled ones, so it stays stable when a track is hidden.
    pub video_index: usize,
    /// What the track shows at this time.
    pub content: LayerContent,
}

/// What a layer draws.
#[derive(Clone, Debug, PartialEq)]
pub enum LayerContent {
    /// A single clip.
    Clip(LayerClip),
    /// A transition between two adjacent clips on the track.
    Transition(LayerTransition),
}

/// A clip resolved at one timeline position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerClip {
    /// Which clip.
    pub clip_id: ClipId,
    /// The clip's source media.
    pub media: MediaItemId,
    /// Position inside the source: `source_in + (t − timeline_in)`.
    pub source_pts: RationalTime,
    /// Clip-relative time `t − timeline_in`, for keyframe evaluation.
    /// Negative for a transition's incoming side before its cut.
    pub clip_time: RationalTime,
    /// The clip's blend mode.
    pub blend_mode: BlendMode,
}

/// A transition resolved at one timeline position. Both sides are full
/// clips (each with its own effects and grades).
#[derive(Clone, Debug, PartialEq)]
pub struct LayerTransition {
    /// Which transition.
    pub transition_id: TransitionId,
    /// Dissolve or a WASM plugin.
    pub kind: TransitionKind,
    /// Outgoing clip (A side).
    pub from: LayerClip,
    /// Incoming clip (B side).
    pub to: LayerClip,
    /// Linear progress in `[0, 1]`; 0 = all A, 1 = all B.
    pub mix: f32,
    /// Plugin param overrides (WASM transitions; empty otherwise).
    pub params: std::collections::BTreeMap<String, crate::effect::EffectValue>,
}

/// Resolve every visible layer at `t`, bottom → top.
///
/// Enabled video tracks only. Per track: an enclosing transition (both
/// neighbours present and enabled) wins, else the enabled clip at `t`.
/// Orphaned transitions fall through to the clip. Negative `t` → empty.
pub fn scene_plan_at(seq: &Sequence, t: RationalTime) -> ScenePlan {
    let mut plan = ScenePlan::default();
    if t.0 < Rational::ZERO {
        return plan;
    }
    let video = seq.tracks.iter().filter(|tr| tr.kind == TrackKind::Video);
    for (video_index, track) in video.enumerate() {
        if !track.enabled {
            continue;
        }
        let content = layer_transition_at(track, t)
            .map(LayerContent::Transition)
            .or_else(|| {
                clip_at(track, t)
                    .filter(|c| c.enabled)
                    .map(|c| LayerContent::Clip(layer_clip(c, t)))
            });
        if let Some(content) = content {
            plan.layers.push(Layer {
                track_id: track.id,
                video_index,
                content,
            });
        }
    }
    plan
}

/// `clip` resolved at timeline `t` (virtually extended past its edges, as a
/// transition's sides are).
fn layer_clip(clip: &Clip, t: RationalTime) -> LayerClip {
    LayerClip {
        clip_id: clip.id,
        media: clip.media,
        source_pts: RationalTime(clip.source_in.0 + (t.0 - clip.timeline_in.0)),
        clip_time: RationalTime(t.0 - clip.timeline_in.0),
        blend_mode: clip.blend_mode,
    }
}

/// First transition on `track` whose window contains `t` and whose two
/// neighbouring clips both exist and are enabled.
fn layer_transition_at(track: &Track, t: RationalTime) -> Option<LayerTransition> {
    track.transitions.iter().find_map(|tr| {
        if !tr.contains(t) {
            return None;
        }
        let from = track
            .clips
            .iter()
            .find(|c| c.timeline_out() == tr.position)?;
        let to = track.clips.iter().find(|c| c.timeline_in == tr.position)?;
        if !from.enabled || !to.enabled {
            return None;
        }
        Some(LayerTransition {
            transition_id: tr.id,
            kind: tr.kind.clone(),
            from: layer_clip(from, t),
            to: layer_clip(to, t),
            mix: transition_mix(tr, t),
            params: tr.params.clone(),
        })
    })
}

/// Linear mix factor for a transition at `timeline_t`: 0 at
/// `window_in`, 1 at `window_out`, linear between.
fn transition_mix(tr: &Transition, timeline_t: RationalTime) -> f32 {
    let win_in = tr.window_in();
    let win_out = tr.window_out();
    let span = win_out.0 - win_in.0;
    if span <= Rational::ZERO {
        return 0.0;
    }
    let into = timeline_t.0 - win_in.0;
    let raw = (into.as_f64() / span.as_f64()).clamp(0.0, 1.0);
    raw as f32
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
        assert_eq!(
            scene_at(&seq, RationalTime::new(-1, 1).unwrap()),
            SceneAt::Empty
        );
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

    // ---- scene_plan_at ----------------------------------------------------

    /// V1 [0,10) source 0, V2 [2,6) source 20 (Screen), plus A1 with a clip.
    fn two_layer_seq() -> (Sequence, [TrackId; 2], [ClipId; 2]) {
        let media = MediaItemId::new();
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        seq = ops::add_track(&seq, TrackKind::Video, "V2");
        seq = ops::add_track(&seq, TrackKind::Audio, "A1");
        let (v1, v2, a1) = (seq.tracks[0].id, seq.tracks[1].id, seq.tracks[2].id);
        let low = Clip::new(media, "low", t(0), t(0), t(10)).unwrap();
        let mut high = Clip::new(media, "high", t(2), t(20), t(24)).unwrap();
        high.blend_mode = BlendMode::Screen;
        let ids = [low.id, high.id];
        seq = ops::insert_clip(&seq, v1, low).unwrap();
        seq = ops::insert_clip(&seq, v2, high).unwrap();
        let audio = Clip::new(media, "a", t(0), t(0), t(10)).unwrap();
        seq = ops::insert_clip(&seq, a1, audio).unwrap();
        (seq, [v1, v2], ids)
    }

    fn clip_layer(l: &Layer) -> LayerClip {
        match &l.content {
            LayerContent::Clip(c) => *c,
            other => panic!("expected a clip layer, got {other:?}"),
        }
    }

    #[test]
    fn plan_stacks_layers_bottom_to_top_with_times() {
        let (seq, [v1, v2], [low, high]) = two_layer_seq();
        let plan = scene_plan_at(&seq, t(3));
        assert_eq!(plan.layers.len(), 2, "audio track contributes no layer");
        assert_eq!(plan.layers[0].track_id, v1);
        assert_eq!(plan.layers[0].video_index, 0);
        assert_eq!(plan.layers[1].track_id, v2);
        assert_eq!(plan.layers[1].video_index, 1);
        let (a, b) = (clip_layer(&plan.layers[0]), clip_layer(&plan.layers[1]));
        assert_eq!((a.clip_id, a.source_pts, a.clip_time), (low, t(3), t(3)));
        assert_eq!((b.clip_id, b.source_pts, b.clip_time), (high, t(21), t(1)));
        assert_eq!(a.blend_mode, BlendMode::Normal);
        assert_eq!(b.blend_mode, BlendMode::Screen);
    }

    #[test]
    fn plan_skips_gaps_and_is_empty_outside_clips() {
        let (seq, [v1, _], _) = two_layer_seq();
        let plan = scene_plan_at(&seq, t(7)); // V2 clip ended at 6
        assert_eq!(plan.layers.len(), 1);
        assert_eq!(plan.layers[0].track_id, v1);
        assert_eq!(scene_plan_at(&seq, t(10)), ScenePlan::default());
        assert!(scene_plan_at(&seq, RationalTime::new(-1, 1).unwrap())
            .layers
            .is_empty());
    }

    #[test]
    fn plan_hidden_track_keeps_video_index_of_the_rest() {
        let (seq, [v1, v2], _) = two_layer_seq();
        let seq = ops::set_track_enabled(&seq, v1, false).unwrap();
        let plan = scene_plan_at(&seq, t(3));
        assert_eq!(plan.layers.len(), 1);
        assert_eq!(plan.layers[0].track_id, v2);
        assert_eq!(plan.layers[0].video_index, 1, "V2 stays index 1");
    }

    #[test]
    fn plan_skips_disabled_clips() {
        let (seq, [_, v2], [_, high]) = two_layer_seq();
        let seq = ops::set_clip_mute(&seq, v2, high, true).unwrap();
        assert_eq!(scene_plan_at(&seq, t(3)).layers.len(), 1);
    }

    #[test]
    fn plan_resolves_transition_per_track_and_orphans_fall_through() {
        // V1: a [0,5) src 0 | b [5,10) src 100, 2 s dissolve at the cut.
        // V2: c [0,10) — no transition.
        let media = MediaItemId::new();
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = ops::add_track(&seq, TrackKind::Video, "V1");
        seq = ops::add_track(&seq, TrackKind::Video, "V2");
        let (v1, v2) = (seq.tracks[0].id, seq.tracks[1].id);
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let b = Clip::new(media, "b", t(5), t(100), t(105)).unwrap();
        let c = Clip::new(media, "c", t(0), t(0), t(10)).unwrap();
        let (a_id, b_id, c_id) = (a.id, b.id, c.id);
        seq = ops::insert_clip(&seq, v1, a).unwrap();
        seq = ops::insert_clip(&seq, v1, b).unwrap();
        seq = ops::insert_clip(&seq, v2, c).unwrap();
        let (seq, tr_id) =
            ops::add_transition_at_cut(&seq, v1, a_id, TransitionKind::Dissolve, t(2)).unwrap();

        let plan = scene_plan_at(&seq, t(5)); // window [4, 6], midpoint
        assert_eq!(plan.layers.len(), 2);
        match &plan.layers[0].content {
            LayerContent::Transition(tr) => {
                assert_eq!(tr.transition_id, tr_id);
                assert_eq!(tr.kind, TransitionKind::Dissolve);
                assert_eq!(tr.from.clip_id, a_id);
                assert_eq!(tr.to.clip_id, b_id);
                assert!((tr.mix - 0.5).abs() < 1e-6);
                assert_eq!(tr.from.source_pts, t(5), "A virtually extends");
                assert_eq!(tr.to.source_pts, t(100));
                assert_eq!(tr.to.clip_time, t(0));
            }
            other => panic!("expected a transition on V1, got {other:?}"),
        }
        assert_eq!(clip_layer(&plan.layers[1]).clip_id, c_id);

        // Before the cut, B's clip time is negative (it starts early).
        let early = scene_plan_at(&seq, RationalTime::new(9, 2).unwrap());
        match &early.layers[0].content {
            LayerContent::Transition(tr) => {
                assert_eq!(tr.to.clip_time, RationalTime::new(-1, 2).unwrap());
                assert!((tr.mix - 0.25).abs() < 1e-6);
            }
            other => panic!("expected a transition, got {other:?}"),
        }

        // Disable B: the transition is orphaned and the plain clip shows.
        let muted = ops::set_clip_mute(&seq, v1, b_id, true).unwrap();
        let plan = scene_plan_at(&muted, RationalTime::new(9, 2).unwrap());
        assert_eq!(clip_layer(&plan.layers[0]).clip_id, a_id);
        let plan = scene_plan_at(&muted, RationalTime::new(11, 2).unwrap());
        assert_eq!(plan.layers.len(), 1, "B disabled → V1 shows nothing");
        assert_eq!(clip_layer(&plan.layers[0]).clip_id, c_id);
    }
}
