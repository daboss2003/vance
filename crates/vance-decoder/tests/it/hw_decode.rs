//! Hardware decode integration test (Phase 2.2).
//!
//! On macOS, opening an H.264 file should automatically engage VideoToolbox
//! and the StreamInfo's `pixel_format` should report NV12 (VT's native
//! output format) — distinct from the YUV420P that the SW path returns
//! when `VANCE_FORCE_SW=1`.
//!
//! Skipped on non-macOS until other hwaccel backends land.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use vance_decoder::{DecoderClient, PixelFormat};

fn sidecar_binary_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push("vance-decoder");
    p
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/testsrc_320x240_5s.mp4")
}

#[test]
fn videotoolbox_path_returns_nv12_frames() {
    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing");
    let fixture = fixture_path();
    assert!(fixture.exists(), "fixture missing");

    // Spawn with HW decode (default on macOS).
    let mut client = DecoderClient::spawn(&bin).expect("spawn HW client");
    let info = client
        .open(fixture.to_string_lossy().to_string())
        .expect("open");
    assert_eq!(
        info.pixel_format,
        PixelFormat::Nv12,
        "HW path on macOS should report NV12; got {:?}",
        info.pixel_format
    );

    // Decode a frame and verify the byte count matches NV12 layout.
    let frame = client.decode_at(0).expect("decode frame 0");
    assert_eq!(frame.pixel_format, PixelFormat::Nv12);
    let expected_size = PixelFormat::Nv12.frame_size(info.width, info.height);
    assert_eq!(frame.bytes.len(), expected_size);
    // NV12: 2 planes (Y, UV).
    assert_eq!(frame.strides.len(), 2);

    client.shutdown().expect("shutdown");
}

#[test]
fn force_software_returns_yuv420p_frames() {
    let bin = sidecar_binary_path();
    assert!(bin.exists());
    let fixture = fixture_path();

    let mut client =
        DecoderClient::spawn_with_env(&bin, [("VANCE_FORCE_SW", "1")]).expect("spawn SW client");
    let info = client
        .open(fixture.to_string_lossy().to_string())
        .expect("open");
    assert_eq!(
        info.pixel_format,
        PixelFormat::Yuv420P,
        "SW path should report YUV420P; got {:?}",
        info.pixel_format
    );

    let frame = client.decode_at(0).expect("decode SW frame 0");
    assert_eq!(frame.pixel_format, PixelFormat::Yuv420P);

    client.shutdown().expect("shutdown");
}
