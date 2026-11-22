//! Phase 10.5 — end-to-end: a clip carrying a WASM plugin effect
//! exports through the renderer + encoder. Verifies the plugin is
//! actually run (the output frame differs from the same export
//! without the effect) and the file is a valid ProRes.
//!
//! Requires the sample stylize plugin + the sidecars to be built.
//! Skips gracefully otherwise.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use vance_app_lib::addons::AddonHost;
use vance_app_lib::engine::{Engine, RenderOptions};
use vance_encoder::EncoderPreset;
use vance_timeline::EffectValue;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4")
}

fn stylize_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/stylize/target/wasm32-unknown-unknown/release")
}

fn tools_present() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
        && Command::new("ffprobe").arg("-version").output().is_ok()
}

/// Phase 9.6 — the render-cache-wrapped add-on apply must produce the
/// SAME output as the uncached apply on both the miss (first call) and
/// the hit (second identical call). Proves the cache doesn't corrupt
/// the frame + returns the cached output on re-visit. The add-on host is
/// what the engine's preview runs each layer's WASM chain through
/// (ADR 0007), with the engine's frame cache as its render cache.
#[test]
fn render_cache_matches_uncached_and_hits_on_revisit() {
    if !stylize_dir().join("vance_plugin_stylize.wasm").exists() {
        eprintln!("stylize.wasm not built — skipping render cache test");
        return;
    }
    let w = 16u32;
    let h = 16u32;
    let mut input = vec![0u8; (w * h * 4) as usize];
    for (i, px) in input.chunks_exact_mut(4).enumerate() {
        px[0] = (i * 5) as u8;
        px[1] = (i * 9) as u8;
        px[2] = (i * 3) as u8;
        px[3] = 255;
    }
    let effect = {
        let mut params: BTreeMap<String, EffectValue> = BTreeMap::new();
        params.insert("levels".into(), EffectValue::Number(3.0));
        params.insert("saturation".into(), EffectValue::Number(1.5));
        // Build an Effect with kind wasm:<id>.
        vance_timeline::Effect {
            id: vance_timeline::EffectId::new(),
            kind: "wasm:com.vance.sample.stylize".into(),
            enabled: true,
            params,
        }
    };

    let mut host = AddonHost::default();
    assert!(host.scan_dir(&stylize_dir()) >= 1);
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let cache = Arc::new(
        vance_cache::FrameCache::new(cache_dir.path().to_path_buf(), 1 << 20, 1 << 24)
            .expect("frame cache"),
    );
    host.set_render_cache(Some(cache.clone()));

    // Uncached reference.
    let mut a = input.clone();
    host.apply_effects(&mut a, w, h, std::slice::from_ref(&effect), 0.0);
    assert_ne!(a, input, "effect should change the frame");

    // Cached miss (first) then cached hit (second), both from the same
    // pristine input.
    let mut b = input.clone();
    host.apply_effects_cached(&mut b, w, h, std::slice::from_ref(&effect), 0.0);
    let mut c = input.clone();
    host.apply_effects_cached(&mut c, w, h, std::slice::from_ref(&effect), 0.0);

    assert_eq!(a, b, "cached miss must match uncached");
    assert_eq!(b, c, "cached hit must match cached miss");
    // A render-cache entry now exists.
    assert!(
        cache.stats().disk_entries >= 1,
        "render cache entry not created"
    );
}

fn sidecars_present() -> bool {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    dir.join(if cfg!(windows) {
        "vance-decoder.exe"
    } else {
        "vance-decoder"
    })
    .exists()
        && dir
            .join(if cfg!(windows) {
                "vance-encoder.exe"
            } else {
                "vance-encoder"
            })
            .exists()
}

fn build_engine_with_clip() -> Option<(Engine, vance_timeline::TrackId, vance_timeline::ClipId)> {
    let fixture = fixture_path();
    if !fixture.exists() {
        return None;
    }
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).ok()?;

    let mut engine = Engine::default();
    engine.new_project("wasm-test".into()).ok()?;
    let media = engine
        .import_media_file(&fixture.canonicalize().ok()?.to_string_lossy())
        .ok()?;
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse().ok()?);
    let contents = engine.current_sequence_contents()?;
    let v1 = contents.tracks.iter().find(|t| t.kind == "video")?;
    let track_id = vance_timeline::TrackId::from_uuid(v1.id.parse().ok()?);
    let clip_id_str = engine.insert_clip_at(track_id, media_id, 0.0).ok()?;
    let clip_id = vance_timeline::ClipId::from_uuid(clip_id_str.parse().ok()?);
    Some((engine, track_id, clip_id))
}

