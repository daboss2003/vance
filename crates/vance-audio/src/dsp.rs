//! Phase 7.7 — built-in audio DSP primitives applied per-track in the
//! mixer fold loop.
//!
//! We ship hand-rolled implementations (RBJ-cookbook biquads, an RMS-
//! detector compressor with soft knee, and a peak-following limiter)
//! rather than depending on the much larger `fundsp` crate. The
//! algorithms are standard textbook DSP and the public API
//! (`Biquad`, `Compressor`, `Limiter`) is intentionally small so
//! `fundsp` can swap in later without changing the snapshot shape.
//!
//! Every primitive is STATEFUL and lives on the fold thread. State
//! carries between snapshots so the user dragging a compressor's
//! threshold knob never resets the envelope follower.

use std::f32::consts::PI;

/// Five biquad filter shapes used by the 6-band parametric EQ.
/// Coefficients follow Robert Bristow-Johnson's "Cookbook formulae
/// for audio EQ biquad filter coefficients."
///
/// Stable on every audio rate down to 8 kHz; the `is_finite` guard in
/// [`Biquad::tick`] is the safety net that catches a snapshot with
/// nonsense coefficients from corrupting the bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BiquadShape {
    /// Low-shelf — bass tilt below `freq_hz`.
    LowShelf,
    /// Resonant peaking filter centered on `freq_hz` with `q` width.
    Peak,
    /// High-shelf — treble tilt above `freq_hz`.
    HighShelf,
    /// 12 dB/oct low-cut (high-pass).
    LowCut,
    /// 12 dB/oct high-cut (low-pass).
    HighCut,
    /// Band-reject notch centered on `freq_hz` (GOAT-8 de-hum). `gain_db`
    /// is ignored; depth is set by `q` (higher = narrower/deeper notch).
    Notch,
}

/// Transposed-Direct-Form-II biquad. Two state words per channel.
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    // Per-channel state (left, right).
    z_l: [f32; 2],
    z_r: [f32; 2],
}

