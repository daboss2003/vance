//! Compositor layer/composite/output pass tests (ADR 0007): single layer vs
//! the legacy path, 50% white over black, picture-in-picture and crop reveal,
//! aspect fit, dissolve with mixed blend modes, RGBA readback/writeback round
//! trip, misuse errors, and an ignored perf check.
//!
//! Shared fixtures live here (`pub(crate)`) for `blend_modes` and `keyer`.

use std::sync::Arc;

use vance_decoder::{FrameData, PixelFormat};
use vance_render::{
    CompositeOp, Compositor, CurveSet, GradeMode, GradeNodeParams, KeyerParams, LayerKey,
    LayerParams, LayerSource, OutputSpec, Qualifier, RenderError, Renderer, RendererConfig,
    RgbaImage,
};
use vance_timeline::{Keyframe, KeyframeInterp};

// ---- fixtures -----------------------------------------------------------------

pub(crate) fn renderer() -> Renderer {
    Renderer::new(RendererConfig::default()).expect("renderer")
}

pub(crate) fn hdr_renderer() -> Renderer {
    Renderer::new(RendererConfig {
        target_format: wgpu::TextureFormat::Rgba16Float,
        ..RendererConfig::default()
    })
    .expect("hdr renderer")
}

/// A solid YUV420P frame (as `yuv_to_rgb.rs`).
pub(crate) fn solid_yuv420p(w: u32, h: u32, y: u8, u: u8, v: u8) -> FrameData {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut bytes = Vec::with_capacity(PixelFormat::Yuv420P.frame_size(w, h));
    bytes.extend(std::iter::repeat_n(y, (w * h) as usize));
    bytes.extend(std::iter::repeat_n(u, (cw * ch) as usize));
    bytes.extend(std::iter::repeat_n(v, (cw * ch) as usize));
    FrameData {
        pts: 0,
        width: w,
        height: h,
        pixel_format: PixelFormat::Yuv420P,
        strides: vec![w, cw, cw],
        bytes: bytes.into(),
        color: Default::default(),
    }
}

/// A YUV420P frame with horizontal luma ramps and chroma gradients, so a
/// parity check exercises sampling, not just one colour.
pub(crate) fn gradient_yuv420p(w: u32, h: u32) -> FrameData {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut b = Vec::new();
    for y in 0..h {
        for x in 0..w {
            b.push((16 + x * 219 / w) as u8 + (y % 7) as u8);
        }
    }
    for y in 0..ch {
        for x in 0..cw {
            b.push((40 + (x * 150 / cw) + y % 5) as u8);
        }
    }
    for y in 0..ch {
        for x in 0..cw {
            b.push((210 - (y * 150 / ch) - x % 5) as u8);
        }
    }
    FrameData {
        pts: 0,
        width: w,
        height: h,
        pixel_format: PixelFormat::Yuv420P,
        strides: vec![w, cw, cw],
        bytes: b.into(),
        color: Default::default(),
    }
}

fn to_nv12(f: &FrameData) -> FrameData {
    let (cw, ch) = (f.width.div_ceil(2), f.height.div_ceil(2));
    let (ys, cs) = ((f.width * f.height) as usize, (cw * ch) as usize);
    let mut b = f.bytes[..ys].to_vec();
    for i in 0..cs {
        b.push(f.bytes[ys + i]);
        b.push(f.bytes[ys + cs + i]);
    }
    FrameData {
        pixel_format: PixelFormat::Nv12,
        strides: vec![f.width, cw * 2],
        bytes: b.into(),
        ..f.clone()
    }
}

/// The 8-bit frame as 10-bit (codes × 4), optionally lifted toward PQ
/// highlights.
pub(crate) fn to_10bit(f: &FrameData, lift: u16) -> FrameData {
    let b: Vec<u8> = f
        .bytes
        .iter()
        .flat_map(|&v| {
            (((v as u16) << 2).saturating_add(lift))
                .min(1019)
                .to_le_bytes()
        })
        .collect();
    FrameData {
        pixel_format: PixelFormat::Yuv420P10Le,
        bytes: b.into(),
        ..f.clone()
    }
}

/// A solid straight-alpha RGBA image.
pub(crate) fn solid_rgba(w: u32, h: u32, px: [u8; 4]) -> RgbaImage {
    RgbaImage {
        width: w,
        height: h,
        pixels: px.repeat((w * h) as usize),
    }
}

/// Rec.709 limited → linear Rec.709 → sRGB (the default project pipeline).
pub(crate) fn default_pipeline() -> vance_color::PipelineParams {
    vance_color::Pipeline::default().to_params().unwrap()
}

/// sRGB-encoded RGBA sources: sRGB in → linear Rec.709 working → sRGB out.
pub(crate) fn srgb_pipeline() -> vance_color::PipelineParams {
    vance_color::Pipeline {
        input: vance_color::ColorSpace::Srgb,
        working: vance_color::ColorSpace::LinearRec709,
        output: vance_color::ColorSpace::Srgb,
    }
    .to_params()
    .unwrap()
}

/// Identity layer params for `pipeline`.
pub(crate) fn params(pipeline: vance_color::PipelineParams) -> LayerParams {
    LayerParams {
        pipeline,
        translate: [0.0; 2],
        scale: [1.0; 2],
        rotation: 0.0,
        crop_lrtb: [0.0; 4],
        brightness: 0.0,
        contrast: 1.0,
        chroma_key: None,
        luma_key: None,
        keyer: None,
        grades: Vec::new(),
        lut: None,
        matte_view: false,
        pixel_aspect: 1.0,
    }
}

/// SDR (display-encoded blending) output at `w×h`.
pub(crate) fn sdr_spec(w: u32, h: u32) -> OutputSpec {
    OutputSpec {
        width: w,
        height: h,
        working_to_output: vance_color::Transform::IDENTITY,
        output_transfer: 1,
        hdr: false,
        view_lut: false,
        transparent: false,
        output_primaries: vance_color::Primaries::Bt709,
    }
}

/// HDR (linear blending) output for a resolved pipeline.
pub(crate) fn hdr_spec(w: u32, h: u32, p: &vance_color::PipelineParams) -> OutputSpec {
    OutputSpec {
        width: w,
        height: h,
        working_to_output: p.working_to_output,
        output_transfer: p.output_transfer,
        hdr: true,
        view_lut: false,
        transparent: false,
        // The HDR test pipelines all output Rec.2020 PQ / HLG.
        output_primaries: vance_color::Primaries::Bt2020,
    }
}

pub(crate) const NORMAL: CompositeOp = CompositeOp {
    blend_mode: 0,
    opacity: 1.0,
};

pub(crate) fn op(blend_mode: u32, opacity: f32) -> CompositeOp {
    CompositeOp {
        blend_mode,
        opacity,
    }
}

pub(crate) fn px(buf: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
}

/// Largest per-channel difference between two equal-length byte buffers.
fn max_diff_u8(a: &[u8], b: &[u8]) -> i32 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(x, y)| (*x as i32 - *y as i32).abs())
        .max()
        .unwrap_or(0)
}

fn max_diff_u16(a: &[u8], b: &[u8]) -> i32 {
    assert_eq!(a.len(), b.len());
    a.chunks_exact(2)
        .zip(b.chunks_exact(2))
        .map(|(x, y)| {
            (u16::from_le_bytes([x[0], x[1]]) as i32 - u16::from_le_bytes([y[0], y[1]]) as i32)
                .abs()
        })
        .max()
        .unwrap_or(0)
}

/// One layer, Normal at opacity 1, finished to RGBA8.
pub(crate) fn single_layer_rgba8(
    c: &mut Compositor,
    spec: &OutputSpec,
    src: LayerSource<'_>,
    p: &LayerParams,
) -> Vec<u8> {
    c.begin_frame(spec).unwrap();
    let h = c.render_layer(LayerKey(1), src, p).unwrap();
    c.composite(h, NORMAL).unwrap();
    c.finish_to_rgba8().unwrap()
}

fn kf(x: f64, y: f64) -> Keyframe {
    Keyframe {
        time_seconds: x,
        value: y,
        interp: KeyframeInterp::Linear,
    }
}

/// A primary, a qualified log node and a curves node (tone + HSL).
fn sample_grades() -> Vec<GradeNodeParams> {
    vec![
        GradeNodeParams {
            gain: [1.2, 0.9, 1.0, 1.1],
            lift: [0.01, 0.0, -0.01, 0.0],
            gamma: [1.0, 1.1, 0.9, 1.0],
            ..GradeNodeParams::default()
        },
        GradeNodeParams {
            mode: GradeMode::Log,
            offset: [0.02, 0.0, 0.01],
            midtone: [0.0, 0.02, 0.0],
            qual: Qualifier {
                enabled: true,
                hue: [0.1, 0.4, 0.05],
                ..Qualifier::default()
            },
            ..GradeNodeParams::default()
        },
        GradeNodeParams {
            mode: GradeMode::Curves,
            curves: Some(CurveSet {
                master: vec![kf(0.0, 0.1), kf(0.5, 0.6), kf(1.0, 0.95)],
                hs: vec![kf(0.0, 0.4), kf(1.0, 0.6)],
                ..CurveSet::default()
            }),
            ..GradeNodeParams::default()
        },
    ]
}

fn tinted_lut() -> vance_color::Lut3d {
    let mut l = vance_color::Lut3d::identity(9);
    for (i, v) in l.values.iter_mut().enumerate() {
        if i % 3 == 0 {
            *v = (*v * 0.8 + 0.1).min(1.0);
        } else if i % 3 == 2 {
            *v *= *v;
        }
    }
    l
}

// ---- single layer == legacy path --------------------------------------------

/// Legacy `Renderer` setup mirroring `p` (no transform, keys or opacity —
/// those intentionally differ: transparent bars, alpha, display-space mix).
fn legacy_rgba8(
    frame: &FrameData,
    p: &LayerParams,
    view_lut: Option<&vance_color::Lut3d>,
) -> Vec<u8> {
    let mut r = renderer();
    r.set_pipeline(p.pipeline);
    r.upload_frame(frame).unwrap();
    r.set_grades(&p.grades);
    r.set_brightness_contrast(p.brightness, p.contrast);
    if let Some(l) = &p.lut {
        r.upload_lut(l);
        r.set_lut_enable(1.0);
    }
    if let Some(l) = view_lut {
        r.upload_view_lut(l);
        r.set_view_lut_enable(1.0);
    }
    r.render_to_rgba_buffer(frame.width, frame.height).unwrap()
}

fn compositor_rgba8(
    frame: &FrameData,
    p: &LayerParams,
    view_lut: Option<&vance_color::Lut3d>,
) -> Vec<u8> {
    let mut r = renderer();
    if let Some(l) = view_lut {
        r.upload_view_lut(l);
        r.set_view_lut_enable(1.0);
    }
    let spec = OutputSpec {
        working_to_output: p.pipeline.working_to_output,
        output_transfer: p.pipeline.output_transfer,
        view_lut: view_lut.is_some(),
        ..sdr_spec(frame.width, frame.height)
    };
    single_layer_rgba8(r.compositor(), &spec, LayerSource::Yuv(frame), p)
}

