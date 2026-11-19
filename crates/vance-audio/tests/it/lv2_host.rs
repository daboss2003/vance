//! In-process LV2 host end-to-end (feature `lv2`).
//!
//! Stages the synthetic `vance-lv2-testplugin` as a real `.lv2` bundle
//! (manifest.ttl + plugin.ttl + the dylib), points `LV2_PATH` at it, and
//! loads it through `vance_audio::lv2_host` — verifying the fixture's
//! fixed 0.5x gain comes back. Skips gracefully when the fixture isn't
//! built.
//!
//! Run with: `cargo test -p vance-audio --features lv2`.

#![cfg(feature = "lv2")]

use std::path::PathBuf;

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

#[test]
fn loads_and_processes_real_lv2_plugin() {
    let Some(dylib) = fixture_dylib() else {
        eprintln!("lv2 test fixture not built — skipping");
        return;
    };
    // Shared staging helper: unique temp dir + RAII cleanup on drop (so a
    // panic below still removes the staged bundle).
    let bundle = vance_lv2_testplugin::staging::stage_bundle(&dylib).expect("stage .lv2 bundle");
    // lilv reads LV2_PATH at in-process World load time, so it must be set
    // on this process's env (an in-process World can't take an override).
    // SAFETY: this is the only test in this binary, so no sibling thread
    // reads or writes the environment concurrently.
    unsafe {
        std::env::set_var("LV2_PATH", bundle.root());
    }

    let block = 64usize;
    let mut plugin = vance_audio::lv2_host::instantiate("urn:vance:test:gain", &[], 48_000.0, block)
        .unwrap_or_else(|| {
            panic!(
                "LV2 host failed to instantiate the staged plugin (LV2_PATH={:?})",
                bundle.root()
            )
        });

    // The fixture is a 0.5x mono gain. The host sums L+R*0.5 → mono in,
    // duplicates mono out → L/R. Feed 1.0 on both channels: mono in =
    // 0.5*(1+1) = 1.0, plugin applies 0.5 → 0.5 out on both.
    let inl = vec![1.0f32; block];
    let mut outl = vec![0.0f32; block];
    let mut outr = vec![0.0f32; block];
    plugin.process_block(&inl, &inl, &mut outl, &mut outr);
    for (i, (&l, &r)) in outl.iter().zip(outr.iter()).enumerate() {
        assert!((l - 0.5).abs() < 1e-3, "L[{i}] expected ~0.5, got {l}");
        assert!((r - 0.5).abs() < 1e-3, "R[{i}] expected ~0.5, got {r}");
    }

    drop(bundle); // keep the bundle alive until processing is done
}
