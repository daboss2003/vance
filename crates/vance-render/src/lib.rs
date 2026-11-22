//! `vance-render` — wgpu render pipeline for decoded video frames.
//!
//! Milestone 1.3 scope: a `Renderer` that uploads a YUV frame from
//! `vance-decoder` and renders it as a fullscreen RGB quad onto an arbitrary
//! target texture (or surface, in later milestones).
//!
//! Color: Rec.709 *limited range* throughout this milestone. This is the
//! deliberate Phase-1 shortcut documented in PLAN.md §7 Phase 1; full color
//! management lands in Phase 6.
//!
//! Both [`PixelFormat::Yuv420P`] (planar) and [`PixelFormat::Nv12`]
//! (semi-planar) inputs are accepted. For NV12 inputs the UV plane is
//! split CPU-side at upload so the GPU sees the same three R8 textures
//! either way. Zero-copy NV12 (R8 + Rg8) is a Milestone 1.6 optimization
//! once VideoToolbox hardware decode lands.

#![warn(missing_docs)]

pub mod compositor;
pub mod fusion;
pub mod mograph;
mod renderer;
pub mod title;

pub use compositor::{
    render_fusion_rgba, render_mograph_rgba, render_title_rgba, CompositeOp, Compositor,
    KeyerParams, LayerHandle, LayerKey, LayerParams, LayerSource, OutputSpec, RgbaImage,
};
pub use fusion::render_fusion_yuv420;
pub use mograph::render_mograph_yuv420;
pub use renderer::{
    CurveSet, GradeMode, GradeNodeParams, Qualifier, RenderError, Renderer, RendererConfig, Window,
};
pub use title::{render_title_yuv420, TitleError, TitleParams};
pub use vance_decoder::{FrameData, PixelFormat};
