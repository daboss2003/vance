//! The Engine ties the four Phase-1 crates together inside the host process:
//!
//! - `vance-viewport`: owns the native overlay surface
//! - `vance-render`: owns the wgpu device + YUV→RGB pipeline
//! - `vance-decoder`: a sidecar process producing decoded frames
//!
//! Lives in a `Mutex` inside Tauri's app state. Methods are blocking and
//! intended to be called from Tauri command handlers.

use parking_lot::Mutex;
use std::path::PathBuf;
use std::sync::Arc;
use vance_decoder::{DecoderClient, StreamInfo};
use vance_render::{Renderer, RendererConfig};
use vance_viewport::{HostWindowHandle, Viewport, ViewportBounds};

/// Errors surfaced through Tauri command handlers as plain strings.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("viewport: {0}")]
    Viewport(#[from] vance_viewport::ViewportError),

    #[error("render: {0}")]
    Render(#[from] vance_render::RenderError),

    #[error("decoder: {0}")]
    DecoderClient(#[from] vance_decoder::client::ClientError),

    #[error("decoder spawn: {0}")]
    DecoderSpawn(String),

    #[error("decoder binary not found at any of: {0:?}")]
    DecoderBinaryMissing(Vec<PathBuf>),

    #[error("no surface texture available: {0}")]
    SurfaceAcquire(String),

    #[error("no file opened")]
    NoFileOpen,

    #[error("viewport not yet installed")]
    NoViewport,
}

/// Shared, internally-mutable engine handle. Cheap to clone.
#[derive(Clone, Default)]
pub struct EngineHandle(Arc<Mutex<Engine>>);

impl EngineHandle {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, Engine> {
        self.0.lock()
    }
}

#[derive(Default)]
pub struct Engine {
    /// Set once after the Tauri window exists.
    viewport: Option<Viewport>,
    /// Created lazily after the viewport (so it can share an Instance/Adapter).
    renderer: Option<Renderer>,
    /// Cached surface configuration; reconfigured when bounds change.
    surface_config: Option<wgpu::SurfaceConfiguration>,
    /// Decoder sidecar; spawned on first `open_file`.
    decoder: Option<DecoderClient>,
    /// Stream info from the most recent successful `open_file`.
    stream_info: Option<StreamInfo>,
    /// Last-applied viewport bounds (in physical pixels).
    last_bounds: ViewportBounds,
}

impl Engine {
    /// Install the viewport + create a renderer that shares its instance.
    /// Called once from the Tauri `setup` callback on the main thread.
    pub fn install_viewport(&mut self, host: HostWindowHandle) -> Result<(), EngineError> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(desc);

        let viewport = Viewport::install(host, &instance)?;

        // Build the renderer first with a temporary format; the renderer
        // owns the adapter we'll use for all subsequent surface queries
        // so the pipeline's color target and the surface's configured
        // format are guaranteed to match.
        let mut renderer = Renderer::new_with(
            &instance,
            viewport.surface(),
            RendererConfig {
                target_format: wgpu::TextureFormat::Bgra8UnormSrgb,
                backends: wgpu::Backends::PRIMARY,
            },
        )?;

        // Now ask the renderer's own adapter for the surface's recommended
        // format and rebuild the pipeline if it differs.
        if let Some(rec) = renderer.recommended_surface_config(viewport.surface(), 1, 1) {
            if rec.format != wgpu::TextureFormat::Bgra8UnormSrgb {
                tracing::info!("rebuilding renderer for surface format {:?}", rec.format);
                renderer = Renderer::new_with(
                    &instance,
                    viewport.surface(),
                    RendererConfig {
                        target_format: rec.format,
                        backends: wgpu::Backends::PRIMARY,
                    },
                )?;
            }
        }

        self.viewport = Some(viewport);
        self.renderer = Some(renderer);
        Ok(())
    }

    /// Update the overlay's bounds and reconfigure the surface to match.
    pub fn set_viewport_bounds(&mut self, bounds: ViewportBounds) -> Result<(), EngineError> {
        let viewport = self.viewport.as_ref().ok_or(EngineError::NoViewport)?;
        viewport.set_bounds(bounds);
        self.last_bounds = bounds;

        if !bounds.is_empty() {
            if let Some(renderer) = self.renderer.as_ref() {
                if let Some(cfg) =
                    renderer.recommended_surface_config(viewport.surface(), bounds.w, bounds.h)
                {
                    renderer.configure_surface(viewport.surface(), &cfg);
                    self.surface_config = Some(cfg);
                }
            }
        }
        Ok(())
    }

    /// Spawn the decoder sidecar (if not already running) and open `path`.
    pub fn open_file(&mut self, path: &str) -> Result<StreamInfo, EngineError> {
        if self.decoder.is_none() {
            let bin = locate_decoder_binary()?;
            tracing::info!(bin = %bin.display(), "spawning decoder sidecar");
            let client = DecoderClient::spawn(&bin)
                .map_err(|e| EngineError::DecoderSpawn(format!("{e}")))?;
            tracing::info!(version = %client.ffmpeg_version(), "decoder up");
            self.decoder = Some(client);
        }
        let dec = self.decoder.as_mut().unwrap();
        let info = dec.open(path.to_string())?;
        self.stream_info = Some(info.clone());
        Ok(info)
    }

    /// Decode the frame at `target_pts` (stream time base) and render it
    /// into the viewport's surface.
    pub fn decode_and_render_at(&mut self, target_pts: i64) -> Result<i64, EngineError> {
        let dec = self.decoder.as_mut().ok_or(EngineError::NoFileOpen)?;
        let frame = dec.decode_at(target_pts)?;
        let pts = frame.pts;
        tracing::info!(
            requested_pts = target_pts,
            actual_pts = pts,
            w = frame.width,
            h = frame.height,
            bytes = frame.bytes.len(),
            "decoded frame"
        );

        let renderer = self.renderer.as_mut().ok_or(EngineError::NoViewport)?;
        renderer.upload_frame(&frame)?;

        let viewport = self.viewport.as_ref().ok_or(EngineError::NoViewport)?;
        let surface = viewport.surface();
        tracing::info!(
            bounds_w = self.last_bounds.w,
            bounds_h = self.last_bounds.h,
            cfg_w = self.surface_config.as_ref().map(|c| c.width).unwrap_or(0),
            cfg_h = self.surface_config.as_ref().map(|c| c.height).unwrap_or(0),
            "render bounds"
        );

        // Make sure the surface is configured at the current bounds.
        if self.surface_config.is_none() && !self.last_bounds.is_empty() {
            if let Some(cfg) =
                renderer.recommended_surface_config(surface, self.last_bounds.w, self.last_bounds.h)
            {
                renderer.configure_surface(surface, &cfg);
                self.surface_config = Some(cfg);
            }
        }
        if self.surface_config.is_none() {
            return Err(EngineError::SurfaceAcquire(
                "viewport bounds not yet reported by the chrome".into(),
            ));
        }

        let surface_tex = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated
            | wgpu::CurrentSurfaceTexture::Lost
            | wgpu::CurrentSurfaceTexture::Validation => {
                // Force reconfigure on next frame; fail this one with a hint.
                self.surface_config = None;
                return Err(EngineError::SurfaceAcquire(
                    "surface outdated; will reconfigure next frame".into(),
                ));
            }
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Err(EngineError::SurfaceAcquire(
                    "surface timeout/occluded".into(),
                ));
            }
        };
        let view = surface_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        renderer.render_to(&view)?;
        surface_tex.present();
        tracing::info!(pts, "frame presented");
        Ok(pts)
    }
}

/// Try a list of locations for the `vance-decoder` binary:
/// 1. Right next to the running `vance-app` executable (production layout).
/// 2. Same directory under `cargo build` (`target/<profile>/`).
/// 3. Fallback: `target/debug/vance-decoder` from a workspace-detected root.
fn locate_decoder_binary() -> Result<PathBuf, EngineError> {
    let exe_name = if cfg!(windows) {
        "vance-decoder.exe"
    } else {
        "vance-decoder"
    };

    let mut tried = Vec::new();
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let candidate = dir.join(exe_name);
            if candidate.exists() {
                return Ok(candidate);
            }
            tried.push(candidate);

            // Cargo's `cargo run` puts the binary under target/<profile>/;
            // the decoder sidecar is built into the same dir.
            let cargo_candidate = dir.join(exe_name);
            if cargo_candidate.exists() {
                return Ok(cargo_candidate);
            }
        }
    }
    Err(EngineError::DecoderBinaryMissing(tried))
}
