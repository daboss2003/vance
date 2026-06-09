//! `vance-app` — Tauri host process. Lib target exists so cargo-tauri's
//! mobile/desktop split is happy + so integration tests can drive the
//! Engine without going through the Tauri command surface.

#[path = "peaks.rs"]
pub mod peaks;
#[path = "cached_decode.rs"]
pub mod cached_decode;
#[path = "prefetch.rs"]
pub mod prefetch;
#[path = "proxy.rs"]
pub mod proxy;
#[path = "profiling.rs"]
pub mod profiling;
#[path = "engine.rs"]
pub mod engine;
