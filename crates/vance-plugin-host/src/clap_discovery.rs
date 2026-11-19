//! CLAP plugin discovery.
//!
//! Scans the platform-standard CLAP search paths for `.clap` bundles,
//! then (when the `clap` cargo feature is enabled) loads each bundle via
//! `clack-host` and enumerates every plugin it exposes. A single `.clap`
//! bundle may publish more than one plugin, so each enumerated plugin is
//! surfaced as its own [`PluginDescriptor`] with a composite id of the
//! form `"<bundle-path>::<clap-plugin-id>"`. The in-process CLAP host
//! ([`vance_audio::clap_host`]) parses that id back into a bundle path +
//! a specific plugin to instantiate.
//!
//! Directory scanning is pure filesystem work and always compiled (so it
//! is unit-testable without any plugin installed). The bundle-loading
//! half is feature-gated behind `clap`; without the feature, [`discover`]
//! still scans + reports bundle paths so the FX browser can show what is
//! installed, but enumeration of the plugins inside is skipped.

use std::path::{Path, PathBuf};
use vance_audio_plugins::{PluginDescriptor, PluginFormat};

/// How deep the scanner recurses into a search root. The CLAP spec
/// permits vendors to nest bundles in sub-directories; a small bound
/// keeps a pathological symlink farm from hanging discovery.
const MAX_SCAN_DEPTH: usize = 8;

/// Upper bound on directory entries examined across an entire scan. A
/// hostile or accidentally-enormous tree under a CLAP search root can't
/// stall the (synchronous) `ListPlugins` response past this.
const MAX_SCAN_ENTRIES: usize = 200_000;

/// Upper bound on reported `.clap` bundles. No real machine has anywhere
/// near this many; it's purely a runaway backstop.
const MAX_BUNDLES: usize = 20_000;

