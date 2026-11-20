//! Frame-threaded decode (ADR 0006 step 1): single- vs multi-threaded decode
//! must produce identical frames.
//!
//! Every case runs two sidecars side by side, one with
//! `VANCE_DECODE_THREADS=1` and one with `=8`, and requires the same answers
//! from both: the whole stream, `decode_at` at the start / mid-GOP / between
//! frames / the last frame / past the end and backwards, a stream started
//! mid-file, a stream stopped mid-way and a stream restarted after end of
//! stream. Both run with `VANCE_FORCE_SW=1`: VideoToolbox leaves threading
//! at 1, so only the software path has anything to compare.
//!
//! Fixtures other than the committed H.264 one are generated with the
//! `ffmpeg` CLI at test time; a case is skipped when ffmpeg or the encoder
//! it needs isn't available.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use vance_decoder::{DecoderClient, FrameData};

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

/// `ffmpeg -f lavfi -i <lavfi> <out_args> <unique temp file>`, or `None`
/// (skip) when ffmpeg can't encode with `encoder`.
fn make_fixture(name: &str, encoder: &str, lavfi: &str, out_args: &[&str]) -> Option<Fixture> {
    if !has_encoder(encoder) {
        eprintln!("{name}: ffmpeg with the {encoder} encoder not available, skipping");
        return None;
    }
    let fixture = Fixture(
        std::env::temp_dir().join(format!("vance-threading-{}-{name}", std::process::id())),
    );
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", lavfi])
        .args(out_args)
        .arg(&fixture.0)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg failed to generate {name}");
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

/// The next streamed frame from both clients, which must agree on whether
/// the stream has ended.
fn next_pair(
    what: &str,
    one: &mut DecoderClient,
    many: &mut DecoderClient,
) -> Option<(FrameData, FrameData)> {
    let a = one
        .next_frame()
        .unwrap_or_else(|e| panic!("{what}: 1 thread: {e}"));
    let b = many
        .next_frame()
        .unwrap_or_else(|e| panic!("{what}: 8 threads: {e}"));
    match (a, b) {
        (Some(a), Some(b)) => Some((a, b)),
        (None, None) => None,
        (a, b) => panic!(
            "{what}: 1 thread gave pts {:?}, 8 threads gave pts {:?} (None = end of stream)",
            a.map(|f| f.pts),
            b.map(|f| f.pts)
        ),
    }
}

/// `decode_at(expected.pts)` must return `expected` from both clients.
fn assert_decode_at(
    what: &str,
    one: &mut DecoderClient,
    many: &mut DecoderClient,
    expected: &FrameData,
) {
    let a = one
        .decode_at(expected.pts)
        .unwrap_or_else(|e| panic!("{what}: 1 thread: {e}"));
    let b = many
        .decode_at(expected.pts)
        .unwrap_or_else(|e| panic!("{what}: 8 threads: {e}"));
    assert_same(what, &a, &b);
    assert_same(what, &a, expected);
}

fn start_both(one: &mut DecoderClient, many: &mut DecoderClient, from_pts: i64) {
    one.start_stream(from_pts, 4)
        .expect("start_stream, 1 thread");
    many.start_stream(from_pts, 4)
        .expect("start_stream, 8 threads");
}

/// Runs every comparison on `path`, which holds `expected_frames` frames.
fn assert_threading_is_invisible(name: &str, path: &Path, expected_frames: usize) {
    let path = path.to_string_lossy().into_owned();
    let mut one = spawn(1);
    let mut many = spawn(8);
    let info = one.open(path.clone()).expect("open, 1 thread");
    let info8 = many.open(path).expect("open, 8 threads");
    assert_eq!(
        format!("{info:?}"),
        format!("{info8:?}"),
        "{name}: stream info"
    );

    // (a) The whole stream: same frames, strictly increasing pts, and both
    // reach a clean end of stream. Owned copies are kept for the checks below.
    start_both(&mut one, &mut many, 0);
    let mut frames: Vec<FrameData> = Vec::new();
    while let Some((a, b)) = next_pair(&format!("{name} stream"), &mut one, &mut many) {
        let what = format!("{name} stream frame {}", frames.len());
        assert_same(&what, &a, &b);
        assert_eq!(
            (a.width, a.height, a.pixel_format),
            (info.width, info.height, info.pixel_format),
            "{what}: frame vs stream info"
        );
        if let Some(prev) = frames.last() {
            assert!(a.pts > prev.pts, "{what}: pts {} after {}", a.pts, prev.pts);
        }
        frames.push(a.clone());
        assert!(frames.len() <= expected_frames, "{name}: too many frames");
    }
    assert_eq!(
        frames.len(),
        expected_frames,
        "{name}: streamed frame count"
    );

    // (e) A stream restarted after end of stream starts over.
    start_both(&mut one, &mut many, 0);
    for (i, frame) in frames.iter().take(3).enumerate() {
        let what = format!("{name} restarted stream frame {i}");
        let (a, b) = next_pair(&what, &mut one, &mut many).expect("frame after restart");
        assert_same(&what, &a, &b);
        assert_same(&what, &a, frame);
    }
    one.stop_stream().expect("stop_stream, 1 thread");
    many.stop_stream().expect("stop_stream, 8 threads");

    // (b) One-shot decode_at: the first frame at or after the target, the
    // same from both and the same as the streamed one; past the end, the
    // same error. `mid` is past the middle so it isn't a fixture's keyframe.
    let mid = frames.len() / 2 + 3;
    let last = frames.len() - 1;
    let cases = [
        ("start", 0, Some(0)),
        ("mid-GOP", frames[mid].pts, Some(mid)),
        ("between frames", frames[mid].pts + 1, Some(mid + 1)),
        ("last frame", frames[last].pts, Some(last)),
        ("past the end", frames[last].pts + 1, None),
    ];
    for (label, target, expected) in cases {
        let what = format!("{name} decode_at {label} (pts {target})");
        match (one.decode_at(target), many.decode_at(target), expected) {
            (Ok(a), Ok(b), Some(i)) => {
                assert_same(&what, &a, &b);
                assert_same(&what, &a, &frames[i]);
            }
            (Err(a), Err(b), None) => assert_eq!(a.to_string(), b.to_string(), "{what}"),
            (a, b, _) => panic!(
                "{what}: expected frame {expected:?}; 1 thread: {:?}, 8 threads: {:?}",
                a.map(|f| f.pts),
                b.map(|f| f.pts)
            ),
        }
    }
    // Backward seeks, from every frame of a GOP or more. FFmpeg 7.1's HEVC
    // decoder kept frames across the seek's flush, so frame 22 then frame 1
    // gave frame 23.
    for i in mid..frames.len().min(mid + 16) {
        let what = format!("{name} decode_at frame {i}");
        assert_decode_at(&what, &mut one, &mut many, &frames[i]);
        let what = format!("{name} decode_at frame 1 after frame {i}");
        assert_decode_at(&what, &mut one, &mut many, &frames[1]);
    }

    // (c) A stream started mid-file starts at the requested frame.
    start_both(&mut one, &mut many, frames[mid].pts);
    for (i, frame) in frames[mid..].iter().take(3).enumerate() {
        let what = format!("{name} stream from frame {mid}, frame {}", mid + i);
        let (a, b) = next_pair(&what, &mut one, &mut many).expect("frame of mid-file stream");
        assert_same(&what, &a, &b);
        assert_same(&what, &a, frame);
    }

    // (d) Stopped mid-stream, the file is still usable for decode_at, also
    // backwards (with one thread the stream ran on the one-shot decoder).
    one.stop_stream().expect("stop_stream, 1 thread");
    many.stop_stream().expect("stop_stream, 8 threads");
    let q = frames.len() / 4 + 1;
    let what = format!("{name} decode_at frame {q} after stop");
    assert_decode_at(&what, &mut one, &mut many, &frames[q]);

    one.shutdown().expect("shutdown, 1 thread");
    many.shutdown().expect("shutdown, 8 threads");
}

#[test]
fn h264_fixture_is_identical_threaded() {
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/testsrc_320x240_5s.mp4");
    assert!(fixture.exists(), "fixture missing at {}", fixture.display());
    // 5 s at 30 fps, a keyframe every 15 frames, no B-frames.
    assert_threading_is_invisible("h264", &fixture, 150);
}

#[test]
fn h264_with_b_frames_is_identical_threaded() {
    // Reordered output (B-frames) is where frame threading's output delay
    // and the end-of-stream drain matter most.
    let Some(fx) = make_fixture(
        "h264-bframes.mp4",
        "libx264",
        "testsrc2=s=320x240:r=25:d=2",
        &[
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-bf",
            "3",
            "-g",
            "12",
            "-sc_threshold",
            "0",
        ],
    ) else {
        return;
    };
    assert_threading_is_invisible("h264 b-frames", &fx.0, 50);
}

#[test]
fn prores_422_hq_10bit_is_identical_threaded() {
    // 4:2:2 10-bit: also exercises the 4:2:0 converter on both contexts.
    let Some(fx) = make_fixture(
        "prores.mov",
        "prores_ks",
        "testsrc2=s=640x360:r=25:d=1",
        &[
            "-c:v",
            "prores_ks",
            "-profile:v",
            "3",
            "-pix_fmt",
            "yuv422p10le",
        ],
    ) else {
        return;
    };
    assert_threading_is_invisible("prores", &fx.0, 25);
}

#[test]
fn ffv1_odd_size_is_identical_threaded() {
    // Odd size, 4 slices (slice threads), inter-frame state between
    // keyframes (frame threads) and 4:4:4 (converted to 4:2:0).
    let Some(fx) = make_fixture(
        "ffv1.mkv",
        "ffv1",
        "testsrc=s=161x91:r=25:d=2",
        &[
            "-c:v", "ffv1", "-level", "3", "-slices", "4", "-g", "10", "-pix_fmt", "yuv444p",
        ],
    ) else {
        return;
    };
    assert_threading_is_invisible("ffv1", &fx.0, 50);
}

#[test]
fn mjpeg_is_identical_threaded() {
    let Some(fx) = make_fixture(
        "mjpeg.avi",
        "mjpeg",
        "testsrc=s=161x91:r=25:d=1",
        &["-c:v", "mjpeg", "-pix_fmt", "yuvj422p", "-q:v", "3"],
    ) else {
        return;
    };
    assert_threading_is_invisible("mjpeg", &fx.0, 25);
}

#[test]
fn hevc_main10_is_identical_threaded() {
    let lavfi = "testsrc2=s=320x240:r=25:d=2";
    let fx = if has_encoder("libx265") {
        make_fixture(
            "hevc10.mp4",
            "libx265",
            lavfi,
            &[
                "-c:v",
                "libx265",
                "-pix_fmt",
                "yuv420p10le",
                "-x265-params",
                "log-level=error:keyint=12:min-keyint=12:scenecut=0:open-gop=0:bframes=3",
                "-tag:v",
                "hvc1",
            ],
        )
    } else {
        make_fixture(
            "hevc10.mp4",
            "hevc_videotoolbox",
            lavfi,
            &[
                "-c:v",
                "hevc_videotoolbox",
                "-profile:v",
                "main10",
                "-pix_fmt",
                "p010le",
                "-g",
                "12",
                "-tag:v",
                "hvc1",
            ],
        )
    };
    let Some(fx) = fx else {
        return;
    };
    assert_threading_is_invisible("hevc main10", &fx.0, 50);
}

/// The one `vance-decoder` child of this process.
fn decoder_pid() -> sysinfo::Pid {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let me = Pid::from_u32(std::process::id());
    let children: Vec<Pid> = sys
        .processes()
        .values()
        .filter(|p| p.parent() == Some(me) && p.name().to_string_lossy().contains("vance-decoder"))
        .map(|p| p.pid())
        .collect();
    assert_eq!(children.len(), 1, "run this benchmark alone");
    children[0]
}

/// What frame threading costs and buys at 4K, for 1 and 8 decode threads:
/// one-shot `decode_at` latency, streamed ms/frame, how long `stop_stream`
/// takes mid-stream, and the sidecar's peak RSS while streaming (sampled
/// once per frame). A manual benchmark, not a CI check; run it alone so the
/// only `vance-decoder` child is the one being measured:
///   cargo test -p vance-decoder --test it -- --ignored --nocapture threading::prores_4k
#[test]
#[ignore = "manual benchmark: run with --ignored --nocapture"]
fn prores_4k_threading_costs() {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};
    let Some(fx) = make_fixture(
        "prores-4k.mov",
        "prores_ks",
        "testsrc2=s=3840x2160:r=24:d=2",
        &[
            "-c:v",
            "prores_ks",
            "-profile:v",
            "3",
            "-pix_fmt",
            "yuv422p10le",
        ],
    ) else {
        return;
    };
    for threads in [1, 8] {
        let mut client = spawn(threads);
        let pid = decoder_pid();
        let info = client
            .open(fx.0.to_string_lossy().into_owned())
            .expect("open 4K ProRes");
        let (tb, fr) = (info.time_base, info.frame_rate);
        let frame_pts = |k: i64| k * tb.1 * fr.1 / (tb.0 * fr.0);

        let targets = [0, 13, 26, 39, 47];
        let start = Instant::now();
        for k in targets {
            let f = client.decode_at(frame_pts(k)).expect("decode_at");
            assert_eq!(f.pts, frame_pts(k));
        }
        let decode_at_ms = start.elapsed().as_secs_f64() * 1000.0 / targets.len() as f64;

        let mut sys = System::new();
        let mut peak_rss = 0u64;
        let start = Instant::now();
        client.start_stream(0, 8).expect("start_stream");
        let mut frames = 0u32;
        while client.next_frame().expect("next_frame").is_some() {
            frames += 1;
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[pid]),
                true,
                ProcessRefreshKind::new().with_memory(),
            );
            peak_rss = peak_rss.max(sys.process(pid).map_or(0, |p| p.memory()));
        }
        let stream_ms = start.elapsed().as_secs_f64() * 1000.0 / f64::from(frames);
        assert_eq!(frames, 48, "2 s at 24 fps");

        client.start_stream(0, 8).expect("start_stream");
        for _ in 0..10 {
            client.next_frame().expect("next_frame").expect("not eos");
        }
        let start = Instant::now();
        client.stop_stream().expect("stop_stream");
        let stop_ms = start.elapsed().as_secs_f64() * 1000.0;
        client.shutdown().expect("shutdown");

        eprintln!(
            "prores_4k_threading_costs: {threads} thread(s): decode_at {decode_at_ms:.1} ms, \
             stream {stream_ms:.1} ms/frame, stop_stream {stop_ms:.1} ms, \
             peak RSS while streaming {:.0} MB",
            peak_rss as f64 / (1024.0 * 1024.0)
        );
    }
}
