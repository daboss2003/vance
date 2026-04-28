//! Streaming decoder integration test (Phase 2.1).
//!
//! Validates that the new `StartStream` / `NextFrame` / `StopStream` path
//! delivers decoded frames in PTS order and reaches a clean end-of-stream.

use std::path::PathBuf;
use vance_decoder::DecoderClient;

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

#[test]
fn stream_delivers_frames_in_pts_order_and_eos() {
    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing");
    let fixture = fixture_path();
    assert!(fixture.exists(), "fixture missing");

    let mut client = DecoderClient::spawn(&bin).expect("spawn");
    let info = client
        .open(fixture.to_string_lossy().to_string())
        .expect("open");
    eprintln!(
        "opened: {}x{} @ tb={}/{}",
        info.width, info.height, info.time_base.0, info.time_base.1
    );

    // Start streaming from PTS 0 with a small buffer.
    let first_hint = client.start_stream(0, 4).expect("start_stream");
    eprintln!("stream started, hinted first_pts = {first_hint}");

    // Pull frames until end-of-stream. testsrc 5s @ 30fps = 150 frames.
    let mut count = 0usize;
    let mut last_pts = i64::MIN;
    while let Some(f) = client.next_frame().expect("next_frame") {
        assert!(
            f.pts > last_pts,
            "frame {count}: pts {} not strictly > last {last_pts}",
            f.pts
        );
        assert_eq!(f.width, info.width);
        assert_eq!(f.height, info.height);
        last_pts = f.pts;
        count += 1;
        if count > 1000 {
            panic!("got too many frames; testsrc should be ~150");
        }
    }
    eprintln!("delivered {count} frames, last pts = {last_pts}");

    // testsrc is 5 seconds at 30 fps; expect ~150 frames. Accept a wide
    // tolerance for fixture quirks.
    assert!(
        (100..=200).contains(&count),
        "expected ~150 frames, got {count}",
    );

    // After EOS, the sidecar can be reused: start a fresh stream.
    let _ = client.start_stream(0, 4).expect("restart stream after eos");
    let f0 = client
        .next_frame()
        .expect("first frame of restart")
        .unwrap();
    eprintln!("restart: first frame pts {}", f0.pts);

    client.stop_stream().expect("stop_stream");
    client.close().expect("close");
    client.shutdown().expect("shutdown");
}

#[test]
fn stop_stream_mid_stream_recovers_for_decode_at() {
    // Start a stream, pull a few frames, stop, then use the one-shot
    // decode_at API. Verifies the sidecar's OpenContext is reclaimed
    // cleanly on stop and is usable by the legacy path.
    let bin = sidecar_binary_path();
    let fixture = fixture_path();

    let mut client = DecoderClient::spawn(&bin).expect("spawn");
    let info = client
        .open(fixture.to_string_lossy().to_string())
        .expect("open");

    client.start_stream(0, 4).expect("start_stream");
    for _ in 0..5 {
        let _ = client
            .next_frame()
            .expect("next_frame")
            .expect("not eos yet");
    }
    client.stop_stream().expect("stop_stream");

    // One-shot path still works after stopping.
    let one_sec_pts = info.time_base.1 / info.time_base.0;
    let f = client.decode_at(one_sec_pts).expect("decode_at after stop");
    assert!(f.pts >= one_sec_pts);

    client.shutdown().expect("shutdown");
}
