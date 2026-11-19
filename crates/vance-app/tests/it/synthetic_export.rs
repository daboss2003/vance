//! Real-media e2e for the SYNTHETIC media kinds (Fusion node comps + Motion
//! Graphics titles). These render procedurally (no decoder) via
//! render_fusion_frame / render_mograph_frame → the export encoder path. They
//! have crate-level unit tests, but this proves the whole render→encode→mux
//! chain produces a valid file with the real encoder sidecar + ffmpeg.
//!
//! Skips (eprintln!) without ffmpeg/ffprobe or the sidecars.

use std::path::PathBuf;
use std::process::Command;
use vance_app_lib::engine::{Engine, RenderOptions};
use vance_encoder::EncoderPreset;

fn tool(n: &str) -> bool {
    Command::new(n).arg("-version").output().is_ok()
}
fn sidecars_present() -> bool {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let ext = if cfg!(windows) { ".exe" } else { "" };
    ["vance-encoder", "vance-audio-decoder"]
        .iter()
        .all(|b| d.join(format!("{b}{ext}")).exists())
}
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
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn video_track(engine: &Engine) -> vance_timeline::TrackId {
    let contents = engine.current_sequence_contents().expect("contents");
    let v1 = contents
        .tracks
        .iter()
        .find(|t| t.kind == "video")
        .expect("V1");
    vance_timeline::TrackId::from_uuid(v1.id.parse().unwrap())
}

fn export_and_probe(engine: &mut Engine, label: &str) {
    let out = std::env::temp_dir().join(format!("vance-syn-{label}-{}.mov", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let outcome = engine
        .render_sequence(RenderOptions {
            output_path: out.to_string_lossy().to_string(),
            preset: EncoderPreset::ProRes422HQ,
            width: Some(320),
            height: Some(240),
            render_and_replace_track: None,
            start_seconds: Some(0.0),
            end_seconds: Some(0.4),
            hdr: false,
        })
        .unwrap_or_else(|e| panic!("{label} export: {e}"));
    assert!(
        outcome.video_frames >= 6,
        "{label}: too few frames {}",
        outcome.video_frames
    );
    let meta = std::fs::metadata(&out).expect("output exists");
    assert!(
        meta.len() > 5_000,
        "{label} output tiny: {} bytes",
        meta.len()
    );
    assert_eq!(
        probe(&out, "codec_name"),
        "prores",
        "{label} must be ProRes"
    );
    assert_eq!(probe(&out, "width"), "320", "{label} width");
    let _ = std::fs::remove_file(&out);
}

#[test]
fn fusion_comp_exports_to_valid_prores() {
    if !tool("ffmpeg") || !tool("ffprobe") || !sidecars_present() {
        eprintln!("ffmpeg/ffprobe/sidecars absent — skipping synthetic export");
        return;
    }
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).expect("chdir");

    let mut engine = Engine::default();
    engine.new_project("syn".into()).expect("project");
    // A Fusion node-graph composition (starter: title over gradient).
    let media = engine
        .create_fusion_media(Some("Comp".into()), 320, 240, 2.0)
        .expect("create fusion media");
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse().unwrap());
    let track = video_track(&engine);
    engine
        .insert_clip_at(track, media_id, 0.0)
        .expect("insert fusion clip");
    export_and_probe(&mut engine, "fusion");
}

#[test]
fn mograph_title_exports_to_valid_prores() {
    if !tool("ffmpeg") || !tool("ffprobe") || !sidecars_present() {
        eprintln!("ffmpeg/ffprobe/sidecars absent — skipping synthetic export");
        return;
    }
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).expect("chdir");

    let mut engine = Engine::default();
    engine.new_project("syn".into()).expect("project");
    // The first built-in MoGraph template.
    let catalog = engine.mograph_catalog();
    let Some((key, _)) = catalog.first() else {
        eprintln!("no mograph templates — skipping");
        return;
    };
    let media = engine
        .create_mograph_media(key.clone(), Some("Title".into()))
        .expect("create mograph media");
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse().unwrap());
    let track = video_track(&engine);
    engine
        .insert_clip_at(track, media_id, 0.0)
        .expect("insert mograph clip");
    export_and_probe(&mut engine, "mograph");
}
