//! Host runtime — loads + drives a guest `.wasm` plugin via `wasmtime`.
//!
//! Sandboxing properties (Phase 10 acceptance criterion "plugin crash
//! isolated; main app unaffected"):
//! - **No imports.** The module is instantiated with an empty import
//!   set — no WASI, no host functions, no filesystem, no clock, no
//!   network. A plugin can only touch its own linear memory.
//! - **Traps are values, not panics.** A guest out-of-bounds access,
//!   `unreachable`, or integer divide-by-zero surfaces as
//!   [`PluginError::Trap`] — the host thread keeps running.
//! - **Fuel-bounded.** Each process call is given a fixed fuel budget
//!   so an infinite loop in a plugin returns [`PluginError::Trap`]
//!   ("all fuel consumed") instead of hanging the app.
//! - **Memory-capped.** A [`wasmtime::StoreLimits`] caps the guest's
//!   linear memory growth so a plugin can't OOM the host.

use crate::abi;
use crate::manifest::PluginManifest;
use wasmtime::{
    Config, Engine, Instance, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc,
};

/// Fuel granted per process call. wasmtime charges ~1 unit per
/// executed wasm instruction; 2 billion is generous for a full-frame
/// pixel pass on a 4K image while still bounding a runaway loop.
const FUEL_PER_CALL: u64 = 2_000_000_000;

/// Max linear memory a plugin may grow to (256 MiB).
const MAX_MEMORY_BYTES: usize = 256 * 1024 * 1024;

/// Per-store host state — just the resource limiter.
struct HostState {
    limits: StoreLimits,
}

/// A loaded, instantiated WASM plugin ready to process frames.
pub struct WasmPlugin {
    store: Store<HostState>,
    instance: Instance,
    memory: Memory,
    manifest: PluginManifest,
    has_process: bool,
    has_process2: bool,
}

