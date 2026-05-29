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
use vance_render::{render_title_yuv420, Renderer, RendererConfig};
use std::collections::HashMap;
use vance_timeline::{
    Clip, Effect, EditLog, EditOp, EffectId, EffectValue, MediaItem, MediaItemId, MediaKind,
    Project, Sequence, SequenceId, TitleParams, Track, TrackId, TrackKind,
};

/// Cache of parsed LUT files keyed by absolute path. Loading a 32³
/// cube parses ~100KB of text; doing it per frame would burn CPU.
type LutCache = HashMap<PathBuf, Arc<vance_color::Lut3d>>;

/// Resolution of a clip's media for an engine playback / scrub call.
/// File-backed media goes through the decoder; title media is rendered
/// procedurally each frame via `vance_render::render_title_yuv420`.
enum MediaResolved {
    File(String),
    Title(TitleParams),
}
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
    /// Working color space (Phase 6.2) — stringified for the IPC layer.
    pub working_color_space: String,
    /// Path of the active display LUT (Phase 6.5), or `null` when
    /// monitoring is unfiltered.
    pub view_lut_path: Option<String>,
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
    /// Transitions on this track (cross-dissolves so far).
    pub transitions: Vec<TransitionInfo>,
}

/// One transition's worth of info for the UI.
#[derive(serde::Serialize, Clone, Debug)]
pub struct TransitionInfo {
    /// Transition's UUID.
    pub id: String,
    /// Kind ("dissolve").
    pub kind: String,
    /// Cut position in seconds (the center of the transition window).
    pub position_seconds: f64,
    /// Duration of the transition window in seconds.
    pub duration_seconds: f64,
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
    /// Effects attached to this clip, in display order.
    pub effects: Vec<EffectInfo>,
}

/// One effect's worth of info for the UI.
#[derive(serde::Serialize, Clone, Debug)]
pub struct EffectInfo {
    /// Effect's UUID.
    pub id: String,
    /// Effect kind ("opacity", "transform", ...).
    pub kind: String,
    /// Whether the effect contributes to the render.
    pub enabled: bool,
    /// Effect parameters as a JS-friendly map. Each value is a
    /// number/boolean/string (EffectValue is `#[serde(untagged)]`).
    pub params: std::collections::BTreeMap<String, EffectValue>,
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

/// Product of every enabled "opacity" effect's `value` param,
/// evaluated at clip-relative `t_seconds`. Keyframed params
/// interpolate; plain numbers are constant. Defaults to `1.0` when
/// no opacity effects are attached.
fn eval_clip_opacity(effects: &[vance_timeline::Effect], t_seconds: f64) -> f32 {
    let mut op = 1.0_f64;
    for e in effects {
        if !e.enabled || e.kind != "opacity" {
            continue;
        }
        if let Some(v) = e.params.get("value").and_then(|p| p.eval_number(t_seconds)) {
            op *= v.clamp(0.0, 1.0);
        }
    }
    op.clamp(0.0, 1.0) as f32
}

/// Convenience: find the clip in the sequence and evaluate its
/// opacity at clip-relative `t_seconds`.
fn clip_opacity_at(seq: &Sequence, clip_id: vance_timeline::ClipId, t_seconds: f64) -> f32 {
    for track in seq.tracks.iter() {
        if let Some(clip) = track.clips.iter().find(|c| c.id == clip_id) {
            return eval_clip_opacity(&clip.effects, t_seconds);
        }
    }
    1.0
}

/// 2D transform aggregated from every enabled "transform" effect on
/// the clip. Multiple transforms compose by adding translate + rotation
/// and multiplying scale — order-independent for the parameter shapes
/// we expose. Defaults to the identity when no transform effects exist.
struct ClipTransform {
    translate: [f32; 2],
    scale: [f32; 2],
    rotation_radians: f32,
}

fn eval_clip_transform(
    effects: &[vance_timeline::Effect],
    t_seconds: f64,
) -> ClipTransform {
    let mut t = ClipTransform {
        translate: [0.0, 0.0],
        scale: [1.0, 1.0],
        rotation_radians: 0.0,
    };
    let eval = |k: &str,
                ps: &std::collections::BTreeMap<String, EffectValue>|
     -> Option<f32> {
        ps.get(k).and_then(|p| p.eval_number(t_seconds)).map(|v| v as f32)
    };
    for e in effects {
        if !e.enabled || e.kind != "transform" {
            continue;
        }
        t.translate[0] += eval("translate_x", &e.params).unwrap_or(0.0);
        t.translate[1] += eval("translate_y", &e.params).unwrap_or(0.0);
        t.scale[0] *= eval("scale_x", &e.params).unwrap_or(1.0);
        t.scale[1] *= eval("scale_y", &e.params).unwrap_or(1.0);
        t.rotation_radians +=
            eval("rotation", &e.params).unwrap_or(0.0).to_radians();
    }
    t
}

fn clip_transform_at(
    seq: &Sequence,
    clip_id: vance_timeline::ClipId,
    t_seconds: f64,
) -> ClipTransform {
    for track in seq.tracks.iter() {
        if let Some(clip) = track.clips.iter().find(|c| c.id == clip_id) {
            return eval_clip_transform(&clip.effects, t_seconds);
        }
    }
    ClipTransform {
        translate: [0.0, 0.0],
        scale: [1.0, 1.0],
        rotation_radians: 0.0,
    }
}

/// `[left, right, top, bottom]` crop fractions aggregated from every
/// enabled "crop" effect on the clip; values stack additively (so two
/// crop effects can compose). Defaults to all-zero (no crop) when
/// none are attached.
fn eval_clip_crop(effects: &[vance_timeline::Effect], t_seconds: f64) -> [f32; 4] {
    let mut crop = [0.0_f32; 4];
    let eval = |k: &str,
                ps: &std::collections::BTreeMap<String, EffectValue>|
     -> f32 {
        ps.get(k)
            .and_then(|p| p.eval_number(t_seconds))
            .map(|v| v as f32)
            .unwrap_or(0.0)
    };
    for e in effects {
        if !e.enabled || e.kind != "crop" {
            continue;
        }
        crop[0] += eval("left", &e.params);
        crop[1] += eval("right", &e.params);
        crop[2] += eval("top", &e.params);
        crop[3] += eval("bottom", &e.params);
    }
    crop
}

/// Brightness/contrast values aggregated from every enabled
/// "color" effect on the clip; brightness sums, contrast multiplies.
/// Defaults to `(brightness=0, contrast=1)` (passthrough).
fn eval_clip_brightness_contrast(
    effects: &[vance_timeline::Effect],
    t_seconds: f64,
) -> (f32, f32) {
    let mut brightness = 0.0_f32;
    let mut contrast = 1.0_f32;
    let eval = |k: &str,
                ps: &std::collections::BTreeMap<String, EffectValue>|
     -> Option<f32> {
        ps.get(k).and_then(|p| p.eval_number(t_seconds)).map(|v| v as f32)
    };
    for e in effects {
        if !e.enabled || e.kind != "color" {
            continue;
        }
        brightness += eval("brightness", &e.params).unwrap_or(0.0);
        contrast *= eval("contrast", &e.params).unwrap_or(1.0);
    }
    (brightness, contrast)
}

fn clip_crop_at(
    seq: &Sequence,
    clip_id: vance_timeline::ClipId,
    t_seconds: f64,
) -> [f32; 4] {
    for track in seq.tracks.iter() {
        if let Some(clip) = track.clips.iter().find(|c| c.id == clip_id) {
            return eval_clip_crop(&clip.effects, t_seconds);
        }
    }
    [0.0; 4]
}

fn clip_brightness_contrast_at(
    seq: &Sequence,
    clip_id: vance_timeline::ClipId,
    t_seconds: f64,
) -> (f32, f32) {
    for track in seq.tracks.iter() {
        if let Some(clip) = track.clips.iter().find(|c| c.id == clip_id) {
            return eval_clip_brightness_contrast(&clip.effects, t_seconds);
        }
    }
    (0.0, 1.0)
}

/// Evaluate the serial color-grade chain (Phase 6.6/6.7): every
/// `kind == "grade"` effect, in `Vec` order, each read at clip-relative
/// `t_seconds` so keyframed wheel values animate. Disabled nodes stay
/// in the list (occupying their slot, bypassed in the shader) so the
/// chain order is preserved. Capped at `MAX_GRADE_NODES`.
fn eval_clip_grades(
    effects: &[vance_timeline::Effect],
    t_seconds: f64,
) -> Vec<vance_render::GradeNodeParams> {
    use vance_timeline::grade as gk;
    let num = |ps: &std::collections::BTreeMap<String, EffectValue>, k: &str, default: f32| -> f32 {
        ps.get(k)
            .and_then(|p| p.eval_number(t_seconds))
            .map(|v| v as f32)
            .unwrap_or(default)
    };
    let mut out = Vec::new();
    for e in effects {
        if e.kind != gk::GRADE_KIND {
            continue;
        }
        if out.len() >= gk::MAX_GRADE_NODES {
            break;
        }
        let log_mode = matches!(
            e.params.get(gk::KEY_MODE),
            Some(EffectValue::String(s)) if s == gk::MODE_LOG
        );
        out.push(vance_render::GradeNodeParams {
            enabled: e.enabled,
            log_mode,
            lift: [
                num(&e.params, gk::KEY_LIFT_R, 0.0),
                num(&e.params, gk::KEY_LIFT_G, 0.0),
                num(&e.params, gk::KEY_LIFT_B, 0.0),
                num(&e.params, gk::KEY_LIFT_Y, 0.0),
            ],
            gamma: [
                num(&e.params, gk::KEY_GAMMA_R, 1.0),
                num(&e.params, gk::KEY_GAMMA_G, 1.0),
                num(&e.params, gk::KEY_GAMMA_B, 1.0),
                num(&e.params, gk::KEY_GAMMA_Y, 1.0),
            ],
            gain: [
                num(&e.params, gk::KEY_GAIN_R, 1.0),
                num(&e.params, gk::KEY_GAIN_G, 1.0),
                num(&e.params, gk::KEY_GAIN_B, 1.0),
                num(&e.params, gk::KEY_GAIN_Y, 1.0),
            ],
            offset: [
                num(&e.params, gk::KEY_OFFSET_R, 0.0),
                num(&e.params, gk::KEY_OFFSET_G, 0.0),
                num(&e.params, gk::KEY_OFFSET_B, 0.0),
            ],
            shadow: [
                num(&e.params, gk::KEY_SHADOW_R, 0.0),
                num(&e.params, gk::KEY_SHADOW_G, 0.0),
                num(&e.params, gk::KEY_SHADOW_B, 0.0),
            ],
            midtone: [
                num(&e.params, gk::KEY_MID_R, 0.0),
                num(&e.params, gk::KEY_MID_G, 0.0),
                num(&e.params, gk::KEY_MID_B, 0.0),
            ],
            highlight: [
                num(&e.params, gk::KEY_HI_R, 0.0),
                num(&e.params, gk::KEY_HI_G, 0.0),
                num(&e.params, gk::KEY_HI_B, 0.0),
            ],
        });
    }
    out
}

fn clip_grades_at(
    seq: &Sequence,
    clip_id: vance_timeline::ClipId,
    t_seconds: f64,
) -> Vec<vance_render::GradeNodeParams> {
    for track in seq.tracks.iter() {
        if let Some(clip) = track.clips.iter().find(|c| c.id == clip_id) {
            return eval_clip_grades(&clip.effects, t_seconds);
        }
    }
    Vec::new()
}

/// First enabled "chroma_key" effect on the clip, evaluated at
/// `t_seconds`. Returns `(key_rgb, tolerance, softness, enable)`;
/// `enable = 0.0` means no chroma key effect is on (passthrough).
/// Multiple chroma_key effects are *not* composed — the first one
/// wins (NLE convention: stack the inputs, not the keys).
fn eval_clip_chroma_key(
    effects: &[vance_timeline::Effect],
    t_seconds: f64,
) -> ([f32; 3], f32, f32, f32) {
    let eval = |k: &str,
                ps: &std::collections::BTreeMap<String, EffectValue>,
                fallback: f32|
     -> f32 {
        ps.get(k)
            .and_then(|p| p.eval_number(t_seconds))
            .map(|v| v as f32)
            .unwrap_or(fallback)
    };
    for e in effects {
        if !e.enabled || e.kind != "chroma_key" {
            continue;
        }
        let r = eval("r", &e.params, 0.0).clamp(0.0, 1.0);
        let g = eval("g", &e.params, 1.0).clamp(0.0, 1.0);
        let b = eval("b", &e.params, 0.0).clamp(0.0, 1.0);
        let tolerance = eval("tolerance", &e.params, 0.15).max(0.0);
        let softness = eval("softness", &e.params, 0.05).max(0.0);
        return ([r, g, b], tolerance, softness, 1.0);
    }
    ([0.0, 1.0, 0.0], 0.15, 0.05, 0.0)
}

/// First enabled "luma_key" effect. Returns
/// `(threshold, direction, softness, enable)`. `direction > 0` keeps
/// pixels brighter than `threshold`; `< 0` keeps darker.
fn eval_clip_luma_key(
    effects: &[vance_timeline::Effect],
    t_seconds: f64,
) -> (f32, f32, f32, f32) {
    let eval = |k: &str,
                ps: &std::collections::BTreeMap<String, EffectValue>,
                fallback: f32|
     -> f32 {
        ps.get(k)
            .and_then(|p| p.eval_number(t_seconds))
            .map(|v| v as f32)
            .unwrap_or(fallback)
    };
    for e in effects {
        if !e.enabled || e.kind != "luma_key" {
            continue;
        }
        let threshold = eval("threshold", &e.params, 0.5).clamp(0.0, 1.0);
        // Stored as +1 / -1 (or 0 for "above" as a default). Any
        // positive value keeps bright pixels.
        let direction = eval("direction", &e.params, 1.0);
        let dir = if direction >= 0.0 { 1.0 } else { -1.0 };
        let softness = eval("softness", &e.params, 0.05).clamp(0.0, 1.0);
        return (threshold, dir, softness, 1.0);
    }
    (0.5, 1.0, 0.05, 0.0)
}

fn clip_chroma_key_at(
    seq: &Sequence,
    clip_id: vance_timeline::ClipId,
    t_seconds: f64,
) -> ([f32; 3], f32, f32, f32) {
    for track in seq.tracks.iter() {
        if let Some(clip) = track.clips.iter().find(|c| c.id == clip_id) {
            return eval_clip_chroma_key(&clip.effects, t_seconds);
        }
    }
    ([0.0, 1.0, 0.0], 0.15, 0.05, 0.0)
}

fn clip_luma_key_at(
    seq: &Sequence,
    clip_id: vance_timeline::ClipId,
    t_seconds: f64,
) -> (f32, f32, f32, f32) {
    for track in seq.tracks.iter() {
        if let Some(clip) = track.clips.iter().find(|c| c.id == clip_id) {
            return eval_clip_luma_key(&clip.effects, t_seconds);
        }
    }
    (0.5, 1.0, 0.05, 0.0)
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

/// State of the Source monitor for the UI — what's loaded + where
/// the playhead is. Mirrors the chunk of StreamInfo the chrome
/// actually needs without exposing the full type via JSON.
#[derive(serde::Serialize, Clone, Debug)]
pub struct SourceState {
    pub width: u32,
    pub height: u32,
    pub codec_name: String,
    pub time_base_num: i64,
    pub time_base_den: i64,
    pub duration_pts: Option<i64>,
    /// Last presented PTS in the source stream's time base, or
    /// `None` if nothing has been decoded since the last load.
    pub last_pts: Option<i64>,
    /// Source mark-in PTS, or `None` when unset.
    pub mark_in_pts: Option<i64>,
    /// Source mark-out PTS, or `None` when unset.
    pub mark_out_pts: Option<i64>,
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
    /// "video", "audio", "image", or "title".
    pub kind: String,
    /// Total duration in seconds.
    pub duration_seconds: f64,
    /// Source frame rate as `(num, den)`, or null when unknown.
    pub frame_rate_num: Option<i64>,
    pub frame_rate_den: Option<i64>,
    /// Stringified input color space (Phase 6.2). `None` when the
    /// user hasn't picked one (engine uses Rec.709 Limited fallback).
    pub input_color_space: Option<String>,
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
                MediaKind::Title => "title".into(),
            },
            duration_seconds: rt_to_seconds(m.duration),
            frame_rate_num: fr_n,
            frame_rate_den: fr_d,
            input_color_space: m.input_color_space.map(color_space_to_str),
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
            working_color_space: color_space_to_str(p.working_color_space),
            view_lut_path: p
                .view_lut_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned()),
        }
    }
}

