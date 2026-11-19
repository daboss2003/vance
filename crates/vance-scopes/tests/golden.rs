//! Golden tests for the scope accumulators. All assertions are on the
//! PRE-byte-transport grids, so colorimetry is verified at the source.

use vance_scopes::{color, compute_all, vectorscope_targets, ScopeConfig, ScopeData, HIST_BINS};

const CFG: ScopeConfig = ScopeConfig {
    waveform_cols: 256,
    levels: 256,
    vector_size: 256,
};

/// Build a `w`×`h` solid-color RGBA buffer.
fn solid(w: u32, h: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..w * h {
        v.extend_from_slice(&[r, g, b, 255]);
    }
    v
}

fn argmax(bins: &[u32]) -> usize {
    bins.iter()
        .enumerate()
        .max_by_key(|(_, &c)| c)
        .map(|(i, _)| i)
        .unwrap()
}

#[test]
fn black_fills_lowest_bins() {
    // Width == waveform_cols (256) so columns map 1:1 (no sparse gaps).
    let buf = solid(256, 36, 0, 0, 0);
    let d = compute_all(&buf, 256, 36, &CFG).unwrap();
    assert_eq!(argmax(&d.histogram.luma), 0);
    assert_eq!(argmax(&d.histogram.r), 0);
    // Waveform: everything piles at level 0 of every column.
    for c in 0..d.waveform_luma.cols {
        let base = (c as usize) * (d.waveform_luma.levels as usize);
        assert_eq!(d.waveform_luma.bins[base], 36);
    }
}

#[test]
fn white_fills_highest_bins() {
    let buf = solid(64, 36, 255, 255, 255);
    let d = compute_all(&buf, 64, 36, &CFG).unwrap();
    assert_eq!(argmax(&d.histogram.luma), HIST_BINS - 1);
    assert_eq!(argmax(&d.histogram.r), 255);
    assert_eq!(argmax(&d.histogram.g), 255);
    assert_eq!(argmax(&d.histogram.b), 255);
}

#[test]
fn neutral_gray_is_centered_on_vectorscope() {
    // A neutral gray has zero chroma → all energy at the vectorscope
    // center (size/2, size/2).
    let buf = solid(32, 32, 128, 128, 128);
    let d = compute_all(&buf, 32, 32, &CFG).unwrap();
    let s = d.vectorscope.size as usize;
    let center = (s / 2) * s + (s / 2);
    assert_eq!(d.vectorscope.bins[center], 32 * 32);
}

#[test]
fn gray_ramp_makes_a_diagonal_waveform() {
    // A horizontal gray ramp: column x has luma ≈ x. The brightest bin
    // per column should climb left→right.
    let w = 256u32;
    let h = 8u32;
    let mut buf = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..h {
        for x in 0..w {
            let v = x as u8;
            buf.extend_from_slice(&[v, v, v, 255]);
        }
    }
    let d = compute_all(&buf, w, h, &CFG).unwrap();
    let levels = d.waveform_luma.levels as usize;
    let peak_level = |col: usize| {
        let base = col * levels;
        argmax(&d.waveform_luma.bins[base..base + levels])
    };
    // Monotonic non-decreasing across columns.
    let (mut prev, mut increases) = (0usize, 0u32);
    for c in 0..d.waveform_luma.cols as usize {
        let p = peak_level(c);
        assert!(p >= prev, "waveform not monotonic at col {c}: {p} < {prev}");
        if p > prev {
            increases += 1;
        }
        prev = p;
    }
    assert!(
        increases > 200,
        "ramp should climb across most columns: {increases}"
    );
    assert_eq!(peak_level(0), 0);
    assert!(peak_level(255) >= 254);
}

#[test]
fn parade_separates_channels() {
    // Pure red: parade R column maxes high, G/B max at level 0.
    // Width == cols so column 10 maps 1:1 to source x=10.
    let buf = solid(256, 16, 255, 0, 0);
    let d = compute_all(&buf, 256, 16, &CFG).unwrap();
    let levels = d.parade.r.levels as usize;
    let top = |grid: &vance_scopes::IntensityGrid, col: usize| {
        let base = col * levels;
        argmax(&grid.bins[base..base + levels])
    };
    assert_eq!(top(&d.parade.r, 10), 255);
    assert_eq!(top(&d.parade.g, 10), 0);
    assert_eq!(top(&d.parade.b, 10), 0);
}

#[test]
fn seventyfive_bars_land_in_target_boxes() {
    // Each 75% bar color must accumulate at (within ~3px of) its target
    // coordinate — proving the accumulator and the overlay targets share
    // one normalization.
    let size = CFG.vector_size;
    let targets = vectorscope_targets(size);
    for (idx, &(r, g, b)) in color::BARS_75.iter().enumerate() {
        let (rb, gb, bb) = ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8);
        let buf = solid(16, 16, rb, gb, bb);
        let d = compute_all(&buf, 16, 16, &CFG).unwrap();
        let peak = argmax(&d.vectorscope.bins);
        let (px, py) = ((peak % size as usize) as f32, (peak / size as usize) as f32);
        let [tx, ty] = targets[idx];
        let dist = ((px - tx).powi(2) + (py - ty).powi(2)).sqrt();
        assert!(
            dist <= 3.0,
            "bar {idx} peak ({px},{py}) far from target ({tx},{ty}): dist {dist}"
        );
    }
}

#[test]
fn to_bytes_layout_has_expected_size_and_magic() {
    let buf = solid(8, 8, 10, 20, 30);
    let d: ScopeData = compute_all(&buf, 8, 8, &CFG).unwrap();
    let bytes = d.to_bytes();
    // Header: 8 u32 + 1 f32 = 36 bytes.
    let header = 9 * 4;
    let hist = 4 * HIST_BINS * 4;
    let wf = (CFG.waveform_cols * CFG.levels) as usize * 4;
    let parade = 3 * wf;
    let vec = (CFG.vector_size * CFG.vector_size) as usize * 4;
    let targets = d.vectorscope.targets.len() * 2 * 4;
    assert_eq!(bytes.len(), header + hist + wf + parade + vec + targets);
    assert_eq!(
        u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        vance_scopes::SCOPE_BLOB_MAGIC
    );
}

#[test]
fn rejects_short_buffer() {
    let buf = solid(4, 4, 0, 0, 0);
    assert!(compute_all(&buf, 8, 8, &CFG).is_err());
}
