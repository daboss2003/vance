//! End-to-end CLAP discovery (feature `clap`).
//!
//! Stages the synthetic `vance-clap-testplugin` as a `.clap` under a temp
//! search root and verifies discovery via the REAL out-of-process probe
//! path: the sidecar binary is re-invoked once per bundle with
//! `--vance-probe-clap`, loads it via clack-host, and prints descriptors
//! as JSON. We drive `discover_bundles_with_exe` pointed at the built
//! sidecar binary (`CARGO_BIN_EXE_vance-plugin-host`) — `discover()`
//! itself uses `current_exe()`, which under `cargo test` is the test
//! runner, not the sidecar.
//!
//! Also asserts crash isolation: a garbage `.clap` that fails to load
//! must be skipped (empty), leaving a sibling good bundle discoverable.
//!
//! Run with: `cargo test -p vance-plugin-host --features clap`.

#![cfg(feature = "clap")]

use std::path::PathBuf;

/// Path to the built sidecar binary (with the same features as this test
/// build, i.e. `clap`). Cargo sets this for integration tests.
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
fn discovers_test_clap_out_of_process() {
    let Some(dylib) = fixture_dylib() else {
        eprintln!("clap test fixture not built — skipping discovery e2e");
        return;
    };

    // Stage `<tmp>/VanceTest.clap` as a bare-file bundle (clack-host's
    // resolver accepts a plain shared library as the bundle executable).
    let root = std::env::temp_dir().join(format!("vance-clap-disc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let bundle = root.join("VanceTest.clap");
    std::fs::copy(&dylib, &bundle).unwrap();

    // Confirm the scanner picks it up via CLAP_PATH.
    // SAFETY: single-threaded test; no other thread reads the env here.
    unsafe {
        std::env::set_var("CLAP_PATH", &root);
    }
    let dirs = vance_plugin_host::clap_discovery::default_clap_dirs();
    assert!(dirs.contains(&root), "CLAP_PATH root not in search dirs: {dirs:?}");
    let bundles = vance_plugin_host::clap_discovery::scan_bundles(&dirs);
    assert!(bundles.iter().any(|b| b == &bundle), "staged bundle not scanned");

    // Drive the REAL subprocess probe path against the built sidecar.
    let descs = vance_plugin_host::clap_discovery::discover_bundles_with_exe(
        &sidecar_exe(),
        &[bundle.clone()],
    );
    assert_eq!(descs.len(), 1, "expected exactly one staged plugin, got {descs:?}");
    let d = &descs[0];
    assert!(
        matches!(d.format, vance_audio_plugins::PluginFormat::Clap),
        "format should be Clap"
    );
    assert!(d.runnable, "clap feature on → runnable");
    assert_eq!(d.name, "Vance Test Gain");
    assert!(
        d.id.ends_with("::vance.test.gain"),
        "composite id should carry the clap plugin id, got {}",
        d.id
    );

    unsafe {
        std::env::remove_var("CLAP_PATH");
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// On macOS, real `.clap` plugins ship as bundle DIRECTORIES
/// (`Foo.clap/Contents/MacOS/Foo` + `Info.plist`), not bare dylibs.
/// Exercise clack-host's NSBundle resolution against that real shape so
/// the bare-file test above isn't our only coverage (false confidence).
#[cfg(target_os = "macos")]
#[test]
fn discovers_real_macos_bundle_directory() {
    let Some(dylib) = fixture_dylib() else {
        eprintln!("clap test fixture not built — skipping macOS bundle e2e");
        return;
    };
    let root = std::env::temp_dir().join(format!("vance-clap-bundle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bundle = root.join("VanceTest.clap");
    let macos = bundle.join("Contents/MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    // The bundle executable must be named per CFBundleExecutable.
    std::fs::copy(&dylib, macos.join("VanceTest")).unwrap();
    std::fs::write(
        bundle.join("Contents/Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key>
  <string>VanceTest</string>
  <key>CFBundleIdentifier</key>
  <string>app.vance.clap.test</string>
  <key>CFBundlePackageType</key>
  <string>BNDL</string>
</dict>
</plist>
"#,
    )
    .unwrap();

    // The scanner reports the bundle DIRECTORY (and does not descend in).
    let bundles = vance_plugin_host::clap_discovery::scan_bundles(&[root.clone()]);
    assert!(
        bundles.iter().any(|b| b == &bundle),
        "bundle dir not scanned: {bundles:?}"
    );

    let descs = vance_plugin_host::clap_discovery::discover_bundles_with_exe(&sidecar_exe(), &[bundle.clone()]);
    assert_eq!(descs.len(), 1, "expected the bundled plugin, got {descs:?}");
    assert_eq!(descs[0].name, "Vance Test Gain");
    assert!(descs[0].runnable);
    assert!(descs[0].id.ends_with("::vance.test.gain"), "got {}", descs[0].id);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn garbage_bundle_is_skipped_good_one_survives() {
    let Some(dylib) = fixture_dylib() else {
        eprintln!("clap test fixture not built — skipping isolation e2e");
        return;
    };
    let root = std::env::temp_dir().join(format!("vance-clap-iso-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    // One real plugin + one garbage bundle that cannot load.
    let good = root.join("Good.clap");
    std::fs::copy(&dylib, &good).unwrap();
    let bad = root.join("Bad.clap");
    std::fs::write(&bad, b"this is not a shared library").unwrap();

    // Probe both (bad first, to prove it doesn't poison the rest).
    let descs = vance_plugin_host::clap_discovery::discover_bundles_with_exe(
        &sidecar_exe(),
        &[bad.clone(), good.clone()],
    );
    // The bad bundle contributes nothing; the good one is still found.
    assert_eq!(descs.len(), 1, "bad bundle must be skipped, good kept: {descs:?}");
    assert!(descs[0].id.contains("Good.clap"));
    assert!(descs[0].runnable);

    let _ = std::fs::remove_dir_all(&root);
}