#[test]
fn single_layer_matches_legacy_8bit() {
    let frame = gradient_yuv420p(66, 46);
    let plain = params(default_pipeline());
    let graded = LayerParams {
        grades: sample_grades(),
        brightness: 0.03,
        contrast: 1.2,
        lut: Some(Arc::new(tinted_lut())),
        ..params(default_pipeline())
    };
    for (label, f, p, view) in [
        ("yuv420p", frame.clone(), &plain, None),
        ("nv12", to_nv12(&frame), &plain, None),
        ("graded+lut", frame.clone(), &graded, None),
        ("view lut", frame.clone(), &plain, Some(tinted_lut())),
    ] {
        let legacy = legacy_rgba8(&f, p, view.as_ref());
        let comp = compositor_rgba8(&f, p, view.as_ref());
        let d = max_diff_u8(&legacy, &comp);
        eprintln!("8-bit {label}: max diff {d}");
        assert!(d <= 1, "{label}: compositor vs legacy max diff {d}");
    }
}

#[test]
fn single_layer_matches_legacy_10bit() {
    let frame = to_10bit(&gradient_yuv420p(66, 46), 0);
    let p = LayerParams {
        grades: sample_grades(),
        ..params(default_pipeline())
    };
    let legacy = legacy_rgba8(&frame, &p, None);
    let comp = compositor_rgba8(&frame, &p, None);
    let d = max_diff_u8(&legacy, &comp);
    eprintln!("10-bit: max diff {d}");
    assert!(d <= 1, "10-bit compositor vs legacy max diff {d}");
}

#[test]
fn single_layer_matches_legacy_pq_to_sdr() {
    let frame = to_10bit(&gradient_yuv420p(66, 46), 300);
    let pipeline = vance_color::Pipeline {
        input: vance_color::ColorSpace::Rec2020Pq,
        working: vance_color::ColorSpace::LinearRec2020,
        output: vance_color::ColorSpace::Srgb,
    }
    .to_params()
    .unwrap();
    let p = LayerParams {
        grades: sample_grades()[..1].to_vec(),
        ..params(pipeline)
    };
    let legacy = legacy_rgba8(&frame, &p, None);
    let comp = compositor_rgba8(&frame, &p, None);
    let d = max_diff_u8(&legacy, &comp);
    eprintln!("PQ→SDR (tone-mapped): max diff {d}");
    assert!(d <= 1, "PQ→SDR compositor vs legacy max diff {d}");
}

#[test]
fn single_layer_matches_legacy_pq_to_hdr() {
    let (w, h) = (66, 46);
    for (label, input, lift, output) in [
        (
            "PQ→PQ",
            vance_color::ColorSpace::Rec2020Pq,
            300,
            vance_color::ColorSpace::Rec2020Pq,
        ),
        (
            "709→PQ",
            vance_color::ColorSpace::Rec709Limited,
            0,
            vance_color::ColorSpace::Rec2020Pq,
        ),
        (
            "PQ→HLG",
            vance_color::ColorSpace::Rec2020Pq,
            300,
            vance_color::ColorSpace::Rec2020Hlg,
        ),
    ] {
        let pipeline = vance_color::Pipeline {
            input,
            working: vance_color::ColorSpace::LinearRec709,
            output,
        }
        .to_params()
        .unwrap();
        let frame = to_10bit(&gradient_yuv420p(w, h), lift);
        let p = LayerParams {
            contrast: 1.1,
            ..params(pipeline)
        };
        let mut legacy = hdr_renderer();
        legacy.set_pipeline(pipeline);
        legacy.upload_frame(&frame).unwrap();
        legacy.set_brightness_contrast(p.brightness, p.contrast);
        let want = legacy.render_to_rgba64_buffer(w, h).unwrap();

        let mut r = renderer();
        let c = r.compositor();
        c.begin_frame(&hdr_spec(w, h, &pipeline)).unwrap();
        let l = c
            .render_layer(LayerKey(1), LayerSource::Yuv(&frame), &p)
            .unwrap();
        c.composite(l, NORMAL).unwrap();
        let got = c.finish_to_rgba64().unwrap();
        let d = max_diff_u16(&want, &got);
        eprintln!("{label}: max diff {d}/65535");
        // One 8-bit step (257/65535); the extra linear f16 intermediate
        // costs a few dozen 16-bit codes at most.
        assert!(d <= 257, "{label}: compositor vs legacy max diff {d}");
    }
}

#[test]
fn luts_and_curves_can_load_before_the_first_frame() {
    // Regression: these used to unwrap the not-yet-allocated plane textures.
    let mut r = renderer();
    r.upload_view_lut(&tinted_lut());
    r.set_view_lut_enable(1.0);
    r.upload_lut(&tinted_lut());
    r.set_grades(&sample_grades());
    assert!(matches!(
        r.render_to_rgba_buffer(8, 8),
        Err(RenderError::NoFrame)
    ));
    r.upload_frame(&solid_yuv420p(8, 8, 128, 128, 128)).unwrap();
    r.render_to_rgba_buffer(8, 8).unwrap();
}

// ---- frame-level behaviour -------------------------------------------------

#[test]
fn empty_frame_is_black() {
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(8, 6)).unwrap();
    let buf = c.finish_to_rgba8().unwrap();
    assert!(
        buf.chunks_exact(4).all(|p| p == [0, 0, 0, 255]),
        "{:?}",
        &buf[..8]
    );
    // HDR: black is PQ code 0.
    let p = vance_color::Pipeline {
        input: vance_color::ColorSpace::Rec709Limited,
        working: vance_color::ColorSpace::LinearRec709,
        output: vance_color::ColorSpace::Rec2020Pq,
    }
    .to_params()
    .unwrap();
    c.begin_frame(&hdr_spec(8, 6, &p)).unwrap();
    let buf = c.finish_to_rgba64().unwrap();
    for px in buf.chunks_exact(8) {
        let ch = |i: usize| u16::from_le_bytes([px[i * 2], px[i * 2 + 1]]);
        assert_eq!((ch(0), ch(1), ch(2), ch(3)), (0, 0, 0, 65535));
    }
}

/// White (YUV 235) at `opacity` over black (YUV 16) through the default
/// pipeline; returns the centre pixel.
fn white_over_black(opacity: f32) -> [u8; 4] {
    let (w, h) = (16, 16);
    let black = solid_yuv420p(w, h, 16, 128, 128);
    let white = solid_yuv420p(w, h, 235, 128, 128);
    let p = params(default_pipeline());
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(w, h)).unwrap();
    let lb = c
        .render_layer(LayerKey(1), LayerSource::Yuv(&black), &p)
        .unwrap();
    c.composite(lb, NORMAL).unwrap();
    let lw = c
        .render_layer(LayerKey(2), LayerSource::Yuv(&white), &p)
        .unwrap();
    c.composite(lw, op(0, opacity)).unwrap();
    px(&c.finish_to_rgba8().unwrap(), w, 8, 8)
}

#[test]
fn half_opacity_white_over_black_is_the_display_midpoint() {
    // Display-space blend: 0.5·1.0 + 0.5·0.0 = 0.5 encoded = 127.5/255. The
    // exact value is a rounding tie, so the GPU's unorm conversion may land
    // on either neighbour. Linear-light blending would give
    // sRGB(0.5) = 0.7354 → 188, which this must NOT be.
    let c = white_over_black(0.5);
    for ch in &c[..3] {
        assert!(
            (127..=128).contains(ch),
            "50% white over black = {c:?}, want 127.5"
        );
    }
    assert_eq!(c[3], 255);
    // Off the tie: 40% → 0.4·255 = 102.0 exactly (linear light: 170).
    let c = white_over_black(0.4);
    assert_eq!(&c[..3], &[102, 102, 102], "40% white over black");
}

#[test]
fn pip_reveals_the_lower_layer() {
    let (w, h) = (32, 32);
    let red = solid_rgba(w, h, [255, 0, 0, 255]);
    let blue = solid_rgba(w, h, [0, 0, 255, 255]);
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(w, h)).unwrap();
    let lo = c
        .render_layer(
            LayerKey(1),
            LayerSource::Rgba(&red),
            &params(srgb_pipeline()),
        )
        .unwrap();
    c.composite(lo, NORMAL).unwrap();
    let pip = LayerParams {
        scale: [0.5, 0.5],
        translate: [0.25, 0.25],
        ..params(srgb_pipeline())
    };
    let hi = c
        .render_layer(LayerKey(2), LayerSource::Rgba(&blue), &pip)
        .unwrap();
    c.composite(hi, NORMAL).unwrap();
    let buf = c.finish_to_rgba8().unwrap();
    // Half-size inset in the bottom-right quadrant.
    assert_eq!(px(&buf, w, 24, 24), [0, 0, 255, 255], "inside the PiP");
    assert_eq!(px(&buf, w, 4, 4), [255, 0, 0, 255], "lower layer around it");
    assert_eq!(px(&buf, w, 24, 4), [255, 0, 0, 255]);
    assert_eq!(
        px(&buf, w, 17, 24),
        [0, 0, 255, 255],
        "PiP edge (x = 16..32)"
    );
    assert_eq!(px(&buf, w, 15, 24), [255, 0, 0, 255]);
}

#[test]
fn crop_reveals_the_lower_layer() {
    let (w, h) = (32, 16);
    let red = solid_yuv420p(w, h, 63, 102, 240);
    let white = solid_yuv420p(w, h, 235, 128, 128);
    let p = params(default_pipeline());
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(w, h)).unwrap();
    let lo = c
        .render_layer(LayerKey(1), LayerSource::Yuv(&red), &p)
        .unwrap();
    c.composite(lo, NORMAL).unwrap();
    let cropped = LayerParams {
        crop_lrtb: [0.5, 0.0, 0.0, 0.25],
        ..p.clone()
    };
    let hi = c
        .render_layer(LayerKey(2), LayerSource::Yuv(&white), &cropped)
        .unwrap();
    c.composite(hi, NORMAL).unwrap();
    let buf = c.finish_to_rgba8().unwrap();
    let red_px = px(&buf, w, 4, 4);
    assert!(
        red_px[0] > 200 && red_px[1] < 60,
        "cropped-away left half shows red: {red_px:?}"
    );
    let bottom = px(&buf, w, 24, 14);
    assert!(
        bottom[0] > 200 && bottom[1] < 60,
        "cropped bottom shows red: {bottom:?}"
    );
    let kept = px(&buf, w, 24, 4);
    assert!(
        kept.iter().take(3).all(|&v| v > 250),
        "kept region is white: {kept:?}"
    );
}

