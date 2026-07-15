//! Real-media edge cases: odd output dimensions (must crop to even for 4:2:0
//! chroma, not crash) and a captions SRT/VTT round-trip with tricky text
//! (multi-line, unicode, punctuation) that the tolerant parser must preserve.

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
        .args(["-hide_banner", "-loglevel", "error", "-select_streams", "v:0", "-show_entries"])
        .arg(format!("stream={field}"))
        .args(["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .output()
        .expect("ffprobe");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
fn chdir_root() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let _ = std::env::set_current_dir(&root);
}

#[test]
fn odd_dimensions_export_crops_to_even() {
    if !tool("ffmpeg") || !tool("ffprobe") || !sidecars_present() {
        eprintln!("ffmpeg/ffprobe/sidecars absent — skipping");
        return;
    }
    chdir_root();
    let mut e = Engine::default();
    e.new_project("x".into()).unwrap();
    let f = e.create_fusion_media(Some("c".into()), 321, 241, 1.0).unwrap();
    let media_id = vance_timeline::MediaItemId::from_uuid(f.id.parse().unwrap());
    let track = {
        let c = e.current_sequence_contents().unwrap();
        let v1 = c.tracks.iter().find(|t| t.kind == "video").unwrap();
        vance_timeline::TrackId::from_uuid(v1.id.parse().unwrap())
    };
    e.insert_clip_at(track, media_id, 0.0).unwrap();

    // Odd requested dims through BOTH a ProRes (4:2:2) and an H.264 (4:2:0)
    // encoder — the 4:2:0 path strictly needs even planes, so this is the one
    // that would break without the export's even-rounding.
    for (label, preset) in [
        ("prores", EncoderPreset::ProRes422HQ),
        ("h264", EncoderPreset::H264 { crf: 23 }),
    ] {
        let out = std::env::temp_dir().join(format!("vance-odd-{label}-{}.mov", std::process::id()));
        let _ = std::fs::remove_file(&out);
        e.render_sequence(RenderOptions {
            output_path: out.to_string_lossy().to_string(),
            preset,
            width: Some(321),
            height: Some(241),
            render_and_replace_track: None,
            start_seconds: Some(0.0),
            end_seconds: Some(0.2),
            hdr: false,
        })
        .unwrap_or_else(|e| panic!("{label} odd-dimension export must succeed: {e}"));
        let w: i64 = probe(&out, "width").parse().unwrap_or(0);
        let h: i64 = probe(&out, "height").parse().unwrap_or(0);
        // Rounded down to even (321→320, 241→240).
        assert_eq!((w, h), (320, 240), "{label}: dims must round to even 320x240, got {w}x{h}");
        let _ = std::fs::remove_file(&out);
    }
}

#[test]
fn captions_srt_and_vtt_round_trip_tricky_text() {
    let mut e = Engine::default();
    e.new_project("x".into()).unwrap();
    // Tricky cues: multi-line, unicode, punctuation, adjacent timings.
    e.add_caption(0, 1500, "First line\nsecond line".into()).unwrap();
    e.add_caption(1500, 3000, "Café — déjà vu, 100% sure?".into()).unwrap();
    e.add_caption(3000, 4200, "<i>emphasis</i> & symbols: 3 < 5".into()).unwrap();
    let original: Vec<(i64, i64, String)> = e
        .captions()
        .iter()
        .map(|c| (c.start_ms, c.end_ms, c.text.clone()))
        .collect();
    assert_eq!(original.len(), 3);

    for (ext, vtt) in [("srt", false), ("vtt", true)] {
        let path = std::env::temp_dir()
            .join(format!("vance-cap-{}-{}.{ext}", std::process::id(), ext));
        let _ = std::fs::remove_file(&path);
        e.export_captions(&path.to_string_lossy(), vtt).expect("export captions");
        // Re-import into a fresh engine/sequence and compare timings + text.
        let mut e2 = Engine::default();
        e2.new_project("y".into()).unwrap();
        let n = e2.import_captions(&path.to_string_lossy()).expect("import captions");
        assert_eq!(n, 3, "{ext}: all 3 cues re-imported");
        let back: Vec<(i64, i64, String)> = e2
            .captions()
            .iter()
            .map(|c| (c.start_ms, c.end_ms, c.text.clone()))
            .collect();
        for (i, (o, b)) in original.iter().zip(back.iter()).enumerate() {
            assert_eq!(o.0, b.0, "{ext} cue {i} start");
            assert_eq!(o.1, b.1, "{ext} cue {i} end");
            assert_eq!(o.2, b.2, "{ext} cue {i} text (round-trip): {:?} vs {:?}", o.2, b.2);
        }
        let _ = std::fs::remove_file(&path);
    }
}
