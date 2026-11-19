//! Resolve [`Anim`] scalars to concrete values at a comp-relative time.

use std::cell::RefCell;

use vance_expr::{EvalContext, ExprEngine};
use vance_mograph::eval::eval_keys;

use crate::model::Anim;

thread_local! {
    // One sandboxed expression engine per thread (mirrors vance-mograph):
    // compiles are cheap and a render touches few expressions.
    static ENGINE: RefCell<ExprEngine> = RefCell::new(ExprEngine::new());
}

/// Canvas + timing context for expression evaluation.
#[derive(Clone, Copy, Debug)]
pub struct AnimCtx {
    /// Canvas width in px.
    pub width: f64,
    /// Canvas height in px.
    pub height: f64,
    /// Frames per second.
    pub fps: f64,
    /// Comp duration in seconds.
    pub duration: f64,
}

impl Anim {
    /// Resolve this scalar at time `t` (seconds).
    pub fn resolve(&self, t: f64, ctx: AnimCtx) -> f64 {
        match self {
            Anim::Const(v) => *v,
            Anim::Keys(keys) => eval_keys(keys, t),
            Anim::Expr(src) => ENGINE.with(|e| {
                let ectx = EvalContext {
                    time: t,
                    frame: t * ctx.fps,
                    value: 0.0,
                    duration: ctx.duration,
                    fps: ctx.fps,
                    width: ctx.width,
                    height: ctx.height,
                };
                e.borrow().eval_str(src, &ectx).unwrap_or(0.0)
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vance_mograph::{Interp, Key};

    fn ctx() -> AnimCtx {
        AnimCtx {
            width: 1920.0,
            height: 1080.0,
            fps: 30.0,
            duration: 5.0,
        }
    }

    #[test]
    fn const_keys_expr_resolve() {
        assert_eq!(Anim::Const(3.0).resolve(1.0, ctx()), 3.0);
        let lane = Anim::Keys(vec![
            Key {
                t: 0.0,
                v: 0.0,
                interp: Interp::Linear,
            },
            Key {
                t: 1.0,
                v: 10.0,
                interp: Interp::Linear,
            },
        ]);
        assert!((lane.resolve(0.5, ctx()) - 5.0).abs() < 1e-9);
        let e = Anim::Expr("time / duration".into());
        assert!((e.resolve(2.5, ctx()) - 0.5).abs() < 1e-9);
    }
}
