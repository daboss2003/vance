//! Sample WASM effect plugin: "Stylize".
//!
//! Demonstrates the Phase 10 plugin ABI. Applies a posterize +
//! saturation tweak to each RGBA pixel. Two parameters:
//! - `levels` (2–32): number of quantisation levels per channel.
//! - `saturation` (0–2): 0 = greyscale, 1 = unchanged, 2 = double.
//!
//! Build:
//! ```bash
//! cargo build --release --target wasm32-unknown-unknown \
//!     --manifest-path plugins/stylize/Cargo.toml
//! ```

use vance_plugin_sdk::guest::Plugin;
use vance_plugin_sdk::manifest::{ParamSpec, PluginKind, PluginManifest};

struct Stylize;

impl Plugin for Stylize {
    fn manifest() -> PluginManifest {
        PluginManifest {
            id: "com.vance.sample.stylize".into(),
            name: "Stylize".into(),
            version: "0.1.0".into(),
            author: "Vance".into(),
            kind: PluginKind::Effect,
            params: vec![
                ParamSpec {
                    key: "levels".into(),
                    label: "Posterize Levels".into(),
                    min: 2.0,
                    max: 32.0,
                    default: 6.0,
                },
                ParamSpec {
                    key: "saturation".into(),
                    label: "Saturation".into(),
                    min: 0.0,
                    max: 2.0,
                    default: 1.2,
                },
            ],
        }
    }

    fn process(rgba: &mut [u8], _w: u32, _h: u32, params: &[f32]) {
        let levels = params.first().copied().unwrap_or(6.0).clamp(2.0, 32.0);
        let saturation = params.get(1).copied().unwrap_or(1.0).clamp(0.0, 2.0);
        let step = 255.0 / (levels - 1.0);

        for px in rgba.chunks_exact_mut(4) {
            let mut r = px[0] as f32;
            let mut g = px[1] as f32;
            let mut b = px[2] as f32;

            // Saturation around luma (BT.709 weights).
            let luma = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            r = luma + (r - luma) * saturation;
            g = luma + (g - luma) * saturation;
            b = luma + (b - luma) * saturation;

            // Posterize.
            r = (r / step).round() * step;
            g = (g / step).round() * step;
            b = (b / step).round() * step;

            px[0] = r.clamp(0.0, 255.0) as u8;
            px[1] = g.clamp(0.0, 255.0) as u8;
            px[2] = b.clamp(0.0, 255.0) as u8;
            // Alpha untouched.
        }
    }
}

vance_plugin_sdk::export_plugin!(Stylize);
