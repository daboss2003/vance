//! Plugin runtime backends.
//!
//! The sidecar binary dispatches each [`PluginRequest`] to one of the
//! backends here. Backends are feature-gated so the binary builds
//! cleanly on systems missing the underlying libraries (notably
//! `liblilv` for LV2 on macOS).
//!
//! Phase 7.10 / 7.11 ship a unified architecture. Real LV2 hosting
//! via `livi` and CLAP hosting via `clack-host` activate when the
//! `lv2` / `clap` cargo features are enabled at build time. The
//! pure-Rust [`StubBackend`] runs without any system deps and
//! provides a working passthrough so the IPC + sandbox plumbing can
//! be tested end-to-end without a plugin library installed.

use std::collections::HashMap;
use uuid::Uuid;
use vance_audio_plugins::{PluginDescriptor, PluginFormat, PluginParam};

/// Trait implemented by every plugin backend. The sidecar binary
/// owns one or more backends and routes [`PluginRequest::Instantiate`]
/// to the one that can actually load the requested format.
pub trait PluginBackend {
    /// Which format does this backend host?
    fn format(&self) -> PluginFormat;

    /// Enumerate every plugin this backend can find on the system.
    fn list(&mut self) -> Vec<PluginDescriptor>;

    /// Try to instantiate. Returns `Ok(instance_id)` on success or a
    /// human-readable error string on failure.
    fn instantiate(
        &mut self,
        descriptor: &PluginDescriptor,
        sample_rate: f32,
        max_block_frames: u32,
    ) -> Result<Uuid, String>;

    /// Process one stereo block. The input is interleaved
    /// `(L, R, L, R, ...)` and the implementation MUST return a vector
    /// of the same length.
    fn process(&mut self, instance: Uuid, frames_in: &[f32]) -> Result<Vec<f32>, String>;

    /// Published parameters for one instance.
    fn list_params(&self, instance: Uuid) -> Result<Vec<PluginParam>, String>;

    /// Update one parameter.
    fn set_param(&mut self, instance: Uuid, param_id: u32, value: f32) -> Result<(), String>;

    /// Drop an instance. Idempotent.
    fn destroy(&mut self, instance: Uuid) -> Result<(), String>;
}

/// A pure-Rust no-system-deps backend that hosts a tiny set of
/// built-in passthrough "plugins." Always available so the IPC and
/// sandboxing path can be exercised without LV2 / CLAP installed.
///
/// Two plugins are surfaced — `vance:gain` (a fixed-gain stereo
/// passthrough with one `gain_db` parameter) and `vance:identity` (a
/// true no-op). They exist purely as integration scaffolding; real
/// host-side DSP lives in [`vance_audio::dsp`].
pub struct StubBackend {
    instances: HashMap<Uuid, StubInstance>,
}

struct StubInstance {
    descriptor: PluginDescriptor,
    gain_lin: f32,
}

impl StubBackend {
    /// New empty backend.
    pub fn new() -> Self {
        Self {
            instances: HashMap::new(),
        }
    }
}

impl Default for StubBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginBackend for StubBackend {
    fn format(&self) -> PluginFormat {
        // The stub hosts the in-process built-in reference plugins.
        PluginFormat::Builtin
    }

    fn list(&mut self) -> Vec<PluginDescriptor> {
        vec![
            PluginDescriptor {
                format: PluginFormat::Builtin,
                id: "vance:identity".into(),
                name: "Identity (stub)".into(),
                vendor: "vance".into(),
                runnable: true,
            },
            PluginDescriptor {
                format: PluginFormat::Builtin,
                id: "vance:gain".into(),
                name: "Gain (stub)".into(),
                vendor: "vance".into(),
                runnable: true,
            },
        ]
    }

    fn instantiate(
        &mut self,
        descriptor: &PluginDescriptor,
        _sample_rate: f32,
        _max_block_frames: u32,
    ) -> Result<Uuid, String> {
        if !matches!(descriptor.id.as_str(), "vance:identity" | "vance:gain") {
            return Err(format!(
                "stub backend does not know plugin id `{}` (install the `lv2` or `clap` feature for real hosting)",
                descriptor.id
            ));
        }
        let id = Uuid::new_v4();
        self.instances.insert(
            id,
            StubInstance {
                descriptor: descriptor.clone(),
                gain_lin: 1.0,
            },
        );
        Ok(id)
    }

    fn process(&mut self, instance: Uuid, frames_in: &[f32]) -> Result<Vec<f32>, String> {
        let inst = self
            .instances
            .get(&instance)
            .ok_or_else(|| format!("no such plugin instance `{instance}`"))?;
        match inst.descriptor.id.as_str() {
            "vance:identity" => Ok(frames_in.to_vec()),
            "vance:gain" => {
                let g = inst.gain_lin;
                Ok(frames_in.iter().map(|s| s * g).collect())
            }
            other => Err(format!("stub: cannot process plugin id `{other}`")),
        }
    }

