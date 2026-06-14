//! `.cmgt` — Vance Motion Graphics Template data model.
//!
//! A template is a *parameterized, animated* layered composition. The
//! editor drops an instance on the timeline and tweaks the exposed
//! [`Control`]s (text, color, sliders, fonts, …); the template's layer
//! rig reads those controls (via [`Prop::Bind`] / [`ColorProp::Bind`] /
//! [`TextProp::Bind`]) plus keyframes/expressions to draw each frame.
//!
//! Everything here is `serde` + RON round-trippable so a template (and a
//! per-clip instance) persists in the `.vance` bundle losslessly.

use serde::{Deserialize, Serialize};

/// RGBA in straight-alpha, components in `[0, 1]`.
pub type Rgba = [f64; 4];

/// A complete motion-graphics template.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MographTemplate {
    /// Human metadata + default clip length.
    pub meta: Meta,
    /// Output canvas.
    pub canvas: Canvas,
    /// Editor-facing controls, in display order.
    pub controls: Vec<Control>,
    /// Layers, painted back-to-front (index 0 first / bottom).
    pub layers: Vec<Layer>,
}

/// Template metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Meta {
    /// Display name (e.g. "Lower Third — Clean").
    pub name: String,
    /// Author / pack attribution.
    #[serde(default)]
    pub author: String,
    /// Template schema/content version.
    #[serde(default = "one")]
    pub version: u32,
    /// Default clip duration in seconds when first placed.
    #[serde(default = "default_duration")]
    pub default_duration_s: f64,
}

fn one() -> u32 {
    1
}
fn default_duration() -> f64 {
    5.0
}

/// Output canvas + flatten background.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Canvas {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Background color the composite is flattened over. Use alpha `0.0`
    /// for a transparent template (overlay), `1.0` for a full card.
    #[serde(default)]
    pub background: Rgba,
}

// ---------------------------------------------------------------------
// Controls (editor-facing parameters)
// ---------------------------------------------------------------------

/// One exposed control the editor edits per instance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Control {
    /// Stable id referenced by `Bind`s in the layer rig.
    pub id: String,
    /// Display label.
    pub label: String,
    /// Type + default.
    pub kind: ControlKind,
}

/// Control type + its default value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ControlKind {
    /// Free text (single or multi-line via `\n`).
    Text {
        /// Default string.
        default: String,
    },
    /// Numeric slider/spinner.
    Number {
        /// Default value.
        default: f64,
        /// Inclusive minimum (UI clamp).
        min: f64,
        /// Inclusive maximum (UI clamp).
        max: f64,
    },
    /// RGBA color swatch.
    Color {
        /// Default color.
        default: Rgba,
    },
    /// One-of-N dropdown; the value is the selected index.
    Choice {
        /// Option labels.
        options: Vec<String>,
        /// Default selected index.
        default: usize,
    },
    /// Checkbox.
    Bool {
        /// Default state.
        default: bool,
    },
    /// Font family picker (resolved by [`crate::fonts::FontDb`]).
    Font {
        /// Default family name.
        default: String,
    },
    /// 2-D point in normalized canvas coords `[0,1]×[0,1]`.
    Point {
        /// Default point.
        default: [f64; 2],
    },
}

// ---------------------------------------------------------------------
// Properties (animatable / bindable values inside the rig)
// ---------------------------------------------------------------------

/// An animatable scalar: a constant, a keyframe lane, a time expression,
/// or a binding to a numeric/bool control.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Prop {
    /// Fixed value.
    Const(f64),
    /// Keyframed lane (seconds → value).
    Keys(Vec<Key>),
    /// `vance-expr` expression over `time`, `value`, `duration`, …
    Expr(String),
    /// Read a `Number`/`Bool` control by id (bool → 0.0/1.0).
    Bind(String),
    /// X of a `Point` control, scaled to canvas pixels (`point.x ×
    /// canvas_width`). Lets a draggable position control drive a layer.
    PointX(String),
    /// Y of a `Point` control, scaled to canvas pixels.
    PointY(String),
}

impl Prop {
    /// Convenience constant.
    pub fn c(v: f64) -> Self {
        Prop::Const(v)
    }
}

