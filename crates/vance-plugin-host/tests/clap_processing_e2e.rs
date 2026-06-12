//! End-to-end out-of-process CLAP PROCESSING (feature `clap`).
//!
//! Spawns the real sidecar binary, instantiates the synthetic
//! `vance-clap-testplugin` over IPC, pushes a stereo block through it,
//! and asserts the fixture's fixed 0.5x gain came back — proving audio
//! is processed in the sandbox, not in-process. Also checks a bogus id
//! errors cleanly (not a crash) and that destroy/shutdown are graceful.
//!
//! Run with: `cargo test -p vance-plugin-host --features clap`.

#![cfg(feature = "clap")]

use std::path::PathBuf;
use vance_audio_plugins::{PluginClient, PluginDescriptor, PluginFormat};

fn sidecar_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vance-plugin-host"))
}

fn fixture_dylib() -> Option<PathBuf> {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug")
        .join(format!("{prefix}vance_clap_testplugin.{ext}"));
    p.exists().then_some(p)
}

#[test]
fn processes_clap_block_out_of_process() {
    let Some(dylib) = fixture_dylib() else {
        eprintln!("clap test fixture not built — skipping processing e2e");
        return;
    };
    let mut client = PluginClient::spawn(sidecar_exe()).expect("spawn sidecar");

    let descriptor = PluginDescriptor {
        format: PluginFormat::Clap,
        id: format!("{}::vance.test.gain", dylib.to_string_lossy()),
        name: "Vance Test Gain".into(),
        vendor: "vance".into(),
        runnable: true,
    };
    let inst = client
        .instantiate(descriptor, 48_000.0, 512)
        .expect("instantiate over IPC");

    // 64-frame stereo block of 1.0 → expect ~0.5 back.
    let block: Vec<f32> = vec![1.0; 64 * 2];
    let out = client.process(inst, &block).expect("process over IPC");
    assert_eq!(out.len(), block.len(), "output length must match input");
    for (i, &s) in out.iter().enumerate() {
        assert!((s - 0.5).abs() < 1e-3, "sample {i} expected ~0.5, got {s}");
    }

    client.destroy(inst).expect("destroy");
    client.shutdown().expect("graceful shutdown");
}

#[test]
fn unknown_clap_id_errors_without_crashing() {
    let mut client = PluginClient::spawn(sidecar_exe()).expect("spawn sidecar");
    let descriptor = PluginDescriptor {
        format: PluginFormat::Clap,
        id: "/no/such/bundle.clap::com.nope".into(),
        name: "Ghost".into(),
        vendor: "".into(),
        runnable: true,
    };
    let r = client.instantiate(descriptor, 48_000.0, 512);
    assert!(r.is_err(), "loading a missing bundle must error, not panic");
    // Sidecar is still alive and usable after a clean error.
    client.shutdown().expect("graceful shutdown after error");
}
