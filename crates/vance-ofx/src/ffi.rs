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

use std::ffi::{c_char, c_int, c_uint, c_void};

/// Hard cap on how far we'll scan for a NUL terminator when reading a
/// plugin-supplied C string. The strings are populated by untrusted
/// third-party native code (see module docs), so we must NOT do an
/// unbounded `strlen` walk: a non-terminated / dangling buffer would
/// read out of bounds (UB + potential adjacent-memory disclosure).
/// 4 KiB is far longer than any real OFX identifier / API string.
const MAX_CSTR_BYTES: usize = 4096;

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
        // SAFETY: `i` is in `[0, count)`. `get_plugin` is foreign code
        // and its returned pointer is UNTRUSTED — validity cannot be
        // verified in-process. The sidecar boundary is the only
        // containment for a faulting access; a null is rejected here
        // rather than dereferenced.
        let p = unsafe { get_plugin(i) };
        if p.is_null() {
            return Err(OfxFfiError::BadData(format!("null OfxPlugin at index {i}")));
        }
        // Copy the struct out by value with `read_unaligned` rather
        // than forming `&*p`: building a Rust reference to
        // possibly-misaligned / possibly-invalid foreign memory is
        // itself UB with stricter requirements than a raw read. This
        // narrows (but cannot eliminate — only process isolation can)
        // the unsafety from a hostile binary.
        //
        // SAFETY: `p` is non-null; the read is of exactly one
        // `OfxPlugin`. Memory validity is unverifiable in-process; the
        // sidecar contains any fault.
        let plugin: OfxPlugin = unsafe { std::ptr::read_unaligned(p) };
        let api = read_cstr_bounded(plugin.plugin_api)?;
        let identifier = read_cstr_bounded(plugin.plugin_identifier)?;
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

/// Read a NUL-terminated C string from an UNTRUSTED pointer with a
/// hard length bound. Unlike `CStr::from_ptr`, this never does an
/// unbounded `strlen` walk: it scans at most [`MAX_CSTR_BYTES`] bytes
/// and fails closed if no NUL is found. The pointer's memory validity
/// still cannot be checked in-process — the sidecar boundary contains
/// a fault — but the scan is bounded so it can't walk arbitrarily far
/// into adjacent memory.
fn read_cstr_bounded(ptr: *const c_char) -> Result<String, OfxFfiError> {
    if ptr.is_null() {
        return Err(OfxFfiError::BadData("null C string".into()));
    }
    let mut bytes = Vec::new();
    for i in 0..MAX_CSTR_BYTES {
        // SAFETY: single bounded byte read at `ptr + i`, capped by the
        // loop. `c_char` is one byte so `add(i)` advances `i` bytes.
        // Validity is unverifiable; the sidecar contains any fault.
        let b = unsafe { std::ptr::read(ptr.add(i) as *const u8) };
        if b == 0 {
            return Ok(String::from_utf8_lossy(&bytes).into_owned());
        }
        bytes.push(b);
    }
    Err(OfxFfiError::BadData(format!(
        "unterminated C string (exceeded {MAX_CSTR_BYTES}-byte cap)"
    )))
}
