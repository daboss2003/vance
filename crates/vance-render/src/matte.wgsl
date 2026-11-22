// Keyer matte refinement (ADR 0007), concatenated after common.wgsl: one
// compute pass per line direction, at SOURCE resolution. Choke is an
// erode / dilate (min / max) along a line, soften a blur along a line; the
// keyer chains them (see `compositor::matte_passes`). A workgroup loads a
// segment (plus an apron each side) of 1, 2 or 4 lines — sub-lines, so
// short columns and diagonals don't waste most of a workgroup — into
// workgroup memory, so the cost per pixel grows at most with the log of
// the radius. Each family below has its own entry points, so a workgroup
// holds only the arrays it uses (more workgroups fit on a GPU core):
//   - erode / dilate: a doubling table (extreme over the next 2^k samples),
//     then the extreme of two overlapping power-of-two windows — the same
//     samples as the full window, so the result is exact;
//   - box (softens over 8 px): three equal boxes from exact fixed-point
//     prefix sums (an all-0 window gives exactly 0, an all-1 one exactly
//     1), all in workgroup memory on samples clamped once at the image edge
//     (as the exact kernel's taps clamp), not after every box;
//   - gauss (softens up to 8 px): the exact truncated Gaussian taps.
// Positions past the image clamp per axis, as a clamp-to-edge sampler does.

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<r32float, write>;

// Layout must match `LineUniform` in compositor.rs.
struct LineU {
    // (step x, step y, mode, log2 sub-lines): the line direction in source
    // pixels — (1,0), (0,1), (1,1) or (1,−1) — MODE_*, and how many lines
    // share a workgroup (1, 2 or 4; each gets SHARED / that many samples).
    dir_mode: vec4<i32>,
    // (radius in source pixels, _, _, _)
    radius: vec4<f32>,
    // (width, height, apron, tile): the matte size, the samples loaded
    // before / after each sub-line's segment, and its outputs.
    dims: vec4<u32>,
};
@group(0) @binding(2) var<uniform> lu: LineU;

const MODE_ERODE: i32 = 0;
const MODE_DILATE: i32 = 1;
const MODE_GAUSS: i32 = 2;
const MODE_BOX: i32 = 3;
const LANES: u32 = 256u;
// Samples per lane, and per workgroup: apron + tile + apron
// (`LINE_SHARED` in Rust). Wide segments spread each workgroup's barriers
// over more outputs.
const PER: u32 = 4u;
const SHARED: u32 = 1024u;

// Fixed-point scale of the box prefix sums: 1024 samples of at most 1.0
// sum to at most 2^31, so u32 never overflows.
const FIX: f32 = 2097152.0;

// The line's samples (all), the doubling table (min / max), the box prefix
// sums (fixed point) and their per-lane totals (box).
var<workgroup> buf: array<f32, 1024>;
var<workgroup> wa: array<f32, 1024>;
var<workgroup> wq: array<u32, 1024>;
var<workgroup> lane_sum: array<u32, 256>;
var<workgroup> lane_tmp: array<u32, 256>;

// (start x, start y, length) of line `l` along direction `d`.
fn line_geometry(l: i32, d: vec2<i32>, w: i32, h: i32) -> vec3<i32> {
    if (d.y == 0) {
        return vec3<i32>(0, l, w);
    }
    if (d.x == 0) {
        return vec3<i32>(l, 0, h);
    }
    if (d.y > 0) {
        // x − y = l − (h − 1)
        let c = l - (h - 1);
        let x0 = max(c, 0);
        let y0 = max(-c, 0);
        return vec3<i32>(x0, y0, min(w - x0, h - y0));
    }
    // x + y = l, walking down-left → up-right
    let y0 = min(l, h - 1);
    let x0 = l - y0;
    return vec3<i32>(x0, y0, min(w - x0, y0 + 1));
}

// Sub-line `k` of workgroup `g`: (start x, start y, length); length 0 past
// the last line.
fn sub_line(g: u32, k: u32) -> vec3<i32> {
    let d = lu.dir_mode.xy;
    let w = i32(lu.dims.x);
    let h = i32(lu.dims.y);
    var lines = w + h - 1;
    if (d.y == 0) {
        lines = h;
    } else if (d.x == 0) {
        lines = w;
    }
    let l = i32((g << u32(lu.dir_mode.w)) + k);
    if (l >= lines) {
        return vec3<i32>(0);
    }
    return line_geometry(l, d, w, h);
}

