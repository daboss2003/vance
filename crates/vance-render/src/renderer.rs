//! The wgpu-side renderer.

use thiserror::Error;
use vance_decoder::{FrameData, PixelFormat};

/// WGSL sources. Shared stages live in `common.wgsl` (colour math, no
/// bindings), `grade.wgsl` (grade nodes; needs `samp` + `tex_curve_atlas`)
/// and `blend.wgsl` (blend modes, no bindings), concatenated in front of
/// each pass's own file.
pub(crate) mod wgsl {
    /// The legacy single-clip pipeline (`Renderer::render_to`).
    pub(crate) const LEGACY: &str = concat!(
        include_str!("common.wgsl"),
        "\n",
        include_str!("grade.wgsl"),
        "\n",
        include_str!("shader.wgsl"),
    );
    /// Compositor layer pass (plain and fused with its composite) + the
    /// keyer pre-passes.
    pub(crate) const LAYER: &str = concat!(
        include_str!("common.wgsl"),
        "\n",
        include_str!("grade.wgsl"),
        "\n",
        include_str!("blend.wgsl"),
        "\n",
        include_str!("layer.wgsl"),
    );
    /// Compositor blend pass.
    pub(crate) const COMPOSITE: &str = concat!(
        include_str!("common.wgsl"),
        "\n",
        include_str!("blend.wgsl"),
        "\n",
        include_str!("composite.wgsl"),
    );
    /// Compositor keyer matte line passes (compute) + the matte copy.
    pub(crate) const MATTE: &str = concat!(
        include_str!("common.wgsl"),
        "\n",
        include_str!("matte.wgsl"),
    );
    /// Compositor output pass + RGBA8 pack/unpack.
    pub(crate) const OUTPUT: &str = concat!(
        include_str!("common.wgsl"),
        "\n",
        include_str!("output.wgsl"),
    );
}

/// Renderer errors.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum RenderError {
    /// No suitable wgpu adapter was available.
    #[error("no wgpu adapter found")]
    NoAdapter,

    /// Device/queue creation failed.
    #[error("device request failed: {0}")]
    DeviceRequest(String),

    /// Frame's `bytes` did not match the size implied by `width`, `height`, `pixel_format`.
    #[error("malformed frame: expected {expected} bytes, got {got}")]
    MalformedFrame {
        /// Expected total byte count for the frame.
        expected: usize,
        /// Actual byte count we received.
        got: usize,
    },

    /// The renderer has not yet been given a frame.
    #[error("no frame uploaded yet")]
    NoFrame,

    /// The compositor was used out of order or with bad input (no
    /// `begin_frame`, a stale layer handle, a wrong-sized image).
    #[error("compositor: {0}")]
    Compositor(String),
}

/// Configuration knobs passed to [`Renderer::new`].
#[derive(Debug, Clone)]
pub struct RendererConfig {
    /// Output texture format the rendered RGB will be written to. Phase 1.5
    /// uses the surface format from the parented viewport; the headless test
    /// uses `Rgba8UnormSrgb`.
    pub target_format: wgpu::TextureFormat,
    /// `wgpu::Backends` mask. Defaults to `PRIMARY` (Metal/Vulkan/D3D12).
    pub backends: wgpu::Backends,
}

impl Default for RendererConfig {
    fn default() -> Self {
        Self {
            target_format: wgpu::TextureFormat::Rgba8UnormSrgb,
            backends: wgpu::Backends::PRIMARY,
        }
    }
}

/// A wgpu renderer that converts a YUV frame into RGB on the GPU.
///
/// Owned and reused across frames; resizes its plane textures lazily when
/// the input dimensions change. Multi-layer work (dissolves included) goes
/// through the [`crate::Compositor`].
/// Fullscreen-triangle RGBA passthrough blit (Phase 10 preview). Maps
/// vertex index → a triangle covering the viewport + samples the
/// source RGBA texture.
const BLIT_WGSL: &str = r#"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};
@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var o: VsOut;
    let x = f32((vi << 1u) & 2u);
    let y = f32(vi & 2u);
    o.uv = vec2<f32>(x, y);
    o.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return o;
}
@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var src_smp: sampler;
@fragment
fn fs_main(i: VsOut) -> @location(0) vec4<f32> {
    return textureSample(src_tex, src_smp, i.uv);
}
"#;

/// GPU renderer for decoded video frames. Holds one frame's plane
/// textures, applies the per-clip color pipeline (grades, curves, LUTs,
/// keys, crop/transform), and draws the result to a surface view or reads
/// it back as an RGBA buffer.
pub struct Renderer {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    /// The color-target format `pipeline` was built for. A render-pass
    /// attachment must match it, so the readback targets use this format.
    /// An HDR export renderer is built with `Rgba16Float` here.
    target_format: wgpu::TextureFormat,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Whether the device enabled `TEXTURE_FORMAT_16BIT_NORM` (needed for
    /// R16Unorm 10-bit plane textures). When false, a 10-bit source uploads
    /// as two-byte Rg8 planes the shader filters (the same 10-bit codes) —
    /// the feature is native-only and absent on some Vulkan/GL adapters.
    supports_r16: bool,
    /// Plane textures ([`frame_planes`]): Y / U / V, Y / UV (NV12), or RGBA
    /// in the Y slot; an unused slot holds a 1×1 dummy. `None` until the
    /// first upload.
    tex_y: Option<wgpu::Texture>,
    tex_u: Option<wgpu::Texture>,
    tex_v: Option<wgpu::Texture>,
    /// Loaded 3D LUT — uploaded into a 3D texture sampled by the
    /// fragment shader. `None` until the first `upload_lut` call;
    /// the bind group binds a 1×1×1 dummy when no LUT is in use,
    /// gated by `lut_params.enable`.
    tex_lut: Option<wgpu::Texture>,
    /// Size of the currently-uploaded LUT, or `1` when only the
    /// dummy is bound. Used to skip a re-upload when the new LUT
    /// has the same dimensions.
    lut_size: u32,
    /// Display-side LUT (Phase 6.5). Same shape as `tex_lut` but
    /// applied at the very end of the pipeline (post working→output
    /// matrix). Used for monitoring transforms; export bypasses it.
    tex_view_lut: Option<wgpu::Texture>,
    view_lut_size: u32,
    /// Color-curve LUT atlas (Phase 6.8): a 256×8 Rgba16Float 2D
    /// texture, one row per grade node, packing the baked tone
    /// curves (R/G/B in rgb, luma in alpha). `None` until the first
    /// curves node is uploaded; a 1×1 dummy keeps the bind group valid.
    tex_curve_atlas: Option<wgpu::Texture>,
    /// Width of the currently-allocated `tex_curve_atlas` (1 while it's
    /// the dummy). Compared in `upload_curve_atlas` so the real atlas
    /// replaces the dummy instead of overrunning it — mirrors how
    /// `lut_size` guards the LUT texture.
    curve_atlas_width: u32,
    /// Curve inputs baked into `tex_curve_atlas` on the last upload,
    /// keyed by node index. Compared each `set_grades` so unchanged
    /// curves skip the (expensive) re-bake + texture write.
    last_curve_inputs: Vec<(usize, CurveSet)>,
    bind_group: Option<wgpu::BindGroup>,
    /// Per-frame parameter uniform — opacity (5.2), transform (5.3),
    /// crop + brightness/contrast (5.6), and the transition mix
    /// factor (5.4). The WGSL `Params` struct must keep its layout
    /// in sync.
    params_buffer: wgpu::Buffer,
    current_params: RenderParams,
    /// Lazily-built RGBA blit resources for [`Renderer::present_rgba`]
    /// (Phase 10 — showing CPU-side WASM effect output in the preview).
    blit: Option<BlitResources>,
    /// M1 multi-layer compositor on the same device (ADR 0007).
    compositor: crate::compositor::Compositor,
}

/// Resources for the RGBA passthrough blit (a fullscreen-triangle
/// sample of one Rgba8UnormSrgb texture). Built on first
/// [`Renderer::present_rgba`] call + reused; the source texture is
/// resized as the frame size changes.
struct BlitResources {
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Target format the `pipeline` was built for. Rebuilt if a later
    /// call targets a different format.
    format: wgpu::TextureFormat,
    texture: Option<wgpu::Texture>,
    tex_dims: (u32, u32),
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct RenderParams {
    /// Output color multiplier. 1.0 = passthrough, 0.0 = full fade.
    opacity: f32,
    /// Rotation around the frame center, in radians.
    rotation: f32,
    /// Translation as a fraction of frame size; 0 = no shift.
    translate: [f32; 2],
    /// Scale around the frame center; 1.0 = original size.
    scale: [f32; 2],
    /// Additive RGB offset applied to the gamma-encoded color.
    /// 0.0 = passthrough; range ~[-1, 1].
    brightness: f32,
    /// Multiplicative spread around 0.5. 1.0 = passthrough; 0 = flat
    /// gray; >1 = punchier.
    contrast: f32,
    /// Crop fractions for left/right/top/bottom edges of the source
    /// UV. 0 = no crop; 0.1 left = drop the leftmost 10% of source.
    /// Sum on each axis must stay below 1.0 to leave something visible.
    crop_lrtb: [f32; 4],
    /// Per-sample scale applied to the raw YUV texture samples before the
    /// range expansion. 1.0 for 8-bit (R8Unorm, sample=code/255);
    /// `65535/1023` for 10-bit (R16Unorm, sample=code/65535 → code/1023,
    /// expanded with the 10-bit levels).
    sample_scale: f32,
    /// The frame's pixel aspect (`FrameColor::pixel_aspect`): power windows
    /// are shaped in display pixels.
    pixel_aspect: f32,
    _pad0: [f32; 2],
    /// Chroma key: `(r, g, b, enable)`. When `enable > 0.5`, pixels
    /// within `chroma_params.x` of the key color fade toward black
    /// (no alpha out yet — `vance-render` doesn't blend tracks in v1).
    chroma_key: [f32; 4],
    /// `(tolerance, softness, _, _)` — match-distance threshold and
    /// the falloff width.
    chroma_params: [f32; 4],
    /// Luma key: `(threshold, direction, softness, enable)` where
    /// `direction > 0` keeps pixels *above* threshold and < 0 keeps
    /// pixels below.
    luma_key: [f32; 4],
    /// Color-pipeline flags packed as a `vec4<u32>` in WGSL —
    /// `(input_transfer, output_transfer, input_range, output_range)`.
    /// Transfer / range IDs match `vance_color::pipeline`.
    pipeline_flags: [u32; 4],
    /// Linear-RGB matrix: input primaries → working primaries.
    /// Stored as 3 rows × 4 cols (4th col padding) so the WGSL side
    /// can read it as `array<vec4<f32>, 3>`. Identity when input
    /// primaries already match working primaries.
    pipeline_in_to_working: [[f32; 4]; 3],
    /// Linear-RGB matrix: working primaries → output primaries.
    pipeline_working_to_out: [[f32; 4]; 3],
    /// LUT params: `(enable, _, _, _)`. `enable > 0.5` runs the
    /// loaded 3D LUT against the just-decoded R'G'B' (still in the
    /// source-space gamma) before the linearize stage — matches
    /// how colorists author Rec.709 / sRGB LUTs.
    lut_params: [f32; 4],
    /// View-LUT params: `(enable, _, _, _)`. The *view* LUT is a
    /// monitoring transform — applies at the very end of the
    /// pipeline (post working→output matrix), but its contribution
    /// is meant for display only (export bypasses it). Independent
    /// of the per-clip LUT effect.
    view_lut_params: [f32; 4],
    /// `(YUV matrix, source kind, 10-bit R16Unorm planes, _)`: the matrix
    /// is `PipelineParams::input_matrix`, the kind is `SRC_*`.
    source: [u32; 4],
    /// Number of active color-grade nodes (Phase 6.6). The shader
    /// loops `grades[0..grade_count]` in linear working space. Packed
    /// into a vec4<u32> for std140 alignment; only `.x` is used.
    grade_count: [u32; 4],
    /// Serial chain of grade nodes. Applied in order, right where the
    /// brightness/contrast block runs (linear working space). A node
    /// is a no-op when its `flags.x` (enabled) is 0; `grade_count == 0`
    /// makes the whole loop a no-op, preserving pre-6.6 output exactly.
    grades: [GradeNode; MAX_GRADES],
}

/// Max grade nodes the single-pass renderer applies per frame. Mirror
/// of `vance_timeline::grade::MAX_GRADE_NODES`.
pub(crate) const MAX_GRADES: usize = 8;

/// Curve-atlas rows per grade node (Phase 6.8 + 6.8b). Row 0 = tone
/// curves (R/G/B/luma packed in RGBA); rows 1+2 = the five HSL hue-wheel
/// curves (hh/hs/hl/ss/ll). Atlas height = MAX_GRADES × this.
pub(crate) const CURVE_ROWS_PER_NODE: usize = 3;

/// One color-grade node's GPU params. All `vec4` fields (no bare f32
/// arrays) to dodge std140 stride traps — 9 × 16 = 144 bytes. The WGSL
/// `GradeNode` struct must match this layout byte-for-byte.
///
/// The last three vec4 carry the Phase 6.9 HSL qualifier (secondary
/// key); `qual_c.zw` are reserved for the Phase 6.10 power-window mask
/// (the final node mask is `qualifier_mask * window_mask`).
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct GradeNode {
    /// Lift (shadows) `(r, g, b, master)`. Additive; default 0.
    lift: [f32; 4],
    /// Gamma (midtones) `(r, g, b, master)`. Power; default 1.
    gamma: [f32; 4],
    /// Gain (highlights) `(r, g, b, master)`. Multiplicative; default 1.
    gain: [f32; 4],
    /// Log offset + shadow balance, packed `(off_r-ish)` — see shader.
    /// Layout: `(offset_r, offset_g, offset_b, _)`.
    log_offset: [f32; 4],
    /// Log shadow/mid/highlight balances, packed for the shader.
    /// Layout: `(shadow_y, mid_y, highlight_y, _)` luma weights —
    /// per-channel log balances ride `lift`/`gamma`/`gain` slots when
    /// `mode == log` (documented in the shader's `apply_log`).
    log_smh: [f32; 4],
    /// `(enabled, mode, qual_enable, qual_invert)`. `enabled`: 1 = apply,
    /// 0 = bypass. `mode`: 0 = primary, 1 = log, 2 = curves.
    /// `qual_enable`: 1 = restrict the node to the HSL qualifier mask.
    /// `qual_invert`: 1 = invert the mask.
    flags: [f32; 4],
    /// Qualifier `(hue_center, hue_range, sat_lo, sat_hi)` (6.9).
    qual_a: [f32; 4],
    /// Qualifier `(luma_lo, luma_hi, hue_soft, sat_soft)` (6.9).
    qual_b: [f32; 4],
    /// Qualifier `(luma_soft, show_mask, _, _)`. `show_mask`: 1 = solo
    /// this node's matte to the viewport. The `.z/.w` lanes are now
    /// PERMANENTLY-DEAD reserved padding — the 6.10 power window lives in
    /// its own `win_a`/`win_b` lanes below (kept contiguous + trailing).
    qual_c: [f32; 4],
    /// Power window `(shape, center_x, center_y, rotation_rad)` (6.10).
    /// `shape`: 0 = off (mask = 1, exact passthrough), 1 = ellipse,
    /// 2 = rectangle. Center is in source-UV `[0,1]`; rotation CCW.
    win_a: [f32; 4],
    /// Power window `(half_size_x, half_size_y, feather, invert)` (6.10).
    /// Half-extents/radii in source-UV units; `feather` softness band;
    /// `invert`: 1 = invert the window mask.
    win_b: [f32; 4],
}

const _: () = assert!(std::mem::size_of::<GradeNode>() == 176);
// Compile-time gate on the FULL uniform size so a field add/reorder
// that diverges from the WGSL std140 `Params` layout fails the build,
// not just `cargo test`. 288-byte header + 8 × 176-byte GradeNode.
const _: () = assert!(std::mem::size_of::<RenderParams>() == 1696);

impl GradeNode {
    /// Identity node: passthrough (disabled, primary mode, neutral
    /// lift/gamma/gain, qualifier off, window off).
    pub(crate) fn identity() -> Self {
        Self {
            lift: [0.0, 0.0, 0.0, 0.0],
            gamma: [1.0, 1.0, 1.0, 1.0],
            gain: [1.0, 1.0, 1.0, 1.0],
            log_offset: [0.0; 4],
            log_smh: [0.0; 4],
            flags: [0.0, 0.0, 0.0, 0.0],
            qual_a: [0.0; 4],
            qual_b: [0.0; 4],
            qual_c: [0.0; 4],
            // shape 0 = window off; sane center/size/feather defaults.
            win_a: [0.0, 0.5, 0.5, 0.0],
            win_b: [0.25, 0.25, 0.1, 0.0],
        }
    }
}

/// Which family of color ops a grade node applies. Maps to the
/// shader's `flags.y` (0 primary, 1 log, 2 curves).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GradeMode {
    /// Lift / gamma / gain primary wheels.
    Primary,
    /// Offset / shadow / midtone / highlight log wheels.
    Log,
    /// Per-channel tone curves (master + R/G/B + luma). Phase 6.8.
    Curves,
}

/// Curve control points for a `Curves`-mode grade node. Each is the
/// raw `Keyframe` list from the clip's `curve_*` param (x = input
/// level, y = output level); the renderer bakes them into the LUT
/// atlas inside `set_grades`. Empty lists bake to the identity ramp.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CurveSet {
    /// Master curve (folded into R/G/B at bake time).
    pub master: Vec<vance_timeline::Keyframe>,
    /// Red channel curve.
    pub r: Vec<vance_timeline::Keyframe>,
    /// Green channel curve.
    pub g: Vec<vance_timeline::Keyframe>,
    /// Blue channel curve.
    pub b: Vec<vance_timeline::Keyframe>,
    /// Luma curve (applied as a hue-preserving ratio).
    pub luma: Vec<vance_timeline::Keyframe>,
    // ---- HSL hue-wheel curves (Phase 6.8b) -------------------------------
    //
    // Five secondary curves operating in perceptual HSL. Empty = identity
    // (no-op). The bake interprets remap curves (hh/ss/ll) with empty=y=x
    // and multiplier curves (hs/hl) with empty=constant 0.5 (scale=1 in
    // the shader's `2*y` decoding).
    /// Hue→hue REMAP (cyclic input). y=x is identity.
    pub hh: Vec<vance_timeline::Keyframe>,
    /// Hue→saturation MULTIPLIER (cyclic input). y=0.5 is no-op (scale=1).
    pub hs: Vec<vance_timeline::Keyframe>,
    /// Hue→luminance MULTIPLIER (cyclic input). y=0.5 is no-op.
    pub hl: Vec<vance_timeline::Keyframe>,
    /// Saturation→saturation REMAP. y=x is identity.
    pub ss: Vec<vance_timeline::Keyframe>,
    /// Luminance→luminance REMAP. y=x is identity.
    pub ll: Vec<vance_timeline::Keyframe>,
}

