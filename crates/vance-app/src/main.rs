//! Vance — Tauri 2 host process.
//!
//! Phase 1 / Milestone 1.5 scope: open a window, install the parented
//! viewport, spawn the decoder sidecar on demand, decode a frame at a
//! requested PTS and render it through the wgpu pipeline into the viewport.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod accounts;
mod audio_bounce;
mod cached_decode;
mod capabilities;
mod control;
mod crash;
mod engine;
mod generate;
mod magicmask;
mod peaks;
mod prefetch;
mod profiling;
mod proxy;
mod publish;
mod separate;
mod settings;
mod tools;
mod transcribe;

use engine::{
    EngineHandle, MediaItemInfo, ProjectInfo, SequenceContents, SourceState, TrackSummary,
};
use parking_lot::Mutex as PMutex;
use peaks::{JobState, PeakJob, PeaksService, PeaksServiceError, ProgressSink};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager};
use vance_decoder::StreamInfo;
use vance_peaks::{PeakError, PeakFile};
use vance_timeline::SequenceId;
use vance_viewport::{HostWindowHandle, ViewportBounds};

/// Tauri-managed handle around the optional `PeaksService` (None until
/// `setup` has constructed it; `Some` for the rest of the app's life).
/// Wrapped in a `Mutex<Option<…>>` rather than directly behind an Arc
/// so M6 can swap the service when the active project bundle changes.
type PeaksHandle = Arc<PMutex<Option<Arc<PeaksService>>>>;
/// GOAT-10 — shared control-surface hub (OSC + MIDI listeners → action queue).
type ControlHandle = Arc<PMutex<control::ControlHub>>;
/// GOAT-11 — shared direct-publish hub (OAuth + upload state).
type PublishHandle = Arc<PMutex<publish::PublishHub>>;

#[derive(Serialize)]
struct PeaksStatusResponse {
    /// Lowercase tag: "queued" | "running" | "done" | "failed" |
    /// "cancelled" | "unknown".
    state: &'static str,
    samples_done: Option<u64>,
    samples_total: Option<u64>,
    reason: Option<String>,
}

impl From<Option<JobState>> for PeaksStatusResponse {
    fn from(opt: Option<JobState>) -> Self {
        match opt {
            None => Self {
                state: "unknown",
                samples_done: None,
                samples_total: None,
                reason: None,
            },
            Some(JobState::Queued) => Self {
                state: "queued",
                samples_done: None,
                samples_total: None,
                reason: None,
            },
            Some(JobState::Running {
                samples_done,
                samples_total,
            }) => Self {
                state: "running",
                samples_done: Some(samples_done),
                samples_total,
                reason: None,
            },
            Some(JobState::Done { samples_total }) => Self {
                state: "done",
                samples_done: Some(samples_total),
                samples_total: Some(samples_total),
                reason: None,
            },
            Some(JobState::Failed(r)) => Self {
                state: "failed",
                samples_done: None,
                samples_total: None,
                reason: Some(r),
            },
            Some(JobState::Cancelled) => Self {
                state: "cancelled",
                samples_done: None,
                samples_total: None,
                reason: None,
            },
        }
    }
}

#[derive(Serialize, Clone)]
struct PeaksProgressEvent {
    media_id: String,
    state: &'static str,
    samples_done: Option<u64>,
    samples_total: Option<u64>,
    reason: Option<String>,
}

fn job_state_tag(s: &JobState) -> &'static str {
    match s {
        JobState::Queued => "queued",
        JobState::Running { .. } => "running",
        JobState::Done { .. } => "done",
        JobState::Failed(_) => "failed",
        JobState::Cancelled => "cancelled",
    }
}

/// 4-byte LE sentinel returned by `peaks_for_clip` when the slot isn't
/// ready (queued, running, missing on disk, or hash-mismatched). UI
/// checks the first 4 bytes; any other value means real payload.
const PEAKS_NOT_READY_SENTINEL: [u8; 4] = [0xff, 0xff, 0xff, 0xff];

/// Locate the bundled `vance-audio-decoder` sidecar binary. Mirrors
/// `engine.rs::locate_audio_binary` but returns a plain `String` error
/// for the non-engine context. Returns the path relative to
/// `current_exe`'s directory, where Tauri ships sidecars on every OS.
fn locate_audio_binary() -> Result<PathBuf, String> {
    let exe_name = if cfg!(windows) {
        "vance-audio-decoder.exe"
    } else {
        "vance-audio-decoder"
    };
    let self_exe = std::env::current_exe().map_err(|e| format!("current_exe(): {e}"))?;
    let dir = self_exe
        .parent()
        .ok_or_else(|| "current_exe has no parent".to_string())?;
    let cand = dir.join(exe_name);
    if cand.exists() {
        Ok(cand)
    } else {
        Err(format!("audio sidecar not found at {}", cand.display()))
    }
}

/// Default initial cache dir until M6 swaps it for the active bundle's
/// `<bundle>/cache/peaks/`. Lives under the OS temp dir so it's clearly
/// disposable.
fn default_peaks_cache_dir() -> PathBuf {
    std::env::temp_dir().join("vance-app-peaks-cache")
}

/// Clone the inner `Arc<PeaksService>` out of the handle and release
/// the lock before doing any real work. Every peaks_* command MUST go
/// through this helper — holding the PMutex across disk I/O or the 5s
/// shutdown latency would serialize every other peaks request.
fn checkout_peaks(state: &PeaksHandle) -> Result<Arc<PeaksService>, String> {
    state
        .lock()
        .as_ref()
        .cloned()
        .ok_or_else(|| "peaks service not initialized".to_string())
}

#[tauri::command]
fn peaks_status(
    state: tauri::State<'_, PeaksHandle>,
    media_id: String,
) -> Result<PeaksStatusResponse, String> {
    let uuid = media_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid media id: {e}"))?;
    let id = vance_timeline::MediaItemId::from_uuid(uuid);
    let svc = checkout_peaks(&state)?;
    Ok(svc.status(id).into())
}

#[tauri::command]
fn peaks_prioritize(state: tauri::State<'_, PeaksHandle>, media_id: String) -> Result<(), String> {
    let uuid = media_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid media id: {e}"))?;
    let id = vance_timeline::MediaItemId::from_uuid(uuid);
    let svc = checkout_peaks(&state)?;
    svc.prioritize(id);
    Ok(())
}

#[tauri::command]
fn peaks_cancel(state: tauri::State<'_, PeaksHandle>, media_id: String) -> Result<(), String> {
    let uuid = media_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid media id: {e}"))?;
    let id = vance_timeline::MediaItemId::from_uuid(uuid);
    let svc = checkout_peaks(&state)?;
    svc.cancel(id);
    Ok(())
}

/// Returns whether the peaks subsystem is available, plus a short
/// reason string when it isn't. UI calls this once on mount so a
/// missing sidecar or failed cache-dir create surfaces as a clear
/// "audio waveforms unavailable" banner instead of every peaks_*
/// command throwing the same opaque string.
#[tauri::command]
fn peaks_service_status(state: tauri::State<'_, PeaksHandle>) -> PeaksServiceAvailability {
    let available = state.lock().is_some();
    PeaksServiceAvailability {
        available,
        reason: if available {
            None
        } else {
            Some(
                "PeaksService failed to initialize: likely the audio sidecar binary is \
                 missing or the cache directory is unwritable. See logs."
                    .to_string(),
            )
        },
    }
}

#[derive(Serialize)]
struct PeaksServiceAvailability {
    available: bool,
    reason: Option<String>,
}

#[derive(Serialize)]
struct PeaksLevelMeta {
    level_idx: u8,
    samples_per_bucket: u32,
    bucket_count: u32,
}

#[derive(Serialize)]
struct PeaksMetaResponse {
    sample_rate: u32,
    total_samples: u64,
    level_count: u8,
    levels: Vec<PeaksLevelMeta>,
}

