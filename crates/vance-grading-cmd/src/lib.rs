//! `vance-grading-cmd` — typed grading commands (Phase 6.13).
//!
//! Pure data: the AI assistant submits these in JSON; `vance-app` (the
//! only thing that owns the engine) translates each variant into the
//! corresponding `Engine::*` method call. Lives in its own crate so
//! `vance-ai-runtime` can pull just the *schema* without dragging in
//! `vance-app`/Tauri/wgpu.
//!
//! The `GradingCommand` enum IS the contract — JSON Schema generated
//! via `schemars` is the single source of truth for both Tauri IPC
//! (Svelte) and Anthropic tool-use (the AI assistant), so the prompt
//! and the engine can never drift.

#![warn(missing_docs)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ---- ID newtype -----------------------------------------------------------

/// A stringified UUID — `8-4-4-4-12` hyphenated lowercase hex (same form
/// the Tauri command layer accepts). `#[serde(transparent)]` keeps the
/// JSON wire format identical to a bare string; the manual `JsonSchema`
/// impl pins a regex pattern so the AI assistant's tool-use validator
/// catches malformed ids upfront instead of waiting for the engine to
/// reject them at runtime.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(transparent)]
pub struct Uuid(pub String);

impl From<String> for Uuid {
    fn from(s: String) -> Self {
        Self(s)
    }
}
impl From<&str> for Uuid {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}
impl std::ops::Deref for Uuid {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl AsRef<str> for Uuid {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl JsonSchema for Uuid {
    fn schema_name() -> String {
        "Uuid".into()
    }
    fn json_schema(_gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        let mut obj = schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::String.into()),
            ..Default::default()
        };
        obj.string = Some(Box::new(schemars::schema::StringValidation {
            pattern: Some(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$".into()),
            ..Default::default()
        }));
        obj.metadata = Some(Box::new(schemars::schema::Metadata {
            description: Some(
                "Hyphenated lowercase UUID, e.g. \"00000000-0000-0000-0000-000000000000\".".into(),
            ),
            ..Default::default()
        }));
        schemars::schema::Schema::Object(obj)
    }
}

// ---- Enumerations ---------------------------------------------------------

/// Family of color operations for a grade node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GradeMode {
    /// Primary wheels: lift / gamma / gain (per-channel + master).
    Primary,
    /// Log wheels: offset + shadow / mid / highlight balances.
    Log,
    /// Per-channel tone curves (+ HSL hue-wheel curves).
    Curves,
}

/// Which wheel within a node a `SetWheel` command targets. Primary mode
/// uses `Lift` / `Gamma` / `Gain`; log mode uses `Offset` / `Shadow` /
/// `Mid` / `Highlight`. The dispatcher rejects mismatches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WheelGroup {
    /// Primary mode: shadows lift (additive).
    Lift,
    /// Primary mode: midtone gamma (power).
    Gamma,
    /// Primary mode: highlight gain (multiplicative).
    Gain,
    /// Log mode: overall offset (RGB only, no master).
    Offset,
    /// Log mode: shadow balance.
    Shadow,
    /// Log mode: midtone balance.
    Mid,
    /// Log mode: highlight balance.
    Highlight,
}

/// Which tone-curve channel a `SetToneCurve` command targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToneChannel {
    /// Master tone curve, folded into R/G/B at bake time.
    Master,
    /// Red channel tone curve.
    R,
    /// Green channel tone curve.
    G,
    /// Blue channel tone curve.
    B,
    /// Luma curve (hue-preserving ratio).
    Luma,
}

/// Which HSL hue-wheel curve a `SetHslCurve` command targets. `hh`/`ss`/
/// `ll` are REMAPS (identity y=x); `hs`/`hl` are MULTIPLIERS keyed by
/// hue (identity y=0.5; shader decodes as `2*y`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HslCurve {
    /// Hue → hue REMAP, cyclic input.
    HueHue,
    /// Hue → saturation MULTIPLIER.
    HueSat,
    /// Hue → luminance MULTIPLIER.
    HueLum,
    /// Saturation → saturation REMAP.
    SatSat,
    /// Luminance → luminance REMAP.
    LumLum,
}

/// Geometric shape of a power window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WindowShape {
    /// Window disabled (mask = 1, exact passthrough).
    Off,
    /// Ellipse / circle (when half_x == half_y).
    Ellipse,
    /// Axis-aligned rectangle.
    Rectangle,
}

// ---- Sub-structures -------------------------------------------------------

/// One control point of a curve, in `[0, 1]` on both axes.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CurvePoint {
    /// Input level (x). For hue curves x is cyclic.
    pub x: f32,
    /// Output level (y).
    pub y: f32,
}

/// HSL qualifier hue band — center / range / softness, all `[0, 1]`.
/// Any omitted field is left unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct HueRange {
    /// Cyclic hue center.
    pub center: Option<f32>,
    /// Full hue-wedge width (1.0 = no restriction).
    pub range: Option<f32>,
    /// Edge softness.
    pub soft: Option<f32>,
}

