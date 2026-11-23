//! Multi-track compositor (ADR 0007).
//!
//! Three GPU passes per frame: a layer pass per visible clip into a reusable
//! `Rgba16Float` slot keyed by clip ([`Compositor::render_layer`]), a
//! composite pass applying the clip's blend mode and opacity on
//! premultiplied values ([`Compositor::composite`]), and an output pass
//! (transfer encode, preview-only view LUT).
//!
//! Blending is display-referred (encoded) for SDR output and linear working
//! space for HDR output ([`OutputSpec::hdr`]).
//!
//! A frame is `begin_frame` → (`render_layer` → [`read_layer_rgba8` →
//! CPU → `write_layer_rgba8`] → `composite` / `composite_dissolve`)* →
//! one `finish_*`. Everything is recorded into one command encoder and
//! submitted by `finish_*`; the only mid-frame submits are a CPU readback
//! (`read_layer_rgba8`) and re-rendering a slot whose uploads a recorded
//! pass still reads.
//!
//! `render_layer` uploads and runs the keyer's matte passes but defers the
//! layer pass itself: a plain `composite` then renders the layer straight
//! over the backdrop in one fused pass, and the slot texture is only
//! allocated and written when something needs it (readback, writeback,
//! dissolve).
//!
//! The RGBA rasterizers for synthetic media ([`render_title_rgba`],
//! [`render_mograph_rgba`], [`render_fusion_rgba`]) keep the background's
//! alpha instead of flattening it, so titles can sit over lower tracks.
//!
//! [`read_layer_rgba8`]: Compositor::read_layer_rgba8

use std::collections::HashMap;
use std::sync::Arc;

use vance_decoder::FrameData;
use vance_fusion::Comp;
pub use vance_mograph::RgbaImage;
use vance_mograph::{MographEngine, MographInstance};

use crate::renderer::{
    bake_curve_atlas, curve_inputs_of, ensure_plane_texture, f16_to_f32, f32_to_f16_bits,
    frame_planes, matrix_to_row3, pack_lut_rgba16f, sanitize_crop, sanitize_scale, wgsl,
    write_plane, CurveSet, GradeNode, GradeNodeParams, RenderError, CURVE_ROWS_PER_NODE,
    MAX_GRADES, SRC_RGBA,
};
use crate::title::{TitleError, TitleParams};

/// Where a layer's pixels come from.
#[derive(Clone, Copy, Debug)]
pub enum LayerSource<'a> {
    /// A decoded frame (file-backed media): YUV, or straight-alpha RGBA8
    /// ([`vance_decoder::PixelFormat::Rgba8`]; the pipeline's input transfer applies, the
    /// YUV range and matrix do not).
    Yuv(&'a FrameData),
    /// A straight-alpha sRGB RGBA8 image (titles, MoGraph, Fusion, add-on
    /// output).
    Rgba(&'a RgbaImage),
}

/// Parameters of the `keyer` effect, evaluated at one time. See
/// `vance_timeline::keyer` for the keys and defaults.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyerParams {
    /// Key colour, encoded RGB in `[0, 1]`.
    pub key_rgb: [f32; 3],
    /// Weighting of the two non-primary channels.
    pub balance: f32,
    /// Matte gain.
    pub strength: f32,
    /// Matte values at or below this become transparent.
    pub clip_black: f32,
    /// Matte values at or above this become opaque.
    pub clip_white: f32,
    /// Erode (+) / dilate (−), in the source's display pixels (a stored
    /// pixel is `LayerParams::pixel_aspect` display pixels wide).
    pub choke_px: f32,
    /// Matte blur radius, in the source's display pixels.
    pub soften_px: f32,
    /// Spill suppression amount.
    pub spill: f32,
    /// Spill reference: 0 = average of the other channels, 1 = their max.
    pub spill_bias: f32,
    /// Invert the matte.
    pub invert: bool,
}

/// Everything the layer pass needs for one clip at one time.
#[derive(Clone, Debug)]
pub struct LayerParams {
    /// Colour pipeline; the layer pass reads only the `input_*` fields and
    /// `input_to_working` (the output side belongs to [`OutputSpec`]).
    pub pipeline: vance_color::PipelineParams,
    /// Translation as a fraction of the canvas.
    pub translate: [f32; 2],
    /// Scale around the canvas centre.
    pub scale: [f32; 2],
    /// Rotation around the canvas centre, in radians.
    pub rotation: f32,
    /// Source crop fractions (left, right, top, bottom).
    pub crop_lrtb: [f32; 4],
    /// Additive brightness.
    pub brightness: f32,
    /// Contrast around 0.5.
    pub contrast: f32,
    /// Legacy `chroma_key` effect: `(key_rgb, tolerance, softness)`.
    pub chroma_key: Option<([f32; 3], f32, f32)>,
    /// Legacy `luma_key` effect: `(threshold, direction, softness)`.
    pub luma_key: Option<(f32, f32, f32)>,
    /// The `keyer` effect.
    pub keyer: Option<KeyerParams>,
    /// Colour-grade nodes, applied in order in working space.
    pub grades: Vec<GradeNodeParams>,
    /// Clip LUT, applied to the decoded source before linearising.
    pub lut: Option<Arc<vance_color::Lut3d>>,
    /// Show the keyer's matte instead of the keyed picture (preview only).
    pub matte_view: bool,
    /// Source sample aspect ratio as width / height of one pixel (1.0 =
    /// square). Fit-to-frame uses the display size `src_w * pixel_aspect`
    /// × `src_h`.
    pub pixel_aspect: f32,
}

/// How a rendered layer combines with the accumulated backdrop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositeOp {
    /// `vance_timeline::BlendMode::gpu_id`.
    pub blend_mode: u32,
    /// Layer opacity in `[0, 1]`, applied in the composite pass.
    pub opacity: f32,
}

/// The canvas and output encoding of one composited frame.
#[derive(Clone, Debug)]
pub struct OutputSpec {
    /// Canvas width in pixels.
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
    /// Linear working → output primaries.
    pub working_to_output: vance_color::Transform,
    /// Output transfer id (`vance_color::pipeline`).
    pub output_transfer: u32,
    /// `true` ⇒ composite in LINEAR working space (values above 1.0 must
    /// survive); `false` ⇒ composite display-encoded (user decision 2).
    pub hdr: bool,
    /// Apply the loaded view LUT in the output pass (preview only).
    pub view_lut: bool,
    /// `true` ⇒ composite over a TRANSPARENT backdrop and read back
    /// straight (un-premultiplied) alpha, for render-and-replace into an
    /// alpha-carrying codec (an alpha that rounds to 0 reads as rgb 0);
    /// `false` ⇒ flatten over opaque black. `finish_to_view` (preview)
    /// always flattens.
    pub transparent: bool,
    /// Primaries of the output space (the target `working_to_output`
    /// converts to); e.g. Rec.2020 for a PQ export, Rec.709 for sRGB.
    pub output_primaries: vance_color::Primaries,
}

/// Identifies a layer slot across frames (a clip id's 128 bits), so an
/// unchanged clip can reuse its texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayerKey(pub u128);

impl From<vance_timeline::ClipId> for LayerKey {
    fn from(id: vance_timeline::ClipId) -> Self {
        LayerKey(id.as_uuid().as_u128())
    }
}

/// A layer rendered during the current frame (valid until the next
/// [`Compositor::begin_frame`]; a handle from an earlier frame is an error).
#[derive(Clone, Copy, Debug)]
pub struct LayerHandle {
    index: u32,
    frame: u64,
}

/// Layer slot format: premultiplied colour in the frame's blend space.
const SLOT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// Keyer matte format (source resolution).
const MATTE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R16Float;
/// The matte line passes' targets: r32float is a portable storage format
/// (R16Float storage is not); each pass rounds its values to f16 anyway.
const MATTE_TMP_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Float;
/// Straight sRGB RGBA8: RGBA sources, add-on read/write, SDR readback.
const RGBA8_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// HDR readback target (the encoder's rgba64 path).
const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// Per-layer uniform. Layout must match `LayerU` in layer.wgsl.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct LayerUniform {
    xform: [f32; 4],
    scale_fit: [f32; 4],
    crop_lrtb: [f32; 4],
    adjust: [f32; 4],
    chroma_key: [f32; 4],
    chroma_params: [f32; 4],
    luma_key: [f32; 4],
    flags: [u32; 4],
    toggles: [u32; 4],
    in_to_working: [[f32; 4]; 3],
    working_to_out: [[f32; 4]; 3],
    key_p: [f32; 4],
    key_s1: [f32; 4],
    key_s2: [f32; 4],
    key_clip: [f32; 4],
    key_refine: [f32; 4],
    src_texel: [f32; 4],
    yuv: [u32; 4],
    grade_count: [u32; 4],
    grades: [GradeNode; MAX_GRADES],
}
// 23 vec4 header + 8 × 176-byte grade nodes; a drift from the WGSL struct
// would silently misread every field after it.
const _: () = assert!(std::mem::size_of::<LayerUniform>() == 23 * 16 + MAX_GRADES * 176);

/// One keyer matte line pass. Layout must match `LineU` in matte.wgsl.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct LineUniform {
    dir_mode: [i32; 4],
    radius: [f32; 4],
    dims: [u32; 4],
}

/// `LineU.dir_mode.z` (matte.wgsl).
const MATTE_ERODE: i32 = 0;
const MATTE_DILATE: i32 = 1;
const MATTE_GAUSS: i32 = 2;
const MATTE_BOX: i32 = 3;
/// Samples a matte line workgroup holds (`SHARED` in matte.wgsl).
const LINE_SHARED: u32 = 1024;
/// The longest line-pass radius in source pixels (the apron must fit).
const MAX_LINE_RADIUS: f32 = 120.0;
/// The longest box radius (three boxes need an apron of 3 × radius).
const MAX_BOX_RADIUS: f32 = 64.0;
/// Softens up to this radius use the exact Gaussian taps; larger ones three
/// boxes (constant cost).
const SOFTEN_EXACT_PX: f32 = 8.0;
/// Choke (4) + soften (2 axes).
const MAX_MATTE_PASSES: usize = 6;

/// Per-composite uniform. Layout must match `CompositeU` in composite.wgsl.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct CompositeUniform {
    modes: [u32; 4],
    vals: [f32; 4],
}

/// Output / pack uniform. Layout must match `OutputU` in output.wgsl.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
struct OutputUniform {
    flags: [u32; 4],
    target_flags: [u32; 4],
    working_to_out: [[f32; 4]; 3],
}

/// Pipelines, layouts and dummy bindings, built on the first frame so a
/// `Renderer` that never composites pays nothing.
struct Gpu {
    sampler: wgpu::Sampler,
    layer_bgl: wgpu::BindGroupLayout,
    layer: wgpu::RenderPipeline,
    /// The layer pass fused with a composite over the backdrop.
    layer_over: wgpu::RenderPipeline,
    key: wgpu::RenderPipeline,
    /// Keyer matte line passes (matte.wgsl) — min / max, exact Gaussian,
    /// boxes — and the copy back to R16Float.
    matte_bgl: wgpu::BindGroupLayout,
    matte_lines: [wgpu::ComputePipeline; 3],
    matte_copy_bgl: wgpu::BindGroupLayout,
    matte_copy: wgpu::RenderPipeline,
    composite_bgl: wgpu::BindGroupLayout,
    composite: wgpu::RenderPipeline,
    output_bgl: wgpu::BindGroupLayout,
    output_layout: wgpu::PipelineLayout,
    output_module: wgpu::ShaderModule,
    /// Output pipelines by target format (surfaces vary).
    output: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    pack: wgpu::RenderPipeline,
    unpack: wgpu::RenderPipeline,
    out_uniform: wgpu::Buffer,
    /// Separate pack / unpack uniforms: an unpack pass recorded this frame
    /// must not see a later pack's matrix at submit.
    pack_uniform: wgpu::Buffer,
    unpack_uniform: wgpu::Buffer,
    dummy_plane: wgpu::TextureView,
    dummy_rgba: wgpu::TextureView,
    dummy_lut: wgpu::TextureView,
    dummy_atlas: wgpu::TextureView,
    dummy_matte: wgpu::TextureView,
    dummy_backdrop: wgpu::TextureView,
    dummy_composite: wgpu::Buffer,
}

