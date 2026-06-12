//! Phase 10 — verify the RGBA preview blit (`present_rgba`) round-trips
//! a CPU buffer through the GPU unchanged. This is the path the live
//! preview uses to show WASM-effect output in the viewport.

use vance_render::{Renderer, RendererConfig};

#[test]
fn present_rgba_round_trips_pixels() {
    let mut r = match Renderer::new(RendererConfig::default()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("no GPU adapter ({e}) — skipping present_rgba test");
            return;
        }
    };
    let w = 8u32;
    let h = 8u32;
    // A distinct gradient so any channel swap / offset would show.
    let mut input = vec![0u8; (w * h * 4) as usize];
    for (i, px) in input.chunks_exact_mut(4).enumerate() {
        px[0] = (i * 3) as u8;
        px[1] = (i * 7) as u8;
        px[2] = (i * 11) as u8;
        px[3] = 255;
    }
    let out = r
        .present_rgba_to_buffer(&input, w, h)
        .expect("present_rgba");
    assert_eq!(out.len(), input.len());
    // sRGB upload→sample→re-encode is an identity round-trip up to a
    // rounding LSB. Assert each channel is within ±2.
    let mut max_diff = 0i32;
    for (a, b) in input.iter().zip(out.iter()) {
        max_diff = max_diff.max((*a as i32 - *b as i32).abs());
    }
    assert!(max_diff <= 2, "preview blit altered pixels (max diff {max_diff})");
}

#[test]
fn present_rgba_rejects_bad_buffer() {
    let mut r = match Renderer::new(RendererConfig::default()) {
        Ok(r) => r,
        Err(_) => return,
    };
    // 4×4 needs 64 bytes; pass 10.
    assert!(r.present_rgba_to_buffer(&[0u8; 10], 4, 4).is_err());
}