/// Stringify a `ColorSpace` for the IPC layer. The TypeScript side
/// uses these labels in dropdowns and persists user picks back the
/// same way.
fn color_space_to_str(s: vance_color::ColorSpace) -> String {
    use vance_color::ColorSpace::*;
    match s {
        Rec709Limited => "rec709-limited".into(),
        Rec709Full => "rec709-full".into(),
        Srgb => "srgb".into(),
        LinearRec709 => "linear-rec709".into(),
        LinearRec2020 => "linear-rec2020".into(),
        Rec2020Limited => "rec2020-limited".into(),
        Rec2020Pq => "rec2020-pq".into(),
        Rec2020Hlg => "rec2020-hlg".into(),
        DisplayP3 => "display-p3".into(),
        Acescg => "acescg".into(),
        Acescct => "acescct".into(),
    }
}

/// Parse a stringified `ColorSpace`. Symmetric with `color_space_to_str`.
fn color_space_from_str(s: &str) -> Option<vance_color::ColorSpace> {
    use vance_color::ColorSpace::*;
    Some(match s {
        "rec709-limited" => Rec709Limited,
        "rec709-full" => Rec709Full,
        "srgb" => Srgb,
        "linear-rec709" => LinearRec709,
        "linear-rec2020" => LinearRec2020,
        "rec2020-limited" => Rec2020Limited,
        "rec2020-pq" => Rec2020Pq,
        "rec2020-hlg" => Rec2020Hlg,
        "display-p3" => DisplayP3,
        "acescg" => Acescg,
        "acescct" => Acescct,
        _ => return None,
    })
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
    /// Configuration of the Record monitor's surface.
    pub surface_config: Option<wgpu::SurfaceConfiguration>,
    /// Configuration of the Source monitor's surface — independent
    /// because the two monitors can have different sizes / DPRs at
    /// runtime (split chrome pane, different display configurations).
    pub source_surface_config: Option<wgpu::SurfaceConfiguration>,
}

#[derive(Default)]
pub struct Engine {
    /// Set once after the Tauri window exists. This is the *Record*
    /// monitor — the timeline preview. Phase 4.8 adds a parallel
    /// `source_viewport` for the Source monitor.
    viewport: Option<Arc<Viewport>>,
    /// Source monitor's viewport — installed alongside the Record
    /// viewport in `install_viewport`. Plays a single media item
    /// independently of the timeline.
    source_viewport: Option<Arc<Viewport>>,
    /// Last-applied bounds for the Source viewport.
    source_last_bounds: ViewportBounds,
    /// Parsed LUTs cached by source path so repeated scrubs through
    /// the same LUT effect don't re-read the file each frame.
    lut_cache: LutCache,
    /// Independent decoder sidecar for the Source monitor. Spawned
    /// lazily on the first `load_source`. Kept separate from the
    /// Record decoder so previewing media doesn't have to swap the
    /// timeline decoder's open file (and vice versa).
    source_decoder: Option<DecoderClient>,
    /// Secondary decoder slot for the *Record* side, used by
    /// transition-aware `decode_sequence_at` to pull frames from the
    /// "B" media while the primary decoder holds the "A" media open.
    /// Lazy: spawned the first time a transition window is entered;
    /// its open file may swap between calls. Cleared when the
    /// primary decoder swaps (the secondary's previous "A" became
    /// the new primary's "A", so the slot is stale).
    transition_decoder: Option<DecoderClient>,
    /// Path currently opened in `transition_decoder`. Lets us skip
    /// reopening when scrubbing through the same B side repeatedly.
    transition_media_path: Option<String>,
    /// Stream info for whatever is currently opened in
    /// `transition_decoder`. Used to convert source-seconds → PTS for
    /// the secondary decoder's `decode_at` calls.
    transition_stream_info: Option<StreamInfo>,
    /// Source's currently-opened media's stream info.
    source_stream_info: Option<StreamInfo>,
    /// Last-presented PTS on the Source viewport (after a
    /// `decode_source_at`). Cleared on `load_source`.
    source_last_pts: Option<i64>,
    /// MediaItemId backing the Source preview — set by `load_source`
    /// so `insert_from_source` knows which media to reference when
    /// it constructs the new Clip.
    source_media_id: Option<MediaItemId>,
    /// In-mark on the Source media (PTS in source stream time base).
    /// `None` means "start of media" when building a clip range.
    source_mark_in_pts: Option<i64>,
    /// Out-mark on the Source media. `None` means "end of media".
    source_mark_out_pts: Option<i64>,
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

    /// Per-sequence undo/redo history. Populated when a sequence
    /// becomes the current one (via new_project, load_project, or
    /// set_current_sequence). Each successful edit (insert/move/trim)
    /// applies through this log so it can be undone.
    edit_logs: HashMap<SequenceId, EditLog>,

    /// True while sequence playback should auto-chain to the next
    /// clip on EOS. Set by `play_sequence`; cleared by explicit
    /// `stop_playback`. The `is_playing` auto-reap checks this to
    /// decide whether to advance or end at EOS.
    playing_sequence_active: bool,
    /// Active clip's effects snapshot, set by `play_sequence` /
    /// `decode_sequence_at` before they kick off a playback session.
    /// Consumed by `start_playback` when it spawns the loop so
    /// keyframed params animate per frame.
    next_play_effects: Vec<vance_timeline::Effect>,
    /// Clip-relative source-in (seconds) corresponding to
    /// `next_play_effects`; used to map each frame's source PTS to
    /// the clip-local time for keyframe evaluation.
    next_play_source_in_sec: f64,
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
    /// Product of every "opacity" effect's `value` param on the
    /// active clip (clamped to [0, 1]). 1.0 when no opacity effects
    /// are attached. Phase 5.2's wiring.
    pub opacity: f32,
}

/// Owns the AudioPlayer on a dedicated thread. The thread parks until
/// `stop` is set, then drops the AudioPlayer (which stops the cpal
/// stream and joins its puller).
pub struct AudioOwner {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// Tells `start_playback_inner` whether the engine wants to spin up
/// a real decoder-backed playback session or a synthetic title one.
enum StartPlaybackKind {
    Video {
        from_pts: i64,
    },
    Title {
        params: TitleParams,
        fps: f64,
        duration_seconds: f64,
    },
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
    /// Set by the playback loop when it exits because the playhead
    /// just crossed *out* of a transition window (so the primary
    /// decoder, which holds A's media, should be replaced by one on
    /// B's media — see `is_playing` for the chain). The atomic
    /// stores microseconds; `i64::MIN` means "not requested".
    chain_resume_micros: Arc<std::sync::atomic::AtomicI64>,
    /// Joins to recover the decoders the loop owned during the
    /// session. See `PlaybackLoopOutput` for the contents.
    handle: JoinHandle<PlaybackLoopOutput>,
}

impl Engine {
    /// Install both viewports (Record + Source) + create a renderer
    /// that shares the wgpu instance. Called once from the Tauri
    /// `setup` callback on the main thread.
    pub fn install_viewport(&mut self, host: HostWindowHandle) -> Result<(), EngineError> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::PRIMARY;
        let instance = wgpu::Instance::new(desc);

        let viewport = Viewport::install(host, &instance)?;
        let source_viewport = Viewport::install(host, &instance)?;

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
        self.source_viewport = Some(Arc::new(source_viewport));
        self.render_state = Some(Arc::new(Mutex::new(RenderState {
            renderer,
            surface_config: None,
            source_surface_config: None,
        })));
        Ok(())
    }

    /// Update the Source viewport's bounds. Same shape as
    /// `set_viewport_bounds` but targets the Source monitor.
    pub fn set_source_viewport_bounds(
        &mut self,
        bounds: ViewportBounds,
    ) -> Result<(), EngineError> {
        let viewport = self
            .source_viewport
            .as_ref()
            .ok_or(EngineError::NoViewport)?;
        viewport.set_bounds(bounds);
        self.source_last_bounds = bounds;
        if !bounds.is_empty() {
            if let Some(rs) = self.render_state.as_ref() {
                let mut rs = rs.lock();
                if let Some(cfg) =
                    rs.renderer
                        .recommended_surface_config(viewport.surface(), bounds.w, bounds.h)
                {
                    rs.renderer.configure_surface(viewport.surface(), &cfg);
                    rs.source_surface_config = Some(cfg);
                }
            }
        }
        Ok(())
    }

    /// Open a media item by ID in the Source decoder and render its
    /// first frame to the Source viewport. The MediaItem must already
    /// be registered in the active project's library.
    pub fn load_source(&mut self, media_id: MediaItemId) -> Result<StreamInfo, EngineError> {
        let project = self
            .current_project
            .as_ref()
            .ok_or(EngineError::NoProject)?;
        let media = project
            .media
            .get(&media_id)
            .ok_or(EngineError::MediaNotFound)?
            .clone();
        let path = media.path.to_string_lossy().into_owned();

        // Lazy-spawn the Source decoder.
        if self.source_decoder.is_none() {
            let bin = locate_decoder_binary()?;
            tracing::info!(bin = %bin.display(), "spawning Source decoder sidecar");
            let client = DecoderClient::spawn(&bin)
                .map_err(|e| EngineError::DecoderSpawn(format!("{e}")))?;
            self.source_decoder = Some(client);
        }
        let dec = self.source_decoder.as_mut().unwrap();
        let info = dec.open(path)?;
        self.source_stream_info = Some(info.clone());
        self.source_last_pts = None;
        self.source_media_id = Some(media_id);
        // New media = clear stale marks from the previous load.
        self.source_mark_in_pts = None;
        self.source_mark_out_pts = None;

        // Decode + render the first frame so the user sees a preview
        // immediately. Errors here are non-fatal — they'll surface on
        // the next decode_source_at call.
        if let Err(e) = self.decode_source_at(0) {
            tracing::warn!(?e, "load_source: first-frame render failed");
        }
        Ok(info)
    }

    /// Park the Source mark-in at the current playhead PTS.
    pub fn set_source_mark_in(&mut self) -> Result<(), EngineError> {
        let pts = self.source_last_pts.ok_or(EngineError::NoFileOpen)?;
        // If a mark-out exists and we'd cross it, push it out.
        if let Some(out) = self.source_mark_out_pts {
            if pts >= out {
                self.source_mark_out_pts = None;
            }
        }
        self.source_mark_in_pts = Some(pts);
        Ok(())
    }

    /// Park the Source mark-out at the current playhead PTS.
    pub fn set_source_mark_out(&mut self) -> Result<(), EngineError> {
        let pts = self.source_last_pts.ok_or(EngineError::NoFileOpen)?;
        if let Some(inp) = self.source_mark_in_pts {
            if pts <= inp {
                self.source_mark_in_pts = None;
            }
        }
        self.source_mark_out_pts = Some(pts);
        Ok(())
    }

    /// Clear both Source marks.
    pub fn clear_source_marks(&mut self) {
        self.source_mark_in_pts = None;
        self.source_mark_out_pts = None;
    }

