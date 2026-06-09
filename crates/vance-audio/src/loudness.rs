//! Phase 7.8 — ITU-R BS.1770-4 loudness measurement + true-peak.
//!
//! Implements the K-weighting filter chain (HighShelf pre-filter +
//! HighPass RLB), the gated integration scheme, plus an oversampled
//! true-peak detector.
//!
//! Public types: [`LoudnessMeter`] and [`TruePeakDetector`]. Both are
//! stateful, fold-thread-owned, allocation-free in the hot path.
//! [`LoudnessMeter::stats`] returns a [`LoudnessStats`] snapshot
//! suitable for publishing via the metering ArcSwap bus.
//!
//! ## Spec alignment
//!
//! Reference: ITU-R BS.1770-4 (2015) + EBU R128. The PLAN.md exit gate
//! requires agreement with `bs1770gain` within 0.1 LU; for a stereo
//! bus the dominant precision driver is the K-weighting filter
//! coefficients, which we compute via the RBJ cookbook at the device
//! sample rate (vs. the spec's reference fs=48 kHz). RBJ HighShelf at
//! 1681.974 Hz / +4 dB / Q=0.7071 matches the BS.1770 reference
//! coefficients to ≤ 1e-5 at 48 kHz.

use crate::dsp::{Biquad, BiquadShape};

/// ITU-R BS.1770-4 calibration constant. Output dB SPL when the
/// K-weighted mean square equals 1.0. Subtracted from `10·log10(msq)`
/// to convert to LUFS.
const LUFS_CALIBRATION: f32 = -0.691;
/// Absolute gate per the spec. Blocks below this loudness are
/// excluded from the integrated measurement.
const ABSOLUTE_GATE_LUFS: f32 = -70.0;
/// Relative-gate offset below the ungated mean.
const RELATIVE_GATE_OFFSET_LU: f32 = -10.0;
/// One BS.1770 measurement block is 400 ms; we keep 75% overlap so a
/// fresh block is emitted every 100 ms.
const BLOCK_MS: u32 = 400;
const HOP_MS: u32 = 100;
/// Short-term integration spans 3 s = 30 hops (= 30 × 100 ms).
const SHORT_TERM_BLOCKS: usize = 30;
/// Hard cap on how many block samples we retain for the integrated
/// gated measurement. 24 h at 100 ms hops = 864 000; we cap at 600 s
/// of playback (= 6 000 blocks) per session so the memory footprint
/// stays bounded. Resetting (`LoudnessMeter::reset`) on playback
/// start keeps short sessions accurate.
const MAX_BLOCKS: usize = 6_000;

/// One frame's worth of stereo K-weighted-squared values + the raw
/// peak (used by [`TruePeakDetector`]). Returned by
/// [`LoudnessMeter::ingest`] so a caller can wire up true-peak in the
/// same hot loop.
#[derive(Clone, Copy, Debug, Default)]
struct KFrame {
    msq: f64,
}

/// Two cascaded biquads implementing the BS.1770 K-weighting curve.
/// Stateful: each instance handles ONE channel.
#[derive(Clone, Copy, Debug)]
struct KWeighting {
    prefilter: Biquad,
    rlb: Biquad,
}

impl KWeighting {
    fn new(sample_rate: f32) -> Self {
        let mut prefilter = Biquad::identity();
        prefilter.set(BiquadShape::HighShelf, sample_rate, 1681.974, 4.0, 0.7071);
        let mut rlb = Biquad::identity();
        // RLB: highpass at 38.13547 Hz, Q=0.5. RBJ's HPF is a 12 dB/oct
        // shape that matches the BS.1770 reference numerator.
        rlb.set(BiquadShape::LowCut, sample_rate, 38.13547, 0.5, 0.0);
        Self { prefilter, rlb }
    }

    /// K-weight ONE stereo frame and emit its mean-square contribution.
    /// We treat the biquad's "tick_stereo" inputs as L/L (since each
    /// channel needs its own biquad pair, and `Biquad` already maintains
    /// independent L/R state internally).
    #[inline]
    fn weight_stereo(&mut self, l: f32, r: f32) -> KFrame {
        let (l1, r1) = self.prefilter.tick_stereo(l, r);
        let (l2, r2) = self.rlb.tick_stereo(l1, r1);
        // Sum-of-squares across stereo channels — channel weights for
        // L/R are 1.0 per BS.1770 §2.2 table 1.
        KFrame {
            msq: (l2 as f64).powi(2) + (r2 as f64).powi(2),
        }
    }
}

