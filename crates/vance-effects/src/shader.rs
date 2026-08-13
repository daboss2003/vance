//! The WGSL shader-effect executor (M6a). Standalone (owns a headless GPU
//! context) or built on a shared device; validates the user's `effect()` via
//! `wgpu`/`naga` at compile time and runs it over an RGBA buffer.

use std::borrow::Cow;

/// Max user parameters (packed as 16 × `vec4<f32>` in the uniform block).
pub const MAX_PARAMS: usize = 64;

/// The fixed WGSL prelude wrapped around the user's `effect()` function. The
/// user samples the source via `color` and reads params via `param(i)`.
fn assemble(effect_body: &str) -> String {
    format!(
        r#"struct VsOut {{ @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> }};

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct Params {{ v: array<vec4<f32>, {vecs}u> }};
@group(0) @binding(2) var<uniform> P: Params;

fn param(i: i32) -> f32 {{ return P.v[i / 4][i % 4]; }}

@vertex
fn vs(@builtin(vertex_index) idx: u32) -> VsOut {{
    var uvs = array<vec2<f32>, 3>(vec2<f32>(0.0, 0.0), vec2<f32>(2.0, 0.0), vec2<f32>(0.0, 2.0));
    var out: VsOut;
    out.uv = uvs[idx];
    out.pos = vec4<f32>(uvs[idx] * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    return out;
}}

{user}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {{
    let color = textureSample(src, samp, in.uv);
    return effect(color, in.uv);
}}
"#,
        vecs = MAX_PARAMS / 4,
        user = effect_body,
    )
}

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

pub struct ShaderEffect {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl ShaderEffect {
    /// Validate the user's `effect()` (naga only — no GPU). Cheap; used by the
    /// Effect Studio's save path to surface compile errors.
    pub fn validate(effect_body: &str) -> Result<(), String> {
        validate_wgsl(&assemble(effect_body))
    }

    /// Compile with a fresh headless GPU context (used by tests + preview).
    pub fn compile(effect_body: &str) -> Result<ShaderEffect, String> {
        let (device, queue) = headless_device()?;
        Self::compile_with(device, queue, effect_body)
    }

    /// Compile on a caller-provided device (the engine shares its render device).
    pub fn compile_with(
        device: wgpu::Device,
        queue: wgpu::Queue,
        effect_body: &str,
    ) -> Result<ShaderEffect, String> {
        let src = assemble(effect_body);
        // Validate first so a bad shader is a clean error, not a device panic.
        validate_wgsl(&src)?;
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vance.shaderfx"),
            source: wgpu::ShaderSource::Wgsl(Cow::Owned(src)),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vance.shaderfx.layout"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vance.shaderfx.pl"),
            bind_group_layouts: &[Some(&layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vance.shaderfx.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vance.shaderfx.sampler"),
            ..Default::default()
        });
        Ok(ShaderEffect {
            device,
            queue,
            pipeline,
            layout,
            sampler,
        })
    }

    /// Run the effect over `rgba` (RGBA8, `w*h*4` bytes), mutating in place.
    pub fn process(&self, rgba: &mut [u8], w: u32, h: u32, params: &[f32]) -> Result<(), String> {
        let expected = (w as usize) * (h as usize) * 4;
        if rgba.len() != expected {
            return Err(format!("frame size {} != {expected}", rgba.len()));
        }
        let size = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let input = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vance.shaderfx.in"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &input,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            size,
        );
        let output = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vance.shaderfx.out"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });

        // Params → uniform (packed f32; unused slots zero).
        let mut packed = [0f32; MAX_PARAMS];
        for (i, v) in params.iter().take(MAX_PARAMS).enumerate() {
            packed[i] = *v;
        }
        let ubo = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vance.shaderfx.params"),
            size: (MAX_PARAMS * 4) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bytes: &[u8] = bytemuck_f32_bytes(&packed);
        self.queue.write_buffer(&ubo, 0, bytes);

        let bind = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vance.shaderfx.bg"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(
                        &input.create_view(&wgpu::TextureViewDescriptor::default()),
                    ),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: ubo.as_entire_binding(),
                },
            ],
        });

        let out_view = output.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vance.shaderfx.enc"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("vance.shaderfx.pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &out_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind, &[]);
            pass.draw(0..3, 0..1);
        }

        // Read back (rows padded to 256 bytes).
        let unpadded = w * 4;
        let padded = (unpadded + 255) & !255;
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vance.shaderfx.staging"),
            size: (padded * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &output,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &staging,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            size,
        );
        self.queue.submit(Some(encoder.finish()));

        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: None,
                timeout: None,
            })
            .ok();
        rx.recv()
            .map_err(|e| format!("map recv: {e}"))?
            .map_err(|e| format!("map: {e}"))?;
        let mapped = slice.get_mapped_range();
        for row in 0..h {
            let src = (row * padded) as usize;
            let dst = (row * unpadded) as usize;
            rgba[dst..dst + unpadded as usize]
                .copy_from_slice(&mapped[src..src + unpadded as usize]);
        }
        drop(mapped);
        staging.unmap();
        Ok(())
    }
}

