//! The one frame function every consumer renders through (ADR 0007):
//! scrub, playback and export all call [`compose_frame`] with a
//! [`ScenePlan`] and a [`FrameProvider`]; only the provider differs.
//!
//! The intended preview/export differences are the view LUT and viewer
//! solos (preview only), the output space/format, and the canvas size.
//!
//! Lock ordering: `compose_frame` is called with the render state held (the
//! [`Compositor`] lives in the renderer) and takes the add-on host and LUT
//! cache locks briefly inside it. Never take the render state while holding
//! either of those. Fetch decoded frames BEFORE taking the render state
//! ([`Prefetched`]), so decode waits never hold it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use vance_color::{ColorSpace, Pipeline, PipelineParams};
use vance_core::RationalTime;
use vance_decoder::protocol::{ColorMatrixHint, ColorRangeHint};
use vance_decoder::{FrameColor, FrameData};
use vance_fusion::{Comp, Fit, NodeKind};
use vance_mograph::{MographEngine, MographInstance};
use vance_render::{
    render_fusion_rgba, render_mograph_rgba, render_title_rgba, CompositeOp, Compositor,
    GradeNodeParams, KeyerParams, LayerHandle, LayerKey, LayerParams, LayerSource, OutputSpec,
    RgbaImage, TitleError,
};
use vance_timeline::{
    Clip, ClipId, Effect, EffectId, EffectValue, LayerClip, LayerContent, MediaItem, MediaItemId,
    MediaKind, Project, ScenePlan, Sequence, TitleParams, TrackId, TransitionKind,
};

use crate::addons::{self, AddonHost};
use crate::decoder_pool::fs_probe::{exists_within, DEFAULT_TIMEOUT};
use crate::engine::EngineError;

/// Parsed LUT files keyed by absolute path (same shape as the engine's).
pub type LutCache = HashMap<PathBuf, Arc<vance_color::Lut3d>>;

/// One decoded frame a layer needs.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameRequest {
    /// Clip the frame is for. A Fusion `MediaIn` input carries a synthetic
    /// id derived from (Fusion clip, media), stable across frames.
    pub clip_id: ClipId,
    /// The clip's media item.
    pub media: MediaItemId,
    /// File to decode (the proxy's path when the layer uses one).
    pub path: String,
    /// Source time to show, in seconds.
    pub source_seconds: f64,
}

/// Supplies decoded frames to [`compose_frame`].
///
/// Rule for every implementation: the first frame with
/// `pts >= source_seconds_to_pts(source_seconds, time_base)`, or the last
/// frame at end of stream.
pub trait FrameProvider {
    /// One entry per request, in order; `None` = no frame (the layer is
    /// skipped). Called exactly once per [`compose_frame`], possibly with
    /// an empty slice.
    fn frames(&mut self, reqs: &[FrameRequest]) -> Vec<Option<FrameData>>;
    /// Hint at the requests coming next (prefetch). Default: nothing.
    fn prepare(&mut self, _upcoming: &[FrameRequest]) {}
    /// Why the last [`Self::frames`] call had no frame for `req`, when the
    /// provider knows (an open or decode error). Default: not known.
    fn failure(&self, _req: &FrameRequest) -> Option<String> {
        None
    }
}

/// Who the frame is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// The live viewer (view LUT and solos apply).
    Preview,
    /// An export; `hdr` composites in linear working space.
    Export {
        /// HDR output.
        hdr: bool,
    },
}

/// Transient viewer state (never serialized). Ignored for exports.
#[derive(Clone, Copy, Debug, Default)]
pub struct ViewerState {
    /// Grade node whose HSL-qualifier matte is soloed.
    pub matte_node: Option<EffectId>,
    /// Clip whose keyer matte is soloed.
    pub matte_view: Option<ClipId>,
}

/// Most title cards cached at once (a 1080p card is about 8 MB).
const MAX_CACHED_TITLES: usize = 8;

/// Rendered synthetic media reused across frames. Titles don't animate, so
/// their RGBA is cached by a hash of their params (least recently used
/// evicted first); MoGraph and Fusion re-render per time.
#[derive(Default)]
pub struct SyntheticCache {
    titles: HashMap<u64, TitleEntry>,
    tick: u64,
}

struct TitleEntry {
    params: TitleParams,
    image: Arc<RgbaImage>,
    last_used: u64,
}

impl std::fmt::Debug for SyntheticCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyntheticCache")
            .field("titles", &self.titles.len())
            .finish()
    }
}

impl SyntheticCache {
    /// The title rendered as straight-alpha sRGB RGBA8, from the cache when
    /// the same params were rendered before.
    pub fn title(&mut self, p: &TitleParams) -> Result<Arc<RgbaImage>, TitleError> {
        self.tick += 1;
        let key = title_hash(p);
        if let Some(e) = self.titles.get_mut(&key) {
            // The hash only picks the slot; equality decides a hit.
            if e.params == *p {
                e.last_used = self.tick;
                return Ok(e.image.clone());
            }
        }
        let image = Arc::new(render_title_rgba(p)?);
        if self.titles.len() >= MAX_CACHED_TITLES && !self.titles.contains_key(&key) {
            let oldest = self
                .titles
                .iter()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(k, _)| *k);
            if let Some(k) = oldest {
                self.titles.remove(&k);
            }
        }
        self.titles.insert(
            key,
            TitleEntry {
                params: p.clone(),
                image: image.clone(),
                last_used: self.tick,
            },
        );
        Ok(image)
    }

    /// Number of cached title cards.
    // Intentional: introspection for tests; the app never asks.
    #[allow(dead_code)]
    pub fn cached_titles(&self) -> usize {
        self.titles.len()
    }

    /// Drop every cached image.
    pub fn clear(&mut self) {
        self.titles.clear();
    }
}

fn title_hash(p: &TitleParams) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.text.hash(&mut h);
    p.size_px.to_bits().hash(&mut h);
    for c in p.fg.iter().chain(&p.bg) {
        c.to_bits().hash(&mut h);
    }
    p.width.hash(&mut h);
    p.height.hash(&mut h);
    p.bg_alpha.to_bits().hash(&mut h);
    h.finish()
}

/// Which media files (originals and proxies) existed when it was taken. A
/// playback session checks them once at its start instead of stat-ing a
/// proxy per layer per frame: a stat on a hung network share never returns,
/// and the session's project snapshot can't change while it plays (a proxy
/// that disappears meanwhile fails to open and falls back, see
/// [`crate::decoder_pool`]).
#[derive(Clone, Debug, Default)]
pub struct FileSnapshot {
    existing: HashSet<PathBuf>,
}

impl FileSnapshot {
    /// Check every video / image item's original and proxy now.
    pub fn of(project: &Project) -> Self {
        let mut existing = HashSet::new();
        let files = project
            .media
            .values()
            .filter(|m| matches!(m.kind, MediaKind::Video | MediaKind::Image))
            .flat_map(|m| std::iter::once(&m.path).chain(m.proxy_path.as_ref()));
        for path in files {
            // A file that doesn't answer (dead share) counts as missing.
            if exists_within(path, DEFAULT_TIMEOUT) == Ok(true) {
                existing.insert(path.clone());
            }
        }
        Self { existing }
    }

    fn exists(&self, path: &Path) -> bool {
        self.existing.contains(path)
    }
}

/// Whether `path` exists: from `files` when given, else a timed check (a
/// file on a dead share counts as missing rather than hanging the frame).
fn file_exists(files: Option<&FileSnapshot>, path: &Path) -> bool {
    files.map_or_else(
        || exists_within(path, DEFAULT_TIMEOUT) == Ok(true),
        |f| f.exists(path),
    )
}

/// A renderer created on first use and shared with the engine: building a
/// MoGraph engine or Fusion compositor loads the system font database, so a
/// project without titles or comps never pays for it.
pub type LazyShared<T> = Arc<std::sync::OnceLock<Arc<T>>>;

/// Long-lived state `compose_frame` uses, shareable with the playback
/// thread.
pub struct ComposeResources {
    /// WASM / shader add-ons.
    pub addons: Arc<Mutex<AddonHost>>,
    /// Parsed clip LUTs.
    pub luts: Arc<Mutex<LutCache>>,
    mograph: LazyShared<MographEngine>,
    fusion: LazyShared<vance_fusion::Compositor>,
    /// Rendered synthetic media.
    pub synth: SyntheticCache,
    /// Full-resolution (stored) size of each video / image media item.
    media_dims: HashMap<MediaItemId, (u32, u32)>,
    /// Set by a playback session: file existence without stat-ing.
    files: Option<Arc<FileSnapshot>>,
}

impl ComposeResources {
    /// Resources sharing the engine's add-on host, LUT cache and (lazily
    /// built) MoGraph / Fusion renderers.
    pub fn new(
        addons: Arc<Mutex<AddonHost>>,
        luts: Arc<Mutex<LutCache>>,
        mograph: LazyShared<MographEngine>,
        fusion: LazyShared<vance_fusion::Compositor>,
    ) -> Self {
        Self {
            addons,
            luts,
            mograph,
            fusion,
            synth: SyntheticCache::default(),
            media_dims: HashMap::new(),
            files: None,
        }
    }

    /// New resources sharing these ones' add-on host, LUT cache, renderers
    /// and media sizes, with empty caches: what a playback session that had
    /// to be abandoned hands back instead of its own.
    pub fn fresh(&self) -> Self {
        let mut res = Self::new(
            self.addons.clone(),
            self.luts.clone(),
            self.mograph.clone(),
            self.fusion.clone(),
        );
        res.media_dims = self.media_dims.clone();
        res
    }

    /// Answer file-existence questions from `files` instead of stat-ing
    /// (a playback session sets it for its run, then clears it).
    pub(crate) fn set_file_snapshot(&mut self, files: Option<Arc<FileSnapshot>>) {
        self.files = files;
    }

    /// The snapshot [`Self::set_file_snapshot`] set.
    pub(crate) fn file_snapshot(&self) -> Option<&FileSnapshot> {
        self.files.as_deref()
    }

    /// The engine's probe of each video / image item's full-resolution
    /// (stored) size. A frame decoded from a smaller proxy uses it to scale
    /// source-pixel parameters (keyer choke / soften) to the original's.
    pub fn set_media_dims(&mut self, dims: HashMap<MediaItemId, (u32, u32)>) {
        self.media_dims = dims;
    }

    /// Full-resolution size of `media`, when the engine has probed it.
    pub fn media_dims(&self, media: MediaItemId) -> Option<(u32, u32)> {
        self.media_dims.get(&media).copied()
    }

    /// The MoGraph renderer, built on first use.
    pub fn mograph(&self) -> &Arc<MographEngine> {
        self.mograph.get_or_init(|| Arc::new(MographEngine::new()))
    }

    /// The Fusion compositor, built on first use.
    pub fn fusion(&self) -> &Arc<vance_fusion::Compositor> {
        self.fusion
            .get_or_init(|| Arc::new(vance_fusion::Compositor::new()))
    }
}

/// What [`compose_frame`] couldn't draw this frame.
#[derive(Debug, Default)]
pub struct ComposeReport {
    /// Planned layers that weren't drawn, with why; also layers drawn
    /// without one of their add-on effects (the reason names the effect).
    pub skipped: Vec<SkippedLayer>,
}

/// A planned layer [`compose_frame`] left out.
#[derive(Debug, Clone)]
pub struct SkippedLayer {
    /// The clip (a Fusion `MediaIn` input reports its Fusion clip).
    pub clip_id: ClipId,
    /// Its media item.
    pub media: MediaItemId,
    /// What went wrong, for the caller's policy (an export fails on
    /// missing media or no frame, and carries on past an add-on).
    pub kind: SkipKind,
    /// Why, in words a user can act on.
    pub reason: String,
}

/// What kind of problem left a layer (or part of it) out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipKind {
    /// The media isn't in the library, or its file is offline.
    MissingMedia,
    /// The file is there but no frame came: it wouldn't open or decode, or
    /// (playback) the frame wasn't ready in time.
    NoFrame,
    /// A title / motion graphic / Fusion comp couldn't be rendered.
    RenderFailed,
    /// An add-on effect or transition didn't run; the layer was drawn
    /// without it.
    AddonFailed,
}

/// Frames fetched BEFORE the render lock is taken, so decode waits never
/// hold it: `Prefetched::fetch(provider, plan_requests(..))` outside the
/// lock, then `compose_frame` with the `Prefetched` inside it. Answers each
/// request with the fetched frame for an equal request (moved out, never
/// copied), else `None`.
///
/// Drop it once the frame is drawn: frames from streaming feeds are shares
/// of shared-memory leases, and a feed only has two.
pub struct Prefetched {
    reqs: Vec<FrameRequest>,
    frames: Vec<Option<FrameData>>,
    /// The provider's [`FrameProvider::failure`] for each request it had
    /// no frame for.
    failures: Vec<Option<String>>,
}

impl Prefetched {
    /// Ask `prov` for `reqs` now.
    pub fn fetch(prov: &mut dyn FrameProvider, reqs: Vec<FrameRequest>) -> Self {
        let mut frames = prov.frames(&reqs);
        frames.resize_with(reqs.len(), || None);
        let failures = reqs
            .iter()
            .zip(&frames)
            .map(|(r, f)| if f.is_none() { prov.failure(r) } else { None })
            .collect();
        Self {
            reqs,
            frames,
            failures,
        }
    }
}

impl FrameProvider for Prefetched {
    fn frames(&mut self, reqs: &[FrameRequest]) -> Vec<Option<FrameData>> {
        reqs.iter()
            .map(|r| {
                let i = self
                    .reqs
                    .iter()
                    .zip(&self.frames)
                    .position(|(have, f)| have == r && f.is_some())?;
                self.frames[i].take()
            })
            .collect()
    }

    fn failure(&self, req: &FrameRequest) -> Option<String> {
        let i = self.reqs.iter().position(|have| have == req)?;
        self.failures[i].clone()
    }
}

/// Per-frame inputs to [`compose_frame`].
pub struct ComposeCtx<'a> {
    /// The project (media library, colour settings).
    pub project: &'a Project,
    /// The sequence being rendered.
    pub seq: &'a Sequence,
    /// Preview or export.
    pub purpose: Purpose,
    /// Viewer solos; ignored for [`Purpose::Export`].
    pub viewer: ViewerState,
    /// Render only this track (render-and-replace); `None` = all tracks.
    pub track_filter: Option<TrackId>,
    /// Drawing for realtime playback (frames must come fast; e.g. add-ons
    /// skip the preview render cache). `false` for scrub and export.
    pub realtime: bool,
    /// Shared resources.
    pub res: &'a mut ComposeResources,
}

// ── Effect evaluation ──────────────────────────────────────────────────────

