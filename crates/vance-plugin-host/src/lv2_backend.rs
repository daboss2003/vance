//! LV2 processing backend (feature `lv2`).
//!
//! Hosts third-party LV2 plugins IN THE SIDECAR PROCESS via `livi` (which
//! links the system `liblilv`), so a plugin crash takes down only this
//! sandbox. Out-of-process counterpart to the in-process
//! [`vance_audio::lv2_host`].
//!
//! Scope: mono/stereo audio effects. Audio is exchanged over the
//! [`vance_audio_plugins`] IPC protocol as interleaved stereo. Atom-sequence
//! and CV ports are connected to empty/zeroed buffers (no MIDI routing yet),
//! which lets the common "audio effect that also declares a control/atom
//! input port" run instead of failing livi's port-count check and falling
//! back to passthrough. Control parameters are applied via
//! `set_control_input` (param id = the plugin's absolute LV2 port index);
//! parameter enumeration is a follow-up.

use livi::event::LV2AtomSequence;
use livi::{EmptyPortConnections, Features, FeaturesBuilder, Instance, PortIndex, PortType, World};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;
use vance_audio_plugins::{PluginDescriptor, PluginFormat, PluginParam};

use crate::runtime::PluginBackend;

/// Upper bound on `max_block_frames` accepted from the (trusted) parent.
/// Caps the per-instance scratch allocation so a malformed/oversized
/// request degrades to an error instead of driving a multi-gigabyte
/// allocation that would abort the whole sidecar. 65_536 frames ≈ 1.37 s
/// at 48 kHz and bounds each channel buffer to 256 KiB.
const MAX_BLOCK_FRAMES: usize = 1 << 16;

/// Byte capacity of each atom-sequence port buffer. Large enough for the
/// empty/cleared sequences we hand plugins today; revisit when MIDI/atom
/// routing lands.
const ATOM_SEQ_CAPACITY: usize = 4096;

/// One live LV2 instance plus its scratch buffers.
struct Lv2Instance {
    // Drop order is DECLARATION order and it MATTERS: the instance borrows
    // the world's plugin data (lilv ties them), so it must drop before the
    // features + world — otherwise lilv/sord teardown is a use-after-free.
    instance: Instance,
    _features: Arc<Features>,
    // NOTE (perf, deliberate): each instance owns its own `World`. A fresh
    // `World::new()` per instantiate re-scans `LV2_PATH`, so N live plugins =
    // N parsed lilv worlds in the sidecar (several MB + a disk rescan each).
    // For a sandbox hosting a handful of effect slots this is acceptable and
    // it mirrors the in-process host; if instance counts ever grow, share a
    // single `World` across the backend.
    _world: World,
    n_in: usize,
    n_out: usize,
    in_bufs: Vec<Vec<f32>>,
    out_bufs: Vec<Vec<f32>>,
    atom_in: Vec<LV2AtomSequence>,
    atom_out: Vec<LV2AtomSequence>,
    cv_in: Vec<Vec<f32>>,
    cv_out: Vec<Vec<f32>>,
    max_block: usize,
    /// Set after the first `run()` failure so we warn once, not per block.
    run_warned: bool,
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
        let requested = max_block_frames.max(1) as usize;
        if requested > MAX_BLOCK_FRAMES {
            return Err(format!(
                "max_block_frames {requested} exceeds limit {MAX_BLOCK_FRAMES}"
            ));
        }
        let max_block = requested;
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
        let n_atom_in = plugin.ports_with_type(PortType::AtomSequenceInput).count();
        let n_atom_out = plugin.ports_with_type(PortType::AtomSequenceOutput).count();
        let n_cv_in = plugin.ports_with_type(PortType::CVInput).count();
        let n_cv_out = plugin.ports_with_type(PortType::CVOutput).count();

        // SAFETY: loads third-party native code; failures surface as Err.
        let instance = unsafe { plugin.instantiate(features.clone(), sample_rate as f64) }
            .map_err(|e| format!("LV2 instantiate `{}`: {e:?}", descriptor.id))?;

        let atom_in = (0..n_atom_in)
            .map(|_| LV2AtomSequence::new(&features, ATOM_SEQ_CAPACITY))
            .collect();
        let atom_out = (0..n_atom_out)
            .map(|_| LV2AtomSequence::new(&features, ATOM_SEQ_CAPACITY))
            .collect();

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
                atom_in,
                atom_out,
                cv_in: vec![vec![0.0; max_block]; n_cv_in],
                cv_out: vec![vec![0.0; max_block]; n_cv_out],
                max_block,
                run_warned: false,
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
        // Fail loud rather than silently truncating: the trait contract is
        // output-len == input-len, and a block larger than the instance was
        // built for can't be run safely.
        if total > inst.max_block {
            return Err(format!(
                "block of {total} frames exceeds max_block {}",
                inst.max_block
            ));
        }
        let n = total;

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

        // Present empty atom sequences (inputs cleared; outputs reset to a
        // full-capacity chunk so the plugin sees its available space).
        for s in inst.atom_in.iter_mut() {
            s.clear();
        }
        for s in inst.atom_out.iter_mut() {
            s.clear_as_chunk();
        }

        let n_in = inst.n_in;
        let n_out = inst.n_out;
        let ports = EmptyPortConnections::new()
            .with_audio_inputs(inst.in_bufs.iter().take(n_in).map(|b| &b[..n]))
            .with_audio_outputs(inst.out_bufs.iter_mut().take(n_out).map(|b| &mut b[..n]))
            .with_atom_sequence_inputs(inst.atom_in.iter())
            .with_atom_sequence_outputs(inst.atom_out.iter_mut())
            .with_cv_inputs(inst.cv_in.iter().map(|b| &b[..n]))
            .with_cv_outputs(inst.cv_out.iter_mut().map(|b| &mut b[..n]));
        // SAFETY: buffers are `n`-sized; the instance was built for this
        // block range.
        let ok = match unsafe { inst.instance.run(n, ports) } {
            Ok(()) => true,
            Err(e) => {
                if !inst.run_warned {
                    inst.run_warned = true;
                    tracing::warn!(
                        instance = %instance,
                        error = ?e,
                        "LV2 run() failed; passing audio through unchanged"
                    );
                }
                false
            }
        };

        // One per-block heap allocation (the interleaved result). It's
        // returned by value through the IPC `Processed` response, so a
        // reusable buffer can't be threaded here without an API change —
        // same shape as the stub/CLAP backends. Acceptable for now.
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
        // param id == the plugin's absolute LV2 port index. livi returns None
        // when that index isn't a settable control-input port — surface it as
        // an error (matching the stub/CLAP backends) instead of silently
        // swallowing the write.
        match inst
            .instance
            .set_control_input(PortIndex(param_id as usize), value)
        {
            Some(_) => Ok(()),
            None => Err(format!(
                "LV2 port index {param_id} is not a settable control input on instance {instance}"
            )),
        }
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