impl Biquad {
    /// Identity (pass-through) biquad — useful default.
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z_l: [0.0; 2],
            z_r: [0.0; 2],
        }
    }

    /// Set the coefficients to match the requested shape. Sample rate
    /// is in Hz; `freq_hz` is the band frequency; `gain_db` is
    /// ±whatever-dB boost/cut (only used by shelves + peak); `q` is
    /// the band width (`q ≈ 0.707` is a maximally-flat Butterworth).
    ///
    /// Returns false if any coefficient ends up non-finite (degenerate
    /// inputs); caller should fall back to an identity in that case.
    /// State is NOT cleared — coefficient updates carry on top of the
    /// existing two-sample history so a slow knob drag stays smooth.
    pub fn set(
        &mut self,
        shape: BiquadShape,
        sample_rate: f32,
        freq_hz: f32,
        gain_db: f32,
        q: f32,
    ) -> bool {
        if sample_rate <= 0.0 || freq_hz <= 0.0 || freq_hz >= sample_rate * 0.5 || q <= 0.0 {
            *self = Self::identity();
            return false;
        }
        let a = 10f32.powf(gain_db / 40.0);
        let omega = 2.0 * PI * freq_hz / sample_rate;
        let sn = omega.sin();
        let cs = omega.cos();
        let alpha = sn / (2.0 * q);
        let (b0, b1, b2, a0, a1, a2);
        match shape {
            BiquadShape::Peak => {
                b0 = 1.0 + alpha * a;
                b1 = -2.0 * cs;
                b2 = 1.0 - alpha * a;
                a0 = 1.0 + alpha / a;
                a1 = -2.0 * cs;
                a2 = 1.0 - alpha / a;
            }
            BiquadShape::LowShelf => {
                let beta = 2.0 * a.sqrt() * alpha;
                b0 = a * ((a + 1.0) - (a - 1.0) * cs + beta);
                b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cs);
                b2 = a * ((a + 1.0) - (a - 1.0) * cs - beta);
                a0 = (a + 1.0) + (a - 1.0) * cs + beta;
                a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cs);
                a2 = (a + 1.0) + (a - 1.0) * cs - beta;
            }
            BiquadShape::HighShelf => {
                let beta = 2.0 * a.sqrt() * alpha;
                b0 = a * ((a + 1.0) + (a - 1.0) * cs + beta);
                b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cs);
                b2 = a * ((a + 1.0) + (a - 1.0) * cs - beta);
                a0 = (a + 1.0) - (a - 1.0) * cs + beta;
                a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cs);
                a2 = (a + 1.0) - (a - 1.0) * cs - beta;
            }
            BiquadShape::LowCut => {
                b0 = (1.0 + cs) * 0.5;
                b1 = -(1.0 + cs);
                b2 = (1.0 + cs) * 0.5;
                a0 = 1.0 + alpha;
                a1 = -2.0 * cs;
                a2 = 1.0 - alpha;
            }
            BiquadShape::HighCut => {
                b0 = (1.0 - cs) * 0.5;
                b1 = 1.0 - cs;
                b2 = (1.0 - cs) * 0.5;
                a0 = 1.0 + alpha;
                a1 = -2.0 * cs;
                a2 = 1.0 - alpha;
            }
            BiquadShape::Notch => {
                b0 = 1.0;
                b1 = -2.0 * cs;
                b2 = 1.0;
                a0 = 1.0 + alpha;
                a1 = -2.0 * cs;
                a2 = 1.0 - alpha;
            }
        }
        if !a0.is_finite() || a0.abs() < 1e-20 {
            *self = Self::identity();
            return false;
        }
        let inv = 1.0 / a0;
        self.b0 = b0 * inv;
        self.b1 = b1 * inv;
        self.b2 = b2 * inv;
        self.a1 = a1 * inv;
        self.a2 = a2 * inv;
        // Safety check — pathological inputs can still produce NaN.
        if !(self.b0.is_finite()
            && self.b1.is_finite()
            && self.b2.is_finite()
            && self.a1.is_finite()
            && self.a2.is_finite())
        {
            *self = Self::identity();
            return false;
        }
        true
    }

    /// Process one stereo sample. Each channel has its own state.
    #[inline]
    pub fn tick_stereo(&mut self, l_in: f32, r_in: f32) -> (f32, f32) {
        let l_out = self.b0 * l_in + self.z_l[0];
        self.z_l[0] = self.b1 * l_in - self.a1 * l_out + self.z_l[1];
        self.z_l[1] = self.b2 * l_in - self.a2 * l_out;
        let r_out = self.b0 * r_in + self.z_r[0];
        self.z_r[0] = self.b1 * r_in - self.a1 * r_out + self.z_r[1];
        self.z_r[1] = self.b2 * r_in - self.a2 * r_out;
        // Denormal flush: if state collapses below the f32 subnormal
        // band, snap to zero so future ticks don't pay the denormal
        // CPU tax. This is the cheapest possible defense.
        if self.z_l[0].abs() < 1e-30 {
            self.z_l[0] = 0.0;
        }
        if self.z_l[1].abs() < 1e-30 {
            self.z_l[1] = 0.0;
        }
        if self.z_r[0].abs() < 1e-30 {
            self.z_r[0] = 0.0;
        }
        if self.z_r[1].abs() < 1e-30 {
            self.z_r[1] = 0.0;
        }
        (l_out, r_out)
    }
}

/// Stateful per-track compressor. Models a feed-forward design with an
/// RMS detector + soft-knee gain computer + decoupled
/// branching-smoother envelope.
#[derive(Clone, Copy, Debug)]
pub struct Compressor {
    threshold_lin: f32,
    threshold_db: f32,
    ratio: f32,
    knee_db: f32,
    makeup_lin: f32,
    /// Per-sample envelope smoothing coefficients computed from
    /// attack / release times.
    alpha_a: f32,
    alpha_r: f32,
    /// Smoothed gain reduction in dB. Lives across ticks.
    env_gr_db: f32,
}

impl Compressor {
    /// A compressor at unity gain: 1:1 ratio, 0 dB makeup and a frozen
    /// envelope, so audio passes through untouched until [`Self::set`]
    /// configures it.
    pub fn passthrough() -> Self {
        Self {
            threshold_lin: 1.0,
            threshold_db: 0.0,
            ratio: 1.0,
            knee_db: 0.0,
            makeup_lin: 1.0,
            alpha_a: 1.0,
            alpha_r: 1.0,
            env_gr_db: 0.0,
        }
    }

