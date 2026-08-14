//! Transition: "Zoom Punch". The outgoing clip zooms in and fades while the
//! incoming clip eases from zoomed-in back to normal — the classic TikTok zoom
//! cut. Params:
//! - `progress` (0–1): transition progress (injected by the host, hidden).
//! - `strength` (0–1): how hard the punch zooms.

use vance_plugin_sdk::guest::Plugin;
use vance_plugin_sdk::manifest::{ParamSpec, PluginKind, PluginManifest};

#[inline]
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Bilinear sample with clamp-to-edge; (x,y) in pixel space.
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

struct Zoom;

impl Plugin for Zoom {
    fn manifest() -> PluginManifest {
        PluginManifest {
            id: "com.vance.fx.zoom".into(),
            name: "Zoom Punch".into(),
            version: "0.1.0".into(),
            author: "Vance".into(),
            kind: PluginKind::Transition,
            params: vec![
                ParamSpec { key: "progress".into(), label: "Progress".into(), min: 0.0, max: 1.0, default: 0.5 },
                ParamSpec { key: "strength".into(), label: "Zoom".into(), min: 0.0, max: 1.0, default: 0.5 },
            ],
            gpu: None,
        }
    }

    fn process_transition(a: &mut [u8], b: &[u8], w: u32, h: u32, params: &[f32]) {
        let p = params.first().copied().unwrap_or(0.5).clamp(0.0, 1.0);
        let strength = params.get(1).copied().unwrap_or(0.5).clamp(0.0, 1.0);
        let e = smoothstep(p);
        let cx = (w as f32 - 1.0) * 0.5;
        let cy = (h as f32 - 1.0) * 0.5;
        let s_a = 1.0 + e * strength * 3.0; // A punches in as it leaves
        let s_b = 1.0 + (1.0 - e) * strength * 3.0; // B settles from zoomed-in
        let src = a.to_vec();
        for y in 0..h {
            for x in 0..w {
                let fx = x as f32;
                let fy = y as f32;
                let ca = sample(&src, w, h, cx + (fx - cx) / s_a, cy + (fy - cy) / s_a);
                let cb = sample(b, w, h, cx + (fx - cx) / s_b, cy + (fy - cy) / s_b);
                put(a, ((y * w + x) * 4) as usize, mix4(ca, cb, e));
            }
        }
    }
}

vance_plugin_sdk::export_plugin!(Zoom);
