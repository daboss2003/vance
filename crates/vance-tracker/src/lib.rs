//! `vance-tracker` — point-tracking core (Phase 6.11).
//!
//! Frame-to-frame translation tracking by **zero-mean normalized
//! cross-correlation (ZNCC)** on the Y (luma) plane. Single-scale,
//! integer-pixel argmax over a bounded search window — translation only,
//! no rotation or scale. Pure functions over borrowed slices so the
//! entire per-frame loop is unit-testable without IPC.
//!
//! NCC is preferred over plain SSD because it's invariant to mean /
//! contrast shifts (so it survives exposure flicker between frames that
//! would derail SSD). Lucas-Kanade pyramidal tracking is the obvious
//! next step (6.11b) — it handles faster motion and scale changes, but
//! adds gradients + pyramids + iterative warps that are harder to
//! verify than NCC's deterministic argmax.
//!
//! Known v1 limitations (deliberate, separable; see PLAN.md 6.11b):
//! * integer-pixel precision (no sub-pixel parabolic refinement),
//! * single-scale (no pyramid → fast motion needs a bigger search radius),
//! * no template update (drift on long tracks).

use serde::{Deserialize, Serialize};

/// An owned luma patch (the *template* clipped from the anchor frame).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LumaPatch {
    /// Row-major luma bytes, length `(w * h) as usize`.
    pub data: Vec<u8>,
    /// Patch width in pixels.
    pub w: u32,
    /// Patch height in pixels.
    pub h: u32,
}

/// A non-owning view of a frame's luma plane. `stride` may differ from
/// `w` (decoders typically align rows to a power of two).
#[derive(Clone, Copy, Debug)]
pub struct LumaView<'a> {
    /// Row-major luma bytes (top-left origin).
    pub bytes: &'a [u8],
    /// Row stride in bytes (>= `w`).
    pub stride: u32,
    /// Frame width in pixels.
    pub w: u32,
    /// Frame height in pixels.
    pub h: u32,
}

/// Result of one tracked frame.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrackResult {
    /// Pixel coords of the patch CENTER after this frame.
    pub center_px: (i32, i32),
    /// Best ZNCC score in `[-1, 1]`; 1.0 = perfect match.
    pub score: f32,
}

/// Errors the tracker can raise.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TrackError {
    /// `patch_half` (radius) larger than half the frame in some axis.
    #[error("patch size ({0}) larger than frame ({1}x{2})")]
    PatchTooBig(u32, u32, u32),
    /// The anchor / search box is fully outside the frame after clamping.
    #[error("no overlap between search box and frame")]
    NoOverlap,
    /// A luma slice is shorter than `stride * h` bytes.
    #[error("luma view buffer too small: {got} bytes, need at least {need}")]
    BufferTooSmall { got: usize, need: usize },
}

#[inline]
fn px_at(view: &LumaView, x: i32, y: i32) -> u8 {
    let xi = x as u32;
    let yi = y as u32;
    let i = (yi * view.stride + xi) as usize;
    view.bytes[i]
}

#[inline]
fn check_view(view: &LumaView) -> Result<(), TrackError> {
    let need = (view.stride as usize) * (view.h as usize);
    if view.bytes.len() < need {
        return Err(TrackError::BufferTooSmall {
            got: view.bytes.len(),
            need,
        });
    }
    Ok(())
}

