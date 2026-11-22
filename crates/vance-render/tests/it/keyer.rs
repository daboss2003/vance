//! `keyer` effect cases (ADR 0007): primary-channel colour difference,
//! spill suppression, choke / soften in source pixels, invert, matte view.
//! Alpha is read back through `read_layer_rgba8` (straight RGBA8).

use vance_render::{KeyerParams, LayerKey, LayerParams, LayerSource, RgbaImage};

use crate::compositing::{
    default_pipeline, params, px, renderer, sdr_spec, solid_rgba, solid_yuv420p, srgb_pipeline,
    NORMAL,
};

const GREEN: [u8; 4] = [0, 255, 0, 255];
/// A light skin tone: G below the R/B average, so the green key leaves it
/// fully opaque.
const SKIN: [u8; 4] = [230, 180, 160, 255];
const SIZE: u32 = 32;
/// The foreground square spans `[LO, HI)` on both axes.
const LO: u32 = 10;
const HI: u32 = 22;

/// The effect's defaults (`vance_timeline::keyer`; Clip White 0.9 per
/// decision 3).
fn keyer() -> KeyerParams {
    KeyerParams {
        key_rgb: [0.0, 1.0, 0.0],
        balance: 0.5,
        strength: 1.0,
        clip_black: 0.0,
        clip_white: 0.9,
        choke_px: 0.0,
        soften_px: 0.0,
        spill: 1.0,
        spill_bias: 0.5,
        invert: false,
    }
}

/// A skin square on a pure green screen.
fn plate() -> RgbaImage {
    let mut img = solid_rgba(SIZE, SIZE, GREEN);
    for y in LO..HI {
        for x in LO..HI {
            let i = ((y * SIZE + x) * 4) as usize;
            img.pixels[i..i + 4].copy_from_slice(&SKIN);
        }
    }
    img
}

/// Key `img` with `k` (and `matte_view`) and read the layer back.
fn keyed(img: &RgbaImage, k: KeyerParams, matte_view: bool) -> RgbaImage {
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(img.width, img.height)).unwrap();
    let p = LayerParams {
        keyer: Some(k),
        matte_view,
        ..params(srgb_pipeline())
    };
    let l = c
        .render_layer(LayerKey(1), LayerSource::Rgba(img), &p)
        .unwrap();
    let out = c.read_layer_rgba8(l).unwrap();
    c.composite(l, NORMAL).unwrap();
    c.finish_to_rgba8().unwrap();
    out
}

fn alpha(img: &RgbaImage, x: u32, y: u32) -> u8 {
    px(&img.pixels, img.width, x, y)[3]
}

/// Opaque (alpha > 127) pixels along row `y`.
fn opaque_run(img: &RgbaImage, y: u32) -> u32 {
    (0..img.width).filter(|&x| alpha(img, x, y) > 127).count() as u32
}

#[test]
fn key_colour_is_transparent_and_skin_is_opaque() {
    let out = keyed(&plate(), keyer(), false);
    assert_eq!(alpha(&out, 2, 2), 0, "pure key colour");
    assert_eq!(alpha(&out, 16, 16), 255, "skin tone");
    // The keyed-in skin keeps its colour (no spill to remove).
    let s = px(&out.pixels, SIZE, 16, 16);
    for ch in 0..3 {
        assert!(
            (s[ch] as i32 - SKIN[ch] as i32).abs() <= 1,
            "skin colour {s:?}"
        );
    }
}

#[test]
fn off_key_screen_colour_keys_to_its_own_key() {
    // A real, imperfect screen colour keys out when used as the key colour.
    let screen = [40u8, 200, 60, 255];
    let img = solid_rgba(8, 8, screen);
    let k = KeyerParams {
        key_rgb: [40.0 / 255.0, 200.0 / 255.0, 60.0 / 255.0],
        ..keyer()
    };
    assert_eq!(alpha(&keyed(&img, k, false), 4, 4), 0);
    // And a half-way colour lands mid-matte.
    let mid = solid_rgba(8, 8, [120, 200, 120, 255]);
    let a = alpha(&keyed(&mid, k, false), 4, 4);
    assert!((60..200).contains(&a), "partial key: {a}");
}

