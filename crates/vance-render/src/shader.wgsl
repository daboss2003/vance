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
    // Serial color-grade chain (Phase 6.6/6.7/6.9/6.10). Must match the
    // Rust GradeNode layout (11 vec4 = 176 bytes) byte-for-byte.
    grades: array<GradeNode, 8>,
};

// One color-grade node. Layout mirrors `GradeNode` in renderer.rs.
struct GradeNode {
    lift: vec4<f32>,        // (r, g, b, master)
    gamma: vec4<f32>,       // (r, g, b, master)
    gain: vec4<f32>,        // (r, g, b, master)
    log_offset: vec4<f32>,  // (offset_r, offset_g, offset_b, _)
    log_smh: vec4<f32>,     // (.x = curves non-identity flag)
    flags: vec4<f32>,       // (enabled, mode 0=primary/1=log/2=curves, qual_enable, qual_invert)
    qual_a: vec4<f32>,      // (hue_center, hue_range, sat_lo, sat_hi)
    qual_b: vec4<f32>,      // (luma_lo, luma_hi, hue_soft, sat_soft)
    qual_c: vec4<f32>,      // (luma_soft, show_mask, _, _)
    win_a: vec4<f32>,       // (shape 0=off/1=ellipse/2=rect, center_x, center_y, rotation_rad)
    win_b: vec4<f32>,       // (half_size_x, half_size_y, feather, invert)
};
@group(0) @binding(4) var<uniform> params: Params;

// Set by apply_grades when a node's qualifier "show mask" is on; fs_main
// returns the raw grayscale matte BEFORE the downstream contrast / keys /
// output matrix / view-LUT (which would otherwise tint it). Per-invocation.
var<private> g_matte_hit: bool = false;

// Primary grade: per-channel lift/gamma/gain with a master fold.
// DaVinci-style (NOT ASC-CDL): the gamma exponent is the RECIPROCAL,
// so gamma > 1 brightens midtones. Order: out = pow(max(in*gain +
// lift, 0), 1/gamma), channelwise. We do NOT clamp the upper bound —
// values may exceed 1.0 between nodes so a later node can pull a
// highlight back down (true serial-chain behavior); the pow base is
// only floored at 0 because pow of a negative base is undefined.
fn apply_primary(rgb: vec3<f32>, n: GradeNode) -> vec3<f32> {
    let gain = n.gain.xyz * n.gain.w;
    let lift = n.lift.xyz + vec3<f32>(n.lift.w);
    // Guard gamma away from 0; master multiplies the per-channel gamma.
    let gamma = max(n.gamma.xyz * n.gamma.w, vec3<f32>(1e-3));
    let lifted = max(rgb * gain + lift, vec3<f32>(0.0));
    return pow(lifted, vec3<f32>(1.0) / gamma);
}

// Log grade: an overall offset plus tonal-band shadow / midtone /
// highlight balances. Shadow rides `lift.xyz`, midtone `gamma.xyz`,
// highlight `gain.xyz` (see GradeNodeParams::to_gpu in renderer.rs).
// The bands are windowed in a PERCEPTUAL encoding (approx gamma-2.2 of
// the linear luma) so "midtone" lands near perceptual mid-gray
// (linear ~0.18) rather than linear 0.5 — matching what a colorist's
// eye expects from a printer-lights/log tool. No upper clamp, for the
// same serial-chain reason as apply_primary.
fn apply_log(rgb: vec3<f32>, n: GradeNode) -> vec3<f32> {
    let luma_lin = dot(max(rgb, vec3<f32>(0.0)), vec3<f32>(0.2126, 0.7152, 0.0722));
    let luma = pow(min(luma_lin, 1.0), 1.0 / 2.2);
    // Smooth tonal-band weights partitioning [0,1] (sum == 1).
    let w_sh = 1.0 - smoothstep(0.0, 0.5, luma);
    let w_hi = smoothstep(0.5, 1.0, luma);
    let w_mid = 1.0 - w_sh - w_hi;
    var out = rgb + n.log_offset.xyz;
    out = out + n.lift.xyz * w_sh + n.gamma.xyz * w_mid + n.gain.xyz * w_hi;
    return max(out, vec3<f32>(0.0));
}

