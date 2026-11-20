//! Title rasterization — turn `(text, style)` into a YUV frame.
//!
//! Titles are *synthetic* clips on the timeline: they don't reference a
//! file. When the engine encounters one it calls into here to get a
//! decoded-frame-equivalent it can hand straight to the renderer's
//! `upload_frame` path. We use `fontdue` for glyph rasterization and
//! lay out a single line of text centered on a solid background.
//!
//! Phase 5.7 scope is intentionally minimal:
//! - Single-line text only — no word-wrap yet.
//! - One font face per call (no font picker, fallback chain).
//! - Solid background color; future work can add transparent / image bg.
//! - Latin glyphs assumed available in the host system font.
//!
//! Output is `Yuv420P` because that's the format the renderer's YUV
//! shader path expects; saves the secondary upload from having to
//! convert. RGB→Rec.709 conversion happens CPU-side.

use std::path::PathBuf;
use std::sync::OnceLock;

use vance_decoder::{FrameData, PixelFormat};
pub use vance_timeline::TitleParams;

/// Error from title rasterization.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TitleError {
    /// No system font found on disk.
    #[error("no system font found (tried: {0:?})")]
    NoFont(Vec<PathBuf>),

    /// Couldn't read the font bytes.
    #[error("failed to read font: {0}")]
    FontRead(#[from] std::io::Error),

    /// `fontdue` rejected the font bytes.
    #[error("failed to parse font: {0}")]
    FontParse(String),
}

/// Render the given title params into a YUV420P frame.
///
/// The output dimensions match `params.width × params.height`,
/// rounded up to even values for the chroma planes' sake. Glyph
/// coverage is converted to YUV via Rec.709 limited-range coefficients
/// so the title sits in the same color space as decoded video.
pub fn render_title_yuv420(params: &TitleParams) -> Result<FrameData, TitleError> {
    let w = params.width.max(2) & !1; // even
    let h = params.height.max(2) & !1;

    // 1. RGB fill on a CPU buffer (linear-ish sRGB; we never linearize
    //    explicitly because the rest of the pipeline operates in
    //    gamma-encoded space too).
    let mut rgb = vec![0u8; (w * h * 3) as usize];
    let bg_r = (params.bg[0].clamp(0.0, 1.0) * 255.0) as u8;
    let bg_g = (params.bg[1].clamp(0.0, 1.0) * 255.0) as u8;
    let bg_b = (params.bg[2].clamp(0.0, 1.0) * 255.0) as u8;
    for chunk in rgb.chunks_exact_mut(3) {
        chunk[0] = bg_r;
        chunk[1] = bg_g;
        chunk[2] = bg_b;
    }

    // 2. Lay out the glyphs of `params.text` on a single line and
    //    composite into the RGB buffer via per-glyph alpha.
    let font = load_default_font()?;
    let size = params.size_px.max(8.0);
    // Compute advance widths to find total line width.
    let mut total_w = 0.0_f32;
    let mut max_ascent = 0.0_f32;
    let mut max_descent = 0.0_f32;
    for ch in params.text.chars() {
        let metrics = font.metrics(ch, size);
        total_w += metrics.advance_width;
        max_ascent = max_ascent.max(metrics.ymin as f32 + metrics.height as f32);
        max_descent = max_descent.max(-metrics.ymin as f32);
    }
    // Center horizontally and vertically. `pen_x` is the baseline-x
    // of the first glyph; baseline-y is `y_baseline` (pixel down).
    let pen_x = ((w as f32 - total_w) * 0.5).max(0.0);
    let line_h = max_ascent + max_descent;
    let y_baseline = ((h as f32 + line_h) * 0.5) - max_descent;
    let fg_r = (params.fg[0].clamp(0.0, 1.0) * 255.0) as u8;
    let fg_g = (params.fg[1].clamp(0.0, 1.0) * 255.0) as u8;
    let fg_b = (params.fg[2].clamp(0.0, 1.0) * 255.0) as u8;

    let mut x_cursor = pen_x;
    for ch in params.text.chars() {
        let (metrics, bitmap) = font.rasterize(ch, size);
        // fontdue glyph origin is at the top-left of the bitmap; the
        // glyph's bearing places that origin relative to (pen_x, baseline).
        let glyph_left = x_cursor + metrics.xmin as f32;
        let glyph_top = y_baseline - (metrics.height as f32 + metrics.ymin as f32);
        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let alpha = bitmap[gy * metrics.width + gx] as f32 / 255.0;
                if alpha <= 0.001 {
                    continue;
                }
                let px = (glyph_left + gx as f32).round() as i32;
                let py = (glyph_top + gy as f32).round() as i32;
                if px < 0 || py < 0 || (px as u32) >= w || (py as u32) >= h {
                    continue;
                }
                let off = ((py as u32 * w + px as u32) * 3) as usize;
                // Alpha-blend the foreground over the existing pixel.
                rgb[off] = blend(rgb[off], fg_r, alpha);
                rgb[off + 1] = blend(rgb[off + 1], fg_g, alpha);
                rgb[off + 2] = blend(rgb[off + 2], fg_b, alpha);
            }
        }
        x_cursor += metrics.advance_width;
    }

    // 3. Convert RGB → Rec.709 limited-range YUV420P.
    let yuv = rgb_to_yuv420p_rec709(&rgb, w, h);
    let cw = w / 2;
    Ok(FrameData {
        pts: 0,
        width: w,
        height: h,
        pixel_format: PixelFormat::Yuv420P,
        // YUV420P stride per plane equals each plane's row width.
        strides: vec![w, cw, cw],
        bytes: yuv.into(),
    })
}

