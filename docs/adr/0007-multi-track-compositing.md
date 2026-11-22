# ADR 0007 — Multi-Track Compositing

- **Status:** Accepted (implemented 2026-11-22; see "Follow-up decisions" for what changed since)
- **Date:** 2026-11-20
- **Deciders:** Vance core team
- **Builds on:** ADR 0006 (shared-memory frame transport, which this step needs for several layers)

## Context

Vance has no multi-track video compositing.

- `vance-timeline/src/scene_plan.rs` resolves `SceneAt::{Empty, Clip}`: the topmost enabled
  video track with a clip at time t wins, and nothing below it is drawn.
- `vance-render/src/shader.wgsl` writes `vec4(rgb * opacity * keep, 1.0)`, so alpha is always
  1. Opacity, chroma key and luma key fade the picture to black instead of revealing lower
  tracks. There are no blend modes.
- Titles, MoGraph and Fusion render opaque frames, and Fusion's `MediaIn` gets an empty input map.
- There is no command or UI to add, remove or reorder video tracks; new projects hard-code
  V1 + A1. The UI draws tracks top-down while `scene_at` treats the last video track as the top.
- Live preview and export agree only by convention: the same uniforms are set in about six
  hand-copied blocks in `vance-app/src/engine.rs`. Playback resolves one clip at a time and
  restarts at every cut.

Everything below one layer is therefore a gap: blend modes, meaningful opacity, keying over
footage, titles over video, picture-in-picture, Fusion on footage, background replace.

## Decision

### One frame function, three frame providers

Every consumer renders through `compose_frame(compositor, &ScenePlan, &mut dyn FrameProvider,
&mut ComposeCtx, &OutputSpec)`. Providers differ only in how they get decoded frames:

- `ScrubProvider`: pooled `decode_at` plus the frame cache, fanned out across layers.
- `StreamProvider { Realtime }`: playback, using per-clip streaming feeds that may drop or hold
  frames.
- `StreamProvider { Blocking }`: export, using the same feeds but never dropping.

All three share one frame-selection rule (first frame with pts ≥ target; hold the last at EOF)
and one rounding function. The only intended preview/export differences are the view LUT and
viewer solos (preview only), the output space/format, and the canvas size.

### GPU passes

1. **Layer pass**, per visible clip, into a reusable `Rgba16Float` slot keyed by clip:
   - transform and crop;
   - **fit the source into the canvas without distortion**, with transparent bars;
   - decode YUV or straight-alpha RGBA, then keyer, clip LUT, linearise, input→working, grades,
     brightness/contrast;
   - range fit (SDR clamp or tonemap / HDR `max(0)`);
   - legacy chroma/luma keys, now feeding alpha;
   - output premultiplied.
2. **Composite pass**, W3C separable formulas on premultiplied values, with the clip's opacity
   applied here:
   - **Normal, Add, Multiply, Screen, Overlay, Darken, Lighten, Difference, Soft Light, Hard
     Light, Color Dodge, Color Burn**;
   - a dissolve on one track is `lerp(blend(A over backdrop), blend(B over backdrop), mix)`.
3. **Output pass**: working→output transform, transfer encode, then the view LUT (preview only).

**Blend space (user decision): display-referred, like Premiere / Resolve / Photoshop, not linear
light.** Layers are converted to the output's encoded space before compositing, for SDR output.
HDR output composites in linear working space, because values above 1.0 must survive (marked
`Intentional:`). The accumulators are two ping-pong `Rgba16Float` textures, which is about
130 MB of VRAM at 1080p.

### Other pieces

- **CPU add-ons, per layer.** A layer is read back to straight sRGB RGBA8, run through the
  existing WASM/shader plugin chain, then written back. Plugin state moves into an `AddonHost`
  behind `Arc<Mutex<_>>` so the playback thread can run add-ons too.
- **Transitions, per track.** Both sides become full layers with their own effects and grades,
  which fixes today's "A side only" behaviour. A WASM transition renders both, reads them back,
  calls the plugin and writes the result into A's slot.
- **Titles, MoGraph, Fusion** render RGBA including the background's alpha. `TitleParams` gains
  `bg_alpha` (default 1.0, so old titles look the same; new titles default to transparent).
- **Fusion `MediaIn` resolves to named media items only (user decision).** There is no "tracks
  below" input in this step.
- **Keyer**, a new `keyer` effect that works on the encoded source:
  - primary-channel colour difference with balance and strength;
  - spill suppression and clip black/white;
  - choke and soften by ring taps in source pixels, so preview matches export;
  - invert and matte view.