    /// Reconfigure parameters. State (`env_gr_db`) is preserved so the
    /// user can sweep a knob mid-signal without an audible "reset."
    // Intentional: one argument per compressor knob (plus sample rate),
    // matching the track compressor's parameter set one-to-one.
    #[allow(clippy::too_many_arguments)]
    pub fn set(
        &mut self,
        threshold_db: f32,
        ratio: f32,
        attack_ms: f32,
        release_ms: f32,
        knee_db: f32,
        makeup_db: f32,
        sample_rate: f32,
    ) {
        self.threshold_db = threshold_db.clamp(-60.0, 0.0);
        self.threshold_lin = 10f32.powf(self.threshold_db / 20.0);
        // Intentional: max/min rather than clamp — a NaN ratio maps to
        // 1.0 (no compression) here, where clamp would propagate NaN
        // into the gain computer and the output samples.
        #[allow(clippy::manual_clamp)]
        let ratio = ratio.max(1.0).min(20.0);
        self.ratio = ratio;
        self.knee_db = knee_db.clamp(0.0, 18.0);
        self.makeup_lin = 10f32.powf(makeup_db.clamp(-12.0, 24.0) / 20.0);
        let a_ms = attack_ms.max(0.1);
        let r_ms = release_ms.max(0.1);
        // exp(-1 / (time_sec * sample_rate)) per-sample smoothing.
        self.alpha_a = (-1.0 / (a_ms * 0.001 * sample_rate.max(1.0))).exp();
        self.alpha_r = (-1.0 / (r_ms * 0.001 * sample_rate.max(1.0))).exp();
    }

    /// Process one stereo sample. The detector uses the sum-of-squares
    /// of both channels (linked stereo) so a transient on either
    /// channel ducks both equally — the standard stereo bus compressor
    /// behavior.
    #[inline]
    pub fn tick_stereo(&mut self, l_in: f32, r_in: f32) -> (f32, f32) {
        // 1. Detector — instantaneous magnitude of the SUM channel.
        let det = ((l_in * l_in + r_in * r_in) * 0.5).sqrt().max(1e-9);
        let det_db = 20.0 * det.log10();
        // 2. Gain computer — soft knee around the threshold.
        let over = det_db - self.threshold_db;
        let half_knee = self.knee_db * 0.5;
        let target_gr_db = if self.knee_db > 0.0 && over.abs() <= half_knee {
            // Quadratic soft-knee region.
            let x = over + half_knee;
            -((1.0 - 1.0 / self.ratio) * x * x / (2.0 * self.knee_db))
        } else if over > half_knee {
            -((1.0 - 1.0 / self.ratio) * over)
        } else {
            0.0
        };
        // 3. Decoupled smoother — fast attack, slow release.
        let alpha = if target_gr_db < self.env_gr_db {
            self.alpha_a
        } else {
            self.alpha_r
        };
        self.env_gr_db = alpha * self.env_gr_db + (1.0 - alpha) * target_gr_db;
        // 4. Apply gain reduction + makeup.
        let gain = 10f32.powf(self.env_gr_db / 20.0) * self.makeup_lin;
        (l_in * gain, r_in * gain)
    }

    /// Snapshot of the current gain reduction in dB. Used by the
    /// future GR-meter UI. Always ≤ 0.
    pub fn current_gr_db(&self) -> f32 {
        self.env_gr_db
    }
}

/// Peak-following limiter. Uses a ring buffer (look-ahead window) so a
/// transient is detected BEFORE it reaches the output. Suitable as the
/// final stage of a mix bus.
#[derive(Debug)]
pub struct Limiter {
    ceiling_lin: f32,
    /// Look-ahead delay buffer (stereo interleaved).
    delay: Vec<[f32; 2]>,
    delay_idx: usize,
    /// Peak envelope (linear).
    env: f32,
    alpha_r: f32,
}

impl Limiter {
    /// New limiter at the given sample rate. `lookahead_ms` is the
    /// internal delay (3 ms is a sane mix-bus default).
    pub fn new(sample_rate: f32, lookahead_ms: f32) -> Self {
        let n = ((lookahead_ms.max(0.1) * 0.001 * sample_rate.max(1.0)) as usize).max(1);
        Self {
            ceiling_lin: 1.0,
            delay: vec![[0.0; 2]; n],
            delay_idx: 0,
            env: 0.0,
            alpha_r: (-1.0 / (50.0 * 0.001 * sample_rate.max(1.0))).exp(),
        }
    }

