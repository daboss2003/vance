// Compositor layer pass (ADR 0007), concatenated after common.wgsl,
// grade.wgsl and blend.wgsl. Computes one clip's PREMULTIPLIED colour in the
// frame's blend space:
//   SDR output → display-encoded (sRGB curve, output primaries), the space
//                Premiere/Resolve/Photoshop blend in (user decision 2);
//   HDR output → linear working space, so values above 1.0 survive.
// Opacity is NOT part of the layer colour; the composite step applies it.
// `fs_layer` writes the colour into the clip's slot; `fs_layer_over`
// composites it straight over the backdrop accumulator (the common case,
// saving a full-frame slot write + read).
//
// The keyer's matte is computed at SOURCE resolution — fs_key here, then
// the choke / soften line passes in matte.wgsl — so choke and soften are
// measured in the source's display pixels, identical for preview and export
// at any canvas size.

@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex_y: texture_2d<f32>;
@group(0) @binding(2) var tex_u: texture_2d<f32>;
@group(0) @binding(3) var tex_v: texture_2d<f32>;
@group(0) @binding(5) var tex_rgba: texture_2d<f32>;
@group(0) @binding(6) var tex_lut: texture_3d<f32>;
@group(0) @binding(7) var tex_curve_atlas: texture_2d<f32>;
@group(0) @binding(8) var tex_matte: texture_2d<f32>;
// fs_layer_over only: the backdrop accumulator and its composite op.
@group(0) @binding(9) var tex_backdrop: texture_2d<f32>;
@group(0) @binding(10) var<uniform> cu: CompositeU;

// Layout must match `LayerUniform` in compositor.rs.
struct LayerU {
    // (translate.x, translate.y, _, canvas aspect w/h)
    xform: vec4<f32>,
    // (scale.x, scale.y, fit.x, fit.y) — fit = the source's fitted size in
    // canvas-height units (decision 1: fit without distortion).
    scale_fit: vec4<f32>,
    crop_lrtb: vec4<f32>,
    // (brightness, contrast, YUV sample scale, source DISPLAY aspect w/h:
    // storage width × pixel aspect / height, for the power windows)
    adjust: vec4<f32>,
    // Legacy keys: (r, g, b, enable), (tolerance, softness, _, _),
    // (threshold, direction, softness, enable).
    chroma_key: vec4<f32>,
    chroma_params: vec4<f32>,
    luma_key: vec4<f32>,
    // (input transfer, input range, source kind (SRC_*), hdr)
    flags: vec4<u32>,
    // (clip LUT, keyer, matte view, _)
    toggles: vec4<u32>,
    in_to_working: array<vec4<f32>, 3>,
    working_to_out: array<vec4<f32>, 3>,
    // Keyer: primary channel one-hot (xyz) + the key colour's difference
    // (w; 0 = inert), the other two channels one-hot with balance (s1.w) and
    // strength (s2.w), (clip_black, clip_white, invert, _),
    // (spill, spill_bias, choke_px, soften_px).
    key_p: vec4<f32>,
    key_s1: vec4<f32>,
    key_s2: vec4<f32>,
    key_clip: vec4<f32>,
    key_refine: vec4<f32>,
    // (1/width, 1/height) of the source (luma plane / RGBA), then
    // (cos, sin) of −rotation.
    src_texel: vec4<f32>,
    // (YUV matrix, 10-bit R16Unorm planes, _, _)
    yuv: vec4<u32>,
    grade_count: vec4<u32>,
    grades: array<GradeNode, 8>,
};
@group(0) @binding(4) var<uniform> lp: LayerU;

// Canvas UV → source UV. Forward order (as the legacy shader): scale →
// rotate → translate, around the canvas centre; computed in canvas-height
// units so rotation is rigid on non-square canvases, then divided by the
// fitted source size.
fn layer_source_uv(uv: vec2<f32>) -> vec2<f32> {
    let ac = lp.xform.w;
    let p = vec2<f32>((uv.x - 0.5) * ac, uv.y - 0.5) - vec2<f32>(lp.xform.x * ac, lp.xform.y);
    let c = lp.src_texel.z;
    let s = lp.src_texel.w;
    let r = vec2<f32>(p.x * c - p.y * s, p.x * s + p.y * c);
    return r / lp.scale_fit.xy / lp.scale_fit.zw + vec2<f32>(0.5);
}

