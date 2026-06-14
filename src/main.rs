use fontdue::Font;
use std::fs;

fn main() {
    let bytes = fs::read("/System/Library/Fonts/Supplemental/Arial.ttf").unwrap();
    let font = Font::from_bytes(bytes, fontdue::FontSettings::default()).unwrap();
    let size = 200.0f32;
    for ch in ['j', 'J', 'y', 'f', '/', 'g', 'A', 'a', 'W', 'V', 'T'] {
        let m = font.metrics(ch, size);
        println!(
            "char {:?}: xmin={} width={} advance_width={:.2} ymin={} height={}",
            ch, m.xmin, m.width, m.advance_width, m.ymin, m.height
        );
    }

    // Simulate render_text for left-aligned "j": x_start=0
    let ch = 'j';
    let (m, _bitmap) = font.rasterize(ch, size);
    let x_start = 0.0f32;
    let glyph_left = x_start + m.xmin as f32;
    println!("\nFor left-aligned 'j': glyph_left = {} (ink starts here)", glyph_left);
    // line_width is advance_width based:
    let lw = m.advance_width; // single glyph, no tracking, no kern
    let block_w = lw.ceil().max(1.0);
    println!("block_w = {} (from advance_width)", block_w);
    println!("ink right edge = glyph_left + width = {}", glyph_left + m.width as f32);
    if glyph_left < 0.0 {
        let clipped = (-glyph_left).ceil() as i32;
        println!("=> {} ink columns at x<0 would be CLIPPED (dropped by px<0 check)", clipped);
    } else {
        println!("=> no clipping");
    }
}