    fn list_params(&self, instance: Uuid) -> Result<Vec<PluginParam>, String> {
        let inst = self
            .instances
            .get(&instance)
            .ok_or_else(|| format!("no such plugin instance `{instance}`"))?;
        match inst.descriptor.id.as_str() {
            "vance:gain" => Ok(vec![PluginParam {
                id: 0,
                name: "Gain (dB)".into(),
                min: -60.0,
                max: 24.0,
                default: 0.0,
            }]),
            _ => Ok(vec![]),
        }
    }

    fn set_param(&mut self, instance: Uuid, param_id: u32, value: f32) -> Result<(), String> {
        let inst = self
            .instances
            .get_mut(&instance)
            .ok_or_else(|| format!("no such plugin instance `{instance}`"))?;
        if inst.descriptor.id == "vance:gain" && param_id == 0 {
            let clamped = value.clamp(-60.0, 24.0);
            inst.gain_lin = 10f32.powf(clamped / 20.0);
            return Ok(());
        }
        Err(format!(
            "no such param ({param_id}) on `{}`",
            inst.descriptor.id
        ))
    }

    fn destroy(&mut self, instance: Uuid) -> Result<(), String> {
        self.instances.remove(&instance);
        Ok(())
    }
}

/// Which backend owns a given instance.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Owner {
    Stub,
    #[cfg(feature = "clap")]
    Clap,
    #[cfg(feature = "lv2")]
    Lv2,
}

/// Dispatches plugin requests to the right backend by format (on
/// instantiate) and by instance ownership (thereafter). The built-in
/// [`StubBackend`] is always present; the CLAP processing backend is
/// added when the `clap` feature is compiled in. A crash inside a CLAP
/// plugin takes down this whole sidecar process — which is the point:
/// the parent editor survives and bypasses the slot.
pub struct Router {
    stub: StubBackend,
    #[cfg(feature = "clap")]
    clap: crate::clap_backend::ClapBackend,
    #[cfg(feature = "lv2")]
    lv2: crate::lv2_backend::Lv2Backend,
    owners: HashMap<Uuid, Owner>,
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    /// New router with every compiled-in backend.
    pub fn new() -> Self {
        Self {
            stub: StubBackend::new(),
            #[cfg(feature = "clap")]
            clap: crate::clap_backend::ClapBackend::new(),
            #[cfg(feature = "lv2")]
            lv2: crate::lv2_backend::Lv2Backend::new(),
            owners: HashMap::new(),
        }
    }

    /// Built-in (stub) plugin descriptors. CLAP discovery is appended by
    /// the caller (`main`) from `clap_discovery`.
    pub fn list_builtin(&mut self) -> Vec<PluginDescriptor> {
        self.stub.list()
    }

    /// Instantiate, routing by the descriptor's format.
    pub fn instantiate(
        &mut self,
        descriptor: &PluginDescriptor,
        sample_rate: f32,
        max_block_frames: u32,
    ) -> Result<Uuid, String> {
        match descriptor.format {
            PluginFormat::Builtin => {
                let id = self.stub.instantiate(descriptor, sample_rate, max_block_frames)?;
                self.owners.insert(id, Owner::Stub);
                Ok(id)
            }
            PluginFormat::Clap => {
                #[cfg(feature = "clap")]
                {
                    let id = self.clap.instantiate(descriptor, sample_rate, max_block_frames)?;
                    self.owners.insert(id, Owner::Clap);
                    Ok(id)
                }
                #[cfg(not(feature = "clap"))]
                {
                    let _ = (sample_rate, max_block_frames);
                    Err("CLAP hosting not compiled into this sidecar (build --features clap)".into())
                }
            }
            PluginFormat::Lv2 => {
                #[cfg(feature = "lv2")]
                {
                    let id = self.lv2.instantiate(descriptor, sample_rate, max_block_frames)?;
                    self.owners.insert(id, Owner::Lv2);
                    Ok(id)
                }
                #[cfg(not(feature = "lv2"))]
                {
                    let _ = (sample_rate, max_block_frames);
                    Err("LV2 hosting not compiled into this sidecar (build --features lv2)".into())
                }
            }
        }
    }

    /// Process one stereo block on the backend that owns `instance`.
    pub fn process(&mut self, instance: Uuid, frames_in: &[f32]) -> Result<Vec<f32>, String> {
        match self.owner(instance)? {
            Owner::Stub => self.stub.process(instance, frames_in),
            #[cfg(feature = "clap")]
            Owner::Clap => self.clap.process(instance, frames_in),
            #[cfg(feature = "lv2")]
            Owner::Lv2 => self.lv2.process(instance, frames_in),
        }
    }