/// Product of every enabled "opacity" effect's `value` param at
/// clip-relative `t_seconds`; `1.0` when there are none.
pub(crate) fn eval_clip_opacity(effects: &[Effect], t_seconds: f64) -> f32 {
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

/// 2D transform aggregated from every enabled "transform" effect:
/// translate and rotation add, scale multiplies.
pub(crate) struct ClipTransform {
    pub translate: [f32; 2],
    pub scale: [f32; 2],
    pub rotation_radians: f32,
}

pub(crate) fn eval_clip_transform(effects: &[Effect], t_seconds: f64) -> ClipTransform {
    let mut t = ClipTransform {
        translate: [0.0, 0.0],
        scale: [1.0, 1.0],
        rotation_radians: 0.0,
    };
    let eval = |k: &str, ps: &BTreeMap<String, EffectValue>| -> Option<f32> {
        ps.get(k)
            .and_then(|p| p.eval_number(t_seconds))
            .map(|v| v as f32)
    };
    for e in effects {
        if !e.enabled || e.kind != "transform" {
            continue;
        }
        t.translate[0] += eval("translate_x", &e.params).unwrap_or(0.0);
        t.translate[1] += eval("translate_y", &e.params).unwrap_or(0.0);
        t.scale[0] *= eval("scale_x", &e.params).unwrap_or(1.0);
        t.scale[1] *= eval("scale_y", &e.params).unwrap_or(1.0);
        t.rotation_radians += eval("rotation", &e.params).unwrap_or(0.0).to_radians();
    }
    t
}

/// `[left, right, top, bottom]` crop fractions; crop effects stack
/// additively.
pub(crate) fn eval_clip_crop(effects: &[Effect], t_seconds: f64) -> [f32; 4] {
    let mut crop = [0.0_f32; 4];
    let eval = |k: &str, ps: &BTreeMap<String, EffectValue>| -> f32 {
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

/// `(brightness, contrast)` from every enabled "color" effect: brightness
/// sums, contrast multiplies. Defaults to `(0, 1)`.
pub(crate) fn eval_clip_brightness_contrast(effects: &[Effect], t_seconds: f64) -> (f32, f32) {
    let mut brightness = 0.0_f32;
    let mut contrast = 1.0_f32;
    let eval = |k: &str, ps: &BTreeMap<String, EffectValue>| -> Option<f32> {
        ps.get(k)
            .and_then(|p| p.eval_number(t_seconds))
            .map(|v| v as f32)
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

/// The serial colour-grade chain: every `grade` effect in order, read at
/// `t_seconds`. Disabled nodes keep their slot (bypassed in the shader).
/// Capped at `MAX_GRADE_NODES`. `matte_node` forces that node's qualifier
/// mask on (a transient viewer solo, never persisted).
pub(crate) fn eval_clip_grades(
    effects: &[Effect],
    t_seconds: f64,
    matte_node: Option<EffectId>,
) -> Vec<GradeNodeParams> {
    use vance_timeline::grade as gk;
    let num = |ps: &BTreeMap<String, EffectValue>, k: &str, default: f32| -> f32 {
        ps.get(k)
            .and_then(|p| p.eval_number(t_seconds))
            .map(|v| v as f32)
            .unwrap_or(default)
    };
    // Curve control points are static (input level, output level) pairs,
    // not read at `t_seconds`. Absent = identity ramp.
    let curve = |ps: &BTreeMap<String, EffectValue>, k: &str| -> Vec<vance_timeline::Keyframe> {
        match ps.get(k) {
            Some(EffectValue::Keyframes { keyframes: pts }) => pts.clone(),
            _ => Vec::new(),
        }
    };
    // Warn once per process: this runs per frame.
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
        let is_matte = matte_node == Some(e.id);
        let curves = if matches!(mode, vance_render::GradeMode::Curves) {
            Some(vance_render::CurveSet {
                master: curve(&e.params, gk::KEY_CURVE_MASTER),
                r: curve(&e.params, gk::KEY_CURVE_R),
                g: curve(&e.params, gk::KEY_CURVE_G),
                b: curve(&e.params, gk::KEY_CURVE_B),
                luma: curve(&e.params, gk::KEY_CURVE_LUMA),
                hh: curve(&e.params, gk::KEY_CURVE_HUE_HUE),
                hs: curve(&e.params, gk::KEY_CURVE_HUE_SAT),
                hl: curve(&e.params, gk::KEY_CURVE_HUE_LUM),
                ss: curve(&e.params, gk::KEY_CURVE_SAT_SAT),
                ll: curve(&e.params, gk::KEY_CURVE_LUM_LUM),
            })
        } else {
            None
        };
        out.push(GradeNodeParams {
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
            // Wide-open defaults so an enabled-but-untuned qualifier keys
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
            // Shape rounded + clamped so a fractional interpolation can't
            // read as off/garbage.
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

/// First enabled "chroma_key" effect as `(key_rgb, tolerance, softness)`.
/// Keys don't compose; the first one wins.
pub(crate) fn eval_clip_chroma_key(
    effects: &[Effect],
    t_seconds: f64,
) -> Option<([f32; 3], f32, f32)> {
    let e = effects
        .iter()
        .find(|e| e.enabled && e.kind == "chroma_key")?;
    let eval = |k: &str, fallback: f32| -> f32 {
        e.params
            .get(k)
            .and_then(|p| p.eval_number(t_seconds))
            .map(|v| v as f32)
            .unwrap_or(fallback)
    };
    let r = eval("r", 0.0).clamp(0.0, 1.0);
    let g = eval("g", 1.0).clamp(0.0, 1.0);
    let b = eval("b", 0.0).clamp(0.0, 1.0);
    let tolerance = eval("tolerance", 0.15).max(0.0);
    let softness = eval("softness", 0.05).max(0.0);
    Some(([r, g, b], tolerance, softness))
}

/// First enabled "luma_key" effect as `(threshold, direction, softness)`;
/// `direction` is `+1` (keep brighter) or `-1` (keep darker).
pub(crate) fn eval_clip_luma_key(effects: &[Effect], t_seconds: f64) -> Option<(f32, f32, f32)> {
    let e = effects.iter().find(|e| e.enabled && e.kind == "luma_key")?;
    let eval = |k: &str, fallback: f32| -> f32 {
        e.params
            .get(k)
            .and_then(|p| p.eval_number(t_seconds))
            .map(|v| v as f32)
            .unwrap_or(fallback)
    };
    let threshold = eval("threshold", 0.5).clamp(0.0, 1.0);
    let dir = if eval("direction", 1.0) >= 0.0 {
        1.0
    } else {
        -1.0
    };
    let softness = eval("softness", 0.05).clamp(0.0, 1.0);
    Some((threshold, dir, softness))
}

/// Largest keyer choke / soften, in source pixels (the compositor clamps to
/// the same).
const KEYER_MAX_PX: f32 = 64.0;

/// First enabled `keyer` effect (see `vance_timeline::keyer` for keys and
/// defaults). Non-finite values fall back to the default.
pub(crate) fn eval_clip_keyer(effects: &[Effect], t_seconds: f64) -> Option<KeyerParams> {
    use vance_timeline::keyer as k;
    let e = effects
        .iter()
        .find(|e| e.enabled && e.kind == k::KEYER_KIND)?;
    let num = |key: &str, default: f32| -> f32 {
        e.params
            .get(key)
            .and_then(|p| p.eval_number(t_seconds))
            .filter(|v| v.is_finite())
            .map(|v| v as f32)
            .unwrap_or(default)
    };
    let invert = match e.params.get(k::KEY_INVERT) {
        Some(EffectValue::Bool(b)) => *b,
        _ => num(k::KEY_INVERT, 0.0) != 0.0,
    };
    Some(KeyerParams {
        key_rgb: [
            num(k::KEY_R, 0.0).clamp(0.0, 1.0),
            num(k::KEY_G, 1.0).clamp(0.0, 1.0),
            num(k::KEY_B, 0.0).clamp(0.0, 1.0),
        ],
        balance: num(k::KEY_BALANCE, 0.5).clamp(0.0, 1.0),
        strength: num(k::KEY_STRENGTH, 1.0).max(0.0),
        clip_black: num(k::KEY_CLIP_BLACK, 0.0).clamp(0.0, 1.0),
        // User decision 3: the default Clip White is 0.9.
        clip_white: num(k::KEY_CLIP_WHITE, 0.9).clamp(0.0, 1.0),
        // Clamped here, before any proxy scaling (`scale_to_proxy`), to the
        // compositor's own range: a proxy then chokes as far as the export.
        choke_px: num(k::KEY_CHOKE_PX, 0.0).clamp(-KEYER_MAX_PX, KEYER_MAX_PX),
        soften_px: num(k::KEY_SOFTEN_PX, 0.0).clamp(0.0, KEYER_MAX_PX),
        spill: num(k::KEY_SPILL, 1.0).clamp(0.0, 1.0),
        spill_bias: num(k::KEY_SPILL_BIAS, 0.5).clamp(0.0, 1.0),
        invert,
    })
}

/// The first enabled "lut" effect's parsed LUT (cached by path). An empty
/// path or a load failure means no LUT.
pub(crate) fn clip_lut(
    effects: &[Effect],
    luts: &Mutex<LutCache>,
) -> Option<Arc<vance_color::Lut3d>> {
    let e = effects.iter().find(|e| e.enabled && e.kind == "lut")?;
    let Some(EffectValue::String(s)) = e.params.get("path") else {
        return None;
    };
    if s.is_empty() {
        return None;
    }
    let path = PathBuf::from(s);
    if let Some(lut) = luts.lock().get(&path) {
        return Some(lut.clone());
    }
    // A LUT on a dead share: no LUT this frame rather than a hung read
    // (the check is cached, so the next frames don't wait either).
    if exists_within(&path, DEFAULT_TIMEOUT) != Ok(true) {
        warn_once("lut file missing or not answering; no LUT", 0);
        return None;
    }
    // Parse outside the lock: the playback thread shares the cache.
    match vance_color::load_lut(&path) {
        Ok(lut) => {
            let arc = Arc::new(lut);
            luts.lock().insert(path, arc.clone());
            Some(arc)
        }
        Err(e) => {
            tracing::warn!(?e, ?path, "lut: load failed");
            None
        }
    }
}

// ── Colour pipeline ────────────────────────────────────────────────────────

/// The project's working space, or linear Rec.709 when the built-in path
/// can't work in it (PQ / HLG / ACEScct).
fn working_space(p: &Project) -> ColorSpace {
    let w = p.working_color_space;
    if w.is_builtin_supported() {
        w
    } else {
        warn_once(
            "working colour space not supported; using linear Rec.709",
            0,
        );
        ColorSpace::LinearRec709
    }
}

/// SDR preview / export encode sRGB (the SDR target's store applies the
/// transfer); HDR export encodes Rec.2020 PQ.
fn output_space(purpose: Purpose) -> ColorSpace {
    match purpose {
        Purpose::Export { hdr: true } => ColorSpace::Rec2020Pq,
        _ => ColorSpace::Srgb,
    }
}

/// What a layer's pixels are encoded in: the media's tagged (or
/// import-detected) space, else Rec.709 limited.
fn input_space(p: &Project, media: MediaItemId) -> ColorSpace {
    let Some(m) = p.media.get(&media) else {
        return ColorSpace::Rec709Limited;
    };
    let cs = match m.kind {
        // The title / MoGraph / Fusion rasterizers always emit sRGB RGBA.
        MediaKind::Title | MediaKind::MoGraph | MediaKind::Fusion => ColorSpace::Srgb,
        MediaKind::Video | MediaKind::Image | MediaKind::Audio => {
            m.input_color_space.unwrap_or(ColorSpace::Rec709Limited)
        }
    };
    if cs.is_builtin_input_supported() {
        cs
    } else {
        warn_once(
            "input colour space needs OCIO; decoding as Rec.709 limited",
            media.as_uuid().as_u128(),
        );
        ColorSpace::Rec709Limited
    }
}

/// Rec.709 limited → linear Rec.709 → sRGB, the pre-colour-pipeline look.
fn default_pipeline() -> PipelineParams {
    PipelineParams {
        input_transfer: 2,
        output_transfer: 1,
        input_range: 0,
        output_range: 1,
        input_matrix: vance_color::pipeline::MATRIX_BT709,
        input_to_working: vance_color::Transform::IDENTITY,
        working_to_output: vance_color::Transform::IDENTITY,
    }
}

fn pipeline_params(input: ColorSpace, working: ColorSpace, output: ColorSpace) -> PipelineParams {
    Pipeline {
        input,
        working,
        output,
    }
    .to_params()
    .unwrap_or_else(default_pipeline)
}

/// The colour pipeline of a layer showing `media` for `purpose`.
pub(crate) fn layer_pipeline(p: &Project, media: MediaItemId, purpose: Purpose) -> PipelineParams {
    pipeline_params(
        input_space(p, media),
        working_space(p),
        output_space(purpose),
    )
}

/// [`PipelineParams::input_range`] ids (`vance_color`'s range ids).
const RANGE_LIMITED: u32 = 0;
const RANGE_FULL: u32 = 1;
/// Frames this tall or shorter are standard definition (BT.601).
const SD_MAX_LINES: u32 = 576;

/// How a decoded frame's samples are encoded, as `(input_range,
/// input_matrix)`. What the stream signals wins. Otherwise packed RGBA is
/// full range and YUV takes its input space's range, except PQ / HLG, which
/// are limited (HDR10 and HLG video is limited range; the `ColorSpace`
/// enum only says full because it has no limited variant, and full-range
/// decoding turns code 64 black into grey). The matrix falls back to BT.601
/// for standard definition (`sd_lines`: the ORIGINAL's height, so a small
/// proxy of an HD file stays BT.709) and else the input primaries'.
fn frame_coding(color: FrameColor, rgba: bool, sd_lines: u32, input: ColorSpace) -> (u32, u32) {
    let (primaries, _, range) = input.decompose();
    let range = match (rgba, color.range) {
        (true, _) => RANGE_FULL,
        (false, ColorRangeHint::Limited) => RANGE_LIMITED,
        (false, ColorRangeHint::Full) => RANGE_FULL,
        (false, ColorRangeHint::Unknown) => match input {
            ColorSpace::Rec2020Pq | ColorSpace::Rec2020Hlg => RANGE_LIMITED,
            // Intentional: an untagged file is Rec.709 limited here, but a
            // range the user chose ("Rec.709 (full)", sRGB) is honoured for a
            // stream that doesn't say: that is what the tag is for.
            _ => match range {
                vance_color::Range::Limited => RANGE_LIMITED,
                vance_color::Range::Full => RANGE_FULL,
            },
        },
    };
    let matrix = match color.matrix {
        ColorMatrixHint::Bt601 => vance_color::pipeline::MATRIX_BT601,
        ColorMatrixHint::Bt709 => vance_color::pipeline::MATRIX_BT709,
        ColorMatrixHint::Bt2020Ncl => vance_color::pipeline::MATRIX_BT2020_NCL,
        ColorMatrixHint::Unknown if sd_lines <= SD_MAX_LINES => vance_color::pipeline::MATRIX_BT601,
        ColorMatrixHint::Unknown => vance_color::pipeline::matrix_id_for_primaries(primaries),
    };
    (range, matrix)
}

/// Fill in what a decoded `frame` of `media` says about itself: its
/// range / matrix, pixel aspect, and (for a proxy smaller than the
/// original of `orig` size) source-pixel params scaled to it. The one
/// place scrub, playback and export share.
fn apply_frame(
    params: &mut LayerParams,
    p: &Project,
    media: MediaItemId,
    frame: &FrameData,
    orig: Option<(u32, u32)>,
) {
    let sd_lines = orig.map_or(frame.height, |(_, h)| h);
    let (range, matrix) = frame_coding(
        frame.color,
        frame.pixel_format.is_rgba(),
        sd_lines,
        input_space(p, media),
    );
    params.pipeline.input_range = range;
    params.pipeline.input_matrix = matrix;
    params.pixel_aspect = frame.color.pixel_aspect();
    scale_to_proxy(params, frame.height, orig);
}

/// A power window whose width isn't set gets a ROUND default on a picture
/// `display_w` × `height` (display pixels): its width, a fraction of the
/// picture's, is its height's fraction scaled by `height / display_w`. (A
/// script, or an older project, may enable a shape without a size; the
/// fixed 0.25 × 0.25 default is an ellipse on any non-square picture.) A
/// proxy keeps its original's display aspect (decision 5), so preview and
/// export agree; the frame's own aspect is used because `media_dims` holds
/// the STORED size, which differs from the display size for anamorphic
/// media.
fn round_default_windows(
    params: &mut LayerParams,
    effects: &[Effect],
    clip_t: f64,
    display_w: f32,
    height: u32,
) {
    use vance_timeline::grade as gk;
    if display_w <= 0.0 || height == 0 {
        return;
    }
    let grades = effects.iter().filter(|e| e.kind == gk::GRADE_KIND);
    for (e, g) in grades.zip(params.grades.iter_mut()) {
        let width_set = e
            .params
            .get(gk::KEY_WIN_SIZE_X)
            .and_then(|p| p.eval_number(clip_t))
            .is_some_and(f64::is_finite);
        if !width_set {
            g.window.size[0] = g.window.size[1] * height as f32 / display_w;
        }
    }
}

/// Keyer choke / soften are in source pixels of the ORIGINAL (what export
/// decodes): on a smaller proxy frame they shrink by `frame_h / orig_h`, so
/// a half-resolution preview chokes as far as the export.
fn scale_to_proxy(params: &mut LayerParams, frame_h: u32, orig: Option<(u32, u32)>) {
    let (Some(k), Some((_, orig_h))) = (params.keyer.as_mut(), orig) else {
        return;
    };
    if frame_h == 0 || frame_h >= orig_h {
        return;
    }
    let s = frame_h as f32 / orig_h as f32;
    k.choke_px *= s;
    k.soften_px *= s;
}

/// Layer params with no effects: just the colour pipeline.
fn plain_layer_params(pipeline: PipelineParams) -> LayerParams {
    LayerParams {
        pipeline,
        translate: [0.0; 2],
        scale: [1.0; 2],
        rotation: 0.0,
        crop_lrtb: [0.0; 4],
        brightness: 0.0,
        contrast: 1.0,
        chroma_key: None,
        luma_key: None,
        keyer: None,
        grades: Vec::new(),
        lut: None,
        matte_view: false,
        pixel_aspect: 1.0,
    }
}

/// The layer-pass parameters of `clip` at clip-relative `clip_t` seconds.
/// Opacity is not here: the composite pass applies it ([`CompositeOp`]).
pub fn layer_params_for(
    p: &Project,
    clip: &Clip,
    clip_t: f64,
    purpose: Purpose,
    v: &ViewerState,
    luts: &Mutex<LutCache>,
) -> LayerParams {
    let preview = purpose == Purpose::Preview;
    // Viewer solos are preview-only: an export never renders a matte.
    let matte_node = if preview { v.matte_node } else { None };
    let fx = &clip.effects;
    let xf = eval_clip_transform(fx, clip_t);
    let (brightness, contrast) = eval_clip_brightness_contrast(fx, clip_t);
    LayerParams {
        pipeline: layer_pipeline(p, clip.media, purpose),
        translate: xf.translate,
        scale: xf.scale,
        rotation: xf.rotation_radians,
        crop_lrtb: eval_clip_crop(fx, clip_t),
        brightness,
        contrast,
        chroma_key: eval_clip_chroma_key(fx, clip_t),
        luma_key: eval_clip_luma_key(fx, clip_t),
        keyer: eval_clip_keyer(fx, clip_t),
        grades: eval_clip_grades(fx, clip_t, matte_node),
        lut: clip_lut(fx, luts),
        matte_view: preview && v.matte_view == Some(clip.id),
        // Titles / MoGraph / Fusion render square pixels; a decoded frame's
        // own aspect replaces this (`apply_frame`).
        pixel_aspect: 1.0,
    }
}

/// The canvas and output encoding for `seq`: `seq.frame_size`, else
/// 1920×1080; working → sRGB for SDR, working → Rec.2020 PQ for an HDR
/// export. `view_lut` asks the output pass to apply the loaded view LUT
/// (preview only).
pub fn output_spec_for(p: &Project, seq: &Sequence, purpose: Purpose) -> OutputSpec {
    let (width, height) = seq.frame_size.unwrap_or((1920, 1080));
    let pipe = pipeline_params(ColorSpace::Srgb, working_space(p), output_space(purpose));
    OutputSpec {
        width,
        height,
        working_to_output: pipe.working_to_output,
        output_transfer: pipe.output_transfer,
        hdr: matches!(purpose, Purpose::Export { hdr: true }),
        view_lut: purpose == Purpose::Preview,
        transparent: false,
        output_primaries: output_space(purpose).decompose().0,
    }
}

// ── Frame planning ─────────────────────────────────────────────────────────

/// One clip layer to render this frame.
struct ClipJob<'a> {
    layer: LayerClip,
    clip: &'a Clip,
    source: Source<'a>,
}

enum Source<'a> {
    /// The media item is gone from the library.
    Missing,
    /// A decoded frame: index into the frame requests.
    File {
        req: usize,
    },
    Title(TitleParams),
    MoGraph(&'a MographInstance),
    Fusion {
        comp: &'a Comp,
        inputs: Vec<MediaInput<'a>>,
    },
}

/// One named media item feeding a Fusion comp's `MediaIn` nodes.
struct MediaInput<'a> {
    name: String,
    media: MediaItemId,
    key: LayerKey,
    /// Some node shows it at native size (`Fit::None`).
    native: bool,
    kind: InputKind<'a>,
}

enum InputKind<'a> {
    File { req: usize },
    Title(TitleParams),
    MoGraph(&'a MographInstance),
}

struct TransitionJob<'a> {
    from: ClipJob<'a>,
    to: ClipJob<'a>,
    kind: &'a TransitionKind,
    mix: f32,
    params: &'a BTreeMap<String, EffectValue>,
}

enum Job<'a> {
    Clip(ClipJob<'a>),
    Transition(Box<TransitionJob<'a>>),
}

impl ClipJob<'_> {
    fn clip_t(&self) -> f64 {
        secs(self.layer.clip_time)
    }

    /// The composite op. `normal_only`: render-and-replace (decision 2)
    /// renders one track alone into an alpha file whose new clip keeps the
    /// blend mode, so the layer itself composites Normal; its opacity (and
    /// keyframes, transitions) still applies and lands in the alpha.
    fn op(&self, normal_only: bool) -> CompositeOp {
        let blend_mode = if normal_only {
            vance_timeline::BlendMode::Normal.gpu_id()
        } else {
            self.layer.blend_mode.gpu_id()
        };
        CompositeOp {
            blend_mode,
            opacity: eval_clip_opacity(&self.clip.effects, self.clip_t()),
        }
    }

    fn skipped(&self, kind: SkipKind, reason: String) -> SkippedLayer {
        SkippedLayer {
            clip_id: self.layer.clip_id,
            media: self.layer.media,
            kind,
            reason,
        }
    }
}

fn secs(t: RationalTime) -> f64 {
    t.0.as_f64()
}

/// Resolve every layer of `plan` (bottom → top) and collect the decoded
/// frames they need, so they're fetched in one provider call.
fn plan_jobs<'a>(
    project: &'a Project,
    seq: &'a Sequence,
    plan: &'a ScenePlan,
    purpose: Purpose,
    track_filter: Option<TrackId>,
) -> (Vec<Job<'a>>, Vec<FrameRequest>) {
    plan_jobs_with(project, seq, plan, purpose, track_filter, None)
}

/// [`plan_jobs`], checking proxies against `files` when given.
fn plan_jobs_with<'a>(
    project: &'a Project,
    seq: &'a Sequence,
    plan: &'a ScenePlan,
    purpose: Purpose,
    track_filter: Option<TrackId>,
    files: Option<&FileSnapshot>,
) -> (Vec<Job<'a>>, Vec<FrameRequest>) {
    let mut jobs = Vec::new();
    let mut reqs = Vec::new();
    for layer in &plan.layers {
        if track_filter.is_some_and(|t| t != layer.track_id) {
            continue;
        }
        match &layer.content {
            LayerContent::Clip(lc) => {
                if let Some(j) =
                    clip_job(project, seq, layer.track_id, lc, purpose, files, &mut reqs)
                {
                    jobs.push(Job::Clip(j));
                }
            }
            LayerContent::Transition(lt) => {
                let track = layer.track_id;
                let from = clip_job(project, seq, track, &lt.from, purpose, files, &mut reqs);
                let to = clip_job(project, seq, track, &lt.to, purpose, files, &mut reqs);
                match (from, to) {
                    (Some(from), Some(to)) => jobs.push(Job::Transition(Box::new(TransitionJob {
                        from,
                        to,
                        kind: &lt.kind,
                        mix: lt.mix,
                        params: &lt.params,
                    }))),
                    (Some(one), None) | (None, Some(one)) => jobs.push(Job::Clip(one)),
                    (None, None) => {}
                }
            }
        }
    }
    (jobs, reqs)
}

/// The decoded frames `compose_frame` would ask for to render `plan`, in
/// the same order: what to hand [`FrameProvider::prepare`] for look-ahead.
pub fn plan_requests(
    project: &Project,
    seq: &Sequence,
    plan: &ScenePlan,
    purpose: Purpose,
    track_filter: Option<TrackId>,
) -> Vec<FrameRequest> {
    plan_jobs(project, seq, plan, purpose, track_filter).1
}

/// [`plan_requests`], checking proxies against `files` when given (a
/// playback session's snapshot) instead of stat-ing them.
pub(crate) fn plan_requests_with(
    project: &Project,
    seq: &Sequence,
    plan: &ScenePlan,
    purpose: Purpose,
    track_filter: Option<TrackId>,
    files: Option<&FileSnapshot>,
) -> Vec<FrameRequest> {
    plan_jobs_with(project, seq, plan, purpose, track_filter, files).1
}

/// Load (into `luts`) every clip LUT `plan` uses, so `compose_frame` finds
/// them cached: call it BEFORE taking the render lock, since reading a LUT
/// from a hung network share would otherwise freeze the renderer with it.
pub fn load_plan_luts(seq: &Sequence, plan: &ScenePlan, luts: &Mutex<LutCache>) {
    for layer in &plan.layers {
        let clips: Vec<&LayerClip> = match &layer.content {
            LayerContent::Clip(lc) => vec![lc],
            LayerContent::Transition(lt) => vec![&lt.from, &lt.to],
        };
        for lc in clips {
            if let Some(clip) = find_clip(seq, layer.track_id, lc.clip_id) {
                let _ = clip_lut(&clip.effects, luts);
            }
        }
    }
}

fn find_clip(seq: &Sequence, track: TrackId, id: ClipId) -> Option<&Clip> {
    seq.tracks
        .iter()
        .find(|t| t.id == track)?
        .clips
        .iter()
        .find(|c| c.id == id)
        .map(|c| c.as_ref())
}

fn clip_job<'a>(
    project: &'a Project,
    seq: &'a Sequence,
    track: TrackId,
    lc: &LayerClip,
    purpose: Purpose,
    files: Option<&FileSnapshot>,
    reqs: &mut Vec<FrameRequest>,
) -> Option<ClipJob<'a>> {
    let clip = find_clip(seq, track, lc.clip_id)?;
    let source = match project.media.get(&lc.media) {
        None => Source::Missing,
        Some(m) => match m.kind {
            MediaKind::Title => Source::Title(m.title.clone().unwrap_or_default()),
            // A MoGraph / Fusion item without its payload is malformed; show
            // a default title (as the engine always has) instead of failing.
            MediaKind::MoGraph => match m.mograph.as_ref() {
                Some(inst) => Source::MoGraph(inst),
                None => Source::Title(TitleParams::default()),
            },
            MediaKind::Fusion => match m.fusion.as_ref() {
                Some(comp) => Source::Fusion {
                    comp,
                    inputs: media_inputs(project, lc, comp, purpose, files, reqs),
                },
                None => Source::Title(TitleParams::default()),
            },
            // An audio item on a video track has no picture: no layer, no
            // frame request, and nothing to report.
            MediaKind::Audio => return None,
            MediaKind::Video | MediaKind::Image => {
                reqs.push(FrameRequest {
                    clip_id: lc.clip_id,
                    media: lc.media,
                    path: media_path_with(project, m, purpose, files),
                    source_seconds: secs(lc.source_pts),
                });
                Source::File {
                    req: reqs.len() - 1,
                }
            }
        },
    };
    Some(ClipJob {
        layer: *lc,
        clip,
        source,
    })
}

/// The file to decode: for preview, the proxy when the project uses proxies
/// and the proxy file still exists; otherwise the original.
#[cfg(test)]
fn media_path(project: &Project, m: &MediaItem, purpose: Purpose) -> String {
    media_path_with(project, m, purpose, None)
}

/// [`media_path`], checking the proxy against `files` when given.
fn media_path_with(
    project: &Project,
    m: &MediaItem,
    purpose: Purpose,
    files: Option<&FileSnapshot>,
) -> String {
    // Intentional: an export always decodes the full-resolution original and
    // ignores the "use proxies" toggle (follow-up decision 5); proxies are a
    // preview aid only.
    if purpose == Purpose::Preview && project.use_proxies {
        if let Some(p) = m.proxy_path.as_ref() {
            if file_exists(files, p) {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    m.path.to_string_lossy().into_owned()
}

/// The named media items a Fusion comp's `MediaIn` nodes read, sampled at
/// the comp's time (the Fusion clip's clip-relative time).
fn media_inputs<'a>(
    project: &'a Project,
    fusion: &LayerClip,
    comp: &'a Comp,
    purpose: Purpose,
    files: Option<&FileSnapshot>,
    reqs: &mut Vec<FrameRequest>,
) -> Vec<MediaInput<'a>> {
    let comp_t = secs(fusion.clip_time).max(0.0);
    let mut inputs: Vec<MediaInput<'a>> = Vec::new();
    for node in &comp.nodes {
        let NodeKind::MediaIn(spec) = &node.kind else {
            continue;
        };
        if let Some(seen) = inputs.iter_mut().find(|i| i.name == spec.name) {
            seen.native |= spec.fit == Fit::None;
            continue;
        }
        let Some(m) = resolve_media_in(project, &spec.name) else {
            warn_once(
                "fusion MediaIn names no media item; input left empty",
                fusion.clip_id.as_uuid().as_u128(),
            );
            continue;
        };
        let id = media_in_clip_id(fusion.clip_id, m.id);
        let kind = match m.kind {
            MediaKind::Title => InputKind::Title(m.title.clone().unwrap_or_default()),
            MediaKind::MoGraph => match m.mograph.as_ref() {
                Some(inst) => InputKind::MoGraph(inst),
                None => continue,
            },
            // `resolve_media_in` never returns a comp or audio.
            MediaKind::Fusion | MediaKind::Audio => continue,
            MediaKind::Video | MediaKind::Image => {
                reqs.push(FrameRequest {
                    clip_id: id,
                    media: m.id,
                    path: media_path_with(project, m, purpose, files),
                    source_seconds: comp_t,
                });
                InputKind::File {
                    req: reqs.len() - 1,
                }
            }
        };
        inputs.push(MediaInput {
            name: spec.name.clone(),
            media: m.id,
            key: LayerKey::from(id),
            native: spec.fit == Fit::None,
            kind,
        });
    }
    inputs
}

/// Resolve a Fusion `MediaIn` name to a media item (user decision 4): by
/// id, then by display name. Fusion items are never inputs, so a comp
/// can't read itself or recurse into another comp; audio items have no
/// picture to feed it.
pub(crate) fn resolve_media_in<'a>(project: &'a Project, name: &str) -> Option<&'a MediaItem> {
    let usable = |m: &&Arc<MediaItem>| !matches!(m.kind, MediaKind::Fusion | MediaKind::Audio);
    let by_id = uuid::Uuid::parse_str(name.trim())
        .ok()
        .and_then(|u| project.media.get(&MediaItemId::from_uuid(u)))
        .filter(usable);
    by_id
        .or_else(|| {
            project
                .media
                .values()
                .filter(usable)
                .find(|m| m.display_name == name)
        })
        .map(|m| m.as_ref())
}

