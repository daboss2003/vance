//! `vance-scopes` — video-scope data computation (Phase 6.12).
//!
//! Pure colorimetry: given an RGBA8 frame buffer (the post-grade,
//! post-view-LUT display image, as produced by
//! `Renderer::render_scope_buffer`), [`compute_all`] accumulates the
//! data for four colorist scopes in a SINGLE pass:
//!
//! * **Histogram** — per-channel + luma level counts (256 bins).
//! * **Waveform (luma)** — per-column luma distribution.
//! * **RGB Parade** — three per-column distributions, one per channel.
//! * **Vectorscope** — 2D `Cb`/`Cr` chroma density, with the 75%-bar
//!   target coordinates and the skin-tone line angle.
//!
//! Only raw accumulation **counts** are produced — the UI owns all
//! presentation (log/linear scaling, graticule, channel tints, target
//! boxes). [`ScopeData::to_bytes`] serializes everything to a compact
//! little-endian byte blob for raw-bytes IPC (no JSON stringification of
//! the ~hundreds-of-thousands of integers).

pub mod color;

use serde::{Deserialize, Serialize};

/// Resolution of the scope accumulators (independent of the analyzed
/// frame's pixel size — the frame is sampled into these grids).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeConfig {
    /// Waveform / parade column count (x resolution).
    pub waveform_cols: u32,
    /// Waveform / parade level count (y resolution / signal levels).
    pub levels: u32,
    /// Vectorscope grid side length.
    pub vector_size: u32,
}

impl Default for ScopeConfig {
    fn default() -> Self {
        Self {
            waveform_cols: 256,
            levels: 256,
            vector_size: 256,
        }
    }
}

/// Number of histogram bins (fixed at 256 for 8-bit input).
pub const HIST_BINS: usize = 256;

/// Per-channel + luma level histogram (256 bins each).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Histogram {
    pub r: Vec<u32>,
    pub g: Vec<u32>,
    pub b: Vec<u32>,
    pub luma: Vec<u32>,
}

impl Histogram {
    fn zeroed() -> Self {
        Self {
            r: vec![0; HIST_BINS],
            g: vec![0; HIST_BINS],
            b: vec![0; HIST_BINS],
            luma: vec![0; HIST_BINS],
        }
    }
}

/// A column×level intensity accumulator (`bins[col * levels + level]`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntensityGrid {
    pub cols: u32,
    pub levels: u32,
    pub bins: Vec<u32>,
}

impl IntensityGrid {
    fn zeroed(cols: u32, levels: u32) -> Self {
        Self {
            cols,
            levels,
            bins: vec![0; (cols as usize) * (levels as usize)],
        }
    }
    #[inline]
    fn add(&mut self, col: u32, level: u32) {
        let c = col.min(self.cols - 1);
        let l = level.min(self.levels - 1);
        self.bins[(c as usize) * (self.levels as usize) + l as usize] += 1;
    }
}

/// RGB parade — three per-column distributions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parade {
    pub r: IntensityGrid,
    pub g: IntensityGrid,
    pub b: IntensityGrid,
}

/// Vectorscope chroma density plus overlay geometry. `targets` are the
/// six 75%-bar `(vx, vy)` grid coordinates (same normalization as
/// `bins`), and `skin_line_deg` is the flesh-line angle for the UI.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vectorscope {
    pub size: u32,
    pub bins: Vec<u32>,
    pub targets: Vec<[f32; 2]>,
    pub skin_line_deg: f32,
}

/// Everything the four scopes need for one frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScopeData {
    /// Analyzed frame dimensions (post-downsample).
    pub source_w: u32,
    pub source_h: u32,
    pub histogram: Histogram,
    pub waveform_luma: IntensityGrid,
    pub parade: Parade,
    pub vectorscope: Vectorscope,
}

/// Errors computing scope data.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ScopeError {
    /// The RGBA buffer is shorter than `w * h * 4`.
    #[error("buffer too small: got {got} bytes, need {need} for {w}x{h} RGBA")]
    BufferTooSmall {
        got: usize,
        need: usize,
        w: u32,
        h: u32,
    },
    /// A config dimension was zero.
    #[error("scope config dimension was zero")]
    ZeroDimension,
}

/// The 75%-bar target coordinates for a vectorscope of side `size`,
/// computed with the same `Cb/Cr → grid` normalization the accumulator
/// uses — so overlays land exactly on the density clusters.
pub fn vectorscope_targets(size: u32) -> Vec<[f32; 2]> {
    color::BARS_75
        .iter()
        .map(|&(r, g, b)| {
            let (_, cb, cr) = color::ycbcr_fullrange(r, g, b);
            let (vx, vy) = color::cbcr_to_grid(cb, cr, size);
            [vx, vy]
        })
        .collect()
}

