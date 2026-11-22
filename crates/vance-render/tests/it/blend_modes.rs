//! The 12 blend modes against a CPU reference of the ADR 0007 formulas
//! (premultiplied W3C separable compositing), in SDR display space, plus HDR
//! cases showing linear values above 1.0 survive.

use vance_render::{LayerKey, LayerSource};

use crate::compositing::{
    hdr_spec, op, params, px, renderer, sdr_spec, solid_rgba, solid_yuv420p, srgb_pipeline, NORMAL,
};

const MODES: [&str; 12] = [
    "normal",
    "add",
    "multiply",
    "screen",
    "overlay",
    "darken",
    "lighten",
    "difference",
    "soft_light",
    "hard_light",
    "color_dodge",
    "color_burn",
];

/// Straight RGBA bytes → `[0, 1]`.
pub(crate) fn straight(px: [u8; 4]) -> [f64; 4] {
    px.map(|v| v as f64 / 255.0)
}

/// `B(Cb, Cs)` for one channel, per the design's table. Screen is not
/// clamped (decision 4): above 1.0 it is `max(Cb, Cs)`.
fn blend_channel(mode: u32, cb: f64, cs: f64) -> f64 {
    if mode == 3 && (cb > 1.0 || cs > 1.0) {
        return cb.max(cs);
    }
    let (cb, cs) = if matches!(mode, 4 | 8..=11) {
        (cb.clamp(0.0, 1.0), cs.clamp(0.0, 1.0))
    } else {
        (cb, cs)
    };
    let screen = |b: f64, s: f64| b + s - b * s;
    let hard_light = |b: f64, s: f64| {
        if s <= 0.5 {
            2.0 * b * s
        } else {
            screen(b, 2.0 * s - 1.0)
        }
    };
    match mode {
        1 => cb + cs,
        2 => cb * cs,
        3 => screen(cb, cs),
        4 => hard_light(cs, cb),
        5 => cb.min(cs),
        6 => cb.max(cs),
        7 => (cb - cs).abs(),
        8 => {
            let d = if cb <= 0.25 {
                ((16.0 * cb - 12.0) * cb + 4.0) * cb
            } else {
                cb.sqrt()
            };
            if cs <= 0.5 {
                cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb)
            } else {
                cb + (2.0 * cs - 1.0) * (d - cb)
            }
        }
        9 => hard_light(cb, cs),
        10 => {
            if cb == 0.0 {
                0.0
            } else if cs >= 1.0 {
                1.0
            } else {
                (cb / (1.0 - cs)).min(1.0)
            }
        }
        11 => {
            if cb == 1.0 {
                1.0
            } else if cs <= 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - cb) / cs).min(1.0)
            }
        }
        _ => cs,
    }
}

/// Premultiplied `src` (already × opacity) over premultiplied `bd`:
/// `co = cs(1−ab) + cb(1−as) + as·ab·B(Cb, Cs)`, `ao = as + ab(1−as)`.
fn blend_pm_ref(mode: u32, bd: [f64; 4], src: [f64; 4]) -> [f64; 4] {
    let (ab, as_) = (bd[3], src[3]);
    let un = |c: f64, a: f64| if a > 0.0 { c / a } else { 0.0 };
    let mut out = [0.0; 4];
    for c in 0..3 {
        let b = blend_channel(mode, un(bd[c], ab), un(src[c], as_));
        out[c] = src[c] * (1.0 - ab) + bd[c] * (1.0 - as_) + as_ * ab * b;
    }
    out[3] = as_ + ab * (1.0 - as_);
    out
}

/// Straight `src` at `opacity` over straight `bd`; premultiplied result.
pub(crate) fn blend_ref(mode: u32, bd: [f64; 4], src: [f64; 4], opacity: f64) -> [f64; 4] {
    let pm = |s: [f64; 4], k: f64| [s[0] * s[3] * k, s[1] * s[3] * k, s[2] * s[3] * k, s[3] * k];
    blend_pm_ref(mode, pm(bd, 1.0), pm(src, opacity))
}

/// Round to the nearest IEEE half (ties to even), as the GPU stores the
/// `Rgba16Float` layer slots and accumulators.
fn f16q(v: f64) -> f64 {
    if v == 0.0 || !v.is_finite() {
        return v;
    }
    let e = (v.abs().log2().floor() as i32).max(-14);
    let ulp = 2f64.powi(e - 10);
    ((v.abs() / ulp).round_ties_even() * ulp).copysign(v)
}

