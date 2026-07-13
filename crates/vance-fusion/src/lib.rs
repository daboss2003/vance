//! `vance-fusion` — a node-graph image compositor (`.vfc`).
//!
//! A [`Comp`] is a DAG of [`Node`]s. Source nodes generate an image, filter
//! nodes transform one input, and combiners blend/mask two. Every node's
//! output is the size of the comp canvas, so wiring is always type-safe and
//! the [`Comp::output`] node is the rendered frame.
//!
//! [`Compositor::render`] evaluates the graph at a comp-relative time,
//! memoizing shared sub-graphs and rejecting cycles, and returns a
//! straight-alpha [`RgbaImage`]. Host images (e.g. the clip's decoded frame)
//! are injected by name via the `inputs` map and read by [`NodeKind::MediaIn`],
//! so the crate stays pure/CPU/headless-testable — no decode or GPU
//! dependency.

#![warn(missing_docs)]

pub mod eval;
pub mod model;
pub mod ops;

use std::collections::{BTreeSet, HashMap};

pub use eval::AnimCtx;
pub use model::{
    Anim, BgSpec, BlendMode, BlurMode, BlurSpec, ColorSpec, Comp, CropSpec, Fit, MaskChannel,
    MaskSpec, MediaSpec, MergeSpec, Node, NodeKind, RectSpec, TextSpec, TransformSpec,
};
use vance_mograph::raster::{composite, rounded_rect, LayerXform};
use vance_mograph::{FontDb, RgbaImage};

/// Errors from composition (de)serialization.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FusionError {
    /// RON parse/format error.
    #[error("composition RON: {0}")]
    Ron(String),
    /// JSON parse/format error.
    #[error("composition JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// The node compositor. Holds the system [`FontDb`] (scanned once) for text
/// nodes, so repeated renders reuse cached faces.
#[derive(Debug)]
pub struct Compositor {
    fonts: FontDb,
}

impl Default for Compositor {
    fn default() -> Self {
        Self::new()
    }
}

impl Compositor {
    /// Construct, scanning system fonts once.
    pub fn new() -> Self {
        Compositor {
            fonts: FontDb::system(),
        }
    }

    /// The font database (for populating font pickers).
    pub fn fonts(&self) -> &FontDb {
        &self.fonts
    }

    /// Render `comp` at comp-relative `t_seconds`. `inputs` supplies host
    /// images for [`NodeKind::MediaIn`] nodes (by name). Returns a
    /// straight-alpha canvas the size of the comp; a missing/cyclic output
    /// yields a transparent canvas.
    pub fn render(
        &self,
        comp: &Comp,
        t_seconds: f64,
        inputs: &HashMap<String, RgbaImage>,
    ) -> RgbaImage {
        let ctx = AnimCtx {
            width: comp.width.max(1) as f64,
            height: comp.height.max(1) as f64,
            fps: comp.fps,
            duration: comp.duration_s,
        };
        let mut cache: HashMap<String, RgbaImage> = HashMap::new();
        let mut visiting: BTreeSet<String> = BTreeSet::new();
        self.eval_node(comp, &comp.output, t_seconds, ctx, inputs, &mut cache, &mut visiting, 0)
            .unwrap_or_else(|| RgbaImage::new(comp.width, comp.height))
    }

    /// Render and flatten over the comp background into a packed opaque RGB8
    /// buffer (`width × height × 3`) for the YUV path.
    pub fn render_rgb(
        &self,
        comp: &Comp,
        t_seconds: f64,
        inputs: &HashMap<String, RgbaImage>,
    ) -> (u32, u32, Vec<u8>) {
        let img = self.render(comp, t_seconds, inputs);
        let rgb = img.flatten_rgb(comp.background);
        (img.width, img.height, rgb)
    }

    /// Evaluate one node (memoized). Returns `None` for a missing node, a
    /// cycle, or a depth blow-out (all treated as "transparent").
    #[allow(clippy::too_many_arguments)]
    fn eval_node(
        &self,
        comp: &Comp,
        id: &str,
        t: f64,
        ctx: AnimCtx,
        inputs: &HashMap<String, RgbaImage>,
        cache: &mut HashMap<String, RgbaImage>,
        visiting: &mut BTreeSet<String>,
        depth: usize,
    ) -> Option<RgbaImage> {
        if let Some(img) = cache.get(id) {
            return Some(img.clone());
        }
        if depth > comp.nodes.len() + 1 {
            return None; // pathological depth — bail
        }
        if !visiting.insert(id.to_string()) {
            return None; // cycle
        }
        let result = comp.node(id).map(|node| {
            self.eval_kind(comp, node, t, ctx, inputs, cache, visiting, depth)
        });
        visiting.remove(id);
        let out = result?;
        cache.insert(id.to_string(), out.clone());
        Some(out)
    }

