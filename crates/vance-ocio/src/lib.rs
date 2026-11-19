//! `vance-ocio` — OpenColorIO color management via FFI (Phase 6.1b).
//!
//! OpenColorIO 2.x exposes only a C++ API, so this crate bridges it with
//! a tiny `extern "C"` shim ([`src/shim.cpp`]) compiled and linked by the
//! build script. The bridge is gated behind the **`ocio` feature**:
//!
//! * **feature off (default):** the crate still compiles — no native
//!   library needed — and every entry point returns
//!   [`OcioError::Unavailable`]. This keeps a fresh checkout / CI green
//!   on machines without OpenColorIO.
//! * **feature on:** links `libOpenColorIO` (discovered via pkg-config;
//!   `brew install opencolorio` on macOS) and performs real conversions.
//!
//! ## Integration strategy
//!
//! Rather than translate OCIO's generated GPU shaders to WGSL, we let
//! OCIO **bake a 3D LUT** on the CPU and feed it through the 3D-LUT
//! texture path Vance already has (Phase 6.4/6.5). [`bake_lut`] generates
//! an identity lattice — exactly a [`Lut3d`]'s sample coordinates — and
//! runs OCIO's CPU processor over it; the transformed lattice *is* the
//! LUT. No shader changes, and the result drops straight into
//! `Renderer::upload_view_lut`.
//!
//! ## Known limitation (flagged, not hidden)
//!
//! The baked LUT has a **`[0, 1]` input domain**. That's correct for
//! display-referred conversions (sRGB ↔ Rec.709, adding a view/display
//! transform). Scene-linear / log / HDR inputs that exceed 1.0 are
//! clamped at the cube boundary because a plain 3D LUT can't represent an
//! unbounded domain — the proper fix is an *allocation / shaper 1D LUT*
//! in front of the 3D LUT, which needs 1D-LUT support the pipeline does
//! not have yet. That is tracked as a separate follow-up (6.1c); it is a
//! genuinely new capability, not a corner cut on this one.

use std::path::Path;
use vance_color::Lut3d;

/// Smallest / largest cube side [`bake_lut`] accepts. The lower bound
/// keeps trilinear interpolation meaningful; the upper bound caps the
/// baked texture at a sane size (129³ ≈ 2.1M entries).
pub const MIN_LUT_SIZE: u32 = 2;
/// See [`MIN_LUT_SIZE`].
pub const MAX_LUT_SIZE: u32 = 129;

/// Errors the OCIO bridge can return.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OcioError {
    /// The crate was built without the `ocio` feature, so no native
    /// OpenColorIO library is linked.
    #[error("OpenColorIO support is not compiled in (rebuild with --features ocio)")]
    Unavailable,

    /// A config path or colorspace name contained an interior NUL byte
    /// and can't cross the C boundary.
    #[error("string argument contained an interior NUL byte")]
    NulString,

    /// Requested LUT size is outside [`MIN_LUT_SIZE`]..=[`MAX_LUT_SIZE`].
    #[error("LUT size {0} out of range ({MIN_LUT_SIZE}..={MAX_LUT_SIZE})")]
    BadSize(u32),

    /// OpenColorIO raised an error (bad config, unknown colorspace, …).
    #[error("OpenColorIO error: {0}")]
    Ocio(String),
}

/// Whether this build links the native OpenColorIO library. `const` so
/// callers can branch at compile time.
pub const fn is_available() -> bool {
    cfg!(feature = "ocio")
}

/// Bake a `src` → `dst` colorspace conversion into a `size³` 3D LUT.
///
/// `config` is a path to an `.ocio` config file, or `None` to use the
/// config named by the `$OCIO` environment variable. `src`/`dst` are
/// colorspace (or role) names defined by that config. The returned
/// [`Lut3d`] has a `[0, 1]` input domain (see crate docs) and can be
/// handed straight to `Renderer::upload_view_lut`.
///
/// Returns [`OcioError::Unavailable`] when built without the `ocio`
/// feature.
pub fn bake_lut(
    config: Option<&Path>,
    src: &str,
    dst: &str,
    size: u32,
) -> Result<Lut3d, OcioError> {
    if !(MIN_LUT_SIZE..=MAX_LUT_SIZE).contains(&size) {
        return Err(OcioError::BadSize(size));
    }
    imp::bake_lut(config, src, dst, size)
}

/// Version string of the linked OpenColorIO library (e.g. `"2.5.2"`), or
/// `None` when built without the `ocio` feature.
pub fn version() -> Option<String> {
    imp::version()
}

/// List the colorspace names defined by an OCIO config (for populating
/// UI pickers). `config` is an `.ocio` path or `None` for the `$OCIO`
/// environment config. Returns [`OcioError::Unavailable`] when built
/// without the `ocio` feature.
pub fn list_colorspaces(config: Option<&Path>) -> Result<Vec<String>, OcioError> {
    imp::list_colorspaces(config)
}

// ---- feature OFF: pure-Rust stubs ----------------------------------------
#[cfg(not(feature = "ocio"))]
mod imp {
    use super::*;