impl CurveSet {
    /// True when the TONE curves (master/R/G/B/luma) are all empty or
    /// trivially the diagonal — the shader can skip the tone block.
    fn is_tone_identity(&self) -> bool {
        let id = |p: &[vance_timeline::Keyframe]| {
            p.is_empty()
                || (p.len() == 2
                    && (p[0].time_seconds - p[0].value).abs() < 1e-6
                    && (p[1].time_seconds - p[1].value).abs() < 1e-6
                    && p[0].time_seconds.abs() < 1e-6
                    && (p[1].time_seconds - 1.0).abs() < 1e-6)
        };
        id(&self.master) && id(&self.r) && id(&self.g) && id(&self.b) && id(&self.luma)
    }
    /// True when all 5 HSL curves are empty (the only cheap identity
    /// check — non-empty fields trigger the shader's HSL block even if
    /// they happen to be a diagonal).
    fn is_hsl_identity(&self) -> bool {
        self.hh.is_empty()
            && self.hs.is_empty()
            && self.hl.is_empty()
            && self.ss.is_empty()
            && self.ll.is_empty()
    }
    /// True when both the tone and HSL halves are identity.
    #[cfg(test)]
    fn is_identity(&self) -> bool {
        self.is_tone_identity() && self.is_hsl_identity()
    }
}

/// Public, `vance-color`-free description of one grade node the engine
/// hands to [`Renderer::set_grades`]. Mirror of the GPU `GradeNode`
/// but with named fields the engine fills from evaluated effect params.
#[derive(Clone, Debug, PartialEq)]
pub struct GradeNodeParams {
    /// Enabled (false = bypass).
    pub enabled: bool,
    /// Which op family this node applies.
    pub mode: GradeMode,
    /// Lift `(r, g, b, master)`.
    pub lift: [f32; 4],
    /// Gamma `(r, g, b, master)`.
    pub gamma: [f32; 4],
    /// Gain `(r, g, b, master)`.
    pub gain: [f32; 4],
    /// Log offset `(r, g, b)`.
    pub offset: [f32; 3],
    /// Log shadow balance `(r, g, b)`.
    pub shadow: [f32; 3],
    /// Log midtone balance `(r, g, b)`.
    pub midtone: [f32; 3],
    /// Log highlight balance `(r, g, b)`.
    pub highlight: [f32; 3],
    /// Curve control points (only meaningful when `mode == Curves`).
    pub curves: Option<CurveSet>,
    /// HSL qualifier (secondary key) — restricts the node to a hue/sat/
    /// luma range. Mode-INDEPENDENT (applies in primary/log/curves).
    /// Phase 6.9.
    pub qual: Qualifier,
    /// Power window (shape mask) — intersected with the qualifier mask.
    /// Phase 6.10.
    pub window: Window,
}

/// Power-window / shape-mask params (Phase 6.10). Restricts the node's
/// correction to an ellipse or rectangle in source-UV space, feathered
/// and optionally inverted. `shape == Off` is an exact passthrough. The
/// center is keyframe-animatable (the 6.11 point tracker drives it).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Window {
    /// 0 = off (mask = 1), 1 = ellipse, 2 = rectangle.
    pub shape: u8,
    /// Center `(x, y)` in source-UV `[0, 1]`.
    pub center: [f32; 2],
    /// Half-extents / radii `(x, y)` in source-UV units.
    pub size: [f32; 2],
    /// Rotation in radians (CCW about the center).
    pub rotation: f32,
    /// Feather / softness band width.
    pub feather: f32,
    /// Invert the mask.
    pub invert: bool,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            shape: 0,
            center: [0.5, 0.5],
            size: [0.25, 0.25],
            rotation: 0.0,
            feather: 0.1,
            invert: false,
        }
    }
}

/// HSL qualifier / secondary-key params (Phase 6.9). When `enabled`, the
/// node's correction is masked to pixels whose hue is within
/// `hue_center ± hue_range/2` AND whose saturation/luma fall in the
/// `[lo, hi]` bands, each feathered by its softness. Defaults are
/// WIDE-OPEN (range / hi = 1.0) so enabling + setting only a hue doesn't
/// accidentally key to nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Qualifier {
    /// Restrict the node to the mask.
    pub enabled: bool,
    /// Invert the mask.
    pub invert: bool,
    /// Solo this node's matte to the viewport (display-only).
    pub show_mask: bool,
    /// Hue `(center, range, softness)`, all in `[0, 1]` (cyclic center).
    pub hue: [f32; 3],
    /// Saturation `(lo, hi, softness)` in `[0, 1]`.
    pub sat: [f32; 3],
    /// Luma `(lo, hi, softness)` in `[0, 1]`.
    pub luma: [f32; 3],
}

impl Default for Qualifier {
    fn default() -> Self {
        Self {
            enabled: false,
            invert: false,
            show_mask: false,
            hue: [0.0, 1.0, 0.05],
            sat: [0.0, 1.0, 0.05],
            luma: [0.0, 1.0, 0.05],
        }
    }
}

impl Default for GradeNodeParams {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: GradeMode::Primary,
            lift: [0.0, 0.0, 0.0, 0.0],
            gamma: [1.0, 1.0, 1.0, 1.0],
            gain: [1.0, 1.0, 1.0, 1.0],
            offset: [0.0; 3],
            shadow: [0.0; 3],
            midtone: [0.0; 3],
            highlight: [0.0; 3],
            curves: None,
            qual: Qualifier::default(),
            window: Window::default(),
        }
    }
}

impl GradeNodeParams {
    /// Stamp the qualifier params into a (mode-specific) GradeNode. The
    /// qualifier is mode-independent, so every `to_gpu` arm runs this.
    fn write_qual(&self, mut n: GradeNode) -> GradeNode {
        let q = &self.qual;
        n.flags[2] = if q.enabled { 1.0 } else { 0.0 };
        n.flags[3] = if q.invert { 1.0 } else { 0.0 };
        n.qual_a = [q.hue[0], q.hue[1], q.sat[0], q.sat[1]];
        n.qual_b = [q.luma[0], q.luma[1], q.hue[2], q.sat[2]];
        n.qual_c = [q.luma[2], if q.show_mask { 1.0 } else { 0.0 }, 0.0, 0.0];
        n
    }

