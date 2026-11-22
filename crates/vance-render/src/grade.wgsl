// Colour-grade node math, shared by shader.wgsl (legacy single-clip path)
// and layer.wgsl (compositor layer pass). Needs the including module to
// declare `samp: sampler` and `tex_curve_atlas: texture_2d<f32>`.

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
// content) can animate the center directly. Half-sizes are fractions of
// the frame's width / height; `aspect` = the frame's DISPLAY width / height
// (storage width × pixel aspect), so rotation is rigid and the feather even
// in displayed pixels — also on anamorphic sources.
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

// One ENABLED grade node: the graded colour (xyz) and the node's combined
// mask (w) — qualifier (hue/sat/luma) ∩ power window, computed on the node's
// INPUT pixel + the source UV (standard secondary behavior). Both masks are
// 1.0 when their feature is off. The caller mixes `src → xyz` by `w` and
// handles the show-mask solo.
fn grade_node_eval(src: vec3<f32>, uv: vec2<f32>, i: u32, n: GradeNode, aspect: f32) -> vec4<f32> {
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
    var mask = 1.0;
    if (n.flags.z > 0.5) {                  // qualifier enabled
        mask = qualifier_mask(src, n);
    }
    mask = mask * window_mask(uv, n, aspect);
    return vec4<f32>(graded, mask);
}
