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
    /// M1: background opacity in `[0, 1]`. `1.0` is the pre-M1 opaque card
    /// (and the default for older bundles); titles the UI creates now
    /// default to `0.0` so they sit over the tracks below.
    #[serde(default = "one_f32")]
    pub bg_alpha: f32,
}

fn one_f32() -> f32 {
    1.0
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
            bg_alpha: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_m1_title_loads_opaque_from_json_and_ron() {
        let title = TitleParams {
            bg_alpha: 0.0,
            ..Default::default()
        };
        let mut json = serde_json::to_value(&title).unwrap();
        assert_eq!(
            serde_json::from_value::<TitleParams>(json.clone()).unwrap(),
            title
        );
        json.as_object_mut().unwrap().remove("bg_alpha");
        let old: TitleParams = serde_json::from_value(json).unwrap();
        assert_eq!(old.bg_alpha, 1.0);

        let ron = ron::to_string(&title).unwrap();
        let legacy = ron.replace(",bg_alpha:0.0", "");
        assert_ne!(legacy, ron, "{ron}");
        let old: TitleParams = ron::from_str(&legacy).unwrap();
        assert_eq!(old.bg_alpha, 1.0);
        assert_eq!(old.text, title.text);
    }
}