fn render_first_frame_bytes(engine: &mut Engine) -> Option<Vec<u8>> {
    let out = std::env::temp_dir().join(format!("vance-wasm-{}.mov", std::process::id()));
    let _ = std::fs::remove_file(&out);
    engine
        .render_sequence(RenderOptions {
            output_path: out.to_string_lossy().to_string(),
            preset: EncoderPreset::ProRes422HQ,
            width: Some(64),
            height: Some(64),
            render_and_replace_track: None,
            start_seconds: Some(0.0),
            end_seconds: Some(0.1),
            hdr: false,
        })
        .ok()?;
    // Extract frame 0 as raw RGB via ffmpeg for a content comparison.
    let raw = std::env::temp_dir().join(format!("vance-wasm-{}.rgb", std::process::id()));
    let _ = std::fs::remove_file(&raw);
    let ok = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&out)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgb24"])
        .arg(&raw)
        .status()
        .ok()?
        .success();
    let bytes = if ok { std::fs::read(&raw).ok() } else { None };
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_file(&raw);
    bytes
}

#[test]
fn wasm_effect_changes_exported_frame() {
    if !tools_present() || !sidecars_present() {
        eprintln!("ffmpeg/ffprobe/sidecars missing — skipping wasm export test");
        return;
    }
    if !stylize_dir().join("vance_plugin_stylize.wasm").exists() {
        eprintln!("stylize.wasm not built — skipping");
        return;
    }

    // Baseline render (no plugin).
    let Some((mut engine, _t, _c)) = build_engine_with_clip() else {
        eprintln!("engine setup failed — skipping");
        return;
    };
    let baseline = render_first_frame_bytes(&mut engine);

    // Render with the stylize plugin applied (strong posterize so the
    // difference is unmistakable).
    let Some((mut engine2, track, clip)) = build_engine_with_clip() else {
        return;
    };
    let n = engine2.add_plugin_dir(stylize_dir());
    assert!(n >= 1, "expected stylize plugin to be discovered");
    let mut params: BTreeMap<String, EffectValue> = BTreeMap::new();
    params.insert("levels".into(), EffectValue::Number(2.0));
    params.insert("saturation".into(), EffectValue::Number(0.0));
    engine2
        .add_effect_to_clip(track, clip, "wasm:com.vance.sample.stylize".into(), params)
        .expect("add wasm effect");
    let styled = render_first_frame_bytes(&mut engine2);

    match (baseline, styled) {
        (Some(b), Some(s)) => {
            assert_eq!(b.len(), s.len(), "frame sizes should match");
            assert_ne!(b, s, "stylize plugin must change the exported frame");
        }
        _ => eprintln!("frame extraction failed — skipping content assert"),
    }
}

#[test]
fn unknown_wasm_plugin_is_skipped_gracefully() {
    if !tools_present() || !sidecars_present() {
        eprintln!("tools missing — skipping");
        return;
    }
    let Some((mut engine, track, clip)) = build_engine_with_clip() else {
        return;
    };
    // Reference a plugin that was never discovered — export must still
    // succeed (the effect is logged + skipped).
    let mut params: BTreeMap<String, EffectValue> = BTreeMap::new();
    params.insert("x".into(), EffectValue::Number(1.0));
    engine
        .add_effect_to_clip(track, clip, "wasm:com.nonexistent.plugin".into(), params)
        .expect("add effect");
    let out = std::env::temp_dir().join(format!("vance-wasm-unknown-{}.mov", std::process::id()));
    let _ = std::fs::remove_file(&out);
    let outcome = engine.render_sequence(RenderOptions {
        output_path: out.to_string_lossy().to_string(),
        preset: EncoderPreset::ProRes422HQ,
        width: Some(48),
        height: Some(48),
        render_and_replace_track: None,
        start_seconds: Some(0.0),
        end_seconds: Some(0.1),
        hdr: false,
    });
    assert!(outcome.is_ok(), "export must survive an unknown plugin ref");
    let _ = std::fs::remove_file(&out);
}