/// A layer slot: everything one clip needs on the GPU, reused across frames
/// so one frame's layers never share an upload target or uniform.
struct LayerSlot {
    /// A decoded YUV frame's plane textures (Y/U/V, or Y/UV for NV12).
    planes: [Option<wgpu::Texture>; 3],
    /// An RGBA source (images, titles, RGBA8 frames).
    rgba: Option<wgpu::Texture>,
    lut: Option<(wgpu::Texture, Arc<vance_color::Lut3d>)>,
    atlas: Option<wgpu::Texture>,
    /// Curve inputs baked into `atlas`.
    curves: Vec<(usize, CurveSet)>,
    uniform: wgpu::Buffer,
    /// The key pass's matte and the refined one (choke / soften).
    mattes: Option<[wgpu::Texture; 2]>,
    /// Which of `mattes` holds this frame's final matte (keyer on).
    matte: Option<usize>,
    /// The matte line passes' uniforms (dynamic offsets).
    matte_uniform: Option<wgpu::Buffer>,
    /// The rendered layer (canvas size), allocated only once the layer is
    /// needed on its own (readback, writeback, dissolve); fused composites
    /// never touch it.
    out: Option<wgpu::Texture>,
    /// The layer pass is recorded lazily: `true` until `out` holds this
    /// render (fused composites leave it `true`).
    pending: bool,
    /// Staging texture for `write_layer_rgba8`.
    upload: Option<wgpu::Texture>,
    /// Render generation (stale-handle check).
    generation: u64,
    /// The submit counter when a pass reading this slot's uploads (planes,
    /// uniform, LUT, atlas) was last recorded; re-rendering the slot before
    /// that submit would overwrite what the pass reads.
    pending_at: Option<u64>,
    upload_pending_at: Option<u64>,
}

/// The frame being recorded.
struct Frame {
    /// `begin_frame` count (handles carry it).
    id: u64,
    out: OutputSpec,
    encoder: wgpu::CommandEncoder,
    /// Which accumulator holds the backdrop.
    cur: usize,
    /// Handle index → (slot key, slot generation).
    layers: Vec<(LayerKey, u64)>,
    /// Composite uniforms used so far.
    composites: usize,
    /// Nothing composited yet: the backdrop is transparent black, so the
    /// first composite skips reading it and `begin_frame` skips the clear.
    empty: bool,
    /// A layer shows a matte (grade solo / keyer matte view): skip the view
    /// LUT (SDR) or the encode (HDR) so the matte reads untinted.
    diagnostic: bool,
    /// The largest source this frame's keyer matte line passes ran on.
    matte_need: (u32, u32),
}

/// Layer slots, accumulators and the composite/output pipelines. Owned by
/// the [`crate::Renderer`] and reached through `Renderer::compositor`, so it
/// shares the renderer's device, queue and view LUT.
pub struct Compositor {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// `TEXTURE_FORMAT_16BIT_NORM` enabled (10-bit planes as R16Unorm).
    supports_r16: bool,
    /// The backend needs every workgroup to reach every barrier (DX12 /
    /// FXC): the matte line passes then use the masked entry point.
    barrier_safe: bool,
    gpu: Option<Gpu>,
    /// The renderer's view LUT while it is loaded and enabled.
    view_lut: Option<wgpu::Texture>,
    slots: HashMap<LayerKey, LayerSlot>,
    accum: Vec<wgpu::Texture>,
    comp_uniforms: Vec<wgpu::Buffer>,
    /// Readback (target, staging) by (format, width, height).
    readback: HashMap<(wgpu::TextureFormat, u32, u32), (wgpu::Texture, wgpu::Buffer)>,
    /// Ping-pong r32float targets of the matte line passes, shared by every
    /// keyed layer: grown to the largest source of a frame, fitted to it at
    /// the frame's end, dropped with a canvas-size change.
    matte_tmp: Option<[wgpu::Texture; 2]>,
    frame: Option<Frame>,
    /// Frames begun so far ([`Frame::id`]).
    frames: u64,
    generation: u64,
    submits: u64,
}

fn misuse(msg: impl Into<String>) -> RenderError {
    RenderError::Compositor(msg.into())
}

fn no_frame() -> RenderError {
    misuse("no frame in progress (call begin_frame first)")
}

