//! Vance — Tauri 2 host process.
//!
//! Phase 1 / Milestone 1.5 scope: open a window, install the parented
//! viewport, spawn the decoder sidecar on demand, decode a frame at a
//! requested PTS and render it through the wgpu pipeline into the viewport.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

mod engine;
mod peaks;

use engine::{EngineHandle, MediaItemInfo, ProjectInfo, SequenceContents, SourceState, TrackSummary};
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
    let self_exe =
        std::env::current_exe().map_err(|e| format!("current_exe(): {e}"))?;
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
fn peaks_prioritize(
    state: tauri::State<'_, PeaksHandle>,
    media_id: String,
) -> Result<(), String> {
    let uuid = media_id
        .parse::<uuid::Uuid>()
        .map_err(|e| format!("invalid media id: {e}"))?;
    let id = vance_timeline::MediaItemId::from_uuid(uuid);
    let svc = checkout_peaks(&state)?;
    svc.prioritize(id);
    Ok(())
}

#[tauri::command]
fn peaks_cancel(
    state: tauri::State<'_, PeaksHandle>,
    media_id: String,
) -> Result<(), String> {
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
fn peaks_service_status(
    state: tauri::State<'_, PeaksHandle>,
) -> PeaksServiceAvailability {
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
                return Ok(tauri::ipc::Response::new(
                    PEAKS_NOT_READY_SENTINEL.to_vec(),
                ));
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
fn decode_source_at(
    state: tauri::State<'_, EngineHandle>,
    target_pts: i64,
) -> Result<i64, String> {
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
    state.lock().set_source_mark_out().map_err(|e| e.to_string())
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
        .insert_from_source(
            vance_timeline::TrackId::from_uuid(track_uuid),
            overwrite,
        )
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
fn new_project(
    state: tauri::State<'_, EngineHandle>,
    name: String,
) -> Result<ProjectInfo, String> {
    state.lock().new_project(name).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_project(
    state: tauri::State<'_, EngineHandle>,
    path: Option<String>,
) -> Result<(), String> {
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
    path: String,
) -> Result<MediaItemInfo, String> {
    state
        .lock()
        .import_media_file(&path)
        .map_err(|e| e.to_string())
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
            s.parse::<uuid::Uuid>().map_err(|e| format!("invalid effect id: {e}"))?,
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
    Ok(outcomes.pop().unwrap_or(vance_grading_cmd::GradingCommandOutcome::None))
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
    let kind = match kind.as_str() {
        "dissolve" => vance_timeline::TransitionKind::Dissolve,
        other => return Err(format!("unknown transition kind: {other}")),
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

fn main() {
    // Quiet by default; bump with RUST_LOG=info for diagnostics.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,vance=info".parse().unwrap()),
        )
        .try_init();

    let engine = EngineHandle::new();
    let peaks_handle: PeaksHandle = Arc::new(PMutex::new(None));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(engine.clone())
        .manage(peaks_handle.clone())
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
            move_clip_to,
            remove_clip,
            add_effect_to_clip,
            remove_effect_from_clip,
            update_effect_param,
            move_effect_on_clip,
            set_effect_enabled,
            update_effect_params,
            preview_grade_params,
            add_transition_at_cut,
            remove_transition,
            create_title_media,
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
            peaks_enqueue,
            peaks_status,
            peaks_prioritize,
            peaks_cancel,
            peaks_for_clip,
            peaks_meta,
            peaks_set_cache_dir,
            peaks_use_untitled_cache_dir,
            peaks_service_status,
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
