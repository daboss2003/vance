//! Integration-level property tests for the timeline data model.
//!
//! Lives in `tests/` rather than `src/` so we can pull in `proptest`
//! as a dev-dep without it leaking into release builds.
//!
//! Properties exercised:
//! - Track invariants (sorted-by-timeline-in, no overlaps) hold after
//!   any successful sequence of inserts and removes.
//! - `insert_clip` and `remove_clip` invert each other on a fresh
//!   sequence.
//! - `EditLog::apply` × N then `undo` × N restores the base sequence.
//! - `apply` then `undo` then `redo` matches a single `apply`.
//! - M1: `scene_plan_at`'s top layer agrees with the legacy `scene_at`
//!   whenever it is a plain clip (always so when no transition is active).
//!   Transitions render over their stored length capped at the shorter of
//!   their two clips, so over-long or orphaned ones saved by an older build
//!   never reach past a clip.
//! - M1: transitions follow their cut through every geometry edit, go when
//!   their two clips stop meeting, and always sit on a cut, capped.
//! - M1: track add / insert / move / remove / hide / rename keep video
//!   tracks ahead of audio tracks, keep automatic names (`V<n>` / `A<n>`)
//!   positional, leave custom names intact and refuse names with control,
//!   separator, zero-width or text-direction characters.
//! - M1: the compositing `EditOp`s (locks included) land where an
//!   independent model says, an edit that changes nothing records no undo
//!   step, and undo / redo walk the model's states exactly.
//! - M1: with locks flipping mid-run, a locked track's contents never
//!   change and content edits aimed at it are refused.

use proptest::prelude::*;
use proptest::sample::Index;
use proptest::test_runner::TestCaseError;
use std::collections::HashMap;
use std::sync::Arc;
use vance_core::{FrameRate, RationalTime};
use vance_timeline::{
    ops, scene_at, scene_plan_at, ActiveClip, AudioEffectSpec, BlendMode, BusId, BusOutput, Clip,
    ClipId, Connection, EditLog, EditOp, Effect, FadeCurve, Keyframe, LayerContent, MediaItemId,
    SceneAt, Send, Sequence, Track, TrackId, TrackKind, Transition, TransitionId, TransitionKind,
};

// ---- helpers ---------------------------------------------------------------

fn t(num: i64) -> RationalTime {
    RationalTime::new(num, 1).unwrap()
}

/// One clip with `timeline_in = start`, `duration = duration`. Source
/// range [0, duration).
fn make_clip(media: MediaItemId, start: i64, duration: i64) -> Clip {
    Clip::new(media, "c", t(start), t(0), t(duration)).unwrap()
}

/// Track invariant: clips are sorted by timeline_in and pairwise
/// non-overlapping.
fn track_well_formed(track: &Track) -> bool {
    for w in track.clips.windows(2) {
        let a = &w[0];
        let b = &w[1];
        if a.timeline_in > b.timeline_in {
            return false;
        }
        if a.timeline_out() > b.timeline_in {
            return false;
        }
    }
    true
}

fn seed_seq() -> (Sequence, TrackId, MediaItemId) {
    let seq = Sequence::new("Seq", FrameRate::FPS_24);
    let seq = ops::add_track(&seq, TrackKind::Video, "V1");
    let track_id = seq.tracks[0].id;
    (seq, track_id, MediaItemId::new())
}

// ---- generators ------------------------------------------------------------

/// Vector of (start, duration) pairs that don't overlap on a single track.
/// Both numbers are nonzero positive; durations are bounded so RON output
/// stays small in shrunken cases.
fn non_overlapping_clips(max: usize) -> impl Strategy<Value = Vec<(i64, i64)>> {
    prop::collection::vec((1i64..50, 1i64..20), 0..=max).prop_map(|raw| {
        // Lay out clips end-to-end to guarantee no overlap regardless of
        // shrink choices. (start, duration) → cumulative start.
        let mut out = Vec::with_capacity(raw.len());
        let mut cursor = 0i64;
        for (gap, dur) in raw {
            cursor += gap;
            out.push((cursor, dur));
            cursor += dur;
        }
        out
    })
}

// ---- properties ------------------------------------------------------------

proptest! {
    /// Inserting any non-overlapping sequence of clips produces a
    /// well-formed track.
    #[test]
    fn inserts_keep_track_well_formed(spec in non_overlapping_clips(40)) {
        let (mut seq, tid, media) = seed_seq();
        for (start, dur) in spec {
            let c = make_clip(media, start, dur);
            seq = ops::insert_clip(&seq, tid, c).expect("non-overlapping by construction");
        }
        prop_assert!(track_well_formed(&seq.tracks[0]));
    }

    /// `insert_clip` then `remove_clip` returns to the original
    /// per-clip count and leaves the track empty.
    #[test]
    fn insert_remove_inverts(spec in non_overlapping_clips(20)) {
        let (mut seq, tid, media) = seed_seq();
        let mut clip_ids: Vec<ClipId> = Vec::new();
        for (start, dur) in spec {
            let c = make_clip(media, start, dur);
            clip_ids.push(c.id);
            seq = ops::insert_clip(&seq, tid, c).unwrap();
        }
        for id in clip_ids {
            seq = ops::remove_clip(&seq, tid, id).unwrap();
        }
        prop_assert_eq!(seq.tracks[0].clips.len(), 0);
    }

    /// Apply a chain of inserts through `EditLog`, then undo the chain
    /// — the resulting sequence equals the base.
    #[test]
    fn applies_undo_to_base(spec in non_overlapping_clips(15)) {
        let (base, tid, media) = seed_seq();
        let mut log = EditLog::new(base.clone());
        for (start, dur) in &spec {
            let c = make_clip(media, *start, *dur);
            log.apply(EditOp::InsertClip { track_id: tid, clip: c }).unwrap();
        }
        for _ in &spec {
            log.undo();
        }
        // Pointer-equal Arc<Track> on the only track means we restored the
        // exact pre-edit Arc, not a deep clone.
        prop_assert!(Arc::ptr_eq(&log.current().tracks[0], &base.tracks[0]));
    }

    /// `apply` then `undo` then `redo` lands on the same sequence as a
    /// single `apply`.
    #[test]
    fn apply_undo_redo_matches_apply(spec in non_overlapping_clips(8)) {
        let (base, tid, media) = seed_seq();
        let mut log_a = EditLog::new(base.clone());
        let mut log_b = EditLog::new(base.clone());
        for (start, dur) in &spec {
            let c = make_clip(media, *start, *dur);
            let op = EditOp::InsertClip { track_id: tid, clip: c.clone() };
            log_a.apply(op.clone()).unwrap();
            log_b.apply(op).unwrap();
        }
        // Round-trip log_b through undo/redo for every op.
        for _ in &spec { log_b.undo(); }
        for _ in &spec { log_b.redo(); }
        prop_assert_eq!(log_a.current(), log_b.current());
    }
}

// ---- M1 compositing: generators + helpers ------------------------------------

/// Quarter-second timeline position.
fn q(num: i64) -> RationalTime {
    RationalTime::new(num, 4).unwrap()
}

