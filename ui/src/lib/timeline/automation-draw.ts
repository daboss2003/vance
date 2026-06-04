/**
 * Phase 7.4 M5 — pure-ish canvas draw helpers for the automation overlay.
 *
 * Each helper takes a 2D context + geometry + the lane data and emits
 * draw commands without touching component state. Kept here rather than
 * inline so Timeline.svelte stays focused on interaction wiring.
 *
 * Color discipline (per the spike): only ≤2 new hues introduced —
 *   - cyan #06b6d4 for clip-gain
 *   - amber #f59e0b for track-gain
 * Fade wedges reuse a neutral black shadow; the L/B/H + L/E/G chips
 * reuse the existing M/S button visual language.
 */

import type { GainKeyframe, FadeCurveName } from '$lib/ipc';
import {
  evalKeyframeLaneDb,
  yForDb,
  interpLetter,
  fadeCurveLetter,
  dbToString
} from './automation-geom';

export const CLIP_GAIN_COLOR = '#06b6d4';
export const TRACK_GAIN_COLOR = '#f59e0b';
const FADE_WEDGE_FILL = 'rgba(0, 0, 0, 0.45)';
const BASELINE_COLOR = 'rgba(160, 160, 170, 0.22)';
const BASELINE_COLOR_REINFORCED = 'rgba(160, 160, 170, 0.40)';
const KF_OUTLINE = '#0a0c10';
const KF_GLOW_SELECTED = 'rgba(255, 255, 255, 0.95)';

/** 6×6 px keyframe symbol; shape encodes interp. `selected` adds a glow.
 *  Module-private — only [`drawGainLane`] calls this. Exposing it would
 *  invite callers to draw keyframes outside a lane, which would skip
 *  the lane's clip + alpha logic. */
function drawKeyframeSymbol(
  ctx: CanvasRenderingContext2D,
  cx: number,
  cy: number,
  interp: 'linear' | 'bezier' | 'hold' | undefined,
  color: string,
  selected: boolean,
  size = 6
) {
  ctx.save();
  ctx.lineWidth = 1;
  ctx.strokeStyle = KF_OUTLINE;
  ctx.fillStyle = color;
  ctx.beginPath();
  const half = size / 2;
  switch (interp ?? 'linear') {
    case 'linear':
      // Square (rotated 45° → diamond for visual distinction from clip handles).
      ctx.moveTo(cx, cy - half);
      ctx.lineTo(cx + half, cy);
      ctx.lineTo(cx, cy + half);
      ctx.lineTo(cx - half, cy);
      ctx.closePath();
      break;
    case 'bezier':
      ctx.arc(cx, cy, half, 0, Math.PI * 2);
      break;
    case 'hold':
      ctx.moveTo(cx - half, cy - half);
      ctx.lineTo(cx + half, cy);
      ctx.lineTo(cx - half, cy + half);
      ctx.closePath();
      break;
  }
  ctx.fill();
  ctx.stroke();
  if (selected) {
    ctx.strokeStyle = KF_GLOW_SELECTED;
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    ctx.arc(cx, cy, half + 2.5, 0, Math.PI * 2);
    ctx.stroke();
  }
  ctx.restore();
}

/** Dashed 0 dB baseline across the lane. */
export function drawZeroBaseline(
  ctx: CanvasRenderingContext2D,
  laneX: number,
  laneTop: number,
  laneWidth: number,
  laneHeight: number,
  reinforced = false
) {
  ctx.save();
  ctx.strokeStyle = reinforced ? BASELINE_COLOR_REINFORCED : BASELINE_COLOR;
  ctx.setLineDash([3, 3]);
  ctx.lineWidth = 1;
  const y = Math.round(yForDb(0, laneTop, laneHeight)) + 0.5;
  ctx.beginPath();
  ctx.moveTo(laneX, y);
  ctx.lineTo(laneX + laneWidth, y);
  ctx.stroke();
  ctx.restore();
}

/**
 * Render the gain lane polyline + keyframe symbols.
 *
 * - `xForTime` converts a clip-relative (or timeline-relative) time to
 *   a canvas X. Caller supplies it so we can paint both clip-scope and
 *   track-scope lanes through the same path.
 * - `timeDomain` is the visible-time window inside the lane: the polyline
 *   only samples in that range (cheap LOD).
 * - `selectedKfIndex` lets the caller highlight the selected keyframe.
 * - `alpha` lets the track lane render at reduced opacity over clips.
 */
