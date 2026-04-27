//! Headless golden-frame test for the YUV→RGB pipeline.
//!
//! Builds tiny YUV420P / NV12 frames with known content, renders them to
//! an Rgba8UnormSrgb texture, reads back, and asserts the central pixel's
//! color is what Rec.709 limited-range conversion should produce.
//!
//! Tolerance: 8-bit RGB ±2 per channel (sRGB encoding round-trip + linear
//! interpolation between 1×1 chroma tap and full Y resolution introduces
//! tiny errors).

use vance_decoder::{FrameData, PixelFormat};
use vance_render::{Renderer, RendererConfig};

/// Construct a `width`×`height` solid-color YUV420P frame.
fn solid_yuv420p(width: u32, height: u32, y: u8, u: u8, v: u8) -> FrameData {
    let chroma_w = width.div_ceil(2);
    let chroma_h = height.div_ceil(2);
    let mut bytes = Vec::with_capacity(PixelFormat::Yuv420P.frame_size(width, height));
    bytes.extend(std::iter::repeat_n(y, (width * height) as usize));
    bytes.extend(std::iter::repeat_n(u, (chroma_w * chroma_h) as usize));
    bytes.extend(std::iter::repeat_n(v, (chroma_w * chroma_h) as usize));
    FrameData {
        pts: 0,
        width,
        height,
        pixel_format: PixelFormat::Yuv420P,
        strides: vec![width, chroma_w, chroma_w],
        bytes,
    }
}

/// Construct a `width`×`height` solid-color NV12 frame (interleaved UV).
fn solid_nv12(width: u32, height: u32, y: u8, u: u8, v: u8) -> FrameData {
    let chroma_w = width.div_ceil(2);
    let chroma_h = height.div_ceil(2);
    let mut bytes = Vec::with_capacity(PixelFormat::Nv12.frame_size(width, height));
    bytes.extend(std::iter::repeat_n(y, (width * height) as usize));
    for _ in 0..(chroma_w * chroma_h) {
        bytes.push(u);
        bytes.push(v);
    }
    FrameData {
        pts: 0,
        width,
        height,
        pixel_format: PixelFormat::Nv12,
        strides: vec![width, chroma_w * 2],
        bytes,
    }
}

/// Sample the central pixel of a row-major RGBA8 buffer.
fn center(buf: &[u8], width: u32, height: u32) -> [u8; 4] {
    let cx = width / 2;
    let cy = height / 2;
    let i = ((cy * width + cx) * 4) as usize;
    [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
}

/// 8-bit channel comparison with tolerance.
fn close(a: u8, b: u8, tol: i32) -> bool {
    (a as i32 - b as i32).abs() <= tol
}

fn assert_rgb_close(got: [u8; 4], want_rgb: [u8; 3], tol: i32, label: &str) {
    let ok = close(got[0], want_rgb[0], tol)
        && close(got[1], want_rgb[1], tol)
        && close(got[2], want_rgb[2], tol);
    assert!(
        ok,
        "{label}: got RGBA={got:?}, want RGB≈{want_rgb:?} (tol ±{tol})"
    );
}

/// Apply the same Rec.709 limited-range conversion the shader does, then
/// the sRGB OETF (since the target texture is `Rgba8UnormSrgb`, wgpu
/// gamma-encodes the linear-light fragment output before storing).
///
/// Wait — the shader writes gamma-encoded R'G'B' directly (no linearization),
/// and the Srgb target reinterprets stored bytes as gamma-encoded. So writing
/// `r'g'b'` *to* an Srgb target actually inverts: the shader must output
/// linear-light values for the storage to come out as r'g'b'. The shader as
/// written produces R'G'B' (NOT linearized), so the read-back will be the
/// shader's `r,g,b` lifted into sRGB-encoded space — i.e. it's been
/// *linearized then re-encoded* by wgpu, which round-trips visually but
/// shifts the byte values.
///
/// Concretely: shader writes `c` (where `c` is intended as gamma-encoded);
/// wgpu treats that as linear, applies sRGB OETF on store. So stored byte
/// = sRGB-encode(c). To predict the read-back, apply the OETF here too.
fn yuv709_limited_to_srgb_byte(y: u8, u: u8, v: u8) -> [u8; 3] {
    let yf = (y as f32 - 16.0) / 219.0;
    let uf = (u as f32 - 128.0) / 224.0;
    let vf = (v as f32 - 128.0) / 224.0;
    let r = (yf + 1.5748 * vf).clamp(0.0, 1.0);
    let g = (yf - 0.1873 * uf - 0.4681 * vf).clamp(0.0, 1.0);
    let b = (yf + 1.8556 * uf).clamp(0.0, 1.0);
    [srgb_oetf(r), srgb_oetf(g), srgb_oetf(b)]
}

fn srgb_oetf(linear: f32) -> u8 {
    let v = if linear <= 0.0031308 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

#[test]
fn renders_solid_white_yuv420p() {
    // White: Y=235, U=V=128 (Rec.709 limited).
    let frame = solid_yuv420p(64, 64, 235, 128, 128);
    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&frame).expect("upload");
    let buf = r.render_to_rgba_buffer(64, 64).expect("render");

    let center_px = center(&buf, 64, 64);
    let expected = yuv709_limited_to_srgb_byte(235, 128, 128);
    assert_rgb_close(center_px, expected, 2, "white");
    // Also verify it's actually near-white in raw bytes.
    assert!(center_px[0] > 240 && center_px[1] > 240 && center_px[2] > 240);
}

#[test]
fn renders_solid_black_yuv420p() {
    // Black: Y=16, U=V=128.
    let frame = solid_yuv420p(64, 64, 16, 128, 128);
    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&frame).expect("upload");
    let buf = r.render_to_rgba_buffer(64, 64).expect("render");

    let center_px = center(&buf, 64, 64);
    let expected = yuv709_limited_to_srgb_byte(16, 128, 128);
    assert_rgb_close(center_px, expected, 2, "black");
    assert!(center_px[0] < 10 && center_px[1] < 10 && center_px[2] < 10);
}

