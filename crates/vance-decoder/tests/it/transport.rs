//! Frame transport (ADR 0006): shared-memory frames vs inline blobs.
//!
//! The same media through a shared-memory client (the default) and an
//! inline one (`VANCE_IPC_SHM=0`) must give byte-identical frames, streamed
//! and one-shot. Also: the lease budget spills instead of stalling the
//! sidecar, stopped streams hand their slots back, regions only grow, the
//! test-only slot-size cap sends bigger frames inline, no region outlives
//! its client, and a `ClientOptions::threads` budget changes no frame.
//!
//! Fixtures other than the committed ones are generated with the `ffmpeg`
//! CLI at test time; a case is skipped when ffmpeg or the encoder it needs
//! isn't available.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use vance_decoder::{
    ClientOptions, DecoderClient, FrameData, PixelFormat, StreamInfo, ThreadBudget, TransportStats,
};
use vance_ipc::shm::{live_region_count, SLOT_ALIGN};

/// 320x240 8-bit H.264, 150 frames: 115,200-byte frames (two 64 KiB units).
const H264: &str = "testsrc_320x240_5s.mp4";
/// 320x240 10-bit HEVC, 48 frames: 230,400-byte frames (four units).
const HEVC_10BIT: &str = "hdr10_320x240_2s.mp4";

const INLINE: (&str, &str) = ("VANCE_IPC_SHM", "0");

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