fn pick(a: f32, b: f32, erode: bool) -> f32 {
    return select(max(a, b), min(a, b), erode);
}

fn fixed(v: f32) -> u32 {
    return u32(round(clamp(v, 0.0, 1.0) * FIX));
}

// Inclusive prefix sum of `buf` into `wq`, in fixed point so window sums
// are exact (f32 sums leave a residue where the window is all 0 or all 1):
// each lane sums its PER consecutive samples, a Hillis–Steele scan runs
// over the lane totals, then each lane offsets its own.
fn prefix_sum(li: u32) {
    let b = li * PER;
    let q0 = fixed(buf[b]);
    let q1 = fixed(buf[b + 1u]);
    let q2 = fixed(buf[b + 2u]);
    let q3 = fixed(buf[b + 3u]);
    let l1 = q0 + q1;
    let l2 = l1 + q2;
    let l3 = l2 + q3;
    lane_sum[li] = l3;
    workgroupBarrier();
    // Ping-pong between lane_sum and lane_tmp: one barrier per step. Eight
    // steps end back in lane_sum.
    for (var s = 1u; s < LANES; s = s * 4u) {
        var v = lane_sum[li];
        if (li >= s) { v = v + lane_sum[li - s]; }
        lane_tmp[li] = v;
        workgroupBarrier();
        var u = lane_tmp[li];
        if (li >= 2u * s) { u = u + lane_tmp[li - 2u * s]; }
        lane_sum[li] = u;
        workgroupBarrier();
    }
    var before = 0u;
    if (li > 0u) { before = lane_sum[li - 1u]; }
    wq[b] = before + q0;
    wq[b + 1u] = before + l1;
    wq[b + 2u] = before + l2;
    wq[b + 3u] = before + l3;
    workgroupBarrier();
}

// Box of radius whole + f at sample i (the end samples weigh f); needs
// `prefix_sum` of `buf` in `wq`.
fn box_at(i: u32, whole: u32, f: f32) -> f32 {
    let sum = f32(wq[i + whole] - wq[i - whole - 1u]);
    let ends = f * f32(fixed(buf[i + whole + 1u]) + fixed(buf[i - whole - 1u]));
    return (sum + ends) / ((f32(2u * whole + 1u) + 2.0 * f) * FIX);
}

// A workgroup's segment: the sub-lines its lane's samples i0..i3 lie on
// (sample i is offset (i & sub_mask) of sub-line (i >> sub_shift)), the
// segment start along them, and the offset mask.
struct Segment {
    g0: vec3<i32>,
    g1: vec3<i32>,
    g2: vec3<i32>,
    g3: vec3<i32>,
    seg: i32,
    sub_mask: u32,
};

// Work out this lane's sub-lines and load its four samples into `buf`.
fn load_segment(li: u32, wg: vec3<u32>) -> Segment {
    let log_k = u32(lu.dir_mode.w);
    var sg: Segment;
    sg.sub_mask = (1u << (10u - log_k)) - 1u;
    sg.g0 = sub_line(wg.x, 0u);
    sg.g1 = sg.g0;
    sg.g2 = sg.g0;
    sg.g3 = sg.g0;
    if (log_k == 1u) {
        sg.g2 = sub_line(wg.x, 1u);
        sg.g3 = sg.g2;
    } else if (log_k == 2u) {
        sg.g1 = sub_line(wg.x, 1u);
        sg.g2 = sub_line(wg.x, 2u);
        sg.g3 = sub_line(wg.x, 3u);
    }
    sg.seg = i32(wg.y * lu.dims.w);
    let d = lu.dir_mode.xy;
    let hi = vec2<i32>(i32(lu.dims.x) - 1, i32(lu.dims.y) - 1);
    let base = sg.seg - i32(lu.dims.z);
    let m = sg.sub_mask;
    buf[li] = textureLoad(src, clamp(sg.g0.xy + d * (base + i32(li & m)), vec2<i32>(0), hi), 0).r;
    let i1 = li + LANES;
    buf[i1] = textureLoad(src, clamp(sg.g1.xy + d * (base + i32(i1 & m)), vec2<i32>(0), hi), 0).r;
    let i2 = li + 2u * LANES;
    buf[i2] = textureLoad(src, clamp(sg.g2.xy + d * (base + i32(i2 & m)), vec2<i32>(0), hi), 0).r;
    let i3 = li + 3u * LANES;
    buf[i3] = textureLoad(src, clamp(sg.g3.xy + d * (base + i32(i3 & m)), vec2<i32>(0), hi), 0).r;
    return sg;
}

