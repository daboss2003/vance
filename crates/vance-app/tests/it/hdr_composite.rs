//! An SDR title over PQ footage, exported HDR10 (ADR 0007, decision 2: HDR
//! output composites in linear working space so highlights above 1.0
//! survive).
//!
//! - The file is HEVC Main 10 tagged PQ / BT.2020.
//! - Away from the title, highlights match a single-layer HDR export of the
//!   same clip within ±1 code (10-bit Y', 8×8 block means; see the test).
//! - The title (sRGB white) lands at SDR reference white. Expected value,
//!   from the code: a title is sRGB-encoded RGBA (compose `input_space`), so
//!   white linearises to 1.0 in the working space; Rec.709 → Rec.2020 keeps
//!   white at (1, 1, 1); `output.wgsl` encodes PQ as
//!   `pq_oetf(lin × 100 / 10000)` ("working 1.0 = 100 cd/m²"). So white is
//!   100 nits: PQ E' = 0.5081, 10-bit limited-range Y' = 64 + 876 E' ≈ 509
//!   (BT.2408's 203-nit white would be ≈ 573; full-scale 10000 nits = 940).
//!   Measured: 511 — ffmpeg's rgba64le → yuv420p10le conversion in the
//!   encoder lands E' = 0.5081 at 511 by itself — hence ±4.
//!
//! Skips (eprintln) without ffmpeg / ffprobe, the sidecars or the fixture.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::compositing_e2e::support::*;
use vance_app_lib::engine::RenderOptions;
use vance_encoder::EncoderPreset;

const FW: u32 = 320;
const FH: u32 = 240;
/// 6 frames at 24 fps.
const END: f64 = 0.25;

/// The title card's placement: scale 0.25 around the centre, moved by
/// (−0.3 W, −0.3 H) → centre (64, 48), 80 × 60.
const TITLE_X: (u32, u32) = (24, 104);
const TITLE_Y: (u32, u32) = (18, 78);

fn hdr_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../vance-decoder/tests/fixtures/hdr10_320x240_2s.mp4")
}

fn probe(path: &Path, field: &str) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-select_streams",
            "v:0",
        ])
        .args(["-show_entries", &format!("stream={field}")])
        .args(["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// SMPTE ST 2084 inverse EOTF: absolute luminance (cd/m²) → E' in [0, 1].
fn pq_encode(nits: f64) -> f64 {
    let (m1, m2) = (2610.0 / 16384.0, 2523.0 / 4096.0 * 128.0);
    let (c1, c2, c3) = (
        3424.0 / 4096.0,
        2413.0 / 4096.0 * 32.0,
        2392.0 / 4096.0 * 32.0,
    );
    let l = (nits / 10000.0).powf(m1);
    ((c1 + c2 * l) / (1.0 + c3 * l)).powf(m2)
}

/// One decoded 4:2:0 10-bit frame.
struct Yuv {
    y: Vec<u16>,
    u: Vec<u16>,
    v: Vec<u16>,
}

impl Yuv {
    fn y(&self, x: u32, y: u32) -> u16 {
        self.y[(y * FW + x) as usize]
    }
    fn uv(&self, x: u32, y: u32) -> (u16, u16) {
        let i = ((y / 2) * (FW / 2) + x / 2) as usize;
        (self.u[i], self.v[i])
    }
}

fn decode_yuv(path: &Path) -> Vec<Yuv> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p10le",
            "-",
        ])
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "decode {}", path.display());
    let words: Vec<u16> = out
        .stdout
        .chunks_exact(2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect();
    let (luma, chroma) = ((FW * FH) as usize, (FW * FH / 4) as usize);
    words
        .chunks_exact(luma + 2 * chroma)
        .map(|f| Yuv {
            y: f[..luma].to_vec(),
            u: f[luma..luma + chroma].to_vec(),
            v: f[luma + chroma..].to_vec(),
        })
        .collect()
}

fn export_hdr(rig: &mut Rig, label: &str) -> PathBuf {
    let out = rig.scratch.path(label, "mov");
    let outcome = rig
        .engine
        .render_sequence(RenderOptions {
            output_path: out.to_string_lossy().to_string(),
            // crf 0: the least encoder noise the HDR10 path (x265) offers.
            preset: EncoderPreset::Hevc { crf: 0 },
            width: Some(FW),
            height: Some(FH),
            render_and_replace_track: None,
            start_seconds: Some(0.0),
            end_seconds: Some(END),
            hdr: true,
        })
        .unwrap_or_else(|e| panic!("{label} HDR render_sequence: {e}"));
    assert_eq!(outcome.video_frames, 6, "{label} frames");
    out
}