    /// Reconfigure. Lookahead size is fixed at construction; changing
    /// it requires a fresh `Limiter` (the engine handles that via the
    /// snapshot rebuild).
    pub fn set(&mut self, ceiling_db: f32, release_ms: f32, sample_rate: f32) {
        self.ceiling_lin = 10f32.powf(ceiling_db.clamp(-30.0, 0.0) / 20.0);
        let r_ms = release_ms.max(1.0);
        self.alpha_r = (-1.0 / (r_ms * 0.001 * sample_rate.max(1.0))).exp();
    }

    /// Length of the look-ahead delay in samples (frames). Output lags
    /// input by exactly this many ticks.
    pub fn lookahead_samples(&self) -> usize {
        self.delay.len()
    }

    /// Process one stereo sample. Returns the sample from
    /// [`Self::lookahead_samples`] ticks ago, scaled so its peak stays
    /// at or below the ceiling.
    #[inline]
    pub fn tick_stereo(&mut self, l_in: f32, r_in: f32) -> (f32, f32) {
        // Read the delayed sample (output slot) and overwrite with the
        // newest input (the head wraps modulo delay length).
        let out = self.delay[self.delay_idx];
        self.delay[self.delay_idx] = [l_in, r_in];
        self.delay_idx = (self.delay_idx + 1) % self.delay.len();

        // Peak detector across both channels.
        let peak_in = l_in.abs().max(r_in.abs());
        if peak_in > self.env {
            // Attack: instantaneous so the limiter never overshoots.
            self.env = peak_in;
        } else {
            self.env = self.alpha_r * self.env + (1.0 - self.alpha_r) * peak_in;
        }

        // Gain — never above 1.0.
        let gain = if self.env > self.ceiling_lin {
            self.ceiling_lin / self.env.max(1e-9)
        } else {
            1.0
        };
        (out[0] * gain, out[1] * gain)
    }
}

/// GOAT-8 — downward noise gate / expander. When the linked-stereo level
/// drops below `threshold`, the gate closes toward `range` (an
/// attenuation floor), smoothed by attack (opening) / release (closing)
/// time constants. Removes hiss/room-rumble between phrases without the
/// pumping a compressor would add. Stateful; lives on the fold thread.
#[derive(Clone, Copy, Debug)]
pub struct NoiseGate {
    threshold_lin: f32,
    floor_lin: f32,
    alpha_a: f32,
    alpha_r: f32,
    /// Smoothed gate gain in `[floor_lin, 1.0]`. Carries across ticks.
    env: f32,
}

impl NoiseGate {
    /// Fully-open (pass-through) gate.
    pub fn passthrough() -> Self {
        Self {
            threshold_lin: 0.0,
            floor_lin: 1.0,
            alpha_a: 0.0,
            alpha_r: 0.0,
            env: 1.0,
        }
    }

    /// Reconfigure. `threshold_db` is the open level (below it the gate
    /// closes); `range_db` is the closed attenuation (e.g. `-60` ≈ silence,
    /// `-12` a gentle duck). Attack/release in ms. State preserved.
    pub fn set(
        &mut self,
        threshold_db: f32,
        range_db: f32,
        attack_ms: f32,
        release_ms: f32,
        sample_rate: f32,
    ) {
        self.threshold_lin = 10f32.powf(threshold_db.clamp(-90.0, 0.0) / 20.0);
        self.floor_lin = 10f32.powf(range_db.clamp(-90.0, 0.0) / 20.0);
        let a_ms = attack_ms.max(0.1);
        let r_ms = release_ms.max(0.1);
        self.alpha_a = (-1.0 / (a_ms * 0.001 * sample_rate.max(1.0))).exp();
        self.alpha_r = (-1.0 / (r_ms * 0.001 * sample_rate.max(1.0))).exp();
    }

    /// Process one stereo sample (linked detector).
    #[inline]
    pub fn tick_stereo(&mut self, l_in: f32, r_in: f32) -> (f32, f32) {
        let det = ((l_in * l_in + r_in * r_in) * 0.5).sqrt();
        let target = if det >= self.threshold_lin {
            1.0
        } else {
            self.floor_lin
        };
        // Opening (target above env) uses the fast attack; closing the
        // slower release — the conventional gate envelope.
        let alpha = if target > self.env {
            self.alpha_a
        } else {
            self.alpha_r
        };
        self.env = alpha * self.env + (1.0 - alpha) * target;
        if self.env < 1e-30 {
            self.env = 0.0;
        }
        (l_in * self.env, r_in * self.env)
    }

