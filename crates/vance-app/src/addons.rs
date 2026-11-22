//! CPU add-on host for per-layer effects (ADR 0007).
//!
//! WASM (`wasm:`) and shader (`shader:`) effects run on a layer read back
//! as straight sRGB RGBA8, then the result is written back. The plugin
//! state lives here, behind `Arc<Mutex<AddonHost>>`, so the playback thread
//! can run add-ons too — not on `Engine`, which it can't reach.
//!
//! Lock ordering: the host never locks the render state (shader effects use
//! the device clone given to [`AddonHost::set_gpu`]). `compose_frame` runs
//! with the render state held by its caller, so the only legal order is
//! render state → add-on host; never lock the render state while holding
//! the host.
//!
//! The render cache (`apply_effects_cached`) keys on the input pixels and
//! the chain, including each plugin file's size, mtime and version, so an
//! updated plugin never shows a stale result; and a cached result is only
//! served when every add-on in the chain is allowed (entitlement gate) and
//! loads right now.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use vance_plugin_sdk::discovery::PluginRegistry;
use vance_plugin_sdk::host::WasmPlugin;
use vance_plugin_sdk::manifest::PluginManifest;
use vance_timeline::{Effect, EffectValue};

use crate::engine::EntitlementGate;

/// `true` when `effects` holds an enabled `wasm:` or `shader:` effect, i.e.
/// the layer needs a readback for the CPU add-on chain.
pub fn has_addons(effects: &[Effect]) -> bool {
    effects.iter().any(is_addon)
}

fn is_addon(e: &Effect) -> bool {
    e.enabled && (e.kind.starts_with("wasm:") || e.kind.starts_with("shader:"))
}

/// An add-on effect kind, split.
enum Addon<'a> {
    Wasm(&'a str),
    Shader(&'a str),
}

fn addon(kind: &str) -> Option<Addon<'_>> {
    kind.strip_prefix("wasm:")
        .map(Addon::Wasm)
        .or_else(|| kind.strip_prefix("shader:").map(Addon::Shader))
}

/// Discovered plugins, warm instances and compiled shader effects.
#[derive(Default)]
pub struct AddonHost {
    /// Discovered plugins (id → manifest + path).
    registry: PluginRegistry,
    /// Warm WASM instances keyed by manifest id, with the source file's
    /// mtime at load so an in-place edit reloads.
    plugins: HashMap<String, (WasmPlugin, Option<SystemTime>)>,
    /// Compiled `.vsfx` shader effects keyed by id, with the source mtime.
    shaders: HashMap<String, (vance_effects::ShaderEffect, Option<SystemTime>)>,
    /// Render device + queue for shader effects (the compositor's device).
    gpu: Option<(wgpu::Device, wgpu::Queue)>,
    /// Load-time entitlement gate; `None` = nothing is gated.
    gate: Option<Arc<EntitlementGate>>,
    /// Render cache for preview (`apply_effects_cached`); `None` = uncached.
    render_cache: Option<Arc<vance_cache::FrameCache>>,
}

impl AddonHost {
    /// Replace the plugin registry (after a rescan). Warm WASM instances are
    /// dropped so edited plugins reload, as `Engine::rescan_plugins` does.
    pub fn set_registry(&mut self, registry: PluginRegistry) {
        self.registry = registry;
        self.plugins.clear();
    }

    /// The current plugin registry.
    pub fn registry(&self) -> &PluginRegistry {
        &self.registry
    }

    /// Scan one more directory into the registry; returns the new total.
    pub fn scan_dir(&mut self, dir: &Path) -> usize {
        self.registry.scan_dir(dir);
        self.plugins.clear();
        self.registry.len()
    }

    /// Give shader effects the render device (clone the renderer's).
    pub fn set_gpu(&mut self, device: wgpu::Device, queue: wgpu::Queue) {
        self.gpu = Some((device, queue));
    }

    /// Install (or clear) the entitlement gate consulted at plugin load.
    pub fn set_entitlement_gate(&mut self, gate: Option<Arc<EntitlementGate>>) {
        self.gate = gate;
    }

    /// Install (or clear) the render cache used by
    /// [`Self::apply_effects_cached`].
    pub fn set_render_cache(&mut self, cache: Option<Arc<vance_cache::FrameCache>>) {
        self.render_cache = cache;
    }

