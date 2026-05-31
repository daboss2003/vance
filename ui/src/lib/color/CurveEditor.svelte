<script lang="ts">
  // A tone-curve editor: a unit-square pad where the user drags control
  // points to reshape one channel's input→output mapping. Straight
  // (linear) segments connect the points — this is WYSIWYG with the
  // engine, which bakes the same control points with `Linear` interp
  // into the LUT atlas. Smooth/Bezier tone curves are a 6.8b follow-up
  // (they'd need tangent handles and would make the identity 2-point
  // curve render as an S, so they're opt-in, not the default).
  //
  // Interaction: drag a point to move it; click empty space to add a
  // point there (and start dragging it); double-click an interior point
  // to delete it. The two endpoints (input 0 and input 1) are pinned in
  // x — only their output (y) moves. Drag emits `onInput` live
  // (non-undoable preview); pointer-up emits `onCommit` (one undo step).

  import { untrack } from 'svelte';

  type Pt = { x: number; y: number };

  let {
    label,
    points,
    color = '#e6ebf5',
    identityY = null,
    onInput,
    onCommit
  }: {
    label: string;
    /** Control points in [0,1]×[0,1], sorted by x. Empty = identity. */
    points: Pt[];
    color?: string;
    /** Identity baseline for multiplier-style curves (6.8b hs/hl use
     *  `0.5` so the empty-curve display is a flat no-op line, not the
     *  diagonal). Null/undefined keeps the classic y=x baseline. */
    identityY?: number | null;
    onInput: (points: Pt[]) => void;
    onCommit: (points: Pt[]) => void;
  } = $props();

  const SIZE = 200;
  const HIT_PX = 9; // click-to-grab radius

  // An empty/short list means "identity"; default baseline is y=x, but
  // multiplier curves pass identityY to get a flat horizontal baseline.
  function normalize(p: Pt[]): Pt[] {
    if (p.length >= 2) {
      return p.map((q) => ({ x: q.x, y: q.y })).sort((a, b) => a.x - b.x);
    }
    if (identityY !== null && identityY !== undefined) {
      return [
        { x: 0, y: identityY },
        { x: 1, y: identityY }
      ];
    }
    return [
      { x: 0, y: 0 },
      { x: 1, y: 1 }
    ];
  }

  let pts = $state<Pt[]>([]);
  let dragIdx = $state<number | null>(null);
  // Content key of the props we last synced from. A live preview does
  // NOT mutate the `points` prop (it's pushed straight to the renderer),
  // so during a drag this key stays stable and we leave the in-flight
  // edit alone. Any real CONTENT change (channel switch, node switch,
  // undo, keyframe nav) flips the key — we then resync AND cancel the
  // drag, so a pending pointer-up can't commit this curve's points onto
  // the newly-selected channel. `pre` so the initial seed lands before
  // the first paint (no identity flash).
  let syncedKey = '';
  $effect.pre(() => {
    // Include identityY in the key so switching from a multiplier
    // channel (identityY=0.5) to a remap channel (identityY=null) on a
    // STILL-EMPTY curve re-normalizes the baseline (else the editor
    // keeps showing the old horizontal line instead of the diagonal).
    const key = JSON.stringify(points) + '|' + String(identityY);
    untrack(() => {
      if (key !== syncedKey) {
        syncedKey = key;
        dragIdx = null;
        pts = normalize(points);
      }
    });
  });

  const clamp01 = (v: number) => (v < 0 ? 0 : v > 1 ? 1 : v);
  const toPx = (p: Pt) => ({ px: p.x * SIZE, py: (1 - p.y) * SIZE });

  function evToData(ev: PointerEvent, el: SVGSVGElement): Pt {
    const rect = el.getBoundingClientRect();
    return {
      x: clamp01((ev.clientX - rect.left) / rect.width),
      y: clamp01(1 - (ev.clientY - rect.top) / rect.height)
    };
  }

  // Index of an existing point within HIT_PX of the event, else -1.
  function hitTest(ev: MouseEvent, el: SVGSVGElement): number {
    const rect = el.getBoundingClientRect();
    const mx = ev.clientX - rect.left;
    const my = ev.clientY - rect.top;
    let best = -1;
    let bestD = HIT_PX;
    for (let i = 0; i < pts.length; i++) {
      const { px, py } = toPx(pts[i]);
      const d = Math.hypot(px - mx, py - my);
      if (d <= bestD) {
        bestD = d;
        best = i;
      }
    }
    return best;
  }

  // Move the dragged point, keeping the array sorted and endpoints
  // x-pinned. Interior points clamp between their neighbors (with a
  // tiny epsilon) so two points never collide onto the same x.
  function moveDragged(d: Pt) {
    if (dragIdx === null) return;
    const i = dragIdx;
    const isFirst = i === 0;
    const isLast = i === pts.length - 1;
    let x = d.x;
    if (isFirst) x = 0;
    else if (isLast) x = 1;
    else {
      const lo = pts[i - 1].x + 1e-4;
      const hi = pts[i + 1].x - 1e-4;
      x = Math.min(Math.max(d.x, lo), hi);
    }
    pts[i] = { x, y: d.y };
  }

  function onDown(ev: PointerEvent) {
    const el = ev.currentTarget as SVGSVGElement;
    el.setPointerCapture(ev.pointerId);
    const hit = hitTest(ev, el);
    if (hit >= 0) {
      dragIdx = hit;
    } else {
      // Insert a new point at the click, then drag it.
      const d = evToData(ev, el);
      const next = [...pts, d].sort((a, b) => a.x - b.x);
      pts = next;
      dragIdx = next.findIndex((p) => p === d);
    }
    onInput(pts.map((p) => ({ ...p })));
  }

  function onMove(ev: PointerEvent) {
    if (dragIdx === null) return;
    const el = ev.currentTarget as SVGSVGElement;
    moveDragged(evToData(ev, el));
    onInput(pts.map((p) => ({ ...p })));
  }

  function onUp() {
    if (dragIdx === null) return;
    dragIdx = null;
    onCommit(pts.map((p) => ({ ...p })));
  }

  // Gesture cancelled (context menu, capture loss): commit what we have
  // so the live preview isn't left uncommitted, and disarm.
  function onCancel() {
    if (dragIdx === null) return;
    dragIdx = null;
    onCommit(pts.map((p) => ({ ...p })));
  }

  // Double-click an interior point to delete it. Endpoints can't be
  // deleted (the curve must span input 0..1).
  function onDblClick(ev: MouseEvent) {
    const el = ev.currentTarget as SVGSVGElement;
    const hit = hitTest(ev, el);
    if (hit > 0 && hit < pts.length - 1) {
      pts = pts.filter((_, i) => i !== hit);
      dragIdx = null;
      onCommit(pts.map((p) => ({ ...p })));
    }
  }

  // Reset to identity (clears all interior points). Bound to the button.
  function reset() {
    // Reset to the per-channel identity baseline so a multiplier-curve
    // reset doesn't accidentally encode a diagonal as the new identity.
    const y0 = identityY ?? 0;
    const y1 = identityY ?? 1;
    pts = [
      { x: 0, y: y0 },
      { x: 1, y: y1 }
    ];
    dragIdx = null;
    onCommit(pts.map((p) => ({ ...p })));
  }

  let polyline = $derived(
    pts
      .map((p) => {
        const { px, py } = toPx(p);
        return `${px.toFixed(1)},${py.toFixed(1)}`;
      })
      .join(' ')
  );
