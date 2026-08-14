//! Transition: "RGB Split". A clean chromatic-aberration crossfade — the red
//! and blue channels slide apart, peaking mid-transition, as A dissolves to B.
//! Params:
//! - `progress` (0–1): transition progress (injected by the host, hidden).
//! - `amount` (0–1): how far the channels separate.

use core::f32::consts::PI;
use vance_plugin_sdk::guest::Plugin;
use vance_plugin_sdk::manifest::{ParamSpec, PluginKind, PluginManifest};

#[inline]
fn sample(buf: &[u8], w: u32, h: u32, x: f32, y: f32) -> [f32; 4] {
    if w == 0 || h == 0 {
        return [0.0; 4];
    }
    let xf = x.clamp(0.0, (w - 1) as f32);
    let yf = y.clamp(0.0, (h - 1) as f32);
    let x0 = xf.floor() as u32;
    let y0 = yf.floor() as u32;
    let x1 = (x0 + 1).min(w - 1);
    let y1 = (y0 + 1).min(h - 1);
    let fx = xf - x0 as f32;
    let fy = yf - y0 as f32;
    let px = |xx: u32, yy: u32, c: usize| buf[(((yy * w + xx) * 4) as usize) + c] as f32;
    let mut out = [0.0f32; 4];
    for c in 0..4 {
        let top = px(x0, y0, c) + (px(x1, y0, c) - px(x0, y0, c)) * fx;
        let bot = px(x0, y1, c) + (px(x1, y1, c) - px(x0, y1, c)) * fx;
        out[c] = top + (bot - top) * fy;
    }
    out
}

#[inline]
fn mix4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

struct RgbSplit;

impl Plugin for RgbSplit {
    fn manifest() -> PluginManifest {
        PluginManifest {
            id: "com.vance.fx.rgbsplit".into(),
            name: "RGB Split".into(),
            version: "0.1.0".into(),
            author: "Vance".into(),
            kind: PluginKind::Transition,
            params: vec![
                ParamSpec { key: "progress".into(), label: "Progress".into(), min: 0.0, max: 1.0, default: 0.5 },
                ParamSpec { key: "amount".into(), label: "Separation".into(), min: 0.0, max: 1.0, default: 0.5 },
            ],
            gpu: None,
        }
    }

    fn process_transition(a: &mut [u8], b: &[u8], w: u32, h: u32, params: &[f32]) {
        let p = params.first().copied().unwrap_or(0.5).clamp(0.0, 1.0);
        let amount = params.get(1).copied().unwrap_or(0.5).clamp(0.0, 1.0);
        let split = amount * (PI * p).sin() * (w as f32 * 0.06);
        let src = a.to_vec();
        for y in 0..h {
            let fy = y as f32;
            for x in 0..w {
                let fx = x as f32;
                let field = |xx: f32| mix4(sample(&src, w, h, xx, fy), sample(b, w, h, xx, fy), p);
                let cr = field(fx + split);
                let cg = field(fx);
                let cb = field(fx - split);
                let i = ((y * w + x) * 4) as usize;
                a[i] = cr[0].clamp(0.0, 255.0) as u8;
                a[i + 1] = cg[1].clamp(0.0, 255.0) as u8;
                a[i + 2] = cb[2].clamp(0.0, 255.0) as u8;
                a[i + 3] = cg[3].clamp(0.0, 255.0) as u8;
            }
        }
    }
}

vance_plugin_sdk::export_plugin!(RgbSplit);