    /// Apply every enabled `wasm:` / `shader:` effect in `effects` to a
    /// straight RGBA8 buffer, in chain order, with params evaluated at
    /// clip-relative `clip_t`. Best-effort: a plugin that fails to load or
    /// traps is logged and skipped so one bad plugin can't abort a render.
    /// Returns why each skipped add-on didn't run, in words for the user.
    pub fn apply_effects(
        &mut self,
        rgba: &mut [u8],
        w: u32,
        h: u32,
        effects: &[Effect],
        clip_t: f64,
    ) -> Vec<String> {
        let mut failures = Vec::new();
        for e in effects.iter().filter(|e| e.enabled) {
            match addon(&e.kind) {
                Some(Addon::Wasm(id)) => {
                    if let Err(why) = self.ensure_plugin_loaded(id) {
                        failures.push(why);
                        continue;
                    }
                    let Some((plugin, _)) = self.plugins.get_mut(id) else {
                        continue;
                    };
                    let packed = pack_params(plugin.manifest(), &e.params, clip_t);
                    if let Err(err) = plugin.process(rgba, w, h, &packed) {
                        tracing::warn!(plugin = %id, ?err, "wasm effect process failed; skipping");
                        failures.push(format!("the effect {} failed ({err})", self.name(id)));
                    }
                }
                Some(Addon::Shader(id)) => {
                    if let Err(why) = self.ensure_shader_loaded(id) {
                        failures.push(why);
                        continue;
                    }
                    let Some(disc) = self.registry.by_id(id) else {
                        continue;
                    };
                    let packed = pack_params(&disc.manifest, &e.params, clip_t);
                    if let Some((fx, _)) = self.shaders.get(id) {
                        if let Err(err) = fx.process(rgba, w, h, &packed) {
                            tracing::warn!(plugin = %id, %err, "shader effect process failed; skipping");
                            failures.push(format!("the effect {} failed ({err})", self.name(id)));
                        }
                    }
                }
                None => {}
            }
        }
        failures
    }

    /// [`Self::apply_effects`] through the render cache, keyed on the input
    /// pixels and the time-evaluated chain (plugin identities included). A
    /// hit skips the plugins. Without a cache this is the uncached apply.
    pub fn apply_effects_cached(
        &mut self,
        rgba: &mut [u8],
        w: u32,
        h: u32,
        effects: &[Effect],
        clip_t: f64,
    ) -> Vec<String> {
        let Some(effect_hash) = self.chain_hash(effects, w, h, clip_t) else {
            return Vec::new();
        };
        let Some(cache) = self.render_cache.clone() else {
            return self.apply_effects(rgba, w, h, effects, clip_t);
        };
        // A cached result stands in for running the chain, so only when the
        // whole chain would run now: a revoked entitlement or a plugin that
        // no longer loads must not be served from the (disk-persistent)
        // cache, and a partial result isn't cached.
        if !self.chain_runs(effects) {
            return self.apply_effects(rgba, w, h, effects, clip_t);
        }
        let input_hash = {
            let mut hsh = vance_cache::Fnv1a64::new();
            hsh.write(rgba);
            hsh.finish()
        };
        let key = vance_cache::CacheKey::for_render(input_hash, effect_hash);
        if let Some(cached) = cache.get(&key) {
            if cached.len() == rgba.len() {
                rgba.copy_from_slice(&cached);
                return Vec::new();
            }
        }
        let failures = self.apply_effects(rgba, w, h, effects, clip_t);
        if failures.is_empty() {
            let _ = cache.put(&key, rgba);
        }
        failures
    }

    /// Every add-on in `effects` is allowed and loaded (loading it if need
    /// be).
    fn chain_runs(&mut self, effects: &[Effect]) -> bool {
        effects
            .iter()
            .filter(|e| is_addon(e))
            .all(|e| match addon(&e.kind) {
                Some(Addon::Wasm(id)) => self.ensure_plugin_loaded(id).is_ok(),
                Some(Addon::Shader(id)) => self.ensure_shader_loaded(id).is_ok(),
                None => true,
            })
    }

