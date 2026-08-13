//! Phase 10.3 — plugin discovery.
//!
//! Scans a set of directories for `.wasm` files, loads each one's
//! manifest through the sandboxed host, and builds a registry the UI
//! can browse. A plugin that fails to load (corrupt wasm, ABI
//! mismatch, bad manifest) is skipped with a warning so one bad file
//! never breaks discovery of the rest.
//!
//! ## Precedence
//!
//! De-dup is first-seen-wins by manifest id, and roots are scanned in
//! the order given. So **earlier roots win** on an id conflict —
//! callers that want project-local plugins to override a globally-
//! installed plugin of the same id must pass the project-local root
//! BEFORE the user-global one (see `Engine::rescan_plugins`).

use crate::host::WasmPlugin;
use crate::manifest::PluginManifest;
use std::path::{Path, PathBuf};

/// One discovered plugin: where it lives + its manifest.
#[derive(Clone, Debug)]
pub struct DiscoveredPlugin {
    /// Absolute path to the `.wasm` (plaintext) or `.vef` (encrypted) file.
    pub path: PathBuf,
    /// Parsed + validated manifest. For a `.vef`, read from its plaintext header
    /// without decrypting the body.
    pub manifest: PluginManifest,
    /// True for a `.vef` paid effect: the body is encrypted and the host must
    /// decrypt it (via the entitlement gate) at load. False for a plain `.wasm`.
    pub encrypted: bool,
    /// True for a `.vsfx` shader effect (M6): the manifest here is plaintext and
    /// the app compiles the WGSL body via `vance-effects` at load.
    pub shader: bool,
}

/// Registry of discovered plugins, de-duplicated by manifest id.
#[derive(Clone, Debug, Default)]
pub struct PluginRegistry {
    plugins: Vec<DiscoveredPlugin>,
}

impl PluginRegistry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scan every directory in `roots` (non-recursive within each, but
    /// each root is walked one level deep). Returns a fresh registry.
    /// Missing directories are silently skipped.
    pub fn scan(roots: &[PathBuf]) -> Self {
        let mut reg = Self::new();
        for root in roots {
            reg.scan_dir(root);
        }
        reg
    }

    /// Scan one directory, adding any loadable plugins. Public so the
    /// engine can incrementally add a newly-created plugins folder.
    ///
    /// Entries are sorted by path before loading so that, when two
    /// `.wasm` files in the SAME directory carry the same manifest id,
    /// first-seen-wins de-dup is deterministic across runs +
    /// platforms (`read_dir` order is otherwise unspecified).
    pub fn scan_dir(&mut self, dir: &Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return, // missing dir is fine
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("wasm")
                        | Some(crate::container::VEF_EXTENSION)
                        | Some(crate::shaderpkg::SHADER_EXTENSION)
                )
            })
            .collect();
        paths.sort();
        for path in paths {
            let ext = path.extension().and_then(|e| e.to_str());
            let encrypted = ext == Some(crate::container::VEF_EXTENSION);
            let shader = ext == Some(crate::shaderpkg::SHADER_EXTENSION);
            let loaded = if shader {
                std::fs::read(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|b| crate::shaderpkg::parse(&b).map(|p| p.manifest))
            } else if encrypted {
                std::fs::read(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|b| crate::container::parse_manifest(&b).map_err(|e| e.to_string()))
            } else {
                load_manifest(&path).map_err(|e| e.to_string())
            };
            match loaded {
                Ok(manifest) => self.add(DiscoveredPlugin {
                    path,
                    manifest,
                    encrypted,
                    shader,
                }),
                Err(e) => {
                    tracing::warn!(?path, error = %e, "skipping unloadable plugin");
                }
            }
        }
    }

    /// Insert, de-duplicating by manifest id. First-seen wins; a later
    /// duplicate is logged + dropped so the user's search-root order
    /// (project-local before global, say) is honoured deterministically.
    fn add(&mut self, plugin: DiscoveredPlugin) {
        if self.plugins.iter().any(|p| p.manifest.id == plugin.manifest.id) {
            tracing::info!(
                id = %plugin.manifest.id,
                path = ?plugin.path,
                "duplicate plugin id; keeping first-seen"
            );
            return;
        }
        self.plugins.push(plugin);
    }

    /// All discovered plugins.
    pub fn plugins(&self) -> &[DiscoveredPlugin] {
        &self.plugins
    }

    /// Look up a discovered plugin by manifest id.
    pub fn by_id(&self, id: &str) -> Option<&DiscoveredPlugin> {
        self.plugins.iter().find(|p| p.manifest.id == id)
    }

    /// Number of plugins.
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

/// Load just a plugin's manifest (instantiates, reads manifest, drops
/// the instance). Cheap enough to run across a folder at startup.
pub fn load_manifest(path: &Path) -> Result<PluginManifest, crate::host::PluginError> {
    let bytes = std::fs::read(path)
        .map_err(|e| crate::host::PluginError::Load(format!("read {path:?}: {e}")))?;
    let plugin = WasmPlugin::load(&bytes)?;
    Ok(plugin.manifest().clone())
}

/// The default user-global plugins directory: `~/.vance/plugins/`.
/// Returns `None` if the home directory can't be resolved.
pub fn user_plugins_dir() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".vance").join("plugins"))
}

/// Resolve the user's home directory without pulling an extra crate.
/// Checks `$HOME` (unix) then `$USERPROFILE` (windows).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_missing_dir_is_empty() {
        let reg = PluginRegistry::scan(&[PathBuf::from("/nonexistent/vance/plugins")]);
        assert!(reg.is_empty());
    }

    #[test]
    fn scan_finds_sample_wasm_if_built() {
        // Use the real sample plugins if they've been built.
        let stylize = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/stylize/target/wasm32-unknown-unknown/release");
        if !stylize.join("vance_plugin_stylize.wasm").exists() {
            eprintln!("sample wasm not built — skipping discovery scan test");
            return;
        }
        let reg = PluginRegistry::scan(&[stylize]);
        assert!(!reg.is_empty());
        assert!(reg.by_id("com.vance.sample.stylize").is_some());
    }

    #[test]
    fn user_plugins_dir_under_home() {
        // With HOME set, the path ends in .vance/plugins.
        if let Some(dir) = user_plugins_dir() {
            assert!(dir.ends_with("plugins"));
            assert!(dir.to_string_lossy().contains(".vance"));
        }
    }
}