// The output position along `line` for sample `j`, or −1 when `j` lies in
// an apron or past the line's end (masked store).
fn out_x(j: u32, line: vec3<i32>, sg: Segment) -> i32 {
    let apron = lu.dims.z;
    let off = j & sg.sub_mask;
    if (off < apron || off - apron >= lu.dims.w) {
        return -1;
    }
    let x = i32(off - apron);
    return select(-1, x, sg.seg + x < line.z);
}

fn store(line: vec3<i32>, sg: Segment, x: i32, o: f32) {
    // Intentional: each pass rounds to f16, as the R16Float matte passes
    // this replaces stored it, so the exact modes reproduce their output.
    textureStore(dst, line.xy + lu.dir_mode.xy * (sg.seg + x), vec4<f32>(quantizeToF16(o), 0.0, 0.0, 1.0));
}

// Erode / dilate output at sample j: the extreme of the two overlapping
// power-of-two windows [j − whole, +p) ∪ [j + whole − p + 1, +p).
fn emit_minmax(j: u32, line: vec3<i32>, sg: Segment, p: u32) {
    let x = out_x(j, line, sg);
    if (x < 0) {
        return;
    }
    let r = lu.radius.x;
    let whole = u32(floor(r));
    let f = r - f32(whole);
    let erode = lu.dir_mode.z == MODE_ERODE;
    var o = pick(wa[j - whole], wa[j + whole + 1u - p], erode);
    if (f > 1e-3) {
        // A fractional radius ends on interpolated samples: partial
        // erosion, so the choke slider is continuous.
        let e1 = mix(buf[j + whole], buf[j + whole + 1u], f);
        let e2 = mix(buf[j - whole], buf[j - whole - 1u], f);
        o = pick(o, pick(e1, e2, erode), erode);
    }
    store(line, sg, x, o);
}

// Erode / dilate a segment. Every invocation reaches every barrier.
fn minmax_pass(li: u32, wg: vec3<u32>) {
    let sg = load_segment(li, wg);
    let i0 = li;
    let i1 = li + LANES;
    let i2 = li + 2u * LANES;
    let i3 = li + 3u * LANES;
    let whole = u32(floor(lu.radius.x));
    let erode = lu.dir_mode.z == MODE_ERODE;
    // wa[i] = extreme of buf[i .. i + p), p the largest power of two not
    // above the window length 2·whole + 1.
    wa[i0] = buf[i0];
    wa[i1] = buf[i1];
    wa[i2] = buf[i2];
    wa[i3] = buf[i3];
    workgroupBarrier();
    var p = 1u;
    let span = 2u * whole + 1u;
    for (var s = 1u; 2u * s <= span; s = s * 2u) {
        // The last lane block reads past the end only for its top `s`.
        let v0 = pick(wa[i0], wa[i0 + s], erode);
        let v1 = pick(wa[i1], wa[i1 + s], erode);
        let v2 = pick(wa[i2], wa[i2 + s], erode);
        var v3 = wa[i3];
        if (i3 + s < SHARED) { v3 = pick(v3, wa[i3 + s], erode); }
        workgroupBarrier();
        wa[i0] = v0;
        wa[i1] = v1;
        wa[i2] = v2;
        wa[i3] = v3;
        workgroupBarrier();
        p = 2u * s;
    }
    emit_minmax(i0, sg.g0, sg, p);
    emit_minmax(i1, sg.g1, sg, p);
    emit_minmax(i2, sg.g2, sg, p);
    emit_minmax(i3, sg.g3, sg, p);
}

// Exact truncated Gaussian output at sample j (the taps and arithmetic of
// the soften it replaces).
fn emit_gauss(j: u32, line: vec3<i32>, sg: Segment) {
    let x = out_x(j, line, sg);
    if (x < 0) {
        return;
    }
    let r = lu.radius.x;
    let sigma = max(r * 0.5, 1e-3);
    let taps = u32(ceil(r));
    var acc = buf[j];
    var wsum = 1.0;
    for (var i = 1u; i <= taps; i = i + 1u) {
        let xi = f32(i);
        let wt = exp(-0.5 * (xi / sigma) * (xi / sigma));
        acc = acc + wt * (buf[j + i] + buf[j - i]);
        wsum = wsum + 2.0 * wt;
    }
    store(line, sg, x, acc / wsum);
}