// Curves grade: per-channel tone curve sampled from the atlas row for
// node `i`. The atlas is a function LUT — column p of a row holds the
// curve's output at input level p. Master is folded into R/G/B at bake
// time, so each channel is one sample. The luma curve (alpha lane) is
// applied as a hue-preserving ratio.
//
// The luma curve reads the *input* luma (computed from `rgb`, BEFORE the
// R/G/B curves) rather than the post-R/G/B luma: this decouples the luma
// curve from the R/G/B curves so a colorist's luma adjustment is
// predictable from the source pixel and independent of channel curves.
// (With an identity luma curve `cl == l`, so the ratio is exactly 1
// regardless of which luma is used — the choice only matters when both
// the luma curve and the R/G/B curves are non-identity.)
//
// Sample COORDINATEs are clamped to the LUT's [0,1] domain (super-1.0
// inputs flatten to the curve endpoint), but the output is not
// upper-clamped — same serial-chain convention as apply_primary. Uses
// textureSampleLevel because we're in the (non-uniform) grade loop where
// implicit-derivative sampling is illegal in WGSL.
// ---- HSL hue-wheel curves (Phase 6.8b) ------------------------------------
//
// Five secondary curves operating in the SAME perceptual-encode HSL space
// the 6.9 qualifier uses (encode = pow(linear, 1/2.2)):
//   hh: hue -> hue REMAP, cyclic input. y = sampled (direct remap).
//   hs: hue -> sat MULTIPLIER. scale = 2*sampled (y=0.5 is no-op).
//   hl: hue -> lum MULTIPLIER. scale = 2*sampled (y=0.5 is no-op).
//   ss: sat -> sat REMAP. y = sampled.
//   ll: lum -> lum REMAP. y = sampled.
// Stored in atlas rows 3*i+1 (lanes r,g,b,a = hh,hs,hl,ss) and 3*i+2
// (lane r = ll). Hue domain is CYCLIC; we wrap the sample coordinate
// via fract() in-shader so the existing clamp sampler suffices.

fn hsl_to_rgb_enc(h: f32, s: f32, l: f32) -> vec3<f32> {
    // Standard HSL -> encoded RGB (in [0,1]); returns the gamma-encoded
    // RGB that, when run through pow(_, 2.2), gives back linear RGB.
    let c = (1.0 - abs(2.0 * l - 1.0)) * s;
    let hp = fract(h) * 6.0;
    let x = c * (1.0 - abs((hp - 2.0 * floor(hp / 2.0)) - 1.0));
    var rgb = vec3<f32>(0.0);
    if (hp < 1.0)      { rgb = vec3<f32>(c, x, 0.0); }
    else if (hp < 2.0) { rgb = vec3<f32>(x, c, 0.0); }
    else if (hp < 3.0) { rgb = vec3<f32>(0.0, c, x); }
    else if (hp < 4.0) { rgb = vec3<f32>(0.0, x, c); }
    else if (hp < 5.0) { rgb = vec3<f32>(x, 0.0, c); }
    else               { rgb = vec3<f32>(c, 0.0, x); }
    let m = l - 0.5 * c;
    return rgb + vec3<f32>(m);
}

fn apply_hsl_curves(rgb_lin: vec3<f32>, i: u32) -> vec3<f32> {
    let row1 = (3.0 * f32(i) + 1.5) / 24.0;     // HSL-1: hh, hs, hl, ss
    let row2 = (3.0 * f32(i) + 2.5) / 24.0;     // HSL-2: ll, _, _, _
    let hsl = rgb_to_hsl(rgb_lin);
    let h = hsl.x;
    let s = hsl.y;
    let l = hsl.z;
    // Hue is cyclic — wrap via fract() before the clamp sample. (The
    // 0/1 seam can produce a tiny filter blip for curves with very
    // different endpoints; cheaper than a separate Repeat sampler.)
    let by_hue = textureSampleLevel(tex_curve_atlas, samp, vec2<f32>(fract(h), row1), 0.0);
    // hh = REMAP (direct, cyclic).
    let new_h = fract(by_hue.r);
    // hs / hl = MULTIPLIERS keyed by hue. Stored 0..1 with 0.5 == no-op;
    // decoded as 2*y so the range is [0, 2].
    let s1 = clamp(s * (2.0 * by_hue.g), 0.0, 1.0);
    let l1 = clamp(l * (2.0 * by_hue.b), 0.0, 1.0);
    // ss = REMAP keyed by the intermediate saturation (row1, alpha lane).
    let by_sat = textureSampleLevel(tex_curve_atlas, samp, vec2<f32>(s1, row1), 0.0);
    let new_s = clamp(by_sat.a, 0.0, 1.0);
    // ll = REMAP keyed by the intermediate luminance (row2, r lane).
    let by_lum = textureSampleLevel(tex_curve_atlas, samp, vec2<f32>(l1, row2), 0.0);
    let new_l = clamp(by_lum.r, 0.0, 1.0);
    // Back to linear RGB via HSL -> encoded RGB -> pow(_, 2.2).
    let out_enc = hsl_to_rgb_enc(new_h, new_s, new_l);
    return pow(out_enc, vec3<f32>(2.2));
}