impl Compositor {
    pub(crate) fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        supports_r16: bool,
        barrier_safe: bool,
    ) -> Self {
        Self {
            device,
            queue,
            supports_r16,
            barrier_safe,
            gpu: None,
            view_lut: None,
            slots: HashMap::new(),
            accum: Vec::new(),
            comp_uniforms: Vec::new(),
            readback: HashMap::new(),
            matte_tmp: None,
            frame: None,
            frames: 0,
            generation: 0,
            submits: 0,
        }
    }

    /// The renderer's view LUT (`None` when unloaded or disabled).
    pub(crate) fn set_view_lut(&mut self, lut: Option<wgpu::Texture>) {
        self.view_lut = lut;
    }

    /// Start a frame: size the accumulators for `out`; the backdrop starts
    /// transparent black. An unfinished previous frame is discarded.
    pub fn begin_frame(&mut self, out: &OutputSpec) -> Result<(), RenderError> {
        self.check_dims(out.width, out.height, "canvas")?;
        self.frame = None;
        if self.gpu.is_none() {
            self.gpu = Some(Gpu::new(&self.device, self.barrier_safe));
        }
        if self.accum.first().map(|t| (t.width(), t.height())) != Some((out.width, out.height)) {
            // A new canvas size (e.g. preview → export) also drops the matte
            // scratch; the next keyed layer allocates what it needs.
            self.matte_tmp = None;
            self.accum = (0..2)
                .map(|_| {
                    tex2d(
                        &self.device,
                        "vance-render.compositor.accum",
                        (out.width, out.height),
                        SLOT_FORMAT,
                        wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::TEXTURE_BINDING,
                    )
                })
                .collect();
        }
        self.frames += 1;
        self.frame = Some(Frame {
            id: self.frames,
            out: out.clone(),
            encoder: self.new_encoder(),
            cur: 0,
            layers: Vec::new(),
            composites: 0,
            empty: true,
            diagnostic: false,
            matte_need: (0, 0),
        });
        Ok(())
    }

    /// Run the layer pass for one clip into its slot, fitting the source
    /// into the canvas without distortion (transparent bars).
    pub fn render_layer(
        &mut self,
        k: LayerKey,
        s: LayerSource<'_>,
        p: &LayerParams,
    ) -> Result<LayerHandle, RenderError> {
        let src_dims = match s {
            LayerSource::Yuv(f) => {
                let expected = f.pixel_format.frame_size(f.width, f.height);
                if f.bytes.len() != expected {
                    return Err(RenderError::MalformedFrame {
                        expected,
                        got: f.bytes.len(),
                    });
                }
                (f.width, f.height)
            }
            LayerSource::Rgba(img) => {
                let expected = img.width as usize * img.height as usize * 4;
                if img.pixels.len() != expected {
                    return Err(RenderError::MalformedFrame {
                        expected,
                        got: img.pixels.len(),
                    });
                }
                (img.width, img.height)
            }
        };
        self.check_dims(src_dims.0, src_dims.1, "layer source")?;
        let frame = self.frame.as_ref().ok_or_else(no_frame)?;
        let out = frame.out.clone();
        if self
            .slots
            .get(&k)
            .is_some_and(|sl| sl.pending_at == Some(self.submits))
        {
            // A pass recorded since the last submit still reads this slot's
            // uploads/uniform; submit it before they are overwritten.
            self.flush();
        }
        self.generation += 1;
        let generation = self.generation;
        let submits = self.submits;

        let Self {
            device,
            queue,
            supports_r16,
            barrier_safe,
            gpu,
            slots,
            frame,
            matte_tmp,
            ..
        } = self;
        let gpu = gpu.as_ref().expect("built in begin_frame");
        let frame = frame.as_mut().expect("checked above");
        let slot = slots.entry(k).or_insert_with(|| LayerSlot::new(device));

        // Source upload.
        let src = match s {
            LayerSource::Yuv(f) => slot.upload_frame(device, queue, *supports_r16, f),
            LayerSource::Rgba(img) => {
                slot.upload_rgba(device, queue, img.width, img.height, &img.pixels);
                SourceFormat::RGBA
            }
        };
        let lut_on = slot.sync_lut(device, queue, p.lut.as_ref());
        slot.sync_curves(device, queue, &p.grades);

        let keyer = p.keyer.map(sanitize_keyer);
        let uniform = layer_uniform(p, &out, src_dims, src, lut_on, keyer);
        queue.write_buffer(&slot.uniform, 0, bytemuck::bytes_of(&uniform));

        // A canvas-size change frees the old slot output; it is allocated
        // again only if this layer is needed on its own.
        if slot
            .out
            .as_ref()
            .is_some_and(|t| (t.width(), t.height()) != (out.width, out.height))
        {
            slot.out = None;
        }

        // Keyer matte at source resolution: the key pass into mattes[0], then
        // the choke / soften line passes (compute, r32float ping-pong),
        // copied into mattes[1].
        slot.matte = None;
        if let Some(kp) = keyer {
            if slot
                .mattes
                .as_ref()
                .is_none_or(|m| (m[0].width(), m[0].height()) != src_dims)
            {
                slot.mattes = Some([0, 1].map(|_| {
                    tex2d(
                        device,
                        "vance-render.compositor.matte",
                        src_dims,
                        MATTE_FORMAT,
                        wgpu::TextureUsages::RENDER_ATTACHMENT
                            | wgpu::TextureUsages::TEXTURE_BINDING,
                    )
                }));
            }
            let mattes = slot.mattes.as_ref().expect("allocated above");
            let views = mattes
                .each_ref()
                .map(|t| t.create_view(&Default::default()));
            let bg = slot.bind_group(device, gpu, &gpu.dummy_matte, None);
            let mut pass = begin_pass(&mut frame.encoder, &views[0], None);
            pass.set_pipeline(&gpu.key);
            pass.set_bind_group(0, &bg, &[]);
            pass.draw(0..3, 0..1);
            drop(pass);
            let passes = matte_passes(
                &kp,
                sanitize_pixel_aspect(p.pixel_aspect),
                src_dims,
                !*barrier_safe,
            );
            if passes.is_empty() {
                slot.matte = Some(0);
            } else {
                let align = device.limits().min_uniform_buffer_offset_alignment as u64;
                let uniforms = slot.matte_uniform.get_or_insert_with(|| {
                    device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("vance-render.compositor.matte-uniform"),
                        size: align * MAX_MATTE_PASSES as u64,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    })
                });
                for (i, (u, _)) in passes.iter().enumerate() {
                    queue.write_buffer(uniforms, i as u64 * align, bytemuck::bytes_of(u));
                }
                frame.matte_need = (
                    frame.matte_need.0.max(src_dims.0),
                    frame.matte_need.1.max(src_dims.1),
                );
                let tmp = ensure_matte_tmp(device, matte_tmp, src_dims)
                    .each_ref()
                    .map(|t| t.create_view(&Default::default()));
                for (i, (_, groups)) in passes.iter().enumerate() {
                    let input = if i == 0 { &views[0] } else { &tmp[(i - 1) % 2] };
                    let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("vance-render.compositor.matte-bg"),
                        layout: &gpu.matte_bgl,
                        entries: &[
                            tex_entry(0, input),
                            tex_entry(1, &tmp[i % 2]),
                            wgpu::BindGroupEntry {
                                binding: 2,
                                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                    buffer: uniforms,
                                    offset: 0,
                                    size: wgpu::BufferSize::new(
                                        std::mem::size_of::<LineUniform>() as u64
                                    ),
                                }),
                            },
                        ],
                    });
                    let mut cp = frame
                        .encoder
                        .begin_compute_pass(&wgpu::ComputePassDescriptor {
                            label: Some("vance-render.compositor.matte-line"),
                            timestamp_writes: None,
                        });
                    let family = match passes[i].0.dir_mode[2] {
                        MATTE_GAUSS => 1,
                        MATTE_BOX => 2,
                        _ => 0,
                    };
                    cp.set_pipeline(&gpu.matte_lines[family]);
                    cp.set_bind_group(0, &bg, &[(i as u64 * align) as u32]);
                    cp.dispatch_workgroups(groups.0, groups.1, 1);
                }
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("vance-render.compositor.matte-copy-bg"),
                    layout: &gpu.matte_copy_bgl,
                    entries: &[tex_entry(0, &tmp[(passes.len() - 1) % 2])],
                });
                let mut pass = begin_pass(&mut frame.encoder, &views[1], None);
                pass.set_pipeline(&gpu.matte_copy);
                pass.set_bind_group(0, &bg, &[]);
                pass.draw(0..3, 0..1);
                drop(pass);
                slot.matte = Some(1);
            }
        }
        slot.pending = true;

        frame.diagnostic |= p.matte_view
            || p.grades
                .iter()
                .take(MAX_GRADES)
                .any(|g| g.enabled && g.qual.show_mask);
        slot.generation = generation;
        slot.pending_at = Some(submits);
        frame.layers.push((k, generation));
        Ok(LayerHandle {
            index: frame.layers.len() as u32 - 1,
            frame: frame.id,
        })
    }

    /// Read a rendered layer back as straight-alpha sRGB RGBA8 (for CPU
    /// add-ons). Submits the frame's work so far and waits for the GPU.
    /// HDR layers are clipped to `[0, 1]` on the way out.
    pub fn read_layer_rgba8(&mut self, h: LayerHandle) -> Result<RgbaImage, RenderError> {
        self.materialize(h)?;
        let src = self.layer_view(h)?;
        let out = self.frame.as_ref().ok_or_else(no_frame)?.out.clone();
        let (w, hgt) = (out.width, out.height);
        let gpu = self.gpu.as_ref().expect("built in begin_frame");
        self.queue.write_buffer(
            &gpu.pack_uniform,
            0,
            bytemuck::bytes_of(&pack_uniform(&out, false)),
        );
        let (target, staging) = self.readback_target(RGBA8_FORMAT, w, hgt);
        let target_view = target.create_view(&Default::default());
        let gpu = self.gpu.as_ref().expect("built in begin_frame");
        let bg = output_bind_group(&self.device, gpu, &src, &gpu.pack_uniform, &gpu.dummy_lut);
        let fresh = self.new_encoder();
        let frame = self.frame.as_mut().ok_or_else(no_frame)?;
        let mut pass = begin_pass(&mut frame.encoder, &target_view, None);
        pass.set_pipeline(&gpu.pack);
        pass.set_bind_group(0, &bg, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);
        let encoder = std::mem::replace(&mut frame.encoder, fresh);
        let pixels = self.submit_and_read(encoder, &target, &staging, 4)?;
        Ok(RgbaImage {
            width: w,
            height: hgt,
            pixels,
        })
    }

    /// Replace a rendered layer's pixels with straight-alpha sRGB RGBA8
    /// (canvas-sized, e.g. from [`Self::read_layer_rgba8`]).
    pub fn write_layer_rgba8(
        &mut self,
        h: LayerHandle,
        img: &RgbaImage,
    ) -> Result<(), RenderError> {
        let (key, _) = self.resolve(h)?;
        let out = self.frame.as_ref().ok_or_else(no_frame)?.out.clone();
        let (w, hgt) = (out.width, out.height);
        if (img.width, img.height) != (w, hgt) {
            return Err(misuse(format!(
                "write_layer_rgba8: image is {}x{}, canvas is {w}x{hgt}",
                img.width, img.height
            )));
        }
        let expected = w as usize * hgt as usize * 4;
        if img.pixels.len() != expected {
            return Err(RenderError::MalformedFrame {
                expected,
                got: img.pixels.len(),
            });
        }
        if self.slots[&key].upload_pending_at == Some(self.submits) {
            self.flush();
        }
        let submits = self.submits;
        let Self {
            device,
            queue,
            gpu,
            slots,
            frame,
            ..
        } = self;
        let gpu = gpu.as_ref().expect("built in begin_frame");
        let frame = frame.as_mut().ok_or_else(no_frame)?;
        let slot = slots.get_mut(&key).expect("resolved above");
        queue.write_buffer(
            &gpu.unpack_uniform,
            0,
            bytemuck::bytes_of(&pack_uniform(&out, true)),
        );
        if slot
            .upload
            .as_ref()
            .is_none_or(|t| (t.width(), t.height()) != (w, hgt))
        {
            slot.upload = Some(tex2d(
                device,
                "vance-render.compositor.upload",
                (w, hgt),
                RGBA8_FORMAT,
                wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            ));
        }
        let upload = slot.upload.as_ref().expect("allocated above");
        write_texture(queue, upload, &img.pixels, w * 4);
        slot.upload_pending_at = Some(submits);
        let src = upload.create_view(&Default::default());
        let target = slot
            .ensure_out(device, (w, hgt))
            .create_view(&Default::default());
        let bg = output_bind_group(device, gpu, &src, &gpu.unpack_uniform, &gpu.dummy_lut);
        let mut pass = begin_pass(&mut frame.encoder, &target, None);
        pass.set_pipeline(&gpu.unpack);
        pass.set_bind_group(0, &bg, &[]);
        pass.draw(0..3, 0..1);
        // The slot now holds the written pixels, not its layer pass.
        slot.pending = false;
        Ok(())
    }

    /// Composite a layer over the accumulated backdrop.
    pub fn composite(&mut self, h: LayerHandle, op: CompositeOp) -> Result<(), RenderError> {
        let (key, slot) = self.resolve(h)?;
        if slot.pending {
            return self.composite_fused(key, op);
        }
        let a = self.layer_view(h)?;
        self.composite_pass(&[key], &a, op, None)
    }

    /// Dissolve on one track: `lerp(blend(A over bd), blend(B over bd), mix)`.
    pub fn composite_dissolve(
        &mut self,
        a: (LayerHandle, CompositeOp),
        b: (LayerHandle, CompositeOp),
        mix: f32,
    ) -> Result<(), RenderError> {
        self.materialize(a.0)?;
        self.materialize(b.0)?;
        let va = self.layer_view(a.0)?;
        let vb = self.layer_view(b.0)?;
        let keys = [self.resolve(a.0)?.0, self.resolve(b.0)?.0];
        let mix = if mix.is_finite() {
            mix.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.composite_pass(&keys, &va, a.1, Some((&vb, b.1, mix)))
    }

    /// Run the output pass into a surface view of format `f` (stretched to
    /// the view's size) and submit the frame.
    pub fn finish_to_view(
        &mut self,
        v: &wgpu::TextureView,
        f: wgpu::TextureFormat,
    ) -> Result<(), RenderError> {
        let encoder = self.record_output(v, f, false)?;
        self.submit(encoder);
        Ok(())
    }

    /// Run the output pass and read back RGBA8 — the same bytes as
    /// `Renderer::render_to_rgba_buffer` for a single opaque layer (±1 from
    /// the `Rgba16Float` intermediates). Straight alpha for a
    /// [`OutputSpec::transparent`] frame.
    pub fn finish_to_rgba8(&mut self) -> Result<Vec<u8>, RenderError> {
        let (w, h) = self.canvas()?;
        let (target, staging) = self.readback_target(RGBA8_FORMAT, w, h);
        let view = target.create_view(&Default::default());
        let encoder = self.record_output(&view, RGBA8_FORMAT, true)?;
        self.submit_and_read(encoder, &target, &staging, 4)
    }

    /// Run the output pass and read back RGBA16 — the same bytes as
    /// `Renderer::render_to_rgba64_buffer` (packed `rgba64le`). Straight
    /// alpha for a [`OutputSpec::transparent`] frame.
    pub fn finish_to_rgba64(&mut self) -> Result<Vec<u8>, RenderError> {
        let (w, h) = self.canvas()?;
        let (target, staging) = self.readback_target(HDR_FORMAT, w, h);
        let view = target.create_view(&Default::default());
        let encoder = self.record_output(&view, HDR_FORMAT, true)?;
        let half = self.submit_and_read(encoder, &target, &staging, 8)?;
        // Same conversion as `render_to_rgba64_buffer`.
        let mut out = Vec::with_capacity(half.len());
        for c in half.chunks_exact(2) {
            let v = f16_to_f32(u16::from_le_bytes([c[0], c[1]])).clamp(0.0, 1.0);
            out.extend_from_slice(&((v * 65535.0).round() as u16).to_le_bytes());
        }
        Ok(out)
    }

    /// Drop the slots of every layer not in `live`. Call between frames;
    /// handles to dropped slots fail.
    pub fn retain_only(&mut self, live: &[LayerKey]) {
        self.slots.retain(|k, _| live.contains(k));
    }

    // ---- internals --------------------------------------------------------

    fn check_dims(&self, w: u32, h: u32, what: &str) -> Result<(), RenderError> {
        let max = self.device.limits().max_texture_dimension_2d;
        if w == 0 || h == 0 || w > max || h > max {
            return Err(misuse(format!("{what} size {w}x{h} outside 1..={max}")));
        }
        Ok(())
    }

    fn new_encoder(&self) -> wgpu::CommandEncoder {
        self.device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vance-render.compositor"),
            })
    }

    fn submit(&mut self, encoder: wgpu::CommandEncoder) {
        self.queue.submit(Some(encoder.finish()));
        self.submits += 1;
    }

    /// Submit the frame's work so far and keep recording into a new encoder.
    fn flush(&mut self) {
        let fresh = self.new_encoder();
        if let Some(frame) = self.frame.as_mut() {
            let encoder = std::mem::replace(&mut frame.encoder, fresh);
            self.submit(encoder);
        }
    }

    fn canvas(&self) -> Result<(u32, u32), RenderError> {
        let f = self.frame.as_ref().ok_or_else(no_frame)?;
        Ok((f.out.width, f.out.height))
    }

    fn resolve(&self, h: LayerHandle) -> Result<(LayerKey, &LayerSlot), RenderError> {
        let frame = self.frame.as_ref().ok_or_else(no_frame)?;
        if h.frame != frame.id {
            return Err(misuse("layer handle is from an earlier frame"));
        }
        let &(key, generation) = frame
            .layers
            .get(h.index as usize)
            .ok_or_else(|| misuse("layer handle is not from this frame"))?;
        let slot = self
            .slots
            .get(&key)
            .ok_or_else(|| misuse("layer slot was dropped (retain_only)"))?;
        if slot.generation != generation {
            return Err(misuse("stale layer handle: the slot was re-rendered"));
        }
        Ok((key, slot))
    }

    fn layer_view(&self, h: LayerHandle) -> Result<wgpu::TextureView, RenderError> {
        let (_, slot) = self.resolve(h)?;
        Ok(slot
            .out
            .as_ref()
            .expect("materialized or written slot")
            .create_view(&Default::default()))
    }

    /// A pass reading `key`'s uploads was recorded into the current encoder.
    fn touch(&mut self, key: LayerKey) {
        if let Some(slot) = self.slots.get_mut(&key) {
            slot.pending_at = Some(self.submits);
        }
    }

    /// Queue `u` in this frame's next composite uniform buffer, flagging
    /// (and clearing) an empty backdrop.
    fn composite_uniform(&mut self, mut u: CompositeUniform) -> Result<usize, RenderError> {
        let frame = self.frame.as_mut().ok_or_else(no_frame)?;
        let index = frame.composites;
        frame.composites += 1;
        u.modes[3] = frame.empty as u32;
        frame.empty = false;
        while self.comp_uniforms.len() <= index {
            self.comp_uniforms
                .push(self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("vance-render.compositor.composite-uniform"),
                    size: std::mem::size_of::<CompositeUniform>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }));
        }
        self.queue
            .write_buffer(&self.comp_uniforms[index], 0, bytemuck::bytes_of(&u));
        Ok(index)
    }

    /// Record a pending layer's pass into its slot texture.
    fn materialize(&mut self, h: LayerHandle) -> Result<(), RenderError> {
        let (key, slot) = self.resolve(h)?;
        if !slot.pending {
            return Ok(());
        }
        let Self {
            device,
            gpu,
            slots,
            frame,
            ..
        } = self;
        let gpu = gpu.as_ref().expect("built in begin_frame");
        let frame = frame.as_mut().ok_or_else(no_frame)?;
        let slot = slots.get_mut(&key).expect("resolved above");
        let matte = slot.matte_view();
        let bg = slot.bind_group(
            device,
            gpu,
            matte.as_ref().unwrap_or(&gpu.dummy_matte),
            None,
        );
        let target = slot
            .ensure_out(device, (frame.out.width, frame.out.height))
            .create_view(&Default::default());
        let mut pass = begin_pass(&mut frame.encoder, &target, None);
        pass.set_pipeline(&gpu.layer);
        pass.set_bind_group(0, &bg, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);
        slot.pending = false;
        self.touch(key);
        Ok(())
    }

    /// A pending layer's pass fused with its composite over the backdrop.
    fn composite_fused(&mut self, key: LayerKey, op: CompositeOp) -> Result<(), RenderError> {
        let index = self.composite_uniform(CompositeUniform {
            modes: [op.blend_mode, 0, 0, 0],
            vals: [sanitize_opacity(op.opacity), 0.0, 0.0, 0.0],
        })?;
        let Self {
            device,
            gpu,
            slots,
            frame,
            accum,
            comp_uniforms,
            ..
        } = self;
        let gpu = gpu.as_ref().expect("built in begin_frame");
        let frame = frame.as_mut().ok_or_else(no_frame)?;
        let slot = &slots[&key];
        let backdrop = accum[frame.cur].create_view(&Default::default());
        let target = accum[1 - frame.cur].create_view(&Default::default());
        let matte = slot.matte_view();
        let bg = slot.bind_group(
            device,
            gpu,
            matte.as_ref().unwrap_or(&gpu.dummy_matte),
            Some((&backdrop, &comp_uniforms[index])),
        );
        let mut pass = begin_pass(&mut frame.encoder, &target, None);
        pass.set_pipeline(&gpu.layer_over);
        pass.set_bind_group(0, &bg, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);
        frame.cur = 1 - frame.cur;
        self.touch(key);
        Ok(())
    }

    /// The blend pass over the backdrop for materialized slots (`keys`).
    fn composite_pass(
        &mut self,
        keys: &[LayerKey],
        a: &wgpu::TextureView,
        op_a: CompositeOp,
        b: Option<(&wgpu::TextureView, CompositeOp, f32)>,
    ) -> Result<(), RenderError> {
        let (vb, op_b, mix) = match b {
            Some((v, op, mix)) => (v, op, Some(mix)),
            None => (a, op_a, None),
        };
        let index = self.composite_uniform(CompositeUniform {
            modes: [op_a.blend_mode, op_b.blend_mode, mix.is_some() as u32, 0],
            vals: [
                sanitize_opacity(op_a.opacity),
                sanitize_opacity(op_b.opacity),
                mix.unwrap_or(0.0),
                0.0,
            ],
        })?;
        let frame = self.frame.as_mut().ok_or_else(no_frame)?;
        let gpu = self.gpu.as_ref().expect("built in begin_frame");
        let backdrop = self.accum[frame.cur].create_view(&Default::default());
        let target = self.accum[1 - frame.cur].create_view(&Default::default());
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vance-render.compositor.composite-bg"),
            layout: &gpu.composite_bgl,
            entries: &[
                tex_entry(0, &backdrop),
                tex_entry(1, a),
                tex_entry(2, vb),
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: self.comp_uniforms[index].as_entire_binding(),
                },
            ],
        });
        let mut pass = begin_pass(&mut frame.encoder, &target, None);
        pass.set_pipeline(&gpu.composite);
        pass.set_bind_group(0, &bg, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);
        frame.cur = 1 - frame.cur;
        for &k in keys {
            self.touch(k);
        }
        Ok(())
    }

    /// Record the output pass into `view` and end the frame, returning the
    /// encoder for the caller to add a readback and submit. `readback` ⇒ a
    /// transparent frame reads back straight alpha (the preview never
    /// does).
    fn record_output(
        &mut self,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        readback: bool,
    ) -> Result<wgpu::CommandEncoder, RenderError> {
        let mut frame = self.frame.take().ok_or_else(no_frame)?;
        let transparent = readback && frame.out.transparent;
        // Fit the matte scratch to this frame's largest keyed source: release
        // it when nothing needed it, shrink it when it is over twice the
        // area needed (submitted passes keep the old textures alive).
        let need = frame.matte_need;
        if let Some(t) = &self.matte_tmp {
            let have = t[0].width() as u64 * t[0].height() as u64;
            let want = need.0 as u64 * need.1 as u64;
            if want == 0 {
                self.matte_tmp = None;
            } else if 2 * want < have {
                self.matte_tmp = None;
                ensure_matte_tmp(&self.device, &mut self.matte_tmp, need);
            }
        }
        if frame.empty {
            // Nothing was composited: the output is the cleared backdrop.
            let view = self.accum[frame.cur].create_view(&Default::default());
            drop(begin_pass(
                &mut frame.encoder,
                &view,
                Some(wgpu::Color::TRANSPARENT),
            ));
        }
        let gpu = self.gpu.as_mut().expect("built in begin_frame");
        let lut = self.view_lut.as_ref().filter(|_| frame.out.view_lut);
        let u = OutputUniform {
            flags: [
                frame.out.hdr as u32,
                frame.out.output_transfer,
                lut.is_some() as u32,
                frame.diagnostic as u32,
            ],
            target_flags: [
                format.is_srgb() as u32,
                transparent as u32,
                if format == HDR_FORMAT { 16 } else { 8 },
                0,
            ],
            working_to_out: matrix_to_row3(&frame.out.working_to_output),
        };
        self.queue
            .write_buffer(&gpu.out_uniform, 0, bytemuck::bytes_of(&u));
        if !gpu.output.contains_key(&format) {
            let p = pipeline(
                &self.device,
                &gpu.output_layout,
                &gpu.output_module,
                "fs_output",
                format,
            );
            gpu.output.insert(format, p);
        }
        let src = self.accum[frame.cur].create_view(&Default::default());
        let lut_view = lut.map(|t| t.create_view(&Default::default()));
        let gpu = self.gpu.as_ref().expect("built in begin_frame");
        let bg = output_bind_group(
            &self.device,
            gpu,
            &src,
            &gpu.out_uniform,
            lut_view.as_ref().unwrap_or(&gpu.dummy_lut),
        );
        let mut pass = begin_pass(&mut frame.encoder, view, Some(wgpu::Color::BLACK));
        pass.set_pipeline(&gpu.output[&format]);
        pass.set_bind_group(0, &bg, &[]);
        pass.draw(0..3, 0..1);
        drop(pass);
        Ok(frame.encoder)
    }

    fn readback_target(
        &mut self,
        format: wgpu::TextureFormat,
        w: u32,
        h: u32,
    ) -> (wgpu::Texture, wgpu::Buffer) {
        // Only the current canvas size is kept (one target per format).
        self.readback.retain(|&(_, rw, rh), _| (rw, rh) == (w, h));
        let device = &self.device;
        self.readback
            .entry((format, w, h))
            .or_insert_with(|| {
                let bpp = format.block_copy_size(None).expect("color format");
                let padded = (w * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
                    * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
                let target = tex2d(
                    device,
                    "vance-render.compositor.readback",
                    (w, h),
                    format,
                    wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                );
                let staging = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("vance-render.compositor.staging"),
                    size: padded as u64 * h as u64,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                (target, staging)
            })
            .clone()
    }

    /// Copy `target` into `staging`, submit, wait, and return the unpadded
    /// rows.
    fn submit_and_read(
        &mut self,
        mut encoder: wgpu::CommandEncoder,
        target: &wgpu::Texture,
        staging: &wgpu::Buffer,
        bpp: u32,
    ) -> Result<Vec<u8>, RenderError> {
        let (w, h) = (target.width(), target.height());
        let row = w * bpp;
        let padded =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            target.size(),
        );
        self.submit(encoder);
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
            .map_err(|e| RenderError::DeviceRequest(format!("compositor map recv: {e}")))?
            .map_err(|e| RenderError::DeviceRequest(format!("compositor map: {e}")))?;
        let mapped = slice.get_mapped_range();
        let mut out = Vec::with_capacity((row * h) as usize);
        for y in 0..h as usize {
            let start = y * padded as usize;
            out.extend_from_slice(&mapped[start..start + row as usize]);
        }
        drop(mapped);
        staging.unmap();
        Ok(out)
    }
}