/// A stable pseudo clip id for one `MediaIn` input of one Fusion clip.
/// Providers keep per-clip decode state, so each input needs its own id.
fn media_in_clip_id(fusion_clip: ClipId, media: MediaItemId) -> ClipId {
    let a = fusion_clip.as_uuid().as_u128();
    let b = media.as_uuid().as_u128();
    ClipId::from_uuid(uuid::Uuid::from_u128(a ^ b.rotate_left(64)))
}

/// Every layer slot a frame of `jobs` uses (rendered or not this frame, so
/// a transient decode miss doesn't free a slot that comes right back).
fn live_keys(jobs: &[Job<'_>]) -> Vec<LayerKey> {
    fn push(keys: &mut Vec<LayerKey>, j: &ClipJob<'_>) {
        keys.push(LayerKey::from(j.layer.clip_id));
        if let Source::Fusion { inputs, .. } = &j.source {
            keys.extend(
                inputs
                    .iter()
                    .filter(|i| matches!(i.kind, InputKind::File { .. }))
                    .map(|i| i.key),
            );
        }
    }
    let mut keys = Vec::new();
    for job in jobs {
        match job {
            Job::Clip(j) => push(&mut keys, j),
            Job::Transition(t) => {
                push(&mut keys, &t.from);
                push(&mut keys, &t.to);
            }
        }
    }
    keys
}

/// The op a WASM transition's single result composites with: the clip that
/// dominates the frame (from while `mix < 0.5`, then to).
fn dominant_op(from: CompositeOp, to: CompositeOp, mix: f32) -> CompositeOp {
    if mix < 0.5 {
        from
    } else {
        to
    }
}

fn with_opacity_share(op: CompositeOp, share: f32) -> CompositeOp {
    CompositeOp {
        opacity: op.opacity * share.clamp(0.0, 1.0),
        ..op
    }
}

#[derive(Debug, PartialEq)]
enum TransitionPath<'a> {
    Dissolve,
    Plugin(&'a str),
}

fn transition_path(kind: &TransitionKind, hdr: bool) -> TransitionPath<'_> {
    match kind {
        TransitionKind::Dissolve => TransitionPath::Dissolve,
        // Intentional: WASM transitions are 8-bit sRGB plugins; on an HDR
        // export the read-back would clip both layers, so the window
        // dissolves instead (CPU add-ons are skipped there too).
        TransitionKind::Wasm(_) if hdr => TransitionPath::Dissolve,
        TransitionKind::Wasm(id) => TransitionPath::Plugin(id),
    }
}

/// Log `what` once per (message, id) for the process; per-frame paths
/// would otherwise log at the frame rate.
fn warn_once(what: &'static str, id: u128) {
    static SEEN: OnceLock<Mutex<HashSet<(&'static str, u128)>>> = OnceLock::new();
    let mut seen = SEEN.get_or_init(Default::default).lock();
    if seen.len() < 4096 && seen.insert((what, id)) {
        tracing::warn!(id = %format!("{id:032x}"), "{what}");
    }
}

// ── Rendering ──────────────────────────────────────────────────────────────

struct FrameEnv<'f> {
    project: &'f Project,
    reqs: &'f [FrameRequest],
    frames: &'f [Option<FrameData>],
    /// The provider's reason for each request without a frame.
    failures: &'f [Option<String>],
    out: &'f OutputSpec,
    purpose: Purpose,
    viewer: ViewerState,
    fps: f64,
    realtime: bool,
    /// Render-and-replace: layers composite Normal (see [`ClipJob::op`]).
    normal_only: bool,
    /// A playback session's file snapshot (no stat per frame).
    files: Option<Arc<FileSnapshot>>,
}

impl FrameEnv<'_> {
    /// Why request `req` has no frame, in words for the user.
    fn no_frame(&self, req: usize) -> (SkipKind, String) {
        let r = &self.reqs[req];
        let offline = |path: &str| {
            let why = format!("media is offline: {} can't be found", file_name(path));
            (SkipKind::MissingMedia, why)
        };
        let files = self.files.as_deref();
        if !file_exists(files, Path::new(&r.path)) {
            return offline(&r.path);
        }
        // A proxy that didn't decode falls back to its original: when that
        // is missing too, the original being offline is the news.
        if let Some(m) = self.project.media.get(&r.media) {
            let original = m.path.to_string_lossy();
            if original != r.path && !file_exists(files, &m.path) {
                return offline(&original);
            }
        }
        let name = file_name(&r.path);
        let why = match &self.failures[req] {
            Some(why) => format!("couldn't decode {name} ({why})"),
            None => format!("no frame of {name} was ready in time"),
        };
        (SkipKind::NoFrame, why)
    }
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned())
}