fn apply_curve(rgb: vec3<f32>, i: u32, n: GradeNode) -> vec3<f32> {
    // Atlas is MAX_GRADES × 3 rows tall (6.8b). For node i:
    //   row 3*i     = tone (R/G/B + luma)
    //   row 3*i+1   = HSL-1 (hh, hs, hl, ss)
    //   row 3*i+2   = HSL-2 (ll, _, _, _)
    let row_tone = (3.0 * f32(i) + 0.5) / 24.0;
    var out = rgb;
    if (n.log_smh.x > 0.5) {                              // tone block
        let cr = textureSampleLevel(tex_curve_atlas, samp, vec2<f32>(clamp(rgb.r, 0.0, 1.0), row_tone), 0.0).r;
        let cg = textureSampleLevel(tex_curve_atlas, samp, vec2<f32>(clamp(rgb.g, 0.0, 1.0), row_tone), 0.0).g;
        let cb = textureSampleLevel(tex_curve_atlas, samp, vec2<f32>(clamp(rgb.b, 0.0, 1.0), row_tone), 0.0).b;
        out = vec3<f32>(cr, cg, cb);
        let l = dot(max(rgb, vec3<f32>(0.0)), vec3<f32>(0.2126, 0.7152, 0.0722));
        let cl = textureSampleLevel(tex_curve_atlas, samp, vec2<f32>(clamp(l, 0.0, 1.0), row_tone), 0.0).a;
        // Scale by curve(input_luma)/input_luma to keep hue; guard near black.
        out = select(out, out * (cl / max(l, 1e-4)), l > 1e-4);
    }
    if (n.log_smh.y > 0.5) {                              // HSL block (6.8b)
        out = apply_hsl_curves(out, i);
    }
    return out;
}

// ---- HSL qualifier (secondary key), Phase 6.9 -----------------------------
//
// The key is computed in a PERCEPTUAL encoding (approx gamma 1/2.2) of the
// node's linear input — consistent with apply_log's tonal windowing, and
// because hue/sat are numerically unstable for dark pixels in linear light
// (a "red shirt" must key by appearance). The encode is clamped to [0,1]
// for KEY STABILITY only; the graded value itself is never clamped.

// RGB (linear) -> HSL on the perceptual encoding. Returns (h in [0,1),
// s in [0,1], l in [0,1]).
fn rgb_to_hsl(rgb_lin: vec3<f32>) -> vec3<f32> {
    let c = pow(clamp(rgb_lin, vec3<f32>(0.0), vec3<f32>(1.0)), vec3<f32>(1.0 / 2.2));
    let mx = max(c.r, max(c.g, c.b));
    let mn = min(c.r, min(c.g, c.b));
    let d = mx - mn;
    let l = 0.5 * (mx + mn);
    var h = 0.0;
    if (d > 1e-6) {
        if (mx == c.r) {
            h = (c.g - c.b) / d + select(0.0, 6.0, c.g < c.b);
        } else if (mx == c.g) {
            h = (c.b - c.r) / d + 2.0;
        } else {
            h = (c.r - c.g) / d + 4.0;
        }
        h = h / 6.0;
    }
    let s = select(0.0, d / (1.0 - abs(2.0 * l - 1.0) + 1e-6), d > 1e-6);
    return vec3<f32>(h, clamp(s, 0.0, 1.0), clamp(l, 0.0, 1.0));
}

// Cyclic hue distance in [0, 0.5] (wraps at 1.0).
fn hue_dist(a: f32, b: f32) -> f32 {
    return abs(fract(a - b + 0.5) - 0.5);
}

// One-sided soft band: 1 inside |dist| <= half, feathered to 0 over `soft`.
fn soft_band(dist: f32, half_width: f32, soft: f32) -> f32 {
    return 1.0 - smoothstep(half_width, half_width + max(soft, 1e-4), dist);
}