    /// Stamp the power-window params into a GradeNode (mode-independent,
    /// like `write_qual`). `shape` is clamped to `{0, 1, 2}`.
    fn write_window(&self, mut n: GradeNode) -> GradeNode {
        let w = &self.window;
        let shape = w.shape.min(2) as f32;
        n.win_a = [shape, w.center[0], w.center[1], w.rotation];
        n.win_b = [
            w.size[0],
            w.size[1],
            w.feather,
            if w.invert { 1.0 } else { 0.0 },
        ];
        n
    }

    pub(crate) fn to_gpu(&self) -> GradeNode {
        let enabled = if self.enabled { 1.0 } else { 0.0 };
        let base = match self.mode {
            GradeMode::Log => GradeNode {
                // In log mode the per-channel shadow/mid/highlight
                // balances ride the lift/gamma/gain vec4 slots (r,g,b);
                // log_offset carries the offset.
                lift: [self.shadow[0], self.shadow[1], self.shadow[2], 0.0],
                gamma: [self.midtone[0], self.midtone[1], self.midtone[2], 0.0],
                gain: [self.highlight[0], self.highlight[1], self.highlight[2], 0.0],
                log_offset: [self.offset[0], self.offset[1], self.offset[2], 0.0],
                log_smh: [0.0; 4],
                flags: [enabled, 1.0, 0.0, 0.0],
                ..GradeNode::identity()
            },
            GradeMode::Curves => {
                // Curve data lives in the atlas (row-block per node); the
                // GPU node only flags the mode + which CURVE FAMILIES are
                // non-identity so the shader can skip an idle block:
                //   log_smh.x = tone (R/G/B/luma) non-identity
                //   log_smh.y = HSL  (hh/hs/hl/ss/ll) non-identity (6.8b)
                let (tone_active, hsl_active) = self
                    .curves
                    .as_ref()
                    .map(|c| (!c.is_tone_identity(), !c.is_hsl_identity()))
                    .unwrap_or((false, false));
                GradeNode {
                    lift: [0.0; 4],
                    gamma: [1.0, 1.0, 1.0, 1.0],
                    gain: [1.0, 1.0, 1.0, 1.0],
                    log_offset: [0.0; 4],
                    log_smh: [
                        if tone_active { 1.0 } else { 0.0 },
                        if hsl_active { 1.0 } else { 0.0 },
                        0.0,
                        0.0,
                    ],
                    flags: [enabled, 2.0, 0.0, 0.0],
                    ..GradeNode::identity()
                }
            }
            GradeMode::Primary => GradeNode {
                lift: self.lift,
                gamma: self.gamma,
                gain: self.gain,
                log_offset: [0.0; 4],
                log_smh: [0.0; 4],
                flags: [enabled, 0.0, 0.0, 0.0],
                ..GradeNode::identity()
            },
        };
        self.write_window(self.write_qual(base))
    }
}

impl Default for RenderParams {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            rotation: 0.0,
            translate: [0.0; 2],
            scale: [1.0; 2],
            brightness: 0.0,
            contrast: 1.0,
            crop_lrtb: [0.0; 4],
            sample_scale: 1.0,
            pixel_aspect: 1.0,
            _pad0: [0.0; 2],
            chroma_key: [0.0; 4],
            chroma_params: [0.1, 0.05, 0.0, 0.0],
            luma_key: [0.5, 1.0, 0.05, 0.0],
            // Default = "Rec.709-limited → linear Rec.709 → sRGB"
            // (matches the unwritten Phase 1-5 assumption). Transfer
            // 2 = Rec709, 1 = sRGB; range 0 = Limited, 1 = Full.
            pipeline_flags: [2, 1, 0, 1],
            pipeline_in_to_working: identity_row3(),
            pipeline_working_to_out: identity_row3(),
            lut_params: [0.0; 4],
            view_lut_params: [0.0; 4],
            source: [vance_color::pipeline::MATRIX_BT709, SRC_PLANAR, 0, 0],
            grade_count: [0; 4],
            grades: [GradeNode::identity(); MAX_GRADES],
        }
    }
}

#[inline]
pub(crate) fn identity_row3() -> [[f32; 4]; 3] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
    ]
}

#[inline]
pub(crate) fn matrix_to_row3(t: &vance_color::Transform) -> [[f32; 4]; 3] {
    [
        [t.0[0][0], t.0[0][1], t.0[0][2], 0.0],
        [t.0[1][0], t.0[1][1], t.0[1][2], 0.0],
        [t.0[2][0], t.0[2][1], t.0[2][2], 0.0],
    ]
}

/// Pack a `f32` into IEEE-754 half-precision bits. Used for LUT
/// uploads (`Rgba16Float` texture format). Drops to ±inf / zero on
/// out-of-range inputs; that's fine for LUT values which are
/// normalized to ~`[0, 1]`.
#[inline]
pub(crate) fn f32_to_f16_bits(v: f32) -> u16 {
    let bits = v.to_bits();
    let sign = ((bits >> 31) & 0x1) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let mant = bits & 0x7fffff;
    if exp == 0xff {
        // NaN / Inf — keep the sign, zero the mantissa for inf, set
        // top mantissa bit for NaN.
        return (sign << 15) | 0x7c00 | if mant != 0 { 0x200 } else { 0 };
    }
    let new_exp = exp - 127 + 15;
    if new_exp <= 0 {
        // Underflow → flush to zero.
        return sign << 15;
    }
    if new_exp >= 0x1f {
        // Overflow → ±inf.
        return (sign << 15) | 0x7c00;
    }
    (sign << 15) | ((new_exp as u16) << 10) | ((mant >> 13) as u16)
}

