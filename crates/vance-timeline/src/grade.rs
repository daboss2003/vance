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

/// Param key: node mode — `"primary"` (lift/gamma/gain wheels) or
/// `"log"` (offset/shadow/midtone/highlight wheels). Defaults to
/// `"primary"` when absent.
pub const KEY_MODE: &str = "mode";
/// Param key: human-readable node label (defaults to "Node N").
pub const KEY_LABEL: &str = "label";

/// Mode value: primary (lift/gamma/gain) wheels.
pub const MODE_PRIMARY: &str = "primary";
/// Mode value: log (offset/shadow/midtone/highlight) wheels.
pub const MODE_LOG: &str = "log";

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

/// Max grade nodes the single-pass renderer evaluates per clip. Nodes
/// beyond this are ignored (the UI should warn). Matches the GPU
/// uniform array size in `vance-render`.
pub const MAX_GRADE_NODES: usize = 8;