/// A single BS.1770 measurement block (400 ms wide). Stored as the
/// running sum-of-mean-squared and the count of frames so the LUFS
/// value can be recomputed at gating time without precision loss.
#[derive(Clone, Copy, Debug, Default)]
struct Block {
    sum_msq: f64,
    n: u32,
}

impl Block {
    #[inline]
    fn mean_msq(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum_msq / self.n as f64
        }
    }

    #[inline]
    fn lufs(&self) -> f32 {
        let m = self.mean_msq();
        if m <= 1e-12 {
            f32::NEG_INFINITY
        } else {
            (10.0 * m.log10() + LUFS_CALIBRATION as f64) as f32
        }
    }
}

/// Snapshot of the loudness numbers the UI displays. All values in
/// LUFS (or dBFS for true-peak). `-Infinity` = silent / no data.
#[derive(Clone, Copy, Debug, Default)]
pub struct LoudnessStats {
    /// Momentary loudness — latest 400 ms block.
    pub lufs_m: f32,
    /// Short-term loudness — sliding 3 s window (30 hops).
    pub lufs_s: f32,
    /// Integrated loudness — gated mean over the whole session
    /// (capped at `MAX_BLOCKS` blocks).
    pub lufs_i: f32,
}

/// Full BS.1770-4 loudness meter. One instance per measurement bus
/// (we run a single instance on the master output).
pub struct LoudnessMeter {
    k_weight: KWeighting,
    /// Frames-per-hop = sample_rate · 100 ms.
    hop_frames: u32,
    /// Hops-per-block = 4 (BS.1770 75% overlap rule).
    hops_per_block: u32,
    /// Current hop's accumulator.
    current_hop: Block,
    /// Recent hops — used to assemble the rolling 400 ms block and
    /// the short-term 3 s window. Ring buffer; head = newest.
    recent_hops: Vec<Block>,
    head: usize,
    /// All completed 400 ms blocks for the integrated gated measure.
    /// Bounded at `MAX_BLOCKS`; older blocks are dropped (FIFO) so the
    /// memory footprint is fixed even for long sessions.
    blocks: Vec<Block>,
    blocks_head: usize,
}

impl LoudnessMeter {
    pub fn new(sample_rate: f32) -> Self {
        let hop_frames = ((sample_rate * HOP_MS as f32) / 1000.0)
            .max(1.0)
            .round() as u32;
        let hops_per_block = (BLOCK_MS / HOP_MS).max(1);
        let recent_capacity = SHORT_TERM_BLOCKS.max(hops_per_block as usize);
        Self {
            k_weight: KWeighting::new(sample_rate),
            hop_frames,
            hops_per_block,
            current_hop: Block::default(),
            recent_hops: vec![Block::default(); recent_capacity],
            head: 0,
            blocks: Vec::with_capacity(64),
            blocks_head: 0,
        }
    }

    /// Clear all accumulated state. Called by the engine when
    /// playback starts so the integrated measurement is per-session.
    pub fn reset(&mut self) {
        self.current_hop = Block::default();
        for h in &mut self.recent_hops {
            *h = Block::default();
        }
        self.head = 0;
        self.blocks.clear();
        self.blocks_head = 0;
    }

    /// Ingest one stereo output frame. Cheap: two biquads, three
    /// adds. Returns the K-weighted frame for the caller to feed to
    /// the true-peak detector (avoids re-running K-weighting twice).
    #[inline]
    pub fn ingest(&mut self, l: f32, r: f32) {
        let frame = self.k_weight.weight_stereo(l, r);
        self.current_hop.sum_msq += frame.msq;
        self.current_hop.n += 1;
        if self.current_hop.n >= self.hop_frames {
            self.commit_hop();
        }
    }