/// Render `plan` into the compositor: one layer pass per visible clip
/// (add-ons per layer), composited bottom → top, ready for a `finish_*`.
/// An empty plan leaves the cleared (black) canvas. A layer whose frame the
/// provider can't supply is skipped, not an error; the report says why.
pub fn compose_frame(
    c: &mut Compositor,
    plan: &ScenePlan,
    prov: &mut dyn FrameProvider,
    ctx: &mut ComposeCtx<'_>,
    out: &OutputSpec,
) -> Result<ComposeReport, EngineError> {
    c.begin_frame(out)?;
    let (project, seq) = (ctx.project, ctx.seq);
    let files = ctx.res.files.clone();
    let (jobs, reqs) = plan_jobs_with(
        project,
        seq,
        plan,
        ctx.purpose,
        ctx.track_filter,
        files.as_deref(),
    );
    let mut frames = prov.frames(&reqs);
    // A short answer means "no frame" for the rest.
    frames.resize_with(reqs.len(), || None);
    let failures: Vec<Option<String>> = reqs
        .iter()
        .zip(&frames)
        .map(|(r, f)| if f.is_none() { prov.failure(r) } else { None })
        .collect();
    let env = FrameEnv {
        project,
        reqs: &reqs,
        frames: &frames,
        failures: &failures,
        out,
        purpose: ctx.purpose,
        viewer: ctx.viewer,
        fps: seq.frame_rate.0.as_f64(),
        realtime: ctx.realtime,
        normal_only: ctx.track_filter.is_some(),
        files,
    };
    let res = &mut *ctx.res;
    let mut report = ComposeReport::default();
    let skipped = &mut report.skipped;
    for job in &jobs {
        match job {
            Job::Clip(j) => {
                if let Some(h) = render_clip(c, res, &env, j, skipped)? {
                    c.composite(h, j.op(env.normal_only))?;
                }
            }
            Job::Transition(t) => composite_transition(c, res, &env, t, skipped)?,
        }
    }
    c.retain_only(&live_keys(&jobs));
    Ok(report)
}

/// Layer pass + add-ons for one clip; `None` when it has no picture this
/// frame (and why, in `skipped`).
fn render_clip(
    c: &mut Compositor,
    res: &mut ComposeResources,
    env: &FrameEnv<'_>,
    job: &ClipJob<'_>,
    skipped: &mut Vec<SkippedLayer>,
) -> Result<Option<LayerHandle>, EngineError> {
    let clip_t = job.clip_t();
    let key = LayerKey::from(job.layer.clip_id);
    let mut params = layer_params_for(
        env.project,
        job.clip,
        clip_t,
        env.purpose,
        &env.viewer,
        &res.luts,
    );
    let h = match &job.source {
        Source::Missing => {
            warn_once("clip media missing from the library; layer skipped", key.0);
            let why = "its media is missing from the library".into();
            skipped.push(job.skipped(SkipKind::MissingMedia, why));
            return Ok(None);
        }
        Source::File { req } => {
            let Some(frame) = env.frames[*req].as_ref() else {
                warn_once("no decoded frame for layer; skipped", key.0);
                let (kind, why) = env.no_frame(*req);
                skipped.push(job.skipped(kind, why));
                return Ok(None);
            };
            let media = job.layer.media;
            apply_frame(
                &mut params,
                env.project,
                media,
                frame,
                res.media_dims(media),
            );
            let display_w = frame.width as f32 * params.pixel_aspect;
            round_default_windows(
                &mut params,
                &job.clip.effects,
                clip_t,
                display_w,
                frame.height,
            );
            c.render_layer(key, LayerSource::Yuv(frame), &params)?
        }
        Source::Title(p) => match res.synth.title(p) {
            Ok(img) => {
                let fx = &job.clip.effects;
                round_default_windows(&mut params, fx, clip_t, img.width as f32, img.height);
                c.render_layer(key, LayerSource::Rgba(&img), &params)?
            }
            Err(e) => {
                tracing::debug!(?e, "title render failed");
                warn_once("title render failed; layer skipped", key.0);
                let why = format!("couldn't render the title ({e})");
                skipped.push(job.skipped(SkipKind::RenderFailed, why));
                return Ok(None);
            }
        },
        Source::MoGraph(inst) => {
            let img = render_mograph_rgba(
                res.mograph(),
                inst,
                clip_t.max(0.0),
                env.fps,
                inst.template.meta.default_duration_s,
            );
            if img.width == 0 || img.height == 0 {
                let why = "couldn't render the motion graphic (no size)".into();
                skipped.push(job.skipped(SkipKind::RenderFailed, why));
                return Ok(None);
            }
            let fx = &job.clip.effects;
            round_default_windows(&mut params, fx, clip_t, img.width as f32, img.height);
            c.render_layer(key, LayerSource::Rgba(&img), &params)?
        }
        Source::Fusion { comp, inputs } => {
            let comp_t = clip_t.max(0.0);
            let map = fusion_inputs(c, res, env, job, inputs, comp_t, skipped)?;
            let img = render_fusion_rgba(res.fusion(), comp, comp_t, &map);
            if img.width == 0 || img.height == 0 {
                let why = "couldn't render the Fusion comp (no size)".into();
                skipped.push(job.skipped(SkipKind::RenderFailed, why));
                return Ok(None);
            }
            let fx = &job.clip.effects;
            round_default_windows(&mut params, fx, clip_t, img.width as f32, img.height);
            c.render_layer(key, LayerSource::Rgba(&img), &params)?
        }
    };
    for reason in run_addons(c, &res.addons, env, h, &job.clip.effects, clip_t)? {
        skipped.push(job.skipped(SkipKind::AddonFailed, reason));
    }
    Ok(Some(h))
}

/// The clip's `wasm:` / `shader:` chain on the layer: read back, process on
/// the CPU, write back. Returns why any add-on didn't run.
fn run_addons(
    c: &mut Compositor,
    host: &Mutex<AddonHost>,
    env: &FrameEnv<'_>,
    h: LayerHandle,
    effects: &[Effect],
    clip_t: f64,
) -> Result<Vec<String>, EngineError> {
    // Intentional: the add-ons work on 8-bit sRGB; on an HDR export the
    // read-back would clip the layer's range, so they're skipped (as before
    // ADR 0007).
    if env.out.hdr || !addons::has_addons(effects) {
        return Ok(Vec::new());
    }
    let mut img = c.read_layer_rgba8(h)?;
    let failures = {
        let mut host = host.lock();
        let (w, hgt) = (img.width, img.height);
        // Playback skips the render cache: every played frame is a new
        // input, so caching it only costs a disk write and memory.
        if env.purpose == Purpose::Preview && !env.realtime {
            host.apply_effects_cached(&mut img.pixels, w, hgt, effects, clip_t)
        } else {
            host.apply_effects(&mut img.pixels, w, hgt, effects, clip_t)
        }
    };
    c.write_layer_rgba8(h, &img)?;
    Ok(failures)
}

