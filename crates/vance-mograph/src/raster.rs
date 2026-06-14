//! CPU rasterization: an RGBA canvas, text layout + glyph rendering,
//! rounded-rect fill, and affine alpha-over layer compositing.
//!
//! Everything is straight-alpha RGBA8. Compositing is done in
//! premultiplied f32 internally to avoid edge fringing, then stored back
//! as straight alpha.

use crate::model::{Align, Rgba};

/// A straight-alpha RGBA8 image, row-major, 4 bytes/pixel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbaImage {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width × height × 4` bytes, R,G,B,A per pixel.
    pub pixels: Vec<u8>,
}

impl RgbaImage {
    /// A fully-transparent image.
    pub fn new(width: u32, height: u32) -> Self {
        RgbaImage {
            width,
            height,
            pixels: vec![0u8; (width as usize) * (height as usize) * 4],
        }
    }

    /// An image flood-filled with `color` (straight alpha).
    pub fn filled(width: u32, height: u32, color: Rgba) -> Self {
        let mut img = Self::new(width, height);
        let [r, g, b, a] = quantize(color);
        for px in img.pixels.chunks_exact_mut(4) {
            px[0] = r;
            px[1] = g;
            px[2] = b;
            px[3] = a;
        }
        img
    }

    #[inline]
    fn idx(&self, x: u32, y: u32) -> usize {
        ((y as usize) * (self.width as usize) + x as usize) * 4
    }

    /// Premultiplied f32 sample at integer coords; `[0;4]` out of bounds.
    #[inline]
    fn premul_at(&self, x: i64, y: i64) -> [f32; 4] {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height {
            return [0.0; 4];
        }
        let i = self.idx(x as u32, y as u32);
        let a = self.pixels[i + 3] as f32 / 255.0;
        [
            self.pixels[i] as f32 / 255.0 * a,
            self.pixels[i + 1] as f32 / 255.0 * a,
            self.pixels[i + 2] as f32 / 255.0 * a,
            a,
        ]
    }

    /// Bilinear premultiplied sample (clamps taps to transparent outside).
    fn premul_bilinear(&self, x: f32, y: f32) -> [f32; 4] {
        let x0 = x.floor() as i64;
        let y0 = y.floor() as i64;
        let fx = x - x0 as f32;
        let fy = y - y0 as f32;
        let p00 = self.premul_at(x0, y0);
        let p10 = self.premul_at(x0 + 1, y0);
        let p01 = self.premul_at(x0, y0 + 1);
        let p11 = self.premul_at(x0 + 1, y0 + 1);
        let mut out = [0.0f32; 4];
        for c in 0..4 {
            let top = p00[c] * (1.0 - fx) + p10[c] * fx;
            let bot = p01[c] * (1.0 - fx) + p11[c] * fx;
            out[c] = top * (1.0 - fy) + bot * fy;
        }
        out
    }

    /// Alpha-over a premultiplied source `[pr,pg,pb,a]` onto pixel (x,y).
    #[inline]
    fn over_premul(&mut self, x: u32, y: u32, src: [f32; 4]) {
        let sa = src[3];
        if sa <= 0.0 {
            return;
        }
        let i = self.idx(x, y);
        let ca = self.pixels[i + 3] as f32 / 255.0;
        let cpr = self.pixels[i] as f32 / 255.0 * ca;
        let cpg = self.pixels[i + 1] as f32 / 255.0 * ca;
        let cpb = self.pixels[i + 2] as f32 / 255.0 * ca;
        let inv = 1.0 - sa;
        let opr = src[0] + cpr * inv;
        let opg = src[1] + cpg * inv;
        let opb = src[2] + cpb * inv;
        let oa = sa + ca * inv;
        if oa <= 0.0 {
            self.pixels[i] = 0;
            self.pixels[i + 1] = 0;
            self.pixels[i + 2] = 0;
            self.pixels[i + 3] = 0;
            return;
        }
        self.pixels[i] = ((opr / oa).clamp(0.0, 1.0) * 255.0).round() as u8;
        self.pixels[i + 1] = ((opg / oa).clamp(0.0, 1.0) * 255.0).round() as u8;
        self.pixels[i + 2] = ((opb / oa).clamp(0.0, 1.0) * 255.0).round() as u8;
        self.pixels[i + 3] = (oa.clamp(0.0, 1.0) * 255.0).round() as u8;
    }

    /// Composite a straight-alpha color with given coverage onto (x,y).
    #[inline]
    fn paint(&mut self, x: u32, y: u32, color: Rgba, coverage: f32) {
        let a = (color[3] as f32 * coverage).clamp(0.0, 1.0);
        if a <= 0.0 {
            return;
        }
        self.over_premul(
            x,
            y,
            [
                color[0] as f32 * a,
                color[1] as f32 * a,
                color[2] as f32 * a,
                a,
            ],
        );
    }

