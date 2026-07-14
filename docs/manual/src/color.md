# Color

Switch to the **Color** page to grade. Vance uses a managed color pipeline so
what you see matches what you export.

## The grading graph

Grading is **node-based**: each node is a stage (primaries, curves,
qualifier/secondary, LUT, …) and signal flows through the graph. Add, reorder,
and bypass nodes; a node can key on a qualifier (hue/sat/luma) to grade only
part of the image.

## Scopes

Waveform, parade, vectorscope, and histogram update live so you can grade to
the signal, not just the preview.

## Color management

Vance understands input color spaces and a working/output space:

- **Primaries:** Rec.709, Display P3, Rec.2020, ACES AP1.
- **Transfer:** Rec.709, sRGB, Linear, PQ, HLG, ACEScct.

Tag each clip's input color space (camera-dependent) so the pipeline converts
correctly. LUTs (`.cube`) are parsed once and cached.

> **Note on HDR:** Vance's color science supports Rec.2020 PQ/HLG. Full HDR10
> mastering (10-bit output with mastering-display metadata) depends on a
> 10-bit render path and is on the roadmap; today the pipeline is graded and
> previewed in the working space.

## Expressions & keyframes

Parameters can be **keyframed** or driven by **expressions** (functions of
time), so a grade can animate across a shot.