#[test]
fn yuv_sources_key_on_their_encoded_colour() {
    // Y/U/V 145/54/34 decodes (Rec.709 limited) to R'G'B' ≈ (0, 0.847, 0).
    let f = solid_yuv420p(16, 16, 145, 54, 34);
    let (y, u, v) = (
        (145.0 - 16.0) / 219.0,
        (54.0 - 128.0) / 224.0,
        (34.0 - 128.0) / 224.0,
    );
    let key = [
        (y + 1.5748f32 * v).clamp(0.0, 1.0),
        (y - 0.1873 * u - 0.4681 * v).clamp(0.0, 1.0),
        (y + 1.8556 * u).clamp(0.0, 1.0),
    ];
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(16, 16)).unwrap();
    let p = LayerParams {
        keyer: Some(KeyerParams {
            key_rgb: key,
            ..keyer()
        }),
        ..params(default_pipeline())
    };
    let l = c
        .render_layer(LayerKey(1), LayerSource::Yuv(&f), &p)
        .unwrap();
    let out = c.read_layer_rgba8(l).unwrap();
    assert!(
        alpha(&out, 8, 8) <= 2,
        "keyed YUV plate alpha {}",
        alpha(&out, 8, 8)
    );
    c.finish_to_rgba8().unwrap();
}

#[test]
fn spill_suppression_pulls_green_down_on_an_edge_pixel() {
    // A green-contaminated edge colour (partially keyed).
    let edge = solid_rgba(8, 8, [150, 210, 120, 255]);
    let with = keyed(&edge, keyer(), false);
    let without = keyed(
        &edge,
        KeyerParams {
            spill: 0.0,
            ..keyer()
        },
        false,
    );
    let (pw, po) = (px(&with.pixels, 8, 4, 4), px(&without.pixels, 8, 4, 4));
    assert!(po[1] >= 208, "spill 0 keeps green: {po:?}");
    assert!(
        pw[1] + 40 < po[1],
        "spill suppression reduces green: {pw:?} vs {po:?}"
    );
    // Limit = mix(avg(R,B), max(R,B), 0.5) = 0.559 → ≈ 143.
    assert!((pw[1] as i32 - 143).abs() <= 2, "despilled green {pw:?}");
    // R and B are untouched; the matte itself is the same.
    assert_eq!((pw[0], pw[2]), (po[0], po[2]));
    assert_eq!(pw[3], po[3]);
    // Matte 1 − 0.294 = 0.706, over Clip White 0.9 → 0.784 ≈ 200.
    assert!(
        (190..210).contains(&pw[3]),
        "edge is partially keyed: {}",
        pw[3]
    );
}

#[test]
fn choke_erodes_and_dilates_in_source_pixels() {
    let base = keyed(&plate(), keyer(), false);
    assert_eq!(opaque_run(&base, 16), HI - LO);
    let choked = keyed(
        &plate(),
        KeyerParams {
            choke_px: 2.0,
            ..keyer()
        },
        false,
    );
    let run = opaque_run(&choked, 16);
    assert!(
        (HI - LO - 5..=HI - LO - 3).contains(&run),
        "2 px choke: {} → {run}",
        HI - LO
    );
    assert_eq!(alpha(&choked, 16, 16), 255, "the core stays opaque");
    assert_eq!(alpha(&choked, LO, 16), 0, "the old edge is eroded");
    let grown = keyed(
        &plate(),
        KeyerParams {
            choke_px: -2.0,
            ..keyer()
        },
        false,
    );
    let run = opaque_run(&grown, 16);
    assert!(
        (HI - LO + 3..=HI - LO + 5).contains(&run),
        "2 px dilate: {run}"
    );
}

#[test]
fn soften_gives_intermediate_alpha_at_the_edge() {
    let soft = keyed(
        &plate(),
        KeyerParams {
            soften_px: 3.0,
            ..keyer()
        },
        false,
    );
    let edge = alpha(&soft, LO, 16);
    assert!((40..215).contains(&edge), "softened edge alpha {edge}");
    let outside = alpha(&soft, LO - 1, 16);
    assert!(
        outside > 0 && outside < edge,
        "blur spills outward: {outside} < {edge}"
    );
    assert_eq!(alpha(&soft, 16, 16), 255, "core 6 px from the edge");
    assert_eq!(alpha(&soft, 2, 16), 0, "far background");
}

#[test]
fn invert_flips_the_matte() {
    let inv = keyed(
        &plate(),
        KeyerParams {
            invert: true,
            ..keyer()
        },
        false,
    );
    assert_eq!(alpha(&inv, 2, 2), 255, "the screen is now kept");
    assert_eq!(alpha(&inv, 16, 16), 0, "the skin is now keyed out");
}

