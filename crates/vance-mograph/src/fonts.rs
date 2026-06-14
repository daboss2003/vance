//! System font discovery + caching for the title engine.
//!
//! Scans the platform font directories once, indexing each face by its
//! OpenType family name (via `ttf-parser`'s name table), then loads +
//! caches `fontdue` faces on demand. A `Font` control's options come
//! from [`FontDb::families`]; the rig resolves a family name through
//! [`FontDb::face`], falling back to a default when a name is unknown
//! (so a template authored against a font the host lacks still renders).

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A resolved, indexed set of system fonts.
pub struct FontDb {
    /// Lowercased family name → file path.
    by_name: BTreeMap<String, PathBuf>,
    /// Family names in display order (original case).
    display: Vec<String>,
    /// Lazily-loaded face cache, keyed by lowercased family name.
    loaded: Mutex<HashMap<String, Option<Arc<fontdue::Font>>>>,
    /// Family chosen when a requested name isn't found.
    default_key: Option<String>,
}

impl std::fmt::Debug for FontDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FontDb")
            .field("families", &self.display.len())
            .field("default", &self.default_key)
            .finish()
    }
}

impl FontDb {
    /// Build the index by scanning the platform font directories. Cheap
    /// enough to call once at engine construction; safe on hosts with no
    /// fonts (then [`Self::face`] returns `None` and text is skipped).
    pub fn system() -> Self {
        let mut by_name: BTreeMap<String, PathBuf> = BTreeMap::new();
        let mut display: Vec<String> = Vec::new();
        for dir in font_dirs() {
            collect_fonts(&dir, 0, &mut by_name, &mut display);
        }
        display.sort();
        display.dedup();
        // Prefer a sane default if present, else the first family found.
        let default_key = ["arial", "helvetica", "helvetica neue", "dejavu sans", "liberation sans"]
            .iter()
            .map(|s| s.to_string())
            .find(|k| by_name.contains_key(k))
            .or_else(|| by_name.keys().next().cloned());
        FontDb {
            by_name,
            display,
            loaded: Mutex::new(HashMap::new()),
            default_key,
        }
    }

    /// Family names available on this host, sorted, for a `Font` control.
    pub fn families(&self) -> &[String] {
        &self.display
    }

    /// Whether any fonts were found.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Resolve a family name (case-insensitive) to a loaded face, falling
    /// back to the default family. `None` only when the host has no
    /// usable fonts at all.
    pub fn face(&self, family: &str) -> Option<Arc<fontdue::Font>> {
        let key = family.trim().to_lowercase();
        if let Some(f) = self.load(&key) {
            return Some(f);
        }
        // Fallback to the default family.
        match &self.default_key {
            Some(d) => self.load(d),
            None => None,
        }
    }

    fn load(&self, key: &str) -> Option<Arc<fontdue::Font>> {
        let path = self.by_name.get(key)?.clone();
        let mut cache = self.loaded.lock().ok()?;
        if let Some(entry) = cache.get(key) {
            return entry.clone();
        }
        let face = std::fs::read(&path).ok().and_then(|bytes| {
            fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
                .ok()
                .map(Arc::new)
        });
        cache.insert(key.to_string(), face.clone());
        face
    }
}

/// Platform font search roots.
fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/System/Library/Fonts"));
        dirs.push(PathBuf::from("/System/Library/Fonts/Supplemental"));
        dirs.push(PathBuf::from("/Library/Fonts"));
        if let Some(h) = &home {
            dirs.push(h.join("Library/Fonts"));
        }
    } else if cfg!(target_os = "windows") {
        let win = std::env::var_os("WINDIR").map(PathBuf::from);
        dirs.push(win.unwrap_or_else(|| PathBuf::from(r"C:\Windows")).join("Fonts"));
    } else {
        dirs.push(PathBuf::from("/usr/share/fonts"));
        dirs.push(PathBuf::from("/usr/local/share/fonts"));
        if let Some(h) = &home {
            dirs.push(h.join(".fonts"));
            dirs.push(h.join(".local/share/fonts"));
        }
    }
    dirs
}

/// Recursively (bounded depth) index font files under `dir`.
fn collect_fonts(
    dir: &Path,
    depth: u32,
    by_name: &mut BTreeMap<String, PathBuf>,
    display: &mut Vec<String>,
) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_fonts(&path, depth + 1, by_name, display);
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase());
        if !matches!(ext.as_deref(), Some("ttf") | Some("otf") | Some("ttc")) {
            continue;
        }
        if let Some(name) = read_family_name(&path) {
            let key = name.to_lowercase();
            // First face wins for a given family (regular weight usually
            // sorts first); don't clobber with bold/italic variants.
            by_name.entry(key).or_insert_with(|| path.clone());
            display.push(name);
        }
    }
}

/// Read the OpenType family name (preferring the typographic family) of
/// face 0 in a font file.
fn read_family_name(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let face = ttf_parser::Face::parse(&data, 0).ok()?;
    let mut family: Option<String> = None;
    for name in face.names() {
        let is_family = name.name_id == ttf_parser::name_id::FAMILY;
        let is_typo = name.name_id == ttf_parser::name_id::TYPOGRAPHIC_FAMILY;
        if (is_family || is_typo) && name.is_unicode() {
            if let Some(s) = name.to_string() {
                if is_typo {
                    return Some(s); // typographic family is the best key
                }
                family.get_or_insert(s);
            }
        }
    }
    family.filter(|s| !s.trim().is_empty())
}
