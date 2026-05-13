//! `ColorSpace` and its decomposable parts.
//!
//! A color space, for our purposes, is a triple of:
//! - [`Primaries`] — chromaticities of the R/G/B + white point
//!   (defines an RGB → XYZ matrix).
//! - [`TransferFunction`] — encodes linearity (Linear, sRGB-style,
//!   Rec.709/2020 gamma, PQ, HLG, …).
//! - [`Range`] — full or limited (16–235 for 8-bit luma) signal levels.
//!
//! `ColorSpace` is an enum over the *common* combinations so callers
//! aren't required to specify all three independently. v1 covers the
//! cases the demos and test media exercise; the OCIO path (6.1b)
//! handles the long tail via configured color spaces.

use serde::{Deserialize, Serialize};

/// RGB primaries + white point. The pair of primaries names is
/// shorthand for a known triple of (r, g, b) chromaticities (see e.g.
/// Rec. ITU-R BT.709 §1).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum Primaries {
    /// ITU-R BT.709 — same as sRGB / Display P3 isn't here; sRGB uses
    /// the *same* primaries as 709 (only the transfer differs).
    Bt709,
    /// ITU-R BT.2020.
    Bt2020,
    /// Display P3 (DCI-P3 with sRGB white point).
    DisplayP3,
    /// ACES AP1 (working gamut for ACEScct / ACEScg).
    AcesAp1,
    /// ACES AP0 (encompassing).
    AcesAp0,
}

/// Transfer function (OETF/EOTF). What pixel encoding the channel
/// values live in — linear, gamma-encoded, log, etc.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum TransferFunction {
    /// Linear light. Value is proportional to radiance.
    Linear,
    /// sRGB transfer (~2.2 with a linear toe).
    Srgb,
    /// Rec.709 transfer (~1.961 with a linear toe).
    Rec709,
    /// Rec.2020 transfer — shape identical to 709 but normalised to
    /// 12-bit range.
    Rec2020,
    /// SMPTE ST.2084 (PQ) — HDR with absolute luminance.
    Pq,
    /// Hybrid Log-Gamma (HLG) — HDR with relative luminance.
    Hlg,
    /// ACEScct log encoding.
    Acescct,
}

/// Signal level range. Limited maps Y' ∈ \[16, 235\] (and chroma ∈
/// \[16, 240\]); Full maps the whole 0..255 / 0..1023 / 0..1 span.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum Range {
    /// Studio / video range (16–235 luma at 8-bit).
    Limited,
    /// Full data range.
    Full,
}

/// One of the common color-space *combinations*. Tagged on
/// [`vance_timeline::MediaItem`]s (Phase 6.2) and on
/// [`Project.working_space`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum ColorSpace {
    /// Rec.709 limited-range — what most HD video files declare.
    Rec709Limited,
    /// Rec.709 full-range.
    Rec709Full,
    /// sRGB (Rec.709 primaries, sRGB transfer, full range). What
    /// most still images and screen-recorded video use.
    Srgb,
    /// Linear-light Rec.709 / sRGB (no transfer applied; values are
    /// scene-referred radiance). Default working space.
    LinearRec709,
    /// Linear Rec.2020.
    LinearRec2020,
    /// Rec.2020 with the 709-shaped transfer, limited range.
    /// Common for UHD broadcast SDR.
    Rec2020Limited,
    /// Rec.2020 PQ (HDR10).
    Rec2020Pq,
    /// Rec.2020 HLG (broadcast HDR).
    Rec2020Hlg,
    /// Display P3 (sRGB transfer, P3 primaries) — Apple's wide-gamut
    /// SDR.
    DisplayP3,
    /// ACEScg — AP1 primaries, linear, full range. Common grading
    /// working space.
    Acescg,
    /// ACEScct — AP1 primaries, log encoding (cct). For colorists.
    Acescct,
}

impl ColorSpace {
    /// Decompose into `(primaries, transfer, range)`.
    pub fn decompose(self) -> (Primaries, TransferFunction, Range) {
        use ColorSpace::*;
        match self {
            Rec709Limited => (Primaries::Bt709, TransferFunction::Rec709, Range::Limited),
            Rec709Full => (Primaries::Bt709, TransferFunction::Rec709, Range::Full),
            Srgb => (Primaries::Bt709, TransferFunction::Srgb, Range::Full),
            LinearRec709 => (Primaries::Bt709, TransferFunction::Linear, Range::Full),
            LinearRec2020 => (Primaries::Bt2020, TransferFunction::Linear, Range::Full),
            Rec2020Limited => (Primaries::Bt2020, TransferFunction::Rec2020, Range::Limited),
            Rec2020Pq => (Primaries::Bt2020, TransferFunction::Pq, Range::Full),
            Rec2020Hlg => (Primaries::Bt2020, TransferFunction::Hlg, Range::Full),
            DisplayP3 => (Primaries::DisplayP3, TransferFunction::Srgb, Range::Full),
            Acescg => (Primaries::AcesAp1, TransferFunction::Linear, Range::Full),
            Acescct => (Primaries::AcesAp1, TransferFunction::Acescct, Range::Full),
        }
    }

    /// Built-in support flag — `true` when v1's matrix path can
    /// handle this space, `false` when only OCIO can.
    pub fn is_builtin_supported(self) -> bool {
        use ColorSpace::*;
        // PQ / HLG / Acescct have transfer functions that aren't a
        // simple matrix-and-power; the OCIO path (6.1b) handles them.
        !matches!(self, Rec2020Pq | Rec2020Hlg | Acescct)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decompose_round_trips_known_spaces() {
        assert_eq!(
            ColorSpace::Rec709Limited.decompose(),
            (Primaries::Bt709, TransferFunction::Rec709, Range::Limited)
        );
        assert_eq!(
            ColorSpace::Srgb.decompose(),
            (Primaries::Bt709, TransferFunction::Srgb, Range::Full)
        );
        assert_eq!(
            ColorSpace::Acescg.decompose(),
            (Primaries::AcesAp1, TransferFunction::Linear, Range::Full)
        );
    }

    #[test]
    fn builtin_support_excludes_log_and_hdr_transfers() {
        assert!(ColorSpace::Rec709Limited.is_builtin_supported());
        assert!(ColorSpace::Srgb.is_builtin_supported());
        assert!(ColorSpace::Acescg.is_builtin_supported());
        assert!(!ColorSpace::Rec2020Pq.is_builtin_supported());
        assert!(!ColorSpace::Rec2020Hlg.is_builtin_supported());
        assert!(!ColorSpace::Acescct.is_builtin_supported());
    }
}
