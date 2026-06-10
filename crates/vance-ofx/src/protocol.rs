//! OFX host sidecar wire protocol.

use serde::{Deserialize, Serialize};

/// Wire protocol version.
pub const PROTOCOL_VERSION: u32 = 1;

/// Parent → sidecar.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum OfxRequest {
    /// Handshake.
    Hello {
        /// Parent protocol version.
        protocol_version: u32,
    },
    /// Discover bundles on the given roots, then `dlopen` each binary
    /// and enumerate its plugins. `roots` empty = use the platform
    /// default search paths.
    ListPlugins {
        /// Search roots; empty for platform defaults.
        roots: Vec<String>,
    },
    /// Graceful shutdown.
    Shutdown,
}

/// One enumerated OFX plugin, flattened for the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OfxPluginEntry {
    /// Bundle display name.
    pub bundle: String,
    /// Path to the bundle directory.
    pub bundle_path: String,
    /// API string.
    pub api: String,
    /// API version.
    pub api_version: i32,
    /// Plugin identifier.
    pub identifier: String,
    /// Major version.
    pub version_major: u32,
    /// Minor version.
    pub version_minor: u32,
}

/// Sidecar → parent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum OfxResponse {
    /// Reply to [`OfxRequest::Hello`].
    Hello {
        /// Sidecar protocol version.
        protocol_version: u32,
    },
    /// Reply to [`OfxRequest::ListPlugins`]. Bundles that failed to
    /// load are reported in `errors` rather than aborting the scan.
    Plugins {
        /// Successfully enumerated plugins.
        plugins: Vec<OfxPluginEntry>,
        /// Non-fatal per-bundle errors (bundle path + message).
        errors: Vec<String>,
    },
    /// Reply to [`OfxRequest::Shutdown`].
    Goodbye,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let r = OfxRequest::ListPlugins {
            roots: vec!["/a".into(), "/b".into()],
        };
        let s = serde_json::to_string(&r).unwrap();
        let _: OfxRequest = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn plugin_entry_round_trips() {
        let e = OfxPluginEntry {
            bundle: "Foo".into(),
            bundle_path: "/x/Foo.ofx.bundle".into(),
            api: "OfxImageEffectPluginAPI".into(),
            api_version: 1,
            identifier: "com.example.foo".into(),
            version_major: 2,
            version_minor: 3,
        };
        let s = serde_json::to_string(&e).unwrap();
        let back: OfxPluginEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
    }
}