/// (backdrop, source, opacity) as straight RGBA bytes. Chosen to hit both
/// branches of Hard/Soft Light (Cs ≤ ½ and > ½, Cb ≤ ¼), Cb = 0 / 1 (Color
/// Dodge / Burn special cases), partial alpha on both sides, and opacity < 1.
const PAIRS: [([u8; 4], [u8; 4], f32); 5] = [
    ([51, 128, 204, 255], [230, 102, 26, 255], 1.0),
    ([153, 77, 26, 179], [64, 191, 128, 153], 0.8),
    ([255, 0, 128, 255], [128, 128, 128, 128], 1.0),
    ([13, 242, 102, 102], [179, 51, 250, 255], 0.5),
    ([64, 32, 200, 255], [20, 240, 60, 200], 0.65),
];

#[test]
fn all_twelve_modes_match_the_cpu_reference() {
    let (w, h) = (4, 4);
    let mut r = renderer();
    let c = r.compositor();
    let p = params(srgb_pipeline());
    let mut worst = [0i32; 12];
    for mode in 0..12u32 {
        for (bd, src, opacity) in PAIRS {
            c.begin_frame(&sdr_spec(w, h)).unwrap();
            let lb = c
                .render_layer(LayerKey(1), LayerSource::Rgba(&solid_rgba(w, h, bd)), &p)
                .unwrap();
            c.composite(lb, NORMAL).unwrap();
            let ls = c
                .render_layer(LayerKey(2), LayerSource::Rgba(&solid_rgba(w, h, src)), &p)
                .unwrap();
            c.composite(ls, op(mode, opacity)).unwrap();
            let got = px(&c.finish_to_rgba8().unwrap(), w, 1, 2);

            // Mirror the GPU's storage: layer slots hold f16(premultiplied),
            // the result is stored f16, then shown over black.
            let (b, s) = (straight(bd), straight(src));
            let bd_pm = [b[0] * b[3], b[1] * b[3], b[2] * b[3], b[3]].map(f16q);
            let src_pm = [s[0] * s[3], s[1] * s[3], s[2] * s[3], s[3]]
                .map(f16q)
                .map(|v| v * opacity as f64);
            let want = blend_pm_ref(mode, bd_pm, src_pm).map(f16q);
            for ch in 0..3 {
                let want = (want[ch].clamp(0.0, 1.0) * 255.0).round() as i32;
                let d = (got[ch] as i32 - want).abs();
                worst[mode as usize] = worst[mode as usize].max(d);
                assert!(
                    d <= 1,
                    "{} bd={bd:?} src={src:?} op={opacity} ch{ch}: got {} want {want}",
                    MODES[mode as usize],
                    got[ch]
                );
            }
            assert_eq!(got[3], 255, "output alpha");
        }
    }
    eprintln!(
        "blend modes vs CPU reference, max |diff| per mode: {:?}",
        MODES.iter().zip(worst).collect::<Vec<_>>()
    );
}

#[test]
fn unknown_blend_mode_composites_as_normal() {
    let (w, h) = (4, 4);
    let mut r = renderer();
    let c = r.compositor();
    let p = params(srgb_pipeline());
    let mut run = |mode| {
        c.begin_frame(&sdr_spec(w, h)).unwrap();
        let lb = c
            .render_layer(
                LayerKey(1),
                LayerSource::Rgba(&solid_rgba(w, h, [40, 90, 200, 255])),
                &p,
            )
            .unwrap();
        c.composite(lb, NORMAL).unwrap();
        let ls = c
            .render_layer(
                LayerKey(2),
                LayerSource::Rgba(&solid_rgba(w, h, [250, 60, 10, 140])),
                &p,
            )
            .unwrap();
        c.composite(ls, op(mode, 0.7)).unwrap();
        px(&c.finish_to_rgba8().unwrap(), w, 2, 2)
    };
    assert_eq!(run(99), run(0));
}

/// White (YUV 235) layers through Rec.709 → linear Rec.709 → Rec.2020 PQ;
/// returns the centre pixel's R code (u16).
fn hdr_r_code(layers: &[(u8, u32, f32)]) -> u16 {
    let (w, h) = (8, 8);
    let pipeline = vance_color::Pipeline {
        input: vance_color::ColorSpace::Rec709Limited,
        working: vance_color::ColorSpace::LinearRec709,
        output: vance_color::ColorSpace::Rec2020Pq,
    }
    .to_params()
    .unwrap();
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&hdr_spec(w, h, &pipeline)).unwrap();
    for (i, &(y, mode, opacity)) in layers.iter().enumerate() {
        let f = solid_yuv420p(w, h, y, 128, 128);
        let l = c
            .render_layer(LayerKey(i as u128), LayerSource::Yuv(&f), &params(pipeline))
            .unwrap();
        c.composite(l, op(mode, opacity)).unwrap();
    }
    let buf = c.finish_to_rgba64().unwrap();
    let i = ((4 * w + 4) * 8) as usize;
    u16::from_le_bytes([buf[i], buf[i + 1]])
}

