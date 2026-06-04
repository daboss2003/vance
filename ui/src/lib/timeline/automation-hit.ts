/**
 * Phase 7.4 M5 — pure hit-test helpers for the automation overlay.
 *
 * Each function returns a target descriptor or `null`. Callers stitch
 * them together in a priority order (fade chip → fade grip → clip
 * keyframe → track keyframe → trim → clip body). Pure so the unit
 * tests for the priority resolver can drive them without a canvas
 * instance.
 *
 * Pixel geometry for the fade chip + grip is intentionally a
 * pre-computed [`FadeChipGeom`] passed in by the caller so the draw
 * pass and hit pass cannot drift apart (the M5 adversarial review
 * caught a 28..32 px window where the chip was hit but not drawn, and
 * a separate hit/draw misalignment when the fade duration exceeded
 * the clip width — both follow from duplicated geometry).
 */

import type { GainKeyframe } from '$lib/ipc';

/** Visual grip / chip dimensions kept in one place. */
export const FADE_GRIP_HITBOX = 12;
export const KF_HITBOX = 12;
export const CHIP_W = 14;
export const CHIP_H = 10;

/** Minimum fade pixel width at which the grip is targetable.
 *  Below this we do NOT report a fade-grip hit — the user's click
 *  belongs to trim-left / trim-right (per the documented fade-vs-trim
 *  split rule). Matches the chip's visibility threshold so the cursor
 *  story is consistent across both controls. */
export const FADE_GRIP_MIN_PX = 8;

/**
 * Geometry for one clip's fade controls. Computed by the draw pass
 * (which knows the visual clamp) and reused verbatim by the hit pass
 * via [`hitFadeGrip`] / [`hitFadeChip`]. Both passes consume the same
 * numbers, eliminating the previous draw/hit drift.
 */
export type FadeChipGeom = {
  /** Visually-clamped fade-in width in CSS pixels (0 when no fade). */
  fadeInPx: number;
  /** Visually-clamped fade-out width in CSS pixels (0 when no fade). */
  fadeOutPx: number;
  /** Whether the fade-in chip is drawn (and thus targetable). */
  chipInVisible: boolean;
  /** Whether the fade-out chip is drawn (and thus targetable). */
  chipOutVisible: boolean;
  /** Whether the fade-in grip is drawn (and thus targetable). */
  gripInVisible: boolean;
  /** Whether the fade-out grip is drawn (and thus targetable). */
  gripOutVisible: boolean;
};

/** Build the shared fade chip geometry. Called once per clip per
 *  render so draw and hit both see the same numbers. */
export function fadeChipGeomFor(
  rawFadeInSec: number,
  rawFadeOutSec: number,
  clipWidthPx: number,
  pixelsPerSecond: number
): FadeChipGeom {
  const fadeInPx = Math.max(0, Math.min(clipWidthPx - 2, rawFadeInSec * pixelsPerSecond));
  const fadeOutPx = Math.max(0, Math.min(clipWidthPx - 2, rawFadeOutSec * pixelsPerSecond));
  const chipInVisible = clipWidthPx > 32 && fadeInPx >= 28;
  const chipOutVisible = clipWidthPx > 32 && fadeOutPx >= 28;
  const gripInVisible = fadeInPx >= FADE_GRIP_MIN_PX;
  const gripOutVisible = fadeOutPx >= FADE_GRIP_MIN_PX;
  return {
    fadeInPx,
    fadeOutPx,
    chipInVisible,
    chipOutVisible,
    gripInVisible,
    gripOutVisible
  };
}

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

export type AutomationHit = FadeGripHit | FadeChipHit | ClipKfHit | TrackKfHit;

/**
 * Hit-test the fade grip at the top corner of an audio clip.
 *
 * Phase 7.4 M5 — uses the shared [`FadeChipGeom`] so the grip position
 * matches the draw pass byte-for-byte; the `gripInVisible` /
 * `gripOutVisible` flags inside the geom mean a zero-fade clip
 * correctly defers to the trim-left / trim-right handler instead of
 * silently swallowing the click (adversarial review finding #1).
 */
export function hitFadeGrip(
  cssX: number,
  cssY: number,
  trackId: string,
  clipId: string,
  cx: number,
  cy: number,
  cw: number,
  geom: FadeChipGeom
): FadeGripHit | null {
  if (cssY < cy || cssY > cy + FADE_GRIP_HITBOX) return null;
  if (geom.gripInVisible) {
    const gripIn = cx + geom.fadeInPx;
    if (Math.abs(cssX - gripIn) <= FADE_GRIP_HITBOX / 2) {
      return { kind: 'fade-grip', trackId, clipId, side: 'in' };
    }
  }
  if (geom.gripOutVisible) {
    const gripOut = cx + cw - geom.fadeOutPx;
    if (Math.abs(cssX - gripOut) <= FADE_GRIP_HITBOX / 2) {
      return { kind: 'fade-grip', trackId, clipId, side: 'out' };
    }
  }
  return null;
}

/**
 * Hit-test a 14×10 fade curve chip at the bottom corner of the clip.
 *
 * Phase 7.4 M5 — uses [`FadeChipGeom`] so the chip is reported as hit
 * iff it is also drawn. Closes the 28..32 px draw/hit window the
 * adversarial review caught.
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
  geom: FadeChipGeom
): FadeChipHit | null {
  const chipY = cy + ch - CHIP_H - 2;
  if (cssY < chipY || cssY > chipY + CHIP_H) return null;
  if (geom.chipInVisible) {
    const x = cx + 3;
    if (cssX >= x && cssX <= x + CHIP_W) {
      return { kind: 'fade-chip', trackId, clipId, side: 'in' };
    }
  }
  if (geom.chipOutVisible) {
    const x = cx + cw - CHIP_W - 3;
    if (cssX >= x && cssX <= x + CHIP_W) {
      return { kind: 'fade-chip', trackId, clipId, side: 'out' };
    }
  }
  return null;
}

/**
 * Hit-test against a keyframe on a per-clip gain lane. The caller
 * supplies `xForLocalTime` (clip-relative time → canvas X) and
 * `yForKfDb` (dB → canvas Y); both encapsulate the clip's geometry
 * + scroll + zoom so this helper stays pure. Walks in reverse so the
 * most recently appended keyframe wins on a tie — matches the draw
 * order where later entries paint over earlier ones.
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
