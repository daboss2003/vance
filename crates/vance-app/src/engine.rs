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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
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

    #[error("playback in progress")]
    PlaybackInProgress,

    #[error("no playback session")]
    NoPlayback,

    #[error("playback thread error: {0}")]
    PlaybackThread(String),
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

/// Render-side state shared between the engine and a playback session
/// thread. Wrapped in `Arc<Mutex<...>>` so the scheduler can lock per-frame
/// for upload + render without blocking the engine's other commands when
/// not playing.
pub struct RenderState {
    pub renderer: Renderer,
    pub surface_config: Option<wgpu::SurfaceConfiguration>,
}

#[derive(Default)]
pub struct Engine {
    /// Set once after the Tauri window exists.
    viewport: Option<Arc<Viewport>>,
    /// Renderer + surface config, behind a Mutex so the playback scheduler
    /// can share access without taking the parent EngineHandle's lock.
    render_state: Option<Arc<Mutex<RenderState>>>,
    /// Decoder sidecar; spawned on first `open_file`. **Moved out** during
    /// playback (the scheduler thread owns it then) and reclaimed on stop.
    decoder: Option<DecoderClient>,
    /// Stream info from the most recent successful `open_file`. Stays
    /// available even during playback so the UI can read it.
    stream_info: Option<StreamInfo>,
    /// Last-applied viewport bounds (in physical pixels).
    last_bounds: ViewportBounds,
    /// Active playback, if any.
    playback: Option<PlaybackSession>,
}

/// Owned by the engine while video playback is running.
pub struct PlaybackSession {
    stop: Arc<AtomicBool>,
    /// Joins to recover the DecoderClient back into Engine.decoder.
    handle: JoinHandle<DecoderClient>,
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