/// Video tracks first, then audio — the `Sequence::tracks` convention.
fn video_first(seq: &Sequence) -> bool {
    seq.tracks
        .iter()
        .skip_while(|t| t.kind == TrackKind::Video)
        .all(|t| t.kind == TrackKind::Audio)
}

/// One generated video track: whether it's shown, its clips as
/// `(gap before, duration (0 = a transient zero-length clip), enabled,
/// blend mode index, dissolve after it)`, an optional clip to remove
/// afterwards (its transitions go with it), and transitions stored as an
/// older build could have saved them: `(clip whose end anchors it, length
/// in half seconds, off the cut by a quarter second)` — so over-long and
/// orphaned ones.
#[derive(Clone, Debug)]
struct VideoTrackSpec {
    enabled: bool,
    clips: Vec<(i64, i64, bool, usize, Option<i64>)>,
    remove: Option<Index>,
    stored: Vec<(Index, i64, bool)>,
}

fn video_track_spec() -> impl Strategy<Value = VideoTrackSpec> {
    (
        prop::bool::weighted(0.8),
        prop::collection::vec(
            (
                0i64..4,
                0i64..8,
                prop::bool::weighted(0.85),
                0usize..BlendMode::ALL.len(),
                prop::option::weighted(0.5, 1i64..5),
            ),
            0..6,
        ),
        prop::option::weighted(0.3, any::<Index>()),
        prop::collection::vec((any::<Index>(), 1i64..17, prop::bool::weighted(0.2)), 0..3),
    )
        .prop_map(|(enabled, clips, remove, stored)| VideoTrackSpec {
            enabled,
            clips,
            remove,
            stored,
        })
}

/// Build a sequence from the specs. `audio_slots` drops audio tracks (each
/// with a long clip) at arbitrary indices, as an older bundle could hold
/// them, so layers must come from the video tracks' own order.
fn build_layered(video: &[VideoTrackSpec], audio_slots: &[Index]) -> Sequence {
    let media = MediaItemId::new();
    let mut seq = Sequence::new("S", FrameRate::FPS_24);
    for (ti, spec) in video.iter().enumerate() {
        seq = ops::add_track(&seq, TrackKind::Video, format!("V{}", ti + 1));
        let tid = seq.tracks[ti].id;
        let mut ids = Vec::new();
        let mut cursor = 0i64;
        for &(gap, dur, enabled, blend, _) in &spec.clips {
            cursor += gap;
            let src = 1000 * (ti as i64 + 1) + cursor;
            let mut c = Clip::new(media, "c", t(cursor), t(src), t(src + dur)).unwrap();
            c.enabled = enabled;
            c.blend_mode = BlendMode::ALL[blend];
            ids.push(c.id);
            seq = ops::insert_clip(&seq, tid, c).unwrap();
            cursor += dur;
        }
        for (j, &(.., dissolve)) in spec.clips.iter().enumerate() {
            if let Some(d) = dissolve {
                // Rejections (no next clip, overlapping windows) just skip.
                if let Ok((s, _)) =
                    ops::add_transition_at_cut(&seq, tid, ids[j], TransitionKind::Dissolve, t(d))
                {
                    seq = s;
                }
            }
        }
        if let (Some(ix), false) = (&spec.remove, ids.is_empty()) {
            seq = ops::remove_clip(&seq, tid, ids[ix.index(ids.len())]).unwrap();
        }
        let mut track = (*seq.tracks[ti]).clone();
        for (ix, halves, off) in &spec.stored {
            if let Some(c) =
                (!track.clips.is_empty()).then(|| &track.clips[ix.index(track.clips.len())])
            {
                let quarter = if *off { q(1) } else { RationalTime::ZERO };
                let position = RationalTime(c.timeline_out().0 + quarter.0);
                let duration = RationalTime::new(*halves, 2).unwrap();
                track
                    .transitions
                    .push(Arc::new(Transition::dissolve(position, duration)));
            }
        }
        seq.tracks[ti] = Arc::new(track);
        seq = ops::set_track_enabled(&seq, tid, spec.enabled).unwrap();
    }
    for slot in audio_slots {
        let mut a = Track::new(TrackKind::Audio, "A");
        a.clips
            .push(Arc::new(Clip::new(media, "a", t(0), t(0), t(100)).unwrap()));
        let at = slot.index(seq.tracks.len() + 1);
        seq.tracks.insert(at, Arc::new(a));
    }
    seq
}

fn find_clip(seq: &Sequence, track: TrackId, clip: ClipId) -> &Clip {
    let tr = seq.tracks.iter().find(|t| t.id == track).unwrap();
    tr.clips.iter().find(|c| c.id == clip).unwrap()
}

/// Group length of `kind` in `seq`.
fn group_len(seq: &Sequence, kind: TrackKind) -> usize {
    seq.tracks.iter().filter(|t| t.kind == kind).count()
}

fn kind_of(video: bool) -> TrackKind {
    if video {
        TrackKind::Video
    } else {
        TrackKind::Audio
    }
}

/// Number of names [`pool_name`] draws from.
const NAME_POOL: usize = 41;

/// Track names mixing automatic forms of both kinds, custom names, blanks,
/// one over the 64-character limit, right-to-left text, emoji ZWJ and flag
/// tag sequences, and ones holding characters a name refuses (control,
/// line / paragraph separator, format — including a ZWJ or tag outside an
/// emoji sequence).
fn pool_name(i: usize) -> String {
    match i {
        0 => String::new(),
        1 => "  ".into(),
        2 => "V7".into(),
        3 => "A3".into(),
        4 => "V01".into(),
        5 => "Music".into(),
        6 => "  Pad ".into(),
        7 => "v2".into(),
        8 => "é".repeat(64),
        9 => "x".repeat(65),
        10 => "tab\there".into(),
        11 => "nul\u{0}".into(),
        12 => "two\u{2028}lines".into(),
        13 => "Plate\u{200B}B".into(),
        14 => " \u{FEFF}BOM".into(),
        15 => "\u{202E}gnp.exe".into(),
        16 => "שלום \u{2067}x\u{2069}".into(),
        17 => "Plate\u{200F}".into(),
        18 => "soft\u{00AD}hyphen".into(),
        19 => "tag\u{E0041}".into(),
        20 => "x\u{2062}y".into(),
        21 => "mongol\u{180E}ian".into(),
        22 => "\u{1F9D1}\u{200D}\u{1F4BB}".into(),
        23 => "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308} Pride".into(),
        24 => "\u{1F469}\u{1F3FD}\u{200D}\u{1F52C}".into(),
        25 => "\u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F} Scotland".into(),
        26 => "a\u{200D}b".into(),
        27 => "\u{200D}\u{1F4BB}".into(),
        28 => "\u{1F4BB}\u{200D}".into(),
        29 => "tag \u{E0067}".into(),
        30 => "\u{1F3F4}\u{E0067}\u{E0062}".into(),
        31 => "\u{1F9D1}\u{200D}b".into(),
        32 => "\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}".into(),
        33 => "\u{06A9}\u{062A}\u{0627}\u{0628}\u{200C}\u{062E}\u{0627}\u{0646}\u{0647}".into(),
        34 => "\u{0915}\u{094D}\u{200C}\u{0937}".into(),
        35 => "a\u{200C}1".into(),
        36 => "\u{200C}\u{0646}\u{0627}\u{0645}\u{0647}".into(),
        37 => "a \u{200C}b".into(),
        38 => "\u{1F4BB}\u{200C}a".into(),
        39 => "a\u{200C}\u{200C}b".into(),
        _ => "مرحبا ٣".into(),
    }
}