    /// Insert a clip referencing the Source media's [mark_in, mark_out]
    /// range (or the full duration when marks are unset) at the
    /// current Record playhead position. If `overwrite` is true, the
    /// clip replaces anything it overlaps; otherwise it ripples later
    /// clips on the same track right by its duration.
    pub fn insert_from_source(
        &mut self,
        track_id: TrackId,
        overwrite: bool,
    ) -> Result<String, EngineError> {
        self.pause_for_edit();
        let media_id = self.source_media_id.ok_or(EngineError::NoFileOpen)?;
        let info = self
            .source_stream_info
            .as_ref()
            .ok_or(EngineError::NoFileOpen)?;
        let (tb_n, tb_d) = info.time_base;
        if tb_d == 0 {
            return Err(EngineError::Project(
                "source time base is unusable".into(),
            ));
        }

        // Map mark PTS → source seconds. Default to (0, media.duration).
        let mark_in_pts = self.source_mark_in_pts.unwrap_or(0);
        let mark_out_pts = self
            .source_mark_out_pts
            .or(info.duration_pts)
            .ok_or_else(|| {
                EngineError::Project(
                    "source media has no duration; mark an out-point first".into(),
                )
            })?;
        if mark_out_pts <= mark_in_pts {
            return Err(EngineError::Project(
                "source mark-out is at or before mark-in".into(),
            ));
        }
        let source_in_sec = (mark_in_pts as f64) * (tb_n as f64) / (tb_d as f64);
        let source_out_sec = (mark_out_pts as f64) * (tb_n as f64) / (tb_d as f64);

        // Record playhead — defaults to 0 when the user hasn't moved it.
        let timeline_in_sec = self.current_timeline_seconds().unwrap_or(0.0).max(0.0);

        // Look up the MediaItem so we can use its display name on the
        // new clip and pass it as the source duration anchor.
        let project = self
            .current_project
            .as_ref()
            .ok_or(EngineError::NoProject)?;
        let media = project
            .media
            .get(&media_id)
            .ok_or(EngineError::MediaNotFound)?
            .clone();

        let clip = Clip::new(
            media.id,
            media.display_name.clone(),
            seconds_to_rt(timeline_in_sec),
            seconds_to_rt(source_in_sec),
            seconds_to_rt(source_out_sec),
        )
        .map_err(|e| EngineError::Project(e.to_string()))?;
        let new_clip_id = clip.id;

        let op = if overwrite {
            EditOp::OverwriteClip { track_id, clip }
        } else {
            EditOp::RippleInsertClip { track_id, clip }
        };
        self.apply_edit_op(op)?;
        Ok(new_clip_id.to_string())
    }

    /// Decode the Source media at `target_pts` and render it into
    /// the Source viewport.
    pub fn decode_source_at(&mut self, target_pts: i64) -> Result<i64, EngineError> {
        let dec = self.source_decoder.as_mut().ok_or(EngineError::NoFileOpen)?;
        let frame = dec.decode_at(target_pts)?;
        let pts = frame.pts;
        let viewport = self
            .source_viewport
            .as_ref()
            .ok_or(EngineError::NoViewport)?
            .clone();
        let render_state = self
            .render_state
            .as_ref()
            .ok_or(EngineError::NoViewport)?
            .clone();
        let bounds = self.source_last_bounds;
        // Source monitor renders the raw media — reset the params the
        // Record-side sequence playback may have left on the renderer
        // so opacity/crop/transform/color/mix/keys don't bleed into Source.
        // Pipeline: source's tagged input → linear Rec.709 (a sane
        // intermediate; the source preview doesn't honor the project
        // working space) → sRGB.
        let source_pipeline = self.source_media_id.and_then(|id| {
            let project = self.current_project.as_ref()?;
            let media = project.media.get(&id)?;
            let input = media
                .input_color_space
                .unwrap_or(vance_color::ColorSpace::Rec709Limited);
            vance_color::Pipeline {
                input,
                working: vance_color::ColorSpace::LinearRec709,
                output: vance_color::ColorSpace::Srgb,
            }
            .to_params()
        });
        {
            let mut rs = render_state.lock();
            rs.renderer.set_opacity(1.0);
            rs.renderer.set_transform([0.0, 0.0], [1.0, 1.0], 0.0);
            rs.renderer.set_brightness_contrast(0.0, 1.0);
            rs.renderer.set_crop(0.0, 0.0, 0.0, 0.0);
            rs.renderer.set_mix_factor(0.0);
            rs.renderer.set_chroma_key([0.0, 1.0, 0.0], 0.15, 0.05, 0.0);
            rs.renderer.set_luma_key(0.5, 1.0, 0.05, 0.0);
            rs.renderer.set_lut_enable(0.0);
            rs.renderer.set_grades(&[]);
            if let Some(params) = source_pipeline {
                rs.renderer.set_pipeline(params);
            }
        }
        upload_and_render_source(&render_state, &viewport, &frame, bounds)?;
        self.source_last_pts = Some(pts);
        Ok(pts)
    }

    /// Stream info from the last `load_source`. Used by the UI to
    /// format the time display and scrub bar.
    pub fn source_stream_info(&self) -> Option<&StreamInfo> {
        self.source_stream_info.as_ref()
    }

