//! Annex-B H.264 / HEVC: in MPEG-TS, and bare.
//!
//! **Parameter sets once.** MPEG-TS whose parameter sets (H.264 SPS/PPS,
//! HEVC VPS/SPS/PPS) appear only once, at the start of the file. A stream
//! decodes on a second decoder opened from the stream's codec parameters
//! (ADR 0006), so after a mid-file seek no parameter set lies ahead of it in
//! the file. It decodes only because FFmpeg's stream probing
//! (`avformat_find_stream_info`) extracts the in-band parameter sets into
//! the parameters' extradata. The `*_parameter_sets_once_*` cases pin that
//! on all-intra fixtures, and the `gop_*` cases on 12-frame GOPs: a stream
//! started at 0 and one started mid-file give the same frames as
//! `decode_at`.
//!
//! **Seeking across GOPs.** MPEG-TS has no keyframe index, and FFmpeg's
//! timestamp seek in it can stop on any frame. Decoding from there gave
//! H.264's next GOP instead of the target (even for the first frame), and
//! HEVC frames without their references, until `seek` went on to find a
//! keyframe at or before the target. The `*_seeks_*` cases `decode_at` and
//! start streams on both sides of GOP boundaries, forward and back, and
//! compare with every frame decoded in order from the start, which in turn
//! is checked against the `ffmpeg` CLI's decode.
//!
//! **Bare streams.** A bare `.h264` / `.hevc` file has no timestamps, so it
//! can't be seeked at all; `open` rejects it with `NeedsContainer`.
//!
//! Every case runs with one decode thread (a stream runs on the one-shot
//! decoder) and with eight (it runs on its own, frame-threaded decoder).
//! The fixtures are generated with the `ffmpeg` CLI and skipped when it or
//! the encoder isn't available.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use vance_decoder::client::ClientError;
use vance_decoder::{DecoderClient, FrameData, OpenError, PixelFormat};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    H264,
    Hevc,
}

impl Codec {
    fn nal_type(self, header: u8) -> u8 {
        match self {
            Codec::H264 => header & 0x1f,
            Codec::Hevc => (header >> 1) & 0x3f,
        }
    }

    /// SPS / PPS (H.264), VPS / SPS / PPS (HEVC).
    fn parameter_set_types(self) -> &'static [u8] {
        match self {
            Codec::H264 => &[7, 8],
            Codec::Hevc => &[32, 33, 34],
        }
    }

    fn is_picture(self, nal_type: u8) -> bool {
        match self {
            Codec::H264 => (1..=5).contains(&nal_type),
            Codec::Hevc => nal_type <= 31,
        }
    }

    /// The first header byte of a filler-data NAL unit, which decoders and
    /// parsers skip. HEVC keeps the layer id's top bit (the second byte
    /// holds the rest).
    fn filler_header(self, header: u8) -> u8 {
        match self {
            Codec::H264 => 12,
            Codec::Hevc => (38 << 1) | (header & 1),
        }
    }

    fn filler_type(self) -> u8 {
        match self {
            Codec::H264 => 12,
            Codec::Hevc => 38,
        }
    }
}

/// File offsets of the video elementary stream's bytes in an MPEG-TS file,
/// in stream order: the PES payloads of the first PID that carries a video
/// PES.
fn ts_video_offsets(ts: &[u8]) -> Vec<usize> {
    assert_eq!(ts.len() % 188, 0, "not a whole number of TS packets");
    let mut video_pid = None;
    let mut offsets = Vec::new();
    for (n, pkt) in ts.chunks_exact(188).enumerate() {
        assert_eq!(pkt[0], 0x47, "TS packet {n}: sync byte");
        let pid = (u16::from(pkt[1] & 0x1f) << 8) | u16::from(pkt[2]);
        let unit_start = pkt[1] & 0x40 != 0;
        let control = (pkt[3] >> 4) & 3;
        let mut i = 4;
        if control & 2 != 0 {
            i += 1 + usize::from(pkt[4]);
        }
        if control & 1 == 0 || i >= 188 {
            continue;
        }
        let video_pes = pkt[i..].starts_with(&[0, 0, 1])
            && pkt.get(i + 3).is_some_and(|id| (0xe0..=0xef).contains(id));
        if unit_start && video_pes {
            if *video_pid.get_or_insert(pid) != pid {
                continue;
            }
            // Fixed PES header, then its optional fields.
            i += 9 + usize::from(pkt[i + 8]);
        } else if video_pid != Some(pid) {
            continue;
        }
        offsets.extend((i..188).map(|k| n * 188 + k));
    }
    offsets
}

