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
        ])
        .run(tauri::generate_context!())
        .expect("tauri failed to start");
}
