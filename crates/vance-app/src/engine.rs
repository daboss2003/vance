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
use vance_audio::AudioPlayer;
use vance_decoder::{DecoderClient, StreamInfo};
use vance_render::{Renderer, RendererConfig};
use vance_timeline::{
    Clip, MediaItem, MediaItemId, MediaKind, Project, Sequence, SequenceId, Track, TrackId,
    TrackKind,
};
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

    /// `vance-project` failed to read or write a bundle.
    #[error("project: {0}")]
    Project(String),

    /// A project-level command was issued without a project loaded.
    #[error("no project loaded")]
    NoProject,

    /// `set_current_sequence` was given an ID that's not in the project.
    #[error("sequence not found in current project")]
    SequenceNotFound,

    /// A clip-edit command was issued before any sequence was active.
    #[error("no current sequence selected")]
    NoCurrentSequence,

    /// `insert_clip_at` referenced a `MediaItemId` not in the library.
    #[error("media item not found in project library")]
    MediaNotFound,
}

impl EngineError {
    fn from_core(e: vance_core::Error) -> Self {
        EngineError::Project(e.to_string())
    }
}

/// Lightweight summary of the loaded project for the UI. Kept small so
/// it's cheap to ship over the IPC boundary; the full project lives on
/// the engine side and is accessed via per-entity commands.
#[derive(serde::Serialize, Clone, Debug)]
pub struct ProjectInfo {
    /// Project's display name.
    pub name: String,
    /// Sequences in insertion order, with their IDs and human names.
    pub sequences: Vec<SequenceInfo>,
    /// Currently active sequence's ID, if any.
    pub current_sequence_id: Option<String>,
}

/// One row in `ProjectInfo.sequences`.
#[derive(serde::Serialize, Clone, Debug)]
pub struct SequenceInfo {
    /// Sequence ID as a string (UUID).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Total clip count across all tracks (a useful summary for the UI).
    pub clip_count: usize,
}

/// Full contents of a sequence, in floats-of-seconds, suitable for the
/// JS timeline canvas. Times use `f64` (rather than `RationalTime`)
/// because the canvas just needs a position; rendering accuracy is fine
/// at 64-bit float precision and saves a JSON conversion layer per
/// clip.
#[derive(serde::Serialize, Clone, Debug)]
pub struct SequenceContents {
    /// Sequence ID as a string.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Master frame rate as `(num, den)`.
    pub frame_rate_num: i64,
    /// Master frame rate denominator.
    pub frame_rate_den: i64,
    /// Total duration of the sequence, in seconds.
    pub duration_seconds: f64,
    /// Tracks in render order (video first, then audio by convention).
    pub tracks: Vec<TrackContents>,
}

/// One track's worth of clips in `SequenceContents`.
#[derive(serde::Serialize, Clone, Debug)]
pub struct TrackContents {
    /// Track ID.
    pub id: String,
    /// "video" or "audio".
    pub kind: String,
    /// Display name (V1, A1, etc.).
    pub name: String,
    /// Whether the track contributes to playback.
    pub enabled: bool,
    /// Whether edit ops refuse to modify this track.
    pub locked: bool,
    /// Clips on this track, in timeline order.
    pub clips: Vec<ClipContents>,
}

/// One clip's worth of timing info for the UI.
#[derive(serde::Serialize, Clone, Debug)]
pub struct ClipContents {
    /// Clip ID.
    pub id: String,
    /// MediaItem ID this clip references.
    pub media_id: String,
    /// Display name.
    pub name: String,
    /// Start time on the sequence timeline, in seconds.
    pub timeline_in_seconds: f64,
    /// Length on the timeline, in seconds.
    pub duration_seconds: f64,
    /// First visible time within the source media, in seconds.
    pub source_in_seconds: f64,
    /// Whether the clip is muted.
    pub enabled: bool,
}

fn rt_to_seconds(t: vance_core::RationalTime) -> f64 {
    let n = t.0.numer() as f64;
    let d = t.0.denom() as f64;
    if d == 0.0 {
        0.0
    } else {
        n / d
    }
}

/// Inverse of `rt_to_seconds`. Round to milliseconds — gives plenty
/// of precision for clip placement and keeps the on-disk RON
/// human-readable.
fn seconds_to_rt(s: f64) -> vance_core::RationalTime {
    let ms = (s * 1000.0).round() as i64;
    vance_core::RationalTime::new(ms.max(0), 1000)
        .unwrap_or(vance_core::RationalTime::ZERO)
}

