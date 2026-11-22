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
//! - A locked track's contents (clips, transitions, effects, stored clip
//!   and mixer parameters, routing and sends) can't change and the track
//!   can't be removed: those operations fail with
//!   `TimelineError::InvalidArgument`. Mute,
//!   solo, visibility, stacking order and the name stay available
//!   (lock-policy comment above [`add_track`]).
//! - Automatic track names (`V1`, `A2`, …) follow the track's position and
//!   are renumbered by every structural track edit; custom names are kept.
//! - Operations validate their geometry: zero-or-negative durations,
//!   timeline overlap, negative source positions all reject up-front.
//! - Slip changes the source range, leaving the timeline alone.
//!   Slide changes the timeline position, leaving the source range alone.

use std::sync::Arc;
use vance_core::RationalTime;

use crate::blend::BlendMode;
use crate::bus::{bus_graph_cycle, AudioBus, BusOutput, Send};
use crate::clip::Clip;
use crate::connection::{connected_subtree, would_cycle, Connection};
use crate::effect::{Effect, EffectValue};
use crate::error::{Result, TimelineError};
use crate::id::{BusId, ClipId, EffectId, TrackId, TransitionId};
use crate::scene_plan::{capped_duration, transition_sides, transition_window};
use crate::sequence::Sequence;
use crate::track::{Track, TrackKind};
use crate::transition::{Transition, TransitionKind};
use crate::unicode_tables::{in_table, COMBINING_MARKS, EXTENDED_PICTOGRAPHIC, FORMAT_CHARS};

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

