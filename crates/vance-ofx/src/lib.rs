//! `vance-ofx` — read-only OpenFX plugin host.
//!
//! Phase 10.7. Discovers installed OFX bundles + enumerates the
//! plugins each exports, out-of-process so a malformed `.ofx` binary
//! that crashes on `dlopen` takes down only the sidecar, not the
//! editor.
//!
//! Scope: **read-only catalog**. The host lists installed OFX plugins
//! (identifier, version, API) for the effects browser. Actually
//! *running* an OFX plugin (the image-effect `render` action, which
//! needs the full host property/parameter/clip suites) is the post-v1
//! work the PLAN defers under "read-only first".
//!
//! - [`bundle`] — filesystem discovery of `*.ofx.bundle` dirs.
//! - [`ffi`] — minimal OpenFX FFI to enumerate a binary's plugins.
//! - [`protocol`] — sidecar wire types.
//! - [`OfxHostClient`] — parent-side client that drives the sidecar.

#![warn(missing_docs)]

pub mod bundle;
pub mod ffi;
pub mod protocol;

use protocol::{OfxPluginEntry, OfxRequest, OfxResponse, PROTOCOL_VERSION};
use std::path::Path;
use thiserror::Error;
use vance_ipc::Sidecar;

/// Parent-side OFX host client. Wraps one sidecar process.
pub struct OfxHostClient {
    sidecar: Sidecar<OfxRequest, OfxResponse>,
}

/// Errors from the parent-side client.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum OfxClientError {
    /// IPC failure (sidecar dead / framing).
    #[error("ofx ipc: {0}")]
    Ipc(#[from] vance_ipc::Error),
    /// Protocol version mismatch.
    #[error("ofx protocol mismatch: parent={parent} sidecar={sidecar}")]
    ProtocolMismatch {
        /// Parent version.
        parent: u32,
        /// Sidecar version.
        sidecar: u32,
    },
    /// Unexpected response variant.
    #[error("ofx protocol violation: expected {expected}")]
    Unexpected {
        /// What we expected.
        expected: &'static str,
    },
}

impl OfxHostClient {
    /// Spawn the sidecar binary + handshake.
    pub fn spawn(binary: impl AsRef<Path>) -> Result<Self, OfxClientError> {
        let mut sidecar: Sidecar<OfxRequest, OfxResponse> =
            Sidecar::spawn(binary, std::iter::empty::<&str>())?;
        match sidecar.request(&OfxRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        })? {
            OfxResponse::Hello { protocol_version } => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(OfxClientError::ProtocolMismatch {
                        parent: PROTOCOL_VERSION,
                        sidecar: protocol_version,
                    });
                }
                Ok(Self { sidecar })
            }
            _ => Err(OfxClientError::Unexpected { expected: "Hello" }),
        }
    }

    /// List installed OFX plugins. `roots` empty = platform defaults.
    /// Returns `(plugins, per_bundle_errors)`.
    pub fn list_plugins(
        &mut self,
        roots: Vec<String>,
    ) -> Result<(Vec<OfxPluginEntry>, Vec<String>), OfxClientError> {
        match self.sidecar.request(&OfxRequest::ListPlugins { roots })? {
            OfxResponse::Plugins { plugins, errors } => Ok((plugins, errors)),
            _ => Err(OfxClientError::Unexpected {
                expected: "Plugins",
            }),
        }
    }

    /// Graceful shutdown of the sidecar.
    pub fn shutdown(mut self) -> Result<(), OfxClientError> {
        let _ = self.sidecar.request(&OfxRequest::Shutdown);
        let _ = self.sidecar.shutdown();
        Ok(())
    }
}
