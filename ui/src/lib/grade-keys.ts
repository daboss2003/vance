// Mirror of `crates/vance-timeline/src/grade.rs`. Keep in sync — these
// are the exact param-key strings the engine evaluator reads, so a
// typo here silently produces a no-op wheel.

export const GRADE_KIND = 'grade';

export const KEY_MODE = 'mode';
export const KEY_LABEL = 'label';
export const MODE_PRIMARY = 'primary';
export const MODE_LOG = 'log';
export const MODE_CURVES = 'curves';

// Curve keys (Phase 6.8). Each holds an EffectValue::Keyframes whose
// points are curve control points: time_seconds = input level (x),
// value = output level (y). Master is folded into R/G/B at bake time.
export const CURVE_KEYS = {
  master: 'curve_m',
  r: 'curve_r',
  g: 'curve_g',
  b: 'curve_b',
  luma: 'curve_luma'
} as const;

// HSL hue-wheel curve keys — defined for forward-compat (6.8b); the
// 6.8 renderer ignores them and the UI greys these tabs.
export const HSL_CURVE_KEYS = {
  hueHue: 'curve_hh',
  hueSat: 'curve_hs',
  hueLum: 'curve_hl',
  satSat: 'curve_ss',
  lumLum: 'curve_ll'
} as const;

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

// HSL qualifier / secondary key (Phase 6.9). Mirror of grade.rs. The
// range/hi defaults are WIDE-OPEN (1.0) so enabling the qualifier and
// setting only a hue doesn't key to nothing.
//
// 6.9b: eyedropper "pick-from-pixel" (set hue_c / sat_lo,hi / luma_lo,hi
// by clicking a viewport pixel) is a separable follow-up — it needs a
// new viewport->pixel readback IPC (the on-viewport overlay path that
// 6.10b is also blocked on; see ui/src/routes/+page.svelte). The
// numeric sliders below are the complete v1 surface.
export const QUALIFIER_KEYS = {
  enable: 'qual_enable',
  hueC: 'qual_hue_c',
  hueRange: 'qual_hue_range',
  hueSoft: 'qual_hue_soft',
  satLo: 'qual_sat_lo',
  satHi: 'qual_sat_hi',
  satSoft: 'qual_sat_soft',
  lumaLo: 'qual_luma_lo',
  lumaHi: 'qual_luma_hi',
  lumaSoft: 'qual_luma_soft',
  invert: 'qual_invert'
  // NOTE: "show mask" is NOT a param — it's transient viewer state driven
  // by the `set_matte_node` IPC command (never persisted).
} as const;

/** Default for a qualifier param: range/hi → 1, the three softs → 0.05,
 *  everything else (enable/center/lo/invert/show) → 0. */
export function qualifierDefault(key: string): number {
  if (key === 'qual_hue_range' || key === 'qual_sat_hi' || key === 'qual_luma_hi') return 1;
  if (key.endsWith('_soft')) return 0.05;
  return 0;
}

// Power window / shape mask (Phase 6.10). Mirror of grade.rs — keep in
// sync. shape 0 = off, 1 = ellipse, 2 = rectangle.
export const WINDOW_KEYS = {
  shape: 'win_shape',
  centerX: 'win_cx',
  centerY: 'win_cy',
  sizeX: 'win_sx',
  sizeY: 'win_sy',
  rotation: 'win_rot',
  feather: 'win_feather',
  invert: 'win_invert'
} as const;

/** Default for a window param: center → 0.5, size → 0.25, feather → 0.1,
 *  else (shape/rotation/invert) → 0. */
export function windowDefault(key: string): number {
  if (key === 'win_cx' || key === 'win_cy') return 0.5;
  if (key === 'win_sx' || key === 'win_sy') return 0.25;
  if (key === 'win_feather') return 0.1;
  return 0;
}

export const MAX_GRADE_NODES = 8;
