//! `vance-app` — Tauri host process. Lib target exists so cargo-tauri's
//! mobile/desktop split is happy + so integration tests can drive the
//! Engine without going through the Tauri command surface.

#[path = "addons.rs"]
pub mod addons;
#[path = "audio_bounce.rs"]
pub mod audio_bounce;
#[path = "cached_decode.rs"]
pub mod cached_decode;
#[path = "comp_playback.rs"]
pub mod comp_playback;
#[path = "compose.rs"]
pub mod compose;
#[path = "decoder_pool.rs"]
pub mod decoder_pool;
#[path = "engine.rs"]
pub mod engine;
#[path = "layer_feed.rs"]
pub mod layer_feed;
#[path = "peaks.rs"]
pub mod peaks;
#[path = "prefetch.rs"]
pub mod prefetch;
#[path = "profiling.rs"]
pub mod profiling;
#[path = "proxy.rs"]
pub mod proxy;
