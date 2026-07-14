# Titles, Motion Graphics & Fusion

Vance has three ways to create synthetic visuals: titles, motion-graphics
templates, and the Fusion node compositor. All three are file-less media — you
create them from a panel, and they land in the Browser to drag onto the
timeline. They animate as the playhead crosses them and export identically.

## Titles

Simple text cards with size, color, and background — quick lower-thirds and
slates.

## Motion Graphics Templates (`.cmgt`)

Parameterized, animated title/graphic templates. Pick a built-in template,
then edit its exposed controls (text, color, sliders, fonts, draggable
points). Properties are constants, keyframe lanes, or expressions, so a
template both animates and exposes knobs.

## Fusion — the node compositor (`.vfc`)

A node graph of image operations, evaluated on the CPU. Every node outputs a
full-canvas image, so wiring is always compatible.

**Node types**

- *Sources:* Background (solid/gradient), Text, Rectangle, and **Media In**
  (a host image, e.g. a clip frame).
- *Filters:* Transform (move/scale/rotate/opacity), Blur (box/gaussian),
  Color Correct (lift/gamma/gain + saturation), Crop.
- *Combiners:* Merge (Over / Add / Multiply / Screen + opacity), Mask
  (alpha or luminance, invertible).

**Using the editor**

1. Create a composition (set the canvas size and duration).
2. Add nodes and wire each node's input slots to other nodes.
3. Edit each node's parameters inline.
4. Choose the **Output** node — that's the rendered frame.

Scalar parameters can be constants, keyframes, or expressions, so a comp
animates over its duration. Deeply nested or very large graphs are evaluated
safely (no stack overflow) with bounded memory.
