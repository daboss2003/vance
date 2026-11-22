//! Engine behaviour from the M1 compositing fix round: export retries,
//! render-and-replace with transparency, half-size proxies in the right
//! colours, "couldn't show" notices and the canvas size of old projects.
//!
//! Skips (eprintln) without ffmpeg / ffprobe, the sidecars or libx264.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::compositing_e2e::support::*;
use vance_app_lib::engine::{RenderAndReplaceOptions, RenderOptions};
use vance_app_lib::proxy::{ProxyCodec, ProxyJobStatus};
use vance_encoder::EncoderPreset;
use vance_timeline::BlendMode;

fn probe(path: &Path, field: &str) -> String {
    let out = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-select_streams",
            "v:0",
        ])
        .args(["-show_entries", &format!("stream={field}")])
        .args(["-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The frame at `t` of `path`, decoded by ffmpeg to `pix_fmt` (rgb24 /
/// rgba) at `w × h`.
fn decode_as(path: &Path, pix_fmt: &str, (w, h): (u32, u32), t: f64) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            &t.to_string(),
            "-i",
        ])
        .arg(path)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", pix_fmt, "-"])
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "decode {}", path.display());
    let channels = if pix_fmt == "rgba" { 4 } else { 3 };
    assert_eq!(
        out.stdout.len(),
        (w * h * channels) as usize,
        "decoded frame size"
    );
    out.stdout
}

fn decode_rgb(path: &Path, w: u32, h: u32, t: f64) -> Vec<u8> {
    decode_as(path, "rgb24", (w, h), t)
}

fn export(rig: &mut Rig, out: &Path, end: f64) -> Result<u64, String> {
    rig.engine
        .render_sequence(RenderOptions {
            output_path: out.to_string_lossy().into_owned(),
            preset: EncoderPreset::ProRes422HQ,
            width: None,
            height: None,
            render_and_replace_track: None,
            start_seconds: Some(0.0),
            end_seconds: Some(end),
            hdr: false,
        })
        .map(|o| o.video_frames)
        .map_err(|e| e.to_string())
}

/// V2.1: an export that failed on a missing file kept a dead feed for that
/// clip; the retry then dropped the layer silently. The failure names the
/// clip (export policy: offline media fails an export).
#[test]
fn an_export_retried_after_a_missing_file_returns_includes_its_layer() {
    let Some(mut rig) = Rig::new("fix-retry") else {
        return;
    };
    let Some(blue) = rig.solid("blue", 0x0000FF, W, H) else {
        return;
    };
    let red_file = rig.scratch.path("red", "mp4");
    if !encode(&red_file, &solid_src(0xFF0000, W, H, 2.0), 2.0, 24) {
        return;
    }
    let red = rig.import(&red_file);
    let red_name = rig.media_name(red);
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, blue, 0.0);
    rig.place(v2, red, 0.0);
    rig.canvas(W, H);

    let red_file = red_file.canonicalize().expect("canonical path");
    let away = rig.scratch.path("red-away", "mp4");
    std::fs::rename(&red_file, &away).expect("move the clip's file away");
    let first = rig.scratch.path("first", "mov");
    let failed = export(&mut rig, &first, 0.25);
    std::fs::rename(&away, &red_file).expect("restore the clip's file");
    let err = failed.expect_err("a missing file fails the export");
    let file = red_file.file_name().unwrap().to_string_lossy();
    assert_eq!(
        err,
        format!("Can't export: clip \"{red_name}\" — media is offline: {file} can't be found"),
        "the error names the clip and why"
    );

    let second = rig.scratch.path("second", "mov");
    assert_eq!(export(&mut rig, &second, 0.25), Ok(6), "the retry exports");
    let frame = decode_rgb(&second, W, H, 0.1);
    let i = (((H / 2) * W + W / 2) * 3) as usize;
    let centre = [frame[i], frame[i + 1], frame[i + 2]];
    assert!(
        near([centre[0], centre[1], centre[2], 255], RED, COARSE),
        "the V2 layer is back in the retried export: centre {centre:?}"
    );
}