- **Sequence frame size (user decision).** Each sequence gets an additive `frame_size` field.
  It takes its value from the first clip added, falling back to 1920×1080. Export defaults to it.
- **Decoders.** A `DecoderPool` (LRU, about 6 decoders, keyed by path) replaces the single
  playback and transition decoders. It spawns clients with `ClientOptions { threads:
  Some(thread_budget(active_layers)), .. }` (ADR 0006 follow-up) so decode and conversion
  threads share the cores.
- **Memory budget (user decision, ADR 0006).** Total shared-memory frame memory is capped at
  about 25% of RAM, using `TransportStats.shm_bytes`. Layers past the cap fall back to proxy
  media, with a notice.
- **Playback.** A timeline-driven loop with per-clip streaming feeds and an audio-master clock.
  There is no clip chaining: the mixer runs straight through cuts, and gaps play as black.
- **Tracks.** A lower index is a lower layer, and ops keep video tracks before audio. The UI
  draws video rows reversed, so the top row is the top layer. `Track.enabled` is the visibility
  toggle.

## Contracts to freeze before implementation (main thread)

- `vance-timeline`:
  - `BlendMode` (12 variants, frozen order; serde as a string that also loads from a RON
    identifier; unknown values load as `Normal`).
  - `Clip.blend_mode` and `TitleParams.bg_alpha`, both `#[serde(default)]`.
  - `Sequence.frame_size: Option<(u32, u32)>`.
  - Keyer parameter keys.
  - `ScenePlan { layers }`, where each layer is a `LayerClip` or a `LayerTransition`, and
    `scene_plan_at(seq, t)`.
  - Ops and `EditOp`s: `InsertTrack`, `MoveTrack`, `SetTrackEnabled`, `SetClipBlendMode`,
    `SetFrameSize`.
- `vance-render`: `Compositor::{begin_frame, render_layer, read_layer_rgba8,
  write_layer_rgba8, composite, composite_dissolve, finish_to_view, finish_to_rgba8,
  finish_to_rgba64, retain_only}`, plus `LayerSource`, `LayerParams`, `KeyerParams`,
  `CompositeOp` and `OutputSpec`.
- `vance-app`: `compose_frame`, `FrameProvider`, `ComposeCtx`, `layer_params_for`,
  `output_spec_for`, `DecoderPool`, `ScrubProvider` and `StreamProvider`.
  - Engine methods: `add_track`, `remove_track`, `move_track`, `set_track_enabled`,
    `set_clip_blend_mode`, `set_matte_view`, `preview_frame_rgba` and `export_frame_rgba`.
  - The matching Tauri commands and TypeScript types.

## Work split (parallel seams by file)

Step 0 is serial, done by the main thread or one agent: contract stubs, module declarations,
command registration, IPC TypeScript types, and empty test modules.

Then, in parallel:

| Seam | Files |
|---|---|
| T, timeline | scene plan, blend, keyer, clip, title, ops, edit_log |
| R, render | renderer, the new shader stages and compositor, title/mograph/fusion RGBA |
| S, sources | decoder pool, layer feeds, frame selection |
| C, compose | compose.rs, the add-on host |
| U, UI | Timeline, track menu, composite and keyer inspectors, frame-size setting |
| X, end-to-end tests | the tests listed below |

**E, integration**, is the sole owner of `engine.rs`: rewiring scrub, playback, export, scopes
and grade preview, and deleting the hand-copied paths.

## Tests

- **Scene plan:** layer order, gaps, transitions per track, and the legacy `scene_at` kept.
- **Serde:** defaults and unknown blend modes.
- **GPU:**
  - one layer matches the legacy path within ±1 (8-bit, 10-bit, PQ);
  - 50% white over black;
  - picture-in-picture and crop reveal the layer below;
  - all 12 blend modes against a CPU reference;
  - dissolve with mixed blend modes;
  - RGBA readback/writeback round trip;
  - keyer cases.
- **`lockstep`:** preview equals export byte for byte on a 4-track project (dissolve, keyed
  Screen picture-in-picture, title at 0.7 opacity, Fusion).
- **HDR:** an SDR title over PQ footage.
- **Performance:** 4 × 1080p layers in under 8 ms of GPU time.

## Risks

- **Several 4K layers.** Bandwidth is now addressed by ADR 0006 (about 18–26 ms per 4K ProRes
  frame through shared memory), but layers still multiply the decode cost. Proxies and the
  memory cap are the valves.
- **Rewriting playback** risks regressions in A/V sync and end of stream. The soak and
  real-media tests help.
