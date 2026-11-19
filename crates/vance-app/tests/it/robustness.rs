//! Robustness / error-recovery: feed the engine bad input and assert it
//! returns a clean `Err` — never panics. A panic on malformed input is a
//! user-facing crash. Covers missing/garbage media, corrupt bundles,
//! un-writable export targets, and operations with no project/sequence.
//!
//! The media-import cases need the decoder sidecar; they skip without it.

use std::path::PathBuf;
use std::process::Command;
use vance_app_lib::engine::{Engine, RenderOptions};
use vance_encoder::EncoderPreset;

fn sidecars_present() -> bool {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let ext = if cfg!(windows) { ".exe" } else { "" };
    ["vance-decoder", "vance-encoder", "vance-audio-decoder"]
        .iter()
        .all(|b| d.join(format!("{b}{ext}")).exists())
}
fn ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}
fn chdir_root() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let _ = std::env::set_current_dir(&root);
}

#[test]
fn import_missing_file_errs_not_panics() {
    if !sidecars_present() {
        eprintln!("sidecars absent — skipping");
        return;
    }
    chdir_root();
    let mut e = Engine::default();
    e.new_project("x".into()).unwrap();
    let r = e.import_media_file("/definitely/not/here/nope.mp4");
    assert!(
        r.is_err(),
        "importing a missing file must return Err, not a frame"
    );
}

#[test]
fn import_non_media_file_errs_not_panics() {
    if !sidecars_present() {
        eprintln!("sidecars absent — skipping");
        return;
    }
    chdir_root();
    // A text file that is not decodable video.
    let junk = std::env::temp_dir().join(format!("vance-junk-{}.mp4", std::process::id()));
    std::fs::write(&junk, b"this is not a video file, just some bytes").unwrap();
    let mut e = Engine::default();
    e.new_project("x".into()).unwrap();
    let r = e.import_media_file(&junk.to_string_lossy());
    let _ = std::fs::remove_file(&junk);
    assert!(r.is_err(), "importing garbage must return Err, not panic");
}

#[test]
fn load_corrupt_bundle_errs_not_panics() {
    // A file that is not a valid .vance bundle.
    let bad = std::env::temp_dir().join(format!("vance-bad-{}.vance", std::process::id()));
    std::fs::write(&bad, b"\x00\x01\x02 not a bundle \xff\xfe").unwrap();
    let mut e = Engine::default();
    let r = e.load_project(&bad.to_string_lossy());
    let _ = std::fs::remove_file(&bad);
    assert!(
        r.is_err(),
        "loading a corrupt bundle must return Err, not panic"
    );
}

#[test]
fn load_missing_bundle_errs_not_panics() {
    let mut e = Engine::default();
    let r = e.load_project("/definitely/not/here/proj.vance");
    assert!(r.is_err(), "loading a missing bundle must return Err");
}

#[test]
fn export_to_unwritable_path_errs_not_panics() {
    if !sidecars_present() || !ffmpeg() {
        eprintln!("sidecars/ffmpeg absent — skipping");
        return;
    }
    chdir_root();
    let mut e = Engine::default();
    e.new_project("x".into()).unwrap();
    // A synthetic clip so no decode is needed; export to a path whose parent
    // directory does not exist — the encoder/ffmpeg must fail cleanly.
    let fusion = e
        .create_fusion_media(Some("c".into()), 160, 120, 1.0)
        .unwrap();
    let media_id = vance_timeline::MediaItemId::from_uuid(fusion.id.parse().unwrap());
    let track = {
        let c = e.current_sequence_contents().unwrap();
        let v1 = c.tracks.iter().find(|t| t.kind == "video").unwrap();
        vance_timeline::TrackId::from_uuid(v1.id.parse().unwrap())
    };
    e.insert_clip_at(track, media_id, 0.0).unwrap();
    let r = e.render_sequence(RenderOptions {
        output_path: "/definitely/not/here/sub/out.mov".into(),
        preset: EncoderPreset::ProRes422HQ,
        width: Some(160),
        height: Some(120),
        render_and_replace_track: None,
        start_seconds: Some(0.0),
        end_seconds: Some(0.2),
        hdr: false,
    });
    assert!(
        r.is_err(),
        "export to an unwritable path must return Err, not panic"
    );
}

#[test]
fn empty_range_export_errs() {
    chdir_root();
    let mut e = Engine::default();
    e.new_project("x".into()).unwrap();
    // end <= start → the range guard should reject it.
    let r = e.render_sequence(RenderOptions {
        output_path: std::env::temp_dir()
            .join("vance-empty.mov")
            .to_string_lossy()
            .into(),
        preset: EncoderPreset::ProRes422HQ,
        width: Some(160),
        height: Some(120),
        render_and_replace_track: None,
        start_seconds: Some(1.0),
        end_seconds: Some(1.0),
        hdr: false,
    });
    assert!(r.is_err(), "an empty [start,end) range must return Err");
}

#[test]
fn ops_with_no_project_err_not_panic() {
    // No project loaded — every content op must Err cleanly.
    let mut e = Engine::default();
    assert!(e.add_review_comment(0, "a".into(), "x".into()).is_err());
    assert!(e.add_caption(0, 1000, "x".into()).is_err());
    // Read-side accessors return empty, not panic.
    assert!(e.review_comments().is_empty());
    assert!(e.captions().is_empty());
    assert!(e.media_list().is_empty());
}