#[test]
fn tall_source_is_fitted_with_transparent_bars() {
    // 9:16 source in a 16:9 canvas: full height, 36·(36/64) = 20.25 px wide,
    // centred → columns ~21.9..42.1 covered; the rest shows the lower layer.
    let (w, h) = (64, 36);
    let red = solid_rgba(w, h, [255, 0, 0, 255]);
    let tall = solid_rgba(36, 64, [0, 0, 255, 255]);
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(w, h)).unwrap();
    let lo = c
        .render_layer(
            LayerKey(1),
            LayerSource::Rgba(&red),
            &params(srgb_pipeline()),
        )
        .unwrap();
    c.composite(lo, NORMAL).unwrap();
    let hi = c
        .render_layer(
            LayerKey(2),
            LayerSource::Rgba(&tall),
            &params(srgb_pipeline()),
        )
        .unwrap();
    c.composite(hi, NORMAL).unwrap();
    let buf = c.finish_to_rgba8().unwrap();
    for y in [0, 18, 35] {
        assert_eq!(
            px(&buf, w, 32, y),
            [0, 0, 255, 255],
            "centre column, row {y}"
        );
        assert_eq!(px(&buf, w, 23, y), [0, 0, 255, 255]);
        assert_eq!(px(&buf, w, 41, y), [0, 0, 255, 255]);
        assert_eq!(px(&buf, w, 20, y), [255, 0, 0, 255], "left bar, row {y}");
        assert_eq!(px(&buf, w, 43, y), [255, 0, 0, 255], "right bar, row {y}");
        assert_eq!(px(&buf, w, 0, y), [255, 0, 0, 255]);
    }
    // The bar edges are anti-aliased: column 21 is mostly red, 42 mostly
    // blue-ish mix (coverage ≈ 0.1 / 0.6).
    let edge = px(&buf, w, 21, 18);
    assert!(edge[0] > 0 && edge[2] > 0, "AA edge mixes both: {edge:?}");
}

#[test]
fn dissolve_mixes_independently_blended_sides() {
    // Backdrop grey; A = Screen (orange), B = Normal (teal @ 0.6).
    let (w, h) = (8, 8);
    let bd = [100u8, 100, 100, 255];
    let a = [230u8, 120, 40, 255];
    let b = [20u8, 160, 170, 255];
    let mut r = renderer();
    let c = r.compositor();
    for mix in [0.0f32, 0.5, 1.0] {
        c.begin_frame(&sdr_spec(w, h)).unwrap();
        let p = params(srgb_pipeline());
        let lb = c
            .render_layer(LayerKey(1), LayerSource::Rgba(&solid_rgba(w, h, bd)), &p)
            .unwrap();
        c.composite(lb, NORMAL).unwrap();
        let la = c
            .render_layer(LayerKey(2), LayerSource::Rgba(&solid_rgba(w, h, a)), &p)
            .unwrap();
        let lbb = c
            .render_layer(LayerKey(3), LayerSource::Rgba(&solid_rgba(w, h, b)), &p)
            .unwrap();
        c.composite_dissolve((la, op(3, 1.0)), (lbb, op(0, 0.6)), mix)
            .unwrap();
        let got = px(&c.finish_to_rgba8().unwrap(), w, 4, 4);
        let bdf = crate::blend_modes::straight(bd);
        let side_a = crate::blend_modes::blend_ref(3, bdf, crate::blend_modes::straight(a), 1.0);
        let side_b = crate::blend_modes::blend_ref(0, bdf, crate::blend_modes::straight(b), 0.6);
        for ch in 0..3 {
            let want = side_a[ch] + (side_b[ch] - side_a[ch]) * mix as f64;
            let want = (want.clamp(0.0, 1.0) * 255.0).round() as i32;
            assert!(
                (got[ch] as i32 - want).abs() <= 1,
                "mix {mix} ch{ch}: got {} want {want}",
                got[ch]
            );
        }
    }
}

/// Three layers (keyed + graded YUV, PiP RGBA) with Normal / Screen /
/// Overlay; `materialize` forces each layer into its slot first (a readback),
/// so the composite runs the separate blend pass instead of the fused one.
fn three_layer_frame(c: &mut Compositor, spec: &OutputSpec, materialize: bool) -> Vec<u8> {
    let (w, h) = (spec.width, spec.height);
    let base = gradient_yuv420p(w, h);
    let screen = solid_yuv420p(w, h, 150, 60, 50);
    let title = solid_rgba(w / 2, h / 2, [240, 200, 40, 180]);
    let keyed = LayerParams {
        keyer: Some(KeyerParams {
            key_rgb: [0.0, 0.8, 0.0],
            balance: 0.5,
            strength: 1.0,
            clip_black: 0.0,
            clip_white: 1.0,
            choke_px: 1.0,
            soften_px: 1.5,
            spill: 1.0,
            spill_bias: 0.5,
            invert: false,
        }),
        grades: sample_grades(),
        ..params(default_pipeline())
    };
    let pip = LayerParams {
        scale: [0.6, 0.6],
        rotation: 0.3,
        translate: [-0.1, 0.05],
        ..params(srgb_pipeline())
    };
    c.begin_frame(spec).unwrap();
    let layers = [
        (
            LayerSource::Yuv(&base),
            params(default_pipeline()),
            op(0, 1.0),
        ),
        (LayerSource::Yuv(&screen), keyed, op(3, 0.7)),
        (LayerSource::Rgba(&title), pip, op(4, 0.9)),
    ];
    for (i, (src, p, o)) in layers.iter().enumerate() {
        let l = c.render_layer(LayerKey(i as u128), *src, p).unwrap();
        if materialize {
            c.read_layer_rgba8(l).unwrap();
        }
        c.composite(l, *o).unwrap();
    }
    c.finish_to_rgba8().unwrap()
}

#[test]
fn fused_and_materialized_layers_composite_identically() {
    let spec = sdr_spec(48, 32);
    let mut r = renderer();
    let fused = three_layer_frame(r.compositor(), &spec, false);
    let separate = three_layer_frame(r.compositor(), &spec, true);
    assert_eq!(
        fused, separate,
        "fused layer-over pass must equal slot + blend pass"
    );
    // HDR too (linear blend space).
    let p = vance_color::Pipeline {
        input: vance_color::ColorSpace::Rec709Limited,
        working: vance_color::ColorSpace::LinearRec709,
        output: vance_color::ColorSpace::Rec2020Pq,
    }
    .to_params()
    .unwrap();
    let hdr = hdr_spec(48, 32, &p);
    let c = r.compositor();
    let run = |c: &mut Compositor, materialize: bool| {
        three_layer_frame(c, &hdr, materialize);
        c.begin_frame(&hdr).unwrap();
        let f = gradient_yuv420p(48, 32);
        let l = c
            .render_layer(LayerKey(9), LayerSource::Yuv(&f), &params(p))
            .unwrap();
        if materialize {
            c.read_layer_rgba8(l).unwrap();
        }
        c.composite(l, op(1, 0.8)).unwrap();
        c.finish_to_rgba64().unwrap()
    };
    assert_eq!(run(c, false), run(c, true));
}

#[test]
fn dissolve_as_the_first_composite_sees_a_clear_backdrop() {
    // A dissolve onto the empty backdrop equals mixing the two layers over
    // transparent black (every mode reduces to the source there).
    let (w, h) = (8, 8);
    let a = [200u8, 40, 90, 255];
    let b = [30u8, 180, 220, 128];
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(w, h)).unwrap();
    let p = params(srgb_pipeline());
    let la = c
        .render_layer(LayerKey(1), LayerSource::Rgba(&solid_rgba(w, h, a)), &p)
        .unwrap();
    let lb = c
        .render_layer(LayerKey(2), LayerSource::Rgba(&solid_rgba(w, h, b)), &p)
        .unwrap();
    c.composite_dissolve((la, op(7, 1.0)), (lb, op(2, 1.0)), 0.25)
        .unwrap();
    let got = px(&c.finish_to_rgba8().unwrap(), w, 3, 3);
    let (fa, fb) = (
        crate::blend_modes::straight(a),
        crate::blend_modes::straight(b),
    );
    for ch in 0..3 {
        let want = fa[ch] * fa[3] * 0.75 + fb[ch] * fb[3] * 0.25;
        let want = (want * 255.0).round() as i32;
        assert!(
            (got[ch] as i32 - want).abs() <= 1,
            "ch{ch}: {} vs {want}",
            got[ch]
        );
    }
}

#[test]
fn rgba8_read_write_round_trip_is_identity() {
    let (w, h) = (16, 8);
    let mut img = RgbaImage {
        width: w,
        height: h,
        pixels: Vec::new(),
    };
    for i in 0..(w * h) {
        img.pixels.extend_from_slice(&[
            (i * 7 % 256) as u8,
            (255 - i * 3 % 256) as u8,
            (i * 13 % 256) as u8,
            (16 + i * 11 % 240) as u8,
        ]);
    }
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(w, h)).unwrap();
    let l = c
        .render_layer(
            LayerKey(1),
            LayerSource::Rgba(&img),
            &params(srgb_pipeline()),
        )
        .unwrap();
    let first = c.read_layer_rgba8(l).unwrap();
    assert_eq!((first.width, first.height), (w, h));
    assert!(
        max_diff_u8(&first.pixels, &img.pixels) <= 1,
        "layer readback vs source: {}",
        max_diff_u8(&first.pixels, &img.pixels)
    );
    c.write_layer_rgba8(l, &first).unwrap();
    let second = c.read_layer_rgba8(l).unwrap();
    let d = max_diff_u8(&first.pixels, &second.pixels);
    assert!(d <= 1, "read → write → read max diff {d}");
    for (a, b) in first
        .pixels
        .chunks_exact(4)
        .zip(second.pixels.chunks_exact(4))
    {
        assert!(
            (a[3] as i32 - b[3] as i32).abs() <= 1,
            "alpha kept: {a:?} vs {b:?}"
        );
    }
    // The written layer composites like the original.
    c.composite(l, NORMAL).unwrap();
    let out = c.finish_to_rgba8().unwrap();
    let p = px(&out, w, 3, 2);
    let s = &first.pixels[((2 * w + 3) * 4) as usize..][..4];
    let want = (s[0] as f32 * s[3] as f32 / 255.0).round() as i32;
    assert!(
        (p[0] as i32 - want).abs() <= 1,
        "written layer over black: {p:?} vs src {s:?}"
    );
}

#[test]
fn hdr_layer_round_trip_keeps_sdr_range_values() {
    let p = vance_color::Pipeline {
        input: vance_color::ColorSpace::Srgb,
        working: vance_color::ColorSpace::LinearRec709,
        output: vance_color::ColorSpace::Rec2020Pq,
    }
    .to_params()
    .unwrap();
    let img = solid_rgba(8, 8, [200, 90, 30, 180]);
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&hdr_spec(8, 8, &p)).unwrap();
    let l = c
        .render_layer(LayerKey(1), LayerSource::Rgba(&img), &params(p))
        .unwrap();
    let back = c.read_layer_rgba8(l).unwrap();
    assert!(
        max_diff_u8(&back.pixels, &img.pixels) <= 1,
        "{:?}",
        &back.pixels[..4]
    );
    c.write_layer_rgba8(l, &back).unwrap();
    let again = c.read_layer_rgba8(l).unwrap();
    assert!(max_diff_u8(&again.pixels, &img.pixels) <= 1);
    c.finish_to_rgba64().unwrap();
}

