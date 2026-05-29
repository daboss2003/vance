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
@group(0) @binding(8) var tex_lut: texture_3d<f32>;
@group(0) @binding(9) var tex_view_lut: texture_3d<f32>;

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
    // (input_transfer, output_transfer, input_range, output_range)
    // — IDs match vance_color::pipeline.
    pipeline_flags: vec4<u32>,
    // 3×3 linear-RGB matrices stored as 3 rows × 4 cols (last col
    // padding). Maps input primaries → working, then working → out.
    pipeline_in_to_working: array<vec4<f32>, 3>,
    pipeline_working_to_out: array<vec4<f32>, 3>,
    // (enable, _, _, _) — toggle the bound LUT.
    lut_params: vec4<f32>,
    // View-LUT enable. Applies post-output as a monitoring stage.
    view_lut_params: vec4<f32>,
    // Number of active grade nodes; only .x is read.
    grade_count: vec4<u32>,
    // Serial color-grade chain (Phase 6.6/6.7). Must match the Rust
    // GradeNode layout (6 vec4 = 96 bytes) byte-for-byte.
    grades: array<GradeNode, 8>,
};

// One color-grade node. Layout mirrors `GradeNode` in renderer.rs.
struct GradeNode {
    lift: vec4<f32>,        // (r, g, b, master)
    gamma: vec4<f32>,       // (r, g, b, master)
    gain: vec4<f32>,        // (r, g, b, master)
    log_offset: vec4<f32>,  // (offset_r, offset_g, offset_b, _)
    log_smh: vec4<f32>,     // reserved (luma-window widths)
    flags: vec4<f32>,       // (enabled, mode 0=primary/1=log, _, _)
};
@group(0) @binding(4) var<uniform> params: Params;

// Primary grade: per-channel lift/gamma/gain with a master fold.
// out = pow(clamp(in*gain + lift, 0, 1), 1/gamma), channelwise.
fn apply_primary(rgb: vec3<f32>, n: GradeNode) -> vec3<f32> {
    let gain = n.gain.xyz * n.gain.w;
    let lift = n.lift.xyz + vec3<f32>(n.lift.w);
    // Guard gamma away from 0; master multiplies the per-channel gamma.
    let gamma = max(n.gamma.xyz * n.gamma.w, vec3<f32>(1e-3));
    let lifted = clamp(rgb * gain + lift, vec3<f32>(0.0), vec3<f32>(1.0));
    return pow(lifted, vec3<f32>(1.0) / gamma);
}

// Log grade: an overall offset plus luma-windowed shadow / midtone /
// highlight balances. Shadow rides `lift.xyz`, midtone `gamma.xyz`,
// highlight `gain.xyz` (see GradeNodeParams::to_gpu in renderer.rs).
fn apply_log(rgb: vec3<f32>, n: GradeNode) -> vec3<f32> {
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    // Smooth luma-band weights summing ~1 across the range.
    let w_sh = 1.0 - smoothstep(0.0, 0.5, luma);
    let w_hi = smoothstep(0.5, 1.0, luma);
    let w_mid = 1.0 - w_sh - w_hi;
    var out = rgb + n.log_offset.xyz;
    out = out + n.lift.xyz * w_sh + n.gamma.xyz * w_mid + n.gain.xyz * w_hi;
    return clamp(out, vec3<f32>(0.0), vec3<f32>(1.0));
}

// Apply the serial grade chain in linear working space.
fn apply_grades(rgb_in: vec3<f32>) -> vec3<f32> {
    var rgb = rgb_in;
    let count = params.grade_count.x;
    for (var i = 0u; i < count; i = i + 1u) {
        let n = params.grades[i];
        if (n.flags.x < 0.5) { continue; }      // bypassed node
        if (n.flags.y < 0.5) {
            rgb = apply_primary(rgb, n);
        } else {
            rgb = apply_log(rgb, n);
        }
    }
    return rgb;
}

