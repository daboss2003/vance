//! `vance-timeline` — non-destructive timeline data model.
//!
//! This crate defines the in-memory shape of a project: a `Project`
//! owns a media library and zero or more `Sequence`s; each sequence
//! owns ordered `Track`s of `Clip`s; clips reference `MediaItem`s and
//! carry per-clip `Effect`s. Sequence-level annotations live in
//! `Marker`s.
//!
//! Phase 3.1 scope is shape-only: types, constructors, derived getters
//! (`Clip::duration`, `Track::duration`, `Sequence::duration`,
//! `Sequence::clip_count`). Edit operations land in 3.2 as pure
//! `&Sequence -> Sequence` functions over the `Arc<...>`-shared
//! interior.
//!
//! ## Time
//!
//! All time is `vance_core::RationalTime` (seconds expressed as a
//! rational). Frame rates are `vance_core::FrameRate`. Clips carry both
//! source-domain (`source_in` / `source_out`) and timeline-domain
//! (`timeline_in`) anchors; durations are derived rather than stored to
//! eliminate "the source range and the duration disagree" bugs.
//!
//! ## Identity & sharing
//!
//! Every entity has a stable `Uuid`-based ID that survives save/load
//! (see `id.rs`). Tracks and clips are kept behind `Arc`s so future
//! edit operations can rebuild a `Sequence` while sharing untouched
//! tracks and clips with the previous version, keeping undo/redo cheap.

#![warn(missing_docs)]

pub mod audio_effect;
pub mod clip;
pub mod edit_log;
pub mod effect;
pub mod error;
pub mod fade;
pub mod grade;
pub mod id;
pub mod bus;
pub mod marker;
pub mod media;
pub mod media_query;
pub mod ops;
pub mod project;
pub mod scene_plan;
pub mod sequence;
pub mod title;
pub mod track;
pub mod transition;

pub use audio_effect::{
    AudioEffectSpec, CompressorSpec, Eq6Spec, EqBandKind, EqBandSpec, LimiterSpec,
    PluginParamValue, PluginRefSpec,
};
pub use clip::Clip;
pub use edit_log::{apply, EditLog, EditOp, HistoryNodeView};
pub use effect::{
    eval_curve, validate_expr, Effect, EffectParams, EffectValue, Keyframe, KeyframeInterp,
};
pub use bus::{routing_cycle, AudioBus, BusOutput, Send};
pub use error::{Result, TimelineError};
pub use media_query::{Condition, MediaField, MediaQuery, QueryOp, SmartBin};
pub use fade::{eval_fade_in, eval_fade_out, Fade, FadeCurve};
pub use id::{
    ClipId, EffectId, MarkerId, MediaItemId, ProjectId, SequenceId, TrackId, TransitionId,
};
pub use marker::{Marker, MarkerColor, MarkerKind};
pub use media::{ContentHash, MediaItem, MediaKind};
pub use project::{OcioView, Project};
pub use scene_plan::{scene_at, transition_at, ActiveClip, ActiveTransition, SceneAt};
pub use sequence::Sequence;
pub use title::TitleParams;
pub use track::{Track, TrackKind};
pub use transition::{Transition, TransitionKind};
