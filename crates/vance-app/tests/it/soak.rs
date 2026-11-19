//! Phase 9.8 — soak test harness (the exit gate).
//!
//! Exercises the full editing → playback → cache → render loop in a tight
//! cycle so memory growth + crashes surface fast. The full exit-gate run
//! is 8 hours; this runs a configurable number of cycles (small for CI,
//! large via `VANCE_SOAK_CYCLES=N` for the manual run).
//!
//! Each cycle:
//! 1. Render a short slice via the headless renderer (decode + render +
//!    ProRes encode through the sidecars).
//! 2. Jog a window of frames through the two-tier frame cache
//!    (`cached_decode_at`) so the cache fills, stabilises, and serves
//!    repeat scrubs — the actual Phase 9 deliverable under soak.
//! 3. Sample process RSS so a leak shows up as monotonic growth.
//!
//! Asserts: the cache stays within its configured budgets, repeat scrubs
//! (after a warm-up cycle) are served almost entirely from cache, and
//! resident memory does not grow without bound (the "no memory leaks"
//! gate). NOTE: the default small-cycle run is mainly a crash/sanity
//! guard; the real leak gate is the long `VANCE_SOAK_CYCLES` run, where a
//! slow leak accumulates past the headroom.
//!
//! Skipped when sidecars or `ffmpeg` are missing — unless
//! `VANCE_REQUIRE_SIDECARS=1`, which turns a missing prerequisite into a
//! hard failure so this gate can't silently pass without running.

use std::path::PathBuf;
use std::process::Command;
use vance_app_lib::cached_decode::cached_decode_at;
use vance_app_lib::engine::{Engine, RenderOptions};
use vance_cache::FrameCache;
use vance_decoder::DecoderClient;
use vance_encoder::EncoderPreset;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4")
}

fn ffmpeg_present() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok()
}

/// A missing prerequisite. Silent skip locally; hard failure in CI when
/// `VANCE_REQUIRE_SIDECARS=1`, so a required gate can't pass without
/// executing its body.
fn missing_prereq(reason: &str) {
    if std::env::var_os("VANCE_REQUIRE_SIDECARS").is_some() {
        panic!("VANCE_REQUIRE_SIDECARS set but prerequisite missing: {reason}");
    }
    eprintln!("{reason} — skipping (set VANCE_REQUIRE_SIDECARS=1 to make this a failure)");
}

/// Resident set size of THIS process, in bytes. `None` if unavailable.
fn current_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                // VmRSS is reported in kB regardless of page size.
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        // macOS / other unix: `ps -o rss=` reports KiB.
        let out = Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        let kb: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
        Some(kb * 1024)
    }
}