fn yuv_to_rgb(y_raw: f32, u_raw: f32, v_raw: f32, range: u32) -> vec3<f32> {
    // Range expansion. Limited (0): Y' ∈ [16, 235], chroma ∈ [16, 240].
    // Full (1): pass through.
    var y: f32;
    var u: f32;
    var v: f32;
    if (range == 0u) {
        y = (y_raw - 16.0 / 255.0) * (255.0 / 219.0);
        u = (u_raw - 128.0 / 255.0) * (255.0 / 224.0);
        v = (v_raw - 128.0 / 255.0) * (255.0 / 224.0);
    } else {
        y = y_raw;
        u = u_raw - 128.0 / 255.0;
        v = v_raw - 128.0 / 255.0;
    }
    // Rec.709 Y'CbCr → R'G'B' (gamma-encoded; transfer applied
    // separately by `linearize`). v1 assumes all YUV inputs use the
    // 709 matrix; a 2020-coefficient matrix can be picked up via the
    // input primaries flag in a later refinement.
    let r = y + 1.5748 * v;
    let g = y - 0.1873 * u - 0.4681 * v;
    let b = y + 1.8556 * u;
    return clamp(vec3<f32>(r, g, b), vec3<f32>(0.0), vec3<f32>(1.0));
}

// Transfer-function IDs — keep in sync with vance_color::pipeline.
// 0 = Linear, 1 = sRGB, 2 = Rec.709, 3 = Rec.2020,
// 4 = PQ, 5 = HLG, 6 = ACEScct. v1 ships 0/1/2/3; 4/5/6 fall through
// as passthrough (Phase 6.1b's OCIO path handles them).
fn linearize(rgb: vec3<f32>, id: u32) -> vec3<f32> {
    if (id == 1u) {
        // sRGB inverse EOTF. Piecewise: linear toe below 0.04045,
        // pow((v + 0.055) / 1.055, 2.4) above.
        let toe = rgb / vec3<f32>(12.92);
        let lift = pow(max(rgb + vec3<f32>(0.055), vec3<f32>(0.0)) / vec3<f32>(1.055), vec3<f32>(2.4));
        return select(toe, lift, rgb > vec3<f32>(0.04045));
    } else if (id == 2u || id == 3u) {
        // Rec.709 / Rec.2020 inverse OETF. Linear below 0.081 (709
        // breakpoint; 2020 is 0.0812 for 12-bit which we round here).
        let toe = rgb / vec3<f32>(4.5);
        let lift = pow(max(rgb + vec3<f32>(0.099), vec3<f32>(0.0)) / vec3<f32>(1.099), vec3<f32>(1.0 / 0.45));
        return select(toe, lift, rgb > vec3<f32>(0.081));
    }
    return rgb;
}