/// The lock-policy refusal (policy above [`add_track`]). The message names
/// the track the way the user sees it, by its name.
fn ensure_unlocked(track: &Track) -> Result<()> {
    if track.locked {
        Err(TimelineError::invalid(format!(
            "track \"{}\" is locked; unlock it first",
            track.name
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
        frame_size: seq.frame_size,
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

// Locked tracks (`Track.locked`): a lock freezes what is ON the track — its
// clips, transitions, effects and stored parameters (clip blend mode, gain,
// pan, fades, automation, audio inserts, output routing and sends) — so
// every op that rewrites those calls `ensure_unlocked`, `remove_track`
// refuses a locked track (it would delete those contents) and `remove_bus`
// refuses a bus a locked track uses (it would reroute it). Ripple edits
// across tracks skip locked ones. Non-destructive toggles that only change
// whether or where the track is seen or heard stay available, so the user
// can audition a locked track without unlocking it: mute / solo / clip mute,
// and for M1 visibility (`set_track_enabled`, the video analogue of mute)
// and stacking order (`move_track`, also when it shifts other locked
// tracks). The name is a label, not content: `rename_track` works on a
// locked track, and so does the automatic renumbering below, which has to,
// because moving or inserting another track changes a locked track's
// position. Adding a track never touches an existing one's contents, so
// locks don't affect it.
//
// Track names: an automatic name (`V<n>` on a video track, `A<n>` on an
// audio track, ASCII digits) states the track's position in its kind's
// group: V1 is the bottom video layer, A1 the first audio track. Every
// structural edit (add / insert / move / remove) renumbers them. Custom
// names are never changed, but their tracks still hold a number, so
// [BG, V1] becomes [BG, V2]. The schema has no flag for this; the form of
// the name is the flag.

/// Longest accepted user-entered track name, in characters (after trimming).
pub const MAX_TRACK_NAME_CHARS: usize = 64;

fn automatic_prefix(kind: TrackKind) -> char {
    match kind {
        TrackKind::Video => 'V',
        TrackKind::Audio => 'A',
    }
}

/// `true` iff `name` is an automatic name for a `kind` track: `V` (video) /
/// `A` (audio) followed by one or more ASCII digits.
fn is_automatic_track_name(kind: TrackKind, name: &str) -> bool {
    name.strip_prefix(automatic_prefix(kind))
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

/// Automatic name of the track at zero-based `position` in its kind's group.
fn automatic_track_name(kind: TrackKind, position: usize) -> String {
    format!("{}{}", automatic_prefix(kind), position + 1)
}

/// Rewrite every automatic name to its track's current position in its
/// kind's group. Tracks whose name doesn't change stay shared.
fn renumber_automatic_names(mut seq: Sequence) -> Sequence {
    let (mut video, mut audio) = (0, 0);
    for slot in &mut seq.tracks {
        let next = match slot.kind {
            TrackKind::Video => &mut video,
            TrackKind::Audio => &mut audio,
        };
        let position = *next;
        *next += 1;
        if is_automatic_track_name(slot.kind, &slot.name) {
            let want = automatic_track_name(slot.kind, position);
            if slot.name != want {
                Arc::make_mut(slot).name = want;
            }
        }
    }
    seq
}

fn is_emoji(c: char) -> bool {
    in_table(&EXTENDED_PICTOGRAPHIC, c)
}

/// Why a track name can't hold `c`, or `None` if it can: the Unicode
/// categories that break a one-line label or make two names look alike —
/// controls (Cc), line / paragraph separators (Zl, Zp) and format
/// characters (Cf). Right-to-left letters are ordinary letters and pass.
fn refused_name_char(c: char) -> Option<&'static str> {
    match c {
        '\u{2028}' | '\u{2029}' => Some("line or paragraph separators"),
        c if c.is_control() => Some("control characters such as tabs or line breaks"),
        // Two Cf groups people actually meet get their own names.
        '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}' => Some("zero-width characters"),
        '\u{200E}'
        | '\u{200F}'
        | '\u{061C}'
        | '\u{202A}'..='\u{202E}'
        | '\u{2066}'..='\u{2069}' => Some("text-direction controls"),
        c if in_table(&FORMAT_CHARS, c) => Some("invisible formatting characters"),
        _ => None,
    }
}

/// Trim and validate a user-entered track name. `None` means it was empty:
/// use the automatic name.
fn clean_track_name(name: &str) -> Result<Option<&str>> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    let chars = name.chars().count();
    if chars > MAX_TRACK_NAME_CHARS {
        return Err(TimelineError::invalid(format!(
            "track name is {chars} characters long; the limit is {MAX_TRACK_NAME_CHARS}"
        )));
    }
    if let Some((c, what)) = refused_in_name(name) {
        return Err(TimelineError::invalid(format!(
            "track name can't contain {what} (found U+{:04X})",
            c as u32
        )));
    }
    Ok(Some(name))
}

const ZWJ: char = '\u{200D}';
const ZWNJ: char = '\u{200C}';
const BLACK_FLAG: char = '\u{1F3F4}';
const CANCEL_TAG: char = '\u{E007F}';

/// The first character `name` can't hold and why ([`refused_name_char`]).
/// Intentional: three uses of format characters are ordinary text and pass
/// (decisions 2026-11-22) — a ZWJ joining two emoji (🧑‍💻, 🏳️‍🌈, 👩🏽‍🔬),
/// the tags of a subdivision flag (🏴 + U+E0020–E007E + cancel tag, e.g.
/// Scotland) and a ZWNJ between two letters (Persian, Kurdish and Indic
/// orthography: می‌خواهم). Anywhere else they are refused as before.
fn refused_in_name(name: &str) -> Option<(char, &'static str)> {
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == BLACK_FLAG {
            if let Some(end) = flag_tags_end(&chars, i + 1) {
                i = end + 1;
                continue;
            }
        }
        if c == ZWJ && joins_emoji(&chars, i) {
            i += 1;
            continue;
        }
        if c == ZWNJ && joins_letters(&chars, i) {
            i += 1;
            continue;
        }
        if let Some(why) = refused_name_char(c) {
            return Some((c, why));
        }
        i += 1;
    }
    None
}

/// Index of the cancel tag closing a flag's tag sequence that starts at
/// `start`: one or more tag characters (U+E0020–E007E), then U+E007F.
fn flag_tags_end(chars: &[char], start: usize) -> Option<usize> {
    let tags = chars[start..]
        .iter()
        .take_while(|c| ('\u{E0020}'..='\u{E007E}').contains(*c))
        .count();
    let end = start + tags;
    (tags > 0 && chars.get(end) == Some(&CANCEL_TAG)).then_some(end)
}

/// Whether the ZWJ at `at` sits between two emoji: the scalar before it,
/// looking past variation selectors and skin-tone modifiers (🏳️, 👩🏽), and
/// the one right after it.
fn joins_emoji(chars: &[char], at: usize) -> bool {
    let decoration = |c: &&char| matches!(**c, '\u{FE0E}' | '\u{FE0F}' | '\u{1F3FB}'..='\u{1F3FF}');
    let before = chars[..at].iter().rev().find(|c| !decoration(c));
    before.is_some_and(|&c| is_emoji(c)) && chars.get(at + 1).is_some_and(|&c| is_emoji(c))
}

/// Whether the ZWNJ at `at` sits between two letters (`char::is_alphabetic`):
/// the scalar before it, looking past combining marks (Mn / Mc: a virama, a
/// vowel sign, a diacritic), and the one right after it. This is RFC 5892's
/// CONTEXTJ intent for ZWNJ (after a virama, or between joining letters)
/// without Unicode joining types, which std doesn't expose, so any two
/// letters qualify.
fn joins_letters(chars: &[char], at: usize) -> bool {
    let before = chars[..at]
        .iter()
        .rev()
        .find(|&&c| !in_table(&COMBINING_MARKS, c));
    before.is_some_and(|c| c.is_alphabetic())
        && chars.get(at + 1).is_some_and(|c| c.is_alphabetic())
}

/// The name a track gets from a user-entered `name` (see [`rename_track`]):
/// empty or automatic-form names become the automatic name for `position`.
fn chosen_track_name(kind: TrackKind, name: &str, position: usize) -> Result<String> {
    Ok(match clean_track_name(name)? {
        Some(custom) if !is_automatic_track_name(kind, custom) => custom.to_owned(),
        // Intentional: a typed "V7" makes the track automatic again and is
        // stored as its position ("V2"); there is no schema flag, the name's
        // form decides (follow-up decision 2026-11-22).
        _ => automatic_track_name(kind, position),
    })
}

/// Add a new empty track with a freshly allocated ID at the end of its
/// kind's group (see [`add_track_with_id`]).
pub fn add_track(seq: &Sequence, kind: TrackKind, name: impl Into<String>) -> Sequence {
    add_track_with_id(seq, TrackId::new(), kind, name)
}

/// Add a new empty track using the supplied ID, after the last track of its
/// kind: a video track becomes the new top layer and stays ahead of the
/// audio tracks, an audio track goes last. The name is taken verbatim (an
/// automatic one is renumbered); user-entered names go through
/// [`insert_track_with_id`], which validates them. The `EditLog` calls this
/// when re-applying a logged `AddTrack` so undo/redo round-trips are
/// faithful.
pub fn add_track_with_id(
    seq: &Sequence,
    track_id: TrackId,
    kind: TrackKind,
    name: impl Into<String>,
) -> Sequence {
    let mut track = Track::new(kind, name);
    track.id = track_id;
    let at = track_slot(seq, kind, kind_group(seq, kind).len());
    let mut out = seq.clone();
    out.tracks.insert(at, Arc::new(track));
    renumber_automatic_names(out)
}

/// Remove a track by ID, with its clips. Connections that referenced one of
/// those clips are dropped too. Refuses a locked track (lock policy above).
pub fn remove_track(seq: &Sequence, track_id: TrackId) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    let mut out = seq.clone();
    out.tracks.remove(idx);
    Ok(renumber_automatic_names(prune_dangling_connections(out)))
}

/// Indices into `seq.tracks` of every track of `kind`, in order.
fn kind_group(seq: &Sequence, kind: TrackKind) -> Vec<usize> {
    seq.tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.kind == kind)
        .map(|(i, _)| i)
        .collect()
}

/// Index into `seq.tracks` where a new track of `kind` lands so it ends up
/// at `position` within that kind's group (`position <= group length`;
/// callers validate). Past the group's last track means right after it; an
/// empty video group starts at 0 and an empty audio group at the end, so
/// video stays first.
fn track_slot(seq: &Sequence, kind: TrackKind, position: usize) -> usize {
    let group = kind_group(seq, kind);
    match (group.get(position), group.last(), kind) {
        (Some(&i), _, _) => i,
        (None, Some(&last), _) => last + 1,
        (None, None, TrackKind::Video) => 0,
        (None, None, TrackKind::Audio) => seq.tracks.len(),
    }
}

/// Insert a new empty track of `kind` with the supplied ID at `position`
/// within that kind's group (0 = first video / first audio track; the
/// group's length = after its last track). With no track of that kind yet,
/// a video track goes before every track and an audio track after them,
/// so video stays first. `name` follows the [`rename_track`] rules (an
/// empty name gets the automatic one). Rejects a duplicate ID, a position
/// past the end of the group, or an invalid name (see [`rename_track`]).
pub fn insert_track_with_id(
    seq: &Sequence,
    id: TrackId,
    kind: TrackKind,
    name: impl Into<String>,
    position: usize,
) -> Result<Sequence> {
    if seq.tracks.iter().any(|t| t.id == id) {
        return Err(TimelineError::invalid(format!("track {id} already exists")));
    }
    let group_len = kind_group(seq, kind).len();
    if position > group_len {
        return Err(TimelineError::invalid(format!(
            "track position {position} is past the end of the {group_len} {kind:?} track(s)"
        )));
    }
    let name = chosen_track_name(kind, &name.into(), position)?;
    let at = track_slot(seq, kind, position);
    let mut track = Track::new(kind, name);
    track.id = id;
    let mut out = seq.clone();
    out.tracks.insert(at, Arc::new(track));
    Ok(renumber_automatic_names(out))
}

/// Move a track to `to_position` within its kind's group (0 = bottom video
/// layer / first audio track). Only that group's tracks are reordered, so
/// tracks of the other kind keep their slots. Allowed on locked tracks (see
/// the lock policy above).
pub fn move_track(seq: &Sequence, id: TrackId, to_position: usize) -> Result<Sequence> {
    let (idx, track) = find_track(seq, id)?;
    let group = kind_group(seq, track.kind);
    if to_position >= group.len() {
        return Err(TimelineError::invalid(format!(
            "track position {to_position} is out of range for {} {:?} track(s)",
            group.len(),
            track.kind
        )));
    }
    let from = group
        .iter()
        .position(|&i| i == idx)
        .ok_or_else(|| TimelineError::invalid(format!("track {id} not in its kind group")))?;
    if from == to_position {
        return Ok(seq.clone());
    }
    let mut members: Vec<Arc<Track>> = group.iter().map(|&i| seq.tracks[i].clone()).collect();
    let moved = members.remove(from);
    members.insert(to_position, moved);
    let mut out = seq.clone();
    for (&slot, t) in group.iter().zip(members) {
        out.tracks[slot] = t;
    }
    Ok(renumber_automatic_names(out))
}

/// Set `Track.enabled` (the visibility toggle for video tracks). Allowed on
/// locked tracks, like mute/solo (see the lock policy above).
pub fn set_track_enabled(seq: &Sequence, id: TrackId, enabled: bool) -> Result<Sequence> {
    let (idx, track) = find_track(seq, id)?;
    if track.enabled == enabled {
        return Ok(seq.clone());
    }
    let mut new_track = (*track).clone();
    new_track.enabled = enabled;
    Ok(replace_track(seq, idx, new_track))
}

/// Lock or unlock a track (`Track.locked`; lock policy above). Always
/// allowed: unlocking is how a locked track becomes editable again.
pub fn set_track_locked(seq: &Sequence, id: TrackId, locked: bool) -> Result<Sequence> {
    let (idx, track) = find_track(seq, id)?;
    if track.locked == locked {
        return Ok(seq.clone());
    }
    let mut new_track = (*track).clone();
    new_track.locked = locked;
    Ok(replace_track(seq, idx, new_track))
}

/// Rename a track. `name` is trimmed; an empty name reverts to the
/// automatic positional name, and so does one in the automatic form (e.g.
/// "V7" on a video track is stored as its position, "V2"). Rejects names
/// over [`MAX_TRACK_NAME_CHARS`] characters or with control, line /
/// paragraph separator or format (Unicode Cf: zero-width, text-direction,
/// soft hyphen, tag, …) characters, except a ZWJ between two emoji, a 🏴
/// flag's tags and a ZWNJ between two letters. The other automatic names are
/// renumbered too, so stale ones from an older bundle can't end up
/// duplicated. Allowed on locked tracks (see the lock policy above).
pub fn rename_track(seq: &Sequence, id: TrackId, name: &str) -> Result<Sequence> {
    let (idx, track) = find_track(seq, id)?;
    let position = seq.tracks[..idx]
        .iter()
        .filter(|t| t.kind == track.kind)
        .count();
    let name = chosen_track_name(track.kind, name, position)?;
    if track.name == name {
        return Ok(seq.clone());
    }
    let mut new_track = (*track).clone();
    new_track.name = name;
    Ok(renumber_automatic_names(replace_track(seq, idx, new_track)))
}

/// Set `Clip.blend_mode`. Rejects locked tracks, like other clip params
/// (see the lock policy above).
pub fn set_clip_blend_mode(
    seq: &Sequence,
    track: TrackId,
    clip: ClipId,
    mode: BlendMode,
) -> Result<Sequence> {
    let (idx, tr) = find_track(seq, track)?;
    ensure_unlocked(&tr)?;
    let (clip_idx, c) = find_clip(&tr, clip)?;
    if c.blend_mode == mode {
        return Ok(seq.clone());
    }
    let mut updated = (*c).clone();
    updated.blend_mode = mode;
    let mut clips = tr.clips.clone();
    clips[clip_idx] = Arc::new(updated);
    Ok(replace_track(seq, idx, rebuild_track(&tr, clips)))
}

/// Smallest accepted canvas edge, in pixels.
const MIN_FRAME_EDGE: u32 = 16;
/// Largest accepted canvas edge, in pixels (the mograph/fusion raster cap).
const MAX_FRAME_EDGE: u32 = 16384;

/// Set (or clear, with `None`) the sequence's compositing canvas size. Each
/// edge must be in `[16, 16384]`.
pub fn set_frame_size(seq: &Sequence, size: Option<(u32, u32)>) -> Result<Sequence> {
    let size = match size {
        None => None,
        Some((w, h)) => {
            for (label, v) in [("width", w), ("height", h)] {
                if !(MIN_FRAME_EDGE..=MAX_FRAME_EDGE).contains(&v) {
                    return Err(TimelineError::invalid(format!(
                        "frame {label} {v} must be in [{MIN_FRAME_EDGE}, {MAX_FRAME_EDGE}]"
                    )));
                }
            }
            // Intentional: odd edges round DOWN to even — 4:2:0 encoders and
            // the YUV paths need even dimensions.
            Some((w & !1, h & !1))
        }
    };
    if seq.frame_size == size {
        return Ok(seq.clone());
    }
    let mut out = seq.clone();
    out.frame_size = size;
    Ok(out)
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
    Ok(replace_track(
        seq,
        idx,
        follow_cuts(&track, new_track, same_clip),
    ))
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
    Ok(replace_track(
        seq,
        idx,
        follow_cuts(&track, new_track, same_clip),
    ))
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
    // Which re-IDed piece now holds an overwritten clip's in / out edge
    // (`None`: the new clip covers it), for `follow_cuts`.
    let mut edges: std::collections::HashMap<(ClipId, Edge), Option<ClipId>> =
        std::collections::HashMap::new();
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
        edges.insert((c.id, Edge::In), None);
        edges.insert((c.id, Edge::Out), None);

        if left_kept {
            // Keep the part of `c` that ends at `new_in`.
            let kept_dur = sub(new_in, c_in);
            let mut left = (**c).clone();
            left.id = ClipId::new();
            left.source_out = add(c.source_in, kept_dur);
            edges.insert((c.id, Edge::In), Some(left.id));
            kept.push(Arc::new(left));
        }
        if right_kept {
            // Keep the part of `c` that starts at `new_out`.
            let trimmed_off = sub(new_out, c_in);
            let mut right = (**c).clone();
            right.id = ClipId::new();
            right.source_in = add(c.source_in, trimmed_off);
            right.timeline_in = new_out;
            edges.insert((c.id, Edge::Out), Some(right.id));
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
    let new_track = follow_cuts(&track, new_track, |id, edge| {
        edges.get(&(id, edge)).copied().unwrap_or(Some(id))
    });
    // Overwriting re-IDs trimmed survivors / removes covered clips, so any
    // connection to a vanished id is pruned (GOAT-6 integrity).
    Ok(prune_dangling_connections(replace_track(
        seq, idx, new_track,
    )))
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
    Ok(prune_dangling_connections(replace_track(
        seq,
        idx,
        follow_cuts(&track, new_track, same_clip),
    )))
}

/// Remove `clip_id` from `track_id` and shift every later clip on the
/// same track left by the removed clip's duration so the gap closes.
pub fn ripple_remove_clip(seq: &Sequence, track_id: TrackId, clip_id: ClipId) -> Result<Sequence> {
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
    Ok(prune_dangling_connections(replace_track(
        seq,
        idx,
        follow_cuts(&track, new_track, same_clip),
    )))
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
        return Err(TimelineError::invalid(
            "slide would move timeline_in negative",
        ));
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
    Ok(replace_track(
        seq,
        idx,
        follow_cuts(&track, new_track, same_clip),
    ))
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
    Ok(replace_track(
        seq,
        idx,
        follow_cuts(&track, new_track, same_clip),
    ))
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
    Ok(replace_track(
        seq,
        idx,
        follow_cuts(&track, new_track, same_clip),
    ))
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
pub fn set_track_gain_db(seq: &Sequence, track_id: TrackId, gain_db: f32) -> Result<Sequence> {
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
pub fn remove_audio_effect(seq: &Sequence, track_id: TrackId, index: usize) -> Result<Sequence> {
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
        return Err(TimelineError::invalid("fade duration must be non-negative"));
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
        frame_size: seq.frame_size,
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
    // The right half carries the clip's out-point, and with it the
    // transition there; the left half keeps the id and the in-point.
    let new_track = follow_cuts(&track, new_track, |id, edge| {
        Some(if id == clip_id && edge == Edge::Out {
            right_id
        } else {
            id
        })
    });
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

// A transition belongs to the cut between its two clips (decisions
// 2026-11-22, matching Premiere / Resolve):
// - It follows the cut: every op that rebuilds a track's clips runs
//   `follow_cuts`, which moves each transition to where its outgoing clip's
//   out-point and incoming clip's in-point now meet, and removes it once
//   they don't (a side removed, the two moved apart, a gap opened or a clip
//   put between them). Undo brings it back like any edit.
// - Length cap: it can't outlast either clip, so its window (centred on
//   the cut) never reaches past them — the outgoing side is never read
//   before its start nor the incoming side past its end, and the clip
//   after the incoming one is never hidden. Adding one clamps its length,
//   and `follow_cuts` clamps again when an edit shortens a side.
//   Intentional: the requested length isn't kept, so a clamped transition
//   stays short when its side grows again (clamp, don't remember).
// So the ops never leave an orphaned or over-long transition; the scene
// plan still caps on read for bundles saved by older builds.

/// A clip edge a transition hangs on: the incoming clip's in-point or the
/// outgoing clip's out-point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Edge {
    In,
    Out,
}

/// `follow_cuts` carrier for edits that keep every surviving clip's id.
fn same_clip(id: ClipId, _: Edge) -> Option<ClipId> {
    Some(id)
}

/// Re-anchor `old`'s transitions on `new`, the same track after an edit
/// rebuilt its clips (lock policy: callers only pass unlocked tracks).
/// `carrier(id, edge)` names the clip in `new` that now holds clip `id`'s
/// `edge` (`None` if the edit destroyed it); it is `id` itself unless the
/// edit re-IDed a piece (split, overwrite). A transition whose two edges
/// still meet moves to that cut and is capped; any other is removed, and so
/// is one that was already orphaned (it renders nothing and could otherwise
/// spring onto an unrelated cut). Unchanged transitions stay shared.
fn follow_cuts(
    old: &Track,
    mut new: Track,
    carrier: impl Fn(ClipId, Edge) -> Option<ClipId>,
) -> Track {
    let live = |id: ClipId| {
        new.clips
            .iter()
            .find(|c| c.id == id && c.duration() > RationalTime::ZERO)
    };
    let mut kept: Vec<Arc<Transition>> = Vec::with_capacity(old.transitions.len());
    for tr in &old.transitions {
        let (Some(from), Some(to)) = transition_sides(&old.clips, tr.position) else {
            continue;
        };
        let from = carrier(from.id, Edge::Out).and_then(live);
        let to = carrier(to.id, Edge::In).and_then(live);
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        if from.timeline_out() != to.timeline_in {
            continue;
        }
        let (position, duration) = (to.timeline_in, capped_duration(tr.duration, from, to));
        if (position, duration) == (tr.position, tr.duration) {
            kept.push(tr.clone());
        } else {
            let mut moved = (**tr).clone();
            moved.position = position;
            moved.duration = duration;
            kept.push(Arc::new(moved));
        }
    }
    kept.sort_by_key(|t| t.position);
    new.transitions = kept;
    new
}

/// Add a transition with a pre-allocated ID. EditLog replay calls this
/// so undo/redo produces a byte-identical sequence; UI / engine calls
/// go through the wrapper that allocates an ID for them. The duration is
/// clamped as [`add_transition_at_cut`] describes.
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
    if clip.duration() <= RationalTime::ZERO {
        return Err(TimelineError::invalid(
            "the clip before the cut has no length — nothing to transition from",
        ));
    }
    // A zero-length clip at the cut is never a side (scene-plan rule).
    let incoming = track.clips[clip_pos + 1..]
        .iter()
        .find(|c| c.duration() > RationalTime::ZERO)
        .ok_or_else(|| {
            TimelineError::invalid(
                "the clip after the cut has no length — nothing to transition to",
            )
        })?;
    if incoming.timeline_in != cut {
        return Err(TimelineError::invalid(
            "no clip starts at the cut — a transition needs two clips that meet",
        ));
    }
    let duration = capped_duration(duration, &clip, incoming);

    let new_t = Transition {
        id: transition_id,
        kind,
        position: cut,
        duration,
        params: std::collections::BTreeMap::new(),
    };
    // Against the windows as they render (capped; orphans render nothing).
    for existing in track.transitions.iter() {
        let Some((win_in, win_out)) = transition_window(&track, existing) else {
            continue;
        };
        if new_t.window_in() < win_out && win_in < new_t.window_out() {
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
/// and extends `duration / 2` to each side. `duration` is clamped to the
/// length of the anchor clip and of the next clip of positive length, so
/// the stored transition (in the returned sequence) can be shorter than
/// asked. Rejects when no clip starts where the anchor clip ends (none
/// after it, or a gap), when either side has no length, when `duration`
/// is zero, or when the new (clamped) transition would overlap an existing
/// one. The transition then follows that cut (see `follow_cuts`). Returns
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
        frame_size: seq.frame_size,
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
/// Refused while a locked track routes or sends to it, since that would
/// rewrite the locked track's routing (lock policy above [`add_track`]).
pub fn remove_bus(seq: &Sequence, bus_id: BusId) -> Result<Sequence> {
    find_bus(seq, bus_id)?;
    let uses_bus = |t: &Track| {
        t.output == BusOutput::Bus(bus_id) || t.sends.iter().any(|s| s.target == bus_id)
    };
    if let Some(t) = seq.tracks.iter().find(|t| t.locked && uses_bus(t)) {
        return Err(TimelineError::invalid(format!(
            "track \"{}\" is locked and uses this bus; unlock it first",
            t.name
        )));
    }
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
            if uses_bus(t) {
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

/// Route a track's main output (to master or a bus). Rejects a missing bus
/// and a locked track (routing is a stored mixer parameter).
pub fn route_track_to_bus(
    seq: &Sequence,
    track_id: TrackId,
    output: BusOutput,
) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    if let BusOutput::Bus(target) = output {
        find_bus(seq, target)?;
    }
    let mut nt = (*track).clone();
    nt.output = output;
    Ok(replace_track(seq, idx, nt))
}

/// Add an aux send from a track to a bus. Rejects a missing target bus and
/// a locked track.
pub fn add_track_send(seq: &Sequence, track_id: TrackId, send: Send) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
    find_bus(seq, send.target)?;
    let mut nt = (*track).clone();
    nt.sends.push(send);
    Ok(replace_track(seq, idx, nt))
}

/// Remove a track's send at `index`. No-op if the index is out of range.
/// Rejects a locked track.
pub fn remove_track_send(seq: &Sequence, track_id: TrackId, index: usize) -> Result<Sequence> {
    let (idx, track) = find_track(seq, track_id)?;
    ensure_unlocked(&track)?;
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
        return Err(TimelineError::invalid(
            "bus send would create a feedback cycle",
        ));
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
    let (_, track) = find_track(seq, track_id)?;
    // Up front, so a move that clamps to nothing is refused on a locked
    // track too (the other tracks in the group are checked below).
    ensure_unlocked(&track)?;
    let (_, clip) = find_clip(&track, clip_id)?;
    let mut delta = sub(new_timeline_in, clip.timeline_in);

    let moved: std::collections::HashSet<ClipId> = connected_subtree(&seq.connections, clip_id)
        .into_iter()
        .collect();

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
        // The dragged clip's own track passed the check above, so a locked
        // one here only holds clips connected to it.
        if t.locked {
            return Err(TimelineError::invalid(format!(
                "a clip connected to this one sits on track \"{}\", which is locked; \
                 unlock it first",
                t.name
            )));
        }
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
                if intervals_overlap(
                    a.timeline_in,
                    a.timeline_out(),
                    b.timeline_in,
                    b.timeline_out(),
                ) {
                    return Err(TimelineError::invalid(
                        "magnetic move would overlap an existing clip",
                    ));
                }
            }
        }
        new_clips.sort_by_key(|c| c.timeline_in);
        new_tracks.push(Arc::new(follow_cuts(
            t,
            rebuild_track(t, new_clips),
            same_clip,
        )));
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
        new_tracks.push(Arc::new(follow_cuts(
            t,
            rebuild_track(t, new_clips),
            same_clip,
        )));
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
        if t.clips
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
        new_tracks.push(Arc::new(follow_cuts(
            t,
            rebuild_track(t, new_clips),
            same_clip,
        )));
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
        assert_eq!(
            music_bus.output,
            BusOutput::Master,
            "Music re-routed to master"
        );
    }

    #[test]
    fn bus_edit_ops_undo_redo_round_trip() {
        use crate::bus::BusOutput;
        use crate::edit_log::{EditLog, EditOp};
        use crate::id::BusId;
        let seq = Sequence::new("S", FrameRate::FPS_24);
        let mut log = EditLog::new(seq);
        let tid = TrackId::new();
        log.apply(EditOp::AddTrack {
            track_id: tid,
            kind: TrackKind::Audio,
            name: "A1".into(),
        })
        .unwrap();
        let bus = BusId::new();
        log.apply(EditOp::AddBus {
            bus_id: bus,
            name: "Music".into(),
        })
        .unwrap();
        log.apply(EditOp::RouteTrackToBus {
            track_id: tid,
            output: BusOutput::Bus(bus),
        })
        .unwrap();
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
        assert_eq!(
            seq.buses
                .iter()
                .find(|b| b.id == music)
                .unwrap()
                .inserts
                .len(),
            1
        );

        // Bus send: Music → Reverb is fine; self-send + feedback rejected.
        let seq = add_bus_send(&seq, music, Send::to(reverb)).unwrap();
        assert_eq!(
            seq.buses
                .iter()
                .find(|b| b.id == music)
                .unwrap()
                .sends
                .len(),
            1
        );
        assert!(
            add_bus_send(&seq, music, Send::to(music)).is_err(),
            "self-send"
        );
        // Reverb → Music would close a loop (Music already sends to Reverb).
        assert!(
            add_bus_send(&seq, reverb, Send::to(music)).is_err(),
            "feedback"
        );
        let seq = remove_bus_send(&seq, music, 0).unwrap();
        assert_eq!(
            seq.buses
                .iter()
                .find(|b| b.id == music)
                .unwrap()
                .sends
                .len(),
            0
        );
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
        log.apply(EditOp::AddBus {
            bus_id: bus,
            name: "Music".into(),
        })
        .unwrap();
        log.apply(EditOp::AddBus {
            bus_id: rev,
            name: "Reverb".into(),
        })
        .unwrap();
        log.apply(EditOp::AddBusInsert {
            bus_id: bus,
            effect: AudioEffectSpec::default_eq6(),
        })
        .unwrap();
        log.apply(EditOp::AddBusSend {
            bus_id: bus,
            send: Send::to(rev),
        })
        .unwrap();
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
        assert!(
            clip.effects
                .iter()
                .find(|e| e.id == ids[0])
                .unwrap()
                .enabled
        );
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
            frame_size: seq.frame_size,
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
        assert_eq!(
            tclip.timeline_in,
            t(5),
            "connected clip dragged by same delta"
        );
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
            assert!(
                connected.contains(&c.id),
                "each half is connected to the anchor"
            );
        }
    }

    #[test]
    fn magnetic_edit_ops_undo_redo() {
        use crate::edit_log::{EditLog, EditOp};
        let (seq, v1, _v2, aid, tid, _m) = two_track_with_clips();
        let mut log = EditLog::new(seq);
        log.apply(EditOp::ConnectClip {
            anchor: aid,
            connected: tid,
        })
        .unwrap();
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
        assert_eq!(
            log.current().tracks[1].clips[0].timeline_in,
            t(1),
            "move undone"
        );
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
        log.apply(EditOp::SplitCaption {
            cue_id: cid,
            at_ms: 2000,
        })
        .unwrap();
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

    // ---- M1: track ops, connections, lock policy ----------------------------

    fn track_names(seq: &Sequence) -> Vec<&str> {
        seq.tracks.iter().map(|t| t.name.as_str()).collect()
    }

    fn lock(seq: &Sequence, id: TrackId) -> Sequence {
        let (idx, track) = find_track(seq, id).unwrap();
        let mut locked = (*track).clone();
        locked.locked = true;
        replace_track(seq, idx, locked)
    }

    #[test]
    fn add_track_keeps_video_ahead_of_audio() {
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        for (kind, name) in [
            (TrackKind::Audio, "A1"),
            (TrackKind::Video, "V1"),
            (TrackKind::Video, "V2"),
            (TrackKind::Audio, "A2"),
            (TrackKind::Video, "V3"),
        ] {
            seq = add_track(&seq, kind, name);
        }
        assert_eq!(track_names(&seq), ["V1", "V2", "V3", "A1", "A2"]);
    }

    #[test]
    fn add_track_shares_the_existing_tracks() {
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        seq = add_track(&seq, TrackKind::Video, "V1");
        seq = add_track(&seq, TrackKind::Audio, "A1");
        let (v1, a1) = (seq.tracks[0].clone(), seq.tracks[1].clone());
        let id = TrackId::new();
        let seq = add_track_with_id(&seq, id, TrackKind::Video, "V2");
        assert_eq!(seq.tracks[1].id, id);
        assert!(Arc::ptr_eq(&seq.tracks[0], &v1));
        assert!(Arc::ptr_eq(&seq.tracks[2], &a1));
    }

    #[test]
    fn add_track_on_a_misordered_sequence_lands_on_the_top_layer() {
        // An older bundle may hold [V1, A1, V2]. A new video track goes after
        // the last video track (the top layer), not before the first audio
        // track, which would slide it under V2.
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        for (kind, name) in [
            (TrackKind::Video, "V1"),
            (TrackKind::Audio, "A1"),
            (TrackKind::Video, "V2"),
        ] {
            seq.tracks.push(Arc::new(Track::new(kind, name)));
        }
        let seq = add_track(&seq, TrackKind::Video, "V3");
        assert_eq!(track_names(&seq), ["V1", "A1", "V2", "V3"]);
        let seq = add_track(&seq, TrackKind::Audio, "A2");
        assert_eq!(track_names(&seq), ["V1", "A1", "A2", "V2", "V3"]);
    }

    #[test]
    fn add_track_edit_op_replays_into_the_video_group() {
        use crate::edit_log::{EditLog, EditOp};
        let mut log = EditLog::new(Sequence::new("S", FrameRate::FPS_24));
        for (kind, name) in [
            (TrackKind::Video, "V1"),
            (TrackKind::Audio, "A1"),
            (TrackKind::Video, "V2"),
        ] {
            log.apply(EditOp::AddTrack {
                track_id: TrackId::new(),
                kind,
                name: name.into(),
            })
            .unwrap();
        }
        assert_eq!(track_names(log.current()), ["V1", "V2", "A1"]);
        log.undo().unwrap();
        assert_eq!(track_names(log.current()), ["V1", "A1"]);
        log.redo().unwrap();
        assert_eq!(track_names(log.current()), ["V1", "V2", "A1"]);
    }

    #[test]
    fn remove_track_prunes_connections_to_its_clips() {
        // V1: a [0,5), b [5,7). V2: t [1,3). Connections a→t and a→b.
        let (seq, v1, v2, aid, tid, media) = two_track_with_clips();
        let b = Clip::new(media, "b", t(5), t(0), t(2)).unwrap();
        let bid = b.id;
        let seq = insert_clip(&seq, v1, b).unwrap();
        let seq = connect_clip(&seq, aid, tid).unwrap();
        let seq = connect_clip(&seq, aid, bid).unwrap();

        let without_v2 = remove_track(&seq, v2).unwrap();
        assert_eq!(
            without_v2.connections,
            [Connection::new(aid, bid)],
            "only the connection to V2's clip goes"
        );
        let without_v1 = remove_track(&seq, v1).unwrap();
        assert!(
            without_v1.connections.is_empty(),
            "removing the anchor's track drops every connection from it"
        );
        assert!(remove_track(&seq, TrackId::new()).is_err());
    }

    #[test]
    fn remove_track_undo_restores_its_connections() {
        use crate::edit_log::{EditLog, EditOp};
        let (seq, _v1, v2, aid, tid, _media) = two_track_with_clips();
        let seq = connect_clip(&seq, aid, tid).unwrap();
        let mut log = EditLog::new(seq.clone());
        log.apply(EditOp::RemoveTrack { track_id: v2 }).unwrap();
        assert!(log.current().connections.is_empty());
        log.undo().unwrap();
        assert_eq!(log.current(), &seq);
    }

    #[test]
    fn locked_track_can_be_hidden_and_moved() {
        let (seq, v1, media) = seed_seq();
        let seq = add_track(&seq, TrackKind::Video, "V2");
        let seq = add_track(&seq, TrackKind::Video, "V3");
        let v3 = seq.tracks[2].id;
        let c = Clip::new(media, "c", t(0), t(0), t(5)).unwrap();
        let seq = lock(&insert_clip(&seq, v1, c).unwrap(), v1);
        let contents = |s: &Sequence| {
            let (_, tr) = find_track(s, v1).unwrap();
            (tr.clips.clone(), tr.transitions.clone(), tr.locked)
        };
        let before = contents(&seq);

        let v2 = seq.tracks[1].id;
        let hidden = set_track_enabled(&seq, v1, false).unwrap();
        assert!(!find_track(&hidden, v1).unwrap().1.enabled);
        let moved = move_track(&seq, v1, 2).unwrap();
        assert_eq!(track_ids(&moved), [v2, v3, v1]);
        // Moving another track past the locked one shifts it; also allowed.
        let under = move_track(&seq, v3, 0).unwrap();
        assert_eq!(track_ids(&under), [v3, v1, v2]);
        for s in [&hidden, &moved, &under] {
            assert_eq!(contents(s), before, "the lock and contents are kept");
            assert_eq!(track_names(s), ["V1", "V2", "V3"], "names follow position");
        }
    }

    #[test]
    fn locked_track_rejects_blend_mode_only_on_its_own_clips() {
        let (seq, v1, v2, aid, tid, _media) = two_track_with_clips();
        let seq = lock(&seq, v1);
        let err = set_clip_blend_mode(&seq, v1, aid, BlendMode::Screen).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid argument: track \"V1\" is locked; unlock it first"
        );
        // Even a no-op change is rejected, like the other clip params.
        assert!(set_clip_blend_mode(&seq, v1, aid, BlendMode::Normal).is_err());
        let seq = set_clip_blend_mode(&seq, v2, tid, BlendMode::Screen).unwrap();
        assert_eq!(seq.tracks[1].clips[0].blend_mode, BlendMode::Screen);
    }

    #[test]
    fn locks_do_not_block_adding_tracks_or_sizing_the_canvas() {
        let (seq, v1, _media) = seed_seq();
        let seq = lock(&seq, v1);
        let v0 = TrackId::new();
        let seq = insert_track_with_id(&seq, v0, TrackKind::Video, "V0", 0).unwrap();
        let seq = add_track(&seq, TrackKind::Video, "V2");
        assert_eq!(track_ids(&seq)[..2], [v0, v1]);
        // The locked track is renumbered V1 → V2 and keeps its lock.
        assert_eq!(track_names(&seq), ["V1", "V2", "V3"]);
        assert!(find_track(&seq, v1).unwrap().1.locked);
        assert!(set_frame_size(&seq, Some((1280, 720))).is_ok());
    }

    fn track_ids(seq: &Sequence) -> Vec<TrackId> {
        seq.tracks.iter().map(|t| t.id).collect()
    }

    /// Video `[V1, BG, V3]` + audio `[A1, Music]`, built bottom → top.
    fn mixed_names() -> Sequence {
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        for (kind, name) in [
            (TrackKind::Video, "V1"),
            (TrackKind::Video, "BG"),
            (TrackKind::Video, "V3"),
            (TrackKind::Audio, "A1"),
            (TrackKind::Audio, "Music"),
        ] {
            seq = add_track(&seq, kind, name);
        }
        assert_eq!(track_names(&seq), ["V1", "BG", "V3", "A1", "Music"]);
        seq
    }

    #[test]
    fn inserting_tracks_renumbers_automatic_names_only() {
        let seq = mixed_names();
        let ids = track_ids(&seq);
        let insert = |s: &Sequence, kind, name: &str, position| {
            insert_track_with_id(s, TrackId::new(), kind, name, position).unwrap()
        };

        // Below everything: every automatic video name moves up one.
        let below = insert(&seq, TrackKind::Video, "", 0);
        assert_eq!(track_names(&below), ["V1", "V2", "BG", "V4", "A1", "Music"]);
        assert_eq!(track_ids(&below)[1..], ids[..]);
        // In the middle, with a custom name: only the tracks above shift.
        let middle = insert(&seq, TrackKind::Video, "Titles", 2);
        assert_eq!(
            track_names(&middle),
            ["V1", "BG", "Titles", "V4", "A1", "Music"]
        );
        // At the top, with an automatic-form name: it gets its position.
        let top = insert(&seq, TrackKind::Video, "V9", 3);
        assert_eq!(track_names(&top), ["V1", "BG", "V3", "V4", "A1", "Music"]);
        assert!(
            Arc::ptr_eq(&top.tracks[0], &seq.tracks[0]),
            "unchanged names stay shared"
        );
        // Audio is numbered on its own; the video names are untouched.
        let audio = insert(&seq, TrackKind::Audio, "A7", 0);
        assert_eq!(track_names(&audio), ["V1", "BG", "V3", "A1", "A2", "Music"]);
        let appended = add_track(&seq, TrackKind::Audio, "A1");
        assert_eq!(
            track_names(&appended),
            ["V1", "BG", "V3", "A1", "Music", "A3"]
        );
    }

    #[test]
    fn moving_and_removing_tracks_renumbers_automatic_names_only() {
        let seq = mixed_names();
        let [v1, bg, v3, a1, music] = track_ids(&seq)[..] else {
            unreachable!()
        };
        // V1 up to the top: the others drop one slot.
        let up = move_track(&seq, v1, 2).unwrap();
        assert_eq!(track_ids(&up), [bg, v3, v1, a1, music]);
        assert_eq!(track_names(&up), ["BG", "V2", "V3", "A1", "Music"]);
        // The top track down to the bottom.
        let down = move_track(&seq, v3, 0).unwrap();
        assert_eq!(track_ids(&down), [v3, v1, bg, a1, music]);
        assert_eq!(track_names(&down), ["V1", "V2", "BG", "A1", "Music"]);
        // Audio reorders within its own group.
        let audio = move_track(&seq, music, 0).unwrap();
        assert_eq!(track_names(&audio), ["V1", "BG", "V3", "Music", "A2"]);

        let without_bottom = remove_track(&seq, v1).unwrap();
        assert_eq!(track_names(&without_bottom), ["BG", "V2", "A1", "Music"]);
        let without_custom = remove_track(&seq, bg).unwrap();
        assert_eq!(track_names(&without_custom), ["V1", "V2", "A1", "Music"]);
        let without_a1 = remove_track(&seq, a1).unwrap();
        assert_eq!(track_names(&without_a1), ["V1", "BG", "V3", "Music"]);
    }

    #[test]
    fn automatic_names_need_the_kind_prefix_and_ascii_digits() {
        // Only `V<digits>` on video / `A<digits>` on audio is automatic.
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        for name in ["v2", "V", "V2 copy", "A1", "V٣", "V01"] {
            seq = add_track(&seq, TrackKind::Video, name);
        }
        seq = add_track(&seq, TrackKind::Audio, "V1");
        assert_eq!(
            track_names(&seq),
            ["v2", "V", "V2 copy", "A1", "V٣", "V6", "V1"]
        );
    }

    #[test]
    fn remove_track_refuses_a_locked_track() {
        use crate::edit_log::{EditLog, EditOp};
        let (seq, v1, v2, _aid, _tid, _media) = two_track_with_clips();
        let locked = lock(&seq, v2);
        let err = remove_track(&locked, v2).unwrap_err();
        assert!(
            err.to_string()
                .contains("track \"V2\" is locked; unlock it first"),
            "{err}"
        );
        // Other tracks can still go, and the lock moves with the renumbering.
        let without_v1 = remove_track(&locked, v1).unwrap();
        assert_eq!(track_names(&without_v1), ["V1"]);
        assert!(without_v1.tracks[0].locked);

        let mut log = EditLog::new(locked.clone());
        assert!(log.apply(EditOp::RemoveTrack { track_id: v2 }).is_err());
        assert!(log.is_empty(), "a refused removal isn't recorded");
        assert_eq!(log.current(), &locked);
        // Unlocked, it can be removed.
        assert!(remove_track(&seq, v2).is_ok());
    }

    #[test]
    fn rename_track_trims_validates_and_reverts_to_the_automatic_name() {
        let seq = mixed_names();
        let [v1, bg, v3, _a1, music] = track_ids(&seq)[..] else {
            unreachable!()
        };
        let name_of = |s: &Sequence, id| find_track(s, id).unwrap().1.name.clone();

        let titled = rename_track(&seq, v3, "  Titles  ").unwrap();
        assert_eq!(name_of(&titled, v3), "Titles");
        assert!(Arc::ptr_eq(&titled.tracks[0], &seq.tracks[0]));
        // Empty or blank reverts to the positional name.
        for blank in ["", "   ", "\t"] {
            assert_eq!(
                name_of(&rename_track(&titled, v3, blank).unwrap(), v3),
                "V3"
            );
            assert_eq!(name_of(&rename_track(&seq, bg, blank).unwrap(), bg), "V2");
        }
        // An automatic-form name is stored as the position and then follows it.
        let auto = rename_track(&seq, bg, "V7").unwrap();
        assert_eq!(name_of(&auto, bg), "V2");
        let moved = move_track(&auto, bg, 2).unwrap();
        assert_eq!(track_names(&moved), ["V1", "V2", "V3", "A1", "Music"]);
        assert_eq!(name_of(&moved, bg), "V3");
        // A name in the other kind's form is custom.
        assert_eq!(name_of(&rename_track(&seq, v1, "A1").unwrap(), v1), "A1");
        assert_eq!(
            name_of(&rename_track(&seq, music, "A9").unwrap(), music),
            "A2"
        );
        // Same name: no change.
        assert_eq!(rename_track(&seq, bg, "BG").unwrap(), seq);

        // The limit counts characters, not bytes.
        let max = "é".repeat(MAX_TRACK_NAME_CHARS);
        assert_eq!(name_of(&rename_track(&seq, bg, &max).unwrap(), bg), max);
        let err = rename_track(&seq, bg, &format!("{max}x")).unwrap_err();
        assert!(err.to_string().contains("limit is 64"), "{err}");
        assert!(rename_track(&seq, bg, "two\nlines").is_err());
        assert!(rename_track(&seq, TrackId::new(), "X").is_err());

        // Allowed on a locked track: the name is a label, not content.
        let locked = lock(&seq, bg);
        let renamed = rename_track(&locked, bg, "Plate").unwrap();
        assert_eq!(name_of(&renamed, bg), "Plate");
        assert!(find_track(&renamed, bg).unwrap().1.locked);
    }

    #[test]
    fn insert_track_validates_its_name_like_rename() {
        let seq = mixed_names();
        let insert =
            |name: &str| insert_track_with_id(&seq, TrackId::new(), TrackKind::Audio, name, 2);
        assert_eq!(track_names(&insert("  Pad ").unwrap())[5], "Pad");
        assert_eq!(track_names(&insert("").unwrap())[5], "A3");
        assert!(insert(&"x".repeat(MAX_TRACK_NAME_CHARS + 1)).is_err());
        assert!(insert("a\u{0}b").is_err());
    }

    #[test]
    fn rename_track_renumbers_stale_automatic_names() {
        // An older bundle can hold automatic names that no longer state
        // their positions; a rename must not leave two tracks called V2.
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        for (kind, name) in [
            (TrackKind::Video, "V2"),
            (TrackKind::Video, "V3"),
            (TrackKind::Audio, "A1"),
        ] {
            seq.tracks.push(Arc::new(Track::new(kind, name)));
        }
        let second = seq.tracks[1].id;
        let automatic = rename_track(&seq, second, "").unwrap();
        assert_eq!(track_names(&automatic), ["V1", "V2", "A1"]);
        let custom = rename_track(&seq, second, "Titles").unwrap();
        assert_eq!(track_names(&custom), ["V1", "Titles", "A1"]);
    }

    #[test]
    fn track_names_refuse_invisible_and_direction_characters() {
        let seq = mixed_names();
        let bg = seq.tracks[1].id;
        let line = "line or paragraph separators";
        let zero = "zero-width characters";
        let bidi = "text-direction controls";
        let control = "control characters";
        let format = "invisible formatting characters";
        let refused = [
            ('\u{2028}', line),
            ('\u{2029}', line),
            ('\u{200B}', zero),
            // U+200C between letters is allowed (Persian orthography); its
            // refused positions are in track_names_allow_zwnj_between_letters_only.
            ('\u{200D}', zero),
            ('\u{2060}', zero),
            ('\u{FEFF}', zero),
            ('\u{202A}', bidi),
            ('\u{202B}', bidi),
            ('\u{202C}', bidi),
            ('\u{202D}', bidi),
            ('\u{202E}', bidi),
            ('\u{2066}', bidi),
            ('\u{2067}', bidi),
            ('\u{2068}', bidi),
            ('\u{2069}', bidi),
            ('\u{200E}', bidi),
            ('\u{200F}', bidi),
            ('\u{061C}', bidi),
            ('\t', control),
            ('\n', control),
            ('\u{7}', control),
            ('\u{85}', control),
            // The rest of the format category (Cf).
            ('\u{00AD}', format),
            ('\u{0600}', format),
            ('\u{180E}', format),
            ('\u{2061}', format),
            ('\u{2062}', format),
            ('\u{2063}', format),
            ('\u{2064}', format),
            ('\u{206A}', format),
            ('\u{206F}', format),
            ('\u{FFF9}', format),
            ('\u{1D173}', format),
            ('\u{E0001}', format),
            ('\u{E0041}', format),
            ('\u{E007F}', format),
        ];
        for (c, what) in refused {
            let name = format!("Plate{c}B");
            let err = rename_track(&seq, bg, &name).unwrap_err().to_string();
            let code = format!("U+{:04X}", c as u32);
            assert!(err.contains(what) && err.contains(&code), "{code}: {err}");
            assert!(insert_track_with_id(&seq, TrackId::new(), TrackKind::Video, name, 0).is_err());
        }
        // A zero-width or direction character at an edge isn't whitespace,
        // so trimming doesn't hide it.
        assert!(rename_track(&seq, bg, "\u{FEFF}Plate").is_err());
        assert!(rename_track(&seq, bg, "Plate\u{202E}").is_err());
        assert!(rename_track(&seq, bg, "Plate\u{200F}").is_err());
        // Ordinary text in any script is fine, right-to-left included.
        for ok in [
            "Plate é",
            "日本語",
            "שלום",
            "مرحبا",
            "قناة ٣",
            "ܫܠܡܐ",
            "Plate شاشة 2",
            "Ünïcödé — B",
            "a-b_c (2)",
        ] {
            let renamed = rename_track(&seq, bg, ok).unwrap();
            assert_eq!(find_track(&renamed, bg).unwrap().1.name, ok);
        }
    }

    #[test]
    fn track_names_allow_emoji_sequences_and_nothing_else_invisible() {
        let seq = mixed_names();
        let bg = seq.tracks[1].id;
        let scotland = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}";
        let wales = "\u{1F3F4}\u{E0067}\u{E0062}\u{E0077}\u{E006C}\u{E0073}\u{E007F}";
        let technologist = "\u{1F9D1}\u{200D}\u{1F4BB}"; // 🧑‍💻
        let allowed = [
            technologist.to_owned(),
            "\u{1F3F3}\u{FE0F}\u{200D}\u{1F308}".to_owned(), // 🏳️‍🌈
            "\u{1F469}\u{1F3FD}\u{200D}\u{1F52C}".to_owned(), // 👩🏽‍🔬
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}".to_owned(), // family
            "\u{2764}\u{FE0F}\u{200D}\u{1F525}".to_owned(),  // ❤️‍🔥
            "\u{1F9D4}\u{1F3FB}\u{200D}\u{2642}\u{FE0F}".to_owned(), // 🧔🏻‍♂️
            scotland.to_owned(),
            format!("Plate {wales} B"),
            format!("Edit {technologist} {scotland}"),
            // Variation selectors and skin-tone modifiers aren't format
            // characters.
            "\u{263A}\u{FE0F} a\u{FE0E}".to_owned(),
            "\u{1F3FB}".to_owned(),
        ];
        for ok in allowed {
            let renamed =
                rename_track(&seq, bg, &ok).unwrap_or_else(|e| panic!("{ok:?} refused: {e}"));
            assert_eq!(find_track(&renamed, bg).unwrap().1.name, ok);
        }
        let zero = "zero-width characters";
        let format = "invisible formatting characters";
        let refused = [
            // ZWJ between letters, at either end, next to a non-emoji, after
            // only a variation selector, doubled.
            ("a\u{200D}b", '\u{200D}', zero),
            ("\u{200D}\u{1F4BB}", '\u{200D}', zero),
            ("\u{1F4BB}\u{200D}", '\u{200D}', zero),
            ("\u{1F9D1}\u{200D}b", '\u{200D}', zero),
            ("a\u{200D}\u{1F4BB}", '\u{200D}', zero),
            ("\u{FE0F}\u{200D}\u{1F4BB}", '\u{200D}', zero),
            ("\u{1F9D1}\u{200D}\u{200D}\u{1F4BB}", '\u{200D}', zero),
            ("\u{1F9D1}\u{200D}\u{1F3FB}", '\u{200D}', zero),
            // Tags: alone, unterminated, without the black flag, no tags
            // before the cancel tag, a language tag in the sequence.
            ("tag \u{E0067}", '\u{E0067}', format),
            ("\u{1F3F4}\u{E0067}\u{E0062}", '\u{E0067}', format),
            ("\u{1F3F3}\u{E0067}\u{E007F}", '\u{E0067}', format),
            ("x\u{E007F}", '\u{E007F}', format),
            ("\u{1F3F4}\u{E007F}", '\u{E007F}', format),
            ("\u{1F3F4}\u{E0001}\u{E0067}\u{E007F}", '\u{E0001}', format),
        ];
        for (name, c, what) in refused {
            let err = rename_track(&seq, bg, name).unwrap_err().to_string();
            let code = format!("U+{:04X}", c as u32);
            assert!(err.contains(what) && err.contains(&code), "{name:?}: {err}");
        }
    }

    #[test]
    fn track_names_allow_zwnj_between_letters_only() {
        let seq = mixed_names();
        let bg = seq.tracks[1].id;
        let nameha = "\u{0646}\u{0627}\u{0645}\u{0647}\u{200C}\u{0647}\u{0627}"; // نامه‌ها
        let allowed = [
            "\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0647}\u{0645}".to_owned(), // می‌خواهم
            nameha.to_owned(),
            "\u{06A9}\u{062A}\u{0627}\u{0628}\u{200C}\u{062E}\u{0627}\u{0646}\u{0647}".to_owned(), // کتاب‌خانه
            format!("Plate {nameha} 2"),
            // Combining marks before it are looked past: a Hindi virama
            // (an explicit half form, क्‌ष) and an acute accent.
            "\u{0915}\u{094D}\u{200C}\u{0937}".to_owned(),
            "e\u{0301}\u{200C}b".to_owned(),
        ];
        for ok in allowed {
            let renamed =
                rename_track(&seq, bg, &ok).unwrap_or_else(|e| panic!("{ok:?} refused: {e}"));
            assert_eq!(find_track(&renamed, bg).unwrap().1.name, ok);
        }
        let refused = [
            "\u{200C}\u{0646}\u{0627}", // leading
            "\u{0646}\u{0627}\u{200C}", // trailing
            "a\u{200C}1",               // before a digit
            "1\u{200C}a",               // after a digit
            "\u{06F1}\u{200C}\u{0627}", // after a Persian digit
            "a \u{200C}b",              // after a space
            "a\u{200C} b",              // before a space
            "a\u{200C}\u{1F4BB}",       // before an emoji
            "\u{1F4BB}\u{200C}a",       // after an emoji
            "a\u{200C}\u{200C}b",       // doubled
            "\u{0301}\u{200C}a",        // only a mark before it
            "a\u{200C}\u{0301}",        // a mark, not a letter, after it
        ];
        for name in refused {
            let err = rename_track(&seq, bg, name).unwrap_err().to_string();
            assert!(
                err.contains("zero-width characters") && err.contains("U+200C"),
                "{name:?}: {err}"
            );
        }
    }

    #[test]
    fn skin_tones_are_modifiers_not_emoji() {
        // So a ZWJ right before one doesn't join two emoji.
        assert!(is_emoji('\u{1F9D1}') && is_emoji('\u{2642}'));
        for c in '\u{1F3FB}'..='\u{1F3FF}' {
            assert!(!is_emoji(c), "U+{:04X}", c as u32);
        }
    }

    #[test]
    fn format_char_table_is_unicode_16_cf() {
        // Unicode 16.0.0 has 170 Cf code points in these 21 ranges.
        let mut count = 0;
        for pair in FORMAT_CHARS.windows(2) {
            assert!(pair[0].1 < pair[1].0, "sorted, apart: {pair:?}");
        }
        for &(lo, hi) in &FORMAT_CHARS {
            assert!(lo <= hi);
            for c in lo..=hi {
                count += 1;
                assert!(refused_name_char(c).is_some(), "U+{:04X}", c as u32);
            }
            // The neighbours of each range are outside Cf; only U+2029
            // (before U+202A) is refused, as a paragraph separator.
            for n in [lo as u32 - 1, hi as u32 + 1] {
                let n = char::from_u32(n).unwrap();
                let want = (n == '\u{2029}').then_some("line or paragraph separators");
                assert_eq!(refused_name_char(n), want, "U+{:04X}", n as u32);
            }
        }
        assert_eq!(count, 170);
    }

    #[test]
    fn locked_track_refuses_mixer_routing_changes() {
        let (seq, tid, _) = seed_audio_seq();
        let bus = BusId::new();
        let seq = add_bus_with_id(&seq, bus, "Music");
        let seq = add_track_send(&seq, tid, Send::to(bus)).unwrap();
        let locked = lock_first_track_for_test(&seq);
        let attempts = [
            route_track_to_bus(&locked, tid, BusOutput::Bus(bus)),
            add_track_send(&locked, tid, Send::to(bus)),
            remove_track_send(&locked, tid, 0),
        ];
        for result in attempts {
            let err = result.unwrap_err();
            assert!(
                err.to_string()
                    .ends_with("track \"A1\" is locked; unlock it first"),
                "{err}"
            );
        }
        // The same edits work once the track is unlocked.
        let unlocked = set_track_locked(&locked, tid, false).unwrap();
        let routed = route_track_to_bus(&unlocked, tid, BusOutput::Bus(bus)).unwrap();
        assert_eq!(routed.tracks[0].output, BusOutput::Bus(bus));
        assert_eq!(
            remove_track_send(&unlocked, tid, 0).unwrap().tracks[0]
                .sends
                .len(),
            0
        );
        assert_eq!(
            add_track_send(&unlocked, tid, Send::to(bus))
                .unwrap()
                .tracks[0]
                .sends
                .len(),
            2
        );
    }

    #[test]
    fn magnetic_move_refuses_a_locked_track_even_when_clamped_to_nothing() {
        // Moving a clip at 0 to before 0 clamps to no move; on a locked
        // track it is still refused, like every other clip edit there.
        let (seq, v1, _v2, aid, _tid, _m) = two_track_with_clips();
        let locked = lock(&seq, v1);
        let before = RationalTime::new(-1, 2).unwrap();
        for to in [before, t(0), t(3)] {
            let err = magnetic_move_clip(&locked, v1, aid, to).unwrap_err();
            assert!(
                err.to_string()
                    .ends_with("track \"V1\" is locked; unlock it first"),
                "{err}"
            );
        }
        assert_eq!(magnetic_move_clip(&seq, v1, aid, before).unwrap(), seq);
    }

    #[test]
    fn magnetic_move_names_the_locked_track_its_connected_clip_is_on() {
        // a on V1 drags t on V2 with it; V2 is locked, so the move is
        // refused and the message points at V2, not at a's own track.
        let (seq, v1, v2, aid, tid, _m) = two_track_with_clips();
        let seq = connect_clip(&seq, aid, tid).unwrap();
        let locked = lock(&seq, v2);
        let err = magnetic_move_clip(&locked, v1, aid, t(4)).unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid argument: a clip connected to this one sits on track \"V2\", \
             which is locked; unlock it first"
        );
        assert!(magnetic_move_clip(&seq, v1, aid, t(4)).is_ok());
    }

    #[test]
    fn remove_bus_refuses_to_reroute_a_locked_track() {
        // Removing a bus reroutes the tracks that use it, so a locked
        // track routed or sending to it blocks the removal.
        let (seq, tid, _) = seed_audio_seq();
        let seq = add_track(&seq, TrackKind::Audio, "A2");
        let other = seq.tracks[1].id;
        let (music, reverb, spare) = (BusId::new(), BusId::new(), BusId::new());
        let mut seq = seq;
        for (bus, name) in [(music, "Music"), (reverb, "Reverb"), (spare, "Spare")] {
            seq = add_bus_with_id(&seq, bus, name);
        }
        let seq = route_track_to_bus(&seq, tid, BusOutput::Bus(music)).unwrap();
        let seq = add_track_send(&seq, tid, Send::to(reverb)).unwrap();
        let seq = route_track_to_bus(&seq, other, BusOutput::Bus(spare)).unwrap();
        let locked = lock_first_track_for_test(&seq);
        for bus in [music, reverb] {
            let err = remove_bus(&locked, bus).unwrap_err();
            assert!(err.to_string().contains("\"A1\" is locked"), "{err}");
        }
        // A bus the locked track doesn't use can go; the unlocked user of
        // it falls back to master.
        let without_spare = remove_bus(&locked, spare).unwrap();
        assert_eq!(without_spare.tracks[1].output, BusOutput::Master);
        assert!(Arc::ptr_eq(&without_spare.tracks[0], &locked.tracks[0]));
        // Unlocked, the removal reroutes it as before.
        let without_music = remove_bus(&seq, music).unwrap();
        assert_eq!(without_music.tracks[0].output, BusOutput::Master);
        assert_eq!(remove_bus(&seq, reverb).unwrap().tracks[0].sends.len(), 0);
    }

    // ---- transition length cap ----------------------------------------------

    fn half(num: i64) -> RationalTime {
        RationalTime::new(num, 2).unwrap()
    }

    /// V1 holding clips of the given lengths laid end to end from 0, each
    /// reading its source from 0.
    fn butted(lens: &[i64]) -> (Sequence, TrackId, MediaItemId, Vec<ClipId>) {
        let (mut seq, v1, media) = seed_seq();
        let mut ids = Vec::new();
        let mut at = 0;
        for &len in lens {
            let c = Clip::new(media, "c", t(at), t(0), t(len)).unwrap();
            ids.push(c.id);
            seq = insert_clip(&seq, v1, c).unwrap();
            at += len;
        }
        (seq, v1, media, ids)
    }

    fn durations(seq: &Sequence) -> Vec<RationalTime> {
        seq.tracks[0]
            .transitions
            .iter()
            .map(|tr| tr.duration)
            .collect()
    }

    fn dissolve(seq: &Sequence, v1: TrackId, after: ClipId, len: RationalTime) -> Sequence {
        add_transition_at_cut(seq, v1, after, TransitionKind::Dissolve, len)
            .unwrap()
            .0
    }

    #[test]
    fn transition_is_capped_at_the_shorter_neighbour() {
        use crate::scene_plan::{scene_plan_at, LayerContent};
        // a [0,5) | b [5,6) | c [6,10): a 4 s dissolve after a would run to
        // 7, hiding c at 6.5 and reading b past its end.
        let (seq, v1, _, ids) = butted(&[5, 1, 4]);
        let (capped, id) =
            add_transition_at_cut(&seq, v1, ids[0], TransitionKind::Dissolve, t(4)).unwrap();
        let tr = &capped.tracks[0].transitions[0];
        assert_eq!((tr.id, tr.duration), (id, t(1)));
        assert_eq!((tr.window_in(), tr.window_out()), (half(9), half(11)));
        let plan = scene_plan_at(&capped, RationalTime::new(13, 2).unwrap());
        match &plan.layers[0].content {
            LayerContent::Clip(c) => assert_eq!(c.clip_id, ids[2], "c shows at 6.5"),
            other => panic!("expected clip c, got {other:?}"),
        }
        // The capped window leaves room for a dissolve after b (capped too).
        let both = dissolve(&capped, v1, ids[1], t(4));
        assert_eq!(durations(&both), [t(1), t(1)]);
        // The outgoing side caps it as well; one that fits is kept.
        let (seq, v1, _, ids) = butted(&[2, 5]);
        assert_eq!(durations(&dissolve(&seq, v1, ids[0], t(3))), [t(2)]);
        assert_eq!(durations(&dissolve(&seq, v1, ids[0], t(2))), [t(2)]);
        assert_eq!(durations(&dissolve(&seq, v1, ids[0], half(3))), [half(3)]);
        // Replaying the logged op reproduces the capped transition.
        let mut log = crate::edit_log::EditLog::new(seq.clone());
        let op = crate::edit_log::EditOp::AddTransitionAtCut {
            track_id: v1,
            transition_id: TransitionId::new(),
            after_clip_id: ids[0],
            kind: TransitionKind::Dissolve,
            duration: t(9),
        };
        log.apply(op.clone()).unwrap();
        assert_eq!(durations(log.current()), [t(2)]);
        log.undo().unwrap();
        log.redo().unwrap();
        assert_eq!(log.current(), &crate::edit_log::apply(&seq, &op).unwrap());
    }

    #[test]
    fn transition_cap_skips_zero_length_clips_and_rejects_no_length() {
        // [a, z, b] with z = [5,5): b is the incoming side, so b caps it.
        let (seq, v1, media) = seed_seq();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let z = Clip::new(media, "z", t(5), t(0), t(0)).unwrap();
        let b = Clip::new(media, "b", t(5), t(0), t(1)).unwrap();
        let (a_id, z_id) = (a.id, z.id);
        let mut seq = seq;
        for c in [a, z, b] {
            seq = insert_clip(&seq, v1, c).unwrap();
        }
        assert_eq!(durations(&dissolve(&seq, v1, a_id, t(4))), [t(1)]);
        // A zero-length anchor has no length to dissolve from.
        let err = add_transition_at_cut(&seq, v1, z_id, TransitionKind::Dissolve, t(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no length"), "{err}");
        // Only a zero-length clip after the anchor: nothing to dissolve to.
        let (mut seq, v1, media) = seed_seq();
        let a = Clip::new(media, "a", t(0), t(0), t(5)).unwrap();
        let a_id = a.id;
        seq = insert_clip(&seq, v1, a).unwrap();
        seq = insert_clip(&seq, v1, Clip::new(media, "z", t(5), t(0), t(0)).unwrap()).unwrap();
        assert!(add_transition_at_cut(&seq, v1, a_id, TransitionKind::Dissolve, t(1)).is_err());
    }

    #[test]
    fn edits_that_shorten_a_side_cap_its_transition() {
        // a [0,5) | b [5,10), a 4 s dissolve at 5 (both sides are 5 s).
        let (seq, v1, media, ids) = butted(&[5, 5]);
        let seq = dissolve(&seq, v1, ids[0], t(4));
        assert_eq!(durations(&seq), [t(4)]);
        // b's out-point trimmed to 2 s.
        let trimmed = trim_out(&seq, v1, ids[1], t(2)).unwrap();
        assert_eq!(durations(&trimmed), [t(2)]);
        // A capped transition doesn't grow back when the side does.
        assert_eq!(
            durations(&trim_out(&trimmed, v1, ids[1], t(5)).unwrap()),
            [t(2)]
        );
        // a's in-point trimmed: a = [3,5).
        assert_eq!(durations(&trim_in(&seq, v1, ids[0], t(3)).unwrap()), [t(2)]);
        // a split at 4.5: its right half [4.5,5) is the outgoing side.
        assert_eq!(
            durations(&split(&seq, v1, ids[0], half(9)).unwrap()),
            [half(1)]
        );
        // Overwriting [7,10) leaves b as [5,7).
        let x = Clip::new(media, "x", t(7), t(0), t(3)).unwrap();
        assert_eq!(durations(&overwrite_clip(&seq, v1, x).unwrap()), [t(2)]);
        // An edit that leaves both sides long enough keeps it, shared.
        let longer = trim_out(&seq, v1, ids[1], half(9)).unwrap();
        assert!(Arc::ptr_eq(
            &longer.tracks[0].transitions[0],
            &seq.tracks[0].transitions[0]
        ));
    }

    // ---- transitions follow their cut ----------------------------------------

    /// `(position, duration)` of V1's transitions, in order.
    fn cuts(seq: &Sequence) -> Vec<(RationalTime, RationalTime)> {
        seq.tracks[0]
            .transitions
            .iter()
            .map(|tr| (tr.position, tr.duration))
            .collect()
    }

    /// a [0,5) | b [5,10) | c [10,15) on V1 with 4 s dissolves at 5 and 10.
    fn three_with_dissolves() -> (Sequence, TrackId, MediaItemId, Vec<ClipId>) {
        let (seq, v1, media, ids) = butted(&[5, 5, 5]);
        let seq = dissolve(&seq, v1, ids[0], t(4));
        let seq = dissolve(&seq, v1, ids[1], t(4));
        assert_eq!(cuts(&seq), [(t(5), t(4)), (t(10), t(4))]);
        (seq, v1, media, ids)
    }

    #[test]
    fn transitions_move_with_a_cut_that_shifts() {
        let (seq, v1, media, ids) = three_with_dissolves();
        let at = |start: i64, len: i64| Clip::new(media, "i", t(start), t(0), t(len)).unwrap();
        // Ripple-inserting 3 s at the start pushes both cuts.
        let pushed = [(t(8), t(4)), (t(13), t(4))];
        assert_eq!(
            cuts(&ripple_insert_clip(&seq, v1, at(0, 3)).unwrap()),
            pushed
        );
        assert_eq!(
            cuts(&magnetic_ripple_insert(&seq, v1, at(0, 3)).unwrap()),
            pushed
        );
        // Ripple-deleting a takes its dissolve and pulls b|c back to 5.
        assert_eq!(
            cuts(&ripple_remove_clip(&seq, v1, ids[0]).unwrap()),
            [(t(5), t(4))]
        );
        assert_eq!(
            cuts(&magnetic_ripple_delete(&seq, v1, ids[0]).unwrap()),
            [(t(5), t(4))]
        );
        // b and c moved together (c connected to b) keep the cut between them.
        let linked = connect_clip(&seq, ids[1], ids[2]).unwrap();
        assert_eq!(
            cuts(&magnetic_move_clip(&linked, v1, ids[1], t(6)).unwrap()),
            [(t(11), t(4))]
        );
        // Undo brings back what an edit removed, where it was.
        let mut log = crate::edit_log::EditLog::new(seq.clone());
        log.apply(crate::edit_log::EditOp::RippleRemoveClip {
            track_id: v1,
            clip_id: ids[0],
        })
        .unwrap();
        log.undo().unwrap();
        assert_eq!(log.current(), &seq);
    }

    #[test]
    fn transitions_go_when_their_clips_stop_meeting() {
        let (seq, v1, media, ids) = three_with_dissolves();
        let at = |start: i64, len: i64| Clip::new(media, "i", t(start), t(0), t(len)).unwrap();
        // A removed clip takes both its dissolves; the new a|c cut has none.
        assert!(cuts(&remove_clip(&seq, v1, ids[1]).unwrap()).is_empty());
        assert!(cuts(&ripple_remove_clip(&seq, v1, ids[1]).unwrap()).is_empty());
        assert!(cuts(&magnetic_ripple_delete(&seq, v1, ids[1]).unwrap()).is_empty());
        // A clip rippled in between a and b; b|c moves on.
        let between = [(t(11), t(4))];
        assert_eq!(
            cuts(&ripple_insert_clip(&seq, v1, at(5, 1)).unwrap()),
            between
        );
        assert_eq!(
            cuts(&magnetic_ripple_insert(&seq, v1, at(5, 1)).unwrap()),
            between
        );
        // Moved apart: c slid or magnetically moved away from b.
        assert_eq!(
            cuts(&slide(&seq, v1, ids[2], t(1)).unwrap()),
            [(t(5), t(4))]
        );
        assert_eq!(
            cuts(&magnetic_move_clip(&seq, v1, ids[2], t(11)).unwrap()),
            [(t(5), t(4))]
        );
        // A gap opened by trimming a's out-point or c's in-point.
        assert_eq!(
            cuts(&trim_out(&seq, v1, ids[0], t(4)).unwrap()),
            [(t(10), t(4))]
        );
        assert_eq!(
            cuts(&trim_in(&seq, v1, ids[2], t(1)).unwrap()),
            [(t(5), t(4))]
        );
        // An overwrite across the cut at 5 replaces b's head: a|b goes, b|c
        // stays on b's re-IDed tail.
        let over = overwrite_clip(&seq, v1, at(4, 2)).unwrap();
        assert_eq!(cuts(&over), [(t(10), t(4))]);
        assert_ne!(over.tracks[0].clips[2].id, ids[1]);
        // A track with a gap after the anchor can't take one.
        let gap = remove_clip(&seq, v1, ids[1]).unwrap();
        let err = add_transition_at_cut(&gap, v1, ids[0], TransitionKind::Dissolve, t(1))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no clip starts at the cut"), "{err}");
    }

    #[test]
    fn transitions_stay_on_clips_cut_into_pieces() {
        let (seq, v1, media, ids) = three_with_dissolves();
        // Splitting b leaves a|b on its left piece and b|c on its right one,
        // each capped by the piece's length.
        let split_b = split(&seq, v1, ids[1], t(6)).unwrap();
        assert_eq!(cuts(&split_b), [(t(5), t(1)), (t(10), t(4))]);
        // Splitting a leaves a|b on a's right half.
        assert_eq!(
            cuts(&split(&seq, v1, ids[0], half(7)).unwrap()),
            [(t(5), half(3)), (t(10), t(4))]
        );
        // Overwriting inside b keeps both dissolves on b's re-IDed pieces.
        let x = Clip::new(media, "x", t(6), t(0), t(2)).unwrap();
        assert_eq!(
            cuts(&overwrite_clip(&seq, v1, x).unwrap()),
            [(t(5), t(1)), (t(10), t(2))]
        );
    }

    #[test]
    fn transitions_saved_by_older_builds_are_capped_or_dropped() {
        // a [0,5) | b [5,6) | c [6,10) holding a 4 s dissolve at 5, stored
        // as an older build could have saved it.
        let (seq, v1, media, ids) = butted(&[5, 1, 4]);
        let stored = |seq: &Sequence, tr: Transition| {
            let mut track = (*seq.tracks[0]).clone();
            track.transitions.push(Arc::new(tr));
            replace_track(seq, 0, track)
        };
        let seq = stored(&seq, Transition::dissolve(t(5), t(4)));
        // It renders as 1 s, so a dissolve at 6 fits beside it.
        let both = dissolve(&seq, v1, ids[1], t(4));
        assert_eq!(cuts(&both), [(t(5), t(4)), (t(6), t(1))]);
        // The next edit of the track caps it, and drops an orphan (nothing
        // meets at 12) rather than let it land on a cut made there later.
        let orphaned = stored(&seq, Transition::dissolve(t(12), t(2)));
        let x = Clip::new(media, "x", t(10), t(0), t(2)).unwrap();
        let edited = insert_clip(&orphaned, v1, x).unwrap();
        assert_eq!(cuts(&edited), [(t(5), t(1))]);
        let y = Clip::new(media, "y", t(12), t(0), t(2)).unwrap();
        assert_eq!(cuts(&insert_clip(&edited, v1, y).unwrap()), [(t(5), t(1))]);
    }

    #[test]
    fn magnetic_ripples_carry_other_tracks_transitions() {
        // V1: p [0,2) | q [2,5) | r [5,8). V2: x [2,5) | y [5,8), 2 s
        // dissolve at 5.
        let (seq, v1, media, ids) = butted(&[2, 3, 3]);
        let seq = add_track(&seq, TrackKind::Video, "V2");
        let v2 = seq.tracks[1].id;
        let x = Clip::new(media, "x", t(2), t(0), t(3)).unwrap();
        let x_id = x.id;
        let seq = insert_clip(&seq, v2, x).unwrap();
        let seq = insert_clip(&seq, v2, Clip::new(media, "y", t(5), t(0), t(3)).unwrap()).unwrap();
        let (seq, _) =
            add_transition_at_cut(&seq, v2, x_id, TransitionKind::Dissolve, t(2)).unwrap();
        let v2_cuts = |s: &Sequence| -> Vec<RationalTime> {
            s.tracks[1]
                .transitions
                .iter()
                .map(|tr| tr.position)
                .collect()
        };
        // Deleting p closes 2 s on every track: V2's cut moves to 3.
        assert_eq!(
            v2_cuts(&magnetic_ripple_delete(&seq, v1, ids[0]).unwrap()),
            [t(3)]
        );
        // Opening 1 s at 2 (before x) moves it to 6.
        let one = |at: i64| Clip::new(media, "i", t(at), t(0), t(1)).unwrap();
        assert_eq!(
            v2_cuts(&magnetic_ripple_insert(&seq, v1, one(2)).unwrap()),
            [t(6)]
        );
        // Opening 1 s at 5 pushes y off x: the dissolve between them goes.
        assert!(v2_cuts(&magnetic_ripple_insert(&seq, v1, one(5)).unwrap()).is_empty());
        // A locked V2 doesn't ripple, so its dissolve stays put.
        let locked = lock(&seq, v2);
        assert_eq!(
            v2_cuts(&magnetic_ripple_delete(&locked, v1, ids[0]).unwrap()),
            [t(5)]
        );
    }
}
