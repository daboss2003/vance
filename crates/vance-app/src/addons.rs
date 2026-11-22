//! CPU add-on host for per-layer effects (ADR 0007).
//!
//! WASM (`wasm:`) and shader (`shader:`) effects run on a layer read back
//! as straight sRGB RGBA8, then the result is written back. The plugin
//! state lives here, behind `Arc<Mutex<AddonHost>>`, so the playback thread
//! can run add-ons too — not on `Engine`, which it can't reach.
//!
//! Lock ordering: never hold the render state while plugins run.

/// Discovered plugins, warm instances and compiled shader effects.
// Intentional: an empty placeholder until the compose seam (C) copies the
// plugin registry / instance caches out of the engine.
#[derive(Default)]
pub struct AddonHost {}
