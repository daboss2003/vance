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
    /// GOAT-5 — a Motion Graphics Template instance, rasterized at the
    /// clip-relative time via `vance_render::render_mograph_yuv420`.
    /// Boxed because the instance embeds its template inline.
    MoGraph(Box<vance_mograph::MographInstance>),
    /// GOAT-12 — a Fusion node-graph composition, rasterized at the
    /// clip-relative time via `vance_render::render_fusion_yuv420`.
    /// Boxed because the comp embeds its whole node graph inline.
    Fusion(Box<vance_fusion::Comp>),
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

    /// OpenColorIO error (not enabled in build, bad config / colorspace).
    #[error("ocio: {0}")]
    Ocio(String),

    /// Scope computation error (Phase 6.12).
    #[error("scopes: {0}")]
    Scopes(String),
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
    /// Active OpenColorIO display transform (Phase 6.1b), or `null`.
    pub ocio_view: Option<OcioViewInfo>,
    /// Whether this build links OpenColorIO (so the UI can grey the
    /// OCIO controls when it doesn't).
    pub ocio_available: bool,
}

/// IPC-facing view of the project's OCIO display transform.
#[derive(serde::Serialize, Clone, Debug)]
pub struct OcioViewInfo {
    /// `.ocio` config path, or `null` for the `$OCIO` env config.
    pub config_path: Option<String>,
    /// Source colorspace / role name.
    pub src: String,
    /// Destination (display) colorspace / role name.
    pub dst: String,
    /// Baked cube side length.
    pub size: u32,
}

/// Summary returned from a `track_window` invocation (Phase 6.11).
#[derive(serde::Serialize, Clone, Copy, Debug)]
pub struct TrackSummary {
    /// Number of keyframes written (incl. the anchor at `start_pts`).
    pub frames_tracked: u32,
    /// Mean ZNCC score in `[-1, 1]` (1.0 = perfect match), averaged
    /// over EVERY frame in `frames_tracked` (the anchor's self-match
    /// counts as 1.0, so the metric covers all written keyframes).
    /// Low values mean the tracker probably drifted.
    pub mean_score: f32,
    /// Clip-relative time of the anchor (and first keyframe written).
    pub start_clip_seconds: f64,
    /// Clip-relative time of the last keyframe written. The destructive
    /// "replace keyframes in range" effect bounds are
    /// `[start_clip_seconds, end_clip_seconds]`.
    pub end_clip_seconds: f64,
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
    /// Phase 7.4: master bus gain in dB. UI's master fader binds to
    /// `set_master_gain_db`.
    pub master_gain_db: f32,
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
    /// Phase 7.3: mixer mute. Distinct from `enabled` (project on/off).
    pub mute: bool,
    /// Phase 7.3: solo flag. UI shows the "S" button highlighted.
    pub solo: bool,
    /// Phase 7.3: mixer gain in dB (0.0 = unity).
    pub gain_db: f32,
    /// Phase 7.4: time-varying gain automation lane in timeline-
    /// seconds. Empty = use the static `gain_db`. Each keyframe's
    /// `value` is in dB.
    pub gain_keyframes: Vec<vance_timeline::Keyframe>,
    /// Phase 7.6: stereo pan position, range `[-1, +1]`.
    pub pan: f32,
    /// Phase 7.7: ordered audio effect insert chain. Each entry is
    /// serialised verbatim from the timeline crate's
    /// [`vance_timeline::AudioEffectSpec`] tagged enum.
    pub audio_effects: Vec<vance_timeline::AudioEffectSpec>,
}

/// One transition's worth of info for the UI.
#[derive(serde::Serialize, Clone, Debug)]
pub struct TransitionInfo {
    /// Transition's UUID.
    pub id: String,
    /// Kind: "dissolve" or "wasm:<plugin-id>".
    pub kind: String,
    /// Cut position in seconds (the center of the transition window).
    pub position_seconds: f64,
    /// Duration of the transition window in seconds.
    pub duration_seconds: f64,
    /// Plugin param overrides (WASM transitions); empty for dissolve.
    pub params: std::collections::BTreeMap<String, vance_timeline::EffectValue>,
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
    /// Phase 7.3: per-clip mixer gain in dB (0.0 = unity).
    pub gain_db: f32,
    /// Phase 7.4: time-varying gain automation lane in CLIP-RELATIVE
    /// seconds. Empty = use the static `gain_db`.
    pub gain_keyframes: Vec<vance_timeline::Keyframe>,
    /// Phase 7.4 M3: optional fade-in. `None` = no fade-in.
    pub fade_in: Option<vance_timeline::Fade>,
    /// Phase 7.4 M3: optional fade-out. `None` = no fade-out.
    pub fade_out: Option<vance_timeline::Fade>,
    /// Phase 7.6: per-clip stereo pan position, range `[-1, +1]`.
    pub pan: f32,
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

/// Current wall-clock time as Unix seconds (UTC), for review-comment
/// timestamps. `0` if the clock is before the epoch (never, in practice).
fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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

/// GOAT-9 — smart-reframe transform for one frame. Given the subject at
/// normalized source UV `subj` and a target aspect (`tgt_w:tgt_h`) plus an
/// extra `zoom` (≥1), returns the `(translate, scale)` for a "transform"
/// effect that fills a target-aspect output with a subject-centered,
/// undistorted window of the source. The renderer maps output UV → source
/// UV as `source = (out - translate - 0.5)/scale + 0.5`, and its UV is
/// aspect-agnostic, so the per-axis scale below makes the chosen source
/// sub-window's pixel aspect equal the (target) output aspect — no stretch
/// when exported at the target resolution. The subject is clamped so the
/// window never runs past a source edge (no black bars).
fn reframe_transform(
    subj: (f32, f32),
    src_w: u32,
    src_h: u32,
    tgt_w: u32,
    tgt_h: u32,
    zoom: f32,
) -> ([f32; 2], [f32; 2]) {
    let ar_s = (src_w.max(1) as f32) / (src_h.max(1) as f32);
    let ar_t = (tgt_w.max(1) as f32) / (tgt_h.max(1) as f32);
    let (mut sx, mut sy) = if ar_t <= ar_s {
        // Target narrower than source → full height, zoom X to a slice.
        (ar_s / ar_t, 1.0)
    } else {
        // Target wider → full width, zoom Y.
        (1.0, ar_t / ar_s)
    };
    let z = zoom.max(1.0);
    sx *= z;
    sy *= z;
    // Half-window in source-UV; clamp the subject so the window stays in-frame.
    let hx = (0.5 / sx).min(0.5);
    let hy = (0.5 / sy).min(0.5);
    let cx = subj.0.clamp(hx, 1.0 - hx);
    let cy = subj.1.clamp(hy, 1.0 - hy);
    let tx = -sx * (cx - 0.5);
    let ty = -sy * (cy - 0.5);
    ([tx, ty], [sx, sy])
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
/// Read the raw keyframe vector for a param key. A plain `Number(n)` is
/// returned as a single `t=0` keyframe so a constant value survives the
/// "merge with tracked keyframes" path (Phase 6.11). Anything else
/// (Bool/String/absent) returns an empty Vec.
fn read_keyframes(
    params: &std::collections::BTreeMap<String, EffectValue>,
    key: &str,
) -> Vec<vance_timeline::Keyframe> {
    match params.get(key) {
        Some(EffectValue::Keyframes { keyframes: v }) => v.clone(),
        Some(EffectValue::Number(n)) => vec![vance_timeline::Keyframe {
            time_seconds: 0.0,
            value: *n,
            interp: vance_timeline::KeyframeInterp::Linear,
        }],
        _ => Vec::new(),
    }
}

/// Evaluate a keyframe list at clip-relative time `t`, returning
/// `default` when the list is empty (so the tracker has a fallback
/// anchor when no win_cx/cy keyframe has been authored yet).
fn eval_keyframes_or(kfs: &[vance_timeline::Keyframe], t: f64, default: f64) -> f64 {
    if kfs.is_empty() {
        return default;
    }
    // Reuse the existing linear-interp evaluator by wrapping in an
    // EffectValue::Keyframes (cheap clone of small Vec).
    EffectValue::Keyframes { keyframes: kfs.to_vec() }
        .eval_number(t)
        .unwrap_or(default)
}

fn eval_clip_grades(
    effects: &[vance_timeline::Effect],
    t_seconds: f64,
    matte_node: Option<EffectId>,
) -> Vec<vance_render::GradeNodeParams> {
    use vance_timeline::grade as gk;
    let num = |ps: &std::collections::BTreeMap<String, EffectValue>, k: &str, default: f32| -> f32 {
        ps.get(k)
            .and_then(|p| p.eval_number(t_seconds))
            .map(|v| v as f32)
            .unwrap_or(default)
    };
    // Curve control points live in an `EffectValue::Keyframes` whose
    // points are (input level, output level) pairs — static, NOT read
    // at `t_seconds` (the curve shape itself isn't keyframe-animated in
    // 6.8). Absent/other variants yield an empty list, which the baker
    // treats as the identity ramp.
    let curve = |ps: &std::collections::BTreeMap<String, EffectValue>, k: &str| -> Vec<vance_timeline::Keyframe> {
        match ps.get(k) {
            Some(EffectValue::Keyframes { keyframes: pts }) => pts.clone(),
            _ => Vec::new(),
        }
    };
    // The renderer evaluates at most MAX_GRADE_NODES per clip; extras are
    // dropped. The UI warns interactively, but a project loaded from disk
    // or authored by an external tool has no such guard — so surface the
    // truncation here too. Warn once per process (this runs per frame, so
    // an unconditional log would spam the console at the frame rate).
    let grade_count = effects.iter().filter(|e| e.kind == gk::GRADE_KIND).count();
    if grade_count > gk::MAX_GRADE_NODES {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                grade_count,
                max = gk::MAX_GRADE_NODES,
                "clip has more grade nodes than the renderer supports; extra nodes are ignored"
            );
        }
    }
    let mut out = Vec::new();
    for e in effects {
        if e.kind != gk::GRADE_KIND {
            continue;
        }
        if out.len() >= gk::MAX_GRADE_NODES {
            break;
        }
        let mode = match e.params.get(gk::KEY_MODE) {
            Some(EffectValue::String(s)) if s == gk::MODE_LOG => vance_render::GradeMode::Log,
            Some(EffectValue::String(s)) if s == gk::MODE_CURVES => vance_render::GradeMode::Curves,
            _ => vance_render::GradeMode::Primary,
        };
        // Matte solo is a transient VIEWER state (never persisted): the
        // engine forces show_mask (and enable, so the mask computes) on
        // the one node the UI is inspecting. This avoids serializing a
        // "show mask" param that would reopen the project in matte view.
        let is_matte = matte_node == Some(e.id);
        let curves = if matches!(mode, vance_render::GradeMode::Curves) {
            Some(vance_render::CurveSet {
                master: curve(&e.params, gk::KEY_CURVE_MASTER),
                r: curve(&e.params, gk::KEY_CURVE_R),
                g: curve(&e.params, gk::KEY_CURVE_G),
                b: curve(&e.params, gk::KEY_CURVE_B),
                luma: curve(&e.params, gk::KEY_CURVE_LUMA),
                // HSL hue-wheel curves (6.8b). Empty = identity.
                hh: curve(&e.params, gk::KEY_CURVE_HUE_HUE),
                hs: curve(&e.params, gk::KEY_CURVE_HUE_SAT),
                hl: curve(&e.params, gk::KEY_CURVE_HUE_LUM),
                ss: curve(&e.params, gk::KEY_CURVE_SAT_SAT),
                ll: curve(&e.params, gk::KEY_CURVE_LUM_LUM),
            })
        } else {
            None
        };
        out.push(vance_render::GradeNodeParams {
            enabled: e.enabled,
            mode,
            curves,
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
            // HSL qualifier (6.9) — read for EVERY node regardless of
            // mode (the key is mode-independent). Wide-open defaults
            // (range/hi = 1.0) so an enabled-but-untuned qualifier keys
            // the whole frame rather than nothing.
            qual: vance_render::Qualifier {
                enabled: num(&e.params, gk::KEY_QUAL_ENABLE, 0.0) > 0.5 || is_matte,
                invert: num(&e.params, gk::KEY_QUAL_INVERT, 0.0) > 0.5,
                show_mask: is_matte,
                hue: [
                    num(&e.params, gk::KEY_QUAL_HUE_C, 0.0),
                    num(&e.params, gk::KEY_QUAL_HUE_RANGE, 1.0),
                    num(&e.params, gk::KEY_QUAL_HUE_SOFT, 0.05),
                ],
                sat: [
                    num(&e.params, gk::KEY_QUAL_SAT_LO, 0.0),
                    num(&e.params, gk::KEY_QUAL_SAT_HI, 1.0),
                    num(&e.params, gk::KEY_QUAL_SAT_SOFT, 0.05),
                ],
                luma: [
                    num(&e.params, gk::KEY_QUAL_LUMA_LO, 0.0),
                    num(&e.params, gk::KEY_QUAL_LUMA_HI, 1.0),
                    num(&e.params, gk::KEY_QUAL_LUMA_SOFT, 0.05),
                ],
            },
            // Power window (6.10) — keyframe-animatable; center is what
            // the 6.11 tracker drives. shape rounded + clamped so a
            // fractional interpolation can't read as off/garbage.
            window: vance_render::Window {
                shape: (num(&e.params, gk::KEY_WIN_SHAPE, 0.0).round() as i64).clamp(0, 2) as u8,
                center: [
                    num(&e.params, gk::KEY_WIN_CENTER_X, 0.5),
                    num(&e.params, gk::KEY_WIN_CENTER_Y, 0.5),
                ],
                size: [
                    num(&e.params, gk::KEY_WIN_SIZE_X, 0.25),
                    num(&e.params, gk::KEY_WIN_SIZE_Y, 0.25),
                ],
                rotation: num(&e.params, gk::KEY_WIN_ROTATION, 0.0),
                feather: num(&e.params, gk::KEY_WIN_FEATHER, 0.1),
                invert: num(&e.params, gk::KEY_WIN_INVERT, 0.0) > 0.5,
            },
        });
    }
    out
}

fn clip_grades_at(
    seq: &Sequence,
    clip_id: vance_timeline::ClipId,
    t_seconds: f64,
    matte_node: Option<EffectId>,
) -> Vec<vance_render::GradeNodeParams> {
    for track in seq.tracks.iter() {
        if let Some(clip) = track.clips.iter().find(|c| c.id == clip_id) {
            return eval_clip_grades(&clip.effects, t_seconds, matte_node);
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
    /// 24-char lowercase hex of the first 12 bytes of the media's
    /// content hash. Used by the peaks subsystem (Phase 7.1) to
    /// name the on-disk `.peaks` slot. Falls back to a stable
    /// path+mtime+length hash when no real content hash is recorded
    /// yet — same media always yields the same prefix.
    pub content_hash_prefix_hex: String,
    /// Whether the source has an audio stream (Phase 7.2). UI uses
    /// this to decide whether to enqueue peaks extraction and
    /// whether to render a waveform on a video clip's body.
    pub has_audio_track: bool,
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
                MediaKind::MoGraph => "mograph".into(),
                MediaKind::Fusion => "fusion".into(),
            },
            duration_seconds: rt_to_seconds(m.duration),
            frame_rate_num: fr_n,
            frame_rate_den: fr_d,
            input_color_space: m.input_color_space.map(color_space_to_str),
            content_hash_prefix_hex: media_item_hash_prefix_hex(m),
            has_audio_track: m.has_audio_track,
        }
    }
}

/// Resolve the 24-char content-hash prefix the peaks subsystem uses to
/// name `.peaks` slots. Prefers the media item's recorded
/// `content_hash` (BLAKE3) when present; falls back to a stable
/// path+mtime+length FNV-1a hash via `vance_app::peaks::hash_prefix_from_path`
/// when the indexer hasn't computed a real one yet.
fn media_item_hash_prefix_hex(m: &MediaItem) -> String {
    let prefix = if let Some(ch) = m.content_hash {
        let mut p = [0u8; 12];
        p.copy_from_slice(&ch[..12]);
        p
    } else {
        // Single metadata call — split into mtime + length together so
        // a race between two reads of the same file can't conjure a
        // prefix that mixes pre- and post-write state.
        let (mtime_nanos, len_bytes) = match std::fs::metadata(&m.path) {
            Ok(md) => {
                let mtime = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as i128)
                    .unwrap_or(0);
                (mtime, md.len())
            }
            Err(_) => (0i128, 0u64),
        };
        crate::peaks::hash_prefix_from_path(&m.path, mtime_nanos, len_bytes)
    };
    let mut s = String::with_capacity(24);
    for byte in &prefix {
        s.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
        s.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap());
    }
    s
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
            ocio_view: p.ocio_view.as_ref().map(|v| OcioViewInfo {
                config_path: v.config_path.as_ref().map(|c| c.to_string_lossy().into_owned()),
                src: v.src.clone(),
                dst: v.dst.clone(),
                size: v.size,
            }),
            ocio_available: vance_ocio::is_available(),
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

/// M2 — load-time entitlement gate (spec §6). Shared (via `Arc`) between the
/// engine and the accounts layer: the accounts layer keeps the two sets current
/// (which plugin ids are marketplace-gated, and which are currently entitled),
/// and the engine consults `allowed()` immediately before instantiating a plugin.
#[derive(Default)]
pub struct EntitlementGate {
    gated: parking_lot::RwLock<std::collections::HashSet<String>>,
    entitled: parking_lot::RwLock<std::collections::HashSet<String>>,
}

impl EntitlementGate {
    /// Replace the current view of gated + entitled plugin ids.
    pub fn set_state(
        &self,
        gated: std::collections::HashSet<String>,
        entitled: std::collections::HashSet<String>,
    ) {
        *self.gated.write() = gated;
        *self.entitled.write() = entitled;
    }

    /// A non-gated (side-loaded / free) plugin always loads; a gated one only
    /// while its entitlement is currently valid.
    pub fn allowed(&self, id: &str) -> bool {
        !self.gated.read().contains(id) || self.entitled.read().contains(id)
    }
}

#[derive(Default)]
pub struct Engine {
    /// M2 — load-time entitlement gate. `None` until the accounts layer installs
    /// one; side-loaded plugins are never gated (spec §6).
    pub(crate) entitlement_gate: Option<Arc<EntitlementGate>>,
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
    /// Phase 9.2 — frame cache used by scrub paths to skip the
    /// decoder when the same PTS is re-hit during jog.
    pub(crate) frame_cache: Option<Arc<vance_cache::FrameCache>>,
    /// Phase 9.3 — background prefetch worker. Owns its own decoder
    /// sidecar so the primary decoder isn't fighting for the open
    /// file handle.
    pub(crate) prefetch: Option<crate::prefetch::PrefetchWorker>,
    /// Phase 9.4 — proxy generation worker.
    pub(crate) proxy_worker: Option<crate::proxy::ProxyWorker>,
    /// Phase 10.3 — discovered WASM plugins (id → manifest + path).
    pub(crate) plugin_registry: vance_plugin_sdk::discovery::PluginRegistry,
    /// Phase 10.5 — lazily-instantiated WASM plugin instances, keyed
    /// by manifest id. A `WasmPlugin` owns a wasmtime store so it's
    /// kept warm across frames rather than re-loaded each call. The
    /// `SystemTime` is the source `.wasm` file's mtime at load — used
    /// to auto-reload when a plugin is edited in place between exports
    /// (without an explicit rescan).
    pub(crate) loaded_plugins: std::collections::HashMap<
        String,
        (vance_plugin_sdk::host::WasmPlugin, Option<std::time::SystemTime>),
    >,
    /// Phase 10 — transient hand-off from `decode_sequence_at` to
    /// `decode_and_render_at`: the active clip's effect chain + its
    /// clip-relative time, so the live preview can apply WASM effects
    /// (readback → process → blit) in the viewport. Taken (cleared) on
    /// each render.
    pub(crate) preview_wasm: Option<(Vec<vance_timeline::Effect>, f64)>,
    /// Last-applied viewport bounds (in physical pixels).
    last_bounds: ViewportBounds,
    /// Active playback, if any.
    playback: Option<PlaybackSession>,
    /// Audio playback backend (single-file AudioPlayer or multi-source
    /// AudioMixer per Phase 7.3). Both are owned by a dedicated
    /// thread because `cpal::Stream` is `!Send`.
    audio_owner: Option<AudioBackend>,
    /// Active scrub session: a persistent cpal output + a warm audio
    /// decoder, so a playhead drag renders sample-accurate grains without
    /// respawning sidecars. `None` outside a scrub.
    scrub: Option<ScrubSession>,
    /// Phase 7.3: snapshot-bus handle to the active [`AudioMixer`]
    /// during sequence playback. The AudioMixer itself stays on its
    /// owner thread (cpal::Stream is !Send); this Send+Sync handle
    /// lets the edit-log thread publish fresh snapshots when a
    /// mixer-affecting EditOp lands. `None` outside of mixer
    /// playback.
    audio_mixer_snapshot: Option<Arc<arc_swap::ArcSwap<vance_audio::MixerSnapshot>>>,
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

    /// Caches the last OCIO-baked display LUT keyed by its spec, so the
    /// per-clip-transition `apply_view_lut` doesn't re-run OCIO's CPU
    /// processor over the cube lattice every time. Invalidated whenever
    /// the project's `ocio_view` differs from the cached spec. Phase 6.1b.
    ocio_lut_cache: Option<(vance_timeline::OcioView, Arc<vance_color::Lut3d>)>,

    /// Whether the Color-page scopes panel is visible (Phase 6.12). When
    /// false, `compute_scopes` skips the GPU readback entirely — zero
    /// cost while scopes are hidden.
    scopes_enabled: bool,
    /// Last computed scope data, returned as a fallback while playing or
    /// when the render lock is momentarily contended (so the UI keeps the
    /// previous frame's scopes instead of flickering empty).
    last_scopes: Option<vance_scopes::ScopeData>,

    /// Grade node whose HSL-qualifier matte is soloed to the viewport
    /// (Phase 6.9). Transient VIEWER state — never serialized — so a
    /// project never reopens stuck in matte view. Applied during grade
    /// evaluation by forcing `show_mask` on the matching node.
    matte_node: Option<EffectId>,

    /// GOAT-5 — the Motion Graphics Template render engine (font cache +
    /// rasterizer). Built lazily on first mograph render so projects with
    /// no titles pay nothing (and no font scan happens at startup). Held
    /// behind `Arc` so the playback thread can share the same font cache.
    mograph_engine: std::cell::OnceCell<std::sync::Arc<vance_mograph::MographEngine>>,

    /// GOAT-12 — the Fusion node-graph compositor (font cache + rasterizer).
    /// Built lazily on first Fusion render (same rationale as
    /// `mograph_engine`); shared with the playback thread via `Arc`.
    fusion_compositor: std::cell::OnceCell<std::sync::Arc<vance_fusion::Compositor>>,

    /// HDR export — overrides the pipeline OUTPUT color space (normally the
    /// SDR `Srgb`) for the duration of an HDR render. Set by
    /// `render_sequence` on the HDR path and cleared when it returns, so
    /// preview always resolves to SDR. `None` = SDR output (the default).
    render_output_space_override: Option<vance_color::ColorSpace>,
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

/// Either the single-file [`AudioPlayer`] owner or the multi-source
/// [`vance_audio::AudioMixer`] owner. Phase 7.3 added the second
/// variant — the playback loop only needs an `AudioSync` clock from
/// either, so the variant lives at the boundary where the engine
/// constructs / tears down the backend.
/// Length of a scrub grain (the short slice of audio played at the
/// playhead while dragging). Short enough to feel responsive, long
/// enough to be audible.
const SCRUB_GRAIN_SECONDS: f64 = 0.08;

/// A live scrub session: the persistent output stream + a warm decoder
/// that grains are rendered through without respawning.
struct ScrubSession {
    output: vance_audio::scrub_output::ScrubOutput,
    audio: vance_audio::AudioClient,
}

/// Fade a grain's first/last few ms in/out so replacing grains during a
/// drag doesn't click. Operates on interleaved stereo.
fn apply_grain_window(grain: &mut [f32], sample_rate: u32) {
    let frames = grain.len() / 2;
    if frames == 0 {
        return;
    }
    // ~8 ms ramps, capped at a quarter of the grain so in/out don't overlap.
    let ramp = ((sample_rate as f64 * 0.008) as usize)
        .min(frames / 4)
        .max(1);
    for i in 0..ramp {
        let g = i as f32 / ramp as f32;
        grain[i * 2] *= g;
        grain[i * 2 + 1] *= g;
        let j = frames - 1 - i;
        grain[j * 2] *= g;
        grain[j * 2 + 1] *= g;
    }
}

pub enum AudioBackend {
    /// Single-file path (`AudioPlayer`). Used by Source-monitor
    /// preview and any single-clip workflow that doesn't need mixing.
    Single(AudioOwner),
    /// Multi-source mixer path (`vance_audio::AudioMixer`). Used by
    /// `play_sequence` so mute / solo / gain are audible live.
    Mixer(AudioMixerOwner),
}

impl AudioBackend {
    fn shutdown(self) {
        match self {
            AudioBackend::Single(owner) => {
                owner.stop.store(true, Ordering::Release);
                let _ = owner.handle.join();
            }
            AudioBackend::Mixer(owner) => owner.shutdown(),
        }
    }
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
        /// Phase 7.3: if true, route audio through the multi-source
        /// [`vance_audio::AudioMixer`] (used by `play_sequence` for
        /// real mute/solo/gain mixing). Single-clip Source-monitor
        /// preview stays on the existing single-file [`AudioPlayer`]
        /// path so we don't perturb its sync behavior.
        use_mixer: bool,
    },
    Title {
        params: TitleParams,
        fps: f64,
        duration_seconds: f64,
    },
    /// GOAT-5 — animated Motion Graphics Template; the loop re-rasterizes
    /// per tick at the elapsed time so the rig animates during playback.
    MoGraph {
        instance: Box<vance_mograph::MographInstance>,
        fps: f64,
        duration_seconds: f64,
        /// Clip-relative time playback resumed at (so animation continues
        /// from where the playhead was, not from the rig's start).
        start_offset_sec: f64,
    },
    /// GOAT-12 — animated Fusion composition; the loop re-rasterizes the
    /// node graph per tick at the elapsed time so it animates during
    /// playback.
    Fusion {
        comp: Box<vance_fusion::Comp>,
        fps: f64,
        duration_seconds: f64,
        /// Clip-relative time playback resumed at.
        start_offset_sec: f64,
    },
}

/// A synthetic (file-less) playback source — a title, mograph, or Fusion.
enum SyntheticPlayback {
    Title(TitleParams),
    MoGraph(Box<vance_mograph::MographInstance>),
    Fusion(Box<vance_fusion::Comp>),
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
        // Phase 9.2 — fast scrub path: hit the frame cache before
        // round-tripping to the decoder sidecar. Cache is lazily
        // built on first use; failures degrade to a direct decode.
        let frame_cache = self.ensure_frame_cache();
        let uri_owned = self
            .current_media_path
            .clone()
            .unwrap_or_else(|| String::from(""));
        let dec = self.decoder.as_mut().ok_or(EngineError::NoFileOpen)?;
        let (frame, _was_hit) = if !uri_owned.is_empty() {
            crate::cached_decode::cached_decode_at(dec, &frame_cache, &uri_owned, target_pts)
                .map_err(EngineError::from)?
        } else {
            let f = dec.decode_at(target_pts)?;
            (f, false)
        };
        let pts = frame.pts;
        // Phase 9.3 — fire-and-forget kick to the background prefetch
        // worker so the next jog/play hits the cache.
        self.kick_prefetch(pts);
        tracing::info!(
            requested_pts = target_pts,
            actual_pts = pts,
            w = frame.width,
            h = frame.height,
            bytes = frame.bytes.len(),
            cache_hit = _was_hit,
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
        // Phase 10 — if the active clip carries enabled WASM effects,
        // render via the CPU readback → WASM → blit path so the plugin
        // output shows in the viewport. Otherwise the normal GPU path.
        let preview_wasm = self.preview_wasm.take();
        match preview_wasm {
            Some((effects, clip_t))
                if effects
                    .iter()
                    .any(|e| e.enabled && e.kind.starts_with("wasm:")) =>
            {
                self.upload_render_with_wasm(
                    &render_state,
                    &viewport,
                    &frame,
                    last_bounds,
                    &effects,
                    clip_t,
                )?;
            }
            _ => {
                upload_and_render(&render_state, &viewport, &frame, last_bounds)?;
            }
        }
        Ok(pts)
    }

    /// Phase 10 — live-preview render that applies the clip's WASM
    /// effects: composite to an offscreen RGBA buffer (native effects
    /// already set on the renderer), run the WASM plugins over it on
    /// the CPU, then blit the result to the viewport surface.
    fn upload_render_with_wasm(
        &mut self,
        render_state: &Arc<Mutex<RenderState>>,
        viewport: &Arc<Viewport>,
        frame: &vance_decoder::FrameData,
        bounds: ViewportBounds,
        effects: &[vance_timeline::Effect],
        clip_t: f64,
    ) -> Result<(), EngineError> {
        if bounds.is_empty() {
            // No surface size yet → fall back to the normal path.
            return upload_and_render(render_state, viewport, frame, bounds);
        }
        let (w, h) = (bounds.w, bounds.h);
        // 1. Upload + offscreen composite (native effects already on
        //    the renderer's uniforms).
        let mut rgba = {
            let mut rs = render_state.lock();
            rs.renderer
                .upload_frame(frame)
                .map_err(|e| EngineError::Project(format!("preview upload: {e}")))?;
            rs.renderer
                .render_to_rgba_buffer(w, h)
                .map_err(|e| EngineError::Project(format!("preview readback: {e}")))?
        };
        // 2. CPU WASM effects (render_state unlocked), render-cached so
        //    a scrub-back to the same (frame, effects) is instant.
        self.apply_wasm_effects_cached(&mut rgba, w, h, effects, clip_t);
        // 3. Blit to the surface.
        self.present_rgba_to_surface(render_state, viewport, &rgba, w, h)
    }

    /// Blit a finished CPU RGBA frame to the viewport surface. Shared by
    /// the WASM-effect and WASM-transition preview paths.
    fn present_rgba_to_surface(
        &self,
        render_state: &Arc<Mutex<RenderState>>,
        viewport: &Arc<Viewport>,
        rgba: &[u8],
        w: u32,
        h: u32,
    ) -> Result<(), EngineError> {
        let mut rs = render_state.lock();
        let surface = viewport.surface();
        if rs.surface_config.is_none() {
            if let Some(cfg) = rs.renderer.recommended_surface_config(surface, w, h) {
                rs.renderer.configure_surface(surface, &cfg);
                rs.surface_config = Some(cfg);
            }
        }
        let Some(format) = rs.surface_config.as_ref().map(|c| c.format) else {
            return Err(EngineError::SurfaceAcquire(
                "viewport bounds not yet reported".into(),
            ));
        };
        let surface_tex = match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
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
        rs.renderer
            .present_rgba(rgba, w, h, &view, format)
            .map_err(|e| EngineError::Project(format!("preview blit: {e}")))?;
        surface_tex.present();
        Ok(())
    }

    /// Live-preview a WASM transition: render both clips to RGBA, let the
    /// plugin blend B into A, and blit the result. Mirrors the export
    /// path's WASM-transition branch. Falls back to the GPU dissolve
    /// preview if the bounds aren't known or the renders fail.
    fn decode_wasm_transition_at(
        &mut self,
        seq: &Sequence,
        active_tr: vance_timeline::ActiveTransition,
        plugin_id: &str,
    ) -> Result<i64, EngineError> {
        let bounds = self.last_bounds;
        if bounds.is_empty() {
            return self.decode_transition_at(seq, active_tr, 0.0);
        }
        let (w, h) = (bounds.w, bounds.h);
        let from_src_pts = rt_to_seconds(active_tr.from.source_pts);
        let to_src_pts = rt_to_seconds(active_tr.to.source_pts);
        let from_src_in = clip_anchor_seconds(seq, active_tr.from.clip_id)
            .map(|(_, s)| s)
            .unwrap_or(0.0);
        let to_src_in = clip_anchor_seconds(seq, active_tr.to.clip_id)
            .map(|(_, s)| s)
            .unwrap_or(0.0);

        let Some((mut a, pts_a)) = self.render_clip_to_rgba(
            active_tr.from.clip_id,
            active_tr.from.media,
            from_src_pts,
            from_src_in,
            w,
            h,
        ) else {
            return self.decode_transition_at(seq, active_tr, 0.0);
        };
        let Some((b, _)) = self.render_clip_to_rgba(
            active_tr.to.clip_id,
            active_tr.to.media,
            to_src_pts,
            to_src_in,
            w,
            h,
        ) else {
            return self.decode_transition_at(seq, active_tr, 0.0);
        };
        let to_clip_t = to_src_pts - to_src_in;
        self.apply_wasm_transition(&mut a, &b, w, h, plugin_id, &active_tr.params, active_tr.mix, to_clip_t);

        let viewport = self.viewport.as_ref().ok_or(EngineError::NoViewport)?.clone();
        let render_state = self.render_state.as_ref().ok_or(EngineError::NoViewport)?.clone();
        self.present_rgba_to_surface(&render_state, &viewport, &a, w, h)?;
        self.last_resume_pts = Some(pts_a);
        Ok(pts_a)
    }

