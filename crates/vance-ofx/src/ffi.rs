//! Minimal OpenFX FFI — just enough to *enumerate* the plugins a
//! `.ofx` binary exports.
//!
//! An OFX binary exports two C entry points:
//! - `int OfxGetNumberOfPlugins(void)`
//! - `OfxPlugin* OfxGetPlugin(int nth)`
//!
//! The returned [`OfxPlugin`] struct carries the plugin's API string,
//! identifier, and version as plain readable fields. Enumeration
//! reads those fields directly — it does NOT call `setHost` or
//! `mainEntry`, so no host-side suite implementation is required for
//! the read-only catalog. Running a plugin (the `render` action) is
//! the post-v1 work the PLAN defers.
//!
//! All of this is `unsafe` FFI into third-party native code, which is
//! exactly why the caller runs it inside the sidecar process: a
//! malformed `.ofx` that segfaults on `dlopen` or returns garbage
//! pointers takes down only the sidecar.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};

/// Mirror of the OpenFX `OfxPlugin` struct (ABI-compatible layout).
/// Only the leading fields are read; the two function pointers are
/// kept for layout correctness but never invoked during enumeration.
#[repr(C)]
pub struct OfxPlugin {
    /// API string, e.g. `"OfxImageEffectPluginAPI"`.
    pub plugin_api: *const c_char,
    /// API version (1 or 2 for image effects).
    pub api_version: c_int,
    /// Reverse-DNS-ish plugin identifier.
    pub plugin_identifier: *const c_char,
    /// Major version.
    pub plugin_version_major: c_uint,
    /// Minor version.
    pub plugin_version_minor: c_uint,
    /// `void (*setHost)(OfxHost*)` — not called during enumeration.
    pub set_host: *const c_void,
    /// `OfxStatus (*mainEntry)(...)` — not called during enumeration.
    pub main_entry: *const c_void,
}

/// One enumerated OFX plugin's read-only metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct OfxPluginInfo {
    /// API string (typically `OfxImageEffectPluginAPI`).
    pub api: String,
    /// API version.
    pub api_version: i32,
    /// Plugin identifier.
    pub identifier: String,
    /// Major version.
    pub version_major: u32,
    /// Minor version.
    pub version_minor: u32,
}

/// Errors enumerating a `.ofx` binary.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum OfxFfiError {
    /// `dlopen` / `LoadLibrary` failed.
    #[error("ofx dlopen `{path}`: {source}")]
    Open {
        /// Binary path.
        path: String,
        /// Underlying loader error.
        source: libloading::Error,
    },
    /// A required entry point symbol was missing.
    #[error("ofx binary missing symbol `{0}`")]
    MissingSymbol(&'static str),
    /// The plugin returned an unreadable / null pointer.
    #[error("ofx enumeration: {0}")]
    BadData(String),
}

type GetNumberOfPlugins = unsafe extern "C" fn() -> c_int;
type GetPlugin = unsafe extern "C" fn(c_int) -> *const OfxPlugin;

/// Enumerate the plugins a `.ofx` binary exports. Loads the library,
/// reads each `OfxPlugin`'s identifier/version, and unloads.
///
/// # Safety / isolation
/// This dereferences pointers returned by third-party native code.
/// Run it inside the sidecar process; a crash is then contained.
pub fn enumerate(binary_path: &std::path::Path) -> Result<Vec<OfxPluginInfo>, OfxFfiError> {
    // SAFETY: loading an arbitrary shared library is inherently
    // unsafe; the sidecar process boundary is the containment.
    let lib = unsafe { libloading::Library::new(binary_path) }.map_err(|source| {
        OfxFfiError::Open {
            path: binary_path.display().to_string(),
            source,
        }
    })?;

    let count = unsafe {
        let f: libloading::Symbol<GetNumberOfPlugins> = lib
            .get(b"OfxGetNumberOfPlugins\0")
            .map_err(|_| OfxFfiError::MissingSymbol("OfxGetNumberOfPlugins"))?;
        f()
    };
    if count < 0 || count > 10_000 {
        return Err(OfxFfiError::BadData(format!(
            "implausible plugin count {count}"
        )));
    }

    let get_plugin: libloading::Symbol<GetPlugin> = unsafe {
        lib.get(b"OfxGetPlugin\0")
            .map_err(|_| OfxFfiError::MissingSymbol("OfxGetPlugin"))?
    };

    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count {
        // SAFETY: index in [0, count); the plugin must return a valid
        // pointer per the OFX contract. A null is treated as data
        // error rather than dereferenced.
        let p = unsafe { get_plugin(i) };
        if p.is_null() {
            return Err(OfxFfiError::BadData(format!("null OfxPlugin at index {i}")));
        }
        let plugin = unsafe { &*p };
        let api = read_cstr(plugin.plugin_api)?;
        let identifier = read_cstr(plugin.plugin_identifier)?;
        out.push(OfxPluginInfo {
            api,
            api_version: plugin.api_version as i32,
            identifier,
            version_major: plugin.plugin_version_major,
            version_minor: plugin.plugin_version_minor,
        });
    }
    Ok(out)
}

fn read_cstr(ptr: *const c_char) -> Result<String, OfxFfiError> {
    if ptr.is_null() {
        return Err(OfxFfiError::BadData("null C string".into()));
    }
    // SAFETY: the OFX contract guarantees NUL-terminated static
    // strings for these fields.
    let s = unsafe { CStr::from_ptr(ptr) };
    Ok(s.to_string_lossy().into_owned())
}
