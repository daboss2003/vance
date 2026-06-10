//! Phase 10.7 — OFX enumeration tests.
//!
//! Uses the `vance-ofx-testplugin` cdylib (a fake OFX binary that
//! exports `OfxGetNumberOfPlugins` / `OfxGetPlugin`) to exercise both
//! the direct FFI path and the full sidecar path with a synthetic
//! bundle layout. Skips gracefully if the fixture dylib or the
//! sidecar binary aren't built.

use std::path::PathBuf;
use vance_ofx::{bundle, ffi};

fn target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug")
}

fn testplugin_lib() -> Option<PathBuf> {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let p = target_dir().join(format!("{prefix}vance_ofx_testplugin.{ext}"));
    p.exists().then_some(p)
}

fn ofx_host_bin() -> Option<PathBuf> {
    let p = target_dir().join(if cfg!(windows) {
        "vance-ofx-host.exe"
    } else {
        "vance-ofx-host"
    });
    p.exists().then_some(p)
}

#[test]
fn ffi_enumerates_fake_plugin() {
    let Some(lib) = testplugin_lib() else {
        eprintln!("testplugin dylib not built — skipping");
        return;
    };
    let infos = ffi::enumerate(&lib).expect("enumerate");
    assert_eq!(infos.len(), 1);
    let p = &infos[0];
    assert_eq!(p.identifier, "com.vance.test.invert");
    assert_eq!(p.api, "OfxImageEffectPluginAPI");
    assert_eq!(p.api_version, 1);
    assert_eq!(p.version_major, 2);
    assert_eq!(p.version_minor, 5);
}

#[test]
fn ffi_rejects_non_ofx_library() {
    // A library without the OFX entry points → MissingSymbol.
    // Reuse a system lib that exists but isn't an OFX plugin: point at
    // a temp file containing garbage → Open error instead. Either way
    // it must be an Err, not a crash.
    let tmp = std::env::temp_dir().join(format!("vance-ofx-garbage-{}.bin", std::process::id()));
    std::fs::write(&tmp, b"not a shared library").unwrap();
    let err = ffi::enumerate(&tmp);
    assert!(err.is_err(), "garbage must not enumerate");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn discovery_plus_sidecar_lists_synthetic_bundle() {
    let Some(lib) = testplugin_lib() else {
        eprintln!("testplugin dylib not built — skipping");
        return;
    };
    let Some(host) = ofx_host_bin() else {
        eprintln!("vance-ofx-host not built — skipping");
        return;
    };

    // Build a synthetic OFX bundle whose binary is a copy of the fake
    // plugin dylib, placed in the platform arch subdir the discovery
    // code looks in.
    let root = std::env::temp_dir().join(format!("vance-ofx-bundle-{}", std::process::id()));
    let arch = if cfg!(target_os = "macos") {
        "MacOS"
    } else if cfg!(target_os = "windows") {
        "Win64"
    } else {
        "Linux-x86-64"
    };
    let bin_dir = root.join("Invert.ofx.bundle").join("Contents").join(arch);
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::copy(&lib, bin_dir.join("Invert.ofx")).unwrap();

    // Sanity: the discovery layer sees the bundle + resolves its binary.
    let bundles = bundle::discover(&[root.clone()]);
    assert_eq!(bundles.len(), 1);
    assert_eq!(bundles[0].name, "Invert");
    assert!(bundles[0].binary_path.is_some());

    // Now drive it through the sidecar (out-of-process dlopen).
    let mut client = vance_ofx::OfxHostClient::spawn(&host).expect("spawn ofx host");
    let (plugins, errors) = client
        .list_plugins(vec![root.to_string_lossy().to_string()])
        .expect("list");
    let _ = client.shutdown();

    assert!(
        errors.is_empty(),
        "unexpected enumerate errors: {errors:?}"
    );
    assert_eq!(plugins.len(), 1, "expected one fake plugin");
    assert_eq!(plugins[0].identifier, "com.vance.test.invert");
    assert_eq!(plugins[0].bundle, "Invert");

    std::fs::remove_dir_all(&root).ok();
}
