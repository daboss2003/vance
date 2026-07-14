//! `vance-fusion` — a node-graph image compositor (`.vfc`).
//!
//! A [`Comp`] is a DAG of [`Node`]s. Source nodes generate an image, filter
//! nodes transform one input, and combiners blend/mask two. Every node's
//! output is the size of the comp canvas, so wiring is always type-safe and
//! the [`Comp::output`] node is the rendered frame.
//!
//! [`Compositor::render`] evaluates the graph at a comp-relative time. The
//! evaluator is ITERATIVE (an explicit worklist post-order, not native
//! recursion) so an arbitrarily deep chain can never overflow the stack, and
//! it is REFERENCE-COUNTED: each intermediate image is freed the moment its
//! last consumer has read it, bounding peak memory to the active frontier
//! rather than one image per node. A hard live-memory budget fails a
//! pathological comp gracefully to a transparent canvas. Host images (e.g.
//! the clip's decoded frame) are injected by name via the `inputs` map and
//! read by [`NodeKind::MediaIn`], so the crate stays pure/CPU/headless —
//! no decode or GPU dependency.

#![warn(missing_docs)]

pub mod eval;
pub mod model;
pub mod ops;

use std::collections::HashMap;

pub use eval::AnimCtx;
pub use model::{
    Anim, BgSpec, BlendMode, BlurMode, BlurSpec, ColorSpec, Comp, CropSpec, Fit, MaskChannel,
    MaskSpec, MediaSpec, MergeSpec, Node, NodeKind, RectSpec, TextSpec, TransformSpec,
};
use vance_mograph::raster::{composite, rounded_rect, LayerXform, MAX_DIM};
use vance_mograph::{FontDb, RgbaImage};

/// Hard ceiling on the number of reachable nodes evaluated in one render.
/// Defends against a pathological / corrupt `.vfc`; exceeding it renders a
/// transparent canvas instead of doing unbounded work. Far above any real
/// composition (interactive editing produces tens of nodes).
const MAX_NODES: usize = 10_000;

/// Ceiling on the comp canvas in pixels (`8192² = 67 MP`, i.e. 4× an 8K
/// frame). A larger canvas makes every node a >1 GiB image and blows up
/// per-op scratch (a blur allocates several f32 planes), so past this we
/// render a transparent frame of the requested size rather than do the
/// work. Real video resolutions (≤ 8K = ~33 MP) are unaffected.
const MAX_CANVAS_PIXELS: u64 = 8192 * 8192;

