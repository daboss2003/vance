// W3C separable blend modes on premultiplied colour (ADR 0007). No resource
// bindings; concatenated in front of composite.wgsl (the blend pass) and
// layer.wgsl (its fused layer-over-backdrop pass).
//
// With C = c / a (0 when a = 0) and the source already scaled by the clip's
// opacity:
//   co = cs·(1 − ab) + cb·(1 − as) + as·ab·B(Cb, Cs);  ao = as + ab·(1 − as)

// Layout must match `CompositeUniform` in compositor.rs.
struct CompositeU {
    // (mode A, mode B, dissolve 0/1, backdrop still empty 0/1) — modes are
    // BlendMode::gpu_id. Over an empty (transparent) backdrop every mode
    // reduces exactly to the source, so the backdrop is not read.
    modes: vec4<u32>,
    // (opacity A, opacity B, dissolve mix, _)
    vals: vec4<f32>,
};

fn unpremul(c: vec4<f32>) -> vec3<f32> {
    if (c.a > 0.0) {
        return c.rgb / c.a;
    }
    return vec3<f32>(0.0);
}

fn screen3(b: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    return b + s - b * s;
}

// Screen without the [0,1] clamp (decision 4: an HDR highlight survives a
// Screen layer). Intentional: the W3C formula while both values are in
// [0,1] (every SDR value), max(Cb, Cs) once either is above 1; the two agree
// at 1, so the result is continuous. The plain formula would darken above 1
// (Cb 10 under Cs 1 gives 1.0, and two highlights of 2.0 give 0.0).
fn screen_hdr(b: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    let unit = (b <= vec3<f32>(1.0)) & (s <= vec3<f32>(1.0));
    return select(max(b, s), screen3(b, s), unit);
}

fn hard_light3(b: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    return select(screen3(b, 2.0 * s - vec3<f32>(1.0)), 2.0 * b * s, s <= vec3<f32>(0.5));
}

fn soft_light3(b: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    let d = select(sqrt(b), ((16.0 * b - vec3<f32>(12.0)) * b + vec3<f32>(4.0)) * b, b <= vec3<f32>(0.25));
    let lo = b - (vec3<f32>(1.0) - 2.0 * s) * b * (vec3<f32>(1.0) - b);
    let hi = b + (2.0 * s - vec3<f32>(1.0)) * (d - b);
    return select(hi, lo, s <= vec3<f32>(0.5));
}

fn color_dodge3(b: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    let one = vec3<f32>(1.0);
    let q = min(one, b / max(one - s, vec3<f32>(1e-6)));
    return select(select(q, one, s >= one), vec3<f32>(0.0), b == vec3<f32>(0.0));
}

fn color_burn3(b: vec3<f32>, s: vec3<f32>) -> vec3<f32> {
    let one = vec3<f32>(1.0);
    let q = one - min(one, (one - b) / max(s, vec3<f32>(1e-6)));
    return select(select(q, vec3<f32>(0.0), s <= vec3<f32>(0.0)), one, b == one);
}

// `src` over `bd` with blend mode `mode` (unknown ids composite as Normal).
fn blend_pm(bd: vec4<f32>, src: vec4<f32>, mode: u32) -> vec4<f32> {
    let ab = bd.a;
    let as_ = src.a;
    let ao = as_ + ab * (1.0 - as_);
    if (mode == 0u || mode > 11u) {
        // Normal: B = Cs reduces exactly to source-over.
        return vec4<f32>(src.rgb + bd.rgb * (1.0 - as_), ao);
    }
    var cb = unpremul(bd);
    var cs = unpremul(src);
    // Overlay, Soft/Hard Light, Color Dodge/Burn are defined on [0,1]; HDR
    // values above 1 would turn them non-monotonic. Screen is not clamped
    // (decision 4, see `screen_hdr`).
    if (mode == 4u || mode >= 8u) {
        cb = clamp(cb, vec3<f32>(0.0), vec3<f32>(1.0));
        cs = clamp(cs, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    var b: vec3<f32>;
    switch (mode) {
        case 1u: { b = cb + cs; }                 // Add
        case 2u: { b = cb * cs; }                 // Multiply
        case 3u: { b = screen_hdr(cb, cs); }      // Screen
        case 4u: { b = hard_light3(cs, cb); }     // Overlay = HardLight, swapped
        case 5u: { b = min(cb, cs); }             // Darken
        case 6u: { b = max(cb, cs); }             // Lighten
        case 7u: { b = abs(cb - cs); }            // Difference
        case 8u: { b = soft_light3(cb, cs); }     // Soft Light
        case 9u: { b = hard_light3(cb, cs); }     // Hard Light
        case 10u: { b = color_dodge3(cb, cs); }   // Color Dodge
        default: { b = color_burn3(cb, cs); }     // Color Burn (11)
    }
    let co = src.rgb * (1.0 - ab) + bd.rgb * (1.0 - as_) + as_ * ab * b;
    return vec4<f32>(co, ao);
}