/// Copy a `(2*patch_half+1)²` patch around `center_px` out of a frame.
/// Clamps the source rectangle to the frame bounds; if the resulting box
/// is empty, returns `NoOverlap`. The template is owned so it survives
/// the source `FrameData` being dropped.
pub fn extract_patch(
    view: &LumaView,
    center_px: (i32, i32),
    patch_half: i32,
) -> Result<LumaPatch, TrackError> {
    check_view(view)?;
    if patch_half < 0 {
        return Err(TrackError::PatchTooBig(0, view.w, view.h));
    }
    let side = (2 * patch_half + 1) as u32;
    if side > view.w || side > view.h {
        return Err(TrackError::PatchTooBig(side, view.w, view.h));
    }
    // Clamp center so the full patch fits inside the frame.
    let cx = (center_px.0).clamp(patch_half, view.w as i32 - 1 - patch_half);
    let cy = (center_px.1).clamp(patch_half, view.h as i32 - 1 - patch_half);
    let x0 = cx - patch_half;
    let y0 = cy - patch_half;
    let mut data = Vec::with_capacity((side * side) as usize);
    for y in 0..side {
        let row_off = ((y0 as u32 + y) * view.stride + x0 as u32) as usize;
        data.extend_from_slice(&view.bytes[row_off..row_off + side as usize]);
    }
    Ok(LumaPatch {
        data,
        w: side,
        h: side,
    })
}

/// ZNCC between two equal-length patches expressed as raw bytes. Each
/// is centered by subtracting its mean; returns `0.0` when either side
/// is uniform (zero variance, no signal to match).
fn zncc(a: &[u8], b: &[u8]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let n = a.len() as f32;
    let mut sa = 0.0f32;
    let mut sb = 0.0f32;
    for i in 0..a.len() {
        sa += a[i] as f32;
        sb += b[i] as f32;
    }
    let ma = sa / n;
    let mb = sb / n;
    let mut num = 0.0f32;
    let mut da = 0.0f32;
    let mut db = 0.0f32;
    for i in 0..a.len() {
        let av = a[i] as f32 - ma;
        let bv = b[i] as f32 - mb;
        num += av * bv;
        da += av * av;
        db += bv * bv;
    }
    let den = (da * db).sqrt();
    if den < 1e-6 {
        0.0
    } else {
        num / den
    }
}

/// Find the center, near `prev_center_px`, where the `frame` patch best
/// matches `template` by ZNCC. Search is an integer-pixel raster over
/// `[-search_radius_px, +search_radius_px]` on each axis; candidate
/// centers are clipped so the patch stays fully inside the frame.
pub fn track_ncc(
    template: &LumaPatch,
    frame: &LumaView,
    prev_center_px: (i32, i32),
    search_radius_px: i32,
    patch_half: i32,
) -> Result<TrackResult, TrackError> {
    check_view(frame)?;
    if template.w != template.h || template.w != (2 * patch_half + 1) as u32 {
        return Err(TrackError::PatchTooBig(template.w, frame.w, frame.h));
    }
    let side = (2 * patch_half + 1) as u32;
    if side > frame.w || side > frame.h {
        return Err(TrackError::PatchTooBig(side, frame.w, frame.h));
    }
    let min_c = patch_half;
    let max_cx = frame.w as i32 - 1 - patch_half;
    let max_cy = frame.h as i32 - 1 - patch_half;
    // Search box, intersected with the valid-center range.
    let sx0 = (prev_center_px.0 - search_radius_px).max(min_c);
    let sx1 = (prev_center_px.0 + search_radius_px).min(max_cx);
    let sy0 = (prev_center_px.1 - search_radius_px).max(min_c);
    let sy1 = (prev_center_px.1 + search_radius_px).min(max_cy);
    if sx0 > sx1 || sy0 > sy1 {
        return Err(TrackError::NoOverlap);
    }
    // Scratch buffer for the candidate patch.
    let mut cand = vec![0u8; (side * side) as usize];
    let mut best = TrackResult {
        center_px: prev_center_px,
        score: f32::NEG_INFINITY,
    };
    for cy in sy0..=sy1 {
        for cx in sx0..=sx1 {
            // Copy candidate patch row-by-row.
            let x0 = cx - patch_half;
            let y0 = cy - patch_half;
            for y in 0..side {
                let row_off = ((y0 as u32 + y) * frame.stride + x0 as u32) as usize;
                let dst = (y * side) as usize;
                cand[dst..dst + side as usize]
                    .copy_from_slice(&frame.bytes[row_off..row_off + side as usize]);
            }
            let s = zncc(&template.data, &cand);
            if s > best.score {
                best.score = s;
                best.center_px = (cx, cy);
            }
        }
    }
    if best.score == f32::NEG_INFINITY {
        return Err(TrackError::NoOverlap);
    }
    Ok(best)
}

