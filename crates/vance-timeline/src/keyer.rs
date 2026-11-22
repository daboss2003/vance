//! Parameter keys of the `keyer` clip effect (ADR 0007).
//!
//! A primary-channel colour-difference keyer that runs on the encoded
//! source. Every parameter is numeric (so keyframeable); the default for
//! each key is noted beside it. Pixel distances are in SOURCE pixels so
//! preview and export agree at any canvas size.

/// `Effect.kind` of the keyer effect.
pub const KEYER_KIND: &str = "keyer";
/// Key colour, red channel in `[0, 1]`. Default 0.
pub const KEY_R: &str = "key_r";
/// Key colour, green channel in `[0, 1]`. Default 1.
pub const KEY_G: &str = "key_g";
/// Key colour, blue channel in `[0, 1]`. Default 0.
pub const KEY_B: &str = "key_b";
/// Weighting of the two non-primary channels. Default 0.5.
pub const KEY_BALANCE: &str = "balance";
/// Matte gain. Default 1.0.
pub const KEY_STRENGTH: &str = "strength";
/// Matte values at or below this become fully transparent. Default 0.0.
pub const KEY_CLIP_BLACK: &str = "clip_black";
/// Matte values at or above this become fully opaque. Default 1.0.
pub const KEY_CLIP_WHITE: &str = "clip_white";
/// Shrink (+, erode) or grow (−, dilate) the matte, in source pixels. Default 0.
pub const KEY_CHOKE_PX: &str = "choke_px";
/// Matte blur radius, in source pixels. Default 0.
pub const KEY_SOFTEN_PX: &str = "soften_px";
/// Spill suppression amount. Default 1.0.
pub const KEY_SPILL: &str = "spill";
/// Spill reference: 0 = average of the other channels, 1 = their max. Default 0.5.
pub const KEY_SPILL_BIAS: &str = "spill_bias";
/// Invert the matte when non-zero. Default 0.
pub const KEY_INVERT: &str = "invert";
