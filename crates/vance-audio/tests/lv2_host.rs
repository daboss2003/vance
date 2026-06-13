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

/// Build a `.lv2` bundle under a temp dir and return (bundle_parent, dylib_name).
fn stage_bundle(dylib: &PathBuf) -> std::io::Result<PathBuf> {
    let root = std::env::temp_dir().join(format!("vance-lv2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let bundle = root.join("vance-test.lv2");
    std::fs::create_dir_all(&bundle)?;
    let dylib_name = dylib.file_name().unwrap().to_string_lossy().into_owned();
    std::fs::copy(dylib, bundle.join(&dylib_name))?;
    std::fs::write(
        bundle.join("manifest.ttl"),
        format!(
            "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n\
             @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
             <urn:vance:test:gain>\n\
             \ta lv2:Plugin ;\n\
             \tlv2:binary <{dylib_name}> ;\n\
             \trdfs:seeAlso <vance-test.ttl> .\n"
        ),
    )?;
    std::fs::write(
        bundle.join("vance-test.ttl"),
        "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n\
         @prefix doap: <http://usefulinc.com/ns/doap#> .\n\
         <urn:vance:test:gain>\n\
         \ta lv2:Plugin ;\n\
         \tdoap:name \"Vance Test Gain\" ;\n\
         \tlv2:port [\n\
         \t\ta lv2:AudioPort, lv2:InputPort ;\n\
         \t\tlv2:index 0 ; lv2:symbol \"in\" ; lv2:name \"In\"\n\
         \t] , [\n\
         \t\ta lv2:AudioPort, lv2:OutputPort ;\n\
         \t\tlv2:index 1 ; lv2:symbol \"out\" ; lv2:name \"Out\"\n\
         \t] .\n",
    )?;
    Ok(root)
}

#[test]
fn loads_and_processes_real_lv2_plugin() {
    let Some(dylib) = fixture_dylib() else {
        eprintln!("lv2 test fixture not built — skipping");
        return;
    };
    let root = stage_bundle(&dylib).expect("stage .lv2 bundle");
    // lilv reads LV2_PATH at World load time.
    // SAFETY: single-threaded test setup; no other thread reads the env.
    unsafe {
        std::env::set_var("LV2_PATH", &root);
    }

    let block = 64usize;
    let mut plugin =
        match vance_audio::lv2_host::instantiate("urn:vance:test:gain", &[], 48_000.0, block) {
            Some(p) => p,
            None => {
                let _ = std::fs::remove_dir_all(&root);
                panic!("LV2 host failed to instantiate the staged plugin (LV2_PATH={root:?})");
            }
        };

    // The fixture is a 0.5x mono gain. The host sums L+R*0.5 → mono in,
    // duplicates mono out → L/R. Feed 1.0 on both channels: mono in =
    // 0.5*(1+1) = 1.0, plugin applies 0.5 → 0.5 out on both.
    let inl = vec![1.0f32; block];
    let mut outl = vec![0.0f32; block];
    let mut outr = vec![0.0f32; block];
    use vance_audio::audio_plugin::AudioPlugin;
    plugin.process_block(&inl, &inl, &mut outl, &mut outr);
    for (i, (&l, &r)) in outl.iter().zip(outr.iter()).enumerate() {
        assert!((l - 0.5).abs() < 1e-3, "L[{i}] expected ~0.5, got {l}");
        assert!((r - 0.5).abs() < 1e-3, "R[{i}] expected ~0.5, got {r}");
    }

    let _ = std::fs::remove_dir_all(&root);
}
