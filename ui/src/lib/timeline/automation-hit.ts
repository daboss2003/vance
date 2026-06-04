/**
 * Phase 7.4 M5 — pure hit-test helpers for the automation overlay.
 *
 * Each function returns a target descriptor or `null`. Callers stitch
 * them together in a priority order (fade chip → interp chip → fade
 * grip → clip keyframe → track keyframe → trim → clip body). Pure so
 * the unit tests for the priority resolver can drive them without a
 * canvas instance.
 */

import type { GainKeyframe } from '$lib/ipc';

/** Visual grip / chip dimensions kept in one place. */
export const FADE_GRIP_HITBOX = 12;
export const KF_HITBOX = 12;
export const CHIP_W = 14;
export const CHIP_H = 10;

export type FadeGripHit = {
  kind: 'fade-grip';
  trackId: string;
  clipId: string;
  side: 'in' | 'out';
};

export type FadeChipHit = {
  kind: 'fade-chip';
  trackId: string;
  clipId: string;
  side: 'in' | 'out';
};

export type ClipKfHit = {
  kind: 'kf-clip';
  trackId: string;
  clipId: string;
  index: number;
};

export type TrackKfHit = {
  kind: 'kf-track';
  trackId: string;
  index: number;
};

export type InterpChipHit = {
  kind: 'interp-chip';
  scope: 'clip' | 'track';
  trackId: string;
  clipId?: string;
};

export type AutomationHit = FadeGripHit | FadeChipHit | ClipKfHit | TrackKfHit | InterpChipHit;

/**
 * Hit-test the fade grip at the top corner of an audio clip.
 * Returns null when the click is outside the 12×12 grab zone.
 */
export function hitFadeGrip(
  cssX: number,
  cssY: number,
  trackId: string,
  clipId: string,
  cx: number,
  cy: number,
  cw: number,
  fadeInPx: number,
  fadeOutPx: number
): FadeGripHit | null {
  // Top 12 px of the clip is reserved for the grip on each side; the
  // bottom (ch - 12) stays available for the trim handle (per the spec's
  // documented fade-vs-trim split rule).
  if (cssY < cy || cssY > cy + FADE_GRIP_HITBOX) return null;
  const gripIn = cx + Math.max(0, fadeInPx);
  if (Math.abs(cssX - gripIn) <= FADE_GRIP_HITBOX / 2) {
    return { kind: 'fade-grip', trackId, clipId, side: 'in' };
  }
  const gripOut = cx + cw - Math.max(0, fadeOutPx);
  if (Math.abs(cssX - gripOut) <= FADE_GRIP_HITBOX / 2) {
    return { kind: 'fade-grip', trackId, clipId, side: 'out' };
  }
  return null;
}

/**
 * Hit-test a 14×10 fade curve chip at the bottom corner of the clip.
 * Only valid when the corresponding fade is wide enough to host it.
 */
export function hitFadeChip(
  cssX: number,
  cssY: number,
  trackId: string,
  clipId: string,
  cx: number,
  cy: number,
  cw: number,
  ch: number,
  fadeInPx: number,
  fadeOutPx: number
): FadeChipHit | null {
  const chipY = cy + ch - CHIP_H - 2;
  if (cssY < chipY || cssY > chipY + CHIP_H) return null;
  // fade-in chip
  if (fadeInPx >= 28) {
    const x = cx + 3;
    if (cssX >= x && cssX <= x + CHIP_W) {
      return { kind: 'fade-chip', trackId, clipId, side: 'in' };
    }
  }
  // fade-out chip
  if (fadeOutPx >= 28) {
    const x = cx + cw - CHIP_W - 3;
    if (cssX >= x && cssX <= x + CHIP_W) {
      return { kind: 'fade-chip', trackId, clipId, side: 'out' };
    }
  }
  return null;
}

/**
 * Hit-test against a keyframe on a per-clip gain lane.
 * `pixelsPerSecond` converts time → x; `cx + (k.time_seconds - clip_t_in
 * + offset_t) * pps` is the keyframe's canvas x. Caller supplies an
 * `xForTime` to keep the math local.
 */
export function hitClipKeyframe(
  cssX: number,
  cssY: number,
  trackId: string,
  clipId: string,
  lane: GainKeyframe[],
  xForLocalTime: (tClip: number) => number,
  yForKfDb: (db: number) => number
): ClipKfHit | null {
  // Walk in reverse so the most recently appended keyframe wins on ties.
  for (let i = lane.length - 1; i >= 0; i--) {
    const k = lane[i];
    const x = xForLocalTime(k.time_seconds);
    const y = yForKfDb(k.value);
    if (Math.abs(cssX - x) <= KF_HITBOX / 2 && Math.abs(cssY - y) <= KF_HITBOX / 2) {
      return { kind: 'kf-clip', trackId, clipId, index: i };
    }
  }
  return null;
}

/**
 * Hit-test against a keyframe on a track-wide automation lane.
 */
export function hitTrackKeyframe(
  cssX: number,
  cssY: number,
  trackId: string,
  lane: GainKeyframe[],
  xForTime: (t: number) => number,
  yForKfDb: (db: number) => number
): TrackKfHit | null {
  for (let i = lane.length - 1; i >= 0; i--) {
    const k = lane[i];
    const x = xForTime(k.time_seconds);
    const y = yForKfDb(k.value);
    if (Math.abs(cssX - x) <= KF_HITBOX / 2 && Math.abs(cssY - y) <= KF_HITBOX / 2) {
      return { kind: 'kf-track', trackId, index: i };
    }
  }
  return null;
}