#[test]
fn soak_cycle_keeps_cache_within_budget() {
    if !ffmpeg_present() {
        missing_prereq("ffmpeg missing");
        return;
    }
    let fixture = fixture_path();
    if !fixture.exists() {
        missing_prereq("fixture missing");
        return;
    }
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let dec_bin = target_dir.join(if cfg!(windows) {
        "vance-decoder.exe"
    } else {
        "vance-decoder"
    });
    let enc_bin = target_dir.join(if cfg!(windows) {
        "vance-encoder.exe"
    } else {
        "vance-encoder"
    });
    if !dec_bin.exists() || !enc_bin.exists() {
        missing_prereq("sidecars missing");
        return;
    }
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::env::set_current_dir(&workspace_root).expect("chdir");

    // Default: 6 cycles. Override with VANCE_SOAK_CYCLES for the
    // exit-gate 8-hour run.
    let cycles: u32 = std::env::var("VANCE_SOAK_CYCLES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6);

    let mut engine = Engine::default();
    engine.new_project("soak".into()).expect("new project");
    let media = engine
        .import_media_file(&fixture.canonicalize().unwrap().to_string_lossy())
        .expect("import");
    let media_id = vance_timeline::MediaItemId::from_uuid(media.id.parse::<uuid::Uuid>().unwrap());
    let track_id = {
        let contents = engine.current_sequence_contents().expect("seq");
        let v1 = contents
            .tracks
            .iter()
            .find(|t| t.kind == "video")
            .expect("V1");
        vance_timeline::TrackId::from_uuid(v1.id.parse::<uuid::Uuid>().unwrap())
    };
    engine
        .insert_clip_at(track_id, media_id, 0.0)
        .expect("clip");

    // A persistent decoder + cache for the scrub portion, reused every
    // cycle so the cache (and the long-lived sidecar) are what's soaked.
    // The cache dir + render output are RAII-scoped so a panic mid-run
    // doesn't leak them on disk.
    let uri = fixture
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let mut scrub_dec = DecoderClient::spawn(&dec_bin).expect("spawn scrub decoder");
    let info = scrub_dec.open(uri.clone()).expect("open fixture");
    let dur = info.duration_pts.unwrap_or(0).max(1);
    const RING_BUDGET: usize = 64 * 1024 * 1024;
    const DISK_BUDGET: u64 = 256 * 1024 * 1024;
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = FrameCache::new(cache_dir.path().to_path_buf(), RING_BUDGET, DISK_BUDGET).unwrap();
    let scrub_window: Vec<i64> = (0..8).map(|i| (dur * i) / 16).collect();

    let out_dir = tempfile::tempdir().expect("render tempdir");
    let out = out_dir.path().join("soak.mov");
    let mut rss_samples: Vec<u64> = Vec::new();
    for cycle in 0..cycles {
        // 1. Render a tiny half-second slice (decode + render + encode).
        let _ = std::fs::remove_file(&out);
        let outcome = engine
            .render_sequence(RenderOptions {
                output_path: out.to_string_lossy().to_string(),
                preset: EncoderPreset::ProRes422HQ,
                width: Some(160),
                height: Some(120),
                render_and_replace_track: None,
                start_seconds: Some(0.0),
                end_seconds: Some(0.25),
                hdr: false,
            })
            .expect("render");
        assert!(outcome.video_frames >= 4, "cycle {cycle}: too few frames");

        // 2. Jog the window through the frame cache (forward + back).
        for &pts in scrub_window.iter().chain(scrub_window.iter().rev()) {
            cached_decode_at(&mut scrub_dec, &cache, &uri, pts).expect("scrub decode");
        }
        // After the cold first cycle warms the cache, reset the hit/miss
        // counters so the final hit-ratio assertion measures STEADY-STATE
        // scrub performance (independent of cycle count) rather than a
        // lifetime ratio dominated by the unavoidable cold first touch.
        if cycle == 0 {
            cache.reset_hit_stats();
        }

        // 3. Cache must stay within its configured budgets every cycle
        //    (both tiers self-evict on put, so this is a tight bound).
        let cs = cache.stats();
        assert!(
            cs.ring_total_bytes <= RING_BUDGET,
            "cycle {cycle}: ring {} bytes exceeds budget {RING_BUDGET}",
            cs.ring_total_bytes
        );
        assert!(
            cs.disk_bytes <= DISK_BUDGET,
            "cycle {cycle}: disk {} bytes exceeds budget {DISK_BUDGET}",
            cs.disk_bytes
        );

        let rss = current_rss_bytes().unwrap_or(0);
        rss_samples.push(rss);
        eprintln!(
            "[soak {cycle}/{cycles}] rss={} MiB ring={} KiB disk={} KiB hit_ratio={:.3}",
            rss / (1024 * 1024),
            cs.ring_total_bytes / 1024,
            cs.disk_bytes / 1024,
            cs.hit_ratio(),
        );
    }

    // Steady-state: every repeat scrub after the warm-up cycle hits the
    // same frames, so post-reset ratio should be ~1.0. Only meaningful
    // once there's at least one post-reset (warm) cycle.
    if cycles >= 2 {
        let cs = cache.stats();
        assert!(
            cs.hit_ratio() > 0.80,
            "steady-state scrub hit ratio {:.3} below 0.80 — cache not serving repeat scrubs (hits={}, misses={})",
            cs.hit_ratio(),
            cs.ring_hits + cs.disk_hits,
            cs.misses,
        );
    }

    // Leak gate: resident memory must not grow without bound. Baseline is
    // the first post-warmup sample; assert no later sample exceeds it by
    // more than the headroom (ring budget + allocator slack). A real leak
    // accumulates across cycles and trips this on a long run.
    if rss_samples.contains(&0) {
        // RSS sampling failed on this platform — make the SKIP loud rather
        // than letting the leak gate pass without measuring memory.
        eprintln!(
            "[soak] WARNING: RSS sampling unavailable — LEAK GATE SKIPPED (samples={rss_samples:?})"
        );
        if std::env::var_os("VANCE_REQUIRE_SIDECARS").is_some() {
            panic!(
                "VANCE_REQUIRE_SIDECARS set but RSS could not be sampled — leak gate cannot run"
            );
        }
    } else if rss_samples.len() >= 3 {
        let baseline = rss_samples[1]; // first post-warmup sample
        let peak = *rss_samples[1..].iter().max().unwrap();
        let headroom = 128 * 1024 * 1024u64; // 64 MiB ring + slack
        assert!(
            peak <= baseline.saturating_add(headroom),
            "RSS grew from {} MiB (baseline) to {} MiB (peak) — possible leak (headroom {} MiB)",
            baseline / (1024 * 1024),
            peak / (1024 * 1024),
            headroom / (1024 * 1024),
        );
    }
}
