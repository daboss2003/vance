//! The Engine ties the four Phase-1 crates together inside the host process:
//!
//! - `vance-viewport`: owns the native overlay surface
//! - `vance-render`: owns the wgpu device + YUV→RGB pipeline
//! - `vance-decoder`: a sidecar process producing decoded frames
//!
//! Lives in a `Mutex` inside Tauri's app state. Methods are blocking and
//! intended to be called from Tauri command handlers.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vance_audio::AudioPlayer;
use vance_decoder::{DecoderClient, StreamInfo};
use vance_render::{Renderer, RendererConfig};
use vance_timeline::{
    Clip, EditLog, EditOp, Effect, EffectId, EffectValue, MediaItem, MediaItemId, MediaKind,
    Project, Sequence, SequenceId, TitleParams, Track, TrackId, TrackKind,
};
use vance_viewport::{HostWindowHandle, Viewport, ViewportBounds};

use crate::addons::AddonHost;
use crate::comp_playback::{
    self, present_composed, timeline_rt, PlaybackInputs, PlaybackTarget, SequencePlayback,
};
use crate::compose::{
    compose_frame, output_spec_for, plan_requests, ComposeCtx, ComposeResources, FrameProvider,
    FrameRequest, LutCache, Prefetched, Purpose, SkipKind, SkippedLayer, ViewerState,
};
use crate::decoder_pool::{total_ram_bytes, DecoderPool, MemoryBudget};
use crate::layer_feed::{FeedPolicy, StreamProvider};

/// Errors surfaced through Tauri command handlers as plain strings.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("viewport: {0}")]
    Viewport(#[from] vance_viewport::ViewportError),

    #[error("render: {0}")]
    Render(#[from] vance_render::RenderError),

    #[error("{}", decoder_error_message(.0))]
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

    /// The timeline refused an edit; the message is for the user as is
    /// (e.g. "no clip starts at the cut — a transition needs two clips
    /// that meet").
    #[error("{0}")]
    EditRefused(String),

    /// An export stopped because a frame would be missing content; the
    /// message names the clip and why.
    #[error("{0}")]
    Export(String),

    /// The export was cancelled ([`ExportCanceller::cancel`]); nothing was
    /// written to its destination.
    #[error("Export cancelled")]
    Cancelled,

    /// Opening a file didn't come back (a hung or disconnected drive); the
    /// file's name.
    #[error("{0} didn't respond (is the drive connected?)")]
    NotResponding(String),

    /// An import waiting on its file was cancelled
    /// ([`EngineHandle::cancel_import`]); nothing was added.
    #[error("{}", IMPORT_CANCELLED)]
    ImportCancelled,
}

impl EngineError {
    fn from_core(e: vance_core::Error) -> Self {
        EngineError::Project(e.to_string())
    }

    /// A rejected edit op: the timeline's own sentence, without the
    /// "project: invalid argument:" wrapping.
    fn from_edit(e: vance_timeline::TimelineError) -> Self {
        match e {
            vance_timeline::TimelineError::InvalidArgument(m) => EngineError::EditRefused(m),
            other => EngineError::Project(other.to_string()),
        }
    }
}

/// How a decoder error reads in the UI: "decoder: " and the client's
/// wording, except a bare elementary stream (`.h264`, `.hevc`), which says
/// what is wrong and how to fix it.
fn decoder_error_message(e: &vance_decoder::client::ClientError) -> String {
    use vance_decoder::{client::ClientError, OpenError};
    let ClientError::Open(OpenError::NeedsContainer { format }) = e else {
        return format!("decoder: {e}");
    };
    let (codec, ext) = match format.as_str() {
        "h264" => ("H.264".to_string(), "h264"),
        "hevc" => ("HEVC (H.265)".to_string(), "hevc"),
        "mpegvideo" => ("MPEG-1/2 video".to_string(), "m2v"),
        other => (other.to_uppercase(), other),
    };
    // Intentional: the command re-encodes. `-c copy` from a bare stream with
    // B-frames drops the first frames and leaves the rest out of order in
    // MP4, and fails for MKV and MPEG-TS (FFmpeg 7.1 and 8.1).
    format!(
        "This is a raw {codec} stream with no container (no timestamps), so Vance can't \
         seek or play it. Convert it to MP4 first, e.g. \
         `ffmpeg -i in.{ext} -c:v libx264 -crf 17 out.mp4`. A lossless `-c copy` remux \
         only keeps the timing right for streams without B-frames."
    )
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
    /// M1: compositing canvas `[width, height]`; `None` until the first
    /// clip sets it (renderers then use 1920×1080).
    pub frame_size: Option<[u32; 2]>,
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
    /// GOAT-4: where the track's main output goes, as serde writes
    /// [`vance_timeline::BusOutput`]: `"Master"` or `{ "Bus": "<bus id>" }`.
    pub output: vance_timeline::BusOutput,
    /// GOAT-4: aux sends, `{ target, level_db, pre_fader }` each.
    pub sends: Vec<vance_timeline::Send>,
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
    /// Duration of the window as it renders (an over-long stored one is
    /// capped at the shorter clip), in seconds.
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
    /// M1: compositing blend mode (`BlendMode::as_str`, e.g. "soft_light").
    pub blend_mode: String,
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
    vance_core::RationalTime::new(ms.max(0), 1000).unwrap_or(vance_core::RationalTime::ZERO)
}

/// Current wall-clock time as Unix seconds (UTC), for review-comment
/// timestamps. `0` if the clock is before the epoch (never, in practice).
fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// GOAT-9 — smart-reframe transform for one frame. Given the subject at
/// normalized source UV `subj` and a target aspect (`tgt_w:tgt_h`) plus an
/// extra `zoom` (≥1), returns the `(translate, scale)` for a "transform"
/// effect that fills a target-aspect canvas with a subject-centered,
/// undistorted window of the source. The compositor fits the source into
/// the canvas without distortion (ADR 0007), so filling it takes one
/// uniform scale: the cover factor `max(ar_s/ar_t, ar_t/ar_s)`. On the
/// filled axis translate is then a fraction of the canvas, as before. The
/// subject is clamped so the window never runs past a source edge (no
/// black bars).
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
    let cover = sx.max(sy);
    ([tx, ty], [cover, cover])
}

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
    EffectValue::Keyframes {
        keyframes: kfs.to_vec(),
    }
    .eval_number(t)
    .unwrap_or(default)
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
    /// Display aspect ratio (width / height, square pixels): a video or
    /// image item's stored size × its pixel aspect once the engine has
    /// measured it, a title / MoGraph / Fusion item's own size. `None` when
    /// not measured yet (and for audio).
    pub display_aspect: Option<f64>,
}

/// Width / height of a `w × h` picture with sample aspect `sar` (pixel
/// width / height), or `None` for an empty one.
fn aspect_of((w, h): (u32, u32), (n, d): (u32, u32)) -> Option<f64> {
    let pixel = if n > 0 && d > 0 {
        f64::from(n) / f64::from(d)
    } else {
        1.0
    };
    (w > 0 && h > 0).then(|| f64::from(w) * pixel / f64::from(h))
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
            // Synthetic media know their size; files get theirs from the
            // engine's measurements (`Engine::media_info`).
            display_aspect: match m.kind {
                MediaKind::Title => m
                    .title
                    .as_ref()
                    .and_then(|t| aspect_of((t.width, t.height), (1, 1))),
                MediaKind::MoGraph => m.mograph.as_ref().and_then(|i| {
                    aspect_of((i.template.canvas.width, i.template.canvas.height), (1, 1))
                }),
                MediaKind::Fusion => m
                    .fusion
                    .as_ref()
                    .and_then(|c| aspect_of((c.width, c.height), (1, 1))),
                MediaKind::Video | MediaKind::Image | MediaKind::Audio => None,
            },
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
        // prefix that mixes pre- and post-write state. Timed: on a dead
        // mount even `stat` blocks (under the engine lock, for the media
        // list); a file that doesn't answer counts as unreadable.
        use crate::decoder_pool::fs_probe::{metadata_within, DEFAULT_TIMEOUT};
        let (mtime_nanos, len_bytes) = match metadata_within(&m.path, DEFAULT_TIMEOUT) {
            Ok(Some((mtime, len))) => {
                let mtime = mtime
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos() as i128)
                    .unwrap_or(0);
                (mtime, len)
            }
            _ => (0i128, 0u64),
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
                config_path: v
                    .config_path
                    .as_ref()
                    .map(|c| c.to_string_lossy().into_owned()),
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
pub(crate) fn color_space_to_str(s: vance_color::ColorSpace) -> String {
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
pub(crate) fn color_space_from_str(s: &str) -> Option<vance_color::ColorSpace> {
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

/// What every engine command answers, at once, while an export holds the
/// engine. FROZEN: the UI matches this exact string.
pub const EXPORT_IN_PROGRESS: &str = "Export in progress";

/// Shared, internally-mutable engine handle. Cheap to clone.
#[derive(Clone)]
pub struct EngineHandle {
    engine: Arc<Mutex<Engine>>,
    /// An export holds (or is about to hold) the engine.
    exporting: Arc<AtomicBool>,
    /// The engine's export canceller, reachable without the lock.
    canceller: ExportCanceller,
    /// The monitors' latest bounds, applied to their windows at once (even
    /// during an export) and to their surfaces when the engine is free.
    viewports: Arc<ViewportSlots>,
    /// The imports opening their files, off the engine lock.
    imports: Arc<Imports>,
}

impl Default for EngineHandle {
    fn default() -> Self {
        Self::with_engine(Engine::default())
    }
}

/// How long [`EngineHandle::engine`] waits for the lock between checks of
/// the export flag: an export that starts meanwhile is noticed this soon.
const LOCK_SLICE: Duration = Duration::from_millis(50);

/// One of the two monitors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Monitor {
    Record,
    Source,
}

/// A monitor's native window, as the lock-free bounds path moves it.
trait MonitorWindow: Send + Sync {
    fn move_to(&self, bounds: ViewportBounds);
}

impl MonitorWindow for Viewport {
    fn move_to(&self, bounds: ViewportBounds) {
        self.set_bounds(bounds);
    }
}

/// The monitors' windows outside the engine lock, and the bounds whose
/// surface sizing waits for the engine.
#[derive(Default)]
struct ViewportSlots {
    record: Mutex<Option<Arc<dyn MonitorWindow>>>,
    source: Mutex<Option<Arc<dyn MonitorWindow>>>,
    /// Bounds moved to while the engine was busy: [record, source].
    unfitted: Mutex<[Option<ViewportBounds>; 2]>,
}

impl ViewportSlots {
    fn window(&self, monitor: Monitor) -> Option<Arc<dyn MonitorWindow>> {
        match monitor {
            Monitor::Record => self.record.lock().clone(),
            Monitor::Source => self.source.lock().clone(),
        }
    }

    fn slot(monitor: Monitor) -> usize {
        match monitor {
            Monitor::Record => 0,
            Monitor::Source => 1,
        }
    }
}

impl EngineHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// A handle around an existing engine.
    pub fn with_engine(engine: Engine) -> Self {
        let canceller = engine.export_canceller();
        let viewports = engine.viewport_slots.clone();
        let imports = engine.imports.clone();
        Self {
            engine: Arc::new(Mutex::new(engine)),
            exporting: Arc::default(),
            canceller,
            viewports,
            imports,
        }
    }

    /// Cancels the export in progress, lock-free (see [`ExportCanceller`]).
    pub fn export_canceller(&self) -> ExportCanceller {
        self.canceller.clone()
    }

    /// The engine, waiting for it however long. For setup and internal use;
    /// commands use [`Self::engine`].
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, Engine> {
        self.engine.lock()
    }

    /// The engine for a command: [`EXPORT_IN_PROGRESS`] at once while an
    /// export holds it (one that starts while this waits is noticed within
    /// [`LOCK_SLICE`]), else the engine once the command before is done.
    pub fn engine(&self) -> Result<parking_lot::MutexGuard<'_, Engine>, String> {
        loop {
            if self.is_exporting() {
                return Err(EXPORT_IN_PROGRESS.to_string());
            }
            if let Some(guard) = self.engine.try_lock_for(LOCK_SLICE) {
                // Intentional: the flag is checked again with the lock held,
                // so a command never runs inside an export's window.
                if self.is_exporting() {
                    return Err(EXPORT_IN_PROGRESS.to_string());
                }
                return Ok(guard);
            }
        }
    }

    /// Whether an export holds (or is about to hold) the engine.
    pub fn is_exporting(&self) -> bool {
        self.exporting.load(Ordering::Acquire)
    }

    /// Run an export with the engine: other commands get
    /// [`EXPORT_IN_PROGRESS`] meanwhile. A second export at the same time
    /// gets it too. A cancel from the moment this is called counts, even
    /// while it still waits for the command before: the export then doesn't
    /// start, and this answers "Export cancelled".
    pub fn run_export<T>(&self, f: impl FnOnce(&mut Engine) -> T) -> Result<T, String> {
        if self.exporting.swap(true, Ordering::AcqRel) {
            return Err(EXPORT_IN_PROGRESS.to_string());
        }
        struct Done<'a>(&'a AtomicBool);
        impl Drop for Done<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _done = Done(&self.exporting);
        // Armed before the wait below, and disarmed after `f` (dropped
        // before `_done`, so a new export can't be armed and then disarmed).
        let _armed = self.canceller.arm();
        // Waits for the command before to finish (a short wait: commands
        // started after the flag was set bail out instead of locking).
        let mut engine = self.engine.lock();
        if self.canceller.is_cancelled() {
            return Err(EngineError::Cancelled.to_string());
        }
        let out = f(&mut engine);
        // The monitors moved meanwhile: their surfaces follow now.
        let unfitted = std::mem::take(&mut *self.viewports.unfitted.lock());
        for (monitor, bounds) in [Monitor::Record, Monitor::Source].into_iter().zip(unfitted) {
            if let Some(bounds) = bounds {
                let _ = engine.fit_monitor_surface(monitor, bounds);
            }
        }
        Ok(out)
    }

    /// Import a media file without holding the engine while it opens (a
    /// slow or hung drive can take long; after [`IMPORT_PROBE_LIMIT`] it
    /// "didn't respond"). If it hasn't opened after [`IMPORT_WAITING_AFTER`],
    /// `waiting` runs, [`Self::cancel_import`] can stop it from then on
    /// ([`IMPORT_CANCELLED`]), and `settled` runs once the wait ends, however
    /// it ends. The file is then added to the project (with whether it has
    /// sound), after an export if one started meanwhile.
    pub fn import_file(
        &self,
        path: &str,
        waiting: impl FnOnce() + Send + 'static,
        settled: impl FnOnce(),
    ) -> Result<MediaItemInfo, String> {
        let limit = {
            let engine = self.engine()?;
            if engine.current_project.is_none() {
                return Err(EngineError::NoProject.to_string());
            }
            engine.import_probe_limit()
        };
        let kind = ProbeKind::Video { with_audio: true };
        let (probed, waited) = self.imports.probe(path, kind, limit, waiting);
        if waited {
            settled();
        }
        // Intentional: waits an export out; the file is open already.
        probed
            .and_then(|probed| self.engine_after_export().add_probed(path, probed))
            .map_err(|e| e.to_string())
    }

    /// Stop the imports that said they're waiting on their files: they
    /// answer [`IMPORT_CANCELLED`] and add nothing. Lock-free.
    pub fn cancel_import(&self) {
        self.imports.cancel_waiting();
    }

    /// A monitor moves to `bounds` (the UI reports them every 250 ms): its
    /// window at once, even while an export holds the engine (a dialog that
    /// opens hides the monitors, and must be clickable); its surface when
    /// the engine is free within `wait`, else when the export ends.
    pub fn set_monitor_bounds(
        &self,
        monitor: Monitor,
        bounds: ViewportBounds,
        wait: Duration,
    ) -> Result<(), String> {
        let window = self
            .viewports
            .window(monitor)
            .ok_or_else(|| EngineError::NoViewport.to_string())?;
        window.move_to(bounds);
        let slot = ViewportSlots::slot(monitor);
        match self.lock_within(wait) {
            Some(mut engine) => {
                self.viewports.unfitted.lock()[slot] = None;
                engine
                    .fit_monitor_surface(monitor, bounds)
                    .map_err(|e| e.to_string())
            }
            None => {
                self.viewports.unfitted.lock()[slot] = Some(bounds);
                Ok(())
            }
        }
    }

    /// The engine for the step that applies a slow command's finished work
    /// (a transcript, a mask, an import): waits out an export instead of
    /// answering [`EXPORT_IN_PROGRESS`], so the work isn't thrown away.
    pub fn engine_after_export(&self) -> parking_lot::MutexGuard<'_, Engine> {
        loop {
            if !self.is_exporting() {
                if let Some(guard) = self.engine.try_lock_for(LOCK_SLICE) {
                    // Intentional: checked again with the lock held, as in
                    // `engine`: an export that started meanwhile goes first.
                    if !self.is_exporting() {
                        return guard;
                    }
                }
            }
            std::thread::sleep(LOCK_SLICE);
        }
    }

    /// The engine within `wait`, or `None` while something (an export)
    /// holds it longer: for the UI's periodic polls, which keep their old
    /// answer (skip) rather than fail.
    pub fn lock_within(&self, wait: Duration) -> Option<parking_lot::MutexGuard<'_, Engine>> {
        if self.is_exporting() {
            return None;
        }
        self.engine.try_lock_for(wait)
    }
}

/// Cancels the export in progress from another thread: the export holds the
/// engine lock throughout, so this works without it. Cancelling sets a flag
/// the frame loop checks and kills the export pool's busy decoders (one
/// stuck opening a file on a hung drive, say), so the export returns
/// [`EngineError::Cancelled`] promptly; the encoder then drops its partial
/// file. A cancel with no export running does nothing.
#[derive(Clone, Default)]
pub struct ExportCanceller(Arc<CancelState>);

#[derive(Default)]
struct CancelState {
    cancelled: AtomicBool,
    running: Mutex<Running>,
    /// Test support: see [`ExportCanceller::on_audio_pushed`].
    audio_pushed: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct Running {
    /// An export is in progress (from its first step).
    active: bool,
    /// Its audio mixdown runs (its decoder is watched).
    mixing_audio: bool,
    /// Its decoders, once its pool exists.
    sidecars: Option<crate::decoder_pool::SidecarRegistry>,
    /// Its audio mixdown's decoder, while that runs.
    audio: Option<vance_audio::AudioKill>,
}

impl ExportCanceller {
    /// Cancel the running export, if any.
    pub fn cancel(&self) {
        let running = self.0.running.lock();
        if !running.active {
            return;
        }
        self.0.cancelled.store(true, Ordering::Release);
        let killed = running.sidecars.as_ref().map_or(0, |r| r.kill_busy());
        if let Some(audio) = running.audio.as_ref() {
            audio.kill();
        }
        tracing::info!(killed, "export cancelled");
    }

    /// An export starts: cancels count until the guard drops. Nested (the
    /// export `run_export` armed calls this again) it changes nothing, so a
    /// cancel that came meanwhile stays.
    fn arm(&self) -> CancelArmed<'_> {
        let mut running = self.0.running.lock();
        if running.active {
            return CancelArmed(self, false);
        }
        self.0.cancelled.store(false, Ordering::Release);
        *running = Running {
            active: true,
            mixing_audio: false,
            sidecars: None,
            audio: None,
        };
        CancelArmed(self, true)
    }

    /// Whether the running export is mixing its audio down now.
    // Intentional: lib API for the integration tests (they wait for the
    // mixdown to start); the bin target never calls it.
    #[allow(dead_code)]
    pub fn mixing_audio(&self) -> bool {
        self.0.running.lock().mixing_audio
    }

    /// Test support: `hook` runs in each export once its audio is pushed,
    /// just before the encoder is told to finish (to cancel at that moment).
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn on_audio_pushed(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.0.audio_pushed.lock() = Some(Arc::new(hook));
    }

    fn audio_pushed(&self) {
        let hook = self.0.audio_pushed.lock().clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// The mixdown's audio decoder, killed by a cancel (at once if one came
    /// already). Only while an export runs.
    fn watch_audio(&self, audio: vance_audio::AudioKill) {
        let mut running = self.0.running.lock();
        if !running.active {
            return;
        }
        if self.is_cancelled() {
            audio.kill();
        }
        running.audio = Some(audio);
        running.mixing_audio = true;
    }

    fn unwatch_audio(&self) {
        let mut running = self.0.running.lock();
        running.audio = None;
        running.mixing_audio = false;
    }

    /// Its decoders, killed by a cancel (at once if one came already).
    fn watch(&self, sidecars: crate::decoder_pool::SidecarRegistry) {
        let mut running = self.0.running.lock();
        if self.is_cancelled() {
            sidecars.kill_busy();
        }
        running.sidecars = Some(sidecars);
    }

    fn end(&self) {
        let mut running = std::mem::take(&mut *self.0.running.lock());
        if let Some(registry) = running.sidecars.take() {
            registry.resume();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
}

/// Keeps an [`ExportCanceller`] armed for one export (disarms on drop if
/// it armed it).
struct CancelArmed<'a>(&'a ExportCanceller, bool);

impl Drop for CancelArmed<'_> {
    fn drop(&mut self) {
        if self.1 {
            self.0.end();
        }
    }
}

/// What an import stopped by [`EngineHandle::cancel_import`] answers.
/// FROZEN: the UI matches this exact string.
pub const IMPORT_CANCELLED: &str = "Import cancelled";

/// After how long an import still waiting on its file says so (the UI then
/// shows "Waiting for <file>…" with Cancel).
pub const IMPORT_WAITING_AFTER: Duration = Duration::from_secs(2);

/// How long an import waits on its file before "didn't respond": a slow
/// network share can take a while, and the UI offers Cancel meanwhile.
const IMPORT_PROBE_LIMIT: Duration = Duration::from_secs(120);

/// A media item's canvas size, as far as it can be told now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Measured {
    Size((u32, u32)),
    /// It has none: audio, a size outside the canvas limits, or a file
    /// that's gone or won't open.
    Unmeasurable,
    /// Its file didn't answer in time (a hung drive): unknown for now.
    Unknown,
}

/// A background measure's answer: the file's stream (`None`: not a video).
type BackgroundMeasure = Result<Option<StreamInfo>, EngineError>;

/// How long a background measure waits on a file before giving up (and
/// killing its decoder): nothing waits on it meanwhile.
const BACKGROUND_MEASURE_LIMIT: Duration = Duration::from_secs(30);

/// What an import learns from its file (opened off the engine lock).
enum Probed {
    Video {
        stream: StreamInfo,
        /// Whether it has an audio stream (`None`: not asked, or unknown).
        has_audio: Option<bool>,
    },
    Audio(vance_audio::AudioStreamInfo),
}

#[derive(Clone, Copy)]
enum ProbeKind {
    /// A video (or still) file; `with_audio` also asks about its audio.
    Video { with_audio: bool },
    /// An audio-only file.
    Audio,
}

/// The imports opening their files, for [`EngineHandle::cancel_import`]
/// (lock-free), and idle sidecars so each import doesn't spawn its own.
#[derive(Default)]
struct Imports {
    next_id: std::sync::atomic::AtomicU64,
    in_flight: Mutex<Vec<InFlight>>,
    idle_video: Mutex<Option<DecoderClient>>,
    idle_audio: Mutex<Option<vance_audio::AudioClient>>,
}

struct InFlight {
    id: u64,
    /// The UI was told it's waiting: Cancel applies to it.
    waiting: bool,
    cancelled: bool,
    /// Its sidecar was killed for taking past the limit.
    timed_out: bool,
    /// Kills the sidecar opening its file now.
    kill: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Imports {
    /// Open `path` as `kind`, waiting at most `limit`; `waiting` runs once
    /// if it takes [`IMPORT_WAITING_AFTER`]. Returns whether it ran.
    fn probe(
        self: &Arc<Self>,
        path: &str,
        kind: ProbeKind,
        limit: Duration,
        waiting: impl FnOnce() + Send + 'static,
    ) -> (Result<Probed, EngineError>, bool) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.in_flight.lock().push(InFlight {
            id,
            waiting: false,
            cancelled: false,
            timed_out: false,
            kill: None,
        });
        let imports = self.clone();
        let timer = Watchdog::arm(IMPORT_WAITING_AFTER, move || {
            imports.with(id, |f| f.waiting = true);
            waiting();
        });
        let imports = self.clone();
        // A file on a hung drive never opens: its sidecar is killed then.
        let watchdog = Watchdog::arm(limit, move || {
            let kill = imports.with(id, |f| {
                f.timed_out = true;
                f.kill.clone()
            });
            if let Some(kill) = kill.flatten() {
                kill();
            }
        });
        let probed = self.open(id, path, kind);
        let timed_out = watchdog.disarm();
        let waited = timer.disarm();
        let cancelled = {
            let mut in_flight = self.in_flight.lock();
            let at = in_flight.iter().position(|f| f.id == id);
            at.is_some_and(|at| in_flight.swap_remove(at).cancelled)
        };
        let result = if cancelled {
            Err(EngineError::ImportCancelled)
        } else if timed_out {
            Err(EngineError::NotResponding(file_label(path)))
        } else {
            probed
        };
        (result, waited)
    }

    /// Cancel the imports the UI was told are waiting.
    fn cancel_waiting(&self) {
        let kills: Vec<_> = {
            let mut in_flight = self.in_flight.lock();
            in_flight
                .iter_mut()
                .filter(|f| f.waiting)
                .map(|f| {
                    f.cancelled = true;
                    f.kill.clone()
                })
                .collect()
        };
        let cancelled = kills.len();
        kills.into_iter().flatten().for_each(|kill| kill());
        tracing::info!(cancelled, "imports cancelled");
    }

    fn with<T>(&self, id: u64, f: impl FnOnce(&mut InFlight) -> T) -> Option<T> {
        self.in_flight.lock().iter_mut().find(|i| i.id == id).map(f)
    }

    /// The sidecar now opening import `id`'s file (killed at once if the
    /// import was cancelled already); `None` when it's done.
    fn watch(&self, id: u64, kill: Option<Arc<dyn Fn() + Send + Sync>>) {
        let stop = self
            .with(id, |f| {
                f.kill = kill.clone();
                f.cancelled || f.timed_out
            })
            .unwrap_or(false);
        if let (true, Some(kill)) = (stop, kill) {
            kill();
        }
    }

    fn open(&self, id: u64, path: &str, kind: ProbeKind) -> Result<Probed, EngineError> {
        match kind {
            ProbeKind::Audio => {
                let info = self
                    .open_audio(id, path)?
                    .map_err(|e| EngineError::Project(format!("open audio: {e}")))?;
                Ok(Probed::Audio(info))
            }
            ProbeKind::Video { with_audio } => {
                let stream = self.open_video(id, path)?;
                let has_audio = if with_audio {
                    match self.open_audio(id, path) {
                        Ok(Ok(_)) => Some(true),
                        // No audio stream (or none it can decode).
                        Ok(Err(vance_audio::AudioClientError::Open(_))) => Some(false),
                        _ => None,
                    }
                } else {
                    None
                };
                Ok(Probed::Video { stream, has_audio })
            }
        }
    }

    fn open_video(&self, id: u64, path: &str) -> Result<StreamInfo, EngineError> {
        let idle = self.idle_video.lock().take();
        let reused = idle.is_some();
        let mut dec = match idle {
            Some(dec) => dec,
            None => spawn_decoder()?,
        };
        let opened = self.open_with_video(id, &mut dec, path);
        // An idle sidecar may have died meanwhile: once more with a new one.
        let (mut dec, opened) = match opened {
            Err(vance_decoder::client::ClientError::Ipc(_)) if reused && !self.stopped(id) => {
                let mut fresh = spawn_decoder()?;
                let again = self.open_with_video(id, &mut fresh, path);
                (fresh, again)
            }
            other => (dec, other),
        };
        let stream = opened?;
        if dec.close().is_ok() {
            *self.idle_video.lock() = Some(dec);
        }
        Ok(stream)
    }

    fn open_with_video(
        &self,
        id: u64,
        dec: &mut DecoderClient,
        path: &str,
    ) -> Result<StreamInfo, vance_decoder::client::ClientError> {
        let kill = dec.kill_handle();
        self.watch(id, Some(Arc::new(move || kill.kill())));
        let opened = dec.open(path.to_string());
        self.watch(id, None);
        opened
    }

    /// The audio sidecar's answer (`Err` only when it can't be started).
    fn open_audio(
        &self,
        id: u64,
        path: &str,
    ) -> Result<Result<vance_audio::AudioStreamInfo, vance_audio::AudioClientError>, EngineError>
    {
        let idle = self.idle_audio.lock().take();
        let reused = idle.is_some();
        let mut audio = match idle {
            Some(audio) => audio,
            None => spawn_audio_prober()?,
        };
        let opened = self.open_with_audio(id, &mut audio, path);
        let (mut audio, opened) = match opened {
            Err(vance_audio::AudioClientError::Ipc(_)) if reused && !self.stopped(id) => {
                let mut fresh = spawn_audio_prober()?;
                let again = self.open_with_audio(id, &mut fresh, path);
                (fresh, again)
            }
            other => (audio, other),
        };
        // Alive after an answer (a file without audio is an answer too).
        let alive = matches!(opened, Ok(_) | Err(vance_audio::AudioClientError::Open(_)));
        if alive && audio.close().is_ok() {
            *self.idle_audio.lock() = Some(audio);
        }
        Ok(opened)
    }

    fn open_with_audio(
        &self,
        id: u64,
        audio: &mut vance_audio::AudioClient,
        path: &str,
    ) -> Result<vance_audio::AudioStreamInfo, vance_audio::AudioClientError> {
        let kill = audio.kill_handle();
        self.watch(id, Some(Arc::new(move || kill.kill())));
        let opened = audio.open(path.to_string());
        self.watch(id, None);
        opened
    }

    /// Import `id` was cancelled, or its sidecar killed for the limit.
    fn stopped(&self, id: u64) -> bool {
        self.with(id, |f| f.cancelled || f.timed_out)
            .unwrap_or(true)
    }
}

fn spawn_decoder() -> Result<DecoderClient, EngineError> {
    let bin = locate_decoder_binary()?;
    DecoderClient::spawn(&bin).map_err(|e| EngineError::DecoderSpawn(format!("{e}")))
}

fn spawn_audio_prober() -> Result<vance_audio::AudioClient, EngineError> {
    let bin = locate_audio_binary()?;
    vance_audio::AudioClient::spawn(&bin)
        .map_err(|e| EngineError::Project(format!("audio probe spawn: {e}")))
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

/// One paid effect's decryption material (spec §7): the unsealed content key +
/// the lease-signed plaintext hash to check the decrypted bytes against.
#[derive(Clone)]
struct CekEntry {
    cek: [u8; 32],
    plaintext_sha256: String,
}

/// M2/M4 — load-time entitlement gate (spec §6–§7). Shared (via `Arc`) between the
/// engine and the accounts layer: the accounts layer keeps it current (which
/// plugin ids are marketplace-gated, which are entitled, and — for paid effects —
/// the per-device content keys), and the engine consults it immediately before
/// instantiating a plugin. For encrypted effects the boundary is cryptographic:
/// without a valid key the `.vef` body can't be decrypted, so it can't run.
#[derive(Default)]
pub struct EntitlementGate {
    gated: parking_lot::RwLock<std::collections::HashSet<String>>,
    entitled: parking_lot::RwLock<std::collections::HashSet<String>>,
    keys: parking_lot::RwLock<std::collections::HashMap<String, CekEntry>>,
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

    /// Replace the per-effect content keys `(sku → (cek, plaintext_sha256))`.
    pub fn set_keys(&self, keys: std::collections::HashMap<String, ([u8; 32], String)>) {
        *self.keys.write() = keys
            .into_iter()
            .map(|(k, (cek, plaintext_sha256))| {
                (
                    k,
                    CekEntry {
                        cek,
                        plaintext_sha256,
                    },
                )
            })
            .collect();
    }

    /// A non-gated (side-loaded / free) plugin always loads; a gated one only
    /// while its entitlement is currently valid.
    pub fn allowed(&self, id: &str) -> bool {
        !self.gated.read().contains(id) || self.entitled.read().contains(id)
    }

    /// Decrypt an encrypted effect's `.vef` body (`nonce || AEAD ciphertext`) to
    /// the plaintext `.wasm`. Returns `None` if we hold no valid key for `id` or
    /// the decrypted bytes don't match the lease-signed hash. The key only
    /// materialises here — never persisted in plaintext.
    pub fn decrypt(&self, id: &str, body: &[u8]) -> Option<Vec<u8>> {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Key, Nonce};
        use base64::Engine as _;
        use sha2::{Digest, Sha256};

        let entry = self.keys.read().get(id)?.clone();
        if body.len() < 12 {
            return None;
        }
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&entry.cek));
        let plaintext = cipher
            .decrypt(Nonce::from_slice(&body[..12]), &body[12..])
            .ok()?;
        let got =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&plaintext));
        if got != entry.plaintext_sha256 {
            return None;
        }
        Some(plaintext)
    }
}