    /// Render-cache hash of the add-on chain (kind, the plugin file's
    /// identity, time-evaluated params, dims), or `None` when the chain has
    /// no add-on. Covers `shader:` effects as well as `wasm:` ones, so a
    /// shader-only chain still runs and a shader param change invalidates
    /// the cached output.
    fn chain_hash(&self, effects: &[Effect], w: u32, h: u32, clip_t: f64) -> Option<u64> {
        let mut chain = vance_cache::Fnv1a64::new();
        chain.write(&w.to_le_bytes());
        chain.write(&h.to_le_bytes());
        let mut any = false;
        for e in effects.iter().filter(|e| is_addon(e)) {
            any = true;
            chain.write(e.kind.as_bytes());
            chain.write(&[0]);
            if let Some(Addon::Wasm(id) | Addon::Shader(id)) = addon(&e.kind) {
                self.write_plugin_identity(id, &mut chain);
            }
            // BTreeMap iteration is key-sorted → deterministic.
            for (k, v) in &e.params {
                if let Some(n) = v.eval_number(clip_t) {
                    chain.write(k.as_bytes());
                    chain.write(&n.to_le_bytes());
                }
            }
            chain.write(&[0xff]);
        }
        any.then(|| chain.finish())
    }

    /// The plugin file behind `id` (size, mtime) and its manifest version:
    /// an updated or edited plugin hashes differently.
    fn write_plugin_identity(&self, id: &str, h: &mut vance_cache::Fnv1a64) {
        let Some(disc) = self.registry.by_id(id) else {
            h.write(b"?not-installed");
            return;
        };
        h.write(disc.manifest.version.as_bytes());
        h.write(&[0]);
        match std::fs::metadata(&disc.path) {
            Ok(m) => {
                h.write(&m.len().to_le_bytes());
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_nanos());
                h.write(&mtime.to_le_bytes());
            }
            Err(_) => h.write(b"?unreadable"),
        }
        h.write(&[0]);
    }

    /// `id`'s display name, or `id` when it isn't installed.
    fn name(&self, id: &str) -> String {
        self.registry
            .by_id(id)
            .map_or_else(|| id.to_owned(), |d| d.manifest.name.clone())
    }

    /// Blend `b_rgba` into `a_rgba` with WASM transition `plugin_id`.
    /// `progress` (0 → 1) feeds the plugin's `mix` / `progress` param; the
    /// rest come from `params` (evaluated at `t`) or the manifest defaults.
    /// On any failure `a_rgba` is left unchanged (the transition shows A)
    /// and the reason, in words for the user, is returned.
    // Intentional: both frames, their size, the plugin and its param inputs
    // are independent per call, so they stay separate arguments (as on the
    // engine's original).
    #[allow(clippy::too_many_arguments)]
    pub fn apply_transition(
        &mut self,
        a_rgba: &mut [u8],
        b_rgba: &[u8],
        w: u32,
        h: u32,
        plugin_id: &str,
        params: &BTreeMap<String, EffectValue>,
        progress: f32,
        t: f64,
    ) -> Option<String> {
        if let Err(why) = self.ensure_plugin_loaded(plugin_id) {
            return Some(why);
        }
        let name = self.name(plugin_id);
        let (plugin, _) = self.plugins.get_mut(plugin_id)?;
        let packed = plugin.manifest().pack_params(&|key| {
            let lk = key.to_ascii_lowercase();
            if lk == "mix" || lk == "progress" {
                Some(progress)
            } else {
                params
                    .get(key)
                    .and_then(|v| v.eval_number(t))
                    .filter(|n| n.is_finite())
                    .map(|n| n as f32)
            }
        });
        if let Err(err) = plugin.process_transition(a_rgba, b_rgba, w, h, &packed) {
            tracing::warn!(plugin = %plugin_id, ?err, "wasm transition process failed; showing A");
            return Some(format!("the transition {name} failed ({err})"));
        }
        None
    }

    /// Whether the entitlement gate lets `id` run; the reason when not.
    fn gate_allows(&self, id: &str) -> Result<(), String> {
        match &self.gate {
            Some(gate) if !gate.allowed(id) => {
                tracing::warn!(plugin = %id, "blocked: this effect needs a valid entitlement");
                Err(format!(
                    "the effect {} needs a valid entitlement",
                    self.name(id)
                ))
            }
            _ => Ok(()),
        }
    }

    /// Ensure the `.vsfx` shader effect `id` is compiled and fresh (reloads
    /// on an in-place edit). Needs the device from [`Self::set_gpu`]. Gated
    /// like WASM effects (marketplace shaders are sold too).
    fn ensure_shader_loaded(&mut self, id: &str) -> Result<(), String> {
        self.gate_allows(id)?;
        let Some(disc) = self.registry.by_id(id).filter(|d| d.shader).cloned() else {
            return Err(format!("the effect {id} isn't installed"));
        };
        let name = disc.manifest.name.clone();
        let current_mtime = std::fs::metadata(&disc.path)
            .and_then(|m| m.modified())
            .ok();
        if let Some((_, cached)) = self.shaders.get(id) {
            if *cached == current_mtime {
                return Ok(());
            }
            self.shaders.remove(id);
        }
        let Some((device, queue)) = self.gpu.clone() else {
            tracing::warn!(plugin = %id, "no render device yet for shader effect");
            return Err(format!(
                "the effect {name} needs the GPU, which isn't ready"
            ));
        };
        let body = match std::fs::read(&disc.path)
            .map_err(|e| e.to_string())
            .and_then(|b| vance_plugin_sdk::shaderpkg::parse(&b).map(|p| p.shader))
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(plugin = %id, error = %e, "bad shader package");
                return Err(format!("the effect {name} is damaged ({e})"));
            }
        };
        match vance_effects::ShaderEffect::compile_with(device, queue, &body) {
            Ok(fx) => {
                self.shaders.insert(id.to_string(), (fx, current_mtime));
                Ok(())
            }
            Err(err) => {
                tracing::warn!(plugin = %id, %err, "shader effect compile failed");
                Err(format!("the effect {name} failed to compile ({err})"))
            }
        }
    }

    /// Ensure the WASM plugin `id` is loaded and fresh (reloads when the
    /// source file's mtime changed). The entitlement gate is checked on
    /// every call, so a revoked entitlement stops a warm instance too.
    fn ensure_plugin_loaded(&mut self, id: &str) -> Result<(), String> {
        self.gate_allows(id)?;
        let Some(disc) = self.registry.by_id(id).cloned() else {
            tracing::warn!(plugin = %id, "wasm effect references unknown plugin");
            return Err(format!("the effect {id} isn't installed"));
        };
        let name = disc.manifest.name.clone();
        let current_mtime = std::fs::metadata(&disc.path)
            .and_then(|m| m.modified())
            .ok();
        if let Some((_, cached_mtime)) = self.plugins.get(id) {
            if *cached_mtime == current_mtime {
                return Ok(());
            }
            self.plugins.remove(id);
        }
        let raw = match std::fs::read(&disc.path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(plugin = %id, error = %e, "wasm plugin read failed");
                return Err(format!("couldn't read the effect {name} ({e})"));
            }
        };
        // Encrypted (paid) effects are `.vef` containers: decrypt the body
        // via the gate. Without a valid key no plaintext ever materialises.
        let wasm_bytes: std::borrow::Cow<[u8]> = if disc.encrypted {
            let body = match vance_plugin_sdk::container::parse(&raw) {
                Ok(c) => c.body,
                Err(e) => {
                    tracing::warn!(plugin = %id, error = %e, "bad .vef container");
                    return Err(format!("the effect {name} is damaged ({e})"));
                }
            };
            match self.gate.as_ref().and_then(|g| g.decrypt(id, &body)) {
                Some(pt) => std::borrow::Cow::Owned(pt),
                None => {
                    tracing::warn!(plugin = %id, "blocked: paid effect couldn't be decrypted");
                    return Err(format!(
                        "the effect {name} couldn't be unlocked on this device"
                    ));
                }
            }
        } else {
            std::borrow::Cow::Borrowed(&raw[..])
        };
        match WasmPlugin::load(&wasm_bytes).map_err(|e| e.to_string()) {
            Ok(p) => {
                self.plugins.insert(id.to_string(), (p, current_mtime));
                Ok(())
            }
            Err(err) => {
                tracing::warn!(plugin = %id, %err, "wasm plugin load failed");
                Err(format!("the effect {name} failed to load ({err})"))
            }
        }
    }
}

