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

use engine::{EngineHandle, MediaItemInfo, ProjectInfo, SequenceContents};
use serde::Serialize;
use tauri::Manager;
use vance_decoder::StreamInfo;
use vance_timeline::SequenceId;
use vance_viewport::{HostWindowHandle, ViewportBounds};

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

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(engine.clone())
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

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            backend_version,
            set_viewport_bounds,
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
            play_sequence,
            decode_sequence_at,
            current_timeline_seconds,
        ])
        .run(tauri::generate_context!())
        .expect("tauri failed to start");
}