#[inline]
fn blend(under: u8, over: u8, alpha: f32) -> u8 {
    let u = under as f32;
    let o = over as f32;
    (u + (o - u) * alpha).clamp(0.0, 255.0) as u8
}

pub(crate) fn rgb_to_yuv420p_rec709(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
    // Y plane: full size; U, V: w/2 × h/2.
    let y_size = (w * h) as usize;
    let cw = w / 2;
    let ch = h / 2;
    let chroma_size = (cw * ch) as usize;
    let mut out = vec![0u8; y_size + 2 * chroma_size];
    let (y_plane, rest) = out.split_at_mut(y_size);
    let (u_plane, v_plane) = rest.split_at_mut(chroma_size);

    // Rec.709 limited-range conversion.
    // Y = 0.2126 R + 0.7152 G + 0.0722 B (full range) → scale to [16, 235].
    // U / V from B-Y / R-Y, scaled to [16, 240].
    for y in 0..h {
        for x in 0..w {
            let off = ((y * w + x) * 3) as usize;
            let r = rgb[off] as f32;
            let g = rgb[off + 1] as f32;
            let b = rgb[off + 2] as f32;
            let y_full = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let y_lim = (y_full * (219.0 / 255.0) + 16.0).clamp(0.0, 255.0) as u8;
            y_plane[(y * w + x) as usize] = y_lim;
        }
    }
    // 2×2 box-averaged chroma.
    for cy in 0..ch {
        for cx in 0..cw {
            let mut sum_r = 0.0_f32;
            let mut sum_g = 0.0_f32;
            let mut sum_b = 0.0_f32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let px = cx * 2 + dx;
                    let py = cy * 2 + dy;
                    let off = ((py * w + px) * 3) as usize;
                    sum_r += rgb[off] as f32;
                    sum_g += rgb[off + 1] as f32;
                    sum_b += rgb[off + 2] as f32;
                }
            }
            let r = sum_r * 0.25;
            let g = sum_g * 0.25;
            let b = sum_b * 0.25;
            let y_full = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            // Cb = (B - Y) * (224 / 2 / (255 * (1 - 0.0722))) + 128
            // Simplified per Rec.709 limited-range:
            let cb =
                ((b - y_full) * (224.0 / 2.0) / (255.0 - 16.0) + 128.0).clamp(0.0, 255.0) as u8;
            let cr =
                ((r - y_full) * (224.0 / 2.0) / (255.0 - 16.0) + 128.0).clamp(0.0, 255.0) as u8;
            u_plane[(cy * cw + cx) as usize] = cb;
            v_plane[(cy * cw + cx) as usize] = cr;
        }
    }

    out
}

/// Load the bundled / system fallback font (lazy, once per process).
/// We try a list of well-known paths per OS; if none are present the
/// caller gets a clear error.
fn load_default_font() -> Result<&'static fontdue::Font, TitleError> {
    static CACHED: OnceLock<Result<fontdue::Font, TitleError>> = OnceLock::new();
    let r = CACHED.get_or_init(|| {
        let candidates = system_font_candidates();
        let mut tried = Vec::new();
        for p in candidates.iter() {
            if p.exists() {
                match std::fs::read(p) {
                    Ok(bytes) => {
                        match fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                            Ok(f) => return Ok(f),
                            Err(e) => {
                                tracing::warn!(?p, ?e, "title: font parse failed");
                            }
                        }
                    }
                    Err(e) => tracing::warn!(?p, ?e, "title: font read failed"),
                }
            }
            tried.push(p.clone());
        }
        Err(TitleError::NoFont(tried))
    });
    r.as_ref().map_err(|e| match e {
        TitleError::NoFont(p) => TitleError::NoFont(p.clone()),
        // FontRead/FontParse not cacheable as errors; we mapped them to
        // NoFont with the tried list. This arm shouldn't fire.
        other => TitleError::FontParse(format!("{other}")),
    })
}

fn system_font_candidates() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/System/Library/Fonts/Supplemental/Arial.ttf"),
            PathBuf::from("/System/Library/Fonts/Helvetica.ttf"),
            PathBuf::from("/Library/Fonts/Arial.ttf"),
        ]
    } else if cfg!(target_os = "windows") {
        vec![
            PathBuf::from(r"C:\Windows\Fonts\arial.ttf"),
            PathBuf::from(r"C:\Windows\Fonts\calibri.ttf"),
        ]
    } else {
        vec![
            PathBuf::from("/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"),
            PathBuf::from("/usr/share/fonts/TTF/DejaVuSans.ttf"),
            PathBuf::from("/usr/share/fonts/dejavu/DejaVuSans.ttf"),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_frame_of_requested_size() {
        // Only runs on systems that have one of the candidate fonts;
        // we skip otherwise rather than fail.
        if !system_font_candidates().iter().any(|p| p.exists()) {
            return;
        }
        let f = render_title_yuv420(&TitleParams {
            text: "Hello".into(),
            ..Default::default()
        })
        .expect("title render");
        assert_eq!(f.width, 1280);
        assert_eq!(f.height, 720);
        assert_eq!(f.pixel_format, PixelFormat::Yuv420P);
        assert_eq!(f.bytes.len(), (1280 * 720 + 2 * 640 * 360) as usize);
    }

    #[test]
    fn black_background_yields_y16() {
        if !system_font_candidates().iter().any(|p| p.exists()) {
            return;
        }
        let f = render_title_yuv420(&TitleParams {
            text: "".into(), // no glyphs, so background only
            bg: [0.0, 0.0, 0.0],
            ..Default::default()
        })
        .expect("title render");
        // Rec.709 limited-range maps full-black RGB → Y=16.
        let y0 = f.bytes[0];
        assert_eq!(y0, 16, "black should map to limited-range Y=16");
    }
}
