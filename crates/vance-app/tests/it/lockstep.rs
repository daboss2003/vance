//! Preview == export, byte for byte, on a 4-track project (dissolve, keyed
//! Screen picture-in-picture, title at 0.7 opacity, Fusion) — ADR 0007.
//! Then the real export (`render_sequence` → ProRes 4444, decoded back to
//! rgb24 by ffmpeg) matches `export_frame_rgba` within ±3 per channel.
//!
//! Skips (eprintln) without ffmpeg / ffprobe, the sidecars or libx264.

use std::process::Command;

use crate::compositing_e2e::support::*;
use vance_app_lib::engine::RenderOptions;
use vance_encoder::EncoderPreset;
use vance_timeline::{BlendMode, MediaItemId, TitleParams, TrackId, TransitionKind};

/// Sample times (24 fps, frame-exact): before / inside / at the midpoint /
/// after the V1 dissolve (window [0.75, 1.25]), mid-B, a V1 gap under the
/// upper tracks, a gap on every track, and the last clip.
const TIMES: [f64; 8] = [0.5, 0.875, 1.0, 1.125, 1.5, 2.25, 3.25, 3.625];
/// The sequence end.
const END: f64 = 4.0;
const FPS: f64 = 24.0;

/// A Fusion comp: footage `name` via `MediaIn`, scaled to 30% in the
/// top-right corner over a transparent canvas.
fn thumbnail_comp(name: &str) -> vance_fusion::Comp {
    use vance_fusion::{Anim, Comp, Fit, MediaSpec, Node, NodeKind, TransformSpec};
    let mut comp = Comp::empty(W, H);
    comp.fps = FPS;
    comp.duration_s = 3.0;
    comp.nodes.push(Node::new(
        "plate",
        "Plate",
        NodeKind::MediaIn(MediaSpec {
            name: name.into(),
            fit: Fit::Contain,
        }),
    ));
    let mut xf = TransformSpec::identity(W, H);
    xf.position = [Anim::c(250.0), Anim::c(40.0)];
    xf.scale = [Anim::c(0.3), Anim::c(0.3)];
    comp.nodes
        .push(Node::new("thumb", "Thumb", NodeKind::Transform(xf)).with_input("input", "plate"));
    comp.output = "thumb".into();
    comp
}

/// V1: testsrc2 [0, 1) ⟶ 0.5 s dissolve ⟶ testsrc [1, 2), a gap, then
/// testsrc again [3.5, 4). V2: a moving red square on green, keyed,
/// Screen-blended at half size. V3: a transparent-background title at 0.7
/// opacity. V4: a Fusion comp showing testsrc via `MediaIn`. All upper
/// tracks span [0, 3).
fn build(rig: &mut Rig) -> Option<[TrackId; 4]> {
    let a = rig.clip("a", &format!("testsrc2=s={W}x{H}:r=24:d=2"), 2.0)?;
    let b = rig.clip("b", &format!("testsrc=s={W}x{H}:r=24:d=2"), 2.0)?;
    let screen = format!(
        "{}[bg];{}[fg];[bg][fg]overlay=x='40+60*t':y=42",
        solid_src(0x00FF00, W, H, 3.0),
        solid_src(0xFF2020, 96, 96, 3.0)
    );
    let plate = rig.clip("greenscreen", &screen, 3.0)?;
    let title = rig.title(
        "title",
        TitleParams {
            text: "LOCKSTEP".into(),
            size_px: 40.0,
            fg: [1.0, 0.9, 0.2],
            bg: [0.0; 3],
            width: W,
            height: H,
            bg_alpha: 0.0,
        },
        3.0,
    );
    let info = rig
        .engine
        .create_fusion_media(Some("Comp".into()), W, H, 3.0)
        .expect("create fusion media");
    let fusion = MediaItemId::from_uuid(info.id.parse().unwrap());
    let b_name = rig.media_name(b);
    rig.engine
        .set_fusion_comp(fusion, thumbnail_comp(&b_name))
        .expect("set fusion comp");

    let v = [rig.track(0), rig.track(1), rig.track(2), rig.track(3)];
    let e = &mut rig.engine;
    let ca = {
        let id = e.insert_clip_at(v[0], a, 0.0).expect("insert A");
        vance_timeline::ClipId::from_uuid(id.parse().unwrap())
    };
    e.trim_clip_out(v[0], ca, 1.0).expect("trim A");
    let cb = {
        let id = e.insert_clip_at(v[0], b, 1.0).expect("insert B");
        vance_timeline::ClipId::from_uuid(id.parse().unwrap())
    };
    e.trim_clip_out(v[0], cb, 1.0).expect("trim B");
    let last = {
        let id = e.insert_clip_at(v[0], b, 3.5).expect("insert last");
        vance_timeline::ClipId::from_uuid(id.parse().unwrap())
    };
    e.trim_clip_out(v[0], last, 0.5).expect("trim last");
    e.add_transition_at_cut(v[0], ca, TransitionKind::Dissolve, 0.5)
        .expect("dissolve");

    let cg = rig.place(v[1], plate, 0.0);
    rig.fx(v[1], cg, vance_timeline::keyer::KEYER_KIND, &[]);
    rig.fx(
        v[1],
        cg,
        "transform",
        &[
            ("scale_x", 0.5),
            ("scale_y", 0.5),
            ("translate_x", 0.2),
            ("translate_y", 0.15),
        ],
    );
    rig.engine
        .set_clip_blend_mode(v[1], cg, BlendMode::Screen)
        .expect("screen blend");
    let ct = rig.place(v[2], title, 0.0);
    rig.fx(v[2], ct, "opacity", &[("value", 0.7)]);
    rig.place(v[3], fusion, 0.0);
    rig.canvas(W, H);

    let contents = rig.engine.current_sequence_contents().unwrap();
    let v1 = &contents.tracks[0];
    assert_eq!(v1.clips.len(), 3, "V1 holds A, B and the last clip");
    assert!(
        (contents.duration_seconds - END).abs() < 1e-9,
        "sequence ends at {END}, got {}",
        contents.duration_seconds
    );
    assert_eq!(v1.transitions.len(), 1, "V1 has the dissolve");
    assert_eq!(contents.tracks[1].clips[0].blend_mode, "screen");
    Some(v)
}

