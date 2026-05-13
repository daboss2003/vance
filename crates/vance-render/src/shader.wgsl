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
@group(0) @binding(1) var tex_y: texture_2d<f32>;
@group(0) @binding(2) var tex_u: texture_2d<f32>;
@group(0) @binding(3) var tex_v: texture_2d<f32>;

// Per-frame effect parameters. Layout must match `RenderParams` in
// renderer.rs.
struct Params {
    opacity: f32,
    rotation: f32,
    translate: vec2<f32>,
    scale: vec2<f32>,
};
@group(0) @binding(4) var<uniform> params: Params;

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

    let y_raw = textureSample(tex_y, samp, source_uv).r;
    let u_raw = textureSample(tex_u, samp, source_uv).r;
    let v_raw = textureSample(tex_v, samp, source_uv).r;

    // Rec.709 limited-range expansion.
    let y = (y_raw - 16.0 / 255.0) * (255.0 / 219.0);
    let u = (u_raw - 128.0 / 255.0) * (255.0 / 224.0);
    let v = (v_raw - 128.0 / 255.0) * (255.0 / 224.0);

    // Rec.709 Y'CbCr → R'G'B' (gamma-encoded; not linearized).
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;

    let rgb = clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
    return vec4<f32>(rgb * params.opacity, 1.0);
}