/// Pack an effect's params for `manifest`, each evaluated at `clip_t` so
/// keyframes and expressions animate; non-finite values use the default.
fn pack_params(
    manifest: &PluginManifest,
    params: &BTreeMap<String, EffectValue>,
    clip_t: f64,
) -> Vec<f32> {
    manifest.pack_params(&|key| {
        params
            .get(key)
            .and_then(|v| v.eval_number(clip_t))
            .filter(|n| n.is_finite())
            .map(|n| n as f32)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use vance_plugin_sdk::manifest::{ParamSpec, PluginKind};
    use vance_plugin_sdk::shaderpkg::{self, ShaderPackage};

    fn effect(kind: &str, params: &[(&str, EffectValue)]) -> Effect {
        let mut e = Effect::new(kind);
        for (k, v) in params {
            e.params.insert((*k).to_string(), v.clone());
        }
        e
    }

    fn sample_plugin_dir(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins")
            .join(name)
            .join("target/wasm32-unknown-unknown/release")
    }

    fn gray(w: u32, h: u32) -> Vec<u8> {
        let mut v = vec![0u8; (w * h * 4) as usize];
        for (i, px) in v.chunks_exact_mut(4).enumerate() {
            px.copy_from_slice(&[(i * 7) as u8, (i * 3) as u8, (i * 11) as u8, 255]);
        }
        v
    }

    /// A `.vsfx` that scales colour by `amount`, discovered from `dir`.
    pub(crate) fn write_brighten_vsfx(dir: &Path) {
        let pkg = ShaderPackage {
            manifest: PluginManifest {
                id: "com.test.brighten".into(),
                name: "Brighten".into(),
                version: "1.0.0".into(),
                author: "Test".into(),
                kind: PluginKind::Effect,
                params: vec![ParamSpec {
                    key: "amount".into(),
                    label: "Amount".into(),
                    min: 0.0,
                    max: 2.0,
                    default: 0.5,
                }],
                gpu: None,
            },
            shader: "fn effect(color: vec4<f32>, uv: vec2<f32>) -> vec4<f32> { return vec4<f32>(color.rgb * param(0), color.a); }".into(),
        };
        std::fs::write(dir.join("com.test.brighten.vsfx"), shaderpkg::build(&pkg)).unwrap();
    }

    fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
        let r = vance_render::Renderer::new(vance_render::RendererConfig::default()).ok()?;
        Some((r.device().clone(), r.queue().clone()))
    }

    #[test]
    fn has_addons_needs_an_enabled_wasm_or_shader_effect() {
        assert!(!has_addons(&[]));
        assert!(!has_addons(&[effect("opacity", &[])]));
        assert!(has_addons(&[effect("wasm:x", &[])]));
        assert!(has_addons(&[effect("shader:x", &[])]));
        let mut off = effect("wasm:x", &[]);
        off.enabled = false;
        assert!(!has_addons(&[off]));
    }

    #[test]
    fn chain_hash_covers_shader_effects_and_their_params() {
        let host = AddonHost::default();
        let chain_hash = |fx: &[Effect], w, h| host.chain_hash(fx, w, h, 0.0);
        let a = effect("shader:fx", &[("amount", EffectValue::Number(0.5))]);
        let b = effect("shader:fx", &[("amount", EffectValue::Number(0.6))]);
        let ha = chain_hash(std::slice::from_ref(&a), 4, 4).expect("shader-only chain hashes");
        let hb = chain_hash(std::slice::from_ref(&b), 4, 4).unwrap();
        assert_ne!(ha, hb, "a shader param change must invalidate the cache");
        let w = effect("wasm:fx", &[("amount", EffectValue::Number(0.5))]);
        assert_ne!(ha, chain_hash(&[w], 4, 4).unwrap(), "kind prefix is hashed");
        assert_ne!(ha, chain_hash(std::slice::from_ref(&a), 4, 2).unwrap());
        assert!(chain_hash(&[effect("crop", &[])], 4, 4).is_none());
    }

    #[test]
    fn chain_hash_changes_when_the_plugin_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        let fx = [effect(
            "shader:com.test.brighten",
            &[("amount", EffectValue::Number(0.5))],
        )];
        let mut host = AddonHost::default();
        let not_installed = host.chain_hash(&fx, 4, 4, 0.0).unwrap();
        write_brighten_vsfx(dir.path());
        host.set_registry(PluginRegistry::scan(&[dir.path().to_path_buf()]));
        let installed = host.chain_hash(&fx, 4, 4, 0.0).unwrap();
        assert_ne!(installed, not_installed, "installing the plugin");
        assert_eq!(
            installed,
            host.chain_hash(&fx, 4, 4, 0.0).unwrap(),
            "stable"
        );
        // An update in place: same id, new file contents (and mtime).
        let file = dir.path().join("com.test.brighten.vsfx");
        let mut bytes = std::fs::read(&file).unwrap();
        bytes.extend_from_slice(b"\n");
        std::fs::write(&file, &bytes).unwrap();
        let updated = host.chain_hash(&fx, 4, 4, 0.0).unwrap();
        assert_ne!(updated, installed, "an updated plugin must miss the cache");
        // Same size, new mtime only (an edit that kept the length).
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_ne!(host.chain_hash(&fx, 4, 4, 0.0).unwrap(), updated, "mtime");
    }

    #[test]
    fn a_cache_hit_needs_the_entitlement_now() {
        let dir = sample_plugin_dir("stylize");
        if !dir.join("vance_plugin_stylize.wasm").exists() {
            eprintln!("stylize.wasm not built — skipping");
            return;
        }
        let id = "com.vance.sample.stylize";
        let mut host = AddonHost::default();
        host.set_registry(PluginRegistry::scan(&[dir]));
        let cache_dir = tempfile::tempdir().unwrap();
        let cache =
            vance_cache::FrameCache::new(cache_dir.path().to_path_buf(), 1 << 20, 1 << 22).unwrap();
        host.set_render_cache(Some(Arc::new(cache)));
        let gate = Arc::new(EntitlementGate::default());
        let gated: std::collections::HashSet<String> = [id.to_string()].into_iter().collect();
        gate.set_state(gated.clone(), gated.clone());
        host.set_entitlement_gate(Some(gate.clone()));
        let fx = [effect(
            &format!("wasm:{id}"),
            &[("levels", EffectValue::Number(3.0))],
        )];
        let input = gray(8, 8);
        let mut entitled = input.clone();
        assert!(host
            .apply_effects_cached(&mut entitled, 8, 8, &fx, 0.0)
            .is_empty());
        assert_ne!(entitled, input, "ran while entitled (and was cached)");
        gate.set_state(gated, Default::default());
        let mut revoked = input.clone();
        let failures = host.apply_effects_cached(&mut revoked, 8, 8, &fx, 0.0);
        assert_eq!(
            revoked, input,
            "a revoked entitlement must not be served from the cache"
        );
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("entitlement"), "{failures:?}");
    }

    #[test]
    fn chain_hash_tracks_keyframed_params_over_time() {
        let host = AddonHost::default();
        let chain_hash = |fx: &[Effect], t| host.chain_hash(fx, 2, 2, t);
        let kf = EffectValue::Keyframes {
            keyframes: vec![
                vance_timeline::Keyframe {
                    time_seconds: 0.0,
                    value: 0.0,
                    interp: vance_timeline::KeyframeInterp::Linear,
                },
                vance_timeline::Keyframe {
                    time_seconds: 1.0,
                    value: 1.0,
                    interp: vance_timeline::KeyframeInterp::Linear,
                },
            ],
        };
        let e = [effect("wasm:fx", &[("k", kf)])];
        assert_ne!(chain_hash(&e, 0.25), chain_hash(&e, 0.75));
    }

    #[test]
    fn unknown_plugins_are_skipped_without_touching_pixels() {
        let mut host = AddonHost::default();
        let input = gray(4, 4);
        let mut px = input.clone();
        let fx = [effect("wasm:nope", &[]), effect("shader:nope", &[])];
        let failures = host.apply_effects(&mut px, 4, 4, &fx, 0.0);
        assert_eq!(
            failures,
            [
                "the effect nope isn't installed",
                "the effect nope isn't installed"
            ]
        );
        assert_eq!(host.apply_effects_cached(&mut px, 4, 4, &fx, 0.0).len(), 2);
        let b = [255u8; 64];
        let why = host.apply_transition(&mut px, &b, 4, 4, "nope", &BTreeMap::new(), 0.5, 0.0);
        assert!(why.is_some_and(|w| w.contains("isn't installed")));
        assert_eq!(px, input);
    }

    #[test]
    fn gated_plugin_is_refused_even_when_discovered() {
        let dir = sample_plugin_dir("stylize");
        if !dir.join("vance_plugin_stylize.wasm").exists() {
            eprintln!("stylize.wasm not built — skipping");
            return;
        }
        let mut host = AddonHost::default();
        assert!(host.scan_dir(&dir) >= 1);
        let gate = Arc::new(EntitlementGate::default());
        gate.set_state(
            ["com.vance.sample.stylize".to_string()]
                .into_iter()
                .collect(),
            Default::default(),
        );
        host.set_entitlement_gate(Some(gate));
        let input = gray(8, 8);
        let mut px = input.clone();
        let fx = effect(
            "wasm:com.vance.sample.stylize",
            &[("levels", EffectValue::Number(3.0))],
        );
        let failures = host.apply_effects(&mut px, 8, 8, &[fx], 0.0);
        assert_eq!(px, input, "a gated effect without entitlement must not run");
        assert!(
            failures[0].contains("needs a valid entitlement"),
            "{failures:?}"
        );
    }

    #[test]
    fn wasm_effect_runs_and_cache_matches_uncached() {
        let dir = sample_plugin_dir("stylize");
        if !dir.join("vance_plugin_stylize.wasm").exists() {
            eprintln!("stylize.wasm not built — skipping");
            return;
        }
        let mut host = AddonHost::default();
        host.set_registry(PluginRegistry::scan(&[dir]));
        let cache_dir = tempfile::tempdir().unwrap();
        let cache =
            vance_cache::FrameCache::new(cache_dir.path().to_path_buf(), 1 << 20, 1 << 22).unwrap();
        host.set_render_cache(Some(Arc::new(cache)));
        let fx = [effect(
            "wasm:com.vance.sample.stylize",
            &[
                ("levels", EffectValue::Number(3.0)),
                ("saturation", EffectValue::Number(1.5)),
            ],
        )];
        let input = gray(16, 16);
        let mut a = input.clone();
        host.apply_effects(&mut a, 16, 16, &fx, 0.0);
        assert_ne!(a, input, "the plugin changes the frame");
        let mut b = input.clone();
        host.apply_effects_cached(&mut b, 16, 16, &fx, 0.0);
        let mut c = input.clone();
        host.apply_effects_cached(&mut c, 16, 16, &fx, 0.0);
        assert_eq!(a, b, "cache miss == uncached");
        assert_eq!(a, c, "cache hit == uncached");
    }

    #[test]
    fn wasm_transition_blends_b_into_a() {
        let dir = sample_plugin_dir("dissolve");
        if !dir.join("vance_plugin_dissolve.wasm").exists() {
            eprintln!("dissolve.wasm not built — skipping");
            return;
        }
        let mut host = AddonHost::default();
        host.set_registry(PluginRegistry::scan(&[dir]));
        let id = "com.vance.sample.dissolve";
        assert!(host.registry().by_id(id).is_some(), "dissolve discovered");
        let a0 = [0u8, 0, 0, 255].repeat(16);
        let b = [255u8, 255, 255, 255].repeat(16);
        let mut a = a0.clone();
        host.apply_transition(&mut a, &b, 4, 4, id, &BTreeMap::new(), 1.0, 0.0);
        assert_ne!(a, a0, "progress 1 shows (some of) B");
    }

    #[test]
    fn shader_only_chain_runs_cached_and_uncached() {
        let Some((device, queue)) = gpu() else {
            eprintln!("no GPU — skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        write_brighten_vsfx(dir.path());
        let mut host = AddonHost::default();
        host.set_registry(PluginRegistry::scan(&[dir.path().to_path_buf()]));
        host.set_gpu(device, queue);
        let cache_dir = tempfile::tempdir().unwrap();
        let cache =
            vance_cache::FrameCache::new(cache_dir.path().to_path_buf(), 1 << 20, 1 << 22).unwrap();
        host.set_render_cache(Some(Arc::new(cache)));
        let fx = [effect(
            "shader:com.test.brighten",
            &[("amount", EffectValue::Number(0.5))],
        )];
        let input = [100u8, 100, 100, 255].repeat(4);
        let mut a = input.clone();
        host.apply_effects(&mut a, 2, 2, &fx, 0.0);
        assert!(
            (a[0] as i32 - 50).abs() <= 1,
            "uncached shader ran: {}",
            a[0]
        );
        let mut b = input.clone();
        host.apply_effects_cached(&mut b, 2, 2, &fx, 0.0);
        assert_eq!(a, b, "a shader-only chain runs through the cached path too");
        // Shaders are gated like WASM effects.
        let gate = Arc::new(EntitlementGate::default());
        gate.set_state(
            ["com.test.brighten".to_string()].into_iter().collect(),
            Default::default(),
        );
        host.set_entitlement_gate(Some(gate));
        let mut c = input.clone();
        let failures = host.apply_effects_cached(&mut c, 2, 2, &fx, 0.0);
        assert_eq!(c, input, "a gated shader without entitlement must not run");
        assert!(failures[0].contains("entitlement"), "{failures:?}");
    }
}