/// HSL qualifier sat/luma band — lo / hi / softness, all `[0, 1]`.
/// Any omitted field is left unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct BandRange {
    /// Lower bound.
    pub lo: Option<f32>,
    /// Upper bound.
    pub hi: Option<f32>,
    /// Edge softness.
    pub soft: Option<f32>,
}

// ---- The command enum -----------------------------------------------------

/// One typed grading operation. Internally-tagged so the JSON carries
/// a stable `"op"` field (no shape-collision routing hazards) and every
/// variant uses struct fields with descriptive names suitable for an
/// LLM tool-use schema.
///
/// `track_id` / `clip_id` / `effect_id` are stringified UUIDs to match
/// the existing Tauri command convention.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum GradingCommand {
    /// Append a new grade node to a clip.
    AddGradeNode {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Initial mode (primary / log / curves).
        mode: GradeMode,
        /// Optional human-readable label.
        label: Option<String>,
    },
    /// Remove a grade node from a clip.
    RemoveGradeNode {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Effect UUID of the node.
        effect_id: Uuid,
    },
    /// Move a grade node to a new index in the chain (top-of-list = first
    /// applied; indices clamped to the chain length).
    ReorderGradeNode {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Effect UUID of the node to move.
        effect_id: Uuid,
        /// New 0-based index in the chain.
        new_index: u32,
    },
    /// Enable / bypass a grade node.
    SetGradeNodeEnabled {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Effect UUID.
        effect_id: Uuid,
        /// 1 = apply, 0 = bypass.
        enabled: bool,
    },
    /// Set the node's display label (UI-only; does not affect rendering).
    SetGradeNodeLabel {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Effect UUID.
        effect_id: Uuid,
        /// New label string.
        label: String,
    },
    /// Switch an existing node between primary / log / curves modes.
    SetNodeMode {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Effect UUID.
        effect_id: Uuid,
        /// New mode.
        mode: GradeMode,
    },
    /// Set a primary or log wheel's RGB balance (and optional master,
    /// primary mode only). `reset = true` snaps the wheel back to its
    /// neutral (RGB 0, master 1 for gamma/gain; master 0 for lift).
    SetWheel {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Effect UUID.
        effect_id: Uuid,
        /// Which wheel.
        group: WheelGroup,
        /// `[r, g, b]` in the wheel's natural range (omit to leave RGB
        /// unchanged).
        rgb: Option<[f32; 3]>,
        /// Master scalar (primary mode only; ignored on log wheels).
        master: Option<f32>,
        /// Snap this wheel back to neutral and ignore `rgb` / `master`.
        reset: Option<bool>,
    },
    /// Replace one tone curve's control points (Master / R / G / B / Luma).
    /// Pass an empty `points` list to reset to identity.
    SetToneCurve {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Effect UUID (must be a curves-mode node).
        effect_id: Uuid,
        /// Which tone channel.
        channel: ToneChannel,
        /// Control points; empty = identity (y = x for master/R/G/B,
        /// hue-preserving ratio = 1 for luma).
        points: Vec<CurvePoint>,
    },
    /// Replace one HSL hue-wheel curve's control points
    /// (HueHue / HueSat / HueLum / SatSat / LumLum). Pass an empty
    /// `points` list to reset to identity (y = x for remap curves;
    /// constant 0.5 for the hs/hl multiplier curves).
    SetHslCurve {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Effect UUID (must be a curves-mode node).
        effect_id: Uuid,
        /// Which HSL curve.
        curve: HslCurve,
        /// Control points; empty = identity.
        points: Vec<CurvePoint>,
    },
    /// Partial update of a node's HSL qualifier (secondary key). Any
    /// omitted sub-field is left unchanged. Setting `enabled = false`
    /// keeps the band/hue settings but stops applying them.
    SetQualifier {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Effect UUID.
        effect_id: Uuid,
        /// On / off (omit to leave unchanged).
        enabled: Option<bool>,
        /// Invert the mask (omit to leave unchanged).
        invert: Option<bool>,
        /// Hue band partial update.
        hue: Option<HueRange>,
        /// Saturation band partial update.
        sat: Option<BandRange>,
        /// Luma band partial update.
        luma: Option<BandRange>,
    },
    /// Partial update of a node's power window. `shape = Off` disables
    /// the window (mask = 1, exact passthrough). Any omitted field is
    /// left unchanged.
    SetWindow {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Effect UUID.
        effect_id: Uuid,
        /// Shape (off / ellipse / rectangle).
        shape: Option<WindowShape>,
        /// Center `(x, y)` in source-UV `[0, 1]`.
        center: Option<[f32; 2]>,
        /// Half-extents `(x, y)` in source-UV units.
        size: Option<[f32; 2]>,
        /// Rotation in radians (CCW about center).
        rotation: Option<f32>,
        /// Feather / softness band width.
        feather: Option<f32>,
        /// Invert the mask.
        invert: Option<bool>,
    },
    /// Run the point tracker over a power window's center forward N
    /// frames, writing keyframes to win_cx / win_cy. Synchronous —
    /// blocks the engine for ~N decode round-trips; capped at 240 frames
    /// per call by the engine. The decoder must already be open on this
    /// clip's source file.
    TrackWindow {
        /// Track UUID.
        track_id: Uuid,
        /// Clip UUID.
        clip_id: Uuid,
        /// Effect UUID (the power-window node).
        effect_id: Uuid,
        /// Source-time start (seconds in the clip's source media).
        start_source_seconds: f64,
        /// Number of frames to track (anchor + N-1 forward). Clamped
        /// engine-side to `[2, 240]`.
        n_frames: u32,
        /// Half-side of the template patch in pixels (e.g. 15 → 31×31).
        patch_half: u32,
        /// Per-frame max search radius in pixels.
        search_radius: u32,
    },
    /// Apply an OpenColorIO display transform to the viewport. `config`
    /// may be `None` to use the `$OCIO` env config. Phase 6.1b.
    SetOcioViewTransform {
        /// Path to the `.ocio` config (omit for `$OCIO` env config).
        config: Option<String>,
        /// Source colorspace / role name.
        src: String,
        /// Destination (display) colorspace / role name.
        dst: String,
        /// Baked cube side length (typical: 33).
        size: u32,
    },
    /// Set (or clear) the project's display LUT. `path = None` clears.
    SetProjectViewLut {
        /// Filesystem path to a `.cube` / `.3dl`. `None` clears.
        path: Option<String>,
    },
    /// Replace the project's working color space.
    SetProjectWorkingColorSpace {
        /// Color-space label (matches the engine's `color_space_to_str`
        /// values: e.g. `"linear_rec709"`, `"linear_rec2020"`, ...).
        color_space: String,
    },
}

