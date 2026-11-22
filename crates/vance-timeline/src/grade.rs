//! Color-grade node constants (Phase 6.6 / 6.7).
//!
//! A "grade node" on the color page is not a new type — it's an
//! ordinary [`crate::effect::Effect`] whose `kind` is [`GRADE_KIND`],
//! stored in `Clip.effects` in serial-chain order (top of the list =
//! first applied). Per-node parameters live in the effect's
//! string-keyed `params` map, so every wheel value is keyframe-
//! animatable for free through `EffectValue::eval_number`.
//!
//! These constants centralize the `kind` string and every param key so
//! the engine evaluator, the IPC layer, and the UI all agree on the
//! exact spelling — stringly-typed keys are otherwise a silent-typo
//! hazard. The TypeScript side mirrors these in
//! `ui/src/lib/grade-keys.ts`.
//!
//! The param-key constants are self-describing (the module-level docs
//! group them), so we opt out of `missing_docs` for the individual
//! `const` items rather than repeat each key name as a doc comment.
#![allow(missing_docs)]

/// `Effect.kind` value identifying a color-grade node.
pub const GRADE_KIND: &str = "grade";

/// Param key: node mode — `"primary"` (lift/gamma/gain wheels),
/// `"log"` (offset/shadow/midtone/highlight wheels), or `"curves"`
/// (per-channel tone curves). Defaults to `"primary"` when absent.
pub const KEY_MODE: &str = "mode";
/// Param key: human-readable node label (defaults to "Node N").
pub const KEY_LABEL: &str = "label";

/// Mode value: primary (lift/gamma/gain) wheels.
pub const MODE_PRIMARY: &str = "primary";
/// Mode value: log (offset/shadow/midtone/highlight) wheels.
pub const MODE_LOG: &str = "log";
/// Mode value: tone curves (master + R/G/B + luma). Phase 6.8.
pub const MODE_CURVES: &str = "curves";

// ---- Curve keys (Phase 6.8) -------------------------------------------
//
// Each curve_* key holds an `EffectValue::Keyframes` whose points are
// CURVE CONTROL POINTS, not animation keyframes: `time_seconds` is the
// input level (x, 0..1) and `value` is the output level (y, 0..1), with
// `interp` the segment shape. The engine bakes them into a 1-D LUT row
// via `vance_timeline::eval_curve`. An ABSENT or EMPTY curve bakes to
// the identity ramp `y = x` (never 0). The master curve is composited
// into each of R/G/B at bake time: `curve_r(curve_m(x))`.
/// Master tone curve (folded into R/G/B at bake time).
pub const KEY_CURVE_MASTER: &str = "curve_m";
/// Red channel tone curve.
pub const KEY_CURVE_R: &str = "curve_r";
/// Green channel tone curve.
pub const KEY_CURVE_G: &str = "curve_g";
/// Blue channel tone curve.
pub const KEY_CURVE_B: &str = "curve_b";
/// Luma curve (applied as a hue-preserving ratio).
pub const KEY_CURVE_LUMA: &str = "curve_luma";

// ---- HSL hue-wheel curve keys (Phase 6.8b — defined, not yet rendered)
//
// These are STRUCTURALLY different from the tone curves above: the
// input channel differs from the output channel (e.g. hue-vs-sat looks
// up by the pixel's hue but scales its saturation), the hue domain is
// cyclic (0..1 wraps), and applying them needs an RGB<->HSL conversion
// in the shader. 6.8 defines + round-trips these keys so projects
// authored now carry the data, but the v1 baker ignores them and the
// UI greys the HSL tabs. 6.8b adds: a wrapping (Repeat) sampler for the
// hue domain, the RGB<->HSL math, extra atlas rows per node, and an
// `apply_hsl_curves` block; the atlas grows in height (rows-per-node)
// with no format break.
/// Hue → hue rotation curve. (6.8b)
pub const KEY_CURVE_HUE_HUE: &str = "curve_hh";
/// Hue → saturation scale curve. (6.8b)
pub const KEY_CURVE_HUE_SAT: &str = "curve_hs";
/// Hue → luminance scale curve. (6.8b)
pub const KEY_CURVE_HUE_LUM: &str = "curve_hl";
/// Saturation → saturation curve. (6.8b)
pub const KEY_CURVE_SAT_SAT: &str = "curve_ss";
/// Luminance → luminance curve (in HSL space). (6.8b)
pub const KEY_CURVE_LUM_LUM: &str = "curve_ll";

/// Width of the baked curve LUT (one column per input level).
pub const CURVE_LUT_WIDTH: usize = 256;

// ---- Primary mode keys (lift / gamma / gain, per channel + master) ----
/// Lift (shadows) per-channel + master. Default 0.0.
pub const KEY_LIFT_R: &str = "lift_r";
pub const KEY_LIFT_G: &str = "lift_g";
pub const KEY_LIFT_B: &str = "lift_b";
pub const KEY_LIFT_Y: &str = "lift_y";
/// Gamma (midtones) per-channel + master. Default 1.0.
pub const KEY_GAMMA_R: &str = "gamma_r";
pub const KEY_GAMMA_G: &str = "gamma_g";
pub const KEY_GAMMA_B: &str = "gamma_b";
pub const KEY_GAMMA_Y: &str = "gamma_y";
/// Gain (highlights) per-channel + master. Default 1.0.
pub const KEY_GAIN_R: &str = "gain_r";
pub const KEY_GAIN_G: &str = "gain_g";
pub const KEY_GAIN_B: &str = "gain_b";
pub const KEY_GAIN_Y: &str = "gain_y";

