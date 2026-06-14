//! Effect placeholder.
//!
//! v1 effects (the actual transform graph + parameter system) land in
//! Phase 5. This crate just defines the shape of an effect *reference*
//! so clips and tracks can carry a serializable list of them today —
//! when Phase 5 lands, it fills in the parameters and binds each
//! `Effect.kind` to a real implementation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::id::EffectId;

/// A placeholder for an effect's typed parameters. v1 will replace this
/// with a typed parameter tree; we keep it as a string-keyed map for
/// now so projects from this milestone forward-migrate without losing
/// data.
pub type EffectParams = BTreeMap<String, EffectValue>;

/// A loosely-typed parameter value. Numbers can also be animated
/// via a `Keyframes` list — each entry pins a value at a
/// clip-relative time, evaluated by linear interpolation between
/// surrounding keyframes (5.9 will add bezier).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EffectValue {
    /// A plain number.
    Number(f64),
    /// A boolean toggle.
    Bool(bool),
    /// A free-form string.
    String(String),
    /// Animated numeric value — a list of keyframes in ascending
    /// time order, interpolated between samples.
    Keyframes(Vec<Keyframe>),
    /// A scripted value (GOAT expression engine) — a `vance-expr` script
    /// evaluated per frame against `time` (clip-relative seconds) etc.
    /// Serializes as `{ expr: "..." }`, distinct from the scalar/array
    /// shapes so the untagged enum stays unambiguous and older projects
    /// are unaffected.
    Expr {
        /// The expression source (e.g. `"value + 30.0 * noise(time * 2.0)"`).
        expr: String,
    },
}

/// How the engine should interpolate FROM this keyframe to the next
/// one. Stored per-keyframe (outgoing side) so a curve can mix
/// linear and eased segments. `Linear` is the default and matches
/// the 5.5 behavior; older projects without the field deserialize
/// to it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum KeyframeInterp {
    /// Straight line between this keyframe and the next.
    #[default]
    Linear,
    /// Smooth "ease in / ease out" — cubic curve with auto tangents
    /// derived from the segment endpoints. v2 will allow custom
    /// tangents; v1 ships the auto shape.
    Bezier,
    /// Step: value stays at this keyframe's value until the next one,
    /// then snaps. Useful for boolean-like sequences.
    Hold,
}

/// One keyframe pinning a numeric value at a specific clip-relative
/// time (seconds from the clip's `source_in`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Keyframe {
    /// Clip-relative time in seconds (for animation), or input level
    /// when this `Keyframe` is reused as a curve control point (6.8).
    pub time_seconds: f64,
    /// Pinned numeric value, or output level for a curve control point.
    pub value: f64,
    /// Outgoing interpolation toward the next keyframe. Defaults to
    /// `Linear` so older project files (which lacked the field)
    /// continue to play unchanged.
    #[serde(default)]
    pub interp: KeyframeInterp,
}

impl EffectValue {
    /// Evaluate this parameter as a number at clip-relative time
    /// `t_seconds`. `Number` returns its constant value; `Keyframes`
    /// linearly interpolates between the nearest neighbors, clamping at
    /// the endpoints; `Expr` evaluates the script with `time = t_seconds`
    /// (a compile error or non-numeric result yields `None`, so a broken
    /// expression simply falls back to the param's default rather than
    /// crashing the render). `Bool`/`String` return `None`.
    pub fn eval_number(&self, t_seconds: f64) -> Option<f64> {
        match self {
            EffectValue::Number(v) => Some(*v),
            EffectValue::Keyframes(kfs) => Some(eval_keyframes(kfs, t_seconds)),
            EffectValue::Expr { expr } => eval_expr_cached(expr, t_seconds),
            _ => None,
        }
    }

    /// `true` if this value is a scripted expression.
    pub fn is_expr(&self) -> bool {
        matches!(self, EffectValue::Expr { .. })
    }
}

thread_local! {
    /// Per-thread expression engine + compiled-AST cache, so the render
    /// path doesn't rebuild the engine or recompile a script every frame.
    static EXPR_EVAL: std::cell::RefCell<ExprEvalCache> =
        std::cell::RefCell::new(ExprEvalCache::default());
}

#[derive(Default)]
struct ExprEvalCache {
    engine: vance_expr::ExprEngine,
    /// `None` value = the source failed to compile (cached so we don't
    /// retry compiling a broken expression every frame).
    compiled: std::collections::HashMap<String, Option<vance_expr::Expr>>,
}

