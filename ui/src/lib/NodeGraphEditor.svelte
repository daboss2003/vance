<script lang="ts">
  /**
   * Node-graph editor (M6 Phase 2) — an n8n-style canvas. Pan (drag empty
   * space), zoom (wheel), and wire nodes by DRAGGING from an output ● to an
   * input ○. Multiple effect nodes compose into one; the graph compiles to a
   * single WGSL `effect(color, uv)` live. Model/compiler are unchanged — this
   * is purely the interaction surface.
   */
  import {
    graph,
    kindDef,
    addNode,
    moveNode,
    deleteNode,
    deleteEdge,
    connect,
    setParam,
    ensureSeeded,
    type UINode
  } from '$lib/nodegraph.svelte';

  const NODE_W = 176;
  const HEAD_H = 30;
  const ROW_H = 22;

  ensureSeeded();

  // ---- view (pan/zoom) ------------------------------------------------------
  let vpEl = $state<HTMLDivElement | null>(null);
  let view = $state({ x: 28, y: 28, k: 1 });
  let selected = $state<string | null>(null);

  type Gesture =
    | { kind: 'pan'; sx: number; sy: number; ox: number; oy: number }
    | { kind: 'node'; id: string; dx: number; dy: number }
    | { kind: 'wire'; from: string; gx: number; gy: number };
  let gesture = $state<Gesture | null>(null);
  const wiring = $derived(gesture?.kind === 'wire');

  function screenToGraph(clientX: number, clientY: number): { x: number; y: number } {
    const r = vpEl!.getBoundingClientRect();
    return { x: (clientX - r.left - view.x) / view.k, y: (clientY - r.top - view.y) / view.k };
  }

  // ---- port geometry (graph coords) ----------------------------------------
  function nodeById(id: string): UINode | undefined {
    return graph.nodes.find((m) => m.id === id);
  }
  function inputs(n: UINode): string[] {
    return kindDef(n.kind)?.inputs ?? [];
  }
  function outPos(n: UINode): { x: number; y: number } {
    return { x: n.x + NODE_W, y: n.y + HEAD_H / 2 };
  }
  function inPos(n: UINode, input: string): { x: number; y: number } {
    const list = inputs(n);
    const i = Math.max(0, list.indexOf(input));
    const c = list.length;
    return { x: n.x, y: n.y + HEAD_H / 2 + (i - (c - 1) / 2) * ROW_H };
  }
  function inTop(n: UINode, i: number, c: number): number {
    return HEAD_H / 2 + (i - (c - 1) / 2) * ROW_H - 6;
  }

  // Nearest input port to a graph point (for wire drop). ~18px screen radius.
  function nearestInput(gx: number, gy: number): { node: string; input: string } | null {
    let best: { node: string; input: string } | null = null;
    let bestD = Infinity;
    for (const n of graph.nodes) {
      for (const input of inputs(n)) {
        const p = inPos(n, input);
        const d = Math.hypot(p.x - gx, p.y - gy);
        if (d < bestD) {
          bestD = d;
          best = { node: n.id, input };
        }
      }
    }
    return best && bestD <= 18 / view.k ? best : null;
  }

  // ---- pointer gestures -----------------------------------------------------
  function capture(e: PointerEvent) {
    vpEl?.setPointerCapture(e.pointerId);
  }
  function onCanvasDown(e: PointerEvent) {
    selected = null;
    gesture = { kind: 'pan', sx: e.clientX, sy: e.clientY, ox: view.x, oy: view.y };
    capture(e);
  }
  function onHeadDown(e: PointerEvent, id: string) {
    e.stopPropagation();
    selected = id;
    const g = screenToGraph(e.clientX, e.clientY);
    const n = nodeById(id)!;
    gesture = { kind: 'node', id, dx: g.x - n.x, dy: g.y - n.y };
    capture(e);
  }
  function onOutDown(e: PointerEvent, id: string) {
    e.stopPropagation();
    const g = screenToGraph(e.clientX, e.clientY);
    graph.pendingFrom = id;
    gesture = { kind: 'wire', from: id, gx: g.x, gy: g.y };
    capture(e);
  }
  function onMove(e: PointerEvent) {
    if (!gesture) return;
    if (gesture.kind === 'pan') {
      view = { ...view, x: gesture.ox + (e.clientX - gesture.sx), y: gesture.oy + (e.clientY - gesture.sy) };
    } else if (gesture.kind === 'node') {
      const g = screenToGraph(e.clientX, e.clientY);
      moveNode(gesture.id, Math.round(g.x - gesture.dx), Math.round(g.y - gesture.dy));
    } else {
      const g = screenToGraph(e.clientX, e.clientY);
      gesture = { ...gesture, gx: g.x, gy: g.y };
    }
  }
  function onUp(e: PointerEvent) {
    if (gesture?.kind === 'wire') {
      const g = screenToGraph(e.clientX, e.clientY);
      const hit = nearestInput(g.x, g.y);
      if (hit) connect(gesture.from, hit.node, hit.input);
      graph.pendingFrom = null;
    }
    gesture = null;
  }
  function onKey(e: KeyboardEvent) {
    if ((e.key === 'Delete' || e.key === 'Backspace') && selected) {
      const n = nodeById(selected);
      if (n && n.kind !== 'input' && n.kind !== 'output') {
        deleteNode(selected);
        selected = null;
      }
    }
  }

  // ---- zoom controls --------------------------------------------------------
  function zoomAround(cx: number, cy: number, factor: number) {
    const gx = (cx - view.x) / view.k;
    const gy = (cy - view.y) / view.k;
    const k = Math.min(2.5, Math.max(0.3, view.k * factor));
    view = { k, x: cx - gx * k, y: cy - gy * k };
  }
  function onWheel(e: WheelEvent) {
    e.preventDefault();
    const r = vpEl!.getBoundingClientRect();
    zoomAround(e.clientX - r.left, e.clientY - r.top, e.deltaY < 0 ? 1.1 : 1 / 1.1);
  }
  function zoomBtn(factor: number) {
    const r = vpEl!.getBoundingClientRect();
    zoomAround(r.width / 2, r.height / 2, factor);
  }
  function resetView() {
    view = { x: 28, y: 28, k: 1 };
  }
  function fitView() {
    if (!graph.nodes.length || !vpEl) return;
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const n of graph.nodes) {
      minX = Math.min(minX, n.x);
      minY = Math.min(minY, n.y);
      maxX = Math.max(maxX, n.x + NODE_W);
      maxY = Math.max(maxY, n.y + 96);
    }
    const r = vpEl.getBoundingClientRect();
    const pad = 40;
    const k = Math.min(1.6, Math.max(0.3, Math.min((r.width - pad * 2) / (maxX - minX), (r.height - pad * 2) / (maxY - minY))));
    view = {
      k,
      x: (r.width - (maxX - minX) * k) / 2 - minX * k,
      y: (r.height - (maxY - minY) * k) / 2 - minY * k
    };
  }

  // ---- add-node placement (center of the current view) ----------------------
  function addAt(kind: string) {
    const r = vpEl?.getBoundingClientRect();
    if (!r) return addNode(kind);
    const c = screenToGraph(r.left + r.width / 2, r.top + r.height / 2);
    addNode(kind, Math.round(c.x - NODE_W / 2), Math.round(c.y - HEAD_H));
  }
  function canAdd(kind: string): boolean {
    if (kind === 'output') return !graph.nodes.some((n) => n.kind === 'output');
    return true;
  }

  // ---- cosmetics ------------------------------------------------------------
  const CAT: Record<string, string> = {
    input: '#5fbf8f',
    output: '#4c9dff',
    brightness: '#f2c14e',
    contrast: '#f2c14e',
    gamma: '#f2c14e',
    saturation: '#b98cff',
    invert: '#b98cff',
    tint: '#b98cff',
    mix: '#4ec7d0'
  };
  const cat = (k: string) => CAT[k] ?? '#8a93a3';

  function bezier(a: { x: number; y: number }, b: { x: number; y: number }): string {
    const dx = Math.max(40, Math.abs(b.x - a.x) * 0.5);
    return `M ${a.x} ${a.y} C ${a.x + dx} ${a.y}, ${b.x - dx} ${b.y}, ${b.x} ${b.y}`;
  }
