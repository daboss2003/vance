//! LV2 processing backend (feature `lv2`).
//!
//! Hosts third-party LV2 plugins IN THE SIDECAR PROCESS via `livi` (which
//! links the system `liblilv`), so a plugin crash takes down only this
//! sandbox. Out-of-process counterpart to the in-process
//! [`vance_audio::lv2_host`].
//!
//! v1 scope: mono/stereo audio effects. Audio is exchanged over the
//! [`vance_audio_plugins`] IPC protocol as interleaved stereo. Control
//! parameters are applied via `set_control_input` (param id = the
//! plugin's absolute LV2 port index); parameter enumeration is a follow-up.

use livi::{EmptyPortConnections, Features, FeaturesBuilder, Instance, PortIndex, PortType, World};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;
use vance_audio_plugins::{PluginDescriptor, PluginFormat, PluginParam};

use crate::runtime::PluginBackend;

/// One live LV2 instance plus its scratch buffers.
struct Lv2Instance {
    // Drop order is DECLARATION order and it MATTERS: the instance borrows
    // the world's plugin data (lilv ties them), so it must drop before the
    // features + world — otherwise lilv/sord teardown is a use-after-free.
    instance: Instance,
    _features: Arc<Features>,
    _world: World,
    n_in: usize,
    n_out: usize,
    in_bufs: Vec<Vec<f32>>,
    out_bufs: Vec<Vec<f32>>,
    max_block: usize,
}

/// Hosts LV2 instances in the sidecar process.
#[derive(Default)]
pub struct Lv2Backend {
    instances: HashMap<Uuid, Lv2Instance>,
}

impl Lv2Backend {
    /// New empty backend.
    pub fn new() -> Self {
        Self {
            instances: HashMap::new(),
        }
    }
}

impl PluginBackend for Lv2Backend {
    fn format(&self) -> PluginFormat {
        PluginFormat::Lv2
    }

    fn list(&mut self) -> Vec<PluginDescriptor> {
        // Discovery is handled out-of-band (lv2_discovery); the live
        // processing backend lists nothing.
        Vec::new()
    }

    fn instantiate(
        &mut self,
        descriptor: &PluginDescriptor,
        sample_rate: f32,
        max_block_frames: u32,
    ) -> Result<Uuid, String> {
        let max_block = (max_block_frames.max(1)) as usize;
        let world = World::new();
        let features = world.build_features(FeaturesBuilder {
            min_block_length: 1,
            max_block_length: max_block,
        });
        let plugin = world
            .plugin_by_uri(&descriptor.id)
            .ok_or_else(|| format!("LV2 plugin `{}` not found (LV2_PATH?)", descriptor.id))?;
        let n_in = plugin.ports_with_type(PortType::AudioInput).count();
        let n_out = plugin.ports_with_type(PortType::AudioOutput).count();

        // SAFETY: loads third-party native code; failures surface as Err.
        let instance = unsafe { plugin.instantiate(features.clone(), sample_rate as f64) }
            .map_err(|e| format!("LV2 instantiate `{}`: {e:?}", descriptor.id))?;

        let id = Uuid::new_v4();
        self.instances.insert(
            id,
            Lv2Instance {
                instance,
                _features: features,
                _world: world,
                n_in,
                n_out,
                in_bufs: vec![vec![0.0; max_block]; n_in.max(1)],
                out_bufs: vec![vec![0.0; max_block]; n_out.max(1)],
                max_block,
            },
        );
        Ok(id)
    }

    fn process(&mut self, instance: Uuid, frames_in: &[f32]) -> Result<Vec<f32>, String> {
        let inst = self
            .instances
            .get_mut(&instance)
            .ok_or_else(|| format!("no such plugin instance `{instance}`"))?;
        let total = frames_in.len() / 2;
        if total == 0 {
            return Ok(Vec::new());
        }
        let n = total.min(inst.max_block);

        // Map interleaved stereo onto the plugin's audio inputs.
        match inst.n_in {
            0 => {}
            1 => {
                for i in 0..n {
                    inst.in_bufs[0][i] = 0.5 * (frames_in[2 * i] + frames_in[2 * i + 1]);
                }
            }
            _ => {
                for i in 0..n {
                    inst.in_bufs[0][i] = frames_in[2 * i];
                    inst.in_bufs[1][i] = frames_in[2 * i + 1];
                }
            }
        }

        let n_in = inst.n_in;
        let n_out = inst.n_out;
        let ports = EmptyPortConnections::new()
            .with_audio_inputs(inst.in_bufs.iter().take(n_in).map(|b| &b[..n]))
            .with_audio_outputs(inst.out_bufs.iter_mut().take(n_out).map(|b| &mut b[..n]));
        // SAFETY: buffers are `n`-sized; the instance was built for this
        // block range.
        let ok = unsafe { inst.instance.run(n, ports) }.is_ok();

        let mut out = vec![0.0f32; n * 2];
        if ok {
            match n_out {
                0 => out.copy_from_slice(&frames_in[..n * 2]),
                1 => {
                    for i in 0..n {
                        out[2 * i] = inst.out_bufs[0][i];
                        out[2 * i + 1] = inst.out_bufs[0][i];
                    }
                }
                _ => {
                    for i in 0..n {
                        out[2 * i] = inst.out_bufs[0][i];
                        out[2 * i + 1] = inst.out_bufs[1][i];
                    }
                }
            }
        } else {
            out.copy_from_slice(&frames_in[..n * 2]);
        }
        Ok(out)
    }

    fn list_params(&self, instance: Uuid) -> Result<Vec<PluginParam>, String> {
        // LV2 control-port enumeration is a follow-up; report none.
        if self.instances.contains_key(&instance) {
            Ok(Vec::new())
        } else {
            Err(format!("no such plugin instance `{instance}`"))
        }
    }

    fn set_param(&mut self, instance: Uuid, param_id: u32, value: f32) -> Result<(), String> {
        let inst = self
            .instances
            .get_mut(&instance)
            .ok_or_else(|| format!("no such plugin instance `{instance}`"))?;
        // param id == the plugin's absolute LV2 port index.
        inst.instance
            .set_control_input(PortIndex(param_id as usize), value);
        Ok(())
    }

    fn destroy(&mut self, instance: Uuid) -> Result<(), String> {
        self.instances.remove(&instance);
        Ok(())
    }
}

/// Enumerate the LV2 plugins installed on the system (via `LV2_PATH` +
/// lilv's default search dirs). Reports each as `format: Lv2`, `id` = the
/// plugin URI. Runs `World::new()` once.
pub fn discover() -> Vec<PluginDescriptor> {
    let world = World::new();
    let mut out = Vec::new();
    for plugin in world.iter_plugins() {
        let uri = plugin.uri();
        // A name() helper isn't on every livi version; fall back to the
        // URI's tail for a human label.
        let name = uri.rsplit(['#', '/', ':']).next().unwrap_or(&uri).to_string();
        out.push(PluginDescriptor {
            format: PluginFormat::Lv2,
            id: uri,
            name,
            vendor: String::new(),
            runnable: true,
        });
    }
    out
}