#[test]
fn misuse_is_an_error_not_a_panic() {
    let mut r = renderer();
    let c = r.compositor();
    let img = solid_rgba(4, 4, [1, 2, 3, 255]);
    let p = params(srgb_pipeline());
    assert!(matches!(
        c.render_layer(LayerKey(1), LayerSource::Rgba(&img), &p),
        Err(RenderError::Compositor(_))
    ));
    assert!(c.finish_to_rgba8().is_err(), "finish without begin_frame");
    c.begin_frame(&sdr_spec(4, 4)).unwrap();
    let bad = RgbaImage {
        pixels: vec![0; 7],
        ..img.clone()
    };
    assert!(matches!(
        c.render_layer(LayerKey(1), LayerSource::Rgba(&bad), &p),
        Err(RenderError::MalformedFrame { .. })
    ));
    let h = c
        .render_layer(LayerKey(1), LayerSource::Rgba(&img), &p)
        .unwrap();
    assert!(
        c.write_layer_rgba8(h, &solid_rgba(2, 2, [0; 4])).is_err(),
        "wrong size"
    );
    // Re-rendering the same key invalidates the earlier handle.
    let h2 = c
        .render_layer(LayerKey(1), LayerSource::Rgba(&img), &p)
        .unwrap();
    assert!(c.composite(h, NORMAL).is_err(), "stale handle");
    c.composite(h2, NORMAL).unwrap();
    c.finish_to_rgba8().unwrap();
    // Handles die with their frame, even when the new frame has a layer at
    // the same index.
    c.begin_frame(&sdr_spec(4, 4)).unwrap();
    c.render_layer(LayerKey(3), LayerSource::Rgba(&img), &p)
        .unwrap();
    c.render_layer(LayerKey(4), LayerSource::Rgba(&img), &p)
        .unwrap();
    assert!(
        c.composite(h2, NORMAL).is_err(),
        "handle from a previous frame"
    );
    c.retain_only(&[]);
    assert!(c.finish_to_rgba8().is_ok());
}

#[test]
fn finish_to_view_renders_into_a_surface_format() {
    let (w, h) = (8, 8);
    let mut r = renderer();
    let target = r.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("test-surface"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let c = r.compositor();
    c.begin_frame(&sdr_spec(w, h)).unwrap();
    let img = solid_rgba(w, h, [10, 200, 30, 255]);
    let l = c
        .render_layer(
            LayerKey(1),
            LayerSource::Rgba(&img),
            &params(srgb_pipeline()),
        )
        .unwrap();
    c.composite(l, NORMAL).unwrap();
    c.finish_to_view(&view, wgpu::TextureFormat::Bgra8UnormSrgb)
        .unwrap();
}

#[test]
fn slots_are_reused_across_frames_and_sizes() {
    let mut r = renderer();
    let c = r.compositor();
    let p = params(default_pipeline());
    for (cw, chh, fw, fh) in [
        (32, 18, 32, 18),
        (32, 18, 16, 16),
        (20, 20, 16, 16),
        (20, 20, 40, 10),
    ] {
        c.begin_frame(&sdr_spec(cw, chh)).unwrap();
        let f = solid_yuv420p(fw, fh, 180, 90, 160);
        let l = c
            .render_layer(LayerKey(7), LayerSource::Yuv(&f), &p)
            .unwrap();
        c.composite(l, NORMAL).unwrap();
        let buf = c.finish_to_rgba8().unwrap();
        assert_eq!(buf.len(), (cw * chh * 4) as usize);
        assert!(px(&buf, cw, cw / 2, chh / 2)[0] > 0);
    }
    c.retain_only(&[LayerKey(7)]);
}

// ---- YUV matrix and range (B1) ----------------------------------------------

/// Encoded Y'CbCr (already range-expanded) → clamped R'G'B' with `matrix`.
fn ycbcr_to_rgb(y: f32, u: f32, v: f32, matrix: u32) -> [f32; 3] {
    let (kr, kb) = match matrix {
        1 => (0.299f32, 0.114f32),
        2 => (0.2627, 0.0593),
        _ => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    [
        y + 2.0 * (1.0 - kr) * v,
        y - 2.0 * kb * (1.0 - kb) / kg * u - 2.0 * kr * (1.0 - kr) / kg * v,
        y + 2.0 * (1.0 - kb) * u,
    ]
    .map(|c| c.clamp(0.0, 1.0))
}

/// R'G'B' (Rec.709 transfer) → the sRGB byte the default pipeline shows.
fn rec709_enc_to_srgb_byte(c: f32) -> u8 {
    let lin = if c < 0.081 {
        c / 4.5
    } else {
        ((c + 0.099) / 1.099).powf(1.0 / 0.45)
    };
    let s = if lin <= 0.0031308 {
        12.92 * lin
    } else {
        1.055 * lin.powf(1.0 / 2.4) - 0.055
    };
    (s.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Centre pixel of one solid frame through the legacy renderer and the
/// compositor.
fn centre_both(frame: &FrameData, pipeline: vance_color::PipelineParams) -> ([u8; 4], [u8; 4]) {
    let (w, h) = (frame.width, frame.height);
    let p = params(pipeline);
    let legacy = legacy_rgba8(frame, &p, None);
    let comp = compositor_rgba8(frame, &p, None);
    (px(&legacy, w, w / 2, h / 2), px(&comp, w, w / 2, h / 2))
}

#[test]
fn bt601_and_bt2020_frames_decode_with_their_own_matrix() {
    let (y, u, v) = (110u8, 80u8, 200u8);
    let frame = solid_yuv420p(16, 16, y, u, v);
    let (yf, uf, vf) = (
        (y as f32 - 16.0) / 219.0,
        (u as f32 - 128.0) / 224.0,
        (v as f32 - 128.0) / 224.0,
    );
    let mut seen = Vec::new();
    for (matrix, label) in [(0u32, "BT.709"), (1, "BT.601"), (2, "BT.2020")] {
        let pipeline = vance_color::PipelineParams {
            input_matrix: matrix,
            ..default_pipeline()
        };
        let want = ycbcr_to_rgb(yf, uf, vf, matrix).map(rec709_enc_to_srgb_byte);
        let (legacy, comp) = centre_both(&frame, pipeline);
        for (path, got) in [("legacy", legacy), ("compositor", comp)] {
            for ch in 0..3 {
                assert!(
                    (got[ch] as i32 - want[ch] as i32).abs() <= 2,
                    "{label} {path} ch{ch}: got {got:?} want {want:?}"
                );
            }
        }
        seen.push(comp);
    }
    // The three matrices give visibly different colours for this sample.
    let dist = |a: [u8; 4], b: [u8; 4]| (0..3).map(|c| (a[c] as i32 - b[c] as i32).abs()).max();
    assert!(dist(seen[0], seen[1]).unwrap() > 8, "601 vs 709: {seen:?}");
    assert!(dist(seen[0], seen[2]).unwrap() > 4, "2020 vs 709: {seen:?}");
}

/// A solid 10-bit frame (codes, not shifted).
fn solid_yuv420p10(w: u32, h: u32, y: u16, u: u16, v: u16) -> FrameData {
    let mut f = solid_yuv420p(w, h, 0, 0, 0);
    let (luma, chroma) = ((w * h) as usize, (w.div_ceil(2) * h.div_ceil(2)) as usize);
    let mut b = Vec::with_capacity((luma + 2 * chroma) * 2);
    for (n, code) in [(luma, y), (chroma, u), (chroma, v)] {
        for _ in 0..n {
            b.extend_from_slice(&code.to_le_bytes());
        }
    }
    f.pixel_format = PixelFormat::Yuv420P10Le;
    f.bytes = b.into();
    f
}

#[test]
fn ten_bit_limited_range_expands_exactly() {
    // 10-bit limited: Y' = (code − 64) / 876, C = (code − 512) / 896.
    for (y10, label) in [(940u16, "white"), (64, "black"), (502, "half")] {
        let frame = solid_yuv420p10(16, 16, y10, 512, 512);
        let want = rec709_enc_to_srgb_byte((y10 as f32 - 64.0) / 876.0);
        let (legacy, comp) = centre_both(&frame, default_pipeline());
        for (path, got) in [("legacy", legacy), ("compositor", comp)] {
            for ch in 0..3 {
                assert!(
                    (got[ch] as i32 - want as i32).abs() <= 1,
                    "10-bit {label} {path}: got {got:?} want {want}"
                );
            }
        }
        if y10 == 940 {
            assert_eq!(
                &comp[..3],
                &[255, 255, 255],
                "10-bit white is exactly white"
            );
            assert_eq!(&legacy[..3], &[255, 255, 255], "legacy 10-bit white");
        }
    }
}

#[test]
fn ten_bit_full_range_neutral_chroma_stays_neutral() {
    // Full range: C = (code − 512) / 1023, so 512 is exactly neutral. Read
    // at 16-bit precision (an SDR frame into the rgba64 target).
    let frame = solid_yuv420p10(16, 16, 512, 512, 512);
    let pipeline = vance_color::PipelineParams {
        input_range: 1,
        ..default_pipeline()
    };
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(16, 16)).unwrap();
    let l = c
        .render_layer(LayerKey(1), LayerSource::Yuv(&frame), &params(pipeline))
        .unwrap();
    c.composite(l, NORMAL).unwrap();
    let buf = c.finish_to_rgba64().unwrap();
    let i = ((8 * 16 + 8) * 8) as usize;
    let ch = |k: usize| u16::from_le_bytes([buf[i + 2 * k], buf[i + 2 * k + 1]]) as i32;
    let (r16, g16, b16) = (ch(0), ch(1), ch(2));
    assert!(
        (r16 - g16).abs() <= 24 && (b16 - g16).abs() <= 24,
        "full-range grey must be neutral: {r16} {g16} {b16}"
    );
}

// ---- RGBA8 video frames (B2) --------------------------------------------------

/// A straight-alpha RGBA8 video frame (and the same pixels as an image).
fn rgba8_frame(w: u32, h: u32) -> (FrameData, RgbaImage) {
    let mut pixels = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            pixels.extend_from_slice(&[
                (x * 255 / w) as u8,
                (y * 255 / h) as u8,
                ((x + y) * 7 % 256) as u8,
                (40 + (x * 3 + y * 5) % 216) as u8,
            ]);
        }
    }
    let frame = FrameData {
        pts: 0,
        width: w,
        height: h,
        pixel_format: PixelFormat::Rgba8,
        strides: vec![w * 4],
        bytes: pixels.clone().into(),
        color: Default::default(),
    };
    (
        frame,
        RgbaImage {
            width: w,
            height: h,
            pixels,
        },
    )
}

#[test]
fn rgba8_frames_composite_like_rgba_images() {
    let (w, h) = (24, 16);
    let (frame, img) = rgba8_frame(w, h);
    // RGBA frames ignore the YUV range and matrix.
    let pipeline = vance_color::PipelineParams {
        input_range: 0,
        input_matrix: 2,
        ..srgb_pipeline()
    };
    let mut r = renderer();
    let c = r.compositor();
    let backdrop = solid_rgba(w, h, [30, 90, 200, 255]);
    let mut run = |src: LayerSource<'_>| {
        c.begin_frame(&sdr_spec(w, h)).unwrap();
        let b = c
            .render_layer(
                LayerKey(1),
                LayerSource::Rgba(&backdrop),
                &params(srgb_pipeline()),
            )
            .unwrap();
        c.composite(b, NORMAL).unwrap();
        let l = c.render_layer(LayerKey(2), src, &params(pipeline)).unwrap();
        c.composite(l, op(0, 0.9)).unwrap();
        c.finish_to_rgba8().unwrap()
    };
    let from_image = run(LayerSource::Rgba(&img));
    let from_frame = run(LayerSource::Yuv(&frame));
    assert_eq!(from_frame, from_image, "RGBA8 frame vs RGBA image");
}