export function drawGainLane(
  ctx: CanvasRenderingContext2D,
  lane: GainKeyframe[],
  laneX: number,
  laneTop: number,
  laneWidth: number,
  laneHeight: number,
  xForTime: (t: number) => number,
  timeDomain: { start: number; end: number },
  color: string,
  selectedKfIndex: number | null,
  alpha = 1
) {
  if (laneHeight <= 6) return;
  ctx.save();
  ctx.beginPath();
  ctx.rect(laneX, laneTop, laneWidth, laneHeight);
  ctx.clip();
  ctx.globalAlpha = alpha;
  // Sample the curve along the visible x range. At 2px resolution this
  // is ~half the canvas width; cheaper than per-keyframe sampling once
  // a lane gets dense.
  const xLeft = xForTime(timeDomain.start);
  const xRight = xForTime(timeDomain.end);
  if (xRight > xLeft) {
    ctx.strokeStyle = color;
    ctx.lineWidth = 1.5;
    ctx.beginPath();
    const startT = timeDomain.start;
    const endT = timeDomain.end;
    const samples = Math.max(2, Math.ceil((xRight - xLeft) / 2));
    let first = true;
    for (let i = 0; i <= samples; i++) {
      const t = startT + ((endT - startT) * i) / samples;
      const db = evalKeyframeLaneDb(lane, t);
      const x = xForTime(t);
      const y = yForDb(db, laneTop, laneHeight);
      if (first) {
        ctx.moveTo(x, y);
        first = false;
      } else {
        ctx.lineTo(x, y);
      }
    }
    ctx.stroke();
  }
  // Draw keyframe symbols last so they sit on top of the curve.
  for (let i = 0; i < lane.length; i++) {
    const k = lane[i];
    if (k.time_seconds < timeDomain.start - 0.5 || k.time_seconds > timeDomain.end + 0.5) continue;
    const x = xForTime(k.time_seconds);
    const y = yForDb(k.value, laneTop, laneHeight);
    drawKeyframeSymbol(ctx, x, y, k.interp, color, i === selectedKfIndex);
  }
  ctx.restore();
}

/**
 * Fade wedge — a translucent triangle from the clip top at the fade
 * edge to the inside corner. Optional small grip square at the apex
 * for grabbing.
 *
 * `side = 'in'` draws a wedge growing rightward from the clip's left
 * edge; `'out'` mirrors from the right edge.
 */
export function drawFade(
  ctx: CanvasRenderingContext2D,
  cx: number,
  cy: number,
  cw: number,
  ch: number,
  side: 'in' | 'out',
  fadePx: number,
  grip = true
) {
  if (fadePx <= 0.5) return;
  ctx.save();
  ctx.fillStyle = FADE_WEDGE_FILL;
  ctx.beginPath();
  if (side === 'in') {
    ctx.moveTo(cx, cy);
    ctx.lineTo(cx + fadePx, cy);
    ctx.lineTo(cx, cy + ch);
    ctx.closePath();
  } else {
    ctx.moveTo(cx + cw, cy);
    ctx.lineTo(cx + cw - fadePx, cy);
    ctx.lineTo(cx + cw, cy + ch);
    ctx.closePath();
  }
  ctx.fill();
  if (grip) {
    const gx = side === 'in' ? cx + fadePx : cx + cw - fadePx;
    const gy = cy;
    ctx.fillStyle = '#e6ebf5';
    ctx.strokeStyle = '#1a1d23';
    ctx.lineWidth = 1;
    ctx.beginPath();
    // 8×8 diamond centered just below the top edge.
    const r = 4;
    ctx.moveTo(gx, gy - 1);
    ctx.lineTo(gx + r, gy + r - 1);
    ctx.lineTo(gx, gy + 2 * r - 1);
    ctx.lineTo(gx - r, gy + r - 1);
    ctx.closePath();
    ctx.fill();
    ctx.stroke();
  }
  ctx.restore();
}

/**
 * Filled capsule chip (L / E / G letter inside) — used by M5 to label
 * a clip's active fade curve. Mirrors the M/S button's visual language
 * (same roundRect + 10 px semibold), filled in `accent` with dark
 * text on top.
 */
export function drawChip(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  letter: string,
  accent: string
) {
  ctx.save();
  ctx.fillStyle = accent;
  ctx.strokeStyle = accent;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.roundRect(x + 0.5, y + 0.5, w - 1, h - 1, 3);
  ctx.fill();
  ctx.stroke();
  ctx.fillStyle = '#0d0f12';
  ctx.textAlign = 'center';
  ctx.textBaseline = 'middle';
  ctx.font = 'bold 9px ui-sans-serif, system-ui, sans-serif';
  ctx.fillText(letter, x + w / 2, y + h / 2 + 0.5);
  ctx.textAlign = 'left';
  ctx.restore();
}

/** Floating drag-readout label near the cursor. */
export function drawDragTag(
  ctx: CanvasRenderingContext2D,
  cursorX: number,
  cursorY: number,
  text: string
) {
  ctx.save();
  ctx.font = '11px ui-monospace, SFMono-Regular, Menlo, monospace';
  ctx.textBaseline = 'top';
  const padding = 4;
  const w = ctx.measureText(text).width + padding * 2;
  const h = 16;
  const x = cursorX + 12;
  const y = cursorY - 18;
  ctx.fillStyle = 'rgba(20, 22, 28, 0.92)';
  ctx.strokeStyle = 'rgba(255, 255, 255, 0.18)';
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.roundRect(x + 0.5, y + 0.5, w - 1, h - 1, 3);
  ctx.fill();
  ctx.stroke();
  ctx.fillStyle = '#e6ebf5';
  ctx.fillText(text, x + padding, y + 3);
  ctx.restore();
}

/** Helper to produce the drag tag text given a context. */
export function fadeDragTag(side: 'in' | 'out', durationSec: number, curve: FadeCurveName): string {
  return `fade-${side}  ${durationSec.toFixed(2)}s · ${fadeCurveLetter(curve)}`;
}

export function keyframeDragTag(
  timeSec: number,
  db: number,
  interp: GainKeyframe['interp']
): string {
  return `${timeSec.toFixed(2)}s  ${dbToString(db)} · ${interpLetter(interp)}`;
}
