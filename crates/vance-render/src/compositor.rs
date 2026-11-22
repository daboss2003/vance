//! Multi-track compositor (ADR 0007).
//!
//! Three GPU passes per frame: a layer pass per visible clip into a reusable
//! `Rgba16Float` slot keyed by clip ([`Compositor::render_layer`]), a
//! composite pass applying the clip's blend mode and opacity on
//! premultiplied values ([`Compositor::composite`]), and an output pass
//! (working → output transform, transfer encode, preview-only view LUT).
//!
//! Blending is display-referred (encoded) for SDR output and linear working
//! space for HDR output ([`OutputSpec::hdr`]).
//!
//! The RGBA rasterizers for synthetic media ([`render_title_rgba`],
//! [`render_mograph_rgba`], [`render_fusion_rgba`]) keep the background's
//! alpha instead of flattening it, so titles can sit over lower tracks.

use std::collections::HashMap;
use std::sync::Arc;

use vance_decoder::FrameData;
use vance_fusion::Comp;
pub use vance_mograph::RgbaImage;
use vance_mograph::{MographEngine, MographInstance};

use crate::renderer::{GradeNodeParams, RenderError};
use crate::title::{TitleError, TitleParams};

/// Where a layer's pixels come from.
#[derive(Clone, Copy, Debug)]
pub enum LayerSource<'a> {
    /// A decoded YUV frame (file-backed media).
    Yuv(&'a FrameData),
    /// A straight-alpha sRGB RGBA8 image (titles, MoGraph, Fusion, add-on
    /// output).
    Rgba(&'a RgbaImage),
}

/// Parameters of the `keyer` effect, evaluated at one time. See
/// `vance_timeline::keyer` for the keys and defaults.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyerParams {
    /// Key colour, encoded RGB in `[0, 1]`.
    pub key_rgb: [f32; 3],
    /// Weighting of the two non-primary channels.
    pub balance: f32,
    /// Matte gain.
    pub strength: f32,
    /// Matte values at or below this become transparent.
    pub clip_black: f32,
    /// Matte values at or above this become opaque.
    pub clip_white: f32,
    /// Erode (+) / dilate (−), in source pixels.
    pub choke_px: f32,
    /// Matte blur radius, in source pixels.
    pub soften_px: f32,
    /// Spill suppression amount.
    pub spill: f32,
    /// Spill reference: 0 = average of the other channels, 1 = their max.
    pub spill_bias: f32,
    /// Invert the matte.
    pub invert: bool,
}

/// Everything the layer pass needs for one clip at one time.
#[derive(Clone, Debug)]
pub struct LayerParams {
    /// Colour pipeline; the layer pass reads only the `input_*` fields and
    /// `input_to_working` (the output side belongs to [`OutputSpec`]).
    pub pipeline: vance_color::PipelineParams,
    /// Translation as a fraction of the canvas.
    pub translate: [f32; 2],
    /// Scale around the canvas centre.
    pub scale: [f32; 2],
    /// Rotation around the canvas centre, in radians.
    pub rotation: f32,
    /// Source crop fractions (left, right, top, bottom).
    pub crop_lrtb: [f32; 4],
    /// Additive brightness.
    pub brightness: f32,
    /// Contrast around 0.5.
    pub contrast: f32,
    /// Legacy `chroma_key` effect: `(key_rgb, tolerance, softness)`.
    pub chroma_key: Option<([f32; 3], f32, f32)>,
    /// Legacy `luma_key` effect: `(threshold, direction, softness)`.
    pub luma_key: Option<(f32, f32, f32)>,
    /// The `keyer` effect.
    pub keyer: Option<KeyerParams>,
    /// Colour-grade nodes, applied in order in working space.
    pub grades: Vec<GradeNodeParams>,
    /// Clip LUT, applied to the decoded source before linearising.
    pub lut: Option<Arc<vance_color::Lut3d>>,
    /// Show the keyer's matte instead of the keyed picture (preview only).
    pub matte_view: bool,
}

/// How a rendered layer combines with the accumulated backdrop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompositeOp {
    /// `vance_timeline::BlendMode::gpu_id`.
    pub blend_mode: u32,
    /// Layer opacity in `[0, 1]`, applied in the composite pass.
    pub opacity: f32,
}