/// Decode an IEEE 754 binary16 (half) bit pattern to `f32`. Inverse of
/// [`f32_to_f16_bits`], used to read back the HDR `Rgba16Float` target.
pub(crate) fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) & 0x1) as u32;
    let exp = ((h >> 10) & 0x1f) as u32;
    let mant = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign << 31 // ±0
        } else {
            // Subnormal → normalize into an f32 normal.
            let mut e = -1i32;
            let mut m = mant;
            loop {
                e += 1;
                m <<= 1;
                if m & 0x400 != 0 {
                    break;
                }
            }
            let f32_exp = (127 - 15 - e) as u32;
            (sign << 31) | (f32_exp << 23) | ((m & 0x3ff) << 13)
        }
    } else if exp == 0x1f {
        // Inf / NaN.
        (sign << 31) | 0x7f80_0000 | (mant << 13)
    } else {
        let f32_exp = (exp as i32 - 15 + 127) as u32;
        (sign << 31) | (f32_exp << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

/// Evaluate a curve's control points at input `x`, returning the
/// IDENTITY (`y = x`) when the curve is empty — `eval_curve` returns
/// 0.0 for an empty list (the keyframe convention), which would crush
/// the channel to black. Output is clamped to the LUT's `[0, 1]`
/// domain.
/// Evaluate a curve at `x`, returning a per-curve-type IDENTITY value
/// when the list is empty: pass `default = x` for REMAP curves
/// (y = x is identity, e.g. hh / ss / ll / tone) and `default = 0.5` for
/// MULTIPLIER curves (the shader decodes as `2*y` so 0.5 = scale 1,
/// e.g. hs / hl). Phase 6.8b.
#[inline]
fn eval_curve_or_const(points: &[vance_timeline::Keyframe], x: f64, default: f64) -> f64 {
    if points.is_empty() {
        return default.clamp(0.0, 1.0);
    }
    vance_timeline::eval_curve(points, x).clamp(0.0, 1.0)
}

#[inline]
fn eval_curve_or_identity(points: &[vance_timeline::Keyframe], x: f64) -> f64 {
    if points.is_empty() {
        return x.clamp(0.0, 1.0);
    }
    vance_timeline::eval_curve(points, x).clamp(0.0, 1.0)
}

/// The transform's scale with near-zero axes pinned to `1e-4` (the shader
/// divides by it).
pub(crate) fn sanitize_scale(scale: [f32; 2]) -> [f32; 2] {
    scale.map(|v| if v.abs() < 1e-4 { 1e-4 } else { v })
}

/// Crop fractions `(left, right, top, bottom)` clamped to `[0, 1]`, with
/// each axis shrunk symmetrically to leave a 1% sliver — fully empty crops
/// are an easy oops, not an art choice.
pub(crate) fn sanitize_crop(lrtb: [f32; 4]) -> [f32; 4] {
    let [mut l, mut r, mut t, mut b] = lrtb.map(|v| v.clamp(0.0, 1.0));
    if l + r >= 0.99 {
        let extra = (l + r) - 0.99;
        l -= extra * 0.5;
        r -= extra * 0.5;
    }
    if t + b >= 0.99 {
        let extra = (t + b) - 0.99;
        t -= extra * 0.5;
        b -= extra * 0.5;
    }
    [l, r, t, b]
}

/// Clean a curve's control points before baking. The interactive editor
/// guarantees sorted, finite, in-range points, but a project loaded from
/// disk or authored by an external tool / CLI does NOT — and
/// `eval_keyframes` assumes ascending order. So we:
///   * drop points with a non-finite (NaN/inf) input or output — those
///     would pack to a NaN/inf atlas texel and corrupt every pixel;
///   * clamp the OUTPUT to `[0, 1]` (the displayable / LUT range), making
///     the otherwise-silent bake-time clamp explicit at the data edge;
///   * sort ascending by input level so the segment search is correct.
///
/// The input level is left unclamped: a control point past `x = 1` is
/// simply never reached by the `[0, 1]` sampling (eval clamps at the
/// endpoints), so clamping it would only distort the last segment.
fn sanitize_curve(points: &[vance_timeline::Keyframe]) -> Vec<vance_timeline::Keyframe> {
    let mut out: Vec<vance_timeline::Keyframe> = points
        .iter()
        .filter(|k| k.time_seconds.is_finite() && k.value.is_finite())
        .map(|k| vance_timeline::Keyframe {
            time_seconds: k.time_seconds,
            value: k.value.clamp(0.0, 1.0),
            interp: k.interp,
        })
        .collect();
    out.sort_by(|a, b| {
        a.time_seconds
            .partial_cmp(&b.time_seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// Bake the per-node tone curves into the 256×(8×3) LUT atlas. Rows for
/// non-curve / identity nodes get the identity ramp (harmless — only
/// curves-mode nodes sample the atlas). The master curve is composited into
/// R/G/B here so the shader does a single sample per channel. Shared by the
/// legacy renderer and the compositor's per-layer atlases.
pub(crate) fn bake_curve_atlas(curve_inputs: &[(usize, CurveSet)]) -> Vec<f32> {
    // 256 wide. The atlas grew in 6.8b to host the HSL hue-wheel curves:
    // MAX_GRADES × CURVE_ROWS_PER_NODE rows × w cols × 4 channels. Per node:
    //   row 3*i     = tone curves   (r=R, g=G, b=B, a=luma) — 6.8.
    //   row 3*i+1   = HSL-1         (r=hh, g=hs, b=hl, a=ss) — 6.8b.
    //   row 3*i+2   = HSL-2         (r=ll, g=_, b=_, a=_)    — 6.8b.
    let w = vance_timeline::grade::CURVE_LUT_WIDTH;
    let h = MAX_GRADES * CURVE_ROWS_PER_NODE;
    let mut atlas = vec![0.0_f32; h * w * 4];
    // Default-fill identity per row TYPE: tone + remap curves are
    // identity y=x; HSL multipliers (hs, hl) are stored y=0.5 (the
    // shader decodes as 2*y, so 0.5 == scale 1.0 == no-op).
    for row in 0..h {
        let kind = row % CURVE_ROWS_PER_NODE;
        for u in 0..w {
            let x = u as f32 / (w as f32 - 1.0);
            let off = (row * w + u) * 4;
            match kind {
                0 => {
                    // Tone row: r/g/b/luma all y=x.
                    atlas[off] = x;
                    atlas[off + 1] = x;
                    atlas[off + 2] = x;
                    atlas[off + 3] = x;
                }
                1 => {
                    // HSL-1 identity: hh=y=x (remap), hs=hl=0.5 (the
                    // shader decodes multipliers as 2*y, so 0.5 == no-op
                    // scale 1.0; 1.0 here would silently double them),
                    // ss=y=x (remap).
                    atlas[off] = x;
                    atlas[off + 1] = 0.5;
                    atlas[off + 2] = 0.5;
                    atlas[off + 3] = x;
                }
                _ => {
                    // HSL-2: ll=y=x, rest unused (left 0).
                    atlas[off] = x;
                }
            }
        }
    }
    // Bake each curves node's row.
    for (row, cs) in curve_inputs {
        if *row >= MAX_GRADES {
            continue;
        }
        // Sanitize ONCE per channel before the per-sample loop:
        // eval_keyframes assumes ascending, finite control points,
        // but project files / external tools aren't guaranteed to be
        // sorted, finite, or in-range (only the interactive editor
        // is). An unsorted curve would otherwise read garbage; a NaN
        // point would pack to a NaN texel and corrupt every pixel.
        let m_pts = sanitize_curve(&cs.master);
        let r_pts = sanitize_curve(&cs.r);
        let g_pts = sanitize_curve(&cs.g);
        let b_pts = sanitize_curve(&cs.b);
        let l_pts = sanitize_curve(&cs.luma);
        // HSL curves (6.8b). Each Vec can be empty (identity).
        let hh_pts = sanitize_curve(&cs.hh);
        let hs_pts = sanitize_curve(&cs.hs);
        let hl_pts = sanitize_curve(&cs.hl);
        let ss_pts = sanitize_curve(&cs.ss);
        let ll_pts = sanitize_curve(&cs.ll);
        for u in 0..w {
            let x = u as f64 / (w as f64 - 1.0);
            let m = eval_curve_or_identity(&m_pts, x);
            let r = eval_curve_or_identity(&r_pts, m);
            let g = eval_curve_or_identity(&g_pts, m);
            let b = eval_curve_or_identity(&b_pts, m);
            let l = eval_curve_or_identity(&l_pts, x);
            // Tone row (3*i) = R/G/B/luma packed into RGBA.
            let row_tone = ((*row * CURVE_ROWS_PER_NODE) * w + u) * 4;
            atlas[row_tone] = r as f32;
            atlas[row_tone + 1] = g as f32;
            atlas[row_tone + 2] = b as f32;
            atlas[row_tone + 3] = l as f32;
            // HSL-1 row (3*i+1) = (hh, hs, hl, ss). Empty curves bake
            // to their type's identity (remap → x, multiplier → 0.5,
            // decoded as 2*y in the shader so 0.5 → scale 1).
            let hh = eval_curve_or_const(&hh_pts, x, x);
            let hs = eval_curve_or_const(&hs_pts, x, 0.5);
            let hl = eval_curve_or_const(&hl_pts, x, 0.5);
            let ss = eval_curve_or_const(&ss_pts, x, x);
            let row_hsl1 = ((*row * CURVE_ROWS_PER_NODE + 1) * w + u) * 4;
            atlas[row_hsl1] = hh as f32;
            atlas[row_hsl1 + 1] = hs as f32;
            atlas[row_hsl1 + 2] = hl as f32;
            atlas[row_hsl1 + 3] = ss as f32;
            // HSL-2 row (3*i+2) = (ll, _, _, _). Unused lanes 0.
            let ll = eval_curve_or_const(&ll_pts, x, x);
            let row_hsl2 = ((*row * CURVE_ROWS_PER_NODE + 2) * w + u) * 4;
            atlas[row_hsl2] = ll as f32;
            atlas[row_hsl2 + 1] = 0.0;
            atlas[row_hsl2 + 2] = 0.0;
            atlas[row_hsl2 + 3] = 0.0;
        }
    }
    atlas
}

/// The curve inputs of `nodes` (node index → curves), for the atlas bake and
/// its change gate.
pub(crate) fn curve_inputs_of(nodes: &[GradeNodeParams]) -> Vec<(usize, CurveSet)> {
    nodes
        .iter()
        .take(MAX_GRADES)
        .enumerate()
        .filter(|(_, n)| n.mode == GradeMode::Curves)
        .filter_map(|(i, n)| n.curves.clone().map(|c| (i, c)))
        .collect()
}

/// Pack a 3D LUT as `Rgba16Float` texels (alpha 1) for upload.
pub(crate) fn pack_lut_rgba16f(lut: &vance_color::Lut3d) -> Vec<u16> {
    let n = lut.size as usize;
    let mut packed: Vec<u16> = Vec::with_capacity(n * n * n * 4);
    for triple in lut.values.chunks_exact(3) {
        packed.push(f32_to_f16_bits(triple[0]));
        packed.push(f32_to_f16_bits(triple[1]));
        packed.push(f32_to_f16_bits(triple[2]));
        packed.push(f32_to_f16_bits(1.0));
    }
    packed
}

#[inline]
fn grade_nodes_eq(a: &GradeNode, b: &GradeNode) -> bool {
    let eq4 = |x: &[f32; 4], y: &[f32; 4]| (0..4).all(|i| (x[i] - y[i]).abs() < 1e-6);
    eq4(&a.lift, &b.lift)
        && eq4(&a.gamma, &b.gamma)
        && eq4(&a.gain, &b.gain)
        && eq4(&a.log_offset, &b.log_offset)
        && eq4(&a.log_smh, &b.log_smh)
        && eq4(&a.flags, &b.flags)
        // Qualifier lanes (6.9) — MUST be compared or a hue/sat/luma
        // slider change is wrongly judged "unchanged" and the stale mask
        // stays on the GPU.
        && eq4(&a.qual_a, &b.qual_a)
        && eq4(&a.qual_b, &b.qual_b)
        && eq4(&a.qual_c, &b.qual_c)
        // Power-window lanes (6.10) — MUST be compared or an animated /
        // tracked window freezes on the GPU (same trap as the qualifier).
        && eq4(&a.win_a, &b.win_a)
        && eq4(&a.win_b, &b.win_b)
}

#[inline]
fn matrices_equal(a: &[[f32; 4]; 3], b: &[[f32; 4]; 3]) -> bool {
    for r in 0..3 {
        for c in 0..3 {
            if (a[r][c] - b[r][c]).abs() > 1e-6 {
                return false;
            }
        }
    }
    true
}

impl Renderer {
    /// Create a renderer. Picks a wgpu adapter without an associated surface
    /// (the renderer is surface-agnostic; the *consumer* hands it a target).
    ///
    /// Use [`Renderer::new_with`] when you already have an `Instance` and a
    /// surface to be compatible with — that's the path the production app
    /// takes so the viewport's Surface and the renderer's Device share an
    /// adapter.
    pub fn new(cfg: RendererConfig) -> Result<Self, RenderError> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = cfg.backends;
        let instance = wgpu::Instance::new(desc);
        let (adapter, device, queue) = pollster::block_on(create_device(&instance, None))?;
        Self::from_device_queue(adapter, device, queue, cfg)
    }

    /// Create a renderer using an existing `Instance` and a `Surface` it
    /// must be compatible with. The chosen adapter will satisfy `surface`'s
    /// requirements; the device + queue are picked from that adapter.
    pub fn new_with(
        instance: &wgpu::Instance,
        compatible_surface: &wgpu::Surface<'static>,
        cfg: RendererConfig,
    ) -> Result<Self, RenderError> {
        let (adapter, device, queue) =
            pollster::block_on(create_device(instance, Some(compatible_surface)))?;
        Self::from_device_queue(adapter, device, queue, cfg)
    }

    fn from_device_queue(
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
        cfg: RendererConfig,
    ) -> Result<Self, RenderError> {
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vance-render.yuv-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                yuv_plane_entry(1),
                yuv_plane_entry(2),
                yuv_plane_entry(3),
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 8,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D3,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 9,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D3,
                        multisampled: false,
                    },
                    count: None,
                },
                // binding 10: curve LUT atlas (Phase 6.8), a 2D texture.
                wgpu::BindGroupLayoutEntry {
                    binding: 10,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vance-render.pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            ..Default::default()
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vance-render.yuv-rec709-limited"),
            source: wgpu::ShaderSource::Wgsl(wgsl::LEGACY.into()),
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vance-render.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: cfg.target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vance-render.sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let initial_params = RenderParams::default();
        let params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vance-render.params"),
            size: std::mem::size_of::<RenderParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        queue.write_buffer(&params_buffer, 0, bytemuck::cast_slice(&[initial_params]));

        // Capture before `device` moves into the struct below.
        let supports_r16 = device
            .features()
            .contains(wgpu::Features::TEXTURE_FORMAT_16BIT_NORM);
        // DX12's FXC rejects a workgroup leaving before a barrier; the other
        // backends run the matte passes with an early exit.
        let barrier_safe = adapter.get_info().backend == wgpu::Backend::Dx12;
        let compositor = crate::compositor::Compositor::new(
            device.clone(),
            queue.clone(),
            supports_r16,
            barrier_safe,
        );

        Ok(Self {
            adapter,
            device,
            queue,
            pipeline,
            target_format: cfg.target_format,
            bind_group_layout,
            sampler,
            supports_r16,
            tex_y: None,
            tex_u: None,
            tex_v: None,
            tex_lut: None,
            lut_size: 1,
            tex_view_lut: None,
            view_lut_size: 1,
            tex_curve_atlas: None,
            curve_atlas_width: 1,
            last_curve_inputs: Vec::new(),
            bind_group: None,
            params_buffer,
            current_params: initial_params,
            blit: None,
            compositor,
        })
    }

    /// The multi-layer compositor, sharing this renderer's device (ADR 0007).
    pub fn compositor(&mut self) -> &mut crate::compositor::Compositor {
        &mut self.compositor
    }

    /// Phase 10 — blit a CPU-side RGBA8 (sRGB) buffer to `target_view`
    /// with a fullscreen pass. Used by the live preview to display the
    /// composited frame after CPU WASM effects have run on it. `rgba`
    /// is row-major `width * height * 4` bytes, sRGB-encoded (the same
    /// shape `render_to_rgba_buffer` produces + WASM plugins mutate).
    /// `target_format` must match `target_view`'s texture format.
    pub fn present_rgba(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
        target_view: &wgpu::TextureView,
        target_format: wgpu::TextureFormat,
    ) -> Result<(), RenderError> {
        if width == 0 || height == 0 || rgba.len() != (width as usize) * (height as usize) * 4 {
            return Err(RenderError::DeviceRequest(format!(
                "present_rgba bad buffer: {} bytes for {width}x{height}",
                rgba.len()
            )));
        }
        // (Re)build the blit pipeline if absent or the target format
        // changed (e.g. surface vs offscreen test target).
        let needs_build = match &self.blit {
            Some(b) => b.format != target_format,
            None => true,
        };
        if needs_build {
            self.blit = Some(self.build_blit(target_format));
        }
        // Ensure the source texture matches the frame size.
        {
            let blit = self.blit.as_mut().unwrap();
            if blit.texture.is_none() || blit.tex_dims != (width, height) {
                blit.texture = Some(self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("vance-render.blit-src"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                }));
                blit.tex_dims = (width, height);
            }
        }
        let blit = self.blit.as_ref().unwrap();
        let texture = blit.texture.as_ref().unwrap();
        // Upload the RGBA bytes.
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vance-render.blit-bg"),
            layout: &blit.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&blit.sampler),
                },
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vance-render.blit-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vance-render.blit-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&blit.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
        Ok(())
    }

    fn build_blit(&self, target_format: wgpu::TextureFormat) -> BlitResources {
        let bind_group_layout =
            self.device
                .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("vance-render.blit-bgl"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                            count: None,
                        },
                    ],
                });
        let pipeline_layout = self
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("vance-render.blit-layout"),
                bind_group_layouts: &[Some(&bind_group_layout)],
                ..Default::default()
            });
        let shader = self
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("vance-render.blit-shader"),
                source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
            });
        let pipeline = self
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("vance-render.blit-pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_main"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    ..Default::default()
                },
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        let sampler = self.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vance-render.blit-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        BlitResources {
            pipeline,
            bind_group_layout,
            sampler,
            format: target_format,
            texture: None,
            tex_dims: (0, 0),
        }
    }

    /// Set the per-frame opacity uniform. `1.0` = passthrough,
    /// `0.0` = fade to black. The new value is uploaded immediately
    /// and applies to the next `render_to` call.
    pub fn set_opacity(&mut self, value: f32) {
        let v = value.clamp(0.0, 1.0);
        if (self.current_params.opacity - v).abs() < 1e-4 {
            return;
        }
        self.current_params.opacity = v;
        self.queue.write_buffer(
            &self.params_buffer,
            0,
            bytemuck::cast_slice(&[self.current_params]),
        );
    }

    /// Set per-frame brightness/contrast uniforms. `brightness = 0`
    /// and `contrast = 1` is passthrough. Contrast is applied around
    /// 0.5 (gamma-encoded midpoint) before brightness is added; the
    /// shader clamps the result to [0, 1].
    pub fn set_brightness_contrast(&mut self, brightness: f32, contrast: f32) {
        let b = brightness.clamp(-1.0, 1.0);
        let c = contrast.clamp(0.0, 4.0);
        if (self.current_params.brightness - b).abs() < 1e-5
            && (self.current_params.contrast - c).abs() < 1e-5
        {
            return;
        }
        self.current_params.brightness = b;
        self.current_params.contrast = c;
        self.queue.write_buffer(
            &self.params_buffer,
            0,
            bytemuck::cast_slice(&[self.current_params]),
        );
    }

    /// Set per-frame crop uniforms. Each value is a fraction
    /// `[0, 1)` of the source frame to chop off that edge; e.g.
    /// `set_crop(0.1, 0, 0, 0)` drops the leftmost 10% of the source.
    /// Sums above ~0.99 on an axis are clamped to leave a 1% sliver
    /// visible — fully empty crops are an easy oops, not an art choice.
    pub fn set_crop(&mut self, left: f32, right: f32, top: f32, bottom: f32) {
        let new = sanitize_crop([left, right, top, bottom]);
        if (self.current_params.crop_lrtb[0] - new[0]).abs() < 1e-5
            && (self.current_params.crop_lrtb[1] - new[1]).abs() < 1e-5
            && (self.current_params.crop_lrtb[2] - new[2]).abs() < 1e-5
            && (self.current_params.crop_lrtb[3] - new[3]).abs() < 1e-5
        {
            return;
        }
        self.current_params.crop_lrtb = new;
        self.queue.write_buffer(
            &self.params_buffer,
            0,
            bytemuck::cast_slice(&[self.current_params]),
        );
    }

    /// Set the per-frame 2D transform uniforms. All defaults are the
    /// identity: `rotation = 0`, `translate = (0, 0)`,
    /// `scale = (1, 1)`. The shader applies the inverse transform to
    /// the output UV before sampling the source frame; out-of-frame
    /// regions render black.
    pub fn set_transform(&mut self, translate: [f32; 2], scale: [f32; 2], rotation_radians: f32) {
        let new_scale = sanitize_scale(scale);
        if (self.current_params.rotation - rotation_radians).abs() < 1e-5
            && (self.current_params.translate[0] - translate[0]).abs() < 1e-5
            && (self.current_params.translate[1] - translate[1]).abs() < 1e-5
            && (self.current_params.scale[0] - new_scale[0]).abs() < 1e-5
            && (self.current_params.scale[1] - new_scale[1]).abs() < 1e-5
        {
            return;
        }
        self.current_params.rotation = rotation_radians;
        self.current_params.translate = translate;
        self.current_params.scale = new_scale;
        self.queue.write_buffer(
            &self.params_buffer,
            0,
            bytemuck::cast_slice(&[self.current_params]),
        );
    }

    /// Reference to the underlying wgpu device. Exposed for callers that
    /// need to allocate target textures with the same device.
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// Reference to the underlying wgpu queue.
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// Upload a decoded frame into the renderer's plane textures: YUV
    /// planes as they are (NV12's interleaved UV as one two-channel
    /// texture), or a straight-alpha RGBA8 frame (shown premultiplied over
    /// black).
    pub fn upload_frame(&mut self, frame: &FrameData) -> Result<(), RenderError> {
        let expected = frame.pixel_format.frame_size(frame.width, frame.height);
        if frame.bytes.len() != expected {
            return Err(RenderError::MalformedFrame {
                expected,
                got: frame.bytes.len(),
            });
        }
        let fp = frame_planes(frame, self.supports_r16);
        let slots = [&mut self.tex_y, &mut self.tex_u, &mut self.tex_v];
        let labels = ["tex_y", "tex_u", "tex_v"];
        let mut realloc = false;
        for ((slot, plane), label) in slots.into_iter().zip(&fp.planes).zip(labels) {
            // An unused plane binds a 1×1 dummy.
            let (size, format) = plane
                .as_ref()
                .map_or(((1, 1), wgpu::TextureFormat::R8Unorm), |p| {
                    (p.size, p.format)
                });
            realloc |= ensure_plane_texture(&self.device, slot, size, format, label);
            if let (Some(p), Some(t)) = (plane, slot.as_ref()) {
                write_plane(&self.queue, t, p);
            }
        }
        // Tell the shader how to read the frame: an R16Unorm 10-bit sample
        // is code/65535, so ×65535/1023 gives code/1023.
        let mut next = self.current_params;
        next.sample_scale = fp.sample_scale;
        next.source[1] = fp.kind;
        next.source[2] = fp.ten_bit as u32;
        next.pixel_aspect = frame.color.pixel_aspect();
        if bytemuck::bytes_of(&next) != bytemuck::bytes_of(&self.current_params) {
            self.current_params = next;
            self.queue.write_buffer(
                &self.params_buffer,
                0,
                bytemuck::cast_slice(&[self.current_params]),
            );
        }
        if realloc {
            self.rebuild_bind_group();
        }
        Ok(())
    }

    /// Upload a 3D LUT into the renderer's `tex_lut` slot. Allocates
    /// a fresh 3D texture when the size differs from the previous
    /// upload. Pass via `enable = 1.0` to `set_lut_enable` to switch
    /// it on; the renderer leaves the LUT bound but inactive when
    /// the enable bit is 0, so callers can pre-load and toggle.
    pub fn upload_lut(&mut self, lut: &vance_color::Lut3d) {
        let size = lut.size;
        if self.tex_lut.is_none() || self.lut_size != size {
            self.tex_lut = Some(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vance-render.tex_lut"),
                size: wgpu::Extent3d {
                    width: size,
                    height: size,
                    depth_or_array_layers: size,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            self.lut_size = size;
            self.rebuild_bind_group();
        }
        // Convert RGB triples → RGBA half (alpha = 1) for upload.
        let packed = pack_lut_rgba16f(lut);
        let bytes: &[u8] = bytemuck::cast_slice(&packed);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: self.tex_lut.as_ref().unwrap(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size * 4 * 2),
                rows_per_image: Some(size),
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: size,
            },
        );
    }

    /// Toggle the LUT effect's enable flag. `enable = 0` is a
    /// passthrough (the bound LUT texture is sampled but its
    /// contribution mixed at zero weight). `enable = 1` applies
    /// the LUT in full strength.
    pub fn set_lut_enable(&mut self, enable: f32) {
        let v = enable.clamp(0.0, 1.0);
        if (self.current_params.lut_params[0] - v).abs() < 1e-5 {
            return;
        }
        self.current_params.lut_params[0] = v;
        self.queue.write_buffer(
            &self.params_buffer,
            0,
            bytemuck::cast_slice(&[self.current_params]),
        );
    }

    /// Upload a 3D LUT into the *view* slot (Phase 6.5). Same shape
    /// as `upload_lut` but the renderer routes this one to the
    /// post-output sampling stage — a display LUT, not a clip
    /// effect.
    pub fn upload_view_lut(&mut self, lut: &vance_color::Lut3d) {
        let size = lut.size;
        if self.tex_view_lut.is_none() || self.view_lut_size != size {
            self.tex_view_lut = Some(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vance-render.tex_view_lut"),
                size: wgpu::Extent3d {
                    width: size,
                    height: size,
                    depth_or_array_layers: size,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            self.view_lut_size = size;
            self.rebuild_bind_group();
        }
        let packed = pack_lut_rgba16f(lut);
        let bytes: &[u8] = bytemuck::cast_slice(&packed);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: self.tex_view_lut.as_ref().unwrap(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(size * 4 * 2),
                rows_per_image: Some(size),
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: size,
            },
        );
        self.sync_compositor_view_lut();
    }

    /// Set the serial color-grade node chain (Phase 6.6/6.7/6.8).
    /// Nodes apply in order in linear working space, where the
    /// brightness/contrast block already runs. Anything past
    /// `MAX_GRADES` is dropped (the UI warns). Two idempotency gates:
    /// the uniform write skips when the GPU node array is unchanged,
    /// and the (expensive) curve-atlas re-bake skips when no curve
    /// inputs changed. Baking lives here so every push site gets
    /// curves for free.
    pub fn set_grades(&mut self, nodes: &[GradeNodeParams]) {
        let count = nodes.len().min(MAX_GRADES);
        let mut next = [GradeNode::identity(); MAX_GRADES];
        for (i, n) in nodes.iter().take(count).enumerate() {
            next[i] = n.to_gpu();
        }
        let same_count = self.current_params.grade_count[0] as usize == count;
        let grades_unchanged = same_count
            && (0..count).all(|i| grade_nodes_eq(&self.current_params.grades[i], &next[i]));

        // Collect this call's curve inputs (node index → CurveSet) to
        // (a) gate the atlas re-bake and (b) bake the atlas rows.
        let curve_inputs = curve_inputs_of(nodes);
        let curves_unchanged = curve_inputs == self.last_curve_inputs;

        if grades_unchanged && curves_unchanged {
            return;
        }
        if !grades_unchanged {
            self.current_params.grade_count = [count as u32, 0, 0, 0];
            self.current_params.grades = next;
            self.queue.write_buffer(
                &self.params_buffer,
                0,
                bytemuck::cast_slice(&[self.current_params]),
            );
        }
        if !curves_unchanged {
            self.bake_and_upload_curve_atlas(&curve_inputs);
            self.last_curve_inputs = curve_inputs;
        }
    }

    /// Bake the per-node curves ([`bake_curve_atlas`]) and upload them.
    fn bake_and_upload_curve_atlas(&mut self, curve_inputs: &[(usize, CurveSet)]) {
        let atlas = bake_curve_atlas(curve_inputs);
        self.upload_curve_atlas(&atlas, vance_timeline::grade::CURVE_LUT_WIDTH as u32);
    }

    /// Upload the baked curve atlas (`rows = MAX_GRADES × CURVE_ROWS_PER_NODE`, `width`
    /// columns, 4 f32 channels) to the GPU as an Rgba16Float 2D
    /// texture. Lazily allocates on first call (then rebuilds the bind
    /// group). Mirrors the `upload_lut` packing path.
    fn upload_curve_atlas(&mut self, atlas: &[f32], width: u32) {
        let height = (MAX_GRADES * CURVE_ROWS_PER_NODE) as u32;
        // Replace the texture when absent OR still the 1×1 dummy (or any
        // stale width) — otherwise write_texture would overrun it.
        if self.tex_curve_atlas.is_none() || self.curve_atlas_width != width {
            self.tex_curve_atlas = Some(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vance-render.tex_curve_atlas"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            self.curve_atlas_width = width;
            self.rebuild_bind_group();
        }
        let packed: Vec<u16> = atlas.iter().map(|v| f32_to_f16_bits(*v)).collect();
        let bytes: &[u8] = bytemuck::cast_slice(&packed);
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: self.tex_curve_atlas.as_ref().unwrap(),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4 * 2),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Toggle the view LUT.
    pub fn set_view_lut_enable(&mut self, enable: f32) {
        let v = enable.clamp(0.0, 1.0);
        if (self.current_params.view_lut_params[0] - v).abs() < 1e-5 {
            return;
        }
        self.current_params.view_lut_params[0] = v;
        self.queue.write_buffer(
            &self.params_buffer,
            0,
            bytemuck::cast_slice(&[self.current_params]),
        );
        self.sync_compositor_view_lut();
    }

    /// Share the view LUT with the compositor: the loaded texture while the
    /// view LUT is enabled, else none (`OutputSpec::view_lut` then no-ops).
    fn sync_compositor_view_lut(&mut self) {
        let lut = if self.current_params.view_lut_params[0] > 0.5 && self.view_lut_size > 1 {
            self.tex_view_lut.clone()
        } else {
            None
        };
        self.compositor.set_view_lut(lut);
    }

    /// Push the color-pipeline params (Phase 6.3). These come from
    /// `vance_color::PipelineParams::compute()` and drive the
    /// shader's linearize → primary-change → encode chain. If the
    /// caller doesn't update them, the renderer keeps using whatever
    /// was last set (defaulting to Rec.709 limited → linear Rec.709
    /// → sRGB, which matches the pre-Phase-6 behavior).
    pub fn set_pipeline(&mut self, params: vance_color::PipelineParams) {
        let new_flags = [
            params.input_transfer,
            params.output_transfer,
            params.input_range,
            params.output_range,
        ];
        let in_to_working = matrix_to_row3(&params.input_to_working);
        let working_to_out = matrix_to_row3(&params.working_to_output);
        let same_flags = (0..4).all(|i| self.current_params.pipeline_flags[i] == new_flags[i])
            && self.current_params.source[0] == params.input_matrix;
        let same_in = matrices_equal(&self.current_params.pipeline_in_to_working, &in_to_working);
        let same_out = matrices_equal(
            &self.current_params.pipeline_working_to_out,
            &working_to_out,
        );
        if same_flags && same_in && same_out {
            return;
        }
        self.current_params.pipeline_flags = new_flags;
        self.current_params.source[0] = params.input_matrix;
        self.current_params.pipeline_in_to_working = in_to_working;
        self.current_params.pipeline_working_to_out = working_to_out;
        self.queue.write_buffer(
            &self.params_buffer,
            0,
            bytemuck::cast_slice(&[self.current_params]),
        );
    }

    /// Set the chroma-key params. `enable = 0.0` turns the effect off
    /// (the shader passes pixels through); `> 0.5` enables matching.
    /// `tolerance` is the maximum RGB distance for a "fully keyed"
    /// match; `softness` widens the falloff so the edge doesn't pop.
    pub fn set_chroma_key(
        &mut self,
        key_rgb: [f32; 3],
        tolerance: f32,
        softness: f32,
        enable: f32,
    ) {
        let new_key = [
            key_rgb[0].clamp(0.0, 1.0),
            key_rgb[1].clamp(0.0, 1.0),
            key_rgb[2].clamp(0.0, 1.0),
            enable.clamp(0.0, 1.0),
        ];
        let new_params = [
            tolerance.clamp(0.0, 2.0),
            softness.clamp(0.0, 2.0),
            0.0,
            0.0,
        ];
        let same_key =
            (0..4).all(|i| (self.current_params.chroma_key[i] - new_key[i]).abs() < 1e-5);
        let same_params =
            (0..2).all(|i| (self.current_params.chroma_params[i] - new_params[i]).abs() < 1e-5);
        if same_key && same_params {
            return;
        }
        self.current_params.chroma_key = new_key;
        self.current_params.chroma_params = new_params;
        self.queue.write_buffer(
            &self.params_buffer,
            0,
            bytemuck::cast_slice(&[self.current_params]),
        );
    }

    /// Set the luma-key params. `direction > 0` keeps pixels brighter
    /// than `threshold`; `< 0` keeps pixels darker. `softness` widens
    /// the falloff. `enable = 0.0` is a passthrough.
    pub fn set_luma_key(&mut self, threshold: f32, direction: f32, softness: f32, enable: f32) {
        let dir = if direction >= 0.0 { 1.0 } else { -1.0 };
        let new = [
            threshold.clamp(0.0, 1.0),
            dir,
            softness.clamp(0.0, 1.0),
            enable.clamp(0.0, 1.0),
        ];
        if (0..4).all(|i| (self.current_params.luma_key[i] - new[i]).abs() < 1e-5) {
            return;
        }
        self.current_params.luma_key = new;
        self.queue.write_buffer(
            &self.params_buffer,
            0,
            bytemuck::cast_slice(&[self.current_params]),
        );
    }

    /// Pick a sensible swapchain configuration for `surface` at the given
    /// pixel size. Chooses the first sRGB-capable format the surface
    /// supports — caller must pass that same format as `target_format`
    /// when constructing the renderer.
    pub fn recommended_surface_config(
        &self,
        surface: &wgpu::Surface<'static>,
        width: u32,
        height: u32,
    ) -> Option<wgpu::SurfaceConfiguration> {
        let caps = surface.get_capabilities(&self.adapter);
        if caps.formats.is_empty() {
            return None;
        }
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        Some(wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: width.max(1),
            height: height.max(1),
            present_mode: caps
                .present_modes
                .iter()
                .copied()
                .find(|m| *m == wgpu::PresentMode::Fifo)
                .unwrap_or(caps.present_modes[0]),
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        })
    }

    /// Apply a surface configuration. Thin wrapper so callers don't need to
    /// hold a reference to the device.
    pub fn configure_surface(
        &self,
        surface: &wgpu::Surface<'static>,
        config: &wgpu::SurfaceConfiguration,
    ) {
        surface.configure(&self.device, config);
    }

    /// Render the most-recently-uploaded frame to `target_view`. The view
    /// must be compatible with the format passed to [`Renderer::new`].
    pub fn render_to(&self, target_view: &wgpu::TextureView) -> Result<(), RenderError> {
        let bind_group = self.bind_group.as_ref().ok_or(RenderError::NoFrame)?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vance-render.encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vance-render.pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
        Ok(())
    }

    /// Headless convenience: render to an internal Rgba8UnormSrgb texture
    /// of the given size, then map and return as raw RGBA bytes (row-major,
    /// no padding). Used by the golden-frame test; not by production code.
    pub fn render_to_rgba_buffer(&self, width: u32, height: u32) -> Result<Vec<u8>, RenderError> {
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vance-render.headless-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        self.render_to(&view)?;

        // Read back. wgpu requires bytes_per_row to be a multiple of 256.
        let bytes_per_pixel = 4u32;
        let unpadded_row = width * bytes_per_pixel;
        let row_align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_row = unpadded_row.div_ceil(row_align) * row_align;
        let buffer_size = (padded_row as u64) * (height as u64);

        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vance-render.headless-staging"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vance-render.headless-copy"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = staging.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = sender.send(r);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        receiver
            .recv()
            .map_err(|e| RenderError::DeviceRequest(format!("staging map recv: {e}")))?
            .map_err(|e| RenderError::DeviceRequest(format!("staging map: {e}")))?;
        let mapped = slice.get_mapped_range();

        // Strip row padding.
        let mut out = Vec::with_capacity((unpadded_row * height) as usize);
        for row in 0..height {
            let start = (row * padded_row) as usize;
            let end = start + unpadded_row as usize;
            out.extend_from_slice(&mapped[start..end]);
        }
        drop(mapped);
        staging.unmap();
        Ok(out)
    }

    /// HDR export readback: render to an internal `Rgba16Float` target, then
    /// map and return packed **`rgba64le`** bytes (16-bit unsigned per
    /// channel, little-endian, 8 bytes/pixel) — the encoder's HDR wire
    /// format. The renderer MUST have been built with
    /// `RendererConfig.target_format = Rgba16Float` so `self.pipeline`
    /// matches the float attachment (asserted). The shader emits final PQ/HLG
    /// code values into the float target, which are converted to 16-bit here.
    pub fn render_to_rgba64_buffer(&self, width: u32, height: u32) -> Result<Vec<u8>, RenderError> {
        if self.target_format != wgpu::TextureFormat::Rgba16Float {
            return Err(RenderError::DeviceRequest(
                "render_to_rgba64_buffer requires a renderer built with Rgba16Float target".into(),
            ));
        }
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vance-render.hdr-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba16Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        self.render_to(&view)?;

        // Rgba16Float is 8 bytes/pixel (4 half-floats). Row alignment to 256.
        let src_bpp = 8u32;
        let unpadded_row = width * src_bpp;
        let row_align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_row = unpadded_row.div_ceil(row_align) * row_align;
        let buffer_size = (padded_row as u64) * (height as u64);

        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vance-render.hdr-staging"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vance-render.hdr-copy"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = staging.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = sender.send(r);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        receiver
            .recv()
            .map_err(|e| RenderError::DeviceRequest(format!("hdr staging map recv: {e}")))?
            .map_err(|e| RenderError::DeviceRequest(format!("hdr staging map: {e}")))?;
        let mapped = slice.get_mapped_range();

        // Convert each half-float channel → 16-bit unorm (rgba64le). Strip the
        // row padding while we're at it.
        let mut out = Vec::with_capacity((width as usize) * (height as usize) * 8);
        for row in 0..height {
            let row_start = (row * padded_row) as usize;
            for px in 0..width as usize {
                let base = row_start + px * 8;
                for ch in 0..4 {
                    let h = u16::from_le_bytes([mapped[base + ch * 2], mapped[base + ch * 2 + 1]]);
                    let v = f16_to_f32(h).clamp(0.0, 1.0);
                    let u = (v * 65535.0).round() as u16;
                    out.extend_from_slice(&u.to_le_bytes());
                }
            }
        }
        drop(mapped);
        staging.unmap();
        Ok(out)
    }

    /// Phase 10 — headless convenience: blit `rgba` via
    /// [`Self::present_rgba`] to an internal Rgba8UnormSrgb target and
    /// read it back. Used to test the preview blit path; production
    /// preview blits straight to the surface view.
    pub fn present_rgba_to_buffer(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<u8>, RenderError> {
        let target = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vance-render.blit-test-target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        self.present_rgba(
            rgba,
            width,
            height,
            &view,
            wgpu::TextureFormat::Rgba8UnormSrgb,
        )?;

        let bytes_per_pixel = 4u32;
        let unpadded_row = width * bytes_per_pixel;
        let padded_row = unpadded_row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vance-render.blit-test-staging"),
            size: (padded_row as u64) * (height as u64),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vance-render.blit-test-copy"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = staging.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = sender.send(r);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        receiver
            .recv()
            .map_err(|e| RenderError::DeviceRequest(format!("blit staging recv: {e}")))?
            .map_err(|e| RenderError::DeviceRequest(format!("blit staging map: {e}")))?;
        let mapped = slice.get_mapped_range();
        let mut out = Vec::with_capacity((unpadded_row * height) as usize);
        for row in 0..height {
            let start = (row * padded_row) as usize;
            out.extend_from_slice(&mapped[start..start + unpadded_row as usize]);
        }
        drop(mapped);
        staging.unmap();
        Ok(out)
    }

    /// Render the current frame to an offscreen RGBA8 buffer for scope
    /// analysis (Phase 6.12). Identical to [`Self::render_to_rgba_buffer`]
    /// except that when `pre_view_lut` is true the display/view LUT is
    /// temporarily bypassed so the scopes read the signal *before* the
    /// monitoring transform. The view-LUT enable flag is snapshotted and
    /// RESTORED on every return path (incl. error), so the live monitor
    /// is never left corrupted.
    pub fn render_scope_buffer(
        &mut self,
        width: u32,
        height: u32,
        pre_view_lut: bool,
    ) -> Result<Vec<u8>, RenderError> {
        if !pre_view_lut {
            return self.render_to_rgba_buffer(width, height);
        }
        let saved = self.current_params.view_lut_params[0];
        self.set_view_lut_enable(0.0);
        let result = self.render_to_rgba_buffer(width, height);
        self.set_view_lut_enable(saved);
        result
    }

    fn rebuild_bind_group(&mut self) {
        // A LUT / curve atlas can arrive before the first frame (e.g. a view
        // LUT loaded while only the compositor renders); the first
        // `upload_frame` builds the bind group then.
        if self.tex_y.is_none() {
            return;
        }
        let make_view =
            |t: &Option<wgpu::Texture>| t.as_ref().unwrap().create_view(&Default::default());
        let view_y = make_view(&self.tex_y);
        let view_u = make_view(&self.tex_u);
        let view_v = make_view(&self.tex_v);
        // LUT slot — prime a 1×1×1 dummy so the bind group is valid
        // before the user loads anything. The shader gates against
        // `lut_params.enable`, so the dummy never contributes.
        let dummy_3d = |dev: &wgpu::Device, label: &str| -> wgpu::Texture {
            dev.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D3,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        if self.tex_lut.is_none() {
            self.tex_lut = Some(dummy_3d(&self.device, "vance-render.tex_lut_dummy"));
            self.lut_size = 1;
        }
        if self.tex_view_lut.is_none() {
            self.tex_view_lut = Some(dummy_3d(&self.device, "vance-render.tex_view_lut_dummy"));
            self.view_lut_size = 1;
        }
        // 1×1 D2 dummy for the curve atlas before any curves node loads.
        if self.tex_curve_atlas.is_none() {
            self.tex_curve_atlas = Some(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vance-render.tex_curve_atlas_dummy"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            }));
            self.curve_atlas_width = 1;
        }
        let view_lut = self
            .tex_lut
            .as_ref()
            .unwrap()
            .create_view(&Default::default());
        let view_view_lut = self
            .tex_view_lut
            .as_ref()
            .unwrap()
            .create_view(&Default::default());
        let view_curve_atlas = self
            .tex_curve_atlas
            .as_ref()
            .unwrap()
            .create_view(&Default::default());

        self.bind_group = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vance-render.bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view_y),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&view_u),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&view_v),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.params_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: wgpu::BindingResource::TextureView(&view_lut),
                },
                wgpu::BindGroupEntry {
                    binding: 9,
                    resource: wgpu::BindingResource::TextureView(&view_view_lut),
                },
                wgpu::BindGroupEntry {
                    binding: 10,
                    resource: wgpu::BindingResource::TextureView(&view_curve_atlas),
                },
            ],
        }));
    }
}