#[cfg(test)]
mod entitlement_gate_tests {
    use super::EntitlementGate;
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Key, Nonce};
    use base64::Engine as _;
    use sha2::{Digest, Sha256};

    #[test]
    fn decrypt_recovers_plaintext_rejects_wrong_key_and_hash() {
        let cek = [3u8; 32];
        let plaintext = b"\x00asm\x01\x00\x00\x00-plaintext-effect".to_vec();
        let nonce = [5u8; 12];
        let ct = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&cek))
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_ref())
            .unwrap();
        let mut body = nonce.to_vec();
        body.extend_from_slice(&ct); // .vef body layout: nonce || ciphertext
        let sha =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(&plaintext));

        let gate = EntitlementGate::default();
        gate.set_keys(std::collections::HashMap::from([(
            "com.x.a".to_string(),
            (cek, sha),
        )]));
        assert_eq!(gate.decrypt("com.x.a", &body).unwrap(), plaintext);
        assert!(
            gate.decrypt("com.x.other", &body).is_none(),
            "no key for id"
        );

        // A key whose lease-signed hash disagrees with the plaintext → rejected.
        gate.set_keys(std::collections::HashMap::from([(
            "com.x.a".to_string(),
            (cek, "not-the-hash".to_string()),
        )]));
        assert!(gate.decrypt("com.x.a", &body).is_none(), "hash mismatch");
    }
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
    /// Both monitors, for [`EngineHandle::set_monitor_bounds`] (lock-free).
    viewport_slots: Arc<ViewportSlots>,
    /// Imports opening their files (and their idle sidecars), shared with
    /// [`EngineHandle`] for its lock-free imports.
    imports: Arc<Imports>,
    /// Sequences whose load-time canvas sizing waits on a clip whose file
    /// didn't answer (a hung drive): sized once it does.
    sizing_deferred: std::collections::HashSet<SequenceId>,
    /// When [`Self::sizing_deferred`] was last retried.
    sizing_retried_at: Option<Instant>,
    /// Decodes the preview pool's scrub layers off the engine thread, per
    /// layer ([`ScrubFetcher`]); made with the pool, dropped with it.
    scrub_fetcher: Option<crate::layer_feed::ScrubFetcher>,
    /// Set (from the fetcher's threads) when a layer's decode lands.
    scrub_landed: Arc<AtomicBool>,
    /// The last scrub frame held a layer still decoding: repaint once one
    /// lands.
    scrub_pending: bool,
    /// Files measured off the engine lock for a deferred sizing, by item.
    background_measures: HashMap<MediaItemId, std::sync::mpsc::Receiver<BackgroundMeasure>>,
    /// Items a background measure found have no usable size.
    unmeasurable_media: std::collections::HashSet<MediaItemId>,
    /// The preview pool's proxies as last checked.
    pool_proxies_checked: Option<ProxiesChecked>,
    /// Parsed LUT files (clip and view LUTs) keyed by path, so repeated
    /// frames through the same LUT don't re-read it. Shared with the compose
    /// resources and so with the playback thread.
    luts: Arc<Mutex<LutCache>>,
    /// WASM / shader add-ons: plugin registry, warm instances, entitlement
    /// gate. Shared with the compose resources (ADR 0007).
    addons: Arc<Mutex<AddonHost>>,
    /// What `compose_frame` renders with, built on first use. Moved onto
    /// the playback thread while a sequence plays.
    compose_res: Option<ComposeResources>,
    /// Preview decoders (scrub and sequence playback). Moved onto the
    /// playback thread while a sequence plays.
    scrub_pool: Option<DecoderPool>,
    /// Cancels the running export from outside the engine lock.
    export_cancel: ExportCanceller,
    /// The export decoder pool, which never registers proxy paths: an
    /// export always decodes the originals (decision 5). Kept between
    /// `export_frame_rgba` reads; a stream's feeds never outlive one read
    /// or one `render_sequence`.
    export_pool: Option<DecoderPool>,
    /// Running compositing playback of the current sequence.
    seq_playback: Option<SequencePlayback>,
    /// Viewer solos (grade-node matte, keyer matte). Transient VIEWER
    /// state, never serialized, so a project never reopens in matte view.
    viewer: ViewerState,
    /// The Record viewer's timeline playhead: the last composed (scrubbed
    /// or stopped-at) time. `None` until the sequence has been shown.
    preview_t: Option<f64>,
    /// The Record viewport last showed a single-file (Open File…) frame
    /// rather than the sequence.
    record_shows_file: bool,
    /// Stored (full-resolution) frame size of probed video / image media,
    /// for the sequence canvas and the compose resources.
    media_dims: HashMap<MediaItemId, (u32, u32)>,
    /// Sample aspect ratio of each item in `media_dims`.
    media_sar: HashMap<MediaItemId, (u32, u32)>,
    /// `media_dims` changed since the compose resources got a copy.
    media_dims_stale: bool,
    /// Proxy jobs the engine queued for layers over the memory budget:
    /// job id → original path.
    auto_proxies: HashMap<String, String>,
    /// The "generating proxies" notice was queued for this project.
    proxy_notice_shown: bool,
    /// When the paused Record viewer was last repainted for the sequence.
    preview_painted_at: Option<Instant>,
    /// An edit asked for a repaint inside the coalescing interval; the
    /// next poll or edit after it does it.
    repaint_pending: bool,
    /// Files whose size probe had to be killed (a hung drive), not probed
    /// again until the time given.
    hung_paths: HashMap<String, Instant>,
    /// Old proxies being remade, by original path: put back if the remake
    /// fails, deleted after it succeeds when the engine made them.
    replaced_proxies: HashMap<String, PathBuf>,
    /// Media whose proxy marker was read and gave no size, not read again
    /// this session (until its proxy changes).
    marker_misses: std::collections::HashSet<MediaItemId>,
    /// Marker reads, for the tests.
    #[cfg(test)]
    marker_reads: usize,
    /// Media whose old-format proxy is kept because the original is
    /// offline; remade once the original is back.
    proxies_awaiting_original: std::collections::HashSet<MediaItemId>,
    /// When `proxies_awaiting_original` was last checked.
    originals_checked_at: Option<Instant>,
    /// Clips whose layer notice was queued for this project: (clip, an
    /// add-on notice rather than a "couldn't show").
    skip_noticed: std::collections::HashSet<(vance_timeline::ClipId, bool)>,
    /// UI notices not yet shown (see [`Engine::take_notices`]).
    notices: Vec<String>,
    /// Independent decoder sidecar for the Source monitor. Spawned
    /// lazily on the first `load_source`. Kept separate from the
    /// Record decoder so previewing media doesn't have to swap the
    /// timeline decoder's open file (and vice versa).
    source_decoder: Option<DecoderClient>,
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
    /// Single-file (Open File…) decoder sidecar; spawned on first
    /// `open_file`. **Moved out** during file playback (the scheduler
    /// thread owns it then) and reclaimed on stop.
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
    /// Last-applied viewport bounds (in physical pixels).
    last_bounds: ViewportBounds,
    /// Active single-file playback, if any.
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
    /// Last presented video PTS from the most recent single-file playback
    /// session. Set when a session is reaped (explicit stop OR auto-reap on
    /// EOS), cleared on the next start_playback. Lets the UI learn the EOS
    /// PTS without having to call stop_playback itself.
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

    /// Per-sequence undo/redo history. Populated when a sequence
    /// becomes the current one (via new_project, load_project, or
    /// set_current_sequence). Each successful edit (insert/move/trim)
    /// applies through this log so it can be undone.
    edit_logs: HashMap<SequenceId, EditLog>,

    /// Caches the last OCIO-baked display LUT keyed by its spec, so
    /// repeated `apply_view_lut` calls don't re-run OCIO's CPU processor
    /// over the cube lattice every time. Invalidated whenever the
    /// project's `ocio_view` differs from the cached spec. Phase 6.1b.
    ocio_lut_cache: Option<(vance_timeline::OcioView, Arc<vance_color::Lut3d>)>,

    /// Whether the Color-page scopes panel is visible (Phase 6.12). When
    /// false, `compute_scopes` skips the GPU readback entirely — zero
    /// cost while scopes are hidden.
    scopes_enabled: bool,
    /// Last computed scope data, returned as a fallback while playing or
    /// when the render lock is momentarily contended (so the UI keeps the
    /// previous frame's scopes instead of flickering empty).
    last_scopes: Option<vance_scopes::ScopeData>,

    /// GOAT-5 — the Motion Graphics Template render engine (font cache +
    /// rasterizer). Built on first use by whoever needs it first (see
    /// [`crate::compose::LazyShared`]), so projects without titles never
    /// scan fonts; the cell is shared with the compose resources.
    mograph_engine: crate::compose::LazyShared<vance_mograph::MographEngine>,

    /// GOAT-12 — the Fusion node-graph compositor (font cache + rasterizer).
    /// Built on first use, shared like `mograph_engine`.
    fusion_compositor: crate::compose::LazyShared<vance_fusion::Compositor>,

    /// Unit tests' stand-in for the Record viewport (see `repaint_preview`).
    #[cfg(test)]
    test_viewer: Option<TestViewer>,
    /// Unit tests' auto-proxy folder, instead of the user's media folder.
    #[cfg(test)]
    test_proxy_dir: Option<PathBuf>,
    /// Unit tests' import open limit (instead of [`IMPORT_PROBE_LIMIT`]).
    #[cfg(test)]
    test_probe_limit: Option<Duration>,
    /// Unit tests' scrub wait (see [`Engine::scrub_wait`]).
    #[cfg(test)]
    test_scrub_wait: Option<Duration>,
}

/// A headless Record viewer for unit tests: repaints read back here.
#[cfg(test)]
#[derive(Default)]
struct TestViewer {
    size: (u32, u32),
    frame: Option<Vec<u8>>,
    repaints: usize,
    /// Every repaint fails (as a lost GPU device would).
    fail: bool,
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

/// Owned by the engine while single-file (Open File…) playback runs.
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
    /// Joins to recover the decoder the loop owned during the session.
    handle: JoinHandle<DecoderClient>,
}

impl Engine {
    /// Install both viewports (Record + Source) + create a renderer
    /// that shares the wgpu instance. Called once from the Tauri
    /// `setup` callback on the main thread.
    // Intentional: only the macOS setup path calls this until the Linux and
    // Windows viewports land (Windows/Linux milestone); elsewhere the binary
    // target would flag it as dead code.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
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

        // M6 — shader add-ons compile on the (Arc-backed, cheap-to-clone)
        // device/queue without locking render_state from the render path.
        self.addons
            .lock()
            .set_gpu(renderer.device().clone(), renderer.queue().clone());

