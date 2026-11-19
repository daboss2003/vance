//! Phase 7.10 / 7.11 — parent-side library for sandboxed audio
//! plugin hosting (LV2 + CLAP).
//!
//! This crate exports:
//! - The IPC wire types for the `vance-plugin-host` sidecar binary.
//! - A parent-side [`PluginClient`] (analogous to
//!   `vance_audio::AudioClient`) that owns one sidecar process and
//!   marshals plugin operations across it.
//!
//! The sandbox is per-process: ONE sidecar process can host MANY
//! plugin instances. If the sidecar crashes, every plugin in it dies
//! but the rest of vance keeps running. For maximum isolation the
//! engine creates one sidecar per track; for shared-state plugins it
//! reuses one per project.

#![warn(missing_docs)]

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;
use vance_ipc::Sidecar;

/// Protocol version exchanged on the [`PluginRequest::Hello`] handshake.
/// Bump on any breaking change to the wire format.
pub const PROTOCOL_VERSION: u32 = 1;

/// Plugin formats the sandbox supports. Used both by enumeration and
/// when instantiating. CLAP support depends on the sidecar being
/// built with the `clap` feature; LV2 support depends on the sidecar
/// being built with the `lv2` feature (it always advertises both
/// names in the wire format for forward-compat).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PluginFormat {
    /// Built-in reference effect hosted in-process (no external file).
    /// Ids like `vance:gain`, `vance:identity`.
    Builtin,
    /// LV2 plugin discovered via `lilv` (`LV2_PATH` walk).
    Lv2,
    /// CLAP plugin discovered via the host's `~/.clap` and
    /// `/Library/Audio/Plug-Ins/CLAP` paths (macOS) or the equivalents
    /// on other platforms.
    Clap,
}

/// Split a plugin id into `(bundle path, optional clap plugin id)`.
///
/// CLAP discovery emits ids as either a bare bundle path or the
/// composite `"<bundle-path>::<clap-plugin-id>"` (a `.clap` bundle can
/// publish several plugins). Both halves may themselves contain `::` — a
/// path with `::` is unusual but legal; a clap id with `::` is unusual
/// but not forbidden — so a naive split mis-routes either one. We resolve
/// the ambiguity against the filesystem:
///
/// 1. If the whole id is an existing path, it is a bare bundle (no
///    specific plugin); a real bundle whose name contains `::` wins.
/// 2. Otherwise scan `::` boundaries left-to-right and take the FIRST
///    whose left side is an existing path; the remainder (which may
///    contain further `::`) is the clap plugin id.
/// 3. If no boundary yields an existing path, the whole id is treated as
///    the (doomed) bundle path — the load fails downstream and the slot
///    is bypassed.
///
/// Shared by the in-process host (`vance_audio::clap_host`) and the
/// sidecar backend (`vance_plugin_host::clap_backend`) so both agree.
pub fn split_clap_id(id: &str) -> (&str, Option<&str>) {
    use std::path::Path;
    if Path::new(id).exists() {
        return (id, None);
    }
    let mut from = 0;
    while let Some(rel) = id[from..].find("::") {
        let pos = from + rel;
        let left = &id[..pos];
        if Path::new(left).exists() {
            return (left, Some(&id[pos + 2..]));
        }
        from = pos + 2;
    }
    (id, None)
}

/// One enumerated plugin descriptor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginDescriptor {
    /// Format the plugin uses on disk.
    pub format: PluginFormat,
    /// Format-specific identifier: LV2 plugin URI for LV2,
    /// `bundle_path:plugin_id` for CLAP (since CLAP bundles can host
    /// multiple plugin classes).
    pub id: String,
    /// Human-readable plugin name.
    pub name: String,
    /// Plugin vendor / author.
    pub vendor: String,
    /// Whether the sidecar's compile-time feature set can actually
    /// instantiate this plugin. Plugins with `runnable = false` are
    /// surfaced for transparency but cannot be added to a chain.
    pub runnable: bool,
}

/// One published parameter on an instantiated plugin.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginParam {
    /// Sidecar-assigned numeric id, stable for the instance lifetime.
    pub id: u32,
    /// Human-readable name.
    pub name: String,
    /// Inclusive parameter range.
    pub min: f32,
    /// Inclusive parameter range.
    pub max: f32,
    /// Default value the plugin reports.
    pub default: f32,
}

