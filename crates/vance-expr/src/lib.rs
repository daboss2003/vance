//! `vance-expr` — sandboxed expression engine for parameter scripting.
//!
//! After Effects-style: any effect / keyframe parameter can be driven by
//! a small script evaluated per frame against a typed [`EvalContext`]
//! (`time`, `frame`, `value`, …) plus a math standard library. Powers the
//! GOAT "expression engine" feature.
//!
//! ## Sandbox
//!
//! Scripts are untrusted, so the engine is locked down (the same posture
//! as the WASM plugin host):
//! - **No I/O.** `rhai` has no filesystem / network / clock access unless
//!   the host registers it — and we register only pure math helpers.
//! - **Bounded runtime.** `max_operations` caps instructions per eval, so
//!   an infinite loop returns an `Err` instead of hanging the render.
//! - **Bounded sizes/depth.** String/array/map sizes + expression and
//!   call depth are capped to keep one expression from exhausting memory.
//! - **Silenced `print`/`debug`** so a script can't spam the log.

#![warn(missing_docs)]

use rhai::{Engine, Scope, AST};
use thiserror::Error;

/// An error compiling or evaluating an expression.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ExprError {
    /// The source failed to parse / compile.
    #[error("expression compile error: {0}")]
    Compile(String),
    /// Evaluation failed (runtime error, or a sandbox limit was hit).
    #[error("expression eval error: {0}")]
    Eval(String),
    /// The expression evaluated to a non-numeric value.
    #[error("expression did not evaluate to a number")]
    NotNumber,
}

/// Per-frame inputs available to an expression as named constants.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EvalContext {
    /// Playhead time in seconds.
    pub time: f64,
    /// Frame index.
    pub frame: f64,
    /// The parameter's value before the expression (its static/keyframed
    /// value), so an expression can offset rather than replace it.
    pub value: f64,
    /// Clip or sequence duration in seconds.
    pub duration: f64,
    /// Frames per second.
    pub fps: f64,
    /// Output width in pixels.
    pub width: f64,
    /// Output height in pixels.
    pub height: f64,
}

/// A compiled expression — compile once, evaluate per frame.
pub struct Expr {
    ast: AST,
}

/// A sandboxed engine for compiling + evaluating parameter expressions.
/// Construct once and reuse across compiles/evals.
pub struct ExprEngine {
    engine: Engine,
}

impl Default for ExprEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ExprEngine {
    /// A new sandboxed engine.
    pub fn new() -> Self {
        let mut engine = Engine::new();
        // --- Sandbox limits -------------------------------------------
        engine.set_max_operations(50_000);
        engine.set_max_call_levels(16);
        engine.set_max_expr_depths(64, 64);
        engine.set_max_string_size(8 * 1024);
        engine.set_max_array_size(1024);
        engine.set_max_map_size(1024);
        engine.on_print(|_| {});
        engine.on_debug(|_, _, _| {});

        // --- Numeric ergonomics: allow mixing int literals with the f64
        // context vars (rhai keeps INT and FLOAT distinct by default, so
        // `value * 2` would otherwise be a type error). ----------------
        register_mixed_numeric_ops(&mut engine);

        // --- Pure helper functions ------------------------------------
        engine.register_fn("clamp", |x: f64, lo: f64, hi: f64| x.max(lo).min(hi));
        engine.register_fn("lerp", |a: f64, b: f64, t: f64| a + (b - a) * t);
        engine.register_fn("sign", |x: f64| {
            if x > 0.0 {
                1.0
            } else if x < 0.0 {
                -1.0
            } else {
                0.0
            }
        });
        // Deterministic value noise in [-1, 1] — reproducible across
        // renders (no global RNG). `wiggle` idiom: `amp * noise(time*freq)`.
        engine.register_fn("noise", value_noise);

        Self { engine }
    }

    /// Compile `src` to a reusable [`Expr`].
    pub fn compile(&self, src: &str) -> Result<Expr, ExprError> {
        let ast = self
            .engine
            .compile(src)
            .map_err(|e| ExprError::Compile(e.to_string()))?;
        Ok(Expr { ast })
    }