/// First video clip in the sequence whose `timeline_in >= after_t`,
/// across all enabled video tracks. Used by `play_sequence` /
/// `decode_sequence_at` to do the right thing when the requested
/// timeline position sits in a gap or before the first clip.
struct FirstClipHit {
    start: vance_core::RationalTime,
    active: vance_timeline::ActiveClip,
}

/// Look up `(timeline_in_seconds, source_in_seconds)` for a clip by
/// ID anywhere in the sequence. Used to anchor playback for the
/// playhead-translation math.
fn clip_anchor_seconds(
    seq: &Sequence,
    clip_id: vance_timeline::ClipId,
) -> Option<(f64, f64)> {
    for track in seq.tracks.iter() {
        for clip in track.clips.iter() {
            if clip.id == clip_id {
                return Some((
                    rt_to_seconds(clip.timeline_in),
                    rt_to_seconds(clip.source_in),
                ));
            }
        }
    }
    None
}

fn first_clip_after(
    seq: &Sequence,
    after_t: vance_core::RationalTime,
) -> Option<FirstClipHit> {
    let mut best: Option<FirstClipHit> = None;
    for track in seq.tracks.iter() {
        if track.kind != TrackKind::Video || !track.enabled {
            continue;
        }
        for clip in track.clips.iter() {
            if !clip.enabled {
                continue;
            }
            if clip.timeline_in < after_t {
                continue;
            }
            let hit = FirstClipHit {
                start: clip.timeline_in,
                active: vance_timeline::ActiveClip {
                    clip_id: clip.id,
                    media: clip.media,
                    source_pts: clip.source_in,
                },
            };
            best = match best {
                None => Some(hit),
                Some(b) if hit.start < b.start => Some(hit),
                Some(b) => Some(b),
            };
        }
    }
    best
}

/// Convert a decoder `StreamInfo` duration into a `RationalTime`.
/// Returns `None` when the container didn't declare a duration.
fn duration_from_stream(s: &StreamInfo) -> Option<vance_core::RationalTime> {
    let dur_pts = s.duration_pts?;
    let (tb_n, tb_d) = s.time_base;
    if tb_d == 0 {
        return None;
    }
    // duration_seconds = dur_pts * tb_n / tb_d. We carry that through
    // RationalTime by multiplying the numerator.
    vance_core::RationalTime::new(dur_pts.saturating_mul(tb_n), tb_d).ok()
}

/// Lightweight DTO of a `MediaItem` for the Browser panel.
#[derive(serde::Serialize, Clone, Debug)]
pub struct MediaItemInfo {
    /// Stable UUID as a string.
    pub id: String,
    /// Display name (defaults to the file stem on import).
    pub name: String,
    /// Absolute path on disk.
    pub path: String,
    /// "video", "audio", or "image".
    pub kind: String,
    /// Total duration in seconds.
    pub duration_seconds: f64,
    /// Source frame rate as `(num, den)`, or null when unknown.
    pub frame_rate_num: Option<i64>,
    pub frame_rate_den: Option<i64>,
}

impl From<&MediaItem> for MediaItemInfo {
    fn from(m: &MediaItem) -> Self {
        let (fr_n, fr_d) = match m.frame_rate {
            Some(fr) => (Some(fr.0.numer()), Some(fr.0.denom())),
            None => (None, None),
        };
        Self {
            id: m.id.to_string(),
            name: m.display_name.clone(),
            path: m.path.to_string_lossy().into_owned(),
            kind: match m.kind {
                MediaKind::Video => "video".into(),
                MediaKind::Audio => "audio".into(),
                MediaKind::Image => "image".into(),
            },
            duration_seconds: rt_to_seconds(m.duration),
            frame_rate_num: fr_n,
            frame_rate_den: fr_d,
        }
    }
}

impl From<&Project> for ProjectInfo {
    fn from(p: &Project) -> Self {
        Self {
            name: p.name.clone(),
            sequences: p
                .sequences
                .iter()
                .map(|s| SequenceInfo {
                    id: s.id.to_string(),
                    name: s.name.clone(),
                    clip_count: s.clip_count(),
                })
                .collect(),
            current_sequence_id: None,
        }
    }
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
    /// Most recently opened media path. Needed by `start_playback` to
    /// also open the audio side of the file.
    current_media_path: Option<String>,
    /// Last-applied viewport bounds (in physical pixels).
    last_bounds: ViewportBounds,
    /// Active playback, if any.
    playback: Option<PlaybackSession>,
    /// Audio playback owner thread (holds the AudioPlayer; AudioPlayer's
    /// cpal::Stream is !Send so it has to stay on its own thread).
    audio_owner: Option<AudioOwner>,
    /// Last presented video PTS from the most recent playback session.
    /// Set when a session is reaped (explicit stop OR auto-reap on EOS),
    /// cleared on the next start_playback. Lets the UI learn the EOS PTS
    /// without having to call stop_playback itself.
    last_resume_pts: Option<i64>,