// Coverage of the (cropped) source frame at this pixel: 1 inside, 0
// outside, anti-aliased over one screen pixel at the edges.
fn frame_coverage(suv: vec2<f32>, fw: vec2<f32>) -> f32 {
    let c = lp.crop_lrtb;
    let dx = min(suv.x - c.x, (1.0 - c.y) - suv.x);
    let dy = min(suv.y - c.z, (1.0 - c.w) - suv.y);
    let k = clamp(dx / fw.x + 0.5, 0.0, 1.0) * clamp(dy / fw.y + 0.5, 0.0, 1.0);
    // Snap float residue at the extremes so a full-frame layer's border
    // pixels are exactly opaque and off-frame pixels exactly clear.
    return select(select(k, 1.0, k > 0.999), 0.0, k < 0.001);
}

// Encoded (straight) source colour + source alpha at source UV `suv`
// (same sampling + range expansion as the legacy shader).
fn source_sample(suv: vec2<f32>) -> vec4<f32> {
    if (lp.flags.z == SRC_RGBA) {
        return rgba_bilinear(tex_rgba, suv);
    }
    return sample_planes(
        tex_y, tex_u, tex_v, samp, suv, lp.flags.z, lp.adjust.z, lp.flags.y,
        lp.yuv.x, lp.yuv.y != 0u,
    );
}

// ---- Keyer ------------------------------------------------------------------
//
// Primary-channel colour difference on the ENCODED source: the key colour's
// dominant channel P against a balance-weighted mix of the other two,
// normalised by the key colour's own difference, so the key colour itself
// maps to 0 and anything without a P excess to 1. Strength is the matte
// gain; clip black / white remap the matte's range.
fn key_matte_raw(c: vec3<f32>) -> f32 {
    let kd = lp.key_p.w;
    if (kd <= 0.0) {
        return 1.0;
    }
    let bal = lp.key_s1.w;
    let diff = dot(c, lp.key_p.xyz) - (dot(c, lp.key_s1.xyz) * bal + dot(c, lp.key_s2.xyz) * (1.0 - bal));
    let m = 1.0 - lp.key_s2.w * diff / kd;
    let lo = lp.key_clip.x;
    let hi = max(lp.key_clip.y, lo + 1e-4);
    return clamp((m - lo) / (hi - lo), 0.0, 1.0);
}

// Spill suppression: pull the primary channel down to a limit between the
// average (spill_bias 0) and the max (1) of the other two channels.
fn despill(c: vec3<f32>) -> vec3<f32> {
    let c1 = dot(c, lp.key_s1.xyz);
    let c2 = dot(c, lp.key_s2.xyz);
    let limit = mix(0.5 * (c1 + c2), max(c1, c2), lp.key_refine.y);
    let excess = max(dot(c, lp.key_p.xyz) - limit, 0.0);
    return c - lp.key_p.xyz * (excess * lp.key_refine.x);
}

// Raw matte at source resolution (drawn into a source-sized R16Float).
@fragment
fn fs_key(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(key_matte_raw(source_sample(in.uv).rgb), 0.0, 0.0, 1.0);
}

// ---- Layer -------------------------------------------------------------------

// Serial grade chain (same node code as the legacy shader). Returns the
// graded colour, or w = 1 with the node's mask when a node solos its matte.
fn layer_grades(rgb_in: vec3<f32>, uv: vec2<f32>) -> vec4<f32> {
    var rgb = rgb_in;
    let aspect = lp.adjust.w;
    let count = lp.grade_count.x;
    for (var i = 0u; i < count; i = i + 1u) {
        let n = lp.grades[i];
        if (n.flags.x < 0.5) { continue; }
        let src = rgb;
        let gm = grade_node_eval(src, uv, i, n, aspect);
        rgb = mix(src, gm.xyz, gm.w);
        if (n.qual_c.y > 0.5) {
            return vec4<f32>(vec3<f32>(gm.w), 1.0);
        }
    }
    return vec4<f32>(rgb, 0.0);
}