    /// Begin streaming playback starting at `from_pts`. The scheduler
    /// thread takes ownership of the decoder for the duration of playback;
    /// other commands that need the decoder will return `PlaybackInProgress`
    /// until `stop_playback` is called.
    pub fn start_playback(&mut self, from_pts: i64) -> Result<(), EngineError> {
        tracing::info!(from_pts, "start_playback called");
        self.pause_for_edit();
        self.start_playback_inner(StartPlaybackKind::Video {
            from_pts,
            use_mixer: false,
        })
    }

    /// Phase 7.3: like `start_playback` but routes audio through the
    /// multi-source [`vance_audio::AudioMixer`] so the active
    /// sequence's mute/solo/gain settings are audible live. Used by
    /// `play_sequence` so users actually hear the M/S buttons they
    /// toggle in the track header.
    pub fn start_playback_mixed(&mut self, from_pts: i64) -> Result<(), EngineError> {
        tracing::info!(from_pts, "start_playback_mixed called");
        self.pause_for_edit();
        self.start_playback_inner(StartPlaybackKind::Video {
            from_pts,
            use_mixer: true,
        })
    }

    /// Begin a scrub session: stop any playback, spawn a warm audio
    /// decoder, and open a persistent scrub output stream. Idempotent —
    /// a second call while a session is live is a no-op.
    pub fn scrub_begin(&mut self) -> Result<(), EngineError> {
        if self.scrub.is_some() {
            return Ok(());
        }
        let _ = self.stop_playback();
        let output = vance_audio::scrub_output::ScrubOutput::start()
            .map_err(|e| EngineError::Project(format!("scrub output: {e}")))?;
        let bin = locate_audio_binary()?;
        let audio = vance_audio::AudioClient::spawn(&bin)
            .map_err(|e| EngineError::Project(format!("scrub decoder spawn: {e}")))?;
        self.scrub = Some(ScrubSession { output, audio });
        Ok(())
    }

    /// Render a short, sample-accurate, windowed grain of the mixed audio
    /// centered at `at_seconds` and play it on the scrub output. Replaces
    /// the previous grain, so the audio tracks the latest playhead
    /// position. Lazily begins a session if needed. Built-in DSP applies;
    /// out-of-process (CLAP/LV2) plugins are skipped for latency.
    ///
    /// Note: this decodes the grain inline while holding the engine lock
    /// (tens of ms — the warm decoder seeks + reads ~80 ms from the 1–2
    /// clips under the playhead). During a drag the only frequent command
    /// is `scrub_grain` itself (serialized regardless), so the practical
    /// impact is low. Moving the decode onto a dedicated scrub thread (so
    /// other commands never wait) is a future optimization; the UI
    /// coalesces grains to one per ~40 ms to bound the work.
    pub fn scrub_grain(&mut self, at_seconds: f64) -> Result<(), EngineError> {
        if self.scrub.is_none() {
            self.scrub_begin()?;
        }
        let sample_rate = self.scrub.as_ref().unwrap().output.sample_rate();
        // ~80 ms grain CENTERED on the playhead. Near t=0 we can't decode
        // before the start, so we clamp the rendered range to [0, ..] and
        // pre-pad the grain with that much silence — keeping the playhead
        // at the grain's center (the flat part of the fade window) instead
        // of letting the clamp shift the audible audio later.
        let half = SCRUB_GRAIN_SECONDS / 2.0;
        let raw_start = at_seconds - half;
        let start_s = raw_start.max(0.0);
        let end_s = at_seconds + half;
        let pad_seconds = start_s - raw_start; // 0 except near t=0

        // Render with the warm decoder + no plugin host (built-in DSP
        // only). Take the session out to satisfy the borrow checker while
        // calling &mut self, then put it back.
        let mut session = self.scrub.take().unwrap();
        let result = self.bounce_range_with(&mut session.audio, start_s, end_s, sample_rate, None);
        match result {
            Ok(grain) => {
                let pad_frames = (pad_seconds * sample_rate as f64).round() as usize;
                let mut grain = if pad_frames > 0 {
                    let mut padded = vec![0.0f32; pad_frames * 2];
                    padded.extend_from_slice(&grain);
                    padded
                } else {
                    grain
                };
                apply_grain_window(&mut grain, sample_rate);
                session.output.push_grain(grain);
                self.scrub = Some(session);
                Ok(())
            }
            Err(e) => {
                // Keep the session alive so the next grain can retry.
                self.scrub = Some(session);
                Err(e)
            }
        }
    }

    /// End the scrub session: stop the output stream + drop the warm
    /// decoder. Idempotent.
    pub fn scrub_end(&mut self) {
        if let Some(session) = self.scrub.take() {
            session.output.silence();
            let _ = session.audio.shutdown();
            // `output` drops here → stops the cpal stream + owner thread.
        }
    }

