//! HDR end-to-end with REAL media (closes the "verified only numerically"
//! gap): import a real 10-bit HEVC Main10 PQ clip → the decoder opens it (a
//! 10-bit source was previously REJECTED) and its input color space is
//! auto-tagged Rec.2020 PQ → render + HDR10 export → ffprobe confirms the
//! output is HEVC Main10 tagged bt2020 / smpte2084 (PQ).
//!
//! Skips (with eprintln!) when ffmpeg/ffprobe, the sidecars, or the fixture
//! are absent.

use std::path::PathBuf;
use std::process::Command;
use vance_app_lib::engine::{Engine, RenderOptions};
use vance_encoder::EncoderPreset;

fn hdr_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../vance-decoder/tests/fixtures/hdr10_320x240_2s.mp4")
}

fn tool(n: &str) -> bool {
    Command::new(n).arg("-version").output().is_ok()
}

fn sidecars_present() -> bool {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let ext = if cfg!(windows) { ".exe" } else { "" };
    ["vance-decoder", "vance-encoder", "vance-audio-decoder"]
        .iter()
        .all(|b| d.join(format!("{b}{ext}")).exists())
}

/// Read one `stream=<field>` from ffprobe for the first video stream.
fn probe(path: &PathBuf, field: &str) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
        ])
        .arg(format!("stream={field}"))
        .args(["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn hdr10_source_decodes_autotags_and_exports_hdr10() {
    if !tool("ffmpeg") || !tool("ffprobe") {
        eprintln!("ffmpeg/ffprobe absent — skipping HDR e2e");
        return;
    }
    let fixture = hdr_fixture();
    if !fixture.exists() {
        eprintln!("HDR fixture missing: {} — skipping", fixture.display());
        return;
    }
    if !sidecars_present() {
        eprintln!("sidecar binaries missing — run `cargo build --workspace --bins`");
        return;
    }
    // Confirm the fixture really is a 10-bit HDR PQ source (guards against a
    // regenerated/bad asset making the test pass vacuously).
    assert_eq!(probe(&fixture, "pix_fmt"), "yuv420p10le", "fixture must be 10-bit");
    assert_eq!(probe(&fixture, "color_transfer"), "smpte2084", "fixture must be PQ");

    // Engine locates sidecars relative to the workspace root.
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).expect("chdir workspace root");

    let mut engine = Engine::default();
    engine.new_project("hdr".into()).expect("new project");

    // (1) A 10-bit source now IMPORTS (was rejected pre-10-bit-decode) and is
    // auto-tagged Rec.2020 PQ from its transfer VUI.
    let media = engine
        .import_media_file(&fixture.canonicalize().unwrap().to_string_lossy())
        .expect("10-bit HDR source must import (decoder opens it)");
    assert_eq!(
        media.input_color_space.as_deref(),
        Some("rec2020-pq"),
        "HDR source must auto-tag its input color space to Rec.2020 PQ"
    );

    // (2) Place it on V1 and export HDR10.
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse().unwrap());
    let track_id = {
        let contents = engine.current_sequence_contents().expect("contents");
        let v1 = contents.tracks.iter().find(|t| t.kind == "video").expect("V1");
        vance_timeline::TrackId::from_uuid(v1.id.parse().unwrap())
    };
    engine.insert_clip_at(track_id, media_id, 0.0).expect("insert clip");

    let out = std::env::temp_dir().join(format!("vance-hdr-{}.mov", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let outcome = engine
        .render_sequence(RenderOptions {
            output_path: out.to_string_lossy().to_string(),
            preset: EncoderPreset::Hevc { crf: 20 },
            width: Some(320),
            height: Some(240),
            render_and_replace_track: None,
            start_seconds: Some(0.0),
            end_seconds: Some(0.4),
            hdr: true,
        })
        .expect("HDR render_sequence");
    assert!(outcome.video_frames >= 6, "expected ~10 frames, got {}", outcome.video_frames);

    // (3) The output is a real HEVC Main10 file tagged bt2020 / PQ.
    let meta = std::fs::metadata(&out).expect("output exists");
    assert!(meta.len() > 5_000, "output suspiciously small: {} bytes", meta.len());
    assert_eq!(probe(&out, "codec_name"), "hevc", "HDR export must be HEVC");
    assert!(probe(&out, "profile").contains("Main 10"), "must be Main 10 profile");
    assert_eq!(probe(&out, "color_transfer"), "smpte2084", "output must be PQ-tagged");
    assert_eq!(probe(&out, "color_primaries"), "bt2020", "output must be Rec.2020-tagged");
    let _ = std::fs::remove_file(&out);
}