/// Test oracle: characters a track name refuses — general categories Cc,
/// Zl, Zp and Cf (Unicode 16.0.0, written out here apart from `ops`).
fn refused_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{2028}'
                | '\u{2029}'
                | '\u{00AD}'
                | '\u{0600}'..='\u{0605}'
                | '\u{061C}'
                | '\u{06DD}'
                | '\u{070F}'
                | '\u{0890}'..='\u{0891}'
                | '\u{08E2}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{2064}'
                | '\u{2066}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{110BD}'
                | '\u{110CD}'
                | '\u{13430}'..='\u{1343F}'
                | '\u{1BCA0}'..='\u{1BCA3}'
                | '\u{1D173}'..='\u{1D17A}'
                | '\u{E0001}'
                | '\u{E0020}'..='\u{E007F}'
        )
}

/// Test oracle: is `name` an automatic name for a `kind` track?
fn is_auto(kind: TrackKind, name: &str) -> bool {
    let prefix = if kind == TrackKind::Video { "V" } else { "A" };
    name.strip_prefix(prefix)
        .is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
}

/// Test oracle, coarse: the emoji blocks the pool's names use (Misc
/// Symbols / Dingbats and U+1F300–1FAFF without the skin-tone modifiers).
/// Exact for the pool, not a general Extended_Pictographic.
fn pool_emoji(c: char) -> bool {
    ('\u{2600}'..='\u{27BF}').contains(&c)
        || (('\u{1F300}'..='\u{1FAFF}').contains(&c) && !('\u{1F3FB}'..='\u{1F3FF}').contains(&c))
}

/// Test oracle, coarse: the combining marks the pool's names use (combining
/// diacritics, Arabic harakat, Devanagari signs). Exact for the pool only.
fn pool_mark(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}' | '\u{064B}'..='\u{065F}' | '\u{093A}'..='\u{094F}')
}

/// Test oracle: does a (trimmed) name hold a refused character? A ZWJ
/// between two emoji (looking back past variation selectors and skin
/// tones), a ZWNJ between two letters (looking back past combining marks)
/// and the tags of a 🏴 subdivision flag closed by a cancel tag are
/// allowed; every other [`refused_char`] is refused.
fn name_refused(name: &str) -> bool {
    let cs: Vec<char> = name.chars().collect();
    let mut allowed = vec![false; cs.len()];
    for i in 0..cs.len() {
        if cs[i] == '\u{200C}' {
            let before = cs[..i].iter().rev().find(|c| !pool_mark(**c));
            allowed[i] = before.is_some_and(|c| c.is_alphabetic())
                && cs.get(i + 1).is_some_and(|c| c.is_alphabetic());
        }
        if cs[i] == '\u{200D}' {
            let skip =
                |c: &&char| matches!(**c, '\u{FE0E}' | '\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}');
            let before = cs[..i].iter().rev().find(|c| !skip(c));
            allowed[i] = before.is_some_and(|&c| pool_emoji(c))
                && cs.get(i + 1).is_some_and(|&c| pool_emoji(c));
        }
        if cs[i] == '\u{1F3F4}' {
            let tags = cs[i + 1..]
                .iter()
                .take_while(|c| ('\u{E0020}'..='\u{E007E}').contains(*c))
                .count();
            if tags > 0 && cs.get(i + 1 + tags) == Some(&'\u{E007F}') {
                allowed[i + 1..=i + 1 + tags].fill(true);
            }
        }
    }
    cs.iter()
        .zip(&allowed)
        .any(|(&c, &ok)| !ok && refused_char(c))
}

/// What a user-entered name (insert / rename) becomes: `Err` if rejected,
/// `Ok(None)` for automatic, `Ok(Some(custom))` otherwise.
fn entered_name(kind: TrackKind, name: &str) -> Result<Option<String>, ()> {
    let name = name.trim();
    if name.chars().count() > 64 || name_refused(name) {
        return Err(());
    }
    Ok((!name.is_empty() && !is_auto(kind, name)).then(|| name.to_owned()))
}

/// The automatic name of each track's position: `V1` = bottom video track.
fn positional_names(seq: &Sequence) -> Vec<String> {
    let (mut video, mut audio) = (0, 0);
    seq.tracks
        .iter()
        .map(|t| match t.kind {
            TrackKind::Video => {
                video += 1;
                format!("V{video}")
            }
            TrackKind::Audio => {
                audio += 1;
                format!("A{audio}")
            }
        })
        .collect()
}

/// Each track's name is what `model` says: its positional name when `None`
/// (automatic), else exactly the custom name; and no track is missing.
fn names_match(seq: &Sequence, model: &HashMap<TrackId, Option<String>>) -> bool {
    seq.tracks.len() == model.len()
        && seq
            .tracks
            .iter()
            .zip(positional_names(seq))
            .all(|(t, auto)| match model.get(&t.id) {
                Some(None) => t.name == auto,
                Some(Some(custom)) => &t.name == custom,
                None => false,
            })
}

/// Test oracle: the positive-length clips ending and starting at `pos`.
fn sides_at(track: &Track, pos: RationalTime) -> (Option<&Clip>, Option<&Clip>) {
    let positive = |c: &&Arc<Clip>| c.duration() > RationalTime::ZERO;
    let mut clips = track.clips.iter().filter(positive);
    let from = clips.clone().find(|c| c.timeline_out() == pos);
    let to = clips.find(|c| c.timeline_in == pos);
    (from.map(|c| &**c), to.map(|c| &**c))
}

/// Test oracle: every transition sits exactly on a cut between two clips
/// of positive length that meet there, and lasts no longer than either.
fn transitions_on_cuts(track: &Track) -> bool {
    track
        .transitions
        .iter()
        .all(|tr| match sides_at(track, tr.position) {
            (Some(a), Some(b)) => {
                tr.duration > RationalTime::ZERO
                    && tr.duration <= a.duration()
                    && tr.duration <= b.duration()
            }
            _ => false,
        })
}

/// The window a transition renders over, `2·[in, out]` (doubled so the
/// half-duration stays exact): its stored length capped at the shorter of
/// its two clips. `None` when it has no two sides.
fn rendered_window_x2(track: &Track, tr: &Transition) -> Option<(RationalTime, RationalTime)> {
    let (Some(a), Some(b)) = sides_at(track, tr.position) else {
        return None;
    };
    let d = tr.duration.min(a.duration()).min(b.duration());
    let pos2 = tr.position.0 + tr.position.0;
    Some((RationalTime(pos2 - d.0), RationalTime(pos2 + d.0)))
}

