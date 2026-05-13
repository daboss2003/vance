//! The wgpu-side renderer.

use thiserror::Error;
use vance_decoder::{FrameData, PixelFormat};

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
/// the input dimensions change.
///
/// The renderer always binds *two* YUV plane sets (A and B). Single-input
/// playback uses A only and leaves B's mix factor at 0 — the shader just
/// returns A. Cross-dissolve transitions (Phase 5.4) upload both sides
/// and dial `mix_factor` from 0 → 1 to blend.
pub struct Renderer {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// (width, height) of the currently-allocated A-side Y texture;
    /// `None` if no frame has been uploaded yet.
    plane_dims_a: Option<(u32, u32)>,
    tex_y_a: Option<wgpu::Texture>,
    tex_u_a: Option<wgpu::Texture>,
    tex_v_a: Option<wgpu::Texture>,
    /// Same, for the B side (transition target).
    plane_dims_b: Option<(u32, u32)>,
    tex_y_b: Option<wgpu::Texture>,
    tex_u_b: Option<wgpu::Texture>,
    tex_v_b: Option<wgpu::Texture>,
    /// Loaded 3D LUT — uploaded into a 3D texture sampled by the
    /// fragment shader. `None` until the first `upload_lut` call;
    /// the bind group binds a 1×1×1 dummy when no LUT is in use,
    /// gated by `lut_params.enable`.
    tex_lut: Option<wgpu::Texture>,
    /// Size of the currently-uploaded LUT, or `1` when only the
    /// dummy is bound. Used to skip a re-upload when the new LUT
    /// has the same dimensions.
    lut_size: u32,
    bind_group: Option<wgpu::BindGroup>,
    /// Per-frame parameter uniform — opacity (5.2), transform (5.3),
    /// crop + brightness/contrast (5.6), and the transition mix
    /// factor (5.4). The WGSL `Params` struct must keep its layout
    /// in sync.
    params_buffer: wgpu::Buffer,
    current_params: RenderParams,
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
    /// Cross-dissolve mix factor between the A and B YUV inputs.
    /// 0 = pure A (no transition); 1 = pure B; 0.5 = midpoint.
    /// All other per-clip effects (opacity / transform / crop /
    /// color) apply *after* the mix.
    mix_factor: f32,
    _pad0: [f32; 3],
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
            mix_factor: 0.0,
            _pad0: [0.0; 3],
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
        }
    }
}

#[inline]
fn identity_row3() -> [[f32; 4]; 3] {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
    ]
}

