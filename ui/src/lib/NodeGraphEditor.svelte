<script lang="ts">
  /**
   * Node-graph editor (M6 Phase 2). Draggable nodes with input/output ports;
   * click an output then an input to connect. Compiles to WGSL live.
   */
  import {
    graph,
    kindDef,
    addNode,
    moveNode,
    deleteNode,
    deleteEdge,
    startConnect,
    finishConnect,
    setParam,
    ensureSeeded
  } from '$lib/nodegraph.svelte';

  const NODE_W = 152;
  const HEAD_Y = 20;

  ensureSeeded();

  function outPort(id: string): { x: number; y: number } | null {
    const n = graph.nodes.find((m) => m.id === id);
    return n ? { x: n.x + NODE_W, y: n.y + HEAD_Y } : null;
  }
  function inPort(id: string, input: string): { x: number; y: number } | null {
    const n = graph.nodes.find((m) => m.id === id);
    if (!n) return null;
    const idx = kindDef(n.kind)?.inputs.indexOf(input) ?? 0;
    return { x: n.x, y: n.y + HEAD_Y + idx * 20 };
  }

  // Drag via pointer capture.
  let drag: { id: string; ox: number; oy: number } | null = null;
  function onDown(e: PointerEvent, id: string) {
    const n = graph.nodes.find((m) => m.id === id)!;
    drag = { id, ox: e.clientX - n.x, oy: e.clientY - n.y };
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
  }
  function onMove(e: PointerEvent) {
    if (drag) moveNode(drag.id, Math.max(0, e.clientX - drag.ox), Math.max(0, e.clientY - drag.oy));
  }
  function onUp() {
    drag = null;
  }
</script>

<div class="wrap">
  <div class="palette">
    {#each graph.palette as k (k.key)}
      <button onclick={() => addNode(k.key, 60, 60)}>+ {k.label}</button>
    {/each}
  </div>

  <div class="canvas">
    <!-- edges -->
    <svg class="edges">
      {#each graph.edges as e, i (i)}
        {@const a = outPort(e.from)}
        {@const b = inPort(e.to, e.to_input)}
        {#if a && b}
          <path
            d={`M ${a.x} ${a.y} C ${a.x + 50} ${a.y}, ${b.x - 50} ${b.y}, ${b.x} ${b.y}`}
            class="edge"
            onclick={() => deleteEdge(i)}
          />
        {/if}
      {/each}
    </svg>

    <!-- nodes -->
    {#each graph.nodes as n (n.id)}
      {@const def = kindDef(n.kind)}
      <div
        class="node"
        style={`left:${n.x}px; top:${n.y}px; width:${NODE_W}px`}
        onpointermove={onMove}
        onpointerup={onUp}
      >
        <div class="nhead" onpointerdown={(e) => onDown(e, n.id)}>
          <span>{def?.label ?? n.kind}</span>
          {#if n.kind !== 'input' && n.kind !== 'output'}
            <button class="del" onpointerdown={(e) => e.stopPropagation()} onclick={() => deleteNode(n.id)}>✕</button>
          {/if}
        </div>

        <!-- input ports -->
        {#each def?.inputs ?? [] as input, ii (input)}
          <button
            class="port in"
            style={`top:${HEAD_Y + ii * 20 - 6}px`}
            title={`connect to “${input}”`}
            aria-label={`input ${input}`}
            onclick={() => finishConnect(n.id, input)}
          ></button>
        {/each}
        <!-- output port -->
        {#if n.kind !== 'output'}
          <button
            class="port out"
            class:armed={graph.pendingFrom === n.id}
            style={`top:${HEAD_Y - 6}px`}
            title="start a connection"
            aria-label="output"
            onclick={() => startConnect(n.id)}
          ></button>
        {/if}

        <!-- params -->
        {#each def?.params ?? [] as p (p.key)}
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
          </label>
        {/each}
      </div>
    {/each}
  </div>
  <div class="tip">
    Click an effect's <b>output dot</b> (right), then another's <b>input dot</b> (left) to connect.
    Click a line to remove it. One <b>Source</b> and one <b>Output</b>.
  </div>
</div>

<style>
  .wrap {
    display: flex;
    flex-direction: column;
    height: 100%;
  }
  .palette {
    display: flex;
    flex-wrap: wrap;
    gap: 5px;
    margin-bottom: 8px;
  }
  .palette button {
    font-size: 11px;
    padding: 3px 8px;
    border-radius: 6px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--panel-2, #262a31);
    color: var(--fg, #dfe3e9);
    cursor: pointer;
  }
  .canvas {
    position: relative;
    flex: 1;
    min-height: 320px;
    border: 1px solid var(--border, #30353d);
    border-radius: 8px;
    background:
      radial-gradient(circle, rgba(255, 255, 255, 0.05) 1px, transparent 1px) 0 0 / 18px 18px,
      var(--vance-bg, #17191d);
    overflow: auto;
  }
  .edges {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    pointer-events: none;
    overflow: visible;
  }
  .edge {
    fill: none;
    stroke: var(--accent, #4c9dff);
    stroke-width: 2;
    pointer-events: stroke;
    cursor: pointer;
  }
  .edge:hover {
    stroke: var(--err, #f2686f);
  }
  .node {
    position: absolute;
    background: var(--panel-bg, #1e2127);
    border: 1px solid var(--border-strong, #3a4150);
    border-radius: 8px;
    box-shadow: 0 6px 16px rgba(0, 0, 0, 0.4);
    padding-bottom: 6px;
  }
  .nhead {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 4px 8px;
    font-size: 12px;
    font-weight: 600;
    cursor: grab;
    border-bottom: 1px solid var(--border, #30353d);
    border-radius: 8px 8px 0 0;
    background: var(--panel-2, #262a31);
    user-select: none;
  }
  .del {
    background: none;
    border: none;
    color: var(--muted, #949bab);
    cursor: pointer;
    font-size: 11px;
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
    background: var(--accent, #4c9dff);
    cursor: crosshair;
    padding: 0;
  }
  .port.in {
    left: -7px;
  }
  .port.out {
    right: -7px;
  }
  .port.armed {
    background: #f2c14e;
    box-shadow: 0 0 0 3px rgba(242, 193, 78, 0.3);
  }
  .np {
    display: grid;
    grid-template-columns: 56px 1fr;
    gap: 6px;
    align-items: center;
    padding: 3px 8px 0;
    font-size: 10.5px;
    color: var(--muted, #949bab);
  }
  .np input[type='range'] {
    accent-color: var(--accent, #4c9dff);
  }
  .tip {
    font-size: 11px;
    color: var(--muted, #949bab);
    margin-top: 8px;
    line-height: 1.5;
  }
</style>
