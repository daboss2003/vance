// Mirror of `crates/vance-timeline/src/keyer.rs`. Keep in sync: these are
// the exact param-key strings the engine's keyer reads, so a typo here
// silently produces a keyer that keys nothing.

/** `Effect.kind` of the keyer effect. */
export const KEYER_KIND = 'keyer';

/** Key colour channels in `[0, 1]` (encoded source values). */
export const KEY_R = 'key_r';
export const KEY_G = 'key_g';
export const KEY_B = 'key_b';
/** Weighting of the two non-primary channels. */
export const KEY_BALANCE = 'balance';
/** Matte gain. */
export const KEY_STRENGTH = 'strength';
/** Matte values at or below this become fully transparent. */
export const KEY_CLIP_BLACK = 'clip_black';
/** Matte values at or above this become fully opaque. */
export const KEY_CLIP_WHITE = 'clip_white';
/** Shrink (+, erode) or grow (−, dilate) the matte, in SOURCE pixels. */
export const KEY_CHOKE_PX = 'choke_px';
/** Matte blur radius, in SOURCE pixels. */
export const KEY_SOFTEN_PX = 'soften_px';
/** Spill suppression amount. */
export const KEY_SPILL = 'spill';
/** Spill reference: 0 = average of the other channels, 1 = their max. */
export const KEY_SPILL_BIAS = 'spill_bias';
/** Invert the matte when non-zero. */
export const KEY_INVERT = 'invert';

/** Engine default for every keyer param (pure green key). */
export const KEYER_DEFAULTS: Readonly<Record<string, number>> = {
  [KEY_R]: 0,
  [KEY_G]: 1,
  [KEY_B]: 0,
  [KEY_BALANCE]: 0.5,
  [KEY_STRENGTH]: 1,
  [KEY_CLIP_BLACK]: 0,
  [KEY_CLIP_WHITE]: 0.9,
  [KEY_CHOKE_PX]: 0,
  [KEY_SOFTEN_PX]: 0,
  [KEY_SPILL]: 1,
  [KEY_SPILL_BIAS]: 0.5,
  [KEY_INVERT]: 0
};

/** A fresh params map for `add_effect_to_clip` with every default set. */
export function keyerDefaults(): Record<string, number> {
  return { ...KEYER_DEFAULTS };
}

/** Default for one keyer param (0 for an unknown key). */
export function keyerDefault(key: string): number {
  return KEYER_DEFAULTS[key] ?? 0;
}
