//! Per-pixel image operations for the compositor's filter/combiner nodes.
//!
//! All buffers are straight-alpha RGBA8 [`RgbaImage`]s of the SAME size
//! (the comp canvas), so combiners never have to resample. Blends and blurs
//! run in premultiplied f32 to avoid edge fringing, then store back straight.

use vance_mograph::raster::{composite, LayerXform};
use vance_mograph::{Rgba, RgbaImage};

use crate::model::{BlendMode, Fit, MaskChannel};

#[inline]
fn to_unit(b: u8) -> f32 {
    b as f32 / 255.0
}

#[inline]
fn to_u8(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[inline]
fn luma(r: f32, g: f32, b: f32) -> f32 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// Fill `img` with a solid color or a vertical `top → bottom` gradient.
pub fn fill_background(img: &mut RgbaImage, top: Rgba, bottom: Option<Rgba>) {
    let w = img.width as usize;
    let h = img.height as usize;
    for y in 0..h {
        let color = match bottom {
            None => top,
            Some(bot) => {
                let f = if h > 1 {
                    y as f64 / (h - 1) as f64
                } else {
                    0.0
                };
                [
                    top[0] + (bot[0] - top[0]) * f,
                    top[1] + (bot[1] - top[1]) * f,
                    top[2] + (bot[2] - top[2]) * f,
                    top[3] + (bot[3] - top[3]) * f,
                ]
            }
        };
        let px = [
            to_u8(color[0] as f32),
            to_u8(color[1] as f32),
            to_u8(color[2] as f32),
            to_u8(color[3] as f32),
        ];
        let row = y * w * 4;
        for x in 0..w {
            let i = row + x * 4;
            img.pixels[i] = px[0];
            img.pixels[i + 1] = px[1];
            img.pixels[i + 2] = px[2];
            img.pixels[i + 3] = px[3];
        }
    }
}

/// Place `src` onto `canvas` per a fit mode (centered).
pub fn place_media(canvas: &mut RgbaImage, src: &RgbaImage, fit: Fit) {
    if src.width == 0 || src.height == 0 {
        return;
    }
    let (cw, ch) = (canvas.width as f64, canvas.height as f64);
    let (sw, sh) = (src.width as f64, src.height as f64);
    let (scale_x, scale_y) = match fit {
        Fit::None => (1.0, 1.0),
        Fit::Stretch => (cw / sw, ch / sh),
        Fit::Contain => {
            let s = (cw / sw).min(ch / sh);
            (s, s)
        }
        Fit::Cover => {
            let s = (cw / sw).max(ch / sh);
            (s, s)
        }
    };
    let xf = LayerXform {
        anchor: [0.5, 0.5],
        position: [cw / 2.0, ch / 2.0],
        scale: [scale_x, scale_y],
        rotation_deg: 0.0,
        opacity: 1.0,
    };
    composite(canvas, src, xf);
}

/// Blend `fg` over `bg` (same size) with a mode and foreground opacity,
/// returning a fresh canvas. Works entirely in premultiplied alpha.
pub fn merge(bg: &RgbaImage, fg: &RgbaImage, mode: BlendMode, opacity: f64) -> RgbaImage {
    let mut out = RgbaImage::new(bg.width, bg.height);
    let n = out.pixels.len().min(bg.pixels.len()).min(fg.pixels.len());
    let op = opacity.clamp(0.0, 1.0) as f32;
    let mut i = 0;
    while i + 3 < n {
        let (br, bgc, bb, ba) = (
            to_unit(bg.pixels[i]),
            to_unit(bg.pixels[i + 1]),
            to_unit(bg.pixels[i + 2]),
            to_unit(bg.pixels[i + 3]),
        );
        let (fr, fgc, fb, mut fa) = (
            to_unit(fg.pixels[i]),
            to_unit(fg.pixels[i + 1]),
            to_unit(fg.pixels[i + 2]),
            to_unit(fg.pixels[i + 3]),
        );
        fa *= op;
        // Premultiplied source/backdrop.
        let (spr, spg, spb) = (fr * fa, fgc * fa, fb * fa);
        let (bpr, bpg, bpb) = (br * ba, bgc * ba, bb * ba);
        let (opr, opg, opb, oa) = match mode {
            BlendMode::Add => (
                (spr + bpr).min(1.0),
                (spg + bpg).min(1.0),
                (spb + bpb).min(1.0),
                (fa + ba).min(1.0),
            ),
            BlendMode::Over => (
                spr + bpr * (1.0 - fa),
                spg + bpg * (1.0 - fa),
                spb + bpb * (1.0 - fa),
                fa + ba * (1.0 - fa),
            ),
            // W3C separable blend: co = αs(1-αb)Cs + αs·αb·B(Cb,Cs) + αb(1-αs)Cb.
            BlendMode::Multiply | BlendMode::Screen => {
                let oa = fa + ba * (1.0 - fa);
                let blend = |cb: f32, cs: f32| match mode {
                    BlendMode::Multiply => cb * cs,
                    BlendMode::Screen => cb + cs - cb * cs,
                    _ => cs,
                };
                let mix = |cb: f32, cs: f32| {
                    fa * (1.0 - ba) * cs + fa * ba * blend(cb, cs) + ba * (1.0 - fa) * cb
                };
                (mix(br, fr), mix(bgc, fgc), mix(bb, fb), oa)
            }
        };
        if oa <= 0.0 {
            out.pixels[i] = 0;
            out.pixels[i + 1] = 0;
            out.pixels[i + 2] = 0;
            out.pixels[i + 3] = 0;
        } else {
            out.pixels[i] = to_u8(opr / oa);
            out.pixels[i + 1] = to_u8(opg / oa);
            out.pixels[i + 2] = to_u8(opb / oa);
            out.pixels[i + 3] = to_u8(oa);
        }
        i += 4;
    }
    out
}

/// Lift / gamma / gain + saturation of `input`, returning a fresh canvas.
pub fn color_correct(
    input: &RgbaImage,
    lift: [f64; 3],
    gain: [f64; 3],
    gamma: f64,
    saturation: f64,
) -> RgbaImage {
    let mut out = input.clone();
    let inv_gamma = if gamma > 1e-6 {
        1.0 / gamma as f32
    } else {
        1.0
    };
    let sat = saturation as f32;
    let lift = [lift[0] as f32, lift[1] as f32, lift[2] as f32];
    let gain = [gain[0] as f32, gain[1] as f32, gain[2] as f32];
    let mut i = 0;
    while i + 3 < out.pixels.len() {
        let mut c = [
            to_unit(out.pixels[i]),
            to_unit(out.pixels[i + 1]),
            to_unit(out.pixels[i + 2]),
        ];
        for k in 0..3 {
            c[k] = (c[k] * gain[k] + lift[k]).clamp(0.0, 1.0);
            if inv_gamma != 1.0 {
                c[k] = c[k].powf(inv_gamma);
            }
        }
        if (sat - 1.0).abs() > 1e-6 {
            let l = luma(c[0], c[1], c[2]);
            for k in 0..3 {
                c[k] = (l + (c[k] - l) * sat).clamp(0.0, 1.0);
            }
        }
        out.pixels[i] = to_u8(c[0]);
        out.pixels[i + 1] = to_u8(c[1]);
        out.pixels[i + 2] = to_u8(c[2]);
        i += 4;
    }
    out
}

/// Clear `input` outside a rectangle defined by per-edge px insets.
pub fn crop(input: &RgbaImage, left: f64, top: f64, right: f64, bottom: f64) -> RgbaImage {
    let mut out = input.clone();
    let w = out.width;
    let h = out.height;
    let x0 = left.max(0.0).round() as u32;
    let y0 = top.max(0.0).round() as u32;
    let x1 = w.saturating_sub(right.max(0.0).round() as u32);
    let y1 = h.saturating_sub(bottom.max(0.0).round() as u32);
    for y in 0..h {
        for x in 0..w {
            let keep = x >= x0 && x < x1 && y >= y0 && y < y1;
            if !keep {
                let i = ((y * w + x) * 4) as usize;
                out.pixels[i] = 0;
                out.pixels[i + 1] = 0;
                out.pixels[i + 2] = 0;
                out.pixels[i + 3] = 0;
            }
        }
    }
    out
}

/// Multiply `input`'s alpha by a mask (its alpha or luminance).
pub fn apply_mask(
    input: &RgbaImage,
    mask: &RgbaImage,
    channel: MaskChannel,
    invert: bool,
) -> RgbaImage {
    let mut out = input.clone();
    let n = out.pixels.len().min(mask.pixels.len());
    let mut i = 0;
    while i + 3 < n {
        let ma = to_unit(mask.pixels[i + 3]);
        let mut m = match channel {
            MaskChannel::Alpha => ma,
            // Premultiplied luminance so a transparent mask reads as 0.
            MaskChannel::Luminance => {
                luma(
                    to_unit(mask.pixels[i]),
                    to_unit(mask.pixels[i + 1]),
                    to_unit(mask.pixels[i + 2]),
                ) * ma
            }
        };
        if invert {
            m = 1.0 - m;
        }
        let a = to_unit(out.pixels[i + 3]) * m.clamp(0.0, 1.0);
        out.pixels[i + 3] = to_u8(a);
        i += 4;
    }
    out
}

/// Separable box/gaussian blur of `input` by `radius` px (fresh canvas).
pub fn blur(input: &RgbaImage, radius: f64, passes: u32) -> RgbaImage {
    let w = input.width as usize;
    let h = input.height as usize;
    let max_r = (w.min(h) / 2).max(0);
    let r = (radius.round() as usize).min(max_r);
    if r == 0 || passes == 0 {
        return input.clone();
    }
    // Work in premultiplied f32 so the blur doesn't drag opaque color into
    // transparent regions (halo-free).
    let mut buf = vec![0f32; w * h * 4];
    for p in 0..w * h {
        let a = to_unit(input.pixels[p * 4 + 3]);
        buf[p * 4] = to_unit(input.pixels[p * 4]) * a;
        buf[p * 4 + 1] = to_unit(input.pixels[p * 4 + 1]) * a;
        buf[p * 4 + 2] = to_unit(input.pixels[p * 4 + 2]) * a;
        buf[p * 4 + 3] = a;
    }
    let mut tmp = vec![0f32; w * h * 4];
    for _ in 0..passes {
        box_blur_horizontal(&buf, &mut tmp, w, h, r);
        box_blur_vertical(&tmp, &mut buf, w, h, r);
    }
    // Un-premultiply back to straight alpha.
    let mut out = RgbaImage::new(input.width, input.height);
    for p in 0..w * h {
        let a = buf[p * 4 + 3];
        if a <= 0.0 {
            continue;
        }
        out.pixels[p * 4] = to_u8(buf[p * 4] / a);
        out.pixels[p * 4 + 1] = to_u8(buf[p * 4 + 1] / a);
        out.pixels[p * 4 + 2] = to_u8(buf[p * 4 + 2] / a);
        out.pixels[p * 4 + 3] = to_u8(a);
    }
    out
}

/// One horizontal box pass (window `2r+1`, edge-clamped, O(w) per row).
fn box_blur_horizontal(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    if w == 0 {
        return;
    }
    let win = (2 * r + 1) as f32;
    for y in 0..h {
        let row = y * w * 4;
        for c in 0..4 {
            let mut sum = 0.0f32;
            for k in 0..=(2 * r) {
                let x = k.saturating_sub(r).min(w - 1);
                sum += src[row + x * 4 + c];
            }
            for x in 0..w {
                dst[row + x * 4 + c] = sum / win;
                let x_out = x.saturating_sub(r).min(w - 1);
                let x_in = (x + r + 1).min(w - 1);
                sum += src[row + x_in * 4 + c] - src[row + x_out * 4 + c];
            }
        }
    }
}

/// One vertical box pass (window `2r+1`, edge-clamped, O(h) per column).
fn box_blur_vertical(src: &[f32], dst: &mut [f32], w: usize, h: usize, r: usize) {
    if h == 0 {
        return;
    }
    let win = (2 * r + 1) as f32;
    let stride = w * 4;
    for x in 0..w {
        let col = x * 4;
        for c in 0..4 {
            let mut sum = 0.0f32;
            for k in 0..=(2 * r) {
                let y = k.saturating_sub(r).min(h - 1);
                sum += src[col + y * stride + c];
            }
            for y in 0..h {
                dst[col + y * stride + c] = sum / win;
                let y_out = y.saturating_sub(r).min(h - 1);
                let y_in = (y + r + 1).min(h - 1);
                sum += src[col + y_in * stride + c] - src[col + y_out * stride + c];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, c: Rgba) -> RgbaImage {
        RgbaImage::filled(w, h, c)
    }

    #[test]
    fn merge_over_replaces_with_opaque_fg() {
        let bg = solid(4, 4, [1.0, 0.0, 0.0, 1.0]);
        let fg = solid(4, 4, [0.0, 0.0, 1.0, 1.0]);
        let out = merge(&bg, &fg, BlendMode::Over, 1.0);
        // Opaque blue foreground fully covers red.
        assert_eq!(out.pixels[0], 0);
        assert_eq!(out.pixels[2], 255);
        assert_eq!(out.pixels[3], 255);
    }

    #[test]
    fn merge_over_half_opacity_mixes() {
        let bg = solid(2, 2, [1.0, 0.0, 0.0, 1.0]);
        let fg = solid(2, 2, [0.0, 0.0, 1.0, 1.0]);
        let out = merge(&bg, &fg, BlendMode::Over, 0.5);
        // ~50% blue over red → roughly half each, fully opaque.
        assert!((out.pixels[0] as i32 - 128).abs() <= 4);
        assert!((out.pixels[2] as i32 - 128).abs() <= 4);
        assert_eq!(out.pixels[3], 255);
    }

    #[test]
    fn merge_multiply_darkens() {
        let bg = solid(2, 2, [0.5, 0.5, 0.5, 1.0]);
        let fg = solid(2, 2, [0.5, 0.5, 0.5, 1.0]);
        let out = merge(&bg, &fg, BlendMode::Multiply, 1.0);
        // 0.5*0.5 = 0.25 → ~64.
        assert!((out.pixels[0] as i32 - 64).abs() <= 3);
    }

    #[test]
    fn mask_luminance_zeroes_alpha_where_black() {
        let input = solid(2, 2, [1.0, 1.0, 1.0, 1.0]);
        let mask = solid(2, 2, [0.0, 0.0, 0.0, 1.0]); // black, opaque
        let out = apply_mask(&input, &mask, MaskChannel::Luminance, false);
        assert_eq!(out.pixels[3], 0, "black luminance masks fully out");
        // White mask keeps it.
        let white = solid(2, 2, [1.0, 1.0, 1.0, 1.0]);
        let kept = apply_mask(&input, &white, MaskChannel::Luminance, false);
        assert_eq!(kept.pixels[3], 255);
    }

    #[test]
    fn crop_clears_border() {
        let input = solid(10, 10, [1.0, 1.0, 1.0, 1.0]);
        let out = crop(&input, 2.0, 2.0, 2.0, 2.0);
        // Corner cleared, center kept.
        assert_eq!(out.pixels[3], 0);
        let center = ((5 * 10 + 5) * 4 + 3) as usize;
        assert_eq!(out.pixels[center], 255);
    }

    #[test]
    fn blur_spreads_a_dot() {
        // A single opaque pixel in the center should spread outward.
        let mut input = RgbaImage::new(21, 21);
        let c = ((10 * 21 + 10) * 4) as usize;
        input.pixels[c] = 255;
        input.pixels[c + 3] = 255;
        let out = blur(&input, 3.0, 3);
        // Center alpha reduced, a neighbor now has some alpha.
        let neighbor = ((10 * 21 + 12) * 4 + 3) as usize;
        assert!(
            out.pixels[neighbor] > 0,
            "blur must spread alpha to neighbors"
        );
        assert!(out.pixels[c + 3] < 255, "center energy spread out");
    }

    #[test]
    fn color_correct_gain_brightens() {
        let input = solid(2, 2, [0.25, 0.25, 0.25, 1.0]);
        let out = color_correct(&input, [0.0; 3], [2.0, 2.0, 2.0], 1.0, 1.0);
        assert!(
            (out.pixels[0] as i32 - 128).abs() <= 3,
            "gain 2× on 0.25 → 0.5"
        );
    }

    #[test]
    fn color_correct_desaturates_to_gray() {
        let input = solid(2, 2, [1.0, 0.0, 0.0, 1.0]);
        let out = color_correct(&input, [0.0; 3], [1.0; 3], 1.0, 0.0);
        // Fully desaturated red → its luma (0.2126) on all channels.
        let g = to_u8(0.2126) as i32;
        assert!((out.pixels[0] as i32 - g).abs() <= 2);
        assert_eq!(out.pixels[0], out.pixels[1]);
        assert_eq!(out.pixels[1], out.pixels[2]);
    }
}