/// Live-image memory budget for one render. Intermediate images are freed as
/// soon as their last consumer has read them, but a pathologically WIDE graph
/// could still hold several canvases at once — past this ceiling we fail
/// gracefully to a transparent canvas rather than exhaust memory.
const MAX_LIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

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
    /// straight-alpha canvas the size of the comp; a missing/cyclic output,
    /// an over-large graph, or a comp that would exceed the memory budget
    /// yields a transparent canvas.
    pub fn render(
        &self,
        comp: &Comp,
        t_seconds: f64,
        inputs: &HashMap<String, RgbaImage>,
    ) -> RgbaImage {
        let (w, h) = (comp.width, comp.height);
        // Reject an extreme canvas before allocating anything but the
        // (unavoidable) requested-size output — bounds per-op scratch.
        if (w.max(1) as u64) * (h.max(1) as u64) > MAX_CANVAS_PIXELS {
            tracing::warn!(w, h, "fusion: canvas exceeds cap — rendering transparent");
            return RgbaImage::new(w, h);
        }
        let ctx = AnimCtx {
            width: w.max(1) as f64,
            height: h.max(1) as f64,
            fps: comp.fps,
            duration: comp.duration_s,
        };
        // Evaluation order (reachable-from-output, post-order) + per-node
        // consumer counts. `None` for a missing output or an over-large graph.
        let Some((order, mut remaining)) = topo_order(comp) else {
            tracing::warn!(nodes = comp.nodes.len(), "fusion: graph exceeds node cap — rendering transparent");
            return RgbaImage::new(w, h);
        };
        // Every node output is a full canvas; bound the simultaneously-live
        // set by bytes. Dims match RgbaImage::new's clamp so the estimate is
        // exact.
        let canvas_bytes =
            (w.clamp(1, MAX_DIM) as u64) * (h.clamp(1, MAX_DIM) as u64) * 4;

        let mut ready: HashMap<String, RgbaImage> = HashMap::new();
        for id in &order {
            let Some(node) = comp.node(id) else { continue };
            let slots = used_slots(&node.kind);

            // Gather the resolved image for each input slot (transparent when
            // unconnected / dangling).
            let mut slot_imgs: HashMap<&str, RgbaImage> = HashMap::new();
            for slot in slots {
                if let Some(src) = node.inputs.get(*slot) {
                    if let Some(img) = ready.get(src) {
                        slot_imgs.insert(slot, img.clone());
                    }
                }
            }
            let out = self.compute_node(node, &slot_imgs, t_seconds, ctx, inputs, w, h);

            // Retire consumed inputs: once a source's last consumer has read
            // it, drop it so peak memory tracks the active frontier.
            for slot in slots {
                if let Some(src) = node.inputs.get(*slot) {
                    if let Some(c) = remaining.get_mut(src) {
                        *c = c.saturating_sub(1);
                        if *c == 0 && src != &comp.output {
                            ready.remove(src);
                        }
                    }
                }
            }

            // Enforce the live-memory budget before adding this output.
            if (ready.len() as u64 + 1) * canvas_bytes > MAX_LIVE_BYTES {
                return RgbaImage::new(w, h);
            }
            ready.insert(id.clone(), out);
        }
        ready
            .remove(&comp.output)
            .unwrap_or_else(|| RgbaImage::new(w, h))
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

    /// Rasterize one node given its already-resolved input images (keyed by
    /// slot). Sources ignore `slot_imgs`; a filter/combiner reads its slots,
    /// treating a missing slot as a transparent canvas.
    fn compute_node(
        &self,
        node: &Node,
        slot_imgs: &HashMap<&str, RgbaImage>,
        t: f64,
        ctx: AnimCtx,
        inputs: &HashMap<String, RgbaImage>,
        w: u32,
        h: u32,
    ) -> RgbaImage {
        let get = |slot: &str| -> RgbaImage {
            slot_imgs
                .get(slot)
                .cloned()
                .unwrap_or_else(|| RgbaImage::new(w, h))
        };
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
                let input = get("input");
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
                    scale: [spec.scale[0].resolve(t, ctx), spec.scale[1].resolve(t, ctx)],
                    rotation_deg: spec.rotation_deg.resolve(t, ctx),
                    opacity: spec.opacity.resolve(t, ctx),
                };
                composite(&mut canvas, &input, xf);
                canvas
            }
            NodeKind::Blur(spec) => {
                let input = get("input");
                let passes = match spec.mode {
                    BlurMode::Box => 1,
                    BlurMode::Gaussian => 3,
                };
                ops::blur(&input, spec.radius.resolve(t, ctx), passes)
            }
            NodeKind::ColorCorrect(spec) => {
                let input = get("input");
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
                let input = get("input");
                ops::crop(
                    &input,
                    spec.left.resolve(t, ctx),
                    spec.top.resolve(t, ctx),
                    spec.right.resolve(t, ctx),
                    spec.bottom.resolve(t, ctx),
                )
            }
            NodeKind::Merge(spec) => {
                let bg = get("background");
                let fg = get("foreground");
                ops::merge(&bg, &fg, spec.mode, spec.opacity.resolve(t, ctx))
            }
            NodeKind::Mask(spec) => {
                let input = get("input");
                let mask = get("mask");
                ops::apply_mask(&input, &mask, spec.channel, spec.invert)
            }
        }
    }
}

/// The input slots a node kind reads, in read order.
fn used_slots(kind: &NodeKind) -> &'static [&'static str] {
    match kind {
        NodeKind::Transform(_)
        | NodeKind::Blur(_)
        | NodeKind::ColorCorrect(_)
        | NodeKind::Crop(_) => &["input"],
        NodeKind::Merge(_) => &["background", "foreground"],
        NodeKind::Mask(_) => &["input", "mask"],
        _ => &[],
    }
}