/// What ADR 0007 says an enabled video track shows at `at`: `(from,
/// Some((to, mix)))` for the first transition whose rendered window holds
/// `at` and whose two sides are both enabled, else `(clip, None)` for the
/// enabled clip under `at`.
fn expected_layer(track: &Track, at: RationalTime) -> Option<(ClipId, Option<(ClipId, f64)>)> {
    let at2 = at.0 + at.0;
    for tr in track.transitions.iter() {
        let Some((in2, out2)) = rendered_window_x2(track, tr) else {
            continue;
        };
        if at2 < in2.0 || at2 > out2.0 {
            continue;
        }
        let (Some(a), Some(b)) = sides_at(track, tr.position) else {
            unreachable!()
        };
        if a.enabled && b.enabled {
            let mix = (at2 - in2.0).as_f64() / (out2.0 - in2.0).as_f64();
            return Some((a.id, Some((b.id, mix))));
        }
    }
    track
        .clips
        .iter()
        .find(|c| c.enabled && c.timeline_in <= at && at < c.timeline_out())
        .map(|c| (c.id, None))
}

proptest! {
    /// At every quarter second each enabled video track contributes what
    /// [`expected_layer`] says (and hidden tracks nothing), per-layer times
    /// and blend modes come from the clips, and the top layer is what the
    /// legacy single-source `scene_at` shows whenever that layer is a plain
    /// clip — so always when no transition is active.
    #[test]
    fn scene_plan_top_layer_agrees_with_scene_at(
        video in prop::collection::vec(video_track_spec(), 1..=4),
        audio in prop::collection::vec(any::<Index>(), 0..=2),
    ) {
        let seq = build_layered(&video, &audio);
        let video_tracks: Vec<&Arc<Track>> =
            seq.tracks.iter().filter(|t| t.kind == TrackKind::Video).collect();
        let end = video_tracks
            .iter()
            .map(|t| t.duration().0.as_f64().ceil() as i64)
            .max()
            .unwrap_or(0)
            + 1;
        for k in 0..=4 * end {
            let at = q(k);
            let plan = scene_plan_at(&seq, at);
            let mut last_index = None;
            for layer in &plan.layers {
                let track = video_tracks[layer.video_index];
                prop_assert_eq!(track.id, layer.track_id);
                prop_assert!(track.enabled);
                prop_assert!(last_index < Some(layer.video_index), "bottom → top");
                last_index = Some(layer.video_index);
                let sides = match &layer.content {
                    LayerContent::Clip(lc) => {
                        let clip = find_clip(&seq, layer.track_id, lc.clip_id);
                        prop_assert!(clip.timeline_in <= at && at < clip.timeline_out());
                        vec![*lc]
                    }
                    LayerContent::Transition(tr) => {
                        prop_assert!((0.0..=1.0).contains(&tr.mix), "mix {}", tr.mix);
                        // The window never reaches past either clip.
                        let to = find_clip(&seq, layer.track_id, tr.to.clip_id);
                        prop_assert!(tr.from.clip_time > RationalTime::ZERO, "{:?}", tr);
                        prop_assert!(tr.to.clip_time < to.duration(), "{:?}", tr);
                        vec![tr.from, tr.to]
                    }
                };
                for lc in sides {
                    let clip = find_clip(&seq, layer.track_id, lc.clip_id);
                    prop_assert!(clip.enabled);
                    prop_assert_eq!(lc.clip_time, RationalTime(at.0 - clip.timeline_in.0));
                    prop_assert_eq!(lc.source_pts, RationalTime(clip.source_in.0 + lc.clip_time.0));
                    prop_assert_eq!(lc.blend_mode, clip.blend_mode);
                    prop_assert_eq!(lc.media, clip.media);
                }
            }
            for track in &video_tracks {
                let got = plan.layers.iter().find(|l| l.track_id == track.id).map(|l| {
                    match &l.content {
                        LayerContent::Clip(c) => (c.clip_id, None),
                        LayerContent::Transition(tr) => {
                            (tr.from.clip_id, Some((tr.to.clip_id, f64::from(tr.mix))))
                        }
                    }
                });
                let want = if track.enabled { expected_layer(track, at) } else { None };
                let same = match (got, want) {
                    (Some((a, Some((b, m)))), Some((wa, Some((wb, wm))))) => {
                        a == wa && b == wb && (m - wm).abs() < 1e-6
                    }
                    (got, want) => got.map(|g| (g.0, g.1.is_some())) == want.map(|w| (w.0, w.1.is_some())),
                };
                prop_assert!(same, "{} at {:?}: got {:?}, want {:?}", track.name, at, got, want);
            }

            let transition_active = video_tracks
                .iter()
                .any(|t| t.enabled && t.transitions.iter().any(|tr| tr.contains(at)));
            let has_transition_layer = plan
                .layers
                .iter()
                .any(|l| matches!(l.content, LayerContent::Transition(_)));
            prop_assert!(transition_active || !has_transition_layer);

            let legacy = scene_at(&seq, at);
            match plan.layers.last().map(|l| &l.content) {
                None => {
                    prop_assert_eq!(legacy, SceneAt::Empty, "at {:?}", at);
                }
                Some(LayerContent::Clip(c)) => {
                    let shown = SceneAt::Clip(ActiveClip {
                        clip_id: c.clip_id,
                        media: c.media,
                        source_pts: c.source_pts,
                    });
                    prop_assert_eq!(legacy, shown, "at {:?}", at);
                }
                Some(LayerContent::Transition(_)) => {
                    prop_assert!(transition_active);
                }
            }
        }
    }

    /// Random track edits through the `EditLog` keep video tracks ahead of
    /// audio; positions land where asked, out-of-range ones and bad names
    /// are rejected without a change, moves leave the other kind's slots
    /// alone, automatic names always state the track's position and custom
    /// names are kept exactly.
    #[test]
    fn track_ops_keep_video_first_and_names_positional(
        steps in prop::collection::vec(
            (0u8..6, any::<bool>(), any::<Index>(), 0usize..6, 0usize..NAME_POOL),
            1..30,
        ),
    ) {
        let mut log = EditLog::new(Sequence::new("S", FrameRate::FPS_24));
        // Each track's intended name: `None` = automatic, else custom.
        let mut model: HashMap<TrackId, Option<String>> = HashMap::new();
        for (op, video, pick, pos, name_ix) in steps {
            let before = log.current().clone();
            let picked = (!before.tracks.is_empty())
                .then(|| before.tracks[pick.index(before.tracks.len())].clone());
            let name = pool_name(name_ix);
            let edit = match (op, &picked) {
                (0, _) => EditOp::AddTrack {
                    track_id: TrackId::new(),
                    kind: kind_of(video),
                    name,
                },
                (1, _) => EditOp::InsertTrack {
                    track_id: TrackId::new(),
                    kind: kind_of(video),
                    name,
                    position: pos,
                },
                (2, Some(tr)) => EditOp::MoveTrack { track_id: tr.id, to_position: pos },
                (3, Some(tr)) => EditOp::RemoveTrack { track_id: tr.id },
                (4, Some(tr)) => EditOp::SetTrackEnabled { track_id: tr.id, enabled: video },
                (5, Some(tr)) => EditOp::RenameTrack { track_id: tr.id, name },
                _ => continue,
            };
            let picked_kind = picked.as_ref().map(|t| t.kind);
            let expect_ok = match &edit {
                EditOp::InsertTrack { kind, position, name, .. } => {
                    *position <= group_len(&before, *kind) && entered_name(*kind, name).is_ok()
                }
                EditOp::MoveTrack { to_position, .. } => {
                    *to_position < group_len(&before, picked_kind.unwrap())
                }
                EditOp::RenameTrack { name, .. } => entered_name(picked_kind.unwrap(), name).is_ok(),
                _ => true,
            };
            let ok = log.apply(edit.clone()).is_ok();
            prop_assert_eq!(ok, expect_ok, "{:?}", edit);
            let after = log.current();
            prop_assert!(video_first(after), "{:?} → {:?}", edit,
                after.tracks.iter().map(|t| t.kind).collect::<Vec<_>>());
            if !expect_ok {
                prop_assert_eq!(after, &before);
                continue;
            }
            let group_ids = |s: &Sequence, kind| -> Vec<TrackId> {
                s.tracks.iter().filter(|t| t.kind == kind).map(|t| t.id).collect()
            };
            match &edit {
                EditOp::AddTrack { track_id, kind, .. } => {
                    prop_assert_eq!(group_ids(after, *kind).last().copied(), Some(*track_id));
                }
                EditOp::InsertTrack { track_id, kind, position, .. } => {
                    prop_assert_eq!(group_ids(after, *kind)[*position], *track_id);
                }
                EditOp::MoveTrack { track_id, to_position } => {
                    let kind = picked.as_ref().unwrap().kind;
                    prop_assert_eq!(group_ids(after, kind)[*to_position], *track_id);
                    for (i, (a, b)) in before.tracks.iter().zip(&after.tracks).enumerate() {
                        if a.kind != kind {
                            prop_assert!(Arc::ptr_eq(a, b), "slot {} of the other kind moved", i);
                        }
                    }
                }
                EditOp::RemoveTrack { track_id } => {
                    prop_assert_eq!(after.tracks.len(), before.tracks.len() - 1);
                    prop_assert!(after.tracks.iter().all(|t| t.id != *track_id));
                }
                _ => {}
            }
            match &edit {
                EditOp::AddTrack { track_id, kind, name } => {
                    // The programmatic path takes the name verbatim.
                    model.insert(*track_id, (!is_auto(*kind, name)).then(|| name.clone()));
                }
                EditOp::InsertTrack { track_id, kind, name, .. } => {
                    model.insert(*track_id, entered_name(*kind, name).unwrap());
                }
                EditOp::RenameTrack { track_id, name } => {
                    model.insert(*track_id, entered_name(picked_kind.unwrap(), name).unwrap());
                }
                EditOp::RemoveTrack { track_id } => {
                    model.remove(track_id);
                }
                _ => {}
            }
            prop_assert!(names_match(after, &model), "{:?} → {:?}", edit,
                after.tracks.iter().map(|t| &t.name).collect::<Vec<_>>());
        }
    }

    /// Every compositing edit lands on the state an independent [`Model`] of
    /// the ops predicts (order, names, visibility, locks, blend modes, canvas
    /// size, connections); an edit the model rejects (locked track, bad
    /// position, size or name) fails without a change, and one that changes
    /// nothing records no undo step. Undo then walks back through the
    /// model's states to the exact base and redo forward again, and
    /// replaying the accepted ops reproduces the end state. Locks toggle
    /// mid-run.
    #[test]
    fn compositing_edit_ops_undo_redo_exactly(
        locked in prop::collection::vec(any::<bool>(), 3),
        steps in prop::collection::vec(
            (
                0u8..8,
                any::<Index>(),
                0usize..5,
                0usize..BlendMode::ALL.len(),
                any::<bool>(),
                0usize..NAME_POOL,
                0usize..FRAME_EDGES.len(),
                0usize..FRAME_EDGES.len(),
            ),
            1..30,
        ),
    ) {
        // V1 + V2 with two clips each, A1, a cross-track connection, and
        // random locks.
        let media = MediaItemId::new();
        let mut base = Sequence::new("S", FrameRate::FPS_24);
        for (kind, name) in [
            (TrackKind::Video, "V1"),
            (TrackKind::Video, "V2"),
            (TrackKind::Audio, "A1"),
        ] {
            base = ops::add_track(&base, kind, name);
        }
        let mut clip_ids = Vec::new();
        for ti in 0..3 {
            let tid = base.tracks[ti].id;
            for start in [0, 10] {
                let c = Clip::new(media, "c", t(start), t(0), t(5)).unwrap();
                clip_ids.push(c.id);
                base = ops::insert_clip(&base, tid, c).unwrap();
            }
        }
        base = ops::connect_clip(&base, clip_ids[0], clip_ids[2]).unwrap();
        for (ti, &l) in locked.iter().enumerate() {
            base = ops::set_track_locked(&base, base.tracks[ti].id, l).unwrap();
        }

        let mut model = Model::of(&base);
        prop_assert_eq!(model.view(), view(&base));
        let mut log = EditLog::new(base.clone());
        let mut history = vec![model.clone()];
        let mut accepted = Vec::new();
        for (op, pick, pos, blend, flag, name, w, h) in steps {
            let cur = log.current().clone();
            let track = (!cur.tracks.is_empty())
                .then(|| cur.tracks[pick.index(cur.tracks.len())].clone());
            let edit = match (op, &track) {
                (0, _) => EditOp::InsertTrack {
                    track_id: TrackId::new(),
                    kind: kind_of(flag),
                    name: pool_name(name),
                    position: pos,
                },
                (1, Some(tr)) => EditOp::MoveTrack { track_id: tr.id, to_position: pos },
                (2, Some(tr)) => EditOp::SetTrackEnabled { track_id: tr.id, enabled: flag },
                (3, Some(tr)) => EditOp::SetClipBlendMode {
                    track_id: tr.id,
                    clip_id: tr
                        .clips
                        .get(pos % 2)
                        .map(|c| c.id)
                        .unwrap_or_else(ClipId::new),
                    blend_mode: BlendMode::ALL[blend],
                },
                (4, _) => EditOp::SetFrameSize {
                    size: (!flag).then(|| (FRAME_EDGES[w], FRAME_EDGES[h])),
                },
                (5, Some(tr)) => EditOp::RemoveTrack { track_id: tr.id },
                (6, Some(tr)) => EditOp::RenameTrack { track_id: tr.id, name: pool_name(name) },
                (7, Some(tr)) => EditOp::SetTrackLocked { track_id: tr.id, locked: flag },
                _ => continue,
            };
            let depth = log.len();
            let expected = model.apply(&edit);
            let result = log.apply(edit.clone()).cloned();
            match expected {
                None => {
                    prop_assert!(result.is_err(), "{:?} should be rejected", edit);
                    prop_assert_eq!(log.current(), &cur, "rejected {:?} changed state", edit);
                    prop_assert_eq!(log.len(), depth);
                }
                Some(next) => {
                    let s = result
                        .map_err(|e| TestCaseError::fail(format!("{edit:?} rejected: {e}")))?;
                    prop_assert_eq!(view(&s), next.view(), "{:?}", edit);
                    prop_assert!(video_first(&s));
                    let changed = next != model;
                    prop_assert_eq!(log.len(), depth + usize::from(changed), "{:?}", edit);
                    if changed {
                        history.push(next.clone());
                        accepted.push(edit);
                    }
                    model = next;
                }
            }
        }

        for expected in history.iter().rev().skip(1) {
            prop_assert!(log.undo().is_some());
            prop_assert_eq!(view(log.current()), expected.view());
        }
        prop_assert!(log.undo().is_none());
        prop_assert_eq!(log.current(), &base);
        for expected in history.iter().skip(1) {
            prop_assert!(log.redo().is_some());
            prop_assert_eq!(view(log.current()), expected.view());
        }
        prop_assert!(log.redo().is_none());

        let mut replay = base.clone();
        for edit in &accepted {
            replay = vance_timeline::apply(&replay, edit).unwrap();
        }
        prop_assert_eq!(&replay, log.current());
    }

    /// Random content edits (trims, splits, ripples, magnetic edits across
    /// tracks, transitions), mixer routing and the allowed toggles on tracks
    /// whose locks flip mid-run: a content edit (or removal) aimed at a
    /// locked track is refused, and no accepted edit changes a locked
    /// track's contents (clips other than their mute, transitions, gain,
    /// pan, inserts, routing, sends). Every transition also stays within
    /// its two clips after every edit.
    #[test]
    fn lock_policy_holds_across_random_ops(
        locked in prop::collection::vec(any::<bool>(), 3),
        steps in prop::collection::vec(
            (
                0u8..31,
                any::<Index>(),
                any::<Index>(),
                0usize..6,
                any::<bool>(),
                0usize..NAME_POOL,
            ),
            1..40,
        ),
    ) {
        let (mut base, bus, media) = lock_policy_base();
        for (ti, &l) in locked.iter().enumerate() {
            base = ops::set_track_locked(&base, base.tracks[ti].id, l).unwrap();
        }
        let mut log = EditLog::new(base);
        for (op, pick, clip_pick, pos, flag, name) in steps {
            let cur = log.current().clone();
            if cur.tracks.is_empty() {
                break;
            }
            let tr = cur.tracks[pick.index(cur.tracks.len())].clone();
            let track_id = tr.id;
            let chosen = (!tr.clips.is_empty())
                .then(|| (*tr.clips[clip_pick.index(tr.clips.len())]).clone());
            let clip_id = chosen.as_ref().map_or_else(ClipId::new, |c| c.id);
            let (src_in, at) = chosen
                .as_ref()
                .map_or((RationalTime::ZERO, RationalTime::ZERO), |c| (c.source_in, c.timeline_in));
            let step = RationalTime::new(pos as i64 + 1, 2).unwrap();
            let signed = if flag { step } else { RationalTime(RationalTime::ZERO.0 - step.0) };
            let new_clip = |start: RationalTime| {
                Clip::new(media, "n", start, t(0), t(pos as i64 + 1)).unwrap()
            };
            let edit = match op {
                0 => EditOp::AppendClip { track_id, clip: new_clip(t(0)) },
                1 => EditOp::TrimOut {
                    track_id,
                    clip_id,
                    new_source_out: RationalTime(src_in.0 + step.0),
                },
                2 => EditOp::TrimIn {
                    track_id,
                    clip_id,
                    new_source_in: RationalTime(src_in.0 + step.0),
                },
                3 => EditOp::Split { track_id, clip_id, at_timeline: RationalTime(at.0 + step.0) },
                4 => EditOp::RemoveClip { track_id, clip_id },
                5 => EditOp::RippleRemoveClip { track_id, clip_id },
                6 => EditOp::Slide { track_id, clip_id, delta: signed },
                7 => EditOp::RippleInsertClip { track_id, clip: new_clip(at) },
                8 => EditOp::OverwriteClip {
                    track_id,
                    clip: new_clip(RationalTime(at.0 + step.0)),
                },
                9 => EditOp::MagneticRippleDelete { track_id, clip_id },
                10 => EditOp::MagneticRippleInsert { track_id, clip: new_clip(at) },
                11 => EditOp::MagneticMoveClip {
                    track_id,
                    clip_id,
                    new_timeline_in: RationalTime(at.0 + signed.0),
                },
                12 => EditOp::AddTransitionAtCut {
                    track_id,
                    transition_id: TransitionId::new(),
                    after_clip_id: clip_id,
                    kind: TransitionKind::Dissolve,
                    duration: t(pos as i64 + 1),
                },
                13 => EditOp::SetTrackGainDb { track_id, gain_db: pos as f32 - 3.0 },
                14 => EditOp::SetTrackPan { track_id, pan: pos as f32 / 5.0 - 0.5 },
                15 => EditOp::AddAudioEffect { track_id, effect: AudioEffectSpec::default_eq6() },
                16 => EditOp::RouteTrackToBus {
                    track_id,
                    output: if flag { BusOutput::Bus(bus) } else { BusOutput::Master },
                },
                17 => EditOp::AddTrackSend { track_id, send: Send::to(bus) },
                18 => EditOp::RemoveTrackSend { track_id, index: 0 },
                19 => EditOp::SetClipBlendMode {
                    track_id,
                    clip_id,
                    blend_mode: BlendMode::ALL[pos],
                },
                20 => EditOp::AddEffectToClip { track_id, clip_id, effect: Effect::new("grade") },
                21 => EditOp::SetClipFadeIn {
                    track_id,
                    clip_id,
                    duration_seconds: 0.5,
                    curve: FadeCurve::Linear,
                },
                22 => EditOp::RemoveTrack { track_id },
                23 => EditOp::SetTrackLocked { track_id, locked: flag },
                24 => EditOp::SetTrackMute { track_id, mute: flag },
                25 => EditOp::SetTrackSolo { track_id, solo: flag },
                26 => EditOp::SetTrackEnabled { track_id, enabled: flag },
                27 => EditOp::RenameTrack { track_id, name: pool_name(name) },
                28 => EditOp::MoveTrack { track_id, to_position: pos % 2 },
                29 => EditOp::SetClipMute { track_id, clip_id, mute: flag },
                _ => EditOp::RemoveBus { bus_id: bus },
            };
            let content_edit = op <= 22;
            let result = log.apply(edit.clone()).cloned();
            let after = log.current();
            if tr.locked && content_edit {
                prop_assert!(result.is_err(), "{:?} accepted on a locked track", edit);
            }
            if result.is_err() {
                prop_assert_eq!(after, &cur, "rejected {:?} changed state", edit);
                continue;
            }
            for before in cur.tracks.iter().filter(|t| t.locked) {
                let now = after.tracks.iter().find(|t| t.id == before.id);
                prop_assert!(now.is_some(), "{:?} removed a locked track", edit);
                prop_assert_eq!(content(now.unwrap()), content(before), "{:?}", edit);
            }
            for track in &after.tracks {
                prop_assert!(transitions_on_cuts(track), "{:?} → {:?}", edit, track.transitions);
            }
        }
    }

    /// Transitions belong to the cut between their two clips. Across random
    /// geometry edits on three tracks (trims, slides, splits, overwrites,
    /// ripples, magnetic moves and ripples across tracks), every transition
    /// sits exactly on a cut between two clips that meet, capped at the
    /// shorter one. For the edits that keep clip ids (all but split and
    /// overwrite), a transition survives exactly when its two clips still
    /// meet, stays between those same two clips at their new cut, and keeps
    /// its length unless the cap shortens it. Only adding one makes a
    /// transition appear.
    #[test]
    fn transitions_follow_their_cuts(
        steps in prop::collection::vec(
            (0u8..16, any::<Index>(), any::<Index>(), 0usize..6, any::<bool>()),
            1..40,
        ),
    ) {
        let (base, _, media) = lock_policy_base();
        let mut log = EditLog::new(base);
        for (op, pick, clip_pick, pos, flag) in steps {
            let cur = log.current().clone();
            let tr = cur.tracks[pick.index(cur.tracks.len())].clone();
            let track_id = tr.id;
            let ci = (!tr.clips.is_empty()).then(|| clip_pick.index(tr.clips.len()));
            let chosen = ci.map(|i| (*tr.clips[i]).clone());
            let clip_id = chosen.as_ref().map_or_else(ClipId::new, |c| c.id);
            let (src_in, at) = chosen
                .as_ref()
                .map_or((RationalTime::ZERO, RationalTime::ZERO), |c| (c.source_in, c.timeline_in));
            let step = RationalTime::new(pos as i64 + 1, 2).unwrap();
            let signed = if flag { step } else { RationalTime(RationalTime::ZERO.0 - step.0) };
            let new_clip = |start: RationalTime| {
                Clip::new(media, "n", start, t(0), t(pos as i64 + 1)).unwrap()
            };
            let added = TransitionId::new();
            let edit = match op {
                0 => EditOp::AppendClip { track_id, clip: new_clip(t(0)) },
                1 => EditOp::TrimOut {
                    track_id,
                    clip_id,
                    new_source_out: RationalTime(src_in.0 + step.0),
                },
                2 => EditOp::TrimIn {
                    track_id,
                    clip_id,
                    new_source_in: RationalTime(src_in.0 + step.0),
                },
                3 => EditOp::Split { track_id, clip_id, at_timeline: RationalTime(at.0 + step.0) },
                4 => EditOp::RemoveClip { track_id, clip_id },
                5 => EditOp::RippleRemoveClip { track_id, clip_id },
                6 => EditOp::Slide { track_id, clip_id, delta: signed },
                7 => EditOp::RippleInsertClip { track_id, clip: new_clip(at) },
                8 => EditOp::OverwriteClip {
                    track_id,
                    clip: new_clip(RationalTime(at.0 + step.0)),
                },
                9 => EditOp::MagneticRippleDelete { track_id, clip_id },
                10 => EditOp::MagneticRippleInsert { track_id, clip: new_clip(at) },
                11 => EditOp::MagneticMoveClip {
                    track_id,
                    clip_id,
                    new_timeline_in: RationalTime(at.0 + signed.0),
                },
                // Ties the next clip to this one, so magnetic moves carry
                // both sides of a cut.
                12 => EditOp::ConnectClip {
                    anchor: clip_id,
                    connected: ci
                        .and_then(|i| tr.clips.get(i + 1))
                        .map_or_else(ClipId::new, |c| c.id),
                },
                _ => EditOp::AddTransitionAtCut {
                    track_id,
                    transition_id: added,
                    after_clip_id: clip_id,
                    kind: TransitionKind::Dissolve,
                    duration: step,
                },
            };
            if log.apply(edit.clone()).is_err() {
                prop_assert_eq!(log.current(), &cur, "rejected {:?} changed state", edit);
                continue;
            }
            let after = log.current();
            let keeps_ids = !matches!(edit, EditOp::Split { .. } | EditOp::OverwriteClip { .. });
            for (old, new) in cur.tracks.iter().zip(&after.tracks) {
                prop_assert_eq!(old.id, new.id);
                prop_assert!(transitions_on_cuts(new), "{:?} → {:?}", edit, new.transitions);
                for t_new in &new.transitions {
                    let existed = old.transitions.iter().any(|t| t.id == t_new.id);
                    prop_assert!(existed || t_new.id == added, "{:?} made {:?}", edit, t_new);
                }
                if !keeps_ids {
                    continue;
                }
                let live = |id: ClipId| {
                    new.clips.iter().find(|c| c.id == id && c.duration() > RationalTime::ZERO)
                };
                for t_old in &old.transitions {
                    let (Some(a), Some(b)) = sides_at(old, t_old.position) else {
                        unreachable!("the invariant held before {edit:?}")
                    };
                    let meet = match (live(a.id), live(b.id)) {
                        (Some(a2), Some(b2)) if a2.timeline_out() == b2.timeline_in => Some((a2, b2)),
                        _ => None,
                    };
                    let now = new.transitions.iter().find(|t| t.id == t_old.id);
                    match (meet, now) {
                        (Some((a2, b2)), Some(t_now)) => {
                            let (from, to) = sides_at(new, t_now.position);
                            prop_assert_eq!(
                                (from.map(|c| c.id), to.map(|c| c.id)),
                                (Some(a.id), Some(b.id)),
                                "{:?} moved {:?} to another cut", edit, t_old
                            );
                            let capped = t_old.duration.min(a2.duration()).min(b2.duration());
                            prop_assert_eq!(t_now.duration, capped, "{:?}", edit);
                            let kept = Transition {
                                position: t_now.position,
                                duration: t_now.duration,
                                ..(**t_old).clone()
                            };
                            prop_assert_eq!(&**t_now, &kept, "only position and length change");
                        }
                        (None, None) => {}
                        (meet, now) => {
                            prop_assert!(false, "{:?}: clips meet = {}, kept = {} for {:?}",
                                edit, meet.is_some(), now.is_some(), t_old);
                        }
                    }
                }
            }
        }
    }
}

