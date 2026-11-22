//! AVI: a container that stores no presentation timestamps.
//!
//! An AVI index orders a stream's chunks in decode order and gives each a
//! dts; FFmpeg fills in a packet pts only where it can guess one (every
//! packet of an intra-only stream, the B-frames of MPEG-4 Part 2, none of
//! H.264). Frames decoded from the rest came out without a pts and were
//! all sent as pts 0: a stream from 0 gave fifty frames at pts 0 and
//! `decode_at` failed for every frame after the first.
//!
//! With B-frames, FFmpeg's best-effort timestamp for such a frame is the
//! dts of the packet whose decoding released it, which is the decoder's
//! reorder delay (1 frame for MPEG-4 Part 2, 2 for this H.264) later than
//! the frame's own slot, and the last frames, released by the end-of-stream
//! drain, get none. The same content in MOV shows frame `k` at `k / 25` s,
//! so that is what each AVI here has to give: the same frames as the
//! `ffmpeg` CLI decodes, at the MOV's times, strictly increasing, and
//! `decode_at` and streams started anywhere giving the frames of a stream
//! from 0.
//!
//! Each codec is checked in two layouts: an AVI encoded directly (one
//! chunk per frame, time base 1/25, headers in band, as recorders write
//! them) and the MOV's bitstream remuxed with `-c copy`, which FFmpeg writes
//! with time base 1/600 and 23 empty chunks after every frame, so dts step
//! by 24. Every case runs with one decode thread (a stream runs on the
//! one-shot decoder) and with eight (it runs on its own, frame-threaded
//! decoder); the H.264 and MPEG-4 files also run on VideoToolbox, the
//! app's default path for them on a Mac. The fixtures are generated with the
//! `ffmpeg` CLI and skipped when it or the encoder isn't available.

use std::path::{Path, PathBuf};
use std::process::Command;
use vance_decoder::client::ClientError;
use vance_decoder::{DecoderClient, FrameData, PixelFormat};

fn sidecar_binary_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    let exe = if cfg!(windows) {
        "vance-decoder.exe"
    } else {
        "vance-decoder"
    };
    p.push(exe);
    p
}

/// A generated fixture file, deleted on drop.
struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Whether the local ffmpeg CLI can encode with `encoder`.
fn has_encoder(encoder: &str) -> bool {
    let Ok(out) = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
    else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.split_whitespace().nth(1) == Some(encoder))
}

/// A path in the temp dir for a fixture called `name`, unique to this run.
fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("vance-avi-{}-{name}", std::process::id()))
}

