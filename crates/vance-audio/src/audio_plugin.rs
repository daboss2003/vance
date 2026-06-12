//! Phase 7.10/7.11 — in-process audio effect plugin hosting.
//!
//! Real LV2 / CLAP audio effects process **blocks** of samples, but the
//! mixer's fold chain runs **per-sample** (so a biquad → plugin →
//! limiter chain stays sample-accurate). [`BlockProcessor`] bridges the
//! two: it buffers `block` input samples, runs the plugin's
//! [`AudioPlugin::process_block`] once the buffer fills, and drains the
//! processed samples one at a time. This introduces exactly `block`
//! samples of latency (≈1.3 ms at 48 kHz for the default 64-sample
//! block) — constant and small.
//!
//! Hosting is **in-process** (on the fold thread). Out-of-process
//! sandboxing for *processing* isn't viable in a real-time audio
//! callback (per-block IPC would glitch); the `vance-plugin-host`
//! sidecar still does crash-safe *discovery*. This matches how most
//! DAWs host audio plugins.
//!
//! Backends:
//! - **builtin** — pure-Rust reference effects (`vance:gain`,
//!   `vance:identity`), always available + testable.
//! - **clap** (feature `clap`) — third-party `.clap` plugins via
//!   `clack-host` (pure-Rust + dlopen).
//! - **lv2** (feature `lv2`) — third-party LV2 plugins via `livi`
//!   (links system `liblilv`).

use std::collections::VecDeque;

/// Default processing block size (frames). Small for low latency.
pub const DEFAULT_BLOCK: usize = 64;

/// A block-processing stereo audio effect. Implementations may hold
/// arbitrary internal state; the host calls [`Self::process_block`]
/// with equal-length input/output slices.
pub trait AudioPlugin {
    /// Process one block. `in_l/in_r` and `out_l/out_r` all have the
    /// same length. In-place is allowed (out may alias conceptually,
    /// but the host passes distinct buffers).
    fn process_block(&mut self, in_l: &[f32], in_r: &[f32], out_l: &mut [f32], out_r: &mut [f32]);
}

/// Built-in reference plugin: a stereo gain. Param id `0` = gain in dB.
pub struct BuiltinGain {
    gain_lin: f32,
}

impl BuiltinGain {
    /// New gain from a dB value.
    pub fn from_db(db: f32) -> Self {
        Self {
            gain_lin: 10f32.powf(db / 20.0),
        }
    }
}

impl AudioPlugin for BuiltinGain {
    #[inline]
    fn process_block(&mut self, in_l: &[f32], in_r: &[f32], out_l: &mut [f32], out_r: &mut [f32]) {
        let g = self.gain_lin;
        for i in 0..in_l.len() {
            out_l[i] = in_l[i] * g;
            out_r[i] = in_r[i] * g;
        }
    }
}

/// Buffers a [`AudioPlugin`] so it can run inside the per-sample fold
/// chain. Allocation-free in the hot path: input + scratch are fixed
/// `block`-sized Vecs, the output queue is pre-sized to never realloc.
pub struct BlockProcessor {
    plugin: Box<dyn AudioPlugin>,
    block: usize,
    fill: usize,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    scratch_l: Vec<f32>,
    scratch_r: Vec<f32>,
    out: VecDeque<(f32, f32)>,
}

impl BlockProcessor {
    /// Wrap a plugin with the given block size.
    pub fn new(plugin: Box<dyn AudioPlugin>, block: usize) -> Self {
        let block = block.max(1);
        Self {
            plugin,
            block,
            fill: 0,
            in_l: vec![0.0; block],
            in_r: vec![0.0; block],
            scratch_l: vec![0.0; block],
            scratch_r: vec![0.0; block],
            out: VecDeque::with_capacity(block + 1),
        }
    }

    /// Feed one stereo sample, get one back (delayed by `block`). The
    /// first `block` outputs are silence (priming latency).
    #[inline]
    pub fn tick(&mut self, l: f32, r: f32) -> (f32, f32) {
        self.in_l[self.fill] = l;
        self.in_r[self.fill] = r;
        self.fill += 1;
        if self.fill == self.block {
            self.plugin.process_block(
                &self.in_l,
                &self.in_r,
                &mut self.scratch_l,
                &mut self.scratch_r,
            );
            for i in 0..self.block {
                self.out.push_back((self.scratch_l[i], self.scratch_r[i]));
            }
            self.fill = 0;
        }
        self.out.pop_front().unwrap_or((0.0, 0.0))
    }
}

/// Instantiate a plugin by `(format, id, params)`. `params` is a list
/// of `(param_id, value)`. Returns `None` for unknown plugins (the
/// caller then bypasses that chain slot). `format` is `"builtin"`,
/// `"clap"`, or `"lv2"`.
pub fn instantiate(
    format: &str,
    id: &str,
    params: &[(u32, f32)],
    sample_rate: f32,
    block: usize,
) -> Option<BlockProcessor> {
    let plugin: Box<dyn AudioPlugin> = match format {
        "builtin" => builtin(id, params)?,
        #[cfg(feature = "clap")]
        "clap" => crate::clap_host::instantiate(id, params, sample_rate, block)?,
        #[cfg(feature = "lv2")]
        "lv2" => crate::lv2_host::instantiate(id, params, sample_rate, block)?,
        _ => return None,
    };
    let _ = sample_rate;
    Some(BlockProcessor::new(plugin, block))
}

fn builtin(id: &str, params: &[(u32, f32)]) -> Option<Box<dyn AudioPlugin>> {
    match id {
        "vance:gain" => {
            let db = params
                .iter()
                .find(|(p, _)| *p == 0)
                .map(|(_, v)| *v)
                .unwrap_or(0.0);
            Some(Box::new(BuiltinGain::from_db(db)))
        }
        "vance:identity" => Some(Box::new(BuiltinGain::from_db(0.0))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_gain_attenuates() {
        let mut g = BuiltinGain::from_db(-6.0206); // ≈ 0.5x
        let inp = vec![1.0f32; 8];
        let mut ol = vec![0.0; 8];
        let mut or = vec![0.0; 8];
        g.process_block(&inp, &inp, &mut ol, &mut or);
        for &v in &ol {
            assert!((v - 0.5).abs() < 1e-3, "got {v}");
        }
    }

    #[test]
    fn block_processor_applies_gain_with_latency() {
        // -6 dB gain through a block of 4. Latency is block-1 = 3
        // samples (the tick that completes the input block also emits
        // the first processed sample). After that, steady 0.5x.
        let block = 4usize;
        let mut bp = BlockProcessor::new(Box::new(BuiltinGain::from_db(-6.0206)), block);
        let mut outs = Vec::new();
        for _ in 0..12 {
            outs.push(bp.tick(1.0, 1.0).0);
        }
        // First `block - 1` outputs are priming silence.
        assert_eq!(&outs[..block - 1], &[0.0, 0.0, 0.0]);
        // After that, steady 0.5.
        for &v in &outs[block - 1..] {
            assert!((v - 0.5).abs() < 1e-3, "got {v}");
        }
    }

    #[test]
    fn instantiate_builtin_gain() {
        let bp = instantiate("builtin", "vance:gain", &[(0, -20.0)], 48_000.0, 64);
        assert!(bp.is_some());
    }

    #[test]
    fn instantiate_unknown_is_none() {
        assert!(instantiate("builtin", "nope", &[], 48_000.0, 64).is_none());
        assert!(instantiate("zzz", "x", &[], 48_000.0, 64).is_none());
    }
}