impl LayerSlot {
    fn new(device: &wgpu::Device) -> Self {
        Self {
            planes: [None, None, None],
            rgba: None,
            lut: None,
            atlas: None,
            curves: Vec::new(),
            uniform: device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("vance-render.compositor.layer-uniform"),
                size: std::mem::size_of::<LayerUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
            mattes: None,
            matte: None,
            matte_uniform: None,
            out: None,
            pending: false,
            upload: None,
            generation: 0,
            pending_at: None,
            upload_pending_at: None,
        }
    }

    /// Upload a (size-validated) decoded frame: YUV planes as they are
    /// ([`frame_planes`]), an RGBA8 frame through the RGBA texture.
    fn upload_frame(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        supports_r16: bool,
        f: &FrameData,
    ) -> SourceFormat {
        if f.pixel_format.is_rgba() {
            self.upload_rgba(device, queue, f.width, f.height, &f.bytes);
            return SourceFormat::RGBA;
        }
        let fp = frame_planes(f, supports_r16);
        for (slot, plane) in self.planes.iter_mut().zip(&fp.planes) {
            match plane {
                Some(p) => {
                    ensure_plane_texture(
                        device,
                        slot,
                        p.size,
                        p.format,
                        "vance-render.compositor.plane",
                    );
                    write_plane(queue, slot.as_ref().expect("ensured above"), p);
                }
                None => *slot = None,
            }
        }
        let format = SourceFormat {
            kind: fp.kind,
            ten_bit: fp.ten_bit,
            sample_scale: fp.sample_scale,
        };
        self.rgba = None;
        format
    }

    /// Upload straight-alpha RGBA8 rows (`w * h * 4` bytes, validated).
    fn upload_rgba(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        w: u32,
        h: u32,
        pixels: &[u8],
    ) {
        ensure_plane_texture(
            device,
            &mut self.rgba,
            (w, h),
            RGBA8_FORMAT,
            "vance-render.compositor.rgba-source",
        );
        let t = self.rgba.as_ref().expect("ensured above");
        write_texture(queue, t, pixels, w * 4);
        self.planes = [None, None, None];
    }

    /// The slot's own canvas-size layer texture, allocated on first need.
    fn ensure_out(&mut self, device: &wgpu::Device, canvas: (u32, u32)) -> &wgpu::Texture {
        if self
            .out
            .as_ref()
            .is_none_or(|t| (t.width(), t.height()) != canvas)
        {
            self.out = Some(tex2d(
                device,
                "vance-render.compositor.layer",
                canvas,
                SLOT_FORMAT,
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            ));
        }
        self.out.as_ref().expect("allocated above")
    }

    /// Keep the slot's clip LUT in step with `lut`; returns whether a LUT
    /// is active. Unchanged LUTs (same `Arc` or equal contents) skip the
    /// upload; a malformed LUT is ignored.
    fn sync_lut(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        lut: Option<&Arc<vance_color::Lut3d>>,
    ) -> bool {
        let Some(lut) = lut else {
            return false;
        };
        let n = lut.size as usize;
        if n == 0 || lut.values.len() != n * n * n * 3 {
            tracing::warn!(size = lut.size, "ignoring malformed clip LUT");
            return false;
        }
        if self
            .lut
            .as_ref()
            .is_some_and(|(_, prev)| Arc::ptr_eq(prev, lut) || **prev == **lut)
        {
            return true;
        }
        let size = lut.size;
        let texture = match self.lut.take() {
            Some((t, _)) if t.width() == size => t,
            _ => device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vance-render.compositor.lut"),
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
            }),
        };
        let packed = pack_lut_rgba16f(lut);
        write_texture(queue, &texture, bytemuck::cast_slice(&packed), size * 8);
        self.lut = Some((texture, lut.clone()));
        true
    }

    /// Re-bake the slot's curve atlas when its curve inputs changed.
    fn sync_curves(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        nodes: &[GradeNodeParams],
    ) {
        let curves = curve_inputs_of(nodes);
        if curves.is_empty() || (curves == self.curves && self.atlas.is_some()) {
            self.curves = curves;
            return;
        }
        let width = vance_timeline::grade::CURVE_LUT_WIDTH as u32;
        let height = (MAX_GRADES * CURVE_ROWS_PER_NODE) as u32;
        let atlas = self.atlas.get_or_insert_with(|| {
            tex2d(
                device,
                "vance-render.compositor.curve-atlas",
                (width, height),
                wgpu::TextureFormat::Rgba16Float,
                wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            )
        });
        let packed: Vec<u16> = bake_curve_atlas(&curves)
            .iter()
            .map(|v| f32_to_f16_bits(*v))
            .collect();
        write_texture(queue, atlas, bytemuck::cast_slice(&packed), width * 8);
        self.curves = curves;
    }

    /// This frame's final keyer matte, if the keyer is on.
    fn matte_view(&self) -> Option<wgpu::TextureView> {
        let i = self.matte?;
        Some(self.mattes.as_ref()?[i].create_view(&Default::default()))
    }

    /// The layer bind group; `over` = (backdrop, composite uniform) for the
    /// fused layer-over pass.
    fn bind_group(
        &self,
        device: &wgpu::Device,
        gpu: &Gpu,
        matte: &wgpu::TextureView,
        over: Option<(&wgpu::TextureView, &wgpu::Buffer)>,
    ) -> wgpu::BindGroup {
        let view = |t: &wgpu::Texture| t.create_view(&Default::default());
        let planes = self.planes.each_ref().map(|t| t.as_ref().map(view));
        let rgba = self.rgba.as_ref().map(view);
        let lut = self.lut.as_ref().map(|(t, _)| view(t));
        let atlas = self.atlas.as_ref().map(view);
        let plane = |i: usize| planes[i].as_ref().unwrap_or(&gpu.dummy_plane);
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vance-render.compositor.layer-bg"),
            layout: &gpu.layer_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                },
                tex_entry(1, plane(0)),
                tex_entry(2, plane(1)),
                tex_entry(3, plane(2)),
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.uniform.as_entire_binding(),
                },
                tex_entry(5, rgba.as_ref().unwrap_or(&gpu.dummy_rgba)),
                tex_entry(6, lut.as_ref().unwrap_or(&gpu.dummy_lut)),
                tex_entry(7, atlas.as_ref().unwrap_or(&gpu.dummy_atlas)),
                tex_entry(8, matte),
                tex_entry(9, over.map_or(&gpu.dummy_backdrop, |o| o.0)),
                wgpu::BindGroupEntry {
                    binding: 10,
                    resource: over
                        .map_or(&gpu.dummy_composite, |o| o.1)
                        .as_entire_binding(),
                },
            ],
        })
    }
}