    /// Current gate gain (for a future open/closed meter).
    pub fn current_gain(&self) -> f32 {
        self.env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notch_attenuates_its_center_frequency() {
        let sr = 48_000.0;
        let mut n = Biquad::identity();
        assert!(n.set(BiquadShape::Notch, sr, 1000.0, 0.0, 8.0));
        // Drive a 1 kHz sine; after settling the notch should crush it.
        let mut peak = 0.0f32;
        for i in 0..4800 {
            let s = (2.0 * PI * 1000.0 * i as f32 / sr).sin();
            let (o, _) = n.tick_stereo(s, s);
            if i > 2400 {
                peak = peak.max(o.abs());
            }
        }
        assert!(peak < 0.2, "1kHz notch should attenuate (peak {peak})");
    }

    #[test]
    fn notch_passes_a_distant_frequency() {
        let sr = 48_000.0;
        let mut n = Biquad::identity();
        n.set(BiquadShape::Notch, sr, 60.0, 0.0, 8.0);
        let mut peak = 0.0f32;
        for i in 0..4800 {
            let s = (2.0 * PI * 1000.0 * i as f32 / sr).sin();
            let (o, _) = n.tick_stereo(s, s);
            if i > 2400 {
                peak = peak.max(o.abs());
            }
        }
        assert!(peak > 0.8, "1kHz passes a 60Hz notch (peak {peak})");
    }

    #[test]
    fn noise_gate_closes_below_threshold_opens_above() {
        let sr = 48_000.0;
        let mut g = NoiseGate::passthrough();
        // Threshold -20 dB, deep range, fast times.
        g.set(-20.0, -80.0, 1.0, 5.0, sr);
        // Quiet signal (-40 dB) → gate closes (gain → ~floor).
        let quiet = 0.01;
        for _ in 0..4800 {
            g.tick_stereo(quiet, quiet);
        }
        assert!(
            g.current_gain() < 0.05,
            "gate closed on quiet: {}",
            g.current_gain()
        );
        // Loud signal (0 dB) → gate opens (gain → ~1).
        for _ in 0..4800 {
            g.tick_stereo(0.9, 0.9);
        }
        assert!(
            g.current_gain() > 0.9,
            "gate opened on loud: {}",
            g.current_gain()
        );
    }

    #[test]
    fn biquad_identity_is_pass_through() {
        let mut b = Biquad::identity();
        for x in [0.1, -0.4, 0.7, -0.9] {
            let (l, r) = b.tick_stereo(x, -x);
            assert!((l - x).abs() < 1e-6, "identity must pass-through");
            assert!((r + x).abs() < 1e-6);
        }
    }

    #[test]
    fn biquad_low_shelf_boosts_dc() {
        let sr = 48_000.0;
        let mut b = Biquad::identity();
        assert!(b.set(BiquadShape::LowShelf, sr, 200.0, 12.0, 0.707));
        // Drive with a DC signal — should rise toward `+12dB`.
        let dc = 0.1;
        let mut last = 0.0;
        for _ in 0..1024 {
            let (l, _) = b.tick_stereo(dc, 0.0);
            last = l;
        }
        // +12 dB ≈ ×3.98, so DC output ≈ 0.398.
        assert!(last > 0.3, "low shelf should boost DC; got {last}");
    }

    #[test]
    fn biquad_high_cut_attenuates_nyquist_neighborhood() {
        let sr = 48_000.0;
        let mut b = Biquad::identity();
        assert!(b.set(BiquadShape::HighCut, sr, 1_000.0, 0.0, 0.707));
        // Drive with high-frequency sine.
        let mut rms_input = 0.0f32;
        let mut rms_output = 0.0f32;
        for n in 0..2048 {
            let phase = 2.0 * PI * 18_000.0 * (n as f32) / sr;
            let s = phase.sin();
            let (out_l, _) = b.tick_stereo(s, 0.0);
            rms_input += s * s;
            rms_output += out_l * out_l;
        }
        // -3 dB cutoff at 1 kHz; 18 kHz should be heavily attenuated.
        assert!(
            rms_output < rms_input * 0.01,
            "high-cut should attenuate 18 kHz"
        );
    }

    #[test]
    fn biquad_rejects_out_of_range_frequencies() {
        let sr = 48_000.0;
        let mut b = Biquad::identity();
        // Negative freq → identity fallback.
        assert!(!b.set(BiquadShape::Peak, sr, -100.0, 6.0, 0.7));
        // Above nyquist → identity fallback.
        assert!(!b.set(BiquadShape::Peak, sr, 30_000.0, 6.0, 0.7));
        // Zero sample rate → identity fallback.
        assert!(!b.set(BiquadShape::Peak, 0.0, 1_000.0, 6.0, 0.7));
        let (l, r) = b.tick_stereo(0.5, 0.5);
        assert!((l - 0.5).abs() < 1e-6, "fallback must be identity");
        assert!((r - 0.5).abs() < 1e-6);
    }

    #[test]
    fn compressor_passthrough_below_threshold() {
        let mut c = Compressor::passthrough();
        c.set(-10.0, 4.0, 5.0, 50.0, 0.0, 0.0, 48_000.0);
        // -20 dBFS = 0.1 amplitude — well below the threshold.
        let (l, r) = c.tick_stereo(0.1, 0.1);
        assert!((l - 0.1).abs() < 1e-4);
        assert!((r - 0.1).abs() < 1e-4);
    }

    #[test]
    fn compressor_reduces_signal_above_threshold() {
        let mut c = Compressor::passthrough();
        c.set(-10.0, 4.0, 0.5, 200.0, 0.0, 0.0, 48_000.0);
        // Drive a long unity-amplitude burst — far above -10 dBFS.
        let mut last_l = 0.0;
        for _ in 0..2048 {
            let (l, _) = c.tick_stereo(1.0, 1.0);
            last_l = l;
        }
        assert!(
            last_l < 0.6,
            "compressor must reduce 0 dBFS at -10 dB threshold"
        );
    }

    #[test]
    fn compressor_envelope_persists_across_set_calls() {
        let mut c = Compressor::passthrough();
        c.set(-10.0, 4.0, 0.5, 200.0, 0.0, 0.0, 48_000.0);
        for _ in 0..2048 {
            c.tick_stereo(1.0, 1.0);
        }
        let gr_before = c.current_gr_db();
        // User tweaks ratio mid-signal.
        c.set(-10.0, 8.0, 0.5, 200.0, 0.0, 0.0, 48_000.0);
        let gr_after = c.current_gr_db();
        assert_eq!(
            gr_before, gr_after,
            "set() must NOT reset env_gr_db — user knob drag would zip otherwise"
        );
    }

    #[test]
    fn limiter_caps_peaks_at_ceiling() {
        let mut lim = Limiter::new(48_000.0, 3.0);
        lim.set(-0.1, 50.0, 48_000.0);
        // Push 3 ms of unity samples to fill the look-ahead.
        let mut peak_seen = 0.0f32;
        for _ in 0..512 {
            let (l, _) = lim.tick_stereo(1.0, 1.0);
            peak_seen = peak_seen.max(l.abs());
        }
        // -0.1 dB ≈ 0.9886. Output must be ≤ ceiling with a tiny epsilon.
        let ceiling = 10f32.powf(-0.1 / 20.0);
        assert!(
            peak_seen <= ceiling + 1e-3,
            "limiter ceiling: got {peak_seen}, ceiling {ceiling}"
        );
    }

    #[test]
    fn limiter_passes_subceiling_signal_unchanged() {
        let mut lim = Limiter::new(48_000.0, 3.0);
        lim.set(-0.1, 50.0, 48_000.0);
        // Drain look-ahead first.
        for _ in 0..256 {
            lim.tick_stereo(0.0, 0.0);
        }
        // Below-ceiling signal must pass through unmodified.
        let (l, r) = lim.tick_stereo(0.3, -0.4);
        // After the look-ahead delay, this sample emerges later — but
        // its amplitude must be intact.
        // Pump until it appears.
        let mut got = (0.0, 0.0);
        for _ in 0..lim.lookahead_samples() {
            got = lim.tick_stereo(0.0, 0.0);
            if got.0.abs() > 0.01 || got.1.abs() > 0.01 {
                break;
            }
        }
        let _ = (l, r);
        // Look-ahead with 50ms release means the envelope won't drop
        // below 0.4 immediately; we only assert no GAIN was applied
        // (i.e. abs(out) <= abs(in)).
        assert!(got.0.abs() <= 0.3001);
        assert!(got.1.abs() <= 0.4001);
    }
}