    /// Flatten this image over an opaque `background` color, returning a
    /// packed RGB8 buffer (`width × height × 3`). Used to hand a fully
    /// opaque card to the YUV converter.
    pub fn flatten_rgb(&self, background: Rgba) -> Vec<u8> {
        let [br, bg, bb, _] = background;
        let mut out = vec![0u8; (self.width as usize) * (self.height as usize) * 3];
        for (i, px) in self.pixels.chunks_exact(4).enumerate() {
            let a = px[3] as f32 / 255.0;
            let r = (px[0] as f32 / 255.0) * a + br as f32 * (1.0 - a);
            let g = (px[1] as f32 / 255.0) * a + bg as f32 * (1.0 - a);
            let b = (px[2] as f32 / 255.0) * a + bb as f32 * (1.0 - a);
            out[i * 3] = (r.clamp(0.0, 1.0) * 255.0).round() as u8;
            out[i * 3 + 1] = (g.clamp(0.0, 1.0) * 255.0).round() as u8;
            out[i * 3 + 2] = (b.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        out
    }
}

#[inline]
fn quantize(c: Rgba) -> [u8; 4] {
    [
        (c[0].clamp(0.0, 1.0) * 255.0).round() as u8,
        (c[1].clamp(0.0, 1.0) * 255.0).round() as u8,
        (c[2].clamp(0.0, 1.0) * 255.0).round() as u8,
        (c[3].clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

// ---------------------------------------------------------------------
// Rounded rectangle
// ---------------------------------------------------------------------

/// Rasterize a rounded rect of size `w×h`, corner `radius`, filled with
/// `color`, into a fresh image (1px of analytic antialiasing at edges).
pub fn rounded_rect(w: u32, h: u32, radius: f32, color: Rgba) -> RgbaImage {
    let mut img = RgbaImage::new(w.max(1), h.max(1));
    let hw = w as f32 / 2.0;
    let hh = h as f32 / 2.0;
    let r = radius.clamp(0.0, hw.min(hh));
    for y in 0..img.height {
        for x in 0..img.width {
            let px = x as f32 + 0.5 - hw;
            let py = y as f32 + 0.5 - hh;
            // Signed distance to a rounded box centered at origin.
            let qx = px.abs() - (hw - r);
            let qy = py.abs() - (hh - r);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
            let inside = qx.max(qy).min(0.0);
            let d = outside + inside - r;
            let coverage = (0.5 - d).clamp(0.0, 1.0);
            if coverage > 0.0 {
                img.paint(x, y, color, coverage);
            }
        }
    }
    img
}

// ---------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------

/// A laid-out text block plus its pixel size.
pub struct TextBlock {
    /// Rasterized block (transparent background, glyphs in `color`).
    pub image: RgbaImage,
}

/// Lay out + rasterize `text` with `font` at `size_px`. Supports `\n`
/// breaks and greedy word-wrap when `max_width` is set; aligns each line
/// per `align`; applies `tracking` (letter-spacing) and pairwise kerning.
/// Returns an empty 1×1 image when there's nothing to draw.
#[allow(clippy::too_many_arguments)]
pub fn render_text(
    font: &fontdue::Font,
    text: &str,
    size_px: f32,
    color: Rgba,
    align: Align,
    line_height_mult: f64,
    tracking: f32,
    max_width: Option<f32>,
) -> TextBlock {
    let size = size_px.max(1.0);
    let line_metrics = font.horizontal_line_metrics(size);
    let ascent = line_metrics.map(|m| m.ascent).unwrap_or(size * 0.8);
    let descent = line_metrics.map(|m| m.descent).unwrap_or(-size * 0.2);
    let natural_line = ascent - descent;
    let line_h = (size as f64 * line_height_mult).max(natural_line as f64) as f32;

    // Break into display lines.
    let lines = wrap_lines(font, text, size, tracking, max_width);
    if lines.is_empty() || lines.iter().all(|l| l.is_empty()) {
        return TextBlock {
            image: RgbaImage::new(1, 1),
        };
    }

    // Measure.
    let widths: Vec<f32> = lines.iter().map(|l| line_width(font, l, size, tracking)).collect();
    let block_w = widths
        .iter()
        .cloned()
        .fold(1.0f32, f32::max)
        .ceil()
        .max(1.0);
    let block_h = (line_h * lines.len() as f32).ceil().max(1.0);
    let mut img = RgbaImage::new(block_w as u32, block_h as u32);

    for (li, line) in lines.iter().enumerate() {
        let lw = widths[li];
        let x_start = match align {
            Align::Left => 0.0,
            Align::Center => (block_w - lw) / 2.0,
            Align::Right => block_w - lw,
        };
        let baseline = li as f32 * line_h + ascent;
        draw_line(&mut img, font, line, size, color, tracking, x_start, baseline);
    }

    TextBlock { image: img }
}

/// Advance width of a single line including kerning + tracking.
fn line_width(font: &fontdue::Font, line: &str, size: f32, tracking: f32) -> f32 {
    let mut w = 0.0f32;
    let mut prev: Option<char> = None;
    for ch in line.chars() {
        if let Some(p) = prev {
            w += font.horizontal_kern(p, ch, size).unwrap_or(0.0);
        }
        w += font.metrics(ch, size).advance_width + tracking;
        prev = Some(ch);
    }
    w
}

fn draw_line(
    img: &mut RgbaImage,
    font: &fontdue::Font,
    line: &str,
    size: f32,
    color: Rgba,
    tracking: f32,
    x_start: f32,
    baseline: f32,
) {
    let mut x = x_start;
    let mut prev: Option<char> = None;
    for ch in line.chars() {
        if let Some(p) = prev {
            x += font.horizontal_kern(p, ch, size).unwrap_or(0.0);
        }
        let (metrics, bitmap) = font.rasterize(ch, size);
        let glyph_left = x + metrics.xmin as f32;
        let glyph_top = baseline - (metrics.height as f32 + metrics.ymin as f32);
        for gy in 0..metrics.height {
            for gx in 0..metrics.width {
                let cov = bitmap[gy * metrics.width + gx] as f32 / 255.0;
                if cov <= 0.0 {
                    continue;
                }
                let px = (glyph_left + gx as f32).floor();
                let py = (glyph_top + gy as f32).floor();
                if px < 0.0 || py < 0.0 || px as u32 >= img.width || py as u32 >= img.height {
                    continue;
                }
                img.paint(px as u32, py as u32, color, cov);
            }
        }
        x += metrics.advance_width + tracking;
        prev = Some(ch);
    }
}

/// Split into display lines: hard breaks on `\n`, then greedy word-wrap
/// to `max_width` (if any).
fn wrap_lines(
    font: &fontdue::Font,
    text: &str,
    size: f32,
    tracking: f32,
    max_width: Option<f32>,
) -> Vec<String> {
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        match max_width {
            None => out.push(paragraph.to_string()),
            Some(mw) => {
                let mut current = String::new();
                for word in paragraph.split_whitespace() {
                    let candidate = if current.is_empty() {
                        word.to_string()
                    } else {
                        format!("{current} {word}")
                    };
                    if line_width(font, &candidate, size, tracking) <= mw || current.is_empty() {
                        current = candidate;
                    } else {
                        out.push(std::mem::take(&mut current));
                        current = word.to_string();
                    }
                }
                out.push(current);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------
// Affine composite
// ---------------------------------------------------------------------

/// Resolved layer transform for one frame.
#[derive(Clone, Copy, Debug)]
pub struct LayerXform {
    /// Pivot as a NORMALIZED fraction of the layer's rasterized size
    /// (`[0.5, 0.5]` = center). Resolved to px against the actual layer
    /// dimensions inside [`composite`] so dynamic-size text still pivots
    /// correctly.
    pub anchor: [f64; 2],
    /// Canvas px the anchor lands on.
    pub position: [f64; 2],
    /// Per-axis scale.
    pub scale: [f64; 2],
    /// Clockwise rotation in degrees.
    pub rotation_deg: f64,
    /// Layer opacity `[0,1]`.
    pub opacity: f64,
}

/// Composite `layer` onto `canvas` under `xform` (inverse-mapped bilinear
/// sampling, premultiplied alpha-over, opacity applied). Skips work when
/// the layer is degenerate or fully transparent.
pub fn composite(canvas: &mut RgbaImage, layer: &RgbaImage, xform: LayerXform) {
    let op = xform.opacity.clamp(0.0, 1.0) as f32;
    if op <= 0.0 || layer.width == 0 || layer.height == 0 {
        return;
    }
    let sx = xform.scale[0];
    let sy = xform.scale[1];
    if sx.abs() < 1e-9 || sy.abs() < 1e-9 {
        return;
    }
    let theta = xform.rotation_deg.to_radians();
    let (c, s) = (theta.cos(), theta.sin());
    // Normalized anchor → layer-local px.
    let ax = xform.anchor[0] * layer.width as f64;
    let ay = xform.anchor[1] * layer.height as f64;

    // Forward map a layer-local point to canvas.
    let fwd = |lx: f64, ly: f64| -> [f64; 2] {
        let vx = (lx - ax) * sx;
        let vy = (ly - ay) * sy;
        let rx = vx * c - vy * s;
        let ry = vx * s + vy * c;
        [xform.position[0] + rx, xform.position[1] + ry]
    };
    // Inverse map a canvas point back to layer-local.
    let inv = |cx: f64, cy: f64| -> [f64; 2] {
        let dx = cx - xform.position[0];
        let dy = cy - xform.position[1];
        let ux = dx * c + dy * s;
        let uy = -dx * s + dy * c;
        [ux / sx + ax, uy / sy + ay]
    };

    // Dest bbox = transformed layer corners, clamped to canvas, padded 1px.
    let (w, h) = (layer.width as f64, layer.height as f64);
    let corners = [fwd(0.0, 0.0), fwd(w, 0.0), fwd(0.0, h), fwd(w, h)];
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for p in corners {
        min_x = min_x.min(p[0]);
        min_y = min_y.min(p[1]);
        max_x = max_x.max(p[0]);
        max_y = max_y.max(p[1]);
    }
    let x0 = (min_x.floor() as i64 - 1).max(0);
    let y0 = (min_y.floor() as i64 - 1).max(0);
    let x1 = (max_x.ceil() as i64 + 1).min(canvas.width as i64);
    let y1 = (max_y.ceil() as i64 + 1).min(canvas.height as i64);

    for cy in y0..y1 {
        for cx in x0..x1 {
            let l = inv(cx as f64 + 0.5, cy as f64 + 0.5);
            // Sample at pixel-center coords (-0.5 to align grid).
            let mut src = layer.premul_bilinear(l[0] as f32 - 0.5, l[1] as f32 - 0.5);
            if src[3] <= 0.0 {
                continue;
            }
            for v in &mut src {
                *v *= op;
            }
            canvas.over_premul(cx as u32, cy as u32, src);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha(img: &RgbaImage, x: u32, y: u32) -> u8 {
        img.pixels[((y * img.width + x) * 4 + 3) as usize]
    }

    #[test]
    fn rounded_rect_is_opaque_center_transparent_corner() {
        // A heavily-rounded square: center opaque, extreme corner cut away.
        let r = rounded_rect(40, 40, 20.0, [1.0, 0.0, 0.0, 1.0]);
        assert!(alpha(&r, 20, 20) > 250, "center filled");
        assert_eq!(alpha(&r, 0, 0), 0, "rounded corner is cut");
    }

    #[test]
    fn composite_opacity_scales_alpha() {
        let mut canvas = RgbaImage::new(20, 20);
        let layer = RgbaImage::filled(10, 10, [1.0, 1.0, 1.0, 1.0]);
        let x = LayerXform {
            anchor: [0.0, 0.0],
            position: [5.0, 5.0],
            scale: [1.0, 1.0],
            rotation_deg: 0.0,
            opacity: 0.5,
        };
        composite(&mut canvas, &layer, x);
        // Center of where the layer landed should be ~50% alpha.
        let a = alpha(&canvas, 9, 9);
        assert!((a as i32 - 128).abs() <= 4, "opacity 0.5 → ~128, got {a}");
    }

    #[test]
    fn composite_places_layer_at_position() {
        let mut canvas = RgbaImage::new(40, 40);
        let layer = RgbaImage::filled(8, 8, [1.0, 1.0, 1.0, 1.0]);
        // Anchor center, placed at (30, 30).
        let x = LayerXform {
            anchor: [0.5, 0.5],
            position: [30.0, 30.0],
            scale: [1.0, 1.0],
            rotation_deg: 0.0,
            opacity: 1.0,
        };
        composite(&mut canvas, &layer, x);
        assert!(alpha(&canvas, 30, 30) > 250, "opaque under the anchor");
        assert_eq!(alpha(&canvas, 5, 5), 0, "far from the layer is empty");
    }

    #[test]
    fn composite_rotation_90_keeps_centered_square() {
        // A centered square rotated 90° about its center still covers its
        // center pixel (sanity check the rotation math doesn't fling it).
        let mut canvas = RgbaImage::new(40, 40);
        let layer = RgbaImage::filled(10, 10, [1.0, 1.0, 1.0, 1.0]);
        let x = LayerXform {
            anchor: [0.5, 0.5],
            position: [20.0, 20.0],
            scale: [1.0, 1.0],
            rotation_deg: 90.0,
            opacity: 1.0,
        };
        composite(&mut canvas, &layer, x);
        assert!(alpha(&canvas, 20, 20) > 250, "rotated square still centered");
    }
}
