//! `vance-app` — Tauri host process. Lib target exists so cargo-tauri's
//! mobile/desktop split is happy + so integration tests can drive the
//! Engine without going through the Tauri command surface.

#[path = "peaks.rs"]
pub mod peaks;
#[path = "engine.rs"]
pub mod engine;
