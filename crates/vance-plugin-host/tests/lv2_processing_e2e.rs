//! End-to-end out-of-process LV2 hosting (feature `lv2`).
//!
//! Stages the synthetic `vance-lv2-testplugin` as a real `.lv2` bundle,
//! points `LV2_PATH` at it (inherited by the spawned sidecar), then via
//! IPC: discovers it (ListPlugins → format Lv2), instantiates it, pushes
//! a stereo block, and asserts the fixture's 0.5x gain came back through
//! the sandbox. Skips when the fixture isn't built.
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

/// Build a `.lv2` bundle under a temp dir; return its LV2_PATH root.
fn stage_bundle(dylib: &PathBuf) -> std::io::Result<PathBuf> {
    let root = std::env::temp_dir().join(format!("vance-lv2-sc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bundle = root.join("vance-test.lv2");
    std::fs::create_dir_all(&bundle)?;
    let dy = dylib.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::copy(dylib, bundle.join(&dy))?;
    std::fs::write(
        bundle.join("manifest.ttl"),
        format!(
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n\
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
             <{URI}>\n\ta lv2:Plugin ;\n\tlv2:binary <{dy}> ;\n\trdfs:seeAlso <vance-test.ttl> .\n"
        ),
    )?;
    std::fs::write(
        bundle.join("vance-test.ttl"),
        format!(
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n\
             @prefix doap: <http://usefulinc.com/ns/doap#> .\n\
             <{URI}>\n\ta lv2:Plugin ;\n\tdoap:name \"Vance Test Gain\" ;\n\
             \tlv2:port [\n\t\ta lv2:AudioPort, lv2:InputPort ;\n\t\tlv2:index 0 ; lv2:symbol \"in\" ; lv2:name \"In\"\n\
             \t] , [\n\t\ta lv2:AudioPort, lv2:OutputPort ;\n\t\tlv2:index 1 ; lv2:symbol \"out\" ; lv2:name \"Out\"\n\t] .\n"
        ),
    )?;
    Ok(root)
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
    let root = stage_bundle(&dylib).expect("stage .lv2 bundle");
    // The sidecar inherits this process's env (incl. LV2_PATH).
    // SAFETY: single-threaded test setup.
    unsafe {
        std::env::set_var("LV2_PATH", &root);
    }

    let mut client = PluginClient::spawn(sidecar_exe()).expect("spawn sidecar");

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
    let inst = client.instantiate(descriptor(), 48_000.0, 512).expect("instantiate");
    let block = vec![1.0f32; 64 * 2];
    let out = client.process(inst, &block).expect("process");
    assert_eq!(out.len(), block.len());
    let mean = out.iter().copied().sum::<f32>() / out.len() as f32;
    assert!((mean - 0.5).abs() < 1e-3, "expected ~0.5, got {mean}");

    client.destroy(inst).expect("destroy");
    client.shutdown().expect("shutdown");
    let _ = std::fs::remove_dir_all(&root);
}
