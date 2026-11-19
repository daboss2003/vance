//! 3×3 matrix transforms between RGB color spaces (built-in v1 path).
//!
//! Each known [`Primaries`] has a precomputed `RGB → XYZ` matrix
//! using D65 / DCI-P3 white point. Converting from primaries A to
//! primaries B is `B_to_xyz⁻¹ · A_to_xyz`. The transfer function and
//! range live elsewhere ([`crate::space`] + the shader); this module
//! is pure linear-RGB arithmetic.

use crate::space::Primaries;

/// 3×3 row-major matrix.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Transform(pub [[f32; 3]; 3]);

impl Transform {
    /// Identity transform.
    pub const IDENTITY: Self = Self([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]);

    /// Multiply two row-major matrices: `self * rhs`.
    pub fn compose(&self, rhs: &Self) -> Self {
        let a = &self.0;
        let b = &rhs.0;
        let mut out = [[0.0_f32; 3]; 3];
        for i in 0..3 {
            for j in 0..3 {
                out[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j] + a[i][2] * b[2][j];
            }
        }
        Self(out)
    }

    /// Apply to a column vector `(r, g, b)`.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        let m = &self.0;
        [
            m[0][0] * rgb[0] + m[0][1] * rgb[1] + m[0][2] * rgb[2],
            m[1][0] * rgb[0] + m[1][1] * rgb[1] + m[1][2] * rgb[2],
            m[2][0] * rgb[0] + m[2][1] * rgb[1] + m[2][2] * rgb[2],
        ]
    }

    /// Adjoint-based inverse of a 3×3 matrix.
    pub fn inverse(&self) -> Self {
        let m = &self.0;
        let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
        // Defensive: if the matrix is singular (shouldn't happen for
        // any of the well-formed primaries-to-XYZ matrices below),
        // return identity rather than producing NaNs.
        if det.abs() < 1e-9 {
            return Self::IDENTITY;
        }
        let inv_det = 1.0 / det;
        Self([
            [
                (m[1][1] * m[2][2] - m[1][2] * m[2][1]) * inv_det,
                (m[0][2] * m[2][1] - m[0][1] * m[2][2]) * inv_det,
                (m[0][1] * m[1][2] - m[0][2] * m[1][1]) * inv_det,
            ],
            [
                (m[1][2] * m[2][0] - m[1][0] * m[2][2]) * inv_det,
                (m[0][0] * m[2][2] - m[0][2] * m[2][0]) * inv_det,
                (m[0][2] * m[1][0] - m[0][0] * m[1][2]) * inv_det,
            ],
            [
                (m[1][0] * m[2][1] - m[1][1] * m[2][0]) * inv_det,
                (m[0][1] * m[2][0] - m[0][0] * m[2][1]) * inv_det,
                (m[0][0] * m[1][1] - m[0][1] * m[1][0]) * inv_det,
            ],
        ])
    }
}

/// `RGB → XYZ` matrix for each supported `Primaries`. White points:
/// D65 except DCI-P3 which uses sRGB's D65 (a.k.a. "Display P3").
// Intentional: the matrices are the published values digit-for-digit (ITU-R
// BT.709/BT.2020, ACES S-2014-004) so they can be checked against the specs;
// rounding them to f32 precision would change nothing at runtime.
#[allow(clippy::excessive_precision)]
pub fn primaries_to_xyz(p: Primaries) -> Transform {
    match p {
        // Rec.709 / sRGB primaries, D65 white. From ITU-R BT.709.
        Primaries::Bt709 => Transform([
            [0.4124564, 0.3575761, 0.1804375],
            [0.2126729, 0.7151522, 0.0721750],
            [0.0193339, 0.1191920, 0.9503041],
        ]),
        // Rec.2020 primaries, D65. From ITU-R BT.2020.
        Primaries::Bt2020 => Transform([
            [0.6369580, 0.1446169, 0.1688810],
            [0.2627002, 0.6779981, 0.0593017],
            [0.0000000, 0.0280727, 1.0609851],
        ]),
        // Display P3: P3 primaries with D65 white (Apple's variant).
        Primaries::DisplayP3 => Transform([
            [0.4865709, 0.2656677, 0.1982173],
            [0.2289746, 0.6917385, 0.0792869],
            [0.0000000, 0.0451134, 1.0439444],
        ]),
        // ACES AP1 primaries, D60 white.
        Primaries::AcesAp1 => Transform([
            [0.6624541811, 0.1340042065, 0.1561876870],
            [0.2722287168, 0.6740817658, 0.0536895174],
            [-0.0055746495, 0.0040607335, 1.0103391003],
        ]),
        // ACES AP0 primaries, D60 white.
        Primaries::AcesAp0 => Transform([
            [0.9525523959, 0.0000000000, 0.0000936786],
            [0.3439664498, 0.7281660966, -0.0721325464],
            [0.0000000000, 0.0000000000, 1.0088251844],
        ]),
    }
}

/// Linear-RGB conversion matrix from primaries `src` to primaries
/// `dst` — multiply on the right: `dst_rgb = M * src_rgb`. Returns
/// identity when `src == dst`.
pub fn primaries_to_primaries(src: Primaries, dst: Primaries) -> Transform {
    if src == dst {
        return Transform::IDENTITY;
    }
    let src_to_xyz = primaries_to_xyz(src);
    let dst_to_xyz = primaries_to_xyz(dst);
    let xyz_to_dst = dst_to_xyz.inverse();
    xyz_to_dst.compose(&src_to_xyz)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: [f32; 3], b: [f32; 3], eps: f32) -> bool {
        (a[0] - b[0]).abs() < eps && (a[1] - b[1]).abs() < eps && (a[2] - b[2]).abs() < eps
    }

    #[test]
    fn identity_for_same_primaries() {
        let m = primaries_to_primaries(Primaries::Bt709, Primaries::Bt709);
        assert_eq!(m, Transform::IDENTITY);
    }

    #[test]
    fn rec709_to_rec2020_then_back_is_identity() {
        let to_2020 = primaries_to_primaries(Primaries::Bt709, Primaries::Bt2020);
        let back = primaries_to_primaries(Primaries::Bt2020, Primaries::Bt709);
        // Pick a non-trivial color so the round trip exercises every
        // coefficient.
        let v = [0.4_f32, 0.6, 0.8];
        let out = back.apply(to_2020.apply(v));
        assert!(approx_eq(v, out, 1e-4), "got {out:?}");
    }

    #[test]
    fn xyz_to_xyz_round_trip() {
        let m = primaries_to_xyz(Primaries::Bt709);
        let mi = m.inverse();
        let composed = mi.compose(&m);
        // Should be identity to ~1e-5.
        for i in 0..3 {
            for j in 0..3 {
                let want = if i == j { 1.0 } else { 0.0 };
                assert!(
                    (composed.0[i][j] - want).abs() < 1e-5,
                    "[{i}][{j}] = {} (want {want})",
                    composed.0[i][j]
                );
            }
        }
    }
}