    /// Evaluate a compiled expression against `ctx`, returning a number.
    pub fn eval(&self, expr: &Expr, ctx: &EvalContext) -> Result<f64, ExprError> {
        let mut scope = Scope::new();
        scope.push_constant("time", ctx.time);
        scope.push_constant("frame", ctx.frame);
        scope.push_constant("value", ctx.value);
        scope.push_constant("duration", ctx.duration);
        scope.push_constant("fps", ctx.fps);
        scope.push_constant("width", ctx.width);
        scope.push_constant("height", ctx.height);
        let result = self
            .engine
            .eval_ast_with_scope::<rhai::Dynamic>(&mut scope, &expr.ast)
            .map_err(|e| ExprError::Eval(e.to_string()))?;
        dynamic_to_f64(&result).ok_or(ExprError::NotNumber)
    }

    /// Compile + evaluate in one shot (convenience for one-off use/tests).
    pub fn eval_str(&self, src: &str, ctx: &EvalContext) -> Result<f64, ExprError> {
        let expr = self.compile(src)?;
        self.eval(&expr, ctx)
    }

    /// Compile-check `src` without evaluating — for UI validation as the
    /// user types. Returns the compile error message, or `None` if valid.
    pub fn validate(&self, src: &str) -> Option<String> {
        self.engine.compile(src).err().map(|e| e.to_string())
    }
}

/// Coerce a rhai value to `f64`, accepting float / int / bool.
fn dynamic_to_f64(d: &rhai::Dynamic) -> Option<f64> {
    if let Some(f) = d.clone().try_cast::<f64>() {
        Some(f)
    } else if let Some(i) = d.clone().try_cast::<i64>() {
        Some(i as f64)
    } else {
        d.clone()
            .try_cast::<bool>()
            .map(|b| if b { 1.0 } else { 0.0 })
    }
}

/// Register int×float / float×int arithmetic + comparison operators so an
/// expression can freely mix integer literals with the f64 context vars.
fn register_mixed_numeric_ops(engine: &mut Engine) {
    engine.register_fn("+", |a: i64, b: f64| a as f64 + b);
    engine.register_fn("+", |a: f64, b: i64| a + b as f64);
    engine.register_fn("-", |a: i64, b: f64| a as f64 - b);
    engine.register_fn("-", |a: f64, b: i64| a - b as f64);
    engine.register_fn("*", |a: i64, b: f64| a as f64 * b);
    engine.register_fn("*", |a: f64, b: i64| a * b as f64);
    engine.register_fn("/", |a: i64, b: f64| a as f64 / b);
    engine.register_fn("/", |a: f64, b: i64| a / b as f64);
    engine.register_fn("<", |a: i64, b: f64| (a as f64) < b);
    engine.register_fn("<", |a: f64, b: i64| a < b as f64);
    engine.register_fn(">", |a: i64, b: f64| (a as f64) > b);
    engine.register_fn(">", |a: f64, b: i64| a > b as f64);
    engine.register_fn("<=", |a: i64, b: f64| (a as f64) <= b);
    engine.register_fn("<=", |a: f64, b: i64| a <= b as f64);
    engine.register_fn(">=", |a: i64, b: f64| (a as f64) >= b);
    engine.register_fn(">=", |a: f64, b: i64| a >= b as f64);
}

/// Smooth deterministic value noise in `[-1, 1]`. Hash two integer
/// lattice points and smoothstep between them — no global state, so the
/// same `x` always yields the same value (reproducible renders).
fn value_noise(x: f64) -> f64 {
    let xi = x.floor();
    let xf = x - xi;
    let h0 = hash01(xi as i64);
    let h1 = hash01(xi as i64 + 1);
    let u = xf * xf * (3.0 - 2.0 * xf); // smoothstep
    (h0 + u * (h1 - h0)) * 2.0 - 1.0
}

