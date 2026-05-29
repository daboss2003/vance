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

/// Model the Phase-6.3 pipeline end-to-end and predict the byte the
/// renderer should produce:
/// 1. YUV → R'G'B' (Rec.709 matrix, limited-range expansion).
/// 2. Linearize via Rec.709 inverse OETF.
/// 3. Working == input primaries == Rec.709, so no matrix change.
/// 4. Surface format `Rgba8UnormSrgb` applies sRGB OETF on store.
///
/// Pre-6.3 the shader skipped step 2 — bytes were "double-encoded"
/// (gamma → treated-as-linear → re-encoded). Visuals approximately
/// matched because Rec.709 and sRGB transfer curves are close, but
/// midtone bytes drifted. After 6.3 the math is right; the
/// `renders_solid_red_yuv420p` test caught the difference.
fn yuv709_limited_to_srgb_byte(y: u8, u: u8, v: u8) -> [u8; 3] {
    let yf = (y as f32 - 16.0) / 219.0;
    let uf = (u as f32 - 128.0) / 224.0;
    let vf = (v as f32 - 128.0) / 224.0;
    let r = (yf + 1.5748 * vf).clamp(0.0, 1.0);
    let g = (yf - 0.1873 * uf - 0.4681 * vf).clamp(0.0, 1.0);
    let b = (yf + 1.8556 * uf).clamp(0.0, 1.0);
    let r_lin = rec709_inv_oetf(r);
    let g_lin = rec709_inv_oetf(g);
    let b_lin = rec709_inv_oetf(b);
    [srgb_oetf(r_lin), srgb_oetf(g_lin), srgb_oetf(b_lin)]
}

fn rec709_inv_oetf(encoded: f32) -> f32 {
    if encoded < 0.081 {
        encoded / 4.5
    } else {
        ((encoded + 0.099) / 1.099).powf(1.0 / 0.45)
    }
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

// ---- Phase 6.6/6.7: grade-node chain end-to-end -------------------------

// A non-zero grade_count must actually grade — this pins the grades[]
// base offset, the 96-byte stride, and GradeNode field order on-device
// (the layout the CPU size tests can't validate). Upload a mid-gray
// frame, apply one enabled primary node with gain=2 (master), and
// assert the readback brightened versus the ungraded baseline.
#[test]
fn primary_grade_gain_brightens() {
    use vance_render::GradeNodeParams;
    // Rec.709 limited mid-gray: Y≈128, U=V=128.
    let frame = solid_yuv420p(64, 64, 128, 128, 128);

    let mut base_r = Renderer::new(RendererConfig::default()).expect("renderer");
    base_r.upload_frame(&frame).expect("upload");
    let base = center(&base_r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);

    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&frame).expect("upload");
    r.set_grades(&[GradeNodeParams {
        enabled: true,
        mode: vance_render::GradeMode::Primary,
        gain: [1.0, 1.0, 1.0, 2.0], // master gain ×2
        ..GradeNodeParams::default()
    }]);
    let graded = center(&r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);

    // gain ×2 in linear working space must brighten the mid-gray.
    assert!(
        graded[0] > base[0] + 10 && graded[1] > base[1] + 10 && graded[2] > base[2] + 10,
        "expected gain=2 to brighten: base={base:?} graded={graded:?}"
    );
}

// A disabled node must be a true no-op (bypassed in the shader), even
// with non-neutral params — verifies the flags.x gate.
#[test]
fn disabled_grade_node_is_noop() {
    use vance_render::GradeNodeParams;
    let frame = solid_yuv420p(64, 64, 128, 128, 128);

    let mut base_r = Renderer::new(RendererConfig::default()).expect("renderer");
    base_r.upload_frame(&frame).expect("upload");
    let base = center(&base_r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);

    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&frame).expect("upload");
    r.set_grades(&[GradeNodeParams {
        enabled: false, // bypassed
        gain: [1.0, 1.0, 1.0, 4.0],
        ..GradeNodeParams::default()
    }]);
    let graded = center(&r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);

    assert_eq!(base, graded, "disabled node must not change the image");
}

