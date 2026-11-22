// Shared colour math (no resource bindings). Concatenated in front of
// shader.wgsl (legacy single-clip path), layer.wgsl, composite.wgsl and
// output.wgsl — see `renderer::wgsl`.

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

// Source kinds (`renderer::SRC_*`): three planes (Y, U, V), RGBA, or NV12
// (Y + an interleaved two-channel UV plane).
const SRC_PLANAR: u32 = 0u;
const SRC_RGBA: u32 = 1u;
const SRC_NV12: u32 = 2u;

// `y_raw`/`u_raw`/`v_raw` are code / (2^bits − 1): 8-bit R8 samples as they
// come, 10-bit R16Unorm samples after the sample scale. `range` 0 = limited,
// 1 = full; `matrix` = `vance_color::pipeline::MATRIX_*`.
fn yuv_to_rgb(y_raw: f32, u_raw: f32, v_raw: f32, range: u32, matrix: u32, ten_bit: bool) -> vec3<f32> {
    var y: f32;
    var u: f32;
    var v: f32;
    if (ten_bit) {
        // Limited: Y' ∈ [64, 940], chroma ∈ [64, 960] around 512. Full:
        // chroma (code − 512) / 1023.
        if (range == 0u) {
            y = (y_raw - 64.0 / 1023.0) * (1023.0 / 876.0);
            u = (u_raw - 512.0 / 1023.0) * (1023.0 / 896.0);
            v = (v_raw - 512.0 / 1023.0) * (1023.0 / 896.0);
        } else {
            y = y_raw;
            u = u_raw - 512.0 / 1023.0;
            v = v_raw - 512.0 / 1023.0;
        }
    } else if (range == 0u) {
        // Limited: Y' ∈ [16, 235], chroma ∈ [16, 240].
        y = (y_raw - 16.0 / 255.0) * (255.0 / 219.0);
        u = (u_raw - 128.0 / 255.0) * (255.0 / 224.0);
        v = (v_raw - 128.0 / 255.0) * (255.0 / 224.0);
    } else {
        y = y_raw;
        u = u_raw - 128.0 / 255.0;
        v = v_raw - 128.0 / 255.0;
    }
    // Y'CbCr → R'G'B' (gamma-encoded; `linearize` applies the transfer):
    // R = Y + 2(1−Kr)·Cr, B = Y + 2(1−Kb)·Cb, G from the luma equation.
    var rgb: vec3<f32>;
    if (matrix == 1u) {
        // BT.601: Kr 0.299, Kb 0.114.
        rgb = vec3<f32>(y + 1.402 * v, y - 0.344136 * u - 0.714136 * v, y + 1.772 * u);
    } else if (matrix == 2u) {
        // BT.2020 non-constant luminance: Kr 0.2627, Kb 0.0593.
        rgb = vec3<f32>(y + 1.4746 * v, y - 0.164553 * u - 0.571353 * v, y + 1.8814 * u);
    } else {
        // BT.709: Kr 0.2126, Kb 0.0722.
        rgb = vec3<f32>(y + 1.5748 * v, y - 0.1873 * u - 0.4681 * v, y + 1.8556 * u);
    }
    return clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0));
}

// Straight-alpha RGBA texel of `t` with premultiplied bilinear filtering
// (filtering straight colour would darken edges toward transparent texels'
// RGB). Returns straight colour and alpha.
fn rgba_bilinear(t: texture_2d<f32>, suv: vec2<f32>) -> vec4<f32> {
    let dims = vec2<i32>(textureDimensions(t));
    let p = suv * vec2<f32>(dims) - vec2<f32>(0.5);
    let f = p - floor(p);
    let i0 = vec2<i32>(floor(p));
    let lo = clamp(i0, vec2<i32>(0), dims - vec2<i32>(1));
    let hi = clamp(i0 + vec2<i32>(1), vec2<i32>(0), dims - vec2<i32>(1));
    let t00 = textureLoad(t, vec2<i32>(lo.x, lo.y), 0);
    let t10 = textureLoad(t, vec2<i32>(hi.x, lo.y), 0);
    let t01 = textureLoad(t, vec2<i32>(lo.x, hi.y), 0);
    let t11 = textureLoad(t, vec2<i32>(hi.x, hi.y), 0);
    let p00 = vec4<f32>(t00.rgb * t00.a, t00.a);
    let p10 = vec4<f32>(t10.rgb * t10.a, t10.a);
    let p01 = vec4<f32>(t01.rgb * t01.a, t01.a);
    let p11 = vec4<f32>(t11.rgb * t11.a, t11.a);
    let m = mix(mix(p00, p10, f.x), mix(p01, p11, f.x), f.y);
    if (m.a <= 0.0) {
        return vec4<f32>(0.0);
    }
    return vec4<f32>(m.rgb / m.a, m.a);
}

