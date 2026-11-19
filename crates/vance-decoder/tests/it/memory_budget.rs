//! Memory smoke test (Phase 1 / Milestone 1.6).
//!
//! Asserts the decoder sidecar's resident memory stays within a generous
//! Phase-1 budget after spawn + open + decoding a frame. We measure the
//! *child* process (the decoder), not the parent — the parent's footprint
//! is dominated by Tauri/wgpu and is exercised in Phase 11 distribution
//! testing, not here.
//!
//! Budget rationale: a fresh FFmpeg context for a small H.264 stream
//! should sit comfortably under 100 MB. We allow 200 MB headroom for
//! libavcodec's internal buffers + any allocator slack.

use std::path::PathBuf;
use std::time::Duration;
use sysinfo::{Pid, System};
use vance_decoder::DecoderClient;

const DECODER_BUDGET_BYTES: u64 = 200 * 1024 * 1024;

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

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/testsrc_320x240_5s.mp4")
}

/// Find the only `vance-decoder` child process and return its RSS in bytes.
fn decoder_rss() -> Option<u64> {
    let mut sys = System::new_all();
    sys.refresh_all();
    let me = std::process::id();
    sys.processes()
        .values()
        .find(|p| {
            p.parent() == Some(Pid::from_u32(me))
                && p.name().to_string_lossy().contains("vance-decoder")
        })
        .map(|p| p.memory()) // sysinfo 0.32: bytes
}

#[test]
fn decoder_sidecar_under_budget_after_decode() {
    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing at {}", bin.display());
    let fixture = fixture_path();
    assert!(fixture.exists(), "fixture missing at {}", fixture.display());

    let mut client = DecoderClient::spawn(&bin).expect("spawn decoder sidecar");
    let _info = client
        .open(fixture.to_string_lossy().to_string())
        .expect("open");
    let _frame = client.decode_at(0).expect("decode frame 0");
    // Decode a few more so we exercise allocator paths beyond cold start.
    for _ in 0..5 {
        let _ = client.decode_at(0);
    }

    // Give the OS a moment to settle accounting.
    std::thread::sleep(Duration::from_millis(200));

    let rss = decoder_rss().expect("could not find decoder child process in sysinfo");
    eprintln!(
        "decoder RSS after open + 6 decodes: {:.1} MB (budget {:.0} MB)",
        rss as f64 / 1024.0 / 1024.0,
        DECODER_BUDGET_BYTES as f64 / 1024.0 / 1024.0
    );
    assert!(
        rss <= DECODER_BUDGET_BYTES,
        "decoder RSS {} bytes exceeds budget {} bytes",
        rss,
        DECODER_BUDGET_BYTES
    );

    client.shutdown().expect("clean shutdown");
}