    /// Resolve a named input slot to an image (transparent if unconnected).
    #[allow(clippy::too_many_arguments)]
    fn input_image(
        &self,
        comp: &Comp,
        node: &Node,
        slot: &str,
        t: f64,
        ctx: AnimCtx,
        inputs: &HashMap<String, RgbaImage>,
        cache: &mut HashMap<String, RgbaImage>,
        visiting: &mut BTreeSet<String>,
        depth: usize,
    ) -> RgbaImage {
        node.inputs
            .get(slot)
            .and_then(|src| {
                self.eval_node(comp, src, t, ctx, inputs, cache, visiting, depth + 1)
            })
            .unwrap_or_else(|| RgbaImage::new(comp.width, comp.height))
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_kind(
        &self,
        comp: &Comp,
        node: &Node,
        t: f64,
        ctx: AnimCtx,
        inputs: &HashMap<String, RgbaImage>,
        cache: &mut HashMap<String, RgbaImage>,
        visiting: &mut BTreeSet<String>,
        depth: usize,
    ) -> RgbaImage {
        let (w, h) = (comp.width, comp.height);
        match &node.kind {
            NodeKind::Background(spec) => {
                let mut img = RgbaImage::new(w, h);
                ops::fill_background(&mut img, spec.color, spec.gradient_to);
                img
            }
            NodeKind::MediaIn(spec) => {
                let mut canvas = RgbaImage::new(w, h);
                if let Some(src) = inputs.get(&spec.name) {
                    ops::place_media(&mut canvas, src, spec.fit);
                }
                canvas
            }
            NodeKind::Rectangle(spec) => {
                let mut canvas = RgbaImage::new(w, h);
                let rw = spec.width.resolve(t, ctx).max(0.0).round() as u32;
                let rh = spec.height.resolve(t, ctx).max(0.0).round() as u32;
                if rw > 0 && rh > 0 {
                    let rect = rounded_rect(rw, rh, spec.radius.resolve(t, ctx) as f32, spec.color);
                    composite(&mut canvas, &rect, centered(w, h));
                }
                canvas
            }
            NodeKind::Text(spec) => {
                let mut canvas = RgbaImage::new(w, h);
                if !spec.text.is_empty() {
                    if let Some(face) = self.fonts.face(&spec.font) {
                        let block = vance_mograph::raster::render_text(
                            &face,
                            &spec.text,
                            spec.size_px.resolve(t, ctx) as f32,
                            spec.color,
                            spec.align,
                            spec.line_height,
                            spec.tracking as f32,
                            spec.max_width.map(|x| x as f32),
                        );
                        composite(&mut canvas, &block.image, centered(w, h));
                    }
                }
                canvas
            }
            NodeKind::Transform(spec) => {
                let input = self.input_image(
                    comp, node, "input", t, ctx, inputs, cache, visiting, depth,
                );
                let mut canvas = RgbaImage::new(w, h);
                let xf = LayerXform {
                    anchor: [
                        spec.anchor[0].resolve(t, ctx),
                        spec.anchor[1].resolve(t, ctx),
                    ],
                    position: [
                        spec.position[0].resolve(t, ctx),
                        spec.position[1].resolve(t, ctx),
                    ],
                    scale: [
                        spec.scale[0].resolve(t, ctx),
                        spec.scale[1].resolve(t, ctx),
                    ],
                    rotation_deg: spec.rotation_deg.resolve(t, ctx),
                    opacity: spec.opacity.resolve(t, ctx),
                };
                composite(&mut canvas, &input, xf);
                canvas
            }
            NodeKind::Blur(spec) => {
                let input = self.input_image(
                    comp, node, "input", t, ctx, inputs, cache, visiting, depth,
                );
                let passes = match spec.mode {
                    BlurMode::Box => 1,
                    BlurMode::Gaussian => 3,
                };
                ops::blur(&input, spec.radius.resolve(t, ctx), passes)
            }
            NodeKind::ColorCorrect(spec) => {
                let input = self.input_image(
                    comp, node, "input", t, ctx, inputs, cache, visiting, depth,
                );
                let lift = [
                    spec.lift[0].resolve(t, ctx),
                    spec.lift[1].resolve(t, ctx),
                    spec.lift[2].resolve(t, ctx),
                ];
                let gain = [
                    spec.gain[0].resolve(t, ctx),
                    spec.gain[1].resolve(t, ctx),
                    spec.gain[2].resolve(t, ctx),
                ];
                ops::color_correct(
                    &input,
                    lift,
                    gain,
                    spec.gamma.resolve(t, ctx),
                    spec.saturation.resolve(t, ctx),
                )
            }
            NodeKind::Crop(spec) => {
                let input = self.input_image(
                    comp, node, "input", t, ctx, inputs, cache, visiting, depth,
                );
                ops::crop(
                    &input,
                    spec.left.resolve(t, ctx),
                    spec.top.resolve(t, ctx),
                    spec.right.resolve(t, ctx),
                    spec.bottom.resolve(t, ctx),
                )
            }
            NodeKind::Merge(spec) => {
                let bg = self.input_image(
                    comp, node, "background", t, ctx, inputs, cache, visiting, depth,
                );
                let fg = self.input_image(
                    comp, node, "foreground", t, ctx, inputs, cache, visiting, depth,
                );
                ops::merge(&bg, &fg, spec.mode, spec.opacity.resolve(t, ctx))
            }
            NodeKind::Mask(spec) => {
                let input = self.input_image(
                    comp, node, "input", t, ctx, inputs, cache, visiting, depth,
                );
                let mask = self.input_image(
                    comp, node, "mask", t, ctx, inputs, cache, visiting, depth,
                );
                ops::apply_mask(&input, &mask, spec.channel, spec.invert)
            }
        }
    }
}

/// A centered placement transform for a generator's intrinsic raster onto a
/// `w×h` canvas.
fn centered(w: u32, h: u32) -> LayerXform {
    LayerXform {
        anchor: [0.5, 0.5],
        position: [w as f64 / 2.0, h as f64 / 2.0],
        scale: [1.0, 1.0],
        rotation_deg: 0.0,
        opacity: 1.0,
    }
}

/// Serialize a comp to pretty RON (the `.vfc` on-disk form).
pub fn to_ron(c: &Comp) -> Result<String, FusionError> {
    ron::ser::to_string_pretty(c, ron::ser::PrettyConfig::default())
        .map_err(|e| FusionError::Ron(e.to_string()))
}

/// Parse a `.vfc` composition from RON.
pub fn from_ron(s: &str) -> Result<Comp, FusionError> {
    ron::from_str(s).map_err(|e| FusionError::Ron(e.to_string()))
}

/// A starter composition: a title over a vertical-gradient background,
/// merged, `width×height`. Handy as a default when the editor adds a comp.
pub fn starter(width: u32, height: u32) -> Comp {
    let bg = Node::new(
        "bg",
        "Background",
        NodeKind::Background(BgSpec {
            color: [0.05, 0.07, 0.12, 1.0],
            gradient_to: Some([0.02, 0.02, 0.04, 1.0]),
        }),
    )
    .at(0.0, 0.0);
    let title = Node::new(
        "title",
        "Title",
        NodeKind::Text(TextSpec {
            text: "Vance".into(),
            font: "Arial".into(),
            size_px: Anim::Const((height as f64 / 8.0).max(24.0)),
            color: [1.0, 1.0, 1.0, 1.0],
            align: vance_mograph::Align::Center,
            line_height: 1.2,
            tracking: 0.0,
            max_width: None,
        }),
    )
    .at(0.0, 160.0);
    let merge = Node::new(
        "out",
        "Merge",
        NodeKind::Merge(MergeSpec {
            mode: BlendMode::Over,
            opacity: Anim::Const(1.0),
        }),
    )
    .with_input("background", "bg")
    .with_input("foreground", "title")
    .at(320.0, 80.0);
    Comp {
        width,
        height,
        fps: 30.0,
        duration_s: 5.0,
        background: [0.0, 0.0, 0.0, 1.0],
        nodes: vec![bg, title, merge],
        output: "out".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_inputs() -> HashMap<String, RgbaImage> {
        HashMap::new()
    }

    #[test]
    fn comp_ron_round_trips() {
        let c = starter(1280, 720);
        let s = to_ron(&c).expect("serialize");
        let back = from_ron(&s).expect("parse");
        assert_eq!(c, back, "comp must survive RON round-trip");
    }

    #[test]
    fn render_produces_canvas_sized_frame() {
        let comp = starter(320, 240);
        let img = Compositor::new().render(&comp, 1.0, &no_inputs());
        assert_eq!(img.width, 320);
        assert_eq!(img.height, 240);
        assert_eq!(img.pixels.len(), 320 * 240 * 4);
        // The gradient background makes the top-left corner opaque + dark.
        assert_eq!(img.pixels[3], 255, "opaque background");
    }

    #[test]
    fn missing_output_is_transparent_canvas() {
        let mut comp = starter(64, 64);
        comp.output = "does-not-exist".into();
        let img = Compositor::new().render(&comp, 0.0, &no_inputs());
        assert_eq!(img.width, 64);
        assert!(img.pixels.iter().all(|&b| b == 0), "transparent");
    }

    #[test]
    fn cycle_does_not_hang_and_yields_transparent() {
        // out(Merge) ← foreground ← blur ← input ← out : a cycle.
        let blur = Node::new(
            "blur",
            "Blur",
            NodeKind::Blur(BlurSpec {
                radius: Anim::Const(2.0),
                mode: BlurMode::Box,
            }),
        )
        .with_input("input", "out");
        let merge = Node::new(
            "out",
            "Merge",
            NodeKind::Merge(MergeSpec {
                mode: BlendMode::Over,
                opacity: Anim::Const(1.0),
            }),
        )
        .with_input("background", "blur")
        .with_input("foreground", "blur");
        let comp = Comp {
            width: 32,
            height: 32,
            fps: 30.0,
            duration_s: 5.0,
            background: [0.0, 0.0, 0.0, 0.0],
            nodes: vec![blur, merge],
            output: "out".into(),
        };
        // Must terminate (cycle broken) and produce a canvas.
        let img = Compositor::new().render(&comp, 0.0, &no_inputs());
        assert_eq!(img.width, 32);
    }

    #[test]
    fn media_in_places_host_image() {
        let comp = Comp {
            width: 100,
            height: 100,
            fps: 30.0,
            duration_s: 5.0,
            background: [0.0, 0.0, 0.0, 0.0],
            nodes: vec![Node::new(
                "m",
                "Media",
                NodeKind::MediaIn(MediaSpec {
                    name: "source".into(),
                    fit: Fit::Stretch,
                }),
            )],
            output: "m".into(),
        };
        let mut inputs = HashMap::new();
        inputs.insert("source".to_string(), RgbaImage::filled(10, 10, [1.0, 0.0, 0.0, 1.0]));
        let img = Compositor::new().render(&comp, 0.0, &inputs);
        // Stretched red fills the canvas — center is opaque red.
        let c = ((50 * 100 + 50) * 4) as usize;
        assert_eq!(img.pixels[c + 3], 255);
        assert!(img.pixels[c] > 200, "red channel high");
    }

    #[test]
    fn shared_subgraph_evaluates_once() {
        // A diamond: merge reads the same blurred bg twice. Memoization must
        // make this correct (and it terminates). Just assert it renders.
        let bg = Node::new(
            "bg",
            "BG",
            NodeKind::Background(BgSpec {
                color: [0.5, 0.5, 0.5, 1.0],
                gradient_to: None,
            }),
        );
        let blur = Node::new(
            "blur",
            "Blur",
            NodeKind::Blur(BlurSpec {
                radius: Anim::Const(1.0),
                mode: BlurMode::Box,
            }),
        )
        .with_input("input", "bg");
        let merge = Node::new(
            "out",
            "Merge",
            NodeKind::Merge(MergeSpec {
                mode: BlendMode::Multiply,
                opacity: Anim::Const(1.0),
            }),
        )
        .with_input("background", "blur")
        .with_input("foreground", "blur");
        let comp = Comp {
            width: 16,
            height: 16,
            fps: 30.0,
            duration_s: 5.0,
            background: [0.0, 0.0, 0.0, 1.0],
            nodes: vec![bg, blur, merge],
            output: "out".into(),
        };
        let img = Compositor::new().render(&comp, 0.0, &no_inputs());
        // 0.5 multiplied by 0.5 → ~0.25 → ~64.
        assert!((img.pixels[0] as i32 - 64).abs() <= 4);
    }
}