#[test]
fn sdr_title_over_pq_exports_hdr10_with_highlights_intact() {
    let Some(mut rig) = Rig::new("hdr-composite") else {
        return;
    };
    let fixture = hdr_fixture();
    if !fixture.exists() {
        eprintln!("HDR fixture missing: {} — skipping", fixture.display());
        return;
    }
    let clip = rig.import(&fixture);
    assert_eq!(
        rig.engine
            .media_get_info(clip)
            .unwrap()
            .input_color_space
            .as_deref(),
        Some("rec2020-pq"),
        "the fixture auto-tags PQ"
    );
    let v1 = rig.track(0);
    rig.place(v1, clip, 0.0);
    rig.canvas(FW, FH);
    let single = export_hdr(&mut rig, "single");

    let title = rig.title("sdr-white", card([1.0; 3], FW, FH), 2.0);
    let v2 = rig.track(1);
    let c = rig.place(v2, title, 0.0);
    rig.fx(
        v2,
        c,
        "transform",
        &[
            ("scale_x", 0.25),
            ("scale_y", 0.25),
            ("translate_x", -0.3),
            ("translate_y", -0.3),
        ],
    );
    let dual = export_hdr(&mut rig, "with-title");

    for (field, want) in [
        ("codec_name", "hevc"),
        ("pix_fmt", "yuv420p10le"),
        ("color_transfer", "smpte2084"),
        ("color_primaries", "bt2020"),
        ("color_space", "bt2020nc"),
    ] {
        assert_eq!(probe(&dual, field), want, "HDR10 tag {field}");
    }
    assert!(
        probe(&dual, "profile").contains("Main 10"),
        "Main 10 profile"
    );
    let full_range = probe(&dual, "color_range") == "pc";

    let (a, b) = (decode_yuv(&single), decode_yuv(&dual));
    assert_eq!(a.len(), 6, "single-layer frames");
    assert_eq!(b.len(), 6, "two-layer frames");

    // Highlights well above SDR white (Y' 509), away from the title by more
    // than a coding block, match the single-layer export.
    // Intentional: ±1 code on 8×8 block means, not per pixel. The HDR path
    // is always lossy x265 (crf 0 is the floor), and two encodes whose
    // inputs differ only inside the title still differ by up to ~13 codes
    // per pixel elsewhere (rate control / AQ move the QPs). That noise is
    // zero-mean: measured 8×8 means stay within 0.9 codes, while a clipped
    // or tone-mapped highlight moves them by hundreds.
    let near_title = |x: u32, y: u32| {
        (x + 16 >= TITLE_X.0 && x < TITLE_X.1 + 16) && (y + 16 >= TITLE_Y.0 && y < TITLE_Y.1 + 16)
    };
    const B: u32 = 8;
    let mut blocks = 0usize;
    let mut peak = 0u16;
    for (n, (fa, fb)) in a.iter().zip(&b).enumerate() {
        peak = peak.max(*fa.y.iter().max().unwrap());
        for by in (0..FH).step_by(B as usize) {
            for bx in (0..FW).step_by(B as usize) {
                let px: Vec<(u32, u32)> = (by..by + B)
                    .flat_map(|y| (bx..bx + B).map(move |x| (x, y)))
                    .collect();
                if px
                    .iter()
                    .any(|&(x, y)| fa.y(x, y) < 700 || near_title(x, y))
                {
                    continue;
                }
                blocks += 1;
                let mean = |f: &Yuv| px.iter().map(|&(x, y)| f.y(x, y) as f64).sum::<f64>() / 64.0;
                let (ma, mb) = (mean(fa), mean(fb));
                assert!(
                    (ma - mb).abs() <= 1.0,
                    "frame {n} block ({bx},{by}): highlight mean Y' {mb:.2} with the title vs \
                     {ma:.2} without"
                );
                for &(x, y) in &px {
                    let (ya, yb) = (fa.y(x, y), fb.y(x, y));
                    assert!(
                        ya.abs_diff(yb) <= 16,
                        "frame {n} ({x},{y}): highlight Y' {yb} with the title vs {ya} without \
                         (beyond encoder noise)"
                    );
                }
            }
        }
    }
    assert!(blocks > 500, "too few highlight blocks checked: {blocks}");
    // Highlights survive the HDR path (PQ 888 / 940 in the fixture ≈ 4000+
    // nits); a clip at reference white would cap them near 509.
    assert!(peak > 850, "HDR highlights clipped: peak Y' {peak}");

    // The title interior is SDR reference white (100 nits, see module doc).
    let e = pq_encode(100.0);
    let want = if full_range {
        1023.0 * e
    } else {
        64.0 + 876.0 * e
    };
    for (n, f) in b.iter().enumerate() {
        for y in TITLE_Y.0 + 6..TITLE_Y.1 - 6 {
            for x in TITLE_X.0 + 6..TITLE_X.1 - 6 {
                let got = f.y(x, y) as f64;
                assert!(
                    (got - want).abs() <= 4.0,
                    "frame {n} ({x},{y}): title Y' {got}, want SDR white ≈{want:.1} (100 nits)"
                );
                let (u, v) = f.uv(x, y);
                assert!(
                    u.abs_diff(512) <= 4 && v.abs_diff(512) <= 4,
                    "frame {n} ({x},{y}): title chroma ({u},{v}), want neutral 512"
                );
            }
        }
    }
}
