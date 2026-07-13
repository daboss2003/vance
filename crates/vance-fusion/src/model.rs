//! `.vfc` — Vance Fusion Composition data model.
//!
//! A composition is a directed acyclic graph of image-processing [`Node`]s.
//! Source nodes (background, text, shape, media-in) generate a canvas-sized
//! [`RgbaImage`](vance_mograph::RgbaImage); filter nodes transform one input;
//! combiner nodes (merge, mask) take two. Every node's output is the size of
//! the composition canvas, so connections are always compatible and the
//! final [`Comp::output`] node is the rendered frame.
//!
//! Scalars that vary over time are [`Anim`] (constant / keyframe lane /
//! `vance-expr` expression). The whole model is `serde` + RON
//! round-trippable so a composition persists losslessly in the project.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use vance_mograph::{Align, Key, Rgba};

/// A complete node-graph composition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Comp {
    /// Canvas width in px.
    pub width: u32,
    /// Canvas height in px.
    pub height: u32,
    /// Frames per second (feeds `frame`/`fps` in expressions).
    #[serde(default = "default_fps")]
    pub fps: f64,
    /// Composition duration in seconds (feeds `duration` in expressions).
    #[serde(default = "default_duration")]
    pub duration_s: f64,
    /// Background color the final frame is flattened over when an opaque
    /// buffer is requested. Alpha `0.0` keeps the comp transparent.
    #[serde(default)]
    pub background: Rgba,
    /// The nodes, in no particular order (edges define evaluation order).
    pub nodes: Vec<Node>,
    /// Id of the node whose output is the rendered frame.
    pub output: String,
}

fn default_fps() -> f64 {
    30.0
}
fn default_duration() -> f64 {
    5.0
}

impl Comp {
    /// An empty canvas-only comp (transparent output) of the given size.
    pub fn empty(width: u32, height: u32) -> Self {
        Comp {
            width: width.max(1),
            height: height.max(1),
            fps: default_fps(),
            duration_s: default_duration(),
            background: [0.0, 0.0, 0.0, 0.0],
            nodes: Vec::new(),
            output: String::new(),
        }
    }

    /// Look up a node by id.
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

/// One node in the graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// Stable id (referenced by other nodes' [`Node::inputs`]).
    pub id: String,
    /// Display name for the node editor.
    pub name: String,
    /// What the node computes.
    pub kind: NodeKind,
    /// Input slot name → source node id. Slots a kind doesn't read are
    /// ignored; a missing/unconnected slot resolves to a transparent frame.
    #[serde(default)]
    pub inputs: BTreeMap<String, String>,
    /// Node-editor position `[x, y]` (purely cosmetic).
    #[serde(default)]
    pub ui_pos: [f32; 2],
}

impl Node {
    /// Convenience constructor with no connections.
    pub fn new(id: impl Into<String>, name: impl Into<String>, kind: NodeKind) -> Self {
        Node {
            id: id.into(),
            name: name.into(),
            kind,
            inputs: BTreeMap::new(),
            ui_pos: [0.0, 0.0],
        }
    }

    /// Builder: connect `slot` to source node `src`.
    pub fn with_input(mut self, slot: &str, src: &str) -> Self {
        self.inputs.insert(slot.to_string(), src.to_string());
        self
    }

    /// Builder: set the editor position.
    pub fn at(mut self, x: f32, y: f32) -> Self {
        self.ui_pos = [x, y];
        self
    }
}

/// The computation a node performs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    // --- Sources (0 inputs) ---
    /// A solid fill or vertical gradient covering the canvas.
    Background(BgSpec),
    /// A host-supplied image (e.g. the clip's decoded frame) placed per a
    /// fit mode. Reads from the render call's named-input map.
    MediaIn(MediaSpec),
    /// A filled rounded rectangle, centered on the canvas.
    Rectangle(RectSpec),
    /// A laid-out text block, centered on the canvas.
    Text(TextSpec),

    // --- Filters ("input") ---
    /// Affine move/scale/rotate/opacity of the input.
    Transform(TransformSpec),
    /// A separable blur of the input.
    Blur(BlurSpec),
    /// Lift / gamma / gain + saturation of the input.
    ColorCorrect(ColorSpec),
    /// Clear the input outside a rectangle (per-edge px insets).
    Crop(CropSpec),

    // --- Combiners ("background" + "foreground", or "input" + "mask") ---
    /// Blend `foreground` over `background`.
    Merge(MergeSpec),
    /// Multiply the `input` alpha by the `mask` (alpha or luminance).
    Mask(MaskSpec),
}

/// An animatable scalar: constant, keyframe lane, or time expression.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Anim {
    /// Fixed value.
    Const(f64),
    /// Keyframed lane (seconds → value), reusing the mograph interpolator.
    Keys(Vec<Key>),
    /// `vance-expr` expression over `time`, `frame`, `duration`, `fps`,
    /// `width`, `height`.
    Expr(String),
}

impl Anim {
    /// Convenience constant.
    pub fn c(v: f64) -> Self {
        Anim::Const(v)
    }
}

/// Solid / gradient background.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BgSpec {
    /// Top (or solid) color.
    pub color: Rgba,
    /// When set, a vertical gradient from `color` (top) to this (bottom).
    #[serde(default)]
    pub gradient_to: Option<Rgba>,
}