/// Inspect a `.peaks` slot's header + directory so the UI can compute
/// `choose_level` and bucket ranges without re-opening the file on
/// every draw. Returns `None` (serialized as `null`) when the slot
/// isn't on disk or is hash-mismatched — the UI treats both the same
/// as "wait for peaks://progress done".
#[tauri::command]
fn peaks_meta(
    state: tauri::State<'_, PeaksHandle>,
    content_hash_prefix_hex: String,
) -> Result<Option<PeaksMetaResponse>, String> {
    let prefix = parse_hash_prefix_hex(&content_hash_prefix_hex)?;
    let svc = checkout_peaks(&state)?;
    let slot = svc.slot_path(&prefix);
    match PeakFile::open(&slot) {
        Ok(pf) => {
            if pf.header.content_hash_prefix != prefix {
                return Ok(None);
            }
            let levels = pf
                .levels
                .iter()
                .map(|d| PeaksLevelMeta {
                    level_idx: d.level_idx,
                    samples_per_bucket: d.samples_per_bucket,
                    bucket_count: d.bucket_count,
                })
                .collect();
            Ok(Some(PeaksMetaResponse {
                sample_rate: pf.header.sample_rate,
                total_samples: pf.header.total_samples,
                level_count: pf.header.level_count,
                levels,
            }))
        }
        Err(PeakError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Return raw `[mins|maxes|rms]` lane bytes for the requested bucket
/// range, or a 4-byte not-ready sentinel if the slot isn't on disk
/// yet, isn't on disk ever (hash-mismatched / never extracted), or
/// the slot file's content-hash header doesn't match the requested
/// prefix (stale cache).
///
/// JS contract: response length is either 4 (sentinel — peaks aren't
/// ready) or `bucket_count * 6` (real payload). `bucket_count == 0` is
/// rejected as an error to keep the discrimination unambiguous.
#[tauri::command]
fn peaks_for_clip(
    state: tauri::State<'_, PeaksHandle>,
    content_hash_prefix_hex: String,
    level: u8,
    bucket_start: u32,
    bucket_count: u32,
) -> Result<tauri::ipc::Response, String> {
    if bucket_count == 0 {
        return Err("bucket_count must be > 0".to_string());
    }
    let prefix = parse_hash_prefix_hex(&content_hash_prefix_hex)?;
    let svc = checkout_peaks(&state)?;
    let slot = svc.slot_path(&prefix);
    match PeakFile::open(&slot) {
        Ok(pf) => {
            // Hash-prefix guard: a stale slot from before a re-import
            // could have a header that decodes fine but represents a
            // different file. Return the sentinel so the UI re-fetches
            // peaks via peaks_enqueue instead of rendering garbage.
            if pf.header.content_hash_prefix != prefix {
                return Ok(tauri::ipc::Response::new(PEAKS_NOT_READY_SENTINEL.to_vec()));
            }
            let bytes = pf
                .read_slice(level, bucket_start, bucket_count)
                .map_err(|e| e.to_string())?;
            Ok(tauri::ipc::Response::new(bytes))
        }
        Err(PeakError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(tauri::ipc::Response::new(PEAKS_NOT_READY_SENTINEL.to_vec()))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Convert the 24-char content-hash prefix hex into 12 bytes.
/// Accepts both upper and lower case (the on-disk filenames use
/// lowercase via [`hex_lower`], but tolerating both removes a class
/// of UI-side bugs). Operates byte-by-byte on the ASCII representation
/// so non-ASCII input is rejected with a clear error instead of
/// panicking in the underlying str-indexing.
fn parse_hash_prefix_hex(s: &str) -> Result<[u8; 12], String> {
    let bytes = s.as_bytes();
    if bytes.len() != 24 {
        return Err(format!(
            "content_hash_prefix_hex must be 24 chars, got {}",
            bytes.len()
        ));
    }
    let mut out = [0u8; 12];
    for i in 0..12 {
        let hi = hex_digit(bytes[i * 2])
            .ok_or_else(|| format!("byte {i} hi: not an ascii hex digit"))?;
        let lo = hex_digit(bytes[i * 2 + 1])
            .ok_or_else(|| format!("byte {i} lo: not an ascii hex digit"))?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + b - b'a'),
        b'A'..=b'F' => Some(10 + b - b'A'),
        _ => None,
    }
}

/// Build the `ProgressSink` that forwards every state transition to
/// the webview as a `peaks://progress` event. Throttling is already
/// applied inside `PeaksService` at ≤ 10 Hz; this sink does no further
/// coalescing.
fn build_progress_sink(app: tauri::AppHandle) -> ProgressSink {
    Arc::new(move |id: &vance_timeline::MediaItemId, state: &JobState| {
        let payload = PeaksProgressEvent {
            media_id: id.to_string(),
            state: job_state_tag(state),
            samples_done: match state {
                JobState::Running { samples_done, .. } => Some(*samples_done),
                JobState::Done { samples_total } => Some(*samples_total),
                _ => None,
            },
            samples_total: match state {
                JobState::Running { samples_total, .. } => *samples_total,
                JobState::Done { samples_total } => Some(*samples_total),
                _ => None,
            },
            reason: match state {
                JobState::Failed(r) => Some(r.clone()),
                _ => None,
            },
        };
        if let Err(e) = app.emit("peaks://progress", payload) {
            tracing::warn!(
                target: "vance_app.peaks",
                err = ?e,
                "peaks://progress emit failed"
            );
        }
    })
}

/// M6 will call this from the project-load Tauri command (`load_project`)
/// once it knows the active bundle's `<bundle>/cache/peaks` path. Until
/// then the service points at the OS temp dir.
///
/// Swaps the service atomically: the old worker is shut down (with the
/// 5-second join timeout), then a fresh one is started. Any in-flight
/// extraction is lost; M6 should enqueue everything the project needs
/// immediately after the swap.
/// Point the backend service at a fresh OS-temp-scoped cache dir for
/// an unsaved (Untitled-N) project. UI uses this between newProject
/// and the first Save so peaks for the new project don't write back
/// into whatever bundle was previously loaded. Same swap semantics as
/// [`peaks_set_cache_dir`].
#[tauri::command]
fn peaks_use_untitled_cache_dir(
    state: tauri::State<'_, PeaksHandle>,
    app: tauri::AppHandle,
    label: String,
) -> Result<String, String> {
    // Sanitize label so a UI bug can't traverse out of the temp root.
    let safe: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if safe.is_empty() {
        return Err("untitled label must contain at least one [A-Za-z0-9_-] char".into());
    }
    let dir = std::env::temp_dir()
        .join("vance-app-peaks-untitled")
        .join(&safe);
    let dir_str = dir.to_string_lossy().into_owned();
    peaks_set_cache_dir(state, app, dir_str.clone())?;
    Ok(dir_str)
}

#[tauri::command]
fn peaks_set_cache_dir(
    state: tauri::State<'_, PeaksHandle>,
    app: tauri::AppHandle,
    cache_dir: String,
) -> Result<(), String> {
    let new_dir = PathBuf::from(cache_dir);
    let binary = locate_audio_binary()?;
    let sink = build_progress_sink(app);
    let new_svc = PeaksService::start(binary, new_dir, sink).map_err(stringify_peaks_err)?;
    // Install the new service first, take the old one out, RELEASE the
    // lock, then drop the old. Drop runs PeaksService::do_shutdown
    // which can take up to 5 seconds — we must NOT hold the global
    // PeaksHandle lock across that or every concurrent peaks_*
    // command stalls.
    let old = {
        let mut slot = state.lock();
        let prev = slot.take();
        *slot = Some(Arc::new(new_svc));
        prev
    };
    drop(old);
    Ok(())
}

fn stringify_peaks_err(e: PeaksServiceError) -> String {
    e.to_string()
}

/// Enqueue peaks extraction for a media item. The UI hands back the
/// path + content-hash-prefix-hex; the service either skips work (fresh
/// cache) or schedules extraction. Idempotent: a duplicate call while
/// the same media id is Queued/Running is a no-op.
#[tauri::command]
fn peaks_enqueue(
    state: tauri::State<'_, PeaksHandle>,
    media_id: String,
    path: String,
    content_hash_prefix_hex: String,
    expected_duration_seconds: Option<f64>,
) -> Result<(), String> {
    let uuid = media_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid media id: {e}"))?;
    let prefix = parse_hash_prefix_hex(&content_hash_prefix_hex)?;
    // Sanitize: NaN/infinity/negative durations would feed garbage
    // into the staleness check and cause permanent re-extraction.
    let expected_duration_seconds = expected_duration_seconds.and_then(|d| {
        if d.is_finite() && (0.0..=24.0 * 3600.0).contains(&d) {
            Some(d)
        } else {
            None
        }
    });
    let job = PeakJob {
        media_id: vance_timeline::MediaItemId::from_uuid(uuid),
        path: PathBuf::from(path),
        content_hash_prefix: prefix,
        expected_duration_seconds,
    };
    let svc = checkout_peaks(&state)?;
    svc.enqueue(job).map_err(stringify_peaks_err)
}

#[derive(Serialize)]
struct PeaksBackfillResult {
    /// Number of video items the probe was run against.
    probed: usize,
    /// Number of items that actually had audio (flipped from
    /// `false` → `true`).
    flipped_to_has_audio: usize,
    /// Lookup table of media_id → has_audio_track for the items that
    /// were touched. UI consumes this to refresh its `media` array
    /// + the peaks cache without a full project re-fetch.
    updates: Vec<PeaksBackfillUpdate>,
}

#[derive(Serialize)]
struct PeaksBackfillUpdate {
    media_id: String,
    has_audio_track: bool,
}

/// Backfill `has_audio_track` for every Video media item still
/// holding the post-deserialize default (`false`). Called by
/// `ProjectState.open` after a `.vance/` bundle loads, so legacy
/// projects (pre-Phase-7.2) get their videos probed once and the
/// timeline can paint waveforms on Video clips without the user
/// having to re-import each file.
///
/// Probes run sequentially via the pooled probe sidecar in
/// PeaksService — typical cost ~5-15 ms per video file. A
/// project with 100 video clips therefore pays ~1 s total, executed
/// after the initial project-open render so the UI stays responsive.
#[tauri::command]
fn peaks_backfill_video_audio(
    engine: tauri::State<'_, EngineHandle>,
    peaks: tauri::State<'_, PeaksHandle>,
) -> Result<PeaksBackfillResult, String> {
    let svc = checkout_peaks(&peaks)?;
    let targets = engine.lock().list_video_media_needing_audio_probe();
    let mut updates = Vec::with_capacity(targets.len());
    let mut flipped = 0;
    for (id, path) in &targets {
        let path_str = path.to_string_lossy().into_owned();
        let has_audio = svc.probe_audio(&path_str).unwrap_or(false);
        if has_audio {
            flipped += 1;
        }
        let _ = engine.lock().set_media_has_audio_track(*id, has_audio);
        updates.push(PeaksBackfillUpdate {
            media_id: id.to_string(),
            has_audio_track: has_audio,
        });
    }
    Ok(PeaksBackfillResult {
        probed: targets.len(),
        flipped_to_has_audio: flipped,
        updates,
    })
}

#[derive(Serialize)]
struct OpenResponse {
    width: u32,
    height: u32,
    pixel_format: String,
    codec_name: String,
    frame_rate_num: i64,
    frame_rate_den: i64,
    time_base_num: i64,
    time_base_den: i64,
    duration_pts: Option<i64>,
}

impl From<StreamInfo> for OpenResponse {
    fn from(s: StreamInfo) -> Self {
        Self {
            width: s.width,
            height: s.height,
            pixel_format: format!("{:?}", s.pixel_format),
            codec_name: s.codec_name,
            frame_rate_num: s.frame_rate.0,
            frame_rate_den: s.frame_rate.1,
            time_base_num: s.time_base.0,
            time_base_den: s.time_base.1,
            duration_pts: s.duration_pts,
        }
    }
}

#[tauri::command]
fn backend_version() -> String {
    format!(
        "vance-app {} (rust {})",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_RUST_VERSION"),
    )
}

#[tauri::command]
fn set_viewport_bounds(
    state: tauri::State<'_, EngineHandle>,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<(), String> {
    state
        .lock()
        .set_viewport_bounds(ViewportBounds::new(x, y, w, h))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_source_viewport_bounds(
    state: tauri::State<'_, EngineHandle>,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<(), String> {
    state
        .lock()
        .set_source_viewport_bounds(ViewportBounds::new(x, y, w, h))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn load_source(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
) -> Result<OpenResponse, String> {
    let media_uuid = media_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid media id: {e}"))?;
    let info = state
        .lock()
        .load_source(vance_timeline::MediaItemId::from_uuid(media_uuid))
        .map_err(|e| e.to_string())?;
    Ok(info.into())
}

#[tauri::command]
fn decode_source_at(state: tauri::State<'_, EngineHandle>, target_pts: i64) -> Result<i64, String> {
    state
        .lock()
        .decode_source_at(target_pts)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn source_state(state: tauri::State<'_, EngineHandle>) -> Option<SourceState> {
    state.lock().source_state()
}

#[tauri::command]
fn set_source_mark_in(state: tauri::State<'_, EngineHandle>) -> Result<(), String> {
    state.lock().set_source_mark_in().map_err(|e| e.to_string())
}

#[tauri::command]
fn set_source_mark_out(state: tauri::State<'_, EngineHandle>) -> Result<(), String> {
    state
        .lock()
        .set_source_mark_out()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn clear_source_marks(state: tauri::State<'_, EngineHandle>) {
    state.lock().clear_source_marks();
}

#[tauri::command]
fn insert_from_source(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    overwrite: bool,
) -> Result<String, String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    state
        .lock()
        .insert_from_source(vance_timeline::TrackId::from_uuid(track_uuid), overwrite)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_file(state: tauri::State<'_, EngineHandle>, path: String) -> Result<OpenResponse, String> {
    let info = state.lock().open_file(&path).map_err(|e| e.to_string())?;
    Ok(info.into())
}

#[tauri::command]
fn decode_and_render_at(
    state: tauri::State<'_, EngineHandle>,
    target_pts: i64,
) -> Result<i64, String> {
    state
        .lock()
        .decode_and_render_at(target_pts)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn start_playback(state: tauri::State<'_, EngineHandle>, from_pts: i64) -> Result<(), String> {
    state
        .lock()
        .start_playback(from_pts)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn stop_playback(state: tauri::State<'_, EngineHandle>) -> Result<i64, String> {
    state.lock().stop_playback().map_err(|e| e.to_string())
}

#[tauri::command]
fn is_playing(state: tauri::State<'_, EngineHandle>) -> bool {
    state.lock().is_playing()
}

#[tauri::command]
fn last_resume_pts(state: tauri::State<'_, EngineHandle>) -> Option<i64> {
    state.lock().last_resume_pts()
}

#[tauri::command]
fn load_project(
    state: tauri::State<'_, EngineHandle>,
    path: String,
) -> Result<ProjectInfo, String> {
    state.lock().load_project(&path).map_err(|e| e.to_string())
}

#[tauri::command]
fn new_project(state: tauri::State<'_, EngineHandle>, name: String) -> Result<ProjectInfo, String> {
    state.lock().new_project(name).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_project(state: tauri::State<'_, EngineHandle>, path: Option<String>) -> Result<(), String> {
    state.lock().save_project(path).map_err(|e| e.to_string())
}

#[tauri::command]
fn current_project_path(state: tauri::State<'_, EngineHandle>) -> Option<String> {
    state
        .lock()
        .current_project_path()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
}

#[tauri::command]
fn current_sequence_contents(state: tauri::State<'_, EngineHandle>) -> Option<SequenceContents> {
    state.lock().current_sequence_contents()
}

/// Lets the chrome write a free-form line into the engine's tracing
/// output. Only used for debugging the JS ↔ engine boundary.
#[tauri::command]
fn diag_log(message: String) {
    tracing::info!(target: "ui_diag", "{message}");
}

#[tauri::command]
fn import_media_file(
    state: tauri::State<'_, EngineHandle>,
    peaks: tauri::State<'_, PeaksHandle>,
    path: String,
) -> Result<MediaItemInfo, String> {
    // Phase 7.2 import has two steps that must each acquire the
    // engine lock (one to import, one to set has_audio_track), with
    // the probe IPC happening BETWEEN them so we don't hold the lock
    // across a possibly-slow sidecar round-trip. A concurrent command
    // could in principle delete the just-imported item between those
    // two engine.lock() acquisitions — set_media_has_audio_track
    // tolerates that case (returns Ok if the id is unknown).
    let info = state
        .lock()
        .import_media_file(&path)
        .map_err(|e| e.to_string())?;
    // info.id was just generated by MediaItemId::new() inside the
    // engine, so the UUID parse cannot fail. Unwrap is safe here.
    let media_id = vance_timeline::MediaItemId::from_uuid(
        info.id
            .parse::<uuid::Uuid>()
            .expect("engine-generated UUID"),
    );

    // Only Video items need probing. Audio is always true (forced by
    // MediaItem::new + bundle-load sweep); Image/Title stay false.
    if info.kind == "video" {
        match checkout_peaks(&peaks) {
            Ok(svc) => match svc.probe_audio(&path) {
                Ok(has_audio) => {
                    let _ = state.lock().set_media_has_audio_track(media_id, has_audio);
                }
                Err(e) => {
                    tracing::warn!(
                        target: "vance_app.peaks",
                        path = %path,
                        err = ?e,
                        "audio probe failed; treating as no-audio (waveform won't render until backfill)"
                    );
                }
            },
            Err(e) => {
                // PeaksService never spawned (sidecar missing). UI's
                // backfill pass will retry on next project-open once
                // the service comes back; until then, video clips
                // won't render waveforms.
                tracing::warn!(
                    target: "vance_app.peaks",
                    path = %path,
                    err = %e,
                    "peaks service unavailable at import; has_audio_track stays false"
                );
            }
        }
    }
    // Re-read the (possibly updated) info from the engine so the UI
    // sees has_audio_track reflecting the probe.
    Ok(state.lock().media_get_info(media_id).unwrap_or(info))
}

#[tauri::command]
fn media_list(state: tauri::State<'_, EngineHandle>) -> Vec<MediaItemInfo> {
    state.lock().media_list()
}

#[tauri::command]
fn play_sequence(
    state: tauri::State<'_, EngineHandle>,
    timeline_seconds: f64,
) -> Result<(), String> {
    state
        .lock()
        .play_sequence(timeline_seconds)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn decode_sequence_at(
    state: tauri::State<'_, EngineHandle>,
    timeline_seconds: f64,
) -> Result<i64, String> {
    state
        .lock()
        .decode_sequence_at(timeline_seconds)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn current_timeline_seconds(state: tauri::State<'_, EngineHandle>) -> Option<f64> {
    state.lock().current_timeline_seconds()
}

#[tauri::command]
fn undo(state: tauri::State<'_, EngineHandle>) -> Result<bool, String> {
    state.lock().undo().map_err(|e| e.to_string())
}

#[tauri::command]
fn redo(state: tauri::State<'_, EngineHandle>) -> Result<bool, String> {
    state.lock().redo().map_err(|e| e.to_string())
}

#[tauri::command]
fn can_undo(state: tauri::State<'_, EngineHandle>) -> bool {
    state.lock().can_undo()
}

#[tauri::command]
fn can_redo(state: tauri::State<'_, EngineHandle>) -> bool {
    state.lock().can_redo()
}

/// The current sequence's edit-history tree (for the history / branch panel).
#[tauri::command]
fn edit_history(state: tauri::State<'_, EngineHandle>) -> Vec<vance_timeline::HistoryNodeView> {
    state.lock().edit_history()
}

/// Name the current history node as a checkpoint / branch.
#[tauri::command]
fn checkpoint_history(state: tauri::State<'_, EngineHandle>, name: String) -> Result<(), String> {
    state
        .lock()
        .checkpoint_history(name)
        .map_err(|e| e.to_string())
}

/// Jump to a history node by id (switch branch / restore version).
#[tauri::command]
fn goto_history(state: tauri::State<'_, EngineHandle>, node_id: usize) -> Result<bool, String> {
    state
        .lock()
        .goto_history(node_id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn trim_clip_in(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    new_source_in_seconds: f64,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    state
        .lock()
        .trim_clip_in(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            new_source_in_seconds,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn trim_clip_out(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    new_source_out_seconds: f64,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    state
        .lock()
        .trim_clip_out(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            new_source_out_seconds,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn add_effect_to_clip(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    kind: String,
    params: std::collections::BTreeMap<String, vance_timeline::EffectValue>,
) -> Result<String, String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    state
        .lock()
        .add_effect_to_clip(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            kind,
            params,
        )
        .map_err(|e| e.to_string())
}

/// GOAT-2 — live compile-check of a parameter expression (no apply).
/// Returns the error message, or null when valid.
#[tauri::command]
fn validate_expression(expr: String) -> Option<String> {
    vance_timeline::validate_expr(&expr)
}

#[tauri::command]
fn update_effect_param(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    effect_id: String,
    key: String,
    value: vance_timeline::EffectValue,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    let effect_uuid = effect_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid effect id: {e}"))?;
    state
        .lock()
        .update_effect_param(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            vance_timeline::EffectId::from_uuid(effect_uuid),
            key,
            value,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_effect_from_clip(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    effect_id: String,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    let effect_uuid = effect_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid effect id: {e}"))?;
    state
        .lock()
        .remove_effect_from_clip(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            vance_timeline::EffectId::from_uuid(effect_uuid),
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn move_effect_on_clip(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    effect_id: String,
    new_index: usize,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    let effect_uuid = effect_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid effect id: {e}"))?;
    state
        .lock()
        .move_effect_on_clip(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            vance_timeline::EffectId::from_uuid(effect_uuid),
            new_index,
        )
        .map_err(|e| e.to_string())
}

/// Phase 7.3 mixer command: set a track's mute.
#[tauri::command]
fn set_track_mute(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    mute: bool,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    state
        .lock()
        .set_track_mute(vance_timeline::TrackId::from_uuid(track_uuid), mute)
        .map_err(|e| e.to_string())
}

/// Phase 7.3 mixer command: set a track's solo flag.
#[tauri::command]
fn set_track_solo(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    solo: bool,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    state
        .lock()
        .set_track_solo(vance_timeline::TrackId::from_uuid(track_uuid), solo)
        .map_err(|e| e.to_string())
}

/// Phase 7.3 mixer command: set a track's gain in dB.
#[tauri::command]
fn set_track_gain_db(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    gain_db: f32,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    state
        .lock()
        .set_track_gain_db(vance_timeline::TrackId::from_uuid(track_uuid), gain_db)
        .map_err(|e| e.to_string())
}

/// Phase 7.3 mixer command: set a clip's mute (routes through
/// `Clip.enabled` per the existing semantics).
#[tauri::command]
fn set_clip_mute(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    mute: bool,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    state
        .lock()
        .set_clip_mute(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            mute,
        )
        .map_err(|e| e.to_string())
}

/// Phase 7.4 mixer command: set the master bus gain in dB. Sequence-
/// level; no per-track lock to respect.
#[tauri::command]
fn set_master_gain_db(state: tauri::State<'_, EngineHandle>, gain_db: f32) -> Result<(), String> {
    state
        .lock()
        .set_master_gain_db(gain_db)
        .map_err(|e| e.to_string())
}

// ---- GOAT-4: mixer buses + sends (Fairlight console) ----------------------

fn parse_bus_id(s: &str) -> Result<vance_timeline::BusId, String> {
    s.parse::<uuid::Uuid>()
        .map(vance_timeline::BusId::from_uuid)
        .map_err(|e| format!("bad bus id: {e}"))
}
fn parse_track_id(s: &str) -> Result<vance_timeline::TrackId, String> {
    s.parse::<uuid::Uuid>()
        .map(vance_timeline::TrackId::from_uuid)
        .map_err(|e| format!("bad track id: {e}"))
}

/// All buses on the current sequence (console channel strips).
#[tauri::command]
fn buses(state: tauri::State<'_, EngineHandle>) -> Vec<vance_timeline::AudioBus> {
    state.lock().buses()
}

/// Add a submix bus. Returns its id.
#[tauri::command]
fn add_bus(state: tauri::State<'_, EngineHandle>, name: String) -> Result<String, String> {
    state.lock().add_bus(name).map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_bus(state: tauri::State<'_, EngineHandle>, bus_id: String) -> Result<(), String> {
    let id = parse_bus_id(&bus_id)?;
    state.lock().remove_bus(id).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_bus_gain_db(
    state: tauri::State<'_, EngineHandle>,
    bus_id: String,
    gain_db: f32,
) -> Result<(), String> {
    let id = parse_bus_id(&bus_id)?;
    state
        .lock()
        .set_bus_gain_db(id, gain_db)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_bus_pan(
    state: tauri::State<'_, EngineHandle>,
    bus_id: String,
    pan: f32,
) -> Result<(), String> {
    let id = parse_bus_id(&bus_id)?;
    state.lock().set_bus_pan(id, pan).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_bus_mute(
    state: tauri::State<'_, EngineHandle>,
    bus_id: String,
    mute: bool,
) -> Result<(), String> {
    let id = parse_bus_id(&bus_id)?;
    state
        .lock()
        .set_bus_mute(id, mute)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_bus_output(
    state: tauri::State<'_, EngineHandle>,
    bus_id: String,
    output: vance_timeline::BusOutput,
) -> Result<(), String> {
    let id = parse_bus_id(&bus_id)?;
    state
        .lock()
        .set_bus_output(id, output)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn route_track_to_bus(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    output: vance_timeline::BusOutput,
) -> Result<(), String> {
    let id = parse_track_id(&track_id)?;
    state
        .lock()
        .route_track_to_bus(id, output)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn add_track_send(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    send: vance_timeline::Send,
) -> Result<(), String> {
    let id = parse_track_id(&track_id)?;
    state
        .lock()
        .add_track_send(id, send)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_track_send(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    index: usize,
) -> Result<(), String> {
    let id = parse_track_id(&track_id)?;
    state
        .lock()
        .remove_track_send(id, index)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn add_bus_insert(
    state: tauri::State<'_, EngineHandle>,
    bus_id: String,
    effect: vance_timeline::AudioEffectSpec,
) -> Result<(), String> {
    let id = parse_bus_id(&bus_id)?;
    state
        .lock()
        .add_bus_insert(id, effect)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_bus_insert(
    state: tauri::State<'_, EngineHandle>,
    bus_id: String,
    index: usize,
) -> Result<(), String> {
    let id = parse_bus_id(&bus_id)?;
    state
        .lock()
        .remove_bus_insert(id, index)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn move_bus_insert(
    state: tauri::State<'_, EngineHandle>,
    bus_id: String,
    from_index: usize,
    to_index: usize,
) -> Result<(), String> {
    let id = parse_bus_id(&bus_id)?;
    state
        .lock()
        .move_bus_insert(id, from_index, to_index)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn update_bus_insert(
    state: tauri::State<'_, EngineHandle>,
    bus_id: String,
    index: usize,
    effect: vance_timeline::AudioEffectSpec,
) -> Result<(), String> {
    let id = parse_bus_id(&bus_id)?;
    state
        .lock()
        .update_bus_insert(id, index, effect)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn add_bus_send(
    state: tauri::State<'_, EngineHandle>,
    bus_id: String,
    send: vance_timeline::Send,
) -> Result<(), String> {
    let id = parse_bus_id(&bus_id)?;
    state
        .lock()
        .add_bus_send(id, send)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_bus_send(
    state: tauri::State<'_, EngineHandle>,
    bus_id: String,
    index: usize,
) -> Result<(), String> {
    let id = parse_bus_id(&bus_id)?;
    state
        .lock()
        .remove_bus_send(id, index)
        .map_err(|e| e.to_string())
}

/// Phase 7.5 M1 — UI-facing track meter info.
#[derive(serde::Serialize, Clone, Copy, Debug, Default)]
struct UiTrackMeter {
    peak_l: f32,
    peak_r: f32,
    rms_l: f32,
    rms_r: f32,
}

impl From<vance_audio::TrackMeter> for UiTrackMeter {
    fn from(m: vance_audio::TrackMeter) -> Self {
        Self {
            peak_l: m.peak_l,
            peak_r: m.peak_r,
            rms_l: m.rms_l,
            rms_r: m.rms_r,
        }
    }
}

/// Phase 7.8 — BS.1770 + true-peak readout for the UI.
#[derive(serde::Serialize, Clone, Copy, Debug, Default)]
struct UiLoudness {
    /// Momentary loudness in LUFS (latest 400 ms block). `null`-like
    /// values become serialised `null` on the wire via NaN→null.
    lufs_m: f32,
    /// Short-term loudness — sliding 3 s window.
    lufs_s: f32,
    /// Integrated loudness — gated across the playback session.
    lufs_i: f32,
    /// True-peak in dBFS, left + right. Includes inter-sample peaks
    /// via 4× polyphase oversampling.
    true_peak_l_db: f32,
    true_peak_r_db: f32,
}

/// Phase 7.5 M1 — meter snapshot for the UI. `null` when the mixer
/// backend isn't active (single-source path or no playback).
#[derive(serde::Serialize, Clone, Debug, Default)]
struct UiMeterSnapshot {
    master: UiTrackMeter,
    /// Per-track meters keyed by track UUID (matches `TrackContents.id`).
    tracks: std::collections::HashMap<String, UiTrackMeter>,
    /// Monotonic publish counter. UI uses this to detect a stalled or
    /// reset fold thread without polling at the fold publish rate.
    generation: u64,
    /// Phase 7.8 — BS.1770 loudness + true-peak on the master bus.
    loudness: UiLoudness,
}

/// Phase 8 — UI-facing render request.
#[derive(serde::Deserialize, Clone, Debug)]
struct UiRenderRequest {
    output_path: String,
    /// "ProRes422HQ" | "ProRes422" | "ProRes4444" | "DnxhrHq" |
    /// "DnxhrSq" | "DnxhrLb" | "H264" | "Hevc" | "Av1".
    preset: String,
    #[serde(default)]
    crf: Option<u8>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    start_seconds: Option<f64>,
    #[serde(default)]
    end_seconds: Option<f64>,
    /// HDR10 export (10-bit HEVC Main10, Rec.2020 PQ). Defaults off.
    #[serde(default)]
    hdr: bool,
}

#[derive(serde::Serialize, Clone, Debug)]
struct UiRenderOutcome {
    output_path: String,
    video_frames: u64,
    audio_samples: u64,
    elapsed_seconds: f64,
}

/// Phase 8 — kick off a sequence render via the encoder sidecar.
/// Blocks until the encode is complete. Background queue + cancellation
/// land with 8.5.
#[tauri::command]
fn render_sequence(
    state: tauri::State<'_, EngineHandle>,
    req: UiRenderRequest,
) -> Result<UiRenderOutcome, String> {
    let preset = match req.preset.as_str() {
        "ProRes422HQ" => vance_encoder::EncoderPreset::ProRes422HQ,
        "ProRes422" => vance_encoder::EncoderPreset::ProRes422,
        "ProRes4444" => vance_encoder::EncoderPreset::ProRes4444,
        "DnxhrHq" => vance_encoder::EncoderPreset::DnxhrHq,
        "DnxhrSq" => vance_encoder::EncoderPreset::DnxhrSq,
        "DnxhrLb" => vance_encoder::EncoderPreset::DnxhrLb,
        "H264" => vance_encoder::EncoderPreset::H264 {
            crf: req.crf.unwrap_or(23),
        },
        "Hevc" => vance_encoder::EncoderPreset::Hevc {
            crf: req.crf.unwrap_or(28),
        },
        "Av1" => vance_encoder::EncoderPreset::Av1 {
            crf: req.crf.unwrap_or(30),
        },
        other => return Err(format!("unknown preset `{other}`")),
    };
    let opts = crate::engine::RenderOptions {
        output_path: req.output_path.clone(),
        preset,
        width: req.width,
        height: req.height,
        render_and_replace_track: None,
        start_seconds: req.start_seconds,
        end_seconds: req.end_seconds,
        hdr: req.hdr,
    };
    let outcome = state
        .lock()
        .render_sequence(opts)
        .map_err(|e| e.to_string())?;
    Ok(UiRenderOutcome {
        output_path: outcome.output_path,
        video_frames: outcome.video_frames,
        audio_samples: outcome.audio_samples,
        elapsed_seconds: outcome.elapsed_seconds,
    })
}

/// Phase 8.7 — render-and-replace request from the UI.
#[derive(serde::Deserialize, Clone, Debug)]
struct UiRenderAndReplaceRequest {
    output_path: String,
    preset: String,
    #[serde(default)]
    crf: Option<u8>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    track_id: String,
    start_seconds: f64,
    end_seconds: f64,
}

#[derive(serde::Serialize, Clone, Debug)]
struct UiRenderAndReplaceOutcome {
    render: UiRenderOutcome,
    new_media_id: String,
    new_clip_id: String,
}

/// Phase 8.7 — render a sub-range of the active sequence to a new
/// media file + replace the range on `track_id` with a clip
/// referencing it. Composes render_sequence + import_media_file +
/// OverwriteClip into one undoable operation.
#[tauri::command]
fn render_and_replace(
    state: tauri::State<'_, EngineHandle>,
    req: UiRenderAndReplaceRequest,
) -> Result<UiRenderAndReplaceOutcome, String> {
    let preset = match req.preset.as_str() {
        "ProRes422HQ" => vance_encoder::EncoderPreset::ProRes422HQ,
        "ProRes422" => vance_encoder::EncoderPreset::ProRes422,
        "ProRes4444" => vance_encoder::EncoderPreset::ProRes4444,
        "DnxhrHq" => vance_encoder::EncoderPreset::DnxhrHq,
        "DnxhrSq" => vance_encoder::EncoderPreset::DnxhrSq,
        "DnxhrLb" => vance_encoder::EncoderPreset::DnxhrLb,
        "H264" => vance_encoder::EncoderPreset::H264 {
            crf: req.crf.unwrap_or(23),
        },
        "Hevc" => vance_encoder::EncoderPreset::Hevc {
            crf: req.crf.unwrap_or(28),
        },
        "Av1" => vance_encoder::EncoderPreset::Av1 {
            crf: req.crf.unwrap_or(30),
        },
        other => return Err(format!("unknown preset `{other}`")),
    };
    let track_uuid: uuid::Uuid = req
        .track_id
        .parse()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let opts = crate::engine::RenderAndReplaceOptions {
        output_path: req.output_path,
        preset,
        width: req.width,
        height: req.height,
        track_id: vance_timeline::TrackId::from_uuid(track_uuid),
        start_seconds: req.start_seconds,
        end_seconds: req.end_seconds,
    };
    let outcome = state
        .lock()
        .render_and_replace(opts)
        .map_err(|e| e.to_string())?;
    Ok(UiRenderAndReplaceOutcome {
        render: UiRenderOutcome {
            output_path: outcome.render.output_path,
            video_frames: outcome.render.video_frames,
            audio_samples: outcome.render.audio_samples,
            elapsed_seconds: outcome.render.elapsed_seconds,
        },
        new_media_id: outcome.new_media_id,
        new_clip_id: outcome.new_clip_id,
    })
}

/// Phase 9.4 — enqueue a proxy generation job. Returns the job id.
/// `codec` is `"prores"` (default) or `"dnxhr_lb"`.
#[tauri::command]
fn enqueue_proxy_generation(
    state: tauri::State<'_, EngineHandle>,
    source_path: String,
    output_path: String,
    codec: Option<String>,
) -> Result<String, String> {
    let codec = codec
        .as_deref()
        .map(crate::proxy::ProxyCodec::from_ui_str)
        .unwrap_or_default();
    state
        .lock()
        .enqueue_proxy_generation(source_path, output_path, codec)
        .map_err(|e| e.to_string())
}

#[derive(serde::Serialize, Clone, Debug)]
struct UiProxyJobSnapshot {
    id: String,
    source_path: String,
    output_path: String,
    /// "queued" | "running" | "done" | "error"
    state: &'static str,
    /// Frames processed so far. `null` until the job starts running.
    frames_done: Option<u64>,
    /// Error message when `state == "error"`.
    error: Option<String>,
}

/// Phase 9.4 — poll proxy-generation status. Safe to call at any rate.
#[tauri::command]
fn proxy_jobs(state: tauri::State<'_, EngineHandle>) -> Vec<UiProxyJobSnapshot> {
    state
        .lock()
        .proxy_jobs_snapshot()
        .into_iter()
        .map(|j| {
            let (st, frames_done, error) = match j.status {
                crate::proxy::ProxyJobStatus::Queued => ("queued", None, None),
                crate::proxy::ProxyJobStatus::Running { frames_done } => {
                    ("running", Some(frames_done), None)
                }
                crate::proxy::ProxyJobStatus::Done { frames_done } => {
                    ("done", Some(frames_done), None)
                }
                crate::proxy::ProxyJobStatus::Error(msg) => ("error", None, Some(msg)),
            };
            UiProxyJobSnapshot {
                id: j.id,
                source_path: j.source_path,
                output_path: j.output_path,
                state: st,
                frames_done,
                error,
            }
        })
        .collect()
}

/// Phase 10 — UI descriptor for one discovered WASM plugin.
#[derive(serde::Serialize, Clone, Debug)]
struct UiWasmPluginParam {
    key: String,
    label: String,
    min: f32,
    max: f32,
    default: f32,
}

#[derive(serde::Serialize, Clone, Debug)]
struct UiWasmPlugin {
    id: String,
    name: String,
    version: String,
    author: String,
    /// "effect" | "transition"
    kind: &'static str,
    /// M6 — "wasm" | "shader": how the effect runs, so the UI applies the right
    /// `wasm:<id>` / `shader:<id>` effect kind.
    runtime: &'static str,
    params: Vec<UiWasmPluginParam>,
}

/// Phase 10.3 — rescan the plugin folders. Returns the count found.
#[tauri::command]
fn rescan_plugins(state: tauri::State<'_, EngineHandle>) -> usize {
    state.lock().rescan_plugins()
}

/// Map a discovered plugin to its UI descriptor.
fn ui_wasm_plugin(d: &vance_plugin_sdk::discovery::DiscoveredPlugin) -> UiWasmPlugin {
    UiWasmPlugin {
        id: d.manifest.id.clone(),
        name: d.manifest.name.clone(),
        version: d.manifest.version.clone(),
        author: d.manifest.author.clone(),
        kind: match d.manifest.kind {
            vance_plugin_sdk::manifest::PluginKind::Effect => "effect",
            vance_plugin_sdk::manifest::PluginKind::Transition => "transition",
        },
        runtime: if d.shader { "shader" } else { "wasm" },
        params: d
            .manifest
            .params
            .iter()
            .map(|p| UiWasmPluginParam {
                key: p.key.clone(),
                label: p.label.clone(),
                min: p.min,
                max: p.max,
                default: p.default,
            })
            .collect(),
    }
}

/// Phase 10.5 — list discovered WASM plugins for the effects browser.
/// Triggers a rescan first so freshly-dropped plugins appear without
/// a restart.
#[tauri::command]
fn list_wasm_plugins(state: tauri::State<'_, EngineHandle>) -> Vec<UiWasmPlugin> {
    let mut eng = state.lock();
    eng.rescan_plugins();
    eng.discovered_plugins().iter().map(ui_wasm_plugin).collect()
}

/// Uninstall an add-on effect by id: delete its `.wasm` from the managed
/// add-ons folder and rescan. Refuses to touch project-bundled effects.
#[tauri::command]
fn remove_wasm_plugin(state: tauri::State<'_, EngineHandle>, id: String) -> Result<(), String> {
    let mut eng = state.lock();
    eng.rescan_plugins();
    let path = eng
        .discovered_plugins()
        .into_iter()
        .find(|d| d.manifest.id == id)
        .map(|d| d.path);
    let path = path.ok_or_else(|| "that effect isn't installed".to_string())?;
    // Only allow removing effects the user added (the managed folder), never a
    // project-bundled one.
    let user_dir = vance_plugin_sdk::discovery::user_plugins_dir();
    match &user_dir {
        Some(ud) if path.starts_with(ud) => {}
        _ => return Err("This effect is part of the project, not your add-ons.".into()),
    }
    std::fs::remove_file(&path).map_err(|e| format!("removing the effect: {e}"))?;
    eng.rescan_plugins();
    Ok(())
}

/// The managed add-on-effects folder (`…/.vance/plugins`), created if missing.
#[tauri::command]
fn plugins_folder() -> Result<String, String> {
    let dir = vance_plugin_sdk::discovery::user_plugins_dir()
        .ok_or_else(|| "couldn't locate the add-ons folder".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating add-ons folder: {e}"))?;
    Ok(dir.to_string_lossy().into_owned())
}

/// Install a dropped/imported `.wasm` add-on effect: copy it into the managed
/// add-ons folder, rescan, and return the newly-usable effect — or an error if
/// the file isn't a valid Vance effect. So the user never touches a folder.
#[tauri::command]
fn add_wasm_plugin(
    state: tauri::State<'_, EngineHandle>,
    source: String,
) -> Result<UiWasmPlugin, String> {
    let src = std::path::PathBuf::from(&source);
    if src.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()) != Some("wasm".into())
    {
        return Err("That file isn't an add-on effect (.wasm).".into());
    }
    let dir = vance_plugin_sdk::discovery::user_plugins_dir()
        .ok_or_else(|| "couldn't locate the add-ons folder".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating add-ons folder: {e}"))?;
    let file_name = src
        .file_name()
        .ok_or_else(|| "that file has no name".to_string())?;
    let dest = dir.join(file_name);

    std::fs::copy(&src, &dest).map_err(|e| format!("saving the effect: {e}"))?;

    let mut eng = state.lock();
    eng.rescan_plugins();
    let found = eng
        .discovered_plugins()
        .into_iter()
        .find(|d| d.path == dest);
    match found {
        Some(d) => Ok(ui_wasm_plugin(&d)),
        None => {
            // Not a valid Vance effect — don't leave a dud in the folder.
            let _ = std::fs::remove_file(&dest);
            Err("This file isn't a Vance add-on effect, or it couldn't be loaded.".into())
        }
    }
}

/// Phase 10.7 — read-only OFX plugin catalog entry for the UI.
#[derive(serde::Serialize, Clone, Debug)]
struct UiOfxPlugin {
    bundle: String,
    identifier: String,
    api: String,
    api_version: i32,
    version_major: u32,
    version_minor: u32,
}

#[derive(serde::Serialize, Clone, Debug, Default)]
struct UiOfxCatalog {
    plugins: Vec<UiOfxPlugin>,
    /// Per-bundle non-fatal load errors (so the UI can show why a
    /// bundle didn't enumerate).
    errors: Vec<String>,
}

/// Phase 10.7 — list installed OFX plugins via the sandboxed
/// `vance-ofx-host` sidecar (out-of-process dlopen, so a bad bundle
/// crashes only the sidecar). Read-only catalog: OFX plugins are not
/// yet runnable in the render pipeline (post-v1). Returns an empty
/// catalog when the sidecar binary is absent.
#[tauri::command]
fn list_ofx_plugins(_state: tauri::State<'_, EngineHandle>) -> UiOfxCatalog {
    let bin = match locate_ofx_host_binary() {
        Some(p) => p,
        None => return UiOfxCatalog::default(),
    };
    let mut client = match vance_ofx::OfxHostClient::spawn(&bin) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(?e, "ofx host spawn failed");
            return UiOfxCatalog::default();
        }
    };
    let (plugins, errors) = match client.list_plugins(Vec::new()) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(?e, "ofx list_plugins failed");
            let _ = client.shutdown();
            return UiOfxCatalog::default();
        }
    };
    let _ = client.shutdown();
    UiOfxCatalog {
        plugins: plugins
            .into_iter()
            .map(|p| UiOfxPlugin {
                bundle: p.bundle,
                identifier: p.identifier,
                api: p.api,
                api_version: p.api_version,
                version_major: p.version_major,
                version_minor: p.version_minor,
            })
            .collect(),
        errors,
    }
}

/// Locate the `vance-ofx-host` sidecar binary (next to the app exe, or
/// under target/debug|release for dev).
fn locate_ofx_host_binary() -> Option<std::path::PathBuf> {
    let name = if cfg!(windows) {
        "vance-ofx-host.exe"
    } else {
        "vance-ofx-host"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let p = parent.join(name);
            if p.exists() {
                return Some(p);
            }
        }
    }
    for c in [
        std::path::PathBuf::from("target/debug").join(name),
        std::path::PathBuf::from("target/release").join(name),
    ] {
        if c.exists() {
            return Some(c);
        }
    }
    None
}

/// Phase 9.5 — toggle the project-level "use proxies" flag.
#[tauri::command]
fn set_use_proxies(state: tauri::State<'_, EngineHandle>, on: bool) -> Result<(), String> {
    state.lock().set_use_proxies(on).map_err(|e| e.to_string())
}

/// Phase 9.5 — register a proxy file for a media item. Passing
/// `null` clears it.
#[tauri::command]
fn set_media_proxy_path(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
    proxy_path: Option<String>,
) -> Result<(), String> {
    let uuid: uuid::Uuid = media_id.parse().map_err(|e| format!("bad media id: {e}"))?;
    let id = vance_timeline::MediaItemId::from_uuid(uuid);
    let path = proxy_path.map(std::path::PathBuf::from);
    state
        .lock()
        .set_media_proxy_path(id, path)
        .map_err(|e| e.to_string())
}

// ---- GOAT-3: Avid-grade bins (custom columns + smart bins) ----------------

/// Set or clear (value=null) a custom-column value on a media item.
#[tauri::command]
fn set_media_metadata(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
    key: String,
    value: Option<String>,
) -> Result<(), String> {
    let uuid: uuid::Uuid = media_id.parse().map_err(|e| format!("bad media id: {e}"))?;
    let id = vance_timeline::MediaItemId::from_uuid(uuid);
    state
        .lock()
        .set_media_metadata(id, key, value)
        .map_err(|e| e.to_string())
}

/// Media ids matching a query (the browser filter box / deep search).
#[tauri::command]
fn query_media(
    state: tauri::State<'_, EngineHandle>,
    query: vance_timeline::MediaQuery,
) -> Vec<String> {
    state.lock().query_media(&query)
}

/// Union of custom-column keys across the library (the browser's columns).
#[tauri::command]
fn custom_columns(state: tauri::State<'_, EngineHandle>) -> Vec<String> {
    state.lock().custom_columns()
}

/// Add a smart bin (saved search). Returns its id.
#[tauri::command]
fn add_smart_bin(
    state: tauri::State<'_, EngineHandle>,
    name: String,
    query: vance_timeline::MediaQuery,
) -> Result<String, String> {
    state
        .lock()
        .add_smart_bin(name, query)
        .map_err(|e| e.to_string())
}

/// Remove a smart bin by id. Returns whether one was removed.
#[tauri::command]
fn remove_smart_bin(state: tauri::State<'_, EngineHandle>, id: String) -> Result<bool, String> {
    state
        .lock()
        .remove_smart_bin(&id)
        .map_err(|e| e.to_string())
}

/// All smart bins (for the browser sidebar).
#[tauri::command]
fn smart_bins(state: tauri::State<'_, EngineHandle>) -> Vec<vance_timeline::SmartBin> {
    state.lock().smart_bins()
}

/// Current member ids of a smart bin.
#[tauri::command]
fn smart_bin_members(state: tauri::State<'_, EngineHandle>, id: String) -> Vec<String> {
    state.lock().smart_bin_members(&id)
}

/// Phase 9.2 — poll frame-cache stats.
#[derive(serde::Serialize, Clone, Copy, Debug, Default)]
struct UiCacheStats {
    ring_count: u64,
    ring_total_bytes: u64,
    disk_entries: u64,
    disk_bytes: u64,
    ring_hits: u64,
    disk_hits: u64,
    misses: u64,
    /// Cache hit ratio in `[0, 1]` (ring+disk hits / total lookups).
    hit_ratio: f64,
}

#[tauri::command]
fn frame_cache_stats(state: tauri::State<'_, EngineHandle>) -> UiCacheStats {
    if let Some(s) = state.lock().frame_cache_stats() {
        UiCacheStats {
            ring_count: s.ring_count as u64,
            ring_total_bytes: s.ring_total_bytes as u64,
            disk_entries: s.disk_entries as u64,
            disk_bytes: s.disk_bytes,
            ring_hits: s.ring_hits,
            disk_hits: s.disk_hits,
            misses: s.misses,
            hit_ratio: s.hit_ratio(),
        }
    } else {
        UiCacheStats::default()
    }
}

/// Phase 7.10 / 7.11 — UI-facing plugin descriptor.
#[derive(serde::Serialize, Clone, Debug)]
struct UiPluginDescriptor {
    format: String,
    id: String,
    name: String,
    vendor: String,
    runnable: bool,
}

/// Phase 7.10 / 7.11 — enumerate every plugin the sandboxed
/// `vance-plugin-host` sidecar can host on the current system.
/// Returns an empty list (NOT an error) when the sidecar binary is
/// not present so the UI degrades gracefully.
#[tauri::command]
fn list_audio_plugins(
    _state: tauri::State<'_, EngineHandle>,
) -> Result<Vec<UiPluginDescriptor>, String> {
    let bin = match locate_plugin_host_binary() {
        Some(p) => p,
        None => return Ok(Vec::new()),
    };
    let mut client = match vance_audio_plugins::PluginClient::spawn(&bin) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(?e, "plugin host spawn failed");
            return Ok(Vec::new());
        }
    };

    // Detect a discovery/processing feature split: the app hosts plugins
    // in-process (its own `clap`/`lv2` features) while the sidecar
    // discovers them (its features, reported via the Hello handshake). A
    // partial build (one rebuilt, the other not) yields a confusing
    // "plugin added but silent / discovered but unusable" state — warn
    // loudly so it's diagnosable instead of mysterious.
    let sidecar_formats = client.supported_formats();
    let sidecar_has = |f: vance_audio_plugins::PluginFormat| sidecar_formats.contains(&f);
    for (feat_on, fmt, name) in [
        (
            cfg!(feature = "clap"),
            vance_audio_plugins::PluginFormat::Clap,
            "CLAP",
        ),
        (
            cfg!(feature = "lv2"),
            vance_audio_plugins::PluginFormat::Lv2,
            "LV2",
        ),
    ] {
        let sidecar_on = sidecar_has(fmt);
        if feat_on != sidecar_on {
            tracing::warn!(
                format = name,
                app_supports = feat_on,
                sidecar_supports = sidecar_on,
                "audio-plugin support is split between the app and its discovery sidecar — \
                 rebuild BOTH together, e.g. `cargo build -p vance-app -p vance-plugin-host \
                 --features vance-app/{}`",
                name.to_lowercase()
            );
        }
    }

    let plugins = client.list_plugins().map_err(|e| e.to_string())?;
    // Be tidy — drop the sidecar so we don't leak it just for one query.
    let _ = client.shutdown();
    Ok(plugins
        .into_iter()
        .map(|d| UiPluginDescriptor {
            format: match d.format {
                vance_audio_plugins::PluginFormat::Builtin => "builtin".into(),
                vance_audio_plugins::PluginFormat::Lv2 => "lv2".into(),
                vance_audio_plugins::PluginFormat::Clap => "clap".into(),
            },
            id: d.id,
            name: d.name,
            vendor: d.vendor,
            runnable: d.runnable,
        })
        .collect())
}

/// One enumerated plugin parameter, for the FX panel's knobs.
#[derive(serde::Serialize, Clone, Debug)]
struct UiPluginParam {
    id: u32,
    name: String,
    min: f32,
    max: f32,
    default: f32,
}

/// Enumerate a plugin's parameters so the FX panel can render controls.
/// Spawns a one-shot sidecar, instantiates the plugin, lists its params,
/// then tears down — mirroring [`list_audio_plugins`]. Returns an empty
/// list (not an error) when the sidecar / plugin can't be loaded, so the
/// UI degrades gracefully.
#[tauri::command]
fn list_plugin_params(
    format: String,
    id: String,
    _state: tauri::State<'_, EngineHandle>,
) -> Result<Vec<UiPluginParam>, String> {
    let plugin_format = match format.as_str() {
        "builtin" => vance_audio_plugins::PluginFormat::Builtin,
        "lv2" => vance_audio_plugins::PluginFormat::Lv2,
        "clap" => vance_audio_plugins::PluginFormat::Clap,
        other => return Err(format!("unknown plugin format `{other}`")),
    };
    // Failure paths return Err (NOT an empty list) so the UI can tell
    // "couldn't load this plugin" apart from "plugin has no parameters".
    let bin =
        locate_plugin_host_binary().ok_or_else(|| "plugin sidecar not available".to_string())?;
    let mut client = vance_audio_plugins::PluginClient::spawn(&bin).map_err(|e| {
        tracing::warn!(?e, "plugin host spawn failed");
        format!("could not start plugin sidecar: {e}")
    })?;
    let descriptor = vance_audio_plugins::PluginDescriptor {
        format: plugin_format,
        id,
        name: String::new(),
        vendor: String::new(),
        runnable: true,
    };
    // 48k / 512 is just for the probe; params are sample-rate-independent.
    let inst = client.instantiate(descriptor, 48_000.0, 512).map_err(|e| {
        tracing::warn!(?e, "could not instantiate plugin to list params");
        format!("could not load plugin: {e}")
    })?;
    let params = client
        .list_params(inst)
        .map_err(|e| format!("could not list params: {e}"))?;
    let _ = client.destroy(inst);
    let _ = client.shutdown();
    Ok(params
        .into_iter()
        .map(|p| UiPluginParam {
            id: p.id,
            name: p.name,
            min: p.min,
            max: p.max,
            default: p.default,
        })
        .collect())
}

/// Locate the `vance-plugin-host` sidecar binary. Mirrors
/// [`locate_audio_binary`] for the audio decoder.
fn locate_plugin_host_binary() -> Option<std::path::PathBuf> {
    let name = if cfg!(windows) {
        "vance-plugin-host.exe"
    } else {
        "vance-plugin-host"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let p = parent.join(name);
            if p.exists() {
                return Some(p);
            }
        }
    }
    // Try `target/debug/<name>` from the workspace root for dev builds.
    let candidates = [
        std::path::PathBuf::from("target/debug").join(name),
        std::path::PathBuf::from("target/release").join(name),
    ];
    for c in candidates {
        if c.exists() {
            return Some(c);
        }
    }
    None
}

/// Phase 7.9 — fire a short audio scrub burst at the given timeline
/// PTS. The UI is responsible for sequencing burst start + burst
/// stop (via `stop_playback`) during a playhead drag.
///
/// `time_base_num` / `time_base_den` describe the engine sequence's
/// time base for `from_pts`. For typical sequences this is `(1,
/// sample_rate)` so `from_pts` is in device-sample frames since
/// playback origin. For convenience, the UI calls with `(time_base_num,
/// time_base_den) = (1, 48_000)` and a PTS computed from
/// `timelineSeconds * 48000`.
/// Begin a scrub session (warm up the decoder + output) on drag-start.
#[tauri::command]
fn scrub_begin(state: tauri::State<'_, EngineHandle>) -> Result<(), String> {
    state.lock().scrub_begin().map_err(|e| e.to_string())
}

/// Play a sample-accurate grain of mixed audio at `at_seconds`. Replaces
/// the previous grain so the audio tracks the playhead.
#[tauri::command]
fn scrub_grain(state: tauri::State<'_, EngineHandle>, at_seconds: f64) -> Result<(), String> {
    state
        .lock()
        .scrub_grain(at_seconds)
        .map_err(|e| e.to_string())
}

/// End the scrub session (tear down the output + warm decoder).
#[tauri::command]
fn scrub_end(state: tauri::State<'_, EngineHandle>) -> Result<(), String> {
    state.lock().scrub_end();
    Ok(())
}

/// Phase 7.5 M1: read the latest mixer metering snapshot. Cheap
/// ArcSwap load; safe to poll at 30Hz from the UI.
#[tauri::command]
fn meter_snapshot(state: tauri::State<'_, EngineHandle>) -> Option<UiMeterSnapshot> {
    let snap = state.lock().meter_snapshot()?;
    let tracks = snap
        .tracks
        .into_iter()
        .map(|(uuid, m)| (uuid.to_string(), m.into()))
        .collect();
    let loudness = UiLoudness {
        lufs_m: snap.loudness.lufs_m,
        lufs_s: snap.loudness.lufs_s,
        lufs_i: snap.loudness.lufs_i,
        true_peak_l_db: vance_audio::loudness::lin_to_dbfs(snap.master_true_peak[0]),
        true_peak_r_db: vance_audio::loudness::lin_to_dbfs(snap.master_true_peak[1]),
    };
    Some(UiMeterSnapshot {
        master: snap.master.into(),
        tracks,
        generation: snap.generation,
        loudness,
    })
}

/// Phase 7.4 — JSON-friendly keyframe shape. `interp` matches the
/// vance-timeline `KeyframeInterp` lowercase variants.
#[derive(serde::Deserialize)]
struct UiKeyframe {
    time_seconds: f64,
    value: f64,
    #[serde(default)]
    interp: Option<String>,
}

fn parse_keyframes(kfs: Vec<UiKeyframe>) -> Result<Vec<vance_timeline::Keyframe>, String> {
    kfs.into_iter()
        .map(|k| {
            let interp = match k.interp.as_deref() {
                None | Some("linear") | Some("Linear") => vance_timeline::KeyframeInterp::Linear,
                Some("bezier") | Some("Bezier") => vance_timeline::KeyframeInterp::Bezier,
                Some("hold") | Some("Hold") => vance_timeline::KeyframeInterp::Hold,
                Some(other) => {
                    return Err(format!(
                        "unknown keyframe interp '{other}' (expected linear|bezier|hold)"
                    ))
                }
            };
            Ok(vance_timeline::Keyframe {
                time_seconds: k.time_seconds,
                value: k.value,
                interp,
            })
        })
        .collect()
}

/// Phase 7.4: replace a track's gain automation lane. Empty list =
/// revert to the static `Track.gain_db`.
#[tauri::command]
fn set_track_gain_keyframes(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    keyframes: Vec<UiKeyframe>,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let parsed = parse_keyframes(keyframes)?;
    state
        .lock()
        .set_track_gain_keyframes(vance_timeline::TrackId::from_uuid(track_uuid), parsed)
        .map_err(|e| e.to_string())
}

/// Phase 7.4: replace a clip's gain automation lane (clip-relative time).
#[tauri::command]
fn set_clip_gain_keyframes(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    keyframes: Vec<UiKeyframe>,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    let parsed = parse_keyframes(keyframes)?;
    state
        .lock()
        .set_clip_gain_keyframes(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            parsed,
        )
        .map_err(|e| e.to_string())
}

/// Parse a UI fade-curve string into the timeline enum. `None` /
/// unknown values fall back to `Linear` so the UI is forward-compatible
/// with future shapes.
fn parse_fade_curve(s: Option<String>) -> vance_timeline::FadeCurve {
    match s.as_deref() {
        Some("equal_power") => vance_timeline::FadeCurve::EqualPower,
        Some("logarithmic") => vance_timeline::FadeCurve::Logarithmic,
        _ => vance_timeline::FadeCurve::Linear,
    }
}

/// Phase 7.4 M3: set the clip's fade-in. `duration_seconds == 0.0`
/// clears it. `curve` accepts `"linear" | "equal_power" | "logarithmic"`.
#[tauri::command]
fn set_clip_fade_in(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    duration_seconds: f64,
    curve: Option<String>,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    state
        .lock()
        .set_clip_fade_in(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            duration_seconds,
            parse_fade_curve(curve),
        )
        .map_err(|e| e.to_string())
}

/// Phase 7.4 M3: set the clip's fade-out. Mirror of [`set_clip_fade_in`].
#[tauri::command]
fn set_clip_fade_out(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    duration_seconds: f64,
    curve: Option<String>,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    state
        .lock()
        .set_clip_fade_out(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            duration_seconds,
            parse_fade_curve(curve),
        )
        .map_err(|e| e.to_string())
}

/// Phase 7.3 mixer command: set a clip's mixer gain in dB.
#[tauri::command]
fn set_clip_gain_db(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    gain_db: f32,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    state
        .lock()
        .set_clip_gain_db(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            gain_db,
        )
        .map_err(|e| e.to_string())
}

/// Phase 7.6: set a track's stereo pan. `pan ∈ [-1, +1]`.
#[tauri::command]
fn set_track_pan(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    pan: f32,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    state
        .lock()
        .set_track_pan(vance_timeline::TrackId::from_uuid(track_uuid), pan)
        .map_err(|e| e.to_string())
}

/// Phase 7.6: set a clip's stereo pan. `pan ∈ [-1, +1]`.
#[tauri::command]
fn set_clip_pan(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    pan: f32,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    state
        .lock()
        .set_clip_pan(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            pan,
        )
        .map_err(|e| e.to_string())
}

/// Phase 7.7: append a built-in audio effect to a track's chain. The
/// UI side passes a JSON `AudioEffectSpec` (tagged enum) — serde
/// hydrates the typed variant before we dispatch.
#[tauri::command]
fn add_audio_effect(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    effect: vance_timeline::AudioEffectSpec,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    state
        .lock()
        .add_audio_effect(vance_timeline::TrackId::from_uuid(track_uuid), effect)
        .map_err(|e| e.to_string())
}

/// Phase 7.7: remove the audio effect at `index` from a track's chain.
#[tauri::command]
fn remove_audio_effect(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    index: usize,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    state
        .lock()
        .remove_audio_effect(vance_timeline::TrackId::from_uuid(track_uuid), index)
        .map_err(|e| e.to_string())
}

/// Phase 7.7: reorder one effect within the chain.
#[tauri::command]
fn move_audio_effect(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    from_index: usize,
    to_index: usize,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    state
        .lock()
        .move_audio_effect(
            vance_timeline::TrackId::from_uuid(track_uuid),
            from_index,
            to_index,
        )
        .map_err(|e| e.to_string())
}

/// Phase 7.7: replace one effect's full spec — the call every knob
/// commit goes through.
#[tauri::command]
fn update_audio_effect(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    index: usize,
    effect: vance_timeline::AudioEffectSpec,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    state
        .lock()
        .update_audio_effect(
            vance_timeline::TrackId::from_uuid(track_uuid),
            index,
            effect,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_effect_enabled(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    effect_id: String,
    enabled: bool,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    let effect_uuid = effect_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid effect id: {e}"))?;
    state
        .lock()
        .set_effect_enabled(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            vance_timeline::EffectId::from_uuid(effect_uuid),
            enabled,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn update_effect_params(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    effect_id: String,
    params: Vec<(String, vance_timeline::EffectValue)>,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    let effect_uuid = effect_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid effect id: {e}"))?;
    state
        .lock()
        .update_effect_params(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            vance_timeline::EffectId::from_uuid(effect_uuid),
            params,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn preview_grade_params(
    state: tauri::State<'_, EngineHandle>,
    clip_id: String,
    effect_id: String,
    params: Vec<(String, vance_timeline::EffectValue)>,
) -> Result<(), String> {
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    let effect_uuid = effect_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid effect id: {e}"))?;
    state
        .lock()
        .preview_grade_params(
            vance_timeline::ClipId::from_uuid(clip_uuid),
            vance_timeline::EffectId::from_uuid(effect_uuid),
            params,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_media_input_color_space(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
    color_space: Option<String>,
) -> Result<(), String> {
    let uuid = media_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid media id: {e}"))?;
    state
        .lock()
        .set_media_input_color_space(
            vance_timeline::MediaItemId::from_uuid(uuid),
            color_space.as_deref(),
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_project_working_color_space(
    state: tauri::State<'_, EngineHandle>,
    color_space: String,
) -> Result<(), String> {
    state
        .lock()
        .set_project_working_color_space(&color_space)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_project_view_lut(
    state: tauri::State<'_, EngineHandle>,
    path: Option<String>,
) -> Result<(), String> {
    state
        .lock()
        .set_project_view_lut(path.as_deref())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_ocio_view_transform(
    state: tauri::State<'_, EngineHandle>,
    config: Option<String>,
    src: String,
    dst: String,
    size: u32,
) -> Result<(), String> {
    state
        .lock()
        .set_ocio_view_transform(config.as_deref(), &src, &dst, size)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn clear_ocio_view_transform(state: tauri::State<'_, EngineHandle>) -> Result<(), String> {
    state
        .lock()
        .clear_ocio_view_transform()
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn ocio_list_colorspaces(
    state: tauri::State<'_, EngineHandle>,
    config: Option<String>,
) -> Result<Vec<String>, String> {
    state
        .lock()
        .ocio_list_colorspaces(config.as_deref())
        .map_err(|e| e.to_string())
}

/// Solo a grade node's HSL-qualifier matte to the viewport, or clear it
/// (pass `effectId: null`). Transient viewer state — never persisted.
/// Phase 6.9.
#[tauri::command]
fn set_matte_node(
    state: tauri::State<'_, EngineHandle>,
    clip_id: String,
    effect_id: Option<String>,
) -> Result<(), String> {
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    let effect = match effect_id {
        Some(s) => Some(vance_timeline::EffectId::from_uuid(
            s.parse::<uuid::Uuid>()
                .map_err(|e| format!("invalid effect id: {e}"))?,
        )),
        None => None,
    };
    state
        .lock()
        .set_matte_node(vance_timeline::ClipId::from_uuid(clip_uuid), effect)
        .map_err(|e| e.to_string())
}

/// Track a power-window center across `n_frames` frames forward by ZNCC
/// on the Y plane, writing the result as a single batched keyframe
/// update on the effect's win_cx / win_cy. Phase 6.11.
/// Apply ONE typed grading command (Phase 6.13). Convenience wrapper
/// around `apply_grading_commands` with a single-element batch; produces
/// one undo step on success.
#[tauri::command]
fn apply_grading_command(
    state: tauri::State<'_, EngineHandle>,
    cmd: vance_grading_cmd::GradingCommand,
) -> Result<vance_grading_cmd::GradingCommandOutcome, String> {
    let mut outcomes = state
        .lock()
        .apply_grading_batch(&[cmd], None)
        .map_err(|e| e.to_string())?;
    Ok(outcomes
        .pop()
        .unwrap_or(vance_grading_cmd::GradingCommandOutcome::None))
}

/// Apply a BATCH of typed grading commands as a single AI assistant
/// turn. Sequence-affecting commands collapse into one `EditOp::Compound`
/// (one undo step labeled `label`). Project-level / tracker commands run
/// outside the compound and add their own entries.
#[tauri::command]
fn apply_grading_commands(
    state: tauri::State<'_, EngineHandle>,
    cmds: Vec<vance_grading_cmd::GradingCommand>,
    label: Option<String>,
) -> Result<Vec<vance_grading_cmd::GradingCommandOutcome>, String> {
    state
        .lock()
        .apply_grading_batch(&cmds, label)
        .map_err(|e| e.to_string())
}

/// JSON Schema for `GradingCommand`, for the AI runtime's tool-use spec.
/// Single source of truth — derived from the Rust enum via `schemars`.
#[tauri::command]
fn grading_command_schema() -> serde_json::Value {
    serde_json::to_value(vance_grading_cmd::grading_command_schema())
        .unwrap_or(serde_json::Value::Null)
}

#[tauri::command]
fn track_window(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    effect_id: String,
    start_source_seconds: f64,
    n_frames: u32,
    patch_half: u32,
    search_radius: u32,
) -> Result<TrackSummary, String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    let effect_uuid = effect_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid effect id: {e}"))?;
    state
        .lock()
        .track_window(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            vance_timeline::EffectId::from_uuid(effect_uuid),
            start_source_seconds,
            n_frames,
            patch_half,
            search_radius,
        )
        .map_err(|e| e.to_string())
}

/// GOAT-9 — smart reframe: track the subject then write a keyframed
/// transform that keeps it centered for the target aspect.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
fn smart_reframe(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    target_w: u32,
    target_h: u32,
    zoom: f32,
    start_source_seconds: f64,
    n_frames: u32,
    seed_cx: f32,
    seed_cy: f32,
    patch_half: u32,
    search_radius: u32,
) -> Result<TrackSummary, String> {
    let t = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let c = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    state
        .lock()
        .smart_reframe(
            vance_timeline::TrackId::from_uuid(t),
            vance_timeline::ClipId::from_uuid(c),
            target_w,
            target_h,
            zoom,
            start_source_seconds,
            n_frames,
            seed_cx,
            seed_cy,
            patch_half,
            search_radius,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_scopes_enabled(state: tauri::State<'_, EngineHandle>, on: bool) -> Result<(), String> {
    state.lock().set_scopes_enabled(on);
    Ok(())
}

/// Compute video-scope data and return it as a RAW little-endian byte
/// blob (see `ScopeData::to_bytes`) via `tauri::ipc::Response` — the JS
/// side receives an ArrayBuffer, avoiding JSON-stringification of the
/// hundreds-of-thousands of integer counts. An empty response means
/// "no update" (scopes disabled, playing, or no cached frame). Phase 6.12.
#[tauri::command]
fn compute_scopes(
    state: tauri::State<'_, EngineHandle>,
    width: u32,
    height: u32,
    pre_view_lut: bool,
) -> Result<tauri::ipc::Response, String> {
    let cfg = vance_scopes::ScopeConfig::default();
    let data = state
        .lock()
        .compute_scopes(cfg, width, height, pre_view_lut)
        .map_err(|e| e.to_string())?;
    let bytes = data.map(|d| d.to_bytes()).unwrap_or_default();
    Ok(tauri::ipc::Response::new(bytes))
}

#[tauri::command]
fn create_title_media(
    state: tauri::State<'_, EngineHandle>,
    display_name: String,
    duration_seconds: f64,
    text: String,
    size_px: f32,
    fg_rgb: [f32; 3],
    bg_rgb: [f32; 3],
    width: u32,
    height: u32,
) -> Result<MediaItemInfo, String> {
    let params = vance_timeline::TitleParams {
        text,
        size_px,
        fg: fg_rgb,
        bg: bg_rgb,
        width,
        height,
    };
    state
        .lock()
        .create_title_media(display_name, duration_seconds, params)
        .map_err(|e| e.to_string())
}

/// GOAT-5 — built-in MoGraph template catalog as `[key, name]` pairs.
#[tauri::command]
fn mograph_catalog(state: tauri::State<'_, EngineHandle>) -> Vec<(String, String)> {
    state.lock().mograph_catalog()
}

/// GOAT-5 — instantiate a built-in MoGraph template as a new media item.
#[tauri::command]
fn create_mograph_media(
    state: tauri::State<'_, EngineHandle>,
    template_key: String,
    display_name: Option<String>,
) -> Result<MediaItemInfo, String> {
    state
        .lock()
        .create_mograph_media(template_key, display_name)
        .map_err(|e| e.to_string())
}

/// GOAT-5 — the placed MoGraph instance (template + control values) for
/// the control-editor panel.
#[tauri::command]
fn mograph_instance(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
) -> Result<Option<vance_mograph::MographInstance>, String> {
    let uuid: uuid::Uuid = media_id.parse().map_err(|e| format!("bad media id: {e}"))?;
    let id = vance_timeline::MediaItemId::from_uuid(uuid);
    Ok(state.lock().mograph_instance(id))
}

/// GOAT-5 — set one control value on a MoGraph media item.
#[tauri::command]
fn set_mograph_value(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
    control_id: String,
    value: vance_mograph::ControlValue,
) -> Result<bool, String> {
    let uuid: uuid::Uuid = media_id.parse().map_err(|e| format!("bad media id: {e}"))?;
    let id = vance_timeline::MediaItemId::from_uuid(uuid);
    state
        .lock()
        .set_mograph_value(id, control_id, value)
        .map_err(|e| e.to_string())
}

/// GOAT-12 — create a new Fusion composition media item (starter graph).
#[tauri::command]
fn create_fusion_media(
    state: tauri::State<'_, EngineHandle>,
    display_name: Option<String>,
    width: u32,
    height: u32,
    duration_seconds: f64,
) -> Result<MediaItemInfo, String> {
    state
        .lock()
        .create_fusion_media(display_name, width, height, duration_seconds)
        .map_err(|e| e.to_string())
}

/// GOAT-12 — the Fusion composition (whole node graph) for the node editor.
#[tauri::command]
fn fusion_comp(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
) -> Result<Option<vance_fusion::Comp>, String> {
    let uuid: uuid::Uuid = media_id.parse().map_err(|e| format!("bad media id: {e}"))?;
    let id = vance_timeline::MediaItemId::from_uuid(uuid);
    Ok(state.lock().fusion_comp(id))
}

/// GOAT-12 — replace the Fusion composition on a media item (the editor
/// pushes the whole edited graph back).
#[tauri::command]
fn set_fusion_comp(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
    comp: vance_fusion::Comp,
) -> Result<(), String> {
    let uuid: uuid::Uuid = media_id.parse().map_err(|e| format!("bad media id: {e}"))?;
    let id = vance_timeline::MediaItemId::from_uuid(uuid);
    state
        .lock()
        .set_fusion_comp(id, comp)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn add_transition_at_cut(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    after_clip_id: String,
    kind: String,
    duration_seconds: f64,
) -> Result<String, String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = after_clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    // "dissolve" → built-in GPU dissolve; "wasm:<plugin-id>" → a WASM
    // transition plugin (same convention as wasm effects).
    let kind = if kind == "dissolve" {
        vance_timeline::TransitionKind::Dissolve
    } else if let Some(id) = kind.strip_prefix("wasm:") {
        vance_timeline::TransitionKind::Wasm(id.to_string())
    } else {
        return Err(format!("unknown transition kind: {kind}"));
    };
    state
        .lock()
        .add_transition_at_cut(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            kind,
            duration_seconds,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_transition(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    transition_id: String,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let transition_uuid = transition_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid transition id: {e}"))?;
    state
        .lock()
        .remove_transition(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::TransitionId::from_uuid(transition_uuid),
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_transition_param(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    transition_id: String,
    key: String,
    value: vance_timeline::EffectValue,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let transition_uuid = transition_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid transition id: {e}"))?;
    state
        .lock()
        .set_transition_param(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::TransitionId::from_uuid(transition_uuid),
            key,
            value,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_clip(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    state
        .lock()
        .remove_clip(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn move_clip_to(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    timeline_seconds: f64,
) -> Result<(), String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let clip_uuid = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid clip id: {e}"))?;
    state
        .lock()
        .move_clip_to(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::ClipId::from_uuid(clip_uuid),
            timeline_seconds,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn insert_clip_at(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    media_id: String,
    timeline_seconds: f64,
) -> Result<String, String> {
    let track_uuid = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid track id: {e}"))?;
    let media_uuid = media_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid media id: {e}"))?;
    state
        .lock()
        .insert_clip_at(
            vance_timeline::TrackId::from_uuid(track_uuid),
            vance_timeline::MediaItemId::from_uuid(media_uuid),
            timeline_seconds,
        )
        .map_err(|e| e.to_string())
}

// ---- GOAT-6: magnetic timeline + clip connections --------------------------

#[tauri::command]
fn is_magnetic(state: tauri::State<'_, EngineHandle>) -> bool {
    state.lock().is_magnetic()
}

#[tauri::command]
fn set_magnetic(state: tauri::State<'_, EngineHandle>, magnetic: bool) -> Result<(), String> {
    state
        .lock()
        .set_magnetic(magnetic)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn clip_connections(state: tauri::State<'_, EngineHandle>) -> Vec<(String, String)> {
    state.lock().connections()
}

#[tauri::command]
fn connect_clip(
    state: tauri::State<'_, EngineHandle>,
    anchor_id: String,
    connected_id: String,
) -> Result<(), String> {
    let a = anchor_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad anchor id: {e}"))?;
    let c = connected_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad connected id: {e}"))?;
    state
        .lock()
        .connect_clip(
            vance_timeline::ClipId::from_uuid(a),
            vance_timeline::ClipId::from_uuid(c),
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn disconnect_clip(
    state: tauri::State<'_, EngineHandle>,
    connected_id: String,
) -> Result<(), String> {
    let c = connected_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad clip id: {e}"))?;
    state
        .lock()
        .disconnect_clip(vance_timeline::ClipId::from_uuid(c))
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn magnetic_move_clip(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    timeline_seconds: f64,
) -> Result<(), String> {
    let t = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad track id: {e}"))?;
    let c = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad clip id: {e}"))?;
    state
        .lock()
        .magnetic_move_clip(
            vance_timeline::TrackId::from_uuid(t),
            vance_timeline::ClipId::from_uuid(c),
            timeline_seconds,
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn append_clip(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    media_id: String,
) -> Result<String, String> {
    let t = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad track id: {e}"))?;
    let m = media_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad media id: {e}"))?;
    state
        .lock()
        .append_clip(
            vance_timeline::TrackId::from_uuid(t),
            vance_timeline::MediaItemId::from_uuid(m),
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn magnetic_ripple_delete(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
) -> Result<(), String> {
    let t = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad track id: {e}"))?;
    let c = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad clip id: {e}"))?;
    state
        .lock()
        .magnetic_ripple_delete(
            vance_timeline::TrackId::from_uuid(t),
            vance_timeline::ClipId::from_uuid(c),
        )
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn magnetic_ripple_insert(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    media_id: String,
    timeline_seconds: f64,
) -> Result<String, String> {
    let t = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad track id: {e}"))?;
    let m = media_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad media id: {e}"))?;
    state
        .lock()
        .magnetic_ripple_insert(
            vance_timeline::TrackId::from_uuid(t),
            vance_timeline::MediaItemId::from_uuid(m),
            timeline_seconds,
        )
        .map_err(|e| e.to_string())
}

// ---- GOAT-7: captions / subtitles + transcription --------------------------

fn parse_cue_id(s: &str) -> Result<vance_captions::CueId, String> {
    s.parse::<uuid::Uuid>()
        .map_err(|e| format!("bad cue id: {e}"))
}

#[tauri::command]
fn captions(state: tauri::State<'_, EngineHandle>) -> Vec<vance_captions::Cue> {
    state.lock().captions()
}

// ---- GOAT-gated (collaboration): review comments ----------------------

fn parse_comment_id(s: &str) -> Result<vance_timeline::id::CommentId, String> {
    let uuid: uuid::Uuid = s.parse().map_err(|e| format!("bad comment id: {e}"))?;
    Ok(vance_timeline::id::CommentId::from_uuid(uuid))
}

/// All review comments on the current sequence (sorted by position).
#[tauri::command]
fn review_comments(state: tauri::State<'_, EngineHandle>) -> Vec<vance_timeline::ReviewComment> {
    state.lock().review_comments()
}

/// Add a review comment at `timeline_ms`. Returns its id.
#[tauri::command]
fn add_review_comment(
    state: tauri::State<'_, EngineHandle>,
    timeline_ms: i64,
    author: String,
    text: String,
) -> Result<String, String> {
    if text.trim().is_empty() {
        return Err("comment text is empty".into());
    }
    state
        .lock()
        .add_review_comment(timeline_ms, author, text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_review_comment(
    state: tauri::State<'_, EngineHandle>,
    comment_id: String,
) -> Result<(), String> {
    let id = parse_comment_id(&comment_id)?;
    state
        .lock()
        .remove_review_comment(id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn resolve_review_comment(
    state: tauri::State<'_, EngineHandle>,
    comment_id: String,
    resolved: bool,
) -> Result<(), String> {
    let id = parse_comment_id(&comment_id)?;
    state
        .lock()
        .resolve_review_comment(id, resolved)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_review_comment_text(
    state: tauri::State<'_, EngineHandle>,
    comment_id: String,
    text: String,
) -> Result<(), String> {
    let id = parse_comment_id(&comment_id)?;
    state
        .lock()
        .set_review_comment_text(id, text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn reply_review_comment(
    state: tauri::State<'_, EngineHandle>,
    comment_id: String,
    author: String,
    text: String,
) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("reply text is empty".into());
    }
    let id = parse_comment_id(&comment_id)?;
    state
        .lock()
        .reply_review_comment(id, author, text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn add_caption(
    state: tauri::State<'_, EngineHandle>,
    start_ms: i64,
    end_ms: i64,
    text: String,
) -> Result<String, String> {
    state
        .lock()
        .add_caption(start_ms, end_ms, text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn remove_caption(state: tauri::State<'_, EngineHandle>, cue_id: String) -> Result<(), String> {
    let id = parse_cue_id(&cue_id)?;
    state.lock().remove_caption(id).map_err(|e| e.to_string())
}

#[tauri::command]
fn set_caption_text(
    state: tauri::State<'_, EngineHandle>,
    cue_id: String,
    text: String,
) -> Result<(), String> {
    let id = parse_cue_id(&cue_id)?;
    state
        .lock()
        .set_caption_text(id, text)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_caption_timing(
    state: tauri::State<'_, EngineHandle>,
    cue_id: String,
    start_ms: i64,
    end_ms: i64,
) -> Result<(), String> {
    let id = parse_cue_id(&cue_id)?;
    state
        .lock()
        .set_caption_timing(id, start_ms, end_ms)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn split_caption(
    state: tauri::State<'_, EngineHandle>,
    cue_id: String,
    at_ms: i64,
) -> Result<(), String> {
    let id = parse_cue_id(&cue_id)?;
    state
        .lock()
        .split_caption(id, at_ms)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn merge_caption_with_next(
    state: tauri::State<'_, EngineHandle>,
    cue_id: String,
) -> Result<(), String> {
    let id = parse_cue_id(&cue_id)?;
    state
        .lock()
        .merge_caption_with_next(id)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn shift_all_captions(state: tauri::State<'_, EngineHandle>, delta_ms: i64) -> Result<(), String> {
    state
        .lock()
        .shift_all_captions(delta_ms)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn import_captions(state: tauri::State<'_, EngineHandle>, path: String) -> Result<usize, String> {
    state
        .lock()
        .import_captions(&path)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn export_captions(
    state: tauri::State<'_, EngineHandle>,
    path: String,
    vtt: bool,
) -> Result<(), String> {
    state
        .lock()
        .export_captions(&path, vtt)
        .map_err(|e| e.to_string())
}

/// Whether a local Whisper CLI is installed (enables the Transcribe button).
#[tauri::command]
fn transcription_available() -> bool {
    transcribe::available()
}

/// Auto-transcribe a media item into captions. Runs the Whisper subprocess
/// WITHOUT holding the engine lock (it can take minutes), then applies the
/// result. Returns the cue count.
#[tauri::command]
fn transcribe_media(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
    model: String,
    language: Option<String>,
) -> Result<usize, String> {
    let uuid: uuid::Uuid = media_id.parse().map_err(|e| format!("bad media id: {e}"))?;
    let mid = vance_timeline::MediaItemId::from_uuid(uuid);
    // Read the media path + capture the TARGET sequence under a brief lock,
    // then DROP it for the (slow) subprocess.
    let (path, target_seq) = {
        let eng = state.lock();
        let path = eng
            .media_path_for(mid)
            .ok_or_else(|| "media has no file to transcribe".to_string())?;
        (path, eng.current_sequence_id_string())
    };
    let cfg = transcribe::TranscribeConfig {
        model: if model.is_empty() {
            "base".into()
        } else {
            model
        },
        language: language.filter(|l| !l.is_empty()),
    };
    let cues = transcribe::transcribe_file(&path, &cfg).map_err(|e| e.to_string())?;
    let n = cues.len();
    // Re-lock and only apply if the SAME sequence is still current — else the
    // user switched sequences / loaded a project mid-run and the cues would
    // overwrite the wrong timeline.
    {
        let mut eng = state.lock();
        if eng.current_sequence_id_string() != target_seq {
            return Err(
                "the current sequence changed during transcription; captions were not applied"
                    .into(),
            );
        }
        eng.set_captions(cues).map_err(|e| e.to_string())?;
    }
    Ok(n)
}

// ---- GOAT-8: dialogue leveling + voice isolation ---------------------------

/// Measure a media item's integrated loudness (LUFS). Decodes off-lock.
#[tauri::command]
fn analyze_loudness(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
) -> Result<Option<f32>, String> {
    let uuid: uuid::Uuid = media_id.parse().map_err(|e| format!("bad media id: {e}"))?;
    let mid = vance_timeline::MediaItemId::from_uuid(uuid);
    let path = { state.lock().media_path_for(mid) };
    Ok(path.and_then(|p| engine::measure_loudness(&p)))
}

/// Auto dialogue leveling: measure a clip's loudness off-lock, then set its
/// gain to land at `target_lufs`. Returns the applied gain in dB.
#[tauri::command]
fn level_clip(
    state: tauri::State<'_, EngineHandle>,
    track_id: String,
    clip_id: String,
    target_lufs: f32,
) -> Result<f32, String> {
    let t = track_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad track id: {e}"))?;
    let c = clip_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("bad clip id: {e}"))?;
    let tid = vance_timeline::TrackId::from_uuid(t);
    let cid = vance_timeline::ClipId::from_uuid(c);
    // Resolve the clip's media path + active source range under a brief
    // lock, measure only that region off-lock (correct for trimmed clips).
    let (path, start_sec, dur_sec) = {
        let eng = state.lock();
        let (mid, start, dur) = eng
            .clip_media_and_range(tid, cid)
            .ok_or_else(|| "clip not found".to_string())?;
        let path = eng
            .media_path_for(mid)
            .ok_or_else(|| "clip media has no audio file".to_string())?;
        (path, start, dur)
    };
    let source_lufs = engine::measure_loudness_range(&path, start_sec, Some(dur_sec))
        .ok_or_else(|| "could not measure loudness".to_string())?;
    let gain_db = (target_lufs - source_lufs).clamp(-24.0, 24.0);
    // Re-validate the clip still exists before applying (it may have been
    // deleted during the off-lock measure).
    {
        let mut eng = state.lock();
        if eng.clip_media_and_range(tid, cid).is_none() {
            return Err("clip was removed during analysis".into());
        }
        eng.set_clip_gain_db(tid, cid, gain_db)
            .map_err(|e| e.to_string())?;
    }
    Ok(gain_db)
}

/// Whether a voice-isolation CLI (Demucs) is installed.
#[tauri::command]
fn voice_isolation_available() -> bool {
    separate::available()
}

/// Whether an AI-mask CLI (background remover) is installed.
#[tauri::command]
fn magic_mask_available() -> bool {
    magicmask::available()
}

/// GOAT-9 Magic Mask — isolate the subject of a media item (off-lock) and
/// import the matted result as new media. Returns the new item's info.
#[tauri::command]
fn magic_mask(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
) -> Result<MediaItemInfo, String> {
    let uuid: uuid::Uuid = media_id.parse().map_err(|e| format!("bad media id: {e}"))?;
    let mid = vance_timeline::MediaItemId::from_uuid(uuid);
    let path = {
        state
            .lock()
            .media_path_for(mid)
            .ok_or_else(|| "media has no file to mask".to_string())?
    };
    let out = magicmask::isolate_subject(&path).map_err(|e| e.to_string())?;
    // Copy the matted video out of the volatile temp dir into a stable,
    // app-owned location so the imported media survives save/reopen.
    let dir = engine::persistent_media_dir().map_err(|e| format!("media dir: {e}"))?;
    let ext = out.extension().and_then(|e| e.to_str()).unwrap_or("mov");
    let dest = dir.join(format!("{}-subject.{ext}", uuid::Uuid::new_v4()));
    std::fs::copy(&out, &dest).map_err(|e| format!("copy matte: {e}"))?;
    let dest_str = dest.to_string_lossy().into_owned();
    // Matted output has a video stream → the normal video import path works.
    state
        .lock()
        .import_media_file(&dest_str)
        .map_err(|e| e.to_string())
}

/// Isolate vocals from a media item (off-lock) and import the vocal stem as
/// a new media item. Returns the new item's info.
#[tauri::command]
fn isolate_voice(
    state: tauri::State<'_, EngineHandle>,
    media_id: String,
) -> Result<MediaItemInfo, String> {
    let uuid: uuid::Uuid = media_id.parse().map_err(|e| format!("bad media id: {e}"))?;
    let mid = vance_timeline::MediaItemId::from_uuid(uuid);
    let path = {
        state
            .lock()
            .media_path_for(mid)
            .ok_or_else(|| "media has no file to isolate".to_string())?
    };
    let stems = separate::isolate(&path).map_err(|e| e.to_string())?;
    // Copy the stem out of the volatile temp dir (which the next isolate run
    // wipes and the OS temp reaper clears) into a stable, uuid-named,
    // app-owned location, so the imported media survives save/reopen.
    let dir = engine::persistent_media_dir().map_err(|e| format!("media dir: {e}"))?;
    let ext = stems
        .vocals
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("wav");
    let dest = dir.join(format!("{}-vocals.{ext}", uuid::Uuid::new_v4()));
    std::fs::copy(&stems.vocals, &dest).map_err(|e| format!("copy stem: {e}"))?;
    let dest_str = dest.to_string_lossy().into_owned();
    // Audio-only stem → import via the audio-aware path (the video decoder
    // would reject it with NoVideoStream).
    state
        .lock()
        .import_audio_file(&dest_str)
        .map_err(|e| e.to_string())
}

/// GOAT-gated (generative) — whether an image generator is configured
/// (VANCE_IMAGEGEN_CMD, or `sd` + VANCE_SD_MODEL).
#[tauri::command]
fn image_gen_available() -> bool {
    generate::available()
}

/// A live image-generation progress event on `gen://progress`.
#[derive(Clone, serde::Serialize)]
struct GenProgressEvent {
    /// Current diffusion step (0 for non-step status lines).
    step: u32,
    /// Total steps (0 when unknown, e.g. while loading the model).
    total: u32,
    /// 0..100 when steps are known.
    pct: Option<f32>,
    /// Latest status line from the runner.
    detail: String,
    /// True on the terminal event.
    done: bool,
    /// True only on a successful terminal event.
    ok: bool,
    /// Error text on a failed terminal event.
    error: Option<String>,
}

/// GOAT-gated (generative) — synthesize an image from a prompt via the
/// user's configured model (off-lock), writing it to an app-owned path, and
/// import it as a held Image media item. Streams per-step progress on
/// `gen://progress`. Returns the new item's info.
#[tauri::command]
async fn generate_image(
    app: tauri::AppHandle,
    state: tauri::State<'_, EngineHandle>,
    prompt: String,
    negative: Option<String>,
    width: u32,
    height: u32,
    steps: Option<u32>,
    seed: Option<i64>,
    duration_seconds: Option<f64>,
) -> Result<MediaItemInfo, String> {
    if prompt.trim().is_empty() {
        return Err("enter a prompt".to_string());
    }
    // Clamp to even, sane bounds (models want multiples of 8/64; keep it simple).
    let width = width.clamp(64, 4096) & !1;
    let height = height.clamp(64, 4096) & !1;
    let dir = engine::persistent_media_dir().map_err(|e| format!("media dir: {e}"))?;
    let out_path = dir.join(format!("{}-gen.png", uuid::Uuid::new_v4()));
    let req = generate::GenRequest {
        prompt,
        negative: negative.filter(|s| !s.is_empty()),
        width,
        height,
        steps,
        seed,
        out_path,
    };

    // Model inference is slow — run it on a blocking task and stream progress.
    let emit_app = app.clone();
    let produced = tauri::async_runtime::spawn_blocking(move || {
        let mut on = |step: u32, total: u32, detail: &str| {
            let pct = (total > 0).then(|| (step as f32 / total as f32 * 100.0).min(100.0));
            let _ = emit_app.emit(
                "gen://progress",
                GenProgressEvent {
                    step,
                    total,
                    pct,
                    detail: detail.into(),
                    done: false,
                    ok: false,
                    error: None,
                },
            );
        };
        generate::generate_with_progress(&req, &mut on)
    })
    .await
    .map_err(|e| format!("generate task failed: {e}"))?;

    // Terminal event so the panel can close its progress UI either way.
    let terminal = |ok: bool, error: Option<String>| GenProgressEvent {
        step: 0,
        total: 0,
        pct: ok.then_some(100.0),
        detail: if ok { "Done".into() } else { "Failed".into() },
        done: true,
        ok,
        error,
    };
    match &produced {
        Ok(_) => {
            let _ = app.emit("gen://progress", terminal(true, None));
        }
        Err(e) => {
            let _ = app.emit("gen://progress", terminal(false, Some(e.to_string())));
        }
    }

    let produced = produced.map_err(|e| e.to_string())?;
    let path_str = produced.to_string_lossy().into_owned();
    state
        .lock()
        .import_generated_image(&path_str, duration_seconds.unwrap_or(5.0))
        .map_err(|e| e.to_string())
}

// ---- Phase 11: capabilities, settings, onboarding -------------------

/// Detect available tools (FFmpeg, bundled sidecars, optional AI runners) for
/// the onboarding wizard + diagnostics.
#[tauri::command]
fn detect_capabilities() -> Vec<capabilities::Capability> {
    capabilities::detect()
}

/// The persisted app settings (first-run flag + privacy opt-ins).
#[tauri::command]
fn get_app_settings() -> settings::AppSettings {
    settings::load()
}

/// Persist app settings. Returns the stored value.
#[tauri::command]
fn set_app_settings(new: settings::AppSettings) -> Result<settings::AppSettings, String> {
    settings::save(&new).map_err(|e| format!("save settings: {e}"))?;
    Ok(new)
}

/// Path to the local crash-log directory (for the Help ▸ Diagnostics link).
#[tauri::command]
fn crash_log_dir() -> Result<String, String> {
    crash::crash_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .map_err(|e| format!("crash dir: {e}"))
}

// ---- Optional AI tools: status + on-demand install ------------------------

/// Status of every optional AI helper (captions / voice isolation / subject
/// mask / generative images): installed? managed by Vance? model ready?
#[tauri::command]
fn ai_tool_status() -> Vec<tools::ToolStatus> {
    tools::status()
}

/// Whether a system Python 3 is present to install the pip-based AI tools.
/// Drives the Settings/onboarding UI (offer pip install vs. "install Python").
#[tauri::command]
fn ai_python_available() -> bool {
    tools::python_available()
}

/// A live install-progress event, emitted on `ai-tool://progress` while an
/// install runs so the UI can show it instead of appearing frozen.
#[derive(Clone, serde::Serialize)]
struct AiToolProgress {
    /// Tool id this event is about.
    id: String,
    /// Coarse phase: `setup` | `pip` | `download` | `done` | `error`.
    phase: String,
    /// Human-readable line (latest subprocess output / status).
    detail: String,
    /// 0..100 when known (downloads); `null` for indeterminate steps.
    pct: Option<f32>,
    /// True on the terminal event.
    done: bool,
    /// True only on a successful terminal event.
    ok: bool,
    /// Error text on a failed terminal event.
    error: Option<String>,
}

/// Install an optional AI tool by id (`whisper` / `demucs` /
/// `backgroundremover` / `sd`) into Vance's managed tools dir — pip into the
/// managed venv for the Python tools, or a manifest download for native ones.
/// Runs on a blocking task and streams progress on `ai-tool://progress`;
/// resolves to the installed path.
#[tauri::command]
async fn install_ai_tool(app: tauri::AppHandle, id: String) -> Result<String, String> {
    let tool = tools::Tool::from_id(&id).ok_or_else(|| format!("unknown AI tool: {id}"))?;
    let emit_id = id.clone();
    let emit_app = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let mut on = |phase: &str, detail: &str, pct: Option<f32>| {
            let _ = emit_app.emit(
                "ai-tool://progress",
                AiToolProgress {
                    id: emit_id.clone(),
                    phase: phase.into(),
                    detail: detail.into(),
                    pct,
                    done: false,
                    ok: false,
                    error: None,
                },
            );
        };
        tools::install_with_progress(tool, &mut on)
    })
    .await
    .map_err(|e| format!("install task failed: {e}"))?;

    // Terminal event so the UI can close its progress dialog either way.
    match &result {
        Ok(path) => {
            let _ = app.emit(
                "ai-tool://progress",
                AiToolProgress {
                    id: id.clone(),
                    phase: "done".into(),
                    detail: "Installed".into(),
                    pct: Some(100.0),
                    done: true,
                    ok: true,
                    error: None,
                },
            );
            Ok(path.to_string_lossy().into_owned())
        }
        Err(e) => {
            let msg = e.to_string();
            let _ = app.emit(
                "ai-tool://progress",
                AiToolProgress {
                    id: id.clone(),
                    phase: "error".into(),
                    detail: "Install failed".into(),
                    pct: None,
                    done: true,
                    ok: false,
                    error: Some(msg.clone()),
                },
            );
            Err(msg)
        }
    }
}

// ---- GOAT-10: control surfaces (OSC / MIDI) --------------------------------

/// `(osc_running, osc_port, midi_running, midi_name)`.
#[derive(serde::Serialize)]
struct ControlStatus {
    osc_running: bool,
    osc_port: Option<u16>,
    midi_running: bool,
    midi_name: Option<String>,
}

#[tauri::command]
fn control_status(hub: tauri::State<'_, ControlHandle>) -> ControlStatus {
    let (osc_running, osc_port, midi_running, midi_name) = hub.lock().status();
    ControlStatus {
        osc_running,
        osc_port,
        midi_running,
        midi_name,
    }
}

#[tauri::command]
fn start_osc_control(hub: tauri::State<'_, ControlHandle>, port: u16) -> Result<(), String> {
    hub.lock().start_osc(port).map_err(|e| e.to_string())
}

#[tauri::command]
fn stop_osc_control(hub: tauri::State<'_, ControlHandle>) {
    hub.lock().stop_osc();
}

#[tauri::command]
fn midi_ports() -> Vec<String> {
    control::ControlHub::midi_ports()
}

#[tauri::command]
fn start_midi_control(
    hub: tauri::State<'_, ControlHandle>,
    port_index: usize,
) -> Result<(), String> {
    hub.lock().start_midi(port_index)
}

#[tauri::command]
fn stop_midi_control(hub: tauri::State<'_, ControlHandle>) {
    hub.lock().stop_midi();
}

/// Drain queued control-surface actions (the UI polls this and dispatches
/// each through the normal, thread-correct command path).
#[tauri::command]
fn poll_control_actions(hub: tauri::State<'_, ControlHandle>) -> Vec<vance_control::ControlAction> {
    hub.lock().drain()
}

// ---- GOAT-11: direct publish (YouTube / Vimeo / TikTok) --------------------

fn parse_platform(s: &str) -> Result<vance_publish::Platform, String> {
    match s {
        "YouTube" => Ok(vance_publish::Platform::YouTube),
        "Vimeo" => Ok(vance_publish::Platform::Vimeo),
        "TikTok" => Ok(vance_publish::Platform::TikTok),
        _ => Err(format!("unknown platform '{s}'")),
    }
}

fn parse_privacy(s: &str) -> vance_publish::Privacy {
    match s {
        "public" => vance_publish::Privacy::Public,
        "private" => vance_publish::Privacy::Private,
        _ => vance_publish::Privacy::Unlisted,
    }
}

/// Whether a platform is currently authorized (has an access token).
#[tauri::command]
fn publish_authorized(hub: tauri::State<'_, PublishHandle>, platform: String) -> bool {
    match parse_platform(&platform) {
        Ok(p) => hub.lock().is_authorized(p),
        Err(_) => false,
    }
}

/// Begin OAuth for a platform: spawns the loopback redirect listener, opens
/// the consent page, and returns the authorize URL (also shown in-UI).
#[tauri::command]
fn start_oauth(
    hub: tauri::State<'_, PublishHandle>,
    platform: String,
    client_id: String,
    client_secret: Option<String>,
    redirect_uri: String,
) -> Result<String, String> {
    let p = parse_platform(&platform)?;
    hub.lock()
        .start_oauth(p, client_id, client_secret, redirect_uri)
        .map_err(|e| e.to_string())
}

/// Wait for the browser callback + exchange the code for a token. Blocks up
/// to 3 min (off the hub lock during the HTTP round-trip). Returns true when
/// authorized.
#[tauri::command]
fn await_oauth(hub: tauri::State<'_, PublishHandle>) -> Result<bool, String> {
    // Snapshot the redirect slot + exchange context under a brief lock.
    let (slot, state, ctx) = {
        let h = hub.lock();
        let (slot, state) = h
            .pending_slot()
            .ok_or_else(|| "no OAuth flow in progress".to_string())?;
        let ctx = h
            .exchange_ctx()
            .ok_or_else(|| "no OAuth flow in progress".to_string())?;
        (slot, state, ctx)
    };
    let (platform, cfg, verifier) = ctx;
    // Poll + exchange with the lock DROPPED.
    match publish::await_and_exchange(&slot, &state, platform, &cfg, &verifier) {
        Ok(token) => {
            hub.lock().finish(token);
            Ok(true)
        }
        Err(e) => {
            // On timeout/failure, tear the flow down: stop + join the redirect
            // listener (freeing the port) and clear `pending` so the next
            // Connect starts clean instead of hanging on a stale flow.
            hub.lock().cancel();
            Err(e.to_string())
        }
    }
}

/// Publish a rendered file to a platform. Reads the token under a brief
/// lock, uploads with the lock dropped. Returns the resulting URL.
#[tauri::command]
fn publish_video(
    hub: tauri::State<'_, PublishHandle>,
    platform: String,
    file_path: String,
    title: String,
    description: String,
    tags: Vec<String>,
    privacy: String,
) -> Result<String, String> {
    let p = parse_platform(&platform)?;
    let token = hub
        .lock()
        .token(p)
        .ok_or_else(|| "not authorized — connect the account first".to_string())?;
    let meta = vance_publish::UploadMeta {
        title,
        description,
        tags,
        privacy: parse_privacy(&privacy),
    };
    match p {
        vance_publish::Platform::YouTube => {
            publish::upload_youtube(&token, &file_path, &meta).map_err(|e| e.to_string())
        }
        // OAuth is wired for all three; the platform-specific upload for
        // Vimeo/TikTok (their chunked/tus APIs) is the next step.
        other => Err(format!(
            "{}: OAuth is connected, but Vance's uploader for it isn't wired yet — YouTube is fully supported.",
            other.label()
        )),
    }
}

#[tauri::command]
fn project_info(state: tauri::State<'_, EngineHandle>) -> Option<ProjectInfo> {
    state.lock().project_info()
}

#[tauri::command]
fn set_current_sequence(
    state: tauri::State<'_, EngineHandle>,
    sequence_id: String,
) -> Result<(), String> {
    let uuid = sequence_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid sequence id: {e}"))?;
    state
        .lock()
        .set_current_sequence(SequenceId::from_uuid(uuid))
        .map_err(|e| e.to_string())
}

/// The application menu bar (macOS: the system menu bar; Windows/Linux: a window
/// menu). Custom items emit `menu://action` with their id; the frontend maps
/// each id to the same handler as the matching toolbar button. Predefined items
/// (undo-native/cut/copy/paste/quit/…) act natively.
fn build_app_menu(app: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};

    let item = |id: &str, label: &str, accel: Option<&str>| {
        let mut b = MenuItemBuilder::with_id(id, label);
        if let Some(a) = accel {
            b = b.accelerator(a);
        }
        b.build(app)
    };

    // App menu (shows as "Vance" on macOS).
    let app_menu = SubmenuBuilder::new(app, "Vance")
        .about(Some(tauri::menu::AboutMetadata {
            name: Some("Vance".into()),
            comments: Some("A fast, cross-platform video editor.".into()),
            ..Default::default()
        }))
        .separator()
        .item(&item("settings", "Settings…", Some("CmdOrCtrl+,"))?)
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;

    let file = SubmenuBuilder::new(app, "File")
        .item(&item("new", "New Project", Some("CmdOrCtrl+N"))?)
        .item(&item("open_project", "Open Project…", Some("CmdOrCtrl+Shift+O"))?)
        .separator()
        .item(&item("open_file", "Import Media…", Some("CmdOrCtrl+I"))?)
        .item(&item("new_title", "New Title…", Some("CmdOrCtrl+T"))?)
        .separator()
        .item(&item("save", "Save", Some("CmdOrCtrl+S"))?)
        .item(&item("export", "Export…", Some("CmdOrCtrl+E"))?)
        .separator()
        .close_window()
        .build()?;

    // App-level Undo/Redo (the edit history) + native clipboard items, plus the
    // common timeline edits. The plain-key shortcuts (Delete, T) are handled in
    // the frontend; they're shown in the labels but NOT registered as menu
    // accelerators so they don't hijack the keys while typing in a field.
    let edit = SubmenuBuilder::new(app, "Edit")
        .item(&item("undo", "Undo", Some("CmdOrCtrl+Z"))?)
        .item(&item("redo", "Redo", Some("CmdOrCtrl+Shift+Z"))?)
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .separator()
        .item(&item("delete_clip", "Delete Selected  (Delete)", None)?)
        .item(&item("add_transition", "Add Cross-Dissolve  (T)", None)?)
        .build()?;

    // Three-point editing (Source → Timeline). Shortcuts I/O/X/,/. live in the
    // frontend; shown in labels, not registered here.
    let clip = SubmenuBuilder::new(app, "Clip")
        .item(&item("mark_in", "Mark In  (I)", None)?)
        .item(&item("mark_out", "Mark Out  (O)", None)?)
        .item(&item("clear_marks", "Clear In/Out  (X)", None)?)
        .separator()
        .item(&item("insert", "Insert to Timeline  (,)", None)?)
        .item(&item("overwrite", "Overwrite to Timeline  (.)", None)?)
        .build()?;

    // Space / arrows are handled by the frontend and MUST NOT be menu
    // accelerators (they'd hijack typing/navigation), so these are click-only.
    let playback = SubmenuBuilder::new(app, "Playback")
        .item(&item("play_pause", "Play / Pause  (Space)", None)?)
        .separator()
        .item(&item("prev_frame", "Previous Frame  (←)", None)?)
        .item(&item("next_frame", "Next Frame  (→)", None)?)
        .separator()
        .item(&item("back_1s", "Back 1 Second  (J)", None)?)
        .item(&item("forward_1s", "Forward 1 Second  (L)", None)?)
        .build()?;

    let view = SubmenuBuilder::new(app, "View")
        .item(&item("page_edit", "Edit", Some("CmdOrCtrl+1"))?)
        .item(&item("page_compositor", "Compositor", Some("CmdOrCtrl+2"))?)
        .item(&item("page_color", "Color", Some("CmdOrCtrl+3"))?)
        .item(&item("page_audio", "Audio", Some("CmdOrCtrl+4"))?)
        .item(&item("page_deliver", "Deliver", Some("CmdOrCtrl+5"))?)
        .separator()
        .item(&item("dock_ai_tools", "AI Add-ons…", None)?)
        .build()?;

    let window = SubmenuBuilder::new(app, "Window")
        .minimize()
        .maximize()
        .separator()
        .close_window()
        .build()?;

    let help = SubmenuBuilder::new(app, "Help")
        .item(&item("shortcuts", "Keyboard Shortcuts", None)?)
        .separator()
        .item(&item("diagnostics", "Show Crash Logs", None)?)
        .build()?;

    MenuBuilder::new(app)
        .item(&app_menu)
        .item(&file)
        .item(&edit)
        .item(&clip)
        .item(&playback)
        .item(&view)
        .item(&window)
        .item(&help)
        .build()
}

fn main() {
    // Phase 11 — capture panics to a local crash log before anything else,
    // so even a startup panic leaves a report the user can find.
    crash::install_panic_hook();

    // Phase 9.7 — when VANCE_TRACE_PATH is set we emit chrome trace
    // JSON to that path on top of the stderr fmt subscriber. Without
    // the env var, only the stderr subscriber runs (same as before).
    let chrome_on = profiling::init();
    if chrome_on {
        eprintln!(
            "[vance] chrome tracing enabled → {}",
            std::env::var("VANCE_TRACE_PATH").unwrap_or_default()
        );
    }

    // Headless AI-tool subcommands (used by OS installers' postinstall step and
    // power users): `--install-ai-tools [all|ids]`, `--list-ai-tools`,
    // `--ai-tools-status`. Handled BEFORE any window/engine is built, so it
    // runs and exits without opening the GUI. A plain launch falls through.
    // `args_os` + lossy so a non-UTF-8 argv element (e.g. an Open-With path with
    // odd bytes) can't panic startup; handle_cli only matches ASCII flags.
    let argv: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if let Some(code) = tools::handle_cli(&argv) {
        std::process::exit(code);
    }

    let engine = EngineHandle::new();
    let peaks_handle: PeaksHandle = Arc::new(PMutex::new(None));
    let control_hub: ControlHandle = Arc::new(PMutex::new(control::ControlHub::default()));
    let publish_hub: PublishHandle = Arc::new(PMutex::new(publish::PublishHub::default()));

    // M2 — share one entitlement gate between the accounts layer (which keeps it
    // current) and the engine (which consults it at plugin load, spec §6).
    let accounts_state = accounts::new_state();
    engine.lock().set_entitlement_gate(accounts_state.gate());

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .menu(|handle| build_app_menu(handle))
        .on_menu_event(|app, event| {
            // Custom items carry a known id; forward it to the frontend, which
            // maps it to the same action as the corresponding toolbar button.
            // Predefined items (copy/paste/quit/…) act natively and are ignored.
            let _ = app.emit("menu://action", event.id().0.as_str());
        })
        .manage(engine.clone())
        .manage(peaks_handle.clone())
        .manage(control_hub.clone())
        .manage(publish_hub.clone())
        .manage(accounts_state)
        .setup(move |app| {
            let window = app
                .get_webview_window("main")
                .ok_or("main window not found")?;

            #[cfg(target_os = "macos")]
            {
                let ns_window_ptr = window.ns_window().map_err(|e| e.to_string())?;
                let host = HostWindowHandle::from_ns_window(ns_window_ptr);
                engine
                    .lock()
                    .install_viewport(host)
                    .map_err(|e| e.to_string())?;
            }

            #[cfg(not(target_os = "macos"))]
            {
                let _ = (window, &engine);
                tracing::warn!(
                    "viewport install skipped: only macOS implemented at this checkpoint"
                );
            }

            // Spawn the PeaksService with a temp cache dir. M6 swaps
            // the cache_dir to <bundle>/cache/peaks via the
            // `peaks_set_cache_dir` command once a project is loaded.
            match locate_audio_binary() {
                Ok(binary) => {
                    let sink = build_progress_sink(app.handle().clone());
                    match PeaksService::start(binary, default_peaks_cache_dir(), sink) {
                        Ok(svc) => {
                            *peaks_handle.lock() = Some(Arc::new(svc));
                            tracing::info!(
                                target: "vance_app.peaks",
                                "PeaksService started with default temp cache_dir"
                            );
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "vance_app.peaks",
                                err = ?e,
                                "PeaksService failed to start; waveforms unavailable"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "vance_app.peaks",
                        err = ?e,
                        "audio sidecar missing; PeaksService not started"
                    );
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            backend_version,
            set_viewport_bounds,
            set_source_viewport_bounds,
            load_source,
            decode_source_at,
            source_state,
            set_source_mark_in,
            set_source_mark_out,
            clear_source_marks,
            insert_from_source,
            open_file,
            decode_and_render_at,
            start_playback,
            stop_playback,
            is_playing,
            last_resume_pts,
            load_project,
            project_info,
            set_current_sequence,
            new_project,
            save_project,
            current_project_path,
            current_sequence_contents,
            diag_log,
            import_media_file,
            media_list,
            insert_clip_at,
            is_magnetic,
            set_magnetic,
            clip_connections,
            connect_clip,
            disconnect_clip,
            magnetic_move_clip,
            append_clip,
            magnetic_ripple_delete,
            magnetic_ripple_insert,
            captions,
            add_caption,
            remove_caption,
            review_comments,
            add_review_comment,
            remove_review_comment,
            resolve_review_comment,
            set_review_comment_text,
            reply_review_comment,
            set_caption_text,
            set_caption_timing,
            split_caption,
            merge_caption_with_next,
            shift_all_captions,
            import_captions,
            export_captions,
            transcription_available,
            transcribe_media,
            analyze_loudness,
            level_clip,
            voice_isolation_available,
            isolate_voice,
            magic_mask_available,
            magic_mask,
            image_gen_available,
            generate_image,
            detect_capabilities,
            get_app_settings,
            set_app_settings,
            crash_log_dir,
            ai_tool_status,
            ai_python_available,
            install_ai_tool,
            control_status,
            start_osc_control,
            stop_osc_control,
            midi_ports,
            start_midi_control,
            stop_midi_control,
            poll_control_actions,
            publish_authorized,
            start_oauth,
            await_oauth,
            publish_video,
            move_clip_to,
            remove_clip,
            add_effect_to_clip,
            remove_effect_from_clip,
            update_effect_param,
            validate_expression,
            move_effect_on_clip,
            set_effect_enabled,
            set_track_mute,
            set_track_solo,
            set_track_gain_db,
            set_clip_mute,
            set_clip_gain_db,
            set_track_pan,
            set_clip_pan,
            add_audio_effect,
            remove_audio_effect,
            move_audio_effect,
            update_audio_effect,
            set_master_gain_db,
            buses,
            add_bus,
            remove_bus,
            set_bus_gain_db,
            set_bus_pan,
            set_bus_mute,
            set_bus_output,
            route_track_to_bus,
            add_track_send,
            remove_track_send,
            add_bus_insert,
            remove_bus_insert,
            move_bus_insert,
            update_bus_insert,
            add_bus_send,
            remove_bus_send,
            meter_snapshot,
            scrub_begin,
            scrub_grain,
            scrub_end,
            list_audio_plugins,
            list_plugin_params,
            render_sequence,
            render_and_replace,
            enqueue_proxy_generation,
            proxy_jobs,
            frame_cache_stats,
            set_use_proxies,
            set_media_proxy_path,
            rescan_plugins,
            list_wasm_plugins,
            add_wasm_plugin,
            remove_wasm_plugin,
            plugins_folder,
            list_ofx_plugins,
            set_track_gain_keyframes,
            set_clip_gain_keyframes,
            set_clip_fade_in,
            set_clip_fade_out,
            update_effect_params,
            preview_grade_params,
            add_transition_at_cut,
            remove_transition,
            set_transition_param,
            create_title_media,
            mograph_catalog,
            create_mograph_media,
            mograph_instance,
            set_mograph_value,
            create_fusion_media,
            fusion_comp,
            set_fusion_comp,
            set_media_input_color_space,
            set_project_working_color_space,
            set_project_view_lut,
            set_ocio_view_transform,
            clear_ocio_view_transform,
            ocio_list_colorspaces,
            set_scopes_enabled,
            compute_scopes,
            set_matte_node,
            track_window,
            smart_reframe,
            apply_grading_command,
            apply_grading_commands,
            grading_command_schema,
            trim_clip_in,
            trim_clip_out,
            play_sequence,
            decode_sequence_at,
            current_timeline_seconds,
            undo,
            redo,
            can_undo,
            can_redo,
            edit_history,
            checkpoint_history,
            goto_history,
            set_media_metadata,
            query_media,
            custom_columns,
            add_smart_bin,
            remove_smart_bin,
            smart_bins,
            smart_bin_members,
            peaks_enqueue,
            peaks_status,
            peaks_prioritize,
            peaks_cancel,
            peaks_for_clip,
            peaks_meta,
            peaks_set_cache_dir,
            peaks_use_untitled_cache_dir,
            peaks_service_status,
            peaks_backfill_video_audio,
            accounts::account_status,
            accounts::account_sign_in,
            accounts::account_sign_out,
            accounts::account_list_devices,
            accounts::account_stepup,
            accounts::account_revoke_device,
            accounts::account_set_password,
            accounts::account_base_url,
            accounts::account_set_base_url,
            accounts::entitlements_list,
            accounts::entitlement_install,
            accounts::entitlement_uninstall,
            accounts::entitlements_reconcile,
            accounts::catalog_browse,
            accounts::catalog_detail,
            accounts::catalog_reviews,
            accounts::catalog_install,
            accounts::catalog_review,
            accounts::payment_checkout,
            accounts::payment_connect,
            accounts::payment_connect_status,
            accounts::catalog_report,
            accounts::seller_status,
            accounts::seller_effects,
        ])
        .build(tauri::generate_context!())
        .expect("tauri failed to start")
        // The build()+run() form gives us a RunEvent stream so the
        // PeaksService worker + its audio sidecar can be drained
        // deterministically at app exit. Otherwise managed-state Drop
        // races process teardown and child processes can be orphaned.
        .run(|app_handle, event| {
            if matches!(
                event,
                tauri::RunEvent::ExitRequested { .. } | tauri::RunEvent::Exit
            ) {
                if let Some(handle) = app_handle.try_state::<PeaksHandle>() {
                    // Pull the Arc out under a scoped lock, then drop
                    // it AFTER releasing both the lock and the State
                    // borrow. PeaksService::Drop can wait up to 5
                    // seconds on the worker join.
                    let taken = {
                        let mut slot = handle.lock();
                        slot.take()
                    };
                    if let Some(svc) = taken {
                        drop(svc);
                    }
                }
            }
        });
}
