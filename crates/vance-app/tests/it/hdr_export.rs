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
fn hlg_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../vance-decoder/tests/fixtures/hlg_320x240_2s.mp4")
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
    assert_eq!(
        probe(&fixture, "pix_fmt"),
        "yuv420p10le",
        "fixture must be 10-bit"
    );
    assert_eq!(
        probe(&fixture, "color_transfer"),
        "smpte2084",
        "fixture must be PQ"
    );

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
        let v1 = contents
            .tracks
            .iter()
            .find(|t| t.kind == "video")
            .expect("V1");
        vance_timeline::TrackId::from_uuid(v1.id.parse().unwrap())
    };
    engine
        .insert_clip_at(track_id, media_id, 0.0)
        .expect("insert clip");

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
    assert!(
        outcome.video_frames >= 6,
        "expected ~10 frames, got {}",
        outcome.video_frames
    );

    // (3) The output is a real HEVC Main10 file tagged bt2020 / PQ.
    let meta = std::fs::metadata(&out).expect("output exists");
    assert!(
        meta.len() > 5_000,
        "output suspiciously small: {} bytes",
        meta.len()
    );
    assert_eq!(probe(&out, "codec_name"), "hevc", "HDR export must be HEVC");
    assert!(
        probe(&out, "profile").contains("Main 10"),
        "must be Main 10 profile"
    );
    assert_eq!(
        probe(&out, "color_transfer"),
        "smpte2084",
        "output must be PQ-tagged"
    );
    assert_eq!(
        probe(&out, "color_primaries"),
        "bt2020",
        "output must be Rec.2020-tagged"
    );
    let _ = std::fs::remove_file(&out);
}

/// 4K HDR10 export. One rgba64le 3840×2160 frame is 66 MB, more than an IPC
/// message may carry (`vance_ipc::MAX_FRAME_BYTES`), so this catches a push
/// path that puts pixels inside the message. Three frames keep it quick.
#[test]
fn hdr10_export_at_4k() {
    if !tool("ffmpeg") || !tool("ffprobe") || !sidecars_present() {
        eprintln!("ffmpeg/ffprobe/sidecars absent — skipping 4K HDR export");
        return;
    }
    let fixture = hdr_fixture();
    if !fixture.exists() {
        eprintln!("HDR fixture missing: {} — skipping", fixture.display());
        return;
    }
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).expect("chdir workspace root");

    let mut engine = Engine::default();
    engine.new_project("hdr-4k".into()).expect("new project");
    let media = engine
        .import_media_file(&fixture.canonicalize().unwrap().to_string_lossy())
        .expect("HDR source must import");
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse().unwrap());
    let track_id = {
        let contents = engine.current_sequence_contents().expect("contents");
        let v1 = contents
            .tracks
            .iter()
            .find(|t| t.kind == "video")
            .expect("V1");
        vance_timeline::TrackId::from_uuid(v1.id.parse().unwrap())
    };
    engine
        .insert_clip_at(track_id, media_id, 0.0)
        .expect("insert clip");

    let out = std::env::temp_dir().join(format!("vance-hdr-4k-{}.mov", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let outcome = engine.render_sequence(RenderOptions {
        output_path: out.to_string_lossy().to_string(),
        preset: EncoderPreset::Hevc { crf: 28 },
        width: Some(3840),
        height: Some(2160),
        render_and_replace_track: None,
        start_seconds: Some(0.0),
        // 3 frames at the new sequence's 24 fps (0.125 s is exact in binary).
        end_seconds: Some(0.125),
        hdr: true,
    });
    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_file(&out);
            panic!("4K HDR render_sequence: {e}");
        }
    };
    assert_eq!(outcome.video_frames, 3);

    let streams = [
        ("codec_name", "hevc"),
        ("width", "3840"),
        ("height", "2160"),
        ("pix_fmt", "yuv420p10le"),
        ("color_transfer", "smpte2084"),
        ("color_primaries", "bt2020"),
        ("nb_frames", "3"),
    ]
    .map(|(field, want)| (field, want, probe(&out, field)));
    let profile = probe(&out, "profile");
    let _ = std::fs::remove_file(&out);
    for (field, want, got) in streams {
        assert_eq!(got, want, "4K HDR export {field}");
    }
    assert!(
        profile.contains("Main 10"),
        "must be Main 10, got {profile}"
    );
}