/// How to fit a host image into the canvas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fit {
    /// Native size, centered.
    None,
    /// Stretch to exactly fill the canvas (ignores aspect).
    Stretch,
    /// Scale to fit inside the canvas (letterbox), preserving aspect.
    #[default]
    Contain,
    /// Scale to cover the canvas (crop), preserving aspect.
    Cover,
}

/// A host-supplied image source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaSpec {
    /// Key into the render call's named-input map.
    pub name: String,
    /// Fit mode.
    #[serde(default)]
    pub fit: Fit,
}

/// A filled rounded rectangle centered on the canvas.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RectSpec {
    /// Width in px.
    pub width: Anim,
    /// Height in px.
    pub height: Anim,
    /// Corner radius in px.
    #[serde(default = "zero_anim")]
    pub radius: Anim,
    /// Fill color.
    pub color: Rgba,
}

fn zero_anim() -> Anim {
    Anim::Const(0.0)
}

/// A centered text block.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextSpec {
    /// Text body (may contain `\n`).
    pub text: String,
    /// Font family name (resolved via the shared font DB).
    pub font: String,
    /// Size in px.
    pub size_px: Anim,
    /// Fill color.
    pub color: Rgba,
    /// Horizontal alignment.
    #[serde(default)]
    pub align: Align,
    /// Line height multiple of size.
    #[serde(default = "line_height_default")]
    pub line_height: f64,
    /// Letter-spacing in px.
    #[serde(default)]
    pub tracking: f64,
    /// Optional wrap width in px.
    #[serde(default)]
    pub max_width: Option<f64>,
}

fn line_height_default() -> f64 {
    1.2
}

/// Affine transform of the input. Identity places the input centered at
/// full size (position defaults to the canvas center — set at build time).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransformSpec {
    /// Pivot as a normalized fraction of the (canvas-sized) input `[x, y]`.
    pub anchor: [Anim; 2],
    /// Canvas px the anchor lands on `[x, y]`.
    pub position: [Anim; 2],
    /// Scale `[x, y]`.
    pub scale: [Anim; 2],
    /// Clockwise rotation in degrees.
    #[serde(default = "zero_anim")]
    pub rotation_deg: Anim,
    /// Opacity `[0,1]`.
    #[serde(default = "one_anim")]
    pub opacity: Anim,
}

fn one_anim() -> Anim {
    Anim::Const(1.0)
}

impl TransformSpec {
    /// An identity transform for a `width×height` canvas (centered anchor
    /// landing on the canvas center, unit scale).
    pub fn identity(width: u32, height: u32) -> Self {
        TransformSpec {
            anchor: [Anim::Const(0.5), Anim::Const(0.5)],
            position: [
                Anim::Const(width as f64 / 2.0),
                Anim::Const(height as f64 / 2.0),
            ],
            scale: [Anim::Const(1.0), Anim::Const(1.0)],
            rotation_deg: Anim::Const(0.0),
            opacity: Anim::Const(1.0),
        }
    }
}

/// Blur kernel shape.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlurMode {
    /// Single box pass (fast, boxy).
    Box,
    /// Three box passes approximating a Gaussian (smooth).
    #[default]
    Gaussian,
}

/// A separable blur.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlurSpec {
    /// Blur radius in px.
    pub radius: Anim,
    /// Kernel shape.
    #[serde(default)]
    pub mode: BlurMode,
}

/// Lift / gamma / gain + saturation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorSpec {
    /// Per-channel additive offset (RGB), applied after gain.
    #[serde(default = "zero3")]
    pub lift: [Anim; 3],
    /// Per-channel multiplier (RGB), applied first.
    #[serde(default = "one3")]
    pub gain: [Anim; 3],
    /// Gamma (`>0`); output = input^(1/gamma).
    #[serde(default = "one_anim")]
    pub gamma: Anim,
    /// Saturation (`1.0` = unchanged, `0.0` = grayscale).
    #[serde(default = "one_anim")]
    pub saturation: Anim,
}

fn zero3() -> [Anim; 3] {
    [Anim::Const(0.0), Anim::Const(0.0), Anim::Const(0.0)]
}
fn one3() -> [Anim; 3] {
    [Anim::Const(1.0), Anim::Const(1.0), Anim::Const(1.0)]
}

/// Per-edge crop insets in px.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CropSpec {
    /// Pixels removed from the left.
    #[serde(default = "zero_anim")]
    pub left: Anim,
    /// Pixels removed from the top.
    #[serde(default = "zero_anim")]
    pub top: Anim,
    /// Pixels removed from the right.
    #[serde(default = "zero_anim")]
    pub right: Anim,
    /// Pixels removed from the bottom.
    #[serde(default = "zero_anim")]
    pub bottom: Anim,
}

/// How `foreground` combines with `background`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BlendMode {
    /// Source-over (normal).
    #[default]
    Over,
    /// Additive (plus).
    Add,
    /// Multiply.
    Multiply,
    /// Screen.
    Screen,
}

/// Blend two inputs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergeSpec {
    /// Blend mode.
    #[serde(default)]
    pub mode: BlendMode,
    /// Foreground opacity `[0,1]`.
    #[serde(default = "one_anim")]
    pub opacity: Anim,
}

/// Which channel of the mask input drives the matte.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaskChannel {
    /// Use the mask's alpha channel.
    #[default]
    Alpha,
    /// Use the mask's luminance.
    Luminance,
}

/// Multiply the input alpha by a mask.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaskSpec {
    /// Mask channel.
    #[serde(default)]
    pub channel: MaskChannel,
    /// Invert the mask factor (`1 - m`).
    #[serde(default)]
    pub invert: bool,
}
