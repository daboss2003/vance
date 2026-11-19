//! Guest-side helpers for plugin authors.
//!
//! A plugin author writes a small Rust crate (`crate-type =
//! ["cdylib"]`), implements the [`Plugin`] trait, and invokes
//! [`export_plugin!`] to generate the `#[no_mangle]` ABI exports.
//! Compile with:
//!
//! ```bash
//! cargo build --release --target wasm32-unknown-unknown \
//!     -p my-plugin --features vance-plugin-sdk/guest
//! ```
//!
//! The resulting `.wasm` is the single file users drop into their
//! plugins folder.

use crate::manifest::PluginManifest;

/// The trait a guest plugin implements.
pub trait Plugin {
    /// Return the plugin's manifest. Called once at load time.
    fn manifest() -> PluginManifest;

    /// Effect process: mutate `rgba` (length `w * h * 4`) in place.
    /// `params` is the packed `f32` array in manifest declaration
    /// order. Default impl is a no-op so transition-only plugins can
    /// skip it.
    fn process(_rgba: &mut [u8], _w: u32, _h: u32, _params: &[f32]) {}

    /// Transition process: blend `b` into `a` in place. `a` is both
    /// the B-input destination and the output. Default impl is a
    /// no-op so effect-only plugins can skip it.
    fn process_transition(_a: &mut [u8], _b: &[u8], _w: u32, _h: u32, _params: &[f32]) {}
}

/// Allocation helpers — the macro-generated `vance_alloc` /
/// `vance_dealloc` delegate here. Exposed so authors can reuse them
/// in custom glue if they bypass the macro.
pub mod alloc_helpers {
    /// Allocate `len` bytes and leak the pointer to the host. The
    /// host is responsible for a matching [`dealloc`].
    pub fn alloc(len: i32) -> i32 {
        if len <= 0 {
            return 0;
        }
        let mut buf = Vec::<u8>::with_capacity(len as usize);
        let ptr = buf.as_mut_ptr();
        std::mem::forget(buf);
        ptr as i32
    }

    /// Reconstruct + drop a buffer previously returned by [`alloc`].
    ///
    /// # Safety
    /// `ptr`/`len` must come from a prior [`alloc`] with the same
    /// `len` and not have been freed already.
    pub unsafe fn dealloc(ptr: i32, len: i32) {
        if ptr == 0 || len <= 0 {
            return;
        }
        let _ = Vec::from_raw_parts(ptr as *mut u8, 0, len as usize);
    }
}

/// Generate the `#[no_mangle]` ABI exports for a [`Plugin`] impl.
///
/// ```ignore
/// struct MyPlugin;
/// impl vance_plugin_sdk::guest::Plugin for MyPlugin { /* … */ }
/// vance_plugin_sdk::export_plugin!(MyPlugin);
/// ```
#[macro_export]
macro_rules! export_plugin {
    ($ty:ty) => {
        /// Static manifest RON, baked once at first call.
        #[no_mangle]
        pub extern "C" fn vance_abi_version() -> i32 {
            $crate::abi::ABI_VERSION
        }

        #[no_mangle]
        pub extern "C" fn vance_alloc(len: i32) -> i32 {
            $crate::guest::alloc_helpers::alloc(len)
        }

        #[no_mangle]
        pub extern "C" fn vance_dealloc(ptr: i32, len: i32) {
            unsafe { $crate::guest::alloc_helpers::dealloc(ptr, len) }
        }

        #[no_mangle]
        pub extern "C" fn vance_manifest() -> i64 {
            // Serialize the manifest, leak it, hand the host the
            // (ptr, len). The host calls vance_dealloc afterwards.
            let ron = <$ty as $crate::guest::Plugin>::manifest().to_ron();
            let bytes = ron.into_bytes();
            let len = bytes.len() as u32;
            let mut boxed = bytes.into_boxed_slice();
            let ptr = boxed.as_mut_ptr() as u32;
            ::std::mem::forget(boxed);
            $crate::abi::pack_ptr_len(ptr, len)
        }

        #[no_mangle]
        pub extern "C" fn vance_process(buf: i32, w: i32, h: i32, params: i32, plen: i32) -> i32 {
            let pixels = (w as usize) * (h as usize) * 4;
            // SAFETY: the host guarantees `buf` points at `pixels`
            // bytes and `params` at `plen` bytes of valid guest memory.
            let rgba = unsafe { ::std::slice::from_raw_parts_mut(buf as *mut u8, pixels) };
            let param_slice =
                unsafe { ::std::slice::from_raw_parts(params as *const f32, (plen as usize) / 4) };
            <$ty as $crate::guest::Plugin>::process(rgba, w as u32, h as u32, param_slice);
            0
        }

        #[no_mangle]
        pub extern "C" fn vance_process2(
            a: i32,
            b: i32,
            w: i32,
            h: i32,
            params: i32,
            plen: i32,
        ) -> i32 {
            let pixels = (w as usize) * (h as usize) * 4;
            let a_rgba = unsafe { ::std::slice::from_raw_parts_mut(a as *mut u8, pixels) };
            let b_rgba = unsafe { ::std::slice::from_raw_parts(b as *const u8, pixels) };
            let param_slice =
                unsafe { ::std::slice::from_raw_parts(params as *const f32, (plen as usize) / 4) };
            <$ty as $crate::guest::Plugin>::process_transition(
                a_rgba,
                b_rgba,
                w as u32,
                h as u32,
                param_slice,
            );
            0
        }
    };
}
