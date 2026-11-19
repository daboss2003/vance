//! `vance-app` — Tauri host process. Lib target exists so cargo-tauri's
//! mobile/desktop split is happy + so integration tests can drive the
//! Engine without going through the Tauri command surface.

#[path = "audio_bounce.rs"]
pub mod audio_bounce;
#[path = "cached_decode.rs"]
pub mod cached_decode;
#[path = "engine.rs"]
pub mod engine;
#[path = "peaks.rs"]
pub mod peaks;
#[path = "prefetch.rs"]
pub mod prefetch;
#[path = "profiling.rs"]
pub mod profiling;
#[path = "proxy.rs"]
pub mod proxy;
