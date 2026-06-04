//! Per-clip audio fades — a thin, project-level data shape for the
//! mixer to compose into the gain stage.
//!
//! Fades are kept SEPARATE from the manual `gain_keyframes` automation
//! lane (Phase 7.4 M2) so the two can coexist without clobbering each
//! other: setting a fade-in does not erase keyframes the user placed
//! mid-clip, and dragging a keyframe does not eat the fade region.
//! The mixer multiplies fade ∗ keyframe-lane ∗ static-gain at evaluation
//! time, so the data model stays orthogonal.
//!
//! All three [`FadeCurve`] variants ship real implementations since
//! Phase 7.5 M2. `eval_fade_in` / `eval_fade_out` mirror the math in
//! `vance-audio::eval_fade_*_audio` exactly so the two crates stay in
//! lockstep without depending on each other.

use serde::{Deserialize, Serialize};

/// Shape of the fade curve as the gain transitions between silent
/// and unity. All three variants are implemented since 7.5 M2; the
/// enum was always serialized complete so projects authored on
/// 7.5 round-trip cleanly through any 7.x build.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FadeCurve {
    /// Straight ramp in amplitude — `gain(t) = t/d`. Default for
    /// forward-compat with projects that predate the field.
    #[default]
    Linear,
    /// −3 dB equal-power crossfade shape — `gain(t) = sin(t·π/2)` for
    /// fade-in. Paired fade-in² + fade-out² = 1 keeps total power
    /// constant across a crossfade.
    EqualPower,
    /// Quadratic / "audio taper" — `gain(t) = (t/d)²`. Slow at
    /// silence, fast near unity; the perceptually-linear "audio fade"
    /// most NLEs ship. Named "Logarithmic" by NLE convention even
    /// though the math is a power curve.
    Logarithmic,
}

/// One end of a clip's fade pair. `duration_seconds` is clip-relative;
/// 0.0 means "no fade" and the mixer skips the region entirely.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fade {
    /// Fade length in seconds, clip-relative. Validated to be finite,
    /// non-negative, and `<= clip duration` at edit time.
    pub duration_seconds: f64,
    /// Curve shape. Defaults to `Linear` for projects authored before
    /// the field existed.
    #[serde(default)]
    pub curve: FadeCurve,
}

impl Fade {
    /// Build a `Fade` from a duration. The curve defaults to `Linear`.
    pub fn linear(duration_seconds: f64) -> Self {
        Self {
            duration_seconds,
            curve: FadeCurve::Linear,
        }
    }
}

/// Evaluate the multiplicative fade-in gain at clip-relative time
/// `t_clip_seconds`. Returns 1.0 once `t >= duration`. `duration <= 0`
/// also returns 1.0 (the "no fade" case).
///
/// Phase 7.5 M2: all three curves ship real implementations:
/// `Linear` = `t/d`, `EqualPower` = `sin((t/d)·π/2)`, `Logarithmic` =
/// `(t/d)²`. Defense-in-depth duplicate of `vance-audio`'s
/// `eval_fade_in_audio` — both crates agree on the shape family.
pub fn eval_fade_in(duration_seconds: f64, curve: FadeCurve, t_clip_seconds: f64) -> f32 {
    if !(duration_seconds > 0.0) {
        return 1.0;
    }
    if t_clip_seconds <= 0.0 {
        return 0.0;
    }
    if t_clip_seconds >= duration_seconds {
        return 1.0;
    }
    let frac = (t_clip_seconds / duration_seconds) as f32;
    match curve {
        FadeCurve::Linear => frac,
        FadeCurve::EqualPower => (frac * std::f32::consts::FRAC_PI_2).sin(),
        FadeCurve::Logarithmic => frac * frac,
    }
}

