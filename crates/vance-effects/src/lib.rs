//! `vance-effects` — GPU shader effects (M6, spec §10).
//!
//! A creator writes ONE WGSL function `effect(color, uv) -> vec4<f32>` and
//! declares parameters; [`ShaderEffect`] wraps it in a fixed fullscreen pass and
//! runs it over an RGBA frame — matching the same `process(rgba, w, h, params)`
//! contract the CPU WASM effects use, so a shader effect slots straight into the
//! existing effect chain. This is the engine of the no-code "Effect Studio".

mod shader;
pub use shader::{ShaderEffect, MAX_PARAMS};
