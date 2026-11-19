//! `vance-color` — color-space types and conversions.
//!
//! The crate's job is to map decoded frames (in whatever color space
//! their container declared) into a project-defined *working space*,
//! and then onto a display transform on the way to the viewport. The
//! engine and renderer talk to this crate via a [`Pipeline`] that
//! resolves to a small set of GPU-uploadable parameters.
//!
//! Phase 6 scope (per PLAN.md §6) calls for OCIO under the hood. v1
//! ships built-in matrices for the common spaces — Rec.709, sRGB,
//! Rec.2020, Linear sRGB, Display P3, ACEScg — so the rest of Phase
//! 6 (LUTs, color page, scopes) can land on top without waiting for
//! the OCIO FFI shim. The `ocio` feature lights up the OpenColorIO
//! path (Phase 6.1b) once the host has libOpenColorIO installed.

#![warn(missing_docs)]

pub mod lut;
pub mod pipeline;
pub mod space;
pub mod transform;

pub use lut::{load_from_path as load_lut, parse_3dl, parse_cube, Lut3d, LutError};
pub use pipeline::{Pipeline, PipelineParams};
pub use space::{ColorSpace, Primaries, Range, TransferFunction};
pub use transform::Transform;

/// Errors returned by the color pipeline.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ColorError {
    /// The requested color space is not built-in and OCIO is not
    /// available (feature off / runtime config missing).
    #[error("unsupported color space without OCIO: {0:?}")]
    UnsupportedSpace(ColorSpace),
}