/// Source kinds the shaders branch on (`SRC_*` in common.wgsl).
pub(crate) const SRC_PLANAR: u32 = 0;
/// Straight-alpha RGBA8 (images, titles, `PixelFormat::Rgba8` frames).
pub(crate) const SRC_RGBA: u32 = 1;
/// NV12: Y + one interleaved two-channel UV plane.
pub(crate) const SRC_NV12: u32 = 2;
/// 10-bit Y / U / V as little-endian two-byte Rg8 texels (no 16-bit-norm
/// textures): filtered in the shader.
pub(crate) const SRC_PLANAR10: u32 = 3;

/// One plane texture's upload: its size and format and the tightly packed
/// rows.
pub(crate) struct Plane<'a> {
    pub(crate) size: (u32, u32),
    pub(crate) format: wgpu::TextureFormat,
    pub(crate) bytes: &'a [u8],
    pub(crate) bytes_per_row: u32,
}

/// How a (size-validated) decoded frame uploads.
pub(crate) struct FramePlanes<'a> {
    /// `SRC_*`.
    pub(crate) kind: u32,
    /// The planes hold 10-bit codes as R16Unorm.
    pub(crate) ten_bit: bool,
    /// Shader sample scale: a sample × this = code / (2^bits − 1).
    pub(crate) sample_scale: f32,
    /// Y (or RGBA), U (or NV12's UV), V; `None` = unused.
    pub(crate) planes: [Option<Plane<'a>>; 3],
}

