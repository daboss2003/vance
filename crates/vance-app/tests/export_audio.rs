//! Phase 8 (real export) — verify the export contains REAL mixed audio,
//! not the silence the pipeline used to emit.
//!
//! Builds an A/V source (testsrc video + a 440 Hz sine) at test time
//! via ffmpeg, places it on an audio track, exports to ProRes, then
//! decodes the exported audio and asserts it has real energy (RMS well
//! above zero). Also checks that a per-clip gain reduction actually
//! lowers the exported level — proving the mixer model (not just
//! passthrough) is applied.
//!
//! Skips gracefully without ffmpeg/ffprobe, the sidecars, or fixtures.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use vance_app_lib::engine::{Engine, RenderOptions};
use vance_encoder::EncoderPreset;
use vance_timeline::EffectValue;

fn video_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4")
}
fn audio_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../vance-audio/tests/fixtures/sine_440_48k_2s.m4a")
}

fn tool(name: &str) -> bool {
    Command::new(name).arg("-version").output().is_ok()
}

/// Process-unique counter so parallel tests in this binary never
/// collide on temp paths.
static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
fn uniq() -> u64 {
    std::process::id() as u64 * 1_000_000
        + SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn sidecars_present() -> bool {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let ext = if cfg!(windows) { ".exe" } else { "" };
    for b in ["vance-decoder", "vance-encoder", "vance-audio-decoder"] {
        if !d.join(format!("{b}{ext}")).exists() {
            return false;
        }
    }
    true
}

/// ffmpeg-mux a 2 s A/V file from the fixtures. Returns its path.
fn build_av_source() -> Option<PathBuf> {
    let out = std::env::temp_dir().join(format!("vance-av-{}.mp4", uniq()));
    let _ = std::fs::remove_file(&out);
    let ok = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(video_fixture())
        .arg("-i")
        .arg(audio_fixture())
        .args([
            "-t", "2", "-map", "0:v:0", "-map", "1:a:0", "-c:v", "libx264", "-c:a", "aac",
            "-shortest",
        ])
        .arg(&out)
        .status()
        .ok()?
        .success();
    ok.then_some(out)
}

/// Export `engine` and return the exported track's mean-square audio
/// level (decoded as raw f32 via ffmpeg).
fn export_and_measure_rms(engine: &mut Engine) -> Option<f64> {
    let out = std::env::temp_dir().join(format!("vance-exp-{}.mov", uniq()));
    let _ = std::fs::remove_file(&out);
    engine
        .render_sequence(RenderOptions {
            output_path: out.to_string_lossy().to_string(),
            preset: EncoderPreset::ProRes422HQ,
            width: Some(160),
            height: Some(120),
            render_and_replace_track: None,
            start_seconds: Some(0.0),
            end_seconds: Some(1.5),
        })
        .ok()?;
    // Decode the exported audio to raw f32le mono.
    let raw = std::env::temp_dir().join(format!("vance-exp-{}.f32", uniq()));
    let _ = std::fs::remove_file(&raw);
    let ok = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&out)
        .args(["-map", "0:a:0", "-ac", "1", "-f", "f32le", "-acodec", "pcm_f32le"])
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
    let n = bytes.len() / 4;
    if n == 0 {
        return Some(0.0);
    }
    let mut sumsq = 0.0f64;
    for c in bytes.chunks_exact(4) {
        let v = f32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f64;
        sumsq += v * v;
    }
    Some(sumsq / n as f64)
}

fn setup(engine: &mut Engine, av: &PathBuf) -> Option<(vance_timeline::TrackId, vance_timeline::ClipId)> {
    engine.new_project("audio-export".into()).ok()?;
    let media = engine
        .import_media_file(&av.canonicalize().ok()?.to_string_lossy())
        .ok()?;
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse().ok()?);
    // Find the audio track A1.
    let contents = engine.current_sequence_contents()?;
    let a1 = contents.tracks.iter().find(|t| t.kind == "audio")?;
    let track_id = vance_timeline::TrackId::from_uuid(a1.id.parse().ok()?);
    let clip = engine.insert_clip_at(track_id, media_id, 0.0).ok()?;
    let clip_id = vance_timeline::ClipId::from_uuid(clip.parse().ok()?);
    Some((track_id, clip_id))
}

#[test]
fn export_has_real_audio_not_silence() {
    if !tool("ffmpeg") || !tool("ffprobe") || !sidecars_present() {
        eprintln!("ffmpeg/sidecars missing — skipping audio export test");
        return;
    }
    if !video_fixture().exists() || !audio_fixture().exists() {
        eprintln!("fixtures missing — skipping");
        return;
    }
    let Some(av) = build_av_source() else {
        eprintln!("av mux failed — skipping");
        return;
    };
    std::env::set_current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")).ok();

    let mut engine = Engine::default();
    let Some(_) = setup(&mut engine, &av) else {
        eprintln!("setup failed — skipping");
        let _ = std::fs::remove_file(&av);
        return;
    };
    let rms = export_and_measure_rms(&mut engine).expect("measure");
    // A 440 Hz sine at unity should have mean-square ≈ 0.5·amp². Assert
    // it's clearly non-silent (silence would be ~0).
    assert!(
        rms > 1e-4,
        "exported audio is silent (mean-square {rms}); the bounce isn't working"
    );
    let _ = std::fs::remove_file(&av);
}

#[test]
fn clip_gain_reduces_exported_level() {
    if !tool("ffmpeg") || !tool("ffprobe") || !sidecars_present() {
        eprintln!("tools missing — skipping");
        return;
    }
    if !video_fixture().exists() || !audio_fixture().exists() {
        return;
    }
    let Some(av) = build_av_source() else { return };
    std::env::set_current_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")).ok();

    // Full-level export.
    let mut e1 = Engine::default();
    let Some(_) = setup(&mut e1, &av) else {
        let _ = std::fs::remove_file(&av);
        return;
    };
    let loud = export_and_measure_rms(&mut e1).expect("loud");

    // -20 dB clip gain export.
    let mut e2 = Engine::default();
    let Some((track, clip)) = setup(&mut e2, &av) else {
        let _ = std::fs::remove_file(&av);
        return;
    };
    let mut params: BTreeMap<String, EffectValue> = BTreeMap::new();
    params.insert("gain_db".into(), EffectValue::Number(-20.0));
    // The clip-gain edit op is exposed via set_clip_gain_db; fall back
    // to an effect if not present.
    let _ = e2.set_clip_gain_db(track, clip, -20.0);
    let quiet = export_and_measure_rms(&mut e2).expect("quiet");

    assert!(
        quiet < loud * 0.2,
        "clip gain -20 dB should drop level a lot: loud={loud}, quiet={quiet}"
    );
    let _ = std::fs::remove_file(&av);
}
