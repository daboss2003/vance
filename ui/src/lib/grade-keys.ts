// Mirror of `crates/vance-timeline/src/grade.rs`. Keep in sync — these
// are the exact param-key strings the engine evaluator reads, so a
// typo here silently produces a no-op wheel.

export const GRADE_KIND = 'grade';

export const KEY_MODE = 'mode';
export const KEY_LABEL = 'label';
export const MODE_PRIMARY = 'primary';
export const MODE_LOG = 'log';

// Primary mode: lift / gamma / gain, per channel (r/g/b) + master (y).
export const PRIMARY_KEYS = {
  lift: ['lift_r', 'lift_g', 'lift_b', 'lift_y'],
  gamma: ['gamma_r', 'gamma_g', 'gamma_b', 'gamma_y'],
  gain: ['gain_r', 'gain_g', 'gain_b', 'gain_y']
} as const;

// Log mode: offset / shadow / midtone / highlight, per channel (r/g/b).
export const LOG_KEYS = {
  offset: ['offset_r', 'offset_g', 'offset_b'],
  shadow: ['sh_r', 'sh_g', 'sh_b'],
  midtone: ['mid_r', 'mid_g', 'mid_b'],
  highlight: ['hi_r', 'hi_g', 'hi_b']
} as const;

/** Default for a wheel-group param: lift/offset/balances → 0, gamma/gain → 1. */
export function gradeDefault(key: string): number {
  if (key.startsWith('gamma_') || key.startsWith('gain_')) return 1;
  return 0;
}

export const MAX_GRADE_NODES = 8;
