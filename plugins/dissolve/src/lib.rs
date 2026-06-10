//! Sample WASM transition plugin: "Luma Wipe Dissolve".
//!
//! Demonstrates the two-frame transition ABI. Blends frame B into
//! frame A, gated by a moving luma threshold so brighter regions of
//! B reveal first — a soft luma-wipe dissolve. One parameter:
//! - `mix` (0–1): transition progress. 0 = pure A, 1 = pure B.
//! - `softness` (0–1): width of the wipe's feathered edge.
//!
//! Build:
//! ```bash
//! cargo build --release --target wasm32-unknown-unknown \
//!     --manifest-path plugins/dissolve/Cargo.toml
//! ```

use vance_plugin_sdk::guest::Plugin;
use vance_plugin_sdk::manifest::{ParamSpec, PluginKind, PluginManifest};

struct Dissolve;

impl Plugin for Dissolve {
    fn manifest() -> PluginManifest {
        PluginManifest {
            id: "com.vance.sample.dissolve".into(),
            name: "Luma Wipe Dissolve".into(),
            version: "0.1.0".into(),
            author: "Vance".into(),
            kind: PluginKind::Transition,
            params: vec![
                ParamSpec {
                    key: "mix".into(),
                    label: "Progress".into(),
                    min: 0.0,
                    max: 1.0,
                    default: 0.5,
                },
                ParamSpec {
                    key: "softness".into(),
                    label: "Edge Softness".into(),
                    min: 0.0,
                    max: 1.0,
                    default: 0.25,
                },
            ],
        }
    }

    fn process_transition(a: &mut [u8], b: &[u8], _w: u32, _h: u32, params: &[f32]) {
        let mix = params.first().copied().unwrap_or(0.5).clamp(0.0, 1.0);
        let softness = params.get(1).copied().unwrap_or(0.25).clamp(0.0001, 1.0);

        // The wipe threshold sweeps from (1+softness) down to
        // (-softness) as mix goes 0..1, so at mix=0 the threshold sits
        // above every possible luma (nothing of B shows) and at mix=1
        // it sits below every luma (all of B shows).
        let threshold = (1.0 - mix) * (1.0 + softness) - softness;

        for (pa, pb) in a.chunks_exact_mut(4).zip(b.chunks_exact(4)) {
            let b_luma = (0.2126 * pb[0] as f32
                + 0.7152 * pb[1] as f32
                + 0.0722 * pb[2] as f32)
                / 255.0;
            // Reveal factor: 1 where b_luma is well above threshold,
            // 0 where well below, feathered across `softness`.
            let reveal = ((b_luma - threshold) / softness).clamp(0.0, 1.0);
            for c in 0..4 {
                let av = pa[c] as f32;
                let bv = pb[c] as f32;
                pa[c] = (av + (bv - av) * reveal).clamp(0.0, 255.0) as u8;
            }
        }
    }
}

vance_plugin_sdk::export_plugin!(Dissolve);
