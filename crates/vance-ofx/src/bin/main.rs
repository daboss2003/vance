//! `vance-ofx-host` sidecar.
//!
//! Discovers OFX bundles + `dlopen`s each to enumerate its plugins.
//! Running this out-of-process is the whole point: a malformed `.ofx`
//! that segfaults on load kills only this sidecar, leaving the editor
//! alive.

use std::path::PathBuf;
use vance_ofx::bundle;
use vance_ofx::ffi;
use vance_ofx::protocol::{OfxPluginEntry, OfxRequest, OfxResponse, PROTOCOL_VERSION};

fn main() -> vance_ipc::Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    tracing::info!("ofx-host sidecar starting");

    vance_ipc::serve::<OfxRequest, OfxResponse, _>(|req| match req {
        OfxRequest::Hello { .. } => OfxResponse::Hello {
            protocol_version: PROTOCOL_VERSION,
        },
        OfxRequest::ListPlugins { roots } => {
            let roots: Vec<PathBuf> = if roots.is_empty() {
                bundle::default_search_paths()
            } else {
                roots.into_iter().map(PathBuf::from).collect()
            };
            let bundles = bundle::discover(&roots);
            let mut plugins = Vec::new();
            let mut errors = Vec::new();
            for b in bundles {
                let Some(bin) = b.binary_path.as_ref() else {
                    errors.push(format!("{}: no binary for this platform", b.name));
                    continue;
                };
                match ffi::enumerate(bin) {
                    Ok(infos) => {
                        for info in infos {
                            plugins.push(OfxPluginEntry {
                                bundle: b.name.clone(),
                                bundle_path: b.bundle_path.display().to_string(),
                                api: info.api,
                                api_version: info.api_version,
                                identifier: info.identifier,
                                version_major: info.version_major,
                                version_minor: info.version_minor,
                            });
                        }
                    }
                    Err(e) => {
                        tracing::warn!(bundle = %b.name, error = %e, "ofx enumerate failed");
                        errors.push(format!("{}: {e}", b.name));
                    }
                }
            }
            OfxResponse::Plugins { plugins, errors }
        }
        OfxRequest::Shutdown => {
            tracing::info!("ofx-host sidecar shutting down");
            OfxResponse::Goodbye
        }
    })
}
