//! End-to-end color pipeline: decode-side, working-space, display-side.
//!
//! Renderers consume a [`Pipeline`] by reading [`PipelineParams`] —
//! GPU-uploadable scalars + matrices that drive the shader. The
//! shader applies them in this order, all in linear RGB once
//! decoded:
//!
//! 1. Decode-side transfer + range expand: byte/levels-encoded source
//!    samples → linear in the *source* primaries.
//! 2. Primaries change: source primaries → working primaries (matrix
//!    multiply in linear RGB).
//! 3. Color grading nodes (Phase 6.6+) run here, in working space.
//! 4. Display-side primaries: working → output primaries.
//! 5. Display-side transfer + range encode: linear → encoded output.
//!
//! v1 ships the matrix path; the [`crate::ocio`] module (Phase 6.1b)
//! can replace the math with an OCIO processor for arbitrary configs.

use crate::space::{ColorSpace, Range, TransferFunction};
use crate::transform::{primaries_to_primaries, Transform};

/// One color pipeline configuration: input space → working space →
/// output space. Stored on the engine; passed to the renderer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pipeline {
    /// What the decoded source samples are in.
    pub input: ColorSpace,
    /// Where grading happens. Renderer's intermediate.
    pub working: ColorSpace,
    /// What the viewport surface expects.
    pub output: ColorSpace,
}

impl Default for Pipeline {
    fn default() -> Self {
        // Conservative SDR default that matches existing Phase 1-5
        // behavior: decode Rec.709 limited, work in linear Rec.709,
        // present as sRGB (the viewport's surface format).
        Self {
            input: ColorSpace::Rec709Limited,
            working: ColorSpace::LinearRec709,
            output: ColorSpace::Srgb,
        }
    }
}

/// Flattened pipeline parameters the shader uses. Sized to fit a
/// single uniform buffer; layout matches the WGSL counterpart added
/// in Phase 6.3.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PipelineParams {
    /// Decode-side transfer enum (cast to f32 for shader switch).
    pub input_transfer: u32,
    /// Encode-side transfer enum.
    pub output_transfer: u32,
    /// Decode-side range (Limited / Full).
    pub input_range: u32,
    /// Encode-side range.
    pub output_range: u32,
    /// Linear-RGB matrix: input primaries → working primaries.
    pub input_to_working: Transform,
    /// Linear-RGB matrix: working primaries → output primaries.
    pub working_to_output: Transform,
}

impl Pipeline {
    /// Compute the flattened params. Returns `None` if either side
    /// uses a transfer the built-in path can't handle (PQ, HLG,
    /// ACEScct) and the `ocio` feature is off — the engine then
    /// falls back to OCIO or surfaces a clear error.
    pub fn to_params(&self) -> Option<PipelineParams> {
        // Input + working must be built-in DECODE-able (no PQ/HLG decode in
        // the shader); the output may additionally be Rec.2020 PQ/HLG since
        // the shader now has forward PQ/HLG OETFs (HDR export).
        if !self.input.is_builtin_supported()
            || !self.working.is_builtin_supported()
            || !self.output.is_builtin_output_supported()
        {
            return None;
        }
        let (in_p, in_t, in_r) = self.input.decompose();
        let (wk_p, _, _) = self.working.decompose();
        let (out_p, out_t, out_r) = self.output.decompose();
        Some(PipelineParams {
            input_transfer: transfer_id(in_t),
            output_transfer: transfer_id(out_t),
            input_range: range_id(in_r),
            output_range: range_id(out_r),
            input_to_working: primaries_to_primaries(in_p, wk_p),
            working_to_output: primaries_to_primaries(wk_p, out_p),
        })
    }
}

fn transfer_id(t: TransferFunction) -> u32 {
    match t {
        TransferFunction::Linear => 0,
        TransferFunction::Srgb => 1,
        TransferFunction::Rec709 => 2,
        TransferFunction::Rec2020 => 3,
        TransferFunction::Pq => 4,
        TransferFunction::Hlg => 5,
        TransferFunction::Acescct => 6,
    }
}

fn range_id(r: Range) -> u32 {
    match r {
        Range::Limited => 0,
        Range::Full => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_pipeline_resolves_to_params() {
        let p = Pipeline::default();
        let params = p.to_params().expect("default pipeline is built-in-supported");
        // Rec.709 → Linear Rec.709 → sRGB: input transfer Rec.709,
        // output transfer sRGB; both primaries are 709 so the
        // matrices are identity.
        assert_eq!(params.input_transfer, transfer_id(TransferFunction::Rec709));
        assert_eq!(params.output_transfer, transfer_id(TransferFunction::Srgb));
        assert_eq!(params.input_to_working, Transform::IDENTITY);
        assert_eq!(params.working_to_output, Transform::IDENTITY);
    }

    #[test]
    fn rec2020_pq_output_resolves_with_pq_transfer() {
        // HDR export: Rec.709 input, Linear Rec.709 working, Rec.2020 PQ
        // output now produces params (the shader encodes PQ), with a
        // 709→2020 primaries matrix and output transfer id 4 (PQ).
        let p = Pipeline {
            input: ColorSpace::Rec709Limited,
            working: ColorSpace::LinearRec709,
            output: ColorSpace::Rec2020Pq,
        };
        let params = p.to_params().expect("Rec.2020 PQ output is now built-in-encodable");
        assert_eq!(params.output_transfer, transfer_id(TransferFunction::Pq));
        // 709 → 2020 primaries is NOT identity.
        assert_ne!(params.working_to_output, Transform::IDENTITY);
    }

    #[test]
    fn pq_input_falls_back_through_ocio() {
        let p = Pipeline {
            input: ColorSpace::Rec2020Pq,
            ..Pipeline::default()
        };
        // Built-in path can't handle PQ, so params come back None.
        // The engine layer will then route to OCIO (6.1b) or surface
        // ColorError::UnsupportedSpace.
        assert!(p.to_params().is_none());
    }

    #[test]
    fn cross_primaries_path_uses_matrix() {
        let p = Pipeline {
            input: ColorSpace::Rec2020Limited,
            working: ColorSpace::LinearRec709,
            output: ColorSpace::Srgb,
        };
        let params = p.to_params().expect("built-in supported");
        // 2020 → 709 should NOT be identity.
        assert_ne!(params.input_to_working, Transform::IDENTITY);
        // 709 → 709 (working to output share 709 primaries) IS identity.
        assert_eq!(params.working_to_output, Transform::IDENTITY);
    }
}