/// The canvas and output encoding of one composited frame.
#[derive(Clone, Debug)]
pub struct OutputSpec {
    /// Canvas width in pixels.
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
    /// Linear working → output primaries.
    pub working_to_output: vance_color::Transform,
    /// Output transfer id (`vance_color::pipeline`).
    pub output_transfer: u32,
    /// `true` ⇒ composite in LINEAR working space (values above 1.0 must
    /// survive); `false` ⇒ composite display-encoded (user decision 2).
    pub hdr: bool,
    /// Apply the loaded view LUT in the output pass (preview only).
    pub view_lut: bool,
}

/// Identifies a layer slot across frames (a clip id's 128 bits), so an
/// unchanged clip can reuse its texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LayerKey(pub u128);

impl From<vance_timeline::ClipId> for LayerKey {
    fn from(id: vance_timeline::ClipId) -> Self {
        LayerKey(id.as_uuid().as_u128())
    }
}

/// A layer rendered during the current frame (valid until the next
/// [`Compositor::begin_frame`]).
#[derive(Clone, Copy, Debug)]
// Intentional: the slot index is read by the passes the render seam adds.
#[allow(dead_code)]
pub struct LayerHandle(u32);

/// Layer slots, accumulators and the composite/output pipelines. Owned by
/// the [`crate::Renderer`] and reached through `Renderer::compositor`, so it
/// shares the renderer's device.
pub struct Compositor {
    // Intentional: held for the GPU passes the render seam adds; nothing
    // reads them until then.
    #[allow(dead_code)]
    device: wgpu::Device,
    #[allow(dead_code)]
    queue: wgpu::Queue,
}

// Intentional: every pass returns `Unimplemented` until the render seam (R)
// lands the layer / composite / output shaders. Callers can be written and
// compiled against this API now.
impl Compositor {
    pub(crate) fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        Self { device, queue }
    }

    /// Start a frame: size the accumulators for `out` and clear to
    /// transparent black.
    pub fn begin_frame(&mut self, _out: &OutputSpec) -> Result<(), RenderError> {
        Err(RenderError::Unimplemented("Compositor::begin_frame"))
    }

    /// Run the layer pass for one clip into its slot, fitting the source
    /// into the canvas without distortion (transparent bars).
    pub fn render_layer(
        &mut self,
        _k: LayerKey,
        _s: LayerSource<'_>,
        _p: &LayerParams,
    ) -> Result<LayerHandle, RenderError> {
        Err(RenderError::Unimplemented("Compositor::render_layer"))
    }

    /// Read a rendered layer back as straight-alpha sRGB RGBA8 (for CPU
    /// add-ons).
    pub fn read_layer_rgba8(&mut self, _h: LayerHandle) -> Result<RgbaImage, RenderError> {
        Err(RenderError::Unimplemented("Compositor::read_layer_rgba8"))
    }

    /// Replace a rendered layer's pixels with straight-alpha sRGB RGBA8.
    pub fn write_layer_rgba8(
        &mut self,
        _h: LayerHandle,
        _img: &RgbaImage,
    ) -> Result<(), RenderError> {
        Err(RenderError::Unimplemented("Compositor::write_layer_rgba8"))
    }

    /// Composite a layer over the accumulated backdrop.
    pub fn composite(&mut self, _h: LayerHandle, _op: CompositeOp) -> Result<(), RenderError> {
        Err(RenderError::Unimplemented("Compositor::composite"))
    }

    /// Dissolve on one track: `lerp(blend(A over bd), blend(B over bd), mix)`.
    pub fn composite_dissolve(
        &mut self,
        _a: (LayerHandle, CompositeOp),
        _b: (LayerHandle, CompositeOp),
        _mix: f32,
    ) -> Result<(), RenderError> {
        Err(RenderError::Unimplemented("Compositor::composite_dissolve"))
    }

    /// Run the output pass into a surface view of format `f`.
    pub fn finish_to_view(
        &mut self,
        _v: &wgpu::TextureView,
        _f: wgpu::TextureFormat,
    ) -> Result<(), RenderError> {
        Err(RenderError::Unimplemented("Compositor::finish_to_view"))
    }

    /// Run the output pass and read back RGBA8 — the same bytes as
    /// `Renderer::render_to_rgba_buffer` for a single opaque layer.
    pub fn finish_to_rgba8(&mut self) -> Result<Vec<u8>, RenderError> {
        Err(RenderError::Unimplemented("Compositor::finish_to_rgba8"))
    }

    /// Run the output pass and read back RGBA16 — the same bytes as
    /// `Renderer::render_to_rgba64_buffer`.
    pub fn finish_to_rgba64(&mut self) -> Result<Vec<u8>, RenderError> {
        Err(RenderError::Unimplemented("Compositor::finish_to_rgba64"))
    }

    /// Drop the slots of every layer not in `live`.
    pub fn retain_only(&mut self, _live: &[LayerKey]) {}
}