/// The plane uploads of `frame`, as views into its bytes (no copies): NV12's
/// UV plane stays interleaved (a two-channel texture), 10-bit samples stay
/// little-endian `u16` — R16Unorm planes, or on a GPU without 16-bit-norm
/// textures two-byte Rg8Unorm planes the shader filters itself (the same
/// 10-bit codes) — and RGBA8 is one plane.
pub(crate) fn frame_planes<'a>(frame: &'a FrameData, supports_r16: bool) -> FramePlanes<'a> {
    use wgpu::TextureFormat::{R16Unorm, R8Unorm, Rg8Unorm, Rgba8Unorm};
    let (w, h) = (frame.width, frame.height);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let luma = w as usize * h as usize;
    let chroma = cw as usize * ch as usize;
    let plane = |bytes, size: (u32, u32), format, bpp: u32| {
        Some(Plane {
            size,
            format,
            bytes,
            bytes_per_row: size.0 * bpp,
        })
    };
    let planar = |b: &'a [u8], format, bpp: usize| {
        let (y, rest) = b.split_at(luma * bpp);
        let (u, v) = rest.split_at(chroma * bpp);
        let bpp = bpp as u32;
        [
            plane(y, (w, h), format, bpp),
            plane(u, (cw, ch), format, bpp),
            plane(v, (cw, ch), format, bpp),
        ]
    };
    let bytes: &'a [u8] = &frame.bytes;
    match frame.pixel_format {
        PixelFormat::Yuv420P => FramePlanes {
            kind: SRC_PLANAR,
            ten_bit: false,
            sample_scale: 1.0,
            planes: planar(bytes, R8Unorm, 1),
        },
        PixelFormat::Nv12 => {
            let (y, uv) = bytes.split_at(luma);
            FramePlanes {
                kind: SRC_NV12,
                ten_bit: false,
                sample_scale: 1.0,
                planes: [
                    plane(y, (w, h), R8Unorm, 1),
                    plane(uv, (cw, ch), Rg8Unorm, 2),
                    None,
                ],
            }
        }
        PixelFormat::Yuv420P10Le if supports_r16 => FramePlanes {
            kind: SRC_PLANAR,
            ten_bit: true,
            sample_scale: 65535.0 / 1023.0,
            planes: planar(bytes, R16Unorm, 2),
        },
        PixelFormat::Yuv420P10Le => FramePlanes {
            kind: SRC_PLANAR10,
            ten_bit: true,
            sample_scale: 1.0,
            planes: planar(bytes, Rg8Unorm, 2),
        },
        PixelFormat::Rgba8 => FramePlanes {
            kind: SRC_RGBA,
            ten_bit: false,
            sample_scale: 1.0,
            planes: [plane(bytes, (w, h), Rgba8Unorm, 4), None, None],
        },
    }
}