// A curves node with a lifting master curve must brighten mid-gray.
// The grade chain runs in linear light, so a curve that raises the low
// end (0 → 0.3) pushes the mid-gray's linear value up. Verifies the
// atlas bake + binding 10 + shader apply_curve dispatch end-to-end.
#[test]
fn curves_master_lift_brightens_midgray() {
    use vance_render::{CurveSet, GradeMode, GradeNodeParams};
    use vance_timeline::{Keyframe, KeyframeInterp};
    let frame = solid_yuv420p(64, 64, 128, 128, 128);

    let mut base_r = Renderer::new(RendererConfig::default()).expect("renderer");
    base_r.upload_frame(&frame).expect("upload");
    let base = center(&base_r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);

    let ckf = |x: f64, y: f64| Keyframe {
        time_seconds: x,
        value: y,
        interp: KeyframeInterp::Linear,
    };
    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&frame).expect("upload");
    r.set_grades(&[GradeNodeParams {
        enabled: true,
        mode: GradeMode::Curves,
        curves: Some(CurveSet {
            master: vec![ckf(0.0, 0.3), ckf(1.0, 1.0)],
            ..CurveSet::default()
        }),
        ..GradeNodeParams::default()
    }]);
    let graded = center(&r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);

    assert!(
        graded[0] > base[0] + 10 && graded[1] > base[1] + 10 && graded[2] > base[2] + 10,
        "expected lifting master curve to brighten: base={base:?} graded={graded:?}"
    );
}

// The LUMA curve (atlas alpha lane) must brighten via the hue-preserving
// ratio path — exercises a different lane than the rgb-channel test and
// the input-luma sampling in apply_curve.
#[test]
fn curves_luma_lift_brightens_midgray() {
    use vance_render::{CurveSet, GradeMode, GradeNodeParams};
    use vance_timeline::{Keyframe, KeyframeInterp};
    let frame = solid_yuv420p(64, 64, 128, 128, 128);

    let mut base_r = Renderer::new(RendererConfig::default()).expect("renderer");
    base_r.upload_frame(&frame).expect("upload");
    let base = center(&base_r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);

    let ckf = |x: f64, y: f64| Keyframe {
        time_seconds: x,
        value: y,
        interp: KeyframeInterp::Linear,
    };
    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&frame).expect("upload");
    r.set_grades(&[GradeNodeParams {
        enabled: true,
        mode: GradeMode::Curves,
        curves: Some(CurveSet {
            // Lift the whole luminance range: out *= curve(l)/l > 1.
            luma: vec![ckf(0.0, 0.3), ckf(1.0, 1.0)],
            ..CurveSet::default()
        }),
        ..GradeNodeParams::default()
    }]);
    let graded = center(&r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);

    assert!(
        graded[0] > base[0] + 8 && graded[1] > base[1] + 8 && graded[2] > base[2] + 8,
        "expected luma lift to brighten: base={base:?} graded={graded:?}"
    );
}

// An all-identity curves node must be a true no-op (the shader skips
// sampling when log_smh.x == 0). Guards against the atlas default rows
// or the identity-skip flag drifting.
#[test]
fn identity_curves_node_is_noop() {
    use vance_render::{CurveSet, GradeMode, GradeNodeParams};
    let frame = solid_yuv420p(64, 64, 96, 140, 110);

    let mut base_r = Renderer::new(RendererConfig::default()).expect("renderer");
    base_r.upload_frame(&frame).expect("upload");
    let base = center(&base_r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);

    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&frame).expect("upload");
    r.set_grades(&[GradeNodeParams {
        enabled: true,
        mode: GradeMode::Curves,
        curves: Some(CurveSet::default()), // all empty = identity
        ..GradeNodeParams::default()
    }]);
    let graded = center(&r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);

    assert_eq!(base, graded, "identity curves node must not change the image");
}

// ---- HSL qualifier / secondary key (Phase 6.9) ---------------------------

use vance_render::{GradeMode, GradeNodeParams, Qualifier};

// Build a primary node with a strong master gain (×2) and an attached
// qualifier. Mask-modulated: where mask==0 the node is a no-op.
fn gain_node(qual: Qualifier) -> GradeNodeParams {
    GradeNodeParams {
        enabled: true,
        mode: GradeMode::Primary,
        gain: [1.0, 1.0, 1.0, 2.0],
        qual,
        ..GradeNodeParams::default()
    }
}

fn matte_node(qual: Qualifier) -> GradeNodeParams {
    let mut q = qual;
    q.enabled = true;
    q.show_mask = true;
    GradeNodeParams {
        enabled: true,
        mode: GradeMode::Primary,
        qual: q,
        ..GradeNodeParams::default()
    }
}

fn render_center(grades: &[GradeNodeParams], frame: &vance_decoder::FrameData) -> [u8; 4] {
    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(frame).expect("upload");
    r.set_grades(grades);
    center(&r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64)
}

// A disabled qualifier (even with non-neutral ranges) must be a true
// passthrough — mask==1, so the node behaves exactly as pre-6.9.
#[test]
fn qualifier_off_is_passthrough() {
    let gray = solid_yuv420p(64, 64, 128, 128, 128);
    let plain = render_center(&[gain_node(Qualifier::default())], &gray);
    let weird_but_off = render_center(
        &[gain_node(Qualifier {
            enabled: false,
            hue: [0.3, 0.1, 0.02],
            sat: [0.8, 0.9, 0.01],
            luma: [0.1, 0.2, 0.01],
            ..Qualifier::default()
        })],
        &gray,
    );
    assert_eq!(plain, weird_but_off, "disabled qualifier must not change output");
}