#[test]
fn legacy_renders_rgba8_frames_over_black() {
    let (w, h) = (24, 16);
    let (frame, _) = rgba8_frame(w, h);
    let p = params(srgb_pipeline());
    let legacy = legacy_rgba8(&frame, &p, None);
    let comp = compositor_rgba8(&frame, &p, None);
    let d = max_diff_u8(&legacy, &comp);
    assert!(d <= 1, "legacy vs compositor RGBA8 frame: max diff {d}");
    // Alpha shows as the colour premultiplied over black.
    let s = &frame.bytes[..4];
    let want = (s[0] as f32 * s[3] as f32 / 255.0).round() as i32;
    assert!((legacy[0] as i32 - want).abs() <= 1, "{:?}", &legacy[..4]);
}

// ---- pixel aspect (B3) --------------------------------------------------------

/// A blue `src` layer with `pixel_aspect` over a red canvas; the row through
/// the middle.
fn aspect_row(canvas: (u32, u32), src: (u32, u32), pixel_aspect: f32) -> Vec<[u8; 4]> {
    let (w, h) = canvas;
    let red = solid_rgba(w, h, [255, 0, 0, 255]);
    let blue = solid_rgba(src.0, src.1, [0, 0, 255, 255]);
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(w, h)).unwrap();
    let lo = c
        .render_layer(
            LayerKey(1),
            LayerSource::Rgba(&red),
            &params(srgb_pipeline()),
        )
        .unwrap();
    c.composite(lo, NORMAL).unwrap();
    let p = LayerParams {
        pixel_aspect,
        ..params(srgb_pipeline())
    };
    let hi = c
        .render_layer(LayerKey(2), LayerSource::Rgba(&blue), &p)
        .unwrap();
    c.composite(hi, NORMAL).unwrap();
    let buf = c.finish_to_rgba8().unwrap();
    (0..w).map(|x| px(&buf, w, x, h / 2)).collect()
}

const BLUE: [u8; 4] = [0, 0, 255, 255];
const RED: [u8; 4] = [255, 0, 0, 255];

#[test]
fn anamorphic_sources_fit_by_their_display_size() {
    // HDV 1440×1080, SAR 4:3 → 1920×1080: fills a 16:9 canvas.
    let row = aspect_row((192, 108), (144, 108), 4.0 / 3.0);
    assert_eq!(row[1], BLUE, "HDV fills to the left edge");
    assert_eq!(row[190], BLUE, "HDV fills to the right edge");
    // DV NTSC 720×480, SAR 32:27 → 16:9: fills too.
    let row = aspect_row((96, 54), (72, 48), 32.0 / 27.0);
    assert_eq!(row[1], BLUE, "DV 16:9 fills");
    assert_eq!(row[94], BLUE);
    // DV NTSC, SAR 8:9 → 4:3: pillarboxed to 54·4/3 = 72 px, x = 12..84.
    let row = aspect_row((96, 54), (72, 48), 8.0 / 9.0);
    assert_eq!(row[10], RED, "4:3 DV pillarbox bar");
    assert_eq!(row[85], RED);
    assert_eq!(row[14], BLUE, "4:3 DV picture");
    assert_eq!(row[81], BLUE);
    // Square pixels are unchanged; a non-finite aspect falls back to 1.
    let row = aspect_row((96, 54), (72, 54), f32::NAN);
    assert_eq!(row[10], RED, "4:3 square-pixel pillarbox");
    assert_eq!(row[14], BLUE);
}

// ---- transparent output (B4) ------------------------------------------------

fn transparent_spec(w: u32, h: u32) -> OutputSpec {
    OutputSpec {
        transparent: true,
        ..sdr_spec(w, h)
    }
}

#[test]
fn transparent_output_reads_back_straight_alpha() {
    let (w, h) = (32, 16);
    let mut r = renderer();
    let c = r.compositor();
    // A half-size red PiP at 50% over nothing.
    c.begin_frame(&transparent_spec(w, h)).unwrap();
    let pip = LayerParams {
        scale: [0.5, 0.5],
        ..params(srgb_pipeline())
    };
    let l = c
        .render_layer(
            LayerKey(1),
            LayerSource::Rgba(&solid_rgba(w, h, [255, 0, 0, 255])),
            &pip,
        )
        .unwrap();
    c.composite(l, op(0, 0.5)).unwrap();
    let buf = c.finish_to_rgba8().unwrap();
    let mid = px(&buf, w, 16, 8);
    assert_eq!(&mid[..3], &[255, 0, 0], "straight colour {mid:?}");
    assert!(
        (127..=128).contains(&mid[3]),
        "opacity shows as alpha {mid:?}"
    );
    assert_eq!(px(&buf, w, 1, 1), [0, 0, 0, 0], "outside: alpha 0, rgb 0");

    // Two partial layers: alpha = 1 − 0.5·0.5, colour = premul / alpha.
    c.begin_frame(&transparent_spec(w, h)).unwrap();
    for (k, rgb) in [(1u128, [0u8, 0, 255]), (2, [255, 0, 0])] {
        let l = c
            .render_layer(
                LayerKey(k),
                LayerSource::Rgba(&solid_rgba(w, h, [rgb[0], rgb[1], rgb[2], 255])),
                &params(srgb_pipeline()),
            )
            .unwrap();
        c.composite(l, op(0, 0.5)).unwrap();
    }
    let p2 = px(&c.finish_to_rgba8().unwrap(), w, 5, 5);
    let want = [170, 0, 85, 191];
    for ch in 0..4 {
        assert!(
            (p2[ch] as i32 - want[ch]).abs() <= 1,
            "two 50% layers: {p2:?} vs {want:?}"
        );
    }

    // Fit-to-frame bars stay transparent; the same frame opaque is black.
    let tall = solid_rgba(8, 16, [10, 200, 30, 255]);
    let bars = |c: &mut Compositor, transparent: bool| {
        c.begin_frame(&OutputSpec {
            transparent,
            ..sdr_spec(w, h)
        })
        .unwrap();
        let l = c
            .render_layer(
                LayerKey(3),
                LayerSource::Rgba(&tall),
                &params(srgb_pipeline()),
            )
            .unwrap();
        c.composite(l, NORMAL).unwrap();
        c.finish_to_rgba8().unwrap()
    };
    let t = bars(c, true);
    assert_eq!(px(&t, w, 2, 8), [0, 0, 0, 0], "transparent bar");
    assert_eq!(px(&t, w, 16, 8), [10, 200, 30, 255], "opaque picture");
    let o = bars(c, false);
    assert_eq!(px(&o, w, 2, 8), [0, 0, 0, 255], "opaque output: black bar");
}

#[test]
fn transparent_hdr_readback_carries_alpha() {
    let p = vance_color::Pipeline {
        input: vance_color::ColorSpace::Srgb,
        working: vance_color::ColorSpace::LinearRec709,
        output: vance_color::ColorSpace::Rec2020Pq,
    }
    .to_params()
    .unwrap();
    let (w, h) = (16, 8);
    let mut r = renderer();
    let c = r.compositor();
    let read = |c: &mut Compositor, transparent: bool| {
        c.begin_frame(&OutputSpec {
            transparent,
            ..hdr_spec(w, h, &p)
        })
        .unwrap();
        let pip = LayerParams {
            scale: [0.5, 0.5],
            ..params(p)
        };
        let l = c
            .render_layer(
                LayerKey(1),
                LayerSource::Rgba(&solid_rgba(w, h, [255, 255, 255, 255])),
                &pip,
            )
            .unwrap();
        c.composite(l, op(0, 0.5)).unwrap();
        let buf = c.finish_to_rgba64().unwrap();
        let at = |x: u32, y: u32| {
            let i = ((y * w + x) * 8) as usize;
            [0, 1, 2, 3].map(|k| u16::from_le_bytes([buf[i + 2 * k], buf[i + 2 * k + 1]]))
        };
        (at(8, 4), at(1, 1))
    };
    let (opaque_mid, _) = read(c, false);
    let (mid, outside) = read(c, true);
    assert!(
        (mid[3] as i32 - 32768).abs() < 40,
        "alpha 0.5 in rgba64: {mid:?}"
    );
    // Straight colour = PQ(100-nit white), brighter than the 50% mix over
    // black the opaque output shows.
    assert!(mid[0] > opaque_mid[0] + 2000, "{mid:?} vs {opaque_mid:?}");
    assert_eq!(outside, [0, 0, 0, 0], "transparent bar in rgba64");
}

#[test]
fn preview_is_never_transparent() {
    let (w, h) = (8, 8);
    let mut r = renderer();
    let target = r.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("test-view"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&Default::default());
    let c = r.compositor();
    c.begin_frame(&transparent_spec(w, h)).unwrap();
    let l = c
        .render_layer(
            LayerKey(1),
            LayerSource::Rgba(&solid_rgba(w, h, [200, 100, 50, 255])),
            &params(srgb_pipeline()),
        )
        .unwrap();
    c.composite(l, op(0, 0.5)).unwrap();
    c.finish_to_view(&view, wgpu::TextureFormat::Rgba8Unorm)
        .unwrap();
    let buf = read_texture_rgba8(&r, &target);
    assert_eq!(
        px(&buf, w, 4, 4),
        [100, 50, 25, 255],
        "flattened over black"
    );
}

/// Read back an `Rgba8Unorm` texture (COPY_SRC) as tight rows.
fn read_texture_rgba8(r: &Renderer, t: &wgpu::Texture) -> Vec<u8> {
    let (w, h) = (t.width(), t.height());
    let padded =
        (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let staging = r.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("test-staging"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = r.device().create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
        t.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(h),
            },
        },
        t.size(),
    );
    r.queue().submit(Some(enc.finish()));
    staging.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    r.device()
        .poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: None,
        })
        .unwrap();
    let mapped = staging.slice(..).get_mapped_range();
    let mut out = Vec::new();
    for y in 0..h as usize {
        out.extend_from_slice(&mapped[y * padded as usize..][..(w * 4) as usize]);
    }
    out
}

