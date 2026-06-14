//! Phase 9 acceptance — "cache hit ratio > 80% during typical scrubbing."
//!
//! Drives the exact cache-fronted decode the engine's scrub path uses
//! (`cached_decode::cached_decode_at`) with a real decoder sidecar + a
//! real two-tier cache. Warms a jog window with a cold forward pass,
//! resets the hit/miss counters, then jogs back and forth over the same
//! window (typical scrubbing) and asserts >80% of those lookups were
//! served from cache. Headless — no GPU viewport required.
//!
//! Skipped when the decoder sidecar / the fixture are missing.

use std::path::PathBuf;
use vance_app_lib::cached_decode::cached_decode_at;
use vance_cache::FrameCache;
use vance_decoder::DecoderClient;

fn decoder_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(if cfg!(windows) { "vance-decoder.exe" } else { "vance-decoder" })
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4")
}

#[test]
fn scrub_jog_hit_ratio_exceeds_80pct() {
    let bin = decoder_bin();
    if !bin.exists() {
        eprintln!("decoder sidecar missing — skipping scrub hit-ratio test");
        return;
    }
    let fixture = fixture_path();
    if !fixture.exists() {
        eprintln!("fixture missing — skipping scrub hit-ratio test");
        return;
    }

    let mut client = DecoderClient::spawn(&bin).expect("spawn decoder sidecar");
    let uri = fixture.to_string_lossy().to_string();
    let info = client.open(uri.clone()).expect("open fixture");
    let dur = info.duration_pts.unwrap_or(0);
    assert!(dur > 0, "fixture reports no duration");

    let dir = tempfile::tempdir().expect("tempdir");
    let cache =
        FrameCache::new(dir.path().to_path_buf(), 64 * 1024 * 1024, 256 * 1024 * 1024).unwrap();

    // 5 distinct scrub targets across the first third of the clip. The
    // cache keys on the REQUESTED pts, so revisiting a target hits — which
    // is exactly what jogging back over a frame does.
    let window: Vec<i64> = (0..5).map(|i| (dur * i) / 12).collect();

    // Cold forward pass warms the cache (every target misses).
    for &pts in &window {
        let (_f, hit) = cached_decode_at(&mut client, &cache, &uri, pts).expect("warmup decode");
        assert!(!hit, "cold target {pts} should miss");
    }

    // Measure only the jog over the warmed window (typical scrubbing).
    cache.reset_hit_stats();
    for pass in 0..8 {
        if pass % 2 == 0 {
            for &pts in window.iter() {
                cached_decode_at(&mut client, &cache, &uri, pts).expect("jog decode");
            }
        } else {
            for &pts in window.iter().rev() {
                cached_decode_at(&mut client, &cache, &uri, pts).expect("jog decode");
            }
        }
    }

    let s = cache.stats();
    eprintln!(
        "[scrub] hits={} misses={} ratio={:.3} (ring={}, disk={})",
        s.ring_hits + s.disk_hits,
        s.misses,
        s.hit_ratio(),
        s.ring_hits,
        s.disk_hits,
    );
    assert!(s.lookups() > 0, "no cache lookups recorded");
    assert!(
        s.hit_ratio() > 0.80,
        "scrub hit ratio {:.3} is below the 0.80 acceptance criterion",
        s.hit_ratio()
    );
}
