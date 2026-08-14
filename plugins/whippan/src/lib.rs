//! Transition: "Whip Pan". The outgoing clip streaks off in one direction with
//! motion blur while the incoming clip whips in from the other side — the fast
//! camera-swish cut. Params:
//! - `progress` (0–1): transition progress (injected by the host, hidden).
//! - `angle` (0–1): pan direction around a full circle (0 = left→right).
//! - `blur` (0–1): motion-blur streak length.

use core::f32::consts::TAU;
use vance_plugin_sdk::guest::Plugin;
use vance_plugin_sdk::manifest::{ParamSpec, PluginKind, PluginManifest};

const TAPS: usize = 6;

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

struct WhipPan;

impl Plugin for WhipPan {
    fn manifest() -> PluginManifest {
        PluginManifest {
            id: "com.vance.fx.whippan".into(),
            name: "Whip Pan".into(),
            version: "0.1.0".into(),
            author: "Vance".into(),
            kind: PluginKind::Transition,
            params: vec![
                ParamSpec { key: "progress".into(), label: "Progress".into(), min: 0.0, max: 1.0, default: 0.5 },
                ParamSpec { key: "angle".into(), label: "Direction".into(), min: 0.0, max: 1.0, default: 0.0 },
                ParamSpec { key: "blur".into(), label: "Blur".into(), min: 0.0, max: 1.0, default: 0.5 },
            ],
            gpu: None,
        }
    }

    fn process_transition(a: &mut [u8], b: &[u8], w: u32, h: u32, params: &[f32]) {
        let p = params.first().copied().unwrap_or(0.5).clamp(0.0, 1.0);
        let angle = params.get(1).copied().unwrap_or(0.0).clamp(0.0, 1.0);
        let blur = params.get(2).copied().unwrap_or(0.5).clamp(0.0, 1.0);
        let e = smoothstep(p);
        let th = angle * TAU;
        let (dx, dy) = (th.cos(), th.sin());
        let wf = w as f32;
        let off_a = e * wf; // A streaks out along +dir
        let off_b = (1.0 - e) * wf; // B rushes in from -dir
        let spread = blur * wf * 0.18 * (core::f32::consts::PI * p).sin();
        let src = a.to_vec();
        for y in 0..h {
            for x in 0..w {
                let fx = x as f32;
                let fy = y as f32;
                let mut acc_a = [0.0f32; 4];
                let mut acc_b = [0.0f32; 4];
                for t in 0..TAPS {
                    let f = t as f32 / (TAPS as f32 - 1.0) - 0.5; // -0.5..0.5
                    let ox = f * spread * dx;
                    let oy = f * spread * dy;
                    let sa = sample(&src, w, h, fx - dx * off_a + ox, fy - dy * off_a + oy);
                    let sb = sample(b, w, h, fx + dx * off_b + ox, fy + dy * off_b + oy);
                    for c in 0..4 {
                        acc_a[c] += sa[c];
                        acc_b[c] += sb[c];
                    }
                }
                let inv = 1.0 / TAPS as f32;
                for c in 0..4 {
                    acc_a[c] *= inv;
                    acc_b[c] *= inv;
                }
                let o = mix4(acc_a, acc_b, e);
                let i = ((y * w + x) * 4) as usize;
                for c in 0..4 {
                    a[i + c] = o[c].clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
}

vance_plugin_sdk::export_plugin!(WhipPan);