        self.viewport = Some(Arc::new(viewport));
        self.render_state = Some(Arc::new(Mutex::new(RenderState {
            renderer,
            surface_config: None,
        })));
        Ok(())
    }

    /// Update the overlay's bounds and reconfigure the surface to match.
    pub fn set_viewport_bounds(&mut self, bounds: ViewportBounds) -> Result<(), EngineError> {
        let viewport = self.viewport.as_ref().ok_or(EngineError::NoViewport)?;
        viewport.set_bounds(bounds);
        self.last_bounds = bounds;

        if !bounds.is_empty() {
            if let Some(rs) = self.render_state.as_ref() {
                let mut rs = rs.lock();
                if let Some(cfg) =
                    rs.renderer
                        .recommended_surface_config(viewport.surface(), bounds.w, bounds.h)
                {
                    rs.renderer.configure_surface(viewport.surface(), &cfg);
                    rs.surface_config = Some(cfg);
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
    /// into the viewport's surface. Single-shot path; not used during
    /// playback (the scheduler handles its own render loop).
    pub fn decode_and_render_at(&mut self, target_pts: i64) -> Result<i64, EngineError> {
        if self.playback.is_some() {
            return Err(EngineError::PlaybackInProgress);
        }
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

        let viewport = self
            .viewport
            .as_ref()
            .ok_or(EngineError::NoViewport)?
            .clone();
        let render_state = self
            .render_state
            .as_ref()
            .ok_or(EngineError::NoViewport)?
            .clone();
        let last_bounds = self.last_bounds;
        upload_and_render(&render_state, &viewport, &frame, last_bounds)?;
        Ok(pts)
    }

    /// Begin streaming playback starting at `from_pts`. The scheduler
    /// thread takes ownership of the decoder for the duration of playback;
    /// other commands that need the decoder will return `PlaybackInProgress`
    /// until `stop_playback` is called.
    pub fn start_playback(&mut self, from_pts: i64) -> Result<(), EngineError> {
        tracing::info!(from_pts, "start_playback called");
        if self.playback.is_some() {
            return Err(EngineError::PlaybackInProgress);
        }
        let viewport = self
            .viewport
            .as_ref()
            .ok_or(EngineError::NoViewport)?
            .clone();
        let render_state = self
            .render_state
            .as_ref()
            .ok_or(EngineError::NoViewport)?
            .clone();
        let info = self
            .stream_info
            .as_ref()
            .ok_or(EngineError::NoFileOpen)?
            .clone();
        let mut decoder = self.decoder.take().ok_or(EngineError::NoFileOpen)?;

        // Kick off streaming on the sidecar before handing off the client.
        if let Err(e) = decoder.start_stream(from_pts, 8) {
            // Restore decoder so the engine isn't stuck with no decoder.
            self.decoder = Some(decoder);
            return Err(EngineError::DecoderClient(e));
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let last_bounds = self.last_bounds;
        let handle = std::thread::Builder::new()
            .name("vance-engine.playback".into())
            .spawn(move || {
                playback_loop(
                    decoder,
                    info,
                    from_pts,
                    last_bounds,
                    viewport,
                    render_state,
                    stop_clone,
                )
            })
            .map_err(|e| EngineError::PlaybackThread(format!("spawn: {e}")))?;
        self.playback = Some(PlaybackSession { stop, handle });
        Ok(())
    }

    /// Stop the current playback session (if any) and reclaim the decoder.
    pub fn stop_playback(&mut self) -> Result<(), EngineError> {
        let Some(session) = self.playback.take() else {
            return Err(EngineError::NoPlayback);
        };
        session.stop.store(true, Ordering::Release);
        match session.handle.join() {
            Ok(decoder) => {
                self.decoder = Some(decoder);
                Ok(())
            }
            Err(_) => Err(EngineError::PlaybackThread("scheduler panicked".into())),
        }
    }

    /// Is playback currently running?
    pub fn is_playing(&self) -> bool {
        self.playback.is_some()
    }
}

/// Background scheduler. Pulls frames from the decoder in PTS order;
/// for each, sleeps until its target wall-clock present time then
/// uploads + renders + presents. Wall-clock-only for 2.4; A/V sync to
/// the audio clock lands in 2.5.
///
/// Returns the `DecoderClient` so the engine can resume single-shot use.
#[allow(clippy::too_many_arguments)]
fn playback_loop(
    mut decoder: DecoderClient,
    info: StreamInfo,
    from_pts: i64,
    initial_bounds: ViewportBounds,
    viewport: Arc<Viewport>,
    render_state: Arc<Mutex<RenderState>>,
    stop: Arc<AtomicBool>,
) -> DecoderClient {
    // Stream-time-base → seconds: pts * tb_num / tb_den.
    let (tb_num, tb_den) = info.time_base;
    let pts_to_sec = |pts: i64| -> f64 {
        if tb_den == 0 {
            0.0
        } else {
            (pts as f64) * (tb_num as f64) / (tb_den as f64)
        }
    };

    // Anchor wall clock to the moment we receive the FIRST frame, mapped
    // to its PTS. Subsequent frames' target time = anchor + (pts - first).
    tracing::info!(from_pts, "playback thread started");
    let mut anchor: Option<(Instant, f64)> = None;
    let mut bounds = initial_bounds;
    let mut last_logged_drops = 0u64;
    let mut total_drops = 0u64;
    let mut frames_presented = 0u64;

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let frame = match decoder.next_frame() {
            Ok(Some(f)) => f,
            Ok(None) => {
                tracing::info!("playback: end of stream");
                break;
            }
            Err(e) => {
                tracing::error!(?e, "playback: decoder error");
                break;
            }
        };
        let frame_sec = pts_to_sec(frame.pts);

        // First-frame anchor.
        if anchor.is_none() {
            anchor = Some((Instant::now(), frame_sec));
        }
        let (origin, origin_sec) = anchor.unwrap();
        let target = origin + Duration::from_secs_f64((frame_sec - origin_sec).max(0.0));
        let now = Instant::now();
        if target > now {
            let wait = target - now;
            // Bounded sleep so a stop signal isn't ignored for too long.
            std::thread::sleep(wait.min(Duration::from_millis(50)));
            // Re-check stop after the sleep.
            if stop.load(Ordering::Acquire) {
                break;
            }
            // If the remaining wait is still meaningful, sleep again.
            let now2 = Instant::now();
            if target > now2 {
                std::thread::sleep(target - now2);
            }
        } else if (now - target) > Duration::from_millis(80) {
            // > ~2 frames late at 24fps: drop this one. Keeps the playhead
            // honest under load instead of letting drift accumulate.
            total_drops += 1;
            if total_drops - last_logged_drops >= 30 {
                tracing::warn!(total_drops, "playback: dropped frames so far");
                last_logged_drops = total_drops;
            }
            continue;
        }

        if let Err(e) = upload_and_render(&render_state, &viewport, &frame, bounds) {
            tracing::warn!(?e, "playback: render error (continuing)");
        }
        frames_presented += 1;
        if frames_presented == 1 || frames_presented.is_multiple_of(30) {
            tracing::info!(
                frames_presented,
                pts = frame.pts,
                "playback: frame presented"
            );
        }
        // Refresh bounds from the viewport in case the chrome resized.
        bounds = viewport.current_bounds();
        let _ = from_pts; // keep arg meaningful; not used after anchor set
    }
    tracing::info!(frames_presented, total_drops, "playback: thread exiting");

    let _ = decoder.stop_stream();
    decoder
}

/// Single render frame: configure surface if needed, acquire texture,
/// upload + render + present.
fn upload_and_render(
    render_state: &Arc<Mutex<RenderState>>,
    viewport: &Arc<Viewport>,
    frame: &vance_decoder::FrameData,
    bounds: ViewportBounds,
) -> Result<(), EngineError> {
    let mut rs = render_state.lock();
    rs.renderer.upload_frame(frame)?;

    let surface = viewport.surface();
    if rs.surface_config.is_none() && !bounds.is_empty() {
        if let Some(cfg) = rs
            .renderer
            .recommended_surface_config(surface, bounds.w, bounds.h)
        {
            rs.renderer.configure_surface(surface, &cfg);
            rs.surface_config = Some(cfg);
        }
    }
    if rs.surface_config.is_none() {
        return Err(EngineError::SurfaceAcquire(
            "viewport bounds not yet reported".into(),
        ));
    }
    let surface_tex = match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
        wgpu::CurrentSurfaceTexture::Outdated
        | wgpu::CurrentSurfaceTexture::Lost
        | wgpu::CurrentSurfaceTexture::Validation => {
            rs.surface_config = None;
            return Err(EngineError::SurfaceAcquire("surface outdated".into()));
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
    rs.renderer.render_to(&view)?;
    surface_tex.present();
    Ok(())
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