#[test]
fn hlg_source_decodes_and_autotags() {
    if !tool("ffmpeg") || !tool("ffprobe") || !sidecars_present() {
        eprintln!("ffmpeg/ffprobe/sidecars absent — skipping HLG e2e");
        return;
    }
    let fixture = hlg_fixture();
    if !fixture.exists() {
        eprintln!("HLG fixture missing — skipping");
        return;
    }
    assert_eq!(
        probe(&fixture, "color_transfer"),
        "arib-std-b67",
        "fixture must be HLG"
    );
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).expect("chdir");
    let mut engine = Engine::default();
    engine.new_project("hlg".into()).unwrap();
    // A 10-bit HLG source imports and auto-tags Rec.2020 HLG.
    let media = engine
        .import_media_file(&fixture.canonicalize().unwrap().to_string_lossy())
        .expect("HLG source must import");
    assert_eq!(
        media.input_color_space.as_deref(),
        Some("rec2020-hlg"),
        "HLG source must auto-tag Rec.2020 HLG"
    );
}

/// The HDR10 fixture's black (limited-range Y' 64, chroma 512) exports as
/// black. Read as full range it was 64 / 1023 of the PQ signal (≈ 0.1 nit),
/// which comes back out near Y' 119 instead of 64.
#[test]
fn hdr10_black_exports_as_black() {
    if !tool("ffmpeg") || !tool("ffprobe") || !sidecars_present() {
        eprintln!("ffmpeg/ffprobe/sidecars absent — skipping HDR black");
        return;
    }
    let fixture = hdr_fixture();
    if !fixture.exists() {
        eprintln!("HDR fixture missing: {} — skipping", fixture.display());
        return;
    }
    let (w, h) = (320usize, 240usize);
    // The fixture's top-left 12×12 is Y' 64 / chroma 512 in every frame.
    const PATCH: std::ops::Range<usize> = 2..10;
    let decode = |path: &PathBuf| -> Vec<(Vec<u16>, Vec<u16>, Vec<u16>)> {
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
        let (luma, chroma) = (w * h, w * h / 4);
        words
            .chunks_exact(luma + 2 * chroma)
            .map(|f| {
                (
                    f[..luma].to_vec(),
                    f[luma..luma + chroma].to_vec(),
                    f[luma + chroma..].to_vec(),
                )
            })
            .collect()
    };
    for (y, u, v) in decode(&fixture).iter().take(6) {
        for r in PATCH {
            for c in PATCH {
                assert_eq!(y[r * w + c], 64, "fixture black at ({c},{r})");
                assert_eq!(u[(r / 2) * (w / 2) + c / 2], 512);
                assert_eq!(v[(r / 2) * (w / 2) + c / 2], 512);
            }
        }
    }

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).expect("chdir workspace root");
    let mut engine = Engine::default();
    engine.new_project("hdr-black".into()).expect("new project");
    let media = engine
        .import_media_file(&fixture.canonicalize().unwrap().to_string_lossy())
        .expect("import");
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse().unwrap());
    let track_id = {
        let contents = engine.current_sequence_contents().expect("contents");
        let v1 = contents
            .tracks
            .iter()
            .find(|t| t.kind == "video")
            .expect("V1");
        vance_timeline::TrackId::from_uuid(v1.id.parse().unwrap())
    };
    engine
        .insert_clip_at(track_id, media_id, 0.0)
        .expect("insert clip");
    let out = std::env::temp_dir().join(format!("vance-hdr-black-{}.mov", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let outcome = engine.render_sequence(RenderOptions {
        output_path: out.to_string_lossy().to_string(),
        preset: EncoderPreset::Hevc { crf: 0 },
        width: Some(w as u32),
        height: Some(h as u32),
        render_and_replace_track: None,
        start_seconds: Some(0.0),
        end_seconds: Some(0.25),
        hdr: true,
    });
    let frames = outcome.map(|_| decode(&out));
    let full_range = probe(&out, "color_range") == "pc";
    let _ = std::fs::remove_file(&out);
    let frames = frames.expect("HDR render_sequence");
    assert_eq!(frames.len(), 6);
    let black = if full_range { 0 } else { 64 };
    for (n, (y, u, v)) in frames.iter().enumerate() {
        for r in PATCH {
            for c in PATCH {
                let (luma, cb, cr) = (
                    y[r * w + c],
                    u[(r / 2) * (w / 2) + c / 2],
                    v[(r / 2) * (w / 2) + c / 2],
                );
                assert!(
                    luma.abs_diff(black) <= 2 && cb.abs_diff(512) <= 2 && cr.abs_diff(512) <= 2,
                    "frame {n} ({c},{r}): Y'CbCr ({luma},{cb},{cr}), want black ({black},512,512)"
                );
            }
        }
    }
}