/// V2.9 / decision 2: the track renders alone with its transparency into
/// ProRes 4444; the new clip keeps the Screen blend and drops the (baked)
/// opacity, so the stack looks the same. Needs RGBA decoding (seam A) and
/// the compositor's RGBA path (seam B).
#[test]
fn render_and_replace_of_a_half_opacity_screen_title_looks_the_same() {
    let Some(mut rig) = Rig::new("fix-rnr") else {
        return;
    };
    let Some(base) = rig.clip("base", &format!("testsrc2=s={W}x{H}:r=24:d=2"), 2.0) else {
        return;
    };
    let title = rig.title("warm", card([0.9, 0.5, 0.2], W, H), 2.0);
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, base, 0.0);
    let c = rig.place(v2, title, 0.0);
    rig.fx(v2, c, "opacity", &[("value", 0.5)]);
    rig.engine
        .set_clip_blend_mode(v2, c, BlendMode::Screen)
        .expect("screen");
    rig.canvas(W, H);
    let before = rig.preview(0.5);

    let out = rig.scratch.path("rendered", "mov");
    let outcome = rig
        .engine
        .render_and_replace(RenderAndReplaceOptions {
            output_path: out.to_string_lossy().into_owned(),
            width: None,
            height: None,
            track_id: v2,
            start_seconds: 0.0,
            end_seconds: 1.0,
        })
        .expect("render_and_replace");
    assert_eq!(probe(&out, "codec_name"), "prores");
    // ffmpeg reports ProRes 4444 as 12-bit on decode.
    let pix_fmt = probe(&out, "pix_fmt");
    assert!(
        pix_fmt.starts_with("yuva444p"),
        "the alpha is kept: {pix_fmt}"
    );
    // The track alone, straight alpha: the card's colour at half coverage.
    let rendered = decode_as(&out, "rgba", (W, H), 0.5);
    let p = px(&rendered, W, W / 2, H / 2);
    assert!(
        p[3].abs_diff(128) <= 2
            && [230u8, 128, 51]
                .iter()
                .zip(p)
                .all(|(w, g)| w.abs_diff(g) <= 3),
        "rendered pixel {p:?}, want ≈[230, 128, 51, 128] (straight, opacity in the alpha)"
    );

    let contents = rig.engine.current_sequence_contents().expect("contents");
    let track = contents
        .tracks
        .iter()
        .find(|t| t.id == v2.to_string())
        .expect("V2");
    let clip = track
        .clips
        .iter()
        .find(|cl| cl.id == outcome.new_clip_id)
        .expect("the rendered clip");
    assert_eq!(
        clip.blend_mode, "screen",
        "the clips' blend mode carries over"
    );
    assert!(clip.effects.is_empty(), "opacity is baked into the alpha");
    let new_media = vance_timeline::MediaItemId::from_uuid(outcome.new_media_id.parse().unwrap());
    assert_eq!(
        rig.engine
            .media_get_info(new_media)
            .and_then(|m| m.input_color_space),
        Some("srgb".to_string()),
        "the render is the compositor's sRGB output and is decoded as such"
    );

    let after = rig.preview(0.5);
    let worst = before
        .chunks_exact(4)
        .zip(after.chunks_exact(4))
        .enumerate()
        .map(|(i, (a, b))| ((0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap(), i))
        .max()
        .unwrap();
    assert!(
        worst.0 <= 3,
        "render-and-replace changed the picture by {} at ({}, {})",
        worst.0,
        worst.1 as u32 % W,
        worst.1 as u32 / W
    );
}

/// V2.11 / decision 5: half the display size, and the proxy previews in the
/// original's colours (limited range expanded, the stream's matrix).
#[test]
fn a_proxy_is_half_size_and_previews_in_the_originals_colours() {
    let Some(mut rig) = Rig::new("fix-proxy") else {
        return;
    };
    let (sw, sh) = (640, 480);
    let Some(bars) = rig.clip("bars", &format!("smptebars=s={sw}x{sh}:r=24:d=1"), 1.0) else {
        return;
    };
    let source = rig.engine.media_path_for(bars).expect("source path");
    let proxy = rig.scratch.path("bars-proxy", "mov");
    let id = rig
        .engine
        .enqueue_proxy_generation(
            source.to_string_lossy().into_owned(),
            proxy.to_string_lossy().into_owned(),
            ProxyCodec::ProRes422,
        )
        .expect("enqueue proxy");
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let job = rig
            .engine
            .proxy_jobs_snapshot()
            .into_iter()
            .find(|j| j.id == id)
            .expect("the job");
        match job.status {
            ProxyJobStatus::Done { .. } => break,
            ProxyJobStatus::Error(e) => panic!("proxy generation failed: {e}"),
            _ if Instant::now() > deadline => panic!("proxy generation timed out"),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    assert!(
        vance_app_lib::proxy::current_marker(&proxy).is_some(),
        "marked as this build's proxy format"
    );
    assert_eq!(probe(&proxy, "width"), "320", "half width");
    assert_eq!(probe(&proxy, "height"), "240", "half height");

    let v1 = rig.track(0);
    rig.place(v1, bars, 0.0);
    rig.canvas(320, 240);
    let original = rig.preview(0.5);
    rig.engine
        .set_media_proxy_path(bars, Some(proxy.clone()))
        .expect("attach proxy");
    rig.engine.set_use_proxies(true).expect("use proxies");
    let proxied = rig.preview(0.5);
    assert_ne!(original, proxied, "the preview reads the proxy");
    // The centres of the seven top bars, away from their edges.
    for bar in 0..7u32 {
        let (x, y) = ((2 * bar + 1) * 320 / 14, 60);
        let (a, b) = (px(&original, 320, x, y), px(&proxied, 320, x, y));
        assert!(
            (0..3).all(|c| a[c].abs_diff(b[c]) <= 3),
            "bar {bar} at ({x},{y}): proxy {b:?} vs original {a:?}"
        );
    }
}

/// V2.13: a layer the viewer can't show is reported once per clip, not per
/// frame. Needs compose's skipped-layer report (seam V1).
#[test]
fn a_layer_that_cant_be_shown_gives_one_notice() {
    let Some(mut rig) = Rig::new("fix-notice") else {
        return;
    };
    let Some(blue) = rig.solid("blue", 0x0000FF, W, H) else {
        return;
    };
    let file = rig.scratch.path("gone", "mp4");
    if !encode(&file, &solid_src(0xFF0000, W, H, 2.0), 2.0, 24) {
        return;
    }
    let gone = rig.import(&file);
    let name = rig.media_name(gone);
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, blue, 0.0);
    rig.place(v2, gone, 0.0);
    rig.canvas(W, H);
    let file = file.canonicalize().expect("canonical path");
    let away = rig.scratch.path("gone-away", "mp4");
    std::fs::rename(&file, &away).expect("move the file away");
    let _ = rig.engine.take_notices();

    let frame = rig.preview(0.5);
    let second = rig.preview(0.75);
    let notices = rig.engine.take_notices();
    std::fs::rename(&away, &file).expect("restore the file");
    expect_at(
        &frame,
        W,
        &[(W / 2, H / 2)],
        BLUE,
        COARSE,
        "the layer below shows",
    );
    expect_at(
        &second,
        W,
        &[(W / 2, H / 2)],
        BLUE,
        COARSE,
        "and keeps showing",
    );
    let ours: Vec<&String> = notices
        .iter()
        .filter(|n| n.starts_with(&format!("Couldn't show {name}: ")))
        .collect();
    assert_eq!(ours.len(), 1, "one notice for the clip, got {notices:?}");
}