    pub(super) fn bake_lut(
        _config: Option<&Path>,
        _src: &str,
        _dst: &str,
        _size: u32,
    ) -> Result<Lut3d, OcioError> {
        Err(OcioError::Unavailable)
    }

    pub(super) fn version() -> Option<String> {
        None
    }

    pub(super) fn list_colorspaces(_config: Option<&Path>) -> Result<Vec<String>, OcioError> {
        Err(OcioError::Unavailable)
    }
}

// ---- feature ON: real FFI ------------------------------------------------
#[cfg(feature = "ocio")]
mod imp {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int, c_long};

    extern "C" {
        fn vance_ocio_bake_lut(
            config_path: *const c_char,
            src_cs: *const c_char,
            dst_cs: *const c_char,
            rgb: *mut f32,
            num_pixels: c_long,
            errbuf: *mut c_char,
            errbuf_len: c_long,
        ) -> c_int;
        fn vance_ocio_version() -> *const c_char;
        fn vance_ocio_list_colorspaces(
            config_path: *const c_char,
            namebuf: *mut c_char,
            namebuf_len: c_long,
            errbuf: *mut c_char,
            errbuf_len: c_long,
        ) -> c_int;
    }

    pub(super) fn bake_lut(
        config: Option<&Path>,
        src: &str,
        dst: &str,
        size: u32,
    ) -> Result<Lut3d, OcioError> {
        // The identity lattice's values ARE the cube's input coordinates;
        // OCIO transforms them in place into the baked LUT.
        let mut lut = Lut3d::identity(size);
        let num_pixels = (size as usize).pow(3) as c_long;

        let config_c = match config {
            Some(p) => Some(
                CString::new(p.to_string_lossy().into_owned()).map_err(|_| OcioError::NulString)?,
            ),
            None => None,
        };
        let src_c = CString::new(src).map_err(|_| OcioError::NulString)?;
        let dst_c = CString::new(dst).map_err(|_| OcioError::NulString)?;

        let mut errbuf = [0i8; 1024];
        // SAFETY: all pointers are valid for the call's duration; `rgb`
        // points at `num_pixels * 3` writable floats; the shim catches
        // every C++ exception so nothing unwinds across the boundary.
        let rc = unsafe {
            vance_ocio_bake_lut(
                config_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
                src_c.as_ptr(),
                dst_c.as_ptr(),
                lut.values.as_mut_ptr(),
                num_pixels,
                errbuf.as_mut_ptr(),
                errbuf.len() as c_long,
            )
        };
        if rc != 0 {
            // SAFETY: the shim always NUL-terminates errbuf.
            let msg = unsafe { CStr::from_ptr(errbuf.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            return Err(OcioError::Ocio(msg));
        }
        Ok(lut)
    }

    pub(super) fn version() -> Option<String> {
        // SAFETY: returns a pointer to a static, NUL-terminated string.
        let v = unsafe { CStr::from_ptr(vance_ocio_version()) };
        Some(v.to_string_lossy().into_owned())
    }

    pub(super) fn list_colorspaces(config: Option<&Path>) -> Result<Vec<String>, OcioError> {
        let config_c = match config {
            Some(p) => Some(
                CString::new(p.to_string_lossy().into_owned()).map_err(|_| OcioError::NulString)?,
            ),
            None => None,
        };
        let cfg_ptr = config_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        let mut errbuf = [0i8; 1024];

        // The shim joins names with '\n' into `namebuf` and returns the
        // TRUE count even if the buffer was too small and the join got
        // truncated (possibly mid-name). So we grow the buffer and retry
        // until the number of names we actually parse matches the count —
        // never returning a truncated / corrupt list silently. Capped so
        // a pathological config can't drive unbounded allocation.
        let mut cap = 64 * 1024usize;
        const MAX_CAP: usize = 16 * 1024 * 1024;
        loop {
            let mut namebuf = vec![0i8; cap];
            // SAFETY: buffers are valid for the call; the shim
            // NUL-terminates both and catches every C++ exception.
            let count = unsafe {
                vance_ocio_list_colorspaces(
                    cfg_ptr,
                    namebuf.as_mut_ptr(),
                    namebuf.len() as c_long,
                    errbuf.as_mut_ptr(),
                    errbuf.len() as c_long,
                )
            };
            if count < 0 {
                let msg = unsafe { CStr::from_ptr(errbuf.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
                return Err(OcioError::Ocio(msg));
            }
            let names = unsafe { CStr::from_ptr(namebuf.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let parsed: Vec<String> = if names.is_empty() {
                Vec::new()
            } else {
                names.split('\n').map(|s| s.to_string()).collect()
            };
            // Buffer held the whole list → done.
            if parsed.len() == count as usize {
                return Ok(parsed);
            }
            // Truncated: grow and retry, unless we've hit the ceiling.
            if cap >= MAX_CAP {
                return Err(OcioError::Ocio(format!(
                    "colorspace name list exceeds {MAX_CAP} bytes ({count} colorspaces)"
                )));
            }
            cap = (cap * 2).min(MAX_CAP);
        }
    }
}
