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

/// A decoder stuck on its sidecar (stopped with SIGSTOP here, as one hung on
/// a network share would be) is freed by a kill handle used from another
/// thread: the blocked request returns promptly with an IPC error. Killing
/// again does nothing more, and after the client has shut its sidecar down
/// (reaped it, so its pid could be anyone's) the handle sends no signal.
///
/// The request runs on a thread of its own with a deadline, so a handle
/// that does nothing fails this test instead of hanging it.
#[cfg(unix)]
#[test]
fn a_kill_handle_unblocks_a_request_stuck_on_its_sidecar() {
    use std::time::{Duration, Instant};
    let signal = |sig: &str, pid: u32| {
        std::process::Command::new("kill")
            .args([sig, &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success())
    };
    let mut client = DecoderClient::spawn(sidecar_binary_path()).expect("spawn");
    client
        .open(fixture_path().to_string_lossy().into_owned())
        .expect("open");
    let pid = client.pid().expect("pid");
    let handle = client.kill_handle();
    assert!(!handle.signalled());
    assert!(signal("-STOP", pid), "SIGSTOP {pid}");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let start = Instant::now();
        let result = client.decode_at(0).map(|f| f.pts);
        let _ = tx.send((result, start.elapsed(), client));
    });
    std::thread::sleep(Duration::from_millis(300));
    handle.kill();
    let Ok((result, waited, client)) = rx.recv_timeout(Duration::from_secs(10)) else {
        // Don't leave a stopped sidecar behind.
        signal("-KILL", pid);
        panic!("the request was still blocked 10 s after kill()");
    };
    assert!(
        matches!(result, Err(ClientError::Ipc(_))),
        "a killed sidecar's request fails with an IPC error: {result:?}"
    );
    assert!(waited < Duration::from_secs(5), "returned after {waited:?}");
    assert!(handle.signalled());
    handle.kill();
    drop(client);
    handle.kill();
    assert!(handle.signalled(), "still the one signal");

    // After a clean shutdown: reaped, so no signal at all.
    let client = DecoderClient::spawn(sidecar_binary_path()).expect("spawn");
    let handle = client.kill_handle();
    client.shutdown().expect("shutdown");
    handle.kill();
    handle.kill();
    assert!(
        !handle.signalled(),
        "no signal after the process was reaped"
    );
    // And after a plain drop.
    let client = DecoderClient::spawn(sidecar_binary_path()).expect("spawn");
    let handle = client.kill_handle();
    drop(client);
    handle.kill();
    assert!(!handle.signalled(), "no signal after drop");
}

/// The handle crosses threads and clones freely.
#[test]
fn kill_handles_are_send_sync_and_clone() {
    fn assert_traits<T: Clone + Send + Sync + 'static>() {}
    assert_traits::<vance_decoder::SidecarKill>();
}

/// A sidecar that hangs during startup (this fake one never answers
/// `Hello`) can be killed from another thread: `spawn_with_kill_handle`
/// hands over the kill handle before the handshake, and the spawn then
/// fails promptly instead of blocking forever.
#[cfg(unix)]
#[test]
fn a_sidecar_stuck_in_its_handshake_can_be_killed() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};
    let dir = std::env::temp_dir();
    let script = dir.join(format!("vance-fake-decoder-{}.sh", std::process::id()));
    let pid_file = dir.join(format!("vance-fake-decoder-{}.pid", std::process::id()));
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 3600\n",
            pid_file.display()
        ),
    )
    .expect("write the fake sidecar");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cleanup = || {
        if let Ok(pid) = std::fs::read_to_string(&pid_file) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", pid.trim()])
                .status();
        }
        let _ = std::fs::remove_file(&script);
        let _ = std::fs::remove_file(&pid_file);
    };

    let (handle_tx, handle_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let binary = script.clone();
    std::thread::spawn(move || {
        let start = Instant::now();
        let spawned = DecoderClient::spawn_with_kill_handle(
            &binary,
            std::iter::empty::<(&str, &str)>(),
            vance_decoder::ClientOptions::default(),
            |kill| handle_tx.send(kill).expect("send the handle"),
        );
        let _ = done_tx.send((spawned.err().map(|e| e.to_string()), start.elapsed()));
    });
    let Ok(handle) = handle_rx.recv_timeout(Duration::from_secs(5)) else {
        cleanup();
        panic!("no kill handle before the handshake");
    };
    std::thread::sleep(Duration::from_millis(300));
    handle.kill();
    let Ok((error, waited)) = done_rx.recv_timeout(Duration::from_secs(10)) else {
        cleanup();
        panic!("spawn still blocked 10 s after kill()");
    };
    cleanup();
    assert!(handle.signalled());
    let error = error.expect("a killed handshake is an error");
    assert!(error.contains("ipc"), "{error}");
    assert!(waited < Duration::from_secs(5), "returned after {waited:?}");
}

/// The handle `spawn_with_kill_handle` hands over is the client's own.
#[test]
fn the_spawn_time_kill_handle_is_the_clients() {
    let mut early = None;
    let mut client = DecoderClient::spawn_with_kill_handle(
        sidecar_binary_path(),
        std::iter::empty::<(&str, &str)>(),
        vance_decoder::ClientOptions::default(),
        |kill| early = Some(kill),
    )
    .expect("spawn");
    let early = early.expect("the handle was handed over");
    client
        .open(fixture_path().to_string_lossy().into_owned())
        .expect("open");
    early.kill();
    assert!(early.signalled() && client.kill_handle().signalled());
    assert!(
        matches!(client.decode_at(0), Err(ClientError::Ipc(_))),
        "killed through the early handle"
    );
}