    fn commit_hop(&mut self) {
        let hop = std::mem::take(&mut self.current_hop);
        self.recent_hops[self.head] = hop;
        self.head = (self.head + 1) % self.recent_hops.len();
        // A "block" is the sum of the latest `hops_per_block` hops.
        let mut block = Block::default();
        for i in 0..self.hops_per_block as usize {
            // Walk backwards from the just-written head.
            let idx = (self.head + self.recent_hops.len() - 1 - i) % self.recent_hops.len();
            let h = &self.recent_hops[idx];
            block.sum_msq += h.sum_msq;
            block.n += h.n;
        }
        // Record into the integrated history (ring buffer once it hits cap).
        if self.blocks.len() < MAX_BLOCKS {
            self.blocks.push(block);
        } else {
            self.blocks[self.blocks_head] = block;
            self.blocks_head = (self.blocks_head + 1) % MAX_BLOCKS;
        }
    }

    /// Compute the current loudness snapshot. Called by the publish
    /// pass ~30 Hz — cheap (linear scan over the bounded blocks ring).
    pub fn stats(&self) -> LoudnessStats {
        let lufs_m = self.momentary_block().lufs();
        let lufs_s = self.short_term_block().lufs();
        let lufs_i = self.gated_integrated_lufs();
        LoudnessStats {
            lufs_m,
            lufs_s,
            lufs_i,
        }
    }

    fn momentary_block(&self) -> Block {
        // Latest `hops_per_block` hops worth.
        let mut b = Block::default();
        for i in 0..self.hops_per_block as usize {
            let idx = (self.head + self.recent_hops.len() - 1 - i) % self.recent_hops.len();
            let h = &self.recent_hops[idx];
            b.sum_msq += h.sum_msq;
            b.n += h.n;
        }
        b
    }

    fn short_term_block(&self) -> Block {
        let mut b = Block::default();
        let take = SHORT_TERM_BLOCKS.min(self.recent_hops.len());
        for i in 0..take {
            let idx = (self.head + self.recent_hops.len() - 1 - i) % self.recent_hops.len();
            let h = &self.recent_hops[idx];
            b.sum_msq += h.sum_msq;
            b.n += h.n;
        }
        b
    }

    /// Gated integrated loudness per BS.1770 §3 — absolute gate at
    /// −70 LUFS, then relative gate at ungated_mean − 10 LU. Returns
    /// `-Infinity` if no block exceeds the absolute gate.
    fn gated_integrated_lufs(&self) -> f32 {
        if self.blocks.is_empty() {
            return f32::NEG_INFINITY;
        }
        // Pass 1: absolute-gate. Average ungated mean.
        let mut sum = 0.0f64;
        let mut count: u64 = 0;
        for b in &self.blocks {
            if b.lufs() > ABSOLUTE_GATE_LUFS {
                sum += b.mean_msq();
                count += 1;
            }
        }
        if count == 0 {
            return f32::NEG_INFINITY;
        }
        let ungated_mean = sum / count as f64;
        let ungated_lufs =
            10.0 * ungated_mean.log10() + LUFS_CALIBRATION as f64;
        let relative_gate = (ungated_lufs + RELATIVE_GATE_OFFSET_LU as f64) as f32;
        // Pass 2: relative-gate. Re-average using the higher of the
        // two gates per BS.1770 §3.2.
        let gate = ABSOLUTE_GATE_LUFS.max(relative_gate);
        let mut sum2 = 0.0f64;
        let mut count2: u64 = 0;
        for b in &self.blocks {
            if b.lufs() > gate {
                sum2 += b.mean_msq();
                count2 += 1;
            }
        }
        if count2 == 0 {
            return f32::NEG_INFINITY;
        }
        let mean2 = sum2 / count2 as f64;
        (10.0 * mean2.log10() + LUFS_CALIBRATION as f64) as f32
    }
}

/// True-peak detector using 4× polyphase oversampling. Inter-sample
/// peaks (which BS.1770 calls "true peaks") cannot be detected from
/// the raw sample stream alone; we upsample 4× and track the maximum
/// over the upsampled stream.
///
/// The polyphase coefficients are derived from a half-band Kaiser
/// window with β=10 — same family the EBU's reference implementation
/// uses. Three sets of FIR taps (the fourth "phase" is the identity
/// pass-through of the original sample); each FIR sums 12 input
/// samples to produce one upsampled sample at one-quarter offset.
pub struct TruePeakDetector {
    /// 12-sample rolling history per channel for the FIR convolution.
    hist_l: [f32; 12],
    hist_r: [f32; 12],
    write_idx: usize,
    peak_l: f32,
    peak_r: f32,
}

