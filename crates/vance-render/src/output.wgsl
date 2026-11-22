// Compositor output pass + RGBA8 pack/unpack (ADR 0007), concatenated after
// common.wgsl. The accumulator is premultiplied in the frame's blend space;
// the output is that composite over black with alpha 1, or — for a
// transparent output — its straight (un-premultiplied) colour and alpha.

@group(0) @binding(0) var samp: sampler;
@group(0) @binding(1) var tex_src: texture_2d<f32>;
@group(0) @binding(3) var tex_view_lut: texture_3d<f32>;

// Layout must match `OutputUniform` in compositor.rs.
struct OutputU {
    // (hdr, output transfer, view LUT on, diagnostic bypass)
    flags: vec4<u32>,
    // (target applies the sRGB curve on store, transparent output, alpha
    // bits of the readback (8 / 16), _)
    target_flags: vec4<u32>,
    // Output pass: working → output primaries. Pack: linear working →
    // Rec.709; unpack: the inverse (HDR only).
    working_to_out: array<vec4<f32>, 3>,
};
@group(0) @binding(2) var<uniform> ou: OutputU;

fn view_lut(rgb: vec3<f32>) -> vec3<f32> {
    let n = f32(textureDimensions(tex_view_lut).x);
    let uvw = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)) * (max(1.0, n - 1.0) / n)
        + vec3<f32>(0.5 / n);
    return textureSampleLevel(tex_view_lut, samp, uvw, 0.0).rgb;
}

// Straight alpha below this reads back as 0 at the target's bit depth, so
// its colour is zeroed too (un-premultiplying it would show garbage).
fn zero_alpha() -> f32 {
    return 0.5 / f32((1u << ou.target_flags.z) - 1u);
}

@fragment
fn fs_output(in: VsOut) -> @location(0) vec4<f32> {
    // Premultiplied colour IS the composite over black.
    let acc = textureSampleLevel(tex_src, samp, in.uv, 0.0);
    var col = acc.rgb;
    var alpha = 1.0;
    if (ou.target_flags.y != 0u) {
        // Transparent output: straight colour (the encoder's alpha codecs
        // take straight alpha); an alpha that rounds to 0 reads (0, 0, 0, 0).
        if (acc.a < zero_alpha()) {
            return vec4<f32>(0.0);
        }
        alpha = min(acc.a, 1.0);
        col = acc.rgb / acc.a;
    }
    let srgb_target = ou.target_flags.x != 0u;
    let bypass = ou.flags.w != 0u;
    if (ou.flags.x == 0u) {
        // SDR: already display-encoded.
        let enc = clamp(col, vec3<f32>(0.0), vec3<f32>(1.0));
        if (ou.flags.z != 0u && !bypass) {
            // Intentional: the view LUT samples LINEAR output, as the legacy
            // path did (it ran before its sRGB target's store encode), so
            // existing monitoring LUTs keep their look.
            let lin = view_lut(srgb_eotf(enc));
            return vec4<f32>(select(srgb_oetf(lin), lin, srgb_target), alpha);
        }
        return vec4<f32>(select(enc, srgb_eotf(enc), srgb_target), alpha);
    }
    // HDR: linear working → output primaries + transfer encode.
    if (bypass) {
        return vec4<f32>(col, alpha);
    }
    var rgb = apply_mat3(col, ou.working_to_out);
    let t = ou.flags.y;
    if (t == 4u) {
        // PQ: working 1.0 = 100 cd/m²; highlights up to 100.0 = 10000 nits.
        let lin = clamp(rgb, vec3<f32>(0.0), vec3<f32>(100.0));
        rgb = pq_oetf(lin * (100.0 / 10000.0));
    } else if (t == 5u) {
        rgb = hlg_oetf(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)));
    } else {
        rgb = clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    }
    if (ou.flags.z != 0u) {
        rgb = view_lut(rgb);
    }
    return vec4<f32>(rgb, alpha);
}

// Layer slot → straight-alpha sRGB RGBA8 (CPU add-ons, WASM transitions).
// HDR slots are linear working: converted to Rec.709 primaries, clipped to
// [0,1] and sRGB-encoded. SDR slots are already sRGB-encoded output
// (Rec.709 primaries).
@fragment
fn fs_pack(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureLoad(tex_src, vec2<i32>(in.clip_position.xy), 0);
    if (c.a < zero_alpha()) {
        return vec4<f32>(0.0);
    }
    var rgb = c.rgb / c.a;
    if (ou.flags.x != 0u) {
        rgb = srgb_oetf(apply_mat3(rgb, ou.working_to_out));
    }
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), clamp(c.a, 0.0, 1.0));
}

// Straight-alpha sRGB RGBA8 → layer slot; the inverse of `fs_pack` (HDR:
// `working_to_out` holds Rec.709 → working).
@fragment
fn fs_unpack(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureLoad(tex_src, vec2<i32>(in.clip_position.xy), 0);
    var rgb = c.rgb;
    if (ou.flags.x != 0u) {
        rgb = apply_mat3(srgb_eotf(rgb), ou.working_to_out);
    }
    return vec4<f32>(rgb * c.a, c.a);
}
