//! Compositing throughput (ADR 0007 target: export ≥ 30 fps on an M1 with
//! 4 × 1080p layers). Prints numbers; asserts only that frames come back.
//!
//!   cargo test -p vance-app --test it perf_compositing:: -- --ignored --nocapture
//!
//! Skips (eprintln) without ffmpeg / ffprobe, the sidecars or libx264.

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use crate::compositing_e2e::support::*;
use vance_app_lib::engine::RenderOptions;
use vance_encoder::EncoderPreset;
use vance_timeline::BlendMode;

const FW: u32 = 1920;
const FH: u32 = 1080;
/// Sequence rate of a new project.
const FPS: f64 = 24.0;
const FRAMES: usize = 48;

/// `secs` of 1080p30 H.264 at a typical delivery quality (crf 18), not the
/// lossless encode the correctness tests use: decode cost is part of the
/// measurement.
fn encode_1080p30(out: &Path, src: &str, secs: f64) -> bool {
    let ok = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args(["-i", &format!("{src}=s={FW}x{FH}:r=30:d={secs}")])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "18"])
        .args(["-pix_fmt", "yuv420p", "-g", "30"])
        .arg(out)
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!("ffmpeg could not encode {src} (libx264 missing?) — skipping");
    }
    ok
}

fn fps(frames: usize, secs: f64) -> f64 {
    frames as f64 / secs.max(1e-9)
}

#[test]
#[ignore = "perf: run with --ignored --nocapture"]
fn four_layer_1080p_throughput() {
    let Some(mut rig) = Rig::new("perf-comp") else {
        return;
    };
    // Four different files, so each layer has its own decoder as real
    // footage would.
    let mut media = Vec::new();
    for src in ["testsrc2", "testsrc", "smptehdbars", "rgbtestsrc"] {
        let out = rig.scratch.path(src, "mp4");
        if !encode_1080p30(&out, src, 3.0) {
            return;
        }
        media.push(rig.import(&out));
    }
    let v: Vec<_> = (0..4).map(|n| rig.track(n)).collect();
    let c: Vec<_> = (0..4).map(|n| rig.place(v[n], media[n], 0.0)).collect();
    rig.fx(
        v[1],
        c[1],
        "transform",
        &[("scale_x", 0.5), ("scale_y", 0.5)],
    );
    rig.fx(v[1], c[1], "opacity", &[("value", 0.8)]);
    rig.fx(
        v[2],
        c[2],
        "transform",
        &[("scale_x", 0.75), ("scale_y", 0.75), ("rotation", 10.0)],
    );
    rig.engine
        .set_clip_blend_mode(v[2], c[2], BlendMode::Screen)
        .expect("screen");
    rig.fx(v[3], c[3], "opacity", &[("value", 0.6)]);
    rig.engine
        .set_clip_blend_mode(v[3], c[3], BlendMode::Multiply)
        .expect("multiply");
    rig.canvas(FW, FH);

    // Warm up: decoder spawn, first seek, GPU pipelines.
    rig.export(0.0);
    rig.preview(0.0);

    let t0 = Instant::now();
    for k in 1..=FRAMES {
        rig.export(k as f64 / FPS);
    }
    let export_s = t0.elapsed().as_secs_f64();

    let t0 = Instant::now();
    for k in 1..=FRAMES {
        rig.preview(k as f64 / FPS);
    }
    let preview_s = t0.elapsed().as_secs_f64();

    // Baseline: the same export with only V1 visible.
    for t in &v[1..] {
        rig.engine.set_track_enabled(*t, false).expect("hide");
    }
    rig.export(0.0);
    let t0 = Instant::now();
    for k in 1..=FRAMES {
        rig.export(k as f64 / FPS);
    }
    let single_s = t0.elapsed().as_secs_f64();
    for t in &v[1..] {
        rig.engine.set_track_enabled(*t, true).expect("show");
    }

    let mut renders = Vec::new();
    for (label, preset) in [
        ("ProRes 422 HQ", EncoderPreset::ProRes422HQ),
        ("H.264 crf 23", EncoderPreset::H264 { crf: 23 }),
    ] {
        let out = rig.scratch.path("render", "mov");
        let outcome = rig
            .engine
            .render_sequence(RenderOptions {
                output_path: out.to_string_lossy().to_string(),
                preset,
                width: Some(FW),
                height: Some(FH),
                render_and_replace_track: None,
                start_seconds: Some(0.0),
                end_seconds: Some(FRAMES as f64 / FPS),
                hdr: false,
            })
            .unwrap_or_else(|e| panic!("{label} render_sequence: {e}"));
        assert_eq!(outcome.video_frames, FRAMES as u64, "{label} frames");
        renders.push((
            label,
            fps(FRAMES, outcome.elapsed_seconds),
            outcome.elapsed_seconds,
        ));
    }

    println!("perf_compositing: 4 × 1080p layers (normal, PiP 0.8, screen rotated, multiply 0.6)");
    println!(
        "  export_frame_rgba   {FRAMES} frames in {export_s:.2} s = {:.1} fps ({:.1} ms/frame; target ≥ 30 fps)",
        fps(FRAMES, export_s),
        export_s * 1000.0 / FRAMES as f64
    );
    println!(
        "  export_frame_rgba   1 layer (V1 only): {:.1} fps ({:.1} ms/frame)",
        fps(FRAMES, single_s),
        single_s * 1000.0 / FRAMES as f64
    );
    println!(
        "  preview_frame_rgba  {FRAMES} frames in {preview_s:.2} s = {:.1} fps ({:.1} ms/frame)",
        fps(FRAMES, preview_s),
        preview_s * 1000.0 / FRAMES as f64
    );
    for (label, f, s) in renders {
        println!("  render_sequence     {label}: {FRAMES} frames in {s:.2} s = {f:.1} fps");
    }
}