fn committed(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// A generated fixture file, deleted on drop.
struct Fixture(PathBuf);

impl Fixture {
    fn path(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

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

/// `ffmpeg -f lavfi -i <lavfi> <out_args> <unique temp file>.<ext>`, or
/// `None` (skip) when ffmpeg can't encode with `encoder`.
fn make_fixture(ext: &str, encoder: &str, lavfi: &str, out_args: &[&str]) -> Option<Fixture> {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    if !has_encoder(encoder) {
        eprintln!("ffmpeg with the {encoder} encoder not available, skipping");
        return None;
    }
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let fixture = Fixture(
        std::env::temp_dir().join(format!("vance-transport-{}-{n}.{ext}", std::process::id())),
    );
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "lavfi", "-i", lavfi])
        .args(out_args)
        .arg(&fixture.0)
        .status()
        .expect("run ffmpeg");
    assert!(status.success(), "ffmpeg failed to generate {ext} fixture");
    Some(fixture)
}

fn spawn(env: &[(&str, &str)]) -> DecoderClient {
    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing at {}", bin.display());
    DecoderClient::spawn_with_env(&bin, env.iter().copied()).expect("spawn decoder")
}

/// A client whose frames all arrive inline, as the reference.
fn spawn_inline(env: &[(&str, &str)]) -> DecoderClient {
    let mut env = env.to_vec();
    env.push(INLINE);
    spawn(&env)
}

fn frame_bytes(info: &StreamInfo) -> u64 {
    info.pixel_format.frame_size(info.width, info.height) as u64
}

/// The slot size the client picks for `info`'s frames.
fn slot_size(info: &StreamInfo) -> u64 {
    frame_bytes(info).next_multiple_of(SLOT_ALIGN)
}

/// Slots per region with the default options.
fn region_slots() -> u64 {
    let o = ClientOptions::default();
    u64::from(o.ring_slots) + o.max_leases as u64 + 1
}

/// Same pts, geometry, format, strides and pixels, at the full frame size.
fn assert_same(what: &str, got: &FrameData, want: &FrameData) {
    assert_eq!(
        (
            got.pts,
            got.width,
            got.height,
            got.pixel_format,
            &got.strides
        ),
        (
            want.pts,
            want.width,
            want.height,
            want.pixel_format,
            &want.strides
        ),
        "{what}: frame description"
    );
    assert_eq!(
        got.bytes.len(),
        got.pixel_format.frame_size(got.width, got.height),
        "{what}: payload size"
    );
    assert_eq!(got.bytes.len(), want.bytes.len(), "{what}: payload size");
    if let Some(i) = got
        .bytes
        .iter()
        .zip(want.bytes.iter())
        .position(|(x, y)| x != y)
    {
        panic!("{what} (pts {}): pixels differ from byte {i}", got.pts);
    }
}

/// The next streamed frame from both clients, which must agree on whether
/// the stream has ended.
fn next_pair(
    what: &str,
    shm: &mut DecoderClient,
    inline: &mut DecoderClient,
) -> Option<(FrameData, FrameData)> {
    let got = shm
        .next_frame()
        .unwrap_or_else(|e| panic!("{what}: shm client: {e}"));
    let want = inline
        .next_frame()
        .unwrap_or_else(|e| panic!("{what}: inline client: {e}"));
    match (got, want) {
        (Some(got), Some(want)) => Some((got, want)),
        (None, None) => None,
        (got, want) => panic!(
            "{what}: shm gave pts {:?}, inline gave pts {:?} (None = end of stream)",
            got.map(|f| f.pts),
            want.map(|f| f.pts)
        ),
    }
}

/// Streams `path` through both transports in lockstep (one lease out at a
/// time), then decodes the first, a mid, a between-frames and the last pts
/// one-shot. Every frame must match, and every shm frame must be a lease.
fn assert_transports_agree(path: &str, env: &[(&str, &str)], expected: PixelFormat) {
    let mut shm = spawn(env);
    let mut inline = spawn_inline(env);
    let info = shm.open(path).expect("open, shm client");
    let info_inline = inline.open(path).expect("open, inline client");
    assert_eq!(
        format!("{info:?}"),
        format!("{info_inline:?}"),
        "{path}: stream info"
    );
    assert_eq!(info.pixel_format, expected, "{path}: pixel format");

    shm.start_stream(0, 4).expect("start_stream, shm client");
    inline
        .start_stream(0, 4)
        .expect("start_stream, inline client");
    let mut pts = Vec::new();
    while let Some((got, want)) = next_pair(path, &mut shm, &mut inline) {
        let what = format!("{path} stream frame {}", pts.len());
        assert!(got.bytes.is_shared(), "{what}: not a lease");
        assert!(!want.bytes.is_shared(), "{what}: inline client leased");
        assert_same(&what, &got, &want);
        pts.push(got.pts);
    }
    assert!(pts.len() >= 2, "{path}: only {} frames", pts.len());

    let mid = pts[pts.len() / 2];
    let targets = [pts[0], mid, mid + 1, pts[pts.len() - 1]];
    for target in targets {
        let what = format!("{path} decode_at({target})");
        let got = shm
            .decode_at(target)
            .unwrap_or_else(|e| panic!("{what}: shm client: {e}"));
        let want = inline
            .decode_at(target)
            .unwrap_or_else(|e| panic!("{what}: inline client: {e}"));
        assert!(got.bytes.is_shared(), "{what}: not a lease");
        assert_same(&what, &got, &want);
    }

    let frames = (pts.len() + targets.len()) as u64;
    let stats = shm.transport_stats();
    assert!(stats.shared > 0 && stats.inline == 0, "{path}: {stats:?}");
    assert_eq!(
        (stats.shared, stats.spilled),
        (frames, 0),
        "{path}: {stats:?}"
    );
    assert!(stats.shm_epoch.is_some(), "{path}: {stats:?}");
    assert_eq!(
        stats.shm_bytes,
        region_slots() * slot_size(&info),
        "{path}: {stats:?}"
    );
    assert_eq!(
        inline.transport_stats(),
        TransportStats {
            inline: frames,
            ..TransportStats::default()
        },
        "{path}: inline client"
    );
    shm.shutdown().expect("shutdown, shm client");
    inline.shutdown().expect("shutdown, inline client");
}

/// On macOS the default spawn decodes H.264 with VideoToolbox, whose frames
/// are NV12; elsewhere this is the software YUV420P path.
#[test]
fn h264_frames_match_inline() {
    let expected = if cfg!(target_os = "macos") {
        PixelFormat::Nv12
    } else {
        PixelFormat::Yuv420P
    };
    assert_transports_agree(&committed(H264), &[], expected);
}

#[test]
fn h264_software_frames_match_inline() {
    assert_transports_agree(
        &committed(H264),
        &[("VANCE_FORCE_SW", "1")],
        PixelFormat::Yuv420P,
    );
}

#[test]
fn hevc_10bit_frames_match_inline() {
    assert_transports_agree(&committed(HEVC_10BIT), &[], PixelFormat::Yuv420P10Le);
}

#[test]
fn prores_10bit_frames_match_inline() {
    let Some(fx) = make_fixture(
        "mov",
        "prores_ks",
        "testsrc2=s=320x180:r=25:d=0.4",
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
    assert_transports_agree(&fx.path(), &[], PixelFormat::Yuv420P10Le);
}

/// Odd size, converted 4:4:4 -> 4:2:0: chroma rows of ceil(w/2) samples.
#[test]
fn odd_size_ffv1_frames_match_inline() {
    let Some(fx) = make_fixture(
        "mkv",
        "ffv1",
        "testsrc2=s=161x91:r=25:d=0.4,format=yuv444p",
        &["-c:v", "ffv1", "-pix_fmt", "yuv444p"],
    ) else {
        return;
    };
    assert_transports_agree(&fx.path(), &[], PixelFormat::Yuv420P);
}

/// Every frame of a stream held at once, far past the lease budget: the
/// first `max_leases` are leases, the rest are copied out, the sidecar keeps
/// delivering (no stall waiting for slots) and nothing differs from inline,
/// including the leases re-read after the whole stream has gone past them.
#[test]
fn holding_past_the_lease_budget_spills_without_stalling() {
    let max = ClientOptions::default().max_leases;
    let path = committed(H264);
    let mut shm = spawn(&[]);
    let mut inline = spawn_inline(&[]);
    shm.open(path.as_str()).expect("open, shm client");
    inline.open(path.as_str()).expect("open, inline client");
    shm.start_stream(0, 4).expect("start_stream, shm client");
    inline
        .start_stream(0, 4)
        .expect("start_stream, inline client");

    let mut held = Vec::new();
    let mut reference = Vec::new();
    while let Some((got, want)) = next_pair("held stream", &mut shm, &mut inline) {
        let what = format!("held stream frame {}", held.len());
        assert_eq!(got.bytes.is_shared(), held.len() < max, "{what}: leased?");
        assert_same(&what, &got, &want);
        held.push(got);
        if reference.len() < 3 * max {
            reference.push(want);
        }
    }
    assert!(held.len() >= 3 * max, "only {} frames", held.len());
    for (i, (got, want)) in held.iter().zip(&reference).enumerate() {
        assert_same(&format!("held frame {i}, re-read"), got, want);
    }
    let stats = shm.transport_stats();
    assert_eq!(
        (stats.shared, stats.spilled, stats.inline),
        (max as u64, (held.len() - max) as u64, 0),
        "{stats:?}"
    );

    // With the leases given back, frames are leases again.
    drop(held);
    let got = shm.decode_at(0).expect("decode_at after the held stream");
    let want = inline.decode_at(0).expect("decode_at, inline client");
    assert!(got.bytes.is_shared(), "{:?}", shm.transport_stats());
    assert_same("decode_at after the held stream", &got, &want);
}

/// Stopping a stream leaves decoded, unsent frames in the sidecar's ring;
/// their slots must go back to the pool, and so must frames still held
/// across the stop. After 100 cycles a whole stream still comes through
/// slots, never inline.
#[test]
fn stopped_streams_hand_their_slots_back() {
    let mut client = spawn(&[]);
    let info = client.open(committed(H264)).expect("open");
    let one_sec = info.time_base.1 / info.time_base.0;
    for cycle in 0..100 {
        let from = (cycle % 4) * one_sec;
        client.start_stream(from, 4).expect("start_stream");
        let first = client
            .next_frame()
            .expect("next_frame")
            .expect("not at end of stream");
        assert!(
            first.bytes.is_shared(),
            "cycle {cycle}: {:?}",
            client.transport_stats()
        );
        // Every other cycle holds its frame across the stop; its slot goes
        // back with a later request.
        let kept = (cycle % 2 == 0).then_some(first);
        client.stop_stream().expect("stop_stream");
        drop(kept);
    }

    client
        .start_stream(0, 4)
        .expect("start_stream after the cycles");
    let mut frames = 0;
    while let Some(f) = client.next_frame().expect("next_frame") {
        assert!(f.bytes.is_shared(), "frame {frames}: not a lease");
        frames += 1;
    }
    assert!(frames >= 100, "only {frames} frames");
    let stats = client.transport_stats();
    assert_eq!((stats.inline, stats.spilled), (0, 0), "{stats:?}");
    client.shutdown().expect("shutdown");
}

/// `VANCE_IPC_SHM_SLOT_BYTES` caps the slot size: frames that fit still go
/// through slots; bigger ones go inline (and match an inline client's), and
/// the capped region isn't regrown for them.
#[test]
fn a_slot_size_cap_sends_bigger_frames_inline() {
    let cap = 2 * SLOT_ALIGN;
    let cap_env = cap.to_string();
    let mut client = spawn(&[("VANCE_IPC_SHM_SLOT_BYTES", cap_env.as_str())]);

    let small = client.open(committed(H264)).expect("open H.264");
    assert!(frame_bytes(&small) <= cap);
    let epoch = client.transport_stats().shm_epoch.expect("region attached");
    assert_eq!(client.transport_stats().shm_bytes, region_slots() * cap);
    let f = client.decode_at(0).expect("decode_at, small frames");
    assert!(f.bytes.is_shared(), "{:?}", client.transport_stats());
    drop(f);

    let big = client
        .open(committed(HEVC_10BIT))
        .expect("open HEVC 10-bit");
    assert!(frame_bytes(&big) > cap);
    let mut inline = spawn_inline(&[]);
    inline
        .open(committed(HEVC_10BIT))
        .expect("open, inline client");
    let got = client.decode_at(0).expect("decode_at, big frames");
    let want = inline.decode_at(0).expect("decode_at, inline client");
    assert!(
        !got.bytes.is_shared(),
        "a frame bigger than its slot leased"
    );
    assert_same("capped decode_at", &got, &want);

    client.start_stream(0, 4).expect("start_stream");
    inline
        .start_stream(0, 4)
        .expect("start_stream, inline client");
    for i in 0..5 {
        let what = format!("capped stream frame {i}");
        let (got, want) = next_pair(&what, &mut client, &mut inline).expect("not at end");
        assert!(!got.bytes.is_shared(), "{what}: leased");
        assert_same(&what, &got, &want);
    }
    client.stop_stream().expect("stop_stream");

    let stats = client.transport_stats();
    assert_eq!(stats.shm_epoch, Some(epoch), "capped region regrown");
    assert_eq!(stats.shm_bytes, region_slots() * cap, "{stats:?}");
    assert_eq!(
        (stats.shared, stats.inline, stats.spilled),
        (1, 6, 0),
        "{stats:?}"
    );
}

/// Regions only grow: an open with bigger frames gets a new, bigger region
/// with a higher epoch, and a smaller open after it keeps that region. A
/// lease on the replaced region stays readable, and its late release (for
/// an epoch the sidecar no longer has) disturbs nothing.
#[test]
fn a_bigger_open_regrows_the_region() {
    let mut client = spawn(&[]);
    let mut inline = spawn_inline(&[]);

    let small = client.open(committed(H264)).expect("open H.264");
    inline
        .open(committed(H264))
        .expect("open H.264, inline client");
    let first = client.transport_stats();
    let first_epoch = first.shm_epoch.expect("region attached");
    assert_eq!(first.shm_bytes, region_slots() * slot_size(&small));
    let old = client.decode_at(0).expect("decode_at, first region");
    let old_want = inline.decode_at(0).expect("decode_at, inline client");
    assert!(old.bytes.is_shared());

    let big = client
        .open(committed(HEVC_10BIT))
        .expect("open HEVC 10-bit");
    inline
        .open(committed(HEVC_10BIT))
        .expect("open HEVC 10-bit, inline client");
    assert!(slot_size(&big) > slot_size(&small));
    let grown = client.transport_stats();
    let grown_epoch = grown.shm_epoch.expect("region attached");
    assert!(grown_epoch > first_epoch, "{first:?} -> {grown:?}");
    assert_eq!(grown.shm_bytes, region_slots() * slot_size(&big));
    let got = client.decode_at(0).expect("decode_at, grown region");
    let want = inline.decode_at(0).expect("decode_at, inline client");
    assert!(got.bytes.is_shared());
    assert_same("decode_at, grown region", &got, &want);

    assert_same("lease on the replaced region", &old, &old_want);
    drop(old);

    client.open(committed(H264)).expect("re-open H.264");
    inline
        .open(committed(H264))
        .expect("re-open H.264, inline client");
    let after = client.transport_stats();
    assert_eq!(
        (after.shm_epoch, after.shm_bytes),
        (grown.shm_epoch, grown.shm_bytes),
        "a smaller open must not replace the region"
    );
    let one_sec = small.time_base.1 / small.time_base.0;
    let got = client.decode_at(one_sec).expect("decode_at, small frames");
    let want = inline.decode_at(one_sec).expect("decode_at, inline client");
    assert!(got.bytes.is_shared());
    assert_same("small frames in the grown region", &got, &want);
    assert_eq!(client.transport_stats().inline, 0);
}

/// A client spawned with the smallest thread budget (one decode and one
/// conversion thread) gets the same frames as a default client, streamed and
/// one-shot. ProRes 4:2:2 10-bit takes the frame-threaded decode and the
/// swscale conversion; software H.264 the frame-threaded decode alone.
/// (Whether the sidecar honours the counts is `threading`'s to test.)
#[test]
fn a_thread_budget_changes_no_frame() {
    let budget = ClientOptions {
        threads: Some(ThreadBudget {
            decode: 1,
            convert: 1,
        }),
        ..ClientOptions::default()
    };
    let prores = make_fixture(
        "mov",
        "prores_ks",
        "testsrc2=s=320x180:r=25:d=0.4",
        &[
            "-c:v",
            "prores_ks",
            "-profile:v",
            "3",
            "-pix_fmt",
            "yuv422p10le",
        ],
    );
    let mut cases = vec![(committed(H264), vec![("VANCE_FORCE_SW", "1")])];
    if let Some(fx) = &prores {
        cases.push((fx.path(), vec![]));
    }
    for (path, env) in cases {
        let mut budgeted = DecoderClient::spawn_with_options(
            sidecar_binary_path(),
            env.iter().copied(),
            budget.clone(),
        )
        .expect("spawn decoder with a thread budget");
        let mut default = spawn(&env);
        let info = budgeted.open(path.as_str()).expect("open, budgeted client");
        let info_default = default.open(path.as_str()).expect("open, default client");
        assert_eq!(
            format!("{info:?}"),
            format!("{info_default:?}"),
            "{path}: stream info"
        );

        budgeted.start_stream(0, 4).expect("start_stream, budgeted");
        default.start_stream(0, 4).expect("start_stream, default");
        let mut pts = Vec::new();
        while let Some((got, want)) = next_pair(&path, &mut budgeted, &mut default) {
            assert_same(&format!("{path} stream frame {}", pts.len()), &got, &want);
            pts.push(got.pts);
        }
        assert!(pts.len() >= 2, "{path}: only {} frames", pts.len());

        let mid = pts[pts.len() / 2];
        for target in [pts[0], mid, mid + 1, pts[pts.len() - 1]] {
            let what = format!("{path} decode_at({target})");
            let got = budgeted
                .decode_at(target)
                .unwrap_or_else(|e| panic!("{what}: budgeted client: {e}"));
            let want = default
                .decode_at(target)
                .unwrap_or_else(|e| panic!("{what}: default client: {e}"));
            assert_same(&what, &got, &want);
        }
        budgeted.shutdown().expect("shutdown, budgeted client");
        default.shutdown().expect("shutdown, default client");
    }
}

const PROBE_ENV: &str = "VANCE_TEST_REGION_LEAK_PROBE";

/// No region outlives its client and its frames, however the client goes.
///
/// Intentional: the count is taken in a process of its own (this test
/// re-runs the test binary with only `region_leak_probe`).
/// `live_region_count` is process-wide, and every other test in this binary
/// maps regions concurrently.
#[test]
fn clients_leave_no_regions_mapped() {
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "transport::region_leak_probe",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(PROBE_ENV, "1")
        .output()
        .expect("re-run the test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "leak probe failed:\n{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains("1 passed"),
        "leak probe did not run:\n{stdout}"
    );
}

#[test]
#[ignore = "run in a process of its own by clients_leave_no_regions_mapped"]
fn region_leak_probe() {
    if std::env::var_os(PROBE_ENV).is_none() {
        return;
    }
    let baseline = live_region_count();
    for cycle in 0..20 {
        let mut client = spawn(&[]);
        client.open(committed(H264)).expect("open");
        assert_eq!(
            live_region_count(),
            baseline + 1,
            "cycle {cycle}: one read-only mapping per client"
        );
        let frame = client.decode_at(0).expect("decode_at");
        assert!(frame.bytes.is_shared(), "cycle {cycle}");
        match cycle % 4 {
            0 => {
                client.shutdown().expect("shutdown");
                drop(frame);
            }
            1 => {
                drop(client);
                assert_eq!(
                    live_region_count(),
                    baseline + 1,
                    "cycle {cycle}: the lease keeps the region mapped"
                );
                drop(frame);
            }
            2 => {
                client.kill().expect("kill");
                drop(frame);
                drop(client);
            }
            _ => {
                client.open(committed(HEVC_10BIT)).expect("open, regrow");
                assert_eq!(
                    live_region_count(),
                    baseline + 2,
                    "cycle {cycle}: the replaced region is kept by its lease"
                );
                drop(frame);
                assert_eq!(live_region_count(), baseline + 1, "cycle {cycle}");
                drop(client);
            }
        }
        assert_eq!(
            live_region_count(),
            baseline,
            "cycle {cycle}: a region outlived its client"
        );
    }
}