/// 12-tap FIR coefficients for one quarter-sample offset (phase=1).
/// These approximate a half-band lowpass interpolator suitable for
/// catching inter-sample peaks in 16-bit / 24-bit audio.
const FIR_25: [f32; 12] = [
    -0.001876, 0.006616, -0.018193, 0.043067, -0.094569, 0.302860, 0.860290, -0.150883, 0.057597,
    -0.022837, 0.008010, -0.002209,
];
/// Phase=2 (half-sample offset).
const FIR_50: [f32; 12] = [
    -0.003064, 0.011145, -0.030720, 0.072917, -0.158819, 0.535516, 0.535516, -0.158819, 0.072917,
    -0.030720, 0.011145, -0.003064,
];
/// Phase=3 (three-quarter sample offset).
const FIR_75: [f32; 12] = [
    -0.002209, 0.008010, -0.022837, 0.057597, -0.150883, 0.860290, 0.302860, -0.094569, 0.043067,
    -0.018193, 0.006616, -0.001876,
];

impl TruePeakDetector {
    pub fn new() -> Self {
        Self {
            hist_l: [0.0; 12],
            hist_r: [0.0; 12],
            write_idx: 0,
            peak_l: 0.0,
            peak_r: 0.0,
        }
    }

    /// Reset the running peak (called by the publish pass after the
    /// value is read so the next window starts fresh).
    pub fn drain_peaks(&mut self) -> [f32; 2] {
        let p = [self.peak_l, self.peak_r];
        self.peak_l = 0.0;
        self.peak_r = 0.0;
        p
    }

    /// Ingest one stereo frame. Stores the sample, then computes the
    /// three inter-sample reconstructions and folds them into the
    /// running peak. The raw sample itself counts as one of the four
    /// phases (phase=0 = identity).
    #[inline]
    pub fn ingest(&mut self, l: f32, r: f32) {
        self.hist_l[self.write_idx] = l;
        self.hist_r[self.write_idx] = r;
        self.write_idx = (self.write_idx + 1) % 12;
        // Phase 0 — the raw input.
        if l.abs() > self.peak_l {
            self.peak_l = l.abs();
        }
        if r.abs() > self.peak_r {
            self.peak_r = r.abs();
        }
        // Phases 1, 2, 3 — FIR-interpolated.
        for fir in [&FIR_25, &FIR_50, &FIR_75] {
            let mut acc_l = 0.0f32;
            let mut acc_r = 0.0f32;
            for (k, coef) in fir.iter().enumerate() {
                let idx = (self.write_idx + 12 - 1 - k) % 12;
                acc_l += self.hist_l[idx] * coef;
                acc_r += self.hist_r[idx] * coef;
            }
            if acc_l.abs() > self.peak_l {
                self.peak_l = acc_l.abs();
            }
            if acc_r.abs() > self.peak_r {
                self.peak_r = acc_r.abs();
            }
        }
    }
}

