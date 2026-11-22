// Track lock (M1, ADR 0007): shared labels and the rules for what a lock
// blocks, so the timeline, the track menu, the inspector, the Color page and
// the mixer agree.
// Mirrors the lock policy in `crates/vance-timeline/src/ops.rs`: a lock
// freezes the track's clips, their effects and transitions, and keeps the
// track from being deleted; mute, solo, visibility, order and name stay
// editable.

/** Label (and tooltip) of the control that toggles `locked`. */
export function lockActionLabel(locked: boolean): string {
  return locked ? 'Unlock track' : 'Lock track';
}

/** Why "Delete track" is unavailable on a locked track. */
export const LOCKED_DELETE_HINT = 'Unlock the track to delete it.';

/** Why a locked track's clip can't be edited. */
export const LOCKED_EDIT_HINT = 'Unlock the track to edit this clip.';

/** Whether the engine refuses to delete `track` (it refuses a locked one). */
export function canDeleteTrack(track: { locked: boolean }): boolean {
  return !track.locked;
}

/** Whether the engine accepts edits to the clips on `track`. */
export function canEditTrackContents(track: { locked: boolean }): boolean {
  return !track.locked;
}

/** Hint shown when an edit on a locked track is refused before it starts. */
export function lockedTrackMessage(name: string): string {
  return `Track ${name} is locked. Unlock it to edit its clips.`;
}

type NamedLock = { name: string; locked: boolean };

/** Refuses edits on locked tracks before they start (so the engine never
 *  has to), hinting at most once per gesture: a press-and-drag, a drop or
 *  a key press. */
export function createLockGuard(notify: (message: string) => void) {
  let hinted = false;
  return {
    /** A new gesture starts; its first refusal hints again. */
    begin() {
      hinted = false;
    },
    /** True when `track` is locked: the caller must not start the edit. */
    refuses(track: NamedLock | null | undefined): boolean {
      if (!track || canEditTrackContents(track)) return false;
      if (!hinted) {
        hinted = true;
        notify(lockedTrackMessage(track.name));
      }
      return true;
    }
  };
}

/** Timeline cursor while hovering (no button held). On a locked track a
 *  trim edge or a fade / keyframe handle reads as unavailable; a clip body
 *  keeps the normal cursor, since clicking it still selects. */
export function laneCursor(at: {
  edge: boolean;
  handle: boolean;
  headerButton: boolean;
  locked: boolean;
  /** Carrying media from the Browser over a locked track. */
  dropBlocked: boolean;
}): string {
  if (at.dropBlocked || (at.locked && (at.edge || at.handle))) return 'not-allowed';
  if (at.edge) return 'ew-resize';
  if (at.headerButton) return 'pointer';
  return '';
}

/** A press on a locked clip that moved past the 3 px click radius as a
 *  move or trim: the lock refused the drag, but the release still selects
 *  the clip (so the inspector can offer the unlock). */
export function isRefusedClipDrag(kind: string, movedPx: number, locked: boolean): boolean {
  return (
    locked && movedPx > 3 && (kind === 'move' || kind === 'trim-left' || kind === 'trim-right')
  );
}

type LockTrack = NamedLock & { id: string; clips: readonly { id: string }[] };

/** The clips a move of `root` carries: `root` plus, in magnetic mode, every
 *  clip connected below it (`[anchor, connected]` pairs, followed
 *  transitively). Mirrors vance-timeline's `connected_subtree`. */
export function moveGroup(
  root: string,
  magnetic: boolean,
  connections: readonly (readonly [string, string])[]
): string[] {
  const out = [root];
  if (!magnetic) return out;
  for (let i = 0; i < out.length; i++) {
    for (const [anchor, connected] of connections) {
      if (anchor === out[i] && !out.includes(connected)) out.push(connected);
    }
  }
  return out;
}

/** The first locked track holding any of `clipIds` (the engine refuses a
 *  move whose group touches one), or null. */
export function lockedTrackOf<T extends LockTrack>(
  tracks: readonly T[],
  clipIds: readonly string[]
): T | null {
  return (
    tracks.find((t) => !canEditTrackContents(t) && t.clips.some((c) => clipIds.includes(c.id))) ??
    null
  );
}

/** Split `clipIds` into the clips whose track accepts edits and the ones on
 *  locked tracks (ids not in `tracks` are dropped). */
export function splitByLock<T extends LockTrack>(
  tracks: readonly T[],
  clipIds: readonly string[]
): { editable: { trackId: string; clipId: string }[]; locked: { track: T; clipId: string }[] } {
  const editable: { trackId: string; clipId: string }[] = [];
  const locked: { track: T; clipId: string }[] = [];
  for (const clipId of clipIds) {
    const track = tracks.find((t) => t.clips.some((c) => c.id === clipId));
    if (!track) continue;
    if (canEditTrackContents(track)) editable.push({ trackId: track.id, clipId });
    else locked.push({ track, clipId });
  }
  return { editable, locked };
}

/** Where focus goes after "Unlock track" in a lock notice, which then goes
 *  away. From the keyboard: the track's lock button. It shows the new state
 *  and edits nothing, so the next key can't change the clip by accident (a
 *  re-enabled control would: Space toggles a node's bypass, opens the blend
 *  select). After a click: nowhere (null); focus stays where the pointer
 *  left it. */
export function focusAfterUnlock<T>(viaKeyboard: boolean, lockButton: T | null): T | null {
  return viaKeyboard ? lockButton : null;
}

type BusRef = 'Master' | { Bus: string };

/** The first locked track whose main output or a send uses bus `busId`
 *  (the engine refuses to remove such a bus), or null. Tracks without
 *  routing data never match. */
export function lockedTrackUsingBus<
  T extends NamedLock & { output?: BusRef; sends?: readonly { target: string }[] }
>(tracks: readonly T[], busId: string): T | null {
  return (
    tracks.find(
      (t) =>
        t.locked &&
        ((typeof t.output === 'object' && t.output.Bus === busId) ||
          (t.sends ?? []).some((s) => s.target === busId))
    ) ?? null
  );
}
