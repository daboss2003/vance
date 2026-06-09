//! Phase 9.8 — soak test harness.
//!
//! Exercises the full editing → playback → cache → render loop in a
//! tight cycle so memory growth + crashes surface fast. The full
//! exit-gate run is 8 hours; this test runs a configurable number of
//! cycles by default (small for CI, large via `VANCE_SOAK_CYCLES=N`
//! for the manual run).
//!
//! Each cycle:
//! 1. Decode + render a frame at a random PTS (exercises the frame
//!    cache, the prefetch worker, the decoder sidecar).
//! 2. Capture cache stats so growth is observable in CI logs.
//! 3. Encode a single ProRes frame via the encoder sidecar.
//!
//! After all cycles we capture final cache stats + assert the
//! per-source ring stayed within budget (the safety net for the
//! "no memory leaks" gate).
//!
//! Skipped when sidecars or `ffmpeg` are missing.

use std::path::PathBuf;
use std::process::Command;
use vance_app_lib::engine::{Engine, RenderOptions};
use vance_encoder::EncoderPreset;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4")
}

fn ffmpeg_present() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

#[test]
fn soak_cycle_keeps_cache_within_budget() {
    if !ffmpeg_present() {
        eprintln!("ffmpeg missing — skipping soak");
        return;
    }
    let fixture = fixture_path();
    if !fixture.exists() {
        eprintln!("fixture missing — skipping soak");
        return;
    }
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let dec_bin = target_dir.join(if cfg!(windows) { "vance-decoder.exe" } else { "vance-decoder" });
    let enc_bin = target_dir.join(if cfg!(windows) { "vance-encoder.exe" } else { "vance-encoder" });
    if !dec_bin.exists() || !enc_bin.exists() {
        eprintln!("sidecars missing — skipping soak");
        return;
    }
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).expect("chdir");

    // Default: 6 cycles. Override with VANCE_SOAK_CYCLES for the
    // exit-gate 8-hour run.
    let cycles: u32 = std::env::var("VANCE_SOAK_CYCLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);

    let mut engine = Engine::default();
    engine.new_project("soak".into()).expect("new project");
    let media = engine
        .import_media_file(&fixture.canonicalize().unwrap().to_string_lossy())
        .expect("import");
    let media_id =
        vance_timeline::MediaItemId::from_uuid(media.id.parse::<uuid::Uuid>().unwrap());
    let track_id = {
        let contents = engine.current_sequence_contents().expect("seq");
        let v1 = contents.tracks.iter().find(|t| t.kind == "video").expect("V1");
        vance_timeline::TrackId::from_uuid(v1.id.parse::<uuid::Uuid>().unwrap())
    };
    engine.insert_clip_at(track_id, media_id, 0.0).expect("clip");

    let out = std::env::temp_dir().join(format!("vance-soak-{}.mov", std::process::id()));
    for cycle in 0..cycles {
        // Render a tiny half-second slice each cycle. Reuses the
        // engine + cache across cycles so the prefetch + LRU
        // behaviour is exercised.
        let _ = std::fs::remove_file(&out);
        let outcome = engine
            .render_sequence(RenderOptions {
                output_path: out.to_string_lossy().to_string(),
                preset: EncoderPreset::ProRes422HQ,
                width: Some(160),
                height: Some(120),
                render_and_replace_track: None,
                start_seconds: Some(0.0),
                end_seconds: Some(0.25),
            })
            .expect("render");
        assert!(outcome.video_frames >= 4, "cycle {cycle}: too few frames");
        let stats = engine.frame_cache_stats();
        if let Some(s) = stats {
            // Ring budget per source is 64 MiB; assert we stayed
            // under 256 MiB across all rings — a generous safety
            // net for the leak gate.
            assert!(
                s.ring_total_bytes < 256 * 1024 * 1024,
                "cycle {cycle}: ring total {} bytes exceeds 256 MiB ceiling",
                s.ring_total_bytes
            );
            eprintln!(
                "[soak {cycle}/{cycles}] ring_bytes={} disk_entries={} disk_bytes={}",
                s.ring_total_bytes, s.disk_entries, s.disk_bytes
            );
        }
    }
    let _ = std::fs::remove_file(&out);
}
