//! Pure cue-list operations (in-place; callers own the `Vec<Cue>`).
//!
//! These keep the list sorted by `start_ms` and clamp timings sane. The
//! timeline edit layer clones the sequence's cue list, applies one of
//! these, and stores the result (so every edit is undoable).

use crate::{Cue, CueId};

/// Sort cues by start time (stable on ties).
pub fn sort_cues(cues: &mut [Cue]) {
    cues.sort_by_key(|c| c.start_ms);
}

/// Insert `cue`, keeping the list sorted by start time. Returns its id.
pub fn add_cue(cues: &mut Vec<Cue>, cue: Cue) -> CueId {
    let id = cue.id;
    let pos = cues
        .iter()
        .position(|c| c.start_ms > cue.start_ms)
        .unwrap_or(cues.len());
    cues.insert(pos, cue);
    id
}

/// Remove the cue with `id`. Returns true if one was removed.
pub fn remove_cue(cues: &mut Vec<Cue>, id: CueId) -> bool {
    let before = cues.len();
    cues.retain(|c| c.id != id);
    cues.len() != before
}

/// Replace a cue's text. Returns true if the cue exists.
pub fn set_text(cues: &mut [Cue], id: CueId, text: impl Into<String>) -> bool {
    if let Some(c) = cues.iter_mut().find(|c| c.id == id) {
        c.text = text.into();
        true
    } else {
        false
    }
}

/// Set a cue's `[start, end)` (clamped to `start >= 0`, `end >= start`),
/// then re-sort. Returns true if the cue exists.
pub fn set_timing(cues: &mut [Cue], id: CueId, start_ms: i64, end_ms: i64) -> bool {
    let found = if let Some(c) = cues.iter_mut().find(|c| c.id == id) {
        c.start_ms = start_ms.max(0);
        c.end_ms = end_ms.max(c.start_ms);
        true
    } else {
        false
    };
    if found {
        sort_cues(cues);
    }
    found
}

/// Shift one cue by `delta_ms` (start + end), clamped to `start >= 0`,
/// then re-sort. Returns true if the cue exists.
pub fn shift_cue(cues: &mut [Cue], id: CueId, delta_ms: i64) -> bool {
    let found = if let Some(c) = cues.iter_mut().find(|c| c.id == id) {
        let new_start = (c.start_ms + delta_ms).max(0);
        let applied = new_start - c.start_ms;
        c.start_ms = new_start;
        c.end_ms += applied;
        true
    } else {
        false
    };
    if found {
        sort_cues(cues);
    }
    found
}

/// Shift EVERY cue by `delta_ms` (e.g. to re-sync an imported track),
/// clamping each start to `>= 0`.
pub fn shift_all(cues: &mut [Cue], delta_ms: i64) {
    for c in cues.iter_mut() {
        let new_start = (c.start_ms + delta_ms).max(0);
        let applied = new_start - c.start_ms;
        c.start_ms = new_start;
        c.end_ms += applied;
    }
}

/// Split the cue with `id` at `at_ms` into two cues (the left keeps the
/// id; the right gets a fresh id + the same text). No-op if `at_ms` isn't
/// strictly inside the cue. Returns the new right cue's id when it split.
pub fn split_cue(cues: &mut Vec<Cue>, id: CueId, at_ms: i64) -> Option<CueId> {
    let (idx, left_text, end) = {
        let (idx, c) = cues.iter().enumerate().find(|(_, c)| c.id == id)?;
        if at_ms <= c.start_ms || at_ms >= c.end_ms {
            return None;
        }
        (idx, c.text.clone(), c.end_ms)
    };
    cues[idx].end_ms = at_ms;
    let mut right = Cue::new(at_ms, end, left_text);
    let new_id = right.id;
    right.speaker = cues[idx].speaker.clone();
    cues.insert(idx + 1, right);
    Some(new_id)
}

/// Merge the cue with `id` into the cue that immediately follows it in
/// start order: text is joined with a space, the end extends to the
/// later end, and the follower is removed. No-op if there's no follower.
/// Returns true if a merge happened.
pub fn merge_with_next(cues: &mut Vec<Cue>, id: CueId) -> bool {
    sort_cues(cues);
    let Some(idx) = cues.iter().position(|c| c.id == id) else {
        return false;
    };
    if idx + 1 >= cues.len() {
        return false;
    }
    let next = cues.remove(idx + 1);
    let cur = &mut cues[idx];
    cur.end_ms = cur.end_ms.max(next.end_ms);
    if !next.text.trim().is_empty() {
        if !cur.text.is_empty() {
            cur.text.push(' ');
        }
        cur.text.push_str(next.text.trim());
    }
    true
}

/// The cue active at `t_ms` (the first whose `[start, end)` contains it).
pub fn cue_at(cues: &[Cue], t_ms: i64) -> Option<&Cue> {
    cues.iter().find(|c| c.contains(t_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> Vec<Cue> {
        vec![
            Cue::new(0, 1000, "a"),
            Cue::new(1000, 2000, "b"),
            Cue::new(2000, 3000, "c"),
        ]
    }

    #[test]
    fn add_keeps_sorted() {
        let mut cues = seed();
        add_cue(&mut cues, Cue::new(500, 800, "mid"));
        assert_eq!(cues[1].text, "mid");
        assert!(cues.windows(2).all(|w| w[0].start_ms <= w[1].start_ms));
    }

    #[test]
    fn split_and_merge_round_trip_text() {
        let mut cues = vec![Cue::new(0, 2000, "hello world")];
        let id = cues[0].id;
        let right = split_cue(&mut cues, id, 1000).unwrap();
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].end_ms, 1000);
        assert_eq!(cues[1].start_ms, 1000);
        assert_eq!(cues[1].id, right);
        // Merge them back: text joined, end extended.
        assert!(merge_with_next(&mut cues, id));
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].end_ms, 2000);
        assert_eq!(cues[0].text, "hello world hello world");
    }

    #[test]
    fn shift_clamps_and_resorts() {
        let mut cues = seed();
        let last = cues[2].id;
        // Shift the last cue way left → clamped to start 0 and re-sorted
        // ahead of "b" (stable sort keeps the pre-existing 0-start "a" first).
        shift_cue(&mut cues, last, -5000);
        let c = cues.iter().find(|c| c.text == "c").unwrap();
        assert_eq!(c.start_ms, 0, "clamped to zero");
        assert_eq!(c.end_ms, 1000, "duration preserved under clamp");
        assert_eq!(cues.last().unwrap().text, "b", "b is now the latest cue");
    }

    #[test]
    fn cue_at_finds_active() {
        let cues = seed();
        assert_eq!(cue_at(&cues, 1500).map(|c| c.text.as_str()), Some("b"));
        assert_eq!(cue_at(&cues, 9999), None);
    }
}