/// Frame edges for `SetFrameSize`: below, at and above both limits
/// (16 and 16384), odd ones, and `u32::MAX`.
const FRAME_EDGES: [u32; 12] = [
    0,
    8,
    15,
    16,
    17,
    1080,
    1081,
    1920,
    16383,
    16384,
    16385,
    u32::MAX,
];

// ---- independent model of the compositing track ops --------------------------

/// One track as [`Model`] sees it. `custom` is the custom name; `None`
/// means automatic, i.e. named after the position.
#[derive(Clone, Debug, PartialEq)]
struct ModelTrack {
    id: TrackId,
    custom: Option<String>,
    enabled: bool,
    locked: bool,
    clips: Vec<Clip>,
}

/// What the compositing ops should do to a sequence, written apart from
/// `ops`: the video and audio groups are separate lists (the sequence is
/// video then audio), so positions are plain list indices.
#[derive(Clone, Debug, PartialEq)]
struct Model {
    video: Vec<ModelTrack>,
    audio: Vec<ModelTrack>,
    frame_size: Option<(u32, u32)>,
    connections: Vec<Connection>,
}

/// Every track field the compositing ops touch, plus the canvas size and
/// connections.
type TrackView = (TrackId, TrackKind, String, bool, bool, Vec<Clip>);
type View = (Vec<TrackView>, Option<(u32, u32)>, Vec<Connection>);