fn apply_mat3(rgb: vec3<f32>, m: array<vec4<f32>, 3>) -> vec3<f32> {
    let m0 = m[0].xyz;
    let m1 = m[1].xyz;
    let m2 = m[2].xyz;
    return vec3<f32>(
        dot(m0, rgb),
        dot(m1, rgb),
        dot(m2, rgb),
    );
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

    let input_range = params.pipeline_flags.z;
    let rgb_a_enc = yuv_to_rgb(
        textureSample(tex_y_a, samp, source_uv).r,
        textureSample(tex_u_a, samp, source_uv).r,
        textureSample(tex_v_a, samp, source_uv).r,
        input_range,
    );

    // Cross-dissolve blend with the B-side. When mix_factor == 0
    // (the common case) the B samples vanish; the GPU still touches
    // those textures but the cost is negligible. Sampling B with
    // the same source_uv assumes both clips share the resolved
    // frame UV — the transform/crop apply uniformly to the blended
    // image. (If we ever want per-side transforms, that's a future
    // refactor.)
    let rgb_b_enc = yuv_to_rgb(
        textureSample(tex_y_b, samp, source_uv).r,
        textureSample(tex_u_b, samp, source_uv).r,
        textureSample(tex_v_b, samp, source_uv).r,
        input_range,
    );

    // 1. Mix in gamma-encoded space so the cross-dissolve curve
    //    behaves the same way the renderer did pre-6.3. We could
    //    blend in linear, but for SDR sources the perceptual change
    //    is minor and the gamma-space mix is what colorists expect.
    var rgb_enc = mix(rgb_a_enc, rgb_b_enc, params.mix_factor);
    // 1b. Apply the loaded 3D LUT, if enabled. LUTs are sampled in
    //     source-space gamma RGB — matches the convention colorists
    //     use when authoring Rec.709 / sRGB film-emulation LUTs.
    //     `lut_params.x` gates contribution; trilinear filtering on
    //     the texture lookup gives smooth in-between values.
    if (params.lut_params.x > 0.5) {
        // Inset the lookup by 0.5/N on each axis so the sample
        // coordinates land at texel centers (texture3D coords are
        // 0..1, and texels live at (i+0.5)/N).
        let size_minus_1 = max(1.0, f32(textureDimensions(tex_lut).x) - 1.0);
        let half_texel = 0.5 / f32(textureDimensions(tex_lut).x);
        let uvw = clamp(rgb_enc, vec3<f32>(0.0), vec3<f32>(1.0))
            * (size_minus_1 / f32(textureDimensions(tex_lut).x))
            + vec3<f32>(half_texel);
        let lut_rgb = textureSample(tex_lut, samp, uvw).rgb;
        rgb_enc = lut_rgb;
    }
    // 2. Linearize via input transfer, then change primaries to the
    //    working space. Effects from here on operate on linear-RGB.
    let input_transfer = params.pipeline_flags.x;
    let rgb_lin_in = linearize(rgb_enc, input_transfer);
    var rgb_lin_working = apply_mat3(rgb_lin_in, params.pipeline_in_to_working);
    // 2b. Serial color-grade chain (Phase 6.6/6.7) — lift/gamma/gain
    //     and log wheels, in linear working space. No-op when
    //     grade_count == 0, so pre-6.6 frames render bit-identically.
    //     Pipeline contract: grade chain -> brightness/contrast ->
    //     keys -> working->output -> view-LUT.
    rgb_lin_working = apply_grades(rgb_lin_working);
    // 3. Contrast pivots around 0.5 *linear* — in linear-light that
    //    midpoint is darker than gamma-0.5, so sliders feel more
    //    sensitive in the shadows. Documented behavior change in
    //    6.3's commit; matches how Resolve operates in ACEScct/cg.
    var rgb = (rgb_lin_working - vec3<f32>(0.5)) * params.contrast
        + vec3<f32>(0.5) + vec3<f32>(params.brightness);
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
    // 4. Working → output primaries. For the v1 sRGB surface this is
    //    identity when working == LinearRec709 (Rec.709 primaries
    //    match sRGB's), so the matrix multiply is effectively a
    //    no-op in the common case but lights up automatically when
    //    the user picks a different working space (e.g., ACEScg).
    var rgb_out = apply_mat3(rgb, params.pipeline_working_to_out);
    // 5. The surface format (Rgba8UnormSrgb) handles the linear →
    //    sRGB encoding on write, so we leave the output linear here.
    //    `pipeline_flags.y` (output_transfer) is recorded for
    //    future use when we land non-sRGB surface targets (HDR).
    rgb_out = clamp(rgb_out, vec3<f32>(0.0), vec3<f32>(1.0));
    // 6. Optional view LUT — a monitoring transform applied *after*
    //    the project's color pipeline. Export bypasses this slot
    //    so the on-disk render isn't tinted by the display look.
    if (params.view_lut_params.x > 0.5) {
        let size_minus_1 = max(1.0, f32(textureDimensions(tex_view_lut).x) - 1.0);
        let half_texel = 0.5 / f32(textureDimensions(tex_view_lut).x);
        let uvw = clamp(rgb_out, vec3<f32>(0.0), vec3<f32>(1.0))
            * (size_minus_1 / f32(textureDimensions(tex_view_lut).x))
            + vec3<f32>(half_texel);
        rgb_out = textureSample(tex_view_lut, samp, uvw).rgb;
    }
    return vec4<f32>(rgb_out * params.opacity * keep, 1.0);
}
