//! Phase 9.3 — prefetch worker integration.
//!
//! After a prefetch request, frames AHEAD of the anchor should appear in
//! the cache (the worker decodes forward into it on its own thread). We
//! poll the cache with a bounded timeout, then assert prefetched frames
//! landed. Skipped when the decoder sidecar / fixture are missing.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vance_app_lib::prefetch::{PrefetchRequest, PrefetchWorker};
use vance_cache::{CacheKey, FrameCache};
use vance_decoder::DecoderClient;

fn decoder_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(if cfg!(windows) {
            "vance-decoder.exe"
        } else {
            "vance-decoder"
        })
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4")
}

/// Missing prerequisite: silent skip locally, hard failure in CI when
/// `VANCE_REQUIRE_SIDECARS=1`.
fn missing_prereq(reason: &str) {
    if std::env::var_os("VANCE_REQUIRE_SIDECARS").is_some() {
        panic!("VANCE_REQUIRE_SIDECARS set but prerequisite missing: {reason}");
    }
    eprintln!("{reason} — skipping (set VANCE_REQUIRE_SIDECARS=1 to make this a failure)");
}

#[test]
fn prefetch_warms_frames_ahead_of_anchor() {
    let bin = decoder_bin();
    if !bin.exists() {
        missing_prereq("decoder sidecar missing");
        return;
    }
    let fixture = fixture_path();
    if !fixture.exists() {
        missing_prereq("fixture missing");
        return;
    }
    let uri = fixture.to_string_lossy().to_string();

    // Probe duration so we can pick a valid forward stride.
    let mut probe = DecoderClient::spawn(&bin).expect("spawn probe decoder");
    let info = probe.open(uri.clone()).expect("open fixture");
    let dur = info.duration_pts.unwrap_or(0);
    assert!(dur > 0, "fixture reports no duration");
    drop(probe);

    let dir = tempfile::tempdir().expect("tempdir");
    let cache = Arc::new(
        FrameCache::new(
            dir.path().to_path_buf(),
            64 * 1024 * 1024,
            256 * 1024 * 1024,
        )
        .unwrap(),
    );

    let step_pts = (dur / 30).max(1);
    let horizon = 6u32;
    let worker = PrefetchWorker::spawn(cache.clone(), bin);
    worker.request(PrefetchRequest {
        source_uri: uri.clone(),
        anchor_pts: 0,
        step_pts,
        horizon_frames: horizon,
    });

    // The worker spawns its own decoder lazily, so allow a few seconds.
    // Poll until it has walked the FULL horizon (not just one frame) — a
    // supersession/off-by-one regression that decoded one frame and
    // stalled would otherwise pass. Allow one frame of slack for timing.
    let expected: Vec<i64> = (1..=horizon as i64).map(|k| k * step_pts).collect();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut warmed = 0;
    while Instant::now() < deadline {
        warmed = expected
            .iter()
            .filter(|&&pts| cache.contains(&CacheKey::for_source_frame(uri.clone(), pts, 0)))
            .count();
        if warmed >= horizon as usize {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    worker.shutdown();

    eprintln!(
        "[prefetch] warmed {warmed}/{} frames ahead of anchor",
        expected.len()
    );
    assert!(
        warmed >= horizon as usize - 1,
        "prefetch warmed only {warmed}/{} frames — worker did not walk the horizon",
        expected.len()
    );
}