// Working-space colour → the frame's blend space, premultiplied by `a`.
fn to_blend_space(rgb: vec3<f32>, a: f32, input_transfer: u32) -> vec4<f32> {
    // Intentional: the split IS decision 2. SDR output blends display-
    // encoded (output primaries + sRGB curve, the curve the legacy sRGB
    // target applied on store); HDR output blends in linear working space
    // so values above 1.0 survive (the output pass encodes PQ/HLG).
    if (lp.flags.w != 0u) {
        return vec4<f32>(rgb * a, a);
    }
    var o = apply_mat3(rgb, lp.working_to_out);
    if (input_transfer == 4u) {
        // HDR (PQ) source on SDR output: tone-map, as the legacy path.
        o = aces_tonemap(o);
    } else {
        o = clamp(o, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    return vec4<f32>(srgb_oetf(o) * a, a);
}

// A grayscale diagnostic (grade matte solo, keyer matte view): `v` is shown
// as-is — SDR as the display value, HDR raw (the output pass skips its
// encode for such frames).
fn diag_view(v: f32, a: f32) -> vec4<f32> {
    return vec4<f32>(vec3<f32>(v) * a, a);
}

// The layer's premultiplied colour at canvas UV `uv`. Called first thing
// from an entry point (it takes derivatives).
fn layer_color(uv: vec2<f32>) -> vec4<f32> {
    // Derivatives first, in uniform control flow.
    let suv = layer_source_uv(uv);
    let fw = max(fwidth(suv), vec2<f32>(1e-7));
    let cover = frame_coverage(suv, fw);

    let src = source_sample(suv);
    var rgb_enc = src.rgb;
    let base_a = cover * src.a;

    // Keyer on the encoded source, before the clip LUT.
    var matte = 1.0;
    if (lp.toggles.y != 0u) {
        matte = textureSampleLevel(tex_matte, samp, suv, 0.0).r;
        if (lp.key_clip.z > 0.5) {
            matte = 1.0 - matte;
        } else if (lp.key_p.w > 0.0) {
            // Decision 6: no spill suppression on an inverted key (it would
            // pull the kept screen colour down) or an inert one.
            rgb_enc = despill(rgb_enc);
        }
    }

    // Clip LUT in source-space gamma RGB (as the legacy shader).
    if (lp.toggles.x != 0u) {
        let size_minus_1 = max(1.0, f32(textureDimensions(tex_lut).x) - 1.0);
        let half_texel = 0.5 / f32(textureDimensions(tex_lut).x);
        let uvw = clamp(rgb_enc, vec3<f32>(0.0), vec3<f32>(1.0))
            * (size_minus_1 / f32(textureDimensions(tex_lut).x))
            + vec3<f32>(half_texel);
        rgb_enc = textureSampleLevel(tex_lut, samp, uvw, 0.0).rgb;
    }

    let input_transfer = lp.flags.x;
    let rgb_lin_in = linearize(rgb_enc, input_transfer);
    let graded = layer_grades(apply_mat3(rgb_lin_in, lp.in_to_working), suv);
    if (graded.w > 0.5) {
        // Grade-node matte solo: the raw mask, bypassing contrast / keys /
        // output transform, written as the legacy path did (linear into an
        // sRGB store) so it reads back the same.
        if (lp.flags.w != 0u) {
            return diag_view(graded.x, base_a);
        }
        return diag_view(srgb_oetf(vec3<f32>(graded.x)).x, base_a);
    }

    // Brightness/contrast around linear 0.5, then the legacy range clamp:
    // [0,1] for SDR sources, only the 0 floor for PQ (highlights > 1.0).
    var rgb = (graded.xyz - vec3<f32>(0.5)) * lp.adjust.y
        + vec3<f32>(0.5) + vec3<f32>(lp.adjust.x);
    if (input_transfer == 4u) {
        rgb = max(rgb, vec3<f32>(0.0));
    } else {
        rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    }

    // Legacy chroma / luma keys, now feeding alpha instead of fading to
    // black.
    if (lp.chroma_key.a > 0.5) {
        let dist = length(rgb - lp.chroma_key.rgb);
        let edge0 = lp.chroma_params.x;
        let edge1 = lp.chroma_params.x + max(lp.chroma_params.y, 0.0001);
        matte = matte * smoothstep(edge0, edge1, dist);
    }
    if (lp.luma_key.a > 0.5) {
        let lum = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
        let th = lp.luma_key.x;
        let soft = max(lp.luma_key.z, 0.0001);
        if (lp.luma_key.y > 0.0) {
            matte = matte * smoothstep(th - soft, th + soft, lum);
        } else {
            matte = matte * (1.0 - smoothstep(th - soft, th + soft, lum));
        }
    }

    if (lp.toggles.z != 0u) {
        return diag_view(matte, base_a);
    }
    return to_blend_space(rgb, base_a * matte, input_transfer);
}

// Intentional: both entry points round the layer colour with
// quantizeToF16 themselves. The slot then stores an exact f16 value, and the
// fused pass rounds identically, so fused and slot + blend agree bit for bit
// on any GPU (render-target f16 stores round differently across hardware).
@fragment
fn fs_layer(in: VsOut) -> @location(0) vec4<f32> {
    return quantizeToF16(layer_color(in.uv));
}

// Layer pass fused with its composite: identical to fs_layer → slot →
// fs_composite.
@fragment
fn fs_layer_over(in: VsOut) -> @location(0) vec4<f32> {
    let src = quantizeToF16(layer_color(in.uv));
    if (cu.modes.w != 0u) {
        return src * cu.vals.x;
    }
    let bd = textureLoad(tex_backdrop, vec2<i32>(in.clip_position.xy), 0);
    return blend_pm(bd, src * cu.vals.x, cu.modes.x);
}