// ---- HDR pack / unpack primaries (B7) ---------------------------------------

/// A PQ rgba64 code as linear light (working 1.0 = 100 cd/m²).
fn pq_linear(code: u16) -> f64 {
    let (m1, m2, c1, c2, c3) = (0.1593017578125, 78.84375, 0.8359375, 18.8515625, 18.6875);
    let v = (code as f64 / 65535.0).powf(1.0 / m2);
    ((v - c1).max(0.0) / (c2 - c3 * v)).powf(1.0 / m1) * 100.0
}

fn srgb_linear(code: f64) -> f64 {
    let e = (code / 255.0).clamp(0.0, 1.0);
    if e <= 0.04045 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

/// Compare two PQ rgba64 renders of the straight sRGB `img` (output
/// primaries Rec.2020, working → output `w2o`) per channel in linear light,
/// against what precision allows at that pixel: one 8-bit sRGB code of the
/// source (a GPU may round the CPU copy differently) through Rec.709 →
/// Rec.2020 and the alpha, four f16 roundings of the largest working value
/// (slot and accumulator, each side) into the output primaries, and one f16
/// step of the PQ code each side. Returns the worst error / allowance and
/// where it is.
fn hdr_compare(a: &[u8], b: &[u8], img: &RgbaImage, w2o: vance_color::Transform) -> (f64, String) {
    let m = vance_color::transform::primaries_to_primaries(
        vance_color::Primaries::Bt709,
        vance_color::Primaries::Bt2020,
    );
    let o2w = w2o.inverse();
    let norm =
        |t: &vance_color::Transform, i: usize| t.0[i].iter().map(|v| v.abs() as f64).sum::<f64>();
    let o2w_max = (0..3).map(|i| norm(&o2w, i)).fold(0.0, f64::max);
    let mut worst = (0.0f64, String::new());
    for p in 0..(img.width * img.height) as usize {
        let src = &img.pixels[p * 4..p * 4 + 4];
        let alpha = src[3] as f64 / 255.0;
        let code =
            |f: &[u8], ch: usize| u16::from_le_bytes([f[p * 8 + 2 * ch], f[p * 8 + 2 * ch + 1]]);
        let lin = |f: &[u8], ch: usize| pq_linear(code(f, ch));
        let largest = (0..3)
            .map(|ch| lin(a, ch).max(lin(b, ch)))
            .fold(0.0, f64::max);
        let step: Vec<f64> = (0..3)
            .map(|j| {
                let c = src[j] as f64;
                (srgb_linear(c + 1.0) - srgb_linear(c)).max(srgb_linear(c) - srgb_linear(c - 1.0))
            })
            .collect();
        for ch in 0..3 {
            let eight_bit = alpha
                * (0..3)
                    .map(|j| m.0[ch][j].abs() as f64 * step[j])
                    .sum::<f64>();
            let f16 = 4.0 * 2f64.powi(-11) * largest * o2w_max * norm(&w2o, ch);
            // The readback target is Rgba16Float: PQ codes carry f16
            // spacing (16 codes around 0.4).
            let pq_step = |f: &[u8]| {
                let c = code(f, ch);
                let e = (c as f64 / 65535.0).max(2f64.powi(-14));
                let ulp = (2f64.powi(e.log2().floor() as i32 - 10) * 65535.0)
                    .ceil()
                    .max(1.0);
                pq_linear(c.saturating_add(ulp as u16)) - pq_linear(c)
            };
            // Intentional: an f16 store may flush a subnormal (< 2^-14) to 0;
            // Vulkan allows it and Mesa's lavapipe does it, Metal doesn't. One
            // flush per side's working-space chain, carried into the output.
            let f16_flush = 2.0 * 2f64.powi(-14) * norm(&w2o, ch);
            let allowed = eight_bit + f16 + f16_flush + pq_step(a) + pq_step(b);
            let err = (lin(a, ch) - lin(b, ch)).abs();
            let ratio = err / allowed;
            if ratio > worst.0 {
                worst = (
                    ratio,
                    format!(
                        "pixel {p} ch{ch}: source {src:?}, PQ code {} vs {}, linear {:.6} vs \
                         {:.6}, allowed ±{allowed:.6}",
                        code(a, ch),
                        code(b, ch),
                        lin(a, ch),
                        lin(b, ch)
                    ),
                );
            }
        }
    }
    worst
}

#[test]
fn hdr_readback_round_trips_in_a_wide_working_space() {
    // Read back → write back must reproduce the layer up to what precision
    // allows (`hdr_compare`). Not a fixed PQ-code tolerance: near black one
    // 8-bit sRGB code is ~1000 PQ codes, so a GPU rounding the CPU copy
    // differently (float → unorm8) looks like a bug there; and not straight
    // sRGB, where converting wide-gamut values back to Rec.709 multiplies
    // the f16 error of a saturated colour.
    let img = rgba8_frame(16, 8).1;
    for working in [
        vance_color::ColorSpace::LinearRec2020,
        vance_color::ColorSpace::Acescg,
        vance_color::ColorSpace::LinearRec709,
    ] {
        let p = vance_color::Pipeline {
            input: vance_color::ColorSpace::Srgb,
            working,
            output: vance_color::ColorSpace::Rec2020Pq,
        }
        .to_params()
        .unwrap();
        let mut r = renderer();
        let c = r.compositor();
        let spec = hdr_spec(16, 8, &p);
        // Straight through …
        c.begin_frame(&spec).unwrap();
        let l = c
            .render_layer(LayerKey(1), LayerSource::Rgba(&img), &params(p))
            .unwrap();
        c.composite(l, NORMAL).unwrap();
        let direct = c.finish_to_rgba64().unwrap();
        // … and through a CPU round trip (read → write → read).
        let trip = |c: &mut Compositor, edit: &dyn Fn(&mut RgbaImage)| {
            c.begin_frame(&spec).unwrap();
            let l = c
                .render_layer(LayerKey(1), LayerSource::Rgba(&img), &params(p))
                .unwrap();
            let mut back = c.read_layer_rgba8(l).unwrap();
            edit(&mut back);
            c.write_layer_rgba8(l, &back).unwrap();
            let again = c.read_layer_rgba8(l).unwrap();
            c.composite(l, NORMAL).unwrap();
            (back, again, c.finish_to_rgba64().unwrap())
        };
        let (back, again, tripped) = trip(c, &|_| {});
        let d = max_diff_u8(&back.pixels, &img.pixels);
        assert!(d <= 1, "{working:?}: readback vs the sRGB source: {d}");
        let d = max_diff_u8(&again.pixels, &back.pixels);
        assert!(
            d <= 1,
            "{working:?}: read after write vs what was written: {d}"
        );
        let (d, at) = hdr_compare(&direct, &tripped, &img, spec.working_to_output);
        eprintln!("{working:?}: write-back composite vs direct: {d:.2} of the allowance ({at})");
        assert!(
            d <= 1.0,
            "{working:?}: write-back composite vs direct: {d:.2} × allowed at {at}"
        );
        if working == vance_color::ColorSpace::LinearRec2020 {
            // One 8-bit code above the source on the black pixel (0, 0) — as
            // a GPU rounding differently can produce — is within the
            // allowance but far past any fixed PQ-code tolerance.
            let one_up = img.pixels[0] + 1;
            let (_, _, tripped) = trip(c, &|b: &mut RgbaImage| b.pixels[0] = one_up);
            let (d, at) = hdr_compare(&direct, &tripped, &img, spec.working_to_output);
            assert!(d <= 1.0, "one code on black: {d:.2} × allowed at {at}");
            let pq = max_diff_u16(&direct, &tripped);
            assert!(pq > 700, "one code on black is {pq} PQ codes");
        }
    }
}

// ---- odd-size edges (B9) ------------------------------------------------------

#[test]
fn odd_size_clip_fills_its_even_rounded_canvas() {
    for (canvas, src) in [((1278, 718), (1279, 719)), ((718, 1278), (719, 1279))] {
        let (w, h) = canvas;
        let red = solid_rgba(w, h, [255, 0, 0, 255]);
        let white = solid_rgba(src.0, src.1, [255, 255, 255, 255]);
        let mut r = renderer();
        let c = r.compositor();
        c.begin_frame(&sdr_spec(w, h)).unwrap();
        let lo = c
            .render_layer(
                LayerKey(1),
                LayerSource::Rgba(&red),
                &params(srgb_pipeline()),
            )
            .unwrap();
        c.composite(lo, NORMAL).unwrap();
        let hi = c
            .render_layer(
                LayerKey(2),
                LayerSource::Rgba(&white),
                &params(srgb_pipeline()),
            )
            .unwrap();
        c.composite(hi, NORMAL).unwrap();
        let buf = c.finish_to_rgba8().unwrap();
        for (x, y) in [(0, h / 2), (w - 1, h / 2), (w / 2, 0), (w / 2, h - 1)] {
            assert_eq!(
                px(&buf, w, x, y),
                [255, 255, 255, 255],
                "{src:?} on {canvas:?}: edge pixel ({x}, {y})"
            );
        }
    }
}

// ---- handles and slot reuse (B10) ---------------------------------------------

#[test]
fn a_handle_from_an_earlier_frame_never_resolves_in_a_new_one() {
    let mut r = renderer();
    let c = r.compositor();
    let p = params(srgb_pipeline());
    let img = solid_rgba(4, 4, [1, 2, 3, 255]);
    c.begin_frame(&sdr_spec(4, 4)).unwrap();
    let _first = c
        .render_layer(LayerKey(1), LayerSource::Rgba(&img), &p)
        .unwrap();
    let old = c
        .render_layer(LayerKey(2), LayerSource::Rgba(&img), &p)
        .unwrap();
    c.composite(old, NORMAL).unwrap();
    c.finish_to_rgba8().unwrap();
    // The new frame has layers at the same indices; the old handle must
    // still fail instead of resolving to one of them.
    c.begin_frame(&sdr_spec(4, 4)).unwrap();
    for k in [5u128, 6] {
        c.render_layer(LayerKey(k), LayerSource::Rgba(&img), &p)
            .unwrap();
    }
    assert!(matches!(
        c.composite(old, NORMAL),
        Err(RenderError::Compositor(_))
    ));
    assert!(c.read_layer_rgba8(old).is_err());
    c.finish_to_rgba8().unwrap();
}

#[test]
fn rerendering_a_slot_keeps_an_earlier_recorded_composite() {
    // render(A) → render(B) → read(B) [submit] → composite(A) → render(A')
    // must not let A' overwrite the uploads composite(A) still reads.
    let (w, h) = (16, 16);
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(w, h)).unwrap();
    let a = c
        .render_layer(
            LayerKey(1),
            LayerSource::Rgba(&solid_rgba(w, h, [255, 0, 0, 255])),
            &params(srgb_pipeline()),
        )
        .unwrap();
    let b = c
        .render_layer(
            LayerKey(2),
            LayerSource::Rgba(&solid_rgba(w, h, [0, 255, 0, 255])),
            &params(srgb_pipeline()),
        )
        .unwrap();
    c.read_layer_rgba8(b).unwrap();
    c.composite(a, NORMAL).unwrap();
    let pip = LayerParams {
        scale: [0.5, 0.5],
        ..params(srgb_pipeline())
    };
    let a2 = c
        .render_layer(
            LayerKey(1),
            LayerSource::Rgba(&solid_rgba(w, h, [0, 0, 255, 255])),
            &pip,
        )
        .unwrap();
    c.composite(a2, NORMAL).unwrap();
    let buf = c.finish_to_rgba8().unwrap();
    assert_eq!(px(&buf, w, 1, 1), [255, 0, 0, 255], "the first A composite");
    assert_eq!(px(&buf, w, 8, 8), [0, 0, 255, 255], "A' on top");
}

