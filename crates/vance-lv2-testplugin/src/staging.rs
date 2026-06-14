//! Test-only helper: stage the compiled plugin dylib as a real `.lv2`
//! bundle under a unique temp dir, returning an RAII guard that removes
//! the bundle tree on drop (so a panicking test doesn't leak it).
//!
//! Shared by every LV2 host test (in-process and sidecar) so the bundle
//! `.ttl` lives in exactly one place.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::PLUGIN_URI;

/// Disambiguates concurrent stagings within one test binary (where
/// `process::id()` is constant), so two stagings never share a temp root.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A staged `.lv2` bundle. Removes the whole bundle tree on drop — so a
/// test that panics mid-way doesn't leak it under the temp dir.
pub struct BundleGuard {
    root: PathBuf,
}

impl BundleGuard {
    /// The directory to put on `LV2_PATH` (the bundle's parent).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `LV2_PATH` value as an owned string (convenience for env setup).
    pub fn lv2_path(&self) -> String {
        self.root.to_string_lossy().into_owned()
    }
}

impl Drop for BundleGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Write a `.lv2` bundle for the gain fixture under a fresh temp dir.
/// `dylib` is the compiled `libvance_lv2_testplugin` artifact (located by
/// the caller in the workspace target dir). The bundle declares two audio
/// ports plus an (ignored) atom-sequence input port, so the host's atom/CV
/// port wiring is exercised. Returns a guard whose [`BundleGuard::root`]
/// belongs on `LV2_PATH`.
pub fn stage_bundle(dylib: &Path) -> std::io::Result<BundleGuard> {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("vance-lv2-{}-{}", std::process::id(), n));
    let _ = std::fs::remove_dir_all(&root);
    let bundle = root.join("vance-test.lv2");
    std::fs::create_dir_all(&bundle)?;
    let dy = dylib
        .file_name()
        .expect("dylib path has a file name")
        .to_string_lossy()
        .into_owned();
    std::fs::copy(dylib, bundle.join(&dy))?;
    std::fs::write(bundle.join("manifest.ttl"), manifest_ttl(&dy))?;
    std::fs::write(bundle.join("vance-test.ttl"), plugin_ttl())?;
    Ok(BundleGuard { root })
}

fn manifest_ttl(dylib_name: &str) -> String {
    format!(
        "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         <{PLUGIN_URI}>\n\ta lv2:Plugin ;\n\tlv2:binary <{dylib_name}> ;\n\trdfs:seeAlso <vance-test.ttl> .\n"
    )
}

fn plugin_ttl() -> String {
    format!(
        "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n\
         @prefix atom: <http://lv2plug.in/ns/ext/atom#> .\n\
         @prefix doap: <http://usefulinc.com/ns/doap#> .\n\
         <{PLUGIN_URI}>\n\ta lv2:Plugin ;\n\tdoap:name \"Vance Test Gain\" ;\n\
         \tlv2:port [\n\t\ta lv2:AudioPort, lv2:InputPort ;\n\t\tlv2:index 0 ; lv2:symbol \"in\" ; lv2:name \"In\"\n\
         \t] , [\n\t\ta lv2:AudioPort, lv2:OutputPort ;\n\t\tlv2:index 1 ; lv2:symbol \"out\" ; lv2:name \"Out\"\n\
         \t] , [\n\t\ta atom:AtomPort, lv2:InputPort ;\n\t\tatom:bufferType atom:Sequence ;\n\t\tlv2:index 2 ; lv2:symbol \"control\" ; lv2:name \"Control\"\n\t] .\n"
    )
}