/// Track the template across a sequence of frames, walking `prev_center`
/// forward each frame. Returns one [`TrackResult`] per frame in order;
/// stops on the first error (caller can inspect partial output if
/// desired by collecting before calling — this version returns nothing
/// on error to keep the contract simple).
pub fn track_sequence<'a, I: IntoIterator<Item = LumaView<'a>>>(
    template: &LumaPatch,
    frames: I,
    seed_center_px: (i32, i32),
    search_radius_px: i32,
    patch_half: i32,
) -> Result<Vec<TrackResult>, TrackError> {
    let mut prev = seed_center_px;
    let mut out = Vec::new();
    for frame in frames {
        let r = track_ncc(template, &frame, prev, search_radius_px, patch_half)?;
        prev = r.center_px;
        out.push(r);
    }
    Ok(out)
}

// Pixel <-> source-UV helpers — kept here so engine + tests share one
// rounding rule. The convention is "pixel CENTER" so a uv of 0.5 maps to
// pixel (w/2, h/2) and the inverse maps the integer pixel center back.
/// Source-UV (`[0, 1]`) -> integer pixel index, rounded.
pub fn uv_to_px(uv: (f32, f32), w: u32, h: u32) -> (i32, i32) {
    let x = (uv.0 * w as f32).round() as i32;
    let y = (uv.1 * h as f32).round() as i32;
    (x.clamp(0, w as i32 - 1), y.clamp(0, h as i32 - 1))
}

