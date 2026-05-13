//! 3D look-up tables (LUTs) — parse from `.cube` and `.3dl`.
//!
//! A 3D LUT is a uniform sampling of a color transform: at every
//! `(r, g, b)` corner of a `N × N × N` cube (typically 17, 32, or
//! 64), the file lists the output RGB. The renderer (Phase 6.4)
//! uploads the cube as a 3D texture and samples it trilinearly per
//! pixel. v1 covers the two formats colorists actually exchange:
//!
//! - **Adobe `.cube`** (most common). ASCII; `LUT_3D_SIZE N` header
//!   followed by `N^3` lines of `r g b` floats. Optional
//!   `DOMAIN_MIN` / `DOMAIN_MAX` for non-unit input ranges.
//! - **Autodesk `.3dl`** (Lustre / Flame). ASCII; an optional
//!   first row of integer "shaper" values (one per input axis), then
//!   `N^3` lines of three integers scaled by the max (commonly
//!   10-bit = 1023 or 12-bit = 4095).
//!
//! Both end up as the same in-memory `Lut3d`. The renderer doesn't
//! need to know which format produced it.

use std::path::Path;

/// One 3D look-up table. `values` holds `size³` RGB triples in the
/// scan order that varies R fastest, then G, then B (the convention
/// `.cube` uses; `.3dl` is normalized into the same order at parse
/// time).
#[derive(Clone, Debug, PartialEq)]
pub struct Lut3d {
    /// Cube side length (`size = 17` means 17×17×17 = 4913 entries).
    pub size: u32,
    /// `size³` RGB triples, flat-packed as `[r0, g0, b0, r1, g1, b1, …]`.
    pub values: Vec<f32>,
    /// Lower bound of the input domain (`r, g, b`). Default 0.
    pub domain_min: [f32; 3],
    /// Upper bound of the input domain (`r, g, b`). Default 1.
    pub domain_max: [f32; 3],
}

impl Lut3d {
    /// Identity LUT (output == input) at the given size. Useful as
    /// a default / null effect.
    pub fn identity(size: u32) -> Self {
        let n = size as usize;
        let mut values = Vec::with_capacity(n * n * n * 3);
        let denom = (n - 1).max(1) as f32;
        for b in 0..n {
            for g in 0..n {
                for r in 0..n {
                    values.push(r as f32 / denom);
                    values.push(g as f32 / denom);
                    values.push(b as f32 / denom);
                }
            }
        }
        Self {
            size,
            values,
            domain_min: [0.0; 3],
            domain_max: [1.0; 3],
        }
    }

    /// Total entries — `size³`.
    pub fn entry_count(&self) -> usize {
        (self.size as usize).pow(3)
    }
}

/// Errors a LUT parser can return.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LutError {
    /// IO error reading the file.
    #[error("read failed: {0}")]
    Read(#[from] std::io::Error),

    /// Couldn't determine the format from the extension or content.
    #[error("unsupported LUT format: {0}")]
    UnsupportedFormat(String),

    /// Required `LUT_3D_SIZE` (.cube) or row dimension (.3dl) was missing.
    #[error("missing or invalid LUT size")]
    BadSize,

    /// Body row count didn't match `size³` — the LUT was probably
    /// truncated on disk.
    #[error("expected {expected} entries, found {got}")]
    EntryCountMismatch {
        /// Number of `r g b` rows the header promised (`size³`).
        expected: usize,
        /// Rows actually parsed from the body.
        got: usize,
    },

    /// A `r g b` row didn't have three numeric fields.
    #[error("malformed row at line {line}: {raw:?}")]
    MalformedRow {
        /// 1-based source-line number for the failing row.
        line: usize,
        /// The raw line content (truncated for log readability).
        raw: String,
    },
}

/// Dispatch on the file extension and parse. Returns `Lut3d`.
pub fn load_from_path(path: &Path) -> Result<Lut3d, LutError> {
    let text = std::fs::read_to_string(path)?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "cube" => parse_cube(&text),
        "3dl" => parse_3dl(&text),
        other => Err(LutError::UnsupportedFormat(other.into())),
    }
}