/// Turn every parameter set after the first picture of the MPEG-TS file at
/// `path` into a filler NAL unit, in place: one header byte each, so the
/// TS packets stay as they are. Returns the NAL unit counts by type after.
fn keep_parameter_sets_only_at_start(path: &Path, codec: Codec) -> BTreeMap<u8, usize> {
    let mut ts = std::fs::read(path).expect("read fixture");
    let offsets = ts_video_offsets(&ts);
    let es: Vec<u8> = offsets.iter().map(|&o| ts[o]).collect();
    let mut counts = BTreeMap::new();
    let mut seen_picture = false;
    let mut i = 0;
    while i + 3 < es.len() {
        if es[i..i + 3] != [0, 0, 1] {
            i += 1;
            continue;
        }
        let header = i + 3;
        let mut nal_type = codec.nal_type(es[header]);
        if seen_picture && codec.parameter_set_types().contains(&nal_type) {
            ts[offsets[header]] = codec.filler_header(es[header]);
            nal_type = codec.filler_type();
        }
        seen_picture |= codec.is_picture(nal_type);
        *counts.entry(nal_type).or_insert(0) += 1;
        i = header;
    }
    std::fs::write(path, ts).expect("write fixture");
    counts
}

/// A path in the temp dir for a fixture called `name`, unique to this run.
fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("vance-annexb-{}-{name}", std::process::id()))
}

/// 2 s of 25 fps, 320x240 `testsrc2` (50 frames) encoded with `encoder` in
/// `gop`-frame GOPs (all-intra for 1, else with B-frames where the encoder
/// has them) and written by FFmpeg's `muxer`, or `None` (skip) without
/// ffmpeg / `encoder`.
fn encode_fixture(name: &str, encoder: &str, gop: u32, muxer: &str) -> Option<Fixture> {
    encode_source(name, "testsrc2=s=320x240:r=25:d=2", encoder, gop, muxer)
}

/// [`encode_fixture`] of the lavfi `source` (320x240 for the checks here).
fn encode_source(
    name: &str,
    source: &str,
    encoder: &str,
    gop: u32,
    muxer: &str,
) -> Option<Fixture> {
    if !has_encoder(encoder) {
        eprintln!("{name}: ffmpeg with the {encoder} encoder not available, skipping");
        return None;
    }
    let fixture = Fixture(temp_path(name));
    let gop_arg = gop.to_string();
    let x265_params = format!(
        "log-level=error:keyint={gop}:min-keyint={gop}:scenecut=0:open-gop=0:bframes={}",
        if gop > 1 { 2 } else { 0 }
    );
    let encode: Vec<&str> = match encoder {
        "libx264" => vec!["-c:v", "libx264", "-g", &gop_arg, "-bf", "2"],
        "libx265" => vec!["-c:v", "libx265", "-x265-params", &x265_params],
        "hevc_videotoolbox" | "mpeg2video" | "mpeg4" => vec!["-c:v", encoder, "-g", &gop_arg],
        other => panic!("no recipe for {other}"),
    };
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", source])
        .args(encode)
        .args(["-pix_fmt", "yuv420p", "-sc_threshold", "0", "-f", muxer])
        .arg(&fixture.0)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg failed to generate {name}");
    Some(fixture)
}