/// V2.8 / decision 1: an old project's canvas comes from its earliest video
/// clip's DISPLAY size (anamorphic pixels made square). Needs the stream's
/// sample aspect ratio (seam A).
#[test]
fn an_old_project_takes_the_display_size_of_its_anamorphic_video() {
    let Some(mut rig) = Rig::new("fix-anamorphic") else {
        return;
    };
    let Some(wide) = rig.clip("wide", "color=c=0x808080:s=360x240:r=24:d=1,setsar=2", 1.0) else {
        return;
    };
    let path = rig.engine.media_path_for(wide).expect("path");
    assert_eq!(probe(&path, "sample_aspect_ratio"), "2:1", "fixture SAR");
    let v1 = rig.track(0);
    rig.place(v1, wide, 0.0);
    assert_eq!(
        rig.engine.current_sequence_contents().unwrap().frame_size,
        Some([720, 240]),
        "the first clip sets its display size"
    );
    // As saved before sequences had a canvas size.
    rig.engine.set_frame_size(None).expect("clear size");
    let bundle =
        std::env::temp_dir().join(format!("vance-fix-anamorphic-{}.vance", std::process::id()));
    let _ = std::fs::remove_dir_all(&bundle);
    rig.engine
        .save_project(Some(bundle.to_string_lossy().into_owned()))
        .expect("save");
    let mut opened = vance_app_lib::engine::Engine::default();
    let loaded = opened.load_project(&bundle.to_string_lossy());
    let _ = std::fs::remove_dir_all(&bundle);
    loaded.expect("load");
    assert_eq!(
        opened.current_sequence_contents().unwrap().frame_size,
        Some([720, 240])
    );
}

