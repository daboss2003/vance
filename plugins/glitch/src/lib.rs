//! Transition: "Glitch". A digital datamosh — horizontal block displacement +
//! RGB channel split that flickers and peaks mid-transition, crossfading A→B.
//! Params:
//! - `progress` (0–1): transition progress (injected by the host, hidden).
//! - `intensity` (0–1): displacement + channel-split strength.
//! - `blocks` (2–40): number of horizontal glitch bands.

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

/// Cheap integer hash → [0,1).
#[inline]
fn hash(n: u32) -> f32 {
    let mut x = n.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    x = ((x >> ((x >> 28).wrapping_add(4))) ^ x).wrapping_mul(277_803_737);
    x = (x >> 22) ^ x;
    (x & 0x00ff_ffff) as f32 / 16_777_215.0
}

struct Glitch;

impl Plugin for Glitch {
    fn manifest() -> PluginManifest {
        PluginManifest {
            id: "com.vance.fx.glitch".into(),
            name: "Glitch".into(),
            version: "0.1.0".into(),
            author: "Vance".into(),
            kind: PluginKind::Transition,
            params: vec![
                ParamSpec { key: "progress".into(), label: "Progress".into(), min: 0.0, max: 1.0, default: 0.5 },
                ParamSpec { key: "intensity".into(), label: "Intensity".into(), min: 0.0, max: 1.0, default: 0.6 },
                ParamSpec { key: "blocks".into(), label: "Bands".into(), min: 2.0, max: 40.0, default: 18.0 },
            ],
            gpu: None,
        }
    }

    fn process_transition(a: &mut [u8], b: &[u8], w: u32, h: u32, params: &[f32]) {
        let p = params.first().copied().unwrap_or(0.5).clamp(0.0, 1.0);
        let intensity = params.get(1).copied().unwrap_or(0.6).clamp(0.0, 1.0);
        let blocks = params.get(2).copied().unwrap_or(18.0).clamp(2.0, 40.0);
        let env = (PI * p).sin(); // 0 at the ends, 1 in the middle
        let block_h = (h as f32 / blocks).max(1.0);
        let step = (p * 24.0).floor() as u32; // flicker frames
        let wf = w as f32;
        let src = a.to_vec();
        for y in 0..h {
            let bi = (y as f32 / block_h) as u32;
            let r = hash(bi.wrapping_mul(97).wrapping_add(step.wrapping_mul(131)));
            let shift = (r * 2.0 - 1.0) * intensity * env * (wf * 0.12);
            let split = intensity * env * (wf * 0.03);
            for x in 0..w {
                let bx = x as f32 + shift;
                let fy = y as f32;
                let field = |xx: f32| mix4(sample(&src, w, h, xx, fy), sample(b, w, h, xx, fy), p);
                let cr = field(bx + split);
                let cg = field(bx);
                let cb = field(bx - split);
                let i = ((y * w + x) * 4) as usize;
                a[i] = cr[0].clamp(0.0, 255.0) as u8;
                a[i + 1] = cg[1].clamp(0.0, 255.0) as u8;
                a[i + 2] = cb[2].clamp(0.0, 255.0) as u8;
                a[i + 3] = cg[3].clamp(0.0, 255.0) as u8;
            }
        }
    }
}

vance_plugin_sdk::export_plugin!(Glitch);