fn gauss_pass(li: u32, wg: vec3<u32>) {
    let sg = load_segment(li, wg);
    workgroupBarrier();
    emit_gauss(li, sg.g0, sg);
    emit_gauss(li + LANES, sg.g1, sg);
    emit_gauss(li + 2u * LANES, sg.g2, sg);
    emit_gauss(li + 3u * LANES, sg.g3, sg);
}

fn emit_box(j: u32, line: vec3<i32>, sg: Segment) {
    let x = out_x(j, line, sg);
    if (x >= 0) {
        store(line, sg, x, buf[j]);
    }
}

// Three boxes in place; after box n the samples within n·(whole+1) of a
// sub-line's ends are stale (they mix in the neighbouring sub-line), which
// the apron (≥ 3·(whole+1)) keeps away from the outputs. Every invocation
// reaches every barrier.
fn box_pass(li: u32, wg: vec3<u32>) {
    let sg = load_segment(li, wg);
    let i0 = li;
    let i1 = li + LANES;
    let i2 = li + 2u * LANES;
    let i3 = li + 3u * LANES;
    let r = lu.radius.x;
    let whole = u32(floor(r));
    let f = r - f32(whole);
    let lo = whole + 1u;
    for (var n = 0u; n < 3u; n = n + 1u) {
        workgroupBarrier();
        prefix_sum(li);
        var v0 = buf[i0];
        var v1 = buf[i1];
        var v2 = buf[i2];
        var v3 = buf[i3];
        if (i0 >= lo) { v0 = box_at(i0, whole, f); }
        v1 = box_at(i1, whole, f);
        v2 = box_at(i2, whole, f);
        if (i3 + lo < SHARED) { v3 = box_at(i3, whole, f); }
        workgroupBarrier();
        buf[i0] = v0;
        buf[i1] = v1;
        buf[i2] = v2;
        buf[i3] = v3;
    }
    workgroupBarrier();
    emit_box(i0, sg.g0, sg);
    emit_box(i1, sg.g1, sg);
    emit_box(i2, sg.g2, sg);
    emit_box(i3, sg.g3, sg);
}

// Some sub-line of this workgroup reaches its segment.
fn busy(wg: vec3<u32>) -> bool {
    let seg = i32(wg.y * lu.dims.w);
    var hit = false;
    for (var k = 0u; k < (1u << u32(lu.dir_mode.w)); k = k + 1u) {
        hit = hit || seg < sub_line(wg.x, k).z;
    }
    return hit;
}

// Intentional: two entry points per family. The plain ones (DX12) run every
// barrier in every workgroup — FXC rejects a return before a barrier
// (X3663) — and mask the stores of workgroups past every sub-line's end.
// The `_exit` ones (Metal, Vulkan) skip those workgroups, a workgroup-
// uniform early exit those backends accept, saving the idle diagonal
// segments' work.
@compute @workgroup_size(256)
fn cs_minmax(@builtin(local_invocation_index) li: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    minmax_pass(li, wg);
}

@compute @workgroup_size(256)
fn cs_minmax_exit(@builtin(local_invocation_index) li: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    if (!busy(wg)) {
        return;
    }
    minmax_pass(li, wg);
}

@compute @workgroup_size(256)
fn cs_gauss(@builtin(local_invocation_index) li: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    gauss_pass(li, wg);
}

@compute @workgroup_size(256)
fn cs_gauss_exit(@builtin(local_invocation_index) li: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    if (!busy(wg)) {
        return;
    }
    gauss_pass(li, wg);
}

@compute @workgroup_size(256)
fn cs_box(@builtin(local_invocation_index) li: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    box_pass(li, wg);
}

@compute @workgroup_size(256)
fn cs_box_exit(@builtin(local_invocation_index) li: u32, @builtin(workgroup_id) wg: vec3<u32>) {
    if (!busy(wg)) {
        return;
    }
    box_pass(li, wg);
}

// The refined matte (r32float) → the layer's R16Float matte.
@fragment
fn fs_matte_copy(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(textureLoad(src, vec2<i32>(in.clip_position.xy), 0).r, 0.0, 0.0, 1.0);
}
