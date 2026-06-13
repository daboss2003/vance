//! CLAP processing backend (feature `clap`).
//!
//! Hosts third-party `.clap` plugins IN THE SIDECAR PROCESS via
//! `clack-host`, so a plugin crash (panic / abort / segfault) takes down
//! only this sandbox — the parent editor detects the broken pipe and
//! bypasses the slot. This is the out-of-process counterpart to the
//! in-process [`vance_audio::clap_host`]; the two share the composite-id
//! grammar via [`vance_audio_plugins::split_clap_id`].
//!
//! Audio is exchanged over the existing [`vance_audio_plugins`] IPC
//! protocol as interleaved stereo (`l, r, l, r, …`). Parameter
//! automation over CLAP events is a follow-up (the in-process host
//! doesn't do it yet either); `process` runs audio with empty event
//! streams, matching that behaviour exactly.

use clack_extensions::params::{ParamInfoBuffer, PluginParams};
use clack_host::events::event_types::ParamValueEvent;
use clack_host::events::io::{InputEvents, OutputEvents};
use clack_host::events::Pckn;
use clack_host::prelude::*;
use clack_host::utils::Cookie;
use std::collections::HashMap;
use uuid::Uuid;
use vance_audio_plugins::{split_clap_id, PluginDescriptor, PluginFormat, PluginParam};

use crate::runtime::PluginBackend;

/// Minimal no-op host. Audio plugins only need the three request
/// callbacks (we don't act on them — pull model).
struct VanceHostShared;
impl<'a> SharedHandler<'a> for VanceHostShared {
    fn request_restart(&self) {}
    fn request_process(&self) {}
    fn request_callback(&self) {}
}

struct VanceHost;
impl HostHandlers for VanceHost {
    type Shared<'a> = VanceHostShared;
    type MainThread<'a> = ();
    type AudioProcessor<'a> = ();
}

/// One live CLAP instance plus its scratch buffers. Field order matters
/// for Drop: the processor must drop before the instance.
struct Instance {
    processor: Option<StartedPluginAudioProcessor<VanceHost>>,
    instance: PluginInstance<VanceHost>,
    in_ports: AudioPorts,
    out_ports: AudioPorts,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    out_l: Vec<f32>,
    out_r: Vec<f32>,
    /// Plugin output events (we don't act on them — drained + cleared).
    event_out: EventBuffer,
    /// Parameter-value events to feed the plugin on the next `process`.
    event_in: EventBuffer,
    max_block: usize,
    /// Parameters enumerated at instantiate (for the FX UI).
    params: Vec<PluginParam>,
    /// Desired parameter values set by the host UI (param_id → value).
    pending: HashMap<u32, f32>,
    /// Param ids changed since the last `process`, to emit as events.
    dirty: Vec<u32>,
}

/// Hosts CLAP instances in the sidecar process.
#[derive(Default)]
pub struct ClapBackend {
    instances: HashMap<Uuid, Instance>,
}

impl ClapBackend {
    /// New empty backend.
    pub fn new() -> Self {
        Self {
            instances: HashMap::new(),
        }
    }
}

impl PluginBackend for ClapBackend {
    fn format(&self) -> PluginFormat {
        PluginFormat::Clap
    }

    fn list(&mut self) -> Vec<PluginDescriptor> {
        // Discovery is handled out-of-band (per-bundle subprocess probes
        // in `clap_discovery`); the live processing backend lists nothing.
        Vec::new()
    }