    /// List one instance's parameters.
    pub fn list_params(&self, instance: Uuid) -> Result<Vec<PluginParam>, String> {
        match self.owner(instance)? {
            Owner::Stub => self.stub.list_params(instance),
            #[cfg(feature = "clap")]
            Owner::Clap => self.clap.list_params(instance),
            #[cfg(feature = "lv2")]
            Owner::Lv2 => self.lv2.list_params(instance),
        }
    }

    /// Set one parameter.
    pub fn set_param(&mut self, instance: Uuid, param_id: u32, value: f32) -> Result<(), String> {
        match self.owner(instance)? {
            Owner::Stub => self.stub.set_param(instance, param_id, value),
            #[cfg(feature = "clap")]
            Owner::Clap => self.clap.set_param(instance, param_id, value),
            #[cfg(feature = "lv2")]
            Owner::Lv2 => self.lv2.set_param(instance, param_id, value),
        }
    }

    /// Destroy an instance (idempotent).
    pub fn destroy(&mut self, instance: Uuid) -> Result<(), String> {
        match self.owners.remove(&instance) {
            Some(Owner::Stub) => self.stub.destroy(instance),
            #[cfg(feature = "clap")]
            Some(Owner::Clap) => self.clap.destroy(instance),
            #[cfg(feature = "lv2")]
            Some(Owner::Lv2) => self.lv2.destroy(instance),
            None => Ok(()), // unknown / already gone → no-op
        }
    }

    fn owner(&self, instance: Uuid) -> Result<Owner, String> {
        self.owners
            .get(&instance)
            .copied()
            .ok_or_else(|| format!("no such plugin instance `{instance}`"))
    }
}

/// Static list of formats the binary was compiled with. The parent
/// uses this to populate its UI plugin-format dropdown.
pub fn supported_formats() -> Vec<PluginFormat> {
    // The built-in stub backend is always present; CLAP / LV2 hosting
    // light up only when their cargo features are compiled in.
    // `mut` is unused in a default (no-feature) build — the cfg pushes
    // below are the only mutators — so silence the lint feature-agnostically.
    #[allow(unused_mut)]
    let mut out = vec![PluginFormat::Builtin];
    #[cfg(feature = "clap")]
    out.push(PluginFormat::Clap);
    #[cfg(feature = "lv2")]
    out.push(PluginFormat::Lv2);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_lists_two_plugins() {
        let mut b = StubBackend::new();
        let plugins = b.list();
        assert_eq!(plugins.len(), 2);
        assert!(plugins.iter().any(|p| p.id == "vance:identity"));
        assert!(plugins.iter().any(|p| p.id == "vance:gain"));
    }

    #[test]
    fn stub_identity_passes_audio_through_unchanged() {
        let mut b = StubBackend::new();
        let id = b
            .instantiate(
                &PluginDescriptor {
                    format: PluginFormat::Lv2,
                    id: "vance:identity".into(),
                    name: "id".into(),
                    vendor: "v".into(),
                    runnable: true,
                },
                48_000.0,
                256,
            )
            .unwrap();
        let input = vec![0.1f32, -0.4, 0.7, -0.9];
        let out = b.process(id, &input).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn stub_gain_applies_db_correctly() {
        let mut b = StubBackend::new();
        let id = b
            .instantiate(
                &PluginDescriptor {
                    format: PluginFormat::Lv2,
                    id: "vance:gain".into(),
                    name: "g".into(),
                    vendor: "v".into(),
                    runnable: true,
                },
                48_000.0,
                256,
            )
            .unwrap();
        // Default is 0 dB (unity); apply -6 dB.
        b.set_param(id, 0, -6.0).unwrap();
        let input = vec![1.0f32, 1.0];
        let out = b.process(id, &input).unwrap();
        let expected = 10f32.powf(-6.0 / 20.0);
        assert!((out[0] - expected).abs() < 1e-4);
        assert!((out[1] - expected).abs() < 1e-4);
    }

    #[test]
    fn stub_rejects_unknown_plugin_id() {
        let mut b = StubBackend::new();
        let r = b.instantiate(
            &PluginDescriptor {
                format: PluginFormat::Lv2,
                id: "http://unknown.example.com/x".into(),
                name: "x".into(),
                vendor: "v".into(),
                runnable: true,
            },
            48_000.0,
            256,
        );
        assert!(r.is_err());
    }

    #[test]
    fn stub_destroy_is_idempotent() {
        let mut b = StubBackend::new();
        let id = b
            .instantiate(
                &PluginDescriptor {
                    format: PluginFormat::Lv2,
                    id: "vance:identity".into(),
                    name: "i".into(),
                    vendor: "v".into(),
                    runnable: true,
                },
                48_000.0,
                256,
            )
            .unwrap();
        assert!(b.destroy(id).is_ok());
        // Second destroy: no-op, not an error.
        assert!(b.destroy(id).is_ok());
        // Subsequent process fails cleanly.
        let r = b.process(id, &[0.0, 0.0]);
        assert!(r.is_err());
    }
}