/// Keep `slot` a sampled, copy-target `size` × `format` texture, reusing a
/// matching one; returns whether it was (re)allocated.
pub(crate) fn ensure_plane_texture(
    device: &wgpu::Device,
    slot: &mut Option<wgpu::Texture>,
    size: (u32, u32),
    format: wgpu::TextureFormat,
    label: &str,
) -> bool {
    if slot
        .as_ref()
        .is_some_and(|t| (t.width(), t.height()) == size && t.format() == format)
    {
        return false;
    }
    *slot = Some(device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    }));
    true
}

/// Queue `p`'s rows into `t` (sized for it by [`ensure_plane_texture`]).
pub(crate) fn write_plane(queue: &wgpu::Queue, t: &wgpu::Texture, p: &Plane<'_>) {
    queue.write_texture(
        t.as_image_copy(),
        p.bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(p.bytes_per_row),
            rows_per_image: Some(p.size.1),
        },
        t.size(),
    );
}

async fn create_device(
    instance: &wgpu::Instance,
    compatible_surface: Option<&wgpu::Surface<'static>>,
) -> Result<(wgpu::Adapter, wgpu::Device, wgpu::Queue), RenderError> {
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface,
            force_fallback_adapter: false,
        })
        .await
        .map_err(|_| RenderError::NoAdapter)?;
    // Downlevel defaults cap max_texture_dimension_2d at 2048, which is
    // smaller than 4K (3840). Use the adapter's native limits so we can
    // allocate Y/U/V planes at any reasonable resolution. We still keep
    // the rest of the request defensive (no special features).
    let limits = wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits());
    // R16Unorm plane textures (10-bit YUV sources) need TEXTURE_FORMAT_16BIT_
    // NORM, which is a native (non-baseline) feature. Request it only when
    // the adapter offers it (universal on desktop Metal/Vulkan/D3D12);
    // without it 10-bit planes upload as two-byte Rg8 texels.
    let features = wgpu::Features::TEXTURE_FORMAT_16BIT_NORM & adapter.features();
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("vance-render.device"),
            required_features: features,
            required_limits: limits,
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        })
        .await
        .map_err(|e| RenderError::DeviceRequest(format!("{e}")))?;
    Ok((adapter, device, queue))
}

pub(crate) fn yuv_plane_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

#[cfg(test)]
mod hdr_tests {
    use super::*;

    /// Reference PQ (SMPTE ST 2084) OETF mirroring the WGSL `pq_oetf`, used
    /// to lock the constants.
    fn pq_oetf_ref(e: f64) -> f64 {
        let m1 = 0.1593017578125;
        let m2 = 78.84375;
        let c1 = 0.8359375;
        let c2 = 18.8515625;
        let c3 = 18.6875;
        let en = e.clamp(0.0, 1.0);
        let ym1 = en.powf(m1);
        ((c1 + c2 * ym1) / (1.0 + c3 * ym1)).powf(m2)
    }

    #[test]
    fn pq_constants_match_reference() {
        // Known ST 2084 anchors: E=0 → a tiny floor (~7e-7, rounds to code 0
        // at 16-bit); E=1 (10000 nits) → 1.
        assert!(pq_oetf_ref(0.0) < 1e-5, "PQ(0) floor: {}", pq_oetf_ref(0.0));
        assert!(
            (pq_oetf_ref(0.0) * 65535.0).round() as u32 == 0,
            "PQ(0) rounds to code 0"
        );
        assert!((pq_oetf_ref(1.0) - 1.0).abs() < 1e-6);
        // 100 nits (E=0.01, SDR reference white) → ≈0.5081 code value.
        assert!(
            (pq_oetf_ref(0.01) - 0.5081).abs() < 2e-3,
            "PQ(100 nits) = {}",
            pq_oetf_ref(0.01)
        );
        // Monotonic increasing.
        assert!(pq_oetf_ref(0.001) < pq_oetf_ref(0.01));
        assert!(pq_oetf_ref(0.01) < pq_oetf_ref(0.1));
    }

