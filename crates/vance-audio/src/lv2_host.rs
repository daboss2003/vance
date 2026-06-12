//! Phase 7.10 — LV2 audio-plugin backend (feature `lv2`).
//!
//! Hosts a third-party LV2 plugin in-process via `livi` (which links
//! the system `liblilv`) and adapts it to the fold chain's
//! [`crate::audio_plugin::AudioPlugin`] block interface.
//!
//! **Build requirement:** `liblilv` must be installed (`brew install
//! lilv` on macOS, `apt install liblilv-dev` on Debian/Ubuntu). The
//! default build does NOT enable this feature, so a box without lilv
//! still builds the rest of vance.
//!
//! v1 scope: mono or stereo audio effects, fixed block size. Parameter
//! values are applied at instantiate via `set_control_input` (param id
//! = the plugin's absolute LV2 port index). MIDI/atom and CV ports are
//! left empty.

use crate::audio_plugin::AudioPlugin;
use livi::{EmptyPortConnections, Features, FeaturesBuilder, Instance, PortIndex, PortType, World};
use std::sync::Arc;

/// A loaded LV2 plugin wrapped as a block-processing [`AudioPlugin`].
pub struct Lv2Plugin {
    // Keep the world + features alive for the instance's lifetime.
    _world: World,
    _features: Arc<Features>,
    instance: Instance,
    n_in: usize,
    n_out: usize,
    in_bufs: Vec<Vec<f32>>,
    out_bufs: Vec<Vec<f32>>,
    block: usize,
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
                for i in 0..n {
                    self.in_bufs[0][i] = in_l[i];
                    self.in_bufs[1][i] = in_r[i];
                }
            }
        }

        let ports = EmptyPortConnections::new()
            .with_audio_inputs(self.in_bufs.iter().take(self.n_in).map(|b| b.as_slice()))
            .with_audio_outputs(
                self.out_bufs
                    .iter_mut()
                    .take(self.n_out)
                    .map(|b| b.as_mut_slice()),
            );
        // SAFETY: buffers are `block`-sized and `n == block`; the plugin
        // was instantiated for this block range.
        let ok = unsafe { self.instance.run(n, ports) }.is_ok();

        if ok {
            match self.n_out {
                0 => {
                    out_l[..n].copy_from_slice(&in_l[..n]);
                    out_r[..n].copy_from_slice(&in_r[..n]);
                }
                1 => {
                    for i in 0..n {
                        out_l[i] = self.out_bufs[0][i];
                        out_r[i] = self.out_bufs[0][i];
                    }
                }
                _ => {
                    for i in 0..n {
                        out_l[i] = self.out_bufs[0][i];
                        out_r[i] = self.out_bufs[1][i];
                    }
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
    let block = block.max(1);
    let world = World::new();
    let features = world.build_features(FeaturesBuilder {
        min_block_length: 1,
        max_block_length: block,
    });
    let plugin = world.plugin_by_uri(uri)?;
    let n_in = plugin.ports_with_type(PortType::AudioInput).count();
    let n_out = plugin.ports_with_type(PortType::AudioOutput).count();

    // SAFETY: `livi::Plugin::instantiate` is unsafe because it loads
    // third-party native code; failures surface as `Err`.
    let mut instance = unsafe { plugin.instantiate(features.clone(), sample_rate as f64) }.ok()?;
    for &(pid, val) in params {
        instance.set_control_input(PortIndex(pid as usize), val);
    }

    Some(Box::new(Lv2Plugin {
        _world: world,
        _features: features,
        instance,
        n_in,
        n_out,
        in_bufs: vec![vec![0.0; block]; n_in.max(1)],
        out_bufs: vec![vec![0.0; block]; n_out.max(1)],
        block,
    }))
}