// ---- Log mode keys (offset / shadow / midtone / highlight) ----
/// Offset (overall lift) per-channel. Default 0.0.
pub const KEY_OFFSET_R: &str = "offset_r";
pub const KEY_OFFSET_G: &str = "offset_g";
pub const KEY_OFFSET_B: &str = "offset_b";
/// Shadow region balance per-channel. Default 0.0.
pub const KEY_SHADOW_R: &str = "sh_r";
pub const KEY_SHADOW_G: &str = "sh_g";
pub const KEY_SHADOW_B: &str = "sh_b";
/// Midtone region balance per-channel. Default 0.0.
pub const KEY_MID_R: &str = "mid_r";
pub const KEY_MID_G: &str = "mid_g";
pub const KEY_MID_B: &str = "mid_b";
/// Highlight region balance per-channel. Default 0.0.
pub const KEY_HI_R: &str = "hi_r";
pub const KEY_HI_G: &str = "hi_g";
pub const KEY_HI_B: &str = "hi_b";

// ---- HSL qualifier / secondary key (Phase 6.9) ------------------------
//
// When `qual_enable` is on, the node's correction is masked to pixels
// whose hue is within `hue_c ± hue_range/2` AND whose saturation/luma
// fall in the `[lo, hi]` bands, each feathered by its softness. The
// range/hi defaults are WIDE-OPEN (1.0) so enabling the qualifier and
// setting only a hue doesn't accidentally key to nothing. All values are
// `EffectValue` numbers, so they're keyframe-animatable for free.
/// Restrict the node to the qualifier mask. 0/1, default 0.
pub const KEY_QUAL_ENABLE: &str = "qual_enable";
/// Hue center (cyclic, 0..1). Default 0.
pub const KEY_QUAL_HUE_C: &str = "qual_hue_c";
/// Hue range / full width (0..1). Default 1.0 (no hue restriction).
pub const KEY_QUAL_HUE_RANGE: &str = "qual_hue_range";
/// Hue edge softness. Default 0.05.
pub const KEY_QUAL_HUE_SOFT: &str = "qual_hue_soft";
/// Saturation low bound (0..1). Default 0.
pub const KEY_QUAL_SAT_LO: &str = "qual_sat_lo";
/// Saturation high bound (0..1). Default 1.0.
pub const KEY_QUAL_SAT_HI: &str = "qual_sat_hi";
/// Saturation edge softness. Default 0.05.
pub const KEY_QUAL_SAT_SOFT: &str = "qual_sat_soft";
/// Luma low bound (0..1). Default 0.
pub const KEY_QUAL_LUMA_LO: &str = "qual_luma_lo";
/// Luma high bound (0..1). Default 1.0.
pub const KEY_QUAL_LUMA_HI: &str = "qual_luma_hi";
/// Luma edge softness. Default 0.05.
pub const KEY_QUAL_LUMA_SOFT: &str = "qual_luma_soft";
/// Invert the mask. 0/1, default 0.
pub const KEY_QUAL_INVERT: &str = "qual_invert";
// NOTE: "show mask" (matte solo) is intentionally NOT a persisted param.
// It's a transient viewer state driven by the engine's `set_matte_node`
// (Phase 6.9) so a saved project never reopens stuck in matte view.

// ---- Power window / shape mask (Phase 6.10) ---------------------------
//
// A geometric mask intersected with the qualifier mask. All keys are
// keyframe-animatable numbers (the 6.11 point tracker animates the
// center). `win_shape` 0 = OFF (mask = 1, exact passthrough).
/// Window shape: 0 off, 1 ellipse, 2 rectangle. Default 0.
pub const KEY_WIN_SHAPE: &str = "win_shape";
/// Center X in source-UV [0,1]. Default 0.5.
pub const KEY_WIN_CENTER_X: &str = "win_cx";
/// Center Y in source-UV [0,1]. Default 0.5.
pub const KEY_WIN_CENTER_Y: &str = "win_cy";
/// Half-width (radius x) as a fraction of the clip's width. Default: round
/// on screen with the default half-height, `0.25 × height / display width`.
pub const KEY_WIN_SIZE_X: &str = "win_sx";
/// Half-height (radius y) in source-UV units. Default 0.25.
pub const KEY_WIN_SIZE_Y: &str = "win_sy";
/// Rotation in radians (CCW). Default 0.
pub const KEY_WIN_ROTATION: &str = "win_rot";
/// Feather / softness band width. Default 0.1.
pub const KEY_WIN_FEATHER: &str = "win_feather";
/// Invert the window mask. 0/1, default 0.
pub const KEY_WIN_INVERT: &str = "win_invert";

/// Max grade nodes the single-pass renderer evaluates per clip. Nodes
/// beyond this are ignored (the UI should warn). Matches the GPU
/// uniform array size in `vance-render`.
pub const MAX_GRADE_NODES: usize = 8;