/// The platform-standard CLAP search directories, plus any extra paths
/// from the `CLAP_PATH` environment variable. Order is: user paths
/// first, then system paths — matching how DAWs resolve duplicates
/// (user override wins). Missing directories are returned anyway; the
/// scanner skips ones that don't exist.
pub fn default_clap_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();

    // `CLAP_PATH` (env override) is consulted first so power users can
    // point at a private build tree. Separator matches the platform.
    if let Ok(raw) = std::env::var("CLAP_PATH") {
        let sep = if cfg!(windows) { ';' } else { ':' };
        for part in raw.split(sep).filter(|s| !s.is_empty()) {
            dirs.push(PathBuf::from(part));
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = home_dir() {
            dirs.push(home.join("Library/Audio/Plug-Ins/CLAP"));
        }
        dirs.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Programs/Common/CLAP"));
        }
        if let Ok(common) = std::env::var("COMMONPROGRAMFILES") {
            dirs.push(PathBuf::from(common).join("CLAP"));
        }
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(home) = home_dir() {
            dirs.push(home.join(".clap"));
        }
        dirs.push(PathBuf::from("/usr/lib/clap"));
        dirs.push(PathBuf::from("/usr/local/lib/clap"));
    }

    // De-duplicate while preserving order (CLAP_PATH may overlap a
    // standard dir).
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// The current user's home directory, or `None` if it can't be
/// determined. Avoids pulling in the `dirs` crate for one lookup.
fn home_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    } else {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Recursively collect every `.clap` bundle under the given roots. A
/// `.clap` entry is reported whether it is a bundle directory (macOS) or
/// a bare shared library (Linux/Windows); the loader handles both. Once
/// a `.clap` is found the scanner does not descend into it.
///
/// Symlinked directories ARE followed (vendors commonly symlink a CLAP
/// folder into the standard search path), but every directory is
/// canonicalized and recorded before recursion so a symlink cycle can't
/// loop forever. Depth, total entries, and total bundles are all bounded.
pub fn scan_bundles(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut st = ScanState::default();
    for root in roots {
        scan_dir(root, 0, &mut st);
    }
    if st.budget_hit {
        tracing::warn!(
            entries = st.entries_seen,
            bundles = st.out.len(),
            "CLAP scan hit a resource budget — results may be incomplete"
        );
    }
    st.out
}

#[derive(Default)]
struct ScanState {
    out: Vec<PathBuf>,
    /// Canonical paths of bundles already reported (cross-root dedup).
    seen_bundles: std::collections::HashSet<PathBuf>,
    /// Canonical paths of directories already descended (cycle guard).
    visited_dirs: std::collections::HashSet<PathBuf>,
    entries_seen: usize,
    budget_hit: bool,
}

impl ScanState {
    fn over_budget(&mut self) -> bool {
        if self.entries_seen >= MAX_SCAN_ENTRIES || self.out.len() >= MAX_BUNDLES {
            self.budget_hit = true;
        }
        self.budget_hit
    }
}

fn scan_dir(dir: &Path, depth: usize, st: &mut ScanState) {
    if depth > MAX_SCAN_DEPTH || st.over_budget() {
        return;
    }
    // Record this directory by canonical path BEFORE recursing so a
    // self-referential / mutually-referential symlink can't loop.
    let canon_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if !st.visited_dirs.insert(canon_dir) {
        return; // already walked (cycle or duplicate root)
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // missing / unreadable dir → silently skip
    };
    for entry in entries.flatten() {
        if st.over_budget() {
            return;
        }
        st.entries_seen += 1;
        let path = entry.path();
        let is_clap = path
            .extension()
            .map(|e| e.eq_ignore_ascii_case("clap"))
            .unwrap_or(false);
        if is_clap {
            // Canonicalize so the same bundle reached via two roots /
            // symlinks is reported once.
            let key = path.canonicalize().unwrap_or_else(|_| path.clone());
            if st.seen_bundles.insert(key) {
                st.out.push(path);
            }
            continue; // never descend into a bundle
        }
        // Follow symlinked directories too (real vendor layouts use
        // them). `is_dir()` follows symlinks; the canonical visited-set
        // above is what actually breaks cycles.
        if path.is_dir() {
            scan_dir(&path, depth + 1, st);
        }
    }
}

/// The hidden CLI flag the sidecar passes to a copy of itself to probe a
/// single `.clap` bundle out-of-process. See [`probe_main`].
pub const PROBE_FLAG: &str = "--vance-probe-clap";

/// Per-bundle probe timeout. A plugin whose entry init deadlocks can't
/// stall the whole discovery pass past this.
#[cfg(feature = "clap")]
const PROBE_TIMEOUT_SECS: u64 = 15;

/// Discover every installed CLAP plugin. Scans the standard search
/// paths, then enumerates each bundle. With the `clap` feature off, each
/// bundle is reported as a single non-runnable descriptor (so the user
/// can see what is installed and why it isn't usable without the
/// feature); with the feature on, every plugin inside every bundle is
/// enumerated and marked runnable.
///
/// With the `clap` feature on, each bundle is loaded in a SHORT-LIVED
/// CHILD PROCESS (a copy of this binary re-invoked with [`PROBE_FLAG`]).
/// Loading a `.clap` runs untrusted native entry code via `dlopen`; a
/// malformed or hostile bundle that panics (no-op to catch under the
/// release `panic = "abort"`), calls `abort()`, or segfaults then kills
/// only its probe child — the parent records the failure, skips that
/// bundle, and keeps going, so one bad plugin can never wipe the whole
/// plugin list.
pub fn discover() -> Vec<PluginDescriptor> {
    let bundles = scan_bundles(&default_clap_dirs());
    discover_bundles(&bundles)
}

#[cfg(feature = "clap")]
fn discover_bundles(bundles: &[PathBuf]) -> Vec<PluginDescriptor> {
    match std::env::current_exe() {
        Ok(exe) => discover_bundles_with_exe(&exe, bundles),
        Err(e) => {
            // Can't re-invoke ourselves; fall back to in-process
            // enumeration (less crash-safe, but better than nothing).
            tracing::warn!(?e, "current_exe failed — CLAP probes run in-process");
            bundles
                .iter()
                .flat_map(|b| enumerate_bundle_inproc(b))
                .collect()
        }
    }
}

/// Probe every bundle out-of-process using `exe` (a copy of the sidecar
/// binary) as the probe child. Exposed so tests can drive the real
/// subprocess path against the built binary rather than the test runner.
#[cfg(feature = "clap")]
pub fn discover_bundles_with_exe(exe: &Path, bundles: &[PathBuf]) -> Vec<PluginDescriptor> {
    let mut out = Vec::new();
    for bundle in bundles {
        out.extend(probe_bundle_subprocess(exe, bundle));
    }
    out
}

#[cfg(not(feature = "clap"))]
fn discover_bundles(bundles: &[PathBuf]) -> Vec<PluginDescriptor> {
    // No host compiled in → nothing dangerous to load; enumerate
    // directly (each is just a path→non-runnable descriptor).
    bundles
        .iter()
        .flat_map(|b| enumerate_bundle_inproc(b))
        .collect()
}

/// Load one bundle in a child process and collect its descriptors. Any
/// crash / timeout / non-JSON output → empty (bundle skipped).
#[cfg(feature = "clap")]
fn probe_bundle_subprocess(exe: &Path, bundle: &Path) -> Vec<PluginDescriptor> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;
    use std::time::Duration;

    let mut child = match Command::new(exe)
        .arg(PROBE_FLAG)
        .arg(bundle)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()) // discard the child's own tracing noise
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(?e, path = %bundle.display(), "failed to spawn CLAP probe");
            return Vec::new();
        }
    };

    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Vec::new();
    };
    // Read stdout to EOF on a helper thread so a child that fills the
    // pipe can't deadlock us, and so we can time out a hung child.
    let (tx, rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let buf = match rx.recv_timeout(Duration::from_secs(PROBE_TIMEOUT_SECS)) {
        Ok(buf) => {
            let _ = child.wait();
            buf
        }
        Err(_) => {
            tracing::warn!(path = %bundle.display(), "CLAP probe timed out — bundle skipped");
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Vec::new();
        }
    };
    let _ = reader.join();

    // On success the child prints a JSON array of descriptors. On a
    // crash (segfault / abort / panic-under-abort) stdout is empty or
    // truncated and the parse fails → skip the bundle.
    match serde_json::from_slice::<Vec<PluginDescriptor>>(&buf) {
        Ok(descs) => descs,
        Err(_) => {
            tracing::warn!(
                path = %bundle.display(),
                "CLAP probe produced no valid output — bundle skipped (likely crashed on load)"
            );
            Vec::new()
        }
    }
}