</script>

<div class="flex flex-col gap-1">
  <div class="flex items-center justify-between">
    <span class="text-vance-muted text-[10px] uppercase tracking-wide">{label}</span>
    <button
      class="text-vance-muted text-[10px] hover:brightness-150"
      onclick={reset}
      title="Reset this channel to identity"
    >
      reset
    </button>
  </div>
  <svg
    width={SIZE}
    height={SIZE}
    viewBox={`0 0 ${SIZE} ${SIZE}`}
    class="cursor-crosshair touch-none select-none rounded border border-vance-border bg-[#15181d]"
    role="application"
    aria-label={`${label} tone curve, ${pts.length} points`}
    onpointerdown={onDown}
    onpointermove={onMove}
    onpointerup={onUp}
    onpointercancel={onCancel}
    onlostpointercapture={onCancel}
    ondblclick={onDblClick}
  >
    <!-- Quarter grid -->
    {#each [0.25, 0.5, 0.75] as g (g)}
      <line x1={g * SIZE} y1="0" x2={g * SIZE} y2={SIZE} stroke="#22262e" stroke-width="1" />
      <line x1="0" y1={g * SIZE} x2={SIZE} y2={g * SIZE} stroke="#22262e" stroke-width="1" />
    {/each}
    <!-- Identity reference diagonal -->
    <line x1="0" y1={SIZE} x2={SIZE} y2="0" stroke="#2f3540" stroke-width="1" stroke-dasharray="3 3" />
    <!-- The curve (straight segments = Linear interp, WYSIWYG) -->
    <polyline points={polyline} fill="none" stroke={color} stroke-width="1.5" />
    <!-- Control points -->
    {#each pts as p, i (i)}
      {@const c = toPx(p)}
      <circle
        cx={c.px}
        cy={c.py}
        r={i === dragIdx ? 5 : 4}
        fill={i === dragIdx ? color : '#15181d'}
        stroke={color}
        stroke-width="1.5"
      />
    {/each}
  </svg>
  <span class="font-mono text-[9px] text-vance-muted">
    {pts.length} pts · drag to edit · click to add · dbl-click to remove
  </span>
</div>