fn hash01(n: i64) -> f64 {
    let mut h = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    (h >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> EvalContext {
        EvalContext {
            time: 2.0,
            frame: 48.0,
            value: 5.0,
            duration: 10.0,
            fps: 24.0,
            width: 1920.0,
            height: 1080.0,
        }
    }

    #[test]
    fn evaluates_constant() {
        let e = ExprEngine::new();
        assert_eq!(e.eval_str("1.0 + 2.0", &ctx()).unwrap(), 3.0);
    }

    #[test]
    fn reads_context_vars() {
        let e = ExprEngine::new();
        assert_eq!(e.eval_str("value", &ctx()).unwrap(), 5.0);
        assert_eq!(e.eval_str("time * fps", &ctx()).unwrap(), 48.0);
        assert_eq!(
            e.eval_str("width / height", &ctx()).unwrap(),
            1920.0 / 1080.0
        );
    }

    #[test]
    fn mixes_int_literals_with_float_vars() {
        let e = ExprEngine::new();
        // `value * 2` mixes f64 var with i64 literal — must not type-error.
        assert_eq!(e.eval_str("value * 2", &ctx()).unwrap(), 10.0);
        assert_eq!(e.eval_str("value + 1", &ctx()).unwrap(), 6.0);
        assert_eq!(e.eval_str("frame / 2", &ctx()).unwrap(), 24.0);
    }

    #[test]
    fn integer_result_coerces_to_f64() {
        let e = ExprEngine::new();
        assert_eq!(e.eval_str("2 + 3", &ctx()).unwrap(), 5.0);
        assert_eq!(e.eval_str("true", &ctx()).unwrap(), 1.0);
    }

    #[test]
    fn math_stdlib_available() {
        let e = ExprEngine::new();
        let v = e.eval_str("sin(time)", &ctx()).unwrap();
        assert!((v - 2.0_f64.sin()).abs() < 1e-9);
        assert_eq!(e.eval_str("abs(0.0 - 4.0)", &ctx()).unwrap(), 4.0);
    }

    #[test]
    fn helpers_clamp_lerp_sign_noise() {
        let e = ExprEngine::new();
        assert_eq!(e.eval_str("clamp(value, 0.0, 3.0)", &ctx()).unwrap(), 3.0);
        assert_eq!(e.eval_str("lerp(0.0, 10.0, 0.5)", &ctx()).unwrap(), 5.0);
        assert_eq!(e.eval_str("sign(0.0 - 2.0)", &ctx()).unwrap(), -1.0);
        // noise is deterministic + bounded.
        let n1 = e.eval_str("noise(time * 3.0)", &ctx()).unwrap();
        let n2 = e.eval_str("noise(time * 3.0)", &ctx()).unwrap();
        assert_eq!(n1, n2, "noise must be deterministic");
        assert!((-1.0..=1.0).contains(&n1));
    }

    #[test]
    fn conditional_expression() {
        let e = ExprEngine::new();
        // `time > 1` mixes f64 var with int literal in a comparison.
        assert_eq!(
            e.eval_str("if time > 1 { 10.0 } else { 0.0 }", &ctx())
                .unwrap(),
            10.0
        );
    }

    #[test]
    fn compile_error_is_reported() {
        let e = ExprEngine::new();
        assert!(e.eval_str("value * * 2", &ctx()).is_err());
        assert!(e.validate("value +").is_some());
        assert!(e.validate("value * 2.0").is_none());
    }

    #[test]
    fn infinite_loop_is_bounded_by_max_operations() {
        let e = ExprEngine::new();
        // A runaway loop must terminate as an Err (sandbox), not hang.
        let r = e.eval_str("let mut i = 0; while true { i += 1; } i", &ctx());
        assert!(r.is_err(), "runaway loop should hit the operation cap");
    }

    #[test]
    fn non_numeric_result_is_err() {
        let e = ExprEngine::new();
        assert!(matches!(
            e.eval_str("\"hello\"", &ctx()),
            Err(ExprError::NotNumber)
        ));
    }

    #[test]
    fn no_filesystem_access() {
        // No host I/O functions are registered, so any attempt to call
        // one is a compile/eval error — never an actual file open.
        let e = ExprEngine::new();
        assert!(e.eval_str("open_file(\"/etc/passwd\")", &ctx()).is_err());
    }
}
