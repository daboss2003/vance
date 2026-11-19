//! `vance-mograph` — Vance Motion Graphics Templates (`.cmgt`).
//!
//! A parameterized, animated, layered title/graphic engine. A
//! [`MographTemplate`] declares editor-facing [`Control`]s and a layer
//! rig; a [`MographInstance`] pairs a template with the editor's control
//! overrides. [`MographEngine::render`] evaluates the rig at a
//! clip-relative time and rasterizes it (CPU) to a straight-alpha
//! [`RgbaImage`], ready to flatten into the video pipeline.
//!
//! Design notes:
//! - Pure-CPU rasterization (fontdue glyphs + analytic rounded rects +
//!   premultiplied affine compositing) — no GPU dependency, so it's
//!   headless-testable and slots beside the existing title path.
//! - Properties are [`Prop`] (const / keyframes / `vance-expr` / control
//!   binding), so a template animates AND exposes knobs from the same
//!   model.
//! - Serde + RON round-trippable end to end.

#![warn(missing_docs)]

pub mod builtins;
pub mod eval;
pub mod fonts;
pub mod model;
pub mod raster;
pub mod value;

pub use eval::EvalDims;
pub use fonts::FontDb;
pub use model::{
    Align, Canvas, ColorProp, Content, Control, ControlKind, Interp, Key, Layer, Meta,
    MographTemplate, Prop, RectSpec, Rgba, TextProp, TextSpec, Transform,
};
pub use raster::{composite, rounded_rect, LayerXform, RgbaImage};
pub use value::{ControlValue, MographInstance};

/// Errors from template (de)serialization.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MographError {
    /// RON parse/format error.
    #[error("template RON: {0}")]
    Ron(String),
    /// JSON parse/format error.
    #[error("template JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// The render engine. Holds the system [`FontDb`] (scanned once) so
/// repeated renders reuse cached faces.
#[derive(Debug)]
pub struct MographEngine {
    fonts: FontDb,
}

impl Default for MographEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl MographEngine {
    /// Construct, scanning system fonts once.
    pub fn new() -> Self {
        MographEngine {
            fonts: FontDb::system(),
        }
    }

    /// The font database (for populating a `Font` control's options).
    pub fn fonts(&self) -> &FontDb {
        &self.fonts
    }

    /// Render `inst` at clip-relative `t_seconds`. `fps`/`duration_s`
    /// feed expression context (`frame`, `fps`, `duration`). Returns a
    /// straight-alpha RGBA canvas the size of the template's canvas.
    pub fn render(
        &self,
        inst: &MographInstance,
        t_seconds: f64,
        fps: f64,
        duration_s: f64,
    ) -> RgbaImage {
        let cw = inst.template.canvas.width.max(1);
        let ch = inst.template.canvas.height.max(1);
        let dims = EvalDims {
            width: cw as f64,
            height: ch as f64,
            fps,
            duration: duration_s,
        };
        let mut canvas = RgbaImage::new(cw, ch);

        for layer in &inst.template.layers {
            if !layer.visible {
                continue;
            }
            let t = &layer.transform;
            let xform = LayerXform {
                anchor: [
                    t.anchor[0].resolve(inst, t_seconds, dims),
                    t.anchor[1].resolve(inst, t_seconds, dims),
                ],
                position: [
                    t.position[0].resolve(inst, t_seconds, dims),
                    t.position[1].resolve(inst, t_seconds, dims),
                ],
                scale: [
                    t.scale[0].resolve(inst, t_seconds, dims),
                    t.scale[1].resolve(inst, t_seconds, dims),
                ],
                rotation_deg: t.rotation_deg.resolve(inst, t_seconds, dims),
                opacity: t.opacity.resolve(inst, t_seconds, dims),
            };
            if xform.opacity <= 0.0 {
                continue;
            }
            let layer_img = self.rasterize_content(inst, layer, t_seconds, dims);
            composite(&mut canvas, &layer_img, xform);
        }
        canvas
    }

    /// Render `inst` and flatten over the template's canvas background,
    /// returning a packed opaque RGB8 buffer (`width × height × 3`) — the
    /// shape the YUV title path consumes for an opaque card.
    pub fn render_rgb(
        &self,
        inst: &MographInstance,
        t_seconds: f64,
        fps: f64,
        duration_s: f64,
    ) -> (u32, u32, Vec<u8>) {
        let img = self.render(inst, t_seconds, fps, duration_s);
        let rgb = img.flatten_rgb(inst.template.canvas.background);
        (img.width, img.height, rgb)
    }

