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
//! Only [`FadeCurve::Linear`] is implemented in 7.4. The enum is shipped
//! complete (with `EqualPower` and `Logarithmic` variants) so 7.5 can
//! add the missing shapes without a schema break or another EditOp.

use serde::{Deserialize, Serialize};

/// Shape of the fade curve as the gain transitions between silent
/// and unity.
///
/// `Linear` is the only variant implemented in 7.4. The other two are
/// reserved for 7.5 and currently fall back to linear at evaluation
/// time. Persisting them today is intentional — projects authored
/// against 7.5 must remain readable in older betas, and vice-versa.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FadeCurve {
    /// Straight ramp in amplitude. Default for forward-compat with
    /// projects that predate the field.
    #[default]
    Linear,
    /// −3 dB equal-power crossfade shape. Reserved for 7.5; the 7.4
    /// mixer treats this as `Linear` until the math lands so projects
    /// remain playable rather than silent.
    EqualPower,
    /// Logarithmic / "audio taper" — slow at silence, fast near unity.
    /// Reserved for 7.5; treated as `Linear` at evaluation today.
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
/// also returns 1.0 (the "no fade" case). Linear shape only in 7.4;
/// other curves fall back to linear so they don't silently mute.
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
        FadeCurve::Linear | FadeCurve::EqualPower | FadeCurve::Logarithmic => frac,
    }
}

/// Evaluate the multiplicative fade-out gain at clip-relative time
/// `t_clip_seconds`. `clip_duration_seconds` is the clip's total
/// length (timeline-out − timeline-in). Returns 1.0 when the playhead
/// is before the fade region. Linear shape only in 7.4; other curves
/// fall back to linear.
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
        FadeCurve::Linear | FadeCurve::EqualPower | FadeCurve::Logarithmic => frac,
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
    fn future_curves_fall_back_to_linear_in_v1() {
        // Reserved-for-7.5 variants must not silently mute audio.
        for c in [FadeCurve::EqualPower, FadeCurve::Logarithmic] {
            let g_in = eval_fade_in(2.0, c, 1.0);
            let g_out = eval_fade_out(2.0, c, 5.0, 4.0);
            assert!((g_in - 0.5).abs() < 1e-6);
            assert!((g_out - 0.5).abs() < 1e-6);
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