/// Compile-check an expression source without evaluating it — for live
/// UI validation as the user types. Returns the error message, or `None`
/// when the expression is valid.
pub fn validate_expr(src: &str) -> Option<String> {
    EXPR_EVAL.with(|cell| cell.borrow().engine.validate(src))
}

/// Evaluate `src` at clip-relative `t_seconds`, caching the compiled AST
/// per thread. Returns `None` on compile error or a non-numeric result.
///
/// NOTE: only `time` (= `t_seconds`) and the math stdlib are bound here;
/// `value`/`fps`/`width`/`height`/`frame` default to 0 at this layer. The
/// richer context is threaded in where the render path has it.
fn eval_expr_cached(src: &str, t_seconds: f64) -> Option<f64> {
    EXPR_EVAL.with(|cell| {
        let mut cache = cell.borrow_mut();
        if !cache.compiled.contains_key(src) {
            let compiled = cache.engine.compile(src).ok();
            cache.compiled.insert(src.to_string(), compiled);
        }
        let ExprEvalCache { engine, compiled } = &*cache;
        let expr = compiled.get(src)?.as_ref()?;
        let ctx = vance_expr::EvalContext {
            time: t_seconds,
            ..Default::default()
        };
        engine.eval(expr, &ctx).ok()
    })
}

/// Evaluate a curve given as control points at input level `x`.
///
/// Phase 6.8 color curves reuse the [`Keyframe`] shape as curve control
/// points — `time_seconds` is the input level and `value` the output
/// level, with `interp` the segment shape (Linear / Bezier=smoothstep /
/// Hold). This is exactly the [`eval_keyframes`] math (sorted,
/// clamp-at-endpoints, per-segment interp), so curves bake with no new
/// interpolation code.
///
/// NOTE: an *empty* point list returns 0.0 (the keyframe convention) —
/// callers baking a curve LUT must treat empty as the identity ramp
/// `y = x`, never 0, or the curve silently crushes to black.
pub fn eval_curve(points: &[Keyframe], x: f64) -> f64 {
    eval_keyframes(points, x)
}

fn eval_keyframes(kfs: &[Keyframe], t: f64) -> f64 {
    if kfs.is_empty() {
        return 0.0;
    }
    if kfs.len() == 1 || t <= kfs[0].time_seconds {
        return kfs[0].value;
    }
    if t >= kfs[kfs.len() - 1].time_seconds {
        return kfs[kfs.len() - 1].value;
    }
    // Find the segment containing t.
    for i in 0..kfs.len() - 1 {
        let a = kfs[i];
        let b = kfs[i + 1];
        if t >= a.time_seconds && t <= b.time_seconds {
            let span = b.time_seconds - a.time_seconds;
            if span <= 0.0 {
                return a.value;
            }
            let frac = (t - a.time_seconds) / span;
            // `a.interp` governs the *outgoing* side of segment i.
            return match a.interp {
                KeyframeInterp::Linear => a.value + (b.value - a.value) * frac,
                KeyframeInterp::Hold => a.value,
                KeyframeInterp::Bezier => {
                    // Smoothstep easing (3t² − 2t³): zero derivative at
                    // both ends, monotonic. Enough for v1; custom-
                    // tangent cubic Bezier lands later.
                    let s = frac * frac * (3.0 - 2.0 * frac);
                    a.value + (b.value - a.value) * s
                }
            };
        }
    }
    kfs[kfs.len() - 1].value
}

/// One effect attached to a clip or track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Effect {
    /// Stable ID.
    pub id: EffectId,
    /// Kind identifier — e.g. `"transform"`, `"opacity"`, `"audio.gain"`.
    /// Looked up against the effect registry once Phase 5 lands.
    pub kind: String,
    /// Whether the effect contributes to the render. Disabled effects
    /// stay in the project for one-click re-enable.
    pub enabled: bool,
    /// Free-form parameter map for now (see module docs).
    pub params: EffectParams,
}

impl Effect {
    /// Create a new effect with a fresh ID, no parameters, enabled.
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            id: EffectId::new(),
            kind: kind.into(),
            enabled: true,
            params: EffectParams::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_effect_is_enabled_with_no_params() {
        let e = Effect::new("opacity");
        assert!(e.enabled);
        assert!(e.params.is_empty());
        assert_eq!(e.kind, "opacity");
    }