    /// Currently loaded project, if any. Phase 4 introduces project
    /// state into the engine so playback can target a sequence rather
    /// than just a file.
    current_project: Option<Project>,
    /// Which sequence in `current_project` is the active edit. Cleared
    /// when the project changes. Defaults to the first sequence on
    /// `load_project`.
    current_sequence_id: Option<SequenceId>,
    /// Path of the `.vance/` bundle the current project was loaded
    /// from (or last saved to). `save_project(None)` writes here.
    current_project_path: Option<PathBuf>,

    /// Timeline/source anchors of the clip currently being played
    /// from the active sequence. Lets us translate the playback
    /// loop's source-PTS readout into a timeline position for the
    /// canvas playhead. Cleared when no sequence playback is in
    /// flight (single-file `start_playback` doesn't set it).
    playing_clip_anchor: Option<ClipAnchor>,
}

/// Anchors a stream-time-base playback position back to a sequence
/// timeline. `timeline_seconds = timeline_in + (source_seconds -
/// source_in)`, where `source_seconds = stream_pts * tb_num / tb_den`.
#[derive(Clone, Copy, Debug)]
pub struct ClipAnchor {
    pub timeline_in_seconds: f64,
    pub source_in_seconds: f64,
    pub time_base_num: i64,
    pub time_base_den: i64,
}

/// Owns the AudioPlayer on a dedicated thread. The thread parks until
/// `stop` is set, then drops the AudioPlayer (which stops the cpal
/// stream and joins its puller).
pub struct AudioOwner {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// Owned by the engine while video playback is running.
pub struct PlaybackSession {
    stop: Arc<AtomicBool>,
    /// Last video PTS the scheduler presented. Updated atomically each
    /// frame; read by `stop_playback` so the UI can resume from here on
    /// the next Play.
    last_pts: Arc<std::sync::atomic::AtomicI64>,
    /// Set by the playback loop when it exits via natural end-of-stream
    /// (as opposed to an explicit stop or a decoder error). Tells
    /// `stop_playback` to report a resume PTS of 0 so the next Play
    /// rewinds to the start instead of getting stuck at EOS.
    ended_at_eos: Arc<AtomicBool>,
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
        self.current_media_path = Some(path.to_string());
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
        let media_path = self
            .current_media_path
            .clone()
            .ok_or(EngineError::NoFileOpen)?;
        let mut decoder = self.decoder.take().ok_or(EngineError::NoFileOpen)?;

        // Kick off streaming on the sidecar before handing off the client.
        if let Err(e) = decoder.start_stream(from_pts, 8) {
            // Restore decoder so the engine isn't stuck with no decoder.
            self.decoder = Some(decoder);
            return Err(EngineError::DecoderClient(e));
        }