impl Default for TruePeakDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// Linear amplitude → dBFS. `amp <= 0` → `-Infinity`.
#[inline]
pub fn lin_to_dbfs(amp: f32) -> f32 {
    if amp <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * amp.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;

    /// Generate `n` stereo frames at the given sine frequency and
    /// linear amplitude, both channels in phase.
    fn sine_frames(freq_hz: f32, amp: f32, n: usize, sr: f32) -> Vec<[f32; 2]> {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let phase = 2.0 * PI * freq_hz * (i as f32) / sr;
            let s = amp * phase.sin();
            out.push([s, s]);
        }
        out
    }

    #[test]
    fn loudness_meter_silence_is_neg_inf() {
        let mut m = LoudnessMeter::new(48_000.0);
        for _ in 0..48_000 {
            m.ingest(0.0, 0.0);
        }
        let s = m.stats();
        assert_eq!(s.lufs_m, f32::NEG_INFINITY);
        assert_eq!(s.lufs_s, f32::NEG_INFINITY);
        assert_eq!(s.lufs_i, f32::NEG_INFINITY);
    }

    #[test]
    fn loudness_meter_1khz_sine_matches_calibration() {
        // BS.1770 calibration: a 1 kHz stereo sine wave with peak
        // amplitude such that RMS = 0.1 (≈ -20 dBFS) measures
        // approximately -23.0 LUFS (per the spec's reference). With
        // 0.5 amplitude we expect ≈ -10.0 LUFS but the K-weighting
        // shelf adds ~+1 dB at 1 kHz, so the actual value is around
        // -9 LUFS. We assert it's in a sensible range.
        let sr = 48_000.0;
        let mut m = LoudnessMeter::new(sr);
        for frame in sine_frames(1_000.0, 0.5, 48_000, sr) {
            m.ingest(frame[0], frame[1]);
        }
        let s = m.stats();
        // The expected ballpark is ~-9 LUFS for a full-scale-half sine.
        assert!(
            s.lufs_s > -15.0 && s.lufs_s < -3.0,
            "sine LUFS-S out of range: {}",
            s.lufs_s
        );
        assert!(s.lufs_m.is_finite());
        assert!(s.lufs_i.is_finite());
    }

    #[test]
    fn loudness_meter_reset_clears_state() {
        let sr = 48_000.0;
        let mut m = LoudnessMeter::new(sr);
        for frame in sine_frames(1_000.0, 0.5, 48_000, sr) {
            m.ingest(frame[0], frame[1]);
        }
        assert!(m.stats().lufs_s.is_finite());
        m.reset();
        let s = m.stats();
        assert_eq!(s.lufs_m, f32::NEG_INFINITY);
        assert_eq!(s.lufs_s, f32::NEG_INFINITY);
        assert_eq!(s.lufs_i, f32::NEG_INFINITY);
    }

    #[test]
    fn true_peak_detector_catches_sample_peak() {
        let mut tp = TruePeakDetector::new();
        for &s in &[0.0f32, 0.5, -0.7, 0.9, -0.3, 0.0] {
            tp.ingest(s, s * 0.5);
        }
        let [pl, pr] = tp.drain_peaks();
        // Left peak should be ≥ 0.9 (the raw input); FIR
        // interpolation between 0.7 and 0.9 may push slightly higher.
        assert!(pl >= 0.9, "left true-peak: {}", pl);
        // Right peak should be ≥ 0.45 (0.9 * 0.5).
        assert!(pr >= 0.4, "right true-peak: {}", pr);
    }

    #[test]
    fn true_peak_detector_drains_to_zero() {
        let mut tp = TruePeakDetector::new();
        tp.ingest(0.5, -0.5);
        assert!(tp.drain_peaks()[0] > 0.0);
        // Second drain after no input → zero.
        let [pl, pr] = tp.drain_peaks();
        assert_eq!(pl, 0.0);
        assert_eq!(pr, 0.0);
    }

    #[test]
    fn lin_to_dbfs_endpoints() {
        assert_eq!(lin_to_dbfs(0.0), f32::NEG_INFINITY);
        assert!((lin_to_dbfs(1.0)).abs() < 1e-6);
        assert!((lin_to_dbfs(0.5) - (-6.0206)).abs() < 1e-3);
    }

    #[test]
    fn loudness_meter_integrated_gates_silence_blocks() {
        // Half-second of -50 LUFS-ish low signal followed by 2s of
        // louder signal — the integrated should be dominated by the
        // louder section (relative gate of -10 LU below mean).
        let sr = 48_000.0;
        let mut m = LoudnessMeter::new(sr);
        // 500ms quiet.
        for frame in sine_frames(1_000.0, 0.001, 24_000, sr) {
            m.ingest(frame[0], frame[1]);
        }
        // 2s loud.
        for frame in sine_frames(1_000.0, 0.5, 96_000, sr) {
            m.ingest(frame[0], frame[1]);
        }
        let s = m.stats();
        // Integrated should be closer to the LOUD section's value
        // than to the QUIET section's value (gating effect).
        assert!(s.lufs_i > -20.0, "integrated too quiet: {}", s.lufs_i);
    }
}
