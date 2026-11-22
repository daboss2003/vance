//! Frame selection: Blocking == Realtime == ScrubProvider == `decode_at` PTS
//! (first frame with pts >= target; hold the last at EOF) — ADR 0007.
//!
//! Each provider walks the same request sequence: 48 consecutive 30 fps
//! targets mid-file, 48 more across the end of the file (the last half past
//! it), a backward jump, and a forward jump of more than a second. The
//! expected frame for each target comes from the file's full list of frame
//! PTS (a plain stream), and `decode_at` is checked against it too. Two
//! files: the 30 fps H.264 fixture, and a generated 23.976 fps H.264 file
//! with B-frames (a frame-rate mismatch: some source frames serve two
//! targets).
//!
//! Skipped when the decoder sidecar (or, for the generated file, ffmpeg) is
//! missing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vance_app_lib::compose::{FrameProvider, FrameRequest};
use vance_app_lib::decoder_pool::{source_seconds_to_pts, DecoderPool, MemoryBudget};
use vance_app_lib::layer_feed::{FeedPolicy, ScrubProvider, StreamProvider};
use vance_cache::FrameCache;
use vance_decoder::DecoderClient;
use vance_timeline::{ClipId, MediaItemId};

/// Timeline rate of the targets.
const FPS: f64 = 30.0;
/// Consecutive targets per run.
const RUN: usize = 48;

fn decoder_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(if cfg!(windows) {
            "vance-decoder.exe"
        } else {
            "vance-decoder"
        })
}

fn h264_fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4")
}

/// Missing prerequisite: silent skip locally, hard failure in CI when
/// `VANCE_REQUIRE_SIDECARS=1`.
fn missing_prereq(reason: &str) {
    if std::env::var_os("VANCE_REQUIRE_SIDECARS").is_some() {
        panic!("VANCE_REQUIRE_SIDECARS set but prerequisite missing: {reason}");
    }
    eprintln!("{reason} — skipping (set VANCE_REQUIRE_SIDECARS=1 to make this a failure)");
}

fn sidecar() -> Option<PathBuf> {
    let bin = decoder_bin();
    if bin.exists() {
        Some(bin)
    } else {
        missing_prereq("decoder sidecar missing");
        None
    }
}

/// Encode a lavfi test pattern; `None` (skipped) without ffmpeg.
fn encode(out: &Path, args: &[&str]) -> Option<PathBuf> {
    let ok = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .args(args)
        .arg(out)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        Some(out.to_path_buf())
    } else {
        missing_prereq("ffmpeg could not make a fixture");
        None
    }
}

/// 3 s of 23.976 fps H.264 with B-frames (frames reorder in decode).
fn fixture_23976(dir: &Path) -> Option<PathBuf> {
    encode(
        &dir.join("testsrc_23976_bframes.mp4"),
        &[
            "testsrc=duration=3:size=320x240:rate=24000/1001",
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-bf",
            "3",
            "-g",
            "24",
            "-pix_fmt",
            "yuv420p",
        ],
    )
}

/// A file's frames: every PTS in display order, from a plain stream.
struct Truth {
    path: String,
    time_base: (i64, i64),
    pts: Vec<i64>,
    duration_s: f64,
}

impl Truth {
    fn of(bin: &Path, path: &Path) -> Truth {
        let path = path.to_string_lossy().into_owned();
        let mut c = DecoderClient::spawn(bin).expect("spawn");
        let info = c.open(path.clone()).expect("open");
        c.start_stream(i64::MIN, 8).expect("stream");
        let mut pts = Vec::new();
        while let Some(f) = c.next_frame().expect("next frame") {
            pts.push(f.pts);
        }
        assert!(pts.len() > 30, "{path}: {} frames", pts.len());
        assert!(
            pts.windows(2).all(|w| w[0] < w[1]),
            "{path}: pts not increasing"
        );
        let (num, den) = info.time_base;
        let duration_s = (*pts.last().unwrap() as f64 * num as f64) / den as f64;
        Truth {
            path,
            time_base: info.time_base,
            pts,
            duration_s,
        }
    }

    fn target(&self, seconds: f64) -> i64 {
        source_seconds_to_pts(seconds, self.time_base)
    }

    /// The rule: the first frame at or after the target, else the last.
    fn expected(&self, seconds: f64) -> i64 {
        let t = self.target(seconds);
        let last = *self.pts.last().unwrap();
        self.pts.iter().copied().find(|&p| p >= t).unwrap_or(last)
    }

