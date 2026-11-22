//! All integration tests for `vance-app`, compiled as ONE test binary.
//!
//! Intentional: one binary instead of one per file. Every integration-test
//! file is otherwise its own executable that statically links this crate's
//! whole dependency tree, which multiplied link time and target-dir size.
//! Add new tests as a module here (`mod my_test;` + `it/my_test.rs`), not as
//! a new top-level `tests/*.rs` file. Run one module with:
//!   cargo test -p vance-app --test it <module>::

// Tests run ffmpeg/ffprobe directly to build fixtures and check output; the
// spawn lock (vance_ipc::process, enforced by ../../clippy.toml) is for app code.
#![allow(clippy::disallowed_methods)]

mod compositing_e2e;
mod edge_cases;
mod encrypted_effect;
mod export_audio;
mod export_video;
mod frame_select;
mod hdr_composite;
mod hdr_export;
mod lockstep;
mod perf_compositing;
mod prefetch;
mod project_roundtrip;
mod render_sequence;
mod robustness;
mod scrub_cache;
mod shader_effect;
mod soak;
mod synthetic_export;
mod wasm_plugin_export;
