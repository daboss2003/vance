// Phase 1 video viewport shader (the legacy single-clip path).
//
// Inputs: the frame's plane textures — Y, U, V (R8, or R16Unorm for
// 10-bit); Y + an interleaved UV (Rg8) for NV12; or one straight-alpha RGBA8
// texture in the Y slot (`Params.source`). The colour pipeline (range, YUV
// matrix, transfers, primaries) comes from `Params`.

// Shared code: common.wgsl (transfer functions, YUV, fullscreen vertex
// shader) and grade.wgsl (grade nodes) are concatenated in front of this
// file at pipeline build time — see `renderer::wgsl`.

@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex_y: texture_2d<f32>;
@group(0) @binding(2) var tex_u: texture_2d<f32>;
@group(0) @binding(3) var tex_v: texture_2d<f32>;
@group(0) @binding(8) var tex_lut: texture_3d<f32>;
@group(0) @binding(9) var tex_view_lut: texture_3d<f32>;
// Curve LUT atlas (Phase 6.8): 256 wide × 8 rows (one per grade node).
// rgb lanes = baked R/G/B tone curves (master folded in), alpha = luma.
@group(0) @binding(10) var tex_curve_atlas: texture_2d<f32>;

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
    // (sample_scale, pixel_aspect, _, _) — the YUV sample scale (1.0 for
    // 8-bit, 65535/1023 for 10-bit R16Unorm sources) and the frame's pixel
    // aspect. A 16-byte-aligned vec4 at offset 48, matching Rust's
    // `sample_scale + pixel_aspect + _pad0`.
    misc: vec4<f32>,
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
    // (YUV matrix, source kind A, source kind B, 10-bit R16Unorm planes:
    // bit 0 = A, bit 1 = B). Kinds are SRC_* (common.wgsl).
    source: vec4<u32>,
    // Number of active grade nodes; only .x is read.
    grade_count: vec4<u32>,
    // Serial color-grade chain (Phase 6.6/6.7/6.9/6.10). Must match the
    // Rust GradeNode layout (11 vec4 = 176 bytes) byte-for-byte.
    grades: array<GradeNode, 8>,
};

@group(0) @binding(4) var<uniform> params: Params;

// Set by apply_grades when a node's qualifier "show mask" is on; fs_main
// returns the raw grayscale matte BEFORE the downstream contrast / keys /
// output matrix / view-LUT (which would otherwise tint it). Per-invocation.
var<private> g_matte_hit: bool = false;