// ---- power windows in display space -----------------------------------------

/// A grade node soloing `window`'s mask (sRGB-encoded grey in the output).
fn window_solo(window: vance_render::Window) -> GradeNodeParams {
    GradeNodeParams {
        qual: Qualifier {
            show_mask: true,
            ..Qualifier::default()
        },
        window,
        ..GradeNodeParams::default()
    }
}

/// CPU mirror of grade.wgsl's `window_mask` at source UV `uv`; `aspect` is
/// the frame's DISPLAY width / height.
fn window_mask_ref(w: &vance_render::Window, uv: [f32; 2], aspect: f32) -> f32 {
    let d = [(uv[0] - w.center[0]) * aspect, uv[1] - w.center[1]];
    let (c, s) = ((-w.rotation).cos(), (-w.rotation).sin());
    let l = [d[0] * c - d[1] * s, d[0] * s + d[1] * c];
    let r = [w.size[0].max(1e-4) * aspect, w.size[1].max(1e-4)];
    let nd = if w.shape == 1 {
        ((l[0] / r[0]).powi(2) + (l[1] / r[1]).powi(2)).sqrt()
    } else {
        (l[0].abs() / r[0]).max(l[1].abs() / r[1])
    };
    let band = w.feather.max(1e-4) / (0.5 * (r[0] + r[1])).max(1e-4);
    let t = ((nd - 1.0) / band).clamp(0.0, 1.0);
    let m = 1.0 - t * t * (3.0 - 2.0 * t);
    if w.invert {
        1.0 - m
    } else {
        m
    }
}

fn srgb_byte(v: f32) -> i32 {
    let v = v.clamp(0.0, 1.0);
    let e = if v <= 0.0031308 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (e * 255.0).round() as i32
}

/// A grey 4:3-SAR frame (HDV-shaped, 144×108 stored → 192×108 displayed).
fn hdv_frame(pixel_aspect: (u32, u32)) -> FrameData {
    let mut f = solid_yuv420p(144, 108, 128, 128, 128);
    f.color = vance_decoder::FrameColor {
        sample_aspect: pixel_aspect,
        ..Default::default()
    };
    f
}

/// A circle of radius 32.4 display px (0.3 of the height), rotated — on a
/// 4:3-SAR source its x half-size is 0.3 · 108/192 of the width.
fn display_circle() -> vance_render::Window {
    vance_render::Window {
        shape: 1,
        center: [0.5, 0.5],
        size: [0.3 * 108.0 / 192.0, 0.3],
        rotation: 0.7,
        feather: 0.01,
        invert: false,
    }
}

/// Every pixel of a 192×108 matte: white well inside the 32.4 px circle,
/// black well outside it.
fn assert_covers_display_circle(buf: &[u8], label: &str) {
    let (w, h) = (192u32, 108u32);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f32 + 0.5 - 96.0).powi(2) + (y as f32 + 0.5 - 54.0).powi(2)).sqrt();
            let v = px(buf, w, x, y)[0];
            if d <= 30.9 {
                assert!(v >= 250, "{label}: ({x}, {y}) {d:.1} px from centre is {v}");
            } else if d >= 34.9 {
                assert!(v <= 5, "{label}: ({x}, {y}) {d:.1} px from centre is {v}");
            }
        }
    }
}

#[test]
fn a_circular_window_on_an_anamorphic_source_is_a_circle_on_the_canvas() {
    let frame = hdv_frame((4, 3));
    let p = LayerParams {
        grades: vec![window_solo(display_circle())],
        pixel_aspect: 4.0 / 3.0,
        ..params(default_pipeline())
    };
    let comp = single_layer_rgba8(
        renderer().compositor(),
        &sdr_spec(192, 108),
        LayerSource::Yuv(&frame),
        &p,
    );
    assert_covers_display_circle(&comp, "compositor");
    // The legacy renderer reads the frame's own sample aspect.
    let mut r = renderer();
    r.upload_frame(&frame).unwrap();
    r.set_grades(&p.grades);
    let legacy = r.render_to_rgba_buffer(192, 108).unwrap();
    assert_covers_display_circle(&legacy, "legacy");
}

