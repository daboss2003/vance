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
        bytes: bytes.into(),
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
        bytes: bytes.into(),
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

/// Construct a `width`×`height` solid 10-bit YUV420P10LE frame (u16 LE
/// samples, 3 planes).
fn solid_yuv420p10le(width: u32, height: u32, y: u16, u: u16, v: u16) -> FrameData {
    let cw = width.div_ceil(2);
    let ch = height.div_ceil(2);
    let mut bytes = Vec::with_capacity((width * height * 3) as usize);
    for _ in 0..(width * height) {
        bytes.extend_from_slice(&y.to_le_bytes());
    }
    for _ in 0..(cw * ch) {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    for _ in 0..(cw * ch) {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    FrameData {
        pts: 0,
        width,
        height,
        pixel_format: PixelFormat::Yuv420P10Le,
        strides: vec![width, cw, cw],
        bytes: bytes.into(),
    }
}

/// A 10-bit source decodes to the SAME RGB as its 8-bit equivalent (the
/// R16Unorm upload + sample-scale path is correct): 10-bit white
/// (Y=940, C=512) matches 8-bit white (Y=235, C=128).
#[test]
fn ten_bit_matches_eight_bit_equivalent() {
    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");

    // 8-bit reference white.
    r.upload_frame(&solid_yuv420p(64, 64, 235, 128, 128))
        .unwrap();
    let ref8 = center(&r.render_to_rgba_buffer(64, 64).unwrap(), 64, 64);

    // 10-bit white (values ×4 of the 8-bit limited points).
    r.upload_frame(&solid_yuv420p10le(64, 64, 940, 512, 512))
        .unwrap();
    let px10 = center(&r.render_to_rgba_buffer(64, 64).unwrap(), 64, 64);

    for ch in 0..3 {
        assert!(
            (px10[ch] as i32 - ref8[ch] as i32).abs() <= 2,
            "10-bit ch{ch}={} vs 8-bit ref={} (should match within rounding)",
            px10[ch],
            ref8[ch]
        );
    }
    assert!(px10[0] > 240, "10-bit white should be bright: {}", px10[0]);

    // A 10-bit mid-gray also matches its 8-bit equivalent.
    r.upload_frame(&solid_yuv420p(64, 64, 128, 128, 128))
        .unwrap();
    let gray8 = center(&r.render_to_rgba_buffer(64, 64).unwrap(), 64, 64);
    r.upload_frame(&solid_yuv420p10le(64, 64, 512, 512, 512))
        .unwrap();
    let gray10 = center(&r.render_to_rgba_buffer(64, 64).unwrap(), 64, 64);
    assert!(
        (gray10[0] as i32 - gray8[0] as i32).abs() <= 3,
        "mid-gray 10 vs 8 bit"
    );
}

/// FULL 10-bit HDR round-trip: a 10-bit PQ Rec.2020 source decoded and
/// re-encoded to PQ Rec.2020 must come back unchanged (PQ input EOTF is the
/// exact inverse of the PQ output OETF), and a highlight above reference
/// white must survive rather than clip. This exercises: 10-bit R16 upload →
/// YUV→R'G'B' → PQ inverse-EOTF (input) → working → PQ OETF (output) → 10-bit
/// readback.
#[test]
fn ten_bit_pq_source_round_trips_including_highlights() {
    use vance_render::{Renderer as R, RendererConfig as Cfg};
    let compositor_pipeline = |code: f64| {
        // 10-bit limited-range luma for an encoded R'G'B' value `code`:
        // Y10 = code·876 + 64; neutral chroma = 512.
        (code * 876.0 + 64.0).round() as u16
    };
    let mut r = R::new(Cfg {
        target_format: wgpu::TextureFormat::Rgba16Float,
        ..Cfg::default()
    })
    .expect("hdr renderer");
    // Input Rec.2020 PQ, working Linear Rec.2020 (identity primaries), output
    // Rec.2020 PQ — a pure transfer round-trip.
    let params = vance_color::Pipeline {
        input: vance_color::ColorSpace::Rec2020Pq,
        working: vance_color::ColorSpace::LinearRec2020,
        output: vance_color::ColorSpace::Rec2020Pq,
    }
    .to_params()
    .expect("PQ→PQ resolves");
    r.set_pipeline(params);

    let read_center_r = |r: &R| -> u16 {
        let buf = r.render_to_rgba64_buffer(64, 64).unwrap();
        let i = ((32 * 64 + 32) * 8) as usize;
        u16::from_le_bytes([buf[i], buf[i + 1]])
    };

    // Reference-white-ish mid PQ code 0.5 round-trips close to 0.5 (the PQ
    // input EOTF is the inverse of the output OETF).
    r.upload_frame(&solid_yuv420p10le(
        64,
        64,
        compositor_pipeline(0.5),
        512,
        512,
    ))
    .unwrap();
    let mid = read_center_r(&r);
    let want_mid = (0.5 * 65535.0) as i32;
    assert!(
        (mid as i32 - want_mid).abs() < 1200,
        "PQ 0.5 round-trip: {mid} vs ~{want_mid}"
    );

    // A HIGHLIGHT (PQ code 0.75, far above reference white) must SURVIVE, not
    // clip to reference white. If the [0,1] working clamp weren't lifted for
    // HDR, working would clamp at 1.0 (100 nits) and the highlight would read
    // ~0.5 (≈ mid). It reads much higher, and round-trips approximately (the
    // residual is the accumulated 10-bit↔8-bit-constant + steep-PQ-curve
    // approximation; exact HDR fidelity is a hardware-verified follow-up).
    r.upload_frame(&solid_yuv420p10le(
        64,
        64,
        compositor_pipeline(0.75),
        512,
        512,
    ))
    .unwrap();
    let hi = read_center_r(&r);
    assert!(
        hi > mid + 10000,
        "highlight must survive well above reference white (not clipped): hi={hi} mid={mid}"
    );
    assert!(
        hi > 42000 && hi < 55000,
        "highlight round-trips near PQ 0.75: {hi}"
    );
}

/// HDR → SDR preview tone map: an HDR (PQ) source shown on an SDR output
/// must preserve highlight DETAIL — three increasingly bright HDR inputs
/// produce three strictly-increasing SDR values. The old behavior (clamp to
/// [0,1]) would flatten every above-white highlight to the same 255.
#[test]
fn hdr_source_to_sdr_tonemaps_highlights() {
    let y_for = |code: f64| (code * 876.0 + 64.0).round() as u16;
    // SDR (Rgba8) output, HDR (PQ) input → the tone-map path.
    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    let params = vance_color::Pipeline {
        input: vance_color::ColorSpace::Rec2020Pq,
        working: vance_color::ColorSpace::LinearRec2020,
        output: vance_color::ColorSpace::Srgb,
    }
    .to_params()
    .expect("PQ input → SDR output resolves");
    r.set_pipeline(params);

    let render_pq = |r: &mut Renderer, code: f64| -> u8 {
        r.upload_frame(&solid_yuv420p10le(64, 64, y_for(code), 512, 512))
            .unwrap();
        center(&r.render_to_rgba_buffer(64, 64).unwrap(), 64, 64)[0]
    };
    // PQ 0.4 (~50 nits), 0.6 (~180 nits), 0.85 (bright) — all above SDR
    // reference-white in working space.
    let a = render_pq(&mut r, 0.4);
    let b = render_pq(&mut r, 0.6);
    let c = render_pq(&mut r, 0.85);
    assert!(
        a < b && b < c,
        "tone map must keep highlight ordering: {a} < {b} < {c}"
    );
    assert!(
        c > 240,
        "the brightest highlight should be near SDR white: {c}"
    );
    // Detail is preserved: the mid-highlight is clearly below white (a hard
    // clamp would have pinned it to 255 along with the brighter ones).
    assert!(b < 250, "mid-highlight must not be fully clipped: {b}");
}

/// HDR end-to-end (render → PQ → 10-bit readback): white through an
/// Rgba16Float renderer with a Rec.709 → Rec.2020-PQ pipeline must come back
/// as the analytic PQ code value of SDR reference white (100 cd/m²).
#[test]
fn hdr_white_encodes_to_pq_reference() {
    use vance_render::{Renderer as R, RendererConfig as Cfg};
    // White YUV → Rec.709 (1,1,1) → linear 1.0 → Rec.2020 primaries (white
    // stays white) → * 100/10000 = 0.01 → PQ(0.01) ≈ 0.5081.
    let frame = solid_yuv420p(64, 64, 235, 128, 128);
    let mut r = R::new(Cfg {
        target_format: wgpu::TextureFormat::Rgba16Float,
        ..Cfg::default()
    })
    .expect("hdr renderer");
    // Rec.709 limited input, Linear Rec.709 working, Rec.2020 PQ output.
    let params = vance_color::Pipeline {
        input: vance_color::ColorSpace::Rec709Limited,
        working: vance_color::ColorSpace::LinearRec709,
        output: vance_color::ColorSpace::Rec2020Pq,
    }
    .to_params()
    .expect("Rec.2020 PQ output resolves");
    r.set_pipeline(params);
    r.upload_frame(&frame).expect("upload");
    let buf = r.render_to_rgba64_buffer(64, 64).expect("hdr render");

    // Center pixel, R channel as u16 LE (8 bytes/px).
    let cx = 32u32;
    let cy = 32u32;
    let i = ((cy * 64 + cx) * 8) as usize;
    let r16 = u16::from_le_bytes([buf[i], buf[i + 1]]);
    let g16 = u16::from_le_bytes([buf[i + 2], buf[i + 3]]);
    let b16 = u16::from_le_bytes([buf[i + 4], buf[i + 5]]);
    let a16 = u16::from_le_bytes([buf[i + 6], buf[i + 7]]);

    // PQ(0.01) ≈ 0.5081 → code ≈ 33305. Tolerance covers half-float precision
    // + YUV/round-trip.
    let expected = (pq_ref(0.01) * 65535.0).round() as i32;
    for (ch, v) in [("R", r16), ("G", g16), ("B", b16)] {
        assert!(
            (v as i32 - expected).abs() < 500,
            "{ch} PQ code {v} vs expected ~{expected}"
        );
    }
    assert_eq!(a16, 65535, "opaque alpha");
    // Sanity: a PQ-coded 100-nit white is a MID code, not near-max (which is
    // what an SDR white or a mis-tag would produce).
    assert!(
        r16 > 25000 && r16 < 42000,
        "PQ white must be mid-range, got {r16}"
    );
}

/// Reference SMPTE ST 2084 (PQ) OETF, `E = L/10000` in [0,1].
fn pq_ref(e: f64) -> f64 {
    let (m1, m2, c1, c2, c3) = (0.1593017578125, 78.84375, 0.8359375, 18.8515625, 18.6875);
    let ym1 = e.clamp(0.0, 1.0).powf(m1);
    ((c1 + c2 * ym1) / (1.0 + c3 * ym1)).powf(m2)
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
fn odd_dimension_frames_upload_and_render() {
    // 4:2:0 chroma planes are ceil(w/2) x ceil(h/2), so an odd-sized frame
    // carries more bytes than w*h*3/2. Every format must accept a correctly
    // built odd frame instead of rejecting it as malformed.
    let (w, h) = (161u32, 91u32);
    let expected = yuv709_limited_to_srgb_byte(63, 102, 240);
    let mut ten_bit = solid_yuv420p(w, h, 0, 0, 0);
    ten_bit.pixel_format = PixelFormat::Yuv420P10Le;
    ten_bit.bytes = [
        (w * h, 63u16 << 2),
        (w.div_ceil(2) * h.div_ceil(2), 102 << 2),
    ]
    .into_iter()
    .chain([(w.div_ceil(2) * h.div_ceil(2), 240 << 2)])
    .flat_map(|(n, code)| std::iter::repeat_n(code.to_le_bytes(), n as usize).flatten())
    .collect::<Vec<u8>>()
    .into();
    for (label, frame) in [
        ("yuv420p", solid_yuv420p(w, h, 63, 102, 240)),
        ("nv12", solid_nv12(w, h, 63, 102, 240)),
        ("yuv420p10le", ten_bit),
    ] {
        let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
        r.upload_frame(&frame)
            .unwrap_or_else(|e| panic!("{label} {w}x{h} upload: {e:?}"));
        let buf = r.render_to_rgba_buffer(w, h).expect("render");
        assert_rgb_close(center(&buf, w, h), expected, 3, label);
    }
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
        bytes: vec![0; 100].into(), // way too small
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
    let base = center(
        &base_r.render_to_rgba_buffer(64, 64).expect("render"),
        64,
        64,
    );

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
    let base = center(
        &base_r.render_to_rgba_buffer(64, 64).expect("render"),
        64,
        64,
    );

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
    let base = center(
        &base_r.render_to_rgba_buffer(64, 64).expect("render"),
        64,
        64,
    );

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
    let base = center(
        &base_r.render_to_rgba_buffer(64, 64).expect("render"),
        64,
        64,
    );

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
    let base = center(
        &base_r.render_to_rgba_buffer(64, 64).expect("render"),
        64,
        64,
    );

    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&frame).expect("upload");
    r.set_grades(&[GradeNodeParams {
        enabled: true,
        mode: GradeMode::Curves,
        curves: Some(CurveSet::default()), // all empty = identity
        ..GradeNodeParams::default()
    }]);
    let graded = center(&r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);

    assert_eq!(
        base, graded,
        "identity curves node must not change the image"
    );
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
    assert_eq!(
        plain, weird_but_off,
        "disabled qualifier must not change output"
    );
}

// Wide-open qualifier (default ranges) -> mask==1 everywhere -> matte white.
#[test]
fn matte_wide_open_is_white() {
    let gray = solid_yuv420p(64, 64, 128, 128, 128);
    let m = render_center(&[matte_node(Qualifier::default())], &gray);
    assert!(
        m[0] > 250 && m[1] > 250 && m[2] > 250,
        "expected white matte, got {m:?}"
    );
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
    assert!(
        m[0] < 5 && m[1] < 5 && m[2] < 5,
        "expected black matte, got {m:?}"
    );
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
    assert!(
        m[0] > 250 && m[1] > 250 && m[2] > 250,
        "inverted matte should be white, got {m:?}"
    );
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
    assert!(
        included[0] > 250,
        "white in [0.5,1] luma band should key white, got {included:?}"
    );
    assert!(
        excluded[0] < 5,
        "white outside [0,0.4] luma band should be black, got {excluded:?}"
    );
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
    assert!(
        not_keyed[0] < 15,
        "red should be excluded at hue 0.5, got {not_keyed:?}"
    );
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
    assert_eq!(
        base, off_hue,
        "out-of-key node must be a pixel-exact bypass"
    );
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

// ---- Power window / shape mask (Phase 6.10) ------------------------------

use vance_render::Window;

/// Sample an arbitrary pixel of a row-major RGBA8 buffer.
fn sample_at(buf: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
}

/// A matte node that solos its (window-only) mask to the viewport.
fn window_matte_node(window: Window) -> GradeNodeParams {
    GradeNodeParams {
        enabled: true,
        qual: Qualifier {
            show_mask: true,
            ..Qualifier::default()
        },
        window,
        ..GradeNodeParams::default()
    }
}

fn ellipse(cx: f32, cy: f32, rx: f32, ry: f32) -> Window {
    Window {
        shape: 1,
        center: [cx, cy],
        size: [rx, ry],
        feather: 0.01,
        ..Window::default()
    }
}

// A centered ellipse window keys the center pixel and excludes the corner
// (matte: center white, corner black).
#[test]
fn window_ellipse_matte_center_white_corner_black() {
    let gray = solid_yuv420p(64, 64, 128, 128, 128);
    let buf = {
        let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
        r.upload_frame(&gray).expect("upload");
        r.set_grades(&[window_matte_node(ellipse(0.5, 0.5, 0.3, 0.3))]);
        r.render_to_rgba_buffer(64, 64).expect("render")
    };
    let mid = center(&buf, 64, 64);
    let corner = sample_at(&buf, 64, 2, 2);
    assert!(
        mid[0] > 250,
        "center should be inside the window (white matte): {mid:?}"
    );
    assert!(
        corner[0] < 5,
        "corner should be outside (black matte): {corner:?}"
    );
}

// A rectangle window includes a near-corner point that the inscribed
// ellipse would exclude (sampled inside the box, off the ellipse).
#[test]
fn window_rect_matte_fills_box() {
    let gray = solid_yuv420p(64, 64, 128, 128, 128);
    let rect = Window {
        shape: 2,
        center: [0.5, 0.5],
        size: [0.4, 0.4],
        feather: 0.01,
        ..Window::default()
    };
    let buf = {
        let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
        r.upload_frame(&gray).expect("upload");
        r.set_grades(&[window_matte_node(rect)]);
        r.render_to_rgba_buffer(64, 64).expect("render")
    };
    // (0.5+0.3, 0.5+0.3) in uv -> pixel ~ (51, 51): inside the 0.4 box,
    // outside the inscribed ellipse.
    let box_corner = sample_at(&buf, 64, 51, 51);
    assert!(
        box_corner[0] > 200,
        "box corner should be keyed by a rect window: {box_corner:?}"
    );
}

// Inverting the window flips inside/outside.
#[test]
fn window_invert_matte_flips() {
    let gray = solid_yuv420p(64, 64, 128, 128, 128);
    let mut win = ellipse(0.5, 0.5, 0.3, 0.3);
    win.invert = true;
    let buf = {
        let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
        r.upload_frame(&gray).expect("upload");
        r.set_grades(&[window_matte_node(win)]);
        r.render_to_rgba_buffer(64, 64).expect("render")
    };
    let mid = center(&buf, 64, 64);
    let corner = sample_at(&buf, 64, 2, 2);
    assert!(mid[0] < 5, "inverted: center now excluded: {mid:?}");
    assert!(corner[0] > 250, "inverted: corner now keyed: {corner:?}");
}

// shape == 0 (window off) is a byte-for-byte passthrough even with
// non-neutral window params present.
#[test]
fn window_off_is_bit_identical() {
    let gray = solid_yuv420p(64, 64, 128, 128, 128);
    let plain = render_center(&[gain_node(Qualifier::default())], &gray);
    let with_off_window = render_center(
        &[GradeNodeParams {
            enabled: true,
            gain: [1.0, 1.0, 1.0, 2.0],
            window: Window {
                shape: 0,
                center: [0.2, 0.3],
                size: [0.1, 0.1],
                ..Window::default()
            },
            ..GradeNodeParams::default()
        }],
        &gray,
    );
    assert_eq!(
        plain, with_off_window,
        "window shape 0 must be an exact passthrough"
    );
}

// The node mask is qualifier_mask * window_mask: a red frame with the
// qualifier keyed to CYAN (excludes red -> 0) makes the combined matte
// black everywhere regardless of the window shape.
#[test]
fn window_times_qualifier_intersection() {
    let red = solid_yuv420p(64, 64, 63, 102, 240);
    // qualifier keyed to red (passes) + ellipse window -> center white.
    let pass = {
        let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
        r.upload_frame(&red).expect("upload");
        r.set_grades(&[GradeNodeParams {
            enabled: true,
            qual: Qualifier {
                enabled: true,
                show_mask: true,
                hue: [0.0, 0.2, 0.02],
                ..Qualifier::default()
            },
            window: ellipse(0.5, 0.5, 0.3, 0.3),
            ..GradeNodeParams::default()
        }]);
        center(&r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64)
    };
    // qualifier keyed to cyan (excludes red) -> combined mask 0 -> black.
    let excluded = {
        let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
        r.upload_frame(&red).expect("upload");
        r.set_grades(&[GradeNodeParams {
            enabled: true,
            qual: Qualifier {
                enabled: true,
                show_mask: true,
                hue: [0.5, 0.2, 0.02],
                ..Qualifier::default()
            },
            window: ellipse(0.5, 0.5, 0.3, 0.3),
            ..GradeNodeParams::default()
        }]);
        center(&r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64)
    };
    assert!(pass[0] > 240, "in-hue + inside window -> keyed: {pass:?}");
    assert!(
        excluded[0] < 15,
        "off-hue zeroes the combined mask: {excluded:?}"
    );
}

// Aspect: on a non-square (96x54, 16:9) frame, an equal-radii ellipse is
// circular in PIXELS, so a horizontal offset scaled by 1/aspect and the
// matching vertical offset land at the same mask value.
#[test]
fn window_aspect_circular_on_wide_frame() {
    let frame = solid_yuv420p(96, 54, 128, 128, 128);
    let buf = {
        let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
        r.upload_frame(&frame).expect("upload");
        r.set_grades(&[window_matte_node(ellipse(0.5, 0.5, 0.3, 0.3))]);
        r.render_to_rgba_buffer(96, 54).expect("render")
    };
    let aspect = 96.0 / 54.0;
    // uv offset 0.25 in x (scaled by 1/aspect) vs 0.25 in y from center.
    let px = ((0.5 + 0.25 / aspect) * 96.0) as u32;
    let x_pt = sample_at(&buf, 96, px, 27);
    let y_pt = sample_at(&buf, 96, 48, ((0.5 + 0.25) * 54.0) as u32);
    assert!(
        (x_pt[0] as i32 - y_pt[0] as i32).abs() < 24,
        "circle must be symmetric in pixels: x={x_pt:?} y={y_pt:?}"
    );
}

// ---- HSL hue-wheel curves (Phase 6.8b) -----------------------------------

use vance_render::CurveSet;
use vance_timeline::{Keyframe, KeyframeInterp};

fn lkf(x: f64, y: f64) -> Keyframe {
    Keyframe {
        time_seconds: x,
        value: y,
        interp: KeyframeInterp::Linear,
    }
}
fn const_curve(y: f64) -> Vec<Keyframe> {
    // Same y for x=0 and x=1 -> a constant remap independent of input.
    vec![lkf(0.0, y), lkf(1.0, y)]
}

// hh = constant 1/3 -> every pixel's hue is remapped to green; a red
// frame should render GREEN-dominant.
#[test]
fn curves_hsl_hue_rotate_red_to_green() {
    let red = solid_yuv420p(64, 64, 63, 102, 240);
    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&red).expect("upload");
    r.set_grades(&[GradeNodeParams {
        enabled: true,
        mode: GradeMode::Curves,
        curves: Some(CurveSet {
            hh: const_curve(1.0 / 3.0),
            ..CurveSet::default()
        }),
        ..GradeNodeParams::default()
    }]);
    let g = center(&r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);
    assert!(
        g[1] > g[0] + 30 && g[1] > g[2] + 30,
        "expected green-dominant, got {g:?}"
    );
}

// hs = constant 0 -> hue-driven sat multiplier = 0 -> saturation killed.
// A red frame should render NEAR-GRAY (R ≈ G ≈ B).
#[test]
fn curves_hsl_sat_kill_makes_red_gray() {
    let red = solid_yuv420p(64, 64, 63, 102, 240);
    let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
    r.upload_frame(&red).expect("upload");
    r.set_grades(&[GradeNodeParams {
        enabled: true,
        mode: GradeMode::Curves,
        curves: Some(CurveSet {
            hs: const_curve(0.0),
            ..CurveSet::default()
        }),
        ..GradeNodeParams::default()
    }]);
    let g = center(&r.render_to_rgba_buffer(64, 64).expect("render"), 64, 64);
    let max = g[0].max(g[1]).max(g[2]);
    let min = g[0].min(g[1]).min(g[2]);
    assert!(
        (max as i32 - min as i32) < 20,
        "sat-kill should be near-gray (RGB equal), got {g:?}"
    );
}

// An ALL-EMPTY HSL CurveSet (the default) must be bit-for-bit identical
// to no HSL curves at all — the log_smh.y flag stays 0 and the shader's
// HSL block never runs. This is the regression gate that protects every
// pre-6.8b golden.
#[test]
fn curves_hsl_identity_is_bit_identical() {
    let frame = solid_yuv420p(64, 64, 128, 128, 128);
    let plain = render_center(&[], &frame);
    let with_empty_hsl = render_center(
        &[GradeNodeParams {
            enabled: true,
            mode: GradeMode::Curves,
            curves: Some(CurveSet::default()), // tone + HSL all empty
            ..GradeNodeParams::default()
        }],
        &frame,
    );
    assert_eq!(
        plain, with_empty_hsl,
        "all-empty curves node must be a passthrough"
    );
}
