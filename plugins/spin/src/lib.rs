//! Transition: "Spin". The outgoing clip rotates + scales away while the
//! incoming clip spins in from the opposite angle. Params:
//! - `progress` (0–1): transition progress (injected by the host, hidden).
//! - `turns` (0–2): how many rotations across the transition.
//! - `zoom` (0–1): extra scale-in during the spin.

use core::f32::consts::TAU;
use vance_plugin_sdk::guest::Plugin;
use vance_plugin_sdk::manifest::{ParamSpec, PluginKind, PluginManifest};

#[inline]
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

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

#[inline]
fn put(dst: &mut [u8], i: usize, c: [f32; 4]) {
    for k in 0..4 {
        dst[i + k] = c[k].clamp(0.0, 255.0) as u8;
    }
}

struct Spin;

impl Plugin for Spin {
    fn manifest() -> PluginManifest {
        PluginManifest {
            id: "com.vance.fx.spin".into(),
            name: "Spin".into(),
            version: "0.1.0".into(),
            author: "Vance".into(),
            kind: PluginKind::Transition,
            params: vec![
                ParamSpec { key: "progress".into(), label: "Progress".into(), min: 0.0, max: 1.0, default: 0.5 },
                ParamSpec { key: "turns".into(), label: "Turns".into(), min: 0.0, max: 2.0, default: 0.5 },
                ParamSpec { key: "zoom".into(), label: "Zoom".into(), min: 0.0, max: 1.0, default: 0.3 },
            ],
            gpu: None,
        }
    }

    fn process_transition(a: &mut [u8], b: &[u8], w: u32, h: u32, params: &[f32]) {
        let p = params.first().copied().unwrap_or(0.5).clamp(0.0, 1.0);
        let turns = params.get(1).copied().unwrap_or(0.5).clamp(0.0, 2.0);
        let zoom = params.get(2).copied().unwrap_or(0.3).clamp(0.0, 1.0);
        let e = smoothstep(p);
        let cx = (w as f32 - 1.0) * 0.5;
        let cy = (h as f32 - 1.0) * 0.5;

        // source = center + R(-ang)/s * (out - center)
        let ang_a = e * turns * TAU;
        let ang_b = -(1.0 - e) * turns * TAU;
        let (sa, ac) = (ang_a.sin(), ang_a.cos());
        let (sb, bc) = (ang_b.sin(), ang_b.cos());
        let s_a = 1.0 + e * zoom * 2.0;
        let s_b = 1.0 + (1.0 - e) * zoom * 2.0;

        let src = a.to_vec();
        for y in 0..h {
            for x in 0..w {
                let dx = x as f32 - cx;
                let dy = y as f32 - cy;
                let ca = sample(&src, w, h, cx + (dx * ac + dy * sa) / s_a, cy + (-dx * sa + dy * ac) / s_a);
                let cb = sample(b, w, h, cx + (dx * bc + dy * sb) / s_b, cy + (-dx * sb + dy * bc) / s_b);
                put(a, ((y * w + x) * 4) as usize, mix4(ca, cb, e));
            }
        }
    }
}

vance_plugin_sdk::export_plugin!(Spin);