    /// Wraps the heavy lifting so `start_playback` and the title-mode
    /// entry point share thread setup, atomics, and join handling.
    fn start_playback_inner(
        &mut self,
        kind: StartPlaybackKind,
    ) -> Result<(), EngineError> {
        // Scrubbing and playback both drive the default output device;
        // they're mutually exclusive. Tear down any scrub session before
        // starting playback (the reverse — scrub_begin → stop_playback —
        // is handled in scrub_begin) so two streams never fight, and the
        // scrub's warm decoder + output thread don't leak.
        self.scrub_end();
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
            StartPlaybackKind::Video {
                from_pts,
                use_mixer,
            } => {
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
                let (audio_owner, audio_sync) = if use_mixer {
                    match self.start_mixer_session(from_seconds) {
                        Ok((owner, sync)) => {
                            (Some(AudioBackend::Mixer(owner)), Some(sync))
                        }
                        Err(e) => {
                            tracing::warn!(
                                ?e,
                                "audio mixer: not started; falling back to single-clip audio"
                            );
                            match start_audio(&media_path, from_seconds) {
                                Ok((owner, sync)) => {
                                    (Some(AudioBackend::Single(owner)), Some(sync))
                                }
                                Err(e2) => {
                                    tracing::warn!(?e2, "audio: not started either; wall-clock");
                                    (None, None)
                                }
                            }
                        }
                    }
                } else {
                    match start_audio(&media_path, from_seconds) {
                        Ok((owner, sync)) => (Some(AudioBackend::Single(owner)), Some(sync)),
                        Err(e) => {
                            tracing::warn!(?e, "audio: not started; video will use wall-clock");
                            (None, None)
                        }
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
            StartPlaybackKind::MoGraph {
                instance,
                fps,
                duration_seconds,
                start_offset_sec,
            } => {
                // GOAT-5 — share the engine's font cache with the loop and
                // re-rasterize per tick (mograph is silent in v1, like
                // titles). The first frame is at the resume offset.
                let engine = self.mograph_engine();
                let frame = vance_render::render_mograph_yuv420(
                    &engine,
                    &instance,
                    start_offset_sec,
                    fps,
                    duration_seconds,
                );
                (
                    PlaybackPrimary::MoGraph {
                        engine,
                        instance,
                        frame,
                        fps,
                        duration_seconds,
                        start_offset_sec,
                    },
                    None,
                    None,
                    0,
                )
            }
            StartPlaybackKind::Fusion {
                comp,
                fps,
                duration_seconds,
                start_offset_sec,
            } => {
                // GOAT-12 — share the compositor's font cache with the loop
                // and re-rasterize the node graph per tick (Fusion is silent
                // in v1, like titles). The first frame is at the resume offset.
                let compositor = self.fusion_compositor();
                let frame = vance_render::render_fusion_yuv420(
                    &compositor,
                    &comp,
                    start_offset_sec,
                    &std::collections::HashMap::new(),
                );
                (
                    PlaybackPrimary::Fusion {
                        compositor,
                        comp,
                        frame,
                        fps,
                        duration_seconds,
                        start_offset_sec,
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
        // Stop the audio backend. AudioBackend::shutdown drops the
        // owned cpal stream + joins the puller (and the fold thread
        // in the mixer case).
        if let Some(backend) = self.audio_owner.take() {
            backend.shutdown();
        }
        // Phase 7.3: clear the snapshot bus so subsequent EditOps
        // don't try to publish to a torn-down mixer.
        self.end_mixer_session();
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
        // Drop the previous project's OCIO bake — the new project may
        // reference the same (config, src, dst) spec while the .ocio file
        // on disk differs, which would otherwise serve a stale LUT.
        self.ocio_lut_cache = None;
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
        self.ocio_lut_cache = None;
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
        // Auto-tag an HDR source's input color space from the stream's
        // transfer VUI so it grades as HDR without manual setup. SDR / unknown
        // stays `None` (the engine's Rec.709 default). The user can override.
        item.input_color_space = match stream.color_transfer {
            vance_decoder::protocol::ColorTransferHint::Pq => {
                Some(vance_color::ColorSpace::Rec2020Pq)
            }
            vance_decoder::protocol::ColorTransferHint::Hlg => {
                Some(vance_color::ColorSpace::Rec2020Hlg)
            }
            vance_decoder::protocol::ColorTransferHint::Unknown => item.input_color_space,
        };
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

    /// GOAT-8 — import an AUDIO-ONLY file (e.g. an isolated voice stem).
    /// Probes via the audio decoder sidecar (which, unlike the video
    /// decoder, doesn't require a video stream) and tags `MediaKind::Audio`.
    pub fn import_audio_file(&mut self, path: &str) -> Result<MediaItemInfo, EngineError> {
        if self.current_project.is_none() {
            return Err(EngineError::NoProject);
        }
        let bin = locate_audio_binary()?;
        let mut audio = vance_audio::AudioClient::spawn(&bin)
            .map_err(|e| EngineError::Project(format!("audio probe spawn: {e}")))?;
        let info = audio
            .open(path.to_string())
            .map_err(|e| EngineError::Project(format!("open audio: {e}")))?;
        let _ = audio.shutdown();
        // Duration from the stream's duration_pts × time_base, or a small
        // fallback so a probe-less stem still lands with a sane length.
        let (tb_n, tb_d) = info.time_base;
        let secs = match info.duration_pts {
            Some(pts) if tb_d != 0 => (pts as f64) * (tb_n as f64) / (tb_d as f64),
            _ => 1.0,
        };
        let duration = seconds_to_rt(secs.max(0.04));
        let display_name = std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string();
        let item = MediaItem::new(
            std::path::PathBuf::from(path),
            display_name,
            MediaKind::Audio,
            duration,
        );
        let id = item.id;
        let out = MediaItemInfo::from(&item);
        self.current_project
            .as_mut()
            .unwrap()
            .media
            .insert(id, Arc::new(item));
        tracing::info!(id = %id, path = %path, "imported audio-only media");
        Ok(out)
    }

    /// GOAT-gated (generative) — register a generated still PNG as a held
    /// `Image` media item with a chosen on-timeline duration. The file is
    /// already at a stable app-owned path (the generator wrote it there), so
    /// no copy is needed; the decoder reads its true dimensions at play time.
    pub fn import_generated_image(
        &mut self,
        path: &str,
        duration_seconds: f64,
    ) -> Result<MediaItemInfo, EngineError> {
        if self.current_project.is_none() {
            return Err(EngineError::NoProject);
        }
        let duration = seconds_to_rt(duration_seconds.max(0.04));
        let display_name = std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("generated")
            .to_string();
        let item = MediaItem::new(
            std::path::PathBuf::from(path),
            display_name,
            MediaKind::Image,
            duration,
        );
        let id = item.id;
        let out = MediaItemInfo::from(&item);
        self.current_project
            .as_mut()
            .unwrap()
            .media
            .insert(id, Arc::new(item));
        tracing::info!(id = %id, path = %path, "imported generated image");
        Ok(out)
    }

    /// Update `MediaItem.has_audio_track` (Phase 7.2). Called from the
    /// Tauri layer's `import_media_file` wrapper after probing via
    /// `PeaksService::probe_audio`, and from the project-load
    /// backfill pass. Returns `Ok(())` if the id is unknown — backfill
    /// should not error on a stale id.
    pub fn set_media_has_audio_track(
        &mut self,
        media_id: MediaItemId,
        has_audio: bool,
    ) -> Result<(), EngineError> {
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        let Some(existing) = project.media.get(&media_id).cloned() else {
            return Ok(());
        };
        if existing.has_audio_track == has_audio {
            return Ok(());
        }
        let mut updated = (*existing).clone();
        updated.has_audio_track = has_audio;
        project.media.insert(media_id, Arc::new(updated));
        Ok(())
    }

    /// Snapshot every video MediaItem whose `has_audio_track` is still
    /// the post-deserialize default (`false`). Used by the project-load
    /// backfill pass to know which items need a probe. Returns
    /// `(media_id, path)` pairs the caller can hand to
    /// `PeaksService::probe_audio`. Empty when no project is loaded.
    pub fn list_video_media_needing_audio_probe(&self) -> Vec<(MediaItemId, PathBuf)> {
        let Some(project) = self.current_project.as_ref() else {
            return Vec::new();
        };
        project
            .media
            .iter()
            .filter_map(|(id, m)| {
                if m.kind == MediaKind::Video && !m.has_audio_track {
                    Some((*id, m.path.clone()))
                } else {
                    None
                }
            })
            .collect()
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
        if self.render_state.is_none() {
            return;
        }
        // OCIO display transform takes precedence over a plain file LUT —
        // both drive the single viewport display-LUT slot, and OCIO is the
        // more specific, color-managed choice when configured.
        let ocio_view = self
            .current_project
            .as_ref()
            .and_then(|p| p.ocio_view.clone());
        if let Some(view) = ocio_view {
            self.apply_ocio_view_lut(&view);
            return;
        }
        let rs = self.render_state.as_ref().unwrap();
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

    /// Bake `view`'s `src → dst` conversion into a 3D LUT via OpenColorIO
    /// and load it into the viewport display-LUT slot. Cached by spec so
    /// repeated `apply_view_lut` calls (every clip transition) don't
    /// re-run OCIO. On bake failure the slot is disabled and the error is
    /// logged — a missing/invalid config must not blank the viewport
    /// silently or wedge the engine. Phase 6.1b.
    fn apply_ocio_view_lut(&mut self, view: &vance_timeline::OcioView) {
        let Some(rs) = self.render_state.as_ref() else {
            return;
        };
        // Cache hit: re-upload the already-baked LUT, no OCIO call.
        if let Some((cached_spec, lut)) = self.ocio_lut_cache.as_ref() {
            if cached_spec == view {
                let mut rs = rs.lock();
                rs.renderer.upload_view_lut(lut);
                rs.renderer.set_view_lut_enable(1.0);
                return;
            }
        }
        let config = view.config_path.as_deref();
        match vance_ocio::bake_lut(config, &view.src, &view.dst, view.size) {
            Ok(lut) => {
                let arc = Arc::new(lut);
                self.ocio_lut_cache = Some((view.clone(), arc.clone()));
                let mut rs = rs.lock();
                rs.renderer.upload_view_lut(&arc);
                rs.renderer.set_view_lut_enable(1.0);
            }
            Err(e) => {
                tracing::warn!(?e, src = %view.src, dst = %view.dst, "ocio: bake failed");
                let mut rs = rs.lock();
                rs.renderer.set_view_lut_enable(0.0);
            }
        }
    }

    /// Set the project's OpenColorIO display transform (`src → dst`) and
    /// apply it to the viewport immediately. `config` is an `.ocio` path
    /// or `None` for the `$OCIO` env config. Returns an error (without
    /// changing state) when OCIO isn't compiled in or the bake fails, so
    /// the UI can surface it rather than silently no-op. Phase 6.1b.
    pub fn set_ocio_view_transform(
        &mut self,
        config: Option<&str>,
        src: &str,
        dst: &str,
        size: u32,
    ) -> Result<(), EngineError> {
        if !vance_ocio::is_available() {
            return Err(EngineError::Ocio(
                "OpenColorIO support is not enabled in this build".into(),
            ));
        }
        let view = vance_timeline::OcioView {
            config_path: config.filter(|s| !s.is_empty()).map(PathBuf::from),
            src: src.to_string(),
            dst: dst.to_string(),
            size,
        };
        // Validate by baking up front, so a bad colorspace fails the
        // command instead of silently disabling the slot later.
        vance_ocio::bake_lut(view.config_path.as_deref(), &view.src, &view.dst, view.size)
            .map_err(|e| EngineError::Ocio(e.to_string()))?;
        {
            let project = self
                .current_project
                .as_mut()
                .ok_or(EngineError::NoProject)?;
            project.ocio_view = Some(view);
        }
        self.apply_view_lut();
        Ok(())
    }

    /// Clear the project's OCIO display transform, reverting the viewport
    /// to the file view-LUT (or unfiltered). Phase 6.1b.
    pub fn clear_ocio_view_transform(&mut self) -> Result<(), EngineError> {
        {
            let project = self
                .current_project
                .as_mut()
                .ok_or(EngineError::NoProject)?;
            project.ocio_view = None;
        }
        self.ocio_lut_cache = None;
        self.apply_view_lut();
        Ok(())
    }

    /// List the colorspace names in an OCIO config (for UI pickers).
    /// `config` is an `.ocio` path or `None` for the `$OCIO` env config.
    /// Phase 6.1b.
    pub fn ocio_list_colorspaces(
        &self,
        config: Option<&str>,
    ) -> Result<Vec<String>, EngineError> {
        if !vance_ocio::is_available() {
            return Err(EngineError::Ocio(
                "OpenColorIO support is not enabled in this build".into(),
            ));
        }
        let path = config.filter(|s| !s.is_empty()).map(PathBuf::from);
        vance_ocio::list_colorspaces(path.as_deref()).map_err(|e| EngineError::Ocio(e.to_string()))
    }

    /// Solo a grade node's HSL-qualifier matte to the viewport, or clear
    /// it (`None`). Transient — not persisted. Re-evaluates `clip_id`'s
    /// grades with the matte override and repaints so the matte
    /// appears/disappears immediately (paused-workflow, like preview).
    /// Phase 6.9.
    pub fn set_matte_node(
        &mut self,
        clip_id: vance_timeline::ClipId,
        effect_id: Option<EffectId>,
    ) -> Result<(), EngineError> {
        self.matte_node = effect_id;
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
            clip_grades_at(seq, clip_id, 0.0, self.matte_node)
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

    /// Enable/disable scope computation (Phase 6.12). The UI sets this
    /// when the scopes panel becomes visible/hidden so `compute_scopes`
    /// is a no-op (no GPU readback) while scopes aren't shown.
    pub fn set_scopes_enabled(&mut self, on: bool) {
        self.scopes_enabled = on;
    }

    /// Compute video-scope data (waveform/parade/vectorscope/histogram)
    /// from the current frame at `width`×`height` analysis resolution.
    /// Phase 6.12.
    ///
    /// This is a PAUSED-workflow tool and must never stall playback:
    /// - returns the cached scopes (a no-op) while a playback session is
    ///   live or scopes are disabled — no lock attempt, no readback;
    /// - otherwise `try_lock`s the render state and, if it's momentarily
    ///   contended, returns the cache rather than blocking the scheduler.
    /// The GPU readback runs while holding the render lock; the (pure)
    /// accumulation runs AFTER the lock is released.
    ///
    /// 6.12b: live-during-playback scopes would require the playback
    /// scheduler to emit a downsampled copy on a background channel and
    /// ship it without ever blocking frame pacing — a separable feature,
    /// not a gap in this paused-workflow tool.
    pub fn compute_scopes(
        &mut self,
        cfg: vance_scopes::ScopeConfig,
        width: u32,
        height: u32,
        pre_view_lut: bool,
    ) -> Result<Option<vance_scopes::ScopeData>, EngineError> {
        if self.playback.is_some() || !self.scopes_enabled {
            return Ok(self.last_scopes.clone());
        }
        let Some(rs) = self.render_state.as_ref() else {
            return Ok(self.last_scopes.clone());
        };
        let rgba = {
            let Some(mut guard) = rs.try_lock() else {
                // Scheduler/other path holds the lock — never block it.
                return Ok(self.last_scopes.clone());
            };
            guard.renderer.render_scope_buffer(width, height, pre_view_lut)?
        };
        let data = vance_scopes::compute_all(&rgba, width, height, &cfg)
            .map_err(|e| EngineError::Scopes(e.to_string()))?;
        self.last_scopes = Some(data.clone());
        Ok(Some(data))
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

    /// GOAT-5 — built-in MoGraph template catalog as `(key, name)` pairs
    /// for the "New Title…" menu.
    pub fn mograph_catalog(&self) -> Vec<(String, String)> {
        vance_mograph::builtins::catalog()
            .into_iter()
            .map(|(k, n)| (k.to_string(), n))
            .collect()
    }

    /// GOAT-5 — instantiate a built-in MoGraph template (by catalog key)
    /// as a new synthetic media item. Returns its info for the browser.
    pub fn create_mograph_media(
        &mut self,
        template_key: String,
        display_name: Option<String>,
    ) -> Result<MediaItemInfo, EngineError> {
        if self.current_project.is_none() {
            return Err(EngineError::NoProject);
        }
        let template = vance_mograph::builtins::by_key(&template_key)
            .ok_or_else(|| EngineError::Project(format!("unknown template '{template_key}'")))?;
        let name = display_name.unwrap_or_else(|| template.meta.name.clone());
        let duration = seconds_to_rt(template.meta.default_duration_s.max(0.04));
        let inst = vance_mograph::MographInstance::new(template);
        let item = MediaItem::new_mograph(name, duration, inst);
        let id = item.id;
        let info = MediaItemInfo::from(&item);
        let project = self.current_project.as_mut().unwrap();
        project.media.insert(id, Arc::new(item));
        tracing::info!(id = %id, key = %template_key, "created mograph media");
        Ok(info)
    }

    /// GOAT-5 — the placed MoGraph instance for `media_id` (template +
    /// current control values), for the control-editor panel. `None` if
    /// the item isn't a mograph.
    pub fn mograph_instance(
        &self,
        media_id: MediaItemId,
    ) -> Option<vance_mograph::MographInstance> {
        self.current_project
            .as_ref()?
            .media
            .get(&media_id)?
            .mograph
            .clone()
    }

    /// GOAT-5 — set one control value on a mograph media item. Returns
    /// `true` if the value was accepted (known control, type-correct).
    pub fn set_mograph_value(
        &mut self,
        media_id: MediaItemId,
        control_id: String,
        value: vance_mograph::ControlValue,
    ) -> Result<bool, EngineError> {
        let project = self.current_project.as_mut().ok_or(EngineError::NoProject)?;
        let arc = project.media.get(&media_id).ok_or(EngineError::MediaNotFound)?;
        let mut item = (**arc).clone();
        let Some(inst) = item.mograph.as_mut() else {
            return Err(EngineError::Project("media is not a mograph".into()));
        };
        let ok = inst.set(&control_id, value);
        if ok {
            project.media.insert(media_id, Arc::new(item));
        }
        Ok(ok)
    }

    /// GOAT-12 — create a new Fusion composition media item seeded with a
    /// starter graph (title over a gradient background) at the given canvas
    /// size. Returns its info for the browser.
    pub fn create_fusion_media(
        &mut self,
        display_name: Option<String>,
        width: u32,
        height: u32,
        duration_seconds: f64,
    ) -> Result<MediaItemInfo, EngineError> {
        if self.current_project.is_none() {
            return Err(EngineError::NoProject);
        }
        let mut comp = vance_fusion::starter(width.max(2), height.max(2));
        comp.duration_s = duration_seconds.max(0.04);
        comp.fps = self.current_sequence_fps().unwrap_or(30.0);
        let name = display_name.unwrap_or_else(|| "Fusion Composition".to_string());
        let duration = seconds_to_rt(comp.duration_s);
        let item = MediaItem::new_fusion(name, duration, comp);
        let id = item.id;
        let info = MediaItemInfo::from(&item);
        let project = self.current_project.as_mut().unwrap();
        project.media.insert(id, Arc::new(item));
        tracing::info!(id = %id, "created fusion media");
        Ok(info)
    }

    /// GOAT-12 — the Fusion composition for `media_id` (the whole node
    /// graph), for the node editor. `None` if the item isn't a Fusion comp.
    pub fn fusion_comp(&self, media_id: MediaItemId) -> Option<vance_fusion::Comp> {
        self.current_project
            .as_ref()?
            .media
            .get(&media_id)?
            .fusion
            .clone()
    }

    /// GOAT-12 — replace the Fusion composition on a media item (the editor
    /// edits the graph and pushes the whole comp back). Errors if the item
    /// isn't a Fusion comp.
    pub fn set_fusion_comp(
        &mut self,
        media_id: MediaItemId,
        comp: vance_fusion::Comp,
    ) -> Result<(), EngineError> {
        let project = self.current_project.as_mut().ok_or(EngineError::NoProject)?;
        let arc = project.media.get(&media_id).ok_or(EngineError::MediaNotFound)?;
        let mut item = (**arc).clone();
        if item.fusion.is_none() {
            return Err(EngineError::Project("media is not a fusion comp".into()));
        }
        item.fusion = Some(comp);
        project.media.insert(media_id, Arc::new(item));
        Ok(())
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

    /// Look up a single media item's info. `None` when no project is
    /// loaded or the id isn't in the library. Used by the Phase 7.2
    /// import path to fetch the post-probe view of a freshly added
    /// item without re-listing the whole library.
    pub fn media_get_info(&self, media_id: MediaItemId) -> Option<MediaItemInfo> {
        self.current_project
            .as_ref()
            .and_then(|p| p.media.get(&media_id))
            .map(|arc| MediaItemInfo::from(arc.as_ref()))
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

        // For synthetic clips (title / mograph), take the synthetic-
        // playback branch and skip the decoder/audio/stream-info logic.
        let media_path = match resolved {
            MediaResolved::Title(params) => {
                return self.play_synthetic_clip(
                    SyntheticPlayback::Title(params),
                    anchor_clip_id,
                    anchor_timeline_in,
                    anchor_source_in,
                    source_pts_seconds,
                    anchor_opacity,
                    clip_duration_sec,
                );
            }
            MediaResolved::MoGraph(inst) => {
                return self.play_synthetic_clip(
                    SyntheticPlayback::MoGraph(inst),
                    anchor_clip_id,
                    anchor_timeline_in,
                    anchor_source_in,
                    source_pts_seconds,
                    anchor_opacity,
                    clip_duration_sec,
                );
            }
            MediaResolved::Fusion(comp) => {
                return self.play_synthetic_clip(
                    SyntheticPlayback::Fusion(comp),
                    anchor_clip_id,
                    anchor_timeline_in,
                    anchor_source_in,
                    source_pts_seconds,
                    anchor_opacity,
                    clip_duration_sec,
                );
            }
            MediaResolved::File(p) => p,
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
                self.matte_node,
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
        // Phase 7.3: sequence playback uses the multi-source mixer so
        // mute/solo/gain are live; the single-clip path remains for
        // Source-monitor preview.
        self.start_playback_mixed(from_pts)
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
        // B-side clip-relative time, for an animated mograph on the incoming
        // side (source_pts − its own source_in; a trimmed B has source_in>0).
        let (_, to_src_in) = clip_anchor_seconds(seq, active_tr.to.clip_id).unwrap_or((0.0, 0.0));
        let to_clip_t = rt_to_seconds(active_tr.to.source_pts) - to_src_in;
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
                MediaResolved::MoGraph(inst) => {
                    (self.render_mograph_frame(&inst, from_clip_t), 0, (1, 1000))
                }
                MediaResolved::Fusion(comp) => {
                    (self.render_fusion_frame(&comp, from_clip_t), 0, (1, 1000))
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
            MediaResolved::MoGraph(inst) => self.render_mograph_frame(&inst, to_clip_t),
            MediaResolved::Fusion(comp) => self.render_fusion_frame(&comp, to_clip_t),
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
            rs.renderer
                .set_grades(&clip_grades_at(seq, from_clip_id, from_clip_t, self.matte_node));
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
    fn play_synthetic_clip(
        &mut self,
        synthetic: SyntheticPlayback,
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
            rs.renderer.set_grades(&eval_clip_grades(&effects, 0.0, self.matte_node));
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
        let kind = match synthetic {
            SyntheticPlayback::Title(params) => StartPlaybackKind::Title {
                params,
                fps: 24.0,
                duration_seconds: remaining_dur,
            },
            SyntheticPlayback::MoGraph(instance) => StartPlaybackKind::MoGraph {
                instance,
                fps: self.current_sequence_fps().unwrap_or(30.0),
                duration_seconds: remaining_dur,
                start_offset_sec: entry_offset,
            },
            SyntheticPlayback::Fusion(comp) => StartPlaybackKind::Fusion {
                comp,
                fps: self.current_sequence_fps().unwrap_or(30.0),
                duration_seconds: remaining_dur,
                start_offset_sec: entry_offset,
            },
        };
        self.start_playback_inner(kind)
    }

    /// GOAT-5 — the lazily-built MoGraph render engine (cloneable `Arc`
    /// so the playback thread shares the font cache). The system font
    /// scan happens once, on first mograph render, so title-free projects
    /// pay nothing.
    fn mograph_engine(&self) -> std::sync::Arc<vance_mograph::MographEngine> {
        self.mograph_engine
            .get_or_init(|| std::sync::Arc::new(vance_mograph::MographEngine::new()))
            .clone()
    }

    /// Current sequence frame rate as fps, or `None` when no sequence.
    fn current_sequence_fps(&self) -> Option<f64> {
        let id = self.current_sequence_id?;
        let seq = self.current_project.as_ref()?.sequence(id)?;
        Some(seq.frame_rate.0.as_f64())
    }

    /// GOAT-5 — rasterize a MoGraph instance to a YUV frame at
    /// clip-relative `clip_t` seconds (so the rig animates as the playhead
    /// moves through the clip).
    fn render_mograph_frame(
        &self,
        inst: &vance_mograph::MographInstance,
        clip_t: f64,
    ) -> vance_render::FrameData {
        let fps = self.current_sequence_fps().unwrap_or(30.0);
        let duration = inst.template.meta.default_duration_s;
        let engine = self.mograph_engine();
        vance_render::render_mograph_yuv420(&engine, inst, clip_t.max(0.0), fps, duration)
    }

    /// GOAT-12 — the lazily-built Fusion compositor (cloneable `Arc` so the
    /// playback thread shares the font cache). The system font scan happens
    /// once, on first Fusion render.
    fn fusion_compositor(&self) -> std::sync::Arc<vance_fusion::Compositor> {
        self.fusion_compositor
            .get_or_init(|| std::sync::Arc::new(vance_fusion::Compositor::new()))
            .clone()
    }

    /// GOAT-12 — rasterize a Fusion comp to a YUV frame at clip-relative
    /// `clip_t` seconds (so the graph animates as the playhead moves). The
    /// standalone comp media takes no host inputs.
    fn render_fusion_frame(
        &self,
        comp: &vance_fusion::Comp,
        clip_t: f64,
    ) -> vance_render::FrameData {
        let compositor = self.fusion_compositor();
        vance_render::render_fusion_yuv420(
            &compositor,
            comp,
            clip_t.max(0.0),
            &std::collections::HashMap::new(),
        )
    }

    /// Single-shot title render — same role as `decode_and_render_at`
    /// but for synthetic clips. Rasterizes the title via
    /// `vance_render::render_title_yuv420`, applies the clip's effect
    /// chain (opacity/transform/crop/color), uploads + presents.
    /// Returns a synthetic PTS of 0; callers use timeline-seconds
    /// for the playhead instead of the source PTS.
    fn decode_title_scrub(
        &mut self,
        frame: vance_render::FrameData,
        clip_id: vance_timeline::ClipId,
        anchor_timeline_in: f64,
        anchor_source_in: f64,
        source_pts_seconds: f64,
        opacity: f32,
    ) -> Result<i64, EngineError> {
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
                clip_grades_at(seq, clip_id, clip_t, self.matte_node),
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

    /// Phase 8 (real export) — set ALL per-clip render uniforms
    /// (opacity, transform, brightness/contrast, crop, chroma/luma
    /// key, color grades, color pipeline, per-clip LUT) for `clip_id`
    /// at clip-relative time `entry_clip_t`. This is the single source
    /// of truth shared by the live preview (`decode_and_render_at`) and
    /// the offline export (`render_sequence`), so an exported frame
    /// renders identically to what the viewport shows.
    ///
    /// Call AFTER `upload_frame` and BEFORE `render_to`/
    /// `render_to_rgba_buffer`. Takes its own render-state lock(s);
    /// must not be called while a lock is held.
    fn apply_clip_render_params(
        &mut self,
        clip_id: vance_timeline::ClipId,
        media_id: MediaItemId,
        entry_clip_t: f64,
    ) {
        let matte = self.matte_node;
        // Color pipeline is resolved from the media's input color space.
        let pipeline_params = self.pipeline_for_media(media_id);
        let computed = {
            let project = match self.current_project.as_ref() {
                Some(p) => p,
                None => return,
            };
            let seq_id = match self.current_sequence_id {
                Some(id) => id,
                None => return,
            };
            let seq = match project.sequence(seq_id) {
                Some(s) => s,
                None => return,
            };
            (
                clip_opacity_at(seq, clip_id, entry_clip_t),
                clip_transform_at(seq, clip_id, entry_clip_t),
                clip_crop_at(seq, clip_id, entry_clip_t),
                clip_brightness_contrast_at(seq, clip_id, entry_clip_t),
                clip_chroma_key_at(seq, clip_id, entry_clip_t),
                clip_luma_key_at(seq, clip_id, entry_clip_t),
                clip_grades_at(seq, clip_id, entry_clip_t, matte),
            )
        };
        let (opacity, xform, crop, bc, chroma, luma, grades) = computed;
        if let Some(rs) = self.render_state.as_ref() {
            let mut rs = rs.lock();
            rs.renderer.set_opacity(opacity);
            rs.renderer
                .set_transform(xform.translate, xform.scale, xform.rotation_radians);
            rs.renderer.set_brightness_contrast(bc.0, bc.1);
            rs.renderer.set_crop(crop[0], crop[1], crop[2], crop[3]);
            rs.renderer.set_chroma_key(chroma.0, chroma.1, chroma.2, chroma.3);
            rs.renderer.set_luma_key(luma.0, luma.1, luma.2, luma.3);
            rs.renderer.set_grades(&grades);
            if let Some(p) = pipeline_params {
                rs.renderer.set_pipeline(p);
            }
        }
        // LUT application locks render_state itself — call after drop.
        self.apply_lut_for_clip(clip_id);
    }

    /// Phase 8 (real export) — decode (or rasterize) one clip's frame
    /// at `source_pts_sec` into a [`vance_decoder::FrameData`]. Files
    /// go through the primary decoder (opening on demand); titles are
    /// rasterized procedurally. Returns `None` on any failure.
    fn decode_clip_framedata(
        &mut self,
        media_id: MediaItemId,
        source_pts_sec: f64,
        source_in_sec: f64,
    ) -> Option<vance_decoder::FrameData> {
        let resolved = self.media_for_active(media_id).ok()?;
        match resolved {
            MediaResolved::File(path) => {
                let needs_open = self
                    .current_media_path
                    .as_deref()
                    .map(|p| p != path.as_str())
                    .unwrap_or(true);
                if needs_open {
                    self.open_file(&path).ok()?;
                }
                let target_pts = if let Some(info) = self.stream_info.as_ref() {
                    let (tb_n, tb_d) = info.time_base;
                    if tb_n != 0 {
                        (source_pts_sec * (tb_d as f64) / (tb_n as f64)).round() as i64
                    } else {
                        0
                    }
                } else {
                    0
                };
                let dec = self.decoder.as_mut()?;
                dec.decode_at(target_pts).ok()
            }
            MediaResolved::Title(params) => render_title_yuv420(&params).ok(),
            // Clip-relative time = source_pts − source_in (a trimmed/split
            // mograph clip has source_in > 0), so the exported animation
            // phase matches the live preview.
            MediaResolved::MoGraph(inst) => {
                Some(self.render_mograph_frame(&inst, source_pts_sec - source_in_sec))
            }
            // Clip-relative time = source_pts − source_in, so a trimmed/split
            // Fusion clip exports the same animation phase as live preview.
            MediaResolved::Fusion(comp) => {
                Some(self.render_fusion_frame(&comp, source_pts_sec - source_in_sec))
            }
        }
    }

    /// Phase 8 (real export) — offline audio mixdown for the timeline
    /// range `[start_s, end_s)` at `sample_rate`, returning interleaved
    /// stereo f32. Faithfully mirrors the live fold thread's full
    /// channel-strip + bus graph: per-clip gain/fades/keyframes +
    /// equal-power pan (pre-fader), per-track fader, per-track insert DSP
    /// (EQ/comp/limiter/plugins via `TrackDspState`), GOAT-4 aux sends
    /// (pre/post-fader) and the bus graph (per-bus inserts, fader, pan,
    /// sends, routing via `fold_bus_graph`/`bus_process_order`), then the
    /// master fader — but headless, re-decoding each clip via a dedicated
    /// audio sidecar. Replaces the silent audio the encoder used to
    /// receive.
    pub fn offline_audio_bounce(
        &mut self,
        start_s: f64,
        end_s: f64,
        sample_rate: u32,
    ) -> Result<Vec<f32>, EngineError> {
        // Spawn a dedicated audio decoder sidecar + plugin host for this
        // one bounce, then delegate to the shared range mixer.
        let bin = locate_audio_binary()?;
        let mut audio = vance_audio::AudioClient::spawn(&bin)
            .map_err(|e| EngineError::Project(format!("audio decoder spawn: {e}")))?;
        let mut plugin_host = locate_plugin_host_binary().map(|bin| {
            vance_audio::remote_plugin::RemotePluginHost::new(
                bin,
                sample_rate as f32,
                vance_audio::audio_plugin::DEFAULT_BLOCK as u32,
            )
        });
        let out =
            self.bounce_range_with(&mut audio, start_s, end_s, sample_rate, plugin_host.as_mut());
        let _ = audio.shutdown();
        out
    }

    /// Bounce the mixed audio for `[start_s, end_s)` using the supplied
    /// (possibly warm) decoder `audio` and optional `plugin_host`. Shared
    /// by the export path (fresh sidecar, plugins on) and the scrub grain
    /// renderer (warm sidecar, plugins off). Does NOT shut down `audio`
    /// (the caller owns its lifecycle), so it can be reused across calls.
    fn bounce_range_with(
        &mut self,
        audio: &mut vance_audio::AudioClient,
        start_s: f64,
        end_s: f64,
        sample_rate: u32,
        mut plugin_host: Option<&mut vance_audio::remote_plugin::RemotePluginHost>,
    ) -> Result<Vec<f32>, EngineError> {
        let snapshot = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
            let seq = project.sequence(seq_id).ok_or(EngineError::SequenceNotFound)?;
            build_mixer_snapshot(seq)
        };
        let master_lin = snapshot.master_gain_linear;
        let total_frames = (((end_s - start_s) * sample_rate as f64).round() as i64).max(0) as usize;
        let mut master = vec![0.0f32; total_frames * 2];
        if total_frames == 0 {
            return Ok(master);
        }

        // Gather audio-clip decode jobs + the audio tracks' DSP chains,
        // resolving media paths, while we hold the project borrow.
        struct ClipJob {
            clip_id: uuid::Uuid,
            track_id: uuid::Uuid,
            path: String,
            timeline_in: f64,
            source_in: f64,
            duration: f64,
        }
        let (clip_jobs, audio_tracks): (Vec<ClipJob>, Vec<uuid::Uuid>) = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
            let seq = project.sequence(seq_id).ok_or(EngineError::SequenceNotFound)?;
            let mut jobs = Vec::new();
            let mut tracks = Vec::new();
            for track in &seq.tracks {
                if track.kind != TrackKind::Audio {
                    continue;
                }
                tracks.push(track.id.as_uuid());
                // Track-level mute / solo gating is time-invariant, so
                // skip decoding clips that can't contribute at all.
                if track.mute || (snapshot.solo_anywhere && !track.solo) {
                    continue;
                }
                for clip in &track.clips {
                    if !clip.enabled {
                        continue;
                    }
                    let Some(media) = project.media.get(&clip.media) else {
                        continue;
                    };
                    if media.path.as_os_str().is_empty() {
                        continue; // titles / synthetic media have no audio
                    }
                    jobs.push(ClipJob {
                        clip_id: clip.id.as_uuid(),
                        track_id: track.id.as_uuid(),
                        path: media.path.to_string_lossy().into_owned(),
                        timeline_in: rt_to_seconds(clip.timeline_in),
                        source_in: rt_to_seconds(clip.source_in),
                        duration: rt_to_seconds(clip.duration()),
                    });
                }
            }
            (jobs, tracks)
        };

        // Per-track accumulators (interleaved stereo).
        let mut track_bufs: std::collections::HashMap<uuid::Uuid, Vec<f32>> = audio_tracks
            .iter()
            .map(|id| (*id, vec![0.0f32; total_frames * 2]))
            .collect();

        for job in &clip_jobs {
            // Timeline overlap with the export range.
            let clip_tl_start = job.timeline_in.max(start_s);
            let clip_tl_end = (job.timeline_in + job.duration).min(end_s);
            if clip_tl_end <= clip_tl_start {
                continue;
            }
            let out_frames =
                ((clip_tl_end - clip_tl_start) * sample_rate as f64).round() as usize;
            if out_frames == 0 {
                continue;
            }
            let src_start = job.source_in + (clip_tl_start - job.timeline_in);

            // Decode the clip's native-rate PCM for the overlapping span.
            let info = match audio.open(job.path.clone()) {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!(path = %job.path, ?e, "audio bounce: open failed; skipping clip");
                    continue;
                }
            };
            let (tb_n, tb_d) = info.time_base;
            let from_pts = if tb_n != 0 {
                (src_start * tb_d as f64 / tb_n as f64).round() as i64
            } else {
                0
            };
            if audio.start_stream(from_pts, 8192).is_err() {
                let _ = audio.close();
                continue;
            }
            let needed_native =
                ((clip_tl_end - clip_tl_start) * info.sample_rate as f64).ceil() as usize + 2;
            let mut native_pcm: Vec<f32> = Vec::with_capacity(needed_native * 2);
            while native_pcm.len() < needed_native * 2 {
                match audio.next_chunk() {
                    Ok(Some(c)) => native_pcm.extend_from_slice(&c.samples),
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            let _ = audio.stop_stream();
            let _ = audio.close();

            // Resample to the export rate.
            let resampled = crate::audio_bounce::resample_linear_stereo(
                &native_pcm,
                info.sample_rate,
                sample_rate,
                out_frames,
            );

            // Place into the PRE-FADER track buffer: clip gain × fades ×
            // clip pan only. The track fader, inserts, sends, bus graph,
            // and master are applied in the stages below (mirroring the
            // live fold), so this buffer is the pre-fader send tap point.
            let pan = snapshot
                .clips
                .get(&job.clip_id)
                .map(|c| c.pan_linear)
                .unwrap_or([1.0, 1.0]);
            let out_start_frame =
                ((clip_tl_start - start_s) * sample_rate as f64).round() as usize;
            let Some(track_buf) = track_bufs.get_mut(&job.track_id) else {
                continue;
            };
            for j in 0..out_frames {
                let timeline_t = clip_tl_start + j as f64 / sample_rate as f64;
                let pre = snapshot.clip_pre_fader_gain_at(&job.clip_id, timeline_t);
                if pre == 0.0 {
                    continue;
                }
                let oi = out_start_frame + j;
                if oi >= total_frames {
                    break;
                }
                track_buf[oi * 2] += resampled[j * 2] * pre * pan[0];
                track_buf[oi * 2 + 1] += resampled[j * 2 + 1] * pre * pan[1];
            }
        }
        // Per-track FADER → insert DSP, building a post-insert buffer per
        // track while KEEPING the pre-fader buffer (`track_bufs`) for
        // pre-fader sends. CLAP/LV2 plugins process out-of-process through
        // the caller-supplied `plugin_host` (export: on; scrub grain: None
        // → built-in DSP only). Same channel-strip order as live playback.
        let mut post_bufs: std::collections::HashMap<uuid::Uuid, Vec<f32>> =
            std::collections::HashMap::new();
        for track_id in &audio_tracks {
            // Skip muted / non-soloed tracks (mirrors the live fold skip);
            // their pre-fader buffer stays available but contributes
            // nothing (no main, no sends).
            let gated = snapshot
                .tracks
                .get(track_id)
                .map_or(true, |t| t.mute || (snapshot.solo_anywhere && !t.solo));
            if gated {
                continue;
            }
            let Some(pre_buf) = track_bufs.get(track_id) else {
                continue;
            };
            let mut post = pre_buf.clone();
            // Track fader (lane-aware), applied before the inserts.
            for fr in 0..total_frames {
                let t = start_s + fr as f64 / sample_rate as f64;
                let g = snapshot.track_gain_at(track_id, t);
                post[fr * 2] *= g;
                post[fr * 2 + 1] *= g;
            }
            if let Some(tm) = snapshot.tracks.get(track_id) {
                if !tm.audio_effects.is_empty() {
                    let mut dsp = vance_audio::TrackDspState::new();
                    dsp.reconcile_with(
                        &tm.audio_effects,
                        sample_rate as f32,
                        plugin_host.as_deref_mut(),
                    );
                    for fr in 0..total_frames {
                        let (nl, nr) = dsp.tick_stereo(
                            &tm.audio_effects,
                            post[fr * 2],
                            post[fr * 2 + 1],
                            plugin_host.as_deref_mut(),
                        );
                        post[fr * 2] = nl;
                        post[fr * 2 + 1] = nr;
                    }
                }
            }
            post_bufs.insert(*track_id, post);
        }

        // GOAT-4 — per-bus insert DSP states (stateful across frames) +
        // the upstream→downstream processing order, then fold the bus
        // graph per frame exactly like the live fold thread: route each
        // track's main + sends into bus inputs, run inserts/fader/pan/
        // sends per bus, and apply the master fader last.
        let bus_order = snapshot.bus_process_order();
        let mut bus_dsp: std::collections::HashMap<uuid::Uuid, vance_audio::TrackDspState> =
            std::collections::HashMap::new();
        for (bus_id, bm) in snapshot.buses.iter() {
            if !bm.audio_effects.is_empty() {
                let mut dsp = vance_audio::TrackDspState::new();
                dsp.reconcile_with(&bm.audio_effects, sample_rate as f32, plugin_host.as_deref_mut());
                bus_dsp.insert(*bus_id, dsp);
            }
        }
        let mut bus_in: std::collections::HashMap<uuid::Uuid, [f32; 2]> =
            std::collections::HashMap::new();
        for fr in 0..total_frames {
            let mut master_frame = [0.0f32; 2];
            bus_in.clear();
            for track_id in &audio_tracks {
                let Some(post) = post_bufs.get(track_id) else {
                    continue;
                };
                let post_s = [post[fr * 2], post[fr * 2 + 1]];
                let pre = track_bufs
                    .get(track_id)
                    .map(|b| [b[fr * 2], b[fr * 2 + 1]])
                    .unwrap_or([0.0, 0.0]);
                let tm = snapshot.tracks.get(track_id);
                // Aux sends: pre-fader taps the pre-fader buffer, post-
                // fader the post-insert signal. Drop sends to a missing bus.
                if let Some(tm) = tm {
                    for s in &tm.sends {
                        if !snapshot.buses.contains_key(&s.target) {
                            continue;
                        }
                        let src = if s.pre_fader { pre } else { post_s };
                        let e = bus_in.entry(s.target).or_insert([0.0, 0.0]);
                        e[0] += src[0] * s.level_linear;
                        e[1] += src[1] * s.level_linear;
                    }
                }
                // Main output → its bus input, or straight to master.
                match tm.and_then(|t| t.output_bus) {
                    Some(id) if snapshot.buses.contains_key(&id) => {
                        let e = bus_in.entry(id).or_insert([0.0, 0.0]);
                        e[0] += post_s[0];
                        e[1] += post_s[1];
                    }
                    _ => {
                        master_frame[0] += post_s[0];
                        master_frame[1] += post_s[1];
                    }
                }
            }
            vance_audio::fold_bus_graph(
                &snapshot,
                &bus_order,
                &mut bus_in,
                &mut master_frame,
                |bus_id, sig| {
                    if let (Some(dsp), Some(bm)) =
                        (bus_dsp.get_mut(&bus_id), snapshot.buses.get(&bus_id))
                    {
                        let (l, r) = dsp.tick_stereo(
                            &bm.audio_effects,
                            sig[0],
                            sig[1],
                            plugin_host.as_deref_mut(),
                        );
                        return [l, r];
                    }
                    sig
                },
            );
            master[fr * 2] = master_frame[0] * master_lin;
            master[fr * 2 + 1] = master_frame[1] * master_lin;
        }
        Ok(master)
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
            // HDR export overrides this to Rec.2020 PQ; SDR preview/export
            // use sRGB (the surface store applies the SDR transfer).
            output: self
                .render_output_space_override
                .unwrap_or(vance_color::ColorSpace::Srgb),
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
            MediaKind::MoGraph => match media.mograph.clone() {
                Some(inst) => Ok(MediaResolved::MoGraph(Box::new(inst))),
                // A MoGraph item with no instance is malformed; fall back
                // to a default title so resolution never fails hard.
                None => Ok(MediaResolved::Title(TitleParams::default())),
            },
            MediaKind::Fusion => match media.fusion.clone() {
                Some(comp) => Ok(MediaResolved::Fusion(Box::new(comp))),
                // A Fusion item with no comp is malformed; fall back to a
                // default title so resolution never fails hard.
                None => Ok(MediaResolved::Title(TitleParams::default())),
            },
            MediaKind::Video | MediaKind::Image | MediaKind::Audio => {
                // Phase 9.5 — when the project's "use proxies" toggle
                // is on AND this item has a generated proxy file, use
                // the proxy. Falls back to the original source if the
                // proxy file disappeared underneath us (e.g. user
                // deleted it manually).
                if project.use_proxies {
                    if let Some(p) = media.proxy_path.as_ref() {
                        if p.exists() {
                            return Ok(MediaResolved::File(p.to_string_lossy().into_owned()));
                        }
                    }
                }
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
            // A WASM transition blends on the CPU (process_transition);
            // a dissolve blends on the GPU (mix_factor).
            if let vance_timeline::TransitionKind::Wasm(plugin_id) = active_tr.kind.clone() {
                return self.decode_wasm_transition_at(&seq, active_tr, &plugin_id);
            }
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
        // For synthetic clips (title / mograph): render procedurally at
        // the clip-relative time and bypass the decoder.
        let media_path = match resolved {
            MediaResolved::Title(params) => {
                let frame = render_title_yuv420(&params)
                    .map_err(|e| EngineError::Project(format!("title render: {e}")))?;
                return self.decode_title_scrub(
                    frame,
                    anchor_clip_id,
                    anchor_timeline_in,
                    anchor_source_in,
                    source_pts_seconds,
                    anchor_opacity,
                );
            }
            MediaResolved::MoGraph(inst) => {
                let clip_t = source_pts_seconds - anchor_source_in;
                let frame = self.render_mograph_frame(&inst, clip_t);
                return self.decode_title_scrub(
                    frame,
                    anchor_clip_id,
                    anchor_timeline_in,
                    anchor_source_in,
                    source_pts_seconds,
                    anchor_opacity,
                );
            }
            MediaResolved::Fusion(comp) => {
                let clip_t = source_pts_seconds - anchor_source_in;
                let frame = self.render_fusion_frame(&comp, clip_t);
                return self.decode_title_scrub(
                    frame,
                    anchor_clip_id,
                    anchor_timeline_in,
                    anchor_source_in,
                    source_pts_seconds,
                    anchor_opacity,
                );
            }
            MediaResolved::File(p) => p,
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
                clip_grades_at(seq, anchor_clip_id, entry_clip_t, self.matte_node),
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
        // Phase 10 — hand the active clip's effect chain + clip-relative
        // time to `decode_and_render_at` so WASM effects show in the
        // preview. Only worth it when the clip actually has a `wasm:`
        // effect; otherwise leave it clear (normal GPU present).
        self.preview_wasm = {
            let clip_t = source_pts_seconds - anchor_source_in;
            self.current_project
                .as_ref()
                .and_then(|p| p.sequence(self.current_sequence_id?))
                .and_then(|seq| {
                    seq.tracks.iter().find_map(|t| {
                        t.clips
                            .iter()
                            .find(|c| c.id == anchor_clip_id)
                            .map(|c| c.effects.clone())
                    })
                })
                .filter(|effects| {
                    effects
                        .iter()
                        .any(|e| e.enabled && e.kind.starts_with("wasm:"))
                })
                .map(|effects| (effects, clip_t))
        };
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

    /// Set one parameter on a transition (WASM transitions). Undoable.
    pub fn set_transition_param(
        &mut self,
        track_id: TrackId,
        transition_id: vance_timeline::TransitionId,
        key: String,
        value: EffectValue,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::SetTransitionParam {
            track_id,
            transition_id,
            key,
            value,
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

    /// Track a power-window center across `n_frames` frames forward by
    /// zero-mean normalized cross-correlation on the Y plane, then write
    /// the (time, win_cx) and (time, win_cy) trail as a single batched
    /// `update_effect_params` edit (one undo step). Phase 6.11.
    ///
    /// v1 limitations (deliberate, separable as 6.11b):
    /// * synchronous — blocks the engine for ~`n_frames` decode RTTs;
    ///   capped at `TRACK_FRAMES_MAX` so the UI can't hang too long;
    /// * integer-pixel precision (no sub-pixel refinement);
    /// * single-scale (no pyramid — fast motion needs a bigger search
    ///   radius rather than coarser tracking);
    /// * no template update (drift on long shots);
    /// * existing win_cx / win_cy keyframes INSIDE the tracked range are
    ///   replaced; keyframes outside the range are preserved.
    pub fn track_window(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        effect_id: EffectId,
        start_source_seconds: f64,
        n_frames: u32,
        patch_half: u32,
        search_radius: u32,
    ) -> Result<TrackSummary, EngineError> {
        /// Hard cap on the synchronous tracker — keeps the engine from
        /// blocking the command queue for too long. 6.11b's async path
        /// removes this.
        const TRACK_FRAMES_MAX: u32 = 240;
        use vance_timeline::grade as gk;

        self.pause_for_edit();
        let n_frames = n_frames.clamp(2, TRACK_FRAMES_MAX);
        let patch_half_i = patch_half as i32;
        let search_radius_i = search_radius as i32;

        // ---- Snapshot all immutable state BEFORE borrowing the decoder.
        let info = self
            .stream_info
            .as_ref()
            .ok_or(EngineError::NoFileOpen)?
            .clone();
        let tb_num = info.time_base.0;
        let tb_den = info.time_base.1;
        let fr_num = info.frame_rate.0;
        let fr_den = info.frame_rate.1;
        if tb_num == 0 || tb_den == 0 || fr_num == 0 || fr_den == 0 {
            return Err(EngineError::Project(
                "tracker: invalid time_base or frame_rate (zero component)".into(),
            ));
        }
        // step_pts = (fr_den / fr_num) sec/frame  /  (tb_num / tb_den) sec/tick
        //          = fr_den * tb_den / (fr_num * tb_num)
        let step_pts = ((fr_den as i128 * tb_den as i128)
            / (fr_num as i128 * tb_num as i128))
            .max(1) as i64;
        // start_seconds (source-time) -> stream PTS in tb units.
        let start_pts = (start_source_seconds * tb_den as f64 / tb_num as f64).round() as i64;

        // Clip lookup (source_in for clip-relative time conversion).
        // Also resolves the clip's source-file path so we can confirm
        // the decoder is actually open on that file — otherwise a stale
        // decoder (e.g. the user opened a different file in the Source
        // monitor) would silently track frames from the wrong media.
        let (source_in_seconds, expected_path, mut cx_kfs, mut cy_kfs) = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
            let seq = project.sequence(seq_id).ok_or(EngineError::SequenceNotFound)?;
            let clip = seq
                .tracks
                .iter()
                .find_map(|t| t.clips.iter().find(|c| c.id == clip_id))
                .ok_or(EngineError::Project("tracker: clip not found".into()))?;
            let effect = clip
                .effects
                .iter()
                .find(|e| e.id == effect_id)
                .ok_or(EngineError::Project("tracker: effect not found".into()))?;
            let media = project
                .media
                .get(&clip.media)
                .ok_or(EngineError::MediaNotFound)?;
            (
                rt_to_seconds(clip.source_in),
                media.path.to_string_lossy().into_owned(),
                read_keyframes(&effect.params, gk::KEY_WIN_CENTER_X),
                read_keyframes(&effect.params, gk::KEY_WIN_CENTER_Y),
            )
        };
        // Refuse to track if the decoder isn't on this clip's source —
        // silently sampling the wrong file is the worst possible failure.
        match self.current_media_path.as_deref() {
            Some(p) if p == expected_path => {}
            _ => {
                return Err(EngineError::Project(format!(
                    "tracker: decoder is not open on the clip's source ({expected_path}) — \
                     open / play this clip first, then re-run the tracker"
                )));
            }
        }
        // Anchor (clip-relative) time + position from the current
        // keyframes (or defaults).
        let pts_to_clip_t = |pts: i64| -> f64 {
            (pts as f64) * (tb_num as f64) / (tb_den as f64) - source_in_seconds
        };
        let start_t = pts_to_clip_t(start_pts);
        let anchor_cx_uv = eval_keyframes_or(&cx_kfs, start_t, 0.5);
        let anchor_cy_uv = eval_keyframes_or(&cy_kfs, start_t, 0.5);

        // ---- Decode anchor frame, extract template.
        // Capture the anchor frame's ACTUAL dimensions for the seed
        // pixel — using `info.width/height` here would silently desync
        // if the decoder ever returned a rescaled / cropped frame.
        let (template, anchor_w, anchor_h) = {
            let dec = self.decoder.as_mut().ok_or(EngineError::NoFileOpen)?;
            let frame = dec
                .decode_at(start_pts)
                .map_err(|e| EngineError::Project(format!("tracker decode: {e}")))?;
            let stride = frame.strides[0];
            let bytes_end = (frame.height * stride) as usize;
            let view = vance_tracker::LumaView {
                bytes: &frame.bytes[..bytes_end],
                stride,
                w: frame.width,
                h: frame.height,
            };
            let anchor_px = vance_tracker::uv_to_px(
                (anchor_cx_uv as f32, anchor_cy_uv as f32),
                frame.width,
                frame.height,
            );
            let template = vance_tracker::extract_patch(&view, anchor_px, patch_half_i)
                .map_err(|e| EngineError::Project(format!("tracker patch: {e}")))?;
            (template, frame.width, frame.height)
        };
        let mut prev_px = vance_tracker::uv_to_px(
            (anchor_cx_uv as f32, anchor_cy_uv as f32),
            anchor_w,
            anchor_h,
        );

        // ---- Per-frame loop. Frame 0 is the anchor (already used);
        // results[0..n_frames-1] correspond to pts = start_pts + i*step.
        let mut tracked: Vec<(f64, (f32, f32))> = Vec::with_capacity(n_frames as usize);
        // Include the anchor itself so seek-to-start is bit-exact.
        tracked.push((start_t, (anchor_cx_uv as f32, anchor_cy_uv as f32)));
        // Seed `sum_score` with the anchor's self-correlation (exactly
        // 1.0 by construction — the template equals its own source patch)
        // so `mean_score = sum / scored` covers EVERY frame in
        // `frames_tracked`, not just the N-1 tracked frames. Without this
        // a user seeing frames_tracked=N + mean_score=M can't tell
        // whether M covers N or N-1 samples.
        let mut sum_score = 1.0_f32;
        let mut scored = 1u32;
        // Run the per-frame loop in an inner closure so a decode/track
        // failure mid-track doesn't discard the keyframes that already
        // succeeded — we commit the partial trail below before
        // propagating the error.
        let loop_err: Option<EngineError> = (|| -> Result<(), EngineError> {
            for i in 1..n_frames {
                let pts = start_pts + (i as i64) * step_pts;
                let dec = self.decoder.as_mut().ok_or(EngineError::NoFileOpen)?;
                let frame = dec
                    .decode_at(pts)
                    .map_err(|e| EngineError::Project(format!("tracker decode @ pts {pts}: {e}")))?;
                let stride = frame.strides[0];
                let bytes_end = (frame.height * stride) as usize;
                let view = vance_tracker::LumaView {
                    bytes: &frame.bytes[..bytes_end],
                    stride,
                    w: frame.width,
                    h: frame.height,
                };
                let r = vance_tracker::track_ncc(
                    &template,
                    &view,
                    prev_px,
                    search_radius_i,
                    patch_half_i,
                )
                .map_err(|e| EngineError::Project(format!("tracker step @ pts {pts}: {e}")))?;
                prev_px = r.center_px;
                sum_score += r.score;
                scored += 1;
                let t = pts_to_clip_t(pts);
                let uv = vance_tracker::px_to_uv(r.center_px, frame.width, frame.height);
                tracked.push((t, uv));
            }
            Ok(())
        })()
        .err();

        // ---- Merge: drop any existing keyframes in the tracked range
        // (inclusive on both ends), then append the new trail and sort.
        let end_t = tracked.last().map(|(t, _)| *t).unwrap_or(start_t);
        cx_kfs.retain(|k| k.time_seconds < start_t || k.time_seconds > end_t);
        cy_kfs.retain(|k| k.time_seconds < start_t || k.time_seconds > end_t);
        for (t, (u, v)) in &tracked {
            cx_kfs.push(vance_timeline::Keyframe {
                time_seconds: *t,
                value: *u as f64,
                interp: vance_timeline::KeyframeInterp::Linear,
            });
            cy_kfs.push(vance_timeline::Keyframe {
                time_seconds: *t,
                value: *v as f64,
                interp: vance_timeline::KeyframeInterp::Linear,
            });
        }
        let cmp = |a: &vance_timeline::Keyframe, b: &vance_timeline::Keyframe| {
            a.time_seconds
                .partial_cmp(&b.time_seconds)
                .unwrap_or(std::cmp::Ordering::Equal)
        };
        cx_kfs.sort_by(cmp);
        cy_kfs.sort_by(cmp);

        // ---- One batched, undoable update — committed BEFORE
        // propagating any loop error so partial progress is preserved.
        self.update_effect_params(
            track_id,
            clip_id,
            effect_id,
            vec![
                (
                    gk::KEY_WIN_CENTER_X.to_string(),
                    EffectValue::Keyframes { keyframes: cx_kfs },
                ),
                (
                    gk::KEY_WIN_CENTER_Y.to_string(),
                    EffectValue::Keyframes { keyframes: cy_kfs },
                ),
            ],
        )?;
        if let Some(e) = loop_err {
            return Err(e);
        }

        let mean_score = if scored > 0 {
            sum_score / scored as f32
        } else {
            1.0
        };
        let end_clip_seconds = tracked.last().map(|(t, _)| *t).unwrap_or(start_t);
        Ok(TrackSummary {
            frames_tracked: tracked.len() as u32,
            mean_score,
            start_clip_seconds: start_t,
            end_clip_seconds,
        })
    }

    /// GOAT-9 — smart reframe: track the subject across `n_frames` from
    /// `start_source_seconds` (seeded at `seed_cx/seed_cy` in source UV),
    /// then write a keyframed "transform" effect that keeps the subject
    /// centered in a `target_w:target_h` output (with extra `zoom`). Export
    /// at the target resolution to get the reframed video. Reuses the same
    /// NCC tracker as `track_window`; the decoder must already be open on
    /// the clip's source. Returns a `TrackSummary`.
    #[allow(clippy::too_many_arguments)]
    pub fn smart_reframe(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        target_w: u32,
        target_h: u32,
        zoom: f32,
        start_source_seconds: f64,
        n_frames: u32,
        seed_cx: f32,
        seed_cy: f32,
        patch_half: u32,
        search_radius: u32,
    ) -> Result<TrackSummary, EngineError> {
        const TRACK_FRAMES_MAX: u32 = 240;
        self.pause_for_edit();
        let n_frames = n_frames.clamp(2, TRACK_FRAMES_MAX);
        let patch_half_i = patch_half as i32;
        let search_radius_i = search_radius as i32;

        let info = self.stream_info.as_ref().ok_or(EngineError::NoFileOpen)?.clone();
        let (tb_num, tb_den) = info.time_base;
        let (fr_num, fr_den) = info.frame_rate;
        if tb_num == 0 || tb_den == 0 || fr_num == 0 || fr_den == 0 {
            return Err(EngineError::Project("reframe: invalid time_base/frame_rate".into()));
        }
        let step_pts = ((fr_den as i128 * tb_den as i128) / (fr_num as i128 * tb_num as i128))
            .max(1) as i64;
        let start_pts = (start_source_seconds * tb_den as f64 / tb_num as f64).round() as i64;

        let (source_in_seconds, expected_path) = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
            let seq = project.sequence(seq_id).ok_or(EngineError::SequenceNotFound)?;
            let clip = seq
                .tracks
                .iter()
                .find_map(|t| t.clips.iter().find(|c| c.id == clip_id))
                .ok_or(EngineError::Project("reframe: clip not found".into()))?;
            let media = project.media.get(&clip.media).ok_or(EngineError::MediaNotFound)?;
            (rt_to_seconds(clip.source_in), media.path.to_string_lossy().into_owned())
        };
        match self.current_media_path.as_deref() {
            Some(p) if p == expected_path => {}
            _ => {
                return Err(EngineError::Project(format!(
                    "reframe: decoder is not open on the clip's source ({expected_path}) — \
                     open / play this clip first, then re-run reframe"
                )));
            }
        }
        let pts_to_clip_t =
            |pts: i64| -> f64 { (pts as f64) * (tb_num as f64) / (tb_den as f64) - source_in_seconds };

        let seed = (seed_cx.clamp(0.0, 1.0), seed_cy.clamp(0.0, 1.0));
        // Anchor frame → template + source dims.
        let (template, src_w, src_h) = {
            let dec = self.decoder.as_mut().ok_or(EngineError::NoFileOpen)?;
            let frame = dec
                .decode_at(start_pts)
                .map_err(|e| EngineError::Project(format!("reframe decode: {e}")))?;
            let stride = frame.strides[0];
            let end = (frame.height * stride) as usize;
            let view = vance_tracker::LumaView {
                bytes: &frame.bytes[..end],
                stride,
                w: frame.width,
                h: frame.height,
            };
            let px = vance_tracker::uv_to_px(seed, frame.width, frame.height);
            let tpl = vance_tracker::extract_patch(&view, px, patch_half_i)
                .map_err(|e| EngineError::Project(format!("reframe patch: {e}")))?;
            (tpl, frame.width, frame.height)
        };
        let mut prev_px = vance_tracker::uv_to_px(seed, src_w, src_h);

        // Track the subject → per-frame (clip_t, subject_uv).
        let mut trail: Vec<(f64, (f32, f32))> = Vec::with_capacity(n_frames as usize);
        trail.push((pts_to_clip_t(start_pts), seed));
        let mut sum_score = 1.0f32;
        let mut scored = 1u32;
        for i in 1..n_frames {
            let pts = start_pts + (i as i64) * step_pts;
            let dec = self.decoder.as_mut().ok_or(EngineError::NoFileOpen)?;
            let frame = match dec.decode_at(pts) {
                Ok(f) => f,
                Err(_) => break, // partial track — commit what we have
            };
            let stride = frame.strides[0];
            let end = (frame.height * stride) as usize;
            let view = vance_tracker::LumaView {
                bytes: &frame.bytes[..end],
                stride,
                w: frame.width,
                h: frame.height,
            };
            let Ok(r) = vance_tracker::track_ncc(&template, &view, prev_px, search_radius_i, patch_half_i)
            else {
                break;
            };
            prev_px = r.center_px;
            sum_score += r.score;
            scored += 1;
            trail.push((pts_to_clip_t(pts), vance_tracker::px_to_uv(r.center_px, frame.width, frame.height)));
        }

        // Build keyframed translate (per frame) + constant scale.
        let (_, scale) = reframe_transform(seed, src_w, src_h, target_w, target_h, zoom);
        let mut tx_kfs = Vec::with_capacity(trail.len());
        let mut ty_kfs = Vec::with_capacity(trail.len());
        for (t, subj) in &trail {
            let (tr, _) = reframe_transform(*subj, src_w, src_h, target_w, target_h, zoom);
            tx_kfs.push(vance_timeline::Keyframe {
                time_seconds: *t,
                value: tr[0] as f64,
                interp: vance_timeline::KeyframeInterp::Linear,
            });
            ty_kfs.push(vance_timeline::Keyframe {
                time_seconds: *t,
                value: tr[1] as f64,
                interp: vance_timeline::KeyframeInterp::Linear,
            });
        }
        // Idempotent: reframe writes an ABSOLUTE transform, so re-running
        // must REPLACE the prior reframe (not stack another "transform" that
        // eval_clip_transform would multiply/add). Remove any transform this
        // reframe previously authored (tagged with a "reframe" marker param),
        // leaving the user's own manual transforms untouched.
        let prior: Vec<EffectId> = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
            let seq = project.sequence(seq_id).ok_or(EngineError::SequenceNotFound)?;
            seq.tracks
                .iter()
                .find(|t| t.id == track_id)
                .and_then(|t| t.clips.iter().find(|c| c.id == clip_id))
                .map(|c| {
                    c.effects
                        .iter()
                        .filter(|e| e.kind == "transform" && e.params.contains_key("reframe"))
                        .map(|e| e.id)
                        .collect()
                })
                .unwrap_or_default()
        };
        for eid in prior {
            self.remove_effect_from_clip(track_id, clip_id, eid)?;
        }

        let mut params: std::collections::BTreeMap<String, EffectValue> = Default::default();
        params.insert("translate_x".into(), EffectValue::Keyframes { keyframes: tx_kfs });
        params.insert("translate_y".into(), EffectValue::Keyframes { keyframes: ty_kfs });
        params.insert("scale_x".into(), EffectValue::Number(scale[0] as f64));
        params.insert("scale_y".into(), EffectValue::Number(scale[1] as f64));
        params.insert("rotation".into(), EffectValue::Number(0.0));
        // Marker so a re-run can find + replace this reframe (inert to
        // eval_clip_transform, which reads only translate/scale/rotation).
        params.insert("reframe".into(), EffectValue::Number(1.0));
        self.add_effect_to_clip(track_id, clip_id, "transform".into(), params)?;

        let start_t = trail.first().map(|(t, _)| *t).unwrap_or(0.0);
        let end_t = trail.last().map(|(t, _)| *t).unwrap_or(start_t);
        Ok(TrackSummary {
            frames_tracked: trail.len() as u32,
            mean_score: if scored > 0 { sum_score / scored as f32 } else { 1.0 },
            start_clip_seconds: start_t,
            end_clip_seconds: end_t,
        })
    }

    /// Dispatch a batch of typed grading commands (Phase 6.13 — the AI
    /// assistant's entry point). Returns one `GradingCommandOutcome`
    /// per command, in order.
    ///
    /// UNDO semantics: pure sequence-affecting commands accumulate into
    /// a single `EditOp::Compound` labeled `label` (one undo step per
    /// run, atomic rollback on partial failure). Other commands run
    /// inline and add their own entries — flushed BEFORE so order is
    /// preserved relative to the compound:
    /// * `SetOcioViewTransform`, `SetProjectViewLut`,
    ///   `SetProjectWorkingColorSpace` — mutate project state directly;
    ///   NOT recorded in the sequence EditLog at all.
    /// * `TrackWindow` — internally calls `update_effect_params`, which
    ///   IS recorded as its own `EditOp::UpdateEffectParams` entry. So
    ///   a batch containing one tracker yields two entries (the prior
    ///   compound + the tracker write); a batch with N trackers yields
    ///   N+1 entries. Folding the tracker into the compound is a
    ///   6.13b refinement.
    ///
    /// Mode-validation: `SetWheel`, `SetToneCurve`, and `SetHslCurve`
    /// reject mismatched modes upfront with a clear error rather than
    /// silently writing params the renderer would never sample.
    /// Look up the current `mode` param of a grade node ("primary" /
    /// "log" / "curves"). Returns "primary" (the schema's implicit
    /// default) when the node exists but has no mode key. Errors when
    /// the project / sequence / clip / effect can't be found.
    fn lookup_grade_mode(
        &self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        effect_id: EffectId,
    ) -> Result<String, EngineError> {
        let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
        let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
        let seq = project.sequence(seq_id).ok_or(EngineError::SequenceNotFound)?;
        let track = seq
            .tracks
            .iter()
            .find(|t| t.id == track_id)
            .ok_or(EngineError::Project("grading: track not found".into()))?;
        let clip = track
            .clips
            .iter()
            .find(|c| c.id == clip_id)
            .ok_or(EngineError::Project("grading: clip not found".into()))?;
        let effect = clip
            .effects
            .iter()
            .find(|e| e.id == effect_id)
            .ok_or(EngineError::Project("grading: effect not found".into()))?;
        let mode = match effect.params.get(vance_timeline::grade::KEY_MODE) {
            Some(EffectValue::String(s)) => s.clone(),
            _ => vance_timeline::grade::MODE_PRIMARY.to_string(),
        };
        Ok(mode)
    }

    pub fn apply_grading_batch(
        &mut self,
        cmds: &[vance_grading_cmd::GradingCommand],
        label: Option<String>,
    ) -> Result<Vec<vance_grading_cmd::GradingCommandOutcome>, EngineError> {
        use vance_grading_cmd as gc;
        use vance_timeline::grade as gk;
        use vance_timeline::{Effect, EffectId as EId, EffectValue, Keyframe, KeyframeInterp};
        self.pause_for_edit();
        let label = label.unwrap_or_else(|| "grading batch".to_string());

        let parse_uuid = |s: &str, what: &str| -> Result<uuid::Uuid, EngineError> {
            s.parse::<uuid::Uuid>()
                .map_err(|e| EngineError::Project(format!("invalid {what} uuid: {e}")))
        };
        let parse_track = |s: &str| -> Result<TrackId, EngineError> {
            Ok(TrackId::from_uuid(parse_uuid(s, "track_id")?))
        };
        let parse_clip = |s: &str| -> Result<vance_timeline::ClipId, EngineError> {
            Ok(vance_timeline::ClipId::from_uuid(parse_uuid(s, "clip_id")?))
        };
        let parse_effect = |s: &str| -> Result<EId, EngineError> {
            Ok(EId::from_uuid(parse_uuid(s, "effect_id")?))
        };
        let mode_str = |m: gc::GradeMode| match m {
            gc::GradeMode::Primary => gk::MODE_PRIMARY,
            gc::GradeMode::Log => gk::MODE_LOG,
            gc::GradeMode::Curves => gk::MODE_CURVES,
        };
        // Convert curve points -> animatable Keyframes (linear interp,
        // matching the editor's WYSIWYG model).
        let curve_kfs = |pts: &[gc::CurvePoint]| -> EffectValue {
            EffectValue::Keyframes {
                keyframes: pts
                    .iter()
                    .map(|p| Keyframe {
                        time_seconds: p.x as f64,
                        value: p.y as f64,
                        interp: KeyframeInterp::Linear,
                    })
                    .collect(),
            }
        };
        // Wheel-group → (rgb-key triple, master-key option). Primary
        // groups carry a master lane; log groups don't.
        let wheel_keys = |g: gc::WheelGroup| -> ([&'static str; 3], Option<&'static str>) {
            match g {
                gc::WheelGroup::Lift => ([gk::KEY_LIFT_R, gk::KEY_LIFT_G, gk::KEY_LIFT_B], Some(gk::KEY_LIFT_Y)),
                gc::WheelGroup::Gamma => ([gk::KEY_GAMMA_R, gk::KEY_GAMMA_G, gk::KEY_GAMMA_B], Some(gk::KEY_GAMMA_Y)),
                gc::WheelGroup::Gain => ([gk::KEY_GAIN_R, gk::KEY_GAIN_G, gk::KEY_GAIN_B], Some(gk::KEY_GAIN_Y)),
                gc::WheelGroup::Offset => ([gk::KEY_OFFSET_R, gk::KEY_OFFSET_G, gk::KEY_OFFSET_B], None),
                gc::WheelGroup::Shadow => ([gk::KEY_SHADOW_R, gk::KEY_SHADOW_G, gk::KEY_SHADOW_B], None),
                gc::WheelGroup::Mid => ([gk::KEY_MID_R, gk::KEY_MID_G, gk::KEY_MID_B], None),
                gc::WheelGroup::Highlight => ([gk::KEY_HI_R, gk::KEY_HI_G, gk::KEY_HI_B], None),
            }
        };
        // Reset value for a wheel — gamma/gain are y=1 identity;
        // everything else (lift/log balances/offset) is 0.
        let reset_rgb_master = |g: gc::WheelGroup| -> ([f32; 3], f32) {
            match g {
                gc::WheelGroup::Gamma | gc::WheelGroup::Gain => ([1.0, 1.0, 1.0], 1.0),
                _ => ([0.0, 0.0, 0.0], 0.0),
            }
        };

        let mut outcomes: Vec<gc::GradingCommandOutcome> = Vec::with_capacity(cmds.len());
        let mut pending: Vec<EditOp> = Vec::new();
        // Flush the accumulated sequence-EditOps as one Compound (or a
        // single op if exactly one is pending). Called between
        // "project-level" / tracker commands and at the end.
        let flush =
            |this: &mut Self, pending: &mut Vec<EditOp>, label: &str| -> Result<(), EngineError> {
                if pending.is_empty() {
                    return Ok(());
                }
                let op = if pending.len() == 1 {
                    pending.remove(0)
                } else {
                    EditOp::Compound {
                        ops: std::mem::take(pending),
                        label: label.to_string(),
                    }
                };
                this.apply_edit_op(op)
            };

        for cmd in cmds {
            match cmd {
                gc::GradingCommand::AddGradeNode { track_id, clip_id, mode, label: lbl } => {
                    let mut params = std::collections::BTreeMap::new();
                    params.insert(gk::KEY_MODE.to_string(), EffectValue::String(mode_str(*mode).to_string()));
                    if let Some(l) = lbl {
                        params.insert(gk::KEY_LABEL.to_string(), EffectValue::String(l.clone()));
                    }
                    let new_id = EId::new();
                    let effect = Effect {
                        id: new_id,
                        kind: gk::GRADE_KIND.to_string(),
                        enabled: true,
                        params,
                    };
                    pending.push(EditOp::AddEffectToClip {
                        track_id: parse_track(track_id)?,
                        clip_id: parse_clip(clip_id)?,
                        effect,
                    });
                    outcomes.push(gc::GradingCommandOutcome::NodeAdded {
                        effect_id: vance_grading_cmd::Uuid::from(new_id.to_string()),
                    });
                }
                gc::GradingCommand::RemoveGradeNode { track_id, clip_id, effect_id } => {
                    pending.push(EditOp::RemoveEffectFromClip {
                        track_id: parse_track(track_id)?,
                        clip_id: parse_clip(clip_id)?,
                        effect_id: parse_effect(effect_id)?,
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::ReorderGradeNode { track_id, clip_id, effect_id, new_index } => {
                    pending.push(EditOp::MoveEffectOnClip {
                        track_id: parse_track(track_id)?,
                        clip_id: parse_clip(clip_id)?,
                        effect_id: parse_effect(effect_id)?,
                        new_index: *new_index as usize,
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetGradeNodeEnabled { track_id, clip_id, effect_id, enabled } => {
                    pending.push(EditOp::SetEffectEnabled {
                        track_id: parse_track(track_id)?,
                        clip_id: parse_clip(clip_id)?,
                        effect_id: parse_effect(effect_id)?,
                        enabled: *enabled,
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetGradeNodeLabel { track_id, clip_id, effect_id, label: lbl } => {
                    pending.push(EditOp::UpdateEffectParams {
                        track_id: parse_track(track_id)?,
                        clip_id: parse_clip(clip_id)?,
                        effect_id: parse_effect(effect_id)?,
                        params: vec![(gk::KEY_LABEL.to_string(), EffectValue::String(lbl.clone()))],
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetNodeMode { track_id, clip_id, effect_id, mode } => {
                    pending.push(EditOp::UpdateEffectParams {
                        track_id: parse_track(track_id)?,
                        clip_id: parse_clip(clip_id)?,
                        effect_id: parse_effect(effect_id)?,
                        params: vec![(gk::KEY_MODE.to_string(), EffectValue::String(mode_str(*mode).to_string()))],
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetWheel { track_id, clip_id, effect_id, group, rgb, master, reset } => {
                    // Reject mismatched mode (primary wheels on a log
                    // node, log wheels on a primary node) — otherwise
                    // the params write succeeds but the renderer's
                    // mode-arm silently drops them, giving the AI a
                    // confusing "success that did nothing".
                    let tid = parse_track(track_id)?;
                    let cid = parse_clip(clip_id)?;
                    let eid = parse_effect(effect_id)?;
                    let mode = self.lookup_grade_mode(tid, cid, eid)?;
                    let want_primary = matches!(
                        group,
                        gc::WheelGroup::Lift | gc::WheelGroup::Gamma | gc::WheelGroup::Gain
                    );
                    let is_primary = mode == gk::MODE_PRIMARY;
                    if want_primary != is_primary {
                        return Err(EngineError::Project(format!(
                            "grading: SetWheel {:?} requires {} mode, but node is {}",
                            group,
                            if want_primary { "primary" } else { "log" },
                            mode
                        )));
                    }
                    let (rgb_keys, master_key) = wheel_keys(*group);
                    let mut params: Vec<(String, EffectValue)> = Vec::new();
                    if reset.unwrap_or(false) {
                        let (rrgb, rmaster) = reset_rgb_master(*group);
                        for (k, v) in rgb_keys.iter().zip(rrgb.iter()) {
                            params.push((k.to_string(), EffectValue::Number(*v as f64)));
                        }
                        if let Some(mk) = master_key {
                            params.push((mk.to_string(), EffectValue::Number(rmaster as f64)));
                        }
                    } else {
                        if let Some(rgb) = rgb {
                            for (k, v) in rgb_keys.iter().zip(rgb.iter()) {
                                params.push((k.to_string(), EffectValue::Number(*v as f64)));
                            }
                        }
                        if let (Some(mk), Some(m)) = (master_key, master) {
                            params.push((mk.to_string(), EffectValue::Number(*m as f64)));
                        }
                    }
                    if !params.is_empty() {
                        pending.push(EditOp::UpdateEffectParams {
                            track_id: tid,
                            clip_id: cid,
                            effect_id: eid,
                            params,
                        });
                    }
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetToneCurve { track_id, clip_id, effect_id, channel, points } => {
                    let tid = parse_track(track_id)?;
                    let cid = parse_clip(clip_id)?;
                    let eid = parse_effect(effect_id)?;
                    // Curve params on a non-curves node would write into
                    // params that the renderer never samples — reject
                    // upfront so the AI gets a clear error, not silence.
                    let mode = self.lookup_grade_mode(tid, cid, eid)?;
                    if mode != gk::MODE_CURVES {
                        return Err(EngineError::Project(format!(
                            "grading: SetToneCurve requires a curves-mode node (this node is {mode})"
                        )));
                    }
                    let key = match channel {
                        gc::ToneChannel::Master => gk::KEY_CURVE_MASTER,
                        gc::ToneChannel::R => gk::KEY_CURVE_R,
                        gc::ToneChannel::G => gk::KEY_CURVE_G,
                        gc::ToneChannel::B => gk::KEY_CURVE_B,
                        gc::ToneChannel::Luma => gk::KEY_CURVE_LUMA,
                    };
                    pending.push(EditOp::UpdateEffectParams {
                        track_id: tid,
                        clip_id: cid,
                        effect_id: eid,
                        params: vec![(key.to_string(), curve_kfs(points))],
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetHslCurve { track_id, clip_id, effect_id, curve, points } => {
                    let tid = parse_track(track_id)?;
                    let cid = parse_clip(clip_id)?;
                    let eid = parse_effect(effect_id)?;
                    let mode = self.lookup_grade_mode(tid, cid, eid)?;
                    if mode != gk::MODE_CURVES {
                        return Err(EngineError::Project(format!(
                            "grading: SetHslCurve requires a curves-mode node (this node is {mode})"
                        )));
                    }
                    let key = match curve {
                        gc::HslCurve::HueHue => gk::KEY_CURVE_HUE_HUE,
                        gc::HslCurve::HueSat => gk::KEY_CURVE_HUE_SAT,
                        gc::HslCurve::HueLum => gk::KEY_CURVE_HUE_LUM,
                        gc::HslCurve::SatSat => gk::KEY_CURVE_SAT_SAT,
                        gc::HslCurve::LumLum => gk::KEY_CURVE_LUM_LUM,
                    };
                    pending.push(EditOp::UpdateEffectParams {
                        track_id: tid,
                        clip_id: cid,
                        effect_id: eid,
                        params: vec![(key.to_string(), curve_kfs(points))],
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetQualifier { track_id, clip_id, effect_id, enabled, invert, hue, sat, luma } => {
                    let mut params: Vec<(String, EffectValue)> = Vec::new();
                    let mut push_bool = |k: &str, b: Option<bool>| {
                        if let Some(b) = b {
                            params.push((k.to_string(), EffectValue::Number(if b { 1.0 } else { 0.0 })));
                        }
                    };
                    push_bool(gk::KEY_QUAL_ENABLE, *enabled);
                    push_bool(gk::KEY_QUAL_INVERT, *invert);
                    let mut push_num = |k: &str, v: Option<f32>| {
                        if let Some(v) = v {
                            params.push((k.to_string(), EffectValue::Number(v as f64)));
                        }
                    };
                    if let Some(h) = hue {
                        push_num(gk::KEY_QUAL_HUE_C, h.center);
                        push_num(gk::KEY_QUAL_HUE_RANGE, h.range);
                        push_num(gk::KEY_QUAL_HUE_SOFT, h.soft);
                    }
                    if let Some(s) = sat {
                        push_num(gk::KEY_QUAL_SAT_LO, s.lo);
                        push_num(gk::KEY_QUAL_SAT_HI, s.hi);
                        push_num(gk::KEY_QUAL_SAT_SOFT, s.soft);
                    }
                    if let Some(l) = luma {
                        push_num(gk::KEY_QUAL_LUMA_LO, l.lo);
                        push_num(gk::KEY_QUAL_LUMA_HI, l.hi);
                        push_num(gk::KEY_QUAL_LUMA_SOFT, l.soft);
                    }
                    if !params.is_empty() {
                        pending.push(EditOp::UpdateEffectParams {
                            track_id: parse_track(track_id)?,
                            clip_id: parse_clip(clip_id)?,
                            effect_id: parse_effect(effect_id)?,
                            params,
                        });
                    }
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetWindow { track_id, clip_id, effect_id, shape, center, size, rotation, feather, invert } => {
                    let mut params: Vec<(String, EffectValue)> = Vec::new();
                    if let Some(s) = shape {
                        let v = match s {
                            gc::WindowShape::Off => 0.0,
                            gc::WindowShape::Ellipse => 1.0,
                            gc::WindowShape::Rectangle => 2.0,
                        };
                        params.push((gk::KEY_WIN_SHAPE.to_string(), EffectValue::Number(v)));
                    }
                    if let Some(c) = center {
                        params.push((gk::KEY_WIN_CENTER_X.to_string(), EffectValue::Number(c[0] as f64)));
                        params.push((gk::KEY_WIN_CENTER_Y.to_string(), EffectValue::Number(c[1] as f64)));
                    }
                    if let Some(s) = size {
                        params.push((gk::KEY_WIN_SIZE_X.to_string(), EffectValue::Number(s[0] as f64)));
                        params.push((gk::KEY_WIN_SIZE_Y.to_string(), EffectValue::Number(s[1] as f64)));
                    }
                    if let Some(r) = rotation {
                        params.push((gk::KEY_WIN_ROTATION.to_string(), EffectValue::Number(*r as f64)));
                    }
                    if let Some(f) = feather {
                        params.push((gk::KEY_WIN_FEATHER.to_string(), EffectValue::Number(*f as f64)));
                    }
                    if let Some(i) = invert {
                        params.push((gk::KEY_WIN_INVERT.to_string(), EffectValue::Number(if *i { 1.0 } else { 0.0 })));
                    }
                    if !params.is_empty() {
                        pending.push(EditOp::UpdateEffectParams {
                            track_id: parse_track(track_id)?,
                            clip_id: parse_clip(clip_id)?,
                            effect_id: parse_effect(effect_id)?,
                            params,
                        });
                    }
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                // ---- Project-level + tracker: flush the pending compound
                // first (preserves command order), then call inline.
                gc::GradingCommand::TrackWindow { track_id, clip_id, effect_id, start_source_seconds, n_frames, patch_half, search_radius } => {
                    flush(self, &mut pending, &label)?;
                    let summary = self.track_window(
                        parse_track(track_id)?,
                        parse_clip(clip_id)?,
                        parse_effect(effect_id)?,
                        *start_source_seconds,
                        *n_frames,
                        *patch_half,
                        *search_radius,
                    )?;
                    outcomes.push(gc::GradingCommandOutcome::Tracked {
                        frames_tracked: summary.frames_tracked,
                        mean_score: summary.mean_score,
                        start_clip_seconds: summary.start_clip_seconds,
                        end_clip_seconds: summary.end_clip_seconds,
                    });
                }
                gc::GradingCommand::SetOcioViewTransform { config, src, dst, size } => {
                    flush(self, &mut pending, &label)?;
                    self.set_ocio_view_transform(config.as_deref(), src, dst, *size)?;
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetProjectViewLut { path } => {
                    flush(self, &mut pending, &label)?;
                    self.set_project_view_lut(path.as_deref())?;
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetProjectWorkingColorSpace { color_space } => {
                    flush(self, &mut pending, &label)?;
                    self.set_project_working_color_space(color_space)?;
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
            }
        }
        // Final flush for any trailing sequence ops.
        flush(self, &mut pending, &label)?;
        Ok(outcomes)
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

    /// Phase 7.3: set a track's mute. Does NOT pause playback — the
    /// whole point of mixer controls is live response. The snapshot
    /// republish inside `apply_edit_op` makes the change audible
    /// within one mixer buffer (~5-21 ms at the cpal default).
    pub fn set_track_mute(
        &mut self,
        track_id: TrackId,
        mute: bool,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetTrackMute { track_id, mute })
    }

    /// Phase 7.3: set a track's solo flag.
    pub fn set_track_solo(
        &mut self,
        track_id: TrackId,
        solo: bool,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetTrackSolo { track_id, solo })
    }

    /// Phase 7.3: set a track's gain in dB. Live during playback.
    pub fn set_track_gain_db(
        &mut self,
        track_id: TrackId,
        gain_db: f32,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetTrackGainDb { track_id, gain_db })
    }

    /// Phase 7.3: set a clip's mute. Live during playback.
    pub fn set_clip_mute(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        mute: bool,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetClipMute {
            track_id,
            clip_id,
            mute,
        })
    }

    /// Phase 7.3: set a clip's mixer gain in dB. Live during playback.
    pub fn set_clip_gain_db(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        gain_db: f32,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetClipGainDb {
            track_id,
            clip_id,
            gain_db,
        })
    }

    /// Phase 7.6: set a track's stereo pan. Live during playback —
    /// snapshot republish recomputes every clip's `pan_linear`.
    pub fn set_track_pan(&mut self, track_id: TrackId, pan: f32) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetTrackPan { track_id, pan })
    }

    /// Phase 7.6: set a clip's stereo pan. Composes with the parent
    /// track's pan at mixer snapshot build time.
    pub fn set_clip_pan(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        pan: f32,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetClipPan {
            track_id,
            clip_id,
            pan,
        })
    }

    /// Phase 7.7: append an audio effect to the track's chain. Live
    /// during playback — the fold thread reconciles its DSP state on
    /// the next buffer.
    pub fn add_audio_effect(
        &mut self,
        track_id: TrackId,
        effect: vance_timeline::AudioEffectSpec,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::AddAudioEffect { track_id, effect })
    }

    /// Phase 7.7: remove the audio effect at `index` from the chain.
    /// No-op if the index is out of range. Live during playback.
    pub fn remove_audio_effect(
        &mut self,
        track_id: TrackId,
        index: usize,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::RemoveAudioEffect { track_id, index })
    }

    /// Phase 7.7: reorder one effect within the chain.
    pub fn move_audio_effect(
        &mut self,
        track_id: TrackId,
        from_index: usize,
        to_index: usize,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::MoveAudioEffect {
            track_id,
            from_index,
            to_index,
        })
    }

    /// Phase 7.7: replace one effect's spec (used for every knob
    /// commit).
    pub fn update_audio_effect(
        &mut self,
        track_id: TrackId,
        index: usize,
        effect: vance_timeline::AudioEffectSpec,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::UpdateAudioEffect {
            track_id,
            index,
            effect,
        })
    }

    /// Phase 7.4: set the sequence's master bus gain in dB. Live
    /// during playback — the snapshot republish makes it audible
    /// within one mixer buffer.
    pub fn set_master_gain_db(&mut self, gain_db: f32) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetMasterGainDb { gain_db })
    }

    // ---- GOAT-4: mixer buses + sends (Fairlight console) ----------------

    /// Add a submix bus (routed to master). Returns its id.
    pub fn add_bus(&mut self, name: String) -> Result<String, EngineError> {
        let bus_id = vance_timeline::BusId::new();
        self.apply_edit_op(EditOp::AddBus { bus_id, name })?;
        Ok(bus_id.to_string())
    }

    /// Remove a bus (dangling routes fall back to master).
    pub fn remove_bus(&mut self, bus_id: vance_timeline::BusId) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::RemoveBus { bus_id })
    }

    /// Set a bus's fader gain in dB.
    pub fn set_bus_gain_db(
        &mut self,
        bus_id: vance_timeline::BusId,
        gain_db: f32,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetBusGainDb { bus_id, gain_db })
    }

    /// Set a bus's stereo pan `[-1, 1]`.
    pub fn set_bus_pan(
        &mut self,
        bus_id: vance_timeline::BusId,
        pan: f32,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetBusPan { bus_id, pan })
    }

    /// Mute / unmute a bus.
    pub fn set_bus_mute(
        &mut self,
        bus_id: vance_timeline::BusId,
        mute: bool,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetBusMute { bus_id, mute })
    }

    /// Route a bus's main output (master or another bus).
    pub fn set_bus_output(
        &mut self,
        bus_id: vance_timeline::BusId,
        output: vance_timeline::BusOutput,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetBusOutput { bus_id, output })
    }

    /// Route a track's main output (master or a bus).
    pub fn route_track_to_bus(
        &mut self,
        track_id: TrackId,
        output: vance_timeline::BusOutput,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::RouteTrackToBus { track_id, output })
    }

    /// Add an aux send from a track to a bus.
    pub fn add_track_send(
        &mut self,
        track_id: TrackId,
        send: vance_timeline::Send,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::AddTrackSend { track_id, send })
    }

    /// Remove a track's send at `index`.
    pub fn remove_track_send(
        &mut self,
        track_id: TrackId,
        index: usize,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::RemoveTrackSend { track_id, index })
    }

    /// GOAT-4 — append an insert effect to a bus's chain.
    pub fn add_bus_insert(
        &mut self,
        bus_id: vance_timeline::BusId,
        effect: vance_timeline::AudioEffectSpec,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::AddBusInsert { bus_id, effect })
    }

    /// Remove a bus insert at `index`.
    pub fn remove_bus_insert(
        &mut self,
        bus_id: vance_timeline::BusId,
        index: usize,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::RemoveBusInsert { bus_id, index })
    }

    /// Reorder a bus insert.
    pub fn move_bus_insert(
        &mut self,
        bus_id: vance_timeline::BusId,
        from_index: usize,
        to_index: usize,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::MoveBusInsert {
            bus_id,
            from_index,
            to_index,
        })
    }

    /// Replace a bus insert's spec at `index`.
    pub fn update_bus_insert(
        &mut self,
        bus_id: vance_timeline::BusId,
        index: usize,
        effect: vance_timeline::AudioEffectSpec,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::UpdateBusInsert {
            bus_id,
            index,
            effect,
        })
    }

    /// GOAT-4 — add an aux send from a bus to another bus.
    pub fn add_bus_send(
        &mut self,
        bus_id: vance_timeline::BusId,
        send: vance_timeline::Send,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::AddBusSend { bus_id, send })
    }

    /// Remove a bus's send at `index`.
    pub fn remove_bus_send(
        &mut self,
        bus_id: vance_timeline::BusId,
        index: usize,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::RemoveBusSend { bus_id, index })
    }

    /// Read-only snapshot of the current sequence's buses (for the console).
    pub fn buses(&self) -> Vec<vance_timeline::AudioBus> {
        self.current_sequence_id
            .and_then(|id| self.current_project.as_ref().and_then(|p| p.sequence(id)))
            .map(|s| s.buses.clone())
            .unwrap_or_default()
    }

    /// Phase 7.5 M1: read the latest mixer metering snapshot. Returns
    /// `None` when the mixer backend isn't active (no playback or the
    /// single-source backend is in use instead). The returned snapshot
    /// is a cheap clone of the ArcSwap value — the fold thread keeps
    /// writing fresh snapshots ~30Hz regardless.
    pub fn meter_snapshot(&self) -> Option<vance_audio::MeterSnapshot> {
        let owner = match self.audio_owner.as_ref()? {
            AudioBackend::Mixer(o) => o,
            AudioBackend::Single(_) => return None,
        };
        Some((*owner.meter_handle().load_full()).clone())
    }

    /// Phase 7.4: replace a track's gain automation lane. Empty =
    /// revert to the static `Track.gain_db`. Live during playback —
    /// the fold thread reads the new lane via the ArcSwap snapshot
    /// bus on the next buffer.
    pub fn set_track_gain_keyframes(
        &mut self,
        track_id: TrackId,
        keyframes: Vec<vance_timeline::Keyframe>,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetTrackGainKeyframes { track_id, keyframes })
    }

    /// Phase 7.4: replace a clip's gain automation lane (clip-relative
    /// time). Live during playback.
    pub fn set_clip_gain_keyframes(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        keyframes: Vec<vance_timeline::Keyframe>,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetClipGainKeyframes {
            track_id,
            clip_id,
            keyframes,
        })
    }

    /// Phase 7.4 M3: set the clip's fade-in. `duration_seconds == 0.0`
    /// clears any existing fade-in. Live during playback — fades compose
    /// multiplicatively with the keyframe lane.
    pub fn set_clip_fade_in(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        duration_seconds: f64,
        curve: vance_timeline::FadeCurve,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetClipFadeIn {
            track_id,
            clip_id,
            duration_seconds,
            curve,
        })
    }

    /// Phase 7.4 M3: set the clip's fade-out. Mirror of
    /// [`Self::set_clip_fade_in`].
    pub fn set_clip_fade_out(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        duration_seconds: f64,
        curve: vance_timeline::FadeCurve,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetClipFadeOut {
            track_id,
            clip_id,
            duration_seconds,
            curve,
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
            eval_clip_grades(&effects, 0.0, self.matte_node)
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

    // ---- GOAT-6: magnetic timeline + connections ----------------------------

    /// Whether the current sequence is in magnetic-timeline mode.
    pub fn is_magnetic(&self) -> bool {
        self.current_sequence_ref().map(|s| s.magnetic).unwrap_or(false)
    }

    /// Toggle magnetic-timeline mode on the current sequence.
    pub fn set_magnetic(&mut self, magnetic: bool) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetMagnetic { magnetic })
    }

    /// The current sequence's connections as `(anchor, connected)` id pairs.
    pub fn connections(&self) -> Vec<(String, String)> {
        self.current_sequence_ref()
            .map(|s| {
                s.connections
                    .iter()
                    .map(|c| (c.anchor.to_string(), c.connected.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Connect `connected` to `anchor` (FCPX-style attachment).
    pub fn connect_clip(
        &mut self,
        anchor: vance_timeline::ClipId,
        connected: vance_timeline::ClipId,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::ConnectClip { anchor, connected })
    }

    /// Detach `connected` from whatever it's connected to.
    pub fn disconnect_clip(
        &mut self,
        connected: vance_timeline::ClipId,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::DisconnectClip { connected })
    }

    /// Magnetic move: drag a clip + its connected subtree together.
    pub fn magnetic_move_clip(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
        new_timeline_seconds: f64,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::MagneticMoveClip {
            track_id,
            clip_id,
            new_timeline_in: seconds_to_rt(new_timeline_seconds.max(0.0)),
        })
    }

    /// Cut-page append: add `media_id`'s full range at the track's end.
    pub fn append_clip(
        &mut self,
        track_id: TrackId,
        media_id: MediaItemId,
    ) -> Result<String, EngineError> {
        self.pause_for_edit();
        let clip = self.clip_from_media(media_id, vance_core::RationalTime::ZERO)?;
        let id = clip.id;
        self.apply_edit_op(EditOp::AppendClip { track_id, clip })?;
        Ok(id.to_string())
    }

    /// Magnetic ripple delete: remove a clip + close the gap on all tracks.
    pub fn magnetic_ripple_delete(
        &mut self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::MagneticRippleDelete { track_id, clip_id })
    }

    /// Magnetic ripple insert: open space on all tracks, then place the clip.
    pub fn magnetic_ripple_insert(
        &mut self,
        track_id: TrackId,
        media_id: MediaItemId,
        timeline_seconds: f64,
    ) -> Result<String, EngineError> {
        self.pause_for_edit();
        let clip = self.clip_from_media(media_id, seconds_to_rt(timeline_seconds.max(0.0)))?;
        let id = clip.id;
        self.apply_edit_op(EditOp::MagneticRippleInsert { track_id, clip })?;
        Ok(id.to_string())
    }

    /// Build a `Clip` spanning a media item's full visible range, placed at
    /// `timeline_in`. Shared by append / magnetic insert.
    fn clip_from_media(
        &self,
        media_id: MediaItemId,
        timeline_in: vance_core::RationalTime,
    ) -> Result<Clip, EngineError> {
        let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
        let media = project.media.get(&media_id).ok_or(EngineError::MediaNotFound)?;
        Clip::new(
            media.id,
            media.display_name.clone(),
            timeline_in,
            vance_core::RationalTime::ZERO,
            media.duration,
        )
        .map_err(|e| EngineError::Project(e.to_string()))
    }

    // ---- GOAT-7: captions / subtitles ---------------------------------------

    /// The current sequence's caption cues (sorted by start), for the editor.
    pub fn captions(&self) -> Vec<vance_captions::Cue> {
        self.current_sequence_ref()
            .map(|s| s.captions.clone())
            .unwrap_or_default()
    }

    /// Add a caption cue; returns its id.
    pub fn add_caption(
        &mut self,
        start_ms: i64,
        end_ms: i64,
        text: String,
    ) -> Result<String, EngineError> {
        let cue = vance_captions::Cue::new(start_ms, end_ms, text);
        let id = cue.id.to_string();
        self.apply_edit_op(EditOp::AddCaption { cue })?;
        Ok(id)
    }

    /// Remove a caption cue.
    pub fn remove_caption(&mut self, cue_id: vance_captions::CueId) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::RemoveCaption { cue_id })
    }

    // ---- GOAT-gated (collaboration): review comments ----------------------

    /// Add a review comment at a timeline position. Returns its id (string).
    pub fn add_review_comment(
        &mut self,
        timeline_ms: i64,
        author: String,
        text: String,
    ) -> Result<String, EngineError> {
        let comment =
            vance_timeline::ReviewComment::new(timeline_ms, author, text, now_unix_seconds());
        let id = comment.id.to_string();
        self.apply_edit_op(EditOp::AddReviewComment { comment })?;
        Ok(id)
    }

    /// Remove a review comment.
    pub fn remove_review_comment(
        &mut self,
        comment_id: vance_timeline::id::CommentId,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::RemoveReviewComment { comment_id })
    }

    /// Set a review comment's resolved flag.
    pub fn resolve_review_comment(
        &mut self,
        comment_id: vance_timeline::id::CommentId,
        resolved: bool,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::ResolveReviewComment {
            comment_id,
            resolved,
        })
    }

    /// Edit a review comment's body text.
    pub fn set_review_comment_text(
        &mut self,
        comment_id: vance_timeline::id::CommentId,
        text: String,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetReviewCommentText { comment_id, text })
    }

    /// Append a reply to a review comment's thread.
    pub fn reply_review_comment(
        &mut self,
        comment_id: vance_timeline::id::CommentId,
        author: String,
        text: String,
    ) -> Result<(), EngineError> {
        let reply = vance_timeline::ReviewReply {
            author,
            text,
            created_unix: now_unix_seconds(),
        };
        self.apply_edit_op(EditOp::ReplyReviewComment { comment_id, reply })
    }

    /// All review comments on the current sequence (sorted by position).
    pub fn review_comments(&self) -> Vec<vance_timeline::ReviewComment> {
        self.current_sequence_ref()
            .map(|s| s.review_comments.clone())
            .unwrap_or_default()
    }

    /// Edit a caption cue's text.
    pub fn set_caption_text(
        &mut self,
        cue_id: vance_captions::CueId,
        text: String,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetCaptionText { cue_id, text })
    }

    /// Set a caption cue's `[start_ms, end_ms)`.
    pub fn set_caption_timing(
        &mut self,
        cue_id: vance_captions::CueId,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetCaptionTiming {
            cue_id,
            start_ms,
            end_ms,
        })
    }

    /// Split a caption cue at `at_ms`.
    pub fn split_caption(
        &mut self,
        cue_id: vance_captions::CueId,
        at_ms: i64,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SplitCaption { cue_id, at_ms })
    }

    /// Merge a caption cue with the one that follows it.
    pub fn merge_caption_with_next(
        &mut self,
        cue_id: vance_captions::CueId,
    ) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::MergeCaptionWithNext { cue_id })
    }

    /// Shift every caption cue by `delta_ms`.
    pub fn shift_all_captions(&mut self, delta_ms: i64) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::ShiftAllCaptions { delta_ms })
    }

    /// Replace the whole caption list (import / transcription result).
    pub fn set_captions(&mut self, cues: Vec<vance_captions::Cue>) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetCaptions { cues })
    }