/// Child-process entry point: enumerate ONE bundle and print its
/// descriptors as JSON to stdout, then exit. Called from `main` when the
/// binary is re-invoked with [`PROBE_FLAG`]. Always exits the process.
pub fn probe_main(bundle: &Path) -> ! {
    let descs = enumerate_bundle_inproc(bundle);
    // Even if serialization somehow failed, emit a valid empty array so
    // the parent's parse is deterministic.
    let json = serde_json::to_string(&descs).unwrap_or_else(|_| "[]".to_string());
    println!("{json}");
    std::process::exit(0);
}

/// Enumerate the plugins inside a single `.clap` bundle, IN THIS PROCESS.
/// Runs inside the probe child (where a crash is contained) — never call
/// this directly from the long-lived sidecar with the `clap` feature on.
#[cfg(feature = "clap")]
pub fn enumerate_bundle_inproc(path: &Path) -> Vec<PluginDescriptor> {
    use clack_host::prelude::*;

    let path_str = path.to_string_lossy().into_owned();
    // catch_unwind helps only under `panic = "unwind"` (dev); the real
    // crash containment is the parent's subprocess boundary.
    let scanned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let entry = unsafe { PluginEntry::load(&path_str) }.ok()?;
        let factory = entry.get_plugin_factory()?;
        let mut found = Vec::new();
        for desc in factory.plugin_descriptors() {
            let Some(pid) = desc.id().and_then(|c| c.to_str().ok()) else {
                continue; // a plugin with no id can't be instantiated
            };
            let name = desc
                .name()
                .and_then(|c| c.to_str().ok())
                .filter(|s| !s.is_empty())
                .unwrap_or(pid)
                .to_string();
            let vendor = desc
                .vendor()
                .and_then(|c| c.to_str().ok())
                .unwrap_or("")
                .to_string();
            found.push(PluginDescriptor {
                format: PluginFormat::Clap,
                id: format!("{path_str}::{pid}"),
                name,
                vendor,
                runnable: true,
            });
        }
        Some(found)
    }));
    match scanned {
        Ok(Some(found)) => found,
        Ok(None) => {
            tracing::warn!(path = %path_str, "CLAP bundle exposed no plugin factory");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!(path = %path_str, "CLAP bundle panicked during enumeration — skipped");
            Vec::new()
        }
    }
}

