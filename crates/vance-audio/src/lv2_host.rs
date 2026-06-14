//! Phase 7.10 — LV2 audio-plugin backend (feature `lv2`).
//!
//! Hosts a third-party LV2 plugin in-process via `livi` (which links
//! the system `liblilv`) and adapts it to the fold chain's
//! [`crate::audio_plugin::AudioPlugin`] block interface.
//!
//! **Build requirement:** `liblilv` must be installed (`brew install
//! lilv` on macOS, `apt install liblilv-dev libserd-dev` on
//! Debian/Ubuntu). The default build does NOT enable this feature, so a
//! box without lilv still builds the rest of vance.
//!
//! Scope: mono or stereo audio effects. Parameter values are applied at
//! instantiate via `set_control_input` (param id = the plugin's absolute
//! LV2 port index). Atom-sequence and CV ports are connected to
//! empty/zeroed buffers (no MIDI routing yet) so an audio effect that
//! also declares a control/atom input port still runs.

use crate::audio_plugin::AudioPlugin;
use livi::event::LV2AtomSequence;
use livi::{EmptyPortConnections, Features, FeaturesBuilder, Instance, PortIndex, PortType, World};
use std::sync::Arc;

/// Upper bound on the requested block size, capping scratch allocation so a
/// bad caller can't drive a multi-gigabyte allocation.
const MAX_BLOCK_FRAMES: usize = 1 << 16;

/// Byte capacity of each atom-sequence port buffer.
const ATOM_SEQ_CAPACITY: usize = 4096;

/// A loaded LV2 plugin wrapped as a block-processing [`AudioPlugin`].
pub struct Lv2Plugin {
    // Drop order is DECLARATION order, and it matters here: the instance
    // borrows from the world's plugin data (lilv ties them together), so
    // the instance MUST drop before the features and world. Declaring
    // `_world` first (as before) freed the world first → a use-after-free
    // in lilv/sord teardown (SIGSEGV).
    instance: Instance,
    _features: Arc<Features>,
    _world: World,
    n_in: usize,
    n_out: usize,
    in_bufs: Vec<Vec<f32>>,
    out_bufs: Vec<Vec<f32>>,
    atom_in: Vec<LV2AtomSequence>,
    atom_out: Vec<LV2AtomSequence>,
    cv_in: Vec<Vec<f32>>,
    cv_out: Vec<Vec<f32>>,
    block: usize,
    /// Set after the first `run()` failure so we warn once, not per block.
    run_warned: bool,
}

