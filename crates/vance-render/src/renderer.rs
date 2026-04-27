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
pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// (width, height) of the currently-allocated Y texture; `None` if no
    /// frame has been uploaded yet.
    plane_dims: Option<(u32, u32)>,
    tex_y: Option<wgpu::Texture>,
    tex_u: Option<wgpu::Texture>,
    tex_v: Option<wgpu::Texture>,
    bind_group: Option<wgpu::BindGroup>,
}

impl Renderer {
    /// Create a renderer. Picks a wgpu adapter without an associated surface
    /// (the renderer is surface-agnostic; the *consumer* hands it a target).
    pub fn new(cfg: RendererConfig) -> Result<Self, RenderError> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = cfg.backends;
        let instance = wgpu::Instance::new(desc);
        let (device, queue) = pollster::block_on(create_device(&instance))?;

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

        Ok(Self {
            device,
            queue,
            pipeline,
            bind_group_layout,
            sampler,
            plane_dims: None,
            tex_y: None,
            tex_u: None,
            tex_v: None,
            bind_group: None,
        })
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

    /// Upload a decoded frame into the renderer's YUV plane textures. Splits
    /// NV12 UV into separate U and V planes CPU-side so both formats share
    /// the same shader path.
    pub fn upload_frame(&mut self, frame: &FrameData) -> Result<(), RenderError> {
        let (w, h) = (frame.width, frame.height);
        let expected = frame.pixel_format.frame_size(w, h);
        if frame.bytes.len() != expected {
            return Err(RenderError::MalformedFrame {
                expected,
                got: frame.bytes.len(),
            });
        }

        if self.plane_dims != Some((w, h)) {
            self.allocate_plane_textures(w, h);
            self.plane_dims = Some((w, h));
        }

        let chroma_w = w.div_ceil(2);
        let chroma_h = h.div_ceil(2);

        match frame.pixel_format {
            PixelFormat::Yuv420P => {
                let y_size = (w * h) as usize;
                let chroma_size = (chroma_w * chroma_h) as usize;
                let (y_bytes, rest) = frame.bytes.split_at(y_size);
                let (u_bytes, v_bytes) = rest.split_at(chroma_size);
                self.write_plane(self.tex_y.as_ref().unwrap(), w, h, y_bytes);
                self.write_plane(self.tex_u.as_ref().unwrap(), chroma_w, chroma_h, u_bytes);
                self.write_plane(self.tex_v.as_ref().unwrap(), chroma_w, chroma_h, v_bytes);
            }
            PixelFormat::Nv12 => {
                let y_size = (w * h) as usize;
                let uv_size = (chroma_w * chroma_h * 2) as usize;
                let (y_bytes, uv_bytes) = frame.bytes.split_at(y_size);
                debug_assert_eq!(uv_bytes.len(), uv_size);
                // Split interleaved UV → separate U and V buffers.
                let mut u_buf = Vec::with_capacity((chroma_w * chroma_h) as usize);
                let mut v_buf = Vec::with_capacity((chroma_w * chroma_h) as usize);
                for chunk in uv_bytes.chunks_exact(2) {
                    u_buf.push(chunk[0]);
                    v_buf.push(chunk[1]);
                }
                self.write_plane(self.tex_y.as_ref().unwrap(), w, h, y_bytes);
                self.write_plane(self.tex_u.as_ref().unwrap(), chroma_w, chroma_h, &u_buf);
                self.write_plane(self.tex_v.as_ref().unwrap(), chroma_w, chroma_h, &v_buf);
            }
        }
        Ok(())
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

    fn allocate_plane_textures(&mut self, width: u32, height: u32) {
        let chroma_w = width.div_ceil(2);
        let chroma_h = height.div_ceil(2);
        self.tex_y = Some(create_r8_texture(&self.device, width, height, "tex_y"));
        self.tex_u = Some(create_r8_texture(&self.device, chroma_w, chroma_h, "tex_u"));
        self.tex_v = Some(create_r8_texture(&self.device, chroma_w, chroma_h, "tex_v"));

        let view_y = self
            .tex_y
            .as_ref()
            .unwrap()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let view_u = self
            .tex_u
            .as_ref()
            .unwrap()
            .create_view(&wgpu::TextureViewDescriptor::default());
        let view_v = self
            .tex_v
            .as_ref()
            .unwrap()
            .create_view(&wgpu::TextureViewDescriptor::default());

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
) -> Result<(wgpu::Device, wgpu::Queue), RenderError> {
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
        .map_err(|_| RenderError::NoAdapter)?;
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("vance-render.device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        })
        .await
        .map_err(|e| RenderError::DeviceRequest(format!("{e}")))?;
    Ok((device, queue))
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