// Apply the serial grade chain in linear working space. Intermediate
// values are NOT clamped to [0,1] (only floored at 0) so nodes compose
// like a real grade graph; the downstream brightness/contrast block
// (and the output stage) bound the final SDR result.
//
// Each node's correction is masked by `qualifier_mask * window_mask`,
// computed on the node's INPUT pixel + the source UV (standard secondary
// behavior). Both masks are 1.0 when their feature is off, so a node with
// neither is an exact passthrough of pre-6.9/6.10 behavior.
fn apply_grades(rgb_in: vec3<f32>, uv: vec2<f32>) -> vec3<f32> {
    var rgb = rgb_in;
    let count = params.grade_count.x;
    // Display aspect for the power windows (storage width × pixel aspect /
    // height) — tex_y is always bound when the shader runs (render errors
    // NoFrame otherwise). Hoisted out of the loop.
    let dims = textureDimensions(tex_y);
    let aspect = f32(dims.x) / max(f32(dims.y), 1.0) * params.misc.y;
    for (var i = 0u; i < count; i = i + 1u) {
        let n = params.grades[i];
        if (n.flags.x < 0.5) { continue; }      // bypassed node
        let src = rgb;
        // Graded colour (xyz) + combined qualifier ∩ window mask (w).
        let gm = grade_node_eval(src, uv, i, n, aspect);
        rgb = mix(src, gm.xyz, gm.w);
        // Matte solo: show this node's COMBINED mask as grayscale and
        // stop. Gated on show_mask alone so a window-only node can solo.
        if (n.qual_c.y > 0.5) {
            g_matte_hit = true;
            rgb = vec3<f32>(gm.w);
            break;
        }
    }
    return rgb;
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
    let matrix = params.source.x;
    // Sample scale: 1.0 for 8-bit (R8Unorm, code/255) or 65535/1023 for
    // 10-bit (R16Unorm, code/65535 → code/1023).
    let src = sample_planes(
        tex_y, tex_u, tex_v, samp, source_uv, params.source.y, params.misc.x, input_range,
        matrix, params.source.z != 0u,
    );
    var rgb_enc = src.rgb;
    // Straight alpha of an RGBA source (1 for YUV).
    let src_alpha = src.a;
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
    rgb_lin_working = apply_grades(rgb_lin_working, source_uv);
    // Qualifier matte solo (6.9): a grade node requested "show mask", so
    // return the raw grayscale matte BEFORE contrast / keys / output
    // matrix / view-LUT (any of which would tint a mid-gray matte).
    if (g_matte_hit) {
        return vec4<f32>(rgb_lin_working, 1.0);
    }
    // 3. Contrast pivots around 0.5 *linear* — in linear-light that
    //    midpoint is darker than gamma-0.5, so sliders feel more
    //    sensitive in the shadows. Documented behavior change in
    //    6.3's commit; matches how Resolve operates in ACEScct/cg.
    var rgb = (rgb_lin_working - vec3<f32>(0.5)) * params.contrast
        + vec3<f32>(0.5) + vec3<f32>(params.brightness);
    // Clamp to [0,1] for SDR. For a PQ HDR source, working linear carries
    // highlights above reference white (>1.0), so keep only the 0 floor —
    // otherwise the [0,1] clamp would crush every highlight to white here.
    if (input_transfer == 4u) {
        rgb = max(rgb, vec3<f32>(0.0));
    } else {
        rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    }

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
    // 5. Output transfer. `pipeline_flags.y` is the output-transfer id
    //    (0 Linear, 1 sRGB, 2 Rec709, 3 Rec2020, 4 PQ, 5 HLG).
    //    - SDR targets (Rgba8UnormSrgb): the texture store applies the
    //      implicit linear → sRGB encode, so we leave the output linear
    //      (clamped to [0,1]) — bit-identical to prior behavior.
    //    - HDR targets (Rgba16Float, no implicit transfer): we apply the
    //      PQ / HLG OETF here so the stored code values are final. Working
    //      space is already the output primaries (Rec.2020) from step 4.
    let out_transfer = params.pipeline_flags.y;
    if (out_transfer == 4u) {
        // PQ: working linear where 1.0 = 100 cd/m² reference white; allow
        // highlights up to 100.0 (= 10000 nits) so an HDR source's highlights
        // survive to the PQ encode instead of clipping at reference white.
        let lin = clamp(rgb_out, vec3<f32>(0.0), vec3<f32>(100.0));
        rgb_out = pq_oetf(lin * (100.0 / 10000.0));
    } else if (out_transfer == 5u) {
        let lin = clamp(rgb_out, vec3<f32>(0.0), vec3<f32>(1.0));
        rgb_out = hlg_oetf(lin);
    } else if (input_transfer == 4u) {
        // HDR (PQ) source → SDR output (preview / SDR export): tone-map the
        // HDR working range into [0,1] so highlights roll off instead of
        // clipping. The SDR texture store then applies the SDR transfer.
        // (SDR sources keep the plain clamp below — bit-identical.)
        rgb_out = aces_tonemap(rgb_out);
    } else {
        rgb_out = clamp(rgb_out, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    // 5b. An RGBA source shows premultiplied over black, as the compositor
    //     blends it: display-encoded for SDR (the store re-encodes), linear
    //     before the PQ / HLG encode. Skipped at alpha 1 so YUV stays exact.
    if (src_alpha < 1.0) {
        if (out_transfer == 4u || out_transfer == 5u) {
            rgb_out = select(
                hlg_oetf(hlg_eotf(rgb_out) * src_alpha),
                pq_oetf(pq_eotf(rgb_out) * src_alpha),
                out_transfer == 4u,
            );
        } else {
            rgb_out = srgb_eotf(srgb_oetf(rgb_out) * src_alpha);
        }
    }
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