/// An HDR (PQ BT.2020) source's proxy is 10-bit ProRes 422 Proxy holding
/// PQ BT.2020 values, tagged so, and previews like the original.
#[test]
fn an_hdr_proxy_is_tagged_hdr_and_previews_like_the_original() {
    let Some(mut rig) = Rig::new("fix-hdr-proxy") else {
        return;
    };
    let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../vance-decoder/tests/fixtures/hdr10_320x240_2s.mp4");
    if !fixture.exists() {
        eprintln!("HDR fixture missing — skipping");
        return;
    }
    // The encoder sidecar runs ffmpeg; HDR proxies need its ProRes encoder.
    let encoders = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if !encoders.contains("prores_ks") {
        eprintln!("ffmpeg has no prores_ks encoder — skipping");
        return;
    }
    let hdr = rig.import(&fixture);
    let source = rig.engine.media_path_for(hdr).expect("source path");
    let proxy = rig.scratch.path("hdr-proxy", "mov");
    let id = rig
        .engine
        .enqueue_proxy_generation(
            source.to_string_lossy().into_owned(),
            proxy.to_string_lossy().into_owned(),
            ProxyCodec::ProRes422,
        )
        .expect("enqueue proxy");
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let job = rig
            .engine
            .proxy_jobs_snapshot()
            .into_iter()
            .find(|j| j.id == id)
            .expect("the job");
        match job.status {
            ProxyJobStatus::Done { .. } => break,
            ProxyJobStatus::Error(e) => panic!("proxy generation failed: {e}"),
            _ if Instant::now() > deadline => panic!("proxy generation timed out"),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    for (field, want) in [
        ("codec_name", "prores"),
        ("profile", "Proxy"),
        ("pix_fmt", "yuv422p10le"),
        ("color_transfer", "smpte2084"),
        ("color_primaries", "bt2020"),
        ("color_space", "bt2020nc"),
        ("color_range", "tv"),
        ("width", "160"),
        ("height", "120"),
    ] {
        assert_eq!(probe(&proxy, field), want, "HDR proxy {field}");
    }
    let marker = vance_app_lib::proxy::current_marker(&proxy).expect("a current marker");
    assert_eq!(
        marker.codec,
        ProxyCodec::ProRes422,
        "the codec asked for, remade with"
    );

    let v1 = rig.track(0);
    rig.place(v1, hdr, 0.0);
    rig.canvas(160, 120);
    let original = rig.preview(0.5);
    rig.engine
        .set_media_proxy_path(hdr, Some(proxy.clone()))
        .expect("attach proxy");
    rig.engine.set_use_proxies(true).expect("use proxies");
    let proxied = rig.preview(0.5);
    assert_ne!(original, proxied, "the preview reads the proxy");
    // Flat 8×8 blocks of the original (the fixture is colour bars): a
    // transfer or matrix mistake would move them all. At the bars' edges the
    // two pictures differ by design: the proxy's 4:2:0 chroma is at half
    // the source's resolution again, and the GPU fit samples differently.
    let (mut flat, mut worst) = (0, 0u8);
    for by in (0..120).step_by(8) {
        for bx in (0..160).step_by(8) {
            let block = |b: &[u8]| -> Vec<[u8; 4]> {
                (by..by + 8)
                    .flat_map(|y| (bx..bx + 8).map(move |x| (x, y)))
                    .map(|(x, y)| px(b, 160, x, y))
                    .collect()
            };
            let (a, p) = (block(&original), block(&proxied));
            let uniform = |b: &[[u8; 4]]| {
                (0..3).all(|c| {
                    let v = b.iter().map(|q| q[c]);
                    v.clone().max().unwrap() - v.min().unwrap() <= 2
                })
            };
            // Flat in both: a bar edge one pixel off isn't a colour error.
            if !uniform(&a) || !uniform(&p) {
                continue;
            }
            flat += 1;
            worst = worst.max((0..3).map(|c| a[0][c].abs_diff(p[0][c])).max().unwrap());
        }
    }
    assert!(flat >= 100, "too few flat blocks to compare: {flat}");
    // ±3: the proxy's 10-bit codes come back within 2 of the source's
    // (measured with ffmpeg), plus 8-bit rounding of the read-back; measured
    // worst 1.
    assert!(
        worst <= 3,
        "the proxy previews off by {worst} in flat areas"
    );
}

/// The scratch files of these tests take their proxy markers with them.
#[test]
fn scratch_files_take_their_proxy_markers_with_them() {
    let mut scratch = Scratch::new("fix-scratch");
    let proxy = scratch.path("proxy", "mov");
    std::fs::write(&proxy, b"proxy").unwrap();
    let mut marker = proxy.clone().into_os_string();
    marker.push(".vance-proxy");
    let marker = std::path::PathBuf::from(marker);
    std::fs::write(&marker, b"vance-proxy-format=4").unwrap();
    drop(scratch);
    assert!(
        !proxy.exists() && !marker.exists(),
        "nothing left in the temp folder"
    );
}

/// A FIFO at `at`: opening it for reading blocks until a writer comes, as an
/// open on a hung drive does.
#[cfg(unix)]
fn make_fifo(at: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(at.as_os_str().as_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path; mkfifo only reads it.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0, "mkfifo");
}

/// Let a reader blocked opening `fifo` through (it then reads end of file),
/// so a test that failed doesn't leave a decoder hung.
#[cfg(unix)]
fn release_fifo(fifo: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(fifo);
}

/// Let a writer blocked opening `fifo` through (its writes then fail), so a
/// test that failed doesn't leave a thread blocked.
#[cfg(unix)]
fn release_reader(fifo: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(fifo);
}

/// V2 (0–2 s) over V1 (0–4 s); after a warm-up frame at 3 s (V1 only:
/// renderer, decoders, encoder search all started), V2's file is replaced
/// by a FIFO (its drive "hangs").
#[cfg(unix)]
fn rig_with_a_hung_file(tag: &'static str) -> Option<(Rig, std::path::PathBuf, String)> {
    let mut rig = Rig::new(tag)?;
    let blue = rig.clip("blue", &solid_src(0x0000FF, W, H, 4.0), 4.0)?;
    let file = rig.scratch.path("hung", "mp4");
    if !encode(&file, &solid_src(0xFF0000, W, H, 2.0), 2.0, 24) {
        return None;
    }
    let hung = rig.import(&file);
    let name = rig.media_name(hung);
    let (v1, v2) = (rig.track(0), rig.track(1));
    rig.place(v1, blue, 0.0);
    rig.place(v2, hung, 0.0);
    rig.canvas(W, H);
    rig.preview(3.0);
    rig.export(3.0);
    let file = file.canonicalize().expect("canonical path");
    std::fs::remove_file(&file).unwrap();
    make_fifo(&file);
    Some((rig, file, name))
}

/// A hung drive must not hang an export: cancelling it kills the decoder
/// stuck opening the file, and nothing is written.
#[cfg(unix)]
#[test]
fn an_export_stuck_on_a_hung_drive_is_cancelled_promptly() {
    let Some((mut rig, fifo, _)) = rig_with_a_hung_file("fix-cancel") else {
        return;
    };
    let canceller = rig.engine.export_canceller();
    let out = rig.scratch.path("cancelled", "mov");
    let target = out.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(export(&mut rig, &target, 1.0));
        rig
    });
    std::thread::sleep(Duration::from_millis(1500));
    let cancelled_at = Instant::now();
    canceller.cancel();
    let result = rx.recv_timeout(Duration::from_secs(10));
    let took = cancelled_at.elapsed();
    if result.is_err() {
        release_fifo(&fifo);
    }
    let rig = worker.join().expect("export thread");
    let dir = out.parent().unwrap().to_path_buf();
    let stem = out.file_stem().unwrap().to_string_lossy().into_owned();
    let partial_left = std::fs::read_dir(&dir).unwrap().flatten().any(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        n.contains(&stem) && n.contains("vance-partial")
    });
    drop(rig);
    assert_eq!(
        result.ok(),
        Some(Err("Export cancelled".to_string())),
        "the export ends as cancelled"
    );
    assert!(took < Duration::from_secs(5), "promptly: {took:?}");
    assert!(!out.exists(), "no output file");
    assert!(!partial_left, "no partial file either");
}