/// Parse an Adobe `.cube` file body. Handles `LUT_3D_SIZE`,
/// `DOMAIN_MIN`, `DOMAIN_MAX`, comments (`#`), blank lines.
/// 1D LUTs (`LUT_1D_SIZE`) aren't covered in v1 — colorists almost
/// always export 3D for grading.
pub fn parse_cube(text: &str) -> Result<Lut3d, LutError> {
    let mut size: Option<u32> = None;
    let mut domain_min = [0.0_f32; 3];
    let mut domain_max = [1.0_f32; 3];
    let mut values: Vec<f32> = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("LUT_3D_SIZE") {
            size = rest
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|&n| n > 0 && n <= 256);
            if size.is_none() {
                return Err(LutError::BadSize);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("DOMAIN_MIN") {
            domain_min = parse_three(rest, line_no, raw)?;
            continue;
        }
        if let Some(rest) = line.strip_prefix("DOMAIN_MAX") {
            domain_max = parse_three(rest, line_no, raw)?;
            continue;
        }
        if line.starts_with("LUT_1D_SIZE") || line.starts_with("TITLE") {
            // Not used at the engine level (TITLE is just metadata).
            continue;
        }
        // Otherwise it's a data row.
        let rgb = parse_three(line, line_no, raw)?;
        values.extend_from_slice(&rgb);
    }
    let size = size.ok_or(LutError::BadSize)?;
    let expected = (size as usize).pow(3) * 3;
    if values.len() != expected {
        return Err(LutError::EntryCountMismatch {
            expected: expected / 3,
            got: values.len() / 3,
        });
    }
    Ok(Lut3d {
        size,
        values,
        domain_min,
        domain_max,
    })
}

/// Parse an Autodesk Lustre `.3dl` file. v1 supports the common
/// shape: optional first non-comment row holds `N` integers (the
/// "shaper" — input mesh positions; we ignore the values themselves
/// and just take its length as `N`), then `N³` rows of three
/// integers in B-major / G-mid / R-minor order, scaled by the
/// implied bit depth (max value observed in the file).
pub fn parse_3dl(text: &str) -> Result<Lut3d, LutError> {
    let mut size: Option<u32> = None;
    let mut rows: Vec<(u32, u32, u32)> = Vec::new();
    let mut max_seen: u32 = 0;
    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if size.is_none() {
            // First non-comment row is the mesh definition. Its
            // length is the cube size N.
            if parts.len() < 2 {
                return Err(LutError::BadSize);
            }
            size = Some(parts.len() as u32);
            continue;
        }
        if parts.len() != 3 {
            return Err(LutError::MalformedRow {
                line: line_no,
                raw: raw.chars().take(120).collect(),
            });
        }
        let r: u32 = parts[0]
            .parse()
            .map_err(|_| LutError::MalformedRow {
                line: line_no,
                raw: raw.chars().take(120).collect(),
            })?;
        let g: u32 = parts[1]
            .parse()
            .map_err(|_| LutError::MalformedRow {
                line: line_no,
                raw: raw.chars().take(120).collect(),
            })?;
        let b: u32 = parts[2]
            .parse()
            .map_err(|_| LutError::MalformedRow {
                line: line_no,
                raw: raw.chars().take(120).collect(),
            })?;
        max_seen = max_seen.max(r).max(g).max(b);
        rows.push((r, g, b));
    }
    let size = size.ok_or(LutError::BadSize)?;
    let expected = (size as usize).pow(3);
    if rows.len() != expected {
        return Err(LutError::EntryCountMismatch {
            expected,
            got: rows.len(),
        });
    }
    // Guess the bit depth from the max value (1023 = 10-bit, 4095 =
    // 12-bit, etc.). Round up to the next power-of-two minus one.
    let scale = if max_seen <= 255 {
        255.0
    } else if max_seen <= 1023 {
        1023.0
    } else if max_seen <= 4095 {
        4095.0
    } else if max_seen <= 16383 {
        16383.0
    } else {
        65535.0
    };
    // The `.3dl` row order is B-major: outer loop B, middle G, inner R.
    // Convert into the .cube convention (R-fastest) for a single
    // in-memory layout the renderer can rely on.
    let n = size as usize;
    let mut values = vec![0.0_f32; expected * 3];
    for (idx, (r, g, b)) in rows.iter().enumerate() {
        // idx → (i_b, i_g, i_r) in B-major.
        let i_r = idx % n;
        let i_g = (idx / n) % n;
        let i_b = idx / (n * n);
        // Output offset for R-fastest convention.
        let out_idx = ((i_b * n + i_g) * n + i_r) * 3;
        values[out_idx] = *r as f32 / scale;
        values[out_idx + 1] = *g as f32 / scale;
        values[out_idx + 2] = *b as f32 / scale;
    }
    Ok(Lut3d {
        size,
        values,
        domain_min: [0.0; 3],
        domain_max: [1.0; 3],
    })
}