/// `(differing bytes, largest difference, first differing pixel)`.
fn diff(a: &[u8], b: &[u8], w: u32) -> (usize, u8, Option<(u32, u32)>) {
    let mut count = 0;
    let mut max = 0;
    let mut first = None;
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let d = x.abs_diff(*y);
        if d > 0 {
            count += 1;
            max = max.max(d);
            let p = (i / 4) as u32;
            first.get_or_insert((p % w, p / w));
        }
    }
    (count, max, first)
}

#[test]
fn preview_equals_export_byte_for_byte_on_four_tracks() {
    let Some(mut rig) = Rig::new("lockstep-bytes") else {
        return;
    };
    let Some(v) = build(&mut rig) else {
        return;
    };
    for t in TIMES {
        let (preview, export) = (rig.preview(t), rig.export(t));
        let (count, max, first) = diff(&preview, &export, W);
        assert!(
            count == 0,
            "t={t}: preview != export in {count} bytes (max Δ{max}, first at {first:?})"
        );
    }
    // Scrubbing back to an earlier time gives the same bytes again (no
    // stale per-clip decode state).
    let again = rig.preview(TIMES[0]);
    assert_eq!(again, rig.export(TIMES[0]), "re-scrub to t={}", TIMES[0]);

    // Not vacuous: every upper layer contributes at t = 0.5.
    let full = rig.export(0.5);
    for (n, track) in v.iter().enumerate().skip(1) {
        rig.engine.set_track_enabled(*track, false).expect("hide");
        let without = rig.export(0.5);
        assert_eq!(without, rig.preview(0.5), "lockstep with V{} hidden", n + 1);
        assert_ne!(without, full, "V{} must change the frame", n + 1);
        rig.engine.set_track_enabled(*track, true).expect("show");
    }
    assert_eq!(
        rig.export(0.5),
        full,
        "showing every track restores the frame"
    );
}

#[test]
fn render_sequence_matches_export_frames() {
    let Some(mut rig) = Rig::new("lockstep-file") else {
        return;
    };
    if build(&mut rig).is_none() {
        return;
    }
    let out = rig.scratch.path("render", "mov");
    let outcome = rig
        .engine
        .render_sequence(RenderOptions {
            output_path: out.to_string_lossy().to_string(),
            preset: EncoderPreset::ProRes4444,
            width: Some(W),
            height: Some(H),
            render_and_replace_track: None,
            start_seconds: Some(0.0),
            end_seconds: Some(END),
            hdr: false,
        })
        .expect("render_sequence");
    let frames = (END * FPS).round() as u64;
    assert_eq!(outcome.video_frames, frames, "frames pushed");

    // ProRes 4444 is 10-bit 4:4:4; the encoder converts with BT.709 and
    // tags it, and ffmpeg decodes by the tag, so the round trip is within a
    // code or two.
    let decoded = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(&out)
        .args(["-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .expect("run ffmpeg");
    assert!(decoded.status.success(), "decode the export");
    let frame_bytes = (W * H * 3) as usize;
    let file = decoded.stdout;
    assert_eq!(
        file.len(),
        frame_bytes * frames as usize,
        "decoded frame count"
    );
    for t in TIMES {
        let k = (t * FPS).round() as usize;
        let encoded = &file[k * frame_bytes..(k + 1) * frame_bytes];
        let want = rig.export(t);
        let mut worst = (0u8, 0u32, 0u32);
        let mut over = 0usize;
        for (i, (rgb, rgba)) in encoded
            .chunks_exact(3)
            .zip(want.chunks_exact(4))
            .enumerate()
        {
            for c in 0..3 {
                let d = rgb[c].abs_diff(rgba[c]);
                if d > 3 {
                    over += 1;
                }
                if d > worst.0 {
                    worst = (d, i as u32 % W, i as u32 / W);
                }
            }
        }
        assert!(
            over == 0,
            "t={t} (frame {k}): {over} channels differ by more than 3 from export_frame_rgba \
             (worst Δ{} at ({}, {}))",
            worst.0,
            worst.1,
            worst.2
        );
    }
}
