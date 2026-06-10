//! Test fixture: a minimal OpenFX binary exporting the two
//! enumeration entry points (`OfxGetNumberOfPlugins` / `OfxGetPlugin`)
//! so `vance-ofx`'s `dlopen` enumeration can be tested without a real
//! third-party OFX plugin installed.
//!
//! It exposes ONE fake image-effect plugin and intentionally does
//! NOT implement `setHost` / `mainEntry` (left null) — enumeration
//! never calls them.

use std::os::raw::{c_char, c_int, c_uint, c_void};

/// ABI-compatible mirror of `OfxPlugin` (see vance-ofx::ffi).
#[repr(C)]
pub struct OfxPlugin {
    plugin_api: *const c_char,
    api_version: c_int,
    plugin_identifier: *const c_char,
    plugin_version_major: c_uint,
    plugin_version_minor: c_uint,
    set_host: *const c_void,
    main_entry: *const c_void,
}

// The struct holds raw pointers into static NUL-terminated strings,
// which are valid for the program's lifetime. Safe to share.
unsafe impl Sync for OfxPlugin {}

static API: &[u8] = b"OfxImageEffectPluginAPI\0";
static IDENT: &[u8] = b"com.vance.test.invert\0";

static PLUGIN: OfxPlugin = OfxPlugin {
    plugin_api: API.as_ptr() as *const c_char,
    api_version: 1,
    plugin_identifier: IDENT.as_ptr() as *const c_char,
    plugin_version_major: 2,
    plugin_version_minor: 5,
    set_host: std::ptr::null(),
    main_entry: std::ptr::null(),
};

/// OFX entry point: number of plugins this binary provides.
#[no_mangle]
pub extern "C" fn OfxGetNumberOfPlugins() -> c_int {
    1
}

/// OFX entry point: return the nth plugin descriptor.
#[no_mangle]
pub extern "C" fn OfxGetPlugin(nth: c_int) -> *const OfxPlugin {
    if nth == 0 {
        &PLUGIN
    } else {
        std::ptr::null()
    }
}