// Two-sided soft range: 1 inside [lo, hi], feathered by `soft` on each edge.
fn soft_range(x: f32, lo: f32, hi: f32, soft: f32) -> f32 {
    let s = max(soft, 1e-4);
    return smoothstep(lo - s, lo, x) * (1.0 - smoothstep(hi, hi + s, x));
}

// Qualifier mask in [0,1] for the node's input pixel: hue wedge ∩ sat band
// ∩ luma band, optionally inverted. (6.10 will multiply in a window mask.)
fn qualifier_mask(rgb_lin: vec3<f32>, n: GradeNode) -> f32 {
    let hsl = rgb_to_hsl(rgb_lin);
    let k_h = soft_band(hue_dist(hsl.x, n.qual_a.x), n.qual_a.y * 0.5, n.qual_b.z);
    let k_s = soft_range(hsl.y, n.qual_a.z, n.qual_a.w, n.qual_b.w);
    let k_l = soft_range(hsl.z, n.qual_b.x, n.qual_b.y, n.qual_c.x);
    var m = k_h * k_s * k_l;
    if (n.flags.w > 0.5) { m = 1.0 - m; }   // invert
    return clamp(m, 0.0, 1.0);
}

// ---- Power window (shape mask), Phase 6.10 --------------------------------
//
// A geometric mask on the node's correction, computed in SOURCE-UV space
// (image content) so it stays locked to the subject under clip
// transform/crop and so the 6.11 point tracker (which tracks image
// content) can animate the center directly. `aspect = width/height` makes
// equal half-sizes draw a circle in pixels rather than an ellipse.
fn window_mask(uv: vec2<f32>, n: GradeNode, aspect: f32) -> f32 {
    let shape = n.win_a.x;
    if (shape < 0.5) { return 1.0; }   // 0 = off -> no restriction
    let center = n.win_a.yz;
    let rot = n.win_a.w;
    let half = max(n.win_b.xy, vec2<f32>(1e-4));
    let feather = max(n.win_b.z, 1e-4);
    let invert = n.win_b.w;
    // Aspect-correct BOTH the delta and the x radius so the ratio is
    // preserved (a one-sided multiply makes egg shapes).
    var d = uv - center;
    d.x = d.x * aspect;
    let c = cos(-rot);
    let s = sin(-rot);
    let local = vec2<f32>(d.x * c - d.y * s, d.x * s + d.y * c);
    let r = vec2<f32>(half.x * aspect, half.y);
    var nd: f32;
    if (shape < 1.5) {
        nd = length(local / r);                          // ellipse: 1.0 on boundary
    } else {
        let q = abs(local) / r;
        nd = max(q.x, q.y);                              // rect: Chebyshev box
    }
    // Convert the uv-space feather to the nd metric via the mean radius.
    let band = feather / max(0.5 * (r.x + r.y), 1e-4);
    var m = 1.0 - smoothstep(1.0, 1.0 + band, nd);
    if (invert > 0.5) { m = 1.0 - m; }
    return clamp(m, 0.0, 1.0);
}

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
    // Frame aspect for circular windows — tex_y_a is always bound when
    // the shader runs (render errors NoFrame otherwise). Hoisted out of
    // the loop.
    let dims = textureDimensions(tex_y_a);
    let aspect = f32(dims.x) / max(f32(dims.y), 1.0);
    for (var i = 0u; i < count; i = i + 1u) {
        let n = params.grades[i];
        if (n.flags.x < 0.5) { continue; }      // bypassed node
        let src = rgb;
        var graded = src;
        if (n.flags.y < 0.5) {
            graded = apply_primary(src, n);
        } else if (n.flags.y < 1.5) {
            graded = apply_log(src, n);
        } else {
            // mode 2 = curves; run apply_curve when EITHER the tone
            // (log_smh.x) or the HSL (log_smh.y, 6.8b) block is active —
            // each block self-gates inside.
            if (n.log_smh.x > 0.5 || n.log_smh.y > 0.5) {
                graded = apply_curve(src, i, n);
            }
        }
        // Combined node mask: qualifier (hue/sat/luma) ∩ power window.
        var mask = 1.0;
        if (n.flags.z > 0.5) {                  // qualifier enabled
            mask = qualifier_mask(src, n);
        }
        mask = mask * window_mask(uv, n, aspect);
        rgb = mix(src, graded, mask);
        // Matte solo: show this node's COMBINED mask as grayscale and
        // stop. Gated on show_mask alone so a window-only node can solo.
        if (n.qual_c.y > 0.5) {
            g_matte_hit = true;
            rgb = vec3<f32>(mask);
            break;
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
