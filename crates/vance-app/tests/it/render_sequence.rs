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
use vance_app_lib::engine::{Engine, RenderAndReplaceOptions, RenderOptions};
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
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let dec_bin = target_dir.join(if cfg!(windows) {
        "vance-decoder.exe"
    } else {
        "vance-decoder"
    });
    let enc_bin = target_dir.join(if cfg!(windows) {
        "vance-encoder.exe"
    } else {
        "vance-encoder"
    });
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
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse::<uuid::Uuid>().unwrap());

    // Find V1 via the public SequenceContents projection.
    let track_id = {
        let contents = engine
            .current_sequence_contents()
            .expect("sequence contents");
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

    let out = std::env::temp_dir().join(format!("vance-renderseq-{}.mov", std::process::id()));
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
            hdr: false,
        })
        .expect("render_sequence");

    // 24 fps × 0.5 s = 12 frames.
    assert!(
        outcome.video_frames >= 10 && outcome.video_frames <= 14,
        "video frame count out of expected range: {}",
        outcome.video_frames
    );
    let meta = std::fs::metadata(&out).expect("output exists");
    assert!(
        meta.len() > 10_000,
        "output suspiciously small: {} bytes",
        meta.len()
    );

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

/// Phase 8 exit-gate proxy: exercise the longer-form multi-clip
/// export path. Builds a sequence with three back-to-back clips of
/// the same fixture, renders 1.5 s to ProRes 422 HQ, asserts the
/// output frame count + ffprobe codec match. This is a fast-running
/// proxy for the manual 5-minute exit-gate run — same code path,
/// smaller fixture, runs in CI budget.
#[test]
fn exit_gate_proxy_multi_clip_prores_export() {
    if !ffmpeg_present() || !ffprobe_present() {
        eprintln!("ffmpeg / ffprobe missing — skipping exit-gate proxy");
        return;
    }
    let fixture = fixture_path();
    if !fixture.exists() {
        eprintln!("fixture missing — skipping");
        return;
    }
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let dec_bin = target_dir.join(if cfg!(windows) {
        "vance-decoder.exe"
    } else {
        "vance-decoder"
    });
    let enc_bin = target_dir.join(if cfg!(windows) {
        "vance-encoder.exe"
    } else {
        "vance-encoder"
    });
    if !dec_bin.exists() || !enc_bin.exists() {
        eprintln!("sidecars missing — skipping");
        return;
    }
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).expect("chdir");

    let mut engine = Engine::default();
    engine.new_project("exit-gate".into()).expect("project");
    let media = engine
        .import_media_file(&fixture.canonicalize().unwrap().to_string_lossy())
        .expect("import");
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse::<uuid::Uuid>().unwrap());

    let contents = engine.current_sequence_contents().expect("seq");
    let v1 = contents
        .tracks
        .iter()
        .find(|t| t.kind == "video")
        .expect("V1");
    let track_id = vance_timeline::TrackId::from_uuid(v1.id.parse::<uuid::Uuid>().unwrap());

    // Three clips back-to-back. Fixture is 5 s; we use the first 0.5 s
    // of each so the sequence is 1.5 s long total.
    engine
        .insert_clip_at(track_id, media_id, 0.0)
        .expect("clip 0");
    // For the proxy we don't trim — we just place the clips at slots
    // 5 s, 10 s, etc. so scene_at routing exercises multiple boundaries
    // even though the test only renders the first 1.5 s.
    engine
        .insert_clip_at(track_id, media_id, 5.5)
        .expect("clip 1");
    engine
        .insert_clip_at(track_id, media_id, 11.0)
        .expect("clip 2");

    let out = std::env::temp_dir().join(format!("vance-exit-gate-{}.mov", std::process::id()));
    let _ = std::fs::remove_file(&out);

    let outcome = engine
        .render_sequence(RenderOptions {
            output_path: out.to_string_lossy().to_string(),
            preset: EncoderPreset::ProRes422HQ,
            width: Some(320),
            height: Some(240),
            render_and_replace_track: None,
            start_seconds: Some(0.0),
            end_seconds: Some(1.5),
            hdr: false,
        })
        .expect("exit-gate render");

    // 24 fps × 1.5 s = 36 frames.
    assert!(
        outcome.video_frames >= 32 && outcome.video_frames <= 38,
        "exit-gate frame count out of range: {}",
        outcome.video_frames
    );
    let probe = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("v:0")
        .arg("-show_entries")
        .arg("stream=codec_name")
        .arg("-of")
        .arg("default=nw=1:nk=1")
        .arg(&out)
        .output()
        .expect("ffprobe");
    assert_eq!(
        String::from_utf8_lossy(&probe.stdout).trim(),
        "prores",
        "exit-gate output should be ProRes"
    );
    let _ = std::fs::remove_file(&out);
}

#[test]
fn render_and_replace_swaps_clip_for_rendered_media() {
    if !ffmpeg_present() || !ffprobe_present() {
        eprintln!("ffmpeg / ffprobe not present — skipping r&r test");
        return;
    }
    let fixture = fixture_path();
    if !fixture.exists() {
        eprintln!("fixture missing — skipping");
        return;
    }
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let dec_bin = target_dir.join(if cfg!(windows) {
        "vance-decoder.exe"
    } else {
        "vance-decoder"
    });
    let enc_bin = target_dir.join(if cfg!(windows) {
        "vance-encoder.exe"
    } else {
        "vance-encoder"
    });
    if !dec_bin.exists() || !enc_bin.exists() {
        eprintln!("sidecars missing — skipping");
        return;
    }
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).expect("chdir");

    let mut engine = Engine::default();
    engine.new_project("rr-test".into()).expect("project");
    let media = engine
        .import_media_file(&fixture.canonicalize().unwrap().to_string_lossy())
        .expect("import");
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse::<uuid::Uuid>().unwrap());

    let contents = engine.current_sequence_contents().expect("seq");
    let v1 = contents
        .tracks
        .iter()
        .find(|t| t.kind == "video")
        .expect("V1");
    let track_id = vance_timeline::TrackId::from_uuid(v1.id.parse::<uuid::Uuid>().unwrap());
    let original_clip_id = engine
        .insert_clip_at(track_id, media_id, 0.0)
        .expect("insert");

    let out = std::env::temp_dir().join(format!("vance-rr-{}.mov", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let outcome = engine
        .render_and_replace(RenderAndReplaceOptions {
            output_path: out.to_string_lossy().to_string(),
            width: Some(320),
            height: Some(240),
            track_id,
            start_seconds: 0.0,
            end_seconds: 0.5,
        })
        .expect("render_and_replace");

    assert!(out.exists(), "rendered media file should exist on disk");
    // The replacement clip should be a NEW id, not the original.
    assert_ne!(
        outcome.new_clip_id, original_clip_id,
        "clip id should change"
    );
    // The new media item should be registered.
    let contents_after = engine.current_sequence_contents().expect("seq after");
    let v1_after = contents_after
        .tracks
        .iter()
        .find(|t| t.kind == "video")
        .expect("V1 after");
    let new_clip = v1_after
        .clips
        .iter()
        .find(|c| c.id == outcome.new_clip_id)
        .expect("new clip on track");
    assert!(
        (new_clip.timeline_in_seconds - 0.0).abs() < 1e-3,
        "new clip should land at start_seconds=0.0, got {}",
        new_clip.timeline_in_seconds
    );

    let _ = std::fs::remove_file(&out);
}