    #[test]
    fn f16_round_trips_through_readback_conversion() {
        // f16_to_f32 (HDR readback) must invert f32_to_f16_bits within
        // half-float precision across [0,1] (the PQ output range).
        for &v in &[
            0.0f32, 0.001, 0.01, 0.1, 0.25, 0.5, 0.5081, 0.75, 0.999, 1.0,
        ] {
            let back = f16_to_f32(f32_to_f16_bits(v));
            assert!((back - v).abs() <= 1e-3, "f16 round-trip {v} -> {back}");
        }
        // Exact endpoints.
        assert_eq!(f16_to_f32(f32_to_f16_bits(0.0)), 0.0);
        assert!((f16_to_f32(f32_to_f16_bits(1.0)) - 1.0).abs() < 1e-4);
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    // Build-gate: the WGSL `Params` struct must match `RenderParams`
    // byte-for-byte (binding 4 is read as a single uniform). This
    // size is hand-computed from the std140 layout; a mismatch means
    // the shader will read grade/pipeline fields at the wrong offset
    // and silently corrupt output.
    #[test]
    fn legacy_ten_bit_fallback_matches_the_r16_path() {
        let (w, h) = (34u32, 18u32);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut bytes = Vec::new();
        for (n, base) in [(w * h, 64u16), (cw * ch, 300), (cw * ch, 700)] {
            for i in 0..n {
                bytes.extend_from_slice(&(base + (i * 13 % 800) as u16).min(1019).to_le_bytes());
            }
        }
        let frame = FrameData {
            pts: 0,
            width: w,
            height: h,
            pixel_format: PixelFormat::Yuv420P10Le,
            strides: vec![w, cw, cw],
            bytes: bytes.into(),
            color: Default::default(),
        };
        let run = |r16: bool| {
            let mut r = Renderer::new(RendererConfig::default()).expect("renderer");
            r.supports_r16 = r16 && r.supports_r16;
            r.upload_frame(&frame).unwrap();
            r.render_to_rgba_buffer(w, h).unwrap()
        };
        let (hi, lo) = (run(true), run(false));
        let d = hi
            .iter()
            .zip(&lo)
            .map(|(a, b)| (*a as i32 - *b as i32).abs())
            .max()
            .unwrap();
        // Same 10-bit codes on both paths (see the compositor's test).
        assert!(d <= 1, "legacy 10-bit fallback vs R16: max diff {d}");
    }

    #[test]
    fn render_params_size_matches_std140() {
        // Field offsets (bytes), all naturally 4/16-aligned under repr(C):
        //  opacity 0, rotation 4, translate 8, scale 16, brightness 24,
        //  contrast 28, crop_lrtb 32, sample_scale 48, pixel_aspect 52,
        //  _pad0 56,
        //  chroma_key 64, chroma_params 80, luma_key 96,
        //  pipeline_flags 112, in_to_working 128, working_to_out 176,
        //  lut_params 224, view_lut_params 240, source 256,
        //  grade_count 272, grades 288 (8 * 176 = 1408) -> total 1696.
        // GradeNode is 11 vec4 = 176: lift/gamma/gain/log_offset/log_smh/
        // flags + qual_a (96) + qual_b (112) + qual_c (128) (6.9) +
        // win_a (144) + win_b (160) per node (6.10).
        assert_eq!(std::mem::size_of::<RenderParams>(), 1696);
        assert_eq!(std::mem::size_of::<GradeNode>(), 176);
    }

    #[test]
    fn grade_params_default_is_identity_node() {
        let n = GradeNodeParams::default().to_gpu();
        // Disabled by default? No — GradeNodeParams default is enabled,
        // primary, neutral lift/gamma/gain. Verify the GPU mapping.
        assert_eq!(n.flags[0], 1.0); // enabled
        assert_eq!(n.flags[1], 0.0); // primary mode
        assert_eq!(n.lift, [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(n.gamma, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(n.gain, [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn log_mode_routes_balances_into_slots() {
        let p = GradeNodeParams {
            mode: GradeMode::Log,
            offset: [0.1, 0.2, 0.3],
            shadow: [0.4, 0.0, 0.0],
            midtone: [0.0, 0.5, 0.0],
            highlight: [0.0, 0.0, 0.6],
            ..GradeNodeParams::default()
        };
        let n = p.to_gpu();
        assert_eq!(n.flags[1], 1.0); // log mode
        assert_eq!(n.log_offset, [0.1, 0.2, 0.3, 0.0]);
        assert_eq!(n.lift, [0.4, 0.0, 0.0, 0.0]); // shadow
        assert_eq!(n.gamma, [0.0, 0.5, 0.0, 0.0]); // midtone
        assert_eq!(n.gain, [0.0, 0.0, 0.6, 0.0]); // highlight
    }

    fn ckf(x: f64, y: f64) -> vance_timeline::Keyframe {
        vance_timeline::Keyframe {
            time_seconds: x,
            value: y,
            interp: vance_timeline::KeyframeInterp::Linear,
        }
    }

    #[test]
    fn eval_curve_or_identity_empty_is_identity_clamped() {
        // Empty curve must be y = x (NOT 0.0), clamped to the LUT domain.
        assert_eq!(eval_curve_or_identity(&[], 0.5), 0.5);
        assert_eq!(eval_curve_or_identity(&[], -0.3), 0.0);
        assert_eq!(eval_curve_or_identity(&[], 1.7), 1.0);
    }

    #[test]
    fn eval_curve_or_identity_clamps_output_to_lut_domain() {
        // A curve whose output leaves [0,1] is clamped so the f16 atlas
        // sample stays in range.
        let c = [ckf(0.0, -0.5), ckf(1.0, 1.5)];
        assert_eq!(eval_curve_or_identity(&c, 0.0), 0.0);
        assert_eq!(eval_curve_or_identity(&c, 1.0), 1.0);
    }

    #[test]
    fn curveset_is_identity_detects_empty_and_diagonal() {
        // All-empty = identity.
        assert!(CurveSet::default().is_identity());
        // Explicit 2-point diagonal = identity.
        let diag = vec![ckf(0.0, 0.0), ckf(1.0, 1.0)];
        let cs = CurveSet {
            master: diag.clone(),
            r: diag.clone(),
            g: diag.clone(),
            b: diag.clone(),
            luma: diag,
            ..CurveSet::default()
        };
        assert!(cs.is_identity());
        // A lifted master breaks identity.
        let cs2 = CurveSet {
            master: vec![ckf(0.0, 0.3), ckf(1.0, 1.0)],
            ..CurveSet::default()
        };
        assert!(!cs2.is_identity());
    }

    #[test]
    fn curves_mode_to_gpu_flags_identity_vs_active() {
        // Identity curves → flags.y = 2 (curves), log_smh.x = 0 (skip).
        let id = GradeNodeParams {
            mode: GradeMode::Curves,
            curves: Some(CurveSet::default()),
            ..GradeNodeParams::default()
        }
        .to_gpu();
        assert_eq!(id.flags[1], 2.0);
        assert_eq!(id.log_smh[0], 0.0);
        // Non-identity curves → log_smh.x = 1 (sample the atlas).
        let active = GradeNodeParams {
            mode: GradeMode::Curves,
            curves: Some(CurveSet {
                master: vec![ckf(0.0, 0.3), ckf(1.0, 1.0)],
                ..CurveSet::default()
            }),
            ..GradeNodeParams::default()
        }
        .to_gpu();
        assert_eq!(active.flags[1], 2.0);
        assert_eq!(active.log_smh[0], 1.0);
    }

    #[test]
    fn sanitize_curve_sorts_drops_nonfinite_and_clamps_output() {
        // Out-of-order + a NaN point + an out-of-range y.
        let dirty = vec![
            ckf(1.0, 1.0),
            ckf(f64::NAN, 0.5),       // dropped (non-finite x)
            ckf(0.5, 1.7),            // y clamped to 1.0
            ckf(0.0, -0.3),           // y clamped to 0.0
            ckf(0.25, f64::INFINITY), // dropped (non-finite y)
        ];
        let clean = sanitize_curve(&dirty);
        // NaN-x and inf-y points removed; remaining sorted ascending by x.
        assert_eq!(clean.len(), 3);
        assert!(clean[0].time_seconds <= clean[1].time_seconds);
        assert!(clean[1].time_seconds <= clean[2].time_seconds);
        // Outputs clamped into [0,1].
        assert!(clean.iter().all(|k| (0.0..=1.0).contains(&k.value)));
        assert_eq!(clean[0].value, 0.0); // was -0.3 at x=0.0
        assert_eq!(clean[2].value, 1.0); // was 1.0 at x=1.0
    }

    #[test]
    fn sanitize_curve_unsorted_evaluates_correctly() {
        // Same control points, shuffled — must evaluate identically once
        // sanitized (guards the eval_keyframes ascending-order assumption).
        let sorted = vec![ckf(0.0, 0.0), ckf(0.5, 0.7), ckf(1.0, 1.0)];
        let shuffled = vec![ckf(1.0, 1.0), ckf(0.0, 0.0), ckf(0.5, 0.7)];
        let a = sanitize_curve(&sorted);
        let b = sanitize_curve(&shuffled);
        for &x in &[0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!((eval_curve_or_identity(&a, x) - eval_curve_or_identity(&b, x)).abs() < 1e-9);
        }
    }

    #[test]
    fn master_curve_folds_into_rgb_channels() {
        // Bake math: r-lane = curveR(curveM(x)). With identity R and a
        // lifting master, the baked r value should equal master(x).
        let cs = CurveSet {
            master: vec![ckf(0.0, 0.3), ckf(1.0, 1.0)],
            ..CurveSet::default()
        };
        let x = 0.0_f64;
        let m = eval_curve_or_identity(&cs.master, x);
        let r = eval_curve_or_identity(&cs.r, m);
        assert!((m - 0.3).abs() < 1e-9);
        assert!((r - 0.3).abs() < 1e-9, "identity R passes master through");
    }

    #[test]
    fn qualifier_to_gpu_maps_lanes() {
        let p = GradeNodeParams {
            qual: Qualifier {
                enabled: true,
                invert: true,
                show_mask: true,
                hue: [0.0, 0.5, 0.1],
                sat: [0.2, 0.8, 0.05],
                luma: [0.1, 0.9, 0.07],
            },
            ..Default::default()
        };
        let n = p.to_gpu();
        assert_eq!(n.flags[2], 1.0); // qual_enable
        assert_eq!(n.flags[3], 1.0); // qual_invert
        assert_eq!(n.qual_a, [0.0, 0.5, 0.2, 0.8]); // hueC, hueRange, satLo, satHi
        assert_eq!(n.qual_b, [0.1, 0.9, 0.1, 0.05]); // lumaLo, lumaHi, hueSoft, satSoft
        assert_eq!(n.qual_c, [0.07, 1.0, 0.0, 0.0]); // lumaSoft, showMask, rsvd, rsvd
    }

    #[test]
    fn qualifier_default_is_off_in_gpu() {
        let n = GradeNodeParams::default().to_gpu();
        assert_eq!(n.flags[2], 0.0, "qualifier must default off (whole-frame)");
    }

    #[test]
    fn qualifier_written_in_all_modes() {
        // The qualifier is mode-independent — a mode switch must never
        // drop the key.
        for mode in [GradeMode::Primary, GradeMode::Log, GradeMode::Curves] {
            let p = GradeNodeParams {
                mode,
                qual: Qualifier {
                    enabled: true,
                    hue: [0.25, 0.3, 0.05],
                    ..Qualifier::default()
                },
                ..Default::default()
            };
            let n = p.to_gpu();
            assert_eq!(n.flags[2], 1.0, "qualifier dropped in {mode:?}");
            assert_eq!(n.qual_a[0], 0.25, "hue center lost in {mode:?}");
        }
    }

    #[test]
    fn grade_nodes_eq_detects_qualifier_change() {
        // The set_grades idempotency gate keys on grade_nodes_eq; if it
        // ignores the qualifier lanes, a hue/sat/luma slider change is
        // wrongly skipped and the stale mask stays on the GPU.
        let a = GradeNode::identity();
        assert!(grade_nodes_eq(&a, &a), "identical nodes must compare equal");
        for mutate in [
            |n: &mut GradeNode| n.qual_a[0] = 0.5, // hue center
            |n: &mut GradeNode| n.qual_b[2] = 0.2, // hue softness
            |n: &mut GradeNode| n.qual_c[0] = 0.3, // luma softness
            |n: &mut GradeNode| n.win_a[1] = 0.7,  // window center x (6.10)
            |n: &mut GradeNode| n.win_b[2] = 0.3,  // window feather (6.10)
        ] {
            let mut b = a;
            mutate(&mut b);
            assert!(
                !grade_nodes_eq(&a, &b),
                "qualifier/window-lane change must register as different"
            );
        }
    }

    #[test]
    fn window_to_gpu_maps_lanes() {
        let p = GradeNodeParams {
            window: Window {
                shape: 2,
                center: [0.4, 0.6],
                size: [0.3, 0.2],
                rotation: 0.5,
                feather: 0.15,
                invert: true,
            },
            ..Default::default()
        };
        let n = p.to_gpu();
        assert_eq!(n.win_a, [2.0, 0.4, 0.6, 0.5]); // shape, cx, cy, rot
        assert_eq!(n.win_b, [0.3, 0.2, 0.15, 1.0]); // sx, sy, feather, invert
                                                    // Default window is off (shape 0) and identity-mapped.
        let off = GradeNodeParams::default().to_gpu();
        assert_eq!(off.win_a[0], 0.0, "window must default off");
    }

    // Pure-Rust mirror of the WGSL `window_mask` (shader.wgsl) so the SDF
    // is unit-testable without a GPU. KEEP IN SYNC with the shader.
    fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
        let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }
    // Intentional: test-only mirror that takes the shader's `GradeNode`
    // window fields as separate arguments, so each test sets them directly.
    #[allow(clippy::too_many_arguments)]
    fn window_mask_cpu(
        uv: [f32; 2],
        shape: f32,
        center: [f32; 2],
        size: [f32; 2],
        rotation: f32,
        feather: f32,
        invert: bool,
        aspect: f32,
    ) -> f32 {
        if shape < 0.5 {
            return 1.0;
        }
        let half = [size[0].max(1e-4), size[1].max(1e-4)];
        let feather = feather.max(1e-4);
        let mut dx = uv[0] - center[0];
        let dy = uv[1] - center[1];
        dx *= aspect;
        let c = (-rotation).cos();
        let s = (-rotation).sin();
        let lx = dx * c - dy * s;
        let ly = dx * s + dy * c;
        let rx = half[0] * aspect;
        let ry = half[1];
        let nd = if shape < 1.5 {
            ((lx / rx).powi(2) + (ly / ry).powi(2)).sqrt()
        } else {
            (lx.abs() / rx).max(ly.abs() / ry)
        };
        let band = feather / (0.5 * (rx + ry)).max(1e-4);
        let mut m = 1.0 - smoothstep(1.0, 1.0 + band, nd);
        if invert {
            m = 1.0 - m;
        }
        m.clamp(0.0, 1.0)
    }

    #[test]
    fn window_mask_shape_off_is_identity() {
        for uv in [[0.0, 0.0], [0.5, 0.5], [1.0, 1.0]] {
            assert_eq!(
                window_mask_cpu(uv, 0.0, [0.5, 0.5], [0.25, 0.25], 0.0, 0.1, false, 1.0),
                1.0
            );
        }
    }

    #[test]
    fn window_mask_ellipse_center_in_corner_out() {
        let m_center = window_mask_cpu(
            [0.5, 0.5],
            1.0,
            [0.5, 0.5],
            [0.25, 0.25],
            0.0,
            0.01,
            false,
            1.0,
        );
        let m_corner = window_mask_cpu(
            [0.0, 0.0],
            1.0,
            [0.5, 0.5],
            [0.25, 0.25],
            0.0,
            0.01,
            false,
            1.0,
        );
        assert!(m_center > 0.99, "center keyed: {m_center}");
        assert!(m_corner < 0.01, "corner excluded: {m_corner}");
    }

    #[test]
    fn window_mask_invert_flips() {
        let m = window_mask_cpu(
            [0.5, 0.5],
            1.0,
            [0.5, 0.5],
            [0.25, 0.25],
            0.0,
            0.01,
            false,
            1.0,
        );
        let mi = window_mask_cpu(
            [0.5, 0.5],
            1.0,
            [0.5, 0.5],
            [0.25, 0.25],
            0.0,
            0.01,
            true,
            1.0,
        );
        assert!((m + mi - 1.0).abs() < 1e-5, "invert must be 1-m: {m} {mi}");
    }

    #[test]
    fn window_mask_aspect_is_circular_in_pixels() {
        // With aspect=16/9, equal radii: a pixel offset of (r/aspect, 0)
        // and (0, r) must both land on the boundary (nd==1 -> m~0.5).
        let aspect = 16.0 / 9.0;
        let r = 0.25;
        let mx = window_mask_cpu(
            [0.5 + r / aspect, 0.5],
            1.0,
            [0.5, 0.5],
            [r, r],
            0.0,
            0.01,
            false,
            aspect,
        );
        let my = window_mask_cpu(
            [0.5, 0.5 + r],
            1.0,
            [0.5, 0.5],
            [r, r],
            0.0,
            0.01,
            false,
            aspect,
        );
        assert!(
            (mx - my).abs() < 0.05,
            "circle must be symmetric in pixels: {mx} {my}"
        );
    }

    #[test]
    fn window_mask_rect_vs_ellipse_corner() {
        // The square's corner (just inside the box extents diagonally) is
        // inside a rect but outside the inscribed ellipse.
        let uv = [0.5 + 0.24, 0.5 + 0.24];
        let rect = window_mask_cpu(uv, 2.0, [0.5, 0.5], [0.25, 0.25], 0.0, 0.01, false, 1.0);
        let ellipse = window_mask_cpu(uv, 1.0, [0.5, 0.5], [0.25, 0.25], 0.0, 0.01, false, 1.0);
        assert!(rect > 0.99, "rect includes its corner: {rect}");
        assert!(ellipse < 0.5, "ellipse excludes the box corner: {ellipse}");
    }
}
