//! Resolve the rig's animatable/bindable properties to concrete values
//! at a clip-relative time `t`.

use std::cell::RefCell;

use vance_expr::{EvalContext, ExprEngine};

use crate::model::{ColorProp, Interp, Key, Prop, Rgba, TextProp};
use crate::value::{ControlValue, MographInstance};

thread_local! {
    // One sandboxed expression engine per thread; compiles are cheap and
    // a render touches few expressions, so we compile-on-eval.
    static ENGINE: RefCell<ExprEngine> = RefCell::new(ExprEngine::new());
}

/// Canvas + timing context an expression / point-bind needs.
#[derive(Clone, Copy, Debug)]
pub struct EvalDims {
    /// Canvas width in px.
    pub width: f64,
    /// Canvas height in px.
    pub height: f64,
    /// Frames per second (for `frame`/`fps` in expressions).
    pub fps: f64,
    /// Clip duration in seconds.
    pub duration: f64,
}

impl Prop {
    /// Resolve this scalar property at time `t` (seconds).
    pub fn resolve(&self, inst: &MographInstance, t: f64, dims: EvalDims) -> f64 {
        match self {
            Prop::Const(v) => *v,
            Prop::Keys(keys) => eval_keys(keys, t),
            Prop::Expr(src) => ENGINE.with(|e| {
                let ctx = EvalContext {
                    time: t,
                    frame: t * dims.fps,
                    value: 0.0,
                    duration: dims.duration,
                    fps: dims.fps,
                    width: dims.width,
                    height: dims.height,
                };
                e.borrow().eval_str(src, &ctx).unwrap_or(0.0)
            }),
            Prop::Bind(id) => match inst.value(id) {
                Some(ControlValue::Number(v)) => v,
                Some(ControlValue::Bool(true)) => 1.0,
                Some(ControlValue::Bool(false)) => 0.0,
                Some(ControlValue::Choice(i)) => i as f64,
                _ => 0.0,
            },
            Prop::PointX(id) => point_axis(inst, id, 0) * dims.width,
            Prop::PointY(id) => point_axis(inst, id, 1) * dims.height,
        }
    }
}

fn point_axis(inst: &MographInstance, id: &str, axis: usize) -> f64 {
    match inst.value(id) {
        Some(ControlValue::Point(p)) => p[axis],
        _ => 0.0,
    }
}

impl ColorProp {
    /// Resolve to an RGBA color.
    pub fn resolve(&self, inst: &MographInstance) -> Rgba {
        match self {
            ColorProp::Const(c) => *c,
            ColorProp::Bind(id) => match inst.value(id) {
                Some(ControlValue::Color(c)) => c,
                _ => [1.0, 1.0, 1.0, 1.0],
            },
        }
    }
}

impl TextProp {
    /// Resolve to a string. A bind to a `Choice` control yields the
    /// selected option's label; to `Text`/`Font`, the string itself.
    pub fn resolve(&self, inst: &MographInstance) -> String {
        match self {
            TextProp::Const(s) => s.clone(),
            TextProp::Bind(id) => match inst.value(id) {
                Some(ControlValue::Text(s)) | Some(ControlValue::Font(s)) => s,
                Some(ControlValue::Choice(i)) => choice_label(inst, id, i),
                _ => String::new(),
            },
        }
    }
}

fn choice_label(inst: &MographInstance, id: &str, idx: usize) -> String {
    use crate::model::ControlKind;
    match inst.control(id).map(|c| &c.kind) {
        Some(ControlKind::Choice { options, .. }) => options.get(idx).cloned().unwrap_or_default(),
        _ => String::new(),
    }
}

/// Evaluate a keyframe lane at time `t`. Empty → 0.0; clamps at the
/// endpoints; interpolates per the segment's `interp`.
pub fn eval_keys(keys: &[Key], t: f64) -> f64 {
    match keys {
        [] => 0.0,
        [only] => only.v,
        _ => {
            if t <= keys[0].t {
                return keys[0].v;
            }
            let last = &keys[keys.len() - 1];
            if t >= last.t {
                return last.v;
            }
            // Find the bracketing segment [a, b].
            let mut a = &keys[0];
            for w in keys.windows(2) {
                let (lo, hi) = (&w[0], &w[1]);
                if t >= lo.t && t <= hi.t {
                    a = lo;
                    return interp_segment(a, hi, t);
                }
            }
            a.v
        }
    }
}

fn interp_segment(a: &Key, b: &Key, t: f64) -> f64 {
    let span = b.t - a.t;
    if span <= 0.0 {
        return b.v;
    }
    let u = ((t - a.t) / span).clamp(0.0, 1.0);
    match a.interp {
        Interp::Hold => a.v,
        Interp::Linear => a.v + (b.v - a.v) * u,
        Interp::Smooth => {
            let s = u * u * (3.0 - 2.0 * u); // smoothstep
            a.v + (b.v - a.v) * s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins;
    use crate::model::Prop;
    use crate::value::{ControlValue, MographInstance};

    fn k(t: f64, v: f64, i: Interp) -> Key {
        Key { t, v, interp: i }
    }

    #[test]
    fn keys_clamp_and_interpolate() {
        let lane = vec![k(0.0, 0.0, Interp::Linear), k(1.0, 10.0, Interp::Linear)];
        assert_eq!(eval_keys(&lane, -1.0), 0.0, "clamp before first");
        assert_eq!(eval_keys(&lane, 2.0), 10.0, "clamp after last");
        assert!(
            (eval_keys(&lane, 0.5) - 5.0).abs() < 1e-9,
            "linear midpoint"
        );
    }

    #[test]
    fn hold_and_smooth_interp() {
        let hold = vec![k(0.0, 1.0, Interp::Hold), k(1.0, 9.0, Interp::Linear)];
        assert_eq!(eval_keys(&hold, 0.99), 1.0, "hold stays until next key");
        let smooth = vec![k(0.0, 0.0, Interp::Smooth), k(1.0, 1.0, Interp::Linear)];
        // smoothstep(0.5) = 0.5, but eases at the ends.
        assert!((eval_keys(&smooth, 0.5) - 0.5).abs() < 1e-9);
        assert!(eval_keys(&smooth, 0.25) < 0.25, "eased in below linear");
    }

    fn dims() -> EvalDims {
        EvalDims {
            width: 1920.0,
            height: 1080.0,
            fps: 30.0,
            duration: 5.0,
        }
    }

    #[test]
    fn number_and_bool_binds_resolve() {
        let mut inst = MographInstance::new(builtins::centered_title());
        inst.set("size", ControlValue::Number(120.0));
        let p = Prop::Bind("size".into());
        assert_eq!(p.resolve(&inst, 0.0, dims()), 120.0);
    }

    #[test]
    fn expr_prop_uses_time_and_duration() {
        let inst = MographInstance::new(builtins::centered_title());
        // Linear ramp 0→1 across the clip duration.
        let p = Prop::Expr("time / duration".into());
        assert!((p.resolve(&inst, 2.5, dims()) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn choice_bind_resolves_to_option_label() {
        use crate::model::{Control, ControlKind};
        let mut tpl = builtins::centered_title();
        tpl.controls.push(Control {
            id: "style".into(),
            label: "Style".into(),
            kind: ControlKind::Choice {
                options: vec!["Bold".into(), "Light".into()],
                default: 0,
            },
        });
        let mut inst = MographInstance::new(tpl);
        inst.set("style", ControlValue::Choice(1));
        let tp = crate::model::TextProp::Bind("style".into());
        assert_eq!(tp.resolve(&inst), "Light");
    }
}