    /// The source seconds every provider walks (see the module docs).
    fn sequence(&self) -> Vec<f64> {
        let run = |start: f64, n: usize| (0..n).map(move |k| start + k as f64 / FPS);
        let mid = 0.5;
        // Ends RUN / 2 frames past the last frame.
        let across_end = self.duration_s - (RUN as f64 / 2.0 - 1.0) / FPS;
        run(mid, RUN)
            .chain(run(across_end, RUN))
            .chain(run(0.25, 12))
            .chain(run(0.25 + 11.0 / FPS + 1.5, 12))
            .collect()
    }
}

fn req(clip: ClipId, path: &str, seconds: f64) -> FrameRequest {
    FrameRequest {
        clip_id: clip,
        media: MediaItemId::new(),
        path: path.to_owned(),
        source_seconds: seconds,
    }
}

fn pool(bin: &Path) -> DecoderPool {
    DecoderPool::new(
        bin.to_path_buf(),
        6,
        MemoryBudget {
            cap_bytes: u64::MAX,
        },
    )
}

fn cache(dir: &Path) -> Arc<FrameCache> {
    Arc::new(FrameCache::new(dir.join("cache"), 64 << 20, 128 << 20).expect("frame cache"))
}

/// `decode_at` on a plain client agrees with the truth, and fails exactly
/// past the last frame (where the providers hold it).
fn check_decode_at(bin: &Path, truth: &Truth) {
    let mut c = DecoderClient::spawn(bin).expect("spawn");
    c.open(truth.path.clone()).expect("open");
    let last = *truth.pts.last().unwrap();
    for s in truth.sequence() {
        let t = truth.target(s);
        match c.decode_at(t) {
            Ok(f) => assert_eq!(f.pts, truth.expected(s), "decode_at({t}) at {s:.4}s"),
            Err(e) => assert!(t > last, "decode_at({t}) failed before the end: {e}"),
        }
    }
}

fn single_pts(prov: &mut dyn FrameProvider, r: &FrameRequest) -> Option<i64> {
    let mut frames = prov.frames(std::slice::from_ref(r));
    assert_eq!(frames.len(), 1);
    frames.pop().unwrap().map(|f| f.pts)
}

/// Walk the sequence on `prov` with one clip; every target must give the
/// expected frame.
fn check_provider(name: &str, prov: &mut dyn FrameProvider, truth: &Truth) {
    let clip = ClipId::new();
    for s in truth.sequence() {
        let got = single_pts(prov, &req(clip, &truth.path, s));
        assert_eq!(
            got,
            Some(truth.expected(s)),
            "{name} at {s:.4}s ({})",
            truth.path
        );
    }
}