/// The `MediaIn` images of one Fusion comp (clip `job`), by name. File
/// inputs go through the layer pass (so they get the media's colour
/// pipeline) and are read back without the canvas-fit bars.
fn fusion_inputs(
    c: &mut Compositor,
    res: &mut ComposeResources,
    env: &FrameEnv<'_>,
    job: &ClipJob<'_>,
    inputs: &[MediaInput<'_>],
    comp_t: f64,
    skipped: &mut Vec<SkippedLayer>,
) -> Result<HashMap<String, RgbaImage>, EngineError> {
    let mut map = HashMap::new();
    let input_skipped = |media: MediaItemId, (kind, reason): (SkipKind, String)| SkippedLayer {
        clip_id: job.layer.clip_id,
        media,
        kind,
        reason: format!("a Fusion input is missing: {reason}"),
    };
    for inp in inputs {
        let img = match &inp.kind {
            InputKind::File { req } => {
                let Some(frame) = env.frames[*req].as_ref() else {
                    warn_once(
                        "no decoded frame for fusion MediaIn; input left empty",
                        inp.key.0,
                    );
                    skipped.push(input_skipped(inp.media, env.no_frame(*req)));
                    continue;
                };
                let mut params =
                    plain_layer_params(layer_pipeline(env.project, inp.media, env.purpose));
                let orig = res.media_dims(inp.media);
                apply_frame(&mut params, env.project, inp.media, frame, orig);
                let h = c.render_layer(inp.key, LayerSource::Yuv(frame), &params)?;
                // The picture's display size (anamorphic pixels widened).
                let display_w = ((frame.width as f32 * params.pixel_aspect).round() as u32).max(1);
                let fitted = crop_to_fit(c.read_layer_rgba8(h)?, display_w, frame.height);
                if inp.native {
                    let (w, h) = native_size(display_w, frame.height, orig);
                    resample(fitted, w, h)
                } else {
                    fitted
                }
            }
            InputKind::Title(p) => match res.synth.title(p) {
                Ok(img) => (*img).clone(),
                Err(e) => {
                    let why = format!("couldn't render the title ({e})");
                    skipped.push(input_skipped(inp.media, (SkipKind::RenderFailed, why)));
                    continue;
                }
            },
            InputKind::MoGraph(inst) => render_mograph_rgba(
                res.mograph(),
                inst,
                comp_t,
                env.fps,
                inst.template.meta.default_duration_s,
            ),
        };
        map.insert(inp.name.clone(), img);
    }
    Ok(map)
}

/// The native (display) size of a `MediaIn` picture `display_w`×`frame_h`:
/// the original's when the frame is a smaller proxy of it (`orig`, stored
/// size), so `Fit::None` places it as large in preview as in export.
fn native_size(display_w: u32, frame_h: u32, orig: Option<(u32, u32)>) -> (u32, u32) {
    match orig {
        Some((_, orig_h)) if frame_h > 0 && orig_h > frame_h => {
            // A proxy keeps the display aspect (decision 5), so the height
            // ratio scales the width too.
            let s = orig_h as f64 / frame_h as f64;
            (((display_w as f64 * s).round() as u32).max(1), orig_h)
        }
        _ => (display_w, frame_h),
    }
}

/// Cut the picture out of a canvas-sized layer read-back: the layer pass
/// fits a `src_w`×`src_h` source into the canvas with transparent bars.
fn crop_to_fit(img: RgbaImage, src_w: u32, src_h: u32) -> RgbaImage {
    let (cw, ch) = (img.width, img.height);
    if src_w == 0 || src_h == 0 || cw == 0 || ch == 0 {
        return img;
    }
    let s = (cw as f64 / src_w as f64).min(ch as f64 / src_h as f64);
    let (fw, fh) = (src_w as f64 * s, src_h as f64 * s);
    let (x0, y0) = ((cw as f64 - fw) / 2.0, (ch as f64 - fh) / 2.0);
    // Round inward: a partly covered edge pixel is bar, not picture.
    const EPS: f64 = 1e-6;
    let x_start = ((x0 - EPS).ceil().max(0.0) as u32).min(cw - 1);
    let y_start = ((y0 - EPS).ceil().max(0.0) as u32).min(ch - 1);
    let x_end = ((x0 + fw + EPS).floor() as u32).clamp(x_start + 1, cw);
    let y_end = ((y0 + fh + EPS).floor() as u32).clamp(y_start + 1, ch);
    if (x_start, y_start, x_end, y_end) == (0, 0, cw, ch) {
        return img;
    }
    let (w, h) = (x_end - x_start, y_end - y_start);
    let mut out = RgbaImage::new(w, h);
    let row = w as usize * 4;
    for y in 0..h as usize {
        let src = ((y_start as usize + y) * cw as usize + x_start as usize) * 4;
        out.pixels[y * row..(y + 1) * row].copy_from_slice(&img.pixels[src..src + row]);
    }
    out
}

/// `img` stretched to `w`×`h` (a no-op at that size already).
fn resample(img: RgbaImage, w: u32, h: u32) -> RgbaImage {
    if (img.width, img.height) == (w, h) {
        return img;
    }
    let mut out = RgbaImage::new(w, h);
    vance_fusion::ops::place_media(&mut out, &img, Fit::Stretch);
    out
}

/// Both sides of a transition as full layers, combined on one track.
fn composite_transition(
    c: &mut Compositor,
    res: &mut ComposeResources,
    env: &FrameEnv<'_>,
    t: &TransitionJob<'_>,
    skipped: &mut Vec<SkippedLayer>,
) -> Result<(), EngineError> {
    let (op_a, op_b) = (t.from.op(env.normal_only), t.to.op(env.normal_only));
    let ha = render_clip(c, res, env, &t.from, skipped)?;
    let hb = render_clip(c, res, env, &t.to, skipped)?;
    let (a, b) = match (ha, hb) {
        (Some(a), Some(b)) => (a, b),
        // A dissolve against a side with no picture is the other side at
        // its share of the mix (the composite is affine in opacity).
        (Some(a), None) => return Ok(c.composite(a, with_opacity_share(op_a, 1.0 - t.mix))?),
        (None, Some(b)) => return Ok(c.composite(b, with_opacity_share(op_b, t.mix))?),
        (None, None) => return Ok(()),
    };
    match transition_path(t.kind, env.out.hdr) {
        TransitionPath::Dissolve => c.composite_dissolve((a, op_a), (b, op_b), t.mix)?,
        TransitionPath::Plugin(id) => {
            let mut img_a = c.read_layer_rgba8(a)?;
            let img_b = c.read_layer_rgba8(b)?;
            if (img_a.width, img_a.height) != (img_b.width, img_b.height) {
                c.composite_dissolve((a, op_a), (b, op_b), t.mix)?;
                return Ok(());
            }
            let (w, h) = (img_a.width, img_a.height);
            let failed = res.addons.lock().apply_transition(
                &mut img_a.pixels,
                &img_b.pixels,
                w,
                h,
                id,
                t.params,
                t.mix,
                t.to.clip_t(),
            );
            if let Some(why) = failed {
                skipped.push(t.to.skipped(SkipKind::AddonFailed, why));
            }
            c.write_layer_rgba8(a, &img_a)?;
            c.composite(a, dominant_op(op_a, op_b, t.mix))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vance_core::FrameRate;
    use vance_timeline::{ops, Keyframe, KeyframeInterp, TrackKind};

    fn rt(s: f64) -> RationalTime {
        RationalTime::new((s * 1000.0).round() as i64, 1000).unwrap()
    }

    fn num(v: f64) -> EffectValue {
        EffectValue::Number(v)
    }

    fn ramp(t0: f64, v0: f64, t1: f64, v1: f64) -> EffectValue {
        EffectValue::Keyframes {
            keyframes: vec![
                Keyframe {
                    time_seconds: t0,
                    value: v0,
                    interp: KeyframeInterp::Linear,
                },
                Keyframe {
                    time_seconds: t1,
                    value: v1,
                    interp: KeyframeInterp::Linear,
                },
            ],
        }
    }

    fn effect(kind: &str, params: &[(&str, EffectValue)]) -> Effect {
        let mut e = Effect::new(kind);
        for (k, v) in params {
            e.params.insert((*k).to_string(), v.clone());
        }
        e
    }

    fn file_media(name: &str) -> MediaItem {
        MediaItem::new(
            PathBuf::from(format!("{name}.mp4")),
            name,
            MediaKind::Video,
            rt(10.0),
        )
    }

    fn title_media(name: &str, rgb: [f32; 3], w: u32, h: u32) -> MediaItem {
        MediaItem::new_title(
            name,
            rt(10.0),
            TitleParams {
                text: String::new(),
                size_px: 24.0,
                fg: [1.0; 3],
                bg: rgb,
                width: w,
                height: h,
                bg_alpha: 1.0,
            },
        )
    }

    fn project_with(media: Vec<MediaItem>) -> Project {
        let mut p = Project::new("t");
        for m in media {
            p.media.insert(m.id, Arc::new(m));
        }
        p
    }

    fn clip_on(media: MediaItemId, start: f64, dur: f64, effects: Vec<Effect>) -> Clip {
        let mut c = Clip::new(media, "c", rt(start), rt(0.0), rt(dur)).unwrap();
        c.effects = effects;
        c
    }

    fn no_luts() -> Mutex<LutCache> {
        Mutex::new(LutCache::new())
    }

    fn params_of(clip: &Clip, project: &Project, t: f64) -> LayerParams {
        layer_params_for(
            project,
            clip,
            t,
            Purpose::Preview,
            &ViewerState::default(),
            &no_luts(),
        )
    }

    /// A sequence with one video track per entry; each track holds the given
    /// clips (track 0 is the bottom layer).
    fn seq_with(tracks: Vec<Vec<Clip>>) -> (Sequence, Vec<TrackId>) {
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        let mut ids = Vec::new();
        for (i, clips) in tracks.into_iter().enumerate() {
            seq = ops::add_track(&seq, TrackKind::Video, format!("V{}", i + 1));
            let tid = seq.tracks.last().unwrap().id;
            ids.push(tid);
            for c in clips {
                seq = ops::insert_clip(&seq, tid, c).unwrap();
            }
        }
        (seq, ids)
    }

    // ── layer_params_for ──

    #[test]
    fn no_effects_is_identity_with_default_pipeline() {
        let m = file_media("a");
        let clip = clip_on(m.id, 0.0, 5.0, vec![]);
        let p = project_with(vec![m]);
        let lp = params_of(&clip, &p, 1.0);
        assert_eq!(lp.pipeline, default_pipeline());
        assert_eq!(
            (lp.translate, lp.scale, lp.rotation),
            ([0.0; 2], [1.0; 2], 0.0)
        );
        assert_eq!(lp.crop_lrtb, [0.0; 4]);
        assert_eq!((lp.brightness, lp.contrast), (0.0, 1.0));
        assert!(lp.chroma_key.is_none() && lp.luma_key.is_none() && lp.keyer.is_none());
        assert!(lp.grades.is_empty() && lp.lut.is_none() && !lp.matte_view);
    }

    #[test]
    fn transforms_stack_and_keyframes_interpolate() {
        let m = file_media("a");
        let clip = clip_on(
            m.id,
            0.0,
            5.0,
            vec![
                effect(
                    "transform",
                    &[
                        ("translate_x", ramp(0.0, 0.0, 2.0, 0.4)),
                        ("translate_y", num(-0.1)),
                        ("scale_x", num(2.0)),
                        ("rotation", num(90.0)),
                    ],
                ),
                effect(
                    "transform",
                    &[
                        ("translate_x", num(0.1)),
                        ("scale_x", num(0.5)),
                        ("scale_y", num(3.0)),
                    ],
                ),
                {
                    let mut off = effect("transform", &[("translate_x", num(9.0))]);
                    off.enabled = false;
                    off
                },
            ],
        );
        let p = project_with(vec![m]);
        let lp = params_of(&clip, &p, 1.0);
        assert!((lp.translate[0] - 0.3).abs() < 1e-6, "0.2 at t=1 plus 0.1");
        assert!((lp.translate[1] + 0.1).abs() < 1e-6);
        assert_eq!(lp.scale, [1.0, 3.0]);
        assert!((lp.rotation - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
    }

    #[test]
    fn crop_and_color_aggregate() {
        let m = file_media("a");
        let clip = clip_on(
            m.id,
            0.0,
            5.0,
            vec![
                effect("crop", &[("left", num(0.1)), ("top", num(0.2))]),
                effect(
                    "crop",
                    &[
                        ("left", num(0.05)),
                        ("right", num(0.3)),
                        ("bottom", ramp(0.0, 0.0, 1.0, 0.5)),
                    ],
                ),
                effect("color", &[("brightness", num(0.1)), ("contrast", num(1.5))]),
                effect(
                    "color",
                    &[("brightness", num(-0.3)), ("contrast", num(2.0))],
                ),
            ],
        );
        let p = project_with(vec![m]);
        let lp = params_of(&clip, &p, 0.5);
        let want = [0.15, 0.3, 0.2, 0.25];
        for (got, want) in lp.crop_lrtb.iter().zip(want) {
            assert!((got - want).abs() < 1e-6, "{:?}", lp.crop_lrtb);
        }
        assert!((lp.brightness + 0.2).abs() < 1e-6);
        assert!((lp.contrast - 3.0).abs() < 1e-6);
    }

    #[test]
    fn expressions_drive_params() {
        let m = file_media("a");
        let clip = clip_on(
            m.id,
            0.0,
            5.0,
            vec![effect(
                "transform",
                &[(
                    "translate_x",
                    EffectValue::Expr {
                        expr: "time * 0.25".into(),
                    },
                )],
            )],
        );
        let p = project_with(vec![m]);
        assert!((params_of(&clip, &p, 2.0).translate[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn legacy_keys_first_enabled_wins() {
        let m = file_media("a");
        let mut off = effect("chroma_key", &[("r", num(1.0))]);
        off.enabled = false;
        let clip = clip_on(
            m.id,
            0.0,
            5.0,
            vec![
                off,
                effect(
                    "chroma_key",
                    &[("r", num(2.0)), ("g", num(0.2)), ("tolerance", num(0.3))],
                ),
                effect("chroma_key", &[("r", num(0.0))]),
                effect(
                    "luma_key",
                    &[
                        ("threshold", num(0.7)),
                        ("direction", num(-1.0)),
                        ("softness", num(0.2)),
                    ],
                ),
            ],
        );
        let p = project_with(vec![m]);
        let lp = params_of(&clip, &p, 0.0);
        assert_eq!(
            lp.chroma_key,
            Some(([1.0, 0.2, 0.0], 0.3, 0.05)),
            "clamped, defaults kept"
        );
        assert_eq!(lp.luma_key, Some((0.7, -1.0, 0.2)));
    }

    #[test]
    fn keyer_defaults_and_every_key() {
        use vance_timeline::keyer as k;
        let m = file_media("a");
        let bare = clip_on(m.id, 0.0, 5.0, vec![effect(k::KEYER_KIND, &[])]);
        let p = project_with(vec![m.clone()]);
        assert_eq!(
            params_of(&bare, &p, 0.0).keyer,
            Some(KeyerParams {
                key_rgb: [0.0, 1.0, 0.0],
                balance: 0.5,
                strength: 1.0,
                clip_black: 0.0,
                clip_white: 0.9,
                choke_px: 0.0,
                soften_px: 0.0,
                spill: 1.0,
                spill_bias: 0.5,
                invert: false,
            })
        );
        let full = clip_on(
            m.id,
            0.0,
            5.0,
            vec![effect(
                k::KEYER_KIND,
                &[
                    (k::KEY_R, num(0.1)),
                    (k::KEY_G, num(0.2)),
                    (k::KEY_B, num(0.9)),
                    (k::KEY_BALANCE, num(0.3)),
                    (k::KEY_STRENGTH, ramp(0.0, 1.0, 2.0, 3.0)),
                    (k::KEY_CLIP_BLACK, num(0.1)),
                    (k::KEY_CLIP_WHITE, num(0.8)),
                    (k::KEY_CHOKE_PX, num(-2.0)),
                    (k::KEY_SOFTEN_PX, num(1.5)),
                    (k::KEY_SPILL, num(0.4)),
                    (k::KEY_SPILL_BIAS, num(1.0)),
                    (k::KEY_INVERT, num(1.0)),
                ],
            )],
        );
        assert_eq!(
            params_of(&full, &p, 1.0).keyer,
            Some(KeyerParams {
                key_rgb: [0.1, 0.2, 0.9],
                balance: 0.3,
                strength: 2.0,
                clip_black: 0.1,
                clip_white: 0.8,
                choke_px: -2.0,
                soften_px: 1.5,
                spill: 0.4,
                spill_bias: 1.0,
                invert: true,
            })
        );
        let mut off = effect(k::KEYER_KIND, &[]);
        off.enabled = false;
        let disabled = clip_on(m.id, 0.0, 5.0, vec![off]);
        assert_eq!(params_of(&disabled, &p, 0.0).keyer, None);
    }

    #[test]
    fn keyer_clamps_and_rejects_non_finite() {
        use vance_timeline::keyer as k;
        let m = file_media("a");
        let clip = clip_on(
            m.id,
            0.0,
            5.0,
            vec![effect(
                k::KEYER_KIND,
                &[
                    (k::KEY_G, num(f64::NAN)),
                    (k::KEY_R, num(-1.0)),
                    (k::KEY_SOFTEN_PX, num(-4.0)),
                    (k::KEY_SPILL, num(7.0)),
                    (k::KEY_INVERT, EffectValue::Bool(true)),
                ],
            )],
        );
        let p = project_with(vec![m]);
        let kp = params_of(&clip, &p, 0.0).keyer.unwrap();
        assert_eq!(kp.key_rgb, [0.0, 1.0, 0.0]);
        assert_eq!((kp.soften_px, kp.spill, kp.invert), (0.0, 1.0, true));
    }

    #[test]
    fn grades_read_at_time_and_matte_is_preview_only() {
        use vance_timeline::grade as gk;
        let m = file_media("a");
        let g = effect(
            gk::GRADE_KIND,
            &[
                (gk::KEY_LIFT_R, ramp(0.0, 0.0, 1.0, 0.2)),
                (gk::KEY_GAIN_Y, num(1.5)),
            ],
        );
        let gid = g.id;
        let curves = effect(
            gk::GRADE_KIND,
            &[(gk::KEY_MODE, EffectValue::String(gk::MODE_CURVES.into()))],
        );
        let clip = clip_on(m.id, 0.0, 5.0, vec![g, curves]);
        let p = project_with(vec![m]);
        let v = ViewerState {
            matte_node: Some(gid),
            matte_view: Some(clip.id),
        };
        let prev = layer_params_for(&p, &clip, 0.5, Purpose::Preview, &v, &no_luts());
        assert_eq!(prev.grades.len(), 2);
        assert!((prev.grades[0].lift[0] - 0.1).abs() < 1e-6);
        assert_eq!(prev.grades[0].gain[3], 1.5);
        assert!(prev.grades[0].qual.show_mask && prev.grades[0].qual.enabled);
        assert_eq!(prev.grades[1].mode, vance_render::GradeMode::Curves);
        assert!(prev.grades[1].curves.is_some());
        assert!(prev.matte_view);
        let exp = layer_params_for(
            &p,
            &clip,
            0.5,
            Purpose::Export { hdr: false },
            &v,
            &no_luts(),
        );
        assert!(!exp.grades[0].qual.show_mask && !exp.grades[0].qual.enabled);
        assert!(!exp.matte_view, "exports ignore viewer solos");
        let other = ViewerState {
            matte_view: Some(ClipId::new()),
            ..Default::default()
        };
        assert!(!layer_params_for(&p, &clip, 0.5, Purpose::Preview, &other, &no_luts()).matte_view);
    }

    #[test]
    fn lut_is_loaded_once_and_cached() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id.cube");
        let mut cube = String::from("LUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    cube.push_str(&format!("{r} {g} {b}\n"));
                }
            }
        }
        std::fs::write(&path, cube).unwrap();
        let m = file_media("a");
        let lut_fx = |p: &str| effect("lut", &[("path", EffectValue::String(p.into()))]);
        let clip = clip_on(m.id, 0.0, 5.0, vec![lut_fx(path.to_str().unwrap())]);
        let project = project_with(vec![m.clone()]);
        let luts = no_luts();
        let v = ViewerState::default();
        let a = layer_params_for(&project, &clip, 0.0, Purpose::Preview, &v, &luts)
            .lut
            .unwrap();
        assert_eq!(a.size, 2);
        assert_eq!(luts.lock().len(), 1);
        let b = layer_params_for(&project, &clip, 0.0, Purpose::Preview, &v, &luts)
            .lut
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b), "second lookup is a cache hit");
        let empty = clip_on(m.id, 0.0, 5.0, vec![lut_fx("")]);
        assert!(
            layer_params_for(&project, &empty, 0.0, Purpose::Preview, &v, &luts)
                .lut
                .is_none()
        );
        let missing = clip_on(
            m.id,
            0.0,
            5.0,
            vec![lut_fx(dir.path().join("nope.cube").to_str().unwrap())],
        );
        assert!(
            layer_params_for(&project, &missing, 0.0, Purpose::Preview, &v, &luts)
                .lut
                .is_none()
        );
    }

    #[test]
    fn pipeline_follows_media_tag_and_purpose() {
        let mut pq = file_media("hdr");
        pq.input_color_space = Some(ColorSpace::Rec2020Pq);
        let mut log = file_media("log");
        log.input_color_space = Some(ColorSpace::Acescct);
        let title = title_media("t", [0.0; 3], 64, 32);
        let mut p = project_with(vec![pq.clone(), log.clone(), title.clone()]);
        let want = |input, output| pipeline_params(input, ColorSpace::LinearRec709, output);
        assert_eq!(
            layer_pipeline(&p, pq.id, Purpose::Preview),
            want(ColorSpace::Rec2020Pq, ColorSpace::Srgb)
        );
        assert_eq!(
            layer_pipeline(&p, pq.id, Purpose::Export { hdr: true }),
            want(ColorSpace::Rec2020Pq, ColorSpace::Rec2020Pq)
        );
        assert_eq!(
            layer_pipeline(&p, title.id, Purpose::Preview).input_transfer,
            1,
            "sRGB"
        );
        assert_eq!(
            layer_pipeline(&p, log.id, Purpose::Preview),
            default_pipeline()
        );
        assert_eq!(
            layer_pipeline(&p, MediaItemId::new(), Purpose::Preview),
            default_pipeline()
        );
        p.working_color_space = ColorSpace::Acescg;
        assert_ne!(
            layer_pipeline(&p, pq.id, Purpose::Preview).input_to_working,
            vance_color::Transform::IDENTITY,
            "2020 → AP1"
        );
    }

    // ── decoded-frame colour, pixel aspect, proxy scale ──

    fn hinted(range: ColorRangeHint, matrix: ColorMatrixHint) -> FrameColor {
        FrameColor {
            range,
            matrix,
            sample_aspect: (1, 1),
        }
    }

    #[test]
    fn frame_coding_table() {
        use vance_color::pipeline::MATRIX_BT709 as M709;
        use vance_color::pipeline::{MATRIX_BT2020_NCL as M2020, MATRIX_BT601 as M601};
        use ColorMatrixHint as M;
        use ColorRangeHint as R;
        use ColorSpace as Cs;
        let (lim, full) = (RANGE_LIMITED, RANGE_FULL);
        // (signalled range, signalled matrix, rgba, original lines, input space) → (range, matrix)
        let cases = [
            // Signalled hints win, whatever the space says.
            (R::Limited, M::Bt709, false, 1080, Cs::Srgb, (lim, M709)),
            (
                R::Full,
                M::Bt601,
                false,
                1080,
                Cs::Rec709Limited,
                (full, M601),
            ),
            (
                R::Full,
                M::Bt2020Ncl,
                false,
                2160,
                Cs::Rec2020Pq,
                (full, M2020),
            ),
            (
                R::Limited,
                M::Bt709,
                false,
                480,
                Cs::Rec709Limited,
                (lim, M709),
            ),
            // Unknown range: untagged / limited spaces are limited …
            (
                R::Unknown,
                M::Unknown,
                false,
                1080,
                Cs::Rec709Limited,
                (lim, M709),
            ),
            (
                R::Unknown,
                M::Unknown,
                false,
                2160,
                Cs::Rec2020Limited,
                (lim, M2020),
            ),
            // … PQ / HLG too (HDR10 code 64 is black, not grey) …
            (
                R::Unknown,
                M::Unknown,
                false,
                2160,
                Cs::Rec2020Pq,
                (lim, M2020),
            ),
            (
                R::Unknown,
                M::Unknown,
                false,
                2160,
                Cs::Rec2020Hlg,
                (lim, M2020),
            ),
            // … and a full range the user tagged is honoured.
            (
                R::Unknown,
                M::Unknown,
                false,
                1080,
                Cs::Rec709Full,
                (full, M709),
            ),
            (R::Unknown, M::Unknown, false, 1080, Cs::Srgb, (full, M709)),
            // Packed RGBA is full range whatever is signalled.
            (
                R::Limited,
                M::Unknown,
                true,
                1080,
                Cs::Rec709Limited,
                (full, M709),
            ),
            (R::Unknown, M::Unknown, true, 1080, Cs::Srgb, (full, M709)),
            // Unknown matrix: SD (≤ 576 lines) is BT.601, else the primaries'.
            (
                R::Unknown,
                M::Unknown,
                false,
                480,
                Cs::Rec709Limited,
                (lim, M601),
            ),
            (
                R::Unknown,
                M::Unknown,
                false,
                576,
                Cs::Rec709Limited,
                (lim, M601),
            ),
            (
                R::Unknown,
                M::Unknown,
                false,
                577,
                Cs::Rec709Limited,
                (lim, M709),
            ),
            (
                R::Unknown,
                M::Unknown,
                false,
                1080,
                Cs::DisplayP3,
                (full, M709),
            ),
        ];
        for (range, matrix, rgba, lines, space, want) in cases {
            assert_eq!(
                frame_coding(hinted(range, matrix), rgba, lines, space),
                want,
                "{range:?} {matrix:?} rgba={rgba} {lines} lines {space:?}"
            );
        }
    }

    /// A frame's description only: `apply_frame` never reads the pixels.
    fn yuv_frame(w: u32, h: u32, color: FrameColor) -> FrameData {
        let cw = w.div_ceil(2);
        FrameData {
            pts: 0,
            width: w,
            height: h,
            pixel_format: vance_render::PixelFormat::Yuv420P,
            strides: vec![w, cw, cw],
            bytes: Vec::new().into(),
            color,
        }
    }

    #[test]
    fn decoded_frames_set_coding_pixel_aspect_and_proxy_scale() {
        use vance_timeline::keyer as k;
        let mut hdr = file_media("hdr");
        hdr.input_color_space = Some(ColorSpace::Rec2020Pq);
        let keyed = clip_on(
            hdr.id,
            0.0,
            5.0,
            vec![effect(
                k::KEYER_KIND,
                &[(k::KEY_CHOKE_PX, num(4.0)), (k::KEY_SOFTEN_PX, num(2.0))],
            )],
        );
        let p = project_with(vec![hdr.clone()]);
        let base = params_of(&keyed, &p, 0.0);
        assert_eq!(
            base.pipeline.input_range, RANGE_FULL,
            "the space alone says full"
        );

        // HDR10 with nothing signalled, anamorphic 4:3 pixels, at full size.
        let mut sar = hinted(ColorRangeHint::Unknown, ColorMatrixHint::Unknown);
        sar.sample_aspect = (4, 3);
        let full = yuv_frame(3840, 2160, sar);
        let mut lp = base.clone();
        apply_frame(&mut lp, &p, hdr.id, &full, Some((3840, 2160)));
        assert_eq!(lp.pipeline.input_range, RANGE_LIMITED);
        assert_eq!(
            lp.pipeline.input_matrix,
            vance_color::pipeline::MATRIX_BT2020_NCL
        );
        assert!((lp.pixel_aspect - 4.0 / 3.0).abs() < 1e-6);
        let kp = lp.keyer.unwrap();
        assert_eq!(
            (kp.choke_px, kp.soften_px),
            (4.0, 2.0),
            "full size: unscaled"
        );

        // A half-resolution proxy: source-pixel params halve.
        let proxy = yuv_frame(
            1920,
            1080,
            hinted(ColorRangeHint::Full, ColorMatrixHint::Bt709),
        );
        let mut lp = base.clone();
        apply_frame(&mut lp, &p, hdr.id, &proxy, Some((3840, 2160)));
        let kp = lp.keyer.unwrap();
        assert_eq!((kp.choke_px, kp.soften_px), (2.0, 1.0));
        assert_eq!(lp.pipeline.input_range, RANGE_FULL, "signalled");
        assert_eq!(lp.pixel_aspect, 1.0);
        // Unknown original size: nothing to scale against.
        let mut lp = base.clone();
        apply_frame(&mut lp, &p, hdr.id, &proxy, None);
        assert_eq!(lp.keyer.unwrap().choke_px, 4.0);
    }

    #[test]
    fn a_power_window_without_a_width_is_round() {
        use vance_timeline::grade as gk;
        let m = file_media("a");
        let ellipse = |size: &[(&str, EffectValue)]| {
            let mut ps = vec![(gk::KEY_MODE, EffectValue::String("primary".into()))];
            ps.push((gk::KEY_WIN_SHAPE, num(1.0)));
            ps.extend(size.iter().cloned());
            effect(gk::GRADE_KIND, &ps)
        };
        let clip = clip_on(
            m.id,
            0.0,
            5.0,
            vec![
                ellipse(&[]),
                ellipse(&[(gk::KEY_WIN_SIZE_Y, num(0.4))]),
                ellipse(&[(gk::KEY_WIN_SIZE_X, num(0.3))]),
            ],
        );
        let p = project_with(vec![m.clone()]);
        let sizes = |display_w: f32, h: u32| {
            let mut lp = params_of(&clip, &p, 0.0);
            round_default_windows(&mut lp, &clip.effects, 0.0, display_w, h);
            lp.grades.iter().map(|g| g.window.size).collect::<Vec<_>>()
        };
        // 16:9: 0.25 of the height is 0.25 × 1080 / 1920 of the width.
        let hd = sizes(1920.0, 1080);
        assert_eq!(hd[0], [0.140625, 0.25], "round on 16:9");
        assert_eq!(hd[1], [0.225, 0.4], "round from the height that is set");
        assert_eq!(hd[2], [0.3, 0.25], "a set width is used as given");
        // Its half-size proxy: the same window.
        assert_eq!(sizes(960.0, 540), hd);
        // 720×480 with 4:3 pixels is 960 display pixels wide.
        let dv = sizes(720.0 * 4.0 / 3.0, 480);
        assert_eq!(dv[0], [0.125, 0.25]);
        // On the canvas: width × display width == height × height.
        let w = dv[0][0] * 960.0;
        assert!((w - dv[0][1] * 480.0).abs() < 1e-3, "a circle, {w} px wide");
    }

    #[test]
    fn media_in_native_size_is_the_originals() {
        // Full size, or the original unknown: the frame's own size.
        assert_eq!(native_size(640, 360, Some((640, 360))), (640, 360));
        assert_eq!(native_size(320, 180, None), (320, 180));
        // A half-size proxy: the original's display size.
        assert_eq!(native_size(320, 180, Some((640, 360))), (640, 360));
        // Anamorphic DV: stored 720×480, displayed 853 wide; its proxy is
        // square-pixel 426×240 (half the display size, even-rounded).
        assert_eq!(native_size(426, 240, Some((720, 480))), (852, 480));
        assert_eq!(native_size(853, 480, Some((720, 480))), (853, 480));
    }

    #[test]
    fn keyer_pixel_params_clamp_before_the_proxy_scale() {
        use vance_timeline::keyer as k;
        let m = file_media("a");
        let clip = clip_on(
            m.id,
            0.0,
            5.0,
            vec![effect(
                k::KEYER_KIND,
                &[(k::KEY_CHOKE_PX, num(100.0)), (k::KEY_SOFTEN_PX, num(90.0))],
            )],
        );
        let p = project_with(vec![m.clone()]);
        let export = params_of(&clip, &p, 0.0).keyer.unwrap();
        assert_eq!((export.choke_px, export.soften_px), (64.0, 64.0));
        let mut lp = params_of(&clip, &p, 0.0);
        let unknown = hinted(ColorRangeHint::Unknown, ColorMatrixHint::Unknown);
        apply_frame(
            &mut lp,
            &p,
            m.id,
            &yuv_frame(960, 540, unknown),
            Some((1920, 1080)),
        );
        let proxy = lp.keyer.unwrap();
        assert_eq!(
            (proxy.choke_px, proxy.soften_px),
            (32.0, 32.0),
            "half of the export's (clamped) 64, as the export draws it"
        );
        let shrink = clip_on(
            m.id,
            0.0,
            5.0,
            vec![effect(k::KEYER_KIND, &[(k::KEY_CHOKE_PX, num(-99.0))])],
        );
        assert_eq!(params_of(&shrink, &p, 0.0).keyer.unwrap().choke_px, -64.0);
    }

    #[test]
    fn audio_items_never_become_layers_or_media_ins() {
        let video = file_media("Shared name");
        let mut audio = file_media("Shared name");
        audio.kind = MediaKind::Audio;
        let mut music = file_media("music");
        music.kind = MediaKind::Audio;
        let v = clip_on(video.id, 0.0, 2.0, vec![]);
        let a = clip_on(music.id, 2.0, 2.0, vec![]);
        let v_id = v.id;
        let (seq, tracks) = seq_with(vec![vec![v, a]]);
        let (seq, _) =
            ops::add_transition_at_cut(&seq, tracks[0], v_id, TransitionKind::Dissolve, rt(1.0))
                .unwrap();
        let p = project_with(vec![video.clone(), audio.clone(), music.clone()]);
        let at = |t| {
            let plan = vance_timeline::scene_plan_at(&seq, rt(t));
            let (jobs, reqs) = plan_jobs(&p, &seq, &plan, Purpose::Export { hdr: false }, None);
            (jobs.len(), reqs.iter().map(|r| r.media).collect::<Vec<_>>())
        };
        assert_eq!(at(3.0), (0, vec![]), "the audio clip alone: nothing");
        assert_eq!(
            at(2.0),
            (1, vec![video.id]),
            "a dissolve into audio: the video side"
        );
        assert_eq!(
            resolve_media_in(&p, "Shared name").map(|m| m.id),
            Some(video.id),
            "the picture, not the same-named audio"
        );
        assert!(resolve_media_in(&p, &music.id.to_string()).is_none());
    }

    #[test]
    fn a_small_proxy_of_an_hd_original_keeps_the_hd_matrix() {
        let m = file_media("a");
        let clip = clip_on(m.id, 0.0, 5.0, vec![]);
        let p = project_with(vec![m.clone()]);
        let unknown = hinted(ColorRangeHint::Unknown, ColorMatrixHint::Unknown);
        let proxy = yuv_frame(960, 540, unknown);
        let mut lp = params_of(&clip, &p, 0.0);
        apply_frame(&mut lp, &p, m.id, &proxy, Some((1920, 1080)));
        assert_eq!(
            lp.pipeline.input_matrix,
            vance_color::pipeline::MATRIX_BT709
        );
        let sd = yuv_frame(720, 480, unknown);
        apply_frame(&mut lp, &p, m.id, &sd, None);
        assert_eq!(
            lp.pipeline.input_matrix,
            vance_color::pipeline::MATRIX_BT601
        );
    }

    #[test]
    fn render_and_replace_composites_normal_with_opacity() {
        let card = title_media("c", [1.0, 0.0, 0.0], 64, 32);
        let mut clip = clip_on(
            card.id,
            0.0,
            4.0,
            vec![effect("opacity", &[("value", num(0.5))])],
        );
        clip.blend_mode = vance_timeline::BlendMode::Screen;
        let (seq, _) = seq_with(vec![vec![clip]]);
        let p = project_with(vec![card]);
        let plan = vance_timeline::scene_plan_at(&seq, rt(1.0));
        let (jobs, _) = plan_jobs(&p, &seq, &plan, Purpose::Export { hdr: false }, None);
        let Job::Clip(j) = &jobs[0] else {
            panic!("clip job")
        };
        let screen = vance_timeline::BlendMode::Screen.gpu_id();
        assert_eq!(j.op(false).blend_mode, screen);
        let only = j.op(true);
        assert_eq!(only.blend_mode, vance_timeline::BlendMode::Normal.gpu_id());
        assert_eq!(
            only.opacity, 0.5,
            "opacity still applies (baked into alpha)"
        );
    }

    #[test]
    fn prefetched_answers_equal_requests_and_their_failures() {
        struct Half;
        impl FrameProvider for Half {
            fn frames(&mut self, reqs: &[FrameRequest]) -> Vec<Option<FrameData>> {
                vec![None; reqs.len()]
            }
            fn failure(&self, req: &FrameRequest) -> Option<String> {
                Some(format!("broken {}", req.path))
            }
        }
        let req = |path: &str| FrameRequest {
            clip_id: ClipId::new(),
            media: MediaItemId::new(),
            path: path.into(),
            source_seconds: 1.0,
        };
        let (a, b) = (req("a.mp4"), req("b.mp4"));
        let mut pre = Prefetched::fetch(&mut Half, vec![a.clone()]);
        assert_eq!(pre.frames(&[a.clone(), b.clone()]).len(), 2);
        assert_eq!(pre.failure(&a).as_deref(), Some("broken a.mp4"));
        assert_eq!(pre.failure(&b), None, "never fetched");
    }

    // ── output_spec_for ──

    #[test]
    fn output_spec_sdr_preview_and_export() {
        let p = Project::new("t");
        let mut seq = Sequence::new("S", FrameRate::FPS_24);
        let prev = output_spec_for(&p, &seq, Purpose::Preview);
        assert_eq!((prev.width, prev.height), (1920, 1080));
        assert_eq!(prev.working_to_output, vance_color::Transform::IDENTITY);
        assert_eq!(prev.output_transfer, 1, "sRGB");
        assert!(!prev.hdr && prev.view_lut);
        seq.frame_size = Some((1280, 720));
        let exp = output_spec_for(&p, &seq, Purpose::Export { hdr: false });
        assert_eq!((exp.width, exp.height), (1280, 720));
        assert_eq!(exp.output_transfer, 1);
        assert!(!exp.hdr && !exp.view_lut);
    }

    #[test]
    fn output_spec_hdr_export_is_rec2020_pq() {
        use vance_color::{transform::primaries_to_primaries, Primaries};
        let mut p = Project::new("t");
        let seq = Sequence::new("S", FrameRate::FPS_24);
        let hdr = output_spec_for(&p, &seq, Purpose::Export { hdr: true });
        assert!(hdr.hdr && !hdr.view_lut);
        assert_eq!(hdr.output_transfer, 4, "PQ");
        assert_eq!(
            hdr.working_to_output,
            primaries_to_primaries(Primaries::Bt709, Primaries::Bt2020)
        );
        p.working_color_space = ColorSpace::Acescg;
        assert_eq!(
            output_spec_for(&p, &seq, Purpose::Preview).working_to_output,
            primaries_to_primaries(Primaries::AcesAp1, Primaries::Bt709)
        );
        p.working_color_space = ColorSpace::Acescct;
        assert_eq!(
            output_spec_for(&p, &seq, Purpose::Preview).working_to_output,
            vance_color::Transform::IDENTITY,
            "unsupported working space falls back to linear Rec.709"
        );
    }

    // ── planning ──

    #[test]
    fn media_in_resolves_by_id_then_display_name() {
        let plate = file_media("Plate");
        let card = title_media("Card", [1.0, 0.0, 0.0], 64, 32);
        let comp = MediaItem::new_fusion("Comp", rt(5.0), vance_fusion::starter(64, 32));
        let mut named_like_id = file_media("x");
        named_like_id.display_name = card.id.to_string();
        let p = project_with(vec![
            plate.clone(),
            card.clone(),
            comp.clone(),
            named_like_id,
        ]);
        assert_eq!(
            resolve_media_in(&p, &plate.id.to_string()).unwrap().id,
            plate.id
        );
        assert_eq!(resolve_media_in(&p, "Plate").unwrap().id, plate.id);
        assert_eq!(resolve_media_in(&p, "Card").unwrap().id, card.id);
        assert_eq!(
            resolve_media_in(&p, &card.id.to_string()).unwrap().id,
            card.id,
            "an id beats a display name that happens to equal it"
        );
        assert!(
            resolve_media_in(&p, "Comp").is_none(),
            "comps are never inputs"
        );
        assert!(resolve_media_in(&p, &comp.id.to_string()).is_none());
        assert!(resolve_media_in(&p, "nobody").is_none());
    }

    #[test]
    fn plan_requests_both_transition_sides_and_honours_the_track_filter() {
        let (a, b, top) = (file_media("a"), file_media("b"), file_media("top"));
        let ca = clip_on(a.id, 0.0, 2.0, vec![]);
        let cb = clip_on(b.id, 2.0, 2.0, vec![]);
        let ct = clip_on(top.id, 0.0, 4.0, vec![]);
        let (ca_id, cb_id, ct_id) = (ca.id, cb.id, ct.id);
        let (seq, tracks) = seq_with(vec![vec![ca, cb], vec![ct]]);
        let (seq, _) =
            ops::add_transition_at_cut(&seq, tracks[0], ca_id, TransitionKind::Dissolve, rt(1.0))
                .unwrap();
        let p = project_with(vec![a.clone(), b.clone(), top]);
        let plan = vance_timeline::scene_plan_at(&seq, rt(1.75));
        let (jobs, reqs) = plan_jobs(&p, &seq, &plan, Purpose::Preview, None);
        assert_eq!(jobs.len(), 2);
        assert!(matches!(&jobs[0], Job::Transition(t) if (t.mix - 0.25).abs() < 1e-6));
        let ids: Vec<_> = reqs.iter().map(|r| r.clip_id).collect();
        assert_eq!(ids, vec![ca_id, cb_id, ct_id], "bottom → top, both sides");
        assert!((reqs[0].source_seconds - 1.75).abs() < 1e-9);
        assert!(
            (reqs[1].source_seconds + 0.25).abs() < 1e-9,
            "to-side pre-roll: t − timeline_in"
        );
        assert_eq!(reqs[0].path, "a.mp4");
        let keys = live_keys(&jobs);
        assert_eq!(keys.len(), 3);
        let (only, reqs) = plan_jobs(&p, &seq, &plan, Purpose::Preview, Some(tracks[1]));
        assert_eq!((only.len(), reqs.len()), (1, 1));
        assert_eq!(reqs[0].clip_id, ct_id);
    }

    #[test]
    fn synthetic_layers_need_no_frames_and_missing_media_is_kept() {
        let card = title_media("Card", [1.0; 3], 64, 32);
        let gone = MediaItemId::new();
        let (seq, _) = seq_with(vec![
            vec![clip_on(card.id, 0.0, 4.0, vec![])],
            vec![clip_on(gone, 0.0, 4.0, vec![])],
        ]);
        let p = project_with(vec![card]);
        let plan = vance_timeline::scene_plan_at(&seq, rt(1.0));
        let (jobs, reqs) = plan_jobs(&p, &seq, &plan, Purpose::Preview, None);
        assert!(reqs.is_empty());
        assert!(matches!(&jobs[0], Job::Clip(j) if matches!(j.source, Source::Title(_))));
        assert!(matches!(&jobs[1], Job::Clip(j) if matches!(j.source, Source::Missing)));
    }

    #[test]
    fn fusion_media_in_adds_a_request_at_comp_time() {
        let plate = file_media("Plate");
        let mut comp = vance_fusion::starter(64, 32);
        for (id, fit) in [("in1", Fit::Contain), ("in2", Fit::None)] {
            comp.nodes.push(vance_fusion::Node::new(
                id,
                id,
                NodeKind::MediaIn(vance_fusion::MediaSpec {
                    name: "Plate".into(),
                    fit,
                }),
            ));
        }
        comp.nodes.push(vance_fusion::Node::new(
            "in3",
            "in3",
            NodeKind::MediaIn(vance_fusion::MediaSpec {
                name: "missing".into(),
                fit: Fit::Contain,
            }),
        ));
        let fusion = MediaItem::new_fusion("Comp", rt(5.0), comp);
        let fclip = clip_on(fusion.id, 1.0, 4.0, vec![]);
        let fclip_id = fclip.id;
        let (seq, _) = seq_with(vec![vec![fclip]]);
        let p = project_with(vec![plate.clone(), fusion]);
        let plan = vance_timeline::scene_plan_at(&seq, rt(3.5));
        let (jobs, reqs) = plan_jobs(&p, &seq, &plan, Purpose::Preview, None);
        assert_eq!(reqs.len(), 1, "one request per named media, deduplicated");
        let r = &reqs[0];
        assert_eq!(r.media, plate.id);
        assert!(
            (r.source_seconds - 2.5).abs() < 1e-9,
            "the Fusion clip's time"
        );
        assert_eq!(r.clip_id, media_in_clip_id(fclip_id, plate.id));
        assert_ne!(r.clip_id, fclip_id);
        let Job::Clip(j) = &jobs[0] else {
            panic!("clip job")
        };
        let Source::Fusion { inputs, .. } = &j.source else {
            panic!("fusion source")
        };
        assert_eq!(inputs.len(), 1);
        assert!(inputs[0].native, "a Fit::None node asks for native size");
        assert_eq!(
            live_keys(&jobs),
            vec![LayerKey::from(fclip_id), LayerKey::from(r.clip_id)]
        );
    }

    #[test]
    fn media_in_ids_are_stable_and_distinct() {
        let (c1, c2, m) = (ClipId::new(), ClipId::new(), MediaItemId::new());
        assert_eq!(media_in_clip_id(c1, m), media_in_clip_id(c1, m));
        assert_ne!(media_in_clip_id(c1, m), media_in_clip_id(c2, m));
        assert_ne!(
            media_in_clip_id(c1, m),
            media_in_clip_id(c1, MediaItemId::new())
        );
    }

    #[test]
    fn proxies_are_used_only_when_enabled_and_present() {
        let dir = tempfile::tempdir().unwrap();
        let proxy = dir.path().join("p.mov");
        std::fs::write(&proxy, b"x").unwrap();
        let mut m = file_media("a");
        m.proxy_path = Some(proxy.clone());
        let mut p = project_with(vec![m.clone()]);
        let preview = Purpose::Preview;
        assert_eq!(media_path(&p, &m, preview), "a.mp4");
        p.use_proxies = true;
        assert_eq!(media_path(&p, &m, preview), proxy.to_string_lossy());
        m.proxy_path = Some(dir.path().join("gone.mov"));
        assert_eq!(media_path(&p, &m, preview), "a.mp4");
    }

    #[test]
    fn a_file_snapshot_answers_proxy_checks_without_a_stat() {
        let dir = tempfile::tempdir().unwrap();
        let proxy = dir.path().join("p.mov");
        std::fs::write(&proxy, b"x").unwrap();
        let mut m = file_media("a");
        m.proxy_path = Some(proxy.clone());
        let mut p = project_with(vec![m.clone()]);
        p.use_proxies = true;
        let snapshot = FileSnapshot::of(&p);
        assert!(snapshot.exists(&proxy));
        assert!(!snapshot.exists(&m.path), "a.mp4 isn't there");
        std::fs::remove_file(&proxy).unwrap();
        let preview = Purpose::Preview;
        assert_eq!(
            media_path_with(&p, &m, preview, Some(&snapshot)),
            proxy.to_string_lossy(),
            "the snapshot answers; no stat"
        );
        assert_eq!(media_path(&p, &m, preview), "a.mp4", "a stat sees it gone");
        // compose_frame reads the session's snapshot from its resources.
        let clip = clip_on(m.id, 0.0, 4.0, vec![]);
        let (seq, _) = seq_with(vec![vec![clip]]);
        let plan = vance_timeline::scene_plan_at(&seq, rt(1.0));
        let reqs = plan_requests_with(&p, &seq, &plan, preview, None, Some(&snapshot));
        assert_eq!(reqs[0].path, proxy.to_string_lossy());
    }

    #[test]
    fn a_proxy_on_a_dead_share_falls_back_without_hanging() {
        use crate::decoder_pool::fs_probe::tests::{block, checks, release};
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let proxy = dir.path().join("p.mov");
        std::fs::write(&proxy, b"x").unwrap();
        block(&proxy);
        let mut m = file_media("a");
        m.proxy_path = Some(proxy.clone());
        let mut p = project_with(vec![m.clone()]);
        p.use_proxies = true;
        let started = std::time::Instant::now();
        assert_eq!(
            media_path(&p, &m, Purpose::Preview),
            "a.mp4",
            "the original"
        );
        assert!(started.elapsed() < DEFAULT_TIMEOUT + Duration::from_secs(2));
        let again = std::time::Instant::now();
        assert!(
            !FileSnapshot::of(&p).exists(&proxy),
            "missing in a snapshot too"
        );
        assert_eq!(media_path(&p, &m, Purpose::Preview), "a.mp4");
        assert!(
            again.elapsed() < Duration::from_millis(50),
            "not checked again"
        );
        assert_eq!(checks(&proxy), 1);
        release(&proxy);
    }

    #[test]
    fn plan_luts_are_loaded_into_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("id.cube");
        let mut cube = String::from("LUT_3D_SIZE 2\n");
        for b in 0..2 {
            for g in 0..2 {
                for r in 0..2 {
                    cube.push_str(&format!("{r} {g} {b}\n"));
                }
            }
        }
        std::fs::write(&path, cube).unwrap();
        let m = file_media("a");
        let fx = vec![effect(
            "lut",
            &[("path", EffectValue::String(path.to_string_lossy().into()))],
        )];
        let (seq, _) = seq_with(vec![vec![clip_on(m.id, 0.0, 4.0, fx)]]);
        let plan = vance_timeline::scene_plan_at(&seq, rt(1.0));
        let luts = no_luts();
        load_plan_luts(&seq, &plan, &luts);
        assert!(
            luts.lock().contains_key(&path),
            "read before the render lock"
        );
    }

    #[test]
    fn export_always_decodes_the_original() {
        let dir = tempfile::tempdir().unwrap();
        let proxy = dir.path().join("p.mov");
        std::fs::write(&proxy, b"x").unwrap();
        let mut m = file_media("a");
        m.proxy_path = Some(proxy.clone());
        let mut p = project_with(vec![m.clone()]);
        p.use_proxies = true;
        for hdr in [false, true] {
            assert_eq!(media_path(&p, &m, Purpose::Export { hdr }), "a.mp4");
        }
        let clip = clip_on(m.id, 0.0, 4.0, vec![]);
        let (seq, _) = seq_with(vec![vec![clip]]);
        let plan = vance_timeline::scene_plan_at(&seq, rt(1.0));
        let export = plan_requests(&p, &seq, &plan, Purpose::Export { hdr: false }, None);
        assert_eq!(export[0].path, "a.mp4", "export requests name the original");
        let preview = plan_requests(&p, &seq, &plan, Purpose::Preview, None);
        assert_eq!(
            preview[0].path,
            proxy.to_string_lossy(),
            "preview honours proxies"
        );
    }

    #[test]
    fn wasm_transition_dominance_and_hdr_fallback() {
        let from = CompositeOp {
            blend_mode: 3,
            opacity: 0.4,
        };
        let to = CompositeOp {
            blend_mode: 0,
            opacity: 1.0,
        };
        assert_eq!(dominant_op(from, to, 0.0), from);
        assert_eq!(dominant_op(from, to, 0.499), from);
        assert_eq!(dominant_op(from, to, 0.5), to);
        assert_eq!(dominant_op(from, to, 1.0), to);
        let wasm = TransitionKind::Wasm("com.x.wipe".into());
        assert_eq!(
            transition_path(&wasm, false),
            TransitionPath::Plugin("com.x.wipe")
        );
        assert_eq!(transition_path(&wasm, true), TransitionPath::Dissolve);
        assert_eq!(
            transition_path(&TransitionKind::Dissolve, false),
            TransitionPath::Dissolve
        );
        assert_eq!(with_opacity_share(from, 0.5).opacity, 0.2);
        assert_eq!(with_opacity_share(from, 0.5).blend_mode, 3);
    }

    #[test]
    fn crop_to_fit_removes_the_bars() {
        // A 4:3 source fitted into 16×9 occupies x ∈ [2, 14) after inward
        // rounding of [2.0, 14.0).
        let mut img = RgbaImage::new(16, 9);
        for (i, px) in img.pixels.chunks_exact_mut(4).enumerate() {
            let x = (i % 16) as u8;
            px.copy_from_slice(&[x, 0, 0, 255]);
        }
        let out = crop_to_fit(img.clone(), 4, 3);
        assert_eq!((out.width, out.height), (12, 9));
        assert_eq!(out.pixels[0], 2, "first column is canvas x = 2");
        assert_eq!(
            crop_to_fit(img.clone(), 16, 9).pixels,
            img.pixels,
            "same aspect untouched"
        );
        // 32×9 into 16×9: rows [2.25, 6.75) round inward to [3, 6).
        let wide = crop_to_fit(RgbaImage::new(16, 9), 32, 9);
        assert_eq!((wide.width, wide.height), (16, 3));
        assert_eq!(resample(out, 4, 3).pixels.len(), 4 * 3 * 4);
    }

    /// `compose_frame` against the real compositor (seam R).
    mod gpu {
        use super::*;
        use vance_render::{Renderer, RendererConfig};

        const W: u32 = 64;
        const H: u32 = 32;

        fn renderer() -> Option<Renderer> {
            match Renderer::new(RendererConfig::default()) {
                Ok(r) => Some(r),
                Err(e) => {
                    eprintln!("no GPU ({e}) — skipping");
                    None
                }
            }
        }

        fn resources() -> ComposeResources {
            // One set of (lazily built) renderers for every test: each loads
            // the font database.
            type Engines = (
                LazyShared<MographEngine>,
                LazyShared<vance_fusion::Compositor>,
            );
            static ENGINES: OnceLock<Engines> = OnceLock::new();
            let (mograph, fusion) = ENGINES.get_or_init(Default::default).clone();
            ComposeResources::new(
                Arc::new(Mutex::new(AddonHost::default())),
                Arc::new(Mutex::new(LutCache::new())),
                mograph,
                fusion,
            )
        }

        #[derive(Default)]
        struct FakeProvider {
            by_media: HashMap<MediaItemId, FrameData>,
            failing: HashMap<MediaItemId, String>,
            calls: Vec<Vec<FrameRequest>>,
        }

        impl FrameProvider for FakeProvider {
            fn frames(&mut self, reqs: &[FrameRequest]) -> Vec<Option<FrameData>> {
                self.calls.push(reqs.to_vec());
                reqs.iter()
                    .map(|r| self.by_media.get(&r.media).cloned())
                    .collect()
            }

            fn failure(&self, req: &FrameRequest) -> Option<String> {
                self.failing.get(&req.media).cloned()
            }
        }

        fn solid_yuv(w: u32, h: u32, y: u8, u: u8, v: u8) -> FrameData {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let mut b = vec![y; (w * h) as usize];
            b.extend(std::iter::repeat_n(u, (cw * ch) as usize));
            b.extend(std::iter::repeat_n(v, (cw * ch) as usize));
            FrameData {
                pts: 0,
                width: w,
                height: h,
                pixel_format: vance_render::PixelFormat::Yuv420P,
                strides: vec![w, cw, cw],
                bytes: b.into(),
                color: Default::default(),
            }
        }

        struct Run<'a> {
            project: &'a Project,
            seq: &'a Sequence,
            t: f64,
            filter: Option<TrackId>,
            res: &'a mut ComposeResources,
        }

        /// Compose `seq` at `t` into a `W`×`H` SDR export frame.
        fn compose(r: &mut Renderer, run: Run<'_>, prov: &mut FakeProvider) -> Vec<u8> {
            let mut seq = run.seq.clone();
            seq.frame_size = Some((W, H));
            let purpose = Purpose::Export { hdr: false };
            let out = output_spec_for(run.project, &seq, purpose);
            let plan = vance_timeline::scene_plan_at(&seq, rt(run.t));
            let mut ctx = ComposeCtx {
                project: run.project,
                seq: &seq,
                purpose,
                viewer: ViewerState::default(),
                track_filter: run.filter,
                realtime: false,
                res: run.res,
            };
            let c = r.compositor();
            compose_frame(c, &plan, prov, &mut ctx, &out).expect("compose_frame");
            c.finish_to_rgba8().expect("finish_to_rgba8")
        }

        /// Compose `seq` at `t` for the viewer (`realtime`: as playback
        /// does) into a `W`×`H` frame; also returns the report.
        fn compose_preview(
            r: &mut Renderer,
            run: Run<'_>,
            prov: &mut dyn FrameProvider,
            realtime: bool,
        ) -> (Vec<u8>, ComposeReport) {
            let mut seq = run.seq.clone();
            seq.frame_size = Some((W, H));
            let out = output_spec_for(run.project, &seq, Purpose::Preview);
            let plan = vance_timeline::scene_plan_at(&seq, rt(run.t));
            let mut ctx = ComposeCtx {
                project: run.project,
                seq: &seq,
                purpose: Purpose::Preview,
                viewer: ViewerState::default(),
                track_filter: run.filter,
                realtime,
                res: run.res,
            };
            let c = r.compositor();
            let report = compose_frame(c, &plan, prov, &mut ctx, &out).expect("compose_frame");
            (c.finish_to_rgba8().expect("finish_to_rgba8"), report)
        }

        fn px(buf: &[u8], x: u32, y: u32) -> [u8; 4] {
            let i = ((y * W + x) * 4) as usize;
            [buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]
        }

        fn assert_near(got: [u8; 4], want: [u8; 4], tol: u8, what: &str) {
            for (g, w) in got.iter().zip(want) {
                assert!(
                    g.abs_diff(w) <= tol,
                    "{what}: got {got:?}, want {want:?} ±{tol}"
                );
            }
        }

        #[test]
        fn empty_plan_is_opaque_black_and_asks_once() {
            let Some(mut r) = renderer() else { return };
            let p = Project::new("t");
            let (seq, _) = seq_with(vec![vec![]]);
            let mut prov = FakeProvider::default();
            let mut res = resources();
            let run = Run {
                project: &p,
                seq: &seq,
                t: 1.0,
                filter: None,
                res: &mut res,
            };
            let buf = compose(&mut r, run, &mut prov);
            assert!(buf.chunks_exact(4).all(|p| p == [0, 0, 0, 255]));
            assert_eq!(prov.calls, vec![Vec::<FrameRequest>::new()]);
        }

        #[test]
        fn opaque_title_card_fills_the_canvas() {
            let Some(mut r) = renderer() else { return };
            let card = title_media("red", [1.0, 0.0, 0.0], W, H);
            let (seq, _) = seq_with(vec![vec![clip_on(card.id, 0.0, 4.0, vec![])]]);
            let p = project_with(vec![card]);
            let mut res = resources();
            let run = Run {
                project: &p,
                seq: &seq,
                t: 1.0,
                filter: None,
                res: &mut res,
            };
            let buf = compose(&mut r, run, &mut FakeProvider::default());
            assert_near(px(&buf, W / 2, H / 2), [255, 0, 0, 255], 1, "centre");
            assert_near(px(&buf, 0, 0), [255, 0, 0, 255], 1, "corner");
            assert_eq!(res.synth.cached_titles(), 1);
        }

        #[test]
        fn half_opacity_white_over_black_blends_display_referred() {
            let Some(mut r) = renderer() else { return };
            let card = title_media("white", [1.0; 3], W, H);
            let fx = vec![effect("opacity", &[("value", num(0.5))])];
            let (seq, _) = seq_with(vec![vec![clip_on(card.id, 0.0, 4.0, fx)]]);
            let p = project_with(vec![card]);
            let mut res = resources();
            let run = Run {
                project: &p,
                seq: &seq,
                t: 1.0,
                filter: None,
                res: &mut res,
            };
            let buf = compose(&mut r, run, &mut FakeProvider::default());
            // Display-referred (user decision 2): 0.5 × 255, not linear 188.
            assert_near(px(&buf, W / 2, H / 2), [128, 128, 128, 255], 2, "50% white");
        }

        #[test]
        fn dissolve_mixes_both_sides() {
            let Some(mut r) = renderer() else { return };
            let red = title_media("red", [1.0, 0.0, 0.0], W, H);
            let blue = title_media("blue", [0.0, 0.0, 1.0], W, H);
            let a = clip_on(red.id, 0.0, 2.0, vec![]);
            let a_id = a.id;
            let (seq, tracks) = seq_with(vec![vec![a, clip_on(blue.id, 2.0, 2.0, vec![])]]);
            let (seq, _) = ops::add_transition_at_cut(
                &seq,
                tracks[0],
                a_id,
                TransitionKind::Dissolve,
                rt(2.0),
            )
            .unwrap();
            let p = project_with(vec![red, blue]);
            let mut res = resources();
            let run = Run {
                project: &p,
                seq: &seq,
                t: 2.0,
                filter: None,
                res: &mut res,
            };
            let buf = compose(&mut r, run, &mut FakeProvider::default());
            assert_near(px(&buf, W / 2, H / 2), [128, 0, 128, 255], 2, "mix 0.5");
        }

        #[test]
        fn missing_frame_skips_only_that_layer() {
            let Some(mut r) = renderer() else { return };
            let card = title_media("red", [1.0, 0.0, 0.0], W, H);
            let plate = file_media("plate");
            let (seq, _) = seq_with(vec![
                vec![clip_on(card.id, 0.0, 4.0, vec![])],
                vec![clip_on(plate.id, 0.0, 4.0, vec![])],
            ]);
            let p = project_with(vec![card, plate]);
            let mut prov = FakeProvider::default();
            let mut res = resources();
            let run = Run {
                project: &p,
                seq: &seq,
                t: 1.0,
                filter: None,
                res: &mut res,
            };
            let buf = compose(&mut r, run, &mut prov);
            assert_near(
                px(&buf, W / 2, H / 2),
                [255, 0, 0, 255],
                1,
                "lower track shows",
            );
            assert_eq!(prov.calls.len(), 1);
            assert_eq!(prov.calls[0].len(), 1);
        }

        #[test]
        fn track_filter_renders_only_that_track() {
            let Some(mut r) = renderer() else { return };
            let red = title_media("red", [1.0, 0.0, 0.0], W, H);
            let blue = title_media("blue", [0.0, 0.0, 1.0], W, H);
            let (seq, tracks) = seq_with(vec![
                vec![clip_on(red.id, 0.0, 4.0, vec![])],
                vec![clip_on(blue.id, 0.0, 4.0, vec![])],
            ]);
            let p = project_with(vec![red, blue]);
            let mut res = resources();
            let all = Run {
                project: &p,
                seq: &seq,
                t: 1.0,
                filter: None,
                res: &mut res,
            };
            let buf = compose(&mut r, all, &mut FakeProvider::default());
            assert_near(px(&buf, 1, 1), [0, 0, 255, 255], 1, "top track wins");
            let only = Run {
                project: &p,
                seq: &seq,
                t: 1.0,
                filter: Some(tracks[0]),
                res: &mut res,
            };
            let buf = compose(&mut r, only, &mut FakeProvider::default());
            assert_near(px(&buf, 1, 1), [255, 0, 0, 255], 1, "filtered to V1");
        }

        #[test]
        fn fusion_media_in_shows_the_named_media() {
            let Some(mut r) = renderer() else { return };
            let plate = file_media("Plate");
            let comp = vance_fusion::Comp {
                width: W,
                height: H,
                fps: 24.0,
                duration_s: 4.0,
                background: [0.0; 4],
                nodes: vec![vance_fusion::Node::new(
                    "in",
                    "In",
                    NodeKind::MediaIn(vance_fusion::MediaSpec {
                        name: "Plate".into(),
                        fit: Fit::Stretch,
                    }),
                )],
                output: "in".into(),
            };
            let fusion = MediaItem::new_fusion("Comp", rt(4.0), comp);
            let (seq, _) = seq_with(vec![vec![clip_on(fusion.id, 0.0, 4.0, vec![])]]);
            let mut prov = FakeProvider::default();
            // Rec.709 limited white, half the canvas size.
            prov.by_media
                .insert(plate.id, solid_yuv(W / 2, H / 2, 235, 128, 128));
            let p = project_with(vec![plate, fusion]);
            let mut res = resources();
            let run = Run {
                project: &p,
                seq: &seq,
                t: 1.0,
                filter: None,
                res: &mut res,
            };
            let buf = compose(&mut r, run, &mut prov);
            assert_eq!(
                prov.calls[0].len(),
                1,
                "the MediaIn frame is in the one request batch"
            );
            assert_near(
                px(&buf, W / 2, H / 2),
                [255, 255, 255, 255],
                2,
                "plate through MediaIn",
            );
            assert_near(
                px(&buf, 1, 1),
                [255, 255, 255, 255],
                2,
                "stretched, no bars",
            );
        }

        #[test]
        fn playback_never_touches_the_render_cache() {
            let Some(mut r) = renderer() else { return };
            let dir = tempfile::tempdir().unwrap();
            crate::addons::tests::write_brighten_vsfx(dir.path());
            let card = title_media("grey", [0.5; 3], W, H);
            let fx = vec![effect("shader:com.test.brighten", &[("amount", num(0.5))])];
            let (seq, _) = seq_with(vec![vec![clip_on(card.id, 0.0, 4.0, fx)]]);
            let p = project_with(vec![card]);
            let mut res = resources();
            let cache_dir = tempfile::tempdir().unwrap();
            let cache = Arc::new(
                vance_cache::FrameCache::new(cache_dir.path().to_path_buf(), 1 << 22, 1 << 26)
                    .unwrap(),
            );
            {
                let mut host = res.addons.lock();
                host.set_registry(vance_plugin_sdk::discovery::PluginRegistry::scan(&[dir
                    .path()
                    .to_path_buf()]));
                host.set_gpu(r.device().clone(), r.queue().clone());
                host.set_render_cache(Some(cache.clone()));
            }
            fn run<'a>(
                project: &'a Project,
                seq: &'a Sequence,
                res: &'a mut ComposeResources,
            ) -> Run<'a> {
                Run {
                    project,
                    seq,
                    t: 1.0,
                    filter: None,
                    res,
                }
            }
            let (played, report) = compose_preview(
                &mut r,
                run(&p, &seq, &mut res),
                &mut FakeProvider::default(),
                true,
            );
            assert!(report.skipped.is_empty(), "{:?}", report.skipped);
            let s = cache.stats();
            assert_eq!(s.lookups(), 0, "playback must not look the cache up");
            assert_eq!((s.ring_count, s.disk_entries), (0, 0), "nor fill it");
            let (scrubbed, _) = compose_preview(
                &mut r,
                run(&p, &seq, &mut res),
                &mut FakeProvider::default(),
                false,
            );
            let s = cache.stats();
            assert_eq!((s.lookups(), s.disk_entries), (1, 1), "scrub caches");
            assert_eq!(px(&played, W / 2, H / 2), px(&scrubbed, W / 2, H / 2));
        }

        #[test]
        fn skipped_layers_say_why() {
            let Some(mut r) = renderer() else { return };
            let dir = tempfile::tempdir().unwrap();
            let present = dir.path().join("present.mp4");
            std::fs::write(&present, b"x").unwrap();
            let mut bad = file_media("bad");
            bad.path = present.clone();
            let offline = file_media("offline-clip");
            // "Use proxies": the proxy is there (but won't decode), its
            // original is gone.
            let proxy = dir.path().join("orphan.proxy.mov");
            std::fs::write(&proxy, b"x").unwrap();
            let mut orphan = file_media("orphan-original");
            orphan.proxy_path = Some(proxy);
            let card = title_media("card", [0.0, 0.0, 1.0], W, H);
            let gone = MediaItemId::new();
            let card_fx = vec![effect("wasm:com.example.absent", &[])];
            let (seq, _) = seq_with(vec![
                vec![clip_on(card.id, 0.0, 4.0, card_fx)],
                vec![clip_on(bad.id, 0.0, 4.0, vec![])],
                vec![clip_on(offline.id, 0.0, 4.0, vec![])],
                vec![clip_on(gone, 0.0, 4.0, vec![])],
                vec![clip_on(orphan.id, 0.0, 4.0, vec![])],
            ]);
            let mut p = project_with(vec![
                card.clone(),
                bad.clone(),
                offline.clone(),
                orphan.clone(),
            ]);
            p.use_proxies = true;
            let mut fake = FakeProvider::default();
            fake.failing
                .insert(bad.id, "decoder open failed: NoVideoStream".into());
            fake.failing.insert(
                orphan.id,
                "decoder open failed: NotFound(\"orphan-original.mp4\")".into(),
            );
            // Through `Prefetched`, as the callers fetch outside the lock.
            let plan = vance_timeline::scene_plan_at(&seq, rt(1.0));
            let reqs = plan_requests(&p, &seq, &plan, Purpose::Preview, None);
            let mut pre = Prefetched::fetch(&mut fake, reqs);
            let mut res = resources();
            let run = Run {
                project: &p,
                seq: &seq,
                t: 1.0,
                filter: None,
                res: &mut res,
            };
            let (buf, report) = compose_preview(&mut r, run, &mut pre, false);
            assert_near(
                px(&buf, W / 2, H / 2),
                [0, 0, 255, 255],
                1,
                "the title shows",
            );
            let why = |m: MediaItemId| -> Vec<&str> {
                report
                    .skipped
                    .iter()
                    .filter(|s| s.media == m)
                    .map(|s| s.reason.as_str())
                    .collect()
            };
            let kind =
                |m: MediaItemId| report.skipped.iter().find(|s| s.media == m).map(|s| s.kind);
            assert_eq!(report.skipped.len(), 5, "{:?}", report.skipped);
            assert_eq!(kind(gone), Some(SkipKind::MissingMedia));
            assert_eq!(kind(orphan.id), Some(SkipKind::MissingMedia));
            assert_eq!(
                why(orphan.id),
                ["media is offline: orphan-original.mp4 can't be found"],
                "the original's absence, not the proxy's decode error"
            );
            assert_eq!(kind(offline.id), Some(SkipKind::MissingMedia));
            assert_eq!(kind(bad.id), Some(SkipKind::NoFrame));
            assert_eq!(kind(card.id), Some(SkipKind::AddonFailed));
            assert_eq!(why(gone), ["its media is missing from the library"]);
            assert_eq!(
                why(offline.id),
                ["media is offline: offline-clip.mp4 can't be found"]
            );
            assert_eq!(
                why(bad.id),
                ["couldn't decode present.mp4 (decoder open failed: NoVideoStream)"]
            );
            assert_eq!(
                why(card.id),
                ["the effect com.example.absent isn't installed"]
            );
        }

        #[test]
        fn compose_frame_uses_the_sessions_file_snapshot() {
            let Some(mut r) = renderer() else { return };
            let dir = tempfile::tempdir().unwrap();
            let proxy = dir.path().join("p.mov");
            std::fs::write(&proxy, b"x").unwrap();
            let original = dir.path().join("a.mp4");
            std::fs::write(&original, b"x").unwrap();
            let mut m = file_media("a");
            m.path = original.clone();
            m.proxy_path = Some(proxy.clone());
            let mut p = project_with(vec![m.clone()]);
            p.use_proxies = true;
            let (seq, _) = seq_with(vec![vec![clip_on(m.id, 0.0, 4.0, vec![])]]);
            let mut res = resources();
            res.set_file_snapshot(Some(Arc::new(FileSnapshot::of(&p))));
            // Both vanish after the snapshot: the session doesn't stat again.
            std::fs::remove_file(&proxy).unwrap();
            std::fs::remove_file(&original).unwrap();
            let mut prov = FakeProvider::default();
            let run = Run {
                project: &p,
                seq: &seq,
                t: 1.0,
                filter: None,
                res: &mut res,
            };
            let (_, report) = compose_preview(&mut r, run, &mut prov, true);
            assert_eq!(prov.calls[0][0].path, proxy.to_string_lossy(), "no stat");
            assert_eq!(
                report.skipped[0].kind,
                SkipKind::NoFrame,
                "the snapshot says both files are there"
            );
        }

        #[test]
        fn audio_on_a_video_track_is_no_layer_and_no_skip() {
            let Some(mut r) = renderer() else { return };
            let card = title_media("card", [1.0, 0.0, 0.0], W, H);
            let mut music = file_media("music");
            music.kind = MediaKind::Audio;
            let (seq, _) = seq_with(vec![
                vec![clip_on(card.id, 0.0, 4.0, vec![])],
                vec![clip_on(music.id, 0.0, 4.0, vec![])],
            ]);
            let p = project_with(vec![card, music]);
            let mut res = resources();
            let mut prov = FakeProvider::default();
            let run = Run {
                project: &p,
                seq: &seq,
                t: 1.0,
                filter: None,
                res: &mut res,
            };
            let (buf, report) = compose_preview(&mut r, run, &mut prov, false);
            assert!(report.skipped.is_empty(), "{:?}", report.skipped);
            assert_eq!(
                prov.calls,
                vec![Vec::<FrameRequest>::new()],
                "no frame asked"
            );
            assert_near(px(&buf, W / 2, H / 2), [255, 0, 0, 255], 1, "preview");
            // The export path too (an export fails on a skipped layer).
            let mut seq = seq.clone();
            seq.frame_size = Some((W, H));
            let purpose = Purpose::Export { hdr: false };
            let out = output_spec_for(&p, &seq, purpose);
            let plan = vance_timeline::scene_plan_at(&seq, rt(1.0));
            let mut ctx = ComposeCtx {
                project: &p,
                seq: &seq,
                purpose,
                viewer: ViewerState::default(),
                track_filter: None,
                realtime: false,
                res: &mut res,
            };
            let c = r.compositor();
            let report = compose_frame(c, &plan, &mut prov, &mut ctx, &out).unwrap();
            assert!(report.skipped.is_empty(), "export: {:?}", report.skipped);
            let buf = c.finish_to_rgba8().unwrap();
            assert_near(px(&buf, W / 2, H / 2), [255, 0, 0, 255], 1, "export");
        }

        #[test]
        fn render_and_replace_bakes_opacity_into_alpha() {
            // Needs seam B's transparent output (`OutputSpec.transparent`).
            let Some(mut r) = renderer() else { return };
            let card = title_media("red", [1.0, 0.0, 0.0], W, H);
            let mut clip = clip_on(
                card.id,
                0.0,
                4.0,
                vec![effect("opacity", &[("value", num(0.5))])],
            );
            clip.blend_mode = vance_timeline::BlendMode::Screen;
            let (mut seq, tracks) = seq_with(vec![vec![clip]]);
            seq.frame_size = Some((W, H));
            let p = project_with(vec![card]);
            let purpose = Purpose::Export { hdr: false };
            let mut out = output_spec_for(&p, &seq, purpose);
            out.transparent = true;
            let plan = vance_timeline::scene_plan_at(&seq, rt(1.0));
            let mut res = resources();
            let mut ctx = ComposeCtx {
                project: &p,
                seq: &seq,
                purpose,
                viewer: ViewerState::default(),
                track_filter: Some(tracks[0]),
                realtime: false,
                res: &mut res,
            };
            let c = r.compositor();
            compose_frame(c, &plan, &mut FakeProvider::default(), &mut ctx, &out).unwrap();
            let buf = c.finish_to_rgba8().unwrap();
            assert_near(
                px(&buf, W / 2, H / 2),
                [255, 0, 0, 128],
                2,
                "straight red at half alpha",
            );
        }

        #[test]
        fn wasm_effect_runs_on_its_layer() {
            let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../plugins/stylize/target/wasm32-unknown-unknown/release");
            if !dir.join("vance_plugin_stylize.wasm").exists() {
                eprintln!("stylize.wasm not built — skipping");
                return;
            }
            let Some(mut r) = renderer() else { return };
            let card = title_media("teal", [0.2, 0.6, 0.5], W, H);
            let fx = vec![effect(
                "wasm:com.vance.sample.stylize",
                &[("levels", num(2.0)), ("saturation", num(2.0))],
            )];
            let (plain, _) = seq_with(vec![vec![clip_on(card.id, 0.0, 4.0, vec![])]]);
            let (styled, _) = seq_with(vec![vec![clip_on(card.id, 0.0, 4.0, fx)]]);
            let p = project_with(vec![card]);
            let mut res = resources();
            res.addons
                .lock()
                .set_registry(vance_plugin_sdk::discovery::PluginRegistry::scan(&[dir]));
            let a = Run {
                project: &p,
                seq: &plain,
                t: 1.0,
                filter: None,
                res: &mut res,
            };
            let a = compose(&mut r, a, &mut FakeProvider::default());
            let b = Run {
                project: &p,
                seq: &styled,
                t: 1.0,
                filter: None,
                res: &mut res,
            };
            let b = compose(&mut r, b, &mut FakeProvider::default());
            assert_ne!(
                px(&a, W / 2, H / 2),
                px(&b, W / 2, H / 2),
                "the add-on changed the layer"
            );
        }
    }

    #[test]
    fn title_cache_hits_by_params_and_evicts_lru() {
        let mut cache = SyntheticCache::default();
        let card = |r: f32| TitleParams {
            text: String::new(),
            size_px: 24.0,
            fg: [1.0; 3],
            bg: [r, 0.0, 0.0],
            width: 32,
            height: 16,
            bg_alpha: 1.0,
        };
        let a = cache.title(&card(0.5)).unwrap();
        let b = cache.title(&card(0.5)).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(&a.pixels[..4], &[127, 0, 0, 255]);
        let c = cache.title(&card(0.25)).unwrap();
        assert!(!Arc::ptr_eq(&a, &c));
        for i in 0..MAX_CACHED_TITLES + 3 {
            cache.title(&card(i as f32 / 100.0)).unwrap();
            cache.title(&card(0.5)).unwrap(); // keep 0.5 hot
        }
        assert_eq!(cache.cached_titles(), MAX_CACHED_TITLES);
        assert!(
            Arc::ptr_eq(&a, &cache.title(&card(0.5)).unwrap()),
            "hot entry survives"
        );
    }
}