        // Best-effort audio start. If the file has no audio or audio init
        // fails, keep going with wall-clock-only video — better degraded
        // playback than no playback.
        let (vtb_n, vtb_d) = info.time_base;
        let from_seconds = if vtb_d > 0 {
            (from_pts as f64) * (vtb_n as f64) / (vtb_d as f64)
        } else {
            0.0
        };
        let (audio_owner, audio_sync) = match start_audio(&media_path, from_seconds) {
            Ok((owner, sync)) => (Some(owner), Some(sync)),
            Err(e) => {
                tracing::warn!(?e, "audio: not started; video will use wall-clock master");
                (None, None)
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let last_pts = Arc::new(std::sync::atomic::AtomicI64::new(from_pts));
        let last_pts_clone = last_pts.clone();
        let ended_at_eos = Arc::new(AtomicBool::new(false));
        let ended_at_eos_clone = ended_at_eos.clone();
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
                    audio_sync,
                    last_pts_clone,
                    ended_at_eos_clone,
                )
            })
            .map_err(|e| EngineError::PlaybackThread(format!("spawn: {e}")))?;
        self.playback = Some(PlaybackSession {
            stop,
            last_pts,
            ended_at_eos,
            handle,
        });
        self.audio_owner = audio_owner;
        self.last_resume_pts = None;
        Ok(())
    }

    /// Stop the current playback session (if any) and reclaim the decoder.
    /// Stop the current playback session and return the last presented
    /// video PTS so the UI can resume from there on the next Play.
    pub fn stop_playback(&mut self) -> Result<i64, EngineError> {
        let Some(session) = self.playback.take() else {
            return Err(EngineError::NoPlayback);
        };
        session.stop.store(true, Ordering::Release);
        // Stop the audio owner thread (it'll drop the AudioPlayer, which
        // stops cpal + joins the puller thread).
        if let Some(owner) = self.audio_owner.take() {
            owner.stop.store(true, Ordering::Release);
            let _ = owner.handle.join();
        }
        let resume_pts = if session.ended_at_eos.load(Ordering::Acquire) {
            // EOS → tell the UI to start from 0 next time. Otherwise the
            // playhead stays parked at the last frame and Play does
            // nothing because the decoder seeks straight to EOS.
            0
        } else {
            session.last_pts.load(Ordering::Acquire)
        };
        match session.handle.join() {
            Ok(decoder) => {
                self.decoder = Some(decoder);
                self.last_resume_pts = Some(resume_pts);
                Ok(resume_pts)
            }
            Err(_) => Err(EngineError::PlaybackThread("scheduler panicked".into())),
        }
    }

    /// Load a `.vance/` bundle and make it the current project.
    /// Selects the first sequence as the active sequence; `None` if
    /// the project has no sequences. The previous project (if any) is
    /// discarded — a future "save before close" gate is up to the UI.
    pub fn load_project(&mut self, path: &str) -> Result<ProjectInfo, EngineError> {
        if self.playback.is_some() {
            return Err(EngineError::PlaybackInProgress);
        }
        let project = vance_project::open(path).map_err(|e| EngineError::Project(e.to_string()))?;
        let mut info = ProjectInfo::from(&project);
        self.current_sequence_id = project.sequences.first().map(|s| s.id);
        info.current_sequence_id = self.current_sequence_id.map(|id| id.to_string());
        self.current_project_path = Some(PathBuf::from(path));
        self.current_project = Some(project);
        Ok(info)
    }

    /// Replace the current project with a fresh empty one. The save
    /// path is cleared — `save_project(None)` will fail until the
    /// caller passes an explicit path.
    ///
    /// To give the user something to drag onto right away, we seed the
    /// project with one "Sequence 01" (24fps) containing a V1 video
    /// and A1 audio track. The user can rename / re-rate later.
    pub fn new_project(&mut self, name: String) -> Result<ProjectInfo, EngineError> {
        if self.playback.is_some() {
            return Err(EngineError::PlaybackInProgress);
        }
        let mut project = Project::new(name);
        let mut seq = Sequence::new("Sequence 01", vance_core::FrameRate::FPS_24);
        seq.tracks.push(Arc::new(Track::new(TrackKind::Video, "V1")));
        seq.tracks.push(Arc::new(Track::new(TrackKind::Audio, "A1")));
        let seq_id = seq.id;
        project.sequences.push(Arc::new(seq));
        let mut info = ProjectInfo::from(&project);
        info.current_sequence_id = Some(seq_id.to_string());
        self.current_sequence_id = Some(seq_id);
        self.current_project_path = None;
        self.current_project = Some(project);
        Ok(info)
    }

    /// Import a media file by path. Briefly opens it via the decoder
    /// sidecar to capture its metadata (codec, duration, frame rate,
    /// pixel format), then registers it as a `MediaItem` in the
    /// current project's media library.
    pub fn import_media_file(&mut self, path: &str) -> Result<MediaItemInfo, EngineError> {
        if self.playback.is_some() {
            return Err(EngineError::PlaybackInProgress);
        }
        if self.current_project.is_none() {
            return Err(EngineError::NoProject);
        }

        // Spawn the decoder sidecar if needed; we share the same one
        // single-file playback uses. Calling `open` returns the
        // stream's StreamInfo (codec, dims, time_base, duration_pts).
        if self.decoder.is_none() {
            let bin = locate_decoder_binary()?;
            let client = DecoderClient::spawn(&bin)
                .map_err(|e| EngineError::DecoderSpawn(format!("{e}")))?;
            self.decoder = Some(client);
        }
        let dec = self.decoder.as_mut().unwrap();
        let stream = dec.open(path.to_string())?;

        // Convert the decoder's duration_pts (in its time base) to
        // seconds. duration_pts can be None if the container didn't
        // declare one; in that case we pessimistically pick 60 s so
        // there's something draggable. Real fix in a later milestone.
        let duration = duration_from_stream(&stream).unwrap_or_else(|| {
            vance_core::RationalTime::new(60, 1).expect("60s rational")
        });

        // Frame rate as a RationalTime is the inverse; we keep the raw
        // FrameRate on MediaItem since rate is meaningful per kind.
        let frame_rate = if stream.frame_rate.1 != 0 {
            Some(vance_core::FrameRate(
                vance_core::Rational::new(stream.frame_rate.0, stream.frame_rate.1)
                    .map_err(EngineError::from_core)?,
            ))
        } else {
            None
        };

        // Display name from the path's file stem.
        let display_name = std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string();

        let mut item = MediaItem::new(
            std::path::PathBuf::from(path),
            display_name,
            MediaKind::Video, // audio-only / image imports handled later
            duration,
        );
        if let Some(fr) = frame_rate {
            item = item.with_frame_rate(fr);
        }
        let id = item.id;
        let info = MediaItemInfo::from(&item);

        // Add to project. Project::media is BTreeMap<MediaItemId, Arc<MediaItem>>.
        let project = self.current_project.as_mut().unwrap();
        project.media.insert(id, Arc::new(item));

        tracing::info!(
            id = %id,
            path = %path,
            duration = ?duration,
            "imported media"
        );
        Ok(info)
    }

    /// All media items registered in the current project, for the
    /// Browser panel. Empty when no project is loaded.
    pub fn media_list(&self) -> Vec<MediaItemInfo> {
        self.current_project
            .as_ref()
            .map(|p| {
                p.media
                    .values()
                    .map(|arc| MediaItemInfo::from(arc.as_ref()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Start playback of the active sequence from `timeline_seconds`.
    ///
    /// Uses `scene_plan::scene_at` to find the topmost video clip at
    /// that position, swaps the decoder onto that clip's media (which
    /// may differ from whatever was last opened via `open_file`), and
    /// kicks off the existing `start_playback` loop at the right
    /// source PTS. Continuous playback across clip boundaries lands
    /// in a follow-on milestone; for now the playback loop runs
    /// against the active clip's underlying file as a unit.
    pub fn play_sequence(&mut self, timeline_seconds: f64) -> Result<(), EngineError> {
        if self.playback.is_some() {
            return Err(EngineError::PlaybackInProgress);
        }
        // Scope the immutable project borrow tightly so we can call
        // `self.open_file` (mutable borrow) afterwards. Also capture
        // the clip's timeline/source anchors so the UI can translate
        // the running source-PTS back to a timeline position.
        let (media_path, source_pts_seconds, anchor_timeline_in, anchor_source_in) = {
            let project = self
                .current_project
                .as_ref()
                .ok_or(EngineError::NoProject)?;
            let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;

            // If the requested t lands on a clip, use it directly;
            // otherwise jump to the earliest clip in the sequence so
            // "Play" with an empty playhead does the obvious thing.
            let timeline_t = seconds_to_rt(timeline_seconds);
            let active = match vance_timeline::scene_at(seq, timeline_t) {
                vance_timeline::SceneAt::Clip(c) => c,
                vance_timeline::SceneAt::Empty => {
                    let next = first_clip_after(seq, timeline_t).ok_or_else(|| {
                        EngineError::Project("sequence has no clips to play".into())
                    })?;
                    tracing::info!(
                        requested = timeline_seconds,
                        first_clip_start = rt_to_seconds(next.start),
                        "play_sequence: empty at requested t, jumping to first clip"
                    );
                    next.active
                }
            };
            let media = project
                .media
                .get(&active.media)
                .ok_or(EngineError::MediaNotFound)?;
            let path = media.path.to_string_lossy().into_owned();
            // Find the source clip's timeline_in for the anchor.
            let (tl_in_sec, src_in_sec) = clip_anchor_seconds(seq, active.clip_id)
                .unwrap_or((0.0, rt_to_seconds(active.source_pts)));
            (path, rt_to_seconds(active.source_pts), tl_in_sec, src_in_sec)
        };

        // Reopen decoder onto the active clip's media. Idempotent if
        // we're already on this file: the decoder reopens cleanly.
        self.open_file(&media_path)?;

        // Convert source-seconds → stream-time-base PTS.
        let info = self
            .stream_info
            .as_ref()
            .ok_or_else(|| EngineError::Project("stream info missing post-open".into()))?;
        let (tb_n, tb_d) = info.time_base;
        let from_pts = if tb_n != 0 {
            (source_pts_seconds * (tb_d as f64) / (tb_n as f64)).round() as i64
        } else {
            0
        };
        tracing::info!(
            from_seconds = source_pts_seconds,
            from_pts,
            "play_sequence: starting"
        );
        self.playing_clip_anchor = Some(ClipAnchor {
            timeline_in_seconds: anchor_timeline_in,
            source_in_seconds: anchor_source_in,
            time_base_num: tb_n,
            time_base_den: tb_d,
        });
        self.start_playback(from_pts)
    }

    /// Current playhead position on the active sequence's timeline,
    /// in seconds. Reads the playback loop's `last_pts` atomic and
    /// translates through the active clip's anchor. Returns 0 when
    /// no anchor is set (no sequence playback ever started).
    pub fn current_timeline_seconds(&self) -> Option<f64> {
        let anchor = self.playing_clip_anchor.as_ref()?;
        if anchor.time_base_den == 0 {
            return Some(anchor.timeline_in_seconds);
        }
        // Pick the freshest stream-PTS we have: the active session's
        // atomic if playing, else the last-resumed value.
        let stream_pts = if let Some(session) = &self.playback {
            session.last_pts.load(Ordering::Acquire)
        } else if let Some(p) = self.last_resume_pts {
            p
        } else {
            return Some(anchor.timeline_in_seconds);
        };
        let source_sec = (stream_pts as f64) * (anchor.time_base_num as f64)
            / (anchor.time_base_den as f64);
        Some(anchor.timeline_in_seconds + (source_sec - anchor.source_in_seconds))
    }

    /// Decode a single frame from the active sequence at
    /// `timeline_seconds` (used by seek-while-paused). Bypasses the
    /// playback scheduler.
    pub fn decode_sequence_at(&mut self, timeline_seconds: f64) -> Result<i64, EngineError> {
        if self.playback.is_some() {
            return Err(EngineError::PlaybackInProgress);
        }
        let (media_path, source_pts_seconds, anchor_timeline_in, anchor_source_in) = {
            let project = self
                .current_project
                .as_ref()
                .ok_or(EngineError::NoProject)?;
            let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
            let timeline_t = seconds_to_rt(timeline_seconds);
            let active = match vance_timeline::scene_at(seq, timeline_t) {
                vance_timeline::SceneAt::Clip(c) => c,
                vance_timeline::SceneAt::Empty => {
                    let next = first_clip_after(seq, timeline_t).ok_or_else(|| {
                        EngineError::Project("sequence has no clips to decode".into())
                    })?;
                    next.active
                }
            };
            let media = project
                .media
                .get(&active.media)
                .ok_or(EngineError::MediaNotFound)?;
            let (tl_in, src_in) = clip_anchor_seconds(seq, active.clip_id)
                .unwrap_or((0.0, rt_to_seconds(active.source_pts)));
            (
                media.path.to_string_lossy().into_owned(),
                rt_to_seconds(active.source_pts),
                tl_in,
                src_in,
            )
        };
        self.open_file(&media_path)?;
        let info = self
            .stream_info
            .as_ref()
            .ok_or_else(|| EngineError::Project("stream info missing post-open".into()))?;
        let (tb_n, tb_d) = info.time_base;
        let target_pts = if tb_n != 0 {
            (source_pts_seconds * (tb_d as f64) / (tb_n as f64)).round() as i64
        } else {
            0
        };
        self.playing_clip_anchor = Some(ClipAnchor {
            timeline_in_seconds: anchor_timeline_in,
            source_in_seconds: anchor_source_in,
            time_base_num: tb_n,
            time_base_den: tb_d,
        });
        let pts = self.decode_and_render_at(target_pts)?;
        // Park the engine's "where am I" pointer at the just-rendered
        // frame so a subsequent Play resumes from this click, not from
        // whatever was the last *playback* stop position.
        self.last_resume_pts = Some(pts);
        Ok(pts)
    }

    /// Insert a clip referencing `media_id`'s full visible range at
    /// `timeline_seconds` on `track_id`. Returns the new clip's ID
    /// for the UI's selection model in 4.4.
    pub fn insert_clip_at(
        &mut self,
        track_id: TrackId,
        media_id: MediaItemId,
        timeline_seconds: f64,
    ) -> Result<String, EngineError> {
        if self.playback.is_some() {
            return Err(EngineError::PlaybackInProgress);
        }
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        let seq_id = self
            .current_sequence_id
            .ok_or(EngineError::NoCurrentSequence)?;

        let media = project
            .media
            .get(&media_id)
            .ok_or(EngineError::MediaNotFound)?
            .clone();

        let timeline_in = seconds_to_rt(timeline_seconds);
        let clip = Clip::new(
            media.id,
            media.display_name.clone(),
            timeline_in,
            vance_core::RationalTime::ZERO,
            media.duration,
        )
        .map_err(|e| EngineError::Project(e.to_string()))?;
        let new_clip_id = clip.id;

        // Apply the insert via ops::insert_clip on the current sequence.
        let seq = project
            .sequence(seq_id)
            .ok_or(EngineError::SequenceNotFound)?
            .clone();
        let new_seq = vance_timeline::ops::insert_clip(seq.as_ref(), track_id, clip)
            .map_err(|e| EngineError::Project(e.to_string()))?;

        // Replace the sequence in the project's Vec<Arc<Sequence>>.
        if let Some(slot) = project.sequences.iter_mut().find(|s| s.id == seq_id) {
            *slot = Arc::new(new_seq);
        }
        Ok(new_clip_id.to_string())
    }

    /// Save the current project. If `path` is provided it overrides the
    /// remembered bundle path (acts like Save As); otherwise the engine
    /// writes back to the path the project was loaded from.
    pub fn save_project(&mut self, path: Option<String>) -> Result<(), EngineError> {
        let project = self
            .current_project
            .as_ref()
            .ok_or(EngineError::NoProject)?;
        let target = match path {
            Some(p) => PathBuf::from(p),
            None => self
                .current_project_path
                .clone()
                .ok_or_else(|| EngineError::Project("project has no save path; call save_project with an explicit path first".into()))?,
        };
        vance_project::save(project, &target).map_err(|e| EngineError::Project(e.to_string()))?;
        self.current_project_path = Some(target);
        Ok(())
    }

    /// Path the current project was loaded from / last saved to.
    pub fn current_project_path(&self) -> Option<&PathBuf> {
        self.current_project_path.as_ref()
    }

    /// Full contents of the currently active sequence, ready for the
    /// timeline canvas. Returns `None` when no project is loaded or no
    /// sequence is selected.
    pub fn current_sequence_contents(&self) -> Option<SequenceContents> {
        let project = self.current_project.as_ref()?;
        let id = self.current_sequence_id?;
        let seq = project.sequence(id)?;
        Some(SequenceContents {
            id: seq.id.to_string(),
            name: seq.name.clone(),
            frame_rate_num: seq.frame_rate.0.numer(),
            frame_rate_den: seq.frame_rate.0.denom(),
            duration_seconds: rt_to_seconds(seq.duration()),
            tracks: seq
                .tracks
                .iter()
                .map(|t| TrackContents {
                    id: t.id.to_string(),
                    kind: match t.kind {
                        vance_timeline::TrackKind::Video => "video".into(),
                        vance_timeline::TrackKind::Audio => "audio".into(),
                    },
                    name: t.name.clone(),
                    enabled: t.enabled,
                    locked: t.locked,
                    clips: t
                        .clips
                        .iter()
                        .map(|c| ClipContents {
                            id: c.id.to_string(),
                            media_id: c.media.to_string(),
                            name: c.name.clone(),
                            timeline_in_seconds: rt_to_seconds(c.timeline_in),
                            duration_seconds: rt_to_seconds(c.duration()),
                            source_in_seconds: rt_to_seconds(c.source_in),
                            enabled: c.enabled,
                        })
                        .collect(),
                })
                .collect(),
        })
    }

    /// Switch the active sequence. The ID must reference a sequence in
    /// the loaded project.
    pub fn set_current_sequence(&mut self, id: SequenceId) -> Result<(), EngineError> {
        let project = self
            .current_project
            .as_ref()
            .ok_or(EngineError::NoProject)?;
        if project.sequence(id).is_none() {
            return Err(EngineError::SequenceNotFound);
        }
        self.current_sequence_id = Some(id);
        Ok(())
    }

    /// A snapshot of the current project for the UI: name + sequence
    /// list + active sequence ID. Returns `None` when no project is
    /// loaded.
    pub fn project_info(&self) -> Option<ProjectInfo> {
        self.current_project.as_ref().map(|p| {
            let mut info = ProjectInfo::from(p);
            info.current_sequence_id = self.current_sequence_id.map(|id| id.to_string());
            info
        })
    }

    /// Is playback currently running? Auto-reaps sessions whose scheduler
    /// thread has exited (e.g. natural end-of-stream) so the engine state
    /// matches reality and the decoder is reclaimed for single-shot use.
    pub fn is_playing(&mut self) -> bool {
        let finished = self
            .playback
            .as_ref()
            .is_some_and(|s| s.handle.is_finished());
        if finished {
            let _ = self.stop_playback();
            return false;
        }
        self.playback.is_some()
    }

    /// Last presented PTS from the most recent finished playback session,
    /// or `None` if no session has run since the last `start_playback`.
    pub fn last_resume_pts(&self) -> Option<i64> {
        self.last_resume_pts
    }
}

/// What the playback loop needs to read the audio clock as master.
/// Cloned from the AudioPlayer at session start.
pub struct AudioSync {
    /// Most recently consumed audio chunk's leading PTS, in audio time base.
    pub current_pts: Arc<std::sync::atomic::AtomicI64>,
    /// Audio stream's time base as `(num, den)`. Seconds = pts*num/den.
    pub time_base: (i64, i64),
}

/// Try to start the audio side of playback. Spawns an owner thread that
/// holds the AudioPlayer (cpal::Stream is !Send, so it must stay on
/// one thread) and parks until told to stop. Returns a stop handle and
/// a clock-sharing handle for the video loop.
fn start_audio(media_path: &str, from_seconds: f64) -> Result<(AudioOwner, AudioSync), String> {
    let bin = locate_audio_binary().map_err(|e| format!("{e:?}"))?;
    let media_path = media_path.to_string();

    // Use a one-shot channel to ship the AudioSync (and any startup error)
    // back from the owner thread to here.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<AudioSync, String>>(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    let handle = std::thread::Builder::new()
        .name("vance-engine.audio-owner".into())
        .spawn(move || {
            let player = match AudioPlayer::start(&bin, media_path, from_seconds) {
                Ok(p) => p,
                Err(e) => {
                    let _ = tx.send(Err(format!("{e}")));
                    return;
                }
            };
            // current_pts is updated by the puller with chunk PTS values
            // in the *audio stream's* time base — that's what we need to
            // convert to seconds for sync.
            let sync = AudioSync {
                current_pts: player.current_pts_handle(),
                time_base: player.stream_time_base(),
            };
            if tx.send(Ok(sync)).is_err() {
                return; // engine gave up before we could ship the sync
            }

            // Park until stop is signalled. The player keeps streaming.
            while !stop_clone.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(player); // stops cpal + joins puller thread
        })
        .map_err(|e| format!("spawn audio owner: {e}"))?;

    let sync = rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| "audio owner thread did not report ready in 10s".to_string())??;
    Ok((AudioOwner { stop, handle }, sync))
}

fn locate_audio_binary() -> Result<PathBuf, EngineError> {
    let exe_name = if cfg!(windows) {
        "vance-audio-decoder.exe"
    } else {
        "vance-audio-decoder"
    };
    let mut tried = Vec::new();
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let cand = dir.join(exe_name);
            if cand.exists() {
                return Ok(cand);
            }
            tried.push(cand);
        }
    }
    Err(EngineError::DecoderBinaryMissing(tried))
}

/// Background scheduler. Pulls frames from the decoder in PTS order;
/// for each, presents at the right time relative to the **audio clock**
/// (when present) or wall clock (audio missing/failed).
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
    audio: Option<AudioSync>,
    last_pts: Arc<std::sync::atomic::AtomicI64>,
    ended_at_eos: Arc<AtomicBool>,
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
    tracing::info!(
        from_pts,
        master = if audio.is_some() {
            "audio"
        } else {
            "wall-clock"
        },
        "playback thread started"
    );
    // Wall-clock fallback anchor (only used when no audio).
    let mut wall_anchor: Option<(Instant, f64)> = None;
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
                ended_at_eos.store(true, Ordering::Release);
                break;
            }
            Err(e) => {
                tracing::error!(?e, "playback: decoder error");
                break;
            }
        };
        let frame_sec = pts_to_sec(frame.pts);

        // Decide how late/early this frame is relative to the master clock.
        // Positive => frame is in the future, sleep that long.
        // Negative below threshold => frame is too late, drop.
        let wait_secs = if let Some(audio) = audio.as_ref() {
            let audio_pts = audio.current_pts.load(std::sync::atomic::Ordering::Acquire);
            let (atb_n, atb_d) = audio.time_base;
            let audio_sec = if atb_d != 0 {
                (audio_pts as f64) * (atb_n as f64) / (atb_d as f64)
            } else {
                0.0
            };
            // Both clocks are in absolute file seconds now (audio is seeked
            // to from_seconds at start_stream), so the comparison is direct.
            frame_sec - audio_sec
        } else {
            // Wall-clock fallback.
            if wall_anchor.is_none() {
                wall_anchor = Some((Instant::now(), frame_sec));
            }
            let (origin, origin_sec) = wall_anchor.unwrap();
            let now_sec = origin.elapsed().as_secs_f64();
            (frame_sec - origin_sec) - now_sec
        };

        if wait_secs > 0.0 {
            // Future frame; sleep but in chunks so stop is responsive.
            let total = Duration::from_secs_f64(wait_secs);
            let mut remaining = total;
            while remaining > Duration::from_millis(0) {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let chunk = remaining.min(Duration::from_millis(50));
                std::thread::sleep(chunk);
                remaining = remaining.saturating_sub(chunk);
            }
        } else if wait_secs < -0.080 {
            // > ~2 frames late at 24fps; drop to keep the playhead honest.
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
        last_pts.store(frame.pts, Ordering::Release);
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