#[test]
fn matte_view_renders_the_matte_opaque() {
    let m = keyed(
        &plate(),
        KeyerParams {
            soften_px: 2.0,
            ..keyer()
        },
        true,
    );
    assert_eq!(
        px(&m.pixels, SIZE, 16, 16),
        [255, 255, 255, 255],
        "foreground white"
    );
    assert_eq!(
        px(&m.pixels, SIZE, 2, 2),
        [0, 0, 0, 255],
        "background black"
    );
    let e = px(&m.pixels, SIZE, LO, 16);
    assert!(
        e[0] > 30 && e[0] < 225 && e[0] == e[1] && e[1] == e[2],
        "grey edge {e:?}"
    );
    assert_eq!(e[3], 255, "matte view is opaque");
}

#[test]
fn a_neutral_key_colour_is_inert_and_suppresses_no_spill() {
    // Decision 6: a grey-card key has no primary excess, so it neither keys
    // nor despills (skin used to lose its red: (230,180,160) → (175,180,160)).
    let out = keyed(
        &plate(),
        KeyerParams {
            key_rgb: [0.5, 0.5, 0.5],
            ..keyer()
        },
        false,
    );
    for (x, y, want) in [(16, 16, SKIN), (2, 2, GREEN)] {
        let got = px(&out.pixels, SIZE, x, y);
        for ch in 0..4 {
            assert!(
                (got[ch] as i32 - want[ch] as i32).abs() <= 1,
                "inert key at ({x}, {y}): {got:?} vs {want:?}"
            );
        }
    }
}

#[test]
fn an_inverted_key_keeps_the_screen_colour() {
    // Decision 6: no spill suppression on an inverted key (it used to pull
    // the kept green down to black).
    let inv = keyed(
        &plate(),
        KeyerParams {
            invert: true,
            ..keyer()
        },
        false,
    );
    let g = px(&inv.pixels, SIZE, 2, 2);
    assert_eq!(g[3], 255, "the screen is kept");
    assert!(g[1] >= 254 && g[0] <= 1 && g[2] <= 1, "kept green: {g:?}");
}

#[test]
fn a_large_choke_erodes_a_thin_gap_anywhere_within_its_radius() {
    // A 2 px × 6 px hole in an opaque plate, 30 px right of the probe (and
    // one 21 px along each axis on the diagonal): a 40 px choke must reach
    // both — not only gaps that cross a sampled ring.
    let (w, h) = (128u32, 96u32);
    let (cx, cy) = (40u32, 40u32);
    let mut img = solid_rgba(w, h, SKIN);
    let mut hole = |x0: u32, y0: u32| {
        for y in y0..y0 + 6 {
            for x in x0..x0 + 2 {
                let i = ((y * w + x) * 4) as usize;
                img.pixels[i..i + 4].copy_from_slice(&GREEN);
            }
        }
    };
    hole(cx + 30, cy - 3);
    let choked = |img: &RgbaImage| {
        keyed(
            img,
            KeyerParams {
                choke_px: 40.0,
                ..keyer()
            },
            false,
        )
    };
    let out = choked(&img);
    assert_eq!(alpha(&out, cx, cy), 0, "30 px from the gap");
    assert_eq!(alpha(&out, cx + 31, cy + 30), 0, "straight below the gap");
    assert_eq!(alpha(&out, cx - 15, cy), 255, "46 px away stays opaque");
    let mut img2 = solid_rgba(w, h, SKIN);
    for y in cy + 21..cy + 23 {
        for x in cx + 21..cx + 23 {
            let i = ((y * w + x) * 4) as usize;
            img2.pixels[i..i + 4].copy_from_slice(&GREEN);
        }
    }
    let out = choked(&img2);
    assert_eq!(alpha(&out, cx, cy), 0, "a 2×2 gap on the diagonal (30 px)");
    // Dilate: a 2 px opaque dot grows out to the choke radius.
    let mut dot = solid_rgba(w, h, GREEN);
    for y in cy..cy + 2 {
        for x in cx..cx + 2 {
            let i = ((y * w + x) * 4) as usize;
            dot.pixels[i..i + 4].copy_from_slice(&SKIN);
        }
    }
    let grown = keyed(
        &dot,
        KeyerParams {
            choke_px: -20.0,
            ..keyer()
        },
        false,
    );
    assert_eq!(alpha(&grown, cx + 15, cy), 255, "dilated 15 px along x");
    assert_eq!(
        alpha(&grown, cx + 11, cy + 11),
        255,
        "and 15.6 px diagonally"
    );
    assert_eq!(alpha(&grown, cx + 30, cy), 0, "not 30 px away");
}