impl AudioPlugin for Lv2Plugin {
    fn process_block(&mut self, in_l: &[f32], in_r: &[f32], out_l: &mut [f32], out_r: &mut [f32]) {
        let n = in_l.len().min(self.block);
        // Map the stereo fold signal onto the plugin's audio inputs.
        match self.n_in {
            0 => {}
            1 => {
                for i in 0..n {
                    self.in_bufs[0][i] = 0.5 * (in_l[i] + in_r[i]);
                }
            }
            _ => {
                self.in_bufs[0][..n].copy_from_slice(&in_l[..n]);
                self.in_bufs[1][..n].copy_from_slice(&in_r[..n]);
            }
        }

        // Empty atom sequences (inputs cleared; outputs reset to a
        // full-capacity chunk).
        for s in self.atom_in.iter_mut() {
            s.clear();
        }
        for s in self.atom_out.iter_mut() {
            s.clear_as_chunk();
        }

        let n_in = self.n_in;
        let n_out = self.n_out;
        let ports = EmptyPortConnections::new()
            .with_audio_inputs(self.in_bufs.iter().take(n_in).map(|b| &b[..n]))
            .with_audio_outputs(self.out_bufs.iter_mut().take(n_out).map(|b| &mut b[..n]))
            .with_atom_sequence_inputs(self.atom_in.iter())
            .with_atom_sequence_outputs(self.atom_out.iter_mut())
            .with_cv_inputs(self.cv_in.iter().map(|b| &b[..n]))
            .with_cv_outputs(self.cv_out.iter_mut().map(|b| &mut b[..n]));
        // SAFETY: buffers are `block`-sized and `n <= block`; the plugin
        // was instantiated for this block range.
        let ok = match unsafe { self.instance.run(n, ports) } {
            Ok(()) => true,
            Err(e) => {
                if !self.run_warned {
                    self.run_warned = true;
                    tracing::warn!(error = ?e, "LV2 run() failed; passing audio through unchanged");
                }
                false
            }
        };

        if ok {
            match self.n_out {
                0 => {
                    out_l[..n].copy_from_slice(&in_l[..n]);
                    out_r[..n].copy_from_slice(&in_r[..n]);
                }
                1 => {
                    out_l[..n].copy_from_slice(&self.out_bufs[0][..n]);
                    out_r[..n].copy_from_slice(&self.out_bufs[0][..n]);
                }
                _ => {
                    out_l[..n].copy_from_slice(&self.out_bufs[0][..n]);
                    out_r[..n].copy_from_slice(&self.out_bufs[1][..n]);
                }
            }
        } else {
            // Run error → pass audio through unchanged.
            out_l[..n].copy_from_slice(&in_l[..n]);
            out_r[..n].copy_from_slice(&in_r[..n]);
        }
    }
}

/// Instantiate an LV2 plugin by URI. `params` are `(absolute_port_index,
/// value)` pairs applied to the plugin's control inputs. Returns `None`
/// on any failure so the caller bypasses the chain slot.
pub fn instantiate(
    uri: &str,
    params: &[(u32, f32)],
    sample_rate: f32,
    block: usize,
) -> Option<Box<dyn AudioPlugin>> {
    let block = block.clamp(1, MAX_BLOCK_FRAMES);
    let world = World::new();
    let features = world.build_features(FeaturesBuilder {
        min_block_length: 1,
        max_block_length: block,
    });
    let plugin = world.plugin_by_uri(uri)?;
    let n_in = plugin.ports_with_type(PortType::AudioInput).count();
    let n_out = plugin.ports_with_type(PortType::AudioOutput).count();
    let n_atom_in = plugin.ports_with_type(PortType::AtomSequenceInput).count();
    let n_atom_out = plugin.ports_with_type(PortType::AtomSequenceOutput).count();
    let n_cv_in = plugin.ports_with_type(PortType::CVInput).count();
    let n_cv_out = plugin.ports_with_type(PortType::CVOutput).count();

    // SAFETY: `livi::Plugin::instantiate` is unsafe because it loads
    // third-party native code; failures surface as `Err`.
    let mut instance = unsafe { plugin.instantiate(features.clone(), sample_rate as f64) }.ok()?;
    for &(pid, val) in params {
        if instance
            .set_control_input(PortIndex(pid as usize), val)
            .is_none()
        {
            tracing::warn!(
                port = pid,
                "LV2 port index is not a settable control input; ignored"
            );
        }
    }

    let atom_in = (0..n_atom_in)
        .map(|_| LV2AtomSequence::new(&features, ATOM_SEQ_CAPACITY))
        .collect();
    let atom_out = (0..n_atom_out)
        .map(|_| LV2AtomSequence::new(&features, ATOM_SEQ_CAPACITY))
        .collect();

    Some(Box::new(Lv2Plugin {
        instance,
        _features: features,
        _world: world,
        n_in,
        n_out,
        in_bufs: vec![vec![0.0; block]; n_in.max(1)],
        out_bufs: vec![vec![0.0; block]; n_out.max(1)],
        atom_in,
        atom_out,
        cv_in: vec![vec![0.0; block]; n_cv_in],
        cv_out: vec![vec![0.0; block]; n_cv_out],
        block,
        run_warned: false,
    }))
}
