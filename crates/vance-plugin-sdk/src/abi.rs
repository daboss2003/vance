//! The WASM plugin ABI — the contract between the host (`vance-app`)
//! and a guest `.wasm` effect plugin.
//!
//! A plugin is a single `.wasm` module that exports a small set of
//! functions. The host instantiates it inside a sandboxed `wasmtime`
//! store (no WASI, no filesystem, no network — pure compute over a
//! linear memory buffer) and drives frame processing across the
//! boundary.
//!
//! ## Exported functions (guest → host)
//!
//! | name | signature | purpose |
//! |------|-----------|---------|
//! | [`EXPORT_ABI_VERSION`] | `() -> i32` | returns [`ABI_VERSION`]; host refuses mismatches |
//! | [`EXPORT_ALLOC`] | `(len: i32) -> i32` | allocate `len` bytes in guest memory, return ptr |
//! | [`EXPORT_DEALLOC`] | `(ptr: i32, len: i32)` | free a prior [`EXPORT_ALLOC`] |
//! | [`EXPORT_MANIFEST`] | `() -> i64` | return `(ptr << 32) | len` of a RON manifest string |
//! | [`EXPORT_PROCESS`] | `(buf, w, h, params, plen) -> i32` | effect: mutate RGBA in place |
//! | [`EXPORT_PROCESS2`] | `(a, b, w, h, params, plen) -> i32` | transition: blend B into A in place |
//!
//! `buf` / `a` / `b` point at `w * h * 4` bytes of `RGBA8` (row-major,
//! no padding). `params` points at a packed little-endian `f32` array
//! in the order the manifest declares; `plen` is its byte length.
//! Process functions return `0` on success, non-zero on error.
//!
//! The required memory export is the standard `memory`.

/// ABI version. Bump on any breaking change to the function set or
/// memory layout. The host calls [`EXPORT_ABI_VERSION`] and refuses
/// to load a plugin that returns a different value.
pub const ABI_VERSION: i32 = 1;

/// Export name: returns [`ABI_VERSION`].
pub const EXPORT_ABI_VERSION: &str = "vance_abi_version";
/// Export name: allocate guest memory.
pub const EXPORT_ALLOC: &str = "vance_alloc";
/// Export name: free guest memory.
pub const EXPORT_DEALLOC: &str = "vance_dealloc";
/// Export name: return the plugin's RON manifest as a packed
/// `(ptr << 32) | len` `i64`.
pub const EXPORT_MANIFEST: &str = "vance_manifest";
/// Export name: effect process (in-place RGBA mutate).
pub const EXPORT_PROCESS: &str = "vance_process";
/// Export name: transition process (blend B into A).
pub const EXPORT_PROCESS2: &str = "vance_process2";
/// Required memory export name.
pub const EXPORT_MEMORY: &str = "memory";

/// Pack a `(ptr, len)` pair into the single `i64` that
/// [`EXPORT_MANIFEST`] returns. Both must fit in `u32`.
#[inline]
pub fn pack_ptr_len(ptr: u32, len: u32) -> i64 {
    (((ptr as u64) << 32) | (len as u64)) as i64
}

/// Inverse of [`pack_ptr_len`].
#[inline]
pub fn unpack_ptr_len(packed: i64) -> (u32, u32) {
    let p = packed as u64;
    ((p >> 32) as u32, (p & 0xffff_ffff) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_round_trips() {
        for (ptr, len) in [(0u32, 0u32), (1, 2), (0xdead_beef, 0x10), (u32::MAX, u32::MAX)] {
            let packed = pack_ptr_len(ptr, len);
            assert_eq!(unpack_ptr_len(packed), (ptr, len));
        }
    }
}
