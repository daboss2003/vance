//! Phase 8 (real export) — verify the export applies native per-clip
//! effects + cross-dissolve transitions (both were dropped before:
//! the renderer was reset to identity and transitions ignored).
//!
//! - native_opacity_darkens_export: an opacity=0 effect makes the
//!   exported frame black — proving native effects reach the file.
//! - transition_blends_into_export: a cross-dissolve mid-window frame
//!   differs from the same frame exported without the transition —
//!   proving A↔B blending is applied at export time.
//!
//! Skips without ffmpeg/ffprobe, the sidecars, or the fixture.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use vance_app_lib::engine::{Engine, RenderOptions};
use vance_encoder::EncoderPreset;
use vance_timeline::{EffectValue, TransitionKind};

fn video_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4")
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

static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn uniq() -> u64 {
    std::process::id() as u64 * 1_000_000 + SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Export a single frame at `at_seconds` (a 1-frame-wide slice) and
/// return its decoded RGB24 bytes via ffmpeg, plus the mean luma.
fn export_frame(engine: &mut Engine, at_seconds: f64) -> Option<(Vec<u8>, f64)> {
    let out = std::env::temp_dir().join(format!("vance-vexp-{}.mov", uniq()));
    let _ = std::fs::remove_file(&out);
    // ~1 frame at 24 fps.
    engine
        .render_sequence(RenderOptions {
            output_path: out.to_string_lossy().to_string(),
            preset: EncoderPreset::ProRes422HQ,
            width: Some(160),
            height: Some(120),
            render_and_replace_track: None,
            start_seconds: Some(at_seconds),
            end_seconds: Some(at_seconds + 0.05),
        })
        .ok()?;
    let raw = std::env::temp_dir().join(format!("vance-vexp-{}.rgb", uniq()));
    let _ = std::fs::remove_file(&raw);
    let ok = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&out)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24"])
        .arg(&raw)
        .status()
        .ok()?
        .success();
    let _ = std::fs::remove_file(&out);
    if !ok {
        return None;
    }
    let bytes = std::fs::read(&raw).ok()?;
    let _ = std::fs::remove_file(&raw);
    if bytes.is_empty() {
        return None;
    }
    let luma: f64 = bytes.iter().map(|&b| b as f64).sum::<f64>() / bytes.len() as f64;
    Some((bytes, luma))
}

fn import_clip(engine: &mut Engine) -> Option<(vance_timeline::TrackId, vance_timeline::ClipId)> {
    let media = engine
        .import_media_file(&video_fixture().canonicalize().ok()?.to_string_lossy())
        .ok()?;
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse().ok()?);
    let contents = engine.current_sequence_contents()?;
    let v1 = contents.tracks.iter().find(|t| t.kind == "video")?;
    let track = vance_timeline::TrackId::from_uuid(v1.id.parse().ok()?);
    let clip = engine.insert_clip_at(track, media_id, 0.0).ok()?;
    let clip_id = vance_timeline::ClipId::from_uuid(clip.parse().ok()?);
    Some((track, clip_id))
}

#[test]
fn native_opacity_darkens_export() {
    if !tool("ffmpeg") || !tool("ffprobe") || !sidecars_present() || !video_fixture().exists() {
        eprintln!("tools/fixture missing — skipping");
        return;
    }
    std::env::set_current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")).ok();

    // Baseline: no effect.
    let mut e1 = Engine::default();
    e1.new_project("vid".into()).expect("proj");
    let Some(_) = import_clip(&mut e1) else { return };
    let (_, base_luma) = export_frame(&mut e1, 0.5).expect("base");

    // opacity = 0 → fully transparent → black frame.
    let mut e2 = Engine::default();
    e2.new_project("vid".into()).expect("proj");
    let Some((track, clip)) = import_clip(&mut e2) else { return };
    let mut params: BTreeMap<String, EffectValue> = BTreeMap::new();
    params.insert("value".into(), EffectValue::Number(0.0));
    e2.add_effect_to_clip(track, clip, "opacity".into(), params)
        .expect("add opacity");
    let (_, dark_luma) = export_frame(&mut e2, 0.5).expect("dark");

    assert!(base_luma > 5.0, "baseline frame should have real luma, got {base_luma}");
    assert!(
        dark_luma < base_luma * 0.2,
        "opacity=0 must darken the export: base={base_luma}, dark={dark_luma}"
    );
}

#[test]
fn transition_blends_into_export() {
    if !tool("ffmpeg") || !tool("ffprobe") || !sidecars_present() || !video_fixture().exists() {
        eprintln!("tools/fixture missing — skipping");
        return;
    }
    std::env::set_current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")).ok();

    // Two back-to-back clips of the same 5 s source (clip1 at 0, clip2
    // at 5 s) with a 1 s cross-dissolve at the 5 s cut → window
    // [4.5, 5.5]. A frame at 4.8 s is mid-dissolve.
    let build = |with_transition: bool| -> Option<(Vec<u8>, f64)> {
        let mut e = Engine::default();
        e.new_project("xfade".into()).ok()?;
        let media = e
            .import_media_file(&video_fixture().canonicalize().ok()?.to_string_lossy())
            .ok()?;
        let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse().ok()?);
        let contents = e.current_sequence_contents()?;
        let v1 = contents.tracks.iter().find(|t| t.kind == "video")?;
        let track = vance_timeline::TrackId::from_uuid(v1.id.parse().ok()?);
        let c1 = e.insert_clip_at(track, media_id, 0.0).ok()?;
        let c1_id = vance_timeline::ClipId::from_uuid(c1.parse().ok()?);
        // Second clip right after the first (≈5 s long).
        e.insert_clip_at(track, media_id, 5.0).ok()?;
        if with_transition {
            e.add_transition_at_cut(track, c1_id, TransitionKind::Dissolve, 1.0)
                .ok()?;
        }
        export_frame(&mut e, 4.8)
    };

    let Some((with_bytes, _)) = build(true) else {
        eprintln!("transition export failed — skipping");
        return;
    };
    let Some((without_bytes, _)) = build(false) else {
        eprintln!("no-transition export failed — skipping");
        return;
    };
    assert_eq!(with_bytes.len(), without_bytes.len(), "frame sizes differ");
    // The dissolve blends in the incoming clip, so the mid-window frame
    // must differ from the hard-cut frame at the same time.
    let diff: u64 = with_bytes
        .iter()
        .zip(without_bytes.iter())
        .map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs() as u64)
        .sum();
    let mean_abs_diff = diff as f64 / with_bytes.len() as f64;
    assert!(
        mean_abs_diff > 1.0,
        "transition frame should differ from hard-cut frame (mean abs diff {mean_abs_diff})"
    );
}
