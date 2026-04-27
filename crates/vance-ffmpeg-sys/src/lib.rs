//! `vance-ffmpeg-sys` — re-export of the `ffmpeg-the-third` bindings.
//!
//! Isolating FFmpeg behind this crate gives us one place to:
//!
//! - Swap the binding crate (e.g. to `ffmpeg-next` or `rusty_ffmpeg`) without
//!   touching consumer code.
//! - Add vendored / system / static feature flags without polluting downstream.
//! - Initialize FFmpeg once (`init_once`) before any decoder uses it.

pub use ffmpeg_the_third as ffmpeg;

use std::sync::Once;

static INIT: Once = Once::new();

/// Initialize FFmpeg's networking + format/codec registries. Idempotent and
/// thread-safe; call from any sidecar's `main` before doing FFmpeg work.
pub fn init_once() {
    INIT.call_once(|| {
        // ffmpeg-the-third 3.x auto-registers most things; this initializes
        // network protocols (HTTPS sources, etc.) and is cheap.
        let _ = ffmpeg::init();
    });
}