    /// Import captions from an `.srt` / `.vtt` file (by extension), replacing
    /// the current list. Returns the imported cue count.
    pub fn import_captions(&mut self, path: &str) -> Result<usize, EngineError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| EngineError::Project(format!("read captions: {e}")))?;
        let is_vtt = std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("vtt"))
            .unwrap_or(false);
        let cues = if is_vtt {
            vance_captions::vtt::from_vtt(&text)
        } else {
            vance_captions::srt::from_srt(&text)
        }
        .map_err(|e| EngineError::Project(format!("parse captions: {e}")))?;
        let n = cues.len();
        self.set_captions(cues)?;
        Ok(n)
    }

    /// Export the current captions to an `.srt` / `.vtt` file (by `vtt`).
    pub fn export_captions(&self, path: &str, vtt: bool) -> Result<(), EngineError> {
        let cues = self.captions();
        let text = if vtt {
            vance_captions::vtt::to_vtt(&cues)
        } else {
            vance_captions::srt::to_srt(&cues)
        };
        std::fs::write(path, text).map_err(|e| EngineError::Project(format!("write captions: {e}")))
    }

    /// The current sequence id as a string (for capturing the transcription
    /// target across the off-lock subprocess window).
    pub fn current_sequence_id_string(&self) -> Option<String> {
        self.current_sequence_id.map(|id| id.to_string())
    }

    // ---- GOAT-8: dialogue leveling (BS.1770 loudness analysis) ----------------

    /// The media item referenced by a clip on `track_id` (for off-lock
    /// loudness analysis).
    pub fn clip_media_id(
        &self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
    ) -> Option<MediaItemId> {
        self.clip_media_and_range(track_id, clip_id).map(|(m, _, _)| m)
    }

    /// GOAT-8 — a clip's media id + its active source range in seconds
    /// `(media, source_in_sec, duration_sec)`, so leveling measures only
    /// the clip's used region (not the whole source file).
    pub fn clip_media_and_range(
        &self,
        track_id: TrackId,
        clip_id: vance_timeline::ClipId,
    ) -> Option<(MediaItemId, f64, f64)> {
        let clip = self
            .current_sequence_ref()
            .ok()?
            .tracks
            .iter()
            .find(|t| t.id == track_id)?
            .clips
            .iter()
            .find(|c| c.id == clip_id)?
            .clone();
        let start = rt_to_seconds(clip.source_in);
        let dur = rt_to_seconds(clip.duration());
        Some((clip.media, start, dur))
    }


    /// The on-disk path of a media item (for transcription). `None` for
    /// synthetic / missing media.
    pub fn media_path_for(&self, media_id: MediaItemId) -> Option<std::path::PathBuf> {
        let p = self
            .current_project
            .as_ref()?
            .media
            .get(&media_id)?
            .path
            .clone();
        if p.as_os_str().is_empty() {
            None
        } else {
            Some(p)
        }
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
        let touches_mixer = op_touches_mixer(&op);
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
            *slot = Arc::new(new_seq.clone());
        }
        // Phase 7.3: if this edit touched mixer state AND mixer
        // playback is active, publish a fresh snapshot. The
        // ArcSwap::store is lock-free and cheap.
        if touches_mixer {
            self.republish_mixer_snapshot_if_active(&new_seq);
        }
        Ok(())
    }

    /// Build a fresh [`vance_audio::MixerSnapshot`] from the given
    /// sequence and publish it to the active mixer's snapshot bus.
    /// Also republishes the scheduling-interval list so the M4
    /// scheduler picks up topology changes (clip inserts/removes/trims)
    /// on its next tick. No-op when mixer playback isn't active.
    fn republish_mixer_snapshot_if_active(&self, sequence: &vance_timeline::Sequence) {
        if let Some(snapshot) = self.audio_mixer_snapshot.as_ref() {
            let snap = build_mixer_snapshot(sequence);
            snapshot.store(Arc::new(snap));
        }
        if let Some(AudioBackend::Mixer(owner)) = self.audio_owner.as_ref() {
            if let Some(project) = self.current_project.as_ref() {
                owner.publish_sched_clips(build_sched_clips(sequence, project));
            }
        }
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
            *slot = Arc::new(new_seq.clone());
        }
        // Phase 7.3: undo / redo can revert mixer state too — always
        // republish so the active mixer reflects the post-undo
        // sequence.
        self.republish_mixer_snapshot_if_active(&new_seq);
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
            *slot = Arc::new(new_seq.clone());
        }
        // Phase 7.3: redo can re-apply mixer state.
        self.republish_mixer_snapshot_if_active(&new_seq);
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

    /// The current sequence's full edit-history tree (for the history /
    /// branch panel). Empty when there's no current sequence.
    pub fn edit_history(&self) -> Vec<vance_timeline::HistoryNodeView> {
        self.current_sequence_id
            .and_then(|id| self.edit_logs.get(&id))
            .map(|log| log.history())
            .unwrap_or_default()
    }

    /// Tag the current edit-history node with a checkpoint / branch name
    /// (e.g. "v2 — punchier open"), so the user can jump back to it later.
    pub fn checkpoint_history(&mut self, name: String) -> Result<(), EngineError> {
        let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
        let log = self
            .edit_logs
            .get_mut(&seq_id)
            .ok_or(EngineError::NoCurrentSequence)?;
        log.checkpoint(name);
        Ok(())
    }

    /// Jump the current sequence to any history node by id — the "switch
    /// branch / restore version" operation. Mirrors undo/redo: re-points
    /// the project's sequence + republishes mixer state. Returns `false`
    /// if the id is out of range.
    pub fn goto_history(&mut self, node_id: usize) -> Result<bool, EngineError> {
        self.pause_for_edit();
        let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
        let log = self
            .edit_logs
            .get_mut(&seq_id)
            .ok_or(EngineError::NoCurrentSequence)?;
        if log.goto(node_id).is_none() {
            return Ok(false);
        }
        let new_seq = log.current().clone();
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        if let Some(slot) = project.sequences.iter_mut().find(|s| s.id == seq_id) {
            *slot = Arc::new(new_seq.clone());
        }
        self.republish_mixer_snapshot_if_active(&new_seq);
        Ok(true)
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
                            gain_db: c.gain_db,
                            gain_keyframes: c.gain_keyframes.clone(),
                            fade_in: c.fade_in,
                            fade_out: c.fade_out,
                            pan: c.pan,
                        })
                        .collect(),
                    transitions: t
                        .transitions
                        .iter()
                        .map(|tr| TransitionInfo {
                            id: tr.id.to_string(),
                            kind: match &tr.kind {
                                vance_timeline::TransitionKind::Dissolve => "dissolve".into(),
                                // WASM transitions surface as "wasm:<id>"
                                // (same convention as wasm effects).
                                vance_timeline::TransitionKind::Wasm(id) => format!("wasm:{id}"),
                            },
                            position_seconds: rt_to_seconds(tr.position),
                            duration_seconds: rt_to_seconds(tr.duration),
                            params: tr.params.clone(),
                        })
                        .collect(),
                    mute: t.mute,
                    solo: t.solo,
                    gain_db: t.gain_db,
                    gain_keyframes: t.gain_keyframes.clone(),
                    pan: t.pan,
                    audio_effects: t.audio_effects.clone(),
                })
                .collect(),
            master_gain_db: seq.master_gain_db,
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

    /// Phase 7.3: probe the current sequence + project to start the
    /// multi-source audio mixer at `from_seconds`. Returns Err when no
    /// project / sequence is loaded; otherwise spawns the mixer
    /// owner thread, stashes the snapshot bus handle on the engine,
    /// and the caller (M4 Tauri command + UI play_sequence wiring)
    /// uses the returned `AudioSync` + `AudioMixerOwner` as it would
    /// the single-file `AudioSync` + `AudioOwner` from `start_audio`.
    ///
    /// Audible output begins immediately; no video / scheduler
    /// integration in this method — that happens at the playback
    /// loop level in M4.
    pub fn start_mixer_session(
        &mut self,
        from_seconds: f64,
    ) -> Result<(AudioMixerOwner, AudioSync), EngineError> {
        let project = self
            .current_project
            .as_ref()
            .ok_or(EngineError::NoProject)?;
        let seq_id = self
            .current_sequence_id
            .ok_or(EngineError::NoCurrentSequence)?;
        let sequence = project
            .sequences
            .iter()
            .find(|s| s.id == seq_id)
            .ok_or(EngineError::NoCurrentSequence)?
            .clone();
        let (owner, sync, snapshot_handle) =
            start_audio_mix(&sequence, project, from_seconds).map_err(EngineError::Project)?;
        self.audio_mixer_snapshot = Some(snapshot_handle);
        Ok((owner, sync))
    }

    /// Stop the mixer playback session: drop the snapshot bus so
    /// subsequent EditOp applies don't try to publish to a torn-down
    /// mixer. The caller is responsible for actually joining the
    /// `AudioMixerOwner` (it returns synchronously from
    /// `shutdown()`).
    pub fn end_mixer_session(&mut self) {
        self.audio_mixer_snapshot = None;
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

// ============================================================
// Phase 7.3 — multi-source audio mixer integration
// ============================================================

/// Owner thread for the multi-source [`vance_audio::AudioMixer`].
/// Mirrors [`AudioOwner`]: the cpal stream is `!Send`, so the mixer
/// has to live on its own thread.
///
/// Phase 7.4 M4 also owns the scheduler thread + the snapshot bus the
/// scheduler watches for sequence changes. Drop / shutdown joins both.
pub struct AudioMixerOwner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// Phase 7.4 M4: bus of the current scheduling intervals. The
    /// engine publishes a new `Vec<SchedClip>` whenever the sequence
    /// topology / audio clips change; the scheduler reads it each tick.
    sched_clips: Arc<arc_swap::ArcSwap<Vec<SchedClip>>>,
    /// Joinable handle for the scheduler thread. Set to `None` after
    /// `shutdown()` / `Drop`.
    scheduler_handle: Option<std::thread::JoinHandle<()>>,
    /// Scheduler stop flag (separate from the mixer-owner stop so the
    /// scheduler stops emitting commands BEFORE the fold thread tears
    /// down its receiver — avoids spurious spawn-then-immediate-Remove).
    scheduler_stop: Arc<AtomicBool>,
    /// Live counters published by [`vance_audio::AudioMixer`] +
    /// scheduler. Exposed through [`Self::stats`] for the UI / logs.
    stats: Arc<vance_audio::MixerStats>,
    /// Phase 7.5 M1: lock-free metering bus published by the fold
    /// thread ~30Hz. UI polls this to render meter bars.
    meter: Arc<arc_swap::ArcSwap<vance_audio::MeterSnapshot>>,
}