/// Accumulate all four scopes from an RGBA8 buffer (`w * h * 4` bytes,
/// row-major). The buffer is the display image (sRGB code values).
pub fn compute_all(
    rgba: &[u8],
    w: u32,
    h: u32,
    cfg: &ScopeConfig,
) -> Result<ScopeData, ScopeError> {
    if cfg.waveform_cols == 0 || cfg.levels == 0 || cfg.vector_size == 0 {
        return Err(ScopeError::ZeroDimension);
    }
    let need = (w as usize) * (h as usize) * 4;
    if rgba.len() < need {
        return Err(ScopeError::BufferTooSmall {
            got: rgba.len(),
            need,
            w,
            h,
        });
    }

    let mut histogram = Histogram::zeroed();
    let mut waveform_luma = IntensityGrid::zeroed(cfg.waveform_cols, cfg.levels);
    let mut parade = Parade {
        r: IntensityGrid::zeroed(cfg.waveform_cols, cfg.levels),
        g: IntensityGrid::zeroed(cfg.waveform_cols, cfg.levels),
        b: IntensityGrid::zeroed(cfg.waveform_cols, cfg.levels),
    };
    let vsize = cfg.vector_size;
    let mut vbins = vec![0u32; (vsize as usize) * (vsize as usize)];

    let level_scale = (cfg.levels - 1) as f32 / 255.0;
    let cols = cfg.waveform_cols;

    for y in 0..h as usize {
        let row = y * (w as usize) * 4;
        for x in 0..w as usize {
            let i = row + x * 4;
            let r = rgba[i];
            let g = rgba[i + 1];
            let b = rgba[i + 2];

            // Histogram (exact 8-bit bins).
            histogram.r[r as usize] += 1;
            histogram.g[g as usize] += 1;
            histogram.b[b as usize] += 1;
            let yl = color::rec709_luma_codeval(r, g, b);
            histogram.luma[(yl.round() as usize).min(HIST_BINS - 1)] += 1;

            // Waveform / parade: map x → column, code value → level.
            let col = ((x as u64 * cols as u64) / (w as u64)) as u32;
            let lvl = |v: u8| (v as f32 * level_scale).round() as u32;
            waveform_luma.add(col, (yl * level_scale).round() as u32);
            parade.r.add(col, lvl(r));
            parade.g.add(col, lvl(g));
            parade.b.add(col, lvl(b));

            // Vectorscope: full-range Rec.709 Cb/Cr → grid.
            let (_, cb, cr) =
                color::ycbcr_fullrange(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0);
            let (vx, vy) = color::cbcr_to_grid(cb, cr, vsize);
            let vxi = (vx.round().clamp(0.0, (vsize - 1) as f32)) as u32;
            let vyi = (vy.round().clamp(0.0, (vsize - 1) as f32)) as u32;
            vbins[(vyi as usize) * (vsize as usize) + vxi as usize] += 1;
        }
    }

    Ok(ScopeData {
        source_w: w,
        source_h: h,
        histogram,
        waveform_luma,
        parade,
        vectorscope: Vectorscope {
            size: vsize,
            bins: vbins,
            targets: vectorscope_targets(vsize),
            skin_line_deg: color::SKIN_TONE_LINE_DEG,
        },
    })
}

/// Binary blob magic ("SCP1") for the raw-bytes IPC transport.
pub const SCOPE_BLOB_MAGIC: u32 = 0x53435031;

// `to_bytes` uses `bytemuck::cast_slice` (native-endian) for the bulk
// arrays while the TS parser reads them as little-endian. That only
// matches on little-endian hosts — fail the build loudly on a big-endian
// target rather than silently shipping byte-swapped scopes.
const _: () = assert!(
    cfg!(target_endian = "little"),
    "vance-scopes::ScopeData::to_bytes assumes a little-endian target for the IPC blob"
);

impl ScopeData {
    /// Serialize to a compact little-endian byte blob for raw-bytes IPC.
    /// Layout (all multi-byte values little-endian):
    ///
    /// ```text
    /// u32  magic (SCOPE_BLOB_MAGIC)
    /// u32  source_w, source_h
    /// u32  hist_bins (256)
    /// u32  wf_cols, wf_levels
    /// u32  vec_size
    /// u32  targets_count
    /// f32  skin_line_deg
    /// u32[hist_bins] r, g, b, luma            (histogram)
    /// u32[wf_cols*wf_levels] waveform_luma
    /// u32[wf_cols*wf_levels] parade r, g, b
    /// u32[vec_size*vec_size] vectorscope
    /// f32[targets_count*2] targets (vx, vy)
    /// ```
    ///
    /// The TypeScript parser in `ui/src/lib/ipc` mirrors this layout.
    pub fn to_bytes(&self) -> Vec<u8> {
        let wf = &self.waveform_luma;
        let vs = &self.vectorscope;
        let mut out: Vec<u8> = Vec::new();
        let push_u32 = |buf: &mut Vec<u8>, v: u32| buf.extend_from_slice(&v.to_le_bytes());
        push_u32(&mut out, SCOPE_BLOB_MAGIC);
        push_u32(&mut out, self.source_w);
        push_u32(&mut out, self.source_h);
        push_u32(&mut out, HIST_BINS as u32);
        push_u32(&mut out, wf.cols);
        push_u32(&mut out, wf.levels);
        push_u32(&mut out, vs.size);
        push_u32(&mut out, vs.targets.len() as u32);
        out.extend_from_slice(&self.vectorscope.skin_line_deg.to_le_bytes());

        // Bulk u32 arrays — native-endian cast is little-endian on every
        // supported target (x86/ARM macOS/Windows/Linux).
        let push_arr =
            |buf: &mut Vec<u8>, a: &[u32]| buf.extend_from_slice(bytemuck::cast_slice(a));
        push_arr(&mut out, &self.histogram.r);
        push_arr(&mut out, &self.histogram.g);
        push_arr(&mut out, &self.histogram.b);
        push_arr(&mut out, &self.histogram.luma);
        push_arr(&mut out, &wf.bins);
        push_arr(&mut out, &self.parade.r.bins);
        push_arr(&mut out, &self.parade.g.bins);
        push_arr(&mut out, &self.parade.b.bins);
        push_arr(&mut out, &vs.bins);
        for t in &vs.targets {
            out.extend_from_slice(&t[0].to_le_bytes());
            out.extend_from_slice(&t[1].to_le_bytes());
        }
        out
    }
}
