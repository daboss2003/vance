//! End-to-end out-of-process LV2 hosting (feature `lv2`).
//!
//! Stages the synthetic `vance-lv2-testplugin` as a real `.lv2` bundle and
//! hands `LV2_PATH` to the spawned sidecar via its child env (no
//! process-global `set_var`), then via IPC: discovers it (ListPlugins →
//! format Lv2), instantiates it, pushes a stereo block, and asserts the
//! fixture's 0.5x gain came back through the sandbox. Skips when the
//! fixture isn't built.
//!
//! Run with: `cargo test -p vance-plugin-host --features lv2`.

#![cfg(feature = "lv2")]

use std::path::PathBuf;
use vance_audio_plugins::{PluginClient, PluginDescriptor, PluginFormat};

const URI: &str = "urn:vance:test:gain";

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
        .join(format!("{prefix}vance_lv2_testplugin.{ext}"));
    p.exists().then_some(p)
}

fn descriptor() -> PluginDescriptor {
    PluginDescriptor {
        format: PluginFormat::Lv2,
        id: URI.into(),
        name: "Vance Test Gain".into(),
        vendor: String::new(),
        runnable: true,
    }
}

#[test]
fn lv2_discovered_and_processed_out_of_process() {
    let Some(dylib) = fixture_dylib() else {
        eprintln!("lv2 test fixture not built — skipping lv2 e2e");
        return;
    };
    // Shared staging helper: unique temp dir + RAII cleanup on drop.
    let bundle = vance_lv2_testplugin::staging::stage_bundle(&dylib).expect("stage .lv2 bundle");

    // Hand LV2_PATH to the sidecar via its child env (not the parent's
    // process-global env).
    let mut client = PluginClient::spawn_with_env(
        sidecar_exe(),
        std::iter::once(("LV2_PATH", bundle.root().as_os_str())),
    )
    .expect("spawn sidecar");

    // Discovery surfaces the staged plugin as an LV2 descriptor.
    let plugins = client.list_plugins().expect("list_plugins");
    assert!(
        plugins
            .iter()
            .any(|p| p.id == URI && matches!(p.format, PluginFormat::Lv2)),
        "staged LV2 plugin not discovered: {plugins:?}"
    );

    // Instantiate + process: the fixture is a 0.5x mono gain. Feed 1.0 on
    // both channels → ~0.5 back.
    let inst = client
        .instantiate(descriptor(), 48_000.0, 512)
        .expect("instantiate");
    let block = vec![1.0f32; 64 * 2];
    let out = client.process(inst, &block).expect("process");
    assert_eq!(out.len(), block.len());
    let mean = out.iter().copied().sum::<f32>() / out.len() as f32;
    assert!((mean - 0.5).abs() < 1e-3, "expected ~0.5, got {mean}");

    client.destroy(inst).expect("destroy");
    client.shutdown().expect("shutdown");
    drop(bundle); // keep the bundle alive until the sidecar is done with it
}