impl AudioMixerOwner {
    fn stop_inner(&mut self) {
        // Stop scheduler FIRST so it doesn't try to send commands into
        // a torn-down mixer. The fold thread is fine with a
        // disconnected receiver.
        self.scheduler_stop.store(true, Ordering::Release);
        if let Some(h) = self.scheduler_handle.take() {
            let _ = h.join();
        }
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    /// Signal the owner thread to drop the mixer (cpal stream stops,
    /// fold thread exits, pullers detach) and join the owner. Also
    /// stops the scheduler thread.
    ///
    /// Idempotent with `Drop`: calling `shutdown` then dropping (or
    /// vice versa) is safe. The single source of truth is the two
    /// `Option<JoinHandle>` fields — once taken, the second call is a
    /// no-op.
    pub fn shutdown(mut self) {
        self.stop_inner();
    }

    /// Republish the current sequence's audio-clip scheduling intervals
    /// for the scheduler. Called by [`Engine`] whenever an EditOp that
    /// touches mixer topology lands.
    pub fn publish_sched_clips(&self, clips: Vec<SchedClip>) {
        self.sched_clips.store(Arc::new(clips));
    }

    /// Snapshot of the live mixer counters.
    pub fn stats(&self) -> Arc<vance_audio::MixerStats> {
        self.stats.clone()
    }

    /// Phase 7.5 M1: metering bus handle for the UI poll path.
    pub fn meter_handle(&self) -> Arc<arc_swap::ArcSwap<vance_audio::MeterSnapshot>> {
        self.meter.clone()
    }
}

impl Drop for AudioMixerOwner {
    /// Drop is the safety net for non-`shutdown` paths: panics,
    /// `Engine` field reassignment (`self.audio_owner = None`), or any
    /// early return that doesn't go through `shutdown()`. Without this
    /// the scheduler + owner threads would leak, the cpal device would
    /// stay open, and N sidecar child processes per leak would survive
    /// the parent Drop. Idempotent with `shutdown()` via the
    /// `Option<JoinHandle>` taken-once pattern.
    fn drop(&mut self) {
        self.stop_inner();
    }
}

/// One audio clip's scheduling interval — the minimum the scheduler
/// needs to decide whether to spawn / despawn it as the playhead
/// advances.
///
/// Reuses [`uuid::Uuid`] to match the vance-audio identifier shape; the
/// engine maps `TrackId` / `ClipId` → `Uuid` at publish time.
#[derive(Clone, Debug)]
pub struct SchedClip {
    /// Clip uuid. Matches the key in `MixerSnapshot.clips`.
    pub clip_id: uuid::Uuid,
    /// Absolute media path the sidecar should open.
    pub path: PathBuf,
    /// Source-domain start, in seconds (clip's `source_in`).
    pub source_in_seconds: f64,
    /// Timeline-domain start, in seconds (clip's `timeline_in`).
    pub timeline_in_seconds: f64,
    /// Clip's **timeline-domain** length in seconds. In v1 this equals
    /// `source_out - source_in` (no speed / time-remap yet), but the
    /// scheduler ALWAYS interprets this field as timeline-domain — the
    /// day vance-timeline adds time-remap, this value will need to be
    /// computed from `timeline_out - timeline_in` rather than from
    /// `Clip::duration()`, so that the
    /// `live_end = timeline_in + duration_seconds` math stays correct.
    pub duration_seconds: f64,
}

/// Phase 7.4 M4 scheduler tuning. Defaults chosen by the design spike;
/// the constants live here so 7.5 can flip them without re-publishing
/// the mixer ABI.
pub mod sched {
    /// Seconds before a clip's `timeline_in` at which the scheduler
    /// pre-opens its sidecar so the per-source ring is primed when the
    /// playhead crosses the boundary. The fold thread leaves the ring
    /// alone until `t_frame >= timeline_in` — preload is the structural
    /// piece of the seek-onto-edge bounded-silence SLO.
    pub const PRELOAD_SECONDS: f64 = 2.0;
    /// Cap on simultaneously-live sources per mixer. Past this the
    /// scheduler refuses additional spawns and bumps
    /// `scheduler_cap_hits_total`; live-audio-track count + clip density
    /// determines how often this fires. Chosen to be high enough to
    /// cover typical sequences (~8 tracks × 2 overlapping clips) while
    /// not letting a pathological project DoS the audio sidecars.
    pub const MAX_CONCURRENT_SOURCES: usize = 16;
    /// Scheduler tick period. 50 ms is well above one device buffer
    /// (typically ~5 ms at 48 kHz / 256 frames) so we add/remove well
    /// ahead of the buffer boundary, but tight enough that
    /// PRELOAD_SECONDS (2.0 s) gives the sidecar generous headroom.
    pub const TICK_MS: u64 = 50;
}

/// Build the scheduling-interval list for the audio clips in `sequence`.
/// Mirrors the data the mixer's [`vance_audio::MixerSnapshot`] holds,
/// but in flat-vector form for cheap iteration on the scheduler thread.
fn build_sched_clips(
    sequence: &vance_timeline::Sequence,
    project: &vance_timeline::Project,
) -> Vec<SchedClip> {
    let mut out = Vec::new();
    for track in &sequence.tracks {
        if track.kind != TrackKind::Audio || !track.enabled {
            continue;
        }
        for clip in &track.clips {
            let Some(media) = project.media.get(&clip.media) else {
                continue;
            };
            if !media.has_audio_track {
                continue;
            }
            out.push(SchedClip {
                clip_id: clip.id.as_uuid(),
                path: media.path.clone(),
                source_in_seconds: rt_to_seconds(clip.source_in),
                timeline_in_seconds: rt_to_seconds(clip.timeline_in),
                duration_seconds: rt_to_seconds(clip.duration()),
            });
        }
    }
    out
}

/// Output of one [`compute_sched_plan`] tick.
///
/// Pure decision data — no channel I/O. The IO-bound scheduler loop
/// turns this into [`vance_audio::SourceCommand`]s. Splitting it out
/// keeps the planning math unit-testable without a thread or a clock.
#[derive(Debug)]
pub struct SchedPlan {
    /// New clips to spawn this tick.
    pub to_add: Vec<(uuid::Uuid, PathBuf, f64)>, // (clip_id, path, from_seconds)
    /// Live clips to despawn this tick.
    pub to_remove: Vec<uuid::Uuid>,
    /// Number of clips dropped by the MAX_CONCURRENT_SOURCES cap.
    /// Bumps [`vance_audio::MixerStats::scheduler_cap_hits_total`] by
    /// this amount when the IO loop applies the plan.
    pub cap_drops: u64,
}

/// Pure scheduler planner. Given the published interval list, the
/// currently-live set, and the playhead at timeline-time `t`, compute
/// the Add/Remove diff to be sent to the fold thread.
///
/// Phase 7.4 M4 semantics:
/// - **Window**: a clip is "wanted" when its live region overlaps
///   `[t - grace, t + PRELOAD]`. `grace_seconds` is one scheduler tick
///   worth of headroom so a clip that JUST ended isn't yanked
///   mid-buffer. The fold thread additionally enforces a SYMMETRIC
///   past-end gate (skips popping when `t_frame >= timeline_in +
///   duration`), so the grace window only delays cleanup — it does
///   NOT leak post-out source audio.
/// - **Cap**: when more clips than `MAX_CONCURRENT_SOURCES` qualify,
///   prefer the most-imminent (smallest `timeline_in`). The furthest-
///   future are dropped; `cap_drops` records the count. Note: the
///   same drops re-count every tick the cap is still exceeded — read
///   `scheduler_cap_hits_total` as a RATE indicator, not a one-shot
///   counter.
/// - **Add source offset**: `from_seconds = source_in + max(0, t -
///   timeline_in)`. Clips spawning ahead of the playhead start
///   decoding from their own `source_in`; clips already overlapping
///   the playhead at spawn time seek into the source so playback
///   begins from the current playhead immediately.
pub fn compute_sched_plan(
    intervals: &[SchedClip],
    live: &std::collections::HashSet<uuid::Uuid>,
    t: f64,
    grace_seconds: f64,
    preload_seconds: f64,
    max_concurrent: usize,
) -> SchedPlan {
    use std::collections::HashSet;

    let mut candidates: Vec<&SchedClip> = intervals
        .iter()
        .filter(|c| {
            let live_end = c.timeline_in_seconds + c.duration_seconds;
            live_end > t - grace_seconds && c.timeline_in_seconds <= t + preload_seconds
        })
        .collect();
    candidates.sort_by(|a, b| {
        a.timeline_in_seconds
            .partial_cmp(&b.timeline_in_seconds)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let cap_drops = candidates.len().saturating_sub(max_concurrent) as u64;
    if candidates.len() > max_concurrent {
        candidates.truncate(max_concurrent);
    }

    let wanted: HashSet<uuid::Uuid> = candidates.iter().map(|c| c.clip_id).collect();

    let to_remove: Vec<uuid::Uuid> =
        live.iter().filter(|id| !wanted.contains(id)).copied().collect();

    let to_add: Vec<(uuid::Uuid, PathBuf, f64)> = candidates
        .iter()
        .filter(|c| !live.contains(&c.clip_id))
        .map(|c| {
            let from_seconds = c.source_in_seconds + (t - c.timeline_in_seconds).max(0.0);
            (c.clip_id, c.path.clone(), from_seconds)
        })
        .collect();

    SchedPlan {
        to_add,
        to_remove,
        cap_drops,
    }
}

/// Scheduler thread main loop. Polls the master frame counter to derive
/// the current timeline-time, sweeps the published `SchedClip` list, and
/// emits `SourceCommand::Add` / `Remove` to grow / shrink the mixer's
/// live source set.
///
/// Phase 7.4 M4: the scheduler is the **spawn-owner** for all runtime
/// Adds — it calls [`vance_audio::spawn_source`] (synchronous sidecar
/// IPC) directly so the heavy lift never lands on the fold thread that
/// feeds cpal. The fold thread just receives a ready [`vance_audio::SourceStream`]
/// per `Add` command and inserts it into its HashMap.
///
/// The `live` set is seeded with the initial source ids passed in by
/// `start_audio_mix` so the first tick does NOT re-spawn the clips that
/// were already pre-spawned synchronously before playback began.
///
/// On `sample_rate == 0` (cpal misconfiguration; should be unreachable)
/// the loop exits early instead of producing time-invariant scheduling
/// decisions.
#[allow(clippy::too_many_arguments)]
fn scheduler_loop(
    sched_clips: Arc<arc_swap::ArcSwap<Vec<SchedClip>>>,
    command_tx: std::sync::mpsc::Sender<vance_audio::SourceCommand>,
    master_frame: Arc<std::sync::atomic::AtomicI64>,
    sample_rate: u32,
    playback_start_timeline_seconds: f64,
    stats: Arc<vance_audio::MixerStats>,
    stop: Arc<AtomicBool>,
    sidecar_binary: PathBuf,
    per_source_capacity: usize,
    initial_live_ids: std::collections::HashSet<uuid::Uuid>,
) {
    if sample_rate == 0 {
        tracing::error!("scheduler: sample_rate=0; scheduler loop exiting");
        return;
    }
    let inv_sample_rate = 1.0 / sample_rate as f64;
    let grace_seconds = sched::TICK_MS as f64 / 1000.0;
    let mut live: std::collections::HashSet<uuid::Uuid> = initial_live_ids;
    while !stop.load(Ordering::Acquire) {
        let frame = master_frame.load(Ordering::Acquire);
        let t = playback_start_timeline_seconds + frame as f64 * inv_sample_rate;
        let intervals = sched_clips.load_full();
        let plan = compute_sched_plan(
            &intervals,
            &live,
            t,
            grace_seconds,
            sched::PRELOAD_SECONDS,
            sched::MAX_CONCURRENT_SOURCES,
        );
        if plan.cap_drops > 0 {
            // NOTE: this re-counts the same dropped clips every tick
            // the cap remains exceeded — read the counter as a rate.
            stats
                .scheduler_cap_hits_total
                .fetch_add(plan.cap_drops, Ordering::Release);
        }
        for id in plan.to_remove {
            live.remove(&id);
            if command_tx
                .send(vance_audio::SourceCommand::Remove(id))
                .is_err()
            {
                return;
            }
        }
        for (clip_id, path, from_seconds) in plan.to_add {
            // Synchronous spawn on THIS thread — sidecar IPC, file open,
            // start_stream. Takes 50–200ms on a cold cache. The fold
            // thread keeps producing samples for already-live sources
            // while we block here.
            let source = vance_audio::AudioSource {
                clip_id,
                path,
                from_seconds,
            };
            match vance_audio::spawn_source(
                &sidecar_binary,
                &source,
                per_source_capacity,
                &stats,
            ) {
                Ok(stream) => {
                    if command_tx
                        .send(vance_audio::SourceCommand::Add(stream))
                        .is_err()
                    {
                        return;
                    }
                    live.insert(clip_id);
                }
                Err(e) => {
                    tracing::error!(?e, clip_id = %clip_id, "scheduler: spawn failed");
                    // Don't insert into `live` — next tick will retry.
                }
            }
        }
        std::thread::sleep(Duration::from_millis(sched::TICK_MS));
    }
}

/// Translate a [`Track`] from vance-timeline into a [`vance_audio::TrackMix`]
/// with `gain_linear` precomputed at snapshot-build time. Mute and solo
/// pass through verbatim. Tracks of kind != Audio still emit a TrackMix
/// (it's keyed by uuid; non-audio tracks just don't have ClipMixes
/// pointing at them, so they never contribute to the fold).
fn track_to_mix(track: &Track) -> vance_audio::TrackMix {
    vance_audio::TrackMix {
        gain_linear: vance_audio::db_to_linear(track.gain_db),
        gain_lane: translate_gain_keyframes(&track.gain_keyframes),
        mute: track.mute,
        solo: track.solo,
        audio_effects: track
            .audio_effects
            .iter()
            .map(translate_audio_effect)
            .collect(),
        output_bus: match track.output {
            vance_timeline::BusOutput::Master => None,
            vance_timeline::BusOutput::Bus(id) => Some(id.as_uuid()),
        },
        sends: track.sends.iter().map(translate_send).collect(),
    }
}

/// GOAT-4 — translate a timeline `Send` into the fold-thread mirror,
/// precomputing the linear send level from dB.
fn translate_send(send: &vance_timeline::Send) -> vance_audio::SendMix {
    vance_audio::SendMix {
        target: send.target.as_uuid(),
        level_linear: vance_audio::db_to_linear(send.level_db),
        pre_fader: send.pre_fader,
    }
}

/// Phase 7.7 — translate one vance-timeline `AudioEffectSpec` into the
/// vance-audio mirror, sanitising and pre-computing what the fold
/// thread needs. The translation keeps the audio crate decoupled from
/// the timeline crate and centralises range-clamping so the fold
/// thread can trust every value.
fn translate_audio_effect(
    spec: &vance_timeline::AudioEffectSpec,
) -> vance_audio::AudioEffectKind {
    use vance_timeline::AudioEffectSpec as Spec;
    match spec {
        Spec::Eq6 { enabled, eq } => {
            let mut out = [vance_audio::EqBandParams {
                enabled: true,
                shape: vance_audio::dsp::BiquadShape::Peak,
                freq_hz: 1_000.0,
                gain_db: 0.0,
                q: 0.707,
            }; 6];
            for (dst, src) in out.iter_mut().zip(eq.bands.iter()) {
                dst.enabled = src.enabled;
                dst.shape = match src.kind {
                    vance_timeline::EqBandKind::LowShelf => {
                        vance_audio::dsp::BiquadShape::LowShelf
                    }
                    vance_timeline::EqBandKind::Peak => vance_audio::dsp::BiquadShape::Peak,
                    vance_timeline::EqBandKind::HighShelf => {
                        vance_audio::dsp::BiquadShape::HighShelf
                    }
                    vance_timeline::EqBandKind::LowCut => vance_audio::dsp::BiquadShape::LowCut,
                    vance_timeline::EqBandKind::HighCut => vance_audio::dsp::BiquadShape::HighCut,
                };
                dst.freq_hz = src.freq_hz.clamp(10.0, 22_000.0);
                dst.gain_db = src.gain_db.clamp(-24.0, 24.0);
                dst.q = src.q.clamp(0.1, 18.0);
            }
            vance_audio::AudioEffectKind::Eq6 {
                enabled: *enabled,
                bands: out,
            }
        }
        Spec::Compressor { enabled, spec } => vance_audio::AudioEffectKind::Compressor {
            enabled: *enabled,
            params: vance_audio::CompressorParams {
                threshold_db: spec.threshold_db.clamp(-60.0, 0.0),
                ratio: spec.ratio.clamp(1.0, 20.0),
                attack_ms: spec.attack_ms.clamp(0.1, 1000.0),
                release_ms: spec.release_ms.clamp(0.1, 5000.0),
                knee_db: spec.knee_db.clamp(0.0, 18.0),
                makeup_db: spec.makeup_db.clamp(-12.0, 24.0),
            },
        },
        Spec::Limiter { enabled, spec } => vance_audio::AudioEffectKind::Limiter {
            enabled: *enabled,
            params: vance_audio::LimiterParams {
                ceiling_db: spec.ceiling_db.clamp(-30.0, 0.0),
                release_ms: spec.release_ms.clamp(1.0, 5000.0),
            },
        },
        Spec::NoiseGate { enabled, spec } => vance_audio::AudioEffectKind::NoiseGate {
            enabled: *enabled,
            params: vance_audio::NoiseGateParams {
                threshold_db: spec.threshold_db.clamp(-90.0, 0.0),
                range_db: spec.range_db.clamp(-90.0, 0.0),
                attack_ms: spec.attack_ms.clamp(0.1, 500.0),
                release_ms: spec.release_ms.clamp(1.0, 5000.0),
            },
        },
        Spec::DeHum { enabled, spec } => vance_audio::AudioEffectKind::DeHum {
            enabled: *enabled,
            params: vance_audio::DeHumParams {
                base_hz: spec.base_hz.clamp(40.0, 120.0),
                harmonics: spec.harmonics.clamp(1, 10),
                q: spec.q.clamp(0.5, 40.0),
            },
        },
        Spec::Plugin {
            enabled,
            descriptor,
            params,
        } => vance_audio::AudioEffectKind::Plugin {
            enabled: *enabled,
            format_str: descriptor.format.clone(),
            id: descriptor.id.clone(),
            name: descriptor.name.clone(),
            // Project param overrides → packed (param_id, value) the
            // fold thread feeds the in-process plugin.
            params: params.iter().map(|p| (p.param_id, p.value)).collect(),
        },
    }
}

/// Translate a [`Clip`] from vance-timeline into a
/// [`vance_audio::ClipMix`]. `enabled` mirrors `Clip.enabled` (per-clip
/// mute), `gain_linear` is precomputed.
fn clip_to_mix(clip: &Clip, track: &Track) -> vance_audio::ClipMix {
    let (fade_in_dur, fade_in_curve) = clip
        .fade_in
        .as_ref()
        .map(|f| (f.duration_seconds, translate_fade_curve(f.curve)))
        .unwrap_or((0.0, vance_audio::FadeCurve::Linear));
    let (fade_out_dur, fade_out_curve) = clip
        .fade_out
        .as_ref()
        .map(|f| (f.duration_seconds, translate_fade_curve(f.curve)))
        .unwrap_or((0.0, vance_audio::FadeCurve::Linear));
    // Phase 7.6: precompose track + clip pan ONCE per snapshot build so
    // the fold thread never invokes sin/cos. The clamp prevents an
    // out-of-range sum (e.g. track +0.7 with clip +0.6) from
    // overshooting the equal-power conversion's domain.
    let effective_pan = (track.pan + clip.pan).clamp(-1.0, 1.0);
    let pan_linear = vance_audio::pan_to_equal_power(effective_pan);
    vance_audio::ClipMix {
        gain_linear: vance_audio::db_to_linear(clip.gain_db),
        gain_lane: translate_gain_keyframes(&clip.gain_keyframes),
        enabled: clip.enabled,
        track_id: track.id.as_uuid(),
        timeline_in_seconds: rt_to_seconds(clip.timeline_in),
        clip_duration_seconds: rt_to_seconds(clip.duration()),
        fade_in_duration_seconds: fade_in_dur,
        fade_in_curve,
        fade_out_duration_seconds: fade_out_dur,
        fade_out_curve,
        pan_linear,
    }
}

/// Map vance-timeline's `FadeCurve` onto vance-audio's local mirror.
/// Both crates ship the same Linear / EqualPower / Logarithmic shape
/// for 7.5 forward-compat; only `Linear` is honoured in 7.4.
fn translate_fade_curve(curve: vance_timeline::FadeCurve) -> vance_audio::FadeCurve {
    match curve {
        vance_timeline::FadeCurve::Linear => vance_audio::FadeCurve::Linear,
        vance_timeline::FadeCurve::EqualPower => vance_audio::FadeCurve::EqualPower,
        vance_timeline::FadeCurve::Logarithmic => vance_audio::FadeCurve::Logarithmic,
    }
}

/// Translate vance-timeline `Keyframe`s into vance-audio
/// `GainKeyframe`s by precomputing `db_to_linear` at snapshot build
/// time — the fold thread MUST never invoke `powf`.
fn translate_gain_keyframes(
    keyframes: &[vance_timeline::Keyframe],
) -> Vec<vance_audio::GainKeyframe> {
    keyframes
        .iter()
        .map(|k| vance_audio::GainKeyframe {
            time_seconds: k.time_seconds,
            gain_linear: vance_audio::db_to_linear(k.value as f32),
            interp: match k.interp {
                vance_timeline::KeyframeInterp::Linear => vance_audio::GainInterp::Linear,
                vance_timeline::KeyframeInterp::Bezier => vance_audio::GainInterp::Bezier,
                vance_timeline::KeyframeInterp::Hold => vance_audio::GainInterp::Hold,
            },
        })
        .collect()
}

/// Build a [`MixerSnapshot`] covering every track + clip in the
/// current sequence. The snapshot is sequence-scoped — when an EditOp
/// lands, the engine rebuilds it from the post-edit sequence and
/// publishes via [`vance_audio::AudioMixer::publish_snapshot`].
///
/// `solo_anywhere` is precomputed (one pass over tracks) so the fold
/// thread's per-clip lookup never has to sweep the tracks map.
fn build_mixer_snapshot(sequence: &vance_timeline::Sequence) -> vance_audio::MixerSnapshot {
    let mut snap = vance_audio::MixerSnapshot::empty();
    let mut solo_anywhere = false;
    for track in &sequence.tracks {
        snap.tracks.insert(track.id.as_uuid(), track_to_mix(track));
        if track.solo {
            solo_anywhere = true;
        }
        // Only audio tracks contribute clips to the mixer (video
        // tracks are blended, not summed). This keeps the clips map
        // small.
        if track.kind == TrackKind::Audio {
            for clip in &track.clips {
                snap.clips
                    .insert(clip.id.as_uuid(), clip_to_mix(clip, track));
            }
        }
    }
    snap.solo_anywhere = solo_anywhere;
    // GOAT-4: per-bus fader gain + routing for the output-bus chain.
    for bus in &sequence.buses {
        snap.buses.insert(
            bus.id.as_uuid(),
            vance_audio::BusMix {
                gain_linear: vance_audio::db_to_linear(bus.gain_db),
                pan_linear: vance_audio::bus_balance(bus.pan),
                mute: bus.mute,
                output_bus: match bus.output {
                    vance_timeline::BusOutput::Master => None,
                    vance_timeline::BusOutput::Bus(id) => Some(id.as_uuid()),
                },
                audio_effects: bus.inserts.iter().map(translate_audio_effect).collect(),
                sends: bus.sends.iter().map(translate_send).collect(),
            },
        );
    }
    // Phase 7.4: the master fader value lives on the Sequence. Empty
    // / freshly-deserialized pre-7.4 sequences default to 0 dB →
    // unity, so behavior is unchanged for old bundles.
    snap.master_gain_linear = vance_audio::db_to_linear(sequence.master_gain_db);
    snap
}

/// Walk the sequence and gather one [`vance_audio::AudioSource`] per
/// audio clip whose live window intersects `[from_seconds,
/// from_seconds + sched::PRELOAD_SECONDS]`. This includes:
///
/// 1. Clips already audible at the start time (`timeline_in <=
///    from_seconds < timeline_in + duration`) — they're spawned with
///    a source offset that seeks INTO the clip so playback resumes
///    from the playhead.
/// 2. Clips that will become audible within the preload window
///    (`from_seconds < timeline_in <= from_seconds +
///    PRELOAD_SECONDS`) — they're spawned with `from_seconds =
///    source_in`, and the mixer's per-source timeline gate keeps them
///    silent until `t_frame >= clip.timeline_in`.
///
/// Phase 7.4 M4: this is the structural piece of the seek-onto-edge
/// bounded-silence SLO. Cold seek lands these synchronous spawns
/// BEFORE the master frame counter starts ticking, so a user seek
/// landing on or near a clip boundary produces audible audio in ≤1
/// device buffer — the only latency is cpal's own pre-roll.
fn collect_audio_sources_at(
    sequence: &vance_timeline::Sequence,
    project: &vance_timeline::Project,
    from_seconds: f64,
) -> Vec<vance_audio::AudioSource> {
    let mut sources = Vec::new();
    let preload_end = from_seconds + sched::PRELOAD_SECONDS;
    for track in &sequence.tracks {
        if track.kind != TrackKind::Audio || !track.enabled {
            continue;
        }
        for clip in &track.clips {
            let t_in = rt_to_seconds(clip.timeline_in);
            let dur = rt_to_seconds(clip.duration());
            let t_out = t_in + dur;
            // Skip clips entirely behind the playhead.
            if t_out <= from_seconds {
                continue;
            }
            // Skip clips beyond the preload window.
            if t_in > preload_end {
                continue;
            }
            let media = match project.media.get(&clip.media) {
                Some(m) => m,
                None => continue,
            };
            if !media.has_audio_track {
                continue;
            }
            let source_in_sec = rt_to_seconds(clip.source_in);
            // For clips already overlapping the playhead, seek into the
            // source so first sample = current playhead. For future-
            // preload clips, start at source_in and let the timeline
            // gate hold them silent until the playhead arrives.
            let offset = (from_seconds - t_in).max(0.0);
            sources.push(vance_audio::AudioSource {
                clip_id: clip.id.as_uuid(),
                path: media.path.clone(),
                from_seconds: source_in_sec + offset,
            });
        }
    }
    sources
}

/// Spawn the multi-source [`vance_audio::AudioMixer`] on an owner
/// thread and return the owner handle, the master-frame-derived
/// [`AudioSync`], and the snapshot bus handle the engine stores for
/// publishing live updates.
fn start_audio_mix(
    sequence: &vance_timeline::Sequence,
    project: &vance_timeline::Project,
    from_seconds: f64,
) -> Result<
    (
        AudioMixerOwner,
        AudioSync,
        Arc<arc_swap::ArcSwap<vance_audio::MixerSnapshot>>,
    ),
    String,
> {
    let bin = locate_audio_binary().map_err(|e| format!("{e:?}"))?;
    let initial_audio_sources = collect_audio_sources_at(sequence, project, from_seconds);
    let initial_snapshot = build_mixer_snapshot(sequence);
    let initial_sched_clips = build_sched_clips(sequence, project);
    let sched_clips =
        Arc::new(arc_swap::ArcSwap::from_pointee(initial_sched_clips));

    // Scheduler ↔ fold thread channel. Unbounded (mpsc::channel) because
    // scheduler ticks emit at most ~MAX_CONCURRENT_SOURCES commands per
    // tick; the fold thread drains in a tight try_recv loop.
    let (command_tx, command_rx) =
        std::sync::mpsc::channel::<vance_audio::SourceCommand>();
    let stats = Arc::new(vance_audio::MixerStats::default());

    // Probe the cpal device's sample rate up-front so we can size the
    // per-source rings to match what `AudioMixer::start` will see.
    let probed_sample_rate = vance_audio::probe_default_sample_rate()
        .map_err(|e| format!("audio probe: {e}"))?;
    let per_source_capacity = vance_audio::per_source_ring_capacity(probed_sample_rate);

    // Phase 7.4 M4: pre-spawn the initial source set on THIS thread (the
    // calling thread, not the cpal-serving fold thread). This is the
    // structural piece of the seek-onto-edge SLO: when the user seeks,
    // every clip whose audio matters in the next PRELOAD seconds is
    // already spawned (and its puller is already pulling) before cpal
    // ever asks for samples. `initial_live_ids` becomes the scheduler's
    // seeded `live` set, preventing the first tick from issuing a
    // redundant re-spawn for the same clips.
    let mut initial_streams: Vec<vance_audio::SourceStream> =
        Vec::with_capacity(initial_audio_sources.len());
    let mut initial_live_ids: std::collections::HashSet<uuid::Uuid> =
        std::collections::HashSet::with_capacity(initial_audio_sources.len());
    for src in &initial_audio_sources {
        match vance_audio::spawn_source(&bin, src, per_source_capacity, &stats) {
            Ok(s) => {
                initial_live_ids.insert(src.clip_id);
                initial_streams.push(s);
            }
            Err(e) => {
                tracing::error!(?e, clip_id = %src.clip_id, "initial source spawn failed");
            }
        }
    }

    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<
        Result<
            (
                AudioSync,
                Arc<arc_swap::ArcSwap<vance_audio::MixerSnapshot>>,
                Arc<std::sync::atomic::AtomicI64>,
                u32,
                Arc<arc_swap::ArcSwap<vance_audio::MeterSnapshot>>,
            ),
            String,
        >,
    >(1);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    let handle = std::thread::Builder::new()
        .name("vance-engine.audio-mixer-owner".into())
        .spawn(move || {
            let mixer = match vance_audio::AudioMixer::start(
                initial_streams,
                command_rx,
                initial_snapshot,
                from_seconds,
                locate_plugin_host_binary(),
            ) {
                Ok(m) => m,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("{e}")));
                    return;
                }
            };
            // Master-frame-derived AudioSync: the mixer counts
            // device-sample frames since stream start; one frame =
            // 1/sample_rate seconds. Time base = (1, sample_rate)
            // makes seconds = pts * 1 / sample_rate.
            let sync = AudioSync {
                current_pts: mixer.master_frame_handle(),
                time_base: (1, mixer.device_sample_rate() as i64),
            };
            let snapshot_handle = mixer.snapshot_handle();
            let master_frame = mixer.master_frame_handle();
            let sample_rate = mixer.device_sample_rate();
            let meter_handle = mixer.meter_handle();
            if ready_tx
                .send(Ok((sync, snapshot_handle, master_frame, sample_rate, meter_handle)))
                .is_err()
            {
                return;
            }
            while !stop_clone.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(mixer); // cpal stops, fold + puller threads join
        })
        .map_err(|e| format!("spawn audio mixer owner: {e}"))?;