/// Run the ffmpeg CLI with `args` (after the common flags), or panic.
fn ffmpeg(what: &str, args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg failed to {what}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The fixtures of one codec: the same 50 frames (2 s of 25 fps, 320x240
/// `testsrc2`, 12-frame GOPs) in MOV, in an AVI encoded directly, and in
/// the MOV's bitstream remuxed into AVI.
struct Fixtures {
    mov: Fixture,
    avi: Fixture,
    remuxed_avi: Fixture,
}

/// [`Fixtures`] encoded with `encode` (the ffmpeg encoder arguments, the
/// encoder named second), or `None` (skip) without ffmpeg / the encoder.
fn make_fixtures(name: &str, encode: &[&str]) -> Option<Fixtures> {
    let encoder = encode[1];
    if !has_encoder(encoder) {
        eprintln!("{name}: ffmpeg with the {encoder} encoder not available, skipping");
        return None;
    }
    let source = ["-f", "lavfi", "-i", "testsrc2=s=320x240:r=25:d=2"];
    let fixed = ["-g", "12", "-sc_threshold", "0"];
    let mut made = Vec::new();
    for (suffix, muxer) in [("mov", "mov"), ("avi", "avi")] {
        let fixture = Fixture(temp_path(&format!("{name}.{suffix}")));
        let path = fixture.0.to_string_lossy().into_owned();
        let args: Vec<&str> = source
            .iter()
            .chain(encode)
            .chain(&fixed)
            .chain(&["-f", muxer, &path])
            .copied()
            .collect();
        ffmpeg(&format!("encode {name}.{suffix}"), &args);
        made.push(fixture);
    }
    let avi = made.pop().unwrap();
    let mov = made.pop().unwrap();

    let remuxed_avi = Fixture(temp_path(&format!("{name}-remuxed.avi")));
    let mov_path = mov.0.to_string_lossy().into_owned();
    let out_path = remuxed_avi.0.to_string_lossy().into_owned();
    let mut args = vec!["-i", &mov_path, "-map", "0:v:0", "-c", "copy"];
    if encoder == "libx264" {
        // AVI carries H.264 in Annex B, MOV in length-prefixed NAL units.
        args.extend(["-bsf:v", "h264_mp4toannexb"]);
    }
    args.extend(["-f", "avi", &out_path]);
    ffmpeg(&format!("remux {name} into AVI"), &args);
    Some(Fixtures {
        mov,
        avi,
        remuxed_avi,
    })
}

/// Every frame of the 320x240 fixture at `path`, decoded in order by the
/// `ffmpeg` CLI into packed 8-bit 4:2:0 planes, one per decoded frame
/// (`passthrough`: none dropped or repeated to fit a frame rate).
fn ffmpeg_cli_frames(path: &Path) -> Vec<Vec<u8>> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .expect("run ffmpeg");
    assert!(
        out.status.success(),
        "ffmpeg failed to decode {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let size = PixelFormat::Yuv420P.frame_size(320, 240);
    assert_eq!(
        out.stdout.len() % size,
        0,
        "{}: partial frame",
        path.display()
    );
    out.stdout.chunks_exact(size).map(<[u8]>::to_vec).collect()
}

/// How a sidecar decodes.
#[derive(Debug, Clone, Copy)]
enum Decode {
    /// Software, with this many decode threads.
    Software(u32),
    /// VideoToolbox where the codec has it (the default on a Mac).
    Default,
}

fn spawn(decode: Decode) -> DecoderClient {
    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing at {}", bin.display());
    match decode {
        Decode::Software(threads) => {
            let threads = threads.to_string();
            DecoderClient::spawn_with_env(
                &bin,
                [
                    ("VANCE_DECODE_THREADS", threads.as_str()),
                    // VideoToolbox leaves threading at 1: no stream decoder of its own.
                    ("VANCE_FORCE_SW", "1"),
                ],
            )
        }
        Decode::Default => DecoderClient::spawn(&bin),
    }
    .expect("spawn decoder")
}

/// Same pts, geometry, format, strides and pixels.
fn assert_same(what: &str, a: &FrameData, b: &FrameData) {
    assert_eq!(
        (a.pts, a.width, a.height, a.pixel_format, &a.strides),
        (b.pts, b.width, b.height, b.pixel_format, &b.strides),
        "{what}: frame description"
    );
    assert_eq!(a.bytes.len(), b.bytes.len(), "{what}: payload size");
    if let Some(i) = a.bytes.iter().zip(b.bytes.iter()).position(|(x, y)| x != y) {
        panic!("{what} (pts {}): pixels differ from byte {i}", a.pts);
    }
}

/// The packed pixels of `frame` are `want`'s, a frame from the ffmpeg CLI.
fn assert_cli_pixels(what: &str, frame: &FrameData, want: &[u8]) {
    assert_eq!(
        (
            frame.width,
            frame.height,
            frame.pixel_format,
            &frame.strides[..]
        ),
        (320, 240, PixelFormat::Yuv420P, &[320, 160, 160][..]),
        "{what}: frame description"
    );
    if let Some(i) = frame.bytes.iter().zip(want).position(|(x, y)| x != y) {
        panic!("{what} (pts {}): pixels differ from byte {i}", frame.pts);
    }
    assert_eq!(frame.bytes.len(), want.len(), "{what}: payload size");
}

/// Every frame of the stream started with `start_stream`, as owned copies.
fn drain(what: &str, client: &mut DecoderClient) -> Vec<FrameData> {
    let mut frames = Vec::new();
    while let Some(f) = client
        .next_frame()
        .unwrap_or_else(|e| panic!("{what}: frame {}: {e}", frames.len()))
    {
        frames.push(f.clone());
        assert!(frames.len() <= 1000, "{what}: stream never ends");
    }
    frames
}

/// A time `pts` in `time_base`, as an exact fraction of a second.
#[derive(Debug, Clone, Copy)]
struct Seconds(i64, (i64, i64));

impl PartialEq for Seconds {
    fn eq(&self, other: &Self) -> bool {
        let (Seconds(a, (an, ad)), Seconds(b, (bn, bd))) = (*self, *other);
        i128::from(a) * i128::from(an) * i128::from(bd)
            == i128::from(b) * i128::from(bn) * i128::from(ad)
    }
}

/// Frames to `decode_at`, in this order: the first, mid-GOP, both sides of
/// a GOP boundary (keyframes at 0, 12, 24, 36 and 48), the last two, then
/// jumps back and forth across GOPs.
const SEEK_ORDER: [usize; 17] = [
    0, 5, 11, 12, 13, 25, 49, 48, 47, 30, 24, 23, 6, 0, 36, 35, 1,
];

/// Frames to start streams at: the first, both sides of a GOP boundary,
/// mid-GOP, and the last two.
const STREAM_STARTS: [usize; 6] = [0, 11, 12, 18, 48, 49];

/// In a sidecar decoding with `decode`: the AVI at `avi` streams from 0 the
/// frames of the MOV at `mov` (the same content, with real pts), at the
/// same times in seconds, with strictly increasing pts that stay inside the
/// stream's duration; with `cli` (the ffmpeg CLI's decode of the AVI), with
/// exactly its pixels. `decode_at` each frame of [`SEEK_ORDER`] gives that
/// frame, a target past the last frame fails without breaking the sidecar,
/// and a stream started at each frame of [`STREAM_STARTS`] gives it and
/// every frame after. Returns the AVI frames' pts.
fn assert_avi_plays_like_mov(
    what: &str,
    decode: Decode,
    avi: &Path,
    mov: &Path,
    cli: Option<&[Vec<u8>]>,
) -> Vec<i64> {
    let what = format!("{what}, {decode:?}");
    let mut client = spawn(decode);

    let mov_info = client
        .open(mov.to_string_lossy().into_owned())
        .unwrap_or_else(|e| panic!("{what}: open mov: {e}"));
    client.start_stream(0, 4).expect("start_stream mov");
    let reference = drain(&format!("{what}: mov from 0"), &mut client);
    assert_eq!(reference.len(), 50, "{what}: frames in the mov");

    let info = client
        .open(avi.to_string_lossy().into_owned())
        .unwrap_or_else(|e| panic!("{what}: open: {e}"));
    let tb = info.time_base;
    // The rate frames are shown at, not the AVI's chunk rate: the `-c copy`
    // remux counts 24 chunks a frame (23 of them empty), 600 "fps".
    let (rate, mov_rate) = (info.frame_rate, mov_info.frame_rate);
    assert_eq!(
        i128::from(rate.0) * i128::from(mov_rate.1),
        i128::from(mov_rate.0) * i128::from(rate.1),
        "{what}: frame rate {rate:?} vs the mov's {mov_rate:?}"
    );
    client.start_stream(0, 4).expect("start_stream at 0");
    let frames = drain(&format!("{what}: stream from 0"), &mut client);
    let pts: Vec<i64> = frames.iter().map(|f| f.pts).collect();
    assert_eq!(
        frames.len(),
        reference.len(),
        "{what}: frames from 0 {pts:?}"
    );
    for (i, pair) in pts.windows(2).enumerate() {
        assert!(
            pair[0] < pair[1],
            "{what}: pts not increasing at frame {}: {pts:?}",
            i + 1
        );
    }
    for (i, (frame, want)) in frames.iter().zip(&reference).enumerate() {
        assert_eq!(
            Seconds(frame.pts, tb),
            Seconds(want.pts, mov_info.time_base),
            "{what}: frame {i}: pts {} (time base {tb:?}) vs the mov's {} ({:?}); all {pts:?}",
            frame.pts,
            want.pts,
            mov_info.time_base
        );
    }
    let duration = info
        .duration_pts
        .unwrap_or_else(|| panic!("{what}: no duration"));
    assert!(
        pts[pts.len() - 1] < duration,
        "{what}: last frame at {} is past the duration {duration}",
        pts[pts.len() - 1]
    );
    if let Some(cli) = cli {
        assert_eq!(
            cli.len(),
            frames.len(),
            "{what}: frames the ffmpeg CLI decodes"
        );
        for (i, (frame, want)) in frames.iter().zip(cli).enumerate() {
            assert_cli_pixels(&format!("{what}: stream from 0, frame {i}"), frame, want);
        }
    }

    for k in SEEK_ORDER {
        let at = format!("{what}: decode_at frame {k}");
        let got = client
            .decode_at(frames[k].pts)
            .unwrap_or_else(|e| panic!("{at}: {e}"));
        assert_same(&at, &got, &frames[k]);
    }
    let past_end = pts[pts.len() - 1] + 1;
    match client.decode_at(past_end) {
        Err(ClientError::Decode(_)) => {}
        other => panic!(
            "{what}: decode_at {past_end}, past the last frame, gave {:?}",
            other.map(|f| f.pts)
        ),
    }
    let got = client
        .decode_at(frames[20].pts)
        .unwrap_or_else(|e| panic!("{what}: decode_at frame 20 after the end: {e}"));
    assert_same(
        &format!("{what}: decode_at frame 20 after the end"),
        &got,
        &frames[20],
    );

    for k in STREAM_STARTS {
        client
            .start_stream(frames[k].pts, 4)
            .unwrap_or_else(|e| panic!("{what}: start_stream at frame {k}: {e}"));
        let tail = drain(&format!("{what}: stream from frame {k}"), &mut client);
        assert_eq!(
            tail.len(),
            frames.len() - k,
            "{what}: frames from frame {k}; first pts {:?}",
            tail.first().map(|f| f.pts)
        );
        for (i, (got, want)) in tail.iter().zip(&frames[k..]).enumerate() {
            assert_same(
                &format!("{what}: stream from frame {k}, frame {}", k + i),
                got,
                want,
            );
        }
    }
    client.shutdown().expect("shutdown");
    pts
}

/// Both AVI layouts of `name`, encoded with `encode`, against its MOV and
/// the ffmpeg CLI, with 1 and with 8 decode threads (which must give the
/// same pts); then on VideoToolbox when `videotoolbox`.
fn assert_avis_play_like_mov(name: &str, encode: &[&str], videotoolbox: bool) {
    let Some(fx) = make_fixtures(name, encode) else {
        return;
    };
    let mov_cli = ffmpeg_cli_frames(&fx.mov.0);
    for (layout, avi) in [("avi", &fx.avi), ("remuxed avi", &fx.remuxed_avi)] {
        let what = format!("{name} {layout}");
        let cli = ffmpeg_cli_frames(&avi.0);
        if layout == "remuxed avi" {
            // The fixture is what it claims: the mov's bitstream.
            assert!(
                cli == mov_cli,
                "{what}: the ffmpeg CLI decodes the mov differently"
            );
        }
        let runs: Vec<Vec<i64>> = [1, 8]
            .into_iter()
            .map(|threads| {
                assert_avi_plays_like_mov(
                    &what,
                    Decode::Software(threads),
                    &avi.0,
                    &fx.mov.0,
                    Some(&cli),
                )
            })
            .collect();
        assert_eq!(runs[0], runs[1], "{what}: pts, 1 vs 8 threads");
        if videotoolbox && cfg!(target_os = "macos") {
            let pts = assert_avi_plays_like_mov(&what, Decode::Default, &avi.0, &fx.mov.0, None);
            assert_eq!(pts, runs[0], "{what}: pts, VideoToolbox vs software");
        }
    }
}

/// MPEG-4 Part 2 (DivX / Xvid style) without B-frames: FFmpeg guesses a pts
/// for every packet.
#[test]
fn mpeg4_avi_plays_and_seeks_like_mov() {
    assert_avis_play_like_mov("mpeg4", &["-c:v", "mpeg4", "-pix_fmt", "yuv420p"], true);
}

/// MPEG-4 Part 2 with B-frames: FFmpeg guesses a pts for the B-frames only,
/// and the decoder holds every other frame back by one.
#[test]
fn mpeg4_with_b_frames_avi_plays_and_seeks_like_mov() {
    assert_avis_play_like_mov(
        "mpeg4-bframes",
        &["-c:v", "mpeg4", "-bf", "2", "-pix_fmt", "yuv420p"],
        true,
    );
}

/// Motion JPEG (cameras, webcams): every frame a keyframe.
#[test]
fn mjpeg_avi_plays_and_seeks_like_mov() {
    assert_avis_play_like_mov("mjpeg", &["-c:v", "mjpeg", "-pix_fmt", "yuvj420p"], false);
}

/// H.264 without B-frames: no packet has a pts.
#[test]
fn h264_avi_plays_and_seeks_like_mov() {
    assert_avis_play_like_mov(
        "h264",
        &["-c:v", "libx264", "-bf", "0", "-pix_fmt", "yuv420p"],
        true,
    );
}

/// H.264 with B-frames: no packet has a pts, and the decoder reorders two
/// frames deep.
#[test]
fn h264_with_b_frames_avi_plays_and_seeks_like_mov() {
    assert_avis_play_like_mov(
        "h264-bframes",
        &["-c:v", "libx264", "-bf", "2", "-pix_fmt", "yuv420p"],
        true,
    );
}
