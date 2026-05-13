// Phase 1 video viewport shader.
//
// Inputs: three R8 luminance textures — Y at full resolution, U and V at
// half resolution. (NV12 sources are split into separate U and V CPU-side
// at upload; this shader is format-agnostic.)
//
// Color: Rec.709 limited-range YUV → linear-light Rec.709 RGB. NOT color
// managed; that's Phase 6.

// Fullscreen triangle. Generates UVs in [0,1] for a triangle that covers
// the whole viewport when rasterized.
struct VsOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    // Three vertices forming a triangle larger than the viewport.
    // (-1,-1), (3,-1), (-1,3) in clip space → UVs (0,1), (2,1), (0,-1).
    var out: VsOut;
    let x = f32(((vid << 1u) & 2u));   // 0, 2, 0
    let y = f32((vid & 2u));            // 0, 0, 2
    out.clip_position = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    out.uv = vec2<f32>(x, y);
    return out;
}

@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex_y_a: texture_2d<f32>;
@group(0) @binding(2) var tex_u_a: texture_2d<f32>;
@group(0) @binding(3) var tex_v_a: texture_2d<f32>;
@group(0) @binding(5) var tex_y_b: texture_2d<f32>;
@group(0) @binding(6) var tex_u_b: texture_2d<f32>;
@group(0) @binding(7) var tex_v_b: texture_2d<f32>;

// Per-frame effect parameters. Layout must match `RenderParams` in
// renderer.rs.
struct Params {
    opacity: f32,
    rotation: f32,
    translate: vec2<f32>,
    scale: vec2<f32>,
    brightness: f32,
    contrast: f32,
    crop_lrtb: vec4<f32>,
    mix_factor: f32,
    // (`_pad0` lives implicitly here — WGSL aligns the next vec4 to
    // offset 64, matching the 12 bytes Rust spells out explicitly.)
    // (r, g, b, enable) — chroma key color + on/off.
    chroma_key: vec4<f32>,
    // (tolerance, softness, _, _) — match radius + falloff width.
    chroma_params: vec4<f32>,
    // (threshold, direction, softness, enable). direction > 0 keeps
    // bright pixels, < 0 keeps dark ones.
    luma_key: vec4<f32>,
};
@group(0) @binding(4) var<uniform> params: Params;

fn yuv_to_rgb(y_raw: f32, u_raw: f32, v_raw: f32) -> vec3<f32> {
    // Rec.709 limited-range expansion.
    let y = (y_raw - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (u_raw - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (v_raw - 128.0 / 255.0) * (255.0 / 224.0);
    // Rec.709 Y'CbCr → R'G'B' (gamma-encoded; not linearized).
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;
    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Inverse-transform the output UV to find which source UV we
    // should sample. Operations apply in this order (forward):
    // scale around (0.5, 0.5) → rotate around (0.5, 0.5) → translate.
    // Inverse: untranslate → unrotate → unscale.
    var uv = in.uv;
    uv -= params.translate;
    let centered = uv - vec2<f32>(0.5, 0.5);
    let c = cos(-params.rotation);
    let s = sin(-params.rotation);
    let unrotated = vec2<f32>(
        centered.x * c - centered.y * s,
        centered.x * s + centered.y * c,
    );
    let unscaled = unrotated / params.scale;
    let source_uv = unscaled + vec2<f32>(0.5, 0.5);

    // Out-of-frame regions render black so transforms don't tile.
    if (source_uv.x < 0.0 || source_uv.x > 1.0
        || source_uv.y < 0.0 || source_uv.y > 1.0) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    // Crop is in *source* UV space — happens before the YUV→RGB
    // conversion. left/right/top/bottom are fractions of the source
    // frame to chop off each edge; outside that window we render
    // black so cropped pixels don't repeat under transform.
    if (source_uv.x < params.crop_lrtb.x
        || source_uv.x > 1.0 - params.crop_lrtb.y
        || source_uv.y < params.crop_lrtb.z
        || source_uv.y > 1.0 - params.crop_lrtb.w) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    let rgb_a = yuv_to_rgb(
        textureSample(tex_y_a, samp, source_uv).r,
        textureSample(tex_u_a, samp, source_uv).r,
        textureSample(tex_v_a, samp, source_uv).r,
    );

    // Cross-dissolve blend with the B-side. When mix_factor == 0
    // (the common case) the B samples vanish; the GPU still touches
    // those textures but the cost is negligible. Sampling B with
    // the same source_uv assumes both clips share the resolved
    // frame UV — the transform/crop apply uniformly to the blended
    // image. (If we ever want per-side transforms, that's a future
    // refactor.)
    let rgb_b = yuv_to_rgb(
        textureSample(tex_y_b, samp, source_uv).r,
        textureSample(tex_u_b, samp, source_uv).r,
        textureSample(tex_v_b, samp, source_uv).r,
    );
    var rgb = mix(rgb_a, rgb_b, params.mix_factor);
    // Contrast pivots around 0.5 (gamma-encoded midpoint), then
    // brightness shifts. Both apply in gamma space; that's not
    // strictly correct (linear would be cleaner), but it matches
    // the perceptual feel users expect from NLE color sliders.
    rgb = (rgb - vec3<f32>(0.5)) * params.contrast + vec3<f32>(0.5) + vec3<f32>(params.brightness);
    rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));

    // Chroma + luma keys produce a per-pixel "keep" factor in [0,1].
    // Without a multi-track composite path in v1 we just multiply
    // the keep factor into the final RGB — keyed pixels fade to
    // black instead of revealing whatever's underneath. (Multi-
    // track compositing is a later phase.)
    var keep = 1.0;
    if (params.chroma_key.a > 0.5) {
        let dist = length(rgb - params.chroma_key.rgb);
        let edge0 = params.chroma_params.x;
        let edge1 = params.chroma_params.x + max(params.chroma_params.y, 0.0001);
        keep = keep * smoothstep(edge0, edge1, dist);
    }
    if (params.luma_key.a > 0.5) {
        let lum = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
        let th = params.luma_key.x;
        let soft = max(params.luma_key.z, 0.0001);
        if (params.luma_key.y > 0.0) {
            // Keep brighter than threshold.
            keep = keep * smoothstep(th - soft, th + soft, lum);
        } else {
            // Keep darker than threshold.
            keep = keep * (1.0 - smoothstep(th - soft, th + soft, lum));
        }
    }
    return vec4<f32>(rgb * params.opacity * keep, 1.0);
}