/// One keyframe in a [`Prop::Keys`] lane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Key {
    /// Time in seconds (clip-relative).
    pub t: f64,
    /// Value at `t`.
    pub v: f64,
    /// Interpolation INTO the next key.
    #[serde(default)]
    pub interp: Interp,
}

/// Keyframe interpolation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Interp {
    /// Straight line.
    #[default]
    Linear,
    /// Step (hold until the next key).
    Hold,
    /// Smoothstep ease in/out.
    Smooth,
}

/// A color value: constant or bound to a `Color` control.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ColorProp {
    /// Fixed color.
    Const(Rgba),
    /// Read a `Color` control by id.
    Bind(String),
}

/// A string value (text body or font family): constant, bound to a
/// `Text`/`Font` control, or — for `Choice` controls — the selected
/// option label.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TextProp {
    /// Fixed string.
    Const(String),
    /// Read a `Text`/`Font` control, or a `Choice` option label, by id.
    Bind(String),
}

// ---------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------

/// One layer in the composition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layer {
    /// Stable layer id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Master visibility toggle.
    #[serde(default = "yes")]
    pub visible: bool,
    /// What the layer draws.
    pub content: Content,
    /// Where/how it's placed + animated.
    pub transform: Transform,
}

fn yes() -> bool {
    true
}

/// Layer content.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Content {
    /// A filled rounded rectangle (the layer's intrinsic size).
    Rect(RectSpec),
    /// A laid-out text block.
    Text(TextSpec),
}

/// Filled rectangle spec (also used for solid bars / backgrounds).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RectSpec {
    /// Width in layer-local px.
    pub width: Prop,
    /// Height in layer-local px.
    pub height: Prop,
    /// Corner radius in px (`0` = sharp).
    #[serde(default = "zero_prop")]
    pub radius: Prop,
    /// Fill color.
    pub color: ColorProp,
}

fn zero_prop() -> Prop {
    Prop::Const(0.0)
}

/// Text block spec.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextSpec {
    /// The text body (may contain `\n`).
    pub text: TextProp,
    /// Font family name.
    pub font: TextProp,
    /// Font size in px.
    pub size_px: Prop,
    /// Fill color.
    pub color: ColorProp,
    /// Horizontal alignment within the block.
    #[serde(default)]
    pub align: Align,
    /// Line height as a multiple of `size_px`.
    #[serde(default = "line_height_default")]
    pub line_height: f64,
    /// Extra px between glyphs (letter-spacing).
    #[serde(default)]
    pub tracking: f64,
    /// Optional wrap width in px; `None` = wrap only on `\n`.
    #[serde(default)]
    pub max_width: Option<f64>,
}

fn line_height_default() -> f64 {
    1.2
}

/// Horizontal text alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Align {
    /// Flush left.
    #[default]
    Left,
    /// Centered.
    Center,
    /// Flush right.
    Right,
}

/// Affine placement + animation of a layer onto the canvas.
///
/// The mapping is `canvas = position + R(rot)·S(scale)·(local − anchor)`,
/// so `position` is where the `anchor` point lands, and scale/rotation
/// pivot about the anchor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    /// Pivot as a NORMALIZED fraction of the layer's size `[x, y]`
    /// (`[0, 0]` = top-left, `[0.5, 0.5]` = center). Normalized so it
    /// pivots dynamic-size text correctly.
    pub anchor: [Prop; 2],
    /// Canvas px the anchor lands on `[x, y]`.
    pub position: [Prop; 2],
    /// Scale `[x, y]` (`1.0` = 100%).
    pub scale: [Prop; 2],
    /// Clockwise rotation in degrees about the anchor.
    #[serde(default = "zero_prop")]
    pub rotation_deg: Prop,
    /// Layer opacity `[0,1]`.
    #[serde(default = "one_prop")]
    pub opacity: Prop,
}

fn one_prop() -> Prop {
    Prop::Const(1.0)
}

impl Transform {
    /// A neutral transform: anchor at origin, placed at `(x, y)`, unit
    /// scale, no rotation, full opacity.
    pub fn at(x: f64, y: f64) -> Self {
        Transform {
            anchor: [Prop::Const(0.0), Prop::Const(0.0)],
            position: [Prop::Const(x), Prop::Const(y)],
            scale: [Prop::Const(1.0), Prop::Const(1.0)],
            rotation_deg: Prop::Const(0.0),
            opacity: Prop::Const(1.0),
        }
    }
}