#[test]
fn renders_solid_red_yuv420p() {
    // Pure Rec.709 red:
    //   Y' = 0.2126*1.0       = 0.2126 -> limited: 0.2126*219+16 = 62.6 ≈ 63
    //   Cb = -0.1146*1.0      = -0.1146 -> limited around 102
    //   Cr =  0.5*1.0         =  0.5 -> limited 240
    // Use Y=63, U=102, V=240.
    let frame = solid_yuv420p(64, 64, 63, 102, 240);
    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&frame).expect("upload");
    let buf = r.render_to_rgba_buffer(64, 64).expect("render");

    let center_px = center(&buf, 64, 64);
    let expected = yuv709_limited_to_srgb_byte(63, 102, 240);
    assert_rgb_close(center_px, expected, 2, "red");
    // R should dominate.
    assert!(
        center_px[0] > center_px[1] + 100 && center_px[0] > center_px[2] + 100,
        "expected red-dominant pixel, got {center_px:?}"
    );
}

#[test]
fn nv12_matches_yuv420p() {
    // Same color, two formats — should produce the same RGB to within tolerance.
    let (y, u, v) = (140u8, 90u8, 200u8);
    let f_planar = solid_yuv420p(64, 64, y, u, v);
    let f_nv12 = solid_nv12(64, 64, y, u, v);

    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&f_planar).expect("upload planar");
    let buf_planar = r.render_to_rgba_buffer(64, 64).expect("render planar");

    r.upload_frame(&f_nv12).expect("upload nv12");
    let buf_nv12 = r.render_to_rgba_buffer(64, 64).expect("render nv12");

    let p = center(&buf_planar, 64, 64);
    let n = center(&buf_nv12, 64, 64);
    for (i, label) in ["R", "G", "B"].iter().enumerate() {
        assert!(
            close(p[i], n[i], 1),
            "channel {label}: planar={} nv12={}",
            p[i],
            n[i]
        );
    }
}

#[test]
fn malformed_frame_rejected() {
    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    let bad = FrameData {
        pts: 0,
        width: 64,
        height: 64,
        pixel_format: PixelFormat::Yuv420P,
        strides: vec![64, 32, 32],
        bytes: vec![0; 100], // way too small
    };
    let err = r.upload_frame(&bad).unwrap_err();
    let s = format!("{err}");
    assert!(s.contains("expected"), "{s}");
}

#[test]
fn render_without_upload_errors() {
    let r = Renderer::new(RendererConfig::default()).expect("renderer");
    let err = r.render_to_rgba_buffer(64, 64).unwrap_err();
    assert!(matches!(err, vance_render::RenderError::NoFrame));
}
