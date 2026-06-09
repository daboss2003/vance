//! `vance-plugin-host` sidecar binary.
//!
//! The parent `vance-app` process spawns this binary as the sandbox
//! that hosts third-party LV2 / CLAP plugins. All IPC happens over
//! stdin / stdout via the `vance_ipc` frame protocol. A crash here
//! takes down only this sandbox; the parent stays alive.

use vance_audio_plugins::{
    PluginDescriptor, PluginFormat, PluginParam, PluginRequest, PluginResponse, PROTOCOL_VERSION,
};
use vance_ipc::Result;
use vance_plugin_host::runtime::{supported_formats, PluginBackend, StubBackend};

fn main() -> Result<()> {
    // Stderr is inherited by the parent so tracing surfaces there.
    tracing_subscriber::fmt().with_writer(std::io::stderr).init();
    tracing::info!("plugin-host sidecar starting");

    let mut backend = StubBackend::new();
    let mut cached_plugins: Option<Vec<PluginDescriptor>> = None;

    vance_ipc::serve::<PluginRequest, PluginResponse, _>(|req| match req {
        PluginRequest::Hello { protocol_version } => {
            if protocol_version != PROTOCOL_VERSION {
                tracing::warn!(
                    parent = protocol_version,
                    sidecar = PROTOCOL_VERSION,
                    "protocol version mismatch — parent must update"
                );
            }
            PluginResponse::Hello {
                protocol_version: PROTOCOL_VERSION,
                supported_formats: supported_formats(),
            }
        }
        PluginRequest::ListPlugins => {
            let plugins = cached_plugins.get_or_insert_with(|| backend.list()).clone();
            PluginResponse::Plugins { plugins }
        }
        PluginRequest::Instantiate {
            descriptor,
            sample_rate,
            max_block_frames,
        } => match backend.instantiate(&descriptor, sample_rate, max_block_frames) {
            Ok(instance_id) => PluginResponse::Instantiated { instance_id },
            Err(message) => PluginResponse::Error { message },
        },
        PluginRequest::Process {
            instance_id,
            frames_in,
        } => match backend.process(instance_id, &frames_in) {
            Ok(frames_out) => PluginResponse::Processed { frames_out },
            Err(message) => PluginResponse::Error { message },
        },
        PluginRequest::ListParams { instance_id } => match backend.list_params(instance_id) {
            Ok(params) => PluginResponse::Params { params },
            Err(message) => PluginResponse::Error { message },
        },
        PluginRequest::SetParam {
            instance_id,
            param_id,
            value,
        } => match backend.set_param(instance_id, param_id, value) {
            Ok(()) => PluginResponse::Ok,
            Err(message) => PluginResponse::Error { message },
        },
        PluginRequest::Destroy { instance_id } => match backend.destroy(instance_id) {
            Ok(()) => PluginResponse::Ok,
            Err(message) => PluginResponse::Error { message },
        },
        PluginRequest::Shutdown => {
            tracing::info!("plugin-host sidecar shutting down");
            // The serve loop exits when stdin closes; we acknowledge here.
            PluginResponse::Ok
        }
    })?;

    // Avoid dead-code warnings on unused enum variants surfaced by the
    // wire types when the binary lacks every feature.
    let _ = PluginFormat::Clap;
    let _ = PluginParam {
        id: 0,
        name: String::new(),
        min: 0.0,
        max: 0.0,
        default: 0.0,
    };
    Ok(())
}
