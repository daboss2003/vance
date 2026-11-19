//! Scope colorimetry. All math operates on sRGB **gamma-encoded code
//! values** (NOT linearized) — this matches hardware scopes and DaVinci
//! Resolve, which plot Y' on the encoded signal, and is the deliberate
//! convention for monitoring tools.

/// Rec.709 luma coefficients, applied to gamma-encoded code values.
pub const LUMA_R: f32 = 0.2126;
pub const LUMA_G: f32 = 0.7152;
pub const LUMA_B: f32 = 0.0722;

/// Rec.709 luma `Y'` from 8-bit sRGB code values, returned in `[0, 255]`.
#[inline]
pub fn rec709_luma_codeval(r: u8, g: u8, b: u8) -> f32 {
    LUMA_R * r as f32 + LUMA_G * g as f32 + LUMA_B * b as f32
}

/// Full-range Rec.709 `Y'CbCr` from normalized `[0,1]` `R'G'B'`.
/// `Cb`/`Cr` are returned in `[-0.5, 0.5]`. Full-range (not 16–235
/// limited) because the analyzed image is full-range sRGB display data.
#[inline]
pub fn ycbcr_fullrange(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let y = LUMA_R * r + LUMA_G * g + LUMA_B * b;
    let cb = (b - y) / 1.8556;
    let cr = (r - y) / 1.5748;
    (y, cb, cr)
}

/// Map a `(Cb, Cr)` pair to vectorscope grid coordinates for a `size`×
/// `size` grid: `+Cb` → right, `+Cr` → up (so the y axis is flipped, as
/// on a real vectorscope). Returns floating coords (callers round +
/// clamp). This is the SINGLE normalization shared by the density
/// accumulator and the target/overlay coordinates, so they always align.
#[inline]
pub fn cbcr_to_grid(cb: f32, cr: f32, size: u32) -> (f32, f32) {
    let s = (size - 1) as f32;
    let vx = (cb + 0.5) * s;
    let vy = (0.5 - cr) * s;
    (vx, vy)
}

/// The six 75%-saturation color-bar primaries/secondaries in normalized
/// `R'G'B'`, in the canonical vectorscope order R, Mg, B, Cy, G, Yl.
pub const BARS_75: [(f32, f32, f32); 6] = [
    (0.75, 0.0, 0.0),  // Red
    (0.75, 0.0, 0.75), // Magenta
    (0.0, 0.0, 0.75),  // Blue
    (0.0, 0.75, 0.75), // Cyan
    (0.0, 0.75, 0.0),  // Green
    (0.75, 0.75, 0.0), // Yellow
];

/// Skin-tone ("I"/flesh) line angle in degrees, measured CCW from the
/// `+Cb` (right) axis in the grid's screen space. ~123° is the standard
/// graticule value; used only for the UI overlay, not for binning.
pub const SKIN_TONE_LINE_DEG: f32 = 123.0;