/// Rasterize a title as straight-alpha sRGB RGBA8, its background at
/// `p.bg_alpha`.
pub fn render_title_rgba(p: &TitleParams) -> Result<RgbaImage, TitleError> {
    crate::title::render_title_rgba_image(p)
}

/// Render a MoGraph instance over its template background, keeping the
/// background's alpha (a transparent template stays transparent).
pub fn render_mograph_rgba(
    e: &MographEngine,
    i: &MographInstance,
    t: f64,
    fps: f64,
    dur: f64,
) -> RgbaImage {
    over_background(e.render(i, t, fps, dur), i.template.canvas.background)
}

/// Render a Fusion comp over its background, keeping the background's
/// alpha. `inputs` feeds the comp's `MediaIn` nodes by name.
pub fn render_fusion_rgba(
    c: &vance_fusion::Compositor,
    comp: &Comp,
    t: f64,
    inputs: &HashMap<String, RgbaImage>,
) -> RgbaImage {
    over_background(c.render(comp, t, inputs), comp.background)
}

/// Straight-alpha `img` over a solid `bg` (straight RGBA in `[0, 1]`). With
/// an opaque `bg` the colours equal `RgbaImage::flatten_rgb` exactly.
fn over_background(mut img: RgbaImage, bg: [f64; 4]) -> RgbaImage {
    let [br, bgc, bb, ba] = bg.map(|v| v.clamp(0.0, 1.0) as f32);
    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    for px in img.pixels.chunks_exact_mut(4) {
        let a = px[3] as f32 / 255.0;
        let src = [px[0], px[1], px[2]].map(|c| c as f32 / 255.0 * a);
        let (out, out_a) = if ba >= 1.0 {
            // Same arithmetic as `flatten_rgb`.
            let o = [
                src[0] + br * (1.0 - a),
                src[1] + bgc * (1.0 - a),
                src[2] + bb * (1.0 - a),
            ];
            (o, 1.0)
        } else {
            let out_a = a + ba * (1.0 - a);
            if out_a <= 0.0 {
                ([0.0; 3], 0.0)
            } else {
                let k = ba * (1.0 - a);
                let o = [
                    (src[0] + br * k) / out_a,
                    (src[1] + bgc * k) / out_a,
                    (src[2] + bb * k) / out_a,
                ];
                (o, out_a)
            }
        };
        px[0] = q(out[0]);
        px[1] = q(out[1]);
        px[2] = q(out[2]);
        px[3] = q(out_a);
    }
    img
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_opaque_background_matches_flatten_rgb() {
        let mut img = RgbaImage::new(3, 1);
        img.pixels = vec![255, 0, 0, 255, 0, 255, 0, 128, 10, 20, 30, 0];
        let bg = [0.2, 0.4, 0.6, 1.0];
        let flat = img.flatten_rgb(bg);
        let out = over_background(img, bg);
        for i in 0..3 {
            assert_eq!(&out.pixels[i * 4..i * 4 + 3], &flat[i * 3..i * 3 + 3]);
            assert_eq!(out.pixels[i * 4 + 3], 255);
        }
    }

    #[test]
    fn over_transparent_background_keeps_source() {
        let mut img = RgbaImage::new(2, 1);
        img.pixels = vec![200, 100, 50, 128, 0, 0, 0, 0];
        let out = over_background(img.clone(), [1.0, 1.0, 1.0, 0.0]);
        assert_eq!(out.pixels, img.pixels);
    }

    #[test]
    fn mograph_and_fusion_rgba_have_canvas_size() {
        let engine = MographEngine::new();
        let inst = MographInstance::new(vance_mograph::builtins::centered_title());
        let m = render_mograph_rgba(&engine, &inst, 1.0, 30.0, 4.0);
        assert_eq!((m.width, m.height), (1920, 1080));
        assert_eq!(m.pixels.len(), 1920 * 1080 * 4);

        let c = vance_fusion::Compositor::new();
        let comp = vance_fusion::starter(101, 51);
        let f = render_fusion_rgba(&c, &comp, 0.0, &HashMap::new());
        assert_eq!((f.width, f.height), (101, 51), "no even crop for RGBA");
    }

    #[test]
    fn layer_key_from_clip_id_is_its_uuid() {
        let id = vance_timeline::ClipId::new();
        assert_eq!(LayerKey::from(id).0, id.as_uuid().as_u128());
    }
}