/// A 4:3-SAR plate (120×90 stored → 160×90 displayed): a skin rectangle
/// spanning display x 40..120, y 25..65 on green, keyed with `k`, read back
/// on its 160×90 canvas.
fn anamorphic_keyed(k: KeyerParams) -> RgbaImage {
    let (sw, sh) = (120u32, 90u32);
    let mut img = solid_rgba(sw, sh, GREEN);
    for y in 25..65 {
        for x in 30..90 {
            let i = ((y * sw + x) * 4) as usize;
            img.pixels[i..i + 4].copy_from_slice(&SKIN);
        }
    }
    let mut r = renderer();
    let c = r.compositor();
    c.begin_frame(&sdr_spec(160, 90)).unwrap();
    let p = LayerParams {
        keyer: Some(k),
        pixel_aspect: 4.0 / 3.0,
        ..params(srgb_pipeline())
    };
    let l = c
        .render_layer(LayerKey(1), LayerSource::Rgba(&img), &p)
        .unwrap();
    let out = c.read_layer_rgba8(l).unwrap();
    c.finish_to_rgba8().unwrap();
    out
}

#[test]
fn choke_and_soften_are_measured_in_display_pixels() {
    // A 10 px choke shrinks the 80×40 display rectangle by 10 display px on
    // every side → 60×20.
    let out = anamorphic_keyed(KeyerParams {
        choke_px: 10.0,
        ..keyer()
    });
    let run_x = (0..160).filter(|&x| alpha(&out, x, 45) > 127).count() as i32;
    let run_y = (0..90).filter(|&y| alpha(&out, 80, y) > 127).count() as i32;
    assert!((run_x - 60).abs() <= 1, "horizontal: {run_x} display px");
    assert!((run_y - 20).abs() <= 1, "vertical: {run_y} display px");
    // Soften: the 10 %–90 % ramp is as wide across as down.
    let soft = anamorphic_keyed(KeyerParams {
        soften_px: 8.0,
        ..keyer()
    });
    let ramp = |vals: Vec<u8>| {
        let lo = vals.iter().position(|&a| a > 25).unwrap();
        let hi = vals.iter().position(|&a| a > 229).unwrap();
        (hi - lo) as i32
    };
    let wx = ramp((0..80).map(|x| alpha(&soft, x, 45)).collect());
    let wy = ramp((0..45).map(|y| alpha(&soft, 80, y)).collect());
    assert!(
        (wx - wy).abs() <= 1,
        "soften ramp: {wx} across vs {wy} down"
    );
}

/// The exact soften the keyer specifies — a truncated Gaussian, σ = s / 2,
/// taps out to ±ceil(s), clamped at the edges — applied across then down.
fn soften_ref(m: &[f32], w: usize, h: usize, s: f32) -> Vec<f32> {
    let sigma = (s * 0.5).max(1e-3);
    let taps = s.ceil() as i32;
    let weights: Vec<f32> = (0..=taps)
        .map(|i| (-0.5 * (i as f32 / sigma).powi(2)).exp())
        .collect();
    let total = weights[0] + 2.0 * weights[1..].iter().sum::<f32>();
    let pass = |src: &[f32], horizontal: bool| {
        let mut out = vec![0.0; src.len()];
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                let at = |k: i32| {
                    let (xx, yy) = if horizontal {
                        ((x + k).clamp(0, w as i32 - 1), y)
                    } else {
                        (x, (y + k).clamp(0, h as i32 - 1))
                    };
                    src[yy as usize * w + xx as usize]
                };
                let mut acc = weights[0] * at(0);
                for k in 1..=taps {
                    acc += weights[k as usize] * (at(k) + at(-k));
                }
                out[y as usize * w + x as usize] = acc / total;
            }
        }
        out
    };
    pass(&pass(m, true), false)
}