/// IPC request: parent → sidecar.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PluginRequest {
    /// Handshake — first message after the sidecar starts.
    Hello {
        /// Parent's protocol version.
        protocol_version: u32,
    },
    /// Walk the system for installed plugins. Cheap on subsequent
    /// calls — the sidecar caches the enumeration result.
    ListPlugins,
    /// Instantiate a plugin. Returns a stable [`Uuid`] handle.
    Instantiate {
        /// Plugin to load.
        descriptor: PluginDescriptor,
        /// Device sample rate.
        sample_rate: f32,
        /// Maximum block size the parent will pass through
        /// [`PluginRequest::Process`].
        max_block_frames: u32,
    },
    /// Push a stereo audio block through one plugin.
    Process {
        /// Plugin to drive.
        instance_id: Uuid,
        /// Interleaved stereo input (l0, r0, l1, r1, ...).
        frames_in: Vec<f32>,
    },
    /// List the plugin's published parameters.
    ListParams {
        /// Plugin to query.
        instance_id: Uuid,
    },
    /// Set one parameter on a plugin. Live during playback.
    SetParam {
        /// Plugin to mutate.
        instance_id: Uuid,
        /// Parameter id from [`PluginParam::id`].
        param_id: u32,
        /// New value (clamped server-side).
        value: f32,
    },
    /// Destroy a plugin instance.
    Destroy {
        /// Plugin to release.
        instance_id: Uuid,
    },
    /// Graceful shutdown — sidecar drains every instance + exits.
    Shutdown,
}

/// IPC response: sidecar → parent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PluginResponse {
    /// Reply to [`PluginRequest::Hello`].
    Hello {
        /// Sidecar's protocol version.
        protocol_version: u32,
        /// Plugin formats this sidecar build supports.
        supported_formats: Vec<PluginFormat>,
    },
    /// Reply to [`PluginRequest::ListPlugins`].
    Plugins {
        /// All discovered plugins.
        plugins: Vec<PluginDescriptor>,
    },
    /// Reply to [`PluginRequest::Instantiate`].
    Instantiated {
        /// Stable handle for subsequent process / set-param calls.
        instance_id: Uuid,
    },
    /// Reply to [`PluginRequest::Process`].
    Processed {
        /// Output samples, same length as the input.
        frames_out: Vec<f32>,
    },
    /// Reply to [`PluginRequest::ListParams`].
    Params {
        /// Parameters in display order.
        params: Vec<PluginParam>,
    },
    /// Reply to [`PluginRequest::SetParam`] / [`PluginRequest::Destroy`].
    Ok,
    /// Sidecar reports a failure.
    Error {
        /// Human-readable cause.
        message: String,
    },
}

/// Parent-side plugin-host client. Wraps one sidecar process.
pub struct PluginClient {
    sidecar: Sidecar<PluginRequest, PluginResponse>,
    supported_formats: Vec<PluginFormat>,
}

/// Errors surfaced by the parent-side plugin client.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum PluginClientError {
    /// IPC failure (sidecar dead or wire format mismatch).
    #[error("plugin ipc: {0}")]
    Ipc(#[from] vance_ipc::Error),
    /// Sidecar protocol version mismatch.
    #[error("plugin protocol mismatch: parent={parent} sidecar={sidecar}")]
    ProtocolMismatch {
        /// Parent's version.
        parent: u32,
        /// Sidecar's version.
        sidecar: u32,
    },
    /// Sidecar replied with an unexpected variant.
    #[error("plugin protocol violation: expected {expected}, got {got:?}")]
    UnexpectedResponse {
        /// What we expected.
        expected: &'static str,
        /// What we got.
        got: PluginResponse,
    },
    /// Sidecar returned an explicit error.
    #[error("plugin sidecar error: {0}")]
    Sidecar(String),
}

impl PluginClient {
    /// Spawn the sidecar binary at `binary` and complete the
    /// protocol handshake.
    pub fn spawn(binary: impl AsRef<std::path::Path>) -> Result<Self, PluginClientError> {
        Self::spawn_with_env(binary, std::iter::empty::<(&str, &str)>())
    }