/// Iterative post-order over used-slot edges reachable from `comp.output`,
/// with gray/black coloring for cycle detection (a back-edge to a gray node
/// is skipped, breaking cycles). Returns the evaluation order plus, per node,
/// how many reachable consumers read it (so an image can be freed after its
/// last read). `None` when the output node is missing or the reachable graph
/// exceeds [`MAX_NODES`].
fn topo_order(comp: &Comp) -> Option<(Vec<String>, HashMap<String, usize>)> {
    comp.node(&comp.output)?;
    // 1 = gray (in progress), 2 = black (emitted).
    let mut color: HashMap<String, u8> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut stack: Vec<(String, bool)> = vec![(comp.output.clone(), false)];

    while let Some((id, expanded)) = stack.pop() {
        if expanded {
            // Finalize only if still gray (guards against duplicate markers).
            if color.get(&id) == Some(&1) {
                color.insert(id.clone(), 2);
                order.push(id);
            }
            continue;
        }
        if color.contains_key(&id) {
            continue; // already gray/black — don't re-enter
        }
        color.insert(id.clone(), 1);
        stack.push((id.clone(), true));
        if let Some(node) = comp.node(&id) {
            for slot in used_slots(&node.kind) {
                if let Some(src) = node.inputs.get(*slot) {
                    // Only descend into existing, not-yet-seen sources; a gray
                    // source is a cycle back-edge (skip), a black one is done.
                    if comp.node(src).is_some() && !color.contains_key(src) {
                        stack.push((src.clone(), false));
                    }
                }
            }
        }
        if order.len() > MAX_NODES || color.len() > MAX_NODES {
            return None;
        }
    }

    // Consumer counts over reachable used-slot edges.
    let reachable: std::collections::HashSet<&String> = order.iter().collect();
    let mut remaining: HashMap<String, usize> = HashMap::new();
    for id in &order {
        if let Some(node) = comp.node(id) {
            for slot in used_slots(&node.kind) {
                if let Some(src) = node.inputs.get(*slot) {
                    if reachable.contains(src) {
                        *remaining.entry(src.clone()).or_insert(0) += 1;
                    }
                }
            }
        }
    }
    Some((order, remaining))
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
    fn deep_chain_does_not_overflow_stack() {
        // GOAT-12 review: a long linear chain used to recurse N frames deep
        // and overflow the playback thread's 2 MiB stack (~400 nodes). The
        // iterative evaluator handles thousands without recursion.
        let n = 5000usize;
        let mut nodes = Vec::with_capacity(n + 1);
        nodes.push(Node::new(
            "src",
            "BG",
            NodeKind::Background(BgSpec { color: [0.2, 0.4, 0.6, 1.0], gradient_to: None }),
        ));
        // chain[i] -> chain[i+1] -> ... -> src, output = chain[0].
        for i in 0..n {
            let src = if i + 1 < n {
                format!("t{}", i + 1)
            } else {
                "src".to_string()
            };
            nodes.push(
                Node::new(
                    format!("t{i}"),
                    "T",
                    NodeKind::Transform(TransformSpec::identity(16, 16)),
                )
                .with_input("input", &src),
            );
        }
        let comp = Comp {
            width: 16,
            height: 16,
            fps: 30.0,
            duration_s: 5.0,
            background: [0.0, 0.0, 0.0, 1.0],
            nodes,
            output: "t0".into(),
        };
        // Run it on a thread with the same 2 MiB stack as the playback loop.
        let handle = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                let img = Compositor::new().render(&comp, 0.0, &HashMap::new());
                (img.width, img.pixels[3])
            })
            .unwrap();
        let (w, _a) = handle.join().expect("deep chain must not overflow the stack");
        assert_eq!(w, 16);
    }

    #[test]
    fn over_large_canvas_renders_transparent_without_node_work() {
        // A canvas past the pixel cap must fail fast to a transparent frame
        // of the requested size WITHOUT touching the graph (no giant per-op
        // scratch). The output node here would panic if evaluated (bad slot),
        // proving the early return skips evaluation. 8192*8200 > 8192².
        let boom = Node::new("out", "Blur", NodeKind::Blur(BlurSpec {
            radius: Anim::Const(1.0),
            mode: BlurMode::Box,
        }))
        .with_input("input", "out"); // self-cycle; must not be evaluated
        let comp = Comp {
            width: 8192,
            height: 8200,
            fps: 30.0,
            duration_s: 5.0,
            background: [0.0, 0.0, 0.0, 0.0],
            nodes: vec![boom],
            output: "out".into(),
        };
        let img = Compositor::new().render(&comp, 0.0, &no_inputs());
        assert_eq!(img.width, 8192);
        assert_eq!(img.height, 8200);
        assert!(img.pixels.iter().all(|&x| x == 0), "over-cap canvas → transparent");
    }

    #[test]
    fn over_many_nodes_renders_transparent() {
        // A graph with more reachable nodes than MAX_NODES fails gracefully.
        // Tiny canvas so this is cheap. Chain of MAX_NODES+2 Transforms.
        let n = MAX_NODES + 2;
        let mut nodes = Vec::with_capacity(n + 1);
        nodes.push(Node::new(
            "src",
            "BG",
            NodeKind::Background(BgSpec { color: [1.0, 1.0, 1.0, 1.0], gradient_to: None }),
        ));
        for i in 0..n {
            let src = if i + 1 < n { format!("t{}", i + 1) } else { "src".to_string() };
            nodes.push(
                Node::new(format!("t{i}"), "T", NodeKind::Transform(TransformSpec::identity(2, 2)))
                    .with_input("input", &src),
            );
        }
        let comp = Comp {
            width: 2,
            height: 2,
            fps: 30.0,
            duration_s: 5.0,
            background: [0.0, 0.0, 0.0, 0.0],
            nodes,
            output: "t0".into(),
        };
        let img = Compositor::new().render(&comp, 0.0, &no_inputs());
        assert_eq!(img.width, 2);
        assert!(img.pixels.iter().all(|&x| x == 0), "node cap exceeded → transparent");
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
        // A diamond: merge reads the same blurred bg twice. Ref-counted
        // eviction must keep the shared image alive for both reads and the
        // result must be correct.
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
