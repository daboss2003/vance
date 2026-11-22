//! The one frame function every consumer renders through (ADR 0007):
//! scrub, playback and export all call [`compose_frame`] with a
//! [`ScenePlan`] and a [`FrameProvider`]; only the provider differs.
//!
//! The intended preview/export differences are the view LUT and viewer
//! solos (preview only), the output space/format, and the canvas size.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use vance_decoder::FrameData;
use vance_mograph::MographEngine;
use vance_render::{Compositor, LayerParams, OutputSpec};
use vance_timeline::{Clip, ClipId, EffectId, MediaItemId, Project, ScenePlan, Sequence, TrackId};

use crate::addons::AddonHost;
use crate::engine::EngineError;

/// Parsed LUT files keyed by absolute path (same shape as the engine's).
pub type LutCache = HashMap<PathBuf, Arc<vance_color::Lut3d>>;

/// One decoded frame a layer needs.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameRequest {
    /// Clip the frame is for.
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
    /// skipped).
    fn frames(&mut self, reqs: &[FrameRequest]) -> Vec<Option<FrameData>>;
    /// Hint at the requests coming next (prefetch). Default: nothing.
    fn prepare(&mut self, _upcoming: &[FrameRequest]) {}
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

/// Rendered synthetic media (titles, MoGraph, Fusion) reused across frames.
// Intentional: an empty placeholder until the compose seam (C) defines the
// cache key and eviction.
#[derive(Debug, Default)]
pub struct SyntheticCache {}

/// Long-lived state `compose_frame` uses, shareable with the playback
/// thread.
pub struct ComposeResources {
    /// WASM / shader add-ons.
    pub addons: Arc<Mutex<AddonHost>>,
    /// Parsed clip LUTs.
    pub luts: Arc<Mutex<LutCache>>,
    /// MoGraph renderer.
    pub mograph: Arc<MographEngine>,
    /// Fusion renderer.
    pub fusion: Arc<vance_fusion::Compositor>,
    /// Rendered synthetic media.
    pub synth: SyntheticCache,
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
    /// Shared resources.
    pub res: &'a mut ComposeResources,
}

/// The layer-pass parameters of `clip` at clip-relative `clip_t` seconds.
// Intentional: returns identity parameters (Rec.709 limited in, no effects)
// until the compose seam (C) copies the engine's eval_clip_* logic here.
pub fn layer_params_for(
    _p: &Project,
    _clip: &Clip,
    _clip_t: f64,
    _purpose: Purpose,
    _v: &ViewerState,
    _luts: &Mutex<LutCache>,
) -> LayerParams {
    LayerParams {
        pipeline: vance_color::PipelineParams {
            input_transfer: 2,
            output_transfer: 1,
            input_range: 0,
            output_range: 1,
            input_to_working: vance_color::Transform::IDENTITY,
            working_to_output: vance_color::Transform::IDENTITY,
        },
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
    }
}

/// The canvas and output encoding for `seq`: `seq.frame_size`, else
/// 1920×1080.
// Intentional: SDR sRGB, no view LUT, until the compose seam (C) derives
// the output space from the project and purpose.
pub fn output_spec_for(_p: &Project, seq: &Sequence, purpose: Purpose) -> OutputSpec {
    let (width, height) = seq.frame_size.unwrap_or((1920, 1080));
    OutputSpec {
        width,
        height,
        working_to_output: vance_color::Transform::IDENTITY,
        output_transfer: 1,
        hdr: matches!(purpose, Purpose::Export { hdr: true }),
        view_lut: false,
    }
}

/// Render `plan` into the compositor: one layer pass per visible clip
/// (add-ons per layer), composited bottom → top, ready for a `finish_*`.
// Intentional: Unimplemented until the compose seam (C) lands.
pub fn compose_frame(
    _c: &mut Compositor,
    _plan: &ScenePlan,
    _prov: &mut dyn FrameProvider,
    _ctx: &mut ComposeCtx<'_>,
    _out: &OutputSpec,
) -> Result<(), EngineError> {
    Err(EngineError::Unimplemented("compose::compose_frame"))
}