/// Integer pixel center -> source-UV (`[0, 1]`).
pub fn px_to_uv(px: (i32, i32), w: u32, h: u32) -> (f32, f32) {
    (px.0 as f32 / w as f32, px.1 as f32 / h as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build a `w*h` frame whose luma value is a function of (x, y) so
    // tracking has a real signal to lock onto.
    fn synth_frame(w: u32, h: u32, f: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut v = Vec::with_capacity((w * h) as usize);
        for y in 0..h {
            for x in 0..w {
                v.push(f(x, y));
            }
        }
        v
    }
    fn view<'a>(bytes: &'a [u8], w: u32, h: u32) -> LumaView<'a> {
        LumaView {
            bytes,
            stride: w,
            w,
            h,
        }
    }
    // Frame with a sharp dark blob centered at (cx, cy) — gives the
    // tracker an unambiguous correlation peak.
    fn blob_frame(w: u32, h: u32, cx: i32, cy: i32) -> Vec<u8> {
        synth_frame(w, h, |x, y| {
            let dx = x as i32 - cx;
            let dy = y as i32 - cy;
            let d2 = (dx * dx + dy * dy) as f32;
            // Gaussian-ish bump, value 0 at the center → 220 in the bg.
            let v = 220.0 - 200.0 * (-d2 / 32.0).exp();
            v.clamp(0.0, 255.0) as u8
        })
    }

    #[test]
    fn template_against_itself_scores_one() {
        let buf = blob_frame(64, 48, 32, 24);
        let v = view(&buf, 64, 48);
        let tpl = extract_patch(&v, (32, 24), 7).unwrap();
        let r = track_ncc(&tpl, &v, (32, 24), 4, 7).unwrap();
        assert_eq!(r.center_px, (32, 24));
        assert!(
            (r.score - 1.0).abs() < 1e-4,
            "perfect match score {}",
            r.score
        );
    }

    #[test]
    fn programmatic_shift_is_recovered() {
        // Build a template from frame A, then track in frame B where the
        // blob moved by a known (dx, dy).
        let a = blob_frame(64, 48, 32, 24);
        let tpl = extract_patch(&view(&a, 64, 48), (32, 24), 7).unwrap();
        for (dx, dy) in [(0, 0), (3, 0), (0, 2), (-4, 3), (5, -2)] {
            let b = blob_frame(64, 48, 32 + dx, 24 + dy);
            let r = track_ncc(&tpl, &view(&b, 64, 48), (32, 24), 8, 7).unwrap();
            assert_eq!(r.center_px, (32 + dx, 24 + dy), "shift ({dx},{dy})");
            assert!(r.score > 0.95, "match score {}", r.score);
        }
    }

    #[test]
    fn track_sequence_walks_position_forward() {
        // 5 frames; blob moves +2 px in x each frame.
        let a = blob_frame(64, 48, 16, 24);
        let tpl = extract_patch(&view(&a, 64, 48), (16, 24), 6).unwrap();
        let frames: Vec<Vec<u8>> = (1..=5)
            .map(|i| blob_frame(64, 48, 16 + 2 * i, 24))
            .collect();
        let views: Vec<LumaView> = frames.iter().map(|f| view(f, 64, 48)).collect();
        let trail = track_sequence(&tpl, views, (16, 24), 4, 6).unwrap();
        let xs: Vec<i32> = trail.iter().map(|r| r.center_px.0).collect();
        assert_eq!(xs, vec![18, 20, 22, 24, 26]);
    }

    #[test]
    fn invariant_to_mean_offset() {
        // ZNCC tolerates a flat brightness shift (exposure flicker).
        let a = blob_frame(64, 48, 32, 24);
        let tpl = extract_patch(&view(&a, 64, 48), (32, 24), 6).unwrap();
        // Brighten the frame by +20 uniformly (clip at 255).
        let b: Vec<u8> = a.iter().map(|&v| v.saturating_add(20)).collect();
        let r = track_ncc(&tpl, &view(&b, 64, 48), (32, 24), 3, 6).unwrap();
        assert_eq!(r.center_px, (32, 24));
        assert!(r.score > 0.99, "brightness-shifted score {}", r.score);
    }

    #[test]
    fn edge_clamp_no_panic() {
        // Anchor near a corner: extract_patch clamps; track_ncc clamps
        // the search box without going out of bounds.
        let f = blob_frame(64, 48, 4, 4);
        let v = view(&f, 64, 48);
        let tpl = extract_patch(&v, (1, 1), 4).unwrap();
        assert_eq!(tpl.w, 9);
        let r = track_ncc(&tpl, &v, (1, 1), 8, 4).unwrap();
        // Best position should be valid (patch fits) and score sane.
        assert!(r.center_px.0 >= 4 && r.center_px.1 >= 4);
    }

    #[test]
    fn patch_too_big_errors() {
        let f = blob_frame(16, 16, 8, 8);
        let v = view(&f, 16, 16);
        assert!(matches!(
            extract_patch(&v, (8, 8), 10),
            Err(TrackError::PatchTooBig(_, _, _))
        ));
    }

    #[test]
    fn buffer_too_small_errors() {
        let bytes = vec![0u8; 10];
        let v = LumaView {
            bytes: &bytes,
            stride: 16,
            w: 16,
            h: 16,
        };
        let tpl = LumaPatch {
            data: vec![0u8; 9],
            w: 3,
            h: 3,
        };
        assert!(matches!(
            track_ncc(&tpl, &v, (8, 8), 2, 1),
            Err(TrackError::BufferTooSmall { .. })
        ));
    }

    #[test]
    fn uv_pixel_round_trip() {
        let (w, h) = (256u32, 144u32);
        let (px, py) = uv_to_px((0.5, 0.5), w, h);
        assert_eq!((px, py), (128, 72));
        let (u, v) = px_to_uv((128, 72), w, h);
        assert!((u - 0.5).abs() < 1e-6 && (v - 0.5).abs() < 1e-6);
    }
}
