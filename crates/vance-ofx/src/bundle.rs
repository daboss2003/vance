//! OFX bundle discovery.
//!
//! OFX plugins ship as `Foo.ofx.bundle/` directories with a
//! platform-specific binary at
//! `Contents/<arch>/Foo.ofx`. This module walks the standard OFX
//! search paths, finds bundles, and resolves each bundle's binary for
//! the current platform.
//!
//! Search paths (OFX spec §"Installation Location"):
//! - `$OFX_PLUGIN_PATH` (platform path-list separated).
//! - macOS: `/Library/OFX/Plugins`, `~/Library/OFX/Plugins`.
//! - Linux: `/usr/OFX/Plugins`, `/usr/local/OFX/Plugins`.
//! - Windows: `%CommonProgramFiles%\OFX\Plugins`.

use std::path::{Path, PathBuf};

/// One discovered OFX bundle + its loadable binary for this platform.
#[derive(Clone, Debug, PartialEq)]
pub struct OfxBundle {
    /// Bundle directory name, without the `.ofx.bundle` suffix.
    pub name: String,
    /// Path to the `.ofx.bundle` directory.
    pub bundle_path: PathBuf,
    /// Path to the loadable `.ofx` binary inside the bundle, if one
    /// exists for this platform's architecture.
    pub binary_path: Option<PathBuf>,
}

/// The platform-specific `Contents/<arch>` subdirectory names that may
/// hold the binary, in preference order. We try several because
/// vendors aren't always consistent about the arch suffix.
fn arch_subdirs() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &["MacOS", "MacOS-x86-64", "MacOS-arm-64"]
    }
    #[cfg(target_os = "linux")]
    {
        &["Linux-x86-64", "Linux-x86-64-debug", "Linux-x86"]
    }
    #[cfg(target_os = "windows")]
    {
        &["Win64", "Win32"]
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        &[]
    }
}

/// The standard OFX search roots for this platform, plus any from
/// `$OFX_PLUGIN_PATH`.
pub fn default_search_paths() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Some(env) = std::env::var_os("OFX_PLUGIN_PATH") {
        for p in std::env::split_paths(&env) {
            roots.push(p);
        }
    }

    #[cfg(target_os = "macos")]
    {
        roots.push(PathBuf::from("/Library/OFX/Plugins"));
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(PathBuf::from(home).join("Library/OFX/Plugins"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        roots.push(PathBuf::from("/usr/OFX/Plugins"));
        roots.push(PathBuf::from("/usr/local/OFX/Plugins"));
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(cf) = std::env::var_os("CommonProgramFiles") {
            roots.push(PathBuf::from(cf).join("OFX").join("Plugins"));
        }
    }
    roots
}

/// Scan `roots` for `*.ofx.bundle` directories. Missing roots are
/// skipped silently.
pub fn discover(roots: &[PathBuf]) -> Vec<OfxBundle> {
    let mut bundles = Vec::new();
    for root in roots {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !is_bundle_dir(&path) {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.trim_end_matches(".ofx.bundle").to_string())
                .unwrap_or_default();
            let binary_path = resolve_binary(&path, &name);
            bundles.push(OfxBundle {
                name,
                bundle_path: path,
                binary_path,
            });
        }
    }
    bundles
}

fn is_bundle_dir(path: &Path) -> bool {
    path.is_dir()
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(".ofx.bundle"))
            .unwrap_or(false)
}

/// Resolve a bundle's binary by trying each platform arch subdir for a
/// `<name>.ofx` (or any `*.ofx`) file.
fn resolve_binary(bundle_path: &Path, name: &str) -> Option<PathBuf> {
    let contents = bundle_path.join("Contents");
    for arch in arch_subdirs() {
        let dir = contents.join(arch);
        // Preferred: `<name>.ofx`.
        let preferred = dir.join(format!("{name}.ofx"));
        if preferred.is_file() {
            return Some(preferred);
        }
        // Fallback: first `*.ofx` in the dir.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("ofx") {
                    return Some(p);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_missing_root_is_empty() {
        let v = discover(&[PathBuf::from("/nonexistent/ofx/root")]);
        assert!(v.is_empty());
    }

    #[test]
    fn discovers_synthetic_bundle_structure() {
        let tmp = std::env::temp_dir().join(format!("vance-ofx-test-{}", std::process::id()));
        let arch = arch_subdirs().first().copied().unwrap_or("MacOS");
        let bin_dir = tmp
            .join("Plugins")
            .join("Foo.ofx.bundle")
            .join("Contents")
            .join(arch);
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join("Foo.ofx"), b"not a real binary").unwrap();

        let found = discover(&[tmp.join("Plugins")]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Foo");
        assert!(found[0].binary_path.is_some());

        std::fs::remove_dir_all(&tmp).ok();
    }
}