#[test]
fn softens_match_the_exact_gaussian_within_tolerance() {
    // A binary matte (skin keys to exactly 1, the screen to 0) with a 2 px
    // gap. Up to 8 px the soften is the exact kernel (±1 code for float
    // rounding); larger radii are three boxes — within 1.3 % of the exact
    // kernel in 2-D (3.3 codes) plus rounding, clamped once at the edges
    // like the exact taps.
    let (w, h) = (96u32, 64u32);
    let mut img = solid_rgba(w, h, GREEN);
    let mut mask = vec![0.0f32; (w * h) as usize];
    for y in 12..52 {
        for x in 20..80 {
            if x == 50 || x == 51 {
                continue;
            }
            let i = (y * w + x) as usize;
            img.pixels[i * 4..i * 4 + 4].copy_from_slice(&SKIN);
            mask[i] = 1.0;
        }
    }
    for (s, tol) in [(4.0f32, 1i32), (8.0, 1), (12.0, 4), (30.0, 4), (64.0, 4)] {
        let out = keyed(
            &img,
            KeyerParams {
                soften_px: s,
                ..keyer()
            },
            false,
        );
        let want = soften_ref(&mask, w as usize, h as usize, s);
        let mut worst = 0;
        for (i, m) in want.iter().enumerate() {
            let d = (out.pixels[i * 4 + 3] as i32 - (m * 255.0).round() as i32).abs();
            worst = worst.max(d);
        }
        assert!(
            worst <= tol,
            "soften {s}: alpha off by {worst} codes (tolerance {tol})"
        );
    }
}

#[test]
fn a_softened_key_is_exactly_zero_and_one_after_a_soft_matte() {
    // An f32 prefix sum over a run of fractional matte values (a partially
    // keyed subject) rounds differently along the scan's paths, so a window
    // of 0s just past it gets a residue (~3e-6 at soften 10). Zero side: an
    // HDR PQ render over black with the screen colour kept (spill 0), where
    // that residue is ~100 codes. One side: a solid band's straight alpha.
    let (w, h) = (1000u32, 120u32);
    let mut img = solid_rgba(w, h, GREEN);
    let mut paint = |x: u32, y: u32, t: f32| {
        let i = ((y * w + x) * 4) as usize;
        for ch in 0..3 {
            let (g, s) = (GREEN[ch] as f32, SKIN[ch] as f32);
            img.pixels[i + ch] = (g + (s - g) * t).round() as u8;
        }
    };
    for y in 20..60u32 {
        for x in 50..700u32 {
            paint(x, y, 0.2 + 0.6 * ((x * 37 + y * 91) % 97) as f32 / 97.0);
        }
    }
    for y in 64..110u32 {
        for x in 50..900u32 {
            paint(x, y, 1.0);
        }
    }
    let p = vance_color::Pipeline {
        input: vance_color::ColorSpace::Srgb,
        working: vance_color::ColorSpace::LinearRec709,
        output: vance_color::ColorSpace::Rec2020Pq,
    }
    .to_params()
    .unwrap();
    let lp = LayerParams {
        keyer: Some(KeyerParams {
            soften_px: 10.0,
            spill: 0.0,
            ..keyer()
        }),
        ..params(p)
    };
    let mut r = renderer();
    let c = r.compositor();
    let at = |b: &[u8], x: u32, y: u32| {
        let i = ((y * w + x) * 8) as usize;
        [0, 1, 2, 3].map(|k| u16::from_le_bytes([b[i + 2 * k], b[i + 2 * k + 1]]))
    };
    let hdr = crate::compositing::hdr_spec(w, h, &p);
    c.begin_frame(&hdr).unwrap();
    let empty = c.finish_to_rgba64().unwrap();
    let render = |c: &mut vance_render::Compositor, spec: &vance_render::OutputSpec| {
        c.begin_frame(spec).unwrap();
        let l = c
            .render_layer(LayerKey(1), LayerSource::Rgba(&img), &lp)
            .unwrap();
        c.composite(l, NORMAL).unwrap();
        c.finish_to_rgba64().unwrap()
    };
    let keyed = render(c, &hdr);
    for y in 0..44 {
        for x in 760..w {
            assert_eq!(at(&keyed, x, y), at(&empty, x, y), "keyed-out ({x}, {y})");
        }
    }
    for y in 0..h {
        for x in 930..w {
            assert_eq!(at(&keyed, x, y), at(&empty, x, y), "keyed-out ({x}, {y})");
        }
    }
    let straight = render(
        c,
        &vance_render::OutputSpec {
            transparent: true,
            ..hdr
        },
    );
    for y in 80..95 {
        for x in 300..800 {
            assert_eq!(at(&straight, x, y)[3], 65535, "solid ({x}, {y})");
        }
    }
}