/// Parse + validate the assembled WGSL, returning a readable compile error.
fn validate_wgsl(src: &str) -> Result<(), String> {
    let module = naga::front::wgsl::parse_str(src)
        .map_err(|e| format!("shader compile error:\n{}", e.emit_to_string(src)))?;
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    )
    .validate(&module)
    .map_err(|e| format!("shader compile error:\n{}", e.emit_to_string(src)))?;
    Ok(())
}

fn bytemuck_f32_bytes(v: &[f32]) -> &[u8] {
    // Safe: f32 has no padding/invalid bit patterns for read as bytes.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn headless_device() -> Result<(wgpu::Device, wgpu::Queue), String> {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::all();
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .map_err(|_| "no GPU adapter available".to_string())?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("vance.shaderfx.device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_defaults(),
        experimental_features: wgpu::ExperimentalFeatures::default(),
        memory_hints: wgpu::MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }))
    .map_err(|e| format!("request_device: {e}"))?;
    Ok((device, queue))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maybe(body: &str) -> Option<ShaderEffect> {
        match ShaderEffect::compile(body) {
            Ok(fx) => Some(fx),
            Err(e) if e.contains("adapter") => {
                eprintln!("no GPU — skipping shader test");
                None
            }
            Err(e) => panic!("unexpected compile error: {e}"),
        }
    }

    #[test]
    fn brightness_shader_scales_pixels() {
        let Some(fx) = maybe(
            "fn effect(color: vec4<f32>, uv: vec2<f32>) -> vec4<f32> { return vec4<f32>(color.rgb * param(0), color.a); }",
        ) else {
            return;
        };
        // 2×2 solid gray (100), alpha 255.
        let mut rgba = vec![100u8, 100, 100, 255].repeat(4);
        fx.process(&mut rgba, 2, 2, &[0.5]).unwrap();
        // 100/255 * 0.5 * 255 ≈ 50 (allow GPU rounding).
        assert!((rgba[0] as i32 - 50).abs() <= 1, "got {}", rgba[0]);
        assert_eq!(rgba[3], 255, "alpha preserved");
    }

    #[test]
    fn invalid_shader_reports_compile_error() {
        // `no_such_fn` isn't defined → validation error, not a panic.
        match ShaderEffect::compile(
            "fn effect(color: vec4<f32>, uv: vec2<f32>) -> vec4<f32> { return no_such_fn(color); }",
        ) {
            Err(e) if e.contains("adapter") => eprintln!("no GPU — skipping"),
            Err(e) => assert!(e.contains("compile error"), "got: {e}"),
            Ok(_) => panic!("expected a compile error"),
        }
    }
}