        self.viewport = Some(Arc::new(viewport));
        self.source_viewport = Some(Arc::new(source_viewport));
        *self.viewport_slots.record.lock() =
            self.viewport.clone().map(|v| v as Arc<dyn MonitorWindow>);
        *self.viewport_slots.source.lock() = self
            .source_viewport
            .clone()
            .map(|v| v as Arc<dyn MonitorWindow>);
        self.render_state = Some(Arc::new(Mutex::new(RenderState {
            renderer,
            surface_config: None,
            source_surface_config: None,
        })));
        Ok(())
    }

    /// Size a monitor's surface (and remember its bounds) for `bounds`, its
    /// window already moved there ([`EngineHandle::set_monitor_bounds`]).
    fn fit_monitor_surface(
        &mut self,
        monitor: Monitor,
        bounds: ViewportBounds,
    ) -> Result<(), EngineError> {
        if monitor == Monitor::Record {
            return self.fit_record_surface(bounds);
        }
        self.source_last_bounds = bounds;
        let viewport = self
            .source_viewport
            .as_ref()
            .ok_or(EngineError::NoViewport)?;
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
            return Err(EngineError::Project("source time base is unusable".into()));
        }

        // Map mark PTS → source seconds. Default to (0, media.duration).
        let mark_in_pts = self.source_mark_in_pts.unwrap_or(0);
        let mark_out_pts = self
            .source_mark_out_pts
            .or(info.duration_pts)
            .ok_or_else(|| {
                EngineError::Project("source media has no duration; mark an out-point first".into())
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

        let (op, label) = if overwrite {
            (EditOp::OverwriteClip { track_id, clip }, "Overwrite clip")
        } else {
            (EditOp::RippleInsertClip { track_id, clip }, "Ripple insert")
        };
        self.apply_clip_insert(op, label, track_id, media_id)?;
        Ok(new_clip_id.to_string())
    }

    /// Decode the Source media at `target_pts` and render it into
    /// the Source viewport.
    pub fn decode_source_at(&mut self, target_pts: i64) -> Result<i64, EngineError> {
        let dec = self
            .source_decoder
            .as_mut()
            .ok_or(EngineError::NoFileOpen)?;
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
        // The Source monitor shows the raw media through the single-layer
        // renderer. Pipeline: source's tagged input → linear Rec.709 (a
        // sane intermediate; the source preview doesn't honor the project
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
        if let Some(params) = source_pipeline {
            render_state.lock().renderer.set_pipeline(params);
        }
        upload_and_render_source(&render_state, &viewport, &frame, bounds)?;
        self.source_last_pts = Some(pts);
        Ok(pts)
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

    /// Reconfigure the Record surface for `bounds` (see
    /// [`Self::fit_monitor_surface`]).
    fn fit_record_surface(&mut self, bounds: ViewportBounds) -> Result<(), EngineError> {
        self.last_bounds = bounds;
        let viewport = self.viewport.as_ref().ok_or(EngineError::NoViewport)?;

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
        upload_and_render(&render_state, &viewport, &frame, self.last_bounds)?;
        self.record_shows_file = true;
        Ok(pts)
    }

    /// Begin streaming playback starting at `from_pts`. The scheduler
    /// thread takes ownership of the decoder for the duration of playback,
    /// so decoder-dependent commands can't use it until `stop_playback`.
    pub fn start_playback(&mut self, from_pts: i64) -> Result<(), EngineError> {
        tracing::info!(from_pts, "start_playback called");
        self.pause_for_edit();
        self.start_file_playback(from_pts)
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

    /// Single-file playback: the scheduler thread takes the engine's
    /// decoder for the session and plays the file's own audio as the clock.
    fn start_file_playback(&mut self, from_pts: i64) -> Result<(), EngineError> {
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
        let (audio_owner, audio) = match start_audio(&media_path, from_seconds) {
            Ok((owner, sync)) => (Some(AudioBackend::Single(owner)), Some(sync)),
            Err(e) => {
                tracing::warn!(?e, "audio: not started; video will use wall-clock");
                (None, None)
            }
        };

        let stop = Arc::new(AtomicBool::new(false));
        let last_pts = Arc::new(std::sync::atomic::AtomicI64::new(from_pts));
        let ended_at_eos = Arc::new(AtomicBool::new(false));
        let shared = FileLoop {
            bounds: self.last_bounds,
            viewport,
            render_state,
            stop: stop.clone(),
            audio,
            last_pts: last_pts.clone(),
            ended_at_eos: ended_at_eos.clone(),
        };
        let handle = std::thread::Builder::new()
            .name("vance-engine.playback".into())
            .spawn(move || file_playback_loop(decoder, info, shared))
            .map_err(|e| EngineError::PlaybackThread(format!("spawn: {e}")))?;
        self.playback = Some(PlaybackSession {
            stop,
            last_pts,
            ended_at_eos,
            handle,
        });
        self.audio_owner = audio_owner;
        self.last_resume_pts = None;
        self.record_shows_file = true;
        Ok(())
    }

    /// Stop the current playback session and return where it stopped so
    /// the UI can resume there on the next Play: the file's PTS for
    /// single-file playback, the timeline position in milliseconds for
    /// sequence playback.
    pub fn stop_playback(&mut self) -> Result<i64, EngineError> {
        if let Some(session) = self.seq_playback.take() {
            return self.stop_sequence_playback(session);
        }
        let Some(session) = self.playback.take() else {
            return Err(EngineError::NoPlayback);
        };
        session.stop.store(true, Ordering::Release);
        // Stop the audio backend. AudioBackend::shutdown drops the
        // owned cpal stream + joins the puller.
        if let Some(backend) = self.audio_owner.take() {
            backend.shutdown();
        }
        // EOS → restart from 0 next time so Play means "play again".
        let resume_pts = if session.ended_at_eos.load(Ordering::Acquire) {
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

    /// Stop compositing playback: the mixer, then the video thread, whose
    /// decoder pool and compose resources come back to the engine.
    fn stop_sequence_playback(&mut self, session: SequencePlayback) -> Result<i64, EngineError> {
        if let Some(backend) = self.audio_owner.take() {
            backend.shutdown();
        }
        // Phase 7.3: clear the snapshot bus so subsequent EditOps
        // don't try to publish to a torn-down mixer.
        self.end_mixer_session();
        // Read after the join: requests from the session's last frames
        // come back with it.
        let comp_playback::PlaybackReturn {
            mut pool,
            res,
            playhead,
            presented,
            dropped,
            proxy_requests: mut requests,
            broken_proxies: mut broken,
        } = session.stop()?;
        tracing::info!(presented, dropped, playhead, "sequence playback stopped");
        requests.extend(pool.take_proxy_requests());
        broken.extend(pool.take_broken_proxies());
        self.scrub_pool = Some(pool);
        self.compose_res = Some(res);
        self.media_dims_stale = true;
        self.preview_t = Some(playhead);
        self.handle_proxy_requests(requests, &broken);
        Ok((playhead * 1000.0).round() as i64)
    }

    /// Forget the previous project's viewer state: playhead, solos, export
    /// decoders, probed media sizes, the once-per-project notices and any
    /// notice not yet shown (it's about the old project).
    fn reset_viewer(&mut self) {
        self.notices.clear();
        self.repaint_pending = false;
        self.preview_painted_at = None;
        self.preview_t = None;
        self.record_shows_file = false;
        self.viewer = ViewerState::default();
        self.export_pool = None;
        self.media_dims.clear();
        self.media_sar.clear();
        self.media_dims_stale = true;
        self.proxy_notice_shown = false;
        self.skip_noticed.clear();
        self.proxies_awaiting_original.clear();
        self.originals_checked_at = None;
        self.replaced_proxies.clear();
        self.marker_misses.clear();
        self.pool_proxies_checked = None;
        self.hung_paths.clear();
        self.sizing_deferred.clear();
        self.sizing_retried_at = None;
        self.unmeasurable_media.clear();
        // Their threads finish on their own; the answers aren't wanted.
        self.background_measures.clear();
        if let Some(res) = self.compose_res.as_mut() {
            res.synth.clear();
        }
    }

    /// Load a `.vance/` bundle and make it the current project.
    /// Selects the first sequence as the active sequence; `None` if
    /// the project has no sequences. The previous project (if any) is
    /// discarded — a future "save before close" gate is up to the UI.
    pub fn load_project(&mut self, path: &str) -> Result<ProjectInfo, EngineError> {
        self.pause_for_edit();
        // Opened first: a bundle that won't open leaves the loaded project
        // (its playhead, solos and caches) as it was.
        let project = vance_project::open(path).map_err(|e| EngineError::Project(e.to_string()))?;
        // Drop the previous project's OCIO bake — the new project may
        // reference the same (config, src, dst) spec while the .ocio file
        // on disk differs, which would otherwise serve a stale LUT.
        self.ocio_lut_cache = None;
        self.reset_viewer();
        self.current_sequence_id = project.sequences.first().map(|s| s.id);
        self.current_project_path = Some(PathBuf::from(path));
        self.current_project = Some(project);
        self.size_legacy_sequences();
        self.retire_outdated_proxies();
        let project = self.current_project.as_ref().expect("set above");
        let mut info = ProjectInfo::from(project);
        info.current_sequence_id = self.current_sequence_id.map(|id| id.to_string());
        // Fresh edit-log per loaded sequence, seeded with that
        // sequence's current state as the undo baseline.
        self.edit_logs.clear();
        for s in &project.sequences {
            self.edit_logs
                .insert(s.id, EditLog::new(s.as_ref().clone()));
        }
        // Light up the project's saved view LUT (if any) so the
        // viewport reflects the user's last monitoring preference.
        self.apply_view_lut();
        Ok(info)
    }

    /// Load-time migration (decision 1): a sequence saved before it had a
    /// canvas size takes the size of its earliest video clip (by
    /// `timeline_in`, any video track). Not an undo step: the edit logs are
    /// seeded after it (the engine keeps no "modified" flag to set).
    fn size_legacy_sequences(&mut self) {
        let Some(project) = self.current_project.as_ref() else {
            return;
        };
        let pending: Vec<(SequenceId, Vec<MediaItemId>)> = project
            .sequences
            .iter()
            .filter(|s| s.frame_size.is_none())
            .map(|s| (s.id, video_media_by_start(s)))
            .collect();
        for (id, media) in pending {
            match self.earliest_size(&media) {
                Measured::Size(size) => {
                    let Some(project) = self.current_project.as_mut() else {
                        return;
                    };
                    if let Some(slot) = project.sequences.iter_mut().find(|s| s.id == id) {
                        Arc::make_mut(slot).frame_size = Some(size);
                    }
                }
                // A clip before the one that would size it didn't answer:
                // sized once it does (`retry_deferred_sizing`).
                Measured::Unknown => {
                    self.sizing_deferred.insert(id);
                }
                Measured::Unmeasurable => {}
            }
        }
    }

    /// The canvas size the earliest measurable of `media` (in timeline
    /// order) gives. `Unknown` when one before it can't be told now (its
    /// file didn't answer): a later clip mustn't decide meanwhile.
    fn earliest_size(&mut self, media: &[MediaItemId]) -> Measured {
        for &m in media {
            match self.measure_media(m) {
                // Intentional: a clip whose file is offline (or audio, or
                // unreadable) can't be measured, so the next one that can
                // sets the size (else the sequence keeps the 1920×1080
                // default).
                Measured::Unmeasurable => continue,
                other => return other,
            }
        }
        Measured::Unmeasurable
    }

    /// Size the sequences whose sizing waited on a file that didn't answer,
    /// once it does. Their files are measured in the background (off the
    /// engine lock); checked on preview and play, at most every
    /// [`ORIGINALS_RECHECK`].
    fn retry_deferred_sizing(&mut self) {
        if self.sizing_deferred.is_empty()
            || self
                .sizing_retried_at
                .is_some_and(|t| t.elapsed() < ORIGINALS_RECHECK)
        {
            return;
        }
        self.sizing_retried_at = Some(Instant::now());
        self.collect_background_measures();
        let deferred: Vec<SequenceId> = self.sizing_deferred.iter().copied().collect();
        for id in deferred {
            let Some(seq) = self
                .current_project
                .as_ref()
                .and_then(|p| p.sequences.iter().find(|s| s.id == id))
            else {
                self.sizing_deferred.remove(&id);
                continue;
            };
            if seq.frame_size.is_some() {
                self.sizing_deferred.remove(&id);
                continue;
            }
            let media = video_media_by_start(seq);
            match self.earliest_known_size(&media) {
                Measured::Unknown => {}
                Measured::Unmeasurable => {
                    self.sizing_deferred.remove(&id);
                }
                Measured::Size(size) => {
                    self.sizing_deferred.remove(&id);
                    self.size_deferred_sequence(id, size);
                }
            }
        }
    }

    /// As [`Self::earliest_size`], but without opening files on the engine
    /// lock: a clip not measured yet is measured in the background, and is
    /// `Unknown` until that answers.
    fn earliest_known_size(&mut self, media: &[MediaItemId]) -> Measured {
        for &m in media {
            let known = self.known_measure(m);
            match known {
                Some(Measured::Unmeasurable) => continue,
                Some(other) => return other,
                None => {
                    self.measure_in_background(m);
                    return Measured::Unknown;
                }
            }
        }
        Measured::Unmeasurable
    }

    /// What can be told about `id`'s size without opening its file (`None`:
    /// it has to be opened).
    fn known_measure(&mut self, id: MediaItemId) -> Option<Measured> {
        let Some(m) = self
            .current_project
            .as_ref()
            .and_then(|p| p.media.get(&id))
            .cloned()
        else {
            return Some(Measured::Unmeasurable);
        };
        if !matches!(m.kind, MediaKind::Video | MediaKind::Image)
            || self.media_dims.contains_key(&id)
        {
            return Some(self.measure_media(id));
        }
        if self.unmeasurable_media.contains(&id) {
            return Some(Measured::Unmeasurable);
        }
        match file_exists_now(&m.path) {
            Some(false) => Some(Measured::Unmeasurable),
            Some(true) => None,
            None => Some(Measured::Unknown),
        }
    }

    /// Open `id`'s file on a thread of its own (with its own decoder, killed
    /// after [`BACKGROUND_MEASURE_LIMIT`]) to learn its size; picked up by
    /// [`Self::collect_background_measures`]. One at a time per item.
    fn measure_in_background(&mut self, id: MediaItemId) {
        if self.background_measures.contains_key(&id) {
            return;
        }
        let Some(path) = self
            .current_project
            .as_ref()
            .and_then(|p| p.media.get(&id))
            .map(|m| m.path.to_string_lossy().into_owned())
        else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        // Intentional: a registry of its own, so `cancel_import` (meant for
        // the import the UI shows as waiting) never stops it.
        let imports = Arc::new(Imports::default());
        let spawned = std::thread::Builder::new()
            .name("vance-engine.measure".into())
            .spawn(move || {
                let kind = ProbeKind::Video { with_audio: false };
                let (probed, _) = imports.probe(&path, kind, BACKGROUND_MEASURE_LIMIT, || {});
                let _ = tx.send(probed.map(|p| match p {
                    Probed::Video { stream, .. } => Some(stream),
                    Probed::Audio(_) => None,
                }));
            });
        if spawned.is_ok() {
            self.background_measures.insert(id, rx);
        }
    }

    /// Note the sizes the background measures found.
    fn collect_background_measures(&mut self) {
        let mut done = Vec::new();
        for (id, rx) in &self.background_measures {
            match rx.try_recv() {
                Ok(answer) => done.push((*id, Some(answer))),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => done.push((*id, None)),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        for (id, answer) in done {
            self.background_measures.remove(&id);
            match answer {
                Some(Ok(Some(stream))) => self.note_media_stream(id, &stream),
                // Still not answering: measured again next time.
                Some(Err(EngineError::NotResponding(_))) | None => {}
                Some(_) => {
                    self.unmeasurable_media.insert(id);
                }
            }
        }
    }

    /// Give a sequence whose sizing was deferred its size: quietly (like
    /// the load-time migration) while it has no edits since it was loaded,
    /// else as an undo step, so its edit history stays consistent.
    fn size_deferred_sequence(&mut self, id: SequenceId, size: (u32, u32)) {
        let Some(log) = self.edit_logs.get_mut(&id) else {
            return;
        };
        let sized = if log.len() <= 1 {
            let mut seq = log.current().clone();
            seq.frame_size = Some(size);
            *log = EditLog::new(seq.clone());
            seq
        } else {
            match log.apply(EditOp::SetFrameSize { size: Some(size) }) {
                Ok(seq) => seq.clone(),
                Err(e) => {
                    tracing::warn!(error = %e, "deferred canvas size not applied");
                    return;
                }
            }
        };
        if let Some(project) = self.current_project.as_mut() {
            if let Some(slot) = project.sequences.iter_mut().find(|s| s.id == id) {
                *slot = Arc::new(sized);
            }
        }
        tracing::info!(?size, "sized a sequence once its earliest clip answered");
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
        self.reset_viewer();
        let mut project = Project::new(name);
        let mut seq = Sequence::new("Sequence 01", vance_core::FrameRate::FPS_24);
        seq.tracks
            .push(Arc::new(Track::new(TrackKind::Video, "V1")));
        seq.tracks
            .push(Arc::new(Track::new(TrackKind::Audio, "A1")));
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

    /// Import a media file by path. Briefly opens it via a decoder
    /// sidecar to capture its metadata (codec, duration, frame rate,
    /// pixel format), then registers it as a `MediaItem` in the
    /// current project's media library. Holds the engine while the file
    /// opens: the UI imports through [`EngineHandle::import_file`].
    pub fn import_media_file(&mut self, path: &str) -> Result<MediaItemInfo, EngineError> {
        if self.current_project.is_none() {
            return Err(EngineError::NoProject);
        }
        let kind = ProbeKind::Video { with_audio: false };
        let limit = self.import_probe_limit();
        let (probed, _) = self.imports.probe(path, kind, limit, || {});
        self.add_probed(path, probed?)
    }

    /// Register an opened file in the project's media library.
    fn add_probed(&mut self, path: &str, probed: Probed) -> Result<MediaItemInfo, EngineError> {
        let (stream, has_audio) = match probed {
            Probed::Video { stream, has_audio } => (stream, has_audio),
            Probed::Audio(info) => return self.add_audio_file(path, info),
        };
        self.pause_for_edit();
        if self.current_project.is_none() {
            return Err(EngineError::NoProject);
        }

        // Convert the decoder's duration_pts (in its time base) to
        // seconds. duration_pts can be None if the container didn't
        // declare one; in that case we pessimistically pick 60 s so
        // there's something draggable. Real fix in a later milestone.
        let duration = duration_from_stream(&stream)
            .unwrap_or_else(|| vance_core::RationalTime::new(60, 1).expect("60s rational"));

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
        if let Some(has_audio) = has_audio {
            item.has_audio_track = has_audio;
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
        self.note_media_stream(id, &stream);
        let info = self.media_info(&item);

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
        let limit = self.import_probe_limit();
        let (probed, _) = self.imports.probe(path, ProbeKind::Audio, limit, || {});
        self.add_probed(path, probed?)
    }

    fn add_audio_file(
        &mut self,
        path: &str,
        info: vance_audio::AudioStreamInfo,
    ) -> Result<MediaItemInfo, EngineError> {
        if self.current_project.is_none() {
            return Err(EngineError::NoProject);
        }
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
        let resolved =
            match space {
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
        let unsuited = updated.proxy_path.as_ref().is_some_and(|p| {
            file_exists_now(p) == Some(true) && !crate::proxy::matches_media(p, resolved)
        });
        let reachable = file_exists_now(&updated.path) == Some(true);
        project.media.insert(media_id, Arc::new(updated));
        // Its proxy's colours came from the old space: remade for the new one
        // (once the original is reachable, as for an old-format proxy).
        match (unsuited, reachable) {
            (true, true) => self.remake_outdated_proxies(vec![media_id], RemakeWhy::NewColourSpace),
            (true, false) => {
                self.proxies_awaiting_original.insert(media_id);
            }
            (false, _) => {}
        }
        Ok(())
    }

    /// Replace the project's working color space. Phase 6.2.
    pub fn set_project_working_color_space(&mut self, space: &str) -> Result<(), EngineError> {
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
    pub fn set_project_view_lut(&mut self, path: Option<&str>) -> Result<(), EngineError> {
        {
            let project = self
                .current_project
                .as_mut()
                .ok_or(EngineError::NoProject)?;
            project.view_lut_path = path.filter(|s| !s.is_empty()).map(PathBuf::from);
        }
        self.apply_view_lut();
        Ok(())
    }

    /// Load the project's `view_lut_path` (if any) into the renderer's
    /// view-LUT slot (shared with the compositor), or disable the slot.
    /// Called on project load, when the user picks a new LUT, and when a
    /// renderer is created.
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
        // Bound to a statement so the LUT cache unlocks before the render
        // state locks (lock order: render state → LUT cache).
        let cached = self.luts.lock().get(&path).cloned();
        if let Some(lut) = cached {
            let mut rs = rs.lock();
            rs.renderer.upload_view_lut(&lut);
            rs.renderer.set_view_lut_enable(1.0);
            return;
        }
        match vance_color::load_lut(&path) {
            Ok(lut) => {
                let arc = Arc::new(lut);
                self.luts.lock().insert(path.clone(), arc.clone());
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
    pub fn ocio_list_colorspaces(&self, config: Option<&str>) -> Result<Vec<String>, EngineError> {
        if !vance_ocio::is_available() {
            return Err(EngineError::Ocio(
                "OpenColorIO support is not enabled in this build".into(),
            ));
        }
        let path = config.filter(|s| !s.is_empty()).map(PathBuf::from);
        vance_ocio::list_colorspaces(path.as_deref()).map_err(|e| EngineError::Ocio(e.to_string()))
    }

    /// Solo a grade node's HSL-qualifier matte in the viewer, or clear it
    /// (`None`). Transient — not persisted — and repainted at once
    /// (paused workflow). A node id is unique across clips, so `_clip_id`
    /// isn't needed to find it. Phase 6.9.
    pub fn set_matte_node(
        &mut self,
        _clip_id: vance_timeline::ClipId,
        effect_id: Option<EffectId>,
    ) -> Result<(), EngineError> {
        let before = self.viewer.matte_node;
        self.viewer.matte_node = effect_id;
        let repaint = self.repaint_preview(None);
        // The UI keeps its previous state on an error; so does the engine.
        if repaint.is_err() {
            self.viewer.matte_node = before;
        }
        repaint
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
    ///
    /// A sequence frame is recomposed at the analysis size from the frame
    /// cache (no decode) and read back; `pre_view_lut` skips the view LUT
    /// so the scopes read the signal before the monitoring transform. The
    /// (pure) accumulation runs after the render lock is released.
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
        if self.playback.is_some() || self.seq_playback.is_some() || !self.scopes_enabled {
            return Ok(self.last_scopes.clone());
        }
        let Some(rs) = self.render_state.clone() else {
            return Ok(self.last_scopes.clone());
        };
        let rgba = if self.record_shows_file {
            let Some(mut guard) = rs.try_lock() else {
                return Ok(self.last_scopes.clone());
            };
            guard
                .renderer
                .render_scope_buffer(width, height, pre_view_lut)?
        } else if let Some(t) = self.preview_t {
            let frame = self.render_preview(
                &rs,
                t,
                None,
                |out| {
                    out.width = width;
                    out.height = height;
                    out.view_lut = !pre_view_lut;
                },
                PreviewSink::Rgba8 {
                    wait: false,
                    hold: true,
                },
            )?;
            match frame {
                Some(rgba) => rgba,
                None => return Ok(self.last_scopes.clone()),
            }
        } else {
            return Ok(self.last_scopes.clone());
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
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        let arc = project
            .media
            .get(&media_id)
            .ok_or(EngineError::MediaNotFound)?;
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
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
        let arc = project
            .media
            .get(&media_id)
            .ok_or(EngineError::MediaNotFound)?;
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
            .map(|p| p.media.values().map(|arc| self.media_info(arc)).collect())
            .unwrap_or_default()
    }

    /// Look up a single media item's info. `None` when no project is
    /// loaded or the id isn't in the library.
    // Intentional: lib API for the tests; the bin target no longer calls it
    // (an import returns its item's info itself).
    #[allow(dead_code)]
    pub fn media_get_info(&self, media_id: MediaItemId) -> Option<MediaItemInfo> {
        self.current_project
            .as_ref()
            .and_then(|p| p.media.get(&media_id))
            .map(|arc| self.media_info(arc))
    }

    /// `m`'s info with what the engine measured: a video / image item's
    /// display aspect.
    fn media_info(&self, m: &MediaItem) -> MediaItemInfo {
        let mut info = MediaItemInfo::from(m);
        if matches!(m.kind, MediaKind::Video | MediaKind::Image) {
            let sar = self.media_sar.get(&m.id).copied().unwrap_or((1, 1));
            info.display_aspect = self
                .media_dims
                .get(&m.id)
                .and_then(|&size| aspect_of(size, sar));
        }
        info
    }

    /// Play the current sequence from `timeline_seconds`: every enabled
    /// video track composited per frame, paced by the audio mixer's clock,
    /// straight through cuts, with gaps as black (ADR 0007). The playhead
    /// is [`Self::current_timeline_seconds`]; the session ends by itself at
    /// the end of the sequence (see [`Self::is_playing`]).
    pub fn play_sequence(&mut self, timeline_seconds: f64) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.scrub_end();
        self.poll_auto_proxies();
        self.recheck_offline_originals();
        self.retry_deferred_sizing();
        let viewport = self.viewport.clone().ok_or(EngineError::NoViewport)?;
        let render_state = self.render_state.clone().ok_or(EngineError::NoViewport)?;
        let seq = self.current_sequence_ref()?.clone();
        if seq.clip_count() == 0 {
            return Err(EngineError::Project("sequence has no clips to play".into()));
        }
        let project = self.current_project.clone().ok_or(EngineError::NoProject)?;
        let fps = comp_playback::sequence_fps(&seq);
        let from = (timeline_seconds.max(0.0) * fps).round() / fps;
        let (provider, res) = self.prepare_playback(&project, &seq, from);
        let (audio_owner, audio) = match self.start_mixer_session(from) {
            Ok((owner, sync)) => (Some(AudioBackend::Mixer(owner)), Some(sync)),
            Err(e) => {
                tracing::warn!(?e, "audio mixer not started; playback uses the wall clock");
                (None, None)
            }
        };
        let session = SequencePlayback::spawn(PlaybackInputs {
            project,
            seq,
            from_seconds: from,
            audio,
            render_state,
            target: PlaybackTarget::Viewport(viewport),
            provider,
            res,
        });
        match session {
            Ok(session) => {
                self.seq_playback = Some(session);
                self.audio_owner = audio_owner;
                self.record_shows_file = false;
                Ok(())
            }
            Err(e) => {
                if let Some(backend) = audio_owner {
                    backend.shutdown();
                }
                self.end_mixer_session();
                Err(e)
            }
        }
    }

    /// The realtime feeds and compose resources a session at `from` takes:
    /// the media sizes known (from proxy markers for every clip; probing
    /// only the first frame's proxied layers without one, so a big project
    /// doesn't stall the start), and the first frame's streams open before
    /// the clock starts, so playback opens on time.
    fn prepare_playback(
        &mut self,
        project: &Project,
        seq: &Sequence,
        from: f64,
    ) -> (StreamProvider, ComposeResources) {
        self.ensure_compose_res();
        self.ensure_scrub_pool();
        self.refresh_pool_proxies();
        let first = vance_timeline::scene_plan_at(seq, timeline_rt(from));
        let first_reqs = plan_requests(project, seq, &first, Purpose::Preview, None);
        let first_layers: Vec<(MediaItemId, String)> = first_reqs
            .iter()
            .map(|r| (r.media, r.path.clone()))
            .collect();
        self.learn_media_dims(&first_layers, true);
        // Once per media item, not per clip.
        let mut every_item = video_media_by_start(seq);
        let mut seen = std::collections::HashSet::new();
        every_item.retain(|m| seen.insert(*m));
        let every_clip: Vec<(MediaItemId, String)> =
            every_item.into_iter().map(|m| (m, String::new())).collect();
        self.learn_media_dims(&every_clip, false);
        self.sync_media_dims();
        let pool = self.scrub_pool.take().expect("ensured above");
        let res = self.compose_res.take().expect("ensured above");
        let mut provider = StreamProvider::new(pool, FeedPolicy::Realtime);
        provider.prepare(&first_reqs);
        provider.prepare(&comp_playback::upcoming_requests(
            project,
            seq,
            from,
            Purpose::Preview,
            None,
        ));
        drop(provider.frames(&first_reqs));
        (provider, res)
    }

    /// The Record viewer's timeline playhead, in seconds: the frame on
    /// screen while the sequence plays, else the last composed (scrubbed
    /// or stopped-at) time. `None` until the sequence has been shown.
    pub fn current_timeline_seconds(&self) -> Option<f64> {
        match &self.seq_playback {
            Some(session) => Some(session.playhead_seconds()),
            None => self.preview_t,
        }
    }

    /// Current sequence frame rate as fps, or `None` when no sequence.
    fn current_sequence_fps(&self) -> Option<f64> {
        let id = self.current_sequence_id?;
        let seq = self.current_project.as_ref()?.sequence(id)?;
        Some(seq.frame_rate.0.as_f64())
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
        // An export's Cancel kills this decoder too (one reading a file on
        // a hung drive never returns otherwise).
        self.export_cancel.watch_audio(audio.kill_handle());
        let out = self.bounce_range_with(
            &mut audio,
            start_s,
            end_s,
            sample_rate,
            plugin_host.as_mut(),
        );
        self.export_cancel.unwatch_audio();
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
            let project = self
                .current_project
                .as_ref()
                .ok_or(EngineError::NoProject)?;
            let seq_id = self
                .current_sequence_id
                .ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
            build_mixer_snapshot(seq)
        };
        let master_lin = snapshot.master_gain_linear;
        let total_frames =
            (((end_s - start_s) * sample_rate as f64).round() as i64).max(0) as usize;
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
            let project = self
                .current_project
                .as_ref()
                .ok_or(EngineError::NoProject)?;
            let seq_id = self
                .current_sequence_id
                .ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
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
            let out_frames = ((clip_tl_end - clip_tl_start) * sample_rate as f64).round() as usize;
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
            let out_start_frame = ((clip_tl_start - start_s) * sample_rate as f64).round() as usize;
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
                .is_none_or(|t| t.mute || (snapshot.solo_anywhere && !t.solo));
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
                dsp.reconcile_with(
                    &bm.audio_effects,
                    sample_rate as f32,
                    plugin_host.as_deref_mut(),
                );
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

    /// Show the current sequence at `timeline_seconds` in the Record
    /// viewer (seek / scrub while paused): every enabled video track
    /// composited through the scrub path; a gap shows black. Returns the
    /// shown time in milliseconds.
    pub fn decode_sequence_at(&mut self, timeline_seconds: f64) -> Result<i64, EngineError> {
        self.pause_for_edit();
        self.poll_auto_proxies();
        let viewport = self.viewport.clone().ok_or(EngineError::NoViewport)?;
        let render_state = self.render_state.clone().ok_or(EngineError::NoViewport)?;
        let t = timeline_seconds.max(0.0);
        self.preview_t = Some(t);
        self.record_shows_file = false;
        let bounds = self.last_bounds;
        self.render_preview(
            &render_state,
            t,
            None,
            |_| {},
            PreviewSink::Viewport(&viewport, bounds),
        )?;
        // Shows every edit so far: nothing is left to repaint.
        self.preview_painted_at = Some(Instant::now());
        self.repaint_pending = false;
        Ok((t * 1000.0).round() as i64)
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
    // Intentional: mirrors the `track_window` Tauri command's argument
    // list one-to-one (target clip/effect, range and tracker tuning).
    #[allow(clippy::too_many_arguments)]
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

        // Clip lookup (source_in for clip-relative time conversion) and the
        // clip's source file, which a pooled decoder opens: tracking always
        // reads the clip's own media, whatever the viewer last showed.
        let (source_in_seconds, expected_path, mut cx_kfs, mut cy_kfs) = {
            let project = self
                .current_project
                .as_ref()
                .ok_or(EngineError::NoProject)?;
            let seq_id = self
                .current_sequence_id
                .ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
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
        let mut dec = self.take_pooled_decoder(&expected_path)?;
        let (tb_num, tb_den) = dec.info.time_base;
        let (fr_num, fr_den) = dec.info.frame_rate;
        if tb_num == 0 || tb_den == 0 || fr_num == 0 || fr_den == 0 {
            self.return_pooled_decoder(dec);
            return Err(EngineError::Project(
                "tracker: invalid time_base or frame_rate (zero component)".into(),
            ));
        }
        // step_pts = (fr_den / fr_num) sec/frame  /  (tb_num / tb_den) sec/tick
        //          = fr_den * tb_den / (fr_num * tb_num)
        let step_pts =
            ((fr_den as i128 * tb_den as i128) / (fr_num as i128 * tb_num as i128)).max(1) as i64;
        // start_seconds (source-time) -> stream PTS in tb units.
        let start_pts = (start_source_seconds * tb_den as f64 / tb_num as f64).round() as i64;
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
        let anchor = (|| -> Result<_, EngineError> {
            let frame = dec
                .client
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
            Ok((template, frame.width, frame.height))
        })();
        let (template, anchor_w, anchor_h) = match anchor {
            Ok(a) => a,
            Err(e) => {
                self.return_pooled_decoder(dec);
                return Err(e);
            }
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
                let frame = dec.client.decode_at(pts).map_err(|e| {
                    EngineError::Project(format!("tracker decode @ pts {pts}: {e}"))
                })?;
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
        self.return_pooled_decoder(dec);

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
    /// NCC tracker as `track_window`, on a pooled decoder that opens the
    /// clip's source. Returns a `TrackSummary`.
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

        let (source_in_seconds, expected_path) = {
            let project = self
                .current_project
                .as_ref()
                .ok_or(EngineError::NoProject)?;
            let seq_id = self
                .current_sequence_id
                .ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
            let clip = seq
                .tracks
                .iter()
                .find_map(|t| t.clips.iter().find(|c| c.id == clip_id))
                .ok_or(EngineError::Project("reframe: clip not found".into()))?;
            let media = project
                .media
                .get(&clip.media)
                .ok_or(EngineError::MediaNotFound)?;
            (
                rt_to_seconds(clip.source_in),
                media.path.to_string_lossy().into_owned(),
            )
        };
        // A pooled decoder on the clip's own source.
        let mut dec = self.take_pooled_decoder(&expected_path)?;
        let (tb_num, tb_den) = dec.info.time_base;
        let (fr_num, fr_den) = dec.info.frame_rate;
        if tb_num == 0 || tb_den == 0 || fr_num == 0 || fr_den == 0 {
            self.return_pooled_decoder(dec);
            return Err(EngineError::Project(
                "reframe: invalid time_base/frame_rate".into(),
            ));
        }
        let step_pts =
            ((fr_den as i128 * tb_den as i128) / (fr_num as i128 * tb_num as i128)).max(1) as i64;
        let start_pts = (start_source_seconds * tb_den as f64 / tb_num as f64).round() as i64;
        let pts_to_clip_t = |pts: i64| -> f64 {
            (pts as f64) * (tb_num as f64) / (tb_den as f64) - source_in_seconds
        };

        let seed = (seed_cx.clamp(0.0, 1.0), seed_cy.clamp(0.0, 1.0));
        // Anchor frame → template + source dims.
        let anchor = (|| -> Result<_, EngineError> {
            let frame = dec
                .client
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
            Ok((tpl, frame.width, frame.height))
        })();
        let (template, src_w, src_h) = match anchor {
            Ok(a) => a,
            Err(e) => {
                self.return_pooled_decoder(dec);
                return Err(e);
            }
        };
        let mut prev_px = vance_tracker::uv_to_px(seed, src_w, src_h);

        // Track the subject → per-frame (clip_t, subject_uv).
        let mut trail: Vec<(f64, (f32, f32))> = Vec::with_capacity(n_frames as usize);
        trail.push((pts_to_clip_t(start_pts), seed));
        let mut sum_score = 1.0f32;
        let mut scored = 1u32;
        for i in 1..n_frames {
            let pts = start_pts + (i as i64) * step_pts;
            let frame = match dec.client.decode_at(pts) {
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
            let Ok(r) =
                vance_tracker::track_ncc(&template, &view, prev_px, search_radius_i, patch_half_i)
            else {
                break;
            };
            prev_px = r.center_px;
            sum_score += r.score;
            scored += 1;
            trail.push((
                pts_to_clip_t(pts),
                vance_tracker::px_to_uv(r.center_px, frame.width, frame.height),
            ));
        }

        self.return_pooled_decoder(dec);

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
            let project = self
                .current_project
                .as_ref()
                .ok_or(EngineError::NoProject)?;
            let seq_id = self
                .current_sequence_id
                .ok_or(EngineError::NoCurrentSequence)?;
            let seq = project
                .sequence(seq_id)
                .ok_or(EngineError::SequenceNotFound)?;
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
        params.insert(
            "translate_x".into(),
            EffectValue::Keyframes { keyframes: tx_kfs },
        );
        params.insert(
            "translate_y".into(),
            EffectValue::Keyframes { keyframes: ty_kfs },
        );
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
            mean_score: if scored > 0 {
                sum_score / scored as f32
            } else {
                1.0
            },
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
        let project = self
            .current_project
            .as_ref()
            .ok_or(EngineError::NoProject)?;
        let seq_id = self
            .current_sequence_id
            .ok_or(EngineError::NoCurrentSequence)?;
        let seq = project
            .sequence(seq_id)
            .ok_or(EngineError::SequenceNotFound)?;
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
                gc::WheelGroup::Lift => (
                    [gk::KEY_LIFT_R, gk::KEY_LIFT_G, gk::KEY_LIFT_B],
                    Some(gk::KEY_LIFT_Y),
                ),
                gc::WheelGroup::Gamma => (
                    [gk::KEY_GAMMA_R, gk::KEY_GAMMA_G, gk::KEY_GAMMA_B],
                    Some(gk::KEY_GAMMA_Y),
                ),
                gc::WheelGroup::Gain => (
                    [gk::KEY_GAIN_R, gk::KEY_GAIN_G, gk::KEY_GAIN_B],
                    Some(gk::KEY_GAIN_Y),
                ),
                gc::WheelGroup::Offset => {
                    ([gk::KEY_OFFSET_R, gk::KEY_OFFSET_G, gk::KEY_OFFSET_B], None)
                }
                gc::WheelGroup::Shadow => {
                    ([gk::KEY_SHADOW_R, gk::KEY_SHADOW_G, gk::KEY_SHADOW_B], None)
                }
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
                gc::GradingCommand::AddGradeNode {
                    track_id,
                    clip_id,
                    mode,
                    label: lbl,
                } => {
                    let mut params = std::collections::BTreeMap::new();
                    params.insert(
                        gk::KEY_MODE.to_string(),
                        EffectValue::String(mode_str(*mode).to_string()),
                    );
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
                gc::GradingCommand::RemoveGradeNode {
                    track_id,
                    clip_id,
                    effect_id,
                } => {
                    pending.push(EditOp::RemoveEffectFromClip {
                        track_id: parse_track(track_id)?,
                        clip_id: parse_clip(clip_id)?,
                        effect_id: parse_effect(effect_id)?,
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::ReorderGradeNode {
                    track_id,
                    clip_id,
                    effect_id,
                    new_index,
                } => {
                    pending.push(EditOp::MoveEffectOnClip {
                        track_id: parse_track(track_id)?,
                        clip_id: parse_clip(clip_id)?,
                        effect_id: parse_effect(effect_id)?,
                        new_index: *new_index as usize,
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetGradeNodeEnabled {
                    track_id,
                    clip_id,
                    effect_id,
                    enabled,
                } => {
                    pending.push(EditOp::SetEffectEnabled {
                        track_id: parse_track(track_id)?,
                        clip_id: parse_clip(clip_id)?,
                        effect_id: parse_effect(effect_id)?,
                        enabled: *enabled,
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetGradeNodeLabel {
                    track_id,
                    clip_id,
                    effect_id,
                    label: lbl,
                } => {
                    pending.push(EditOp::UpdateEffectParams {
                        track_id: parse_track(track_id)?,
                        clip_id: parse_clip(clip_id)?,
                        effect_id: parse_effect(effect_id)?,
                        params: vec![(gk::KEY_LABEL.to_string(), EffectValue::String(lbl.clone()))],
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetNodeMode {
                    track_id,
                    clip_id,
                    effect_id,
                    mode,
                } => {
                    pending.push(EditOp::UpdateEffectParams {
                        track_id: parse_track(track_id)?,
                        clip_id: parse_clip(clip_id)?,
                        effect_id: parse_effect(effect_id)?,
                        params: vec![(
                            gk::KEY_MODE.to_string(),
                            EffectValue::String(mode_str(*mode).to_string()),
                        )],
                    });
                    outcomes.push(gc::GradingCommandOutcome::None);
                }
                gc::GradingCommand::SetWheel {
                    track_id,
                    clip_id,
                    effect_id,
                    group,
                    rgb,
                    master,
                    reset,
                } => {
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
                gc::GradingCommand::SetToneCurve {
                    track_id,
                    clip_id,
                    effect_id,
                    channel,
                    points,
                } => {
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
                gc::GradingCommand::SetHslCurve {
                    track_id,
                    clip_id,
                    effect_id,
                    curve,
                    points,
                } => {
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
                gc::GradingCommand::SetQualifier {
                    track_id,
                    clip_id,
                    effect_id,
                    enabled,
                    invert,
                    hue,
                    sat,
                    luma,
                } => {
                    let mut params: Vec<(String, EffectValue)> = Vec::new();
                    let mut push_bool = |k: &str, b: Option<bool>| {
                        if let Some(b) = b {
                            params.push((
                                k.to_string(),
                                EffectValue::Number(if b { 1.0 } else { 0.0 }),
                            ));
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
                gc::GradingCommand::SetWindow {
                    track_id,
                    clip_id,
                    effect_id,
                    shape,
                    center,
                    size,
                    rotation,
                    feather,
                    invert,
                } => {
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
                        params.push((
                            gk::KEY_WIN_CENTER_X.to_string(),
                            EffectValue::Number(c[0] as f64),
                        ));
                        params.push((
                            gk::KEY_WIN_CENTER_Y.to_string(),
                            EffectValue::Number(c[1] as f64),
                        ));
                    }
                    if let Some(s) = size {
                        params.push((
                            gk::KEY_WIN_SIZE_X.to_string(),
                            EffectValue::Number(s[0] as f64),
                        ));
                        params.push((
                            gk::KEY_WIN_SIZE_Y.to_string(),
                            EffectValue::Number(s[1] as f64),
                        ));
                    }
                    if let Some(r) = rotation {
                        params.push((
                            gk::KEY_WIN_ROTATION.to_string(),
                            EffectValue::Number(*r as f64),
                        ));
                    }
                    if let Some(f) = feather {
                        params.push((
                            gk::KEY_WIN_FEATHER.to_string(),
                            EffectValue::Number(*f as f64),
                        ));
                    }
                    if let Some(i) = invert {
                        params.push((
                            gk::KEY_WIN_INVERT.to_string(),
                            EffectValue::Number(if *i { 1.0 } else { 0.0 }),
                        ));
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
                gc::GradingCommand::TrackWindow {
                    track_id,
                    clip_id,
                    effect_id,
                    start_source_seconds,
                    n_frames,
                    patch_half,
                    search_radius,
                } => {
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
                gc::GradingCommand::SetOcioViewTransform {
                    config,
                    src,
                    dst,
                    size,
                } => {
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
    pub fn set_track_mute(&mut self, track_id: TrackId, mute: bool) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetTrackMute { track_id, mute })
    }

    /// Phase 7.3: set a track's solo flag.
    pub fn set_track_solo(&mut self, track_id: TrackId, solo: bool) -> Result<(), EngineError> {
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
        self.apply_edit_op(EditOp::SetTrackGainKeyframes {
            track_id,
            keyframes,
        })
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

    /// Live, NON-undoable preview of grade params mid-drag: recomposes the
    /// paused frame with `params` applied to `effect_id` on `clip_id`. The
    /// frames come from the frame cache, so there is no decode; the
    /// pointer-up commit goes through the edit log.
    ///
    /// Grading is a paused workflow: while anything plays this is a no-op
    /// (playback renders the committed grade).
    pub fn preview_grade_params(
        &mut self,
        clip_id: vance_timeline::ClipId,
        effect_id: EffectId,
        params: Vec<(String, EffectValue)>,
    ) -> Result<(), EngineError> {
        let Ok(seq) = self.current_sequence_ref() else {
            return Ok(());
        };
        let mut seq = seq.clone();
        let mut found = false;
        for track in seq.tracks.iter_mut() {
            let Some(i) = track.clips.iter().position(|c| c.id == clip_id) else {
                continue;
            };
            let clip = Arc::make_mut(&mut Arc::make_mut(track).clips[i]);
            if let Some(e) = clip.effects.iter_mut().find(|e| e.id == effect_id) {
                e.params.extend(params);
                found = true;
            }
            break;
        }
        if !found {
            return Ok(());
        }
        self.repaint_preview(Some(seq))
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
        self.apply_clip_insert(
            EditOp::InsertClip { track_id, clip },
            "Insert clip",
            track_id,
            media_id,
        )?;
        Ok(new_clip_id.to_string())
    }

    // ---- GOAT-6: magnetic timeline + connections ----------------------------

    /// Whether the current sequence is in magnetic-timeline mode.
    pub fn is_magnetic(&self) -> bool {
        self.current_sequence_ref()
            .map(|s| s.magnetic)
            .unwrap_or(false)
    }

    /// Toggle magnetic-timeline mode on the current sequence.
    pub fn set_magnetic(&mut self, magnetic: bool) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::SetMagnetic { magnetic })
    }

    // ---- M1 compositing: tracks, blend modes, canvas (ADR 0007) -------------

    /// Add an empty track of `kind` at `position` within its kind's group
    /// (`None` = after the last one). Without a `name` the track gets its
    /// automatic positional name ("V3", "A2"). Returns the new id.
    pub fn add_track(
        &mut self,
        kind: TrackKind,
        name: Option<String>,
        position: Option<usize>,
    ) -> Result<TrackId, EngineError> {
        let group_len = self
            .current_sequence_ref()?
            .tracks
            .iter()
            .filter(|t| t.kind == kind)
            .count();
        let track_id = TrackId::new();
        self.pause_for_edit();
        self.apply_edit_op(EditOp::InsertTrack {
            track_id,
            kind,
            name: name.unwrap_or_default(),
            position: position.unwrap_or(group_len),
        })?;
        Ok(track_id)
    }

    /// Rename a track. The name is trimmed; an empty one (or one in the
    /// automatic "V<n>" / "A<n>" form) reverts to the positional name.
    pub fn rename_track(&mut self, id: TrackId, name: String) -> Result<(), EngineError> {
        self.apply_edit_op(EditOp::RenameTrack { track_id: id, name })
    }

    /// Remove a track (and its clips).
    pub fn remove_track(&mut self, id: TrackId) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::RemoveTrack { track_id: id })
    }

    /// Move a track to `to_position` within its kind's group (0 = bottom
    /// video layer).
    pub fn move_track(&mut self, id: TrackId, to_position: usize) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::MoveTrack {
            track_id: id,
            to_position,
        })
    }

    /// Show or hide a track (`Track.enabled`).
    pub fn set_track_enabled(&mut self, id: TrackId, enabled: bool) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::SetTrackEnabled {
            track_id: id,
            enabled,
        })
    }

    /// Lock or unlock a track (a locked track refuses clip edits and
    /// can't be deleted).
    pub fn set_track_locked(&mut self, id: TrackId, locked: bool) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::SetTrackLocked {
            track_id: id,
            locked,
        })
    }

    /// Set a clip's compositing blend mode.
    pub fn set_clip_blend_mode(
        &mut self,
        t: TrackId,
        c: vance_timeline::ClipId,
        m: vance_timeline::BlendMode,
    ) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::SetClipBlendMode {
            track_id: t,
            clip_id: c,
            blend_mode: m,
        })
    }

    /// Set (or clear) the current sequence's compositing canvas size.
    pub fn set_frame_size(&mut self, size: Option<(u32, u32)>) -> Result<(), EngineError> {
        self.pause_for_edit();
        self.apply_edit_op(EditOp::SetFrameSize { size })
    }

    /// Solo a clip's keyer matte in the viewer (`None` clears). Transient
    /// viewer state like `set_matte_node`: preview only, never exported,
    /// repainted at once.
    pub fn set_matte_view(
        &mut self,
        clip: Option<vance_timeline::ClipId>,
    ) -> Result<(), EngineError> {
        let before = self.viewer.matte_view;
        self.viewer.matte_view = clip;
        let repaint = self.repaint_preview(None);
        if repaint.is_err() {
            self.viewer.matte_view = before;
        }
        repaint
    }

    /// Composite the current sequence at timeline `t` seconds through the
    /// scrub (preview) path on a `w`×`h` canvas and read back RGBA8.
    // Intentional: lib API for the integration tests, which the bin target
    // never calls.
    #[allow(dead_code)]
    pub fn preview_frame_rgba(&mut self, t: f64, w: u32, h: u32) -> Result<Vec<u8>, EngineError> {
        self.pause_for_edit();
        let rs = self.ensure_render_state()?;
        self.render_preview(
            &rs,
            t.max(0.0),
            None,
            |out| {
                out.width = w;
                out.height = h;
            },
            PreviewSink::Rgba8 {
                wait: true,
                hold: false,
            },
        )?
        .ok_or_else(|| EngineError::Project("preview produced no frame".into()))
    }

    /// Composite the current sequence at timeline `t` seconds through the
    /// export per-frame function on a `w`×`h` canvas and read back RGBA8.
    // Intentional: lib API for the integration tests, which the bin target
    // never calls.
    #[allow(dead_code)]
    pub fn export_frame_rgba(&mut self, t: f64, w: u32, h: u32) -> Result<Vec<u8>, EngineError> {
        self.pause_for_edit();
        let rs = self.ensure_render_state()?;
        let mut provider = self.take_export_stream();
        let frame =
            self.export_frame_with(&rs, &mut provider, t.max(0.0), (w, h), false, None, false);
        // Keep the decoders for the next read, never the feeds (see
        // `render_sequence`).
        self.export_pool = Some(provider.into_pool());
        frame
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
        self.apply_clip_insert(
            EditOp::AppendClip { track_id, clip },
            "Append clip",
            track_id,
            media_id,
        )?;
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
        self.apply_clip_insert(
            EditOp::MagneticRippleInsert { track_id, clip },
            "Ripple insert (all tracks)",
            track_id,
            media_id,
        )?;
        Ok(id.to_string())
    }

    /// Build a `Clip` spanning a media item's full visible range, placed at
    /// `timeline_in`. Shared by append / magnetic insert.
    fn clip_from_media(
        &self,
        media_id: MediaItemId,
        timeline_in: vance_core::RationalTime,
    ) -> Result<Clip, EngineError> {
        let project = self
            .current_project
            .as_ref()
            .ok_or(EngineError::NoProject)?;
        let media = project
            .media
            .get(&media_id)
            .ok_or(EngineError::MediaNotFound)?;
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
        let seq_id = self
            .current_sequence_id
            .ok_or(EngineError::NoCurrentSequence)?;
        project
            .sequence(seq_id)
            .map(|arc| arc.as_ref())
            .ok_or(EngineError::SequenceNotFound)
    }

    /// Apply `op` to the current sequence's edit log, then mirror the
    /// log's new `current()` Sequence back into `project.sequences`
    /// so reads (rendering, save, scene_plan) see the latest state.
    fn apply_edit_op(&mut self, op: EditOp) -> Result<(), EngineError> {
        let seq_id = self
            .current_sequence_id
            .ok_or(EngineError::NoCurrentSequence)?;
        let touches_mixer = op_touches_mixer(&op);
        let changes_picture = op_changes_picture(&op);
        let log = self
            .edit_logs
            .get_mut(&seq_id)
            .ok_or(EngineError::NoCurrentSequence)?;
        log.apply(op).map_err(EngineError::from_edit)?;
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
        if changes_picture {
            self.repaint_after_edit();
        }
        Ok(())
    }

    /// Show the edited sequence in the paused Record viewer (a no-op while
    /// anything plays or before the sequence was shown), at most once per
    /// frame interval: a slider sends an edit per input event, and each
    /// repaint is a compose plus a vsync-bound present. One inside the
    /// interval is left pending for [`Self::flush_pending_repaint`].
    fn repaint_after_edit(&mut self) {
        let interval = self.repaint_interval();
        if self
            .preview_painted_at
            .is_some_and(|t| t.elapsed() < interval)
        {
            self.repaint_pending = true;
            return;
        }
        self.repaint_now();
    }

    /// The pending repaint, once its interval has passed. Called from the
    /// UI's 250 ms poll (`is_playing`), so the last edit always shows.
    fn flush_pending_repaint(&mut self) {
        if self.repaint_pending
            && self
                .preview_painted_at
                .is_none_or(|t| t.elapsed() >= self.repaint_interval())
        {
            self.repaint_now();
        }
    }

    fn repaint_now(&mut self) {
        self.repaint_pending = false;
        // Intentional: logged, not returned. The edit itself succeeded and is
        // in the undo log; failing the command would make the UI disagree.
        if let Err(e) = self.repaint_preview(None) {
            tracing::warn!(error = %e, "preview not repainted after an edit");
        }
    }

    /// One frame of the current sequence, at most 1/24 s (so a very low or
    /// malformed rate can't hold back the last edit).
    fn repaint_interval(&self) -> Duration {
        const LONGEST: f64 = 1.0 / 24.0;
        let frame = self
            .current_sequence_fps()
            .filter(|f| f.is_finite() && *f > 0.0)
            .map_or(LONGEST, |fps| 1.0 / fps);
        Duration::from_secs_f64(frame.min(LONGEST))
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
        let seq_id = self
            .current_sequence_id
            .ok_or(EngineError::NoCurrentSequence)?;
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
        self.repaint_after_edit();
        Ok(true)
    }

    /// Redo the next edit on the current sequence. Returns `false`
    /// if there's nothing to redo.
    pub fn redo(&mut self) -> Result<bool, EngineError> {
        self.pause_for_edit();
        let seq_id = self
            .current_sequence_id
            .ok_or(EngineError::NoCurrentSequence)?;
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
        self.repaint_after_edit();
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
        let seq_id = self
            .current_sequence_id
            .ok_or(EngineError::NoCurrentSequence)?;
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
        let seq_id = self
            .current_sequence_id
            .ok_or(EngineError::NoCurrentSequence)?;
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
        self.repaint_after_edit();
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
            None => self.current_project_path.clone().ok_or_else(|| {
                EngineError::Project(
                    "project has no save path; call save_project with an explicit path first"
                        .into(),
                )
            })?,
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
                            blend_mode: c.blend_mode.as_str().to_string(),
                        })
                        .collect(),
                    // As rendered: an over-long dissolve is capped, and an
                    // orphan (no two clips meet at its cut) draws nothing.
                    transitions: t
                        .transitions
                        .iter()
                        .filter_map(|tr| {
                            let (win_in, win_out) = vance_timeline::transition_window(t, tr)?;
                            let (win_in, win_out) = (rt_to_seconds(win_in), rt_to_seconds(win_out));
                            Some((tr, (win_in + win_out) / 2.0, win_out - win_in))
                        })
                        .map(|(tr, centre, length)| TransitionInfo {
                            id: tr.id.to_string(),
                            kind: match &tr.kind {
                                vance_timeline::TransitionKind::Dissolve => "dissolve".into(),
                                // WASM transitions surface as "wasm:<id>"
                                // (same convention as wasm effects).
                                vance_timeline::TransitionKind::Wasm(id) => format!("wasm:{id}"),
                            },
                            position_seconds: centre,
                            duration_seconds: length,
                            params: tr.params.clone(),
                        })
                        .collect(),
                    mute: t.mute,
                    solo: t.solo,
                    gain_db: t.gain_db,
                    gain_keyframes: t.gain_keyframes.clone(),
                    pan: t.pan,
                    audio_effects: t.audio_effects.clone(),
                    output: t.output,
                    sends: t.sends.clone(),
                })
                .collect(),
            master_gain_db: seq.master_gain_db,
            frame_size: seq.frame_size.map(|(w, h)| [w, h]),
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
        // The solos name a clip / grade node of the sequence on screen; kept,
        // they'd come back unasked (and with no control) on returning to it.
        if self.current_sequence_id != Some(id) {
            self.viewer = ViewerState::default();
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
        if self.playback.is_some() || self.seq_playback.is_some() {
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

    /// Is playback running? Reaps a session whose thread has exited (the
    /// end of the sequence or file) so the engine state matches reality
    /// and its decoders come back for single-shot use. There is no clip
    /// chaining: sequence playback runs straight through cuts and gaps.
    pub fn is_playing(&mut self) -> bool {
        self.poll_auto_proxies();
        self.repaint_when_landed();
        self.flush_pending_repaint();
        if let Some(session) = self.seq_playback.as_ref() {
            // One take: a broken proxy never splits from its original's request.
            let comp_playback::ProxyReports {
                requests,
                broken_proxies: broken,
            } = session.take_proxy_reports();
            let finished = session.is_finished();
            self.handle_proxy_requests(requests, &broken);
            if !finished {
                return true;
            }
            let _ = self.stop_playback();
            return false;
        }
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

    let to_remove: Vec<uuid::Uuid> = live
        .iter()
        .filter(|id| !wanted.contains(id))
        .copied()
        .collect();

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
            match vance_audio::spawn_source(&sidecar_binary, &source, per_source_capacity, &stats) {
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
fn translate_audio_effect(spec: &vance_timeline::AudioEffectSpec) -> vance_audio::AudioEffectKind {
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
                    vance_timeline::EqBandKind::LowShelf => vance_audio::dsp::BiquadShape::LowShelf,
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
    let sched_clips = Arc::new(arc_swap::ArcSwap::from_pointee(initial_sched_clips));

    // Scheduler ↔ fold thread channel. Unbounded (mpsc::channel) because
    // scheduler ticks emit at most ~MAX_CONCURRENT_SOURCES commands per
    // tick; the fold thread drains in a tight try_recv loop.
    let (command_tx, command_rx) = std::sync::mpsc::channel::<vance_audio::SourceCommand>();
    let stats = Arc::new(vance_audio::MixerStats::default());

    // Probe the cpal device's sample rate up-front so we can size the
    // per-source rings to match what `AudioMixer::start` will see.
    let probed_sample_rate =
        vance_audio::probe_default_sample_rate().map_err(|e| format!("audio probe: {e}"))?;
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
                .send(Ok((
                    sync,
                    snapshot_handle,
                    master_frame,
                    sample_rate,
                    meter_handle,
                )))
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
/// Whether `op` can change the composed picture (and so the paused viewer
/// needs a repaint). Exhaustive on purpose: a new op has to be sorted here.
fn op_changes_picture(op: &EditOp) -> bool {
    match op {
        EditOp::Compound { ops, .. } => ops.iter().any(op_changes_picture),
        EditOp::RemoveTrack { .. }
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
        | EditOp::AddEffectToClip { .. }
        | EditOp::RemoveEffectFromClip { .. }
        | EditOp::UpdateEffectParam { .. }
        | EditOp::UpdateEffectParams { .. }
        | EditOp::AddTransitionAtCut { .. }
        | EditOp::RemoveTransition { .. }
        | EditOp::SetTransitionParam { .. }
        | EditOp::MoveEffectOnClip { .. }
        | EditOp::SetEffectEnabled { .. }
        | EditOp::MagneticMoveClip { .. }
        | EditOp::AppendClip { .. }
        | EditOp::MagneticRippleDelete { .. }
        | EditOp::MagneticRippleInsert { .. }
        | EditOp::MoveTrack { .. }
        | EditOp::SetTrackEnabled { .. }
        | EditOp::SetClipBlendMode { .. }
        | EditOp::SetFrameSize { .. }
        // Muting a clip disables it (`clip.enabled = !mute`): the scene plan
        // leaves it out.
        | EditOp::SetClipMute { .. } => true,
        // Empty tracks, names, locks, magnetism and connections, the audio
        // mix and routing, captions (drawn by the UI, not composed) and
        // review notes leave the picture as it is.
        EditOp::AddTrack { .. }
        | EditOp::InsertTrack { .. }
        | EditOp::RenameTrack { .. }
        | EditOp::SetTrackLocked { .. }
        | EditOp::SetMagnetic { .. }
        | EditOp::ConnectClip { .. }
        | EditOp::DisconnectClip { .. }
        | EditOp::SetTrackMute { .. }
        | EditOp::SetTrackSolo { .. }
        | EditOp::SetTrackGainDb { .. }
        | EditOp::SetClipGainDb { .. }
        | EditOp::SetMasterGainDb { .. }
        | EditOp::SetTrackPan { .. }
        | EditOp::SetClipPan { .. }
        | EditOp::AddAudioEffect { .. }
        | EditOp::RemoveAudioEffect { .. }
        | EditOp::MoveAudioEffect { .. }
        | EditOp::UpdateAudioEffect { .. }
        | EditOp::SetTrackGainKeyframes { .. }
        | EditOp::SetClipGainKeyframes { .. }
        | EditOp::SetClipFadeIn { .. }
        | EditOp::SetClipFadeOut { .. }
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
        | EditOp::RemoveBusSend { .. }
        | EditOp::AddCaption { .. }
        | EditOp::RemoveCaption { .. }
        | EditOp::SetCaptionText { .. }
        | EditOp::SetCaptionTiming { .. }
        | EditOp::SplitCaption { .. }
        | EditOp::MergeCaptionWithNext { .. }
        | EditOp::ShiftAllCaptions { .. }
        | EditOp::SetCaptions { .. }
        | EditOp::AddReviewComment { .. }
        | EditOp::RemoveReviewComment { .. }
        | EditOp::ResolveReviewComment { .. }
        | EditOp::SetReviewCommentText { .. }
        | EditOp::ReplyReviewComment { .. } => false,
    }
}

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
        | EditOp::InsertTrack { .. }
        | EditOp::MoveTrack { .. }
        | EditOp::SetTrackEnabled { .. }
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
// Intentional: engine.rs is compiled into both the lib and the bin
// target; this is only called from bin-only modules, so the lib build
// alone sees it as unused.
#[allow(dead_code)]
pub(crate) fn measure_loudness(media_path: &std::path::Path) -> Option<f32> {
    measure_loudness_range(media_path, 0.0, None)
}

/// GOAT-8 — measure integrated loudness (LUFS) over `[start_sec, start_sec +
/// dur_sec)` of a media file's audio (or the whole file when `dur_sec`
/// is `None`) via the BS.1770 meter. Slow (decodes) — run OFF the engine
/// lock. Measuring only the clip's active source range makes dialogue
/// leveling correct for trimmed clips.
// Intentional: engine.rs is compiled into both the lib and the bin
// target; this is only called from bin-only modules, so the lib build
// alone sees it as unused.
#[allow(dead_code)]
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
    'decode: while let Ok(Some(chunk)) = audio.next_chunk() {
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

/// The surface's next texture. An outdated or lost surface clears `config`
/// so the next present reconfigures it.
pub(crate) fn acquire_surface_texture(
    surface: &wgpu::Surface<'_>,
    config: &mut Option<wgpu::SurfaceConfiguration>,
) -> Result<wgpu::SurfaceTexture, EngineError> {
    match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => {
            Ok(t)
        }
        wgpu::CurrentSurfaceTexture::Outdated
        | wgpu::CurrentSurfaceTexture::Lost
        | wgpu::CurrentSurfaceTexture::Validation => {
            *config = None;
            Err(EngineError::SurfaceAcquire("surface outdated".into()))
        }
        wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => Err(
            EngineError::SurfaceAcquire("surface timeout/occluded".into()),
        ),
    }
}

/// What the single-file playback thread shares with the engine.
struct FileLoop {
    bounds: ViewportBounds,
    viewport: Arc<Viewport>,
    render_state: Arc<Mutex<RenderState>>,
    stop: Arc<AtomicBool>,
    audio: Option<AudioSync>,
    last_pts: Arc<std::sync::atomic::AtomicI64>,
    ended_at_eos: Arc<AtomicBool>,
}

/// Single-file playback: pulls the file's frames in PTS order and presents
/// each at its time on the **audio clock** (when present) or the wall
/// clock (audio missing/failed). Returns the decoder so the engine can
/// reuse the open file.
fn file_playback_loop(mut client: DecoderClient, info: StreamInfo, s: FileLoop) -> DecoderClient {
    let (tb_num, tb_den) = info.time_base;
    let pts_to_sec = |pts: i64| -> f64 {
        if tb_den == 0 {
            0.0
        } else {
            (pts as f64) * (tb_num as f64) / (tb_den as f64)
        }
    };
    tracing::info!(
        master = if s.audio.is_some() {
            "audio"
        } else {
            "wall-clock"
        },
        "playback thread started"
    );
    // Wall-clock fallback anchor (only used when no audio): the first
    // frame's arrival, mapped to its PTS.
    let mut wall_anchor: Option<(Instant, f64)> = None;
    let mut bounds = s.bounds;
    let mut last_logged_drops = 0u64;
    let mut total_drops = 0u64;
    let mut frames_presented = 0u64;
    loop {
        if s.stop.load(Ordering::Acquire) {
            break;
        }
        let frame = match client.next_frame() {
            Ok(Some(f)) => f,
            Ok(None) => {
                tracing::info!("playback: end of stream");
                s.ended_at_eos.store(true, Ordering::Release);
                break;
            }
            Err(e) => {
                tracing::error!(?e, "playback: decoder error");
                break;
            }
        };
        let frame_sec = pts_to_sec(frame.pts);

        // How late/early this frame is relative to the master clock.
        // Positive => frame is in the future, sleep that long.
        // Negative below threshold => frame is too late, drop.
        let wait_secs = if let Some(audio) = s.audio.as_ref() {
            let audio_pts = audio.current_pts.load(Ordering::Acquire);
            let (atb_n, atb_d) = audio.time_base;
            let audio_sec = if atb_d != 0 {
                (audio_pts as f64) * (atb_n as f64) / (atb_d as f64)
            } else {
                0.0
            };
            // Both clocks are in absolute file seconds (audio is seeked to
            // from_seconds at start_stream), so the comparison is direct.
            frame_sec - audio_sec
        } else {
            let (origin, origin_sec) = *wall_anchor.get_or_insert((Instant::now(), frame_sec));
            (frame_sec - origin_sec) - origin.elapsed().as_secs_f64()
        };

        if wait_secs > 0.0 {
            // Future frame; sleep but in chunks so stop is responsive.
            let mut remaining = Duration::from_secs_f64(wait_secs);
            while remaining > Duration::ZERO {
                if s.stop.load(Ordering::Acquire) {
                    break;
                }
                let chunk = remaining.min(Duration::from_millis(50));
                std::thread::sleep(chunk);
                remaining = remaining.saturating_sub(chunk);
            }
        } else if wait_secs < -0.080 && s.audio.is_some() {
            // Intentional: > ~2 frames late at 24 fps drops to keep the
            // picture honest with the audio clock. With the wall clock there
            // is no external clock to sync to; dropping there would empty
            // the view whenever decode is slower than real time.
            total_drops += 1;
            if total_drops - last_logged_drops >= 30 {
                tracing::warn!(total_drops, "playback: dropped frames so far");
                last_logged_drops = total_drops;
            }
            continue;
        }

        if let Err(e) = upload_and_render(&s.render_state, &s.viewport, &frame, bounds) {
            tracing::warn!(?e, "playback: render error (continuing)");
        }
        s.last_pts.store(frame.pts, Ordering::Release);
        frames_presented += 1;
        if frames_presented == 1 || frames_presented.is_multiple_of(30) {
            tracing::info!(
                frames_presented,
                pts = frame.pts,
                "playback: frame presented"
            );
        }
        // Refresh bounds from the viewport in case the chrome resized.
        bounds = s.viewport.current_bounds();
    }
    tracing::info!(frames_presented, total_drops, "playback: thread exiting");
    let _ = client.stop_stream();
    client
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
    let surface_tex = acquire_surface_texture(surface, &mut rs.source_surface_config)?;
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
    let surface_tex = acquire_surface_texture(surface, &mut rs.surface_config)?;
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
    /// Output resolution. Defaults to the sequence's frame size (else
    /// 1920×1080) when `None`.
    pub width: Option<u32>,
    /// Output resolution.
    pub height: Option<u32>,
    /// Render-and-replace: render only this track. `None` = full export.
    pub render_and_replace_track: Option<vance_timeline::TrackId>,
    /// Start time in seconds. `None` = sequence start.
    pub start_seconds: Option<f64>,
    /// End time in seconds. `None` = sequence end.
    pub end_seconds: Option<f64>,
    /// HDR10 export. When `true`, composite in linear light with Rec.2020-PQ
    /// output, read back rgba64le and encode HEVC Main10 tagged with HDR10
    /// mastering metadata. `#[serde]`-less (an engine input struct);
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
    /// The handle that cancels a running export (see [`ExportCanceller`]);
    /// take it once, before exports start, and keep it outside the engine
    /// lock.
    pub fn export_canceller(&self) -> ExportCanceller {
        self.export_cancel.clone()
    }

    /// Phase 8 — render the current sequence to a file via the
    /// sandboxed `vance-encoder` sidecar. Every frame is composited by
    /// the export per-frame function (`compose_frame` over Blocking feeds
    /// of the full-resolution originals, ADR 0007); HDR composites in
    /// linear light and reads back rgba64le. Audio is the offline mixdown
    /// of the same range.
    pub fn render_sequence(&mut self, opts: RenderOptions) -> Result<RenderOutcome, EngineError> {
        let started = Instant::now();
        // A cancel counts from here (the first steps can take seconds), and
        // the canceller is disarmed when this returns, whichever way.
        let cancel = self.export_cancel.clone();
        let _armed = cancel.arm();

        // 1. Playback owns the preview decoders and the mixer; stop it.
        self.pause_for_edit();

        // 2. Snapshot sequence shape (frame-rate, duration, canvas).
        let (frame_rate_num, frame_rate_den, seq_duration, frame_size) = {
            let seq = self.current_sequence_ref()?;
            let fr_r = seq.frame_rate.0;
            (
                fr_r.numer() as u32,
                fr_r.denom() as u32,
                seq.duration().0.as_f64(),
                seq.frame_size,
            )
        };
        // Export defaults to the sequence canvas (follow-up decision 4).
        let (default_w, default_h) = frame_size.unwrap_or((1920, 1080));
        // Round output dimensions DOWN to even: 4:2:0 codecs (H.264 / HEVC /
        // AV1) require even width AND height for their subsampled chroma
        // planes. Clamp to a minimum of 2 so a tiny/odd request still yields
        // a valid frame.
        let width = (opts.width.unwrap_or(default_w) & !1).max(2);
        let height = (opts.height.unwrap_or(default_h) & !1).max(2);
        let start_s = opts.start_seconds.unwrap_or(0.0).max(0.0);
        let end_s = opts.end_seconds.unwrap_or(seq_duration).min(seq_duration);
        if end_s <= start_s {
            return Err(EngineError::Project(format!(
                "render range empty: start={start_s} end={end_s}"
            )));
        }
        let total_frames =
            ((end_s - start_s) * frame_rate_num as f64 / frame_rate_den as f64).ceil() as u64;
        let sample_rate: u32 = 48_000;

        // 3. The preview renderer (or a headless one) also renders exports:
        //    HDR is an output spec of the same compositor.
        let rs = self.ensure_render_state()?;
        let (video_pixel_format, hdr_meta) = if opts.hdr {
            // HDR10 mastering metadata (Rec.2020 PQ, 1000-nit mastering display).
            (
                vance_encoder::VideoPixelFormat::Rgba64Le,
                Some(vance_encoder::HdrMetadata::hdr10(1000.0, 0.0001, 1000, 400)),
            )
        } else {
            (vance_encoder::VideoPixelFormat::Rgba8, None)
        };

        // 4. Spawn encoder + open the output.
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

        // 5. Video: one composited frame per output frame. One track alone
        //    into an alpha-capable codec keeps its transparency, so the
        //    render can go back over the tracks below (render-and-replace).
        let job = ExportJob {
            start_s,
            total_frames,
            frame_rate_num,
            frame_rate_den,
            size: (width, height),
            hdr: opts.hdr,
            track_filter: opts.render_and_replace_track,
            transparent: opts.render_and_replace_track.is_some()
                && !opts.hdr
                && preset_keeps_alpha(&opts.preset),
        };
        let mut provider = self.take_export_stream();
        cancel.watch(provider.pool_mut().sidecars());
        let video = self.encode_frames(&mut client, &rs, &mut provider, &job, &cancel);
        // Every export ends with its feeds and decoders (success or not): a
        // feed that failed to start would otherwise stay "started" and drop
        // its layer from the next export without an error.
        drop(provider.into_pool());
        if let Err(e) = video {
            // Without `finish` the encoder deletes its partial file: nothing
            // reaches the destination (a file already there is untouched).
            let _ = client.shutdown();
            return Err(e);
        }

        // 6. Real mixed audio — offline bounce of the export range
        //    through the full mixer model (per-clip gain/fades/pan,
        //    per-track DSP, master), then push to the encoder in
        //    chunks. Falls back to silence only if the bounce fails
        //    (e.g. no audio sidecar), so the file is always valid.
        if cancel.is_cancelled() {
            let _ = client.shutdown();
            return Err(EngineError::Cancelled);
        }
        let mixed = match self.offline_audio_bounce(start_s, end_s, sample_rate) {
            Ok(samples) => samples,
            Err(e) => {
                tracing::warn!(?e, "audio bounce failed; exporting silent track");
                let total = ((end_s - start_s) * sample_rate as f64).round() as usize;
                vec![0f32; total * 2]
            }
        };
        if cancel.is_cancelled() {
            let _ = client.shutdown();
            return Err(EngineError::Cancelled);
        }
        // Push in ~0.1 s chunks (interleaved stereo).
        let push_chunk_frames = (sample_rate as usize / 10).max(1);
        for chunk in mixed.chunks(push_chunk_frames * 2) {
            if cancel.is_cancelled() {
                let _ = client.shutdown();
                return Err(EngineError::Cancelled);
            }
            if let Err(e) = client.push_audio(0, chunk.to_vec()) {
                let _ = client.shutdown();
                return Err(EngineError::Project(format!("encoder push_audio: {e}")));
            }
        }
        cancel.audio_pushed();
        // The last moment a cancel can stop it: once `finish` runs, the file
        // is written.
        if cancel.is_cancelled() {
            let _ = client.shutdown();
            return Err(EngineError::Cancelled);
        }

        let (video_frames, audio_samples) = client
            .finish()
            .map_err(|e| EngineError::Project(format!("encoder finish: {e}")))?;
        let _ = client.shutdown();
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
    /// the project on save. Always ProRes 4444: the track renders alone
    /// with its transparency (decision 2).
    pub output_path: String,
    /// Render output width. Falls back to the sequence's frame size when
    /// `None`.
    pub width: Option<u32>,
    /// Render output height. Falls back to the sequence's frame size when
    /// `None`.
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
    /// Phase 8.7 — render a time range of `track_id` ALONE, with its
    /// transparency, to a ProRes 4444 file, import it, and overwrite the
    /// range with a single clip referencing it (decision 2). The new clip
    /// takes the blend mode of the clips it replaces and no opacity:
    /// opacity, keyframes and transitions are baked into the alpha, which
    /// is the same picture for every blend mode (opacity only scales the
    /// source alpha). Clips with different blend modes are refused.
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
        // Every refusal before anything is rendered or imported.
        let blend_mode = self.check_render_and_replace(&opts)?;
        // 1. Render the sub-range out to disk via the existing
        //    sandboxed encoder path.
        let render_outcome = self.render_sequence(RenderOptions {
            output_path: opts.output_path.clone(),
            preset: vance_encoder::EncoderPreset::ProRes4444,
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
        // The render holds the compositor's SDR output, which is sRGB-encoded
        // (`compose::output_spec_for`). Tagged so, it decodes back to the same
        // values; the untagged default (Rec.709) would shift every colour.
        self.set_media_input_color_space(media_id, Some("srgb"))?;

        // 3. Build a Clip targeting the full rendered duration, anchored
        //    at the export's start timeline-time.
        let (clip, new_clip_id) = {
            let project = self
                .current_project
                .as_ref()
                .ok_or(EngineError::NoProject)?;
            let media = project
                .media
                .get(&media_id)
                .ok_or(EngineError::MediaNotFound)?
                .clone();
            let timeline_in = seconds_to_rt(opts.start_seconds);
            let mut clip = Clip::new(
                media.id,
                media.display_name.clone(),
                timeline_in,
                vance_core::RationalTime::ZERO,
                media.duration,
            )
            .map_err(|e| EngineError::Project(e.to_string()))?;
            clip.blend_mode = blend_mode;
            let id = clip.id;
            (clip, id)
        };

        // 4. Overwrite the range on the target track in one undoable
        //    operation. OverwriteClip trims/removes anything the new
        //    clip overlaps, matching the standard NLE render-and-
        //    replace semantics.
        self.apply_clip_insert(
            EditOp::OverwriteClip {
                track_id: opts.track_id,
                clip,
            },
            "Overwrite clip",
            opts.track_id,
            media_id,
        )?;

        Ok(RenderAndReplaceOutcome {
            render: render_outcome,
            new_media_id: media_info.id,
            new_clip_id: new_clip_id.to_string(),
        })
    }
}

impl Engine {
    /// Why render-and-replace of `opts` would leave the sequence different
    /// (or break it), checked before anything is rendered: a locked or
    /// hidden track, HDR media, a range edge inside a transition, a dissolve
    /// under a blend mode that doesn't render exactly, or mixed blend
    /// modes. The blend mode the new clip takes, otherwise.
    fn check_render_and_replace(
        &self,
        opts: &RenderAndReplaceOptions,
    ) -> Result<vance_timeline::BlendMode, EngineError> {
        let refuse = |why: String| Err(EngineError::EditRefused(why));
        let project = self
            .current_project
            .as_ref()
            .ok_or(EngineError::NoProject)?;
        let seq = self.current_sequence_ref()?;
        let track = seq
            .tracks
            .iter()
            .find(|t| t.id == opts.track_id)
            .ok_or_else(|| {
                EngineError::Project("track not found in the current sequence".into())
            })?;
        if track.locked {
            return refuse("The track is locked; unlock it to render and replace it.".into());
        }
        if !track.enabled {
            return refuse(
                "The track is hidden, so the render would be empty; show it to render and \
                 replace it."
                    .into(),
            );
        }
        let (start, end) = (opts.start_seconds, opts.end_seconds);
        let mode = shared_blend_mode(seq, opts.track_id, start, end)?;
        let in_range = |c: &&Arc<Clip>| {
            c.enabled
                && rt_to_seconds(c.timeline_in) < end
                && rt_to_seconds(c.timeline_out()) > start
        };
        if let Some(c) = track
            .clips
            .iter()
            .filter(in_range)
            .find(|c| project.media.get(&c.media).is_some_and(|m| is_hdr_media(m)))
        {
            return refuse(format!(
                "HDR clips can't be rendered and replaced yet (\"{}\" is HDR); leave HDR \
                 clips out of the range.",
                c.name
            ));
        }
        for tr in &track.transitions {
            let Some((win_in, win_out)) = vance_timeline::transition_window(track, tr) else {
                continue;
            };
            // One the scene plan doesn't draw (a side clip disabled) is no
            // dissolve in the picture, so none in the render either.
            if !plan_draws_transition(seq, track.id, tr) {
                continue;
            }
            let (a, b) = (rt_to_seconds(win_in), rt_to_seconds(win_out));
            let inside = |x: f64| a < x && x < b;
            if inside(start) || inside(end) {
                return refuse(format!(
                    "The range starts or ends inside a transition ({a:.2}–{b:.2} s); extend \
                     the range to cover the whole transition."
                ));
            }
            let dissolve_in_range =
                a < end && b > start && matches!(tr.kind, vance_timeline::TransitionKind::Dissolve);
            if dissolve_in_range && !dissolve_renders_exactly(mode) {
                return refuse(format!(
                    "A dissolve under the {} blend mode wouldn't look the same rendered into \
                     one clip; change the blend mode or render the clips separately.",
                    blend_label(mode)
                ));
            }
        }
        Ok(mode)
    }
}

/// Whether the scene plan draws `tr` on `track` (at its cut, inside its
/// window): the plan's own rule for which transitions render.
fn plan_draws_transition(seq: &Sequence, track: TrackId, tr: &vance_timeline::Transition) -> bool {
    vance_timeline::scene_plan_at(seq, tr.position)
        .layers
        .iter()
        .any(|l| {
            l.track_id == track
                && matches!(
                    &l.content,
                    vance_timeline::LayerContent::Transition(lt) if lt.transition_id == tr.id
                )
        })
}

/// A blend mode as the inspector names it.
fn blend_label(mode: vance_timeline::BlendMode) -> String {
    let words = mode.as_str().replace('_', " ");
    let mut c = words.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Decoders a pool keeps alive (ADR 0007: about six, LRU by path).
const POOL_DECODERS: usize = 6;
/// Decoder frame-memory cap where the RAM size can't be read.
const FALLBACK_FRAME_BUDGET: u64 = 3 << 30;

/// A decoder pool whose shared-memory frames are capped at a quarter of
/// RAM (user decision 5).
fn new_decoder_pool() -> DecoderPool {
    // Intentional: without the sidecar the pool still works for synthetic
    // media (titles, MoGraph, Fusion); a file layer then fails to spawn its
    // decoder and reports that.
    let bin = locate_decoder_binary().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "decoder sidecar not found; file layers won't decode");
        PathBuf::new()
    });
    // Intentional: Windows reports no RAM size yet (vance-app has no
    // windows-sys), so it gets a fixed cap.
    let cap_bytes = total_ram_bytes().map_or(FALLBACK_FRAME_BUDGET, |b| b / 4);
    DecoderPool::new(bin, POOL_DECODERS, MemoryBudget { cap_bytes })
}

/// Where a preview composite goes.
enum PreviewSink<'a> {
    /// Present to the Record viewport.
    Viewport(&'a Viewport, ViewportBounds),
    /// Read back RGBA8. With `wait: false` a busy render state gives
    /// `Ok(None)` instead of blocking (the scopes path). With `hold`, a frame
    /// still decoding past [`SCRUB_WAIT`] gives `Ok(None)` too, as a viewer
    /// keeps its picture; without, it's waited out.
    Rgba8 { wait: bool, hold: bool },
}

/// One export's frame walk.
struct ExportJob {
    start_s: f64,
    total_frames: u64,
    frame_rate_num: u32,
    frame_rate_den: u32,
    size: (u32, u32),
    hdr: bool,
    track_filter: Option<TrackId>,
    /// Composite over a transparent backdrop, read back straight alpha.
    transparent: bool,
}

/// How often the engine looks for offline originals coming back (a file
/// check each), while an old-format proxy waits for one.
const ORIGINALS_RECHECK: Duration = Duration::from_secs(2);

/// Delete `proxy` (and its marker) only if it is an auto-proxy the proxy
/// worker made for `media` in `dir`; anything else is logged and left.
/// The path comes from a project bundle, which is untrusted (projects are
/// shared): `Path::starts_with` alone would let `<dir>/../file` through.
fn remove_generated_proxy(dir: &Path, proxy: &Path, media: MediaItemId) {
    for (path, marker) in [(proxy.to_path_buf(), false), (marker_path_of(proxy), true)] {
        match generated_proxy_file(dir, &path, media, marker) {
            Ok(()) => {
                if let Err(e) = std::fs::remove_file(&path) {
                    tracing::warn!(path = %path.display(), error = %e, "old proxy not deleted");
                }
            }
            // A marker that isn't there is nothing to delete.
            Err(_) if marker && std::fs::symlink_metadata(&path).is_err() => {}
            Err(why) => {
                tracing::warn!(path = %path.display(), why, "not an auto-proxy; left in place");
            }
        }
    }
}

fn marker_path_of(proxy: &Path) -> PathBuf {
    let mut name = proxy.as_os_str().to_owned();
    name.push(".vance-proxy");
    PathBuf::from(name)
}

/// Why `path` isn't an auto-proxy (or, with `marker`, its marker) of `media`
/// directly in `dir`, or `Ok` when it is one. A plain absolute path of
/// normal components; a regular file, not a symlink; its folder `dir`
/// itself (both canonicalized); named as the worker names it.
fn generated_proxy_file(
    dir: &Path,
    path: &Path,
    media: MediaItemId,
    marker: bool,
) -> Result<(), &'static str> {
    use std::path::Component;
    if !path.is_absolute() {
        return Err("relative path");
    }
    let mut parts = path.components().peekable();
    while matches!(
        parts.peek(),
        Some(Component::Prefix(_) | Component::RootDir)
    ) {
        parts.next();
    }
    if !parts.all(|c| matches!(c, Component::Normal(_))) {
        return Err("path has `..`, `.` or another non-normal component");
    }
    let meta = std::fs::symlink_metadata(path).map_err(|_| "not there")?;
    if !meta.file_type().is_file() {
        return Err("not a regular file (a symlink or a folder)");
    }
    let parent = path.parent().ok_or("no parent folder")?;
    let same_dir = match (std::fs::canonicalize(parent), std::fs::canonicalize(dir)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if !same_dir {
        return Err("not directly in the proxy folder");
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("no file name")?;
    let name = if marker {
        name.strip_suffix(".vance-proxy")
            .ok_or("not a proxy marker")?
    } else {
        name
    };
    if !is_auto_proxy_name(name, media) {
        return Err("not named as an auto-proxy of this media");
    }
    Ok(())
}

/// The names the worker gives `media`'s auto-proxies: `<id>.proxy.mov`
/// (before format 4) or `<id>.v<format>.proxy.mov`.
fn is_auto_proxy_name(name: &str, media: MediaItemId) -> bool {
    let Some(stem) = name.strip_suffix(".proxy.mov") else {
        return false;
    };
    let id = match stem.rsplit_once(".v") {
        Some((id, version))
            if !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit()) =>
        {
            id
        }
        _ => stem,
    };
    id == media.to_string()
}

/// One auto-proxy job queued.
struct QueuedProxy {
    /// The original it's made from.
    original: String,
    /// The media item's name, for notices.
    name: String,
    need: ProxyNeed,
}

/// How long a size probe on the engine lock (opening a project, inserting a
/// clip) may wait for its decoder before it's killed and the clip counts as
/// not measured yet. Opening a local file takes well under 100 ms and a
/// sleeping disk or network share wakes in a second or two.
const SIZE_PROBE_LIMIT: Duration = Duration::from_secs(3);

/// How long a scrub waits for its layers' decodes. A layer still decoding
/// then keeps its last picture (the others show their new ones), and the
/// viewer is repainted when it lands; its decoder is killed only after
/// [`crate::layer_feed::SCRUB_HANG_LIMIT`] without an answer.
const SCRUB_WAIT: Duration = Duration::from_secs(3);

/// The wait while a layer is still decoding from an earlier scrub: the
/// fetcher waits for every running decode, so a hung layer would otherwise
/// hold each scrub the whole [`SCRUB_WAIT`]. Enough for a file that answers.
const SCRUB_BUSY_WAIT: Duration = Duration::from_millis(250);

/// How long a file whose decoder hung (a size probe) is left out of probes
/// before it's tried again (each try can cost [`SIZE_PROBE_LIMIT`]).
const HUNG_PATH_PAUSE: Duration = Duration::from_secs(10);

/// Runs `on_timeout` (a kill) if the work it watches outlasts its limit:
/// what unblocks a call stuck on a hung drive.
struct Watchdog {
    done: Arc<(Mutex<bool>, parking_lot::Condvar)>,
    thread: Option<JoinHandle<bool>>,
}

impl Watchdog {
    fn arm(limit: Duration, on_timeout: impl FnOnce() + Send + 'static) -> Self {
        let done = Arc::new((Mutex::new(false), parking_lot::Condvar::new()));
        let watch = done.clone();
        let thread = std::thread::Builder::new()
            .name("vance-engine.watchdog".into())
            .spawn(move || {
                let (finished, wake) = &*watch;
                let deadline = Instant::now() + limit;
                let mut finished = finished.lock();
                while !*finished {
                    if wake.wait_until(&mut finished, deadline).timed_out() {
                        break;
                    }
                }
                if *finished {
                    return false;
                }
                drop(finished);
                on_timeout();
                true
            })
            .map_err(|e| tracing::warn!(error = %e, "watchdog not started"))
            .ok();
        Self { done, thread }
    }

    /// Stop watching; true when it had fired.
    fn disarm(mut self) -> bool {
        *self.done.0.lock() = true;
        self.done.1.notify_all();
        self.thread
            .take()
            .and_then(|t| t.join().ok())
            .unwrap_or(false)
    }
}

/// Kills a registry's busy sidecars if a fetch outlasts its limit.
struct FetchWatchdog {
    watchdog: Watchdog,
    sidecars: crate::decoder_pool::SidecarRegistry,
}

impl FetchWatchdog {
    fn arm(sidecars: crate::decoder_pool::SidecarRegistry, limit: Duration) -> Self {
        let registry = sidecars.clone();
        let watchdog = Watchdog::arm(limit, move || {
            let killed = registry.kill_busy();
            tracing::warn!(killed, "decoders stuck (a hung drive?); killed them");
        });
        Self { watchdog, sidecars }
    }

    /// Stop watching. True when it had to kill; the registry is back to
    /// normal either way.
    fn disarm(self) -> bool {
        let fired = self.watchdog.disarm();
        if fired {
            self.sidecars.resume();
        }
        fired
    }
}

/// Whether `path` exists, if its file system answers within the playback
/// paths' timeout; `None` when it doesn't (a dead mount), which callers
/// treat as offline for now. A path that didn't answer isn't checked again
/// for a while (see [`crate::decoder_pool::fs_probe`]).
fn file_exists_now(path: &Path) -> Option<bool> {
    use crate::decoder_pool::fs_probe::{exists_within, DEFAULT_TIMEOUT};
    exists_within(path, DEFAULT_TIMEOUT).ok()
}

/// How long the preview pool's proxies stand checked.
const PROXIES_RECHECK: Duration = Duration::from_secs(1);

/// Which media proxies were checked, when, and those found.
struct ProxiesChecked {
    at: Instant,
    /// (original, proxy) of every item with a proxy.
    candidates: Vec<(String, PathBuf)>,
    /// original → proxy, for the proxies that exist.
    found: HashMap<String, String>,
}

/// A file's bare name for messages and the import events (the path when it
/// has none).
pub fn file_label(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_string(), |n| n.to_string_lossy().into_owned())
}

impl Engine {
    fn import_probe_limit(&self) -> Duration {
        #[cfg(test)]
        if let Some(limit) = self.test_probe_limit {
            return limit;
        }
        IMPORT_PROBE_LIMIT
    }
}

/// Why the engine remakes a proxy that exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RemakeWhy {
    /// It was made in an older format.
    OlderFormat,
    /// Its media item's input space changed.
    NewColourSpace,
}

/// Why the engine queues an auto-proxy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProxyNeed {
    /// The layer went over the preview memory budget.
    OverBudget,
    /// Its proxy file wouldn't open.
    Broken,
    /// Its proxy was made in an older format.
    Outdated,
}

/// Whether a layer left out of an export frame stops the export: anything
/// but an add-on that didn't run (that layer was still drawn).
fn fails_export(s: &SkippedLayer) -> bool {
    s.kind != SkipKind::AddonFailed
}

/// Presets whose files carry an alpha channel (ProRes 4444,
/// `yuva444p10le`).
fn preset_keeps_alpha(p: &vance_encoder::EncoderPreset) -> bool {
    matches!(p, vance_encoder::EncoderPreset::ProRes4444)
}

/// Media of every clip on a video track, earliest `timeline_in` first
/// (ties: the lower track first).
fn video_media_by_start(seq: &Sequence) -> Vec<MediaItemId> {
    let mut clips: Vec<(vance_core::RationalTime, usize, MediaItemId)> = seq
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.kind == TrackKind::Video)
        .flat_map(|(i, t)| t.clips.iter().map(move |c| (c.timeline_in, i, c.media)))
        .collect();
    clips.sort_by_key(|&(start, track, _)| (start, track));
    clips.into_iter().map(|(.., media)| media).collect()
}

/// The blend mode shared by the enabled clips of `track` overlapping
/// `[start, end)` seconds (Normal when there are none); an error when
/// they differ, since one rendered clip can only have one (decision 2).
fn shared_blend_mode(
    seq: &Sequence,
    track: TrackId,
    start: f64,
    end: f64,
) -> Result<vance_timeline::BlendMode, EngineError> {
    let track =
        seq.tracks.iter().find(|t| t.id == track).ok_or_else(|| {
            EngineError::Project("track not found in the current sequence".into())
        })?;
    let mut modes = track
        .clips
        .iter()
        .filter(|c| {
            c.enabled
                && rt_to_seconds(c.timeline_in) < end
                && rt_to_seconds(c.timeline_out()) > start
        })
        .map(|c| c.blend_mode);
    let first = modes.next().unwrap_or_default();
    if modes.any(|m| m != first) {
        return Err(EngineError::EditRefused(
            "the clips in this range use different blend modes; render them separately".into(),
        ));
    }
    Ok(first)
}

/// Whether a dissolve rendered into one clip, then blended with `mode`,
/// looks the same as its two sides blended live. True for the modes whose
/// blend is affine in the source (Normal, Multiply, Screen, Overlay); Add
/// clamps in SDR, and the rest are non-linear in it.
fn dissolve_renders_exactly(mode: vance_timeline::BlendMode) -> bool {
    use vance_timeline::BlendMode as B;
    matches!(mode, B::Normal | B::Multiply | B::Screen | B::Overlay)
}

/// HDR (PQ / HLG) media, which render-and-replace can't carry yet.
fn is_hdr_media(m: &MediaItem) -> bool {
    m.input_color_space.is_some_and(|c| {
        matches!(
            c.decompose().1,
            vance_color::TransferFunction::Pq | vance_color::TransferFunction::Hlg
        )
    })
}

/// Display size of a `stored` frame with sample aspect `sar`: the width
/// scaled to square pixels.
fn display_size(stored: (u32, u32), sar: (u32, u32)) -> (u32, u32) {
    let (w, h) = stored;
    match sar {
        (n, d) if n > 0 && d > 0 && n != d => {
            let w = (u64::from(w) * u64::from(n) + u64::from(d) / 2) / u64::from(d);
            (u32::try_from(w).unwrap_or(u32::MAX), h)
        }
        _ => (w, h),
    }
}

impl Engine {
    /// The renderer, created headless when no viewport was installed
    /// (exports and tests).
    fn ensure_render_state(&mut self) -> Result<Arc<Mutex<RenderState>>, EngineError> {
        if let Some(rs) = self.render_state.as_ref() {
            return Ok(rs.clone());
        }
        let renderer = Renderer::new(RendererConfig::default())
            .map_err(|e| EngineError::Project(format!("headless renderer: {e}")))?;
        self.addons
            .lock()
            .set_gpu(renderer.device().clone(), renderer.queue().clone());
        let arc = Arc::new(Mutex::new(RenderState {
            renderer,
            surface_config: None,
            source_surface_config: None,
        }));
        self.render_state = Some(arc.clone());
        self.apply_view_lut();
        Ok(arc)
    }

    /// Build the compose resources on first use. The MoGraph / Fusion
    /// engines scan the system fonts once, here.
    fn ensure_compose_res(&mut self) {
        if self.compose_res.is_some() {
            return;
        }
        let cache = self.ensure_frame_cache();
        // Preview add-ons go through the render cache; exports run uncached.
        self.addons.lock().set_render_cache(Some(cache));
        // The MoGraph / Fusion cells are shared, not built: a project without
        // titles or comps never loads the font database.
        self.compose_res = Some(ComposeResources::new(
            self.addons.clone(),
            self.luts.clone(),
            self.mograph_engine.clone(),
            self.fusion_compositor.clone(),
        ));
        self.media_dims_stale = true;
    }

    /// Give the compose resources the current media sizes (a frame from a
    /// smaller proxy scales its source-pixel parameters by them).
    fn sync_media_dims(&mut self) {
        if !self.media_dims_stale {
            return;
        }
        if let Some(res) = self.compose_res.as_mut() {
            res.set_media_dims(self.media_dims.clone());
            self.media_dims_stale = false;
        }
    }

    /// Record a video / image item's stream: its stored size and pixel
    /// aspect.
    fn note_media_stream(&mut self, id: MediaItemId, info: &StreamInfo) {
        self.note_media_size(id, (info.width, info.height), info.sample_aspect);
    }

    fn note_media_size(&mut self, id: MediaItemId, size: (u32, u32), sar: (u32, u32)) {
        self.media_dims.insert(id, size);
        self.media_sar.insert(id, sar);
        self.media_dims_stale = true;
    }

    /// Learn the stored size of the layers' media not measured yet
    /// (`(media, file decoded)` pairs): from the preview pool when it has
    /// opened the original, else from the media's proxy marker (which
    /// records its original's size); with `probe`, failing both, by
    /// measuring in the background the original of a layer that decodes
    /// another file (its proxy), whose frames can't tell.
    fn learn_media_dims(&mut self, layers: &[(MediaItemId, String)], probe: bool) {
        let now = Instant::now();
        self.hung_paths.retain(|_, until| *until > now);
        for (media, decoded) in layers {
            if self.media_dims.contains_key(media) {
                continue;
            }
            let Some(m) = self
                .current_project
                .as_ref()
                .and_then(|p| p.media.get(media))
            else {
                continue;
            };
            if !matches!(m.kind, MediaKind::Video | MediaKind::Image) {
                continue;
            }
            let original = m.path.to_string_lossy().into_owned();
            let opened = self
                .scrub_pool
                .as_ref()
                .and_then(|p| p.stream_info(&original))
                .cloned();
            let marked = match m.proxy_path.as_deref() {
                // A proxy on a drive that doesn't answer is skipped (asked
                // again next time).
                Some(proxy)
                    if opened.is_none()
                        && !self.marker_misses.contains(media)
                        && file_exists_now(proxy).is_some() =>
                {
                    #[cfg(test)]
                    {
                        self.marker_reads += 1;
                    }
                    let read = crate::proxy::current_marker(proxy)
                        .and_then(|mk| mk.original_size.map(|size| (size, mk.original_sar)));
                    if read.is_none() {
                        self.marker_misses.insert(*media);
                    }
                    read
                }
                _ => None,
            };
            match (opened, marked) {
                (Some(info), _) => self.note_media_stream(*media, &info),
                (None, Some((size, sar))) => self.note_media_size(*media, size, sar),
                (None, None)
                    if probe
                        && *decoded != original
                        && !self.hung_paths.contains_key(&original) =>
                {
                    // Off the engine lock: a slow drive's open isn't waited
                    // on, nor killed.
                    self.measure_in_background(*media);
                }
                (None, None) => {}
            }
        }
    }

    /// The preview decoder pool, created on first use. Callers stop
    /// sequence playback first: the pool is on the playback thread then.
    fn ensure_scrub_pool(&mut self) {
        if self.scrub_pool.is_none() {
            // Its fetcher's decodes belong to the old pool: dropped (killing
            // any still running) with it.
            self.scrub_fetcher = None;
            self.scrub_pool = Some(new_decoder_pool());
        }
    }

    /// Point the preview pool's over-budget swap at every media item's
    /// existing proxy file. Preview only: the export pool never gets any.
    fn refresh_pool_proxies(&mut self) {
        let (Some(pool), Some(project)) = (self.scrub_pool.as_mut(), self.current_project.as_ref())
        else {
            return;
        };
        let candidates: Vec<(String, PathBuf)> = project
            .media
            .values()
            .filter_map(|m| Some((m.path.to_string_lossy().into_owned(), m.proxy_path.clone()?)))
            .collect();
        // Run per scrub frame: the same proxies aren't checked again within
        // a second (the pool still gets them: it may be a new one).
        let fresh = self
            .pool_proxies_checked
            .as_ref()
            .filter(|c| c.candidates == candidates && c.at.elapsed() < PROXIES_RECHECK);
        let proxies = match fresh {
            Some(checked) => checked.found.clone(),
            None => {
                // Intentional: a proxy whose drive doesn't answer is left out
                // (the original plays), as if it were gone.
                let found: HashMap<String, String> = candidates
                    .iter()
                    .filter(|(_, proxy)| file_exists_now(proxy) == Some(true))
                    .map(|(original, proxy)| {
                        (original.clone(), proxy.to_string_lossy().into_owned())
                    })
                    .collect();
                self.pool_proxies_checked = Some(ProxiesChecked {
                    at: Instant::now(),
                    candidates,
                    found: found.clone(),
                });
                found
            }
        };
        pool.set_proxy_paths(proxies);
    }

    /// Compose the current sequence (or `seq_override`, an edited copy) at
    /// timeline `t` for the viewer through the scrub path, into `sink`.
    /// `adjust` edits the output spec (canvas size, view LUT). The frames
    /// are decoded before the render state is locked, so a slow decode
    /// never holds up playback or another viewer.
    fn render_preview(
        &mut self,
        rs: &Arc<Mutex<RenderState>>,
        t: f64,
        seq_override: Option<Sequence>,
        adjust: impl FnOnce(&mut vance_render::OutputSpec),
        sink: PreviewSink<'_>,
    ) -> Result<Option<Vec<u8>>, EngineError> {
        self.recheck_offline_originals();
        self.retry_deferred_sizing();
        self.ensure_compose_res();
        self.ensure_scrub_pool();
        self.refresh_pool_proxies();
        let cache = self.ensure_frame_cache();
        let seq = match seq_override {
            Some(s) => s,
            None => self.current_sequence_ref()?.clone(),
        };
        let plan = vance_timeline::scene_plan_at(&seq, timeline_rt(t));
        let reqs = {
            let project = self
                .current_project
                .as_ref()
                .ok_or(EngineError::NoProject)?;
            plan_requests(project, &seq, &plan, Purpose::Preview, None)
        };
        let hold = !matches!(sink, PreviewSink::Rgba8 { hold: false, .. });
        // Outside the render lock. A layer still decoding (a slow or hung
        // drive) keeps its last picture; the viewer is repainted once it
        // lands.
        let mut frames = self.scrub_fetch(&reqs, &cache, hold);
        self.scrub_pending = frames.pending();
        let layers: Vec<(MediaItemId, String)> =
            reqs.iter().map(|r| (r.media, r.path.clone())).collect();
        // A layer decoding its proxy can't tell its original's size: that's
        // measured off the engine lock (the size only scales proxy keyer
        // params meanwhile).
        self.collect_background_measures();
        self.learn_media_dims(&layers, true);
        self.sync_media_dims();

        let viewer = self.viewer;
        let project = self
            .current_project
            .as_ref()
            .ok_or(EngineError::NoProject)?;
        let mut out = output_spec_for(project, &seq, Purpose::Preview);
        adjust(&mut out);
        let res = self.compose_res.as_mut().expect("ensured above");
        let mut ctx = ComposeCtx {
            project,
            seq: &seq,
            purpose: Purpose::Preview,
            viewer,
            track_filter: None,
            realtime: false,
            res,
        };
        let mut skipped = Vec::new();
        let result = match sink {
            PreviewSink::Viewport(viewport, bounds) => {
                let mut guard = rs.lock();
                present_composed(&mut guard, viewport, bounds, |c| {
                    skipped = compose_frame(c, &plan, &mut frames, &mut ctx, &out)?.skipped;
                    Ok(())
                })
                .map(|()| None)
            }
            PreviewSink::Rgba8 { wait, .. } => {
                let guard = if wait { Some(rs.lock()) } else { rs.try_lock() };
                match guard {
                    // Contended (the scopes path): the caller keeps what it had.
                    None => Ok(None),
                    Some(mut guard) => {
                        let c = guard.renderer.compositor();
                        match compose_frame(c, &plan, &mut frames, &mut ctx, &out) {
                            Ok(report) => {
                                skipped = report.skipped;
                                c.finish_to_rgba8().map(Some).map_err(EngineError::from)
                            }
                            Err(e) => Err(e),
                        }
                    }
                }
            }
        };
        let pool = self.scrub_pool.as_mut().expect("ensured above");
        // A layer without a frame is skipped and reported; the pool's copy of
        // the reason has been logged.
        let _ = pool.take_error();
        let over_budget = pool.take_proxy_requests();
        let broken = pool.take_broken_proxies();
        self.handle_proxy_requests(over_budget, &broken);
        // A layer still decoding isn't a failure: no notice for it.
        skipped.retain(|s| {
            !reqs
                .iter()
                .any(|r| r.clip_id == s.clip_id && frames.is_pending(r))
        });
        drop(frames);
        self.notice_skipped_layers(&skipped, false);
        self.kick_layer_prefetch(&reqs);
        result
    }

    /// A scrub frame's layers from the preview pool, through its
    /// [`ScrubFetcher`]: each file decodes on a thread of its own, waited for
    /// up to [`Self::scrub_wait`]; one still decoding keeps its last picture
    /// and is pending. Without `hold` (reads that must have every layer)
    /// pending layers are waited out.
    ///
    /// [`ScrubFetcher`]: crate::layer_feed::ScrubFetcher
    fn scrub_fetch(
        &mut self,
        reqs: &[FrameRequest],
        cache: &Arc<vance_cache::FrameCache>,
        hold: bool,
    ) -> crate::layer_feed::ScrubFrames {
        self.ensure_scrub_pool();
        if self.scrub_fetcher.is_none() {
            let landed = self.scrub_landed.clone();
            // Intentional: only a flag, read by the UI's poll: it runs on the
            // fetcher's threads, may fire more than once per decode, and must
            // neither block nor take the engine lock.
            let on_landed: Arc<dyn Fn() + Send + Sync> =
                Arc::new(move || landed.store(true, Ordering::Release));
            self.scrub_fetcher = Some(crate::layer_feed::ScrubFetcher::new(Some(on_landed)));
        }
        loop {
            let wait = self.scrub_wait();
            // What lands from here on is news for the poll; what landed
            // before is in this fetch.
            self.scrub_landed.store(false, Ordering::Release);
            let fetcher = self.scrub_fetcher.as_mut().expect("made above");
            let pool = self.scrub_pool.as_mut().expect("ensured above");
            let frames = fetcher.fetch(pool, cache, reqs.to_vec(), wait);
            if hold || !frames.pending() {
                return frames;
            }
        }
    }

    /// How long a scrub waits for its layers: [`SCRUB_WAIT`], or
    /// [`SCRUB_BUSY_WAIT`] while a layer still decodes from before.
    fn scrub_wait(&self) -> Duration {
        let busy = self.scrub_fetcher.as_ref().is_some_and(|f| f.busy());
        #[cfg(test)]
        if let Some(wait) = self.test_scrub_wait {
            return if busy { wait / 4 } else { wait };
        }
        if busy {
            SCRUB_BUSY_WAIT
        } else {
            SCRUB_WAIT
        }
    }

    /// A layer the last scrub frame held has landed: show the latest scrub
    /// position (on the UI's poll, so landings close together repaint once).
    fn repaint_when_landed(&mut self) {
        if self.scrub_pending && self.scrub_landed.swap(false, Ordering::AcqRel) {
            self.repaint_now();
        }
    }

    /// Queue a notice for each layer a frame left out ("couldn't show")
    /// or drew without an add-on, once per clip per project (not per
    /// frame). `export`: only add-on notices; the rest fail the export.
    fn notice_skipped_layers(&mut self, skipped: &[SkippedLayer], export: bool) {
        for s in skipped {
            let addon = s.kind == SkipKind::AddonFailed;
            if (export && !addon) || !self.skip_noticed.insert((s.clip_id, addon)) {
                continue;
            }
            let name = self.layer_display_name(s.clip_id, s.media);
            self.notices.push(if addon {
                format!("An add-on didn't run on clip \"{name}\": {}", s.reason)
            } else {
                format!("Couldn't show {name}: {}", s.reason)
            });
        }
    }

    /// A layer's name for messages: its clip's name, else its media's, else
    /// its file's.
    fn layer_display_name(&self, clip: vance_timeline::ClipId, media: MediaItemId) -> String {
        let named = |n: &str| (!n.trim().is_empty()).then(|| n.to_string());
        let clip_name = self.current_sequence_ref().ok().and_then(|s| {
            s.tracks
                .iter()
                .flat_map(|t| t.clips.iter())
                .find(|c| c.id == clip)
                .and_then(|c| named(&c.name))
        });
        let item = self
            .current_project
            .as_ref()
            .and_then(|p| p.media.get(&media));
        clip_name
            .or_else(|| item.and_then(|m| named(&m.display_name)))
            .or_else(|| {
                item.and_then(|m| m.path.file_name())
                    .map(|f| f.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "a layer".into())
    }

    /// Recompose the paused Record frame (after an edit, a viewer-state
    /// change or with a preview copy of the sequence). A no-op while
    /// anything plays, before the sequence has been shown, or without a
    /// viewport surface.
    fn repaint_preview(&mut self, seq_override: Option<Sequence>) -> Result<(), EngineError> {
        if self.playback.is_some() || self.seq_playback.is_some() || self.record_shows_file {
            return Ok(());
        }
        let Some(t) = self.preview_t else {
            return Ok(());
        };
        #[cfg(test)]
        if self.viewport.is_none() {
            return self.repaint_test_viewer(t, seq_override);
        }
        let (Some(viewport), Some(rs)) = (self.viewport.clone(), self.render_state.clone()) else {
            return Ok(());
        };
        let bounds = self.last_bounds;
        if bounds.is_empty() && rs.lock().surface_config.is_none() {
            return Ok(());
        }
        self.render_preview(
            &rs,
            t,
            seq_override,
            |_| {},
            PreviewSink::Viewport(&viewport, bounds),
        )?;
        self.preview_painted_at = Some(Instant::now());
        Ok(())
    }

    /// `repaint_preview` into the unit tests' headless viewer.
    #[cfg(test)]
    fn repaint_test_viewer(
        &mut self,
        t: f64,
        seq_override: Option<Sequence>,
    ) -> Result<(), EngineError> {
        let (Some((w, h)), Some(rs)) = (
            self.test_viewer.as_ref().map(|v| v.size),
            self.render_state.clone(),
        ) else {
            return Ok(());
        };
        if self.test_viewer.as_ref().is_some_and(|v| v.fail) {
            return Err(EngineError::Project("test viewer set to fail".into()));
        }
        let frame = self.render_preview(
            &rs,
            t,
            seq_override,
            |out| {
                out.width = w;
                out.height = h;
            },
            PreviewSink::Rgba8 {
                wait: true,
                hold: true,
            },
        )?;
        // `None`: held (still decoding), as the viewport keeps its picture.
        if let (Some(v), Some(frame)) = (self.test_viewer.as_mut(), frame) {
            v.frame = Some(frame);
            v.repaints += 1;
        }
        self.preview_painted_at = Some(Instant::now());
        Ok(())
    }

    /// Prefetch ahead of the top file layer of a scrubbed frame, as the
    /// single-file scrub path does.
    fn kick_layer_prefetch(&mut self, requests: &[FrameRequest]) {
        if let Some((path, info, anchor)) = self.layer_prefetch_target(requests) {
            self.kick_prefetch_for(path, &info, anchor);
        }
    }

    /// What to prefetch after a scrubbed frame: the top file layer, in the
    /// file the preview pool decodes for it, from its stream PTS.
    fn layer_prefetch_target(
        &self,
        requests: &[FrameRequest],
    ) -> Option<(String, StreamInfo, i64)> {
        let top = requests.iter().rev().find(|r| !r.path.is_empty())?;
        let pool = self.scrub_pool.as_ref()?;
        // Over the memory budget the pool decodes a layer's proxy instead
        // (and its original when a proxy won't open).
        let path = pool.opened_path(&top.path).unwrap_or(&top.path).to_owned();
        let info = pool.stream_info(&path)?.clone();
        let anchor = crate::decoder_pool::source_seconds_to_pts(top.source_seconds, info.time_base);
        Some((path, info, anchor))
    }

    /// Queue proxy generation for originals the preview pool found over the
    /// memory budget, plus a once-per-project notice saying so (user
    /// decision 5). `broken`: proxy files the pool couldn't open, whose
    /// originals are among `originals` and are regenerated although the
    /// files exist.
    fn handle_proxy_requests(&mut self, originals: Vec<String>, broken: &[String]) {
        let Some(project) = self.current_project.as_ref() else {
            return;
        };
        let mut wanted: Vec<(String, ProxyNeed, crate::proxy::ProxyCodec)> = Vec::new();
        for path in originals {
            // Only a library item's original without a usable proxy: with
            // "use proxies" on, the request can name a proxy file itself.
            let Some(m) = project
                .media
                .values()
                .find(|m| m.path.to_string_lossy() == path.as_str())
            else {
                continue;
            };
            let existing = match m.proxy_path.as_ref().map(|p| (p, file_exists_now(p))) {
                Some((p, Some(true))) => Some(p),
                // Its drive doesn't answer: asked again next time.
                Some((_, None)) => continue,
                Some((_, Some(false))) | None => None,
            };
            let is_broken =
                existing.is_some_and(|p| broken.iter().any(|b| Path::new(b) == p.as_path()));
            // A remake keeps the codec its proxy was made with.
            let codec = existing
                .and_then(|p| crate::proxy::read_marker(p))
                .map(|m| m.codec)
                .unwrap_or_default();
            match (existing, is_broken) {
                (Some(_), false) => {}
                (_, true) => wanted.push((path, ProxyNeed::Broken, codec)),
                (None, false) => wanted.push((path, ProxyNeed::OverBudget, codec)),
            }
        }
        // Intentional: a failure is logged (in `queue_auto_proxies`), not
        // shown: these clips just keep previewing from their originals.
        let queued = self.queue_auto_proxies(wanted).unwrap_or_default();
        let names = |need: ProxyNeed| -> Vec<String> {
            queued
                .iter()
                .filter(|q| q.need == need)
                .map(|q| q.name.clone())
                .collect()
        };
        let (over_budget, regenerated) = (names(ProxyNeed::OverBudget), names(ProxyNeed::Broken));
        if !regenerated.is_empty() {
            // Not once per project: each job is queued once, and this one
            // tells the user why a clip previews at full size.
            self.notices.push(format!(
                "The proxy of {} couldn't be read. Vance is making a new one; until it's ready \
                 the clip previews from the original.",
                regenerated.join(", ")
            ));
        }
        if !over_budget.is_empty() && !self.proxy_notice_shown {
            self.proxy_notice_shown = true;
            self.notices.push(format!(
                "These clips are too large to preview together within the memory budget: {}. \
                 Vance is generating proxies for them; until those are ready they preview at \
                 full resolution and playback may stutter.",
                over_budget.join(", ")
            ));
        }
    }

    /// Queue an auto-proxy job for each original (a library item's file) not
    /// already queued, made with `codec`. The queued items' names with why;
    /// an error (for the user) when nothing could be queued because the
    /// proxy folder or the proxy worker is unavailable.
    fn queue_auto_proxies(
        &mut self,
        wanted: Vec<(String, ProxyNeed, crate::proxy::ProxyCodec)>,
    ) -> Result<Vec<QueuedProxy>, String> {
        let Some(project) = self.current_project.as_ref() else {
            return Ok(Vec::new());
        };
        struct Job {
            path: String,
            media: MediaItemId,
            name: String,
            input_color_space: Option<vance_color::ColorSpace>,
            need: ProxyNeed,
            codec: crate::proxy::ProxyCodec,
        }
        let mut jobs: Vec<Job> = Vec::new();
        for (path, need, codec) in wanted {
            if self.auto_proxies.values().any(|p| *p == path) || jobs.iter().any(|j| j.path == path)
            {
                continue;
            }
            let Some(m) = project
                .media
                .values()
                .find(|m| m.path.to_string_lossy() == path.as_str())
            else {
                continue;
            };
            jobs.push(Job {
                path,
                media: m.id,
                name: m.display_name.clone(),
                input_color_space: m.input_color_space,
                need,
                codec,
            });
        }
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        let dir = self
            .auto_proxy_dir()
            .and_then(|dir| std::fs::create_dir_all(&dir).map(|()| dir))
            .map_err(|e| {
                tracing::warn!(error = %e, "auto-proxy: no proxy folder");
                format!("the proxy folder can't be created: {e}")
            })?;
        let worker = self.ensure_proxy_worker().map_err(|e| {
            tracing::warn!(error = %e, "auto-proxy: proxy worker unavailable");
            format!("proxies can't be made on this install: {e}")
        })?;
        let mut queued = Vec::new();
        let mut ids = Vec::new();
        for j in jobs {
            let id = format!("auto-proxy-{}", j.media);
            // The format in the name: a remake never overwrites the proxy it
            // replaces (kept until the new one is done).
            let output = dir.join(format!(
                "{}.v{}.proxy.mov",
                j.media,
                crate::proxy::PROXY_FORMAT_VERSION
            ));
            worker.enqueue(crate::proxy::ProxyJob {
                id: id.clone(),
                source_path: j.path.clone(),
                output_path: output.to_string_lossy().into_owned(),
                preset: j.codec.preset(),
                input_color_space: j.input_color_space,
            });
            tracing::info!(path = %j.path, need = ?j.need, codec = ?j.codec, "auto-proxy: queued");
            ids.push((id, j.path.clone()));
            queued.push(QueuedProxy {
                original: j.path,
                name: j.name,
                need: j.need,
            });
        }
        self.auto_proxies.extend(ids);
        Ok(queued)
    }

    /// Load-time: proxies made in an older format (see
    /// [`crate::proxy::PROXY_FORMAT_VERSION`]) are wrong for today's
    /// pipeline. Each whose original is reachable is dropped from its media
    /// item, so preview decodes the original, and made again; one notice
    /// says so.
    fn retire_outdated_proxies(&mut self) {
        let Some(project) = self.current_project.as_ref() else {
            return;
        };
        let mut reachable = Vec::new();
        for m in project.media.values() {
            // A proxy file that's gone is already ignored (and isn't old);
            // one on a drive that doesn't answer is left alone.
            let Some(proxy) = m
                .proxy_path
                .as_ref()
                .filter(|p| file_exists_now(p) == Some(true))
            else {
                continue;
            };
            if crate::proxy::matches_media(proxy, m.input_color_space) {
                continue;
            }
            // Not answering counts as offline: rechecked like one.
            if file_exists_now(&m.path) == Some(true) {
                reachable.push(m.id);
            } else {
                // Intentional: kept. With the original offline (a drive
                // that's not connected) the old proxy is the clip's only
                // picture; it's remade once the original is back.
                self.proxies_awaiting_original.insert(m.id);
            }
        }
        self.originals_checked_at = Some(Instant::now());
        self.remake_outdated_proxies(reachable, RemakeWhy::OlderFormat);
    }

    /// Remake the old-format proxies kept while their originals were
    /// offline, for those whose original is back. Checked on preview and
    /// play, at most every [`ORIGINALS_RECHECK`].
    fn recheck_offline_originals(&mut self) {
        if self.proxies_awaiting_original.is_empty()
            || self
                .originals_checked_at
                .is_some_and(|t| t.elapsed() < ORIGINALS_RECHECK)
        {
            return;
        }
        self.originals_checked_at = Some(Instant::now());
        let Some(project) = self.current_project.as_ref() else {
            return;
        };
        let back: Vec<MediaItemId> = self
            .proxies_awaiting_original
            .iter()
            .copied()
            .filter(|id| {
                project
                    .media
                    .get(id)
                    .is_none_or(|m| file_exists_now(&m.path) == Some(true))
            })
            .collect();
        for id in &back {
            self.proxies_awaiting_original.remove(id);
        }
        self.remake_outdated_proxies(back, RemakeWhy::OlderFormat);
    }

    /// Queue a new proxy for each item whose proxy doesn't suit it (`why`)
    /// and, for each one queued, drop the old one meanwhile (preview decodes
    /// the original); a failed remake puts it back. One notice for them all.
    fn remake_outdated_proxies(&mut self, media: Vec<MediaItemId>, why: RemakeWhy) {
        let Some(project) = self.current_project.as_ref() else {
            return;
        };
        let mut wanted = Vec::new();
        let mut old = HashMap::new();
        for id in media {
            let Some(m) = project.media.get(&id) else {
                continue;
            };
            // Replaced meanwhile (by a suitable proxy, or none): nothing to do.
            let Some(proxy) = m
                .proxy_path
                .as_ref()
                .filter(|p| file_exists_now(p) == Some(true))
            else {
                continue;
            };
            if crate::proxy::matches_media(proxy, m.input_color_space) {
                continue;
            }
            // A remake keeps the codec its proxy was made with, if recorded.
            let codec = crate::proxy::read_marker(proxy)
                .map(|marker| marker.codec)
                .unwrap_or_default();
            let original = m.path.to_string_lossy().into_owned();
            old.insert(original.clone(), (id, proxy.clone()));
            wanted.push((original, ProxyNeed::Outdated, codec));
        }
        if wanted.is_empty() {
            return;
        }
        let tried = wanted.len();
        let queued = match self.queue_auto_proxies(wanted) {
            Ok(queued) => queued,
            Err(reason) => {
                // Intentional: the old proxies stay. They're the wrong format,
                // but a clip that previews beats one that loses its proxy to a
                // remake that can't happen (and the original may be slow to
                // read, or on a drive about to go).
                self.notices.push(match why {
                    RemakeWhy::OlderFormat => format!(
                        "{tried} {} made by an older version of Vance couldn't be remade \
                         ({reason}); the clips keep previewing from them.",
                        if tried == 1 { "proxy" } else { "proxies" }
                    ),
                    RemakeWhy::NewColourSpace => format!(
                        "The proxy couldn't be remade for the new colour space ({reason}); the \
                         clip keeps previewing from the old one."
                    ),
                });
                return;
            }
        };
        let Some(project) = self.current_project.as_mut() else {
            return;
        };
        for q in &queued {
            let Some((id, proxy)) = old.remove(&q.original) else {
                continue;
            };
            if let Some(m) = project.media.get_mut(&id) {
                let mut updated = (**m).clone();
                updated.proxy_path = None;
                *m = Arc::new(updated);
            }
            self.marker_misses.remove(&id);
            self.replaced_proxies.insert(q.original.clone(), proxy);
        }
        let n = queued.len();
        if n == 0 {
            return;
        }
        let names: Vec<&str> = queued.iter().map(|q| q.name.as_str()).collect();
        self.notices.push(match (why, n) {
            (RemakeWhy::OlderFormat, 1) => "Updating 1 proxy made by an older version of Vance; \
                 the clip previews from its original until it's ready."
                .to_string(),
            (RemakeWhy::OlderFormat, _) => format!(
                "Updating {n} proxies made by an older version of Vance; the clips preview from \
                 their originals until they're ready."
            ),
            (RemakeWhy::NewColourSpace, _) => format!(
                "Remaking the proxy of {} for its new colour space; it previews from the \
                 original until it's ready.",
                names.join(", ")
            ),
        });
    }

    /// Where auto-proxies are written (the user's media folder).
    fn auto_proxy_dir(&self) -> std::io::Result<PathBuf> {
        #[cfg(test)]
        if let Some(dir) = self.test_proxy_dir.clone() {
            return Ok(dir);
        }
        persistent_media_dir().map(|d| d.join("proxies"))
    }

    /// Attach finished auto-proxies to their media items and to the
    /// preview pool.
    fn poll_auto_proxies(&mut self) {
        if self.auto_proxies.is_empty() {
            return;
        }
        let Some(worker) = self.proxy_worker.as_ref() else {
            return;
        };
        let mut finished: Vec<(String, String, Option<PathBuf>)> = Vec::new();
        for job in worker.status_snapshot() {
            let Some(original) = self.auto_proxies.get(&job.id) else {
                continue;
            };
            match job.status {
                crate::proxy::ProxyJobStatus::Done { .. } => finished.push((
                    job.id,
                    original.clone(),
                    Some(PathBuf::from(job.output_path)),
                )),
                crate::proxy::ProxyJobStatus::Error(e) => {
                    tracing::warn!(path = %original, error = %e, "auto-proxy: generation failed");
                    finished.push((job.id, original.clone(), None));
                }
                _ => {}
            }
        }
        if finished.is_empty() {
            return;
        }
        let auto_dir = self.auto_proxy_dir().ok();
        for (id, original, proxy) in finished {
            self.auto_proxies.remove(&id);
            let replaced = self.replaced_proxies.remove(&original);
            let Some(project) = self.current_project.as_mut() else {
                continue;
            };
            let Some(m) = project
                .media
                .values_mut()
                .find(|m| m.path.to_string_lossy() == original.as_str())
            else {
                continue;
            };
            let mut updated = (**m).clone();
            let failed = proxy.is_none();
            match (proxy, replaced) {
                (Some(new), old) => {
                    // An auto-proxy it replaces goes; anything else stays.
                    if let (Some(old), Some(dir)) = (old, auto_dir.as_ref()) {
                        if old != new {
                            remove_generated_proxy(dir, &old, updated.id);
                        }
                    }
                    updated.proxy_path = Some(new);
                }
                // The remake failed: the old proxy comes back, unless another
                // was attached meanwhile.
                (None, Some(old)) if updated.proxy_path.is_none() => {
                    updated.proxy_path = Some(old);
                }
                (None, _) => {}
            }
            let (media, name) = (updated.id, updated.display_name.clone());
            *m = Arc::new(updated);
            self.marker_misses.remove(&media);
            if failed {
                self.notices.push(format!(
                    "Couldn't make the proxy of {name}; it previews from what it had before."
                ));
            }
        }
        self.refresh_pool_proxies();
    }

    /// UI notices queued since the last call (e.g. auto-proxy generation),
    /// for the Tauri layer to emit as `engine://notice` events.
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    /// Blocking feeds over the export pool (kept from the last export
    /// frame read, else new).
    fn take_export_stream(&mut self) -> StreamProvider {
        let pool = self.export_pool.take().unwrap_or_else(new_decoder_pool);
        StreamProvider::new(pool, FeedPolicy::Blocking)
    }

    /// The export per-frame function: composite the current sequence at
    /// timeline `t` on a `size` canvas from the full-resolution originals
    /// (no viewer state) and read it back, RGBA8 or (`hdr`) rgba64le;
    /// `transparent` keeps the alpha (straight). A layer left out of the
    /// frame (offline media, no frame, a title / graphic / comp that
    /// didn't render) fails the export, naming the clip.
    // Intentional: an add-on that didn't run doesn't fail it: the layer is
    // drawn without it, the user gets a notice, and an unknown plugin must
    // not break an export (pinned by `wasm_plugin_export`).
    #[allow(clippy::too_many_arguments)]
    fn export_frame_with(
        &mut self,
        rs: &Arc<Mutex<RenderState>>,
        provider: &mut StreamProvider,
        t: f64,
        size: (u32, u32),
        hdr: bool,
        track_filter: Option<TrackId>,
        transparent: bool,
    ) -> Result<Vec<u8>, EngineError> {
        self.ensure_compose_res();
        self.sync_media_dims();
        let project = self
            .current_project
            .as_ref()
            .ok_or(EngineError::NoProject)?;
        let id = self
            .current_sequence_id
            .ok_or(EngineError::NoCurrentSequence)?;
        let seq = project.sequence(id).ok_or(EngineError::SequenceNotFound)?;
        let purpose = Purpose::Export { hdr };
        let plan = vance_timeline::scene_plan_at(seq, timeline_rt(t));
        let mut out = output_spec_for(project, seq, purpose);
        (out.width, out.height) = size;
        out.transparent = transparent;
        // Intentional: no look-ahead `prepare` here. Blocking feeds wait for
        // their frames anyway, and an error from preparing a clip that only
        // starts after the export range would fail the export.
        // Blocking feeds can wait seconds for a frame: never under the lock.
        let reqs = plan_requests(project, seq, &plan, purpose, track_filter);
        let mut frames = Prefetched::fetch(provider, reqs);
        // The skipped layers below carry the reasons (with their clips).
        let _ = provider.pool_mut().take_error();
        let res = self.compose_res.as_mut().expect("ensured above");
        let mut ctx = ComposeCtx {
            project,
            seq,
            purpose,
            viewer: ViewerState::default(),
            track_filter,
            realtime: false,
            res,
        };
        let (skipped, pixels) = {
            let mut guard = rs.lock();
            let c = guard.renderer.compositor();
            let skipped = compose_frame(c, &plan, &mut frames, &mut ctx, &out)?.skipped;
            let pixels = match (skipped.iter().any(fails_export), hdr) {
                (true, _) => None,
                (false, true) => Some(c.finish_to_rgba64()?),
                (false, false) => Some(c.finish_to_rgba8()?),
            };
            (skipped, pixels)
        };
        self.notice_skipped_layers(&skipped, true);
        match (pixels, skipped.iter().find(|s| fails_export(s))) {
            (Some(pixels), _) => Ok(pixels),
            (None, missing) => {
                let (name, reason) = missing.map_or_else(Default::default, |s| {
                    (
                        self.layer_display_name(s.clip_id, s.media),
                        s.reason.clone(),
                    )
                });
                Err(EngineError::Export(format!(
                    "Can't export: clip \"{name}\" — {reason}"
                )))
            }
        }
    }

    /// Composite and push every video frame of an export.
    fn encode_frames(
        &mut self,
        client: &mut vance_encoder::EncoderClient,
        rs: &Arc<Mutex<RenderState>>,
        provider: &mut StreamProvider,
        job: &ExportJob,
        cancel: &ExportCanceller,
    ) -> Result<(), EngineError> {
        for f in 0..job.total_frames {
            if cancel.is_cancelled() {
                return Err(EngineError::Cancelled);
            }
            let t = job.start_s
                + (f as f64) * (job.frame_rate_den as f64) / (job.frame_rate_num as f64);
            let frame = self.export_frame_with(
                rs,
                provider,
                t,
                job.size,
                job.hdr,
                job.track_filter,
                job.transparent,
            );
            // A cancel kills the decoders the frame waits on: that failure
            // is the cancel, not a missing clip.
            let pixels = match frame {
                Err(_) if cancel.is_cancelled() => return Err(EngineError::Cancelled),
                other => other?,
            };
            client
                .push_video(f as i64, pixels)
                .map_err(|e| EngineError::Project(format!("encoder push_video: {e}")))?;
        }
        Ok(())
    }

    /// Apply a clip-inserting `op`. The sequence's first MEASURABLE video
    /// clip sets its canvas size (user decision 3), in the same undo step,
    /// unless the size was already set; once one is there, later clips never
    /// change it (a portrait clip doesn't turn a landscape edit). Measurable
    /// as the open-time migration measures (`media_frame_size`): an audio
    /// item or an offline file on a video track doesn't count, so insert and
    /// reopen agree.
    fn apply_clip_insert(
        &mut self,
        op: EditOp,
        label: &str,
        track_id: TrackId,
        media: MediaItemId,
    ) -> Result<(), EngineError> {
        let (unsized_video, existing) = {
            let seq = self.current_sequence_ref()?;
            let on_video = seq
                .tracks
                .iter()
                .any(|t| t.id == track_id && t.kind == TrackKind::Video);
            (
                seq.frame_size.is_none() && on_video,
                video_media_by_start(seq),
            )
        };
        // The first measurable clip sizes it, unless one already there can't
        // be told now (a hung drive): that one may be the first, so the
        // sizing waits for it (`retry_deferred_sizing`).
        let mut needs_size = unsized_video;
        if unsized_video {
            for m in existing {
                match self.measure_media(m) {
                    Measured::Unmeasurable => {}
                    Measured::Size(_) => {
                        needs_size = false;
                        break;
                    }
                    Measured::Unknown => {
                        needs_size = false;
                        if let Some(seq) = self.current_sequence_id {
                            self.sizing_deferred.insert(seq);
                        }
                        break;
                    }
                }
            }
        }
        match needs_size.then(|| self.measure_media(media)) {
            Some(Measured::Size(size)) => self.apply_edit_op(EditOp::Compound {
                ops: vec![op, EditOp::SetFrameSize { size: Some(size) }],
                label: label.into(),
            }),
            Some(Measured::Unknown) => {
                if let Some(seq) = self.current_sequence_id {
                    self.sizing_deferred.insert(seq);
                }
                self.apply_edit_op(op)
            }
            _ => self.apply_edit_op(op),
        }
    }

    /// A media item's canvas size, rounded down to even and within the
    /// canvas limits: a file's DISPLAY size (its stored size, probed by a
    /// pooled decoder when it wasn't seen at import, with anamorphic pixels
    /// made square), a synthetic item's own canvas. `None` for audio or
    /// when it can't be read (or told now).
    #[cfg(test)]
    fn media_frame_size(&mut self, id: MediaItemId) -> Option<(u32, u32)> {
        match self.measure_media(id) {
            Measured::Size(size) => Some(size),
            _ => None,
        }
    }

    /// A media item's canvas size, as far as it can be told now (see
    /// [`Measured`]).
    fn measure_media(&mut self, id: MediaItemId) -> Measured {
        let Some(m) = self
            .current_project
            .as_ref()
            .and_then(|p| p.media.get(&id))
            .cloned()
        else {
            return Measured::Unmeasurable;
        };
        let size = match m.kind {
            MediaKind::Title => m.title.as_ref().map(|t| (t.width, t.height)),
            MediaKind::MoGraph => m
                .mograph
                .as_ref()
                .map(|i| (i.template.canvas.width, i.template.canvas.height)),
            MediaKind::Fusion => m.fusion.as_ref().map(|c| (c.width, c.height)),
            MediaKind::Audio => None,
            MediaKind::Video | MediaKind::Image => {
                match self.probe_media_dims(id, &m.path.to_string_lossy()) {
                    Measured::Size(stored) => {
                        let sar = self.media_sar.get(&id).copied().unwrap_or((1, 1));
                        Some(display_size(stored, sar))
                    }
                    other => return other,
                }
            }
        };
        let Some((w, h)) = size else {
            return Measured::Unmeasurable;
        };
        let (w, h) = (w & !1, h & !1);
        let edge = 16..=16384;
        if edge.contains(&w) && edge.contains(&h) {
            Measured::Size((w, h))
        } else {
            Measured::Unmeasurable
        }
    }

    /// A video / image item's stored size: known, else probed by a pooled
    /// decoder on its original file. `Unknown` when the file doesn't answer
    /// in time (or didn't, a moment ago).
    fn probe_media_dims(&mut self, id: MediaItemId, path: &str) -> Measured {
        if let Some(&dims) = self.media_dims.get(&id) {
            return Measured::Size(dims);
        }
        let now = Instant::now();
        self.hung_paths.retain(|_, until| *until > now);
        if self.hung_paths.contains_key(path) {
            return Measured::Unknown;
        }
        // A dead mount blocks even `stat`: checked with a timeout first, so
        // it costs that, not the decoder's open limit.
        match file_exists_now(Path::new(path)) {
            Some(true) => {}
            Some(false) => return Measured::Unmeasurable,
            None => return Measured::Unknown,
        }
        // Under the engine lock: a hung drive can't hold it past the limit;
        // the clip is then unmeasured (and the file left alone a while).
        self.ensure_scrub_pool();
        let sidecars = self.scrub_pool.as_ref().expect("ensured above").sidecars();
        let watchdog = FetchWatchdog::arm(sidecars, SIZE_PROBE_LIMIT);
        let taken = self.take_pooled_decoder(path);
        if watchdog.disarm() {
            tracing::warn!(path, "size probe hung; the clip stays unmeasured");
            self.hung_paths
                .insert(path.to_owned(), Instant::now() + HUNG_PATH_PAUSE);
            return Measured::Unknown;
        }
        let Ok(dec) = taken else {
            return Measured::Unmeasurable;
        };
        self.return_pooled_decoder(dec);
        // The pool may have opened the proxy instead (over the memory
        // budget), but it always opens the original first.
        let Some(info) = self
            .scrub_pool
            .as_ref()
            .and_then(|p| p.stream_info(path))
            .cloned()
        else {
            return Measured::Unmeasurable;
        };
        self.note_media_stream(id, &info);
        Measured::Size((info.width, info.height))
    }

    /// A preview-pool decoder with `path` open (one layer's thread budget).
    fn take_pooled_decoder(
        &mut self,
        path: &str,
    ) -> Result<crate::decoder_pool::PooledDecoder, EngineError> {
        self.ensure_scrub_pool();
        self.scrub_pool
            .as_mut()
            .expect("ensured above")
            .take(path, 1)
    }

    fn return_pooled_decoder(&mut self, dec: crate::decoder_pool::PooledDecoder) {
        match self.scrub_pool.as_mut() {
            Some(pool) => pool.put(dec),
            None => drop(dec),
        }
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
                let alt = std::env::temp_dir().join(format!("vance-cache-{}", std::process::id()));
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
        let input_color_space = self.current_project.as_ref().and_then(|p| {
            p.media
                .values()
                .find(|m| m.path.to_string_lossy() == source_path.as_str())
                .and_then(|m| m.input_color_space)
        });
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
            input_color_space,
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
        // Bundled default pack ships next to the executable (same place as the
        // sidecars). Lowest precedence, so a user/project plugin of the same id
        // overrides it.
        if let Ok(exe) = std::env::current_exe() {
            if let Some(parent) = exe.parent() {
                roots.push(parent.join("plugins"));
            }
        }
        let registry = vance_plugin_sdk::discovery::PluginRegistry::scan(&roots);
        let count = registry.len();
        self.addons.lock().set_registry(registry);
        count
    }

    /// Phase 10.5 — UI-facing list of discovered plugins.
    pub fn discovered_plugins(&self) -> Vec<vance_plugin_sdk::discovery::DiscoveredPlugin> {
        self.addons.lock().registry().plugins().to_vec()
    }

    /// Phase 10.3 — additively scan one extra directory into the
    /// plugin registry (e.g. a user-added custom folder). Returns the
    /// new total count. Also drops cached instances so an edited
    /// plugin reloads.
    // Intentional: lib API driven by the integration tests (tests/it); the
    // bin target, which compiles this file too, never calls it.
    #[allow(dead_code)]
    pub fn add_plugin_dir(&mut self, dir: std::path::PathBuf) -> usize {
        self.addons.lock().scan_dir(&dir)
    }

    /// Install the shared entitlement gate consulted at plugin load (M2, §6).
    pub fn set_entitlement_gate(&mut self, gate: Arc<EntitlementGate>) {
        self.addons.lock().set_entitlement_gate(Some(gate));
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
        self.marker_misses.remove(&media_id);
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
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
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
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
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
        let project = self
            .current_project
            .as_mut()
            .ok_or(EngineError::NoProject)?;
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
        let (Some(uri), Some(info)) = (self.current_media_path.clone(), self.stream_info.clone())
        else {
            return;
        };
        self.kick_prefetch_for(uri, &info, anchor_pts);
    }

    /// Prefetch `uri` (whose stream is `info`) forward from `anchor_pts`.
    fn kick_prefetch_for(&mut self, uri: String, info: &StreamInfo, anchor_pts: i64) {
        let (tb_n, tb_d) = info.time_base;
        if info.frame_rate.0 <= 0 || info.frame_rate.1 <= 0 || tb_n <= 0 {
            return;
        }
        // step = (tb_d / tb_n) * (frame_rate.1 / frame_rate.0)
        // → represented as i64 to fit AtomicI64.
        let step_pts = tb_d * info.frame_rate.1 / (tb_n * info.frame_rate.0);
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
    [
        PathBuf::from("target/debug").join(exe_name),
        PathBuf::from("target/release").join(exe_name),
    ]
    .into_iter()
    .find(|c| c.exists())
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
// Intentional: engine.rs is compiled into both the lib and the bin
// target; this is only called from bin-only modules, so the lib build
// alone sees it as unused.
#[allow(dead_code)]
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
    fn compositing_track_ops_edit_the_current_sequence() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let names = |e: &Engine| -> Vec<String> {
            let c = e.current_sequence_contents().unwrap();
            c.tracks.iter().map(|t| t.name.clone()).collect()
        };
        let ids = |e: &Engine| -> Vec<TrackId> {
            let c = e.current_sequence_contents().unwrap();
            c.tracks
                .iter()
                .map(|t| TrackId::from_uuid(t.id.parse().unwrap()))
                .collect()
        };
        let (v1, a1) = {
            let start = ids(&e);
            (start[0], start[1])
        };
        assert_eq!(names(&e), ["V1", "A1"]);

        // Automatic names follow position (V1 = bottom video track); custom
        // names ("Music") are kept but still take up a number.
        let v2 = e.add_track(TrackKind::Video, None, None).unwrap();
        let music = e
            .add_track(TrackKind::Audio, Some("Music".into()), Some(0))
            .unwrap();
        assert_eq!(names(&e), ["V1", "V2", "Music", "A2"]);
        e.move_track(v2, 0).unwrap();
        assert_eq!(ids(&e), [v2, v1, music, a1], "moved to the bottom");
        assert_eq!(names(&e), ["V1", "V2", "Music", "A2"], "renumbered");
        e.remove_track(music).unwrap();
        e.remove_track(v1).unwrap();
        e.add_track(TrackKind::Video, None, None).unwrap();
        assert_eq!(names(&e), ["V1", "V2", "A1"]);
        e.undo().unwrap();
        e.undo().unwrap();
        assert_eq!(ids(&e), [v2, v1, a1], "undo restores v1");
        assert_eq!(names(&e), ["V1", "V2", "A1"]);

        e.set_track_enabled(v2, false).unwrap();
        e.set_frame_size(Some((1281, 720))).unwrap();
        let c = e.current_sequence_contents().unwrap();
        assert!(!c.tracks[0].enabled);
        assert_eq!(c.frame_size, Some([1280, 720]));
        assert!(e.set_frame_size(Some((0, 720))).is_err());

        let title = e
            .create_title_media("T".into(), 2.0, TitleParams::default())
            .unwrap();
        let media = MediaItemId::from_uuid(title.id.parse().unwrap());
        let clip = e.insert_clip_at(v1, media, 0.0).unwrap();
        let clip = vance_timeline::ClipId::from_uuid(clip.parse().unwrap());
        let blend_of = |e: &Engine| {
            e.current_sequence_contents().unwrap().tracks[1].clips[0]
                .blend_mode
                .clone()
        };
        assert_eq!(blend_of(&e), "normal");
        e.set_clip_blend_mode(v1, clip, vance_timeline::BlendMode::Screen)
            .unwrap();
        assert_eq!(blend_of(&e), "screen");

        // Nothing shown yet: the solo is recorded, the repaint a no-op.
        e.set_matte_view(Some(clip)).unwrap();
        assert_eq!(e.viewer.matte_view, Some(clip));
        e.set_matte_view(None).unwrap();
        assert_eq!(e.viewer.matte_view, None);
    }

    fn track_ids(e: &Engine) -> Vec<TrackId> {
        e.current_sequence_contents()
            .unwrap()
            .tracks
            .iter()
            .map(|t| TrackId::from_uuid(t.id.parse().unwrap()))
            .collect()
    }

    fn title_media(e: &mut Engine, params: TitleParams) -> MediaItemId {
        let info = e.create_title_media("T".into(), 2.0, params).unwrap();
        MediaItemId::from_uuid(info.id.parse().unwrap())
    }

    fn sized_title(width: u32, height: u32) -> TitleParams {
        TitleParams {
            width,
            height,
            ..TitleParams::default()
        }
    }

    /// `Ok(None)` when this machine has no GPU (the headless renderer can't
    /// be created): the caller skips.
    fn gpu_frame(r: Result<Vec<u8>, EngineError>) -> Option<Vec<u8>> {
        match r {
            Ok(frame) => Some(frame),
            Err(EngineError::Project(m)) if m.contains("headless renderer") => {
                eprintln!("no GPU ({m}) — skipping");
                None
            }
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn rename_track_trims_reverts_and_undoes() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let name = |e: &Engine| {
            e.current_sequence_contents().unwrap().tracks[0]
                .name
                .clone()
        };
        e.rename_track(v1, "  Background ".into()).unwrap();
        assert_eq!(name(&e), "Background", "trimmed");
        e.rename_track(v1, String::new()).unwrap();
        assert_eq!(name(&e), "V1", "empty reverts to the positional name");
        assert!(e.undo().unwrap());
        assert_eq!(name(&e), "Background", "undo restores the previous name");
        assert!(e.rename_track(TrackId::new(), "x".into()).is_err());
    }

    #[test]
    fn the_first_clip_sets_the_frame_size_in_its_undo_step() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let ids = track_ids(&e);
        let (v1, a1) = (ids[0], ids[1]);
        let size = |e: &Engine| e.current_sequence_contents().unwrap().frame_size;

        let on_audio = title_media(&mut e, sized_title(800, 600));
        e.insert_clip_at(a1, on_audio, 0.0).unwrap();
        assert_eq!(size(&e), None, "a clip on an audio track leaves it unset");

        let first = title_media(&mut e, sized_title(641, 361));
        e.insert_clip_at(v1, first, 0.0).unwrap();
        assert_eq!(size(&e), Some([640, 360]), "odd edges round down to even");
        let second = title_media(&mut e, sized_title(1920, 1080));
        e.insert_clip_at(v1, second, 5.0).unwrap();
        assert_eq!(size(&e), Some([640, 360]), "only the first clip sets it");

        assert!(e.undo().unwrap());
        assert_eq!(size(&e), Some([640, 360]));
        assert!(e.undo().unwrap());
        let c = e.current_sequence_contents().unwrap();
        assert_eq!(c.frame_size, None, "one undo removes the clip and the size");
        assert!(c.tracks[0].clips.is_empty());
        assert!(e.redo().unwrap());
        assert_eq!(size(&e), Some([640, 360]), "redo brings both back");

        // Once set (here by the user), later clips never change it.
        e.set_frame_size(Some((1280, 720))).unwrap();
        e.insert_clip_at(v1, second, 9.0).unwrap();
        assert_eq!(size(&e), Some([1280, 720]));
    }

    #[test]
    fn set_matte_view_solos_the_keyer_matte_in_preview_only() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let red = TitleParams {
            text: String::new(),
            bg: [1.0, 0.0, 0.0],
            width: 64,
            height: 32,
            bg_alpha: 1.0,
            ..TitleParams::default()
        };
        let media = title_media(&mut e, red);
        let clip = e.insert_clip_at(v1, media, 0.0).unwrap();
        let clip = vance_timeline::ClipId::from_uuid(clip.parse().unwrap());
        // The default keyer keys green, so a red card survives the key.
        e.add_effect_to_clip(
            v1,
            clip,
            vance_timeline::keyer::KEYER_KIND.into(),
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        let Some(plain) = gpu_frame(e.preview_frame_rgba(1.0, 64, 32)) else {
            return;
        };
        let centre = |b: &[u8]| -> [u8; 3] {
            let i = (16 * 64 + 32) * 4;
            [b[i], b[i + 1], b[i + 2]]
        };
        let [r, g, b] = centre(&plain);
        assert!(r > 250 && g < 5 && b < 5, "keyed picture: {:?}", [r, g, b]);

        e.set_matte_view(Some(clip)).unwrap();
        let matte = e.preview_frame_rgba(1.0, 64, 32).unwrap();
        let [r, g, b] = centre(&matte);
        assert!(
            r > 250 && g > 250 && b > 250,
            "a kept pixel's matte is white: {:?}",
            [r, g, b]
        );
        let export = e.export_frame_rgba(1.0, 64, 32).unwrap();
        assert_eq!(export, plain, "exports ignore viewer solos");

        e.set_matte_view(None).unwrap();
        assert_eq!(e.preview_frame_rgba(1.0, 64, 32).unwrap(), plain);
    }

    #[test]
    fn export_decodes_the_original_even_with_proxies_on() {
        let fixtures =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../vance-decoder/tests/fixtures");
        let original = fixtures.join("testsrc_320x240_5s.mp4");
        // Any other picture stands in for the proxy.
        let stand_in = fixtures.join("hdr10_320x240_2s.mp4");
        if locate_decoder_binary().is_err() || !original.exists() || !stand_in.exists() {
            eprintln!("decoder sidecar or fixtures missing — skipping");
            return;
        }
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let info = e.import_media_file(&original.to_string_lossy()).unwrap();
        let media = MediaItemId::from_uuid(info.id.parse().unwrap());
        let v1 = track_ids(&e)[0];
        e.insert_clip_at(v1, media, 0.0).unwrap();
        assert_eq!(
            e.current_sequence_contents().unwrap().frame_size,
            Some([320, 240]),
            "a file's size comes from its stream"
        );
        let Some(reference) = gpu_frame(e.export_frame_rgba(0.5, 64, 48)) else {
            return;
        };
        assert_eq!(e.preview_frame_rgba(0.5, 64, 48).unwrap(), reference);

        e.set_media_proxy_path(media, Some(stand_in)).unwrap();
        e.set_use_proxies(true).unwrap();
        let preview = e.preview_frame_rgba(0.5, 64, 48).unwrap();
        assert_ne!(preview, reference, "preview honours the proxy toggle");
        assert_eq!(
            e.export_frame_rgba(0.5, 64, 48).unwrap(),
            reference,
            "an export always decodes the original"
        );
    }

    fn card(rgb: [f32; 3]) -> TitleParams {
        TitleParams {
            text: String::new(),
            bg: rgb,
            fg: rgb,
            width: 64,
            height: 32,
            bg_alpha: 1.0,
            ..TitleParams::default()
        }
    }

    /// An engine showing `t` in the unit tests' headless viewer, or `None`
    /// (skip) without a GPU.
    fn with_test_viewer(mut e: Engine, t: f64) -> Option<Engine> {
        match e.ensure_render_state() {
            Ok(_) => {}
            Err(EngineError::Project(m)) if m.contains("headless renderer") => {
                eprintln!("no GPU ({m}) — skipping");
                return None;
            }
            Err(e) => panic!("{e}"),
        }
        e.test_viewer = Some(TestViewer {
            size: (64, 32),
            ..TestViewer::default()
        });
        e.preview_t = Some(t);
        Some(e)
    }

    fn viewer_centre(e: &Engine) -> [u8; 3] {
        let frame = e
            .test_viewer
            .as_ref()
            .and_then(|v| v.frame.as_ref())
            .expect("a repainted frame");
        let i = (16 * 64 + 32) * 4;
        [frame[i], frame[i + 1], frame[i + 2]]
    }

    fn repaints(e: &Engine) -> usize {
        e.test_viewer.as_ref().map_or(0, |v| v.repaints)
    }

    #[test]
    fn edits_undo_and_redo_repaint_the_paused_viewer() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let v2 = e.add_track(TrackKind::Video, None, None).unwrap();
        let red = title_media(&mut e, card([1.0, 0.0, 0.0]));
        let blue = title_media(&mut e, card([0.0, 0.0, 1.0]));
        e.insert_clip_at(v1, red, 0.0).unwrap();
        let top = e.insert_clip_at(v2, blue, 0.0).unwrap();
        let top = vance_timeline::ClipId::from_uuid(top.parse().unwrap());
        let Some(mut e) = with_test_viewer(e, 1.0) else {
            return;
        };
        let is = |c: [u8; 3], want: [u8; 3]| c.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 2);
        // Edits a frame apart each repaint at once (see the coalescing test).
        let apart = |e: &Engine| std::thread::sleep(e.repaint_interval());

        e.set_track_enabled(v2, false).unwrap();
        assert_eq!(repaints(&e), 1, "the edit repainted the viewer");
        assert!(
            is(viewer_centre(&e), [255, 0, 0]),
            "V2 hidden: {:?}",
            viewer_centre(&e)
        );
        apart(&e);
        assert!(e.undo().unwrap());
        assert_eq!(repaints(&e), 2, "undo repainted");
        assert!(
            is(viewer_centre(&e), [0, 0, 255]),
            "V2 back: {:?}",
            viewer_centre(&e)
        );
        apart(&e);
        assert!(e.redo().unwrap());
        assert_eq!(repaints(&e), 3, "redo repainted");
        assert!(is(viewer_centre(&e), [255, 0, 0]));
        apart(&e);
        assert!(e.undo().unwrap());

        apart(&e);
        e.set_clip_blend_mode(v2, top, vance_timeline::BlendMode::Lighten)
            .unwrap();
        // Lighten(red, blue) = magenta.
        assert!(
            is(viewer_centre(&e), [255, 0, 255]),
            "{:?}",
            viewer_centre(&e)
        );
        let before = repaints(&e);
        // An edit that fails repaints nothing.
        assert!(e.set_track_enabled(TrackId::new(), false).is_err());
        assert_eq!(repaints(&e), before);
    }

    #[test]
    fn only_picture_edits_repaint_and_rapid_ones_coalesce() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let ids = track_ids(&e);
        let (v1, a1) = (ids[0], ids[1]);
        let red = title_media(&mut e, card([1.0, 0.0, 0.0]));
        e.insert_clip_at(v1, red, 0.0).unwrap();
        let Some(mut e) = with_test_viewer(e, 1.0) else {
            return;
        };
        // Mixer, routing and naming edits can't change the picture.
        e.set_master_gain_db(-6.0).unwrap();
        e.set_track_pan(a1, 0.5).unwrap();
        e.add_bus("FX".into()).unwrap();
        e.rename_track(v1, "Plate".into()).unwrap();
        assert_eq!(repaints(&e), 0, "no repaint for edits outside the picture");

        e.set_track_enabled(v1, false).unwrap();
        assert_eq!(repaints(&e), 1, "a picture edit repaints at once");
        // A slider's next event, inside one frame interval: coalesced. (The
        // paint time is pinned, so a slow machine can't miss the interval.)
        e.preview_painted_at = Some(Instant::now());
        e.set_track_enabled(v1, true).unwrap();
        assert_eq!(repaints(&e), 1, "the second repaint waits");
        assert_eq!(viewer_centre(&e), [0, 0, 0], "still the hidden frame");
        std::thread::sleep(e.repaint_interval());
        assert!(!e.is_playing(), "the UI's poll");
        assert_eq!(repaints(&e), 2, "the trailing repaint comes with the poll");
        assert_eq!(viewer_centre(&e), [255, 0, 0], "and shows the last edit");
        assert!(!e.is_playing());
        assert_eq!(repaints(&e), 2, "nothing left to repaint");
    }

    #[test]
    fn an_edit_whose_repaint_fails_still_succeeds() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let media = title_media(&mut e, card([1.0, 0.0, 0.0]));
        e.insert_clip_at(v1, media, 0.0).unwrap();
        let Some(mut e) = with_test_viewer(e, 1.0) else {
            return;
        };
        e.test_viewer.as_mut().unwrap().fail = true;
        e.set_track_enabled(v1, false)
            .expect("the edit stands although the repaint failed");
        assert!(!e.current_sequence_contents().unwrap().tracks[0].enabled);
        assert!(e.undo().expect("undo stands too"));
        assert!(e.current_sequence_contents().unwrap().tracks[0].enabled);
    }

    #[test]
    fn a_failed_matte_repaint_leaves_the_solo_unchanged() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let media = title_media(&mut e, card([1.0, 0.0, 0.0]));
        let clip = e.insert_clip_at(v1, media, 0.0).unwrap();
        let clip = vance_timeline::ClipId::from_uuid(clip.parse().unwrap());
        let node = EffectId::new();
        let Some(mut e) = with_test_viewer(e, 1.0) else {
            return;
        };
        e.set_matte_view(Some(clip)).unwrap();
        assert_eq!(e.viewer.matte_view, Some(clip), "a good repaint keeps it");

        // A stale sequence id makes the repaint fail.
        let current = e.current_sequence_id;
        e.current_sequence_id = Some(SequenceId::new());
        assert!(e.set_matte_view(None).is_err());
        assert_eq!(
            e.viewer.matte_view,
            Some(clip),
            "the engine kept the old solo"
        );
        assert!(e.set_matte_node(clip, Some(node)).is_err());
        assert_eq!(e.viewer.matte_node, None, "the engine kept the old node");

        e.current_sequence_id = current;
        e.set_matte_node(clip, Some(node)).unwrap();
        assert_eq!(e.viewer.matte_node, Some(node));
    }

    #[test]
    fn once_per_project_notices_reset_with_the_project() {
        let mut e = Engine::default();
        e.new_project("a".into()).unwrap();
        e.proxy_notice_shown = true;
        e.skip_noticed
            .insert((vance_timeline::ClipId::new(), false));
        e.new_project("b".into()).unwrap();
        assert!(
            !e.proxy_notice_shown,
            "a new project gets the proxy notice again"
        );
        assert!(e.skip_noticed.is_empty());

        e.proxy_notice_shown = true;
        let dir =
            std::env::temp_dir().join(format!("vance-v2-notice-{}.vance", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        e.save_project(Some(dir.to_string_lossy().into_owned()))
            .unwrap();
        e.load_project(&dir.to_string_lossy()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!e.proxy_notice_shown, "so does an opened one");
    }

    #[test]
    fn switching_sequences_clears_the_viewer_solos() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let first = e.current_sequence_id.unwrap();
        let other = Sequence::new("Sequence 02", vance_core::FrameRate::FPS_24);
        let second = other.id;
        e.current_project
            .as_mut()
            .unwrap()
            .sequences
            .push(Arc::new(other));
        let clip = vance_timeline::ClipId::new();
        let node = EffectId::new();
        e.viewer = ViewerState {
            matte_node: Some(node),
            matte_view: Some(clip),
        };
        e.set_current_sequence(first).unwrap();
        assert_eq!(e.viewer.matte_view, Some(clip), "re-selecting keeps them");
        e.set_current_sequence(second).unwrap();
        assert_eq!(e.viewer.matte_view, None);
        assert_eq!(e.viewer.matte_node, None);
        e.viewer.matte_view = Some(clip);
        assert!(e.set_current_sequence(SequenceId::new()).is_err());
        assert_eq!(
            e.viewer.matte_view,
            Some(clip),
            "a failed switch keeps them"
        );
        e.set_current_sequence(first).unwrap();
        assert_eq!(e.viewer.matte_view, None, "and so does switching back");
    }

    #[test]
    fn sequence_contents_carry_each_tracks_bus_routing() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let ids = track_ids(&e);
        let (v1, a1) = (ids[0], ids[1]);
        let bus: vance_timeline::BusId =
            vance_timeline::BusId::from_uuid(e.add_bus("FX".into()).unwrap().parse().unwrap());
        e.route_track_to_bus(a1, vance_timeline::BusOutput::Bus(bus))
            .unwrap();
        e.add_track_send(
            a1,
            vance_timeline::Send {
                target: bus,
                level_db: -6.0,
                pre_fader: true,
            },
        )
        .unwrap();
        let json = serde_json::to_value(e.current_sequence_contents().unwrap()).unwrap();
        let track = |id: TrackId| {
            json["tracks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["id"] == id.to_string())
                .unwrap()
                .clone()
        };
        // The shapes `TrackContents.output` / `.sends` declare in the UI's
        // ipc/index.ts: `'Master' | { Bus: string }` and `Send[]`.
        assert_eq!(track(v1)["output"], serde_json::json!("Master"));
        assert_eq!(track(v1)["sends"], serde_json::json!([]));
        assert_eq!(
            track(a1)["output"],
            serde_json::json!({ "Bus": bus.to_string() })
        );
        assert_eq!(
            track(a1)["sends"],
            serde_json::json!([{ "target": bus.to_string(), "level_db": -6.0, "pre_fader": true }])
        );
    }

    #[test]
    fn the_ui_gets_transitions_as_they_render() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let a = title_media(&mut e, card([1.0; 3]));
        let first = e.insert_clip_at(v1, a, 0.0).unwrap();
        let first = vance_timeline::ClipId::from_uuid(first.parse().unwrap());
        let cut =
            rt_to_seconds(e.current_sequence_ref().unwrap().tracks[0].clips[0].timeline_out());
        e.insert_clip_at(v1, a, cut).unwrap();
        e.add_transition_at_cut(v1, first, vance_timeline::TransitionKind::Dissolve, 0.5)
            .unwrap();
        // Saved by an older build: stored 10 s long between two 2 s clips,
        // plus an orphan whose clips no longer meet.
        let orphan = vance_timeline::TransitionId::new();
        {
            let project = e.current_project.as_mut().unwrap();
            let seq = Arc::make_mut(&mut project.sequences[0]);
            let track = Arc::make_mut(&mut seq.tracks[0]);
            Arc::make_mut(&mut track.transitions[0]).duration = seconds_to_rt(10.0);
            let mut lone = (*track.transitions[0]).clone();
            lone.id = orphan;
            lone.position = seconds_to_rt(cut + 30.0);
            track.transitions.push(Arc::new(lone));
        }
        let seq = e.current_sequence_ref().unwrap().clone();
        let (win_in, win_out) =
            vance_timeline::transition_window(&seq.tracks[0], &seq.tracks[0].transitions[0])
                .expect("the dissolve renders");
        let contents = e.current_sequence_contents().unwrap();
        let trs = &contents.tracks[0].transitions;
        assert_eq!(trs.len(), 1, "the orphan is left out: {trs:?}");
        assert!(
            (trs[0].position_seconds - cut).abs() < 1e-9,
            "centred on the cut"
        );
        assert!(
            (trs[0].duration_seconds - rt_to_seconds(win_out) + rt_to_seconds(win_in)).abs() < 1e-9
                && trs[0].duration_seconds < 10.0,
            "the rendered (capped) length, not the stored 10 s: {}",
            trs[0].duration_seconds
        );
    }

    #[test]
    fn a_refused_edit_reads_as_the_timeline_says() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let a = title_media(&mut e, card([1.0; 3]));
        let first = e.insert_clip_at(v1, a, 0.0).unwrap();
        let first = vance_timeline::ClipId::from_uuid(first.parse().unwrap());
        // A gap after the clip: nothing starts at its cut.
        e.insert_clip_at(v1, a, 9.0).unwrap();
        let err = e
            .add_transition_at_cut(v1, first, vance_timeline::TransitionKind::Dissolve, 0.5)
            .unwrap_err();
        assert!(matches!(err, EngineError::EditRefused(_)), "{err:?}");
        assert_eq!(
            err.to_string(),
            "no clip starts at the cut — a transition needs two clips that meet"
        );
    }

    #[test]
    fn an_unknown_add_on_exports_with_one_notice() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let media = title_media(&mut e, card([1.0, 0.0, 0.0]));
        let clip = e.insert_clip_at(v1, media, 0.0).unwrap();
        let clip = vance_timeline::ClipId::from_uuid(clip.parse().unwrap());
        e.add_effect_to_clip(
            v1,
            clip,
            "wasm:com.nonexistent.plugin".into(),
            std::collections::BTreeMap::new(),
        )
        .unwrap();
        let _ = e.take_notices();
        let Some(frame) = gpu_frame(e.export_frame_rgba(1.0, 64, 32)) else {
            return;
        };
        assert_eq!(
            &frame[(16 * 64 + 32) * 4..][..3],
            [255, 0, 0],
            "drawn without it"
        );
        e.export_frame_rgba(1.5, 64, 32)
            .expect("still exports the next frame");
        let notices = e.take_notices();
        assert_eq!(notices.len(), 1, "one notice for the clip: {notices:?}");
        assert!(
            notices[0].starts_with("An add-on didn't run on clip \"T\": "),
            "{notices:?}"
        );
        assert!(notices[0].contains("com.nonexistent.plugin"), "{notices:?}");
    }

    #[test]
    fn a_broken_proxy_is_regenerated() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4");
        let bins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
        let exe = |n: &str| bins.join(format!("{n}{}", std::env::consts::EXE_SUFFIX));
        let (dec_bin, enc_bin) = (exe("vance-decoder"), exe("vance-encoder"));
        if !dec_bin.exists() || !enc_bin.exists() || !fixture.exists() {
            eprintln!("sidecars or fixture missing — skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("vance-v2-proxies-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // It exists, so only the pool's "broken" report can get it replaced.
        let broken = dir.join("broken.proxy.mov");
        std::fs::write(&broken, b"not a movie").unwrap();

        // The encoder locator looks from the working directory, which unit
        // tests don't change: the worker gets the workspace's sidecars.
        let mut e = Engine {
            test_proxy_dir: Some(dir.clone()),
            proxy_worker: Some(crate::proxy::ProxyWorker::spawn(dec_bin, enc_bin)),
            ..Default::default()
        };
        e.new_project("t".into()).unwrap();
        let info = e.import_media_file(&fixture.to_string_lossy()).unwrap();
        let media = MediaItemId::from_uuid(info.id.parse().unwrap());
        e.insert_clip_at(track_ids(&e)[0], media, 0.0).unwrap();
        e.set_media_proxy_path(media, Some(broken)).unwrap();
        e.set_use_proxies(true).unwrap();
        let _ = e.take_notices();
        let shown = gpu_frame(e.preview_frame_rgba(0.5, 64, 48));
        let queued: Vec<String> = e.auto_proxies.values().cloned().collect();
        let notices = e.take_notices();
        drop(e); // waits for the proxy job
        let _ = std::fs::remove_dir_all(&dir);
        if shown.is_none() {
            return;
        }
        assert_eq!(
            queued,
            [fixture.to_string_lossy().into_owned()],
            "a new proxy of the original was queued"
        );
        assert!(
            notices.iter().any(|n| n.starts_with("The proxy of ")),
            "{notices:?}"
        );
    }

    /// An engine that saved, then reopened, a project whose one clip (a
    /// copy of a fixture) has a proxy (a stand-in picture; `current` marks it
    /// as this build's format). With `offline` the clip's original is moved
    /// to `away` before the reopen. Its auto-proxies go to a temp folder
    /// through the workspace's sidecars. `None` (skip) without the sidecars
    /// or fixtures.
    struct ReopenedWithProxy {
        e: Engine,
        media: MediaItemId,
        original: PathBuf,
        away: PathBuf,
        proxy: PathBuf,
        dir: PathBuf,
    }

    impl Drop for ReopenedWithProxy {
        fn drop(&mut self) {
            // Engine first: it waits for a queued proxy job.
            self.e = Engine::default();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum ProxyState {
        /// Made before markers existed.
        Old,
        /// `Old`, but the project names it `<auto dir>/../clip.proxy.mov` (a
        /// crafted bundle).
        OldViaTraversal,
        /// `Old`, made by the engine itself: `<auto dir>/<id>.proxy.mov`.
        OldGenerated,
        /// An older format's marker that records DNxHR LB.
        OldDnxhr,
        /// This build's format.
        Current,
    }

    fn reopened_with_proxy(
        tag: &str,
        state: ProxyState,
        offline: bool,
    ) -> Option<ReopenedWithProxy> {
        let fixtures =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../vance-decoder/tests/fixtures");
        let fixture = fixtures.join("testsrc_320x240_5s.mp4");
        let stand_in = fixtures.join("hdr10_320x240_2s.mp4");
        let bins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
        let exe = |n: &str| bins.join(format!("{n}{}", std::env::consts::EXE_SUFFIX));
        let (dec_bin, enc_bin) = (exe("vance-decoder"), exe("vance-encoder"));
        if !dec_bin.exists() || !enc_bin.exists() || !fixture.exists() || !stand_in.exists() {
            eprintln!("sidecars or fixtures missing — skipping");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("vance-v2-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (original, away) = (dir.join("original.mp4"), dir.join("original.away"));
        std::fs::copy(&fixture, &original).unwrap();
        let proxy = dir.join("clip.proxy.mov");
        std::fs::copy(&stand_in, &proxy).unwrap();
        match state {
            ProxyState::Old | ProxyState::OldViaTraversal | ProxyState::OldGenerated => {}
            ProxyState::OldDnxhr => {
                let len = std::fs::metadata(&proxy).unwrap().len();
                let mut marker = proxy.clone().into_os_string();
                marker.push(".vance-proxy");
                let text = format!("vance-proxy-format=2 codec=dnxhr_lb bytes={len}\n");
                std::fs::write(marker, text).unwrap();
            }
            ProxyState::Current => {
                crate::proxy::mark_current(&proxy, test_proxy_source((320, 240))).unwrap();
            }
        }
        let mut e = Engine {
            test_proxy_dir: Some(dir.join("auto")),
            proxy_worker: Some(crate::proxy::ProxyWorker::spawn(dec_bin, enc_bin)),
            ..Default::default()
        };
        e.new_project("t".into()).unwrap();
        let info = e.import_media_file(&original.to_string_lossy()).unwrap();
        let media = MediaItemId::from_uuid(info.id.parse().unwrap());
        e.insert_clip_at(track_ids(&e)[0], media, 0.0).unwrap();
        let auto = dir.join("auto");
        let proxy = match state {
            ProxyState::OldViaTraversal => {
                std::fs::create_dir_all(&auto).unwrap();
                auto.join("..").join("clip.proxy.mov")
            }
            ProxyState::OldGenerated => {
                std::fs::create_dir_all(&auto).unwrap();
                let made = auto.join(format!("{media}.proxy.mov"));
                std::fs::copy(&proxy, &made).unwrap();
                made
            }
            _ => proxy,
        };
        e.set_media_proxy_path(media, Some(proxy.clone())).unwrap();
        e.set_use_proxies(true).unwrap();
        let bundle = dir.join("project.vance");
        e.save_project(Some(bundle.to_string_lossy().into_owned()))
            .unwrap();
        let _ = e.take_notices();
        if offline {
            std::fs::rename(&original, &away).unwrap();
        }
        e.load_project(&bundle.to_string_lossy()).unwrap();
        Some(ReopenedWithProxy {
            e,
            media,
            original,
            away,
            proxy,
            dir,
        })
    }

    /// A ProRes proxy of an untagged (Rec.709 limited) original of `size`.
    fn test_proxy_source(size: (u32, u32)) -> crate::proxy::ProxySource {
        crate::proxy::ProxySource {
            codec: crate::proxy::ProxyCodec::ProRes422,
            size,
            sar: (1, 1),
            input_space: vance_color::ColorSpace::Rec709Limited,
        }
    }

    fn proxy_of(e: &Engine, media: MediaItemId) -> Option<PathBuf> {
        e.current_project
            .as_ref()?
            .media
            .get(&media)?
            .proxy_path
            .clone()
    }

    #[test]
    fn an_old_format_proxy_is_not_used_and_is_remade() {
        let Some(mut r) = reopened_with_proxy("old-proxy", ProxyState::Old, false) else {
            return;
        };
        assert_eq!(proxy_of(&r.e, r.media), None, "the old proxy is dropped");
        let queued: Vec<String> = r.e.auto_proxies.values().cloned().collect();
        assert_eq!(
            queued,
            [r.original.to_string_lossy().into_owned()],
            "and made again"
        );
        let notices = r.e.take_notices();
        assert!(
            notices
                .iter()
                .any(|n| n.starts_with("Updating 1 proxy made by an older version")),
            "{notices:?}"
        );
        let Some(export) = gpu_frame(r.e.export_frame_rgba(0.5, 64, 48)) else {
            return;
        };
        assert_eq!(
            r.e.preview_frame_rgba(0.5, 64, 48).unwrap(),
            export,
            "preview shows the original meanwhile"
        );
    }

    #[test]
    fn an_old_proxy_of_an_offline_original_waits_for_the_original() {
        let Some(mut r) = reopened_with_proxy("offline-original", ProxyState::Old, true) else {
            return;
        };
        assert_eq!(
            proxy_of(&r.e, r.media),
            Some(r.proxy.clone()),
            "kept: with the original offline it's the clip's only picture"
        );
        assert!(r.e.auto_proxies.is_empty(), "nothing to remake from yet");
        let notices = r.e.take_notices();
        assert!(
            !notices.iter().any(|n| n.starts_with("Updating")),
            "{notices:?}"
        );
        if let Some(frame) = gpu_frame(r.e.preview_frame_rgba(0.5, 64, 48)) {
            assert!(
                frame
                    .chunks_exact(4)
                    .any(|p| p[..3].iter().any(|&c| c > 30)),
                "preview shows the old proxy's picture"
            );
            let notices = r.e.take_notices();
            assert!(
                !notices.iter().any(|n| n.starts_with("Couldn't show")),
                "{notices:?}"
            );
        }

        // The drive is back; the next check (on preview or play) remakes it.
        std::fs::rename(&r.away, &r.original).unwrap();
        r.e.originals_checked_at = None;
        match gpu_frame(r.e.preview_frame_rgba(0.5, 64, 48)) {
            Some(_) => {}
            None => r.e.recheck_offline_originals(),
        }
        assert_eq!(
            proxy_of(&r.e, r.media),
            None,
            "dropped once it can be remade"
        );
        let queued: Vec<String> = r.e.auto_proxies.values().cloned().collect();
        assert_eq!(queued, [r.original.to_string_lossy().into_owned()]);
        let notices = r.e.take_notices();
        assert!(
            notices
                .iter()
                .any(|n| n.starts_with("Updating 1 proxy made by an older version")),
            "{notices:?}"
        );
    }

    #[test]
    fn a_remade_proxy_keeps_its_codec() {
        let Some(r) = reopened_with_proxy("codec", ProxyState::OldDnxhr, false) else {
            return;
        };
        let id = format!("auto-proxy-{}", r.media);
        let deadline = Instant::now() + Duration::from_secs(120);
        let output = loop {
            let job = r.e.proxy_jobs_snapshot().into_iter().find(|j| j.id == id);
            match job.map(|j| (j.status, j.output_path)) {
                Some((crate::proxy::ProxyJobStatus::Done { .. }, out)) => break PathBuf::from(out),
                Some((crate::proxy::ProxyJobStatus::Error(e), _)) => panic!("remake failed: {e}"),
                _ if Instant::now() > deadline => panic!("remake timed out"),
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        };
        let marker = crate::proxy::current_marker(&output).expect("a current marker");
        assert_eq!(
            marker.codec,
            crate::proxy::ProxyCodec::DnxhrLb,
            "made as before"
        );
        assert_eq!(
            marker.original_size,
            Some((320, 240)),
            "the original's size"
        );
    }

    #[test]
    fn a_proxy_that_cant_be_remade_is_kept_and_the_notice_says_so() {
        let Some(mut r) = reopened_with_proxy("unqueued", ProxyState::Old, true) else {
            return;
        };
        // The original is back, but the proxy folder can't be made.
        std::fs::rename(&r.away, &r.original).unwrap();
        let blocker = r.dir.join("not-a-folder");
        std::fs::write(&blocker, b"").unwrap();
        r.e.test_proxy_dir = Some(blocker.join("proxies"));
        r.e.originals_checked_at = None;
        let _ = r.e.take_notices();
        r.e.recheck_offline_originals();
        assert!(r.e.auto_proxies.is_empty());
        assert_eq!(
            proxy_of(&r.e, r.media),
            Some(r.proxy.clone()),
            "the old proxy stays: the clip keeps a picture"
        );
        let notices = r.e.take_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            notices[0].starts_with(
                "1 proxy made by an older version of Vance couldn't be remade (the proxy \
                 folder can't be created:"
            ) && notices[0].ends_with("the clips keep previewing from them."),
            "{notices:?}"
        );
        assert!(!notices[0].contains("Updating"));
    }

    /// Waits for `r`'s remake to finish and attaches it.
    fn remade(r: &mut ReopenedWithProxy) -> PathBuf {
        let id = format!("auto-proxy-{}", r.media);
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let job = r.e.proxy_jobs_snapshot().into_iter().find(|j| j.id == id);
            match job.map(|j| j.status) {
                Some(crate::proxy::ProxyJobStatus::Done { .. }) => break,
                Some(crate::proxy::ProxyJobStatus::Error(e)) => panic!("remake failed: {e}"),
                _ if Instant::now() > deadline => panic!("remake timed out"),
                _ => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        r.e.poll_auto_proxies();
        proxy_of(&r.e, r.media).expect("the new proxy is attached")
    }

    #[test]
    fn a_crafted_proxy_path_is_never_deleted_by_a_remake() {
        let Some(mut r) = reopened_with_proxy("traversal", ProxyState::OldViaTraversal, false)
        else {
            return;
        };
        let target = r.dir.join("clip.proxy.mov");
        let new = remade(&mut r);
        assert_ne!(new, r.proxy);
        assert!(
            target.exists(),
            "the file the bundle pointed at is still there"
        );
    }

    #[test]
    fn a_replaced_auto_proxy_is_deleted_once_remade() {
        let Some(mut r) = reopened_with_proxy("replaced", ProxyState::OldGenerated, false) else {
            return;
        };
        let new = remade(&mut r);
        assert!(new.exists() && new != r.proxy);
        assert!(
            !r.proxy.exists(),
            "the engine's own old proxy is cleaned up"
        );
    }

    #[test]
    fn a_failed_remake_puts_the_old_proxy_back() {
        let Some(mut r) = reopened_with_proxy("failed-remake", ProxyState::Old, true) else {
            return;
        };
        // The original is back, but it isn't a movie: the remake fails.
        std::fs::write(&r.original, b"not a movie").unwrap();
        r.e.originals_checked_at = None;
        let _ = r.e.take_notices();
        r.e.recheck_offline_originals();
        assert_eq!(
            proxy_of(&r.e, r.media),
            None,
            "previewing the original meanwhile"
        );
        let id = format!("auto-proxy-{}", r.media);
        let deadline = Instant::now() + Duration::from_secs(60);
        while !r
            .e
            .proxy_jobs_snapshot()
            .iter()
            .any(|j| j.id == id && matches!(j.status, crate::proxy::ProxyJobStatus::Error(_)))
        {
            assert!(Instant::now() < deadline, "the remake didn't fail");
            std::thread::sleep(Duration::from_millis(20));
        }
        r.e.poll_auto_proxies();
        assert_eq!(
            proxy_of(&r.e, r.media),
            Some(r.proxy.clone()),
            "the old proxy is back"
        );
        assert!(r.proxy.exists(), "and was never overwritten");
        let notices = r.e.take_notices();
        assert!(
            notices
                .iter()
                .any(|n| n.starts_with("Couldn't make the proxy of")),
            "{notices:?}"
        );
    }

    #[test]
    fn a_new_colour_space_remakes_the_proxy() {
        let Some(mut r) = reopened_with_proxy("retagged", ProxyState::Current, false) else {
            return;
        };
        assert_eq!(
            proxy_of(&r.e, r.media),
            Some(r.proxy.clone()),
            "made for Rec.709 limited"
        );
        let _ = r.e.take_notices();
        r.e.set_media_input_color_space(r.media, Some("rec709-full"))
            .unwrap();
        assert_eq!(
            proxy_of(&r.e, r.media),
            None,
            "its colours were for the old tag"
        );
        let queued: Vec<String> = r.e.auto_proxies.values().cloned().collect();
        assert_eq!(queued, [r.original.to_string_lossy().into_owned()]);
        let notices = r.e.take_notices();
        assert!(
            notices
                .iter()
                .any(|n| n.starts_with("Remaking the proxy of") && n.contains("colour space")),
            "{notices:?}"
        );
        // Re-tagging to what the proxy was made for needs nothing.
        let Some(mut same) = reopened_with_proxy("same-tag", ProxyState::Current, false) else {
            return;
        };
        same.e
            .set_media_input_color_space(same.media, Some("rec709-limited"))
            .unwrap();
        assert_eq!(proxy_of(&same.e, same.media), Some(same.proxy.clone()));
        assert!(same.e.auto_proxies.is_empty());
    }

    #[test]
    fn marker_reads_are_once_per_media_and_misses_are_remembered() {
        let dir = std::env::temp_dir().join(format!("vance-v2-marker-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A proxy with no marker (another app's): nothing to read a size from.
        let proxy = dir.join("theirs.mov");
        std::fs::write(&proxy, b"a proxy").unwrap();
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let mut item = MediaItem::new(
            PathBuf::from("/nonexistent/theirs.mov"),
            "theirs",
            MediaKind::Video,
            vance_core::RationalTime::new(2, 1).unwrap(),
        );
        item.proxy_path = Some(proxy);
        let media = item.id;
        e.current_project
            .as_mut()
            .unwrap()
            .media
            .insert(media, Arc::new(item));
        let v1 = track_ids(&e)[0];
        for at in [10.0, 20.0, 30.0] {
            e.insert_clip_at(v1, media, at).unwrap();
        }
        for _ in 0..2 {
            let project = e.current_project.clone().unwrap();
            let seq = e.current_sequence_ref().unwrap().clone();
            let (provider, res) = e.prepare_playback(&project, &seq, 0.0);
            e.scrub_pool = Some(provider.into_pool());
            e.compose_res = Some(res);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(e.marker_reads, 1, "three clips, two plays: one read");
    }

    #[test]
    fn muting_a_clip_repaints_the_viewer() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let red = title_media(&mut e, card([1.0, 0.0, 0.0]));
        let clip = e.insert_clip_at(v1, red, 0.0).unwrap();
        let clip = vance_timeline::ClipId::from_uuid(clip.parse().unwrap());
        let Some(mut e) = with_test_viewer(e, 1.0) else {
            return;
        };
        e.set_clip_mute(v1, clip, true).unwrap();
        assert_eq!(repaints(&e), 1, "a muted clip is a disabled clip");
        assert_eq!(viewer_centre(&e), [0, 0, 0]);
    }

    #[test]
    fn the_coalescing_interval_is_at_most_a_24th_of_a_second() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let one_fps = vance_core::FrameRate(vance_core::Rational::new(1, 1).unwrap());
        let project = e.current_project.as_mut().unwrap();
        Arc::make_mut(&mut project.sequences[0]).frame_rate = one_fps;
        assert_eq!(e.repaint_interval(), Duration::from_secs_f64(1.0 / 24.0));
        let project = e.current_project.as_mut().unwrap();
        Arc::make_mut(&mut project.sequences[0]).frame_rate = vance_core::FrameRate::FPS_60;
        assert_eq!(e.repaint_interval(), Duration::from_secs_f64(1.0 / 60.0));
    }

    #[test]
    fn playback_gets_proxied_media_sizes_from_the_marker() {
        let dir = std::env::temp_dir().join(format!("vance-v2-marker-dims-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let proxy = dir.join("late.proxy.mov");
        std::fs::write(&proxy, b"a proxy").unwrap();
        crate::proxy::mark_current(&proxy, test_proxy_source((1280, 720))).unwrap();

        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        // The original is offline: neither the pool nor a probe could say.
        let mut item = MediaItem::new(
            PathBuf::from("/nonexistent/late.mov"),
            "late",
            MediaKind::Video,
            vance_core::RationalTime::new(2, 1).unwrap(),
        );
        item.proxy_path = Some(proxy);
        let media = item.id;
        e.current_project
            .as_mut()
            .unwrap()
            .media
            .insert(media, Arc::new(item));
        e.set_use_proxies(true).unwrap();
        // Not in the first frame playback composes.
        e.insert_clip_at(track_ids(&e)[0], media, 10.0).unwrap();
        let project = e.current_project.clone().unwrap();
        let seq = e.current_sequence_ref().unwrap().clone();
        let (provider, res) = e.prepare_playback(&project, &seq, 0.0);
        let handed = res.media_dims(media);
        e.scrub_pool = Some(provider.into_pool());
        e.compose_res = Some(res);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            handed,
            Some((1280, 720)),
            "the session scales the keyer by the original's size, as scrub does"
        );
    }

    #[test]
    fn a_current_proxy_is_kept_and_used() {
        let Some(mut r) = reopened_with_proxy("current-proxy", ProxyState::Current, false) else {
            return;
        };
        assert_eq!(proxy_of(&r.e, r.media), Some(r.proxy.clone()));
        assert!(r.e.auto_proxies.is_empty(), "nothing to remake");
        assert!(r.e.take_notices().is_empty());
        let Some(export) = gpu_frame(r.e.export_frame_rgba(0.5, 64, 48)) else {
            return;
        };
        assert_ne!(
            r.e.preview_frame_rgba(0.5, 64, 48).unwrap(),
            export,
            "preview shows the proxy (another picture here)"
        );
    }

    #[test]
    fn a_project_that_wont_open_leaves_the_loaded_one_as_it_was() {
        let mut e = Engine::default();
        e.new_project("loaded".into()).unwrap();
        let clip = vance_timeline::ClipId::new();
        e.preview_t = Some(3.0);
        e.viewer.matte_view = Some(clip);
        let media = MediaItemId::new();
        e.media_dims.insert(media, (1920, 1080));
        e.notices.push("about the loaded project".into());
        let missing = std::env::temp_dir().join("vance-v2-no-such-project.vance");
        assert!(e.load_project(&missing.to_string_lossy()).is_err());
        assert_eq!(e.project_info().unwrap().name, "loaded");
        assert_eq!(e.preview_t, Some(3.0), "playhead kept");
        assert_eq!(e.viewer.matte_view, Some(clip), "solo kept");
        assert_eq!(e.media_dims.get(&media), Some(&(1920, 1080)), "probes kept");
        assert_eq!(e.take_notices(), ["about the loaded project"]);

        // A switch that works drops the old project's pending notices.
        e.notices.push("stale".into());
        e.new_project("next".into()).unwrap();
        assert!(e.take_notices().is_empty());
    }

    #[test]
    fn insert_and_reopen_agree_on_the_first_measurable_clip() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let v2 = e.add_track(TrackKind::Video, None, None).unwrap();
        let size = |e: &Engine| e.current_sequence_contents().unwrap().frame_size;
        let add = |e: &mut Engine, kind: MediaKind, path: &str| {
            let item = MediaItem::new(
                PathBuf::from(path),
                "x",
                kind,
                vance_core::RationalTime::new(4, 1).unwrap(),
            );
            let id = item.id;
            e.current_project
                .as_mut()
                .unwrap()
                .media
                .insert(id, Arc::new(item));
            id
        };
        // First on V1: a WAV, and a video file that's offline. Neither has a
        // size.
        let wav = add(&mut e, MediaKind::Audio, "/nonexistent/music.wav");
        let offline = add(&mut e, MediaKind::Video, "/nonexistent/offline.mov");
        e.insert_clip_at(v1, wav, 0.0).unwrap();
        e.insert_clip_at(v1, offline, 5.0).unwrap();
        assert_eq!(size(&e), None);
        // The first clip with a size sets it, though it isn't the first clip.
        let uhd = title_media(&mut e, sized_title(3840, 2160));
        e.insert_clip_at(v2, uhd, 2.0).unwrap();
        assert_eq!(size(&e), Some([3840, 2160]));

        let dir =
            std::env::temp_dir().join(format!("vance-v2-measurable-{}.vance", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        e.save_project(Some(dir.to_string_lossy().into_owned()))
            .unwrap();
        // Saved without a size (an older build), the migration agrees.
        e.set_frame_size(None).unwrap();
        let legacy = dir.with_extension("legacy.vance");
        let _ = std::fs::remove_dir_all(&legacy);
        e.save_project(Some(legacy.to_string_lossy().into_owned()))
            .unwrap();
        let mut reopened = Engine::default();
        reopened.load_project(&dir.to_string_lossy()).unwrap();
        let mut migrated = Engine::default();
        migrated.load_project(&legacy.to_string_lossy()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&legacy);
        assert_eq!(size(&reopened), Some([3840, 2160]), "reopening keeps it");
        assert_eq!(
            size(&migrated),
            Some([3840, 2160]),
            "and the migration picks the same clip"
        );
    }

    /// Two 2 s cards on V1 with a 1 s dissolve at their cut (window
    /// 1.5–2.5 s).
    fn dissolve_rig() -> (Engine, TrackId, [vance_timeline::ClipId; 2]) {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let a = title_media(&mut e, card([1.0, 0.0, 0.0]));
        let b = title_media(&mut e, card([0.0, 0.0, 1.0]));
        let first = e.insert_clip_at(v1, a, 0.0).unwrap();
        let second = e.insert_clip_at(v1, b, 2.0).unwrap();
        let first = vance_timeline::ClipId::from_uuid(first.parse().unwrap());
        let second = vance_timeline::ClipId::from_uuid(second.parse().unwrap());
        e.add_transition_at_cut(v1, first, vance_timeline::TransitionKind::Dissolve, 1.0)
            .unwrap();
        (e, v1, [first, second])
    }

    fn rnr(e: &mut Engine, track: TrackId, (start, end): (f64, f64)) -> Result<String, String> {
        let out = std::env::temp_dir().join(format!(
            "vance-v2-rnr-refused-{}-{start}-{end}.mov",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&out);
        let opts = RenderAndReplaceOptions {
            output_path: out.to_string_lossy().into_owned(),
            width: None,
            height: None,
            track_id: track,
            start_seconds: start,
            end_seconds: end,
        };
        let checked = e.check_render_and_replace(&opts).map(|_| ());
        let result = e.render_and_replace(opts);
        assert!(!out.exists(), "nothing written for a refusal");
        assert!(result.is_err(), "refused (or no encoder in unit tests)");
        checked.map(|()| "ok".into()).map_err(|e| e.to_string())
    }

    #[test]
    fn render_and_replace_refuses_what_wouldnt_look_the_same() {
        let (mut e, v1, [first, second]) = dissolve_rig();
        // The whole dissolve under Normal renders exactly.
        assert_eq!(rnr(&mut e, v1, (0.0, 4.0)), Ok("ok".into()));
        for range in [(0.0, 2.0), (2.0, 4.0), (1.0, 2.2)] {
            let err = rnr(&mut e, v1, range).unwrap_err();
            assert!(
                err.contains("inside a transition") && err.contains("extend the range"),
                "{range:?}: {err}"
            );
        }
        // Edges at the window's ends are fine.
        assert_eq!(rnr(&mut e, v1, (0.0, 1.5)), Ok("ok".into()));
        assert_eq!(rnr(&mut e, v1, (2.5, 4.0)), Ok("ok".into()));

        use vance_timeline::BlendMode as B;
        for mode in B::ALL {
            e.set_clip_blend_mode(v1, first, mode).unwrap();
            e.set_clip_blend_mode(v1, second, mode).unwrap();
            let got = rnr(&mut e, v1, (0.0, 4.0));
            if matches!(mode, B::Normal | B::Multiply | B::Screen | B::Overlay) {
                assert_eq!(got, Ok("ok".into()), "{mode:?} renders a dissolve exactly");
            } else {
                let err = got.unwrap_err();
                assert!(
                    err.contains("change the blend mode or render the clips separately"),
                    "{mode:?}: {err}"
                );
            }
            // Without the dissolve in the range, any mode is exact.
            assert_eq!(rnr(&mut e, v1, (0.0, 1.0)), Ok("ok".into()), "{mode:?}");
        }
    }

    #[test]
    fn render_and_replace_ignores_a_transition_the_plan_doesnt_draw() {
        let (mut e, v1, [first, second]) = dissolve_rig();
        use vance_timeline::BlendMode as B;
        for clip in [first, second] {
            e.set_clip_blend_mode(v1, clip, B::Lighten).unwrap();
        }
        assert!(
            rnr(&mut e, v1, (0.0, 2.0)).is_err(),
            "the dissolve is drawn"
        );
        // A muted (disabled) side: the plan draws no dissolve there.
        e.set_clip_mute(v1, second, true).unwrap();
        assert_eq!(
            rnr(&mut e, v1, (0.0, 2.0)),
            Ok("ok".into()),
            "no edge inside it"
        );
        assert_eq!(
            rnr(&mut e, v1, (0.0, 4.0)),
            Ok("ok".into()),
            "and no dissolve to blend"
        );
    }

    #[test]
    fn render_and_replace_refuses_locked_hidden_and_hdr() {
        let (mut e, v1, _) = dissolve_rig();
        e.set_track_locked(v1, true).unwrap();
        let err = rnr(&mut e, v1, (0.0, 4.0)).unwrap_err();
        assert!(err.contains("locked; unlock it"), "{err}");
        e.set_track_locked(v1, false).unwrap();

        e.set_track_enabled(v1, false).unwrap();
        let err = rnr(&mut e, v1, (0.0, 4.0)).unwrap_err();
        assert!(err.contains("hidden") && err.contains("show it"), "{err}");
        e.set_track_enabled(v1, true).unwrap();

        let mut pq = MediaItem::new(
            PathBuf::from("/nonexistent/hdr.mov"),
            "HDR",
            MediaKind::Video,
            vance_core::RationalTime::new(2, 1).unwrap(),
        );
        pq.input_color_space = Some(vance_color::ColorSpace::Rec2020Pq);
        let pq_id = pq.id;
        e.current_project
            .as_mut()
            .unwrap()
            .media
            .insert(pq_id, Arc::new(pq));
        e.insert_clip_at(v1, pq_id, 6.0).unwrap();
        let err = rnr(&mut e, v1, (5.0, 8.0)).unwrap_err();
        assert!(
            err.starts_with("HDR clips can't be rendered and replaced yet"),
            "{err}"
        );
        assert_eq!(
            rnr(&mut e, v1, (0.0, 4.0)),
            Ok("ok".into()),
            "SDR clips elsewhere"
        );
    }

    #[test]
    fn only_a_generated_auto_proxy_is_ever_deleted() {
        let base =
            std::env::temp_dir().join(format!("vance-v2-delete-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (dir, outside) = (base.join("proxies"), base.join("outside"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let media = MediaItemId::new();
        let other = MediaItemId::new();
        let thesis = outside.join("thesis.docx");
        let write = |p: &Path| std::fs::write(p, b"keep me").unwrap();
        write(&thesis);
        let kept = |p: &Path| p.exists();

        // `..` out of the proxy folder.
        let traversal = dir.join("..").join("outside").join("thesis.docx");
        remove_generated_proxy(&dir, &traversal, media);
        assert!(kept(&thesis), "`..` traversal");
        // An absolute path elsewhere.
        remove_generated_proxy(&dir, &thesis, media);
        assert!(kept(&thesis), "absolute path outside the folder");
        // A symlink in the folder, named like an auto-proxy, pointing out.
        #[cfg(unix)]
        {
            let link = dir.join(format!("{media}.v4.proxy.mov"));
            std::os::unix::fs::symlink(&thesis, &link).unwrap();
            remove_generated_proxy(&dir, &link, media);
            assert!(kept(&thesis), "symlink target");
            assert!(
                std::fs::symlink_metadata(&link).is_ok(),
                "the link is left too"
            );
            std::fs::remove_file(&link).unwrap();
            // A folder link inside the proxy folder: the file "in" it is
            // really outside.
            let sub = dir.join("sub");
            std::os::unix::fs::symlink(&outside, &sub).unwrap();
            let decoy = outside.join(format!("{media}.v4.proxy.mov"));
            write(&decoy);
            remove_generated_proxy(&dir, &sub.join(format!("{media}.v4.proxy.mov")), media);
            assert!(kept(&decoy), "file reached through a linked folder");
        }
        // In the folder, but not a name the worker gives.
        let notes = dir.join("notes.txt");
        write(&notes);
        remove_generated_proxy(&dir, &notes, media);
        assert!(kept(&notes), "unrecognised name");
        let theirs = dir.join(format!("{other}.v4.proxy.mov"));
        write(&theirs);
        remove_generated_proxy(&dir, &theirs, media);
        assert!(kept(&theirs), "another item's proxy");

        // The real thing: the proxy and its marker go.
        for name in [
            format!("{media}.v4.proxy.mov"),
            format!("{media}.proxy.mov"),
        ] {
            let proxy = dir.join(name);
            write(&proxy);
            let marker = marker_path_of(&proxy);
            write(&marker);
            remove_generated_proxy(&dir, &proxy, media);
            assert!(!proxy.exists() && !marker.exists(), "{}", proxy.display());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn media_info_carries_the_display_aspect() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let title = title_media(&mut e, sized_title(800, 600));
        let mut add = |kind: MediaKind| {
            let item = MediaItem::new(
                PathBuf::from("/nonexistent/clip.mov"),
                "clip",
                kind,
                vance_core::RationalTime::new(2, 1).unwrap(),
            );
            let id = item.id;
            e.current_project
                .as_mut()
                .unwrap()
                .media
                .insert(id, Arc::new(item));
            id
        };
        let (anamorphic, unmeasured, audio) = (
            add(MediaKind::Video),
            add(MediaKind::Video),
            add(MediaKind::Audio),
        );
        e.media_dims.insert(anamorphic, (1440, 1080));
        e.media_sar.insert(anamorphic, (4, 3));
        let aspect = |e: &Engine, id| e.media_get_info(id).unwrap().display_aspect;
        assert_eq!(aspect(&e, title), Some(800.0 / 600.0), "its own size");
        let a = aspect(&e, anamorphic).unwrap();
        assert!((a - 16.0 / 9.0).abs() < 1e-12, "stored × pixel aspect: {a}");
        assert_eq!(aspect(&e, unmeasured), None, "not measured yet");
        assert_eq!(aspect(&e, audio), None);
        // In the library listing too, and serialised under this name.
        let listed = e.media_list();
        let json =
            serde_json::to_value(listed.iter().find(|m| m.id == anamorphic.to_string())).unwrap();
        assert!((json["display_aspect"].as_f64().unwrap() - 16.0 / 9.0).abs() < 1e-12);
        let json = serde_json::to_value(e.media_get_info(unmeasured)).unwrap();
        assert!(json["display_aspect"].is_null());
    }

    /// A FIFO in a fresh temp folder: opening it blocks until a writer
    /// comes, as an open on a hung drive does.
    #[cfg(unix)]
    fn hung_file(tag: &str, name: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::ffi::OsStrExt;
        let dir = std::env::temp_dir().join(format!("vance-v2-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join(name);
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path; mkfifo only reads it.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        (dir, fifo)
    }

    /// Lets a reader blocked opening `fifo` through (it reads end of file),
    /// so a failing test doesn't leave a decoder hung.
    #[cfg(unix)]
    fn release(fifo: &Path) {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(fifo);
    }

    /// Runs `f` on `e` in a thread; its result and how long it took, or
    /// `None` (after releasing `fifo`) when it didn't come back in `limit`.
    #[cfg(unix)]
    fn timed<T: Send + 'static>(
        mut e: Engine,
        fifo: &Path,
        limit: Duration,
        f: impl FnOnce(&mut Engine) -> T + Send + 'static,
    ) -> (Engine, Option<(T, Duration)>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let started = Instant::now();
            let out = f(&mut e);
            let _ = tx.send((out, started.elapsed()));
            e
        });
        let got = rx.recv_timeout(limit).ok();
        if got.is_none() {
            release(fifo);
        }
        (worker.join().expect("worker"), got)
    }

    #[cfg(unix)]
    #[test]
    fn importing_a_file_on_a_hung_drive_says_so() {
        if locate_decoder_binary().is_err() {
            eprintln!("decoder sidecar missing — skipping");
            return;
        }
        let (dir, fifo) = hung_file("hung-import", "on-the-nas.mov");
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        e.test_probe_limit = Some(Duration::from_secs(1));
        let path = fifo.to_string_lossy().into_owned();
        let (e, got) = timed(e, &fifo, Duration::from_secs(10), move |e| {
            e.import_media_file(&path)
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let _ = std::fs::remove_dir_all(&dir);
        let (result, took) = got.expect("the import came back");
        assert_eq!(
            result,
            Err("on-the-nas.mov didn't respond (is the drive connected?)".to_string())
        );
        assert!(took < Duration::from_secs(4), "within the limit: {took:?}");
        assert!(e.media_list().is_empty(), "nothing imported");
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_size_probe_leaves_the_clip_unmeasured() {
        if locate_decoder_binary().is_err() {
            eprintln!("decoder sidecar missing — skipping");
            return;
        }
        let (dir, fifo) = hung_file("hung-probe", "on-the-nas.mov");
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        // Imported while the drive was fine (its size not known in this
        // session), now hung.
        let item = MediaItem::new(
            fifo.clone(),
            "nas",
            MediaKind::Video,
            vance_core::RationalTime::new(2, 1).unwrap(),
        );
        let media = item.id;
        e.current_project
            .as_mut()
            .unwrap()
            .media
            .insert(media, Arc::new(item));
        let v1 = track_ids(&e)[0];
        let (e, got) = timed(e, &fifo, Duration::from_secs(15), move |e| {
            e.insert_clip_at(v1, media, 0.0)
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let _ = std::fs::remove_dir_all(&dir);
        let (result, took) = got.expect("the insert came back");
        assert_eq!(result, Ok(()), "the clip is placed");
        assert!(
            took < SIZE_PROBE_LIMIT + Duration::from_secs(3),
            "bounded: {took:?}"
        );
        assert_eq!(
            e.current_sequence_contents().unwrap().frame_size,
            None,
            "unmeasured: it sets no canvas size"
        );
    }

    /// A media item whose file is `path` (a FIFO: a hung drive), added to
    /// the library without opening it.
    #[cfg(unix)]
    fn hung_media(e: &mut Engine, path: &Path) -> MediaItemId {
        hung_media_at(e, path)
    }

    /// A video media item on `path`, added without opening it.
    fn hung_media_at(e: &mut Engine, path: &Path) -> MediaItemId {
        let item = MediaItem::new(
            path.to_path_buf(),
            "nas",
            MediaKind::Video,
            vance_core::RationalTime::new(4, 1).unwrap(),
        );
        let media = item.id;
        e.current_project
            .as_mut()
            .unwrap()
            .media
            .insert(media, Arc::new(item));
        media
    }

    /// Blue on V1 (it sizes the canvas), the hung file over it on V2.
    #[cfg(unix)]
    fn blue_under_a_hung_layer(tag: &str) -> Option<(Engine, PathBuf, PathBuf)> {
        if locate_decoder_binary().is_err() {
            eprintln!("decoder sidecar missing — skipping");
            return None;
        }
        let (dir, fifo) = hung_file(tag, "on-the-nas.mov");
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let v2 = e.add_track(TrackKind::Video, None, None).unwrap();
        let blue = title_media(&mut e, card([0.0, 0.0, 1.0]));
        e.insert_clip_at(v1, blue, 0.0).unwrap();
        let hung = hung_media(&mut e, &fifo);
        e.insert_clip_at(v2, hung, 0.0).unwrap();
        let Some(e) = with_test_viewer(e, 0.5) else {
            let _ = std::fs::remove_dir_all(&dir);
            return None;
        };
        Some((e, dir, fifo))
    }

    /// Polls like the UI (`is_playing`) until the viewer has repainted more
    /// than `after` times, or `limit`; meanwhile lets readers of `fifo`
    /// through, if given (the drive answers, whenever its decoder opens it).
    #[cfg(unix)]
    fn poll_until_repainted(
        e: &mut Engine,
        after: usize,
        limit: Duration,
        fifo: Option<&Path>,
    ) -> Option<Duration> {
        let started = Instant::now();
        while started.elapsed() < limit {
            if let Some(fifo) = fifo {
                release(fifo);
            }
            e.is_playing();
            if repaints(e) > after {
                return Some(started.elapsed());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    /// The test viewer's pixel at (x, y).
    fn viewer_px(e: &Engine, x: usize, y: usize) -> [u8; 3] {
        let frame = e
            .test_viewer
            .as_ref()
            .and_then(|v| v.frame.as_ref())
            .expect("a repainted frame");
        let i = (y * 64 + x) * 4;
        [frame[i], frame[i + 1], frame[i + 2]]
    }

    /// A layer whose file takes long (a slow drive) doesn't hold the frame
    /// past the scrub wait, nor the next scrubs: the rest shows at once,
    /// with no notice while it's still decoding, and its decoder isn't
    /// killed. Once it answers (with nothing playable here) it's reported.
    #[cfg(unix)]
    #[test]
    fn a_layer_still_decoding_holds_neither_the_frame_nor_gives_a_notice() {
        let Some((mut e, dir, fifo)) = blue_under_a_hung_layer("slow-scrub") else {
            return;
        };
        let wait = Duration::from_secs(1);
        e.test_scrub_wait = Some(wait);
        let started = Instant::now();
        let first = e.repaint_preview(None);
        let first_took = started.elapsed();
        let first_repaints = repaints(&e);
        let first_centre = viewer_centre(&e);
        e.preview_t = Some(1.0);
        let started = Instant::now();
        let second = e.repaint_preview(None);
        let second_took = started.elapsed();
        let quiet = e.take_notices();
        let before = repaints(&e);
        let landed = poll_until_repainted(&mut e, before, Duration::from_secs(10), Some(&fifo));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(first.is_ok() && second.is_ok(), "{first:?} {second:?}");
        assert!(
            first_took >= wait && first_took < wait * 3,
            "waited the scrub wait, no longer: {first_took:?}"
        );
        assert_eq!(first_repaints, 1, "shown without the slow layer");
        assert_eq!(first_centre, [0, 0, 255], "the layer below shows");
        assert!(
            second_took < wait * 3 / 4,
            "not held again: {second_took:?}"
        );
        assert_eq!(before, 2);
        assert!(quiet.is_empty(), "no notice while it decodes: {quiet:?}");
        assert!(landed.is_some(), "repainted once it landed");
        assert!(
            !e.hung_paths.contains_key(&*fifo.to_string_lossy()),
            "its decoder wasn't killed for being slow"
        );
        assert!(
            e.take_notices()
                .iter()
                .any(|n| n.starts_with("Couldn't show nas: ")),
            "a layer that answered with nothing is reported"
        );
    }

    /// Makes `out` with ffmpeg from a lavfi `graph`, all keyframes; `false`
    /// (skip) without ffmpeg or libx264.
    fn make_clip(out: &Path, graph: &str, format: Option<&str>) -> bool {
        let mut cmd = std::process::Command::new("ffmpeg");
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            graph,
        ])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "1"]);
        if let Some(format) = format {
            cmd.args(["-f", format]);
        }
        vance_ipc::process::status(cmd.arg(out)).is_ok_and(|s| s.success())
    }

    /// A slow layer and a fast one, scrubbed through the engine: the fast
    /// one updates at once, the slow one keeps its last picture, and shows
    /// its new one once its file answers (repainted on the UI's poll); the
    /// scrub doesn't wait on it meanwhile.
    #[cfg(unix)]
    #[test]
    fn a_slow_layer_keeps_its_picture_while_a_fast_one_updates() {
        if locate_decoder_binary().is_err() {
            eprintln!("decoder sidecar missing — skipping");
            return;
        }
        let (dir, fifo) = hung_file("slow-and-fast", "on-the-nas.ts");
        let (fast, slow, late) = (
            dir.join("fast.mp4"),
            dir.join("slow.mp4"),
            dir.join("late.ts"),
        );
        let made = make_clip(
            &fast,
            "color=c=red:s=64x32:d=0.5:r=24[a];color=c=lime:s=64x32:d=3.5:r=24[b];[a][b]concat=n=2:v=1",
            None,
        ) && make_clip(&slow, "color=c=blue:s=32x32:d=4:r=24", None)
            // Short and small: read from a pipe, its decoder seeks within
            // what it has buffered.
            && make_clip(&late, "color=c=white:s=32x32:d=1:r=24", Some("mpegts"));
        if !made {
            eprintln!("ffmpeg / libx264 missing — skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let v2 = e.add_track(TrackKind::Video, None, None).unwrap();
        let import = |e: &mut Engine, p: &Path| {
            let info = e.import_media_file(&p.to_string_lossy()).unwrap();
            MediaItemId::from_uuid(info.id.parse().unwrap())
        };
        let (fast_media, slow_media) = (import(&mut e, &fast), import(&mut e, &slow));
        e.insert_clip_at(v1, fast_media, 0.0).unwrap();
        e.insert_clip_at(v2, slow_media, 0.0).unwrap();
        let Some(mut e) = with_test_viewer(e, 0.25) else {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let wait = Duration::from_secs(1);
        e.test_scrub_wait = Some(wait);
        let is = |c: [u8; 3], want: [u8; 3]| c.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 24);
        let (red, lime, blue, white) = ([255, 0, 0], [0, 255, 0], [0, 0, 255], [255; 3]);
        let edge_and_centre = |e: &Engine| (viewer_px(e, 2, 16), viewer_centre(e));

        e.repaint_preview(None).unwrap();
        let shown = edge_and_centre(&e);
        // The slow layer's file is now on a drive that takes long.
        let slow_item = e
            .current_project
            .as_mut()
            .unwrap()
            .media
            .get_mut(&slow_media)
            .unwrap();
        Arc::make_mut(slow_item).path = fifo.clone();
        e.preview_t = Some(0.75);
        let started = Instant::now();
        e.repaint_preview(None).unwrap();
        let took = started.elapsed();
        let held = edge_and_centre(&e);
        let quiet = e.take_notices();
        // The drive answers: the file opens (as often as it's opened).
        let bytes = std::fs::read(&late).unwrap();
        let feeding = Arc::new(AtomicBool::new(true));
        let (feed, still) = (fifo.clone(), feeding.clone());
        let feeder = std::thread::spawn(move || {
            use std::io::Write;
            while still.load(Ordering::SeqCst) {
                let Ok(mut w) = std::fs::OpenOptions::new().write(true).open(&feed) else {
                    return;
                };
                let _ = w.write_all(&bytes);
            }
        });
        // Polled like the UI. A landing may repaint more than once (one that
        // fired for a decode the scrub had already waited for, too): until
        // the slow layer's new picture is up.
        let before = repaints(&e);
        let started = Instant::now();
        let mut landed = None;
        while started.elapsed() < Duration::from_secs(10) {
            e.is_playing();
            if repaints(&e) > before && is(viewer_centre(&e), white) {
                landed = Some(started.elapsed());
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let after = edge_and_centre(&e);
        let notices = e.take_notices();
        // Its decoders hold the FIFO open: gone with it, the feeder's writes
        // fail and it stops.
        drop(e);
        feeding.store(false, Ordering::SeqCst);
        {
            // Lets a feeder blocked opening the FIFO through.
            use std::os::unix::fs::OpenOptionsExt;
            let _ = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&fifo);
        }
        let _ = feeder.join();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(is(shown.0, red) && is(shown.1, blue), "both: {shown:?}");
        assert!(
            took < wait * 2,
            "the scrub didn't wait on the slow layer: {took:?}"
        );
        assert!(is(held.0, lime), "the fast layer updated at once: {held:?}");
        assert!(is(held.1, blue), "the slow one kept its picture: {held:?}");
        assert!(quiet.is_empty(), "no notice while it decodes: {quiet:?}");
        assert!(landed.is_some(), "repainted once it landed");
        assert!(
            is(after.0, lime) && is(after.1, white),
            "the slow one's new picture: {after:?} {notices:?}"
        );
    }

    /// A clip whose file doesn't answer may be the one that sizes the
    /// sequence: a later clip doesn't size it meanwhile (at insert, or when
    /// an old project opens). Once its file answers (unusable here), the
    /// next clip does.
    #[cfg(unix)]
    #[test]
    fn a_hung_clip_defers_the_canvas_size_instead_of_a_later_clip() {
        if locate_decoder_binary().is_err() {
            eprintln!("decoder sidecar missing — skipping");
            return;
        }
        let size = |e: &Engine| e.current_sequence_contents().unwrap().frame_size;
        let wait_for_size = |e: &mut Engine| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while size(e).is_none() && Instant::now() < deadline {
                e.sizing_retried_at = None;
                e.retry_deferred_sizing();
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        // Insert: the hung clip first, then a later one.
        let (dir, fifo) = hung_file("hung-sizing", "on-the-nas.mov");
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let hung = hung_media(&mut e, &fifo);
        let later = title_media(&mut e, sized_title(800, 600));
        let (mut e, got) = timed(e, &fifo, Duration::from_secs(20), move |e| {
            e.insert_clip_at(v1, hung, 0.0).unwrap();
            e.insert_clip_at(v1, later, 10.0).unwrap();
        });
        assert!(got.is_some(), "the inserts came back");
        assert_eq!(size(&e), None, "not sized by the later clip");
        // The drive answers: the file is no video, so the later clip sizes it.
        std::fs::remove_file(&fifo).unwrap();
        std::fs::write(&fifo, b"not a video").unwrap();
        e.hung_paths.clear();
        wait_for_size(&mut e);
        assert_eq!(size(&e), Some([800, 600]));

        // Open: an old project whose earliest clip's file is hung.
        std::fs::remove_file(&fifo).unwrap();
        let c = std::ffi::CString::new(fifo.to_string_lossy().into_owned()).unwrap();
        // SAFETY: a valid NUL-terminated path; mkfifo only reads it.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        e.set_frame_size(None).unwrap();
        let bundle = dir.join("old.vance");
        e.save_project(Some(bundle.to_string_lossy().into_owned()))
            .unwrap();
        let path = bundle.to_string_lossy().into_owned();
        let (mut opened, got) = timed(
            Engine::default(),
            &fifo,
            Duration::from_secs(20),
            move |e| e.load_project(&path).map(|_| ()),
        );
        assert!(matches!(got, Some((Ok(()), _))), "it opened");
        assert_eq!(size(&opened), None, "not sized by the later clip");
        std::fs::remove_file(&fifo).unwrap();
        std::fs::write(&fifo, b"not a video").unwrap();
        opened.hung_paths.clear();
        wait_for_size(&mut opened);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(size(&opened), Some([800, 600]));
        assert!(
            !opened.can_undo(),
            "sized as the migration does: no undo step"
        );
    }

    /// The preview path's file checks don't wait on a drive that doesn't
    /// answer: its proxy is left out (as if gone), and an offline original
    /// whose drive doesn't answer stays offline.
    #[test]
    fn file_checks_dont_wait_on_a_drive_that_doesnt_answer() {
        use crate::decoder_pool::fs_probe::tests::{block, release};
        let dir = std::env::temp_dir().join(format!("vance-v2-dead-mount-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (original, proxy) = (dir.join("clip.mov"), dir.join("clip.proxy.mov"));
        std::fs::write(&original, b"x").unwrap();
        std::fs::write(&proxy, b"x").unwrap();
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let media = hung_media_at(&mut e, &original);
        e.set_media_proxy_path(media, Some(proxy.clone())).unwrap();
        e.proxies_awaiting_original.insert(media);
        block(&proxy);
        block(&original);
        e.scrub_pool = Some(new_decoder_pool());
        let started = Instant::now();
        e.refresh_pool_proxies();
        e.recheck_offline_originals();
        let took = started.elapsed();
        let found = e.pool_proxies_checked.as_ref().map(|c| c.found.len());
        let awaiting = e.proxies_awaiting_original.contains(&media);
        release(&proxy);
        release(&original);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(took < Duration::from_secs(2), "{took:?}");
        assert_eq!(found, Some(0), "the proxy that doesn't answer is left out");
        assert!(awaiting, "an original that doesn't answer stays offline");
    }

    /// The media list's hash prefix is the file's (path, mtime, length), as
    /// the peaks side computes it; a file whose drive doesn't answer doesn't
    /// hold the list (it counts as unreadable).
    #[test]
    fn the_media_hash_reads_its_file_with_a_timeout() {
        use crate::decoder_pool::fs_probe::tests::{block, release};
        let dir = std::env::temp_dir().join(format!("vance-v2-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (here, dead) = (dir.join("here.mov"), dir.join("dead.mov"));
        std::fs::write(&here, b"twelve bytes").unwrap();
        std::fs::write(&dead, b"x").unwrap();
        let item =
            |p: &Path| MediaItem::new(p.to_path_buf(), "m", MediaKind::Video, seconds_to_rt(1.0));
        let meta = std::fs::metadata(&here).unwrap();
        let mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i128;
        let want = crate::peaks::hash_prefix_from_path(&here, mtime, meta.len());
        let hex: String = want.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(media_item_hash_prefix_hex(&item(&here)), hex);
        block(&dead);
        let started = Instant::now();
        let unread = media_item_hash_prefix_hex(&item(&dead));
        let took = started.elapsed();
        release(&dead);
        let _ = std::fs::remove_dir_all(&dir);
        let blank: String = crate::peaks::hash_prefix_from_path(&dead, 0, 0)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(took < Duration::from_secs(2), "{took:?}");
        assert_eq!(unread, blank, "unreadable");
    }

    /// A monitor moves at once even while an export holds the engine; its
    /// surface follows when the export ends.
    #[test]
    fn monitors_move_during_an_export() {
        #[derive(Default)]
        struct Window(Mutex<Vec<ViewportBounds>>);
        impl MonitorWindow for Window {
            fn move_to(&self, bounds: ViewportBounds) {
                self.0.lock().push(bounds);
            }
        }
        let handle = EngineHandle::new();
        let record = Arc::new(Window::default());
        *handle.viewports.record.lock() = Some(record.clone());
        let (end, ends) = std::sync::mpsc::channel::<()>();
        let exporter = handle.clone();
        let export = std::thread::spawn(move || exporter.run_export(|_| ends.recv()));
        while !handle.is_exporting() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let to = ViewportBounds::new(10, 20, 300, 200);
        let started = Instant::now();
        let moved = handle.set_monitor_bounds(Monitor::Record, to, Duration::from_millis(50));
        let took = started.elapsed();
        let during = record.0.lock().clone();
        let unfitted = handle.viewports.unfitted.lock()[0];
        let _ = end.send(());
        assert!(export.join().unwrap().is_ok());
        assert_eq!(moved, Ok(()));
        assert_eq!(during, vec![to], "moved during the export");
        assert!(took < Duration::from_millis(500), "{took:?}");
        assert_eq!(unfitted, Some(to), "its surface waits for the engine");
        assert_eq!(
            handle.viewports.unfitted.lock()[0],
            None,
            "fitted when the export ended"
        );
        assert_eq!(handle.lock().last_bounds, to);
        assert_eq!(
            handle.set_monitor_bounds(Monitor::Source, to, Duration::ZERO),
            Err(EngineError::NoViewport.to_string()),
            "no Source window here"
        );
    }

    /// The step that applies a slow command's finished work waits an
    /// export out (instead of "Export in progress"), then gets the engine.
    #[test]
    fn applying_finished_work_waits_the_export_out() {
        let handle = EngineHandle::new();
        handle.lock().new_project("t".into()).unwrap();
        let (end, ends) = std::sync::mpsc::channel::<()>();
        let exporter = handle.clone();
        let export = std::thread::spawn(move || exporter.run_export(|_| ends.recv()));
        while !handle.is_exporting() {
            std::thread::sleep(Duration::from_millis(1));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let applier = handle.clone();
        std::thread::spawn(move || {
            let mut engine = applier.engine_after_export();
            let _ = tx.send(
                engine
                    .create_title_media("T".into(), 1.0, card([1.0; 3]))
                    .is_ok(),
            );
        });
        let busy = handle.engine().map(|_| ());
        let during = rx.recv_timeout(Duration::from_millis(300));
        let _ = end.send(());
        assert!(export.join().unwrap().is_ok());
        let after = rx.recv_timeout(Duration::from_secs(5));
        assert_eq!(busy, Err(EXPORT_IN_PROGRESS.to_string()));
        assert!(during.is_err(), "waits while the export runs");
        assert_eq!(after, Ok(true), "applied once it ended");
        assert_eq!(handle.lock().media_list().len(), 1);
    }

    #[test]
    fn set_track_locked_locks_unlocks_and_undoes() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let locked = |e: &Engine| e.current_sequence_contents().unwrap().tracks[0].locked;
        e.set_track_locked(v1, true).unwrap();
        assert!(locked(&e));
        assert!(
            e.remove_track(v1).is_err(),
            "a locked track can't be deleted"
        );
        e.set_track_locked(v1, false).unwrap();
        assert!(!locked(&e));
        assert!(e.undo().unwrap());
        assert!(locked(&e), "undo relocks it");
        assert!(e.set_track_locked(TrackId::new(), true).is_err());
    }

    #[test]
    fn a_clip_only_sizes_a_sequence_without_video() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let v2 = e.add_track(TrackKind::Video, None, None).unwrap();
        let size = |e: &Engine| e.current_sequence_contents().unwrap().frame_size;
        let landscape = title_media(&mut e, sized_title(640, 360));
        e.insert_clip_at(v1, landscape, 0.0).unwrap();
        assert_eq!(size(&e), Some([640, 360]));
        // The user goes back to the default canvas; the edit has video now.
        e.set_frame_size(None).unwrap();
        let portrait = title_media(&mut e, sized_title(360, 640));
        e.insert_clip_at(v2, portrait, 5.0).unwrap();
        assert_eq!(size(&e), None, "a portrait clip doesn't turn the canvas");
        e.append_clip(v1, portrait).unwrap();
        assert_eq!(size(&e), None);
    }

    #[test]
    fn opening_an_old_project_sizes_it_from_its_earliest_video_clip() {
        let mut e = Engine::default();
        e.new_project("old".into()).unwrap();
        let ids = track_ids(&e);
        let (v1, a1) = (ids[0], ids[1]);
        let v2 = e.add_track(TrackKind::Video, None, None).unwrap();
        let late = title_media(&mut e, sized_title(800, 600));
        let early = title_media(&mut e, sized_title(641, 361));
        let audio_track = title_media(&mut e, sized_title(1280, 720));
        e.insert_clip_at(v1, late, 5.0).unwrap();
        e.insert_clip_at(v2, early, 1.0).unwrap();
        // On an audio track, earlier still: not a video clip.
        e.insert_clip_at(a1, audio_track, 0.0).unwrap();
        // Saved before sequences had a canvas size.
        e.set_frame_size(None).unwrap();
        let dir =
            std::env::temp_dir().join(format!("vance-v2-legacy-{}.vance", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        e.save_project(Some(dir.to_string_lossy().into_owned()))
            .unwrap();

        let mut opened = Engine::default();
        opened.load_project(&dir.to_string_lossy()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            opened.current_sequence_contents().unwrap().frame_size,
            Some([640, 360]),
            "the earliest video clip (V2 at 1 s), even-rounded"
        );
        assert!(!opened.can_undo(), "the migration is not an undo step");
    }

    #[test]
    fn opening_an_old_project_without_video_keeps_the_default() {
        let mut e = Engine::default();
        e.new_project("empty".into()).unwrap();
        let dir =
            std::env::temp_dir().join(format!("vance-v2-novideo-{}.vance", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        e.save_project(Some(dir.to_string_lossy().into_owned()))
            .unwrap();
        let mut opened = Engine::default();
        opened.load_project(&dir.to_string_lossy()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(opened.current_sequence_contents().unwrap().frame_size, None);
    }

    #[test]
    fn anamorphic_media_sizes_the_canvas_with_square_pixels() {
        assert_eq!(display_size((1440, 1080), (4, 3)), (1920, 1080));
        assert_eq!(display_size((720, 576), (64, 45)), (1024, 576));
        assert_eq!(display_size((1920, 1080), (1, 1)), (1920, 1080));
        assert_eq!(display_size((1920, 1080), (0, 0)), (1920, 1080));
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let path = PathBuf::from("/nonexistent/anamorphic.mov");
        let info = {
            let item = MediaItem::new(
                path,
                "ana",
                MediaKind::Video,
                vance_core::RationalTime::new(2, 1).unwrap(),
            );
            let id = item.id;
            e.current_project
                .as_mut()
                .unwrap()
                .media
                .insert(id, Arc::new(item));
            id
        };
        e.media_dims.insert(info, (1440, 1080));
        e.media_sar.insert(info, (4, 3));
        assert_eq!(e.media_frame_size(info), Some((1920, 1080)));
    }

    #[test]
    fn render_and_replace_needs_one_blend_mode_in_the_range() {
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let v1 = track_ids(&e)[0];
        let a = title_media(&mut e, card([1.0; 3]));
        let b = title_media(&mut e, card([0.5; 3]));
        let first = e.insert_clip_at(v1, a, 0.0).unwrap();
        let first = vance_timeline::ClipId::from_uuid(first.parse().unwrap());
        e.insert_clip_at(v1, b, 2.0).unwrap();
        let seq = || e.current_sequence_ref().unwrap().clone();
        use vance_timeline::BlendMode;
        assert_eq!(
            shared_blend_mode(&seq(), v1, 0.0, 4.0).unwrap(),
            BlendMode::Normal
        );
        assert_eq!(
            shared_blend_mode(&seq(), v1, 9.0, 10.0).unwrap(),
            BlendMode::Normal
        );

        let mut e = e;
        e.set_clip_blend_mode(v1, first, BlendMode::Screen).unwrap();
        let seq = e.current_sequence_ref().unwrap().clone();
        assert_eq!(
            shared_blend_mode(&seq, v1, 0.0, 2.0).unwrap(),
            BlendMode::Screen
        );
        let err = shared_blend_mode(&seq, v1, 1.0, 3.0)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("different blend modes; render them separately"),
            "{err}"
        );
        let refused = e.render_and_replace(RenderAndReplaceOptions {
            output_path: std::env::temp_dir()
                .join("vance-v2-never-written.mov")
                .to_string_lossy()
                .into_owned(),
            width: None,
            height: None,
            track_id: v1,
            start_seconds: 1.0,
            end_seconds: 3.0,
        });
        assert!(refused.is_err(), "refused before anything renders");
        assert!(!std::env::temp_dir()
            .join("vance-v2-never-written.mov")
            .exists());
    }

    #[test]
    fn preview_frames_are_decoded_before_the_render_lock() {
        let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4");
        if locate_decoder_binary().is_err() || !fixture.exists() {
            eprintln!("decoder sidecar or fixture missing — skipping");
            return;
        }
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let info = e.import_media_file(&fixture.to_string_lossy()).unwrap();
        let media = MediaItemId::from_uuid(info.id.parse().unwrap());
        e.insert_clip_at(track_ids(&e)[0], media, 0.0).unwrap();
        let rs = match e.ensure_render_state() {
            Ok(rs) => rs,
            Err(EngineError::Project(m)) if m.contains("headless renderer") => {
                eprintln!("no GPU ({m}) — skipping");
                return;
            }
            Err(e) => panic!("{e}"),
        };
        let cache = e.ensure_frame_cache();
        // Every scrub decode looks in the frame cache first.
        let lookups = |c: &vance_cache::FrameCache| {
            let s = c.stats();
            s.ring_hits + s.disk_hits + s.misses
        };
        let before = lookups(&cache);
        let held = rs.lock();
        let worker = std::thread::spawn(move || {
            let frame = e.preview_frame_rgba(0.5, 64, 48);
            (e, frame)
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        while lookups(&cache) == before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let fetched_while_locked = lookups(&cache) > before;
        drop(held);
        let (_e, frame) = worker.join().expect("preview thread");
        frame.expect("the preview finishes once the lock is free");
        assert!(
            fetched_while_locked,
            "the layer's frame is fetched while another holder has the render state"
        );
    }

    #[test]
    fn compose_gets_the_media_sizes_even_through_a_proxy() {
        let fixtures =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../vance-decoder/tests/fixtures");
        let original = fixtures.join("testsrc_320x240_5s.mp4");
        let stand_in = fixtures.join("hdr10_320x240_2s.mp4");
        if locate_decoder_binary().is_err() || !original.exists() || !stand_in.exists() {
            eprintln!("decoder sidecar or fixtures missing — skipping");
            return;
        }
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let info = e.import_media_file(&original.to_string_lossy()).unwrap();
        let media = MediaItemId::from_uuid(info.id.parse().unwrap());
        e.insert_clip_at(track_ids(&e)[0], media, 0.0).unwrap();
        if gpu_frame(e.preview_frame_rgba(0.5, 64, 48)).is_none() {
            return;
        }
        let dims = |e: &Engine| e.compose_res.as_ref().and_then(|r| r.media_dims(media));
        assert_eq!(dims(&e), Some((320, 240)), "imported sizes reach compose");

        // Opened later (canvas already set, so nothing probes at load) and
        // previewed from its proxy only: the original is measured for its
        // size.
        e.set_media_proxy_path(media, Some(stand_in)).unwrap();
        e.set_use_proxies(true).unwrap();
        let dir = std::env::temp_dir().join(format!("vance-v2-dims-{}.vance", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        e.save_project(Some(dir.to_string_lossy().into_owned()))
            .unwrap();
        let mut opened = Engine::default();
        let loaded = opened.load_project(&dir.to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
        loaded.unwrap();
        assert!(opened.media_dims.is_empty());
        // Measured off the engine lock: there by a later frame.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            opened.preview_frame_rgba(0.5, 64, 48).unwrap();
            if dims(&opened).is_some() || Instant::now() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(dims(&opened), Some((320, 240)));
    }

    #[test]
    fn prefetch_follows_the_pool_onto_a_proxy() {
        let fixtures =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../vance-decoder/tests/fixtures");
        let original = fixtures.join("testsrc_320x240_5s.mp4");
        let stand_in = fixtures.join("hdr10_320x240_2s.mp4");
        let Ok(bin) = locate_decoder_binary() else {
            eprintln!("decoder sidecar missing — skipping");
            return;
        };
        if !original.exists() || !stand_in.exists() {
            eprintln!("fixtures missing — skipping");
            return;
        }
        let mut e = Engine::default();
        e.new_project("t".into()).unwrap();
        let info = e.import_media_file(&original.to_string_lossy()).unwrap();
        let media = MediaItemId::from_uuid(info.id.parse().unwrap());
        e.set_media_proxy_path(media, Some(stand_in.clone()))
            .unwrap();
        let request = FrameRequest {
            clip_id: vance_timeline::ClipId::new(),
            media,
            path: original.to_string_lossy().into_owned(),
            source_seconds: 0.5,
        };
        // Within the budget the original is decoded (and prefetched).
        e.scrub_pool = Some(new_decoder_pool());
        e.refresh_pool_proxies();
        let dec = e.take_pooled_decoder(&request.path).unwrap();
        e.return_pooled_decoder(dec);
        let (path, ..) = e
            .layer_prefetch_target(std::slice::from_ref(&request))
            .unwrap();
        assert_eq!(path, request.path);

        // Over it, the pool swaps the layer to its proxy: so does the prefetch.
        e.scrub_pool = Some(DecoderPool::new(bin, 6, MemoryBudget { cap_bytes: 1 }));
        e.refresh_pool_proxies();
        let dec = e.take_pooled_decoder(&request.path).unwrap();
        assert_eq!(dec.path, stand_in.to_string_lossy(), "the pool swapped");
        e.return_pooled_decoder(dec);
        let (path, info, anchor) = e.layer_prefetch_target(&[request]).unwrap();
        assert_eq!(path, stand_in.to_string_lossy());
        assert_eq!(
            anchor,
            crate::decoder_pool::source_seconds_to_pts(0.5, info.time_base),
            "in the proxy's own time base"
        );
    }

    #[test]
    fn a_bare_stream_import_error_says_how_to_fix_it() {
        use vance_decoder::{client::ClientError, OpenError};
        let message = |format: &str| {
            EngineError::from(ClientError::Open(OpenError::NeedsContainer {
                format: format.into(),
            }))
            .to_string()
        };
        assert_eq!(
            message("h264"),
            "This is a raw H.264 stream with no container (no timestamps), so Vance can't \
             seek or play it. Convert it to MP4 first, e.g. \
             `ffmpeg -i in.h264 -c:v libx264 -crf 17 out.mp4`. A lossless `-c copy` remux \
             only keeps the timing right for streams without B-frames."
        );
        let hevc = message("hevc");
        assert!(
            hevc.starts_with("This is a raw HEVC (H.265) stream"),
            "{hevc}"
        );
        assert!(hevc.contains("`ffmpeg -i in.hevc -c:v libx264"), "{hevc}");
        let vvc = message("vvc");
        assert!(vvc.starts_with("This is a raw VVC stream"), "{vvc}");
        assert!(vvc.contains("`ffmpeg -i in.vvc "), "{vvc}");
        // Every other decoder error keeps its wording.
        assert_eq!(
            EngineError::from(ClientError::Open(OpenError::NoVideoStream)).to_string(),
            "decoder: decoder open failed: NoVideoStream"
        );
    }

    #[test]
    fn grain_window_fades_edges_keeps_middle() {
        let sr = 48_000u32;
        let frames = 1000usize;
        let mut grain = vec![1.0f32; frames * 2]; // all unity
        apply_grain_window(&mut grain, sr);
        // First frame fully faded to 0.
        assert!(
            grain[0].abs() < 1e-6 && grain[1].abs() < 1e-6,
            "start not faded"
        );
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
        assert!(e.edit_history()[other].is_current);
        // Out-of-range goto is a clean false, not an error.
        assert!(!e.goto_history(999).expect("goto oob is Ok(false)"));
    }

    // ---- Phase 7.3 mixer integration ---------------------------------

    fn t(num: i64) -> vance_core::RationalTime {
        vance_core::RationalTime::new(num, 1).unwrap()
    }

    fn audio_media(path: &str) -> Arc<MediaItem> {
        let mut m = MediaItem::new(PathBuf::from(path), "clip.wav", MediaKind::Audio, t(10));
        m.has_audio_track = true;
        Arc::new(m)
    }

    #[test]
    fn build_mixer_snapshot_inserts_clipmix_only_for_audio_tracks() {
        let mut seq = vance_timeline::Sequence::new("S", vance_core::FrameRate::FPS_24);
        let video_t = vance_timeline::Track::new(TrackKind::Video, "V1");
        let audio_t = vance_timeline::Track::new(TrackKind::Audio, "A1");
        let video_clip =
            Arc::new(vance_timeline::Clip::new(MediaItemId::new(), "v", t(0), t(0), t(5)).unwrap());
        let audio_clip =
            Arc::new(vance_timeline::Clip::new(MediaItemId::new(), "a", t(0), t(0), t(5)).unwrap());
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
        let cm = snap
            .clips
            .get(&audio_clip_id.as_uuid())
            .expect("audio clip present");
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
        // Narrower target → the fitted (letterboxed) source is scaled up
        // uniformly until it covers the canvas height: ar_s/ar_t on both
        // axes (the compositor fits instead of stretching, ADR 0007).
        let ar_s = 1920.0f32 / 1080.0;
        let ar_t = 1080.0f32 / 1920.0;
        assert!(
            (sc[0] - ar_s / ar_t).abs() < 1e-4,
            "cover scale = ar_s/ar_t"
        );
        assert_eq!(sc[0], sc[1], "uniform: no distortion");
        // The subject (clamped) maps to output-center: out = scale*(subj-0.5)+0.5+translate.
        let hx = 0.5 / sc[0];
        let clamped_cx = 0.9f32.clamp(hx, 1.0 - hx);
        let out_x = sc[0] * (clamped_cx - 0.5) + 0.5 + tr[0];
        assert!(
            (out_x - 0.5).abs() < 1e-4,
            "subject centered horizontally: {out_x}"
        );

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
        mbus.inserts
            .push(vance_timeline::AudioEffectSpec::default_compressor());
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
        let m = snap
            .buses
            .get(&music.as_uuid())
            .expect("music bus in snapshot");
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
        let mut mi_without_inner = MediaItem::new(
            PathBuf::from("/tmp/without.mp4"),
            "x",
            MediaKind::Video,
            t(10),
        );
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
        assert!(op_touches_mixer(&EditOp::SetTrackMute {
            track_id: tid,
            mute: true
        }));
        assert!(op_touches_mixer(&EditOp::SetTrackSolo {
            track_id: tid,
            solo: true
        }));
        assert!(op_touches_mixer(&EditOp::SetTrackGainDb {
            track_id: tid,
            gain_db: -3.0
        }));
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
        // Track topology moves audio between tracks / out of the mix.
        assert!(op_touches_mixer(&EditOp::InsertTrack {
            track_id: tid,
            kind: TrackKind::Audio,
            name: String::new(),
            position: 0,
        }));
        assert!(op_touches_mixer(&EditOp::MoveTrack {
            track_id: tid,
            to_position: 0,
        }));
        assert!(op_touches_mixer(&EditOp::SetTrackEnabled {
            track_id: tid,
            enabled: false,
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
        let plan = compute_sched_plan(std::slice::from_ref(&c), &live, 0.0, 0.05, 2.0, 16);
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
        let plan = compute_sched_plan(&[c], &std::collections::HashSet::new(), 0.0, 0.05, 2.0, 16);
        assert!(plan.to_add.is_empty());
    }

    #[test]
    fn compute_sched_plan_add_source_offset_seeks_into_clip_when_overlapping() {
        // Clip [10..20] with source_in=3.0, playhead at t=12.
        // from_seconds = 3.0 + (12 - 10) = 5.0.
        let mut c = sched(10.0, 10.0);
        c.source_in_seconds = 3.0;
        let plan = compute_sched_plan(
            std::slice::from_ref(&c),
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
        let plan = compute_sched_plan(std::slice::from_ref(&c), &live, 1.02, 0.05, 2.0, 16);
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
        let plan = compute_sched_plan(std::slice::from_ref(&c), &live, 1.06, 0.05, 2.0, 16);
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
        let kept: std::collections::HashSet<_> = plan.to_add.iter().map(|x| x.0).collect();
        assert!(kept.contains(&c0.clip_id));
        assert!(kept.contains(&c1.clip_id));
        assert!(!kept.contains(&c2.clip_id));
        assert!(!kept.contains(&c3.clip_id));
    }

    #[test]
    fn compute_sched_plan_no_cap_drops_under_cap() {
        let c0 = sched(0.0, 5.0);
        let plan = compute_sched_plan(
            std::slice::from_ref(&c0),
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
        let plan = compute_sched_plan(std::slice::from_ref(&c), &live, 2.0, 0.05, 2.0, 16);
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
            std::slice::from_ref(&c),
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
        assert_eq!(
            pre.len(),
            1,
            "preload window should include t-1s-ahead clip"
        );
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
        assert!(
            past.is_empty(),
            "clips behind the playhead must be excluded"
        );
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
        let plan = compute_sched_plan(std::slice::from_ref(&c), &live, 0.0, 0.05, 2.0, 16);
        assert!(
            plan.to_add.is_empty(),
            "seeded live set must suppress respawn"
        );
        assert!(plan.to_remove.is_empty());
    }
}