    let (sync, snapshot_handle, master_frame, sample_rate, meter_handle) = ready_rx
        .recv_timeout(Duration::from_secs(10))
        .map_err(|_| "audio mixer owner thread did not report ready in 10s".to_string())??;

    // Spawn the scheduler thread once the mixer is ready. The scheduler
    // owns the Sender; the fold thread holds the Receiver. The scheduler
    // does the slow `spawn_source` for every runtime Add — the fold
    // thread only receives ready-to-mix [`SourceStream`]s.
    let scheduler_stop = Arc::new(AtomicBool::new(false));
    let sched_clips_for_thread = sched_clips.clone();
    let sched_stop_for_thread = scheduler_stop.clone();
    let sched_stats = stats.clone();
    let sched_bin = bin.clone();
    let scheduler_handle = std::thread::Builder::new()
        .name("vance-engine.audio-scheduler".into())
        .spawn(move || {
            scheduler_loop(
                sched_clips_for_thread,
                command_tx,
                master_frame,
                sample_rate,
                from_seconds,
                sched_stats,
                sched_stop_for_thread,
                sched_bin,
                per_source_capacity,
                initial_live_ids,
            );
        })
        .map_err(|e| format!("spawn audio scheduler: {e}"))?;

    Ok((
        AudioMixerOwner {
            stop,
            handle: Some(handle),
            sched_clips,
            scheduler_handle: Some(scheduler_handle),
            scheduler_stop,
            stats,
            meter: meter_handle,
        },
        sync,
        snapshot_handle,
    ))
}

/// Does this EditOp affect mixer state (tracks gain/mute/solo, clip
/// gain/mute, or topology changes that move/add/remove clips on
/// audio tracks)? The republish path treats any clip-or-track-topology
/// change as mixer-affecting: a deleted clip removes its ClipMix; an
/// inserted clip needs one; trim/move don't change ClipMix shape but
/// the engine republishes anyway to keep the contract conservative.
fn op_touches_mixer(op: &EditOp) -> bool {
    match op {
        EditOp::SetTrackMute { .. }
        | EditOp::SetTrackSolo { .. }
        | EditOp::SetTrackGainDb { .. }
        | EditOp::SetClipMute { .. }
        | EditOp::SetClipGainDb { .. }
        | EditOp::SetMasterGainDb { .. }
        | EditOp::SetTrackGainKeyframes { .. }
        | EditOp::SetClipGainKeyframes { .. }
        | EditOp::SetClipFadeIn { .. }
        | EditOp::SetClipFadeOut { .. }
        | EditOp::SetTrackPan { .. }
        | EditOp::SetClipPan { .. }
        | EditOp::AddAudioEffect { .. }
        | EditOp::RemoveAudioEffect { .. }
        | EditOp::MoveAudioEffect { .. }
        | EditOp::UpdateAudioEffect { .. }
        | EditOp::AddTrack { .. }
        | EditOp::RemoveTrack { .. }
        | EditOp::InsertClip { .. }
        | EditOp::RippleInsertClip { .. }
        | EditOp::OverwriteClip { .. }
        | EditOp::RemoveClip { .. }
        | EditOp::RippleRemoveClip { .. }
        | EditOp::Slip { .. }
        | EditOp::Slide { .. }
        | EditOp::TrimIn { .. }
        | EditOp::TrimOut { .. }
        | EditOp::Split { .. }
        // GOAT-6: magnetic edits move/add/remove clips → audio re-schedules.
        | EditOp::MagneticMoveClip { .. }
        | EditOp::AppendClip { .. }
        | EditOp::MagneticRippleDelete { .. }
        | EditOp::MagneticRippleInsert { .. }
        // GOAT-4: bus/send routing changes the audio sum.
        | EditOp::AddBus { .. }
        | EditOp::RemoveBus { .. }
        | EditOp::SetBusGainDb { .. }
        | EditOp::SetBusPan { .. }
        | EditOp::SetBusMute { .. }
        | EditOp::SetBusOutput { .. }
        | EditOp::RouteTrackToBus { .. }
        | EditOp::AddTrackSend { .. }
        | EditOp::RemoveTrackSend { .. }
        | EditOp::AddBusInsert { .. }
        | EditOp::RemoveBusInsert { .. }
        | EditOp::MoveBusInsert { .. }
        | EditOp::UpdateBusInsert { .. }
        | EditOp::AddBusSend { .. }
        | EditOp::RemoveBusSend { .. } => true,
        // Compound republishes once after the whole batch lands.
        EditOp::Compound { .. } => true,
        _ => false,
    }
}

/// GOAT-8 — a STABLE, app-owned directory for Vance-generated media
/// (isolated stems, renders) that must survive across sessions. Unlike the
/// OS temp dir (which the reaper wipes and which the isolation runner
/// clears on each run), files here persist so a project that references
/// them reopens intact. Created on demand.
pub(crate) fn persistent_media_dir() -> std::io::Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join("Library/Application Support"))
            .unwrap_or_else(std::env::temp_dir)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .unwrap_or_else(std::env::temp_dir)
    };
    let dir = base.join("Vance").join("media");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// GOAT-8 — measure a media file's whole-file integrated loudness (LUFS).
/// Convenience wrapper over [`measure_loudness_range`].
pub(crate) fn measure_loudness(media_path: &std::path::Path) -> Option<f32> {
    measure_loudness_range(media_path, 0.0, None)
}