// A decoded frame's encoded colour + alpha at `uv` from its plane textures:
// three planes, NV12 (`tu` holds UV in .rg), or RGBA in `ty`. `sc` is the
// sample scale (1 for 8-bit, 65535/1023 for 10-bit R16Unorm planes).
fn sample_planes(
    ty: texture_2d<f32>,
    tu: texture_2d<f32>,
    tv: texture_2d<f32>,
    s: sampler,
    uv: vec2<f32>,
    kind: u32,
    sc: f32,
    range: u32,
    matrix: u32,
    ten_bit: bool,
) -> vec4<f32> {
    if (kind == SRC_RGBA) {
        return rgba_bilinear(ty, uv);
    }
    let y = textureSampleLevel(ty, s, uv, 0.0).r * sc;
    // Chroma sits at the centre of its 2×2 luma block (chroma coordinate =
    // luma coordinate / 2). An odd-size frame's chroma plane is ceil(n / 2)
    // wide, so the luma UV is rescaled onto it (exactly ×1 when even).
    let cuv = uv * (vec2<f32>(textureDimensions(ty)) / (2.0 * vec2<f32>(textureDimensions(tu))));
    var c: vec2<f32>;
    if (kind == SRC_NV12) {
        c = textureSampleLevel(tu, s, cuv, 0.0).rg * sc;
    } else {
        c = vec2<f32>(
            textureSampleLevel(tu, s, cuv, 0.0).r * sc,
            textureSampleLevel(tv, s, cuv, 0.0).r * sc,
        );
    }
    return vec4<f32>(yuv_to_rgb(y, c.x, c.y, range, matrix, ten_bit), 1.0);
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
    } else if (id == 4u) {
        // PQ (ST 2084) EOTF → normalized L/10000, scaled ×100 so working 1.0
        // = 100 cd/m² (reference white). This is the exact inverse of the
        // output encode (pq_oetf(lin*100/10000)), so HDR round-trips; HDR
        // highlights land above 1.0 (up to 100.0 = 10000 nits).
        return pq_eotf(rgb) * 100.0;
    } else if (id == 5u) {
        // HLG EOTF → scene-linear [0,1] (relative; working 1.0 = peak white).
        return hlg_eotf(rgb);
    }
    return rgb;
}

// SMPTE ST 2084 (PQ) forward OETF. Input is display luminance NORMALIZED to
// the 10000 cd/m² peak (E = L/10000, in [0,1]); output is the PQ code value
// in [0,1]. Used only on the HDR (Rgba16Float) export target, where the
// store applies no implicit transfer.
fn pq_oetf(e: vec3<f32>) -> vec3<f32> {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let en = clamp(e, vec3<f32>(0.0), vec3<f32>(1.0));
    let ym1 = pow(en, vec3<f32>(m1));
    let num = vec3<f32>(c1) + c2 * ym1;
    let den = vec3<f32>(1.0) + c3 * ym1;
    return pow(num / den, vec3<f32>(m2));
}

// ARIB STD-B67 (HLG) forward OETF. Input is scene linear in [0,1]; output is
// the HLG signal in [0,1].
fn hlg_oetf(e: vec3<f32>) -> vec3<f32> {
    let a = 0.17883277;
    let b = 0.28466892;
    let c = 0.55991073;
    let en = clamp(e, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = sqrt(3.0 * en);
    let hi = a * log(12.0 * en - b) + vec3<f32>(c);
    return select(lo, hi, en > vec3<f32>(1.0 / 12.0));
}

// SMPTE ST 2084 (PQ) inverse EOTF. Input is the PQ code value in [0,1];
// output is display luminance normalized to 10000 cd/m² (E = L/10000).
fn pq_eotf(code: vec3<f32>) -> vec3<f32> {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let cn = clamp(code, vec3<f32>(0.0), vec3<f32>(1.0));
    let v = pow(cn, vec3<f32>(1.0 / m2));
    let num = max(v - vec3<f32>(c1), vec3<f32>(0.0));
    // den = c2 - c3·v ∈ [0.16, 18.85] for v∈[0,1] — always positive.
    let den = vec3<f32>(c2) - c3 * v;
    return pow(num / den, vec3<f32>(1.0 / m1));
}

// ARIB STD-B67 (HLG) inverse OETF. Input is the HLG signal in [0,1]; output
// is scene-linear in [0,1].
fn hlg_eotf(sig: vec3<f32>) -> vec3<f32> {
    let a = 0.17883277;
    let b = 0.28466892;
    let c = 0.55991073;
    let sn = clamp(sig, vec3<f32>(0.0), vec3<f32>(1.0));
    let lo = sn * sn / vec3<f32>(3.0);
    let hi = (exp((sn - vec3<f32>(c)) / vec3<f32>(a)) + vec3<f32>(b)) / vec3<f32>(12.0);
    return select(lo, hi, sn > vec3<f32>(0.5));
}

// Narkowicz ACES-filmic tone map. Compresses HDR working linear (where 1.0 =
// 100 cd/m² reference white and highlights run up to 100.0 = 10000 nits) into
// SDR [0,1] with a filmic highlight roll-off, so an HDR source shown on an
// SDR display preserves highlight DETAIL instead of clipping to flat white.
fn aces_tonemap(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    let xn = max(x, vec3<f32>(0.0));
    let mapped = (xn * (a * xn + vec3<f32>(b))) / (xn * (c * xn + vec3<f32>(d)) + vec3<f32>(e));
    return clamp(mapped, vec3<f32>(0.0), vec3<f32>(1.0));
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

// sRGB OETF (linear → encoded), the same curve the `*UnormSrgb` texture
// formats apply on store. The compositor's SDR blend space is this encoding.
fn srgb_oetf(lin: vec3<f32>) -> vec3<f32> {
    let l = clamp(lin, vec3<f32>(0.0), vec3<f32>(1.0));
    let toe = l * 12.92;
    let lift = 1.055 * pow(l, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(toe, lift, l > vec3<f32>(0.0031308));
}

// sRGB EOTF (encoded → linear), the inverse of `srgb_oetf`.
fn srgb_eotf(enc: vec3<f32>) -> vec3<f32> {
    return linearize(clamp(enc, vec3<f32>(0.0), vec3<f32>(1.0)), 1u);
}