/// A scrub on a slow drive waits for it instead of killing the decoder at
/// a few seconds (the viewer keeps its picture meanwhile; see the unit
/// tests): once the drive answers the frame comes, here without the layer
/// (the FIFO gives no picture), with a notice, and the next frame doesn't
/// wait on that file again.
#[cfg(unix)]
#[test]
fn a_scrub_on_a_slow_drive_waits_for_it() {
    let Some((mut rig, fifo, name)) = rig_with_a_hung_file("fix-scrub-slow") else {
        return;
    };
    let _ = rig.engine.take_notices();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let timed = |rig: &mut Rig, t: f64| {
            let started = Instant::now();
            let frame = rig.preview(t);
            (frame, started.elapsed())
        };
        let first = timed(&mut rig, 0.5);
        let next = timed(&mut rig, 0.75);
        let notices = rig.engine.take_notices();
        let _ = tx.send((first, next, notices));
        rig
    });
    // Past the old 3 s kill: the drive answers at 4 s.
    let answers_after = Duration::from_secs(4);
    std::thread::sleep(answers_after);
    release_fifo(&fifo);
    let result = rx.recv_timeout(Duration::from_secs(20));
    if result.is_err() {
        release_fifo(&fifo);
    }
    let rig = worker.join().expect("scrub thread");
    drop(rig);
    let ((first, first_took), (_, next_took), notices) = result.expect("the scrub returned");
    assert!(
        first_took >= answers_after - Duration::from_millis(100),
        "waited for the drive: {first_took:?}"
    );
    assert!(first_took < Duration::from_secs(8), "{first_took:?}");
    expect_at(
        &first,
        W,
        &[(W / 2, H / 2)],
        BLUE,
        COARSE,
        "the layer below shows",
    );
    assert!(
        next_took < Duration::from_secs(2),
        "not waited on again: {next_took:?}"
    );
    assert!(
        notices
            .iter()
            .any(|n| n.starts_with(&format!("Couldn't show {name}: "))),
        "{notices:?}"
    );
}

fn export_options(out: &Path, end: f64) -> RenderOptions {
    RenderOptions {
        output_path: out.to_string_lossy().into_owned(),
        preset: EncoderPreset::ProRes422HQ,
        width: None,
        height: None,
        render_and_replace_track: None,
        start_seconds: Some(0.0),
        end_seconds: Some(end),
        hdr: false,
    }
}