/// GOAT-8 — measure integrated loudness (LUFS) over `[start_sec, start_sec
/// + dur_sec)` of a media file's audio (or the whole file when `dur_sec`
/// is `None`) via the BS.1770 meter. Slow (decodes) — run OFF the engine
/// lock. Measuring only the clip's active source range makes dialogue
/// leveling correct for trimmed clips.
pub(crate) fn measure_loudness_range(
    media_path: &std::path::Path,
    start_sec: f64,
    dur_sec: Option<f64>,
) -> Option<f32> {
    let bin = locate_audio_binary().ok()?;
    let mut audio = vance_audio::AudioClient::spawn(&bin).ok()?;
    let info = audio.open(media_path.to_string_lossy().into_owned()).ok()?;
    // Seek to the clip's in-point (in the stream time base).
    let (tb_n, tb_d) = info.time_base;
    let from_pts = if start_sec > 0.0 && tb_n != 0 {
        (start_sec * tb_d as f64 / tb_n as f64).round() as i64
    } else {
        0
    };
    if audio.start_stream(from_pts, 8192).is_err() {
        let _ = audio.shutdown();
        return None;
    }
    let mut meter = vance_audio::loudness::LoudnessMeter::new(info.sample_rate as f32);
    // Stop after the clip's used duration (in frames), so we don't measure
    // material past the clip's out-point.
    let max_frames = dur_sec.map(|d| (d.max(0.0) * info.sample_rate as f64).ceil() as u64);
    let mut frames: u64 = 0;
    'decode: loop {
        match audio.next_chunk() {
            Ok(Some(chunk)) => {
                for frame in chunk.samples.chunks_exact(2) {
                    meter.ingest(frame[0], frame[1]);
                    frames += 1;
                    if let Some(cap) = max_frames {
                        if frames >= cap {
                            break 'decode;
                        }
                    }
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    let _ = audio.stop_stream();
    let _ = audio.shutdown();
    let lufs = meter.stats().lufs_i;
    lufs.is_finite().then_some(lufs)
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
            // Next to the running exe (production layout).
            let cand = dir.join(exe_name);
            if cand.exists() {
                return Ok(cand);
            }
            tried.push(cand);
            // When running under `cargo test`, the test binary lives in
            // target/<profile>/deps/ — the sidecar is one level up.
            if let Some(parent) = dir.parent() {
                let cand = parent.join(exe_name);
                if cand.exists() {
                    return Ok(cand);
                }
                tried.push(cand);
            }
        }
    }
    // Dev fallback: relative to the workspace cwd (mirrors
    // locate_encoder_binary / locate_decoder_binary).
    for c in [
        PathBuf::from("target/debug").join(exe_name),
        PathBuf::from("target/release").join(exe_name),
    ] {
        if c.exists() {
            return Ok(c);
        }
        tried.push(c);
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
    /// GOAT-5 — an animated Motion Graphics Template. Unlike `Title`, the
    /// loop RE-RASTERIZES `frame` each tick at the elapsed time so the
    /// rig animates. Shares the engine's font cache via `Arc`.
    MoGraph {
        /// Shared render engine (font cache + rasterizer).
        engine: std::sync::Arc<vance_mograph::MographEngine>,
        /// The placed template instance to render each tick.
        instance: Box<vance_mograph::MographInstance>,
        /// Scratch frame, re-rendered + re-pts'd every tick.
        frame: vance_decoder::FrameData,
        /// Playback frame rate.
        fps: f64,
        /// Remaining duration to play from the resume offset.
        duration_seconds: f64,
        /// Clip-relative time playback resumed at; added to the loop's
        /// elapsed time so the rig animates from the playhead, not t=0.
        start_offset_sec: f64,
    },
    /// GOAT-12 — an animated Fusion composition. Like `MoGraph`, the loop
    /// RE-RASTERIZES the node graph each tick at the elapsed time. Shares
    /// the compositor's font cache via `Arc`.
    Fusion {
        /// Shared compositor (font cache + rasterizer).
        compositor: std::sync::Arc<vance_fusion::Compositor>,
        /// The composition to render each tick.
        comp: Box<vance_fusion::Comp>,
        /// Scratch frame, re-rendered + re-pts'd every tick.
        frame: vance_decoder::FrameData,
        /// Playback frame rate.
        fps: f64,
        /// Remaining duration to play from the resume offset.
        duration_seconds: f64,
        /// Clip-relative time playback resumed at; added to the loop's
        /// elapsed time so the graph animates from the playhead, not t=0.
        start_offset_sec: f64,
    },
}

impl PlaybackPrimary {
    /// `(time_base_num, time_base_den)` for converting the primary's
    /// PTS values back to seconds. Title primaries fake one at
    /// `(1, 1000)` so milliseconds round-trip cleanly.
    fn time_base(&self) -> (i64, i64) {
        match self {
            PlaybackPrimary::Decoder { info, .. } => info.time_base,
            PlaybackPrimary::Title { .. }
            | PlaybackPrimary::MoGraph { .. }
            | PlaybackPrimary::Fusion { .. } => (1, 1000),
        }
    }

    /// Initial PTS — 0 for synthetic, the stream's chosen start
    /// otherwise. Logged only; the loop's own per-frame timing
    /// derives from the actual frame PTS.
    fn from_pts(&self) -> i64 {
        match self {
            PlaybackPrimary::Decoder { from_pts, .. } => *from_pts,
            PlaybackPrimary::Title { .. }
            | PlaybackPrimary::MoGraph { .. }
            | PlaybackPrimary::Fusion { .. } => 0,
        }
    }

    /// On thread exit, the engine wants the DecoderClient back so it
    /// can reuse the open file for single-shot decodes. Title
    /// primaries have no DecoderClient — the engine's slot stays as
    /// it was before playback.
    fn into_decoder(self) -> Option<DecoderClient> {
        match self {
            PlaybackPrimary::Decoder { client, .. } => Some(client),
            PlaybackPrimary::Title { .. }
            | PlaybackPrimary::MoGraph { .. }
            | PlaybackPrimary::Fusion { .. } => None,
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
            PlaybackPrimary::MoGraph {
                engine,
                instance,
                frame,
                fps,
                duration_seconds,
                start_offset_sec,
            } => {
                // GOAT-5 — re-rasterize the rig at the elapsed clip time so
                // the template animates, then present like a title frame.
                let tick_dt_ms = (1000.0 / fps.max(1.0)).round() as i64;
                let elapsed_sec = (title_tick_ms as f64) / 1000.0;
                if elapsed_sec >= *duration_seconds {
                    ended_at_eos.store(true, Ordering::Release);
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
                // Render at the ABSOLUTE clip time = resume offset + elapsed,
                // so starting playback mid-clip continues the animation.
                *frame = vance_render::render_mograph_yuv420(
                    engine,
                    instance,
                    *start_offset_sec + elapsed_sec,
                    *fps,
                    *duration_seconds,
                );
                frame.pts = title_tick_ms;
                title_tick_ms = title_tick_ms.saturating_add(tick_dt_ms);
                frame.clone()
            }
            PlaybackPrimary::Fusion {
                compositor,
                comp,
                frame,
                fps,
                duration_seconds,
                start_offset_sec,
            } => {
                // GOAT-12 — re-rasterize the node graph at the elapsed clip
                // time so the comp animates, then present like a title frame.
                let tick_dt_ms = (1000.0 / fps.max(1.0)).round() as i64;
                let elapsed_sec = (title_tick_ms as f64) / 1000.0;
                if elapsed_sec >= *duration_seconds {
                    ended_at_eos.store(true, Ordering::Release);
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
                *frame = vance_render::render_fusion_yuv420(
                    compositor,
                    comp,
                    *start_offset_sec + elapsed_sec,
                    &std::collections::HashMap::new(),
                );
                frame.pts = title_tick_ms;
                title_tick_ms = title_tick_ms.saturating_add(tick_dt_ms);
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
            // Live playback: matte solo is a paused-only viewer state.
            let grades = eval_clip_grades(&clip_effects, clip_t, None);
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

/// Phase 8 — high-level options for [`Engine::render_sequence`].
#[derive(Clone, Debug)]
pub struct RenderOptions {
    /// Destination container path. Extension picks the container.
    pub output_path: String,
    /// Encoder preset (e.g. ProRes 422 HQ — the exit-gate target).
    pub preset: vance_encoder::EncoderPreset,
    /// Output resolution. Defaults to the sequence's nominal raster
    /// when `None`.
    pub width: Option<u32>,
    /// Output resolution.
    pub height: Option<u32>,
    /// Render-and-replace target track. `None` = full export.
    pub render_and_replace_track: Option<vance_timeline::TrackId>,
    /// Start time in seconds. `None` = sequence start.
    pub start_seconds: Option<f64>,
    /// End time in seconds. `None` = sequence end.
    pub end_seconds: Option<f64>,
    /// HDR10 export. When `true`, render through a 10-bit `Rgba16Float`
    /// pipeline with Rec.2020-PQ output and encode HEVC Main10 tagged with
    /// HDR10 mastering metadata. `#[serde]`-less (an engine input struct);
    /// defaults to `false` (SDR) via the `Default` derive path callers use.
    pub hdr: bool,
}

/// Phase 8 — render outcome surfaced to the UI.
#[derive(Clone, Debug)]
pub struct RenderOutcome {
    /// Final output path the encoder wrote.
    pub output_path: String,
    /// Number of video frames pushed.
    pub video_frames: u64,
    /// Number of audio sample-pairs pushed.
    pub audio_samples: u64,
    /// Wall-clock seconds the encode took.
    pub elapsed_seconds: f64,
}

impl Engine {
    /// Phase 8 — render the current sequence to a file via the
    /// sandboxed `vance-encoder` sidecar.
    ///
    /// v1 scope: synthesises a frame-index test pattern and silent
    /// audio for every frame, exercising the full
    /// IPC → ffmpeg → mux pipeline. The renderer-to-encoder
    /// integration (real wgpu offscreen render target + texture
    /// readback) lands in 8.5 alongside the render queue UI; until
    /// then the sidecar's output is a colour-cycling stripe instead
    /// of the user's actual sequence content, but the file format,
    /// duration, audio channel count + sample rate, and container
    /// are all correct.
    pub fn render_sequence(&mut self, opts: RenderOptions) -> Result<RenderOutcome, EngineError> {
        let started = Instant::now();

        // 1. Guard against running while playback owns the primary
        //    decoder. The integration survey flagged this risk
        //    explicitly (Phase 8.5 plan / risks).
        if self.playback.is_some() {
            let _ = self.stop_playback();
        }

        // 2. Snapshot sequence shape (frame-rate, duration, walk plan).
        let (frame_rate_num, frame_rate_den, seq_duration) = {
            let seq = self.current_sequence_ref()?;
            let seq_duration_r = seq.duration().0;
            let seq_duration = seq_duration_r.numer() as f64 / seq_duration_r.denom() as f64;
            let fr_r = seq.frame_rate.0;
            let frame_rate_num = fr_r.numer() as u32;
            let frame_rate_den = fr_r.denom() as u32;
            (frame_rate_num, frame_rate_den, seq_duration)
        };
        // Round output dimensions DOWN to even: 4:2:0 codecs (H.264 / HEVC /
        // AV1) require even width AND height for their subsampled chroma
        // planes, and even the synthetic YUV path crops to even. Clamp to a
        // minimum of 2 so a tiny/odd request still yields a valid frame.
        let width = (opts.width.unwrap_or(1920) & !1).max(2);
        let height = (opts.height.unwrap_or(1080) & !1).max(2);
        let start_s = opts.start_seconds.unwrap_or(0.0).max(0.0);
        let end_s = opts.end_seconds.unwrap_or(seq_duration).min(seq_duration);
        if end_s <= start_s {
            return Err(EngineError::Project(format!(
                "render range empty: start={start_s} end={end_s}"
            )));
        }
        let total_frames = ((end_s - start_s) * frame_rate_num as f64 / frame_rate_den as f64)
            .ceil() as u64;
        let sample_rate: u32 = 48_000;

        // 3. Ensure a headless render state. install_viewport's render
        //    state works (it was built via Renderer::new_with against a
        //    surface but the renderer is surface-agnostic afterwards).
        //    Fall back to a fresh truly-headless Renderer if none exists.
        //
        // HDR export needs a DEDICATED 10-bit renderer (Rgba16Float target)
        // with Rec.2020-PQ output, so it can't reuse the SDR preview renderer.
        // We temporarily swap it into `self.render_state` (so the shared
        // `apply_clip_render_params` configures it) + set the output-space
        // override, and restore both when the render returns.
        let saved_render_state = self.render_state.clone();
        let (render_state_arc, video_pixel_format, video_bpp) = if opts.hdr {
            let renderer = Renderer::new(RendererConfig {
                target_format: wgpu::TextureFormat::Rgba16Float,
                ..RendererConfig::default()
            })
            .map_err(|e| EngineError::Project(format!("hdr renderer: {e}")))?;
            let arc = Arc::new(Mutex::new(RenderState {
                renderer,
                surface_config: None,
                source_surface_config: None,
            }));
            self.render_state = Some(arc.clone());
            self.render_output_space_override = Some(vance_color::ColorSpace::Rec2020Pq);
            (arc, vance_encoder::VideoPixelFormat::Rgba64Le, 8usize)
        } else if let Some(rs) = self.render_state.as_ref() {
            (rs.clone(), vance_encoder::VideoPixelFormat::Rgba8, 4usize)
        } else {
            let renderer = Renderer::new(RendererConfig::default())
                .map_err(|e| EngineError::Project(format!("headless renderer: {e}")))?;
            let arc = Arc::new(Mutex::new(RenderState {
                renderer,
                surface_config: None,
                source_surface_config: None,
            }));
            self.render_state = Some(arc.clone());
            (arc, vance_encoder::VideoPixelFormat::Rgba8, 4usize)
        };
        // HDR10 mastering metadata (Rec.2020 PQ, 1000-nit mastering display).
        let hdr_meta = if opts.hdr {
            Some(vance_encoder::HdrMetadata::hdr10(1000.0, 0.0001, 1000, 400))
        } else {
            None
        };

        // 4. Spawn encoder + open the output. Same path as v1.
        let bin = locate_encoder_binary()?;
        let mut client = vance_encoder::EncoderClient::spawn(&bin)
            .map_err(|e| EngineError::Project(format!("encoder spawn: {e}")))?;
        client
            .open(
                opts.output_path.clone(),
                Some(vance_encoder::VideoConfig {
                    width,
                    height,
                    frame_rate_num,
                    frame_rate_den,
                    pixel_format: video_pixel_format,
                    preset: opts.preset.clone(),
                    time_base_num: frame_rate_den,
                    time_base_den: frame_rate_num,
                    hdr: hdr_meta,
                }),
                Some(vance_encoder::AudioConfig {
                    sample_rate,
                    channels: 2,
                    sample_format: vance_encoder::AudioSampleFormat::F32Interleaved,
                    codec: vance_encoder::AudioCodec::PcmS24,
                    time_base_num: 1,
                    time_base_den: sample_rate,
                }),
            )
            .map_err(|e| EngineError::Project(format!("encoder open: {e}")))?;

        // 5. Reset per-frame renderer state to identity so any prior
        //    preview doesn't bleed into the export. Mirrors the block
        //    at decode_and_render_at:1644-1655.
        {
            let mut rs = render_state_arc.lock();
            rs.renderer.set_opacity(1.0);
            rs.renderer.set_transform([0.0, 0.0], [1.0, 1.0], 0.0);
            rs.renderer.set_brightness_contrast(0.0, 1.0);
            rs.renderer.set_crop(0.0, 0.0, 0.0, 0.0);
            rs.renderer.set_mix_factor(0.0);
            rs.renderer.set_chroma_key([0.0, 1.0, 0.0], 0.15, 0.05, 0.0);
            rs.renderer.set_luma_key(0.5, 1.0, 0.05, 0.0);
            rs.renderer.set_grades(&[]);
        }

        // 6. Pre-allocate the black fallback (used for SceneAt::Empty +
        //    title clips in v1 + decode errors), in the wire format's
        //    bytes-per-pixel. Opaque black: RGB=0, alpha=max.
        let pixel_count = (width as usize) * (height as usize) * video_bpp;
        let mut black = vec![0u8; pixel_count];
        if video_bpp == 8 {
            // rgba64le: alpha is the last u16 (0xFFFF), little-endian.
            for px in (0..pixel_count).step_by(8) {
                black[px + 6] = 0xFF;
                black[px + 7] = 0xFF;
            }
        } else {
            for px in (0..pixel_count).step_by(4) {
                black[px + 3] = 255;
            }
        }

        // 7. Frame loop. For each output frame: transition_at / scene_at
        //    → decode → upload → effects → readback → push_video.
        //
        // Wrapped in an IIFE so an HDR export's temporary render-state swap +
        // output-space override are ALWAYS restored (below), even if the loop
        // or the encoder finish errors out.
        let export_result: Result<(u64, u64), EngineError> = (|| -> Result<(u64, u64), EngineError> {
        for f in 0..total_frames {
            let timeline_seconds =
                start_s + (f as f64) * (frame_rate_den as f64) / (frame_rate_num as f64);
            let timeline_t = seconds_to_rt(timeline_seconds);

            // Resolve what's on screen from ONE project borrow. An
            // active transition (dissolve window) takes priority over a
            // plain clip, mirroring the live renderer. `transition_plan`
            // carries (from_media, from_clip, from_src_pts, from_src_in,
            // to_media, to_src_pts, mix). The clip tuple is the
            // single-clip fall-through.
            #[allow(clippy::type_complexity)]
            let (
                transition_plan,
                active_media,
                active_source_pts_seconds,
                clip_effects,
                clip_source_in_seconds,
                active_clip_id,
            ): (
                // (transition, from_src_in, to_src_in)
                Option<(vance_timeline::ActiveTransition, f64, f64)>,
                Option<MediaItemId>,
                f64,
                Vec<vance_timeline::Effect>,
                f64,
                Option<vance_timeline::ClipId>,
            ) = {
                let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
                let seq_id = self.current_sequence_id.ok_or(EngineError::NoCurrentSequence)?;
                let seq = project.sequence(seq_id).ok_or(EngineError::SequenceNotFound)?;
                if let Some(tr) = vance_timeline::transition_at(seq, timeline_t) {
                    let src_in = |clip_id: vance_timeline::ClipId| {
                        seq.tracks
                            .iter()
                            .find_map(|t| {
                                t.clips
                                    .iter()
                                    .find(|cl| cl.id == clip_id)
                                    .map(|cl| rt_to_seconds(cl.source_in))
                            })
                            .unwrap_or(0.0)
                    };
                    let from_src_in = src_in(tr.from.clip_id);
                    let to_src_in = src_in(tr.to.clip_id);
                    (Some((tr, from_src_in, to_src_in)), None, 0.0, Vec::new(), 0.0, None)
                } else {
                    match vance_timeline::scene_at(seq, timeline_t) {
                        vance_timeline::SceneAt::Clip(c) => {
                            let (effects, source_in) = seq
                                .tracks
                                .iter()
                                .find_map(|t| {
                                    t.clips.iter().find(|cl| cl.id == c.clip_id).map(|cl| {
                                        (cl.effects.clone(), rt_to_seconds(cl.source_in))
                                    })
                                })
                                .unwrap_or_default();
                            (
                                None,
                                Some(c.media),
                                rt_to_seconds(c.source_pts),
                                effects,
                                source_in,
                                Some(c.clip_id),
                            )
                        }
                        vance_timeline::SceneAt::Empty => {
                            (None, None, 0.0, Vec::new(), 0.0, None)
                        }
                    }
                }
            };

            let mut rgba: Option<Vec<u8>> = None;
            if let Some((tr, from_src_in, to_src_in)) = transition_plan {
                let from_media = tr.from.media;
                let from_src_pts = rt_to_seconds(tr.from.source_pts);
                let to_media = tr.to.media;
                let to_src_pts = rt_to_seconds(tr.to.source_pts);
                match &tr.kind {
                    vance_timeline::TransitionKind::Dissolve => {
                        // Cross-dissolve: decode A → A-side, B → B-side
                        // (the B decode may swap the primary decoder's open
                        // file, but A's frame is already on the GPU), apply
                        // A's native effects, set the mix factor, read back.
                        if let Some(frame_a) =
                            self.decode_clip_framedata(from_media, from_src_pts, from_src_in)
                        {
                            {
                                let mut rs = render_state_arc.lock();
                                let _ = rs.renderer.upload_frame(&frame_a);
                            }
                            if let Some(frame_b) =
                                self.decode_clip_framedata(to_media, to_src_pts, to_src_in)
                            {
                                let mut rs = render_state_arc.lock();
                                let _ = rs.renderer.upload_frame_b(&frame_b);
                            }
                            let from_clip_t = from_src_pts - from_src_in;
                            self.apply_clip_render_params(tr.from.clip_id, from_media, from_clip_t);
                            let mut rs = render_state_arc.lock();
                            rs.renderer.set_mix_factor(tr.mix);
                            rgba = if opts.hdr {
                                rs.renderer.render_to_rgba64_buffer(width, height).ok()
                            } else {
                                rs.renderer.render_to_rgba_buffer(width, height).ok()
                            };
                        }
                    }
                    vance_timeline::TransitionKind::Wasm(plugin_id) => {
                        // Render A and B fully + independently, then let the
                        // WASM plugin blend B into A on the CPU.
                        let a = self
                            .render_clip_to_rgba(
                                tr.from.clip_id,
                                from_media,
                                from_src_pts,
                                from_src_in,
                                width,
                                height,
                            )
                            .map(|(r, _)| r);
                        let b = self
                            .render_clip_to_rgba(
                                tr.to.clip_id,
                                to_media,
                                to_src_pts,
                                to_src_in,
                                width,
                                height,
                            )
                            .map(|(r, _)| r);
                        if let (Some(mut a), Some(b)) = (a, b) {
                            let to_clip_t = to_src_pts - to_src_in;
                            self.apply_wasm_transition(
                                &mut a, &b, width, height, plugin_id, &tr.params, tr.mix, to_clip_t,
                            );
                            rgba = Some(a);
                        }
                    }
                }
            } else if let (Some(media_id), Some(clip_id)) = (active_media, active_clip_id) {
                // Single clip: decode (File or Title), apply native
                // effects, read back.
                let entry_clip_t = active_source_pts_seconds - clip_source_in_seconds;
                if let Some(frame) = self.decode_clip_framedata(
                    media_id,
                    active_source_pts_seconds,
                    clip_source_in_seconds,
                ) {
                    {
                        let mut rs = render_state_arc.lock();
                        let _ = rs.renderer.upload_frame(&frame);
                        rs.renderer.set_mix_factor(0.0);
                    }
                    self.apply_clip_render_params(clip_id, media_id, entry_clip_t);
                    let mut rs = render_state_arc.lock();
                    let readback = if opts.hdr {
                        rs.renderer.render_to_rgba64_buffer(width, height)
                    } else {
                        rs.renderer.render_to_rgba_buffer(width, height)
                    };
                    match readback {
                        Ok(buf) => rgba = Some(buf),
                        Err(e) => {
                            tracing::warn!(?e, frame = f, "readback failed; black");
                        }
                    }
                }
            }
            let mut push = rgba.unwrap_or_else(|| black.clone());
            // Phase 10.5 — run the clip's WASM plugin effects over the
            // composited RGBA before encoding. No-op when the clip has
            // no `wasm:` effects. Skipped on the HDR path: those CPU plugins
            // operate on 8-bit RGBA and can't interpret the 10-bit rgba64le
            // buffer (a documented HDR limitation).
            if !clip_effects.is_empty() && !opts.hdr {
                let clip_t = active_source_pts_seconds - clip_source_in_seconds;
                self.apply_wasm_effects(&mut push, width, height, &clip_effects, clip_t);
            }
            client
                .push_video(f as i64, push)
                .map_err(|e| EngineError::Project(format!("encoder push_video: {e}")))?;
        }

        // 8. Real mixed audio — offline bounce of the export range
        //    through the full mixer model (per-clip gain/fades/pan,
        //    per-track DSP, master), then push to the encoder in
        //    chunks. Falls back to silence only if the bounce fails
        //    (e.g. no audio sidecar), so the file is always valid.
        let mixed = match self.offline_audio_bounce(start_s, end_s, sample_rate) {
            Ok(samples) => samples,
            Err(e) => {
                tracing::warn!(?e, "audio bounce failed; exporting silent track");
                let total = ((end_s - start_s) * sample_rate as f64).round() as usize;
                vec![0f32; total * 2]
            }
        };
        // Push in ~0.1 s chunks (interleaved stereo).
        let push_chunk_frames = (sample_rate as usize / 10).max(1);
        let mut pos = 0usize;
        while pos < mixed.len() {
            let end = (pos + push_chunk_frames * 2).min(mixed.len());
            client
                .push_audio(0, mixed[pos..end].to_vec())
                .map_err(|e| EngineError::Project(format!("encoder push_audio: {e}")))?;
            pos = end;
        }

        let (video_frames, audio_samples) = client
            .finish()
            .map_err(|e| EngineError::Project(format!("encoder finish: {e}")))?;
        let _ = client.shutdown();
        Ok((video_frames, audio_samples))
        })(); // end IIFE

        // Restore SDR render state after an HDR export (the HDR renderer is a
        // dedicated Rgba16Float instance; preview needs the original). Always
        // runs, whether the export succeeded or errored.
        if opts.hdr {
            self.render_state = saved_render_state;
            self.render_output_space_override = None;
        }

        let (video_frames, audio_samples) = export_result?;
        Ok(RenderOutcome {
            output_path: opts.output_path,
            video_frames,
            audio_samples,
            elapsed_seconds: started.elapsed().as_secs_f64(),
        })
    }
}

/// Phase 8.7 — Render-and-replace options.
#[derive(Clone, Debug)]
pub struct RenderAndReplaceOptions {
    /// Destination file for the rendered clip. Convention: a sub-dir
    /// of the project bundle's `media/` so the output travels with
    /// the project on save.
    pub output_path: String,
    /// Encoder preset to use. ProRes 422 HQ is the recommended choice
    /// for render-and-replace — it's deterministic, fast to decode on
    /// playback, and visually lossless.
    pub preset: vance_encoder::EncoderPreset,
    /// Render output width. Falls back to 1920 when `None`.
    pub width: Option<u32>,
    /// Render output height. Falls back to 1080 when `None`.
    pub height: Option<u32>,
    /// Track that the new rendered clip lands on. The same track is
    /// scanned for overlapping clips that get overwritten.
    pub track_id: TrackId,
    /// Start of the range to render + replace (seconds).
    pub start_seconds: f64,
    /// End of the range (seconds, exclusive).
    pub end_seconds: f64,
}

/// Phase 8.7 — outcome of a render-and-replace operation.
#[derive(Clone, Debug)]
pub struct RenderAndReplaceOutcome {
    /// Renderer/encoder summary for the export step.
    pub render: RenderOutcome,
    /// `MediaItemId` registered for the new rendered clip (string form).
    pub new_media_id: String,
    /// `ClipId` of the inserted replacement clip (string form).
    pub new_clip_id: String,
}

impl Engine {
    /// Phase 8.7 — render a time range of the active sequence to a
    /// new media file, import it, and overwrite the range on
    /// `track_id` with a single clip referencing the rendered output.
    ///
    /// Composes from existing primitives — `render_sequence`,
    /// `import_media_file`, and the `OverwriteClip` EditOp — so undo
    /// is preserved automatically: hitting Cmd-Z reverts the overwrite
    /// (restoring the original clips); the rendered file stays on
    /// disk so a follow-up Redo is instant.
    pub fn render_and_replace(
        &mut self,
        opts: RenderAndReplaceOptions,
    ) -> Result<RenderAndReplaceOutcome, EngineError> {
        if opts.end_seconds <= opts.start_seconds {
            return Err(EngineError::Project(format!(
                "render-and-replace range empty: {} → {}",
                opts.start_seconds, opts.end_seconds
            )));
        }
        // 1. Render the sub-range out to disk via the existing
        //    sandboxed encoder path.
        let render_outcome = self.render_sequence(RenderOptions {
            output_path: opts.output_path.clone(),
            preset: opts.preset.clone(),
            width: opts.width,
            height: opts.height,
            render_and_replace_track: Some(opts.track_id),
            start_seconds: Some(opts.start_seconds),
            end_seconds: Some(opts.end_seconds),
            hdr: false, // render-and-replace intermediates stay SDR
        })?;

        // 2. Re-import the rendered file as a fresh MediaItem so the
        //    timeline can reference it.
        let media_info = self.import_media_file(&opts.output_path)?;
        let media_uuid: uuid::Uuid = media_info
            .id
            .parse()
            .map_err(|e| EngineError::Project(format!("media id parse: {e}")))?;
        let media_id = MediaItemId::from_uuid(media_uuid);

        // 3. Build a Clip targeting the full rendered duration, anchored
        //    at the export's start timeline-time.
        let (clip, new_clip_id) = {
            let project = self.current_project.as_ref().ok_or(EngineError::NoProject)?;
            let media = project
                .media
                .get(&media_id)
                .ok_or(EngineError::MediaNotFound)?
                .clone();
            let timeline_in = seconds_to_rt(opts.start_seconds);
            let clip = Clip::new(
                media.id,
                media.display_name.clone(),
                timeline_in,
                vance_core::RationalTime::ZERO,
                media.duration,
            )
            .map_err(|e| EngineError::Project(e.to_string()))?;
            let id = clip.id;
            (clip, id)
        };

        // 4. Overwrite the range on the target track in one undoable
        //    operation. OverwriteClip trims/removes anything the new
        //    clip overlaps, matching the standard NLE render-and-
        //    replace semantics.
        self.apply_edit_op(EditOp::OverwriteClip {
            track_id: opts.track_id,
            clip,
        })?;

        Ok(RenderAndReplaceOutcome {
            render: render_outcome,
            new_media_id: media_info.id,
            new_clip_id: new_clip_id.to_string(),
        })
    }
}

impl Engine {
    /// Phase 9.2 — lazily build the on-disk + in-memory frame cache.
    /// Cache dir lives under the platform's data dir (or under
    /// `target/` during dev) so a fresh project doesn't repeatedly
    /// re-decode the same scrubbed frames.
    pub(crate) fn ensure_frame_cache(&mut self) -> Arc<vance_cache::FrameCache> {
        if let Some(c) = self.frame_cache.as_ref() {
            return c.clone();
        }
        // Conservative defaults — small ring per source, 4 GB on-disk
        // cap. UI will surface knobs in a later milestone.
        let base = std::env::temp_dir().join("vance-cache");
        let ring_budget = 64 * 1024 * 1024; // 64 MiB per source
        let disk_budget = 4 * 1024 * 1024 * 1024; // 4 GiB
        match vance_cache::FrameCache::new(base, ring_budget, disk_budget) {
            Ok(c) => {
                let arc = Arc::new(c);
                self.frame_cache = Some(arc.clone());
                arc
            }
            Err(e) => {
                // Failure here is non-fatal — fall back to caching
                // disabled. Build a sentinel cache rooted in /dev/null
                // equivalents would be wrong; we just leak the error
                // to the trace log and re-attempt next time.
                tracing::warn!(?e, "frame cache init failed; caching disabled");
                // Re-attempt anyway with a fresh process-temp prefix
                // to avoid permanent disable.
                let alt = std::env::temp_dir().join(format!(
                    "vance-cache-{}",
                    std::process::id()
                ));
                let c = vance_cache::FrameCache::new(alt, ring_budget, disk_budget)
                    .expect("fallback cache");
                let arc = Arc::new(c);
                self.frame_cache = Some(arc.clone());
                arc
            }
        }
    }

    /// Phase 9.2 — public read-only cache stats for the UI.
    pub fn frame_cache_stats(&self) -> Option<vance_cache::CacheStats> {
        self.frame_cache.as_ref().map(|c| c.stats())
    }

    /// Reset the cache hit/miss counters (occupancy untouched). Lets the
    /// UI — or a test — measure the hit ratio over a specific window (e.g.
    /// one scrub gesture) instead of the cache's whole lifetime.
    pub fn reset_frame_cache_stats(&self) {
        if let Some(c) = self.frame_cache.as_ref() {
            c.reset_hit_stats();
        }
    }

    /// Phase 9.3 — lazily spawn the prefetch worker. Uses the same
    /// decoder binary discovery path as the primary decoder.
    pub(crate) fn ensure_prefetch_worker(&mut self) -> Option<&crate::prefetch::PrefetchWorker> {
        if self.prefetch.is_none() {
            let bin = locate_decoder_binary().ok()?;
            let cache = self.ensure_frame_cache();
            self.prefetch = Some(crate::prefetch::PrefetchWorker::spawn(cache, bin));
        }
        self.prefetch.as_ref()
    }

    /// Phase 9.4 — enqueue a proxy generation job. Returns the
    /// assigned job id which the UI can poll via
    /// [`Engine::proxy_jobs_snapshot`]. `codec` chooses the proxy
    /// intermediate (ProRes 422 or DNxHR LB).
    pub fn enqueue_proxy_generation(
        &mut self,
        source_path: String,
        output_path: String,
        codec: crate::proxy::ProxyCodec,
    ) -> Result<String, EngineError> {
        let preset = codec.preset();
        let job = crate::proxy::ProxyJob {
            id: format!(
                "proxy-{}-{}",
                std::process::id(),
                std::path::Path::new(&source_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
            ),
            source_path,
            output_path,
            preset,
        };
        let worker = self.ensure_proxy_worker()?;
        let id = worker.enqueue(job);
        Ok(id)
    }

    /// Phase 10.3 — (re)scan the plugin search roots: the user-global
    /// `~/.vance/plugins/` plus the current project's `plugins/`
    /// folder (when the project is saved to disk). Replaces the
    /// registry; loaded instances are cleared so a re-scan picks up
    /// edited plugins. Returns the number discovered.
    pub fn rescan_plugins(&mut self) -> usize {
        // Precedence: project-local plugins should OVERRIDE a globally-
        // installed plugin of the same id. Discovery is first-seen-wins
        // by id, so project-local roots must be pushed BEFORE the
        // user-global root.
        let mut roots = Vec::new();
        if let Some(path) = self.current_project_path.as_ref() {
            let p = std::path::Path::new(path);
            // The project path points at the bundle dir; plugins live
            // in a sibling `plugins/` folder (and, for convenience, an
            // inner one too).
            if let Some(parent) = p.parent() {
                roots.push(parent.join("plugins"));
            }
            roots.push(p.join("plugins"));
        }
        if let Some(dir) = vance_plugin_sdk::discovery::user_plugins_dir() {
            roots.push(dir);
        }
        self.plugin_registry = vance_plugin_sdk::discovery::PluginRegistry::scan(&roots);
        self.loaded_plugins.clear();
        self.plugin_registry.len()
    }

    /// Phase 10.5 — UI-facing list of discovered plugins.
    pub fn discovered_plugins(&self) -> Vec<vance_plugin_sdk::discovery::DiscoveredPlugin> {
        self.plugin_registry.plugins().to_vec()
    }

    /// Phase 10.3 — additively scan one extra directory into the
    /// plugin registry (e.g. a user-added custom folder). Returns the
    /// new total count. Also drops cached instances so an edited
    /// plugin reloads.
    pub fn add_plugin_dir(&mut self, dir: std::path::PathBuf) -> usize {
        self.plugin_registry.scan_dir(&dir);
        self.loaded_plugins.clear();
        self.plugin_registry.len()
    }

    /// Phase 10.5 — apply every enabled `wasm:<id>` effect on a clip to
    /// a CPU RGBA buffer, in chain order. Best-effort: a plugin that
    /// fails to load or traps is logged + skipped so one bad plugin
    /// can't abort an export.
    ///
    /// `clip_t` is the clip-relative time in seconds (source PTS minus
    /// the clip's source-in), used to evaluate keyframed parameters.
    pub fn apply_wasm_effects(
        &mut self,
        rgba: &mut [u8],
        w: u32,
        h: u32,
        effects: &[vance_timeline::Effect],
        clip_t: f64,
    ) {
        for e in effects {
            if !e.enabled {
                continue;
            }
            let Some(id) = e.kind.strip_prefix("wasm:") else {
                continue;
            };
            if !self.ensure_plugin_loaded(id) {
                continue;
            }
            let Some((plugin, _)) = self.loaded_plugins.get_mut(id) else {
                continue;
            };
            let manifest = plugin.manifest().clone();
            // Evaluate each param at the clip-relative time so keyframed
            // (animated) values are honoured, not dropped to the
            // manifest default. Non-finite (NaN/Inf) values fall back to
            // the default rather than poisoning the guest.
            let packed = manifest.pack_params(&|key| {
                e.params
                    .get(key)
                    .and_then(|v| v.eval_number(clip_t))
                    .filter(|n| n.is_finite())
                    .map(|n| n as f32)
            });
            if let Err(err) = plugin.process(rgba, w, h, &packed) {
                tracing::warn!(plugin = %id, ?err, "wasm effect process failed; skipping");
            }
        }
    }

    /// Phase 9.6 — render-cache-wrapped [`Self::apply_wasm_effects`].
    /// Keys the cache on `hash(input RGBA) ⊕ hash(effect chain @ clip_t
    /// + dims)`; a hit returns the previously-processed output without
    /// re-running the (CPU-expensive) WASM plugins. Used by the live
    /// preview so scrubbing back to an already-seen (frame, effects)
    /// pair is instant. The export path stays uncached (linear, every
    /// frame unique → no hits, only hash overhead).
    pub fn apply_wasm_effects_cached(
        &mut self,
        rgba: &mut [u8],
        w: u32,
        h: u32,
        effects: &[vance_timeline::Effect],
        clip_t: f64,
    ) {
        // Hash the effect chain (id + time-evaluated params + dims).
        // BTreeMap param iteration is key-sorted → deterministic.
        let mut chain = vance_cache::Fnv1a64::new();
        chain.write(&w.to_le_bytes());
        chain.write(&h.to_le_bytes());
        let mut any_wasm = false;
        for e in effects {
            if !e.enabled {
                continue;
            }
            let Some(id) = e.kind.strip_prefix("wasm:") else {
                continue;
            };
            any_wasm = true;
            chain.write(id.as_bytes());
            chain.write(&[0]);
            for (k, v) in &e.params {
                if let Some(n) = v.eval_number(clip_t) {
                    chain.write(k.as_bytes());
                    chain.write(&n.to_le_bytes());
                }
            }
            chain.write(&[0xff]);
        }
        if !any_wasm {
            return;
        }
        let effect_hash = chain.finish();
        let input_hash = {
            let mut h = vance_cache::Fnv1a64::new();
            h.write(rgba);
            h.finish()
        };
        let key = vance_cache::CacheKey::for_render(input_hash, effect_hash);
        let cache = self.ensure_frame_cache();
        if let Some(cached) = cache.get(&key) {
            if cached.len() == rgba.len() {
                rgba.copy_from_slice(&cached);
                return;
            }
        }
        // Miss — process then cache.
        self.apply_wasm_effects(rgba, w, h, effects, clip_t);
        let _ = cache.put(&key, rgba);
    }

    /// Effects on a clip by id (cloned), or empty if not found.
    fn clip_effects(&self, clip_id: vance_timeline::ClipId) -> Vec<vance_timeline::Effect> {
        let Some(project) = self.current_project.as_ref() else {
            return Vec::new();
        };
        let Some(seq_id) = self.current_sequence_id else {
            return Vec::new();
        };
        let Some(seq) = project.sequence(seq_id) else {
            return Vec::new();
        };
        seq.tracks
            .iter()
            .find_map(|t| t.clips.iter().find(|c| c.id == clip_id).map(|c| c.effects.clone()))
            .unwrap_or_default()
    }

    /// Render one clip to a final RGBA buffer (decode → native GPU
    /// effects → readback → the clip's own WASM effects). Used to produce
    /// the A and B sides of a WASM transition INDEPENDENTLY — the GPU
    /// dissolve blends both in a single pass, but a WASM transition needs
    /// each side fully composited on the CPU before `process_transition`.
    fn render_clip_to_rgba(
        &mut self,
        clip_id: vance_timeline::ClipId,
        media_id: MediaItemId,
        source_pts_sec: f64,
        source_in_sec: f64,
        width: u32,
        height: u32,
    ) -> Option<(Vec<u8>, i64)> {
        let frame = self.decode_clip_framedata(media_id, source_pts_sec, source_in_sec)?;
        let pts = frame.pts;
        let rs_arc = self.render_state.clone()?;
        {
            let mut rs = rs_arc.lock();
            rs.renderer.upload_frame(&frame).ok()?;
            rs.renderer.set_mix_factor(0.0); // render this clip alone
        }
        let clip_t = source_pts_sec - source_in_sec;
        // Pushes opacity/transform/grades/etc. for this clip (self-locks).
        self.apply_clip_render_params(clip_id, media_id, clip_t);
        let mut out = {
            let mut rs = rs_arc.lock();
            rs.renderer.render_to_rgba_buffer(width, height).ok()?
        };
        let effects = self.clip_effects(clip_id);
        if !effects.is_empty() {
            self.apply_wasm_effects(&mut out, width, height, &effects, clip_t);
        }
        Some((out, pts))
    }

    /// Blend the B clip into the A clip using a WASM transition plugin.
    /// `progress` (0→1, the transition's linear position) is fed to the
    /// plugin as its `mix`/`progress` param; the remaining manifest params
    /// come from `params` (keyframed, evaluated at `t`) or the manifest
    /// defaults. On any failure the A buffer is left unchanged (the
    /// transition shows clip A).
    pub fn apply_wasm_transition(
        &mut self,
        a_rgba: &mut [u8],
        b_rgba: &[u8],
        w: u32,
        h: u32,
        plugin_id: &str,
        params: &std::collections::BTreeMap<String, vance_timeline::EffectValue>,
        progress: f32,
        t: f64,
    ) {
        if !self.ensure_plugin_loaded(plugin_id) {
            return;
        }
        let Some((plugin, _)) = self.loaded_plugins.get_mut(plugin_id) else {
            return;
        };
        let manifest = plugin.manifest().clone();
        let packed = manifest.pack_params(&|key| {
            // The engine drives progress via the conventional mix/progress
            // param; everything else is a user-set (keyframed) override.
            let lk = key.to_ascii_lowercase();
            if lk == "mix" || lk == "progress" {
                Some(progress)
            } else {
                params
                    .get(key)
                    .and_then(|v| v.eval_number(t))
                    .filter(|n| n.is_finite())
                    .map(|n| n as f32)
            }
        });
        if let Err(err) = plugin.process_transition(a_rgba, b_rgba, w, h, &packed) {
            tracing::warn!(plugin = %plugin_id, ?err, "wasm transition process failed; showing A");
        }
    }

    /// Install the shared entitlement gate consulted at plugin load (M2, §6).
    pub fn set_entitlement_gate(&mut self, gate: Arc<EntitlementGate>) {
        self.entitlement_gate = Some(gate);
    }

    /// Ensure the plugin with manifest `id` is loaded + fresh. Reloads
    /// when the source `.wasm` file's mtime changed since the cached
    /// instance was built (so an in-place edit between exports is
    /// picked up without an explicit rescan). Returns whether a usable
    /// instance is now cached.
    fn ensure_plugin_loaded(&mut self, id: &str) -> bool {
        // M2 — load-time entitlement gate (spec §6): the single choke point in
        // front of `WasmPlugin::load`. A marketplace-gated effect whose
        // entitlement isn't currently valid refuses to instantiate.
        if let Some(gate) = &self.entitlement_gate {
            if !gate.allowed(id) {
                tracing::warn!(plugin = %id, "blocked: this effect needs a valid entitlement");
                return false;
            }
        }
        let Some(disc) = self.plugin_registry.by_id(id).cloned() else {
            tracing::warn!(plugin = %id, "wasm effect references unknown plugin");
            return false;
        };
        let current_mtime = std::fs::metadata(&disc.path).and_then(|m| m.modified()).ok();
        // Reuse the cached instance only if its mtime matches.
        if let Some((_, cached_mtime)) = self.loaded_plugins.get(id) {
            if *cached_mtime == current_mtime {
                return true;
            }
            // Stale — drop it so we reload below.
            self.loaded_plugins.remove(id);
        }
        match std::fs::read(&disc.path)
            .map_err(|e| e.to_string())
            .and_then(|b| {
                vance_plugin_sdk::host::WasmPlugin::load(&b).map_err(|e| e.to_string())
            }) {
            Ok(p) => {
                self.loaded_plugins
                    .insert(id.to_string(), (p, current_mtime));
                true
            }
            Err(err) => {
                tracing::warn!(plugin = %id, %err, "wasm plugin load failed");
                false
            }
        }
    }

    /// Phase 9.5 — toggle the project-level "use proxies" flag.
    pub fn set_use_proxies(&mut self, on: bool) -> Result<(), EngineError> {
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        project.use_proxies = on;
        Ok(())
    }

    /// Phase 9.5 — point a media item at its generated proxy file.
    /// Called automatically by the proxy worker on success, but also
    /// exposed for the UI when the user picks an existing proxy.
    pub fn set_media_proxy_path(
        &mut self,
        media_id: MediaItemId,
        proxy_path: Option<std::path::PathBuf>,
    ) -> Result<(), EngineError> {
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        let media = project
            .media
            .get_mut(&media_id)
            .ok_or(EngineError::MediaNotFound)?;
        let mut updated = (**media).clone();
        updated.proxy_path = proxy_path;
        *media = Arc::new(updated);
        Ok(())
    }

    /// GOAT-3 — set or clear a custom-column value on a media item.
    /// `value == None` removes the key. (Media-library metadata, like
    /// proxy paths, lives outside the sequence EditLog.)
    pub fn set_media_metadata(
        &mut self,
        media_id: MediaItemId,
        key: String,
        value: Option<String>,
    ) -> Result<(), EngineError> {
        let project = self.current_project.as_mut().ok_or(EngineError::NoProject)?;
        let media = project
            .media
            .get_mut(&media_id)
            .ok_or(EngineError::MediaNotFound)?;
        let mut updated = (**media).clone();
        match value {
            Some(v) => {
                updated.metadata.insert(key, v);
            }
            None => {
                updated.metadata.remove(&key);
            }
        }
        *media = Arc::new(updated);
        Ok(())
    }

    /// GOAT-3 — media ids (uuid strings) matching `query`, the deep search
    /// behind the browser filter box.
    pub fn query_media(&self, query: &vance_timeline::MediaQuery) -> Vec<String> {
        self.current_project
            .as_ref()
            .map(|p| {
                p.query_media(query)
                    .into_iter()
                    .map(|id| id.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// GOAT-3 — the union of custom-column keys across the media library.
    pub fn custom_columns(&self) -> Vec<String> {
        self.current_project
            .as_ref()
            .map(|p| p.custom_columns())
            .unwrap_or_default()
    }

    /// GOAT-3 — add a smart bin (saved search). Returns its id.
    pub fn add_smart_bin(
        &mut self,
        name: String,
        query: vance_timeline::MediaQuery,
    ) -> Result<String, EngineError> {
        let project = self.current_project.as_mut().ok_or(EngineError::NoProject)?;
        let bin = vance_timeline::SmartBin::new(name, query);
        let id = bin.id.to_string();
        project.smart_bins.push(bin);
        Ok(id)
    }

    /// GOAT-3 — remove a smart bin by id. Returns whether one was removed.
    pub fn remove_smart_bin(&mut self, id: &str) -> Result<bool, EngineError> {
        let uuid = uuid::Uuid::parse_str(id)
            .map_err(|e| EngineError::Project(format!("bad smart-bin id: {e}")))?;
        let target = MediaItemId::from_uuid(uuid);
        let project = self.current_project.as_mut().ok_or(EngineError::NoProject)?;
        let before = project.smart_bins.len();
        project.smart_bins.retain(|b| b.id != target);
        Ok(project.smart_bins.len() != before)
    }

    /// GOAT-3 — all smart bins (for the browser sidebar).
    pub fn smart_bins(&self) -> Vec<vance_timeline::SmartBin> {
        self.current_project
            .as_ref()
            .map(|p| p.smart_bins.clone())
            .unwrap_or_default()
    }

    /// GOAT-3 — current member ids (uuid strings) of a smart bin.
    pub fn smart_bin_members(&self, id: &str) -> Vec<String> {
        let Ok(uuid) = uuid::Uuid::parse_str(id) else {
            return Vec::new();
        };
        let target = MediaItemId::from_uuid(uuid);
        self.current_project
            .as_ref()
            .and_then(|p| p.smart_bin_members(target))
            .map(|ids| ids.into_iter().map(|i| i.to_string()).collect())
            .unwrap_or_default()
    }

    /// Phase 9.4 — snapshot of every known proxy job.
    pub fn proxy_jobs_snapshot(&self) -> Vec<crate::proxy::ProxyJobSnapshot> {
        self.proxy_worker
            .as_ref()
            .map(|w| w.status_snapshot())
            .unwrap_or_default()
    }

    pub(crate) fn ensure_proxy_worker(
        &mut self,
    ) -> Result<&crate::proxy::ProxyWorker, EngineError> {
        if self.proxy_worker.is_none() {
            let dec = locate_decoder_binary()?;
            let enc = locate_encoder_binary()?;
            self.proxy_worker = Some(crate::proxy::ProxyWorker::spawn(dec, enc));
        }
        Ok(self.proxy_worker.as_ref().unwrap())
    }

    /// Phase 9.3 — kick a forward-prefetch session at `anchor_pts`.
    /// `step_pts` should equal `time_base_den / frame_rate_num`. v1
    /// horizon = 24 frames (~1 s at 24 fps).
    pub(crate) fn kick_prefetch(&mut self, anchor_pts: i64) {
        let uri = match self.current_media_path.clone() {
            Some(u) => u,
            None => return,
        };
        let step_pts = match self.stream_info.as_ref() {
            Some(info) if info.frame_rate.0 > 0 && info.frame_rate.1 > 0 => {
                let (tb_n, tb_d) = info.time_base;
                if tb_n <= 0 {
                    return;
                }
                // step = (tb_d / tb_n) * (frame_rate.1 / frame_rate.0)
                // → represented as i64 to fit AtomicI64.
                let num = tb_d as i64 * info.frame_rate.1 as i64;
                let den = tb_n as i64 * info.frame_rate.0 as i64;
                if den == 0 {
                    return;
                }
                num / den
            }
            _ => return,
        };
        if step_pts <= 0 {
            return;
        }
        let req = crate::prefetch::PrefetchRequest {
            source_uri: uri,
            anchor_pts,
            step_pts,
            horizon_frames: 24,
        };
        if let Some(worker) = self.ensure_prefetch_worker() {
            worker.request(req);
        }
    }
}

/// Locate the `vance-encoder` binary using the same fallback chain as
/// the decoder.
fn locate_encoder_binary() -> Result<PathBuf, EngineError> {
    let exe_name = if cfg!(windows) {
        "vance-encoder.exe"
    } else {
        "vance-encoder"
    };
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let candidate = dir.join(exe_name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    for c in [
        PathBuf::from("target/debug").join(exe_name),
        PathBuf::from("target/release").join(exe_name),
    ] {
        if c.exists() {
            return Ok(c);
        }
    }
    Err(EngineError::Project(format!(
        "could not find {exe_name} (looked next to vance-app + target/debug, target/release)"
    )))
}

/// Try a list of locations for the `vance-decoder` binary:
/// 1. Right next to the running `vance-app` executable (production layout).
/// 2. Same directory under `cargo build` (`target/<profile>/`).
/// 3. Fallback: `target/debug/vance-decoder` from a workspace-detected root.
/// Best-effort locate the `vance-plugin-host` sidecar (out-of-process
/// plugin sandbox). Returns `None` if it can't be found — CLAP/LV2
/// effects then bypass rather than failing playback. Mirrors the
/// decoder/encoder locators.
fn locate_plugin_host_binary() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) {
        "vance-plugin-host.exe"
    } else {
        "vance-plugin-host"
    };
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let candidate = dir.join(exe_name);
            if candidate.exists() {
                return Some(candidate);
            }
            if let Some(parent) = dir.parent() {
                let alt = parent.join(exe_name);
                if alt.exists() {
                    return Some(alt);
                }
            }
        }
    }
    for c in [
        PathBuf::from("target/debug").join(exe_name),
        PathBuf::from("target/release").join(exe_name),
    ] {
        if c.exists() {
            return Some(c);
        }
    }
    None
}

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
            // Integration tests run from `target/<profile>/deps/`; the
            // sidecar binary is one level up.
            if let Some(parent) = dir.parent() {
                let alt = parent.join(exe_name);
                if alt.exists() {
                    return Ok(alt);
                }
                tried.push(alt);
            }
        }
    }
    // Workspace-relative fallback (matches locate_encoder_binary).
    for c in [
        PathBuf::from("target/debug").join(exe_name),
        PathBuf::from("target/release").join(exe_name),
    ] {
        if c.exists() {
            return Ok(c);
        }
        tried.push(c);
    }
    Err(EngineError::DecoderBinaryMissing(tried))
}

/// Phase 11 — presence of the bundled media sidecars, for the capability
/// report: `(decoder, encoder, audio)`. Each is located the same way the
/// engine locates it at runtime, so a positive here means it will spawn.
pub(crate) fn sidecar_presence() -> (bool, bool, bool) {
    (
        locate_decoder_binary().is_ok(),
        locate_encoder_binary().is_ok(),
        locate_audio_binary().is_ok(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grain_window_fades_edges_keeps_middle() {
        let sr = 48_000u32;
        let frames = 1000usize;
        let mut grain = vec![1.0f32; frames * 2]; // all unity
        apply_grain_window(&mut grain, sr);
        // First frame fully faded to 0.
        assert!(grain[0].abs() < 1e-6 && grain[1].abs() < 1e-6, "start not faded");
        // Last frame fully faded to 0.
        let last = frames - 1;
        assert!(
            grain[last * 2].abs() < 1e-6 && grain[last * 2 + 1].abs() < 1e-6,
            "end not faded"
        );
        // Middle untouched (ramp is <= frames/4, so the center is unity).
        let mid = frames / 2;
        assert!((grain[mid * 2] - 1.0).abs() < 1e-6, "middle changed");
        // The fade-in is monotonically non-decreasing across the ramp.
        let ramp = ((sr as f64 * 0.008) as usize).min(frames / 4).max(1);
        for i in 1..ramp {
            assert!(grain[i * 2] >= grain[(i - 1) * 2], "fade-in not monotonic");
        }
    }

    #[test]
    fn grain_window_handles_tiny_grains() {
        // A 2-frame grain must not panic (ramp clamps to >= 1).
        let mut g = vec![1.0f32; 4];
        apply_grain_window(&mut g, 48_000);
        let mut empty: Vec<f32> = Vec::new();
        apply_grain_window(&mut empty, 48_000);
    }

    #[test]
    fn compute_scopes_is_noop_when_disabled() {
        // Scopes default to disabled — compute must short-circuit to the
        // cache (None) without touching render state, so it's safe to
        // call even with no GPU / no project (and never stalls playback).
        let mut e = Engine::default();
        assert!(!e.scopes_enabled);
        let out = e
            .compute_scopes(vance_scopes::ScopeConfig::default(), 16, 16, false)
            .expect("noop ok");
        assert!(out.is_none());
    }

    #[test]
    fn set_scopes_enabled_toggles_flag() {
        let mut e = Engine::default();
        e.set_scopes_enabled(true);
        assert!(e.scopes_enabled);
        // Even enabled, with no render_state it returns the (empty) cache
        // rather than erroring.
        let out = e
            .compute_scopes(vance_scopes::ScopeConfig::default(), 16, 16, false)
            .expect("ok without render state");
        assert!(out.is_none());
    }

    #[test]
    fn edit_history_forks_and_goto_switches_branches() {
        let mut e = Engine::default();
        e.new_project("hist".into()).expect("project");
        // One edit → node 1.
        e.set_master_gain_db(-6.0).expect("edit a");
        assert!(e.can_undo());
        // Undo to baseline, then a DIFFERENT edit → forks node 2 (the old
        // branch is NOT discarded).
        assert!(e.undo().expect("undo"));
        e.set_master_gain_db(3.0).expect("edit b");

        let h = e.edit_history();
        assert_eq!(h.len(), 3, "root + 2 forked edits");
        let tips: Vec<usize> = h
            .iter()
            .filter(|n| n.children.is_empty())
            .map(|n| n.id)
            .collect();
        assert_eq!(tips.len(), 2, "two branch tips preserved");

        // Name the current branch, then jump to the OTHER tip and back.
        e.checkpoint_history("loud".into()).expect("checkpoint");
        let other = *tips.iter().find(|&&id| !h[id].is_current).unwrap();
        assert!(e.goto_history(other).expect("goto other branch"));
        assert_eq!(e.edit_history()[other].is_current, true);
        // Out-of-range goto is a clean false, not an error.
        assert!(!e.goto_history(999).expect("goto oob is Ok(false)"));
    }

    // ---- Phase 7.3 mixer integration ---------------------------------

    fn t(num: i64) -> vance_core::RationalTime {
        vance_core::RationalTime::new(num, 1).unwrap()
    }

    fn audio_media(path: &str) -> Arc<MediaItem> {
        let mut m = MediaItem::new(
            PathBuf::from(path),
            "clip.wav",
            MediaKind::Audio,
            t(10),
        );
        m.has_audio_track = true;
        Arc::new(m)
    }

    #[test]
    fn build_mixer_snapshot_inserts_clipmix_only_for_audio_tracks() {
        let mut seq = vance_timeline::Sequence::new("S", vance_core::FrameRate::FPS_24);
        let video_t = vance_timeline::Track::new(TrackKind::Video, "V1");
        let audio_t = vance_timeline::Track::new(TrackKind::Audio, "A1");
        let video_clip = Arc::new(
            vance_timeline::Clip::new(MediaItemId::new(), "v", t(0), t(0), t(5)).unwrap(),
        );
        let audio_clip = Arc::new(
            vance_timeline::Clip::new(MediaItemId::new(), "a", t(0), t(0), t(5)).unwrap(),
        );
        let mut video_t = video_t;
        video_t.clips.push(video_clip.clone());
        let mut audio_t = audio_t;
        audio_t.clips.push(audio_clip.clone());
        let audio_track_id = audio_t.id;
        let audio_clip_id = audio_clip.id;
        seq.tracks.push(Arc::new(video_t));
        seq.tracks.push(Arc::new(audio_t));

        let snap = build_mixer_snapshot(&seq);
        assert_eq!(snap.tracks.len(), 2, "both tracks emit a TrackMix");
        assert_eq!(snap.clips.len(), 1, "only audio tracks emit ClipMixes");
        let cm = snap.clips.get(&audio_clip_id.as_uuid()).expect("audio clip present");
        assert_eq!(cm.track_id, audio_track_id.as_uuid());
        assert!(cm.enabled);
        assert!(!snap.solo_anywhere);
    }

    #[test]
    fn build_mixer_snapshot_precomputes_gain_linear() {
        let mut seq = vance_timeline::Sequence::new("S", vance_core::FrameRate::FPS_24);
        let mut tr = vance_timeline::Track::new(TrackKind::Audio, "A1");
        tr.gain_db = -6.0;
        seq.tracks.push(Arc::new(tr));
        let snap = build_mixer_snapshot(&seq);
        let tm = snap.tracks.values().next().unwrap();
        let expected = vance_audio::db_to_linear(-6.0);
        assert!((tm.gain_linear - expected).abs() < 1e-6);
    }

    #[test]
    fn build_mixer_snapshot_detects_solo_anywhere() {
        let mut seq = vance_timeline::Sequence::new("S", vance_core::FrameRate::FPS_24);
        let mut a1 = vance_timeline::Track::new(TrackKind::Audio, "A1");
        let mut a2 = vance_timeline::Track::new(TrackKind::Audio, "A2");
        a2.solo = true;
        seq.tracks.push(Arc::new(a1));
        seq.tracks.push(Arc::new(a2));
        assert!(build_mixer_snapshot(&seq).solo_anywhere);
        // Without solo on any track:
        seq = vance_timeline::Sequence::new("S", vance_core::FrameRate::FPS_24);
        a1 = vance_timeline::Track::new(TrackKind::Audio, "A1");
        seq.tracks.push(Arc::new(a1));
        assert!(!build_mixer_snapshot(&seq).solo_anywhere);
    }

    #[test]
    fn reframe_transform_centers_subject_and_is_undistorted() {
        // 16:9 → 9:16, subject at the far right → the window clamps in-frame
        // and the subject is centered (or as centered as the clamp allows).
        let (tr, sc) = reframe_transform((0.9, 0.5), 1920, 1080, 1080, 1920, 1.0);
        // Narrower target → full height (scale_y == 1), horizontal zoom.
        assert!((sc[1] - 1.0).abs() < 1e-5, "full height, no vertical zoom");
        let ar_s = 1920.0f32 / 1080.0;
        let ar_t = 1080.0f32 / 1920.0;
        assert!((sc[0] - ar_s / ar_t).abs() < 1e-4, "horizontal scale = ar_s/ar_t");
        // The subject (clamped) maps to output-center: out = scale*(subj-0.5)+0.5+translate.
        let hx = 0.5 / sc[0];
        let clamped_cx = 0.9f32.clamp(hx, 1.0 - hx);
        let out_x = sc[0] * (clamped_cx - 0.5) + 0.5 + tr[0];
        assert!((out_x - 0.5).abs() < 1e-4, "subject centered horizontally: {out_x}");

        // Centered subject at exactly middle → translate ~0.
        let (tr2, _) = reframe_transform((0.5, 0.5), 1920, 1080, 1080, 1920, 1.0);
        assert!(tr2[0].abs() < 1e-5 && tr2[1].abs() < 1e-5);

        // Same aspect + zoom 1 → identity (scale 1, translate 0).
        let (tr3, sc3) = reframe_transform((0.5, 0.5), 1920, 1080, 1920, 1080, 1.0);
        assert!((sc3[0] - 1.0).abs() < 1e-5 && (sc3[1] - 1.0).abs() < 1e-5);
        assert!(tr3[0].abs() < 1e-5 && tr3[1].abs() < 1e-5);
    }

    #[test]
    fn build_mixer_snapshot_translates_bus_inserts_and_sends() {
        let mut seq = vance_timeline::Sequence::new("S", vance_core::FrameRate::FPS_24);
        let music = vance_timeline::BusId::new();
        let reverb = vance_timeline::BusId::new();
        let mut mbus = vance_timeline::AudioBus::new("Music");
        mbus.id = music;
        mbus.inserts.push(vance_timeline::AudioEffectSpec::default_compressor());
        // A post-fader send from Music → Reverb at -6 dB.
        mbus.sends.push(vance_timeline::Send {
            target: reverb,
            level_db: -6.0,
            pre_fader: false,
        });
        let mut rbus = vance_timeline::AudioBus::new("Reverb");
        rbus.id = reverb;
        seq.buses.push(mbus);
        seq.buses.push(rbus);

        let snap = build_mixer_snapshot(&seq);
        let m = snap.buses.get(&music.as_uuid()).expect("music bus in snapshot");
        assert_eq!(m.audio_effects.len(), 1, "bus insert translated");
        assert_eq!(m.sends.len(), 1, "bus send translated");
        assert_eq!(m.sends[0].target, reverb.as_uuid());
        assert!(!m.sends[0].pre_fader);
        // -6 dB precomputed to linear (~0.501).
        assert!((m.sends[0].level_linear - vance_audio::db_to_linear(-6.0)).abs() < 1e-6);
    }

    #[test]
    fn collect_audio_sources_skips_disabled_track_and_clip_without_audio() {
        let mut project = vance_timeline::Project::new("P");
        // Two audio media items: one has audio, one doesn't.
        let mi_with = audio_media("/tmp/with.wav");
        let mut mi_without_inner =
            MediaItem::new(PathBuf::from("/tmp/without.mp4"), "x", MediaKind::Video, t(10));
        mi_without_inner.has_audio_track = false;
        let mi_without = Arc::new(mi_without_inner);
        let id_with = mi_with.id;
        let id_without = mi_without.id;
        project.media.insert(id_with, mi_with);
        project.media.insert(id_without, mi_without);

        let mut seq = vance_timeline::Sequence::new("S", vance_core::FrameRate::FPS_24);
        // Audio track with two clips at t=0..5: one whose media has
        // audio, one whose media doesn't.
        let mut at = vance_timeline::Track::new(TrackKind::Audio, "A1");
        at.clips.push(Arc::new(
            vance_timeline::Clip::new(id_with, "with", t(0), t(0), t(5)).unwrap(),
        ));
        at.clips.push(Arc::new(
            vance_timeline::Clip::new(id_without, "without", t(0), t(0), t(5)).unwrap(),
        ));
        seq.tracks.push(Arc::new(at));
        // Disabled track is silently skipped even if media has audio.
        let mut disabled = vance_timeline::Track::new(TrackKind::Audio, "A2");
        disabled.enabled = false;
        disabled.clips.push(Arc::new(
            vance_timeline::Clip::new(id_with, "off", t(0), t(0), t(5)).unwrap(),
        ));
        seq.tracks.push(Arc::new(disabled));

        let sources = collect_audio_sources_at(&seq, &project, 1.0);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, PathBuf::from("/tmp/with.wav"));
    }

    #[test]
    fn collect_audio_sources_skips_clips_outside_window() {
        let mut project = vance_timeline::Project::new("P");
        let mi = audio_media("/tmp/clip.wav");
        let mid = mi.id;
        project.media.insert(mid, mi);
        let mut seq = vance_timeline::Sequence::new("S", vance_core::FrameRate::FPS_24);
        let mut at = vance_timeline::Track::new(TrackKind::Audio, "A1");
        // Clip at timeline 5..10. Probing at t=2 → empty; t=7 → 1 source.
        at.clips.push(Arc::new(
            vance_timeline::Clip::new(mid, "c", t(5), t(0), t(5)).unwrap(),
        ));
        seq.tracks.push(Arc::new(at));
        assert!(collect_audio_sources_at(&seq, &project, 2.0).is_empty());
        let live = collect_audio_sources_at(&seq, &project, 7.0);
        assert_eq!(live.len(), 1);
        // Offset into the source: at timeline t=7 we're 2s past the
        // clip's timeline_in=5, so the source offset is source_in + 2 = 2.
        assert!((live[0].from_seconds - 2.0).abs() < 1e-6);
    }

    #[test]
    fn op_touches_mixer_covers_all_mixer_variants() {
        use vance_timeline::TrackId;
        let tid = TrackId::new();
        let cid = vance_timeline::ClipId::new();
        assert!(op_touches_mixer(&EditOp::SetTrackMute { track_id: tid, mute: true }));
        assert!(op_touches_mixer(&EditOp::SetTrackSolo { track_id: tid, solo: true }));
        assert!(op_touches_mixer(&EditOp::SetTrackGainDb { track_id: tid, gain_db: -3.0 }));
        assert!(op_touches_mixer(&EditOp::SetClipMute {
            track_id: tid,
            clip_id: cid,
            mute: true,
        }));
        assert!(op_touches_mixer(&EditOp::SetClipGainDb {
            track_id: tid,
            clip_id: cid,
            gain_db: -6.0,
        }));
        assert!(op_touches_mixer(&EditOp::SetClipFadeIn {
            track_id: tid,
            clip_id: cid,
            duration_seconds: 0.5,
            curve: vance_timeline::FadeCurve::Linear,
        }));
        assert!(op_touches_mixer(&EditOp::SetClipFadeOut {
            track_id: tid,
            clip_id: cid,
            duration_seconds: 0.5,
            curve: vance_timeline::FadeCurve::Linear,
        }));
        // Compound is conservative-true so a batched edit republishes once.
        assert!(op_touches_mixer(&EditOp::Compound {
            ops: vec![],
            label: "noop".into(),
        }));
        // SetEffectEnabled does NOT touch mixer state.
        assert!(!op_touches_mixer(&EditOp::SetEffectEnabled {
            track_id: tid,
            clip_id: cid,
            effect_id: vance_timeline::EffectId::new(),
            enabled: true,
        }));
    }

    // ========================================================
    // Phase 7.4 M4 scheduler unit tests
    // ========================================================

    fn sched(timeline_in: f64, duration: f64) -> SchedClip {
        SchedClip {
            clip_id: uuid::Uuid::new_v4(),
            path: PathBuf::from("/tmp/x.wav"),
            source_in_seconds: 0.0,
            timeline_in_seconds: timeline_in,
            duration_seconds: duration,
        }
    }

    #[test]
    fn compute_sched_plan_empty_intervals_produces_empty_plan() {
        let live = std::collections::HashSet::new();
        let plan = compute_sched_plan(&[], &live, 0.0, 0.05, 2.0, 16);
        assert!(plan.to_add.is_empty());
        assert!(plan.to_remove.is_empty());
        assert_eq!(plan.cap_drops, 0);
    }

    #[test]
    fn compute_sched_plan_adds_clip_inside_preload_window() {
        // Clip starts at t=1.5, playhead at t=0.0, PRELOAD=2.0.
        // 1.5 <= 0.0 + 2.0 → in window.
        let c = sched(1.5, 5.0);
        let live = std::collections::HashSet::new();
        let plan = compute_sched_plan(&[c.clone()], &live, 0.0, 0.05, 2.0, 16);
        assert_eq!(plan.to_add.len(), 1);
        assert_eq!(plan.to_add[0].0, c.clip_id);
        assert!(plan.to_remove.is_empty());
        // Spawning ahead of playhead: from_seconds = source_in + 0 = 0.0.
        assert!((plan.to_add[0].2 - 0.0).abs() < 1e-9);
    }

    #[test]
    fn compute_sched_plan_skips_clip_outside_preload() {
        // Clip starts at t=5.0, playhead at t=0.0, PRELOAD=2.0 — too far.
        let c = sched(5.0, 5.0);
        let plan = compute_sched_plan(
            &[c],
            &std::collections::HashSet::new(),
            0.0,
            0.05,
            2.0,
            16,
        );
        assert!(plan.to_add.is_empty());
    }

    #[test]
    fn compute_sched_plan_add_source_offset_seeks_into_clip_when_overlapping() {
        // Clip [10..20] with source_in=3.0, playhead at t=12.
        // from_seconds = 3.0 + (12 - 10) = 5.0.
        let mut c = sched(10.0, 10.0);
        c.source_in_seconds = 3.0;
        let plan = compute_sched_plan(
            &[c.clone()],
            &std::collections::HashSet::new(),
            12.0,
            0.05,
            2.0,
            16,
        );
        assert_eq!(plan.to_add.len(), 1);
        assert!((plan.to_add[0].2 - 5.0).abs() < 1e-9);
    }

    #[test]
    fn compute_sched_plan_removes_live_clip_no_longer_wanted() {
        let c1 = sched(0.0, 1.0); // ends at t=1.0, gone by t=5.0
        let mut live = std::collections::HashSet::new();
        live.insert(c1.clip_id);
        // intervals empty → wanted set is empty → c1 is in to_remove.
        let plan = compute_sched_plan(&[], &live, 5.0, 0.05, 2.0, 16);
        assert_eq!(plan.to_remove, vec![c1.clip_id]);
    }

    #[test]
    fn compute_sched_plan_grace_window_keeps_just_ended_clip() {
        // Clip [0..1], playhead at t=1.02, grace=0.05 → still kept.
        // (Fold thread silences it via effective_gain_at; scheduler
        // does not yank mid-buffer.)
        let c = sched(0.0, 1.0);
        let mut live = std::collections::HashSet::new();
        live.insert(c.clip_id);
        let plan = compute_sched_plan(&[c.clone()], &live, 1.02, 0.05, 2.0, 16);
        // Within grace: c is still wanted, so it stays — to_remove empty.
        assert!(plan.to_remove.is_empty());
        // And not in to_add (already live).
        assert!(plan.to_add.is_empty());
    }

    #[test]
    fn compute_sched_plan_grace_window_removes_after_grace_elapses() {
        let c = sched(0.0, 1.0);
        let mut live = std::collections::HashSet::new();
        live.insert(c.clip_id);
        // playhead past grace: t=1.06, grace=0.05 → removed.
        let plan = compute_sched_plan(&[c.clone()], &live, 1.06, 0.05, 2.0, 16);
        assert_eq!(plan.to_remove, vec![c.clip_id]);
    }

    #[test]
    fn compute_sched_plan_max_concurrent_cap_drops_furthest_future() {
        // 4 candidates, cap=2 → 2 kept (most imminent), 2 dropped.
        let c0 = sched(0.0, 5.0);
        let c1 = sched(0.5, 5.0);
        let c2 = sched(1.0, 5.0);
        let c3 = sched(1.5, 5.0);
        let plan = compute_sched_plan(
            &[c0.clone(), c1.clone(), c2.clone(), c3.clone()],
            &std::collections::HashSet::new(),
            0.0,
            0.05,
            2.0,
            2,
        );
        assert_eq!(plan.cap_drops, 2);
        assert_eq!(plan.to_add.len(), 2);
        let kept: std::collections::HashSet<_> =
            plan.to_add.iter().map(|x| x.0).collect();
        assert!(kept.contains(&c0.clip_id));
        assert!(kept.contains(&c1.clip_id));
        assert!(!kept.contains(&c2.clip_id));
        assert!(!kept.contains(&c3.clip_id));
    }

    #[test]
    fn compute_sched_plan_no_cap_drops_under_cap() {
        let c0 = sched(0.0, 5.0);
        let plan = compute_sched_plan(
            &[c0.clone()],
            &std::collections::HashSet::new(),
            0.0,
            0.05,
            2.0,
            16,
        );
        assert_eq!(plan.cap_drops, 0);
    }

    #[test]
    fn compute_sched_plan_keeps_live_when_still_wanted() {
        let c = sched(0.0, 5.0);
        let mut live = std::collections::HashSet::new();
        live.insert(c.clip_id);
        let plan = compute_sched_plan(&[c.clone()], &live, 2.0, 0.05, 2.0, 16);
        assert!(plan.to_add.is_empty());
        assert!(plan.to_remove.is_empty());
    }

    #[test]
    fn compute_sched_plan_seek_onto_edge_emits_add_with_zero_offset() {
        // Seek-onto-edge: playhead at t = clip.timeline_in exactly. The
        // structural piece of the SLO is that scheduler emits Add this
        // tick (so the fold thread starts mixing on the next batch).
        let c = sched(10.0, 5.0);
        let plan = compute_sched_plan(
            &[c.clone()],
            &std::collections::HashSet::new(),
            10.0,
            0.05,
            2.0,
            16,
        );
        assert_eq!(plan.to_add.len(), 1);
        assert_eq!(plan.to_add[0].0, c.clip_id);
        // Source offset = source_in + (10 - 10) = 0.0.
        assert!((plan.to_add[0].2 - 0.0).abs() < 1e-9);
    }

    #[test]
    fn build_sched_clips_skips_video_tracks_and_disabled_tracks() {
        let mut project = vance_timeline::Project::new("P");
        let mi = audio_media("/tmp/a.wav");
        let mid = mi.id;
        project.media.insert(mid, mi);
        let mut seq = vance_timeline::Sequence::new("S", vance_core::FrameRate::FPS_24);

        // Audio track, enabled — included.
        let mut at = vance_timeline::Track::new(TrackKind::Audio, "A1");
        at.clips.push(Arc::new(
            vance_timeline::Clip::new(mid, "c1", t(0), t(0), t(3)).unwrap(),
        ));
        seq.tracks.push(Arc::new(at));

        // Video track — excluded even if it overlaps a clip.
        let mut vt = vance_timeline::Track::new(TrackKind::Video, "V1");
        vt.clips.push(Arc::new(
            vance_timeline::Clip::new(mid, "v1", t(0), t(0), t(3)).unwrap(),
        ));
        seq.tracks.push(Arc::new(vt));

        // Disabled audio track — excluded.
        let mut disabled = vance_timeline::Track::new(TrackKind::Audio, "A_off");
        disabled.enabled = false;
        disabled.clips.push(Arc::new(
            vance_timeline::Clip::new(mid, "c_off", t(0), t(0), t(3)).unwrap(),
        ));
        seq.tracks.push(Arc::new(disabled));

        let scheds = build_sched_clips(&seq, &project);
        assert_eq!(scheds.len(), 1, "only the enabled audio track contributes");
    }

    #[test]
    fn collect_audio_sources_at_includes_preload_window_clips() {
        // Phase 7.4 M4: cold-seek to a position slightly before a
        // clip's start should pre-spawn the source so the
        // seek-onto-edge SLO holds. PRELOAD_SECONDS=2.0, so a probe at
        // t=4.0 with a clip starting at t=5.0 must return the clip
        // (1.0s lead), with from_seconds = source_in + max(0, 4-5) = 0.
        let mut project = vance_timeline::Project::new("P");
        let mi = audio_media("/tmp/clip.wav");
        let mid = mi.id;
        project.media.insert(mid, mi);
        let mut seq = vance_timeline::Sequence::new("S", vance_core::FrameRate::FPS_24);
        let mut at = vance_timeline::Track::new(TrackKind::Audio, "A1");
        // Clip at timeline 5..10, source_in=0.
        at.clips.push(Arc::new(
            vance_timeline::Clip::new(mid, "c", t(5), t(0), t(5)).unwrap(),
        ));
        seq.tracks.push(Arc::new(at));

        // Within preload (4.0 is 1.0s before clip start).
        let pre = collect_audio_sources_at(&seq, &project, 4.0);
        assert_eq!(pre.len(), 1, "preload window should include t-1s-ahead clip");
        assert!((pre[0].from_seconds - 0.0).abs() < 1e-9);

        // Outside preload (2.0 is 3.0s before clip start; PRELOAD=2.0).
        let outside = collect_audio_sources_at(&seq, &project, 2.0);
        assert!(outside.is_empty(), "clip outside preload must be excluded");

        // Active overlap (7.0 is 2.0s into the clip).
        let active = collect_audio_sources_at(&seq, &project, 7.0);
        assert_eq!(active.len(), 1);
        assert!((active[0].from_seconds - 2.0).abs() < 1e-9);

        // Past the clip (11.0 > clip end 10.0).
        let past = collect_audio_sources_at(&seq, &project, 11.0);
        assert!(past.is_empty(), "clips behind the playhead must be excluded");
    }

    #[test]
    fn compute_sched_plan_with_initial_live_seed_emits_no_redundant_adds() {
        // Phase 7.4 M4: when the scheduler's `live` set is seeded with
        // the initial sources from `start_audio_mix`, the first tick
        // must NOT issue a fresh Add for the same clip. Without the
        // seed, the spawn-then-respawn glitch would interrupt audio
        // ~50 ms after playback begins.
        let c = sched(0.0, 5.0);
        let mut live = std::collections::HashSet::new();
        live.insert(c.clip_id);
        let plan = compute_sched_plan(&[c.clone()], &live, 0.0, 0.05, 2.0, 16);
        assert!(plan.to_add.is_empty(), "seeded live set must suppress respawn");
        assert!(plan.to_remove.is_empty());
    }
}
