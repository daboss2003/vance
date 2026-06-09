//! Phase 8.5 — end-to-end render: real fixture clip → renderer →
//! encoder → ProRes MOV. Verifies via ffprobe that the output is a
//! valid ProRes file at the configured dimensions and that the file
//! is larger than what the synthetic test-pattern v0 would emit
//! (regression guard against silently regressing to the pattern
//! loop).
//!
//! Skips with `eprintln!` when:
//! - `ffmpeg` / `ffprobe` aren't on PATH
//! - the encoder sidecar binary isn't built
//! - the decoder sidecar binary isn't built
//! - the decoder fixture is missing

use std::path::PathBuf;
use std::process::Command;
use vance_app_lib::engine::{Engine, RenderOptions};
use vance_encoder::EncoderPreset;

fn fixture_path() -> PathBuf {
    // Reuse the same fixture the decoder tests rely on so we don't
    // duplicate test assets.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4")
}

fn ffmpeg_present() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

fn ffprobe_present() -> bool {
    Command::new("ffprobe").arg("-version").output().is_ok()
}

#[test]
fn render_sequence_with_one_clip_produces_valid_prores() {
    if !ffmpeg_present() || !ffprobe_present() {
        eprintln!("ffmpeg / ffprobe not present — skipping render_sequence test");
        return;
    }
    let fixture = fixture_path();
    if !fixture.exists() {
        eprintln!("fixture missing: {} — skipping", fixture.display());
        return;
    }
    // Sidecar binaries must be built. cargo's CARGO_BIN_EXE_<name>
    // points to the test crate's own bins; the engine binaries live
    // in target/<profile>/.
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug");
    let dec_bin = target_dir.join(if cfg!(windows) { "vance-decoder.exe" } else { "vance-decoder" });
    let enc_bin = target_dir.join(if cfg!(windows) { "vance-encoder.exe" } else { "vance-encoder" });
    if !dec_bin.exists() || !enc_bin.exists() {
        eprintln!(
            "decoder/encoder binaries missing under {} — run `cargo build -p vance-decoder -p vance-encoder`",
            target_dir.display()
        );
        return;
    }

    // Run from the workspace root so engine's locate_*_binary calls
    // (which check target/debug + target/release relative paths) find
    // the binaries.
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).expect("chdir to workspace root");

    let mut engine = Engine::default();
    engine.new_project("test".into()).expect("new project");
    let media = engine
        .import_media_file(&fixture.canonicalize().unwrap().to_string_lossy())
        .expect("import media");
    let media_id =
        vance_timeline::MediaItemId::from_uuid(media.id.parse::<uuid::Uuid>().unwrap());

    // Find V1 via the public SequenceContents projection.
    let track_id = {
        let contents = engine.current_sequence_contents().expect("sequence contents");
        let v1 = contents
            .tracks
            .iter()
            .find(|t| t.kind == "video")
            .expect("V1");
        vance_timeline::TrackId::from_uuid(v1.id.parse::<uuid::Uuid>().unwrap())
    };

    engine
        .insert_clip_at(track_id, media_id, 0.0)
        .expect("insert clip at 0");

    let out = std::env::temp_dir().join(format!(
        "vance-renderseq-{}.mov",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&out);

    let outcome = engine
        .render_sequence(RenderOptions {
            output_path: out.to_string_lossy().to_string(),
            preset: EncoderPreset::ProRes422HQ,
            width: Some(320),
            height: Some(240),
            render_and_replace_track: None,
            // Half-second slice for fast tests.
            start_seconds: Some(0.0),
            end_seconds: Some(0.5),
        })
        .expect("render_sequence");

    // 24 fps × 0.5 s = 12 frames.
    assert!(
        outcome.video_frames >= 10 && outcome.video_frames <= 14,
        "video frame count out of expected range: {}",
        outcome.video_frames
    );
    let meta = std::fs::metadata(&out).expect("output exists");
    assert!(meta.len() > 10_000, "output suspiciously small: {} bytes", meta.len());

    // ffprobe sanity.
    let probe = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=codec_name,width,height")
        .arg("-of")
        .arg("default=nw=1:nk=1")
        .arg(&out)
        .output()
        .expect("ffprobe");
    let s = String::from_utf8_lossy(&probe.stdout);
    let mut lines = s.lines();
    assert_eq!(lines.next().unwrap_or(""), "prores");
    assert_eq!(lines.next().unwrap_or(""), "320");
    assert_eq!(lines.next().unwrap_or(""), "240");

    let _ = std::fs::remove_file(&out);
}
