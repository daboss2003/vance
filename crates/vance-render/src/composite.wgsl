// Compositor blend pass (ADR 0007), concatenated after common.wgsl and
// blend.wgsl. Reads the backdrop accumulator and one (or, for a dissolve,
// two) rendered layer slots and writes the other accumulator. All values are
// premultiplied, in the frame's blend space (display-encoded for SDR output,
// linear working for HDR). A single layer usually skips this pass: the
// layer pass composites itself (`fs_layer_over` in layer.wgsl).

@group(0) @binding(0) var tex_bd: texture_2d<f32>;
@group(0) @binding(1) var tex_a: texture_2d<f32>;
@group(0) @binding(2) var tex_b: texture_2d<f32>;
@group(0) @binding(3) var<uniform> cu: CompositeU;

@fragment
fn fs_composite(in: VsOut) -> @location(0) vec4<f32> {
    let p = vec2<i32>(in.clip_position.xy);
    var bd = vec4<f32>(0.0);
    if (cu.modes.w == 0u) {
        bd = textureLoad(tex_bd, p, 0);
    }
    let a = blend_pm(bd, textureLoad(tex_a, p, 0) * cu.vals.x, cu.modes.x);
    if (cu.modes.z == 0u) {
        return a;
    }
    // Dissolve on one track: lerp(blend(A over bd), blend(B over bd), mix).
    let b = blend_pm(bd, textureLoad(tex_b, p, 0) * cu.vals.y, cu.modes.y);
    return mix(a, b, cu.vals.z);
}