- **Rgba16Float precision** against the existing golden-test tolerances.
- **Lock ordering:** render state → add-on host / LUT cache, never the reverse. Frames are
  fetched before the render lock is taken (`plan_requests` + `Prefetched`), so a decode wait
  never holds the renderer.

## Follow-up decisions (2026-11-22)

Made during implementation and after the post-build bug hunt:

- **Tracks.** Automatic names (`^[VA]\d+$`) follow position and renumber on every structural
  edit; Rename keeps a custom name. A locked track can't be deleted until it's unlocked, and the
  track header has a lock toggle. Track names refuse control, separator and invisible formatting
  characters, except the joiner between two emoji and the tag characters of a flag sequence. A lock freezes the track's contents and stored clip and mixer
  parameters; mute, solo, visibility, order and name stay editable.
- **Frame size.** Opening a project fills in a missing `frame_size` from the sequence's earliest
  video clip (display size, so anamorphic footage gets its true aspect). After that, a clip sets
  the size only when the sequence has no video clips yet. New titles are created at the sequence
  size.
- **Export renders from full-resolution originals.** Proxies, including the memory-cap swap, are
  preview-only. Proxies are made at half the display size (square pixels), and the keyer's
  pixel-based settings are scaled to match the original. Proxies of HDR (PQ/HLG) footage are
  10-bit ProRes 422 Proxy tagged PQ/HLG BT.2020: intra-frame, built into FFmpeg, fast to scrub.
- **Render-and-replace** renders the track alone over a transparent backdrop into ProRes 4444.
  Opacity and opacity keyframes are baked into the alpha (W3C opacity only scales source alpha,
  so this is exact for every blend mode). The new clip keeps the blend mode the replaced clips
  share. It refuses, before rendering anything, whenever the result wouldn't look the same:
  - mixed blend modes in the range;
  - a range edge inside a dissolve;
  - a dissolve under a blend mode that isn't linear in the source (Darken, Lighten, Difference,
    Soft Light, Hard Light, Color Dodge, Color Burn);
  - HDR (PQ/HLG) footage, until a 16-bit alpha path exists (media/export milestone);
  - a locked or hidden track.
- **Keyer.** Clip White defaults to 0.9, so near-kept colours such as olive skin stay opaque.
  There is no spill suppression when the key is inverted or the key colour is neutral. Choke and
  soften are measured in display pixels (anamorphic sources included) and scaled for proxies.
- **HDR blend modes** clamp only where the formula breaks above 1.0. Screen uses the W3C formula
  when both values are within [0, 1] and `max(backdrop, source)` otherwise. The rule is
  continuous at 1.0, and neither a black nor a white Screen layer dims highlights. Overlay, Soft Light, Hard Light, Color Dodge and Color Burn
  clamp. SDR results are unchanged.
- **Keyer choke and soften** run as compute line passes. Choke is a separable octagon (horizontal,
  vertical, then two diagonal min/max passes) using a doubling table, so its cost doesn't grow with
  the radius and thin gaps erode at any radius up to the 64 px cap. Soften uses exact taps up to
  8 px and three variance-matched box passes above (within 4 codes of the exact kernel). Both are
  measured in the source's own display pixels.
- **Transparent output** (render-and-replace): `OutputSpec.transparent` composites over a
  transparent backdrop, and `finish_to_rgba8/64` return straight alpha. Colour is zeroed wherever
  alpha rounds to 0 at the target bit depth. Preview always flattens.
- **HDR add-on readback** (`read_layer_rgba8` / `write_layer_rgba8`) converts working → Rec.709
  before the sRGB encode and back again, so Fusion `MediaIn` footage keeps its colours in a wide
  working space. The output primaries come from `OutputSpec.output_primaries`, not a guess from
  the transfer.
- **Stream encoding hints.** The decoder reports each stream's YUV range, matrix and sample
  aspect ratio (protocol v6, ADR 0006), and every frame carries them (`FrameData.color`, also in
  the frame cache). Unsignalled YUV is limited range, including PQ and HLG. The unsignalled
  matrix is BT.601 at 576 lines or fewer, otherwise it follows the primaries (BT.2020 or
  BT.709). Sources with alpha arrive as straight-alpha RGBA8.
- **Fonts.** The MoGraph and Fusion renderers, and their font databases, are built on first use.

## Consequences

The look of existing projects changes in several deliberate ways:
- opacity and dissolves mix in display space as before, but now reveal lower tracks instead of
  black;
- cropped and out-of-frame areas become transparent;
- gaps render black;
- mismatched aspect ratios are fitted, not stretched.

The new fields (`Clip.blend_mode`, `TitleParams.bg_alpha`, `Sequence.frame_size`) are additive;
older builds drop them on re-save.
