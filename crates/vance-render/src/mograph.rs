//! GOAT-5 — rasterize a Motion Graphics Template instance to a YUV420P
//! frame, the same shape the title path produces so the engine can hand
//! it straight to the renderer's `upload_frame` path.
//!
//! The mograph engine renders a straight-alpha RGBA canvas; we flatten it
//! over the template's background and convert RGB → Rec.709 limited-range
//! YUV (reusing the title path's converter), cropping to even dimensions
//! the 4:2:0 chroma planes require.

use vance_decoder::{FrameData, PixelFormat};
use vance_mograph::{MographEngine, MographInstance};

use crate::title::rgb_to_yuv420p_rec709;

/// Render `inst` at clip-relative `t_seconds` into a YUV420P frame.
/// `fps`/`duration_s` feed the template's expression context so animated
/// rigs evaluate correctly. Dimensions are the template canvas, rounded
/// DOWN to even for the chroma planes.
pub fn render_mograph_yuv420(
    engine: &MographEngine,
    inst: &MographInstance,
    t_seconds: f64,
    fps: f64,
    duration_s: f64,
) -> FrameData {
    let (w, h, rgb) = engine.render_rgb(inst, t_seconds, fps, duration_s);
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

/// Crop (or minimally pad) a packed RGB8 buffer to even width/height
/// (no-op when already even). 4:2:0 needs even planes ≥ 2. Only copies
/// the bytes the source actually has — a 1-px-wide/tall canvas (which
/// `MographEngine::render` permits via its `.max(1)` clamp) is padded
/// with a zero column/row rather than reading out of bounds.
fn crop_even(rgb: &[u8], w: u32, h: u32) -> (u32, u32, Vec<u8>) {
    let ew = (w & !1).max(2);
    let eh = (h & !1).max(2);
    if ew == w && eh == h {
        return (w, h, rgb.to_vec());
    }
    let mut out = vec![0u8; (ew as usize) * (eh as usize) * 3];
    // Never copy more columns/rows than the source supplies.
    let copy_cols = ew.min(w) as usize * 3;
    let copy_rows = eh.min(h);
    for y in 0..copy_rows {
        let src = (y as usize * w as usize) * 3;
        let dst = (y as usize * ew as usize) * 3;
        out[dst..dst + copy_cols].copy_from_slice(&rgb[src..src + copy_cols]);
    }
    (ew, eh, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vance_mograph::{builtins, MographInstance};

    #[test]
    fn renders_yuv_frame_of_even_canvas_size() {
        let engine = MographEngine::new();
        let inst = MographInstance::new(builtins::centered_title());
        let f = render_mograph_yuv420(&engine, &inst, 1.0, 30.0, 4.0);
        assert_eq!(f.width, 1920);
        assert_eq!(f.height, 1080);
        assert_eq!(f.pixel_format, PixelFormat::Yuv420P);
        assert_eq!(
            f.bytes.len(),
            (1920 * 1080 + 2 * 960 * 540) as usize,
            "YUV420P plane sizes"
        );
    }

    #[test]
    fn crop_even_never_reads_past_source() {
        // A 1-px-wide / 1-px-tall / odd source must pad, not panic.
        let (w, h, out) = crop_even(&[9, 8, 7], 1, 1);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out.len(), 2 * 2 * 3);
        let (w, h, _) = crop_even(&vec![1u8; 3 * 100 * 3], 3, 100);
        assert_eq!((w, h), (2, 100));
        let (w, h, _) = crop_even(&vec![1u8; 5 * 1 * 3], 5, 1);
        assert_eq!((w, h), (4, 2));
    }

    #[test]
    fn degenerate_canvas_renders_without_panic() {
        let engine = MographEngine::new();
        let mut tpl = builtins::centered_title();
        tpl.canvas = vance_mograph::Canvas {
            width: 1,
            height: 1,
            background: [0.0, 0.0, 0.0, 1.0],
        };
        let inst = MographInstance::new(tpl);
        // Must not panic; produces a minimal even YUV frame.
        let f = render_mograph_yuv420(&engine, &inst, 0.0, 30.0, 1.0);
        assert!(f.width >= 2 && f.height >= 2);
        assert_eq!(f.bytes.len(), (f.width * f.height + 2 * (f.width / 2) * (f.height / 2)) as usize);
    }
}