fn view(seq: &Sequence) -> View {
    let tracks = seq
        .tracks
        .iter()
        .map(|t| {
            let clips = t.clips.iter().map(|c| (**c).clone()).collect();
            (t.id, t.kind, t.name.clone(), t.enabled, t.locked, clips)
        })
        .collect();
    (tracks, seq.frame_size, seq.connections.clone())
}

impl Model {
    fn of(seq: &Sequence) -> Self {
        let group = |kind| {
            seq.tracks
                .iter()
                .filter(|t| t.kind == kind)
                .map(|t| ModelTrack {
                    id: t.id,
                    custom: (!is_auto(kind, &t.name)).then(|| t.name.clone()),
                    enabled: t.enabled,
                    locked: t.locked,
                    clips: t.clips.iter().map(|c| (**c).clone()).collect(),
                })
                .collect()
        };
        Model {
            video: group(TrackKind::Video),
            audio: group(TrackKind::Audio),
            frame_size: seq.frame_size,
            connections: seq.connections.clone(),
        }
    }

    fn view(&self) -> View {
        let named = |kind: TrackKind, prefix: &str, group: &[ModelTrack]| -> Vec<TrackView> {
            group
                .iter()
                .enumerate()
                .map(|(i, t)| {
                    let name = t
                        .custom
                        .clone()
                        .unwrap_or_else(|| format!("{prefix}{}", i + 1));
                    (t.id, kind, name, t.enabled, t.locked, t.clips.clone())
                })
                .collect()
        };
        let mut tracks = named(TrackKind::Video, "V", &self.video);
        tracks.extend(named(TrackKind::Audio, "A", &self.audio));
        (tracks, self.frame_size, self.connections.clone())
    }