    #[test]
    fn expr_value_evaluates_against_time() {
        // A time-driven expression resolves through eval_number.
        let v = EffectValue::Expr {
            expr: "10.0 + time * 2.0".into(),
        };
        assert!(v.is_expr());
        assert_eq!(v.eval_number(0.0), Some(10.0));
        assert_eq!(v.eval_number(5.0), Some(20.0));
    }

    #[test]
    fn expr_value_broken_script_falls_back_to_none() {
        // A compile error must NOT panic the render — it yields None so
        // the caller uses the param default.
        let v = EffectValue::Expr {
            expr: "this is not valid +".into(),
        };
        assert_eq!(v.eval_number(1.0), None);
        // A non-numeric result is also None.
        let s = EffectValue::Expr {
            expr: "\"text\"".into(),
        };
        assert_eq!(s.eval_number(1.0), None);
    }

    #[test]
    fn expr_value_round_trips_through_serde_untagged() {
        // The object shape `{ expr: "..." }` is distinct from the scalar /
        // array variants, so the untagged enum stays unambiguous.
        let v = EffectValue::Expr {
            expr: "value + noise(time)".into(),
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: EffectValue = serde_json::from_str(&json).unwrap();
        assert_eq!(v, back);
        // A bare number / string still deserialize to their own variants.
        assert_eq!(
            serde_json::from_str::<EffectValue>("4.5").unwrap(),
            EffectValue::Number(4.5)
        );
        assert_eq!(
            serde_json::from_str::<EffectValue>("\"hi\"").unwrap(),
            EffectValue::String("hi".into())
        );
    }

    fn kf(x: f64, y: f64, interp: KeyframeInterp) -> Keyframe {
        Keyframe {
            time_seconds: x,
            value: y,
            interp,
        }
    }

    #[test]
    fn eval_curve_empty_returns_zero() {
        // The keyframe convention — callers baking a LUT must treat an
        // empty list as identity themselves (the renderer does).
        assert_eq!(eval_curve(&[], 0.5), 0.0);
    }

    #[test]
    fn eval_curve_identity_two_point_is_y_equals_x() {
        let id = [
            kf(0.0, 0.0, KeyframeInterp::Linear),
            kf(1.0, 1.0, KeyframeInterp::Linear),
        ];
        for &x in &[0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            assert!((eval_curve(&id, x) - x).abs() < 1e-9, "x={x}");
        }
    }

    #[test]
    fn eval_curve_linear_lift_maps_endpoints_and_mid() {
        // Lift shadows: (0, 0.3) → (1, 1.0). Endpoints exact; midpoint
        // is the linear interpolation 0.3 + 0.5*(1-0.3) = 0.65.
        let c = [
            kf(0.0, 0.3, KeyframeInterp::Linear),
            kf(1.0, 1.0, KeyframeInterp::Linear),
        ];
        assert!((eval_curve(&c, 0.0) - 0.3).abs() < 1e-9);
        assert!((eval_curve(&c, 1.0) - 1.0).abs() < 1e-9);
        assert!((eval_curve(&c, 0.5) - 0.65).abs() < 1e-9);
    }

    #[test]
    fn eval_curve_clamps_at_endpoints_outside_domain() {
        let c = [
            kf(0.2, 0.1, KeyframeInterp::Linear),
            kf(0.8, 0.9, KeyframeInterp::Linear),
        ];
        assert_eq!(eval_curve(&c, -1.0), 0.1); // below first → first value
        assert_eq!(eval_curve(&c, 2.0), 0.9); // above last → last value
    }

    #[test]
    fn eval_curve_three_point_uses_correct_segment() {
        // S-ish: (0,0) (0.5,0.7) (1,1). Mid of first segment = 0.35.
        let c = [
            kf(0.0, 0.0, KeyframeInterp::Linear),
            kf(0.5, 0.7, KeyframeInterp::Linear),
            kf(1.0, 1.0, KeyframeInterp::Linear),
        ];
        assert!((eval_curve(&c, 0.25) - 0.35).abs() < 1e-9);
        assert!((eval_curve(&c, 0.5) - 0.7).abs() < 1e-9);
        // Second segment midpoint: 0.7 + 0.5*(1-0.7) = 0.85.
        assert!((eval_curve(&c, 0.75) - 0.85).abs() < 1e-9);
    }
}