    /// Snapshot of the Source monitor's state for the UI — stream
    /// info + last-presented PTS + marks. Returns `None` when
    /// nothing has been loaded into Source yet.
    pub fn source_state(&self) -> Option<SourceState> {
        let info = self.source_stream_info.as_ref()?;
        Some(SourceState {
            width: info.width,
            height: info.height,
            codec_name: info.codec_name.clone(),
            time_base_num: info.time_base.0,
            time_base_den: info.time_base.1,
            duration_pts: info.duration_pts,
            last_pts: self.source_last_pts,
            mark_in_pts: self.source_mark_in_pts,
            mark_out_pts: self.source_mark_out_pts,
        })
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
        self.pause_for_edit();
        // Reset effect uniforms to identity when nothing else has
        // anchored them — keeps single-file `Open File…` playback
        // unaffected by a previous sequence preview's params.
        if self.playing_clip_anchor.is_none() {
            if let Some(rs) = self.render_state.as_ref() {
                let mut rs = rs.lock();
                rs.renderer.set_opacity(1.0);
                rs.renderer.set_transform([0.0, 0.0], [1.0, 1.0], 0.0);
                rs.renderer.set_brightness_contrast(0.0, 1.0);
                rs.renderer.set_crop(0.0, 0.0, 0.0, 0.0);
                rs.renderer.set_mix_factor(0.0);
                rs.renderer.set_chroma_key([0.0, 1.0, 0.0], 0.15, 0.05, 0.0);
                rs.renderer.set_luma_key(0.5, 1.0, 0.05, 0.0);
                rs.renderer.set_grades(&[]);
            }
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
        self.pause_for_edit();
        self.start_playback_inner(StartPlaybackKind::Video { from_pts })
    }

    /// Wraps the heavy lifting so `start_playback` and the title-mode
    /// entry point share thread setup, atomics, and join handling.
    fn start_playback_inner(
        &mut self,
        kind: StartPlaybackKind,
    ) -> Result<(), EngineError> {
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

        // Construct the primary source. Video mode takes the engine's
        // decoder out and starts streaming on it; title mode renders
        // the title frame and leaves the decoder slot alone.
        let (primary, audio_owner, audio_sync, from_pts) = match kind {
            StartPlaybackKind::Video { from_pts } => {
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
                if let Err(e) = decoder.start_stream(from_pts, 8) {
                    self.decoder = Some(decoder);
                    return Err(EngineError::DecoderClient(e));
                }
                let (vtb_n, vtb_d) = info.time_base;
                let from_seconds = if vtb_d > 0 {
                    (from_pts as f64) * (vtb_n as f64) / (vtb_d as f64)
                } else {
                    0.0
                };
                let (audio_owner, audio_sync) =
                    match start_audio(&media_path, from_seconds) {
                        Ok((owner, sync)) => (Some(owner), Some(sync)),
                        Err(e) => {
                            tracing::warn!(?e, "audio: not started; video will use wall-clock");
                            (None, None)
                        }
                    };
                (
                    PlaybackPrimary::Decoder {
                        client: decoder,
                        info,
                        from_pts,
                    },
                    audio_owner,
                    audio_sync,
                    from_pts,
                )
            }
            StartPlaybackKind::Title {
                params,
                fps,
                duration_seconds,
            } => {
                let frame = render_title_yuv420(&params)
                    .map_err(|e| EngineError::Project(format!("title render: {e}")))?;
                // Titles are silent in v1 — the loop's wall-clock
                // master keeps timing honest without an audio sidecar.
                (
                    PlaybackPrimary::Title {
                        frame,
                        fps,
                        duration_seconds,
                    },
                    None,
                    None,
                    0,
                )
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();
        let last_pts = Arc::new(std::sync::atomic::AtomicI64::new(from_pts));
        let last_pts_clone = last_pts.clone();
        let ended_at_eos = Arc::new(AtomicBool::new(false));
        let ended_at_eos_clone = ended_at_eos.clone();
        let chain_resume_micros =
            Arc::new(std::sync::atomic::AtomicI64::new(i64::MIN));
        let chain_resume_micros_clone = chain_resume_micros.clone();
        let last_bounds = self.last_bounds;
        // Consume the effects snapshot stashed by play_sequence
        // (empty for single-file Open File… playback).
        let clip_effects = std::mem::take(&mut self.next_play_effects);
        let clip_source_in_sec = self.next_play_source_in_sec;
        self.next_play_source_in_sec = 0.0;

        // Build the optional sequence context. Only set when the
        // anchor was placed by `play_sequence` (i.e., sequence
        // playback, not Open File…) — single-file playback has no
        // transitions to query. The secondary decoder is moved out of
        // engine state so the loop owns it during the run.
        let seq_ctx = if let (Some(project), Some(seq_id), Some(anchor)) = (
            self.current_project.as_ref(),
            self.current_sequence_id,
            self.playing_clip_anchor.as_ref(),
        ) {
            project.sequence(seq_id).map(|seq| SequencePlaybackContext {
                sequence: (**seq).clone(),
                // File-backed media → path map; title media → params map.
                // Skipping unknown variants here is fine — the loop only
                // looks up media IDs that scene_at returned, and those
                // came from the same project.
                media_paths: project
                    .media
                    .iter()
                    .filter(|(_, m)| m.kind != MediaKind::Title)
                    .map(|(id, m)| (*id, m.path.clone()))
                    .collect(),
                title_params: project
                    .media
                    .iter()
                    .filter_map(|(id, m)| {
                        if m.kind == MediaKind::Title {
                            m.title.clone().map(|p| (*id, p))
                        } else {
                            None
                        }
                    })
                    .collect(),
                decoder_bin: locate_decoder_binary().unwrap_or_default(),
                anchor_timeline_in: anchor.timeline_in_seconds,
                anchor_source_in: anchor.source_in_seconds,
            })
        } else {
            None
        };
        let transition_decoder = self.transition_decoder.take();
        let transition_media_path = self.transition_media_path.take();
        let handle = std::thread::Builder::new()
            .name("vance-engine.playback".into())
            .spawn(move || {
                playback_loop(
                    primary,
                    last_bounds,
                    viewport,
                    render_state,
                    stop_clone,
                    audio_sync,
                    last_pts_clone,
                    ended_at_eos_clone,
                    chain_resume_micros_clone,
                    clip_effects,
                    clip_source_in_sec,
                    seq_ctx,
                    transition_decoder,
                    transition_media_path,
                )
            })
            .map_err(|e| EngineError::PlaybackThread(format!("spawn: {e}")))?;
        self.playback = Some(PlaybackSession {
            stop,
            last_pts,
            ended_at_eos,
            chain_resume_micros,
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
        // Explicit stop also ends sequence-chaining; the auto-reap
        // path snapshots and re-sets the flag for itself.
        self.playing_sequence_active = false;
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
        let resume_pts = if session.ended_at_eos.load(Ordering::Acquire)
            && self.playing_clip_anchor.is_none()
        {
            // Single-file EOS → restart from 0 next time so Play means
            // "play again". In sequence mode this would force the
            // playhead back to t=0 on every clip end, which is jarring;
            // the UI is responsible for rewinding (see togglePlayback).
            0
        } else {
            session.last_pts.load(Ordering::Acquire)
        };
        match session.handle.join() {
            Ok(out) => {
                // Primary decoder is only set when the session was
                // video-mode; title sessions leave the engine's
                // existing `decoder` slot alone.
                if let Some(d) = out.primary_decoder {
                    self.decoder = Some(d);
                }
                // Reclaim the warm secondary + its open path so a
                // subsequent scrub / playback through the same B
                // media skips re-opening the sidecar.
                self.transition_decoder = out.secondary_decoder;
                self.transition_media_path = out.secondary_media_path;
                self.transition_stream_info = None;
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
        self.pause_for_edit();
        let project = vance_project::open(path).map_err(|e| EngineError::Project(e.to_string()))?;
        let mut info = ProjectInfo::from(&project);
        self.current_sequence_id = project.sequences.first().map(|s| s.id);
        info.current_sequence_id = self.current_sequence_id.map(|id| id.to_string());
        // Fresh edit-log per loaded sequence, seeded with that
        // sequence's current state as the undo baseline.
        self.edit_logs.clear();
        for s in &project.sequences {
            self.edit_logs
                .insert(s.id, EditLog::new(s.as_ref().clone()));
        }
        self.current_project_path = Some(PathBuf::from(path));
        self.current_project = Some(project);
        // Light up the project's saved view LUT (if any) so the
        // viewport reflects the user's last monitoring preference.
        self.apply_view_lut();
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
        self.pause_for_edit();
        let mut project = Project::new(name);
        let mut seq = Sequence::new("Sequence 01", vance_core::FrameRate::FPS_24);
        seq.tracks.push(Arc::new(Track::new(TrackKind::Video, "V1")));
        seq.tracks.push(Arc::new(Track::new(TrackKind::Audio, "A1")));
        let seq_id = seq.id;
        let seq_for_log = seq.clone();
        project.sequences.push(Arc::new(seq));
        let mut info = ProjectInfo::from(&project);
        info.current_sequence_id = Some(seq_id.to_string());
        self.current_sequence_id = Some(seq_id);
        self.current_project_path = None;
        self.edit_logs.clear();
        self.edit_logs.insert(seq_id, EditLog::new(seq_for_log));
        self.current_project = Some(project);
        self.apply_view_lut();
        Ok(info)
    }

    /// Import a media file by path. Briefly opens it via the decoder
    /// sidecar to capture its metadata (codec, duration, frame rate,
    /// pixel format), then registers it as a `MediaItem` in the
    /// current project's media library.
    pub fn import_media_file(&mut self, path: &str) -> Result<MediaItemInfo, EngineError> {
        self.pause_for_edit();
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

    /// Override a media item's tagged input color space. Pass `None`
    /// to clear (engine then falls back to Rec.709 Limited). `space`
    /// strings are the labels used by `color_space_to_str` / the UI
    /// dropdown — invalid strings return an error. Phase 6.2.
    pub fn set_media_input_color_space(
        &mut self,
        media_id: MediaItemId,
        space: Option<&str>,
    ) -> Result<(), EngineError> {
        let resolved = match space {
            None | Some("") => None,
            Some(s) => Some(color_space_from_str(s).ok_or_else(|| {
                EngineError::Project(format!("unknown color space label: {s}"))
            })?),
        };
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        let existing = project
            .media
            .get(&media_id)
            .ok_or(EngineError::MediaNotFound)?
            .clone();
        let mut updated = (*existing).clone();
        updated.input_color_space = resolved;
        project.media.insert(media_id, Arc::new(updated));
        Ok(())
    }

    /// Replace the project's working color space. Phase 6.2.
    pub fn set_project_working_color_space(
        &mut self,
        space: &str,
    ) -> Result<(), EngineError> {
        let space = color_space_from_str(space)
            .ok_or_else(|| EngineError::Project(format!("unknown color space label: {space}")))?;
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        project.working_color_space = space;
        Ok(())
    }

    /// Set (or clear) the project's display LUT path. The renderer's
    /// view-LUT slot picks it up on the next `apply_view_lut` call —
    /// done immediately so the viewport reflects the change.
    /// Phase 6.5.
    pub fn set_project_view_lut(
        &mut self,
        path: Option<&str>,
    ) -> Result<(), EngineError> {
        {
            let project = self
                .current_project
                .as_mut()
                .ok_or(EngineError::NoProject)?;
            project.view_lut_path = path
                .filter(|s| !s.is_empty())
                .map(PathBuf::from);
        }
        self.apply_view_lut();
        Ok(())
    }

    /// Load the project's `view_lut_path` (if any) into the renderer's
    /// view-LUT slot, or disable the slot. Called on project load,
    /// when the user picks a new LUT, and on every active-clip
    /// transition so re-entry to the engine state lights up the LUT.
    fn apply_view_lut(&mut self) {
        let Some(rs) = self.render_state.as_ref() else {
            return;
        };
        let path = match self
            .current_project
            .as_ref()
            .and_then(|p| p.view_lut_path.clone())
        {
            Some(p) => p,
            None => {
                let mut rs = rs.lock();
                rs.renderer.set_view_lut_enable(0.0);
                return;
            }
        };
        if let Some(lut) = self.lut_cache.get(&path).cloned() {
            let mut rs = rs.lock();
            rs.renderer.upload_view_lut(&lut);
            rs.renderer.set_view_lut_enable(1.0);
            return;
        }
        match vance_color::load_lut(&path) {
            Ok(lut) => {
                let arc = Arc::new(lut);
                self.lut_cache.insert(path.clone(), arc.clone());
                let mut rs = rs.lock();
                rs.renderer.upload_view_lut(&arc);
                rs.renderer.set_view_lut_enable(1.0);
            }
            Err(e) => {
                tracing::warn!(?e, ?path, "view-lut: load failed");
                let mut rs = rs.lock();
                rs.renderer.set_view_lut_enable(0.0);
            }
        }
    }

    /// Add a synthetic title media item to the project's library.
    /// Returns the item's info so the UI can immediately drag it onto
    /// the timeline. No file is created — the engine renders frames
    /// from `params` on demand. Errors when no project is loaded.
    pub fn create_title_media(
        &mut self,
        display_name: String,
        duration_seconds: f64,
        params: TitleParams,
    ) -> Result<MediaItemInfo, EngineError> {
        if self.current_project.is_none() {
            return Err(EngineError::NoProject);
        }
        let duration = seconds_to_rt(duration_seconds.max(0.04));
        let item = MediaItem::new_title(display_name, duration, params);
        let id = item.id;
        let info = MediaItemInfo::from(&item);
        let project = self.current_project.as_mut().unwrap();
        project.media.insert(id, Arc::new(item));
        tracing::info!(id = %id, "created title media");
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
        self.pause_for_edit();
        // Scope the immutable project borrow tightly so we can call
        // `self.open_file` (mutable borrow) afterwards. Also capture
        // the clip's timeline/source anchors and effect-derived
        // opacity so the renderer fades the clip if needed.
        let (
            resolved,
            source_pts_seconds,
            anchor_timeline_in,
            anchor_source_in,
            anchor_opacity,
            anchor_clip_id,
            clip_duration_sec,
        ) = {
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
            let resolved = self.media_for_active(active.media)?;
            // Find the source clip's timeline_in for the anchor.
            let (tl_in_sec, src_in_sec) = clip_anchor_seconds(seq, active.clip_id)
                .unwrap_or((0.0, rt_to_seconds(active.source_pts)));
            // Clip-relative time at the entry point (used for the
            // initial opacity/transform snapshot below). Each
            // subsequent frame re-evaluates inside the playback_loop.
            let entry_clip_t = rt_to_seconds(active.source_pts) - src_in_sec;
            let op = clip_opacity_at(seq, active.clip_id, entry_clip_t);
            // Clip duration on the timeline — needed so title playback
            // knows when to stop synthesizing frames.
            let clip_dur = seq
                .tracks
                .iter()
                .find_map(|t| {
                    t.clips
                        .iter()
                        .find(|c| c.id == active.clip_id)
                        .map(|c| rt_to_seconds(c.duration()))
                })
                .unwrap_or(0.0);
            (
                resolved,
                rt_to_seconds(active.source_pts),
                tl_in_sec,
                src_in_sec,
                op,
                active.clip_id,
                clip_dur,
            )
        };

        // For title clips, take the title-playback branch and skip the
        // decoder open + audio open + stream-info logic entirely.
        if let MediaResolved::Title(params) = resolved {
            return self.play_title_clip(
                params,
                anchor_clip_id,
                anchor_timeline_in,
                anchor_source_in,
                source_pts_seconds,
                anchor_opacity,
                clip_duration_sec,
            );
        }
        let media_path = match resolved {
            MediaResolved::File(p) => p,
            MediaResolved::Title(_) => unreachable!("title handled above"),
        };

        // Reopen decoder onto the active clip's media. Idempotent if
        // we're already on this file: the decoder reopens cleanly.
        self.open_file(&media_path)?;
        // Snapshot the active clip's effects so the playback_loop
        // can re-evaluate keyframes per frame. Also push the
        // entry-time values into the renderer so the first frame
        // looks right before the loop has a chance to update.
        let (entry_xform, entry_crop, entry_color, entry_chroma, entry_luma, clip_effects_snapshot) = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
            let entry_clip_t = source_pts_seconds - anchor_source_in;
            let xform = clip_transform_at(seq, anchor_clip_id, entry_clip_t);
            let crop = clip_crop_at(seq, anchor_clip_id, entry_clip_t);
            let color = clip_brightness_contrast_at(seq, anchor_clip_id, entry_clip_t);
            let chroma = clip_chroma_key_at(seq, anchor_clip_id, entry_clip_t);
            let luma = clip_luma_key_at(seq, anchor_clip_id, entry_clip_t);
            // Snapshot the active clip's effects.
            let effects = seq
                .tracks
                .iter()
                .find_map(|t| {
                    t.clips
                        .iter()
                        .find(|c| c.id == anchor_clip_id)
                        .map(|c| c.effects.clone())
                })
                .unwrap_or_default();
            (xform, crop, color, chroma, luma, effects)
        };
        let entry_pipeline = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
            let media_id = seq
                .tracks
                .iter()
                .find_map(|t| {
                    t.clips
                        .iter()
                        .find(|c| c.id == anchor_clip_id)
                        .map(|c| c.media)
                })
                .ok_or(EngineError::Project("clip not found".into()))?;
            self.pipeline_for_media(media_id)
        };
        if let Some(rs) = self.render_state.as_ref() {
            let mut rs = rs.lock();
            rs.renderer.set_opacity(anchor_opacity);
            rs.renderer.set_transform(
                entry_xform.translate,
                entry_xform.scale,
                entry_xform.rotation_radians,
            );
            rs.renderer.set_brightness_contrast(entry_color.0, entry_color.1);
            rs.renderer.set_crop(entry_crop[0], entry_crop[1], entry_crop[2], entry_crop[3]);
            rs.renderer.set_mix_factor(0.0);
            rs.renderer.set_chroma_key(
                entry_chroma.0,
                entry_chroma.1,
                entry_chroma.2,
                entry_chroma.3,
            );
            rs.renderer.set_luma_key(
                entry_luma.0,
                entry_luma.1,
                entry_luma.2,
                entry_luma.3,
            );
            rs.renderer.set_grades(&eval_clip_grades(
                &clip_effects_snapshot,
                source_pts_seconds - anchor_source_in,
            ));
            if let Some(p) = entry_pipeline {
                rs.renderer.set_pipeline(p);
            }
        }
        self.apply_lut_for_clip(anchor_clip_id);

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
            opacity: anchor_opacity,
        });
        self.playing_sequence_active = true;
        // Stash effects for the playback_loop to evaluate per frame.
        self.next_play_effects = clip_effects_snapshot;
        self.next_play_source_in_sec = anchor_source_in;
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

    /// Open `path` in the transition decoder, spawning the sidecar
    /// process on first use. Idempotent when `path` is already loaded.
    fn ensure_transition_decoder(
        &mut self,
        path: &str,
    ) -> Result<(), EngineError> {
        if self.transition_decoder.is_none() {
            let bin = locate_decoder_binary()?;
            let client = DecoderClient::spawn(bin)?;
            self.transition_decoder = Some(client);
            self.transition_media_path = None;
            self.transition_stream_info = None;
        }
        if self.transition_media_path.as_deref() != Some(path) {
            let dec = self.transition_decoder.as_mut().unwrap();
            let info = dec.open(path)?;
            self.transition_stream_info = Some(info);
            self.transition_media_path = Some(path.to_string());
        }
        Ok(())
    }

    /// Transition-aware single-shot decode + render. Pulls a frame
    /// from each side (decoder or title) and blends them via the
    /// renderer's `mix_factor`. Used by `decode_sequence_at` when the
    /// timeline position is inside an active transition window.
    fn decode_transition_at(
        &mut self,
        seq: &Sequence,
        active_tr: vance_timeline::ActiveTransition,
        _timeline_seconds: f64,
    ) -> Result<i64, EngineError> {
        let from_resolved = self.media_for_active(active_tr.from.media)?;
        let to_resolved = self.media_for_active(active_tr.to.media)?;
        let from_clip_id = active_tr.from.clip_id;
        let (anchor_tl_in, anchor_src_in) =
            clip_anchor_seconds(seq, from_clip_id).unwrap_or((0.0, 0.0));
        let from_clip_t = rt_to_seconds(active_tr.from.source_pts) - anchor_src_in;
        let op = clip_opacity_at(seq, from_clip_id, from_clip_t);
        let xform = clip_transform_at(seq, from_clip_id, from_clip_t);
        let crop = clip_crop_at(seq, from_clip_id, from_clip_t);
        let color = clip_brightness_contrast_at(seq, from_clip_id, from_clip_t);
        let chroma = clip_chroma_key_at(seq, from_clip_id, from_clip_t);
        let luma = clip_luma_key_at(seq, from_clip_id, from_clip_t);

        // Resolve A's frame. Decoder path opens the file and seeks;
        // title path rasterizes from params.
        let (frame_a, pts_a, anchor_tb): (vance_decoder::FrameData, i64, (i64, i64)) =
            match from_resolved {
                MediaResolved::File(from_path) => {
                    self.open_file(&from_path)?;
                    let info = self
                        .stream_info
                        .as_ref()
                        .ok_or_else(|| EngineError::Project("stream info missing post-open".into()))?
                        .clone();
                    let (tb_n, tb_d) = info.time_base;
                    let from_src_sec = rt_to_seconds(active_tr.from.source_pts);
                    let from_pts = if tb_n != 0 {
                        (from_src_sec * (tb_d as f64) / (tb_n as f64)).round() as i64
                    } else {
                        0
                    };
                    let dec = self.decoder.as_mut().ok_or(EngineError::NoFileOpen)?;
                    let frame = dec.decode_at(from_pts)?;
                    let pts = frame.pts;
                    (frame, pts, (tb_n, tb_d))
                }
                MediaResolved::Title(params) => {
                    let frame = render_title_yuv420(&params)
                        .map_err(|e| EngineError::Project(format!("title render: {e}")))?;
                    (frame, 0, (1, 1000))
                }
            };
        self.playing_clip_anchor = Some(ClipAnchor {
            timeline_in_seconds: anchor_tl_in,
            source_in_seconds: anchor_src_in,
            time_base_num: anchor_tb.0,
            time_base_den: anchor_tb.1,
            opacity: op,
        });

        // Resolve B's frame. Same dispatch on title vs decoder.
        let frame_b: vance_decoder::FrameData = match to_resolved {
            MediaResolved::File(to_path) => {
                self.ensure_transition_decoder(&to_path)?;
                let to_info = self
                    .transition_stream_info
                    .as_ref()
                    .ok_or_else(|| {
                        EngineError::Project("transition stream info missing".into())
                    })?
                    .clone();
                let (to_tb_n, to_tb_d) = to_info.time_base;
                let to_src_sec = rt_to_seconds(active_tr.to.source_pts);
                let to_pts = if to_tb_n != 0 {
                    (to_src_sec * (to_tb_d as f64) / (to_tb_n as f64)).round() as i64
                } else {
                    0
                };
                let to_dec = self
                    .transition_decoder
                    .as_mut()
                    .expect("ensure_transition_decoder just initialized it");
                to_dec.decode_at(to_pts)?
            }
            MediaResolved::Title(params) => render_title_yuv420(&params)
                .map_err(|e| EngineError::Project(format!("title render: {e}")))?,
        };

        // Push effect uniforms + mix factor to the renderer. The
        // pipeline params follow A's media (the outgoing side) — v1
        // limitation: if A and B have different input color spaces,
        // B's frames pass through A's pipeline, which can tint the
        // transition window slightly. Two-sided pipelines are a
        // future shader extension.
        let a_pipeline = self.pipeline_for_media(active_tr.from.media);
        if let Some(rs) = self.render_state.as_ref() {
            let mut rs = rs.lock();
            rs.renderer.set_opacity(op);
            rs.renderer
                .set_transform(xform.translate, xform.scale, xform.rotation_radians);
            rs.renderer.set_brightness_contrast(color.0, color.1);
            rs.renderer.set_crop(crop[0], crop[1], crop[2], crop[3]);
            rs.renderer.set_chroma_key(chroma.0, chroma.1, chroma.2, chroma.3);
            rs.renderer.set_luma_key(luma.0, luma.1, luma.2, luma.3);
            rs.renderer.set_grades(&clip_grades_at(seq, from_clip_id, from_clip_t));
            rs.renderer.set_mix_factor(active_tr.mix);
            if let Some(p) = a_pipeline {
                rs.renderer.set_pipeline(p);
            }
            rs.renderer.upload_frame_b(&frame_b)?;
        }
        self.apply_lut_for_clip(from_clip_id);

        let viewport = self.viewport.as_ref().ok_or(EngineError::NoViewport)?.clone();
        let render_state = self
            .render_state
            .as_ref()
            .ok_or(EngineError::NoViewport)?
            .clone();
        upload_and_render(&render_state, &viewport, &frame_a, self.last_bounds)?;
        self.last_resume_pts = Some(pts_a);
        Ok(pts_a)
    }

    /// Decode a single frame from the active sequence at
    /// `timeline_seconds` (used by seek-while-paused). Bypasses the
    /// playback scheduler.
    /// Live playback of a title clip — spawns the playback thread with
    /// a synthetic `PlaybackPrimary::Title` source. Same chain hooks
    /// kick in at EOS (when the clip's `duration_seconds` elapses) so
    /// the next clip on the timeline plays automatically.
    ///
    /// `source_pts_seconds` is the entry point inside the clip's
    /// source range — non-zero when playback resumes mid-title after
    /// a dissolve. The synthetic loop shrinks its remaining duration
    /// and the anchor is shifted forward so the timeline mapping
    /// lands at the correct spot.
    #[allow(clippy::too_many_arguments)]
    fn play_title_clip(
        &mut self,
        params: TitleParams,
        anchor_clip_id: vance_timeline::ClipId,
        anchor_timeline_in: f64,
        anchor_source_in: f64,
        source_pts_seconds: f64,
        anchor_opacity: f32,
        clip_duration_sec: f64,
    ) -> Result<(), EngineError> {
        // Entry offset into the clip's source range. 0 when we start
        // at the title's beginning; > 0 when chained-in after a
        // transition that crossed our timeline_in.
        let entry_offset = (source_pts_seconds - anchor_source_in).max(0.0);
        let remaining_dur = (clip_duration_sec - entry_offset).max(0.04);
        // Shift the anchor forward so timeline_t = anchor_timeline_in
        // at synthetic_pts = 0 lands at the *entry* timeline position,
        // not at the clip's start.
        let effective_tl_in = anchor_timeline_in + entry_offset;
        // Push the clip's effect chain into the renderer for frame 1.
        let (xform, crop, color, chroma, luma, effects) = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let seq_id = self
                .current_sequence_id
                .ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
            let entry_clip_t = 0.0;
            let effects = seq
                .tracks
                .iter()
                .find_map(|t| {
                    t.clips
                        .iter()
                        .find(|c| c.id == anchor_clip_id)
                        .map(|c| c.effects.clone())
                })
                .unwrap_or_default();
            (
                clip_transform_at(seq, anchor_clip_id, entry_clip_t),
                clip_crop_at(seq, anchor_clip_id, entry_clip_t),
                clip_brightness_contrast_at(seq, anchor_clip_id, entry_clip_t),
                clip_chroma_key_at(seq, anchor_clip_id, entry_clip_t),
                clip_luma_key_at(seq, anchor_clip_id, entry_clip_t),
                effects,
            )
        };
        let title_pipeline = vance_color::Pipeline {
            input: vance_color::ColorSpace::Srgb,
            working: self.project_working_space(),
            output: vance_color::ColorSpace::Srgb,
        }
        .to_params();
        if let Some(rs) = self.render_state.as_ref() {
            let mut rs = rs.lock();
            rs.renderer.set_opacity(anchor_opacity);
            rs.renderer
                .set_transform(xform.translate, xform.scale, xform.rotation_radians);
            rs.renderer.set_brightness_contrast(color.0, color.1);
            rs.renderer.set_crop(crop[0], crop[1], crop[2], crop[3]);
            rs.renderer.set_chroma_key(chroma.0, chroma.1, chroma.2, chroma.3);
            rs.renderer.set_luma_key(luma.0, luma.1, luma.2, luma.3);
            rs.renderer.set_grades(&eval_clip_grades(&effects, 0.0));
            rs.renderer.set_mix_factor(0.0);
            if let Some(p) = title_pipeline {
                rs.renderer.set_pipeline(p);
            }
        }
        self.apply_lut_for_clip(anchor_clip_id);
        self.playing_clip_anchor = Some(ClipAnchor {
            timeline_in_seconds: effective_tl_in,
            // The Title primary always ticks from synthetic_pts = 0,
            // so the anchor's source_in is the title's own source_in
            // (typically 0); the loop's frame_sec then equals
            // "elapsed since entry".
            source_in_seconds: anchor_source_in,
            // Title primary's pts_to_sec maps ms → sec (see
            // `PlaybackPrimary::time_base`), so the anchor must match.
            time_base_num: 1,
            time_base_den: 1000,
            opacity: anchor_opacity,
        });
        self.playing_sequence_active = true;
        self.next_play_effects = effects;
        self.next_play_source_in_sec = anchor_source_in;
        self.start_playback_inner(StartPlaybackKind::Title {
            params,
            fps: 24.0,
            duration_seconds: remaining_dur,
        })
    }

    /// Single-shot title render — same role as `decode_and_render_at`
    /// but for synthetic clips. Rasterizes the title via
    /// `vance_render::render_title_yuv420`, applies the clip's effect
    /// chain (opacity/transform/crop/color), uploads + presents.
    /// Returns a synthetic PTS of 0; callers use timeline-seconds
    /// for the playhead instead of the source PTS.
    fn decode_title_scrub(
        &mut self,
        params: TitleParams,
        clip_id: vance_timeline::ClipId,
        anchor_timeline_in: f64,
        anchor_source_in: f64,
        source_pts_seconds: f64,
        opacity: f32,
    ) -> Result<i64, EngineError> {
        let frame = render_title_yuv420(&params)
            .map_err(|e| EngineError::Project(format!("title render: {e}")))?;
        // Effect chain (transform / crop / color / keys) evaluated at
        // the clip-relative time. Titles aren't usually animated but
        // we honor their effects for consistency.
        let (xform, crop, color, chroma, luma, grades) = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let seq_id = self
                .current_sequence_id
                .ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
            let clip_t = source_pts_seconds - anchor_source_in;
            (
                clip_transform_at(seq, clip_id, clip_t),
                clip_crop_at(seq, clip_id, clip_t),
                clip_brightness_contrast_at(seq, clip_id, clip_t),
                clip_chroma_key_at(seq, clip_id, clip_t),
                clip_luma_key_at(seq, clip_id, clip_t),
                clip_grades_at(seq, clip_id, clip_t),
            )
        };
        // Title media is tagged sRGB; route it through the project
        // working space so it matches the rest of the timeline.
        let title_pipeline = vance_color::Pipeline {
            input: vance_color::ColorSpace::Srgb,
            working: self.project_working_space(),
            output: vance_color::ColorSpace::Srgb,
        }
        .to_params();
        if let Some(rs) = self.render_state.as_ref() {
            let mut rs = rs.lock();
            rs.renderer.set_opacity(opacity);
            rs.renderer
                .set_transform(xform.translate, xform.scale, xform.rotation_radians);
            rs.renderer.set_brightness_contrast(color.0, color.1);
            rs.renderer.set_crop(crop[0], crop[1], crop[2], crop[3]);
            rs.renderer.set_chroma_key(chroma.0, chroma.1, chroma.2, chroma.3);
            rs.renderer.set_luma_key(luma.0, luma.1, luma.2, luma.3);
            rs.renderer.set_grades(&grades);
            rs.renderer.set_mix_factor(0.0);
            if let Some(p) = title_pipeline {
                rs.renderer.set_pipeline(p);
            }
        }
        self.apply_lut_for_clip(clip_id);
        // Synthetic anchor — titles have no stream time base, so we
        // pin time_base=(1, 1000) and use timeline seconds as the
        // "stream pts" surrogate (millisecond resolution). Playback
        // chain logic does timeline-seconds math against this, so
        // the chosen base just needs to be consistent.
        self.playing_clip_anchor = Some(ClipAnchor {
            timeline_in_seconds: anchor_timeline_in,
            source_in_seconds: anchor_source_in,
            time_base_num: 1,
            time_base_den: 1000,
            opacity,
        });
        let viewport = self.viewport.as_ref().ok_or(EngineError::NoViewport)?.clone();
        let render_state = self
            .render_state
            .as_ref()
            .ok_or(EngineError::NoViewport)?
            .clone();
        upload_and_render(&render_state, &viewport, &frame, self.last_bounds)?;
        // Use the source-seconds-as-milliseconds for the PTS-like
        // return value so the UI's "current_timeline_seconds" math
        // works the same way it does for video clips.
        let synthetic_pts = (source_pts_seconds * 1000.0).round() as i64;
        self.last_resume_pts = Some(synthetic_pts);
        Ok(synthetic_pts)
    }

    /// Find the first enabled "lut" effect on `clip_id` and push it
    /// to the renderer. When no LUT effect is attached (or its
    /// `enabled` is false), disables the renderer's LUT slot. The
    /// cache absorbs repeat hits — only the first scrub through a
    /// path actually reads + parses the file.
    fn apply_lut_for_clip(&mut self, clip_id: vance_timeline::ClipId) {
        let Some(rs) = self.render_state.as_ref() else {
            return;
        };
        let path_opt: Option<PathBuf> = {
            let project = self.current_project.as_ref();
            let seq_id = self.current_sequence_id;
            let mut path: Option<PathBuf> = None;
            if let (Some(p), Some(id)) = (project, seq_id) {
                if let Some(seq) = p.sequence(id) {
                    'outer: for track in seq.tracks.iter() {
                        for c in track.clips.iter() {
                            if c.id != clip_id {
                                continue;
                            }
                            for e in c.effects.iter() {
                                if !e.enabled || e.kind != "lut" {
                                    continue;
                                }
                                if let Some(EffectValue::String(s)) = e.params.get("path") {
                                    if !s.is_empty() {
                                        path = Some(PathBuf::from(s));
                                    }
                                }
                                break 'outer;
                            }
                            break 'outer;
                        }
                    }
                }
            }
            path
        };
        let Some(path) = path_opt else {
            let mut rs = rs.lock();
            rs.renderer.set_lut_enable(0.0);
            return;
        };
        // Cache hit?
        if let Some(lut) = self.lut_cache.get(&path).cloned() {
            let mut rs = rs.lock();
            rs.renderer.upload_lut(&lut);
            rs.renderer.set_lut_enable(1.0);
            return;
        }
        // Miss — parse from disk + cache.
        match vance_color::load_lut(&path) {
            Ok(lut) => {
                let arc = Arc::new(lut);
                self.lut_cache.insert(path.clone(), arc.clone());
                let mut rs = rs.lock();
                rs.renderer.upload_lut(&arc);
                rs.renderer.set_lut_enable(1.0);
            }
            Err(e) => {
                tracing::warn!(?e, ?path, "lut: load failed");
                let mut rs = rs.lock();
                rs.renderer.set_lut_enable(0.0);
            }
        }
    }

    /// Working color space currently active on the project. Falls
    /// back to LinearRec709 when no project is loaded (single-file
    /// Open File… playback path).
    fn project_working_space(&self) -> vance_color::ColorSpace {
        self.current_project
            .as_ref()
            .map(|p| p.working_color_space)
            .unwrap_or(vance_color::ColorSpace::LinearRec709)
    }

    /// Build the color-pipeline params for a given media item, using
    /// the project's working space as the intermediate and sRGB as
    /// the output (matches the viewport's Rgba8UnormSrgb surface).
    /// Returns `None` when the result would route through an HDR /
    /// log transfer the built-in path can't handle yet — the OCIO
    /// shim (6.1b) will fill that in.
    fn pipeline_for_media(
        &self,
        media_id: MediaItemId,
    ) -> Option<vance_color::PipelineParams> {
        let project = self.current_project.as_ref()?;
        let media = project.media.get(&media_id)?;
        let input = media
            .input_color_space
            .unwrap_or(vance_color::ColorSpace::Rec709Limited);
        let pipeline = vance_color::Pipeline {
            input,
            working: project.working_color_space,
            output: vance_color::ColorSpace::Srgb,
        };
        pipeline.to_params()
    }

    /// Resolve a clip's media to either a file path (for video/image/
    /// audio) or a title spec (for synthetic clips). Returns the
    /// path-or-title plus the clip's display kind so callers can
    /// branch cheaply.
    fn media_for_active(
        &self,
        media_id: MediaItemId,
    ) -> Result<MediaResolved, EngineError> {
        let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
        let media = project
            .media
            .get(&media_id)
            .ok_or(EngineError::MediaNotFound)?;
        match media.kind {
            MediaKind::Title => Ok(MediaResolved::Title(
                media.title.clone().unwrap_or_default(),
            )),
            MediaKind::Video | MediaKind::Image | MediaKind::Audio => {
                Ok(MediaResolved::File(media.path.to_string_lossy().into_owned()))
            }
        }
    }

    pub fn decode_sequence_at(&mut self, timeline_seconds: f64) -> Result<i64, EngineError> {
        self.pause_for_edit();
        // If we're inside a transition window, render via the dual-
        // decoder blend path. Otherwise reset the mix factor so a
        // previous transition's residue doesn't bleed into the next
        // single-clip frame.
        if let Some(active_tr) = {
            let project = self.current_project.as_ref();
            let seq_id = self.current_sequence_id;
            match (project, seq_id) {
                (Some(p), Some(id)) => p
                    .sequence(id)
                    .and_then(|s| {
                        vance_timeline::transition_at(s, seconds_to_rt(timeline_seconds))
                    }),
                _ => None,
            }
        } {
            let seq = {
                let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
                let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
                project
                    .sequence(seq_id)
                    .ok_or(EngineError::SequenceNotFound)?
                    .clone()
            };
            return self.decode_transition_at(&seq, active_tr, timeline_seconds);
        }
        // Not in a transition window — make sure the renderer's mix
        // factor is 0 so the next frame is pure A.
        if let Some(rs) = self.render_state.as_ref() {
            let mut rs = rs.lock();
            rs.renderer.set_mix_factor(0.0);
        }
        let (
            resolved,
            source_pts_seconds,
            anchor_timeline_in,
            anchor_source_in,
            anchor_opacity,
            anchor_clip_id,
        ) = {
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
            let resolved = self.media_for_active(active.media)?;
            let (tl_in, src_in) = clip_anchor_seconds(seq, active.clip_id)
                .unwrap_or((0.0, rt_to_seconds(active.source_pts)));
            // Clip-relative time for keyframe eval at this seek.
            let entry_clip_t = rt_to_seconds(active.source_pts) - src_in;
            let op = clip_opacity_at(seq, active.clip_id, entry_clip_t);
            (
                resolved,
                rt_to_seconds(active.source_pts),
                tl_in,
                src_in,
                op,
                active.clip_id,
            )
        };
        // For title clips: render procedurally and bypass the decoder.
        if let MediaResolved::Title(params) = resolved {
            return self.decode_title_scrub(
                params,
                anchor_clip_id,
                anchor_timeline_in,
                anchor_source_in,
                source_pts_seconds,
                anchor_opacity,
            );
        }
        let media_path = match resolved {
            MediaResolved::File(p) => p,
            MediaResolved::Title(_) => unreachable!("title handled above"),
        };
        // Resolve and push the color pipeline for this clip's media
        // before opening the file — render state stays consistent if
        // open_file fails later.
        let pipeline_params = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
            let media_id = seq
                .tracks
                .iter()
                .find_map(|t| {
                    t.clips
                        .iter()
                        .find(|c| c.id == anchor_clip_id)
                        .map(|c| c.media)
                })
                .ok_or(EngineError::Project("clip not found".into()))?;
            self.pipeline_for_media(media_id)
        };
        self.open_file(&media_path)?;
        let (xform_for_decode, crop_for_decode, color_for_decode, chroma_for_decode, luma_for_decode, grades_for_decode) = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
            let entry_clip_t = source_pts_seconds - anchor_source_in;
            (
                clip_transform_at(seq, anchor_clip_id, entry_clip_t),
                clip_crop_at(seq, anchor_clip_id, entry_clip_t),
                clip_brightness_contrast_at(seq, anchor_clip_id, entry_clip_t),
                clip_chroma_key_at(seq, anchor_clip_id, entry_clip_t),
                clip_luma_key_at(seq, anchor_clip_id, entry_clip_t),
                clip_grades_at(seq, anchor_clip_id, entry_clip_t),
            )
        };
        if let Some(rs) = self.render_state.as_ref() {
            let mut rs = rs.lock();
            rs.renderer.set_opacity(anchor_opacity);
            rs.renderer.set_transform(
                xform_for_decode.translate,
                xform_for_decode.scale,
                xform_for_decode.rotation_radians,
            );
            rs.renderer.set_brightness_contrast(color_for_decode.0, color_for_decode.1);
            rs.renderer.set_crop(
                crop_for_decode[0],
                crop_for_decode[1],
                crop_for_decode[2],
                crop_for_decode[3],
            );
            rs.renderer.set_chroma_key(
                chroma_for_decode.0,
                chroma_for_decode.1,
                chroma_for_decode.2,
                chroma_for_decode.3,
            );
            rs.renderer.set_luma_key(
                luma_for_decode.0,
                luma_for_decode.1,
                luma_for_decode.2,
                luma_for_decode.3,
            );
            rs.renderer.set_grades(&grades_for_decode);
            if let Some(p) = pipeline_params {
                rs.renderer.set_pipeline(p);
            }
        }
        self.apply_lut_for_clip(anchor_clip_id);
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
            opacity: anchor_opacity,
        });
        let pts = self.decode_and_render_at(target_pts)?;
        // Park the engine's "where am I" pointer at the just-rendered
        // frame so a subsequent Play resumes from this click, not from
        // whatever was the last *playback* stop position.
        self.last_resume_pts = Some(pts);
        Ok(pts)
    }

    /// Attach an effect to a clip. The caller picks `kind` (a string
    /// like "opacity" or "transform") + initial params; the engine
    /// allocates the EffectId and applies through the edit log.
    /// Returns the new effect's UUID.
    pub fn add_effect_to_clip(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        kind: String,
        params: std::collections::BTreeMap<String, EffectValue>,
    ) -> Result<String, EngineError> {
        self.pause_for_edit();
        let mut effect = Effect::new(kind);
        effect.params = params;
        let id = effect.id.to_string();
        self.apply_edit_op(EditOp::AddEffectToClip {
            track_id,
            clip_id,
            effect,
        })?;
        Ok(id)
    }

    /// Remove an effect from a clip by ID. Undoable.
    pub fn remove_effect_from_clip(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        effect_id: EffectId,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::RemoveEffectFromClip {
            track_id,
            clip_id,
            effect_id,
        })
    }

    /// Add a cross-dissolve transition at the cut following
    /// `after_clip_id`. Returns the new transition's UUID as a string
    /// for the IPC layer. Recorded in the edit log.
    pub fn add_transition_at_cut(
        &mut self,
        track_id: TrackId,
        after_clip_id: vance_timeline::ClipId,
        kind: vance_timeline::TransitionKind,
        duration_seconds: f64,
    ) -> Result<String, EngineError> {
        self.pause_for_edit();
        let duration = seconds_to_rt(duration_seconds.max(0.0));
        let transition_id = vance_timeline::TransitionId::new();
        self.apply_edit_op(EditOp::AddTransitionAtCut {
            track_id,
            transition_id,
            after_clip_id,
            kind,
            duration,
        })?;
        Ok(transition_id.to_string())
    }

    /// Remove a transition by ID. Undoable.
    pub fn remove_transition(
        &mut self,
        track_id: TrackId,
        transition_id: vance_timeline::TransitionId,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::RemoveTransition {
            track_id,
            transition_id,
        })
    }

    /// Update one parameter on an effect (e.g. opacity's `value`).
    pub fn update_effect_param(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        effect_id: EffectId,
        key: String,
        value: EffectValue,
    ) -> Result<(), EngineError> {
        // Allow live updates while playing? For now, refuse — same as
        // other clip ops — so we don't fight the playback thread.
        self.pause_for_edit();
        self.apply_edit_op(EditOp::UpdateEffectParam {
            track_id,
            clip_id,
            effect_id,
            key,
            value,
        })
    }

    /// Update several effect params in one undoable step. The color
    /// page commits a wheel (R/G/B + master) through this so a single
    /// drag is a single undo entry — not 3-4.
    pub fn update_effect_params(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        effect_id: EffectId,
        params: Vec<(String, EffectValue)>,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::UpdateEffectParams {
            track_id,
            clip_id,
            effect_id,
            params,
        })
    }

    /// Reorder an effect within a clip's chain (color-page node
    /// reorder). Undoable.
    pub fn move_effect_on_clip(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        effect_id: EffectId,
        new_index: usize,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::MoveEffectOnClip {
            track_id,
            clip_id,
            effect_id,
            new_index,
        })
    }

    /// Toggle an effect's `enabled` (bypass) flag. Undoable.
    pub fn set_effect_enabled(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        effect_id: EffectId,
        enabled: bool,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::SetEffectEnabled {
            track_id,
            clip_id,
            effect_id,
            enabled,
        })
    }

    /// Live, NON-undoable preview of one or more grade params mid-drag.
    /// Rebuilds the clip's grade chain with ALL `params` overridden
    /// together on `effect_id` (a wheel drag moves R/G/B + master at
    /// once, so a single-key preview would only show the last channel),
    /// pushes it to the renderer, and repaints. The UI commits exactly
    /// one `update_effect_params` on pointer-up so the EditLog gets a
    /// single step. No-ops if no project/clip is loaded.
    ///
    /// The UI seeks the Record viewport onto the selected clip when the
    /// Color page selection changes (see the color-seek `$effect`), so
    /// by the time a wheel is dragged the loaded frame already belongs
    /// to the clip being graded. This fast path therefore only rebuilds
    /// the grade chain and repaints — no decode.
    ///
    /// Note: this does NOT pause playback (it's a real-time fast path).
    /// Grading is a paused workflow — you judge color on a still frame —
    /// so if a clip is playing the per-frame playback push will overwrite
    /// the preview on the next frame. That's intentional, not a bug.
    pub fn preview_grade_params(
        &mut self,
        clip_id: vance_timeline::ClipId,
        effect_id: EffectId,
        params: Vec<(String, EffectValue)>,
    ) -> Result<(), EngineError> {
        let grades = {
            let project = match self.current_project.as_ref() {
                Some(p) => p,
                None => return Ok(()),
            };
            let seq_id = match self.current_sequence_id {
                Some(id) => id,
                None => return Ok(()),
            };
            let seq = match project.sequence(seq_id) {
                Some(s) => s,
                None => return Ok(()),
            };
            // Find the clip, clone its effects, override all params.
            let mut effects = match seq
                .tracks
                .iter()
                .find_map(|t| t.clips.iter().find(|c| c.id == clip_id))
            {
                Some(c) => c.effects.clone(),
                None => return Ok(()),
            };
            if let Some(e) = effects.iter_mut().find(|e| e.id == effect_id) {
                for (k, v) in &params {
                    e.params.insert(k.clone(), v.clone());
                }
            }
            // Grade params aren't usually keyframed during a live drag;
            // eval at t=0 is a fine preview. The pointer-up commit
            // re-renders at the real playhead time.
            eval_clip_grades(&effects, 0.0)
        };
        if let Some(rs) = self.render_state.as_ref() {
            rs.lock().renderer.set_grades(&grades);
        }
        if let (Some(rs), Some(vp)) = (self.render_state.as_ref(), self.viewport.as_ref()) {
            let rs = rs.clone();
            let vp = vp.clone();
            present_record_only(&rs, &vp)?;
        }
        Ok(())
    }

    /// Remove a clip from a track (leaves a gap on the timeline).
    /// Recorded in the edit log so Cmd+Z brings it back.
    pub fn remove_clip(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::RemoveClip { track_id, clip_id })
    }

    /// Trim a clip's in-point. `new_source_in_seconds` is the new
    /// visible start inside the source media; the clip's `timeline_in`
    /// shifts by the same delta so the right-hand content keeps its
    /// timeline position. Recorded in the edit log.
    pub fn trim_clip_in(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        new_source_in_seconds: f64,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::TrimIn {
            track_id,
            clip_id,
            new_source_in: seconds_to_rt(new_source_in_seconds.max(0.0)),
        })
    }

    /// Trim a clip's out-point. `new_source_out_seconds` is the new
    /// visible end inside the source media; `timeline_in` is unchanged
    /// and the clip's `timeline_out` slides to match the new length.
    pub fn trim_clip_out(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        new_source_out_seconds: f64,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::TrimOut {
            track_id,
            clip_id,
            new_source_out: seconds_to_rt(new_source_out_seconds.max(0.0)),
        })
    }

    /// Move a clip to `new_timeline_seconds` on the same track via
    /// `ops::slide`. Rejects if the new position would overlap a
    /// neighboring clip on the track. Recorded in the edit log so
    /// undo restores the prior position.
    pub fn move_clip_to(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        new_timeline_seconds: f64,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        // Compute delta from the clip's current timeline_in.
        let current_in = self
            .current_sequence_ref()?
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .and_then(|t| t.clips.iter().find(|c| c.id == clip_id))
            .map(|c| c.timeline_in)
            .ok_or_else(|| EngineError::Project("clip not found on track".into()))?;
        let new_in = seconds_to_rt(new_timeline_seconds.max(0.0));
        let delta = vance_core::RationalTime(new_in.0 - current_in.0);
        self.apply_edit_op(EditOp::Slide {
            track_id,
            clip_id,
            delta,
        })
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
        self.pause_for_edit();
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

        // Sanity: track must exist on the current sequence.
        let _ = seq_id;
        let _ = project;

        // Apply through the edit log so it's undoable.
        self.apply_edit_op(EditOp::InsertClip {
            track_id,
            clip,
        })?;
        Ok(new_clip_id.to_string())
    }

    // ---- Edit-log plumbing --------------------------------------------------

    /// Borrow the current sequence from the project. Errors when no
    /// project is loaded or the current_sequence_id is stale.
    fn current_sequence_ref(&self) -> Result<&Sequence, EngineError> {
        let project = self
            .current_project
            .as_ref()
            .ok_or(EngineError::NoProject)?;
        let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
        project
            .sequence(seq_id)
            .map(|arc| arc.as_ref())
            .ok_or(EngineError::SequenceNotFound)
    }

    /// Apply `op` to the current sequence's edit log, then mirror the
    /// log's new `current()` Sequence back into `project.sequences`
    /// so reads (rendering, save, scene_plan) see the latest state.
    fn apply_edit_op(&mut self, op: EditOp) -> Result<(), EngineError> {
        let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
        let log = self
            .edit_logs
            .get_mut(&seq_id)
            .ok_or(EngineError::NoCurrentSequence)?;
        log.apply(op).map_err(|e| EngineError::Project(e.to_string()))?;
        let new_seq = log.current().clone();
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        if let Some(slot) = project.sequences.iter_mut().find(|s| s.id == seq_id) {
            *slot = Arc::new(new_seq);
        }
        Ok(())
    }

    /// Undo the most recent edit on the current sequence. Returns
    /// `false` if there's nothing to undo.
    pub fn undo(&mut self) -> Result<bool, EngineError> {
        self.pause_for_edit();
        let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
        let log = self
            .edit_logs
            .get_mut(&seq_id)
            .ok_or(EngineError::NoCurrentSequence)?;
        if log.undo().is_none() {
            return Ok(false);
        }
        let new_seq = log.current().clone();
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        if let Some(slot) = project.sequences.iter_mut().find(|s| s.id == seq_id) {
            *slot = Arc::new(new_seq);
        }
        Ok(true)
    }

    /// Redo the next edit on the current sequence. Returns `false`
    /// if there's nothing to redo.
    pub fn redo(&mut self) -> Result<bool, EngineError> {
        self.pause_for_edit();
        let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
        let log = self
            .edit_logs
            .get_mut(&seq_id)
            .ok_or(EngineError::NoCurrentSequence)?;
        if log.redo().is_none() {
            return Ok(false);
        }
        let new_seq = log.current().clone();
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        if let Some(slot) = project.sequences.iter_mut().find(|s| s.id == seq_id) {
            *slot = Arc::new(new_seq);
        }
        Ok(true)
    }

    /// True when there's anything to undo on the current sequence.
    pub fn can_undo(&self) -> bool {
        self.current_sequence_id
            .and_then(|id| self.edit_logs.get(&id))
            .is_some_and(|log| log.can_undo())
    }

    /// True when there's anything to redo on the current sequence.
    pub fn can_redo(&self) -> bool {
        self.current_sequence_id
            .and_then(|id| self.edit_logs.get(&id))
            .is_some_and(|log| log.can_redo())
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
                            effects: c
                                .effects
                                .iter()
                                .map(|e| EffectInfo {
                                    id: e.id.to_string(),
                                    kind: e.kind.clone(),
                                    enabled: e.enabled,
                                    params: e.params.clone(),
                                })
                                .collect(),
                        })
                        .collect(),
                    transitions: t
                        .transitions
                        .iter()
                        .map(|tr| TransitionInfo {
                            id: tr.id.to_string(),
                            kind: match tr.kind {
                                vance_timeline::TransitionKind::Dissolve => "dissolve".into(),
                            },
                            position_seconds: rt_to_seconds(tr.position),
                            duration_seconds: rt_to_seconds(tr.duration),
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

    /// Stop in-flight playback so an edit / seek / load can proceed.
    /// Returns the stopped PTS when it actually stopped something,
    /// `None` when there was no playback. Used by every edit op so
    /// the user doesn't have to remember to hit Stop first.
    fn pause_for_edit(&mut self) -> Option<i64> {
        if self.playback.is_some() {
            self.stop_playback().ok()
        } else {
            None
        }
    }

    /// Is playback currently running? Auto-reaps sessions whose scheduler
    /// thread has exited (e.g. natural end-of-stream) so the engine state
    /// matches reality and the decoder is reclaimed for single-shot use.
    ///
    /// Sequence-aware: if a sequence playback just hit EOS and there's
    /// another clip later on the timeline, we kick off the next clip
    /// here so playback feels continuous from the user's perspective.
    pub fn is_playing(&mut self) -> bool {
        let finished = self
            .playback
            .as_ref()
            .is_some_and(|s| s.handle.is_finished());
        if !finished {
            return self.playback.is_some();
        }

        // Snapshot the chaining flag and any explicit resume target
        // before stop_playback clears them.
        let chain = self.playing_sequence_active;
        let chain_resume = self
            .playback
            .as_ref()
            .map(|s| s.chain_resume_micros.load(Ordering::Acquire))
            .unwrap_or(i64::MIN);
        let _ = self.stop_playback();
        if !chain {
            return false;
        }

        // If the loop exited because it crossed *out* of a transition
        // window, it stashed the timeline microseconds it wants us to
        // resume at — there, `play_sequence` resolves to the B clip
        // and swaps the primary onto its media. Otherwise fall back
        // to the EOS-driven "next clip start" path.
        if chain_resume != i64::MIN {
            let resume_t = (chain_resume as f64) / 1_000_000.0;
            tracing::info!(
                resume_t,
                "playback: chaining to post-transition position",
            );
            if self.play_sequence(resume_t).is_ok() {
                return true;
            }
        } else if let Some(next_t) = self.next_clip_start_seconds() {
            tracing::info!(next_t, "playback: chaining to next clip");
            if self.play_sequence(next_t).is_ok() {
                return true;
            }
        }
        false
    }

    /// Smallest `timeline_in` strictly greater than the current
    /// playhead across every enabled video track on the active
    /// sequence. Returns `None` when there's nothing to chain to.
    fn next_clip_start_seconds(&self) -> Option<f64> {
        let project = self.current_project.as_ref()?;
        let seq_id = self.current_sequence_id?;
        let seq = project.sequence(seq_id)?;
        let current_t = self.current_timeline_seconds().unwrap_or(0.0);
        let mut best: Option<f64> = None;
        for track in seq.tracks.iter() {
            if track.kind != TrackKind::Video || !track.enabled {
                continue;
            }
            for c in track.clips.iter() {
                if !c.enabled {
                    continue;
                }
                let t = rt_to_seconds(c.timeline_in);
                // Strictly greater than current_t (with epsilon) so we
                // never re-pick the clip that just finished.
                if t > current_t + 1e-2 {
                    best = match best {
                        Some(b) if b <= t => Some(b),
                        _ => Some(t),
                    };
                }
            }
        }
        best
    }

    /// Last presented PTS from the most recent finished playback session,
    /// or `None` if no session has run since the last `start_playback`.
    pub fn last_resume_pts(&self) -> Option<i64> {
        self.last_resume_pts
    }
}

/// Context passed to the playback loop so it can resolve transitions
/// against the live sequence without touching engine state from
/// another thread. None for single-file (Open File…) playback, where
/// the loop has nothing to look up.
pub struct SequencePlaybackContext {
    /// Snapshot of the current sequence. The loop only reads it (to
    /// query `transition_at`) so a clone is fine and avoids any
    /// cross-thread locking on the engine.
    pub sequence: Sequence,
    /// Resolves `MediaItemId` (from `ActiveTransition.to.media`) to a
    /// filesystem path the secondary decoder can open. Title media
    /// has no path; its `MediaItemId` instead lands in `title_params`.
    pub media_paths: std::collections::HashMap<MediaItemId, PathBuf>,
    /// Title parameters for synthetic (no-file) media items. When a
    /// transition's B-side media is in here, the loop renders the
    /// title frame inline instead of using the secondary decoder.
    pub title_params: std::collections::HashMap<MediaItemId, TitleParams>,
    /// Path to the decoder binary used to spawn the secondary sidecar
    /// lazily on first transition entry.
    pub decoder_bin: PathBuf,
    /// `(timeline_in_seconds, source_in_seconds)` of the clip whose
    /// stream is feeding the *primary* decoder. Used to convert each
    /// primary frame's stream-seconds into a timeline-seconds for
    /// `transition_at` queries.
    pub anchor_timeline_in: f64,
    pub anchor_source_in: f64,
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
/// What the playback loop is driving on the primary side.
/// Cross-dissolves between titles and video need both variants to be
/// first-class, otherwise live playback would have to break out at
/// every title/video boundary.
pub enum PlaybackPrimary {
    /// A real video clip — frames stream from the FFmpeg sidecar.
    Decoder {
        /// The sidecar client. The loop calls `next_frame()` each iteration.
        client: DecoderClient,
        /// Stream info captured at open time, drives PTS→seconds math.
        info: StreamInfo,
        /// First PTS the stream was started at (logged + used by the
        /// loop's pre-arm consistency checks).
        from_pts: i64,
    },
    /// A synthetic title clip — frames are rasterized once on the
    /// host side, then re-presented at `fps` cadence until the clip's
    /// `duration_seconds` elapses (synthetic EOS).
    Title {
        /// Pre-rasterized YUV frame; the loop reuses it every tick.
        /// Its `pts` field is rewritten in place each iteration.
        frame: vance_decoder::FrameData,
        /// Playback frame rate.
        fps: f64,
        /// Total duration of the title clip on the timeline.
        duration_seconds: f64,
    },
}

impl PlaybackPrimary {
    /// `(time_base_num, time_base_den)` for converting the primary's
    /// PTS values back to seconds. Title primaries fake one at
    /// `(1, 1000)` so milliseconds round-trip cleanly.
    fn time_base(&self) -> (i64, i64) {
        match self {
            PlaybackPrimary::Decoder { info, .. } => info.time_base,
            PlaybackPrimary::Title { .. } => (1, 1000),
        }
    }

    /// Initial PTS — 0 for synthetic, the stream's chosen start
    /// otherwise. Logged only; the loop's own per-frame timing
    /// derives from the actual frame PTS.
    fn from_pts(&self) -> i64 {
        match self {
            PlaybackPrimary::Decoder { from_pts, .. } => *from_pts,
            PlaybackPrimary::Title { .. } => 0,
        }
    }

    /// On thread exit, the engine wants the DecoderClient back so it
    /// can reuse the open file for single-shot decodes. Title
    /// primaries have no DecoderClient — the engine's slot stays as
    /// it was before playback.
    fn into_decoder(self) -> Option<DecoderClient> {
        match self {
            PlaybackPrimary::Decoder { client, .. } => Some(client),
            PlaybackPrimary::Title { .. } => None,
        }
    }
}

/// What `playback_loop` returns when it exits — bundles the reclaimed
/// primary + secondary decoders for the engine to put back into its
/// own slots.
pub struct PlaybackLoopOutput {
    /// The primary decoder, when one was in use. `None` after title
    /// playback so the engine's `decoder` slot is left untouched.
    pub primary_decoder: Option<DecoderClient>,
    /// Warm secondary used during dissolves, plus its currently-open
    /// path. Both `None` when no transition was crossed.
    pub secondary_decoder: Option<DecoderClient>,
    pub secondary_media_path: Option<String>,
}

/// (when present) or wall clock (audio missing/failed).
///
/// Returns a `PlaybackLoopOutput` so the engine can reclaim both
/// decoders (primary if it was a real video, plus the secondary warm
/// from any dissolves crossed during this session).
#[allow(clippy::too_many_arguments)]
fn playback_loop(
    mut primary: PlaybackPrimary,
    initial_bounds: ViewportBounds,
    viewport: Arc<Viewport>,
    render_state: Arc<Mutex<RenderState>>,
    stop: Arc<AtomicBool>,
    audio: Option<AudioSync>,
    last_pts: Arc<std::sync::atomic::AtomicI64>,
    ended_at_eos: Arc<AtomicBool>,
    // The loop writes a timeline-position-in-microseconds here when
    // it wants the engine to restart playback at a specific time
    // (used at transition exit to swap the primary onto B's media).
    // `i64::MIN` (the initial value) means "no override; chain to
    // the next clip start instead".
    chain_resume_micros: Arc<std::sync::atomic::AtomicI64>,
    // Effects from the active clip — evaluated per frame so keyframed
    // opacity / transform animate during continuous playback. Empty
    // when the clip has no effects (no-ops then).
    clip_effects: Vec<vance_timeline::Effect>,
    // Source-domain seconds where this clip's visible range starts
    // (i.e. the clip's `source_in`). Used to convert each frame's
    // stream PTS into a clip-relative time for keyframe eval.
    clip_source_in_sec: f64,
    // Sequence-mode-only context for transition handling. `None` for
    // single-file Open File… playback (no transitions to query).
    seq_ctx: Option<SequencePlaybackContext>,
    // Secondary decoder reclaimed from the engine for cross-dissolves.
    // Returned by this function so the engine can hold onto it
    // between sessions and avoid re-spawning the sidecar.
    mut transition_decoder: Option<DecoderClient>,
    // Path currently open in `transition_decoder` (matches engine state
    // before playback). Used to skip a re-open when scrubbing /
    // playing through the same B media repeatedly.
    mut transition_media_path: Option<String>,
) -> PlaybackLoopOutput {
    // Stream-time-base → seconds. For Decoder primaries this is
    // their time_base; for Title primaries the synthetic 1/fps tick.
    let (tb_num, tb_den) = primary.time_base();
    let pts_to_sec = |pts: i64| -> f64 {
        if tb_den == 0 {
            0.0
        } else {
            (pts as f64) * (tb_num as f64) / (tb_den as f64)
        }
    };
    let from_pts = primary.from_pts();

    // Anchor wall clock to the moment we receive the FIRST frame, mapped
    // to its PTS. Subsequent frames' target time = anchor + (pts - first).
    tracing::info!(
        from_pts,
        master = if audio.is_some() {
            "audio"
        } else {
            "wall-clock"
        },
        seq_aware = seq_ctx.is_some(),
        "playback thread started"
    );
    // Wall-clock fallback anchor (only used when no audio).
    let mut wall_anchor: Option<(Instant, f64)> = None;
    let mut bounds = initial_bounds;
    let mut last_logged_drops = 0u64;
    let mut total_drops = 0u64;
    let mut frames_presented = 0u64;
    // Stream-info for whatever is currently open in `transition_decoder`.
    // None until the first transition entry opens (or reuses) the
    // secondary on a media file; cached so subsequent frames in the
    // same window can convert source-seconds → PTS without an extra
    // IPC call.
    let mut transition_stream_info: Option<StreamInfo> = None;
    // True while the playhead is inside an active transition window
    // and the secondary is streaming B-side frames. Flips back to
    // false (resetting the renderer's mix factor) when we leave.
    let mut in_transition = false;

    // Title-mode synthetic pacing. We tick the title's PTS forward by
    // `1000 / fps` ms per loop iteration, treating each tick as one
    // "frame" presentation. EOS hits when total_seconds >=
    // duration_seconds.
    let mut title_tick_ms: i64 = 0;

    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        // Borrow the frame to present this iteration. For Decoder we
        // pull from the sidecar; for Title we tick the pre-rendered
        // frame's PTS forward and present it again.
        let frame: vance_decoder::FrameData = match &mut primary {
            PlaybackPrimary::Decoder { client, .. } => match client.next_frame() {
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
            },
            PlaybackPrimary::Title {
                frame,
                fps,
                duration_seconds,
            } => {
                let tick_dt_ms = (1000.0 / fps.max(1.0)).round() as i64;
                frame.pts = title_tick_ms;
                let elapsed_sec = (title_tick_ms as f64) / 1000.0;
                title_tick_ms = title_tick_ms.saturating_add(tick_dt_ms);
                if elapsed_sec >= *duration_seconds {
                    tracing::info!(
                        elapsed_sec,
                        duration_seconds = *duration_seconds,
                        "playback: title duration reached"
                    );
                    ended_at_eos.store(true, Ordering::Release);
                    // Tell the chain to resume just past our
                    // timeline_out so scene_at picks up whatever
                    // clip starts at the same moment. The
                    // `next_clip_start_seconds` fallback uses strict
                    // `>` and would otherwise miss an exactly-abutting
                    // neighbor. (Skip if the transition-exit branch
                    // already parked a resume target.)
                    if chain_resume_micros.load(Ordering::Acquire) == i64::MIN {
                        if let Some(ctx) = seq_ctx.as_ref() {
                            let end_t = ctx.anchor_timeline_in
                                + (elapsed_sec - ctx.anchor_source_in);
                            let resume_micros =
                                (end_t * 1_000_000.0).round() as i64 + 1000;
                            chain_resume_micros.store(resume_micros, Ordering::Release);
                        }
                    }
                    break;
                }
                frame.clone()
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
        } else if wait_secs < -0.080 && audio.is_some() {
            // > ~2 frames late at 24fps; drop to keep the playhead
            // honest with the audio clock. In *wall-clock-master*
            // mode (no audio) there's no external clock to sync to —
            // dropping just because decode + IPC are slower than
            // real-time silently empties the user's view of the clip.
            // We accept slower-than-realtime playback there instead.
            total_drops += 1;
            if total_drops - last_logged_drops >= 30 {
                tracing::warn!(total_drops, "playback: dropped frames so far");
                last_logged_drops = total_drops;
            }
            continue;
        }

        // Per-frame effect eval — picks up keyframe animation for
        // opacity / transform / crop / color / keys. Skip the lock
        // dance when the clip has no effects.
        if !clip_effects.is_empty() {
            let clip_t = frame_sec - clip_source_in_sec;
            let op = eval_clip_opacity(&clip_effects, clip_t);
            let xf = eval_clip_transform(&clip_effects, clip_t);
            let crop = eval_clip_crop(&clip_effects, clip_t);
            let (br, ct) = eval_clip_brightness_contrast(&clip_effects, clip_t);
            let chroma = eval_clip_chroma_key(&clip_effects, clip_t);
            let luma = eval_clip_luma_key(&clip_effects, clip_t);
            let grades = eval_clip_grades(&clip_effects, clip_t);
            let mut rs = render_state.lock();
            rs.renderer.set_opacity(op);
            rs.renderer.set_transform(xf.translate, xf.scale, xf.rotation_radians);
            rs.renderer.set_brightness_contrast(br, ct);
            rs.renderer.set_crop(crop[0], crop[1], crop[2], crop[3]);
            rs.renderer.set_chroma_key(chroma.0, chroma.1, chroma.2, chroma.3);
            rs.renderer.set_luma_key(luma.0, luma.1, luma.2, luma.3);
            rs.renderer.set_grades(&grades);
        }

        // Transition handling — only relevant when this is sequence
        // playback (seq_ctx set). The primary decoder is on the A
        // side and streams as usual; the secondary is opened lazily
        // on the first frame inside a transition window and the B
        // frames it produces are blended on top via mix_factor.
        let mut active_b_frame: Option<vance_decoder::FrameData> = None;
        let mut active_mix: f32 = 0.0;
        if let Some(ctx) = seq_ctx.as_ref() {
            let timeline_sec = ctx.anchor_timeline_in + (frame_sec - ctx.anchor_source_in);
            let timeline_rt = seconds_to_rt(timeline_sec);
            if let Some(at) = vance_timeline::transition_at(&ctx.sequence, timeline_rt) {
                // First branch: if the B-side media is a title, render
                // it procedurally and skip the secondary decoder entirely.
                if let Some(b_title) = ctx.title_params.get(&at.to.media) {
                    match render_title_yuv420(b_title) {
                        Ok(mut f) => {
                            // Synthetic PTS for B — tied to the
                            // transition's mix so the engine logs are
                            // useful; not actually consumed downstream.
                            f.pts = (at.mix as f64 * 1_000_000.0) as i64;
                            active_b_frame = Some(f);
                            active_mix = at.mix;
                            in_transition = true;
                            let resume_micros =
                                (timeline_sec * 1_000_000.0).round() as i64;
                            chain_resume_micros.store(resume_micros, Ordering::Release);
                        }
                        Err(e) => {
                            tracing::warn!(?e, "transition: title B render failed");
                        }
                    }
                } else if let Some(b_path) = ctx.media_paths.get(&at.to.media) {
                    let b_path_str = b_path.to_string_lossy().into_owned();
                    // Spawn the secondary sidecar on first use.
                    if transition_decoder.is_none() {
                        match DecoderClient::spawn(&ctx.decoder_bin) {
                            Ok(c) => transition_decoder = Some(c),
                            Err(e) => {
                                tracing::warn!(
                                    ?e,
                                    "transition: failed to spawn secondary decoder",
                                );
                            }
                        }
                    }
                    // (Re)open the secondary if its current file
                    // doesn't match the B-side media. After re-open
                    // the cached stream info is stale; clear it.
                    if transition_media_path.as_deref() != Some(&b_path_str) {
                        if let Some(dec) = transition_decoder.as_mut() {
                            match dec.open(&b_path_str) {
                                Ok(info_b) => {
                                    transition_stream_info = Some(info_b);
                                    transition_media_path = Some(b_path_str.clone());
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        ?e,
                                        "transition: failed to open B media on secondary",
                                    );
                                }
                            }
                        }
                    }
                    // With both open + matched, decode the B frame at
                    // the source PTS the scene resolver picked. This
                    // is a seek per frame; for 1–2s dissolve windows
                    // at 24–30fps it stays well under the per-frame
                    // budget on modern hardware.
                    if let (Some(dec), Some(info_b)) = (
                        transition_decoder.as_mut(),
                        transition_stream_info.as_ref(),
                    ) {
                        let (b_tb_n, b_tb_d) = info_b.time_base;
                        let b_src_sec = rt_to_seconds(at.to.source_pts);
                        let b_pts = if b_tb_n != 0 {
                            (b_src_sec * (b_tb_d as f64) / (b_tb_n as f64)).round() as i64
                        } else {
                            0
                        };
                        match dec.decode_at(b_pts) {
                            Ok(f) => {
                                active_b_frame = Some(f);
                                active_mix = at.mix;
                                in_transition = true;
                                // Park the resume target at the
                                // current timeline position so that
                                // *any* exit from this frame onward
                                // (transition-leave OR mid-window
                                // EOS on A) chains onto B's media
                                // rather than skipping to the next
                                // clip after B.
                                let resume_micros =
                                    (timeline_sec * 1_000_000.0).round() as i64;
                                chain_resume_micros
                                    .store(resume_micros, Ordering::Release);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    ?e,
                                    b_pts,
                                    "transition: B decode_at failed",
                                );
                            }
                        }
                    }
                }
            } else if in_transition {
                // Left the window — the primary is still on A, but
                // the timeline says B should now be the source. Tell
                // the engine to resume at this timeline position so
                // the chain logic swaps the primary onto B. Drop the
                // mix factor and break so `stop_playback` joins us.
                {
                    let mut rs = render_state.lock();
                    rs.renderer.set_mix_factor(0.0);
                }
                let resume_micros = (timeline_sec * 1_000_000.0).round() as i64;
                chain_resume_micros.store(resume_micros, Ordering::Release);
                tracing::info!(
                    timeline_sec,
                    "playback: leaving transition window — handing off to chain",
                );
                last_pts.store(frame.pts, Ordering::Release);
                break;
            }
        }

        // Push the B frame + mix factor to the renderer, then do the
        // normal A-side upload + render. Holding the lock once for
        // both avoids racing with a concurrent scrub. When seq_ctx
        // is set we always touch the mix factor each frame — that
        // way a B decode failure mid-window can't leave a stale
        // mix > 0 baked in from the previous frame.
        if seq_ctx.is_some() {
            let mut rs = render_state.lock();
            if let Some(b_frame) = active_b_frame.as_ref() {
                if let Err(e) = rs.renderer.upload_frame_b(b_frame) {
                    tracing::warn!(?e, "transition: upload B failed");
                }
                rs.renderer.set_mix_factor(active_mix);
            } else {
                rs.renderer.set_mix_factor(0.0);
            }
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

    // Stop the primary's stream (decoder only — title primaries have
    // nothing to stop) and return whatever we own back to the engine.
    if let PlaybackPrimary::Decoder { client, .. } = &mut primary {
        let _ = client.stop_stream();
    }
    PlaybackLoopOutput {
        primary_decoder: primary.into_decoder(),
        secondary_decoder: transition_decoder,
        secondary_media_path: transition_media_path,
    }
}

/// Source-monitor variant of `upload_and_render` — uses the Source
/// surface_config slot instead of the Record one.
fn upload_and_render_source(
    render_state: &Arc<Mutex<RenderState>>,
    viewport: &Arc<Viewport>,
    frame: &vance_decoder::FrameData,
    bounds: ViewportBounds,
) -> Result<(), EngineError> {
    let mut rs = render_state.lock();
    rs.renderer.upload_frame(frame)?;
    let surface = viewport.surface();
    if rs.source_surface_config.is_none() && !bounds.is_empty() {
        if let Some(cfg) = rs
            .renderer
            .recommended_surface_config(surface, bounds.w, bounds.h)
        {
            rs.renderer.configure_surface(surface, &cfg);
            rs.source_surface_config = Some(cfg);
        }
    }
    if rs.source_surface_config.is_none() {
        return Err(EngineError::SurfaceAcquire(
            "source viewport bounds not yet reported".into(),
        ));
    }
    let surface_tex = match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
        wgpu::CurrentSurfaceTexture::Outdated
        | wgpu::CurrentSurfaceTexture::Lost
        | wgpu::CurrentSurfaceTexture::Validation => {
            rs.source_surface_config = None;
            return Err(EngineError::SurfaceAcquire(
                "source surface outdated".into(),
            ));
        }
        wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
            return Err(EngineError::SurfaceAcquire(
                "source surface timeout/occluded".into(),
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

/// Re-present the Record viewport using whatever frame is already in
/// the renderer's textures, with the current uniforms. Used by the
/// live color-wheel preview fast-path: after `set_grades` updates the
/// uniform we just need to redraw — no decode. Returns `Ok(())` if
/// the surface isn't configured yet (nothing to repaint).
fn present_record_only(
    render_state: &Arc<Mutex<RenderState>>,
    viewport: &Arc<Viewport>,
) -> Result<(), EngineError> {
    let mut rs = render_state.lock();
    if rs.surface_config.is_none() {
        return Ok(());
    }
    let surface = viewport.surface();
    let surface_tex = match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
        wgpu::CurrentSurfaceTexture::Outdated
        | wgpu::CurrentSurfaceTexture::Lost
        | wgpu::CurrentSurfaceTexture::Validation => {
            rs.surface_config = None;
            return Err(EngineError::SurfaceAcquire("surface outdated".into()));
        }
        wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
            return Err(EngineError::SurfaceAcquire("surface timeout/occluded".into()));
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