    fn instantiate(
        &mut self,
        descriptor: &PluginDescriptor,
        sample_rate: f32,
        max_block_frames: u32,
    ) -> Result<Uuid, String> {
        let (bundle_path, wanted_id) = split_clap_id(&descriptor.id);
        let max_block = (max_block_frames.max(1)) as usize;

        let entry = unsafe { PluginEntry::load(bundle_path) }
            .map_err(|e| format!("CLAP entry load failed for `{bundle_path}`: {e:?}"))?;
        let factory = entry
            .get_plugin_factory()
            .ok_or_else(|| format!("`{bundle_path}` exposes no plugin factory"))?;

        // Select the requested plugin (or the first one).
        let plugin_id = match wanted_id {
            Some(want) => factory
                .plugin_descriptors()
                .find(|d| d.id().and_then(|c| c.to_str().ok()) == Some(want))
                .and_then(|d| d.id())
                .ok_or_else(|| format!("plugin `{want}` not found in `{bundle_path}`"))?
                .to_owned(),
            None => factory
                .plugin_descriptors()
                .next()
                .and_then(|d| d.id())
                .ok_or_else(|| format!("`{bundle_path}` has no plugins"))?
                .to_owned(),
        };

        let host_info = HostInfo::new("Vance", "Vance", "https://vance.app", "1.0")
            .map_err(|e| format!("host info: {e:?}"))?;
        let mut instance =
            PluginInstance::<VanceHost>::new(|_| VanceHostShared, |_| (), &entry, &plugin_id, &host_info)
                .map_err(|e| format!("instantiate `{plugin_id:?}`: {e:?}"))?;

        // Enumerate parameters on the main thread (before activation).
        let params = enumerate_params(&mut instance);

        let config = PluginAudioConfiguration {
            sample_rate: sample_rate as f64,
            min_frames_count: 1,
            max_frames_count: max_block as u32,
        };
        let stopped = instance
            .activate(|_, _| (), config)
            .map_err(|e| format!("activate: {e:?}"))?;
        let started = stopped
            .start_processing()
            .map_err(|e| format!("start_processing: {e:?}"))?;

        let id = Uuid::new_v4();
        self.instances.insert(
            id,
            Instance {
                processor: Some(started),
                instance,
                in_ports: AudioPorts::with_capacity(2, 1),
                out_ports: AudioPorts::with_capacity(2, 1),
                in_l: vec![0.0; max_block],
                in_r: vec![0.0; max_block],
                out_l: vec![0.0; max_block],
                out_r: vec![0.0; max_block],
                event_out: EventBuffer::new(),
                event_in: EventBuffer::new(),
                max_block,
                params,
                pending: HashMap::new(),
                dirty: Vec::new(),
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
        // The parent must respect max_block, but clamp defensively.
        let n = total.min(inst.max_block);

        // Deinterleave into the plugin's input buffers.
        for i in 0..n {
            inst.in_l[i] = frames_in[2 * i];
            inst.in_r[i] = frames_in[2 * i + 1];
        }

        // Build parameter-value events for any pending changes.
        inst.event_in.clear();
        if !inst.dirty.is_empty() {
            let dirty: Vec<u32> = inst.dirty.drain(..).collect();
            for pid in dirty {
                let (Some(clap_id), Some(&value)) = (ClapId::from_raw(pid), inst.pending.get(&pid))
                else {
                    continue;
                };
                let ev = ParamValueEvent::new(
                    0,
                    clap_id,
                    Pckn::match_all(),
                    value as f64,
                    Cookie::empty(),
                );
                inst.event_in.push(&ev);
            }
        }

        let Some(proc) = inst.processor.as_mut() else {
            // No processor → passthrough (truncated to n frames).
            return Ok(frames_in[..n * 2].to_vec());
        };

        let input_audio = inst.in_ports.with_input_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_input_only(
                [
                    InputChannel::variable(&mut inst.in_l[..n]),
                    InputChannel::variable(&mut inst.in_r[..n]),
                ]
                .into_iter(),
            ),
        }]);
        let mut output_audio = inst.out_ports.with_output_buffers([AudioPortBuffer {
            latency: 0,
            channels: AudioPortBufferType::f32_output_only(
                [&mut inst.out_l[..n], &mut inst.out_r[..n]].into_iter(),
            ),
        }]);

        let input_events = InputEvents::from_buffer(&inst.event_in);
        let mut output_events = OutputEvents::from_buffer(&mut inst.event_out);
        let res = proc.process(
            &input_audio,
            &mut output_audio,
            &input_events,
            &mut output_events,
            None,
            None,
        );
        inst.event_out.clear();

        let mut out = vec![0.0f32; n * 2];
        if res.is_ok() {
            for i in 0..n {
                out[2 * i] = inst.out_l[i];
                out[2 * i + 1] = inst.out_r[i];
            }
        } else {
            // Process error → passthrough so the slot is transparent.
            out.copy_from_slice(&frames_in[..n * 2]);
        }
        Ok(out)
    }

    fn list_params(&self, instance: Uuid) -> Result<Vec<PluginParam>, String> {
        self.instances
            .get(&instance)
            .map(|inst| inst.params.clone())
            .ok_or_else(|| format!("no such plugin instance `{instance}`"))
    }

    fn set_param(&mut self, instance: Uuid, param_id: u32, value: f32) -> Result<(), String> {
        let inst = self
            .instances
            .get_mut(&instance)
            .ok_or_else(|| format!("no such plugin instance `{instance}`"))?;
        // Store the desired value + mark it dirty so the next `process`
        // feeds it to the plugin as a CLAP param-value event.
        inst.pending.insert(param_id, value);
        if !inst.dirty.contains(&param_id) {
            inst.dirty.push(param_id);
        }
        Ok(())
    }

    fn destroy(&mut self, instance: Uuid) -> Result<(), String> {
        self.instances.remove(&instance);
        Ok(())
    }
}

/// Enumerate a plugin's parameters via the CLAP `params` extension. Runs
/// on the main thread (call before activation). Returns an empty list for
/// plugins that don't expose the extension.
fn enumerate_params(instance: &mut PluginInstance<VanceHost>) -> Vec<PluginParam> {
    let mut handle = instance.plugin_handle();
    let Some(ext) = handle.get_extension::<PluginParams>() else {
        return Vec::new();
    };
    let count = ext.count(&mut handle);
    let mut out = Vec::with_capacity(count as usize);
    let mut buf = ParamInfoBuffer::default();
    for index in 0..count {
        let Some(info) = ext.get_info(&mut handle, index, &mut buf) else {
            continue;
        };
        let name = String::from_utf8_lossy(info.name)
            .trim_end_matches('\0')
            .to_string();
        out.push(PluginParam {
            id: info.id.get(),
            name,
            min: info.min_value as f32,
            max: info.max_value as f32,
            default: info.default_value as f32,
        });
    }
    out
}