// Wide-open qualifier (default ranges) -> mask==1 everywhere -> matte white.
#[test]
fn matte_wide_open_is_white() {
    let gray = solid_yuv420p(64, 64, 128, 128, 128);
    let m = render_center(&[matte_node(Qualifier::default())], &gray);
    assert!(m[0] > 250 && m[1] > 250 && m[2] > 250, "expected white matte, got {m:?}");
}

// A saturation window that excludes a neutral gray (sat==0) -> mask 0 ->
// black matte.
#[test]
fn matte_sat_window_excludes_gray() {
    let gray = solid_yuv420p(64, 64, 128, 128, 128);
    let m = render_center(
        &[matte_node(Qualifier {
            sat: [0.5, 1.0, 0.02],
            ..Qualifier::default()
        })],
        &gray,
    );
    assert!(m[0] < 5 && m[1] < 5 && m[2] < 5, "expected black matte, got {m:?}");
}

// Invert flips the excluded gray back to fully keyed (white).
#[test]
fn matte_invert_flips() {
    let gray = solid_yuv420p(64, 64, 128, 128, 128);
    let m = render_center(
        &[matte_node(Qualifier {
            sat: [0.5, 1.0, 0.02],
            invert: true,
            ..Qualifier::default()
        })],
        &gray,
    );
    assert!(m[0] > 250 && m[1] > 250 && m[2] > 250, "inverted matte should be white, got {m:?}");
}

// Luma window keys a near-white frame only when its band includes ~1.0.
#[test]
fn matte_luma_window_selects_by_brightness() {
    let white = solid_yuv420p(64, 64, 235, 128, 128);
    let included = render_center(
        &[matte_node(Qualifier {
            luma: [0.5, 1.0, 0.02],
            ..Qualifier::default()
        })],
        &white,
    );
    let excluded = render_center(
        &[matte_node(Qualifier {
            luma: [0.0, 0.4, 0.02],
            ..Qualifier::default()
        })],
        &white,
    );
    assert!(included[0] > 250, "white in [0.5,1] luma band should key white, got {included:?}");
    assert!(excluded[0] < 5, "white outside [0,0.4] luma band should be black, got {excluded:?}");
}

// Hue keying: a red frame (hue ~= 0) is keyed by a narrow band at hue 0
// and excluded by the opposite band at hue 0.5 (tests cyclic distance).
#[test]
fn matte_hue_key_red_in_excluded_at_opposite() {
    let red = solid_yuv420p(64, 64, 63, 102, 240);
    let keyed = render_center(
        &[matte_node(Qualifier {
            hue: [0.0, 0.2, 0.02],
            ..Qualifier::default()
        })],
        &red,
    );
    let not_keyed = render_center(
        &[matte_node(Qualifier {
            hue: [0.5, 0.2, 0.02],
            ..Qualifier::default()
        })],
        &red,
    );
    assert!(keyed[0] > 240, "red should key at hue 0, got {keyed:?}");
    assert!(not_keyed[0] < 15, "red should be excluded at hue 0.5, got {not_keyed:?}");
}

// Out-of-key correction is a bypass: a gain node keyed to a hue the frame
// lacks leaves the image byte-identical to ungraded.
#[test]
fn out_of_key_is_bypass() {
    let red = solid_yuv420p(64, 64, 63, 102, 240);
    let base = render_center(&[], &red);
    let off_hue = render_center(
        &[gain_node(Qualifier {
            enabled: true,
            hue: [0.5, 0.2, 0.02], // cyan key — red is excluded
            ..Qualifier::default()
        })],
        &red,
    );
    assert_eq!(base, off_hue, "out-of-key node must be a pixel-exact bypass");
    // Sanity: an in-key version DOES change the pixel.
    let in_key = render_center(
        &[gain_node(Qualifier {
            enabled: true,
            hue: [0.0, 0.2, 0.02],
            ..Qualifier::default()
        })],
        &red,
    );
    assert_ne!(base, in_key, "in-key node should change the pixel");
}

// The qualifier gates identically regardless of node mode (log here).
#[test]
fn qualifier_is_mode_agnostic() {
    let red = solid_yuv420p(64, 64, 63, 102, 240);
    let base = render_center(&[], &red);
    let log_off_key = render_center(
        &[GradeNodeParams {
            enabled: true,
            mode: GradeMode::Log,
            offset: [0.3, 0.3, 0.3],
            qual: Qualifier {
                enabled: true,
                hue: [0.5, 0.2, 0.02], // exclude red
                ..Qualifier::default()
            },
            ..GradeNodeParams::default()
        }],
        &red,
    );
    assert_eq!(base, log_off_key, "out-of-key LOG node must also bypass");
}
