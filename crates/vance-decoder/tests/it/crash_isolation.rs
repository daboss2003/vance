//! Crash-isolation test (Phase 1 / Milestone 1.6).
//!
//! Validates the architectural promise of ADR 0004 / PLAN.md §3 principle 4:
//! a sidecar process crash must not take down the parent. We spawn the
//! decoder, open a file, kill the sidecar mid-stream, and confirm:
//!
//! 1. The next `decode_at` call returns `Err(ClientError::Ipc(SidecarExited))`
//!    rather than panicking.
//! 2. The error is descriptive enough for the engine to surface it to the
//!    user (string contains "exit" or "exited").
//! 3. We can `spawn` a fresh `DecoderClient` immediately after — the parent
//!    process is still healthy.
//! 4. Frames leased through shared memory stay readable after the crash.

use std::path::PathBuf;
use vance_decoder::{client::ClientError, DecoderClient};

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
fn killed_sidecar_surfaces_clean_error_and_parent_recovers() {
    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing at {}", bin.display());
    let fixture = fixture_path();
    assert!(fixture.exists(), "fixture missing at {}", fixture.display());

    // Spawn + open + decode one frame so we know everything is healthy.
    let mut client = DecoderClient::spawn(&bin).expect("spawn decoder sidecar");
    let _info = client
        .open(fixture.to_string_lossy().to_string())
        .expect("open fixture");
    let _frame = client.decode_at(0).expect("decode frame 0 before kill");

    // Force-kill the sidecar. Give the OS a beat to reap.
    client.kill().expect("kill request");
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Next call must fail cleanly — no panic, no hang.
    let result = client.decode_at(0);
    let err = result.expect_err("decode after kill must fail");

    // The error should bubble from the IPC layer as a SidecarExited.
    let msg = format!("{err}");
    assert!(
        matches!(err, ClientError::Ipc(vance_ipc::Error::SidecarExited(_))),
        "expected SidecarExited, got: {err:?}",
    );
    assert!(
        msg.to_lowercase().contains("exit"),
        "error string should mention exit, got: {msg}",
    );

    // Drop the dead client and prove we can spawn a fresh one — the
    // parent process is still healthy.
    drop(client);
    let mut second = DecoderClient::spawn(&bin).expect("respawn decoder after crash");
    let info2 = second
        .open(fixture.to_string_lossy().to_string())
        .expect("re-open fixture in fresh sidecar");
    assert_eq!(info2.width, 320);
    second.shutdown().expect("clean shutdown of second client");
}

/// With shared-memory frames (ADR 0006): frames leased from a sidecar stay
/// readable after it is killed and after its client is gone, and the next
/// request still reports the crash as `SidecarExited` although the client
/// has a slot release queued for the dead sidecar.
#[test]
fn leased_frames_outlive_a_killed_sidecar() {
    let bin = sidecar_binary_path();
    assert!(bin.exists(), "decoder binary missing at {}", bin.display());
    let fixture = fixture_path().to_string_lossy().into_owned();

    let mut reference =
        DecoderClient::spawn_with_env(&bin, [("VANCE_IPC_SHM", "0")]).expect("spawn reference");
    let info = reference.open(fixture.as_str()).expect("open, reference");
    let one_sec = info.time_base.1 / info.time_base.0;
    let want0 = reference.decode_at(0).expect("decode_at(0), reference");
    let want1 = reference
        .decode_at(one_sec)
        .expect("decode_at(1s), reference");
    reference.shutdown().expect("shutdown, reference");

    let mut client = DecoderClient::spawn(&bin).expect("spawn decoder sidecar");
    client.open(fixture.as_str()).expect("open fixture");
    let f0 = client.decode_at(0).expect("decode_at(0)");
    let f1 = client.decode_at(one_sec).expect("decode_at(1s)");
    assert!(
        f0.bytes.is_shared() && f1.bytes.is_shared(),
        "expected leases: {:?}",
        client.transport_stats()
    );

    client.kill().expect("kill request");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(f0.bytes, want0.bytes, "lease after the kill");
    assert_eq!(f1.bytes, want1.bytes, "lease after the kill");
    drop(f1);

    let err = client
        .decode_at(0)
        .expect_err("decode after kill must fail");
    assert!(
        matches!(err, ClientError::Ipc(vance_ipc::Error::SidecarExited(_))),
        "expected SidecarExited, got: {err:?}",
    );
    drop(client);
    assert_eq!(f0.bytes, want0.bytes, "lease after its client is gone");
}