</script>

<div class="editor">
  <div class="toolbar">
    <div class="add">
      {#each graph.palette as k (k.key)}
        <button
          class="chip"
          disabled={!canAdd(k.key)}
          style={`--c:${cat(k.key)}`}
          onclick={() => addAt(k.key)}
          title={`Add ${k.label}`}
        >
          <span class="dot" style={`background:${cat(k.key)}`}></span>{k.label}
        </button>
      {/each}
    </div>
    <div class="zoom">
      <button onclick={() => zoomBtn(1 / 1.2)} title="Zoom out" aria-label="Zoom out">−</button>
      <span class="pct">{Math.round(view.k * 100)}%</span>
      <button onclick={() => zoomBtn(1.2)} title="Zoom in" aria-label="Zoom in">+</button>
      <button onclick={fitView} title="Fit graph" aria-label="Fit">⤢</button>
      <button onclick={resetView} title="Reset view" aria-label="Reset">⟲</button>
    </div>
  </div>

  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="viewport"
    class:wiring
    bind:this={vpEl}
    tabindex="0"
    role="application"
    aria-label="Node editor canvas"
    onpointerdown={onCanvasDown}
    onpointermove={onMove}
    onpointerup={onUp}
    onwheel={onWheel}
    onkeydown={onKey}
  >
    <div class="scene" style={`transform:translate(${view.x}px,${view.y}px) scale(${view.k})`}>
      <!-- wires -->
      <svg class="edges">
        {#each graph.edges as e, i (i)}
          {@const fn = nodeById(e.from)}
          {@const tn = nodeById(e.to)}
          {#if fn && tn}
            <path class="edge" d={bezier(outPos(fn), inPos(tn, e.to_input))} onclick={() => deleteEdge(i)} />
          {/if}
        {/each}
        {#if gesture?.kind === 'wire'}
          {@const fn = nodeById(gesture.from)}
          {#if fn}
            <path class="edge live" d={bezier(outPos(fn), { x: gesture.gx, y: gesture.gy })} />
          {/if}
        {/if}
      </svg>

      <!-- nodes -->
      {#each graph.nodes as n (n.id)}
        {@const def = kindDef(n.kind)}
        {@const ins = def?.inputs ?? []}
        <div
          class="node"
          class:sel={selected === n.id}
          style={`left:${n.x}px; top:${n.y}px; width:${NODE_W}px; --c:${cat(n.kind)}`}
        >
          <div class="nhead" onpointerdown={(e) => onHeadDown(e, n.id)}>
            <span class="cdot" style={`background:${cat(n.kind)}`}></span>
            <span class="ntitle">{def?.label ?? n.kind}</span>
            {#if n.kind !== 'input' && n.kind !== 'output'}
              <button
                class="del"
                title="Delete node"
                aria-label="Delete node"
                onpointerdown={(e) => e.stopPropagation()}
                onclick={() => (deleteNode(n.id), (selected = null))}>✕</button>
            {/if}
          </div>

          <!-- input ports -->
          {#each ins as input, ii (input)}
            <div class="port in" class:target={wiring} style={`top:${inTop(n, ii, ins.length)}px`} aria-hidden="true"></div>
            {#if ins.length > 1}
              <span class="plabel" style={`top:${inTop(n, ii, ins.length) - 1}px`}>{input}</span>
            {/if}
          {/each}
          <!-- output port -->
          {#if n.kind !== 'output'}
            <div
              class="port out"
              class:armed={graph.pendingFrom === n.id}
              style={`top:${HEAD_H / 2 - 6}px`}
              title="Drag to an input to connect"
              onpointerdown={(e) => onOutDown(e, n.id)}
            ></div>
          {/if}

          <!-- params -->
          {#if def?.params?.length}
            <div class="body">
              {#each def.params as p (p.key)}
                <label class="np">
                  <span>{p.label}</span>
                  <input
                    type="range"
                    min={p.min}
                    max={p.max}
                    step="any"
                    value={n.params[p.key]}
                    oninput={(e) => setParam(n.id, p.key, +e.currentTarget.value)}
                  />
                  <span class="pv">{Number(n.params[p.key]).toFixed(2)}</span>
                </label>
              {/each}
            </div>
          {/if}
        </div>
      {/each}
    </div>

    <div class="hint">
      Drag from an output <b style="color:var(--accent,#4c9dff)">●</b> to an input to connect · scroll to
      zoom · drag empty space to pan · select a node and press <kbd>Delete</kbd>
    </div>
  </div>
</div>

<style>
  .editor {
    display: flex;
    flex-direction: column;
    height: 100%;
    min-height: 360px;
  }
  .toolbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 8px;
    flex-wrap: wrap;
  }
  .add {
    display: flex;
    flex-wrap: wrap;
    gap: 5px;
  }
  .chip {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    font-size: 11px;
    padding: 3px 9px 3px 7px;
    border-radius: 999px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--panel-2, #262a31);
    color: var(--fg, #dfe3e9);
    cursor: pointer;
  }
  .chip:hover:not(:disabled) {
    border-color: var(--c);
  }
  .chip:disabled {
    opacity: 0.4;
    cursor: default;
  }
  .chip .dot {
    width: 7px;
    height: 7px;
    border-radius: 50%;
  }
  .zoom {
    display: inline-flex;
    align-items: center;
    gap: 2px;
  }
  .zoom button {
    width: 24px;
    height: 24px;
    border-radius: 6px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--panel-2, #262a31);
    color: var(--fg, #dfe3e9);
    cursor: pointer;
    font-size: 13px;
    line-height: 1;
  }
  .zoom .pct {
    font-size: 10.5px;
    color: var(--muted, #949bab);
    min-width: 34px;
    text-align: center;
    font-variant-numeric: tabular-nums;
  }
  .viewport {
    position: relative;
    flex: 1;
    min-height: 320px;
    overflow: hidden;
    border: 1px solid var(--border, #30353d);
    border-radius: 10px;
    background:
      radial-gradient(circle, rgba(255, 255, 255, 0.06) 1px, transparent 1px) 0 0 / 20px 20px,
      var(--vance-bg, #17191d);
    cursor: grab;
    touch-action: none;
    user-select: none;
    outline: none;
  }
  .viewport.wiring {
    cursor: crosshair;
  }
  .scene {
    position: absolute;
    top: 0;
    left: 0;
    transform-origin: 0 0;
    will-change: transform;
  }
  .edges {
    position: absolute;
    left: 0;
    top: 0;
    width: 1px;
    height: 1px;
    overflow: visible;
    pointer-events: none;
  }
  .edge {
    fill: none;
    stroke: var(--accent, #4c9dff);
    stroke-width: 2.5;
    pointer-events: stroke;
    cursor: pointer;
  }
  .edge:hover {
    stroke: var(--err, #f2686f);
    stroke-width: 3.5;
  }
  .edge.live {
    stroke: #f2c14e;
    stroke-dasharray: 5 4;
    pointer-events: none;
  }
  .node {
    position: absolute;
    background: var(--panel-bg, #1e2127);
    border: 1px solid var(--border-strong, #3a4150);
    border-radius: 9px;
    box-shadow: 0 6px 18px rgba(0, 0, 0, 0.45);
  }
  .node.sel {
    border-color: var(--c);
    box-shadow: 0 0 0 2px color-mix(in srgb, var(--c) 55%, transparent), 0 6px 18px rgba(0, 0, 0, 0.45);
  }
  .nhead {
    display: flex;
    align-items: center;
    gap: 6px;
    height: 30px;
    padding: 0 8px;
    font-size: 12px;
    font-weight: 600;
    cursor: grab;
    border-bottom: 1px solid var(--border, #30353d);
    border-radius: 9px 9px 0 0;
    background: linear-gradient(color-mix(in srgb, var(--c) 16%, var(--panel-2, #262a31)), var(--panel-2, #262a31));
  }
  .nhead:active {
    cursor: grabbing;
  }
  .cdot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    flex-shrink: 0;
  }
  .ntitle {
    flex: 1;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .del {
    background: none;
    border: none;
    color: var(--muted, #949bab);
    cursor: pointer;
    font-size: 11px;
    padding: 2px;
  }
  .del:hover {
    color: var(--err, #f2686f);
  }
  .port {
    position: absolute;
    width: 12px;
    height: 12px;
    border-radius: 50%;
    border: 2px solid var(--panel-bg, #1e2127);
    background: #8a93a3;
  }
  .port.in {
    left: -6px;
  }
  .port.out {
    right: -6px;
    background: var(--accent, #4c9dff);
    cursor: crosshair;
  }
  .port.out:hover {
    box-shadow: 0 0 0 4px rgba(76, 157, 255, 0.25);
  }
  .port.in.target {
    background: #5fbf8f;
    box-shadow: 0 0 0 4px rgba(95, 191, 143, 0.25);
  }
  .port.armed {
    background: #f2c14e;
    box-shadow: 0 0 0 4px rgba(242, 193, 78, 0.3);
  }
  .plabel {
    position: absolute;
    left: 8px;
    font-size: 9.5px;
    color: var(--muted, #949bab);
    pointer-events: none;
  }
  .body {
    padding: 5px 8px 7px;
  }
  .np {
    display: grid;
    grid-template-columns: 52px 1fr 30px;
    gap: 6px;
    align-items: center;
    padding: 2px 0;
    font-size: 10.5px;
    color: var(--muted, #949bab);
  }
  .np input[type='range'] {
    accent-color: var(--c, #4c9dff);
    width: 100%;
  }
  .np .pv {
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .hint {
    position: absolute;
    left: 10px;
    bottom: 8px;
    right: 10px;
    font-size: 10.5px;
    color: var(--muted, #949bab);
    pointer-events: none;
    line-height: 1.5;
  }
  kbd {
    font-family: inherit;
    font-size: 10px;
    background: var(--panel-2, #262a31);
    border: 1px solid var(--border-strong, #3a4150);
    border-radius: 4px;
    padding: 0 4px;
  }
</style>