#[inline]
fn matrix_to_row3(t: &vance_color::Transform) -> [[f32; 4]; 3] {
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
fn f32_to_f16_bits(v: f32) -> u16 {
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
                yuv_plane_entry(5),
                yuv_plane_entry(6),
                yuv_plane_entry(7),
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
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vance-render.pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            ..Default::default()
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vance-render.yuv-rec709-limited"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
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

        Ok(Self {
            adapter,
            device,
            queue,
            pipeline,
            bind_group_layout,
            sampler,
            plane_dims_a: None,
            tex_y_a: None,
            tex_u_a: None,
            tex_v_a: None,
            plane_dims_b: None,
            tex_y_b: None,
            tex_u_b: None,
            tex_v_b: None,
            tex_lut: None,
            lut_size: 1,
            bind_group: None,
            params_buffer,
            current_params: initial_params,
        })
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
        let mut l = left.clamp(0.0, 1.0);
        let mut r = right.clamp(0.0, 1.0);
        let mut t = top.clamp(0.0, 1.0);
        let mut b = bottom.clamp(0.0, 1.0);
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
        let new = [l, r, t, b];
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
    pub fn set_transform(
        &mut self,
        translate: [f32; 2],
        scale: [f32; 2],
        rotation_radians: f32,
    ) {
        let new_scale = [
            if scale[0].abs() < 1e-4 { 1e-4 } else { scale[0] },
            if scale[1].abs() < 1e-4 { 1e-4 } else { scale[1] },
        ];
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

    /// Upload a decoded frame into the renderer's A-side YUV plane textures.
    /// Splits NV12 UV into separate U and V planes CPU-side so both formats
    /// share the same shader path. Single-input playback only ever calls
    /// this side; the B-side stays as the most recent upload (or a 1×1
    /// dummy) and the shader ignores it while `mix_factor == 0`.
    pub fn upload_frame(&mut self, frame: &FrameData) -> Result<(), RenderError> {
        self.upload_frame_inner(frame, false)
    }

    /// Upload a decoded frame onto the B-side YUV plane textures.
    /// Used by cross-dissolve transitions for the outgoing (or
    /// incoming, depending on direction) clip while A holds the other.
    pub fn upload_frame_b(&mut self, frame: &FrameData) -> Result<(), RenderError> {
        self.upload_frame_inner(frame, true)
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
        let n = size as usize;
        let mut packed: Vec<u16> = Vec::with_capacity(n * n * n * 4);
        for triple in lut.values.chunks_exact(3) {
            packed.push(f32_to_f16_bits(triple[0]));
            packed.push(f32_to_f16_bits(triple[1]));
            packed.push(f32_to_f16_bits(triple[2]));
            packed.push(f32_to_f16_bits(1.0));
        }
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
        let same_flags = (0..4).all(|i| self.current_params.pipeline_flags[i] == new_flags[i]);
        let same_in = matrices_equal(&self.current_params.pipeline_in_to_working, &in_to_working);
        let same_out =
            matrices_equal(&self.current_params.pipeline_working_to_out, &working_to_out);
        if same_flags && same_in && same_out {
            return;
        }
        self.current_params.pipeline_flags = new_flags;
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
        let same_key = (0..4).all(|i| {
            (self.current_params.chroma_key[i] - new_key[i]).abs() < 1e-5
        });
        let same_params = (0..2).all(|i| {
            (self.current_params.chroma_params[i] - new_params[i]).abs() < 1e-5
        });
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
    pub fn set_luma_key(
        &mut self,
        threshold: f32,
        direction: f32,
        softness: f32,
        enable: f32,
    ) {
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

    /// Set the A↔B mix factor. 0 = pure A (no transition), 1 = pure B,
    /// anything in between is a linear cross-dissolve. The renderer
    /// initializes this to 0, so the single-input path is unaffected
    /// until a transition explicitly opts in.
    pub fn set_mix_factor(&mut self, value: f32) {
        let v = value.clamp(0.0, 1.0);
        if (self.current_params.mix_factor - v).abs() < 1e-4 {
            return;
        }
        self.current_params.mix_factor = v;
        self.queue.write_buffer(
            &self.params_buffer,
            0,
            bytemuck::cast_slice(&[self.current_params]),
        );
    }

    fn upload_frame_inner(
        &mut self,
        frame: &FrameData,
        b_side: bool,
    ) -> Result<(), RenderError> {
        let (w, h) = (frame.width, frame.height);
        let expected = frame.pixel_format.frame_size(w, h);
        if frame.bytes.len() != expected {
            return Err(RenderError::MalformedFrame {
                expected,
                got: frame.bytes.len(),
            });
        }

        let needs_alloc = if b_side {
            self.plane_dims_b != Some((w, h))
        } else {
            self.plane_dims_a != Some((w, h))
        };
        if needs_alloc {
            self.allocate_plane_textures(w, h, b_side);
            if b_side {
                self.plane_dims_b = Some((w, h));
            } else {
                self.plane_dims_a = Some((w, h));
            }
        }

        let chroma_w = w.div_ceil(2);
        let chroma_h = h.div_ceil(2);

        let (tex_y, tex_u, tex_v) = if b_side {
            (
                self.tex_y_b.as_ref().unwrap(),
                self.tex_u_b.as_ref().unwrap(),
                self.tex_v_b.as_ref().unwrap(),
            )
        } else {
            (
                self.tex_y_a.as_ref().unwrap(),
                self.tex_u_a.as_ref().unwrap(),
                self.tex_v_a.as_ref().unwrap(),
            )
        };

        match frame.pixel_format {
            PixelFormat::Yuv420P => {
                let y_size = (w * h) as usize;
                let chroma_size = (chroma_w * chroma_h) as usize;
                let (y_bytes, rest) = frame.bytes.split_at(y_size);
                let (u_bytes, v_bytes) = rest.split_at(chroma_size);
                self.write_plane(tex_y, w, h, y_bytes);
                self.write_plane(tex_u, chroma_w, chroma_h, u_bytes);
                self.write_plane(tex_v, chroma_w, chroma_h, v_bytes);
            }
            PixelFormat::Nv12 => {
                let y_size = (w * h) as usize;
                let uv_size = (chroma_w * chroma_h * 2) as usize;
                let (y_bytes, uv_bytes) = frame.bytes.split_at(y_size);
                debug_assert_eq!(uv_bytes.len(), uv_size);
                let mut u_buf = Vec::with_capacity((chroma_w * chroma_h) as usize);
                let mut v_buf = Vec::with_capacity((chroma_w * chroma_h) as usize);
                for chunk in uv_bytes.chunks_exact(2) {
                    u_buf.push(chunk[0]);
                    v_buf.push(chunk[1]);
                }
                self.write_plane(tex_y, w, h, y_bytes);
                self.write_plane(tex_u, chroma_w, chroma_h, &u_buf);
                self.write_plane(tex_v, chroma_w, chroma_h, &v_buf);
            }
        }
        Ok(())
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

    fn allocate_plane_textures(&mut self, width: u32, height: u32, b_side: bool) {
        let chroma_w = width.div_ceil(2);
        let chroma_h = height.div_ceil(2);
        if b_side {
            self.tex_y_b = Some(create_r8_texture(&self.device, width, height, "tex_y_b"));
            self.tex_u_b = Some(create_r8_texture(&self.device, chroma_w, chroma_h, "tex_u_b"));
            self.tex_v_b = Some(create_r8_texture(&self.device, chroma_w, chroma_h, "tex_v_b"));
        } else {
            self.tex_y_a = Some(create_r8_texture(&self.device, width, height, "tex_y_a"));
            self.tex_u_a = Some(create_r8_texture(&self.device, chroma_w, chroma_h, "tex_u_a"));
            self.tex_v_a = Some(create_r8_texture(&self.device, chroma_w, chroma_h, "tex_v_a"));
        }

        // First A-side allocation also primes a 1×1 B-side dummy so
        // the bind group always has valid views to bind. After that
        // first call, subsequent allocations only swap the side that
        // resized.
        if !b_side && self.tex_y_b.is_none() {
            self.tex_y_b = Some(create_r8_texture(&self.device, 1, 1, "tex_y_b_dummy"));
            self.tex_u_b = Some(create_r8_texture(&self.device, 1, 1, "tex_u_b_dummy"));
            self.tex_v_b = Some(create_r8_texture(&self.device, 1, 1, "tex_v_b_dummy"));
            self.plane_dims_b = Some((1, 1));
        }

        self.rebuild_bind_group();
    }

    fn rebuild_bind_group(&mut self) {
        let make_view =
            |t: &Option<wgpu::Texture>| t.as_ref().unwrap().create_view(&Default::default());
        let view_y_a = make_view(&self.tex_y_a);
        let view_u_a = make_view(&self.tex_u_a);
        let view_v_a = make_view(&self.tex_v_a);
        let view_y_b = make_view(&self.tex_y_b);
        let view_u_b = make_view(&self.tex_u_b);
        let view_v_b = make_view(&self.tex_v_b);
        // LUT slot — prime a 1×1×1 dummy so the bind group is valid
        // before the user loads anything. The shader gates against
        // `lut_params.enable`, so the dummy never contributes.
        if self.tex_lut.is_none() {
            self.tex_lut = Some(self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("vance-render.tex_lut_dummy"),
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
            }));
            self.lut_size = 1;
        }
        let view_lut = self
            .tex_lut
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
                    resource: wgpu::BindingResource::TextureView(&view_y_a),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&view_u_a),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::TextureView(&view_v_a),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.params_buffer.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 5,
                    resource: wgpu::BindingResource::TextureView(&view_y_b),
                },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(&view_u_b),
                },
                wgpu::BindGroupEntry {
                    binding: 7,
                    resource: wgpu::BindingResource::TextureView(&view_v_b),
                },
                wgpu::BindGroupEntry {
                    binding: 8,
                    resource: wgpu::BindingResource::TextureView(&view_lut),
                },
            ],
        }));
    }

    fn write_plane(&self, tex: &wgpu::Texture, w: u32, h: u32, bytes: &[u8]) {
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
    }
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
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("vance-render.device"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        })
        .await
        .map_err(|e| RenderError::DeviceRequest(format!("{e}")))?;
    Ok((adapter, device, queue))
}

fn yuv_plane_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
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

fn create_r8_texture(device: &wgpu::Device, w: u32, h: u32, label: &str) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}