    fn group_mut(&mut self, kind: TrackKind) -> &mut Vec<ModelTrack> {
        match kind {
            TrackKind::Video => &mut self.video,
            TrackKind::Audio => &mut self.audio,
        }
    }

    fn locate(&self, id: TrackId) -> Option<(TrackKind, usize)> {
        let find = |g: &[ModelTrack]| g.iter().position(|t| t.id == id);
        find(&self.video)
            .map(|i| (TrackKind::Video, i))
            .or_else(|| find(&self.audio).map(|i| (TrackKind::Audio, i)))
    }

    fn track_mut(&mut self, id: TrackId) -> Option<&mut ModelTrack> {
        let (kind, i) = self.locate(id)?;
        Some(&mut self.group_mut(kind)[i])
    }

    /// The model after `op`, or `None` when `op` must be rejected.
    fn apply(&self, op: &EditOp) -> Option<Model> {
        let mut m = self.clone();
        match op {
            EditOp::InsertTrack {
                track_id,
                kind,
                name,
                position,
            } => {
                if m.locate(*track_id).is_some() {
                    return None;
                }
                let custom = entered_name(*kind, name).ok()?;
                let group = m.group_mut(*kind);
                if *position > group.len() {
                    return None;
                }
                let track = ModelTrack {
                    id: *track_id,
                    custom,
                    enabled: true,
                    locked: false,
                    clips: Vec::new(),
                };
                group.insert(*position, track);
            }
            EditOp::MoveTrack {
                track_id,
                to_position,
            } => {
                let (kind, i) = m.locate(*track_id)?;
                let group = m.group_mut(kind);
                if *to_position >= group.len() {
                    return None;
                }
                let track = group.remove(i);
                group.insert(*to_position, track);
            }
            EditOp::SetTrackEnabled { track_id, enabled } => {
                m.track_mut(*track_id)?.enabled = *enabled;
            }
            EditOp::SetTrackLocked { track_id, locked } => {
                m.track_mut(*track_id)?.locked = *locked;
            }
            EditOp::RenameTrack { track_id, name } => {
                let (kind, _) = m.locate(*track_id)?;
                m.track_mut(*track_id)?.custom = entered_name(kind, name).ok()?;
            }
            EditOp::SetClipBlendMode {
                track_id,
                clip_id,
                blend_mode,
            } => {
                let track = m.track_mut(*track_id)?;
                if track.locked {
                    return None;
                }
                track
                    .clips
                    .iter_mut()
                    .find(|c| c.id == *clip_id)?
                    .blend_mode = *blend_mode;
            }
            EditOp::SetFrameSize { size } => {
                let edge_ok = |e: u32| (16..=16384).contains(&e);
                m.frame_size = match *size {
                    None => None,
                    Some((w, h)) if edge_ok(w) && edge_ok(h) => Some((w / 2 * 2, h / 2 * 2)),
                    Some(_) => return None,
                };
            }
            EditOp::RemoveTrack { track_id } => {
                let (kind, i) = m.locate(*track_id)?;
                if m.group_mut(kind)[i].locked {
                    return None;
                }
                let gone: Vec<ClipId> = m
                    .group_mut(kind)
                    .remove(i)
                    .clips
                    .iter()
                    .map(|c| c.id)
                    .collect();
                m.connections
                    .retain(|c| !gone.contains(&c.anchor) && !gone.contains(&c.connected));
            }
            other => unreachable!("not modelled: {other:?}"),
        }
        Some(m)
    }
}