    /// Like [`PluginClient::spawn`] but also sets extra environment
    /// variables on the sidecar child process. Lets a caller hand the
    /// sandbox e.g. `LV2_PATH` without mutating the parent's process-global
    /// environment (which races with concurrent spawns).
    pub fn spawn_with_env<E, K, V>(
        binary: impl AsRef<std::path::Path>,
        env: E,
    ) -> Result<Self, PluginClientError>
    where
        E: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        let mut sidecar: Sidecar<PluginRequest, PluginResponse> =
            Sidecar::spawn_with_env(binary, std::iter::empty::<&str>(), env)?;
        match sidecar.request(&PluginRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        })? {
            PluginResponse::Hello {
                protocol_version,
                supported_formats,
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(PluginClientError::ProtocolMismatch {
                        parent: PROTOCOL_VERSION,
                        sidecar: protocol_version,
                    });
                }
                Ok(Self {
                    sidecar,
                    supported_formats,
                })
            }
            other => Err(PluginClientError::UnexpectedResponse {
                expected: "Hello",
                got: other,
            }),
        }
    }

    /// Which plugin formats can this sidecar host?
    pub fn supported_formats(&self) -> &[PluginFormat] {
        &self.supported_formats
    }

    /// OS PID of the sidecar process, if it's still running. Used by the
    /// out-of-process host's watchdog to kill a hung sidecar (the owning
    /// thread is blocked in the IPC call and can't kill it via `&mut`).
    pub fn pid(&self) -> Option<u32> {
        self.sidecar.pid()
    }

    /// Enumerate every plugin the sidecar can see.
    pub fn list_plugins(&mut self) -> Result<Vec<PluginDescriptor>, PluginClientError> {
        match self.sidecar.request(&PluginRequest::ListPlugins)? {
            PluginResponse::Plugins { plugins } => Ok(plugins),
            PluginResponse::Error { message } => Err(PluginClientError::Sidecar(message)),
            other => Err(PluginClientError::UnexpectedResponse {
                expected: "Plugins",
                got: other,
            }),
        }
    }

    /// Instantiate a plugin.
    pub fn instantiate(
        &mut self,
        descriptor: PluginDescriptor,
        sample_rate: f32,
        max_block_frames: u32,
    ) -> Result<Uuid, PluginClientError> {
        match self.sidecar.request(&PluginRequest::Instantiate {
            descriptor,
            sample_rate,
            max_block_frames,
        })? {
            PluginResponse::Instantiated { instance_id } => Ok(instance_id),
            PluginResponse::Error { message } => Err(PluginClientError::Sidecar(message)),
            other => Err(PluginClientError::UnexpectedResponse {
                expected: "Instantiated",
                got: other,
            }),
        }
    }

    /// Drive one block of stereo audio through a plugin.
    pub fn process(
        &mut self,
        instance_id: Uuid,
        frames_in: &[f32],
    ) -> Result<Vec<f32>, PluginClientError> {
        match self.sidecar.request(&PluginRequest::Process {
            instance_id,
            frames_in: frames_in.to_vec(),
        })? {
            PluginResponse::Processed { frames_out } => Ok(frames_out),
            PluginResponse::Error { message } => Err(PluginClientError::Sidecar(message)),
            other => Err(PluginClientError::UnexpectedResponse {
                expected: "Processed",
                got: other,
            }),
        }
    }

    /// List the plugin's published parameters.
    pub fn list_params(
        &mut self,
        instance_id: Uuid,
    ) -> Result<Vec<PluginParam>, PluginClientError> {
        match self
            .sidecar
            .request(&PluginRequest::ListParams { instance_id })?
        {
            PluginResponse::Params { params } => Ok(params),
            PluginResponse::Error { message } => Err(PluginClientError::Sidecar(message)),
            other => Err(PluginClientError::UnexpectedResponse {
                expected: "Params",
                got: other,
            }),
        }
    }

    /// Set one parameter on a plugin.
    pub fn set_param(
        &mut self,
        instance_id: Uuid,
        param_id: u32,
        value: f32,
    ) -> Result<(), PluginClientError> {
        match self.sidecar.request(&PluginRequest::SetParam {
            instance_id,
            param_id,
            value,
        })? {
            PluginResponse::Ok => Ok(()),
            PluginResponse::Error { message } => Err(PluginClientError::Sidecar(message)),
            other => Err(PluginClientError::UnexpectedResponse {
                expected: "Ok",
                got: other,
            }),
        }
    }

    /// Tear down a single plugin instance.
    pub fn destroy(&mut self, instance_id: Uuid) -> Result<(), PluginClientError> {
        match self
            .sidecar
            .request(&PluginRequest::Destroy { instance_id })?
        {
            PluginResponse::Ok => Ok(()),
            PluginResponse::Error { message } => Err(PluginClientError::Sidecar(message)),
            other => Err(PluginClientError::UnexpectedResponse {
                expected: "Ok",
                got: other,
            }),
        }
    }

    /// Graceful shutdown of the sidecar process.
    pub fn shutdown(mut self) -> Result<(), PluginClientError> {
        let _ = self.sidecar.request(&PluginRequest::Shutdown);
        let _ = self.sidecar.shutdown();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_round_trips_lv2_and_clap() {
        let lv2 = PluginDescriptor {
            format: PluginFormat::Lv2,
            id: "http://example.com/plugins/test".into(),
            name: "Test".into(),
            vendor: "Vendor".into(),
            runnable: true,
        };
        let clap = PluginDescriptor {
            format: PluginFormat::Clap,
            id: "/Library/Audio/Plug-Ins/CLAP/x.clap:foo".into(),
            name: "X".into(),
            vendor: "Y".into(),
            runnable: false,
        };
        for d in [lv2, clap] {
            let s = serde_json::to_string(&d).unwrap();
            let back: PluginDescriptor = serde_json::from_str(&s).unwrap();
            assert_eq!(back, d);
        }
    }

    #[test]
    fn protocol_request_round_trips_serde() {
        let msg = PluginRequest::Hello {
            protocol_version: 1,
        };
        let s = serde_json::to_string(&msg).unwrap();
        let _: PluginRequest = serde_json::from_str(&s).unwrap();
    }
}