    fn rasterize_content(
        &self,
        inst: &MographInstance,
        layer: &Layer,
        t: f64,
        dims: EvalDims,
    ) -> RgbaImage {
        match &layer.content {
            Content::Rect(spec) => {
                let w = spec.width.resolve(inst, t, dims).max(0.0).round() as u32;
                let h = spec.height.resolve(inst, t, dims).max(0.0).round() as u32;
                if w == 0 || h == 0 {
                    return RgbaImage::new(1, 1);
                }
                let radius = spec.radius.resolve(inst, t, dims) as f32;
                rounded_rect(w, h, radius, spec.color.resolve(inst))
            }
            Content::Text(spec) => {
                let text = spec.text.resolve(inst);
                if text.is_empty() {
                    return RgbaImage::new(1, 1);
                }
                let font_name = spec.font.resolve(inst);
                let Some(face) = self.fonts.face(&font_name) else {
                    return RgbaImage::new(1, 1);
                };
                let size = spec.size_px.resolve(inst, t, dims) as f32;
                raster::render_text(
                    &face,
                    &text,
                    size,
                    spec.color.resolve(inst),
                    spec.align,
                    spec.line_height,
                    spec.tracking as f32,
                    spec.max_width.map(|w| w as f32),
                )
                .image
            }
        }
    }
}

/// Serialize a template to pretty RON (the `.cmgt` on-disk form).
pub fn to_ron(t: &MographTemplate) -> Result<String, MographError> {
    ron::ser::to_string_pretty(t, ron::ser::PrettyConfig::default())
        .map_err(|e| MographError::Ron(e.to_string()))
}

/// Parse a `.cmgt` template from RON.
pub fn from_ron(s: &str) -> Result<MographTemplate, MographError> {
    ron::from_str(s).map_err(|e| MographError::Ron(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_ron_round_trips() {
        let t = builtins::lower_third();
        let s = to_ron(&t).expect("serialize");
        let back = from_ron(&s).expect("parse");
        assert_eq!(t, back, "template must survive RON round-trip");
    }

    #[test]
    fn instance_json_round_trips() {
        let mut inst = MographInstance::new(builtins::centered_title());
        inst.set("headline", ControlValue::Text("Hello".into()));
        let j = serde_json::to_string(&inst).unwrap();
        let back: MographInstance = serde_json::from_str(&j).unwrap();
        assert_eq!(inst, back);
    }

    #[test]
    fn set_rejects_unknown_and_typemismatch() {
        let mut inst = MographInstance::new(builtins::centered_title());
        assert!(
            !inst.set("nope", ControlValue::Text("x".into())),
            "unknown id"
        );
        assert!(
            !inst.set("headline", ControlValue::Number(3.0)),
            "type mismatch"
        );
        assert!(inst.set("headline", ControlValue::Text("ok".into())));
    }

    #[test]
    fn render_produces_canvas_sized_rgba() {
        let eng = MographEngine::new();
        let inst = MographInstance::new(builtins::centered_title());
        let img = eng.render(&inst, 1.0, 30.0, 5.0);
        assert_eq!(img.width, inst.template.canvas.width);
        assert_eq!(img.height, inst.template.canvas.height);
        assert_eq!(img.pixels.len(), (img.width * img.height * 4) as usize);
    }

    #[test]
    fn opacity_keyframe_fades_in() {
        // The centered title's text fades 0→1 over the first 0.5s, so a
        // pixel-energy sum at t=0 is < the sum at t=1 (when a font exists).
        let eng = MographEngine::new();
        if eng.fonts().is_empty() {
            return; // headless host without fonts — skip
        }
        let inst = MographInstance::new(builtins::centered_title());
        let energy = |img: &RgbaImage| -> u64 { img.pixels.iter().map(|&b| b as u64).sum() };
        let early = energy(&eng.render(&inst, 0.0, 30.0, 5.0));
        let late = energy(&eng.render(&inst, 1.0, 30.0, 5.0));
        assert!(late > early, "title should fade in: {early} -> {late}");
    }
}
