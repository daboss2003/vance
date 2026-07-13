//! GOAT-12 — rasterize a Fusion node-graph composition to a YUV420P frame,
//! the same shape the title / mograph paths produce so the engine can hand
//! it straight to the renderer's `upload_frame` path.
//!
//! The compositor renders a straight-alpha RGBA canvas; we flatten it over
//! the comp's background and convert RGB → Rec.709 limited-range YUV
//! (reusing the title path's converter), cropping to even dimensions the
//! 4:2:0 chroma planes require.

use std::collections::HashMap;

use vance_decoder::{FrameData, PixelFormat};
use vance_fusion::{Comp, Compositor};
use vance_mograph::RgbaImage;

use crate::mograph::crop_even;
use crate::title::rgb_to_yuv420p_rec709;

/// Render `comp` at comp-relative `t_seconds` into a YUV420P frame.
/// `inputs` supplies host images (e.g. the clip's decoded frame) for any
/// `MediaIn` nodes — pass an empty map for a self-contained comp.
/// Dimensions are the comp canvas, rounded DOWN to even for the chroma
/// planes.
pub fn render_fusion_yuv420(
    compositor: &Compositor,
    comp: &Comp,
    t_seconds: f64,
    inputs: &HashMap<String, RgbaImage>,
) -> FrameData {
    let (w, h, rgb) = compositor.render_rgb(comp, t_seconds, inputs);
    let (ew, eh, rgb) = crop_even(&rgb, w, h);
    let yuv = rgb_to_yuv420p_rec709(&rgb, ew, eh);
    let cw = ew / 2;
    FrameData {
        pts: 0,
        width: ew,
        height: eh,
        pixel_format: PixelFormat::Yuv420P,
        strides: vec![ew, cw, cw],
        bytes: yuv,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_yuv_frame_of_even_canvas_size() {
        let compositor = Compositor::new();
        let comp = vance_fusion::starter(200, 120);
        let f = render_fusion_yuv420(&compositor, &comp, 0.5, &HashMap::new());
        assert_eq!(f.width % 2, 0);
        assert_eq!(f.height % 2, 0);
        assert_eq!(f.pixel_format, PixelFormat::Yuv420P);
        // Y plane is width*height bytes; total includes two half-res chroma.
        let y = (f.width * f.height) as usize;
        let c = ((f.width / 2) * (f.height / 2)) as usize;
        assert_eq!(f.bytes.len(), y + 2 * c);
    }

    #[test]
    fn odd_canvas_is_cropped_even() {
        let compositor = Compositor::new();
        let comp = vance_fusion::starter(101, 51);
        let f = render_fusion_yuv420(&compositor, &comp, 0.0, &HashMap::new());
        assert_eq!(f.width, 100);
        assert_eq!(f.height, 50);
    }
}