// ---- lock policy ------------------------------------------------------------

/// V1, V2 and A1, each with two butted 5 s clips, a 2 s dissolve at the cut
/// and a send to the "Music" bus; A1 also routes to it.
fn lock_policy_base() -> (Sequence, BusId, MediaItemId) {
    let media = MediaItemId::new();
    let bus = BusId::new();
    let mut seq = Sequence::new("S", FrameRate::FPS_24);
    for (kind, name) in [
        (TrackKind::Video, "V1"),
        (TrackKind::Video, "V2"),
        (TrackKind::Audio, "A1"),
    ] {
        seq = ops::add_track(&seq, kind, name);
    }
    seq = ops::add_bus_with_id(&seq, bus, "Music");
    for ti in 0..3 {
        let tid = seq.tracks[ti].id;
        for start in [0, 5] {
            let c = Clip::new(media, "c", t(start), t(0), t(5)).unwrap();
            seq = ops::insert_clip(&seq, tid, c).unwrap();
        }
        let first = seq.tracks[ti].clips[0].id;
        seq = ops::add_transition_at_cut(&seq, tid, first, TransitionKind::Dissolve, t(2))
            .unwrap()
            .0;
        seq = ops::add_track_send(&seq, tid, Send::to(bus)).unwrap();
    }
    let a1 = seq.tracks[2].id;
    seq = ops::route_track_to_bus(&seq, a1, BusOutput::Bus(bus)).unwrap();
    (seq, bus, media)
}

/// What a lock freezes on a track: everything but its name, visibility,
/// mute / solo, lock, position and its clips' mute (`Clip.enabled`).
#[derive(Debug, PartialEq)]
struct Content {
    id: TrackId,
    kind: TrackKind,
    clips: Vec<Clip>,
    transitions: Vec<Transition>,
    gain_db: f32,
    gain_keyframes: Vec<Keyframe>,
    pan: f32,
    audio_effects: Vec<AudioEffectSpec>,
    output: BusOutput,
    sends: Vec<Send>,
}

fn content(t: &Track) -> Content {
    Content {
        id: t.id,
        kind: t.kind,
        clips: t
            .clips
            .iter()
            .map(|c| Clip {
                enabled: true,
                ..(**c).clone()
            })
            .collect(),
        transitions: t.transitions.iter().map(|tr| (**tr).clone()).collect(),
        gain_db: t.gain_db,
        gain_keyframes: t.gain_keyframes.clone(),
        pan: t.pan,
        audio_effects: t.audio_effects.clone(),
        output: t.output,
        sends: t.sends.clone(),
    }
}