fn pq_code(linear: f64) -> i32 {
    // ST 2084 with working 1.0 = 100 cd/m² (as the output pass).
    let (m1, m2, c1, c2, c3) = (0.1593017578125, 78.84375, 0.8359375, 18.8515625, 18.6875);
    let y = (linear * 100.0 / 10000.0).clamp(0.0, 1.0).powf(m1);
    ((((c1 + c2 * y) / (1.0 + c3 * y)).powf(m2)) * 65535.0).round() as i32
}

#[test]
fn hdr_composites_in_linear_light_and_keeps_values_above_one() {
    let one = hdr_r_code(&[(235, 0, 1.0)]) as i32;
    assert!(
        (one - pq_code(1.0)).abs() < 300,
        "white = PQ(100 nits): {one} vs {}",
        pq_code(1.0)
    );
    // Add: 1.0 + 1.0 = 2.0 linear (200 nits) survives the accumulator.
    let two = hdr_r_code(&[(235, 0, 1.0), (235, 1, 1.0)]) as i32;
    assert!(
        (two - pq_code(2.0)).abs() < 300,
        "white + white = PQ(200 nits): {two} vs {}",
        pq_code(2.0)
    );
    assert!(two > one + 2000, "above reference white: {two} vs {one}");
    // Screen inside [0, 1] is the W3C formula: white screen white = 1.0.
    let screen = hdr_r_code(&[(235, 0, 1.0), (235, 3, 1.0)]) as i32;
    assert!(
        (screen - one).abs() < 64,
        "white screen white = 1.0: {screen} vs {one}"
    );
    // Decision 4: Screen does not clamp in HDR, so a highlight survives it.
    // Ten white Adds build a 10.0 (1000-nit) backdrop.
    let mut ten = vec![(235u8, 0u32, 1.0f32)];
    ten.extend(std::iter::repeat_n((235, 1, 1.0), 9));
    let highlight = hdr_r_code(&ten) as i32;
    assert!(
        (highlight - pq_code(10.0)).abs() < 300,
        "backdrop 10.0: {highlight} vs {}",
        pq_code(10.0)
    );
    let mut black_screen = ten.clone();
    black_screen.push((16, 3, 1.0));
    let kept = hdr_r_code(&black_screen) as i32;
    assert!(
        (kept - highlight).abs() < 64,
        "black Screen over 10.0 leaves 10.0: {kept} vs {highlight}"
    );
    // Above 1.0 Screen is max(Cb, Cs): a white Screen layer never darkens it.
    let mut white_screen = ten;
    white_screen.push((235, 3, 1.0));
    let kept = hdr_r_code(&white_screen) as i32;
    assert!(
        (kept - highlight).abs() < 64,
        "white Screen over 10.0 leaves 10.0: {kept} vs {highlight}"
    );
    // Overlay still clamps: black Overlay over 10.0 sees a backdrop of 1.0,
    // so B = Screen(0, 2·1 − 1) = 1.0.
    let mut overlay = vec![(235u8, 0u32, 1.0f32)];
    overlay.extend(std::iter::repeat_n((235, 1, 1.0), 9));
    overlay.push((16, 4, 1.0));
    let o = hdr_r_code(&overlay) as i32;
    assert!(
        (o - one).abs() < 64,
        "overlay clamps its inputs: {o} vs {one}"
    );
    // 50% white over black blends in LINEAR light for HDR output (0.5 linear),
    // unlike SDR's display-space midpoint.
    let half = hdr_r_code(&[(16, 0, 1.0), (235, 0, 0.5)]) as i32;
    assert!(
        (half - pq_code(0.5)).abs() < 300,
        "50% in linear light: {half} vs {}",
        pq_code(0.5)
    );
}

#[test]
fn sdr_add_clips_at_white() {
    let (w, h) = (4, 4);
    let mut r = renderer();
    let c = r.compositor();
    let p = params(srgb_pipeline());
    c.begin_frame(&sdr_spec(w, h)).unwrap();
    for (i, px_) in [[200u8, 100, 30, 255], [150, 100, 250, 255]]
        .into_iter()
        .enumerate()
    {
        let l = c
            .render_layer(
                LayerKey(i as u128),
                LayerSource::Rgba(&solid_rgba(w, h, px_)),
                &p,
            )
            .unwrap();
        c.composite(l, op(if i == 0 { 0 } else { 1 }, 1.0)).unwrap();
    }
    assert_eq!(
        px(&c.finish_to_rgba8().unwrap(), w, 0, 0),
        [255, 200, 255, 255]
    );
}