impl Gpu {
    fn new(device: &wgpu::Device, barrier_safe: bool) -> Self {
        let tex = |binding, dim, filterable| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable },
                view_dimension: dim,
                multisampled: false,
            },
            count: None,
        };
        let uniform = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let sampler_entry = wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        use wgpu::TextureViewDimension::{D2, D3};

        let layer_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vance-render.compositor.layer-bgl"),
            entries: &[
                sampler_entry,
                tex(1, D2, true),
                tex(2, D2, true),
                tex(3, D2, true),
                uniform(4),
                tex(5, D2, true),
                tex(6, D3, true),
                tex(7, D2, true),
                tex(8, D2, true),
                tex(9, D2, false),
                uniform(10),
            ],
        });
        let composite_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vance-render.compositor.composite-bgl"),
            entries: &[
                tex(0, D2, false),
                tex(1, D2, false),
                tex(2, D2, false),
                uniform(3),
            ],
        });
        let output_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vance-render.compositor.output-bgl"),
            entries: &[
                sampler_entry,
                tex(1, D2, true),
                uniform(2),
                tex(3, D3, true),
            ],
        });
        let layout = |bgl: &wgpu::BindGroupLayout| {
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("vance-render.compositor.layout"),
                bind_group_layouts: &[Some(bgl)],
                ..Default::default()
            })
        };
        let module = |label, src: &str| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some(label),
                source: wgpu::ShaderSource::Wgsl(src.into()),
            })
        };
        let layer_module = module("vance-render.compositor.layer", wgsl::LAYER);
        let composite_module = module("vance-render.compositor.composite", wgsl::COMPOSITE);
        let output_module = module("vance-render.compositor.output", wgsl::OUTPUT);
        let matte_module = module("vance-render.compositor.matte", wgsl::MATTE);
        let layer_layout = layout(&layer_bgl);
        let composite_layout = layout(&composite_bgl);
        let output_layout = layout(&output_bgl);

        let matte_src = |binding, visibility| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: D2,
                multisampled: false,
            },
            count: None,
        };
        let matte_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vance-render.compositor.matte-bgl"),
            entries: &[
                matte_src(0, wgpu::ShaderStages::COMPUTE),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::StorageTexture {
                        access: wgpu::StorageTextureAccess::WriteOnly,
                        format: MATTE_TMP_FORMAT,
                        view_dimension: D2,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: true,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<LineUniform>() as u64
                        ),
                    },
                    count: None,
                },
            ],
        });
        let matte_copy_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vance-render.compositor.matte-copy-bgl"),
            entries: &[matte_src(0, wgpu::ShaderStages::FRAGMENT)],
        });
        let matte_layout = layout(&matte_bgl);
        let matte_lines = ["cs_minmax", "cs_gauss", "cs_box"].map(|family| {
            let entry = if barrier_safe {
                family.to_string()
            } else {
                format!("{family}_exit")
            };
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(&entry),
                layout: Some(&matte_layout),
                module: &matte_module,
                entry_point: Some(&entry),
                compilation_options: Default::default(),
                cache: None,
            })
        });
        let matte_copy = pipeline(
            device,
            &layout(&matte_copy_bgl),
            &matte_module,
            "fs_matte_copy",
            MATTE_FORMAT,
        );

        // 1×1 placeholders for unused bindings (wgpu zero-initialises them;
        // the shader never reads a placeholder it has not been told about).
        let dummy = |label, format, dimension| {
            device
                .create_texture(&wgpu::TextureDescriptor {
                    label: Some(label),
                    size: wgpu::Extent3d {
                        width: 1,
                        height: 1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                })
                .create_view(&Default::default())
        };
        use wgpu::TextureDimension as Dim;
        let uniform_buffer = |label, size| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let out_size = std::mem::size_of::<OutputUniform>() as u64;
        Self {
            sampler: device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("vance-render.compositor.sampler"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Nearest,
                ..Default::default()
            }),
            layer: pipeline(
                device,
                &layer_layout,
                &layer_module,
                "fs_layer",
                SLOT_FORMAT,
            ),
            layer_over: pipeline(
                device,
                &layer_layout,
                &layer_module,
                "fs_layer_over",
                SLOT_FORMAT,
            ),
            key: pipeline(device, &layer_layout, &layer_module, "fs_key", MATTE_FORMAT),
            matte_bgl,
            matte_lines,
            matte_copy_bgl,
            matte_copy,
            composite: pipeline(
                device,
                &composite_layout,
                &composite_module,
                "fs_composite",
                SLOT_FORMAT,
            ),
            pack: pipeline(
                device,
                &output_layout,
                &output_module,
                "fs_pack",
                RGBA8_FORMAT,
            ),
            unpack: pipeline(
                device,
                &output_layout,
                &output_module,
                "fs_unpack",
                SLOT_FORMAT,
            ),
            output: HashMap::new(),
            out_uniform: uniform_buffer("vance-render.compositor.output-uniform", out_size),
            pack_uniform: uniform_buffer("vance-render.compositor.pack-uniform", out_size),
            unpack_uniform: uniform_buffer("vance-render.compositor.unpack-uniform", out_size),
            dummy_plane: dummy("dummy-plane", wgpu::TextureFormat::R8Unorm, Dim::D2),
            dummy_rgba: dummy("dummy-rgba", RGBA8_FORMAT, Dim::D2),
            dummy_lut: dummy("dummy-lut", wgpu::TextureFormat::Rgba16Float, Dim::D3),
            dummy_atlas: dummy("dummy-atlas", wgpu::TextureFormat::Rgba16Float, Dim::D2),
            dummy_matte: dummy("dummy-matte", MATTE_FORMAT, Dim::D2),
            dummy_backdrop: dummy("dummy-backdrop", SLOT_FORMAT, Dim::D2),
            dummy_composite: uniform_buffer(
                "vance-render.compositor.dummy-composite",
                std::mem::size_of::<CompositeUniform>() as u64,
            ),
            layer_bgl,
            composite_bgl,
            output_bgl,
            output_layout,
            output_module,
        }
    }
}

fn pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    module: &wgpu::ShaderModule,
    entry: &str,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(entry),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(entry),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
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
    })
}

fn tex2d(
    device: &wgpu::Device,
    label: &str,
    (width, height): (u32, u32),
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}

fn write_texture(queue: &wgpu::Queue, texture: &wgpu::Texture, bytes: &[u8], bytes_per_row: u32) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(bytes_per_row),
            rows_per_image: Some(texture.height()),
        },
        texture.size(),
    );
}

fn tex_entry(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::TextureView(view),
    }
}

fn output_bind_group(
    device: &wgpu::Device,
    gpu: &Gpu,
    src: &wgpu::TextureView,
    uniform: &wgpu::Buffer,
    lut: &wgpu::TextureView,
) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("vance-render.compositor.output-bg"),
        layout: &gpu.output_bgl,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Sampler(&gpu.sampler),
            },
            tex_entry(1, src),
            wgpu::BindGroupEntry {
                binding: 2,
                resource: uniform.as_entire_binding(),
            },
            tex_entry(3, lut),
        ],
    })
}

/// A fullscreen pass into `view`; `clear` = `None` leaves the old contents
/// (every pass here overwrites all pixels).
fn begin_pass<'e>(
    encoder: &'e mut wgpu::CommandEncoder,
    view: &wgpu::TextureView,
    clear: Option<wgpu::Color>,
) -> wgpu::RenderPass<'e> {
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("vance-render.compositor.pass"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: clear.map_or(wgpu::LoadOp::Load, wgpu::LoadOp::Clear),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    })
}

