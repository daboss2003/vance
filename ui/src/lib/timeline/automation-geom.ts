/**
 * Phase 7.4 M5 — pure geometry helpers for the automation overlay.
 *
 * Kept zero-state so the canvas hit-test, draw, and commit paths all
 * agree on the same math. The dB range is hard-coded at [-60, +12]; the
 * top of the lane = +12 dB, the bottom = -60 dB, with the 0 dB baseline
 * landing at the 35% mark (slightly above center so positive-gain headroom
 * stays visible on a 28 px clip body).
 */

import type { GainKeyframe } from '$lib/ipc';

export const DB_MIN = -60;
export const DB_MAX = 12;
/** Where 0 dB lands within the lane, as a fraction from the top. */
export const DB_ZERO_FRAC = 0.35;

/** Map a dB value to a fractional position 0..1 (0 = top). */
export function dbToFrac(db: number): number {
  const clamped = Math.max(DB_MIN, Math.min(DB_MAX, db));
  if (clamped >= 0) {
    // Positive band: [0, +12] dB spans [DB_ZERO_FRAC, 0].
    return DB_ZERO_FRAC * (1 - clamped / DB_MAX);
  }
  // Negative band: [-60, 0] dB spans [1, DB_ZERO_FRAC].
  return DB_ZERO_FRAC + (1 - DB_ZERO_FRAC) * (clamped / DB_MIN);
}

/** Inverse of dbToFrac — fractional position 0..1 → dB. */
export function fracToDb(frac: number): number {
  const f = Math.max(0, Math.min(1, frac));
  if (f <= DB_ZERO_FRAC) {
    return DB_MAX * (1 - f / DB_ZERO_FRAC);
  }
  return DB_MIN * ((f - DB_ZERO_FRAC) / (1 - DB_ZERO_FRAC));
}

/** Convert a dB value to a Y pixel inside a lane rect. */
export function yForDb(db: number, laneTop: number, laneHeight: number): number {
  return laneTop + dbToFrac(db) * laneHeight;
}

/** Convert a Y pixel inside a lane rect back to a dB value. */
export function dbForY(y: number, laneTop: number, laneHeight: number): number {
  if (laneHeight <= 0) return 0;
  return fracToDb((y - laneTop) / laneHeight);
}

/** Clamp a dB value to the editor's displayable range. */
export function clampDb(db: number): number {
  return Math.max(DB_MIN, Math.min(DB_MAX, db));
}

export function dbToString(db: number): string {
  return `${db >= 0 ? '+' : ''}${db.toFixed(1)} dB`;
}

export function timeToString(seconds: number): string {
  if (!Number.isFinite(seconds)) return '–';
  const sign = seconds < 0 ? '-' : '';
  const abs = Math.abs(seconds);
  const m = Math.floor(abs / 60);
  const s = abs - m * 60;
  return `${sign}${m}:${s.toFixed(3).padStart(6, '0')}`;
}

/** Cubic smoothstep — matches `eval_gain_lane`'s Bezier branch in vance-audio. */
export function smoothstep(t: number): number {
  return t * t * (3 - 2 * t);
}

/**
 * Evaluate a keyframe lane at `t_seconds`. Mirrors
 * `vance_audio::eval_gain_lane`'s semantics in dB space:
 * - Empty lane → 0 dB (unity). Caller treats empty as "no automation".
 * - Before first / after last → clamp to endpoint value.
 * - Per-segment interp uses the START keyframe's `interp` field.
 */
export function evalKeyframeLaneDb(lane: GainKeyframe[], t: number): number {
  if (lane.length === 0) return 0;
  if (lane.length === 1 || t <= lane[0].time_seconds) return lane[0].value;
  const last = lane[lane.length - 1];
  if (t >= last.time_seconds) return last.value;
  for (let i = 0; i < lane.length - 1; i++) {
    const a = lane[i];
    const b = lane[i + 1];
    if (t >= a.time_seconds && t <= b.time_seconds) {
      const span = b.time_seconds - a.time_seconds;
      if (span <= 0) return a.value;
      const frac = (t - a.time_seconds) / span;
      switch (a.interp ?? 'linear') {
        case 'hold':
          return a.value;
        case 'bezier':
          return a.value + (b.value - a.value) * smoothstep(frac);
        case 'linear':
        default:
          return a.value + (b.value - a.value) * frac;
      }
    }
  }
  return last.value;
}

/** Cycle the next interp value — used by the L/B/H chip click. */
export function nextInterp(
  cur: 'linear' | 'bezier' | 'hold' | undefined
): 'linear' | 'bezier' | 'hold' {
  switch (cur ?? 'linear') {
    case 'linear':
      return 'bezier';
    case 'bezier':
      return 'hold';
    case 'hold':
      return 'linear';
  }
}

/** Cycle the next fade curve value — used by the L/E/G chip click. */
export function nextFadeCurve(
  cur: 'linear' | 'equal_power' | 'logarithmic' | undefined
): 'linear' | 'equal_power' | 'logarithmic' {
  switch (cur ?? 'linear') {
    case 'linear':
      return 'equal_power';
    case 'equal_power':
      return 'logarithmic';
    case 'logarithmic':
      return 'linear';
  }
}

/** One-letter chip glyph for an interp value. */
export function interpLetter(i: 'linear' | 'bezier' | 'hold' | undefined): 'L' | 'B' | 'H' {
  switch (i ?? 'linear') {
    case 'linear':
      return 'L';
    case 'bezier':
      return 'B';
    case 'hold':
      return 'H';
  }
}

/** One-letter chip glyph for a fade curve. */
export function fadeCurveLetter(
  c: 'linear' | 'equal_power' | 'logarithmic' | undefined
): 'L' | 'E' | 'G' {
  switch (c ?? 'linear') {
    case 'linear':
      return 'L';
    case 'equal_power':
      return 'E';
    case 'logarithmic':
      return 'G';
  }
}

/**
 * Sort + dedupe a keyframe array. Two keyframes whose times are within
 * `epsilon_t` seconds collapse to the later-modified one (we treat the
 * array's later entry as the survivor). Required because the IPC validator
 * rejects unsorted / duplicate-time lanes.
 */
export function normalizeLane(lane: GainKeyframe[], epsilonT = 1e-6): GainKeyframe[] {
  if (lane.length === 0) return lane;
  const sorted = [...lane].sort((a, b) => a.time_seconds - b.time_seconds);
  const out: GainKeyframe[] = [];
  for (const k of sorted) {
    if (out.length > 0 && Math.abs(out[out.length - 1].time_seconds - k.time_seconds) <= epsilonT) {
      out[out.length - 1] = k;
    } else {
      out.push(k);
    }
  }
  return out;
}