#[test]
fn square_pixel_windows_are_unchanged() {
    // Storage aspect = display aspect: the mask matches the CPU reference
    // at the frame's own width / height, in both paths.
    let (w, h) = (96u32, 54u32);
    let frame = solid_yuv420p(w, h, 128, 128, 128);
    for window in [
        display_circle(),
        vance_render::Window {
            shape: 2,
            center: [0.4, 0.55],
            size: [0.2, 0.3],
            rotation: -0.5,
            feather: 0.08,
            invert: true,
        },
    ] {
        let p = LayerParams {
            grades: vec![window_solo(window)],
            ..params(default_pipeline())
        };
        let comp = single_layer_rgba8(
            renderer().compositor(),
            &sdr_spec(w, h),
            LayerSource::Yuv(&frame),
            &p,
        );
        let mut r = renderer();
        r.upload_frame(&frame).unwrap();
        r.set_grades(&p.grades);
        let legacy = r.render_to_rgba_buffer(w, h).unwrap();
        let aspect = w as f32 / h as f32;
        for y in 0..h {
            for x in 0..w {
                let uv = [(x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32];
                let want = srgb_byte(window_mask_ref(&window, uv, aspect));
                for (label, buf) in [("compositor", &comp), ("legacy", &legacy)] {
                    let got = px(buf, w, x, y)[0] as i32;
                    assert!(
                        (got - want).abs() <= 3,
                        "{label} {window:?} at ({x}, {y}): {got} vs {want}"
                    );
                }
            }
        }
    }
}

// ---- round 2: straight alpha, odd-size chroma, output primaries ---------------

#[test]
fn straight_alpha_zeroes_colour_where_alpha_rounds_to_zero() {
    // An anti-aliased rotated edge has coverage just above 0; at 8 bits an
    // alpha under 0.5/255 reads 0, and its colour must read 0 too (same
    // at 16 bits with 0.5/65535).
    let (w, h) = (256u32, 256u32);
    let img = solid_rgba(w, h, [250, 200, 120, 255]);
    let mut r = renderer();
    let c = r.compositor();
    let mut checked = 0;
    for (rotation, opacity, rgba64) in [
        (0.37f32, 1.0f32, false),
        (0.81, 0.3, false),
        (0.37, 0.004, true),
        (1.13, 0.01, true),
    ] {
        c.begin_frame(&transparent_spec(w, h)).unwrap();
        let p = LayerParams {
            scale: [0.6, 0.6],
            rotation,
            ..params(srgb_pipeline())
        };
        let l = c
            .render_layer(LayerKey(1), LayerSource::Rgba(&img), &p)
            .unwrap();
        c.composite(l, op(0, opacity)).unwrap();
        if rgba64 {
            let buf = c.finish_to_rgba64().unwrap();
            for px in buf.chunks_exact(8) {
                let ch = |k: usize| u16::from_le_bytes([px[2 * k], px[2 * k + 1]]);
                if ch(3) == 0 {
                    assert_eq!((ch(0), ch(1), ch(2)), (0, 0, 0), "rgba64 alpha 0");
                    checked += 1;
                }
            }
        } else {
            let buf = c.finish_to_rgba8().unwrap();
            for px in buf.chunks_exact(4) {
                if px[3] == 0 {
                    assert_eq!(&px[..3], &[0, 0, 0], "rgba8 alpha 0 at {rotation}");
                    checked += 1;
                }
            }
        }
    }
    assert!(checked > 1000, "transparent pixels checked: {checked}");
}

/// A 1279×719 frame (odd both ways): neutral, with a red chroma edge near
/// the right (chroma column 600 → luma ≈ 1200) and a blue-ish one near the
/// bottom (chroma row 350 → luma ≈ 700).
fn odd_chroma_frame() -> FrameData {
    let (w, h) = (1279u32, 719u32);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut b = vec![128u8; (w * h) as usize];
    for i in 0..ch {
        for _ in 0..cw {
            b.push(if i >= 350 { 40 } else { 128 });
        }
    }
    for _ in 0..ch {
        for j in 0..cw {
            b.push(if j >= 600 { 240 } else { 128 });
        }
    }
    FrameData {
        pts: 0,
        width: w,
        height: h,
        pixel_format: PixelFormat::Yuv420P,
        strides: vec![w, cw, cw],
        bytes: b.into(),
        color: Default::default(),
    }
}

/// CPU decode of luma pixel (x, y) of a planar 4:2:0 frame: chroma sited at
/// the centre of its 2×2 luma block, bilinear, edges clamped.
fn decode_ref(f: &FrameData, x: u32, y: u32) -> [u8; 3] {
    let (w, h) = (f.width, f.height);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let luma = (w * h) as usize;
    let chroma = (cw * ch) as usize;
    let at = |plane: usize, i: i32, j: i32| {
        let (i, j) = (i.clamp(0, ch as i32 - 1), j.clamp(0, cw as i32 - 1));
        f.bytes[luma + plane * chroma + (i as usize) * cw as usize + j as usize] as f32
    };
    let (cx, cy) = ((x as f32 + 0.5) / 2.0 - 0.5, (y as f32 + 0.5) / 2.0 - 0.5);
    let (j0, i0) = (cx.floor(), cy.floor());
    let (fx, fy) = (cx - j0, cy - i0);
    let (j0, i0) = (j0 as i32, i0 as i32);
    let bil = |plane| {
        let top = at(plane, i0, j0) * (1.0 - fx) + at(plane, i0, j0 + 1) * fx;
        let bot = at(plane, i0 + 1, j0) * (1.0 - fx) + at(plane, i0 + 1, j0 + 1) * fx;
        top * (1.0 - fy) + bot * fy
    };
    let yv = f.bytes[(y * w + x) as usize] as f32;
    let rgb = ycbcr_to_rgb(
        (yv - 16.0) / 219.0,
        (bil(0) - 128.0) / 224.0,
        (bil(1) - 128.0) / 224.0,
        0,
    );
    rgb.map(rec709_enc_to_srgb_byte)
}

#[test]
fn odd_size_chroma_stays_on_its_luma_grid() {
    let planar = odd_chroma_frame();
    let (w, h) = (planar.width, planar.height);
    let p = params(default_pipeline());
    let mut probes: Vec<(u32, u32)> = (1188..w).map(|x| (x, 100)).collect();
    probes.extend((688..h).map(|y| (100, y)));
    let want: Vec<[u8; 3]> = probes
        .iter()
        .map(|&(x, y)| decode_ref(&planar, x, y))
        .collect();
    for (label, f) in [("planar", planar.clone()), ("nv12", to_nv12(&planar))] {
        let comp = compositor_rgba8(&f, &p, None);
        let legacy = legacy_rgba8(&f, &p, None);
        for (path, buf) in [("compositor", &comp), ("legacy", &legacy)] {
            for (&(x, y), want) in probes.iter().zip(&want) {
                let got = px(buf, w, x, y);
                for ch in 0..3 {
                    assert!(
                        (got[ch] as i32 - want[ch] as i32).abs() <= 2,
                        "{label} {path} ({x}, {y}): {got:?} vs {want:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn hdr_readback_uses_the_declared_output_primaries() {
    // An HDR output in Rec.709 or P3 primaries with a PQ transfer: the pack
    // must convert from the declared primaries, not guess Rec.2020 from PQ.
    let img = rgba8_frame(16, 8).1;
    let p = vance_color::Pipeline {
        input: vance_color::ColorSpace::Srgb,
        working: vance_color::ColorSpace::LinearRec2020,
        output: vance_color::ColorSpace::Rec2020Pq,
    }
    .to_params()
    .unwrap();
    use vance_color::{transform::primaries_to_primaries, Primaries};
    for prim in [Primaries::Bt709, Primaries::DisplayP3, Primaries::Bt2020] {
        let spec = OutputSpec {
            working_to_output: primaries_to_primaries(Primaries::Bt2020, prim),
            output_primaries: prim,
            ..hdr_spec(16, 8, &p)
        };
        let mut r = renderer();
        let c = r.compositor();
        c.begin_frame(&spec).unwrap();
        let l = c
            .render_layer(LayerKey(1), LayerSource::Rgba(&img), &params(p))
            .unwrap();
        let back = c.read_layer_rgba8(l).unwrap();
        let d = max_diff_u8(&back.pixels, &img.pixels);
        assert!(d <= 1, "{prim:?} output: readback vs the sRGB source: {d}");
        c.finish_to_rgba64().unwrap();
    }
}

// ---- perf --------------------------------------------------------------------

fn surface_target(r: &Renderer, w: u32, h: u32) -> wgpu::TextureView {
    r.device()
        .create_texture(&wgpu::TextureDescriptor {
            label: Some("perf-out"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        })
        .create_view(&Default::default())
}

/// The perf frame up to `finish`: 4 × 1080p layers — the bottom one keyed
/// (choke 1 + soften 2) — with `grades[i]` on layer `i`, blended Normal /
/// Screen / Multiply / Overlay.
fn record_perf_frame(c: &mut Compositor, frames: &[FrameData], grades: &[Vec<GradeNodeParams>]) {
    let ops = [op(0, 1.0), op(3, 0.8), op(2, 1.0), op(4, 0.7)];
    c.begin_frame(&sdr_spec(1920, 1080)).unwrap();
    for (i, f) in frames.iter().enumerate() {
        let p = LayerParams {
            keyer: (i == 0).then_some(KeyerParams {
                key_rgb: [0.0, 1.0, 0.0],
                balance: 0.5,
                strength: 1.0,
                clip_black: 0.1,
                clip_white: 0.9,
                choke_px: 1.0,
                soften_px: 2.0,
                spill: 1.0,
                spill_bias: 0.5,
                invert: false,
            }),
            grades: grades[i].clone(),
            ..params(default_pipeline())
        };
        let l = c
            .render_layer(LayerKey(i as u128), LayerSource::Yuv(f), &p)
            .unwrap();
        c.composite(l, ops[i]).unwrap();
    }
}

/// (median sustained ms/frame, median CPU record ms/frame). Apple GPUs
/// clock down between short bursts, so the GPU is first kept busy for a
/// second; frames are then submitted back to back (CPU and GPU overlap) in
/// batches of 20, so ms/frame = max(CPU, GPU) — an upper bound on the GPU
/// frame time.
fn sustained_ms(
    r: &mut Renderer,
    frames: &[FrameData],
    grades: &[Vec<GradeNodeParams>],
) -> (f64, f64) {
    let out = surface_target(r, 1920, 1080);
    let device = r.device().clone();
    let wait = || {
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
    };
    let c = r.compositor();
    let frame = |c: &mut Compositor| {
        record_perf_frame(c, frames, grades);
        c.finish_to_view(&out, wgpu::TextureFormat::Bgra8UnormSrgb)
            .unwrap();
    };
    let warm = std::time::Instant::now();
    while warm.elapsed().as_secs_f64() < 1.0 {
        frame(c);
    }
    wait();
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let batches: Vec<f64> = (0..5)
        .map(|_| {
            let t = std::time::Instant::now();
            for _ in 0..20 {
                frame(c);
            }
            wait();
            t.elapsed().as_secs_f64() * 1e3 / 20.0
        })
        .collect();
    let record: Vec<f64> = (0..9)
        .map(|_| {
            let t = std::time::Instant::now();
            record_perf_frame(c, frames, grades);
            let ms = t.elapsed().as_secs_f64() * 1e3;
            c.finish_to_view(&out, wgpu::TextureFormat::Bgra8UnormSrgb)
                .unwrap();
            wait();
            ms
        })
        .collect();
    (median(batches), median(record))
}

/// 4 layers at 1080p with a keyer, grade nodes and 3 non-Normal blends;
/// prints the sustained frame time (target < 8 ms GPU on an M1).
#[test]
#[ignore]
fn perf_four_1080p_layers() {
    let (w, h) = (1920u32, 1080u32);
    let mut r = renderer();
    // A green-screen-ish plate under three gradient layers.
    let mut frames = vec![solid_yuv420p(w, h, 145, 54, 34)];
    frames.extend((0..3).map(|_| gradient_yuv420p(w, h)));
    // Primary, qualified log, curves (tone + HSL), primary.
    let mut nodes = sample_grades();
    nodes.push(GradeNodeParams {
        gain: [1.0, 1.0, 1.0, 1.05],
        ..GradeNodeParams::default()
    });
    // The brief's frame: 4 grade nodes in total, one per layer …
    let one_each: Vec<Vec<GradeNodeParams>> = (0..4).map(|i| vec![nodes[i].clone()]).collect();
    let (a, a_cpu) = sustained_ms(&mut r, &frames, &one_each);
    // … and a stress variant with all 4 on every layer (16 in total).
    let (b, b_cpu) = sustained_ms(&mut r, &frames, &vec![nodes.clone(); 4]);
    let info = r.device().adapter_info();
    eprintln!(
        "compositor perf on {} ({:?}), 1920×1080, 4 layers, keyer (choke 1 + soften 2) on one, \
         Screen/Multiply/Overlay: 4 grade nodes in total → {a:.2} ms/frame sustained (CPU \
         record {a_cpu:.2} ms); 4 nodes on every layer → {b:.2} ms/frame (CPU {b_cpu:.2} ms). \
         Sustained = max(CPU, GPU) with both overlapped, so the GPU frame time is at most this \
         (debug build CPU side).",
        info.name, info.backend
    );
}

/// 4 × 1080p NV12 layers (the hardware-decode format), keyer on the bottom
/// one, no grades: the per-frame upload cost shows in the CPU record time.
#[test]
#[ignore]
fn perf_four_1080p_nv12_layers() {
    let (w, h) = (1920u32, 1080u32);
    let mut r = renderer();
    let mut frames = vec![to_nv12(&solid_yuv420p(w, h, 145, 54, 34))];
    frames.extend((0..3).map(|_| to_nv12(&gradient_yuv420p(w, h))));
    let none: Vec<Vec<GradeNodeParams>> = vec![Vec::new(); 4];
    let (ms, cpu) = sustained_ms(&mut r, &frames, &none);
    eprintln!(
        "compositor perf, 4 × 1080p NV12 layers: {ms:.2} ms/frame sustained, CPU record \
         {cpu:.2} ms/frame"
    );
}

/// Sustained ms/frame of one keyed `w×h` layer (canvas = frame) with
/// `choke` / `soften` px, measured as `sustained_ms` does.
fn keyed_layer_ms(r: &mut Renderer, w: u32, h: u32, choke: f32, soften: f32) -> f64 {
    let plate = solid_yuv420p(w, h, 145, 54, 34);
    let out = surface_target(r, w, h);
    let device = r.device().clone();
    let wait = || {
        device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
    };
    let p = LayerParams {
        keyer: Some(KeyerParams {
            key_rgb: [0.0, 1.0, 0.0],
            balance: 0.5,
            strength: 1.0,
            clip_black: 0.1,
            clip_white: 0.9,
            choke_px: choke,
            soften_px: soften,
            spill: 1.0,
            spill_bias: 0.5,
            invert: false,
        }),
        ..params(default_pipeline())
    };
    let c = r.compositor();
    let frame = |c: &mut Compositor| {
        c.begin_frame(&sdr_spec(w, h)).unwrap();
        let l = c
            .render_layer(LayerKey(1), LayerSource::Yuv(&plate), &p)
            .unwrap();
        c.composite(l, NORMAL).unwrap();
        c.finish_to_view(&out, wgpu::TextureFormat::Bgra8UnormSrgb)
            .unwrap();
    };
    let warm = std::time::Instant::now();
    while warm.elapsed().as_secs_f64() < 1.0 {
        frame(c);
    }
    wait();
    let mut batches: Vec<f64> = (0..5)
        .map(|_| {
            let t = std::time::Instant::now();
            for _ in 0..10 {
                frame(c);
            }
            wait();
            t.elapsed().as_secs_f64() * 1e3 / 10.0
        })
        .collect();
    batches.sort_by(|a, b| a.partial_cmp(b).unwrap());
    batches[2]
}

/// The keyer's large-radius cost: one keyed layer at 1080p and 4K with no
/// refinement, choke 64 and soften 64 (sustained ms/frame, so the extra
/// over "plain" is the matte passes).
#[test]
#[ignore]
fn perf_keyer_large_choke_and_soften() {
    let mut r = renderer();
    for (w, h, label) in [(1920u32, 1080u32, "1080p"), (3840, 2160, "4K")] {
        let plain = keyed_layer_ms(&mut r, w, h, 0.0, 0.0);
        let choke = keyed_layer_ms(&mut r, w, h, 64.0, 0.0);
        let soften = keyed_layer_ms(&mut r, w, h, 0.0, 64.0);
        let both = keyed_layer_ms(&mut r, w, h, 64.0, 64.0);
        eprintln!(
            "keyer perf {label}: plain {plain:.2} ms, choke 64 {choke:.2} ms, soften 64 \
             {soften:.2} ms, both {both:.2} ms (sustained ms/frame)"
        );
    }
}