/// Errors from loading or running a plugin.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// wasmtime failed to compile / instantiate the module.
    #[error("wasm load: {0}")]
    Load(String),
    /// The module is missing a required export.
    #[error("plugin missing export `{0}`")]
    MissingExport(&'static str),
    /// The plugin's reported ABI version doesn't match the host's.
    #[error("plugin abi mismatch: host={host} plugin={plugin}")]
    AbiMismatch {
        /// Host ABI version.
        host: i32,
        /// Plugin-reported ABI version.
        plugin: i32,
    },
    /// Manifest parse / validate failed.
    #[error("plugin manifest: {0}")]
    Manifest(#[from] crate::manifest::ManifestError),
    /// A guest call trapped (out-of-bounds, fuel exhausted, …).
    #[error("plugin trap: {0}")]
    Trap(String),
    /// Caller passed a buffer whose length doesn't match `w*h*4`.
    #[error("frame size mismatch: got {got}, expected {expected}")]
    FrameSize {
        /// Length the caller passed.
        got: usize,
        /// Length implied by `w*h*4`.
        expected: usize,
    },
    /// Plugin kind doesn't support the requested operation.
    #[error("plugin does not export `{0}`")]
    UnsupportedOp(&'static str),
}

impl WasmPlugin {
    /// Compile + instantiate a plugin from raw `.wasm` bytes.
    pub fn load(wasm_bytes: &[u8]) -> Result<Self, PluginError> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|e| PluginError::Load(e.to_string()))?;
        let module =
            Module::new(&engine, wasm_bytes).map_err(|e| PluginError::Load(e.to_string()))?;

        let limits = StoreLimitsBuilder::new()
            .memory_size(MAX_MEMORY_BYTES)
            .build();
        let mut store = Store::new(&engine, HostState { limits });
        store.limiter(|s| &mut s.limits);
        store
            .set_fuel(FUEL_PER_CALL)
            .map_err(|e| PluginError::Load(e.to_string()))?;

        // Pure-compute module: no imports.
        let instance = Instance::new(&mut store, &module, &[])
            .map_err(|e| PluginError::Load(e.to_string()))?;

        let memory = instance
            .get_memory(&mut store, abi::EXPORT_MEMORY)
            .ok_or(PluginError::MissingExport(abi::EXPORT_MEMORY))?;

        // Required exports must be present.
        let f_abi: TypedFunc<(), i32> = instance
            .get_typed_func(&mut store, abi::EXPORT_ABI_VERSION)
            .map_err(|_| PluginError::MissingExport(abi::EXPORT_ABI_VERSION))?;
        let _: TypedFunc<i32, i32> = instance
            .get_typed_func(&mut store, abi::EXPORT_ALLOC)
            .map_err(|_| PluginError::MissingExport(abi::EXPORT_ALLOC))?;
        let f_dealloc: TypedFunc<(i32, i32), ()> = instance
            .get_typed_func(&mut store, abi::EXPORT_DEALLOC)
            .map_err(|_| PluginError::MissingExport(abi::EXPORT_DEALLOC))?;
        let f_manifest: TypedFunc<(), i64> = instance
            .get_typed_func(&mut store, abi::EXPORT_MANIFEST)
            .map_err(|_| PluginError::MissingExport(abi::EXPORT_MANIFEST))?;
        let has_process = instance
            .get_typed_func::<(i32, i32, i32, i32, i32), i32>(&mut store, abi::EXPORT_PROCESS)
            .is_ok();
        let has_process2 = instance
            .get_typed_func::<(i32, i32, i32, i32, i32, i32), i32>(
                &mut store,
                abi::EXPORT_PROCESS2,
            )
            .is_ok();

        // ABI version gate.
        let plugin_abi = f_abi
            .call(&mut store, ())
            .map_err(|e| PluginError::Trap(e.to_string()))?;
        if plugin_abi != abi::ABI_VERSION {
            return Err(PluginError::AbiMismatch {
                host: abi::ABI_VERSION,
                plugin: plugin_abi,
            });
        }

        // Read manifest.
        let packed = f_manifest
            .call(&mut store, ())
            .map_err(|e| PluginError::Trap(e.to_string()))?;
        let (m_ptr, m_len) = abi::unpack_ptr_len(packed);
        let mut ron = vec![0u8; m_len as usize];
        memory
            .read(&store, m_ptr as usize, &mut ron)
            .map_err(|e| PluginError::Trap(format!("manifest read: {e}")))?;
        let _ = f_dealloc.call(&mut store, (m_ptr as i32, m_len as i32));
        let manifest = PluginManifest::from_ron(std::str::from_utf8(&ron).map_err(|e| {
            PluginError::Manifest(crate::manifest::ManifestError::Invalid(format!(
                "manifest not utf-8: {e}"
            )))
        })?)?;

        Ok(Self {
            store,
            instance,
            memory,
            manifest,
            has_process,
            has_process2,
        })
    }

    /// The plugin's manifest.
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Run an effect plugin: mutate `rgba` in place.
    pub fn process(
        &mut self,
        rgba: &mut [u8],
        w: u32,
        h: u32,
        params: &[f32],
    ) -> Result<(), PluginError> {
        let expected = (w as usize) * (h as usize) * 4;
        if rgba.len() != expected {
            return Err(PluginError::FrameSize {
                got: rgba.len(),
                expected,
            });
        }
        if !self.has_process {
            return Err(PluginError::UnsupportedOp(abi::EXPORT_PROCESS));
        }
        self.refuel()?;

        let buf_ptr = self.guest_alloc(rgba.len() as i32)?;
        let params_bytes = f32_to_le(params);
        let params_ptr = self.guest_alloc(params_bytes.len() as i32)?;

        self.mem_write(buf_ptr, rgba)?;
        self.mem_write(params_ptr, &params_bytes)?;

        let f: TypedFunc<(i32, i32, i32, i32, i32), i32> = self
            .instance
            .get_typed_func(&mut self.store, abi::EXPORT_PROCESS)
            .map_err(|_| PluginError::MissingExport(abi::EXPORT_PROCESS))?;
        let rc = f
            .call(
                &mut self.store,
                (buf_ptr, w as i32, h as i32, params_ptr, params_bytes.len() as i32),
            )
            .map_err(|e| PluginError::Trap(e.to_string()))?;

        self.mem_read(buf_ptr, rgba)?;
        self.guest_dealloc(buf_ptr, rgba.len() as i32);
        self.guest_dealloc(params_ptr, params_bytes.len() as i32);

        if rc != 0 {
            return Err(PluginError::Trap(format!("process returned {rc}")));
        }
        Ok(())
    }

    /// Run a transition plugin: blend `b` into `a` in place.
    pub fn process_transition(
        &mut self,
        a: &mut [u8],
        b: &[u8],
        w: u32,
        h: u32,
        params: &[f32],
    ) -> Result<(), PluginError> {
        let expected = (w as usize) * (h as usize) * 4;
        if a.len() != expected || b.len() != expected {
            return Err(PluginError::FrameSize {
                got: a.len().min(b.len()),
                expected,
            });
        }
        if !self.has_process2 {
            return Err(PluginError::UnsupportedOp(abi::EXPORT_PROCESS2));
        }
        self.refuel()?;

        let a_ptr = self.guest_alloc(a.len() as i32)?;
        let b_ptr = self.guest_alloc(b.len() as i32)?;
        let params_bytes = f32_to_le(params);
        let params_ptr = self.guest_alloc(params_bytes.len() as i32)?;

        self.mem_write(a_ptr, a)?;
        self.mem_write(b_ptr, b)?;
        self.mem_write(params_ptr, &params_bytes)?;

        let f: TypedFunc<(i32, i32, i32, i32, i32, i32), i32> = self
            .instance
            .get_typed_func(&mut self.store, abi::EXPORT_PROCESS2)
            .map_err(|_| PluginError::MissingExport(abi::EXPORT_PROCESS2))?;
        let rc = f
            .call(
                &mut self.store,
                (
                    a_ptr,
                    b_ptr,
                    w as i32,
                    h as i32,
                    params_ptr,
                    params_bytes.len() as i32,
                ),
            )
            .map_err(|e| PluginError::Trap(e.to_string()))?;

        self.mem_read(a_ptr, a)?;
        self.guest_dealloc(a_ptr, a.len() as i32);
        self.guest_dealloc(b_ptr, b.len() as i32);
        self.guest_dealloc(params_ptr, params_bytes.len() as i32);

        if rc != 0 {
            return Err(PluginError::Trap(format!("process2 returned {rc}")));
        }
        Ok(())
    }

    fn refuel(&mut self) -> Result<(), PluginError> {
        self.store
            .set_fuel(FUEL_PER_CALL)
            .map_err(|e| PluginError::Trap(e.to_string()))
    }

    fn guest_alloc(&mut self, len: i32) -> Result<i32, PluginError> {
        let f: TypedFunc<i32, i32> = self
            .instance
            .get_typed_func(&mut self.store, abi::EXPORT_ALLOC)
            .map_err(|_| PluginError::MissingExport(abi::EXPORT_ALLOC))?;
        let ptr = f
            .call(&mut self.store, len)
            .map_err(|e| PluginError::Trap(e.to_string()))?;
        if ptr == 0 && len != 0 {
            return Err(PluginError::Trap("guest alloc returned null".into()));
        }
        Ok(ptr)
    }

    fn guest_dealloc(&mut self, ptr: i32, len: i32) {
        if let Ok(f) = self
            .instance
            .get_typed_func::<(i32, i32), ()>(&mut self.store, abi::EXPORT_DEALLOC)
        {
            let _ = f.call(&mut self.store, (ptr, len));
        }
    }

    fn mem_write(&mut self, ptr: i32, bytes: &[u8]) -> Result<(), PluginError> {
        self.memory
            .write(&mut self.store, ptr as usize, bytes)
            .map_err(|e| PluginError::Trap(format!("mem write: {e}")))
    }

    fn mem_read(&mut self, ptr: i32, out: &mut [u8]) -> Result<(), PluginError> {
        self.memory
            .read(&self.store, ptr as usize, out)
            .map_err(|e| PluginError::Trap(format!("mem read: {e}")))
    }
}

/// Pack an `f32` slice into little-endian bytes.
fn f32_to_le(params: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(params.len() * 4);
    for &p in params {
        out.extend_from_slice(&p.to_le_bytes());
    }
    out
}