/// Outcome of one applied command. Most commands return `None`; the few
/// that produce information for the caller use the variants below.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GradingCommandOutcome {
    /// No structured result; the command succeeded.
    None,
    /// `AddGradeNode` succeeded; the new effect's UUID.
    NodeAdded {
        /// New effect UUID (stringified).
        effect_id: Uuid,
    },
    /// `TrackWindow` succeeded; the tracker summary.
    Tracked {
        /// Number of keyframes written (incl. the anchor).
        frames_tracked: u32,
        /// Mean ZNCC score in `[-1, 1]` over all written frames.
        mean_score: f32,
        /// Clip-relative seconds of the first keyframe.
        start_clip_seconds: f64,
        /// Clip-relative seconds of the last keyframe.
        end_clip_seconds: f64,
    },
}

/// Schema-export entry point: hand this to the AI runtime to embed in
/// the model's tool-use spec. Single source of truth — kept in sync
/// with `GradingCommand` automatically by `schemars`.
pub fn grading_command_schema() -> schemars::schema::RootSchema {
    schemars::schema_for!(GradingCommand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let cmd = GradingCommand::SetWheel {
            track_id: "00000000-0000-0000-0000-000000000001".into(),
            clip_id: "00000000-0000-0000-0000-000000000002".into(),
            effect_id: "00000000-0000-0000-0000-000000000003".into(),
            group: WheelGroup::Lift,
            rgb: Some([0.1, -0.05, 0.0]),
            master: Some(0.02),
            reset: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        // Tagged JSON: must carry "op":"set_wheel" and snake_case fields.
        assert!(json.contains("\"op\":\"set_wheel\""), "tag missing: {json}");
        assert!(json.contains("\"group\":\"lift\""));
        let back: GradingCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn partial_update_omits_none_fields() {
        // Setting only a node label leaves other qualifier fields out
        // of the deserialized command (None semantics).
        let cmd = GradingCommand::SetQualifier {
            track_id: "00000000-0000-0000-0000-000000000001".into(),
            clip_id: "00000000-0000-0000-0000-000000000002".into(),
            effect_id: "00000000-0000-0000-0000-000000000003".into(),
            enabled: Some(true),
            invert: None,
            hue: None,
            sat: None,
            luma: None,
        };
        let back: GradingCommand =
            serde_json::from_str(&serde_json::to_string(&cmd).unwrap()).unwrap();
        assert_eq!(cmd, back);
        if let GradingCommand::SetQualifier {
            hue,
            sat,
            luma,
            invert,
            enabled,
            ..
        } = back
        {
            assert!(hue.is_none() && sat.is_none() && luma.is_none() && invert.is_none());
            assert_eq!(enabled, Some(true));
        } else {
            panic!("wrong variant");
        }
    }

    #[test]
    fn schema_generates_with_descriptions() {
        let schema = grading_command_schema();
        let json = serde_json::to_string(&schema).unwrap();
        // The doc comments on the enum + variants should land in the
        // schema as descriptions so the LLM sees them.
        assert!(json.contains("track_id"));
        assert!(json.contains("set_wheel") || json.contains("SetWheel"));
        // Tag discriminator present.
        assert!(json.contains("\"op\""));
    }

    #[test]
    fn outcome_round_trips() {
        let r = GradingCommandOutcome::Tracked {
            frames_tracked: 60,
            mean_score: 0.97,
            start_clip_seconds: 0.0,
            end_clip_seconds: 2.0,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"kind\":\"tracked\""));
        let back: GradingCommandOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }
}