/// Without the `clap` feature there is no host to enumerate the bundle's
/// contents, so report the bundle itself as a single, non-runnable
/// descriptor. This still lets the UI surface "you have N CLAP plugins
/// installed, rebuild with --features clap to use them."
#[cfg(not(feature = "clap"))]
pub fn enumerate_bundle_inproc(path: &Path) -> Vec<PluginDescriptor> {
    let path_str = path.to_string_lossy().into_owned();
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path_str.clone());
    vec![PluginDescriptor {
        format: PluginFormat::Clap,
        id: path_str,
        name,
        vendor: String::new(),
        runnable: false,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_clap_bundles_and_skips_others() {
        // Build a temp tree:  root/A.clap  root/nested/B.clap  root/c.txt
        let base = std::env::temp_dir().join(format!(
            "vance-clap-scan-{}-{}",
            std::process::id(),
            // a per-test salt so parallel runs don't collide
            "t1"
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("nested")).unwrap();
        // macOS bundles are directories; emulate with a dir that ends
        // in `.clap` plus a bare-file `.clap` to cover both shapes.
        std::fs::create_dir_all(base.join("A.clap/Contents/MacOS")).unwrap();
        std::fs::write(base.join("nested/B.clap"), b"not a real lib").unwrap();
        std::fs::write(base.join("c.txt"), b"ignore me").unwrap();

        let found = scan_bundles(std::slice::from_ref(&base));
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"A.clap".to_string()), "got {names:?}");
        assert!(names.contains(&"B.clap".to_string()), "got {names:?}");
        assert_eq!(found.len(), 2, "should find exactly the two .clap entries");
        // Must NOT descend into A.clap (no Contents listed as a bundle).
        assert!(!names.iter().any(|n| n == "MacOS"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_dedups_and_tolerates_missing_dirs() {
        let missing = std::env::temp_dir().join("vance-definitely-missing-xyz");
        // A missing root must not panic and yields nothing.
        assert!(scan_bundles(&[missing]).is_empty());
    }

    #[test]
    fn default_dirs_are_nonempty_and_unique() {
        let dirs = default_clap_dirs();
        assert!(!dirs.is_empty(), "expected at least one standard CLAP dir");
        let mut seen = std::collections::HashSet::new();
        for d in &dirs {
            assert!(seen.insert(d), "duplicate dir {d:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn scan_follows_symlinked_dirs_without_looping() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("vance-clap-sym-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        // real/Vendor.clap  +  link/ -> real/   +  a self-referential
        // loop  +  a parent-pointing loop, all under `base`.
        std::fs::create_dir_all(base.join("real")).unwrap();
        std::fs::write(base.join("real/Vendor.clap"), b"x").unwrap();
        symlink(base.join("real"), base.join("link")).unwrap();
        // self-loop: loop -> loop  (must not hang)
        symlink(base.join("loop"), base.join("loop")).ok();
        // parent-loop: deep/up -> base  (must not hang / re-report)
        std::fs::create_dir_all(base.join("deep")).unwrap();
        symlink(&base, base.join("deep/up")).unwrap();

        let found = scan_bundles(std::slice::from_ref(&base));
        // The bundle is discovered (via real/ and/or link/) but the
        // canonical dedup means it is reported exactly once.
        let clap_count = found
            .iter()
            .filter(|p| p.file_name().map(|n| n == "Vendor.clap").unwrap_or(false))
            .count();
        assert_eq!(
            clap_count, 1,
            "symlinked bundle should be found exactly once: {found:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(not(feature = "clap"))]
    #[test]
    fn without_feature_bundles_are_non_runnable() {
        let base = std::env::temp_dir().join(format!("vance-clap-nofeat-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("Synth.clap"), b"x").unwrap();
        let descs: Vec<_> = scan_bundles(std::slice::from_ref(&base))
            .iter()
            .flat_map(|p| enumerate_bundle_inproc(p))
            .collect();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].name, "Synth");
        assert!(!descs[0].runnable, "no clap feature → not runnable");
        let _ = std::fs::remove_dir_all(&base);
    }
}