/// While an export holds the engine, other commands answer "Export in
/// progress" at once (the UI matches that string); Cancel still gets
/// through; afterwards commands work again.
#[cfg(unix)]
#[test]
fn other_commands_answer_at_once_during_an_export() {
    use vance_app_lib::engine::{EngineHandle, EXPORT_IN_PROGRESS};
    let Some((mut rig, fifo, _)) = rig_with_a_hung_file("fix-busy") else {
        return;
    };
    let v1 = rig.track(0);
    let out = rig.scratch.path("busy", "mov");
    let handle = EngineHandle::with_engine(std::mem::take(&mut rig.engine));
    let canceller = handle.lock().export_canceller();
    let (exporter, options) = (handle.clone(), export_options(&out, 1.0));
    let export = std::thread::spawn(move || {
        exporter.run_export(|e| {
            e.render_sequence(options)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_exporting() {
        assert!(Instant::now() < deadline, "the export didn't start");
        std::thread::sleep(Duration::from_millis(5));
    }
    // By now it's stuck on the hung file, holding the engine.
    std::thread::sleep(Duration::from_millis(500));
    let (tx, rx) = std::sync::mpsc::channel();
    let editor = handle.clone();
    std::thread::spawn(move || {
        let asked = Instant::now();
        // As the `set_track_enabled` command runs it.
        let answer = editor
            .engine()
            .and_then(|mut e| e.set_track_enabled(v1, false).map_err(|e| e.to_string()));
        let _ = tx.send((answer, asked.elapsed()));
    });
    let answered = rx.recv_timeout(Duration::from_secs(5));
    if answered.is_err() {
        release_fifo(&fifo);
    }
    let second = handle.run_export(|_| ());
    let poll = handle.lock_within(Duration::from_millis(250)).is_none();
    canceller.cancel();
    let exported = export.join().expect("export thread");
    let (answer, took) = answered.expect("the edit answered");
    assert_eq!(answer, Err(EXPORT_IN_PROGRESS.to_string()));
    assert_eq!(
        EXPORT_IN_PROGRESS, "Export in progress",
        "the frozen string"
    );
    assert!(took < Duration::from_millis(100), "at once: {took:?}");
    assert_eq!(
        second,
        Err(EXPORT_IN_PROGRESS.to_string()),
        "one export at a time"
    );
    assert!(poll, "polls skip instead of waiting");
    assert_eq!(
        exported,
        Ok(Err("Export cancelled".to_string())),
        "cancel got through"
    );
    let after = handle.engine().expect("free again");
    drop(after);
    handle
        .engine()
        .and_then(|mut e| e.set_track_enabled(v1, false).map_err(|e| e.to_string()))
        .expect("edits work again");
}

/// A hung AUDIO file can't hang an export either: Cancel kills the
/// mixdown's audio decoder too.
#[cfg(unix)]
#[test]
fn cancel_reaches_an_audio_mixdown_stuck_on_a_hung_drive() {
    let Some(mut rig) = Rig::new("fix-cancel-audio") else {
        return;
    };
    let Some(blue) = rig.solid("blue", 0x0000FF, W, H) else {
        return;
    };
    let wav = rig.scratch.path("tone", "wav");
    let made = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args(["-i", "sine=frequency=440:duration=2"])
        .arg(&wav)
        .status()
        .is_ok_and(|s| s.success());
    if !made {
        eprintln!("ffmpeg couldn't make a WAV — skipping");
        return;
    }
    let wav = wav.canonicalize().unwrap();
    let info = rig
        .engine
        .import_audio_file(&wav.to_string_lossy())
        .expect("import audio");
    let tone = vance_timeline::MediaItemId::from_uuid(info.id.parse().unwrap());
    rig.engine
        .set_media_has_audio_track(tone, true)
        .expect("audio flag");
    let a1 = {
        let contents = rig.engine.current_sequence_contents().unwrap();
        let a1 = contents.tracks.iter().find(|t| t.kind == "audio").unwrap();
        vance_timeline::TrackId::from_uuid(a1.id.parse().unwrap())
    };
    let v1 = rig.track(0);
    rig.place(v1, blue, 0.0);
    rig.place(a1, tone, 0.0);
    rig.canvas(W, H);
    rig.export(0.5);
    std::fs::remove_file(&wav).unwrap();
    make_fifo(&wav);
    let canceller = rig.engine.export_canceller();
    let out = rig.scratch.path("cancelled-audio", "mov");
    let target = out.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(export(&mut rig, &target, 0.5));
        rig
    });
    // After the 12 video frames the mixdown starts, and opens the hung file.
    let deadline = Instant::now() + Duration::from_secs(20);
    while !canceller.mixing_audio() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let mixing = canceller.mixing_audio();
    // Stuck in the open by now (it doesn't return while the FIFO is shut).
    std::thread::sleep(Duration::from_millis(300));
    let cancelled_at = Instant::now();
    canceller.cancel();
    let result = rx.recv_timeout(Duration::from_secs(10));
    let took = cancelled_at.elapsed();
    if result.is_err() {
        release_fifo(&wav);
    }
    drop(worker.join().expect("export thread"));
    assert!(mixing, "the mixdown started");
    assert_eq!(result.ok(), Some(Err("Export cancelled".to_string())));
    assert!(took < Duration::from_secs(5), "promptly: {took:?}");
    assert!(!out.exists(), "no output file");
}

/// A cancel after the audio is pushed, just before the encoder finishes,
/// still stops the export: nothing is written.
#[test]
fn a_cancel_just_before_the_encoder_finishes_writes_nothing() {
    let Some(mut rig) = Rig::new("fix-cancel-finish") else {
        return;
    };
    let Some(blue) = rig.solid("blue", 0x0000FF, W, H) else {
        return;
    };
    let v1 = rig.track(0);
    rig.place(v1, blue, 0.0);
    rig.canvas(W, H);
    let canceller = rig.engine.export_canceller();
    let late = canceller.clone();
    canceller.on_audio_pushed(move || late.cancel());
    let out = rig.scratch.path("cancelled-late", "mov");
    let result = export(&mut rig, &out, 0.5);
    let dir = out.parent().unwrap().to_path_buf();
    let stem = out.file_stem().unwrap().to_string_lossy().into_owned();
    let partial_left = std::fs::read_dir(&dir).unwrap().flatten().any(|e| {
        let n = e.file_name().to_string_lossy().into_owned();
        n.contains(&stem) && n.contains("vance-partial")
    });
    assert_eq!(result, Err("Export cancelled".to_string()));
    assert!(!out.exists(), "no output file");
    assert!(!partial_left, "no partial file either");
}

/// A cancel clicked while the export still waits for the engine (the
/// command before holds it) counts: the export doesn't start.
#[test]
fn a_cancel_while_the_export_waits_for_the_engine_counts() {
    use vance_app_lib::engine::EngineHandle;
    let Some(mut rig) = Rig::new("fix-cancel-early") else {
        return;
    };
    let Some(blue) = rig.solid("blue", 0x0000FF, W, H) else {
        return;
    };
    let v1 = rig.track(0);
    rig.place(v1, blue, 0.0);
    rig.canvas(W, H);
    let out = rig.scratch.path("cancelled-early", "mov");
    let handle = EngineHandle::with_engine(std::mem::take(&mut rig.engine));
    let canceller = handle.export_canceller();
    // A slow command holds the engine.
    let busy = handle.lock();
    let (exporter, options) = (handle.clone(), export_options(&out, 0.5));
    let export = std::thread::spawn(move || {
        exporter.run_export(|e| {
            e.render_sequence(options)
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_exporting() {
        assert!(Instant::now() < deadline, "the export didn't start");
        std::thread::sleep(Duration::from_millis(5));
    }
    canceller.cancel();
    drop(busy);
    let exported = export.join().expect("export thread");
    assert_eq!(exported, Err("Export cancelled".to_string()));
    assert!(!out.exists(), "no output file");
    // And the next export isn't cancelled by it.
    let again = handle.run_export(|e| {
        e.render_sequence(export_options(&out, 0.5))
            .map(|_| ())
            .map_err(|e| e.to_string())
    });
    assert_eq!(again, Ok(Ok(())));
    assert!(out.exists(), "the next export writes its file");
}

/// A file with picture and sound, made by ffmpeg (`None` without it).
fn video_with_sound(rig: &mut Rig, label: &str, container: &str) -> Option<std::path::PathBuf> {
    let out = rig.scratch.path(label, container);
    let made = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args(["-i", "testsrc=size=64x48:rate=24:duration=1", "-f", "lavfi"])
        .args(["-i", "sine=frequency=440:duration=1"])
        .args([
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
        ])
        .arg(&out)
        .status()
        .is_ok_and(|s| s.success());
    if !made {
        eprintln!("ffmpeg couldn't make a {container} with sound — skipping");
        return None;
    }
    Some(out)
}

/// An import whose file opens at once says nothing about waiting, and
/// knows whether its file has sound.
#[test]
fn a_quick_import_says_nothing_and_learns_its_sound() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use vance_app_lib::engine::EngineHandle;
    let Some(mut rig) = Rig::new("fix-import-quick") else {
        return;
    };
    let Some(file) = video_with_sound(&mut rig, "with-sound", "mp4") else {
        return;
    };
    let handle = EngineHandle::with_engine(std::mem::take(&mut rig.engine));
    let told = Arc::new(AtomicUsize::new(0));
    let waiting = told.clone();
    let info = handle
        .import_file(
            &file.to_string_lossy(),
            move || {
                waiting.fetch_add(1, Ordering::SeqCst);
            },
            || {
                told.fetch_add(10, Ordering::SeqCst);
            },
        )
        .expect("imported");
    assert!(info.has_audio_track, "it has sound");
    assert_eq!(info.kind, "video");
    assert_eq!(told.load(Ordering::SeqCst), 0, "no waiting, no settled");
}

/// Runs an import of `fifo` on its own thread: its result arrives on the
/// returned channel; "waiting" / "settled" on `events`.
#[cfg(unix)]
fn import_in_background(
    handle: &vance_app_lib::engine::EngineHandle,
    fifo: &Path,
    events: std::sync::mpsc::Sender<&'static str>,
) -> std::sync::mpsc::Receiver<Result<vance_app_lib::engine::MediaItemInfo, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (importer, path) = (handle.clone(), fifo.to_string_lossy().into_owned());
    std::thread::spawn(move || {
        let waiting = events.clone();
        let result = importer.import_file(
            &path,
            move || {
                let _ = waiting.send("waiting");
            },
            || {
                let _ = events.send("settled");
            },
        );
        let _ = tx.send(result);
    });
    rx
}

/// An import waiting on a hung drive doesn't hold the engine; after 2 s it
/// says it's waiting, and Cancel stops it ("Import cancelled", nothing
/// added), then it says the wait settled.
#[cfg(unix)]
#[test]
fn an_import_waiting_on_a_hung_drive_can_be_cancelled() {
    use vance_app_lib::engine::{file_label, EngineHandle, IMPORT_CANCELLED};
    let Some(mut rig) = Rig::new("fix-import-cancel") else {
        return;
    };
    let fifo = rig.scratch.path("on-the-nas", "mov");
    make_fifo(&fifo);
    let handle = EngineHandle::with_engine(std::mem::take(&mut rig.engine));
    let (events_tx, events) = std::sync::mpsc::channel();
    let started = Instant::now();
    let imported = import_in_background(&handle, &fifo, events_tx);
    let first = events.recv_timeout(Duration::from_secs(10));
    let waited = started.elapsed();
    // Meanwhile the engine answers other commands.
    let asked = Instant::now();
    let free = handle.engine().map(|e| e.media_list().len());
    let answered = asked.elapsed();
    let cancelled_at = Instant::now();
    handle.cancel_import();
    let result = imported.recv_timeout(Duration::from_secs(10));
    let took = cancelled_at.elapsed();
    if result.is_err() {
        release_fifo(&fifo);
    }
    let then = events.recv_timeout(Duration::from_secs(1));
    assert_eq!(first, Ok("waiting"));
    assert!(
        waited >= Duration::from_millis(1900) && waited < Duration::from_secs(5),
        "said so after 2 s: {waited:?}"
    );
    assert_eq!(free, Ok(0), "the engine was free");
    assert!(answered < Duration::from_millis(500), "{answered:?}");
    assert_eq!(
        result.map(|r| r.map(|_| ())),
        Ok(Err(IMPORT_CANCELLED.to_string()))
    );
    assert_eq!(IMPORT_CANCELLED, "Import cancelled", "the frozen string");
    assert!(took < Duration::from_secs(3), "promptly: {took:?}");
    assert_eq!(then, Ok("settled"));
    assert_eq!(
        file_label("/Volumes/NAS/Footage/on-the-nas.mov"),
        "on-the-nas.mov",
        "the events' file_name is the bare name"
    );
    let after = handle.engine().expect("free").media_list();
    assert!(after.is_empty(), "nothing added");
}

/// An import whose file opens while an export runs waits the export out
/// and is added afterwards, instead of failing with "Export in progress"
/// after it did the slow part.
#[cfg(unix)]
#[test]
fn an_import_that_opens_during_an_export_is_added_after_it() {
    use std::io::Write;
    use vance_app_lib::engine::EngineHandle;
    let Some(mut rig) = Rig::new("fix-import-export") else {
        return;
    };
    let Some(stream) = video_with_sound(&mut rig, "stream", "ts") else {
        return;
    };
    let bytes = std::fs::read(&stream).unwrap();
    let fifo = rig.scratch.path("slow-share", "ts");
    make_fifo(&fifo);
    let handle = EngineHandle::with_engine(std::mem::take(&mut rig.engine));
    let (events_tx, events) = std::sync::mpsc::channel();
    let imported = import_in_background(&handle, &fifo, events_tx);
    // Waiting on its file: now an export starts, and holds the engine.
    assert_eq!(events.recv_timeout(Duration::from_secs(10)), Ok("waiting"));
    let (end_export, export_ends) = std::sync::mpsc::channel::<()>();
    let exporter = handle.clone();
    let export = std::thread::spawn(move || exporter.run_export(|_| export_ends.recv()));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_exporting() {
        assert!(Instant::now() < deadline, "the export didn't start");
        std::thread::sleep(Duration::from_millis(5));
    }
    // The drive answers: the file opens (as often as it's opened, for its
    // picture and its sound).
    let feeding = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (feed, still) = (fifo.clone(), feeding.clone());
    let feeder = std::thread::spawn(move || {
        while still.load(std::sync::atomic::Ordering::SeqCst) {
            let Ok(mut writer) = std::fs::OpenOptions::new().write(true).open(&feed) else {
                return;
            };
            let _ = writer.write_all(&bytes);
        }
    });
    let during = imported.recv_timeout(Duration::from_secs(2));
    let _ = end_export.send(());
    let exported = export.join().expect("export thread");
    let after = imported.recv_timeout(Duration::from_secs(10));
    assert!(during.is_err(), "not added during the export: {during:?}");
    assert_eq!(exported.map(|r| r.is_ok()), Ok(true));
    feeding.store(false, std::sync::atomic::Ordering::SeqCst);
    release_reader(&fifo);
    let _ = feeder.join();
    let info = after.expect("came back").expect("added after the export");
    assert_eq!(*info.name, *fifo.file_stem().unwrap().to_string_lossy());
    assert_eq!(events.recv_timeout(Duration::from_secs(1)), Ok("settled"));
    assert_eq!(handle.engine().expect("free").media_list().len(), 1);
}
