// Keyer clip range (M1, ADR 0007): Clip black must stay below Clip white, or
// the matte inverts by accident.

import { evalNumber, isKeyframes, upsertKeyframe, type EffectValue } from './ipc';
import { KEY_CLIP_BLACK, KEY_CLIP_WHITE, keyerDefault } from './keyer-keys';

/** Smallest gap kept between Clip black and Clip white. */
export const CLIP_RANGE_GAP = 0.01;

/** Frame rate assumed when the sequence's is unknown or invalid. */
const FALLBACK_FPS = 24;

/** The clip times the order is checked at: every frame of the clip (the
 *  times the engine renders the keyer at), the write time `at`, and every
 *  keyframe time of either end (exact values, also outside the frame grid). */
export function clipCheckTimes(
  durationSeconds: number,
  fps: number,
  at: number,
  curves: readonly (EffectValue | undefined)[]
): number[] {
  const rate = Number.isFinite(fps) && fps > 0 ? fps : FALLBACK_FPS;
  const frames =
    Number.isFinite(durationSeconds) && durationSeconds > 0 ? durationSeconds * rate : 0;
  const times = new Set<number>([at]);
  for (let i = 0; i <= Math.floor(frames); i++) times.add(i / rate);
  for (const c of curves) {
    if (c !== undefined && isKeyframes(c)) for (const k of c) times.add(k.time_seconds);
  }
  return [...times];
}

/** The value to write for one end of the clip range (`key`, typed as
 *  `typed`) so that black + gap ≤ white at every frame this write changes.
 *  The write is the one `setEffectNumbers` makes at clip time `at`: a
 *  keyframe there when the param is keyframed, else a static value for the
 *  whole clip. Both curves are sampled with the engine's interpolation
 *  (`evalNumber` mirrors `eval_keyframes`: Linear, Bezier = smoothstep,
 *  Hold), so a crossing between keyframes (Linear black under Bezier white)
 *  is caught too. Frames this write doesn't affect are left alone: an order
 *  the user keyed elsewhere isn't this edit's to fix. */
export function orderedClipValue(
  key: typeof KEY_CLIP_BLACK | typeof KEY_CLIP_WHITE,
  typed: number,
  black: EffectValue | undefined,
  white: EffectValue | undefined,
  at: number,
  durationSeconds: number,
  fps: number
): number {
  const isBlack = key === KEY_CLIP_BLACK;
  const own = isBlack ? black : white;
  const other = isBlack ? white : black;
  const ownDefault = keyerDefault(key);
  const otherDefault = keyerDefault(isBlack ? KEY_CLIP_WHITE : KEY_CLIP_BLACK);
  const written = (v: number): EffectValue =>
    own !== undefined && isKeyframes(own) ? upsertKeyframe(own, at, v) : v;

  // At any time t the written curve is a + w·v (each interpolation mode is
  // affine in the written keyframe's value), so each time bounds v on one
  // side.
  const at0 = written(0);
  const at1 = written(1);
  let v = typed;
  for (const t of clipCheckTimes(durationSeconds, fps, at, [black, white])) {
    const a = evalNumber(at0, t, ownDefault);
    const w = evalNumber(at1, t, ownDefault) - a;
    if (w <= 1e-9) continue;
    const o = other !== undefined ? evalNumber(other, t, otherDefault) : otherDefault;
    v = isBlack
      ? Math.min(v, (o - CLIP_RANGE_GAP - a) / w)
      : Math.max(v, (o + CLIP_RANGE_GAP - a) / w);
  }
  return Math.max(0, Math.min(1, v));
}