fn parse_three(s: &str, line: usize, raw: &str) -> Result<[f32; 3], LutError> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 3 {
        return Err(LutError::MalformedRow {
            line,
            raw: raw.chars().take(120).collect(),
        });
    }
    let r = parts[0].parse::<f32>().map_err(|_| LutError::MalformedRow {
        line,
        raw: raw.chars().take(120).collect(),
    })?;
    let g = parts[1].parse::<f32>().map_err(|_| LutError::MalformedRow {
        line,
        raw: raw.chars().take(120).collect(),
    })?;
    let b = parts[2].parse::<f32>().map_err(|_| LutError::MalformedRow {
        line,
        raw: raw.chars().take(120).collect(),
    })?;
    Ok([r, g, b])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_3_yields_8_corners() {
        let lut = Lut3d::identity(2);
        assert_eq!(lut.size, 2);
        assert_eq!(lut.entry_count(), 8);
        // First entry = (0, 0, 0), last = (1, 1, 1).
        assert_eq!(&lut.values[0..3], &[0.0, 0.0, 0.0]);
        assert_eq!(&lut.values[lut.values.len() - 3..], &[1.0, 1.0, 1.0]);
    }

    #[test]
    fn parse_minimal_cube() {
        let body = "\
# tiny test LUT
LUT_3D_SIZE 2
0.0 0.0 0.0
1.0 0.0 0.0
0.0 1.0 0.0
1.0 1.0 0.0
0.0 0.0 1.0
1.0 0.0 1.0
0.0 1.0 1.0
1.0 1.0 1.0
";
        let lut = parse_cube(body).expect("parse");
        assert_eq!(lut.size, 2);
        assert_eq!(lut.entry_count(), 8);
        assert_eq!(&lut.values[0..3], &[0.0, 0.0, 0.0]);
        assert_eq!(&lut.values[3..6], &[1.0, 0.0, 0.0]);
        assert_eq!(&lut.values[21..24], &[1.0, 1.0, 1.0]);
    }

    #[test]
    fn parse_cube_with_domain_overrides() {
        let body = "\
LUT_3D_SIZE 2
DOMAIN_MIN -0.5 -0.5 -0.5
DOMAIN_MAX 1.5 1.5 1.5
0.0 0.0 0.0
1.0 0.0 0.0
0.0 1.0 0.0
1.0 1.0 0.0
0.0 0.0 1.0
1.0 0.0 1.0
0.0 1.0 1.0
1.0 1.0 1.0
";
        let lut = parse_cube(body).expect("parse");
        assert_eq!(lut.domain_min, [-0.5; 3]);
        assert_eq!(lut.domain_max, [1.5; 3]);
    }

    #[test]
    fn parse_cube_rejects_truncated() {
        let body = "LUT_3D_SIZE 2\n0.0 0.0 0.0\n";
        let err = parse_cube(body).unwrap_err();
        assert!(matches!(err, LutError::EntryCountMismatch { .. }));
    }

    #[test]
    fn parse_minimal_3dl() {
        // 2³ = 8 entries; shaper row defines size = 2.
        let body = "\
0 1023
0 0 0
1023 0 0
0 1023 0
1023 1023 0
0 0 1023
1023 0 1023
0 1023 1023
1023 1023 1023
";
        let lut = parse_3dl(body).expect("parse");
        assert_eq!(lut.size, 2);
        // R-fastest output convention: first entry = (0, 0, 0),
        // second = (1, 0, 0). (The .3dl source is B-major; we
        // reordered to R-fastest internally.)
        assert_eq!(&lut.values[0..3], &[0.0, 0.0, 0.0]);
        assert_eq!(&lut.values[3..6], &[1.0, 0.0, 0.0]);
        assert_eq!(&lut.values[21..24], &[1.0, 1.0, 1.0]);
    }
}
