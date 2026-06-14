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
//! After all cycles we assert: the per-source ring stayed within budget,
//! resident memory did not grow without bound (the "no memory leaks"
//! gate), and the cache served the repeat scrubs well.
//!
//! Skipped when sidecars or `ffmpeg` are missing.

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
        eprintln!("ffmpeg missing — skipping soak");
        return;
    }
    let fixture = fixture_path();
    if !fixture.exists() {
        eprintln!("fixture missing — skipping soak");
        return;
    }
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    let dec_bin = target_dir.join(if cfg!(windows) { "vance-decoder.exe" } else { "vance-decoder" });
    let enc_bin = target_dir.join(if cfg!(windows) { "vance-encoder.exe" } else { "vance-encoder" });
    if !dec_bin.exists() || !enc_bin.exists() {
        eprintln!("sidecars missing — skipping soak");
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
    let media_id =
        vance_timeline::MediaItemId::from_uuid(media.id.parse::<uuid::Uuid>().unwrap());
    let track_id = {
        let contents = engine.current_sequence_contents().expect("seq");
        let v1 = contents.tracks.iter().find(|t| t.kind == "video").expect("V1");
        vance_timeline::TrackId::from_uuid(v1.id.parse::<uuid::Uuid>().unwrap())
    };
    engine.insert_clip_at(track_id, media_id, 0.0).expect("clip");

    // A persistent decoder + cache for the scrub portion. Reused across
    // every cycle so the cache (and the long-lived sidecar) are the things
    // actually soaked.
    let uri = fixture.canonicalize().unwrap().to_string_lossy().to_string();
    let mut scrub_dec = DecoderClient::spawn(&dec_bin).expect("spawn scrub decoder");
    let info = scrub_dec.open(uri.clone()).expect("open fixture");
    let dur = info.duration_pts.unwrap_or(0).max(1);
    let cache_dir = std::env::temp_dir().join(format!("vance-soak-cache-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&cache_dir);
    let cache = FrameCache::new(cache_dir.clone(), 64 * 1024 * 1024, 256 * 1024 * 1024).unwrap();
    let scrub_window: Vec<i64> = (0..8).map(|i| (dur * i) / 16).collect();

    let out = std::env::temp_dir().join(format!("vance-soak-{}.mov", std::process::id()));
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
            })
            .expect("render");
        assert!(outcome.video_frames >= 4, "cycle {cycle}: too few frames");

        // 2. Jog the window through the frame cache (forward + back).
        for &pts in scrub_window.iter().chain(scrub_window.iter().rev()) {
            cached_decode_at(&mut scrub_dec, &cache, &uri, pts).expect("scrub decode");
        }

        // 3. Cache must stay within budget every cycle.
        let cs = cache.stats();
        assert!(
            cs.ring_total_bytes < 256 * 1024 * 1024,
            "cycle {cycle}: ring total {} bytes exceeds 256 MiB ceiling",
            cs.ring_total_bytes
        );
        assert!(
            cs.disk_bytes < 512 * 1024 * 1024,
            "cycle {cycle}: disk {} bytes exceeds 512 MiB (budget 256 MiB)",
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
    let _ = std::fs::remove_file(&out);
    let _ = std::fs::remove_dir_all(&cache_dir);

    // The repeat scrubs (after the first cold cycle) should be served
    // overwhelmingly from cache — the same frames, every cycle.
    let cs = cache.stats();
    assert!(
        cs.hit_ratio() > 0.80,
        "soak scrub hit ratio {:.3} below 0.80 — cache not serving repeat scrubs",
        cs.hit_ratio()
    );

    // Leak gate: resident memory must not grow without bound. The frame
    // cache fills to its ring budget early then plateaus, so compare the
    // final sample against a post-warmup baseline. A true leak grows
    // monotonically and blows past the (generous) headroom over a long run.
    if rss_samples.len() >= 3 && rss_samples.iter().all(|&r| r > 0) {
        let warmup = (rss_samples.len() / 3).max(1); // skip the ramp-up third
        let baseline = rss_samples[warmup];
        let final_rss = *rss_samples.last().unwrap();
        // Allow the ring budget (64 MiB) + allocator slack on top of the
        // post-warmup baseline.
        let headroom = 192 * 1024 * 1024u64;
        assert!(
            final_rss <= baseline.saturating_add(headroom),
            "RSS grew from {} MiB (post-warmup) to {} MiB — possible leak (headroom {} MiB)",
            baseline / (1024 * 1024),
            final_rss / (1024 * 1024),
            headroom / (1024 * 1024),
        );
    }
}
