//! Title clip data.
//!
//! Phase 5.7 introduces *synthetic* media — clips that don't reference
//! a file on disk. A title media item carries its text + style here;
//! the engine renders it via `vance-render::title` at draw time. The
//! type lives in the timeline crate (not the render crate) so it
//! round-trips through project save/load without dragging wgpu into
//! pure-data code.

use serde::{Deserialize, Serialize};

/// Parameters describing a single title clip.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TitleParams {
    /// Text to display (single line in v1).
    pub text: String,
    /// Font size in pixels.
    pub size_px: f32,
    /// Foreground (text) color, RGB in `[0, 1]` per channel.
    pub fg: [f32; 3],
    /// Background color, RGB in `[0, 1]` per channel.
    pub bg: [f32; 3],
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
}

impl Default for TitleParams {
    fn default() -> Self {
        Self {
            text: "Title".into(),
            size_px: 96.0,
            fg: [1.0, 1.0, 1.0],
            bg: [0.0, 0.0, 0.0],
            width: 1280,
            height: 720,
        }
    }
}