/// `src`'s video bitstream as it is (`-c copy`), written by FFmpeg's `muxer`.
fn remux(src: &Path, name: &str, muxer: &str) -> Fixture {
    let fixture = Fixture(temp_path(name));
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(src)
        .args(["-map", "0:v:0", "-c", "copy", "-f", muxer])
        .arg(&fixture.0)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg failed to remux into {name}");
    fixture
}

/// Every frame of the 320x240 fixture at `path`, decoded in order by the
/// `ffmpeg` CLI into packed 8-bit 4:2:0 planes: a decode that never seeks,
/// and doesn't go through the sidecar.
fn ffmpeg_cli_frames(path: &Path) -> Vec<Vec<u8>> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args([
            "-map", "0:v:0", "-pix_fmt", "yuv420p", "-f", "rawvideo", "-",
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

/// An MPEG-TS fixture of 50 frames whose parameter sets appear once, or
/// `None` (skip) without ffmpeg / `encoder`. `gop` 1 is all-intra; more gives
/// inter frames.
fn make_ts_fixture(name: &str, codec: Codec, encoder: &str, gop: u32) -> Option<Fixture> {
    let fixture = encode_fixture(&format!("{name}.ts"), encoder, gop, "mpegts")?;

    // The fixture must really be what this module is about: each parameter
    // set exactly once, after the encoder had repeated it at keyframes.
    let counts = keep_parameter_sets_only_at_start(&fixture.0, codec);
    for t in codec.parameter_set_types() {
        assert_eq!(
            counts.get(t),
            Some(&1),
            "{name}: NAL type {t} in {counts:?}"
        );
    }
    assert!(
        counts.contains_key(&codec.filler_type()),
        "{name}: the encoder never repeated its parameter sets ({counts:?})"
    );
    Some(fixture)
}

fn spawn(threads: u32) -> DecoderClient {
    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing at {}", bin.display());
    let threads = threads.to_string();
    DecoderClient::spawn_with_env(
        &bin,
        [
            ("VANCE_DECODE_THREADS", threads.as_str()),
            // VideoToolbox leaves threading at 1: no stream decoder of its own.
            ("VANCE_FORCE_SW", "1"),
        ],
    )
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

/// With 1 and with 8 decode threads: the stream from 0 holds
/// `expected_frames` frames, each byte-identical to `decode_at` its pts;
/// a stream started at 60 % of the file gives the same frames from there to
/// the end; and both thread counts give the same frames.
fn assert_decodes_alike_from_anywhere(name: &str, path: &Path, expected_frames: usize) {
    let path = path.to_string_lossy().into_owned();
    let mut runs: Vec<Vec<FrameData>> = Vec::new();
    for threads in [1, 8] {
        let what = format!("{name}, {threads} decode thread(s)");
        let mut client = spawn(threads);
        client
            .open(path.clone())
            .unwrap_or_else(|e| panic!("{what}: open: {e}"));

        client.start_stream(0, 4).expect("start_stream at 0");
        let frames = drain(&format!("{what}: stream from 0"), &mut client);
        assert_eq!(frames.len(), expected_frames, "{what}: frames from 0");
        for (i, frame) in frames.iter().enumerate() {
            let at = format!("{what}: decode_at frame {i}");
            let got = client
                .decode_at(frame.pts)
                .unwrap_or_else(|e| panic!("{at}: {e}"));
            assert_same(&at, &got, frame);
        }

        let mid = frames.len() * 6 / 10;
        client
            .start_stream(frames[mid].pts, 4)
            .expect("start_stream mid-file");
        let tail = drain(&format!("{what}: stream from frame {mid}"), &mut client);
        let firsts: Vec<i64> = tail.iter().take(3).map(|f| f.pts).collect();
        assert_eq!(
            tail.len(),
            frames.len() - mid,
            "{what}: frames from frame {mid} (pts {}); first pts {firsts:?}",
            frames[mid].pts
        );
        for (i, (got, want)) in tail.iter().zip(&frames[mid..]).enumerate() {
            assert_same(
                &format!("{what}: stream from frame {mid}, frame {}", mid + i),
                got,
                want,
            );
        }
        client.shutdown().expect("shutdown");
        runs.push(frames);
    }
    for (i, (one, many)) in runs[0].iter().zip(&runs[1]).enumerate() {
        assert_same(&format!("{name}: frame {i}, 8 vs 1 thread"), many, one);
    }
}

#[test]
fn h264_ts_with_parameter_sets_once_decodes_from_anywhere() {
    let Some(fx) = make_ts_fixture("h264-intra", Codec::H264, "libx264", 1) else {
        return;
    };
    assert_decodes_alike_from_anywhere("h264 ts", &fx.0, 50);
}

#[test]
fn hevc_ts_with_parameter_sets_once_decodes_from_anywhere() {
    let encoder = if has_encoder("libx265") {
        "libx265"
    } else {
        "hevc_videotoolbox"
    };
    let Some(fx) = make_ts_fixture("hevc-intra", Codec::Hevc, encoder, 1) else {
        return;
    };
    assert_decodes_alike_from_anywhere("hevc ts", &fx.0, 50);
}

/// Regression for MPEG-TS seeks that stopped past the GOP's keyframe: a
/// mid-GOP target decoded from a non-keyframe, which gave H.264's next GOP
/// and HEVC frames without their references.
#[test]
fn gop_h264_ts_decodes_from_anywhere() {
    let Some(fx) = make_ts_fixture("h264-gop", Codec::H264, "libx264", 12) else {
        return;
    };
    assert_decodes_alike_from_anywhere("h264 ts, 12-frame GOPs", &fx.0, 50);
}

/// See `gop_h264_ts_decodes_from_anywhere`.
#[test]
fn gop_hevc_ts_decodes_from_anywhere() {
    let Some(fx) = make_ts_fixture("hevc-gop", Codec::Hevc, "libx265", 12) else {
        return;
    };
    assert_decodes_alike_from_anywhere("hevc ts, 12-frame GOPs", &fx.0, 50);
}

/// Frames of a 50-frame fixture with 12-frame GOPs (keyframes 0, 12, 24, 36
/// and 48) to `decode_at`, in this order: the first, mid-GOP, both sides of
/// a GOP boundary, the last, then jumps back and forth across GOPs.
const SEEK_ORDER: [usize; 16] = [0, 5, 11, 12, 13, 49, 47, 30, 24, 23, 6, 0, 36, 35, 48, 1];

/// Frames to start streams at in the same fixture: both sides of a GOP
/// boundary, mid-GOP, and the last two.
const STREAM_STARTS: [usize; 5] = [11, 12, 18, 48, 49];

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

/// With 1 and with 8 decode threads: every frame of a stream from 0 is the
/// ffmpeg CLI's frame; `decode_at` each frame of [`SEEK_ORDER`] gives that
/// frame, and a stream started at each frame of [`STREAM_STARTS`] gives it
/// and every frame after; both thread counts give the same timestamps.
fn assert_seeks_match_a_decode_from_the_start(name: &str, path: &Path) {
    let cli = ffmpeg_cli_frames(path);
    assert_eq!(cli.len(), 50, "{name}: frames the ffmpeg CLI decodes");
    let path_str = path.to_string_lossy().into_owned();
    let mut pts_by_run: Vec<Vec<i64>> = Vec::new();
    for threads in [1, 8] {
        let what = format!("{name}, {threads} decode thread(s)");
        let mut client = spawn(threads);
        client
            .open(path_str.clone())
            .unwrap_or_else(|e| panic!("{what}: open: {e}"));

        client.start_stream(0, 4).expect("start_stream at 0");
        let frames = drain(&format!("{what}: stream from 0"), &mut client);
        assert_eq!(frames.len(), cli.len(), "{what}: frames from 0");
        for (i, (frame, want)) in frames.iter().zip(&cli).enumerate() {
            assert_cli_pixels(&format!("{what}: stream from 0, frame {i}"), frame, want);
        }

        for k in SEEK_ORDER {
            let at = format!("{what}: decode_at frame {k}");
            let got = client
                .decode_at(frames[k].pts)
                .unwrap_or_else(|e| panic!("{at}: {e}"));
            assert_same(&at, &got, &frames[k]);
        }

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
        pts_by_run.push(frames.iter().map(|f| f.pts).collect());
    }
    assert_eq!(pts_by_run[0], pts_by_run[1], "{name}: pts, 1 vs 8 threads");
}

#[test]
fn h264_ts_seeks_land_on_the_target_across_gops() {
    let Some(fx) = encode_fixture("h264-seeks.ts", "libx264", 12, "mpegts") else {
        return;
    };
    assert_seeks_match_a_decode_from_the_start("h264 ts", &fx.0);
}

#[test]
fn hevc_ts_seeks_land_on_the_target_across_gops() {
    let Some(fx) = encode_fixture("hevc-seeks.ts", "libx265", 12, "mpegts") else {
        return;
    };
    assert_seeks_match_a_decode_from_the_start("hevc ts", &fx.0);
}

/// The indexed containers, which seek straight to the keyframe, through the
/// same checks.
#[test]
fn h264_mp4_and_mkv_seeks_land_on_the_target_across_gops() {
    for (muxer, ext) in [("mp4", "mp4"), ("matroska", "mkv")] {
        let Some(fx) = encode_fixture(&format!("h264-seeks.{ext}"), "libx264", 12, muxer) else {
            return;
        };
        assert_seeks_match_a_decode_from_the_start(&format!("h264 {ext}"), &fx.0);
    }
}

/// One keyframe for the whole file (an endless GOP, as some recorders
/// write): every seek has to step back to the start, for a target 70 s in
/// through the whole 1 s, 2 s ... 64 s ladder.
#[test]
fn h264_ts_with_a_single_keyframe_seeks_back_to_it() {
    let Some(fx) = encode_source(
        "h264-one-gop.ts",
        "testsrc2=s=320x240:r=5:d=70",
        "libx264",
        1000,
        "mpegts",
    ) else {
        return;
    };
    let cli = ffmpeg_cli_frames(&fx.0);
    assert_eq!(cli.len(), 350, "frames the ffmpeg CLI decodes");
    for threads in [1, 8] {
        let what = format!("h264 ts, one keyframe, {threads} decode thread(s)");
        let mut client = spawn(threads);
        client
            .open(fx.0.to_string_lossy().into_owned())
            .unwrap_or_else(|e| panic!("{what}: open: {e}"));
        client.start_stream(0, 4).expect("start_stream at 0");
        let frames = drain(&format!("{what}: stream from 0"), &mut client);
        assert_eq!(frames.len(), cli.len(), "{what}: frames from 0");
        for (i, (frame, want)) in frames.iter().zip(&cli).enumerate() {
            assert_cli_pixels(&format!("{what}: stream from 0, frame {i}"), frame, want);
        }
        for k in [349, 60, 1, 200] {
            let at = format!("{what}: decode_at frame {k}");
            let got = client
                .decode_at(frames[k].pts)
                .unwrap_or_else(|e| panic!("{at}: {e}"));
            assert_same(&at, &got, &frames[k]);
        }
        client
            .start_stream(frames[300].pts, 4)
            .expect("start_stream");
        let tail = drain(&format!("{what}: stream from frame 300"), &mut client);
        assert_eq!(tail.len(), 50, "{what}: frames from frame 300");
        for (i, (got, want)) in tail.iter().zip(&frames[300..]).enumerate() {
            assert_same(
                &format!("{what}: stream from frame 300, frame {}", 300 + i),
                got,
                want,
            );
        }
        client.shutdown().expect("shutdown");
    }
}

/// `open` refuses the bare stream at `path` with `NeedsContainer`, naming
/// FFmpeg's `demuxer`.
fn assert_needs_container(what: &str, client: &mut DecoderClient, path: &Path, demuxer: &str) {
    match client.open(path.to_string_lossy().into_owned()) {
        Err(ClientError::Open(OpenError::NeedsContainer { format })) => {
            assert_eq!(format, demuxer, "{what}: demuxer");
        }
        other => panic!("{what}: open gave {other:?}, not NeedsContainer"),
    }
}

/// A bare stream (`demuxer`'s format) is rejected at `open`, with 1 and with
/// 8 decode threads, and the same bitstream in MP4 and in MPEG-TS then
/// opens in that sidecar and decodes the frames the ffmpeg CLI decodes from
/// the bare stream.
fn assert_only_the_bare_stream_needs_a_container(name: &str, encoder: &str, demuxer: &str) {
    let Some(mp4) = encode_fixture(&format!("{name}.mp4"), encoder, 12, "mp4") else {
        return;
    };
    let bare = remux(&mp4.0, &format!("{name}.{demuxer}"), demuxer);
    let ts = remux(&mp4.0, &format!("{name}.ts"), "mpegts");
    let cli = ffmpeg_cli_frames(&bare.0);
    assert_eq!(cli.len(), 50, "{name}: frames the ffmpeg CLI decodes");
    for threads in [1, 8] {
        let what = format!("{name}, {threads} decode thread(s)");
        let mut client = spawn(threads);
        assert_needs_container(&what, &mut client, &bare.0, demuxer);
        for (container, fx) in [("mp4", &mp4), ("ts", &ts)] {
            let what = format!("{what}, in {container}");
            client
                .open(fx.0.to_string_lossy().into_owned())
                .unwrap_or_else(|e| panic!("{what}: open: {e}"));
            client.start_stream(0, 4).expect("start_stream at 0");
            let frames = drain(&what, &mut client);
            assert_eq!(frames.len(), cli.len(), "{what}: frames");
            for (i, (frame, want)) in frames.iter().zip(&cli).enumerate() {
                assert_cli_pixels(&format!("{what}: frame {i}"), frame, want);
            }
            let got = client
                .decode_at(frames[30].pts)
                .unwrap_or_else(|e| panic!("{what}: decode_at frame 30: {e}"));
            assert_same(&format!("{what}: decode_at frame 30"), &got, &frames[30]);
        }
        client.shutdown().expect("shutdown");
    }
}

#[test]
fn bare_h264_needs_a_container() {
    assert_only_the_bare_stream_needs_a_container("bare-h264", "libx264", "h264");
}

#[test]
fn bare_hevc_needs_a_container() {
    assert_only_the_bare_stream_needs_a_container("bare-hevc", "libx265", "hevc");
}

/// The rule's edges: a raw stream is rejected for its missing pts, not for
/// being raw. Raw MPEG-2 video has only dts made up from the frame rate,
/// so it is rejected too; raw MPEG-4 Part 2 gets a pts per frame from its
/// parser, so it still opens and seeks.
#[test]
fn raw_streams_need_a_container_only_without_pts() {
    if let Some(m2v) = encode_fixture("raw.m2v", "mpeg2video", 12, "mpeg2video") {
        assert_needs_container("raw mpeg-2", &mut spawn(1), &m2v.0, "mpegvideo");
    }
    let Some(m4v) = encode_fixture("raw.m4v", "mpeg4", 12, "m4v") else {
        return;
    };
    let mut client = spawn(1);
    client
        .open(m4v.0.to_string_lossy().into_owned())
        .expect("open raw mpeg-4");
    client.start_stream(0, 4).expect("start_stream at 0");
    let frames = drain("raw mpeg-4", &mut client);
    assert_eq!(frames.len(), 50, "raw mpeg-4: frames");
    for k in [30, 5, 49] {
        let got = client.decode_at(frames[k].pts).expect("decode_at");
        assert_same(
            &format!("raw mpeg-4: decode_at frame {k}"),
            &got,
            &frames[k],
        );
    }
    client.shutdown().expect("shutdown");
}