/// Opacity clamped to `[0, 1]`; a non-finite value composites opaque.
fn sanitize_opacity(o: f32) -> f32 {
    if o.is_finite() {
        o.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// The source's fitted size in canvas-height units (decision 1): as large
/// as fits the canvas without distortion, centred. `src` is the DISPLAY
/// size (storage width × pixel aspect, height).
fn fit_size(canvas: (u32, u32), src: (f32, f32)) -> [f32; 2] {
    let (cw, ch) = (canvas.0 as f32, canvas.1 as f32);
    let ac = cw / ch;
    let a_src = src.0 / src.1;
    let mut fit = if a_src >= ac {
        [ac, ac / a_src]
    } else {
        [a_src, 1.0]
    };
    // Intentional: a fit within a pixel of the canvas fills it (a stretch of
    // under 1%), so e.g. a 1279×719 clip on its even-rounded 1278×718 canvas
    // has no sub-pixel dark edge columns.
    let snap = |gap: f32, size: f32| gap < 2.0 && gap <= size * 0.01;
    if snap(cw - fit[0] * ch, cw) {
        fit[0] = ac;
    }
    if snap(ch - fit[1] * ch, ch) {
        fit[1] = 1.0;
    }
    fit
}

/// Width / height of one source pixel; non-finite or non-positive → square.
fn sanitize_pixel_aspect(pa: f32) -> f32 {
    if pa.is_finite() && pa > 0.0 {
        pa
    } else {
        1.0
    }
}

/// Keyer params with non-finite values replaced by the effect defaults and
/// radii bounded (the ring passes cost grows with them).
fn sanitize_keyer(k: KeyerParams) -> KeyerParams {
    let f = |v: f32, d: f32| if v.is_finite() { v } else { d };
    KeyerParams {
        key_rgb: [
            f(k.key_rgb[0], 0.0).clamp(0.0, 1.0),
            f(k.key_rgb[1], 1.0).clamp(0.0, 1.0),
            f(k.key_rgb[2], 0.0).clamp(0.0, 1.0),
        ],
        balance: f(k.balance, 0.5).clamp(0.0, 1.0),
        strength: f(k.strength, 1.0).max(0.0),
        clip_black: f(k.clip_black, 0.0),
        clip_white: f(k.clip_white, 0.9),
        choke_px: f(k.choke_px, 0.0).clamp(-64.0, 64.0),
        soften_px: f(k.soften_px, 0.0).clamp(0.0, 64.0),
        spill: f(k.spill, 1.0).max(0.0),
        spill_bias: f(k.spill_bias, 0.5).clamp(0.0, 1.0),
        invert: k.invert,
    }
}

/// The choke's structuring element in SOURCE pixels for `choke_px` DISPLAY
/// pixels on a source of pixel aspect `pa`: a near-regular octagon with
/// inradius `|choke_px|` display px = a rectangle of half-size `(ax, ay)`
/// (fractional for a fractional choke) ⊕ a diamond of `d` whole grid-
/// diagonal steps (one step is `(pa, 1)` display px). Square pixels give
/// the regular octagon, `d ≈ |choke|·(1 − 1/√2)`; otherwise `d` puts the
/// slanted edges at the inradius as well. `ax, ay ≥ 1` whenever `d > 0`, as
/// diagonal steps alone only reach every other pixel. Returns `(ax, ay, d)`.
fn choke_octagon(choke_px: f32, pa: f32) -> (f32, f32, f32) {
    let r = choke_px.abs();
    let rx = r / pa;
    let k = if pa == 1.0 {
        1.0 - std::f32::consts::FRAC_1_SQRT_2
    } else {
        (1.0 + pa - (1.0 + pa * pa).sqrt()) / (2.0 * pa)
    };
    let mut d = (r * k).round();
    if r.min(rx) - 2.0 * d < 1.0 {
        d = ((r.min(rx) - 1.0) * 0.5).floor().max(0.0);
    }
    (rx - 2.0 * d, r - 2.0 * d, d)
}

/// The keyer's choke and soften as matte line passes (matte.wgsl) on a
/// `dims` source of pixel aspect `pa`, each with its uniform and dispatch
/// size. Both are measured in the source's DISPLAY pixels: horizontally a
/// stored pixel is `pa` display pixels wide. `early_exit`: idle workgroups
/// cost nothing (the `_exit` entry points).
fn matte_passes(
    k: &KeyerParams,
    pa: f32,
    dims: (u32, u32),
    early_exit: bool,
) -> Vec<(LineUniform, (u32, u32))> {
    let line_pass = |dir, mode, radius| line_pass(dir, mode, radius, dims, early_exit);
    let mut passes = Vec::new();
    if k.choke_px != 0.0 {
        let mode = if k.choke_px > 0.0 {
            MATTE_ERODE
        } else {
            MATTE_DILATE
        };
        let (ax, ay, d) = choke_octagon(k.choke_px, pa);
        passes.push(line_pass((1, 0), mode, ax));
        passes.push(line_pass((0, 1), mode, ay));
        if d > 0.0 {
            passes.push(line_pass((1, 1), mode, d));
            passes.push(line_pass((1, -1), mode, d));
        }
    }
    if k.soften_px > 0.0 {
        for (dir, s) in [((1, 0), k.soften_px / pa), ((0, 1), k.soften_px)] {
            if s <= SOFTEN_EXACT_PX {
                passes.push(line_pass(dir, MATTE_GAUSS, s));
            } else {
                passes.push(line_pass(dir, MATTE_BOX, soften_box_radius(s)));
            }
        }
    }
    passes
}

/// One line pass along `dir` with `radius` source pixels, packing 1, 2 or 4
/// lines into each workgroup — whichever processes the fewest workgroups
/// (`early_exit`: those with a segment to store; else the whole dispatch).
fn line_pass(
    dir: (i32, i32),
    mode: i32,
    radius: f32,
    (w, h): (u32, u32),
    early_exit: bool,
) -> (LineUniform, (u32, u32)) {
    // Intentional: a radius past the workgroup apron is capped (only a
    // pixel aspect below ~0.5 gets there at the 64 px choke / soften cap).
    let (radius, apron) = if mode == MATTE_BOX {
        let r = radius.min(MAX_BOX_RADIUS);
        (r, 3 * (r.floor() as u32 + 1) + 1)
    } else {
        let r = radius.min(MAX_LINE_RADIUS);
        (r, r.ceil() as u32 + 2)
    };
    let lines = match dir {
        (_, 0) => h,
        (0, _) => w,
        _ => w + h - 1,
    };
    // Line lengths (same geometry as `line_geometry` in matte.wgsl).
    let len = |l: u32| match dir {
        (_, 0) => w,
        (0, _) => h,
        (_, 1) => {
            let c = l as i64 - (h as i64 - 1);
            (w as i64 - c.max(0)).min(h as i64 - (-c).max(0)) as u32
        }
        _ => {
            let y0 = l.min(h - 1);
            (w - (l - y0)).min(y0 + 1)
        }
    };
    let mut best: Option<(u64, u32, u32, (u32, u32))> = None;
    for log_k in 0..=2u32 {
        let sub = LINE_SHARED >> log_k;
        if sub < 2 * apron + 16 {
            continue;
        }
        let tile = sub - 2 * apron;
        let groups_x = lines.div_ceil(1 << log_k);
        let (mut work, mut groups_y) = (0u64, 0u32);
        for g in 0..groups_x {
            let longest = (g << log_k..((g + 1) << log_k).min(lines))
                .map(len)
                .max()
                .unwrap_or(0);
            let segments = longest.div_ceil(tile);
            work += segments as u64;
            groups_y = groups_y.max(segments);
        }
        if !early_exit {
            work = groups_x as u64 * groups_y as u64;
        }
        if best.is_none_or(|b| work < b.0) {
            best = Some((work, log_k, tile, (groups_x, groups_y)));
        }
    }
    let (_, log_k, tile, groups) = best.expect("one sub-line always fits");
    (
        LineUniform {
            dir_mode: [dir.0, dir.1, mode, log_k as i32],
            radius: [radius, 0.0, 0.0, 0.0],
            dims: [w, h, apron, tile],
        },
        groups,
    )
}

/// The radius of each of three equal boxes whose cascade has the variance of
/// the exact soften kernel for `s` source pixels (σ = s / 2, taps out to
/// ±ceil(s)); a fractional radius weighs its end samples by the fraction,
/// so the result is continuous in `s`. Within 0.8 % of the exact kernel on
/// a step edge and 0.6 % on a 2 px gap (1-D, s 8.5 … 128).
fn soften_box_radius(s: f32) -> f32 {
    let sigma = (s as f64 * 0.5).max(1e-3);
    let taps = s.ceil() as i32;
    let (mut moment, mut total) = (0.0, 0.0);
    for i in -taps..=taps {
        let w = (-0.5 * (i as f64 / sigma).powi(2)).exp();
        moment += w * (i * i) as f64;
        total += w;
    }
    let target = moment / total / 3.0;
    let box_var = |rho: f64| {
        let r = rho.floor();
        let f = rho - r;
        (r * (r + 1.0) * (2.0 * r + 1.0) / 3.0 + 2.0 * f * (r + 1.0).powi(2))
            / (2.0 * r + 1.0 + 2.0 * f)
    };
    let (mut lo, mut hi) = (0.0, MAX_BOX_RADIUS as f64);
    for _ in 0..50 {
        let mid = 0.5 * (lo + hi);
        if box_var(mid) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lo as f32
}

/// The shared matte line-pass targets, grown to at least `dims`.
fn ensure_matte_tmp<'a>(
    device: &wgpu::Device,
    tmp: &'a mut Option<[wgpu::Texture; 2]>,
    dims: (u32, u32),
) -> &'a [wgpu::Texture; 2] {
    let have = tmp
        .as_ref()
        .map_or((0, 0), |t| (t[0].width(), t[0].height()));
    if have.0 < dims.0 || have.1 < dims.1 {
        let size = (have.0.max(dims.0), have.1.max(dims.1));
        *tmp = Some([0, 1].map(|_| {
            tex2d(
                device,
                "vance-render.compositor.matte-tmp",
                size,
                MATTE_TMP_FORMAT,
                wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            )
        }));
    }
    tmp.as_ref().expect("allocated above")
}

/// The keyer's uniform lanes: primary channel = the key colour's largest
/// channel (ties → the earlier of R, G, B); the other two in RGB order.
fn keyer_lanes(k: &KeyerParams) -> [[f32; 4]; 5] {
    let key = k.key_rgb;
    let p = (0..3).fold(0, |best, i| if key[i] > key[best] { i } else { best });
    let others: Vec<usize> = (0..3).filter(|&i| i != p).collect();
    let (o1, o2) = (others[0], others[1]);
    let one_hot = |i: usize, w: f32| {
        let mut v = [0.0; 4];
        v[i] = 1.0;
        v[3] = w;
        v
    };
    let kd = key[p] - (key[o1] * k.balance + key[o2] * (1.0 - k.balance));
    [
        // A neutral key colour has no primary excess to key on: inert.
        one_hot(p, if kd > 1e-4 { kd } else { 0.0 }),
        one_hot(o1, k.balance),
        one_hot(o2, k.strength),
        [k.clip_black, k.clip_white, k.invert as u32 as f32, 0.0],
        [k.spill, k.spill_bias, k.choke_px, k.soften_px],
    ]
}

/// How a layer's source is sampled.
#[derive(Clone, Copy)]
struct SourceFormat {
    /// `SRC_*`.
    kind: u32,
    /// 10-bit codes in R16Unorm planes.
    ten_bit: bool,
    sample_scale: f32,
}

impl SourceFormat {
    const RGBA: Self = Self {
        kind: SRC_RGBA,
        ten_bit: false,
        sample_scale: 1.0,
    };
}

fn layer_uniform(
    p: &LayerParams,
    out: &OutputSpec,
    src: (u32, u32),
    fmt: SourceFormat,
    lut_on: bool,
    keyer: Option<KeyerParams>,
) -> LayerUniform {
    let pa = sanitize_pixel_aspect(p.pixel_aspect);
    let display_w = src.0 as f32 * pa;
    let fit = fit_size((out.width, out.height), (display_w, src.1 as f32));
    let scale = sanitize_scale(p.scale);
    // Same clamps as the legacy setters (`set_brightness_contrast`,
    // `set_chroma_key`, `set_luma_key`).
    let (chroma_key, chroma_params) = match p.chroma_key {
        Some((k, tol, soft)) => (
            [
                k[0].clamp(0.0, 1.0),
                k[1].clamp(0.0, 1.0),
                k[2].clamp(0.0, 1.0),
                1.0,
            ],
            [tol.clamp(0.0, 2.0), soft.clamp(0.0, 2.0), 0.0, 0.0],
        ),
        None => ([0.0; 4], [0.1, 0.05, 0.0, 0.0]),
    };
    let luma_key = match p.luma_key {
        Some((th, dir, soft)) => [
            th.clamp(0.0, 1.0),
            if dir >= 0.0 { 1.0 } else { -1.0 },
            soft.clamp(0.0, 1.0),
            1.0,
        ],
        None => [0.5, 1.0, 0.05, 0.0],
    };
    let [key_p, key_s1, key_s2, key_clip, key_refine] =
        keyer.as_ref().map(keyer_lanes).unwrap_or([[0.0; 4]; 5]);
    let count = p.grades.len().min(MAX_GRADES);
    let mut grades = [GradeNode::identity(); MAX_GRADES];
    for (slot, node) in grades.iter_mut().zip(&p.grades) {
        *slot = node.to_gpu();
    }
    LayerUniform {
        xform: [
            p.translate[0],
            p.translate[1],
            0.0,
            out.width as f32 / out.height as f32,
        ],
        scale_fit: [scale[0], scale[1], fit[0], fit[1]],
        crop_lrtb: sanitize_crop(p.crop_lrtb),
        adjust: [
            p.brightness.clamp(-1.0, 1.0),
            p.contrast.clamp(0.0, 4.0),
            fmt.sample_scale,
            // Power windows are shaped in DISPLAY pixels (`display_w`).
            display_w / (src.1 as f32).max(1.0),
        ],
        chroma_key,
        chroma_params,
        luma_key,
        flags: [
            p.pipeline.input_transfer,
            p.pipeline.input_range,
            fmt.kind,
            out.hdr as u32,
        ],
        toggles: [
            lut_on as u32,
            keyer.is_some() as u32,
            p.matte_view as u32,
            0,
        ],
        in_to_working: matrix_to_row3(&p.pipeline.input_to_working),
        working_to_out: matrix_to_row3(&out.working_to_output),
        key_p,
        key_s1,
        key_s2,
        key_clip,
        key_refine,
        src_texel: [
            1.0 / src.0 as f32,
            1.0 / src.1 as f32,
            (-p.rotation).cos(),
            (-p.rotation).sin(),
        ],
        yuv: [p.pipeline.input_matrix, fmt.ten_bit as u32, 0, 0],
        grade_count: [count as u32, 0, 0, 0],
        grades,
    }
}

/// Linear working → Rec.709 for the HDR pack (`unpack`: the inverse): the
/// slot's working values must reach the CPU as sRGB, not working primaries
/// labelled sRGB. SDR slots are already sRGB output (no matrix).
fn pack_uniform(out: &OutputSpec, unpack: bool) -> OutputUniform {
    use vance_color::{transform::primaries_to_primaries, Primaries};
    let to_709 = primaries_to_primaries(out.output_primaries, Primaries::Bt709)
        .compose(&out.working_to_output);
    let m = if unpack { to_709.inverse() } else { to_709 };
    OutputUniform {
        flags: [out.hdr as u32, 0, 0, 0],
        // The pack reads back 8-bit straight alpha.
        target_flags: [0, 0, 8, 0],
        working_to_out: matrix_to_row3(&m),
    }
}

/// Rasterize a title as straight-alpha sRGB RGBA8, its background at
/// `p.bg_alpha`.
pub fn render_title_rgba(p: &TitleParams) -> Result<RgbaImage, TitleError> {
    crate::title::render_title_rgba_image(p)
}

/// Render a MoGraph instance over its template background, keeping the
/// background's alpha (a transparent template stays transparent).
pub fn render_mograph_rgba(
    e: &MographEngine,
    i: &MographInstance,
    t: f64,
    fps: f64,
    dur: f64,
) -> RgbaImage {
    over_background(e.render(i, t, fps, dur), i.template.canvas.background)
}

/// Render a Fusion comp over its background, keeping the background's
/// alpha. `inputs` feeds the comp's `MediaIn` nodes by name.
pub fn render_fusion_rgba(
    c: &vance_fusion::Compositor,
    comp: &Comp,
    t: f64,
    inputs: &HashMap<String, RgbaImage>,
) -> RgbaImage {
    over_background(c.render(comp, t, inputs), comp.background)
}

/// Straight-alpha `img` over a solid `bg` (straight RGBA in `[0, 1]`). With
/// an opaque `bg` the colours equal `RgbaImage::flatten_rgb` exactly.
fn over_background(mut img: RgbaImage, bg: [f64; 4]) -> RgbaImage {
    let [br, bgc, bb, ba] = bg.map(|v| v.clamp(0.0, 1.0) as f32);
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    for px in img.pixels.chunks_exact_mut(4) {
        let a = px[3] as f32 / 255.0;
        let src = [px[0], px[1], px[2]].map(|c| c as f32 / 255.0 * a);
        let (out, out_a) = if ba >= 1.0 {
            // Same arithmetic as `flatten_rgb`.
            let o = [
                src[0] + br * (1.0 - a),
                src[1] + bgc * (1.0 - a),
                src[2] + bb * (1.0 - a),
            ];
            (o, 1.0)
        } else {
            let out_a = a + ba * (1.0 - a);
            if out_a <= 0.0 {
                ([0.0; 3], 0.0)
            } else {
                let k = ba * (1.0 - a);
                let o = [
                    (src[0] + br * k) / out_a,
                    (src[1] + bgc * k) / out_a,
                    (src[2] + bb * k) / out_a,
                ];
                (o, out_a)
            }
        };
        px[0] = q(out[0]);
        px[1] = q(out[1]);
        px[2] = q(out[2]);
        px[3] = q(out_a);
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;
    use vance_decoder::PixelFormat;

    #[test]
    fn over_opaque_background_matches_flatten_rgb() {
        let mut img = RgbaImage::new(3, 1);
        img.pixels = vec![255, 0, 0, 255, 0, 255, 0, 128, 10, 20, 30, 0];
        let bg = [0.2, 0.4, 0.6, 1.0];
        let flat = img.flatten_rgb(bg);
        let out = over_background(img, bg);
        for i in 0..3 {
            assert_eq!(&out.pixels[i * 4..i * 4 + 3], &flat[i * 3..i * 3 + 3]);
            assert_eq!(out.pixels[i * 4 + 3], 255);
        }
    }

    #[test]
    fn over_transparent_background_keeps_source() {
        let mut img = RgbaImage::new(2, 1);
        img.pixels = vec![200, 100, 50, 128, 0, 0, 0, 0];
        let out = over_background(img.clone(), [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(out.pixels, img.pixels);
    }

    #[test]
    fn mograph_and_fusion_rgba_have_canvas_size() {
        let engine = MographEngine::new();
        let inst = MographInstance::new(vance_mograph::builtins::centered_title());
        let m = render_mograph_rgba(&engine, &inst, 1.0, 30.0, 4.0);
        assert_eq!((m.width, m.height), (1920, 1080));
        assert_eq!(m.pixels.len(), 1920 * 1080 * 4);

        let c = vance_fusion::Compositor::new();
        let comp = vance_fusion::starter(101, 51);
        let f = render_fusion_rgba(&c, &comp, 0.0, &HashMap::new());
        assert_eq!((f.width, f.height), (101, 51), "no even crop for RGBA");
    }

    #[test]
    fn layer_key_from_clip_id_is_its_uuid() {
        let id = vance_timeline::ClipId::new();
        assert_eq!(LayerKey::from(id).0, id.as_uuid().as_u128());
    }

    #[test]
    fn layer_uniform_matches_wgsl_size() {
        // 23 header vec4 (incl. two 3-row matrices) + 8 grade nodes.
        assert_eq!(std::mem::size_of::<LayerUniform>(), 1776);
        assert_eq!(std::mem::size_of::<LineUniform>(), 48);
        assert_eq!(std::mem::size_of::<CompositeUniform>(), 32);
        assert_eq!(std::mem::size_of::<OutputUniform>(), 80);
    }

    #[test]
    fn fit_letterboxes_or_pillarboxes() {
        // Same aspect → fills the canvas exactly (legacy parity).
        assert_eq!(
            fit_size((1920, 1080), (1280.0, 720.0)),
            [1920.0 / 1080.0, 1.0]
        );
        // 9:16 in 16:9 → full height, narrow width (pillarbox).
        let f = fit_size((1920, 1080), (1080.0, 1920.0));
        assert!((f[0] - 1080.0 / 1920.0).abs() < 1e-6 && f[1] == 1.0);
        // 2.39:1 in 16:9 → full width, shorter (letterbox).
        let f = fit_size((1920, 1080), (2390.0, 1000.0));
        assert!((f[0] - 1920.0 / 1080.0).abs() < 1e-6 && f[1] < 1.0);
    }

    #[test]
    fn a_fit_within_a_pixel_fills_the_canvas() {
        let full = |c: (u32, u32)| [c.0 as f32 / c.1 as f32, 1.0];
        // Odd clips on their even-rounded canvases (0.78 / 1.4 px short).
        assert_eq!(fit_size((1278, 718), (1279.0, 719.0)), full((1278, 718)));
        assert_eq!(fit_size((718, 1278), (719.0, 1279.0)), full((718, 1278)));
        assert_eq!(fit_size((2048, 856), (2049.0, 857.0)), full((2048, 856)));
        // A real letterbox is kept, and a tiny canvas is never stretched.
        assert!(fit_size((1920, 1080), (1920.0, 1079.0))[1] == 1.0);
        assert!(fit_size((1920, 1080), (1920.0, 1076.0))[1] < 1.0);
        assert!(fit_size((4, 3), (3.0, 3.0))[0] < 4.0 / 3.0);
    }

    #[test]
    fn choke_octagon_is_near_regular_and_fills_its_lattice() {
        for (c, want) in [
            (1.0, (1.0, 0.0)),
            (2.0, (2.0, 0.0)),
            (2.5, (2.5, 0.0)),
            (3.0, (1.0, 1.0)),
            (-40.0, (16.0, 12.0)),
            (64.0, (26.0, 19.0)),
        ] {
            assert_eq!(choke_octagon(c, 1.0), (want.0, want.0, want.1), "choke {c}");
        }
        // 4:3 SAR, 10 display px: (1.5 + 2·3)·4/3 = 10 across, 4 + 2·3 down.
        assert_eq!(choke_octagon(10.0, 4.0 / 3.0), (1.5, 4.0, 3.0));
        for pa in [1.0f32, 4.0 / 3.0, 8.0 / 9.0, 32.0 / 27.0, 2.0] {
            for i in 1..=640 {
                let c = i as f32 * 0.1;
                let (ax, ay, d) = choke_octagon(c, pa);
                // Display reach along each axis is the choke.
                assert!(((ax + 2.0 * d) * pa - c).abs() < 1e-3, "x reach {c} @ {pa}");
                assert!((ay + 2.0 * d - c).abs() < 1e-4, "y reach {c} @ {pa}");
                assert!(
                    d == 0.0 || (ax >= 1.0 && ay >= 1.0),
                    "lattice gaps {c} @ {pa}"
                );
                assert_eq!(d, d.floor());
            }
        }
    }

    #[test]
    fn matte_passes_plan_display_pixel_radii() {
        let k = KeyerParams {
            key_rgb: [0.0, 1.0, 0.0],
            balance: 0.5,
            strength: 1.0,
            clip_black: 0.0,
            clip_white: 0.9,
            choke_px: -10.0,
            soften_px: 12.0,
            spill: 1.0,
            spill_bias: 0.5,
            invert: false,
        };
        let passes = matte_passes(&k, 4.0 / 3.0, (120, 90), true);
        let plan: Vec<([i32; 3], f32)> = passes
            .iter()
            .map(|(u, _)| ([u.dir_mode[0], u.dir_mode[1], u.dir_mode[2]], u.radius[0]))
            .collect();
        // Dilate 10 display px: (1.5, 4, 3) source px; soften 12 display px
        // = 9 source px across (boxes) and 12 down (boxes).
        assert_eq!(plan[0], ([1, 0, MATTE_DILATE], 1.5));
        assert_eq!(plan[1], ([0, 1, MATTE_DILATE], 4.0));
        assert_eq!(plan[2], ([1, 1, MATTE_DILATE], 3.0));
        assert_eq!(plan[3], ([1, -1, MATTE_DILATE], 3.0));
        assert_eq!(plan[4], ([1, 0, MATTE_BOX], soften_box_radius(9.0)));
        assert_eq!(plan[5], ([0, 1, MATTE_BOX], soften_box_radius(12.0)));
        assert_eq!(passes.len(), 6);
        // Small softens stay on the exact taps; every tile has outputs.
        let small = KeyerParams {
            choke_px: 0.0,
            soften_px: 6.0,
            ..k
        };
        let p = matte_passes(&small, 1.0, (16, 16), true);
        assert_eq!(p.len(), 2);
        assert!(p
            .iter()
            .all(|(u, _)| u.dir_mode[2] == MATTE_GAUSS && u.radius[0] == 6.0));
        for (u, _) in matte_passes(
            &KeyerParams {
                choke_px: 64.0,
                soften_px: 64.0,
                ..k
            },
            0.5,
            (8, 8),
            true,
        ) {
            let sub = LINE_SHARED >> u.dir_mode[3];
            assert!(u.dims[3] > 0 && 2 * u.dims[2] + u.dims[3] == sub);
        }
    }

    #[test]
    fn soften_boxes_match_the_exact_kernel_variance() {
        let mut prev = 0.0;
        for i in 85..=1280 {
            let s = i as f32 * 0.1;
            let rho = soften_box_radius(s) as f64;
            let (r, f) = (rho.floor(), rho - rho.floor());
            let var = 3.0 * (r * (r + 1.0) * (2.0 * r + 1.0) / 3.0 + 2.0 * f * (r + 1.0).powi(2))
                / (2.0 * r + 1.0 + 2.0 * f);
            let sigma = s as f64 * 0.5;
            let taps = s.ceil() as i32;
            let (mut m2, mut m0) = (0.0, 0.0);
            for k in -taps..=taps {
                let w = (-0.5 * (k as f64 / sigma).powi(2)).exp();
                m2 += w * (k * k) as f64;
                m0 += w;
            }
            assert!((var / (m2 / m0) - 1.0).abs() < 1e-3, "variance at {s}");
            assert!(rho > prev, "continuous and increasing at {s}");
            prev = rho;
        }
    }

    #[test]
    fn the_dx12_matte_entries_reach_every_barrier() {
        // Round 3: on DX12 no invocation may leave before a workgroup
        // barrier (FXC rejects it, X3663): the plain entry points have no
        // return, nor does a pass before its last barrier; idle workgroups
        // mask their stores instead. (The `_exit` entries may return.)
        let src = wgsl::MATTE;
        let code = |name: &str| {
            let body = &src[src.find(&format!("fn {name}(")).expect(name)..];
            let body = &body[..body.find("\n}\n").expect("end of fn")];
            body.lines()
                .map(|l| l.split("//").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n")
        };
        for name in ["cs_minmax", "cs_gauss", "cs_box"] {
            assert!(!code(name).contains("return"), "{name} returns");
            assert!(code(name).contains("_pass("), "{name} runs its pass");
        }
        for name in ["minmax_pass", "gauss_pass", "box_pass", "prefix_sum"] {
            let c = code(name);
            let last = c.rfind("workgroupBarrier()").expect(name);
            assert!(
                !c[..last].contains("return"),
                "{name} returns before a barrier"
            );
        }
    }

    /// A keyed plate (soft disc on green with a grid of holes) and its key
    /// read back straight, through a compositor using `barrier_safe`.
    fn keyed_readback(barrier_safe: bool, choke: f32, soften: f32, pa: f32) -> Vec<u8> {
        let r = gpu_renderer();
        let mut c = Compositor::new(r.device().clone(), r.queue().clone(), true, barrier_safe);
        let (w, h) = (300u32, 180u32);
        let mut img = RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let d = (((x as f32 - 140.0) / 80.0).powi(2) + ((y as f32 - 90.0) / 50.0).powi(2))
                    .sqrt();
                let t = if (x / 7 + y / 5) % 9 == 0 {
                    0.0
                } else {
                    (1.6 - d).clamp(0.0, 1.0)
                };
                let i = ((y * w + x) * 4) as usize;
                img.pixels[i..i + 4].copy_from_slice(&[
                    (230.0 * t) as u8,
                    (255.0 - 75.0 * t) as u8,
                    (160.0 * t) as u8,
                    255,
                ]);
            }
        }
        let p = LayerParams {
            keyer: Some(KeyerParams {
                key_rgb: [0.0, 1.0, 0.0],
                balance: 0.5,
                strength: 1.0,
                clip_black: 0.0,
                clip_white: 0.9,
                choke_px: choke,
                soften_px: soften,
                spill: 1.0,
                spill_bias: 0.5,
                invert: false,
            }),
            pixel_aspect: pa,
            ..plain(srgb())
        };
        c.begin_frame(&spec(w, h)).unwrap();
        let l = c
            .render_layer(LayerKey(1), LayerSource::Rgba(&img), &p)
            .unwrap();
        let out = c.read_layer_rgba8(l).unwrap();
        c.finish_to_rgba8().unwrap();
        out.pixels
    }

    #[test]
    fn masked_and_early_exit_matte_entries_agree() {
        // The DX12 entry (every workgroup runs, stores masked) and the early
        // exit one give the same bytes.
        for (choke, soften, pa) in [(64.0, 0.0, 1.0), (-12.3, 30.0, 1.0), (10.0, 5.0, 4.0 / 3.0)] {
            assert!(
                keyed_readback(true, choke, soften, pa) == keyed_readback(false, choke, soften, pa),
                "choke {choke} soften {soften} pa {pa}"
            );
        }
    }

    #[test]
    fn sub_lines_pack_short_lines_and_keep_results() {
        // 4K columns (2160) waste a third of 1024-sample workgroups; two
        // 512-sample sub-lines per workgroup waste ~5 %.
        let (u, groups) = line_pass((0, 1), MATTE_ERODE, 26.0, (3840, 2160), true);
        assert_eq!(u.dir_mode[3], 1, "two sub-lines");
        assert_eq!(groups, (1920, 5));
        // Rows of 3840 fill one workgroup's 1024 samples best.
        let (u, groups) = line_pass((1, 0), MATTE_ERODE, 26.0, (3840, 2160), true);
        assert_eq!((u.dir_mode[3], groups), (0, (2160, 4)));
    }

    #[test]
    fn matte_scratch_fits_the_frames_largest_keyed_source() {
        let mut r = gpu_renderer();
        let c = r.compositor();
        let soft = |w, h| {
            let mut img = RgbaImage::new(w, h);
            img.pixels = [0u8, 255, 0, 255].repeat((w * h) as usize);
            img
        };
        let keyed = LayerParams {
            keyer: Some(KeyerParams {
                key_rgb: [0.0, 1.0, 0.0],
                balance: 0.5,
                strength: 1.0,
                clip_black: 0.0,
                clip_white: 0.9,
                choke_px: 0.0,
                soften_px: 20.0,
                spill: 1.0,
                spill_bias: 0.5,
                invert: false,
            }),
            ..plain(srgb())
        };
        let frame = |c: &mut Compositor, canvas: (u32, u32), srcs: &[(u32, u32)]| {
            c.begin_frame(&spec(canvas.0, canvas.1)).unwrap();
            for (i, &(w, h)) in srcs.iter().enumerate() {
                let img = soft(w, h);
                let l = c
                    .render_layer(LayerKey(i as u128), LayerSource::Rgba(&img), &keyed)
                    .unwrap();
                c.composite(
                    l,
                    CompositeOp {
                        blend_mode: 0,
                        opacity: 1.0,
                    },
                )
                .unwrap();
            }
            c.finish_to_rgba8().unwrap();
            c.matte_tmp.as_ref().map(|t| (t[0].width(), t[0].height()))
        };
        assert_eq!(frame(c, (64, 64), &[(64, 64), (32, 48)]), Some((64, 64)));
        // A much smaller keyed source shrinks the scratch to it …
        assert_eq!(frame(c, (64, 64), &[(16, 16)]), Some((16, 16)));
        // … a slightly smaller one keeps it …
        assert_eq!(frame(c, (64, 64), &[(14, 15)]), Some((16, 16)));
        // … a frame with no keyed refinement releases it …
        assert_eq!(frame(c, (64, 64), &[]), None);
        assert_eq!(frame(c, (64, 64), &[(40, 20)]), Some((40, 20)));
        // … and so does a canvas-size change.
        c.begin_frame(&spec(32, 32)).unwrap();
        assert!(c.matte_tmp.is_none(), "canvas resize releases the scratch");
        c.finish_to_rgba8().unwrap();
    }

    #[test]
    fn keyer_fallbacks_are_the_effect_defaults() {
        let k = sanitize_keyer(KeyerParams {
            key_rgb: [f32::NAN; 3],
            balance: f32::NAN,
            strength: f32::NAN,
            clip_black: f32::NAN,
            clip_white: f32::NAN,
            choke_px: f32::INFINITY,
            soften_px: f32::NAN,
            spill: f32::NAN,
            spill_bias: f32::NAN,
            invert: false,
        });
        assert_eq!(k.key_rgb, [0.0, 1.0, 0.0]);
        // Decision 3: Clip White defaults to 0.9.
        assert_eq!((k.clip_black, k.clip_white), (0.0, 0.9));
        assert_eq!((k.choke_px, k.soften_px), (0.0, 0.0));
        assert_eq!(
            (k.balance, k.strength, k.spill, k.spill_bias),
            (0.5, 1.0, 1.0, 0.5)
        );
    }

    fn gpu_renderer() -> crate::Renderer {
        crate::Renderer::new(crate::RendererConfig::default()).expect("renderer")
    }

    fn spec(w: u32, h: u32) -> OutputSpec {
        OutputSpec {
            width: w,
            height: h,
            working_to_output: vance_color::Transform::IDENTITY,
            output_transfer: 1,
            hdr: false,
            view_lut: false,
            transparent: false,
            output_primaries: vance_color::Primaries::Bt709,
        }
    }

    fn plain(pipeline: vance_color::PipelineParams) -> LayerParams {
        LayerParams {
            pipeline,
            translate: [0.0; 2],
            scale: [1.0; 2],
            rotation: 0.0,
            crop_lrtb: [0.0; 4],
            brightness: 0.0,
            contrast: 1.0,
            chroma_key: None,
            luma_key: None,
            keyer: None,
            grades: Vec::new(),
            lut: None,
            matte_view: false,
            pixel_aspect: 1.0,
        }
    }

    fn srgb_image(w: u32, h: u32) -> RgbaImage {
        let mut img = RgbaImage::new(w, h);
        img.pixels = [200u8, 120, 40, 255].repeat((w * h) as usize);
        img
    }

    fn srgb() -> vance_color::PipelineParams {
        vance_color::Pipeline {
            input: vance_color::ColorSpace::Srgb,
            working: vance_color::ColorSpace::LinearRec709,
            output: vance_color::ColorSpace::Srgb,
        }
        .to_params()
        .unwrap()
    }

    #[test]
    fn fused_layers_allocate_no_slot_output() {
        let mut r = gpu_renderer();
        let c = r.compositor();
        let img = srgb_image(16, 8);
        c.begin_frame(&spec(16, 8)).unwrap();
        let a = c
            .render_layer(LayerKey(1), LayerSource::Rgba(&img), &plain(srgb()))
            .unwrap();
        let b = c
            .render_layer(LayerKey(2), LayerSource::Rgba(&img), &plain(srgb()))
            .unwrap();
        c.composite(
            a,
            CompositeOp {
                blend_mode: 0,
                opacity: 1.0,
            },
        )
        .unwrap();
        assert!(c.slots[&LayerKey(1)].out.is_none(), "fused: no slot output");
        assert!(c.slots[&LayerKey(2)].out.is_none(), "not composited yet");
        // A readback needs the layer on its own.
        c.read_layer_rgba8(b).unwrap();
        assert!(c.slots[&LayerKey(2)].out.is_some(), "readback: slot output");
        c.composite(
            b,
            CompositeOp {
                blend_mode: 3,
                opacity: 1.0,
            },
        )
        .unwrap();
        c.finish_to_rgba8().unwrap();
        let outs = c.slots.values().filter(|s| s.out.is_some()).count();
        assert_eq!(outs, 1);
    }

    #[test]
    fn readback_cache_keeps_only_the_current_canvas_size() {
        let mut r = gpu_renderer();
        let c = r.compositor();
        for (w, h) in [(16, 8), (32, 8), (16, 16)] {
            let img = srgb_image(w, h);
            c.begin_frame(&spec(w, h)).unwrap();
            let l = c
                .render_layer(LayerKey(1), LayerSource::Rgba(&img), &plain(srgb()))
                .unwrap();
            c.read_layer_rgba8(l).unwrap();
            c.composite(
                l,
                CompositeOp {
                    blend_mode: 0,
                    opacity: 1.0,
                },
            )
            .unwrap();
            c.finish_to_rgba64().unwrap();
            // RGBA8 (layer readback) + RGBA16F (rgba64) at this size only.
            assert_eq!(c.readback.len(), 2, "{w}x{h}");
            assert!(c.readback.keys().all(|&(_, rw, rh)| (rw, rh) == (w, h)));
        }
    }

    #[test]
    fn ten_bit_fallback_matches_the_r16_path() {
        let r = gpu_renderer();
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
        let pipeline = vance_color::Pipeline::default().to_params().unwrap();
        let run = |r16: bool| {
            let mut c = Compositor::new(r.device().clone(), r.queue().clone(), r16, false);
            c.begin_frame(&spec(w, h)).unwrap();
            let l = c
                .render_layer(LayerKey(1), LayerSource::Yuv(&frame), &plain(pipeline))
                .unwrap();
            c.composite(
                l,
                CompositeOp {
                    blend_mode: 0,
                    opacity: 1.0,
                },
            )
            .unwrap();
            c.finish_to_rgba8().unwrap()
        };
        let (hi, lo) = (run(true), run(false));
        let d = hi
            .iter()
            .zip(&lo)
            .map(|(a, b)| (*a as i32 - *b as i32).abs())
            .max()
            .unwrap();
        // The fallback decodes the same 10-bit codes (two bytes per texel,
        // filtered in the shader), so the paths differ only by float
        // rounding at the 8-bit output. Luma sits on texel centres and
        // chroma on quarter-texel offsets here, exact at any filter
        // precision a conformant GPU has (≥ 4 sub-texel bits).
        assert!(d <= 1, "10-bit fallback vs R16: max diff {d}");
    }

    #[test]
    fn keyer_lanes_pick_primary_and_normalise() {
        let k = KeyerParams {
            key_rgb: [0.1, 0.9, 0.2],
            balance: 0.5,
            strength: 1.0,
            clip_black: 0.0,
            clip_white: 1.0,
            choke_px: 0.0,
            soften_px: 0.0,
            spill: 1.0,
            spill_bias: 0.5,
            invert: false,
        };
        let [p, s1, s2, ..] = keyer_lanes(&k);
        assert_eq!(&p[..3], &[0.0, 1.0, 0.0]);
        assert!(
            (p[3] - (0.9 - 0.15)).abs() < 1e-6,
            "key difference {}",
            p[3]
        );
        assert_eq!(&s1[..3], &[1.0, 0.0, 0.0]);
        assert_eq!(&s2[..3], &[0.0, 0.0, 1.0]);
        // A grey key has no primary excess: the keyer goes inert.
        let grey = KeyerParams {
            key_rgb: [0.5; 3],
            ..k
        };
        assert_eq!(keyer_lanes(&grey)[0][3], 0.0);
    }
}