/// Evaluate the multiplicative fade-out gain at clip-relative time
/// `t_clip_seconds`. `clip_duration_seconds` is the clip's total
/// length (timeline-out − timeline-in). Returns 1.0 when the playhead
/// is before the fade region. Phase 7.5 M2 honours all three curves.
pub fn eval_fade_out(
    duration_seconds: f64,
    curve: FadeCurve,
    clip_duration_seconds: f64,
    t_clip_seconds: f64,
) -> f32 {
    if !(duration_seconds > 0.0) {
        return 1.0;
    }
    let fade_start = clip_duration_seconds - duration_seconds;
    if t_clip_seconds <= fade_start {
        return 1.0;
    }
    if t_clip_seconds >= clip_duration_seconds {
        return 0.0;
    }
    let frac = ((clip_duration_seconds - t_clip_seconds) / duration_seconds) as f32;
    match curve {
        FadeCurve::Linear => frac,
        FadeCurve::EqualPower => (frac * std::f32::consts::FRAC_PI_2).sin(),
        FadeCurve::Logarithmic => frac * frac,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_in_zero_duration_is_unity() {
        assert_eq!(eval_fade_in(0.0, FadeCurve::Linear, 0.5), 1.0);
        assert_eq!(eval_fade_in(-1.0, FadeCurve::Linear, 0.5), 1.0);
    }

    #[test]
    fn fade_in_clamps_below_and_above() {
        assert_eq!(eval_fade_in(1.0, FadeCurve::Linear, -0.5), 0.0);
        assert_eq!(eval_fade_in(1.0, FadeCurve::Linear, 0.0), 0.0);
        assert_eq!(eval_fade_in(1.0, FadeCurve::Linear, 1.0), 1.0);
        assert_eq!(eval_fade_in(1.0, FadeCurve::Linear, 2.0), 1.0);
    }

    #[test]
    fn fade_in_linear_midpoint_is_half() {
        let g = eval_fade_in(2.0, FadeCurve::Linear, 1.0);
        assert!((g - 0.5).abs() < 1e-6);
    }

    #[test]
    fn fade_out_zero_duration_is_unity_everywhere() {
        assert_eq!(eval_fade_out(0.0, FadeCurve::Linear, 5.0, 4.9), 1.0);
    }

    #[test]
    fn fade_out_linear_endpoints() {
        // 5s clip, 1s fade-out: unity at t<=4, zero at t>=5, half at 4.5.
        assert_eq!(eval_fade_out(1.0, FadeCurve::Linear, 5.0, 3.0), 1.0);
        assert_eq!(eval_fade_out(1.0, FadeCurve::Linear, 5.0, 4.0), 1.0);
        assert!((eval_fade_out(1.0, FadeCurve::Linear, 5.0, 4.5) - 0.5).abs() < 1e-6);
        assert_eq!(eval_fade_out(1.0, FadeCurve::Linear, 5.0, 5.0), 0.0);
        assert_eq!(eval_fade_out(1.0, FadeCurve::Linear, 5.0, 6.0), 0.0);
    }

    #[test]
    fn equal_power_fade_in_midpoint_is_sin_pi_over_4() {
        // EqualPower fade-in: gain(0.5) = sin(π/4) ≈ 0.7071.
        let g = eval_fade_in(2.0, FadeCurve::EqualPower, 1.0);
        assert!((g - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5);
    }

    #[test]
    fn equal_power_endpoints_match_linear() {
        // 7.5 M2 — EqualPower preserves the endpoint contract: 0 at
        // start, 1 at end. Only the curvature differs from Linear.
        assert_eq!(eval_fade_in(1.0, FadeCurve::EqualPower, 0.0), 0.0);
        assert_eq!(eval_fade_in(1.0, FadeCurve::EqualPower, 1.0), 1.0);
        assert_eq!(eval_fade_out(1.0, FadeCurve::EqualPower, 5.0, 5.0), 0.0);
        assert_eq!(eval_fade_out(1.0, FadeCurve::EqualPower, 5.0, 4.0), 1.0);
    }

    #[test]
    fn logarithmic_fade_in_midpoint_is_one_quarter() {
        // Logarithmic fade-in: gain(0.5) = 0.5² = 0.25.
        let g = eval_fade_in(2.0, FadeCurve::Logarithmic, 1.0);
        assert!((g - 0.25).abs() < 1e-6);
    }

    #[test]
    fn logarithmic_endpoints_match_linear() {
        assert_eq!(eval_fade_in(1.0, FadeCurve::Logarithmic, 0.0), 0.0);
        assert_eq!(eval_fade_in(1.0, FadeCurve::Logarithmic, 1.0), 1.0);
        assert_eq!(eval_fade_out(1.0, FadeCurve::Logarithmic, 5.0, 5.0), 0.0);
        assert_eq!(eval_fade_out(1.0, FadeCurve::Logarithmic, 5.0, 4.0), 1.0);
    }

    #[test]
    fn equal_power_crossfade_keeps_constant_total_power() {
        // Two 1s clips A (fading out) and B (fading in) fully
        // overlap a 1s crossfade. At each interior moment `t`:
        // - fade_in(t)  = sin(t·π/2)
        // - fade_out(t) = cos(t·π/2)
        // - sum of squares = 1.
        // Endpoints excluded since the fade-out past-end gate returns
        // 0 at exact clip-end and the start gate returns 0 at t=0 for
        // the fade-in.
        for k in 1..8 {
            let t = k as f64 / 8.0;
            let g_in = eval_fade_in(1.0, FadeCurve::EqualPower, t);
            let g_out = eval_fade_out(1.0, FadeCurve::EqualPower, 1.0, t);
            let power = g_in * g_in + g_out * g_out;
            assert!((power - 1.0).abs() < 1e-5, "t={t}, power={power}");
        }
    }

    #[test]
    fn fade_curve_default_is_linear() {
        assert_eq!(FadeCurve::default(), FadeCurve::Linear);
    }

    #[test]
    fn fade_linear_constructor() {
        let f = Fade::linear(0.25);
        assert_eq!(f.duration_seconds, 0.25);
        assert_eq!(f.curve, FadeCurve::Linear);
    }
}