/// `Realtime` may hold an older frame while the stream catches up, but it
/// must never skip past the expected one: poll each target until it lands.
fn check_realtime(prov: &mut StreamProvider, truth: &Truth) {
    let clip = ClipId::new();
    for s in truth.sequence() {
        let want = truth.expected(s);
        let r = req(clip, &truth.path, s);
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let got = single_pts(prov, &r);
            if got == Some(want) {
                break;
            }
            if let Some(p) = got {
                assert!(p < want, "Realtime skipped to {p} past {want} at {s:.4}s");
            }
            assert!(
                Instant::now() < deadline,
                "Realtime never reached {want} at {s:.4}s"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn check_all_providers(bin: &Path, truth: &Truth) {
    check_decode_at(bin, truth);
    let dir = tempfile::tempdir().unwrap();

    let mut scrub_pool = pool(bin);
    let cache = cache(dir.path());
    {
        let mut scrub = ScrubProvider {
            pool: &mut scrub_pool,
            cache: Arc::clone(&cache),
        };
        check_provider("Scrub", &mut scrub, truth);
        cache.reset_hit_stats();
        // Again, now served from the cache.
        check_provider("Scrub (cached)", &mut scrub, truth);
    }
    let stats = cache.stats();
    assert!(stats.lookups() > 0 && stats.misses == 0, "{stats:?}");
    assert!(scrub_pool.take_error().is_none());

    let mut blocking = StreamProvider::new(pool(bin), FeedPolicy::Blocking);
    check_provider("Blocking", &mut blocking, truth);
    let mut blocking_pool = blocking.into_pool();
    assert!(blocking_pool.take_error().is_none());

    let mut realtime = StreamProvider::new(blocking_pool, FeedPolicy::Realtime);
    check_realtime(&mut realtime, truth);
    assert!(realtime.into_pool().take_error().is_none());
}

#[test]
fn h264_30fps_providers_pick_the_decode_at_frame() {
    let Some(bin) = sidecar() else { return };
    let truth = Truth::of(&bin, &h264_fixture());
    check_all_providers(&bin, &truth);
}

#[test]
fn bframes_23976_against_30fps_targets_pick_the_decode_at_frame() {
    let Some(bin) = sidecar() else { return };
    let dir = tempfile::tempdir().unwrap();
    let Some(file) = fixture_23976(dir.path()) else {
        return;
    };
    let truth = Truth::of(&bin, &file);
    assert_eq!(truth.time_base.0, 1, "{:?}", truth.time_base);
    // Some source frames serve two consecutive 30 fps targets.
    let seq = truth.sequence();
    assert!(seq[..RUN]
        .windows(2)
        .any(|w| truth.expected(w[0]) == truth.expected(w[1])));
    check_all_providers(&bin, &truth);
}

/// Several layers in one call: two files (decoded on parallel threads by
/// the scrub provider) and a second clip of the same file, with a prepared
/// feed for a clip that starts later.
#[test]
fn several_layers_in_one_call_each_get_their_frame() {
    let Some(bin) = sidecar() else { return };
    let dir = tempfile::tempdir().unwrap();
    let Some(file) = fixture_23976(dir.path()) else {
        return;
    };
    let a = Truth::of(&bin, &h264_fixture());
    let b = Truth::of(&bin, &file);
    let (ca, cb, ca2, later) = (ClipId::new(), ClipId::new(), ClipId::new(), ClipId::new());
    let frame_reqs = |k: usize| {
        let t = k as f64 / FPS;
        vec![
            req(ca, &a.path, 1.0 + t),
            req(cb, &b.path, 0.2 + t),
            req(ca2, &a.path, 3.0 + t),
        ]
    };
    let want = |k: usize| -> Vec<Option<i64>> {
        let t = k as f64 / FPS;
        vec![
            Some(a.expected(1.0 + t)),
            Some(b.expected(0.2 + t)),
            Some(a.expected(3.0 + t)),
        ]
    };
    let pts = |frames: Vec<Option<vance_decoder::FrameData>>| -> Vec<Option<i64>> {
        frames.into_iter().map(|f| f.map(|f| f.pts)).collect()
    };

    let mut scrub_pool = pool(&bin);
    let cache = cache(dir.path());
    let mut scrub = ScrubProvider {
        pool: &mut scrub_pool,
        cache,
    };
    for k in 0..12 {
        assert_eq!(
            pts(scrub.frames(&frame_reqs(k))),
            want(k),
            "Scrub frame {k}"
        );
    }

    let mut stream = StreamProvider::new(scrub_pool, FeedPolicy::Blocking);
    for k in 0..12 {
        stream.prepare(&[req(later, &b.path, 1.5)]);
        assert_eq!(
            pts(stream.frames(&frame_reqs(k))),
            want(k),
            "Blocking frame {k}"
        );
    }
    // The prepared clip starts: its feed is already streaming from 1.5 s.
    let mut reqs = frame_reqs(12);
    reqs.push(req(later, &b.path, 1.5));
    let mut expected = want(12);
    expected.push(Some(b.expected(1.5)));
    assert_eq!(pts(stream.frames(&reqs)), expected);
    let mut back = stream.into_pool();
    assert!(back.take_error().is_none());
    // The feeds' decoders came back stopped and ready for one-shot decodes.
    let mut d = back.take(&a.path, 1).expect("a pooled decoder");
    let f = d
        .client
        .decode_at(a.target(2.0))
        .expect("decode after a stream");
    assert_eq!(f.pts, a.expected(2.0));
    drop(f);
    back.put(d);
}

#[test]
fn a_missing_file_gives_no_frame_and_reports_why() {
    let Some(bin) = sidecar() else { return };
    let dir = tempfile::tempdir().unwrap();
    let gone = dir
        .path()
        .join("offline.mp4")
        .to_string_lossy()
        .into_owned();
    let r = req(ClipId::new(), &gone, 0.0);

    let mut p = pool(&bin);
    let mut scrub = ScrubProvider {
        pool: &mut p,
        cache: cache(dir.path()),
    };
    assert!(scrub.frames(std::slice::from_ref(&r))[0].is_none());
    assert!(p.take_error().is_some());

    let mut stream = StreamProvider::new(p, FeedPolicy::Blocking);
    assert!(stream.frames(std::slice::from_ref(&r))[0].is_none());
    assert!(stream.pool_mut().take_error().is_some());
    // A feed that never started is retried (no sidecar spawns for a file
    // that isn't there), so it reports again and recovers once the file is
    // back instead of leaving its layer out for good.
    assert!(stream.frames(std::slice::from_ref(&r))[0].is_none());
    assert!(stream.pool_mut().take_error().is_some());
    std::fs::copy(h264_fixture(), &gone).expect("restore the file");
    assert!(stream.frames(std::slice::from_ref(&r))[0].is_some());
    assert!(stream.pool_mut().take_error().is_none());
}

/// Decode cost of four 1080p layers per frame: the scrub provider's
/// parallel fan-out against `decode_at` on four decoders one after another,
/// and the Blocking stream provider. Prints ms/frame.
///
/// `cargo test -p vance-app --test it -- --ignored --nocapture frame_select::four_layer_decode_cost`
#[test]
#[ignore = "timing; run by hand"]
fn four_layer_decode_cost() {
    let Some(bin) = sidecar() else { return };
    let dir = tempfile::tempdir().unwrap();
    let Some(src) = encode(
        &dir.path().join("hd.mp4"),
        &[
            "testsrc2=duration=4:size=1920x1080:rate=30",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-g",
            "30",
            "-pix_fmt",
            "yuv420p",
        ],
    ) else {
        return;
    };
    let paths: Vec<String> = (0..4)
        .map(|i| {
            let p = dir.path().join(format!("layer{i}.mp4"));
            std::fs::copy(&src, &p).unwrap();
            p.to_string_lossy().into_owned()
        })
        .collect();
    let clips: Vec<ClipId> = (0..4).map(|_| ClipId::new()).collect();
    let tb = Truth::of(&bin, Path::new(&paths[0])).time_base;
    // Scattered targets: every scrub frame is a seek + decode.
    let targets: Vec<f64> = (0..24).map(|k| ((k * 37) % 100) as f64 / 30.0).collect();
    let layer_reqs = |s: f64| -> Vec<FrameRequest> {
        paths
            .iter()
            .zip(&clips)
            .enumerate()
            .map(|(i, (p, &c))| req(c, p, s + i as f64 * 0.1))
            .collect()
    };

    let mut seq: Vec<DecoderClient> = paths
        .iter()
        .map(|p| {
            let mut c = DecoderClient::spawn(&bin).unwrap();
            c.open(p.clone()).unwrap();
            c
        })
        .collect();
    let start = Instant::now();
    for &s in &targets {
        for (i, c) in seq.iter_mut().enumerate() {
            let t = source_seconds_to_pts(s + i as f64 * 0.1, tb);
            c.decode_at(t).unwrap();
        }
    }
    let sequential = start.elapsed().as_secs_f64() * 1000.0 / targets.len() as f64;

    let mut p = pool(&bin);
    // A tiny cache: every target misses.
    let cache = Arc::new(FrameCache::new(dir.path().join("c"), 1, 1).unwrap());
    let mut scrub = ScrubProvider {
        pool: &mut p,
        cache,
    };
    scrub.frames(&layer_reqs(0.0)); // spawn + open outside the timing
    let start = Instant::now();
    for &s in &targets {
        assert!(scrub.frames(&layer_reqs(s)).iter().all(Option::is_some));
    }
    let parallel = start.elapsed().as_secs_f64() * 1000.0 / targets.len() as f64;

    let mut stream = StreamProvider::new(p, FeedPolicy::Blocking);
    stream.frames(&layer_reqs(0.0));
    let n = 60;
    let start = Instant::now();
    for k in 1..=n {
        let frames = stream.frames(&layer_reqs(k as f64 / 30.0));
        assert!(frames.iter().all(Option::is_some));
    }
    let streaming = start.elapsed().as_secs_f64() * 1000.0 / n as f64;
    drop(stream.into_pool());

    eprintln!(
        "4 x 1080p per frame: scrub sequential decode_at {sequential:.1} ms, \
         ScrubProvider (parallel) {parallel:.1} ms, StreamProvider Blocking {streaming:.1} ms"
    );
}
