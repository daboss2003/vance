<script lang="ts">
  /**
   * GOAT-12 — Fusion node-graph compositor panel. Create a composition,
   * then edit its DAG of nodes: sources (background/text/shape/media),
   * filters (transform/blur/color/crop), and combiners (merge/mask). Each
   * node's key params are editable inline and input slots wire to other
   * nodes. The whole graph is pushed back to the engine on every change.
   * Created comps land in the Browser; drag one onto the timeline.
   */
  import {
    createFusionMedia,
    fusionComp,
    setFusionComp,
    type FusionComp,
    type FusionNode,
    type FusionNodeKind,
    type FusionAnim,
    type FusionRgba
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';

  let editId = $state<string | null>(null);
  let comp = $state<FusionComp | null>(null);
  let busy = $state(false);
  let error = $state<string | null>(null);

  // New-comp form.
  let newW = $state(1920);
  let newH = $state(1080);
  let newDur = $state(5);
  let addKind = $state<AddableKind>('Text');

  let fusionItems = $derived(projectState.media.filter((m) => m.kind === 'fusion'));

  async function act<T>(fn: () => Promise<T>): Promise<T | undefined> {
    busy = true;
    error = null;
    try {
      return await fn();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function create() {
    const info = await act(() =>
      createFusionMedia({ width: newW, height: newH, durationSeconds: newDur })
    );
    if (!info) return;
    await projectState.refresh();
    editId = info.id;
    await load();
  }

  async function load() {
    if (!editId) {
      comp = null;
      return;
    }
    comp = (await act(() => fusionComp(editId!))) ?? null;
  }

  async function onEditSelect(id: string) {
    editId = id || null;
    await load();
  }

  // Push the current graph back to the engine (after any edit).
  async function push() {
    if (!editId || !comp) return;
    await act(() => setFusionComp(editId!, $state.snapshot(comp!) as FusionComp));
  }

  // ---- node model helpers -------------------------------------------

  type AddableKind =
    | 'Background'
    | 'Text'
    | 'Rectangle'
    | 'MediaIn'
    | 'Transform'
    | 'Blur'
    | 'ColorCorrect'
    | 'Crop'
    | 'Merge'
    | 'Mask';

  function variant(node: FusionNode): string {
    return Object.keys(node.kind)[0];
  }
  // The inner spec object of a node's kind (loosely typed for editing).
  function spec(node: FusionNode): any {
    return (node.kind as any)[variant(node)];
  }
  function slotsFor(node: FusionNode): string[] {
    switch (variant(node)) {
      case 'Transform':
      case 'Blur':
      case 'ColorCorrect':
      case 'Crop':
        return ['input'];
      case 'Merge':
        return ['background', 'foreground'];
      case 'Mask':
        return ['input', 'mask'];
      default:
        return [];
    }
  }

  // Anim <-> number (only the Const case is edited inline).
  function animNum(a: FusionAnim | undefined, fallback = 0): number {
    return a && 'Const' in a ? a.Const : fallback;
  }
  function isConst(a: FusionAnim | undefined): boolean {
    return !a || 'Const' in a;
  }

  function toHex(c: FusionRgba): string {
    const h = (x: number) =>
      Math.round(Math.min(1, Math.max(0, x)) * 255)
        .toString(16)
        .padStart(2, '0');
    return `#${h(c[0])}${h(c[1])}${h(c[2])}`;
  }
  function fromHex(hex: string, alpha: number): FusionRgba {
    const n = parseInt(hex.slice(1), 16);
    return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255, alpha];
  }

  // Mutate + persist.
  function edit(fn: () => void) {
    if (!comp) return;
    fn();
    void push();
  }

  function setConn(node: FusionNode, slot: string, src: string) {
    edit(() => {
      if (src) node.inputs[slot] = src;
      else delete node.inputs[slot];
    });
  }

  function deleteNode(id: string) {
    if (!comp) return;
    edit(() => {
      comp!.nodes = comp!.nodes.filter((n) => n.id !== id);
      // Drop dangling connections that referenced it.
      for (const n of comp!.nodes) {
        for (const [slot, src] of Object.entries(n.inputs)) {
          if (src === id) delete n.inputs[slot];
        }
      }
      if (comp!.output === id) comp!.output = comp!.nodes[0]?.id ?? '';
    });
  }

  function uniqueId(base: string): string {
    if (!comp) return base;
    let i = 1;
    let id = `${base}${i}`;
    const has = (x: string) => comp!.nodes.some((n) => n.id === x);
    while (has(id)) {
      i += 1;
      id = `${base}${i}`;
    }
    return id;
  }

  function defaultKind(kind: AddableKind): FusionNodeKind {
    const white: FusionRgba = [1, 1, 1, 1];
    switch (kind) {
      case 'Background':
        return { Background: { color: [0.1, 0.12, 0.16, 1], gradient_to: null } };
      case 'Text':
        return {
          Text: { text: 'Text', font: 'Arial', size_px: { Const: 96 }, color: white, align: 'Center' }
        };
      case 'Rectangle':
        return {
          Rectangle: {
            width: { Const: 400 },
            height: { Const: 200 },
            radius: { Const: 0 },
            color: white
          }
        };
      case 'MediaIn':
        return { MediaIn: { name: 'source', fit: 'Contain' } };
      case 'Transform': {
        const cx = comp ? comp.width / 2 : 960;
        const cy = comp ? comp.height / 2 : 540;
        return {
          Transform: {
            anchor: [{ Const: 0.5 }, { Const: 0.5 }],
            position: [{ Const: cx }, { Const: cy }],
            scale: [{ Const: 1 }, { Const: 1 }],
            rotation_deg: { Const: 0 },
            opacity: { Const: 1 }
          }
        } as FusionNodeKind;
      }
      case 'Blur':
        return { Blur: { radius: { Const: 8 }, mode: 'Gaussian' } };
      case 'ColorCorrect':
        return {
          ColorCorrect: {
            lift: [{ Const: 0 }, { Const: 0 }, { Const: 0 }],
            gain: [{ Const: 1 }, { Const: 1 }, { Const: 1 }],
            gamma: { Const: 1 },
            saturation: { Const: 1 }
          }
        } as FusionNodeKind;
      case 'Crop':
        return {
          Crop: {
            left: { Const: 0 },
            top: { Const: 0 },
            right: { Const: 0 },
            bottom: { Const: 0 }
          }
        } as FusionNodeKind;
      case 'Merge':
        return { Merge: { mode: 'Over', opacity: { Const: 1 } } };
      case 'Mask':
        return { Mask: { channel: 'Luminance', invert: false } };
    }
  }

  function addNode() {
    if (!comp) return;
    edit(() => {
      const id = uniqueId(addKind.toLowerCase().slice(0, 4));
      const node: FusionNode = {
        id,
        name: addKind,
        kind: defaultKind(addKind),
        inputs: {},
        ui_pos: [0, 0]
      };
      comp!.nodes.push(node);
    });
  }
</script>

<div class="panel">
  <div class="head"><span>Fusion — Node Compositor</span></div>
  {#if error}<div class="err">{error}</div>{/if}

  <div class="create">
    <label>W<input type="number" min="2" bind:value={newW} disabled={busy} /></label>
    <label>H<input type="number" min="2" bind:value={newH} disabled={busy} /></label>
    <label>sec<input type="number" min="0.1" step="0.1" bind:value={newDur} disabled={busy} /></label>
    <button onclick={create} disabled={busy}>＋ New Comp</button>
  </div>

  {#if fusionItems.length > 0}
    <div class="edit-pick">
      <span>Edit:</span>
      <select value={editId ?? ''} onchange={(e) => onEditSelect((e.target as HTMLSelectElement).value)} disabled={busy}>
        <option value="">— select —</option>
        {#each fusionItems as m (m.id)}
          <option value={m.id}>{m.name}</option>
        {/each}
      </select>
    </div>
  {/if}

  {#if comp}
    <div class="graphbar">
      <span class="dim">{comp.width}×{comp.height} · {comp.duration_s.toFixed(1)}s</span>
      <label class="out">
        Output
        <select value={comp.output} onchange={(e) => edit(() => (comp!.output = (e.target as HTMLSelectElement).value))} disabled={busy}>
          {#each comp.nodes as n (n.id)}
            <option value={n.id}>{n.name}</option>
          {/each}
        </select>
      </label>
    </div>

    <div class="add">
      <select bind:value={addKind} disabled={busy}>
        {#each ['Background', 'Text', 'Rectangle', 'MediaIn', 'Transform', 'Blur', 'ColorCorrect', 'Crop', 'Merge', 'Mask'] as k (k)}
          <option value={k}>{k}</option>
        {/each}
      </select>
      <button onclick={addNode} disabled={busy}>＋ Add node</button>
    </div>

    <div class="nodes">
      {#each comp.nodes as node (node.id)}
        {@const v = variant(node)}
        {@const s = spec(node)}
        <div class="node" class:isout={comp.output === node.id}>
          <div class="nhead">
            <span class="badge">{v}</span>
            <input class="nname" value={node.name} disabled={busy}
              onchange={(e) => edit(() => (node.name = (e.target as HTMLInputElement).value))} />
            <button class="del" title="Delete node" onclick={() => deleteNode(node.id)} disabled={busy}>✕</button>
          </div>

          <!-- Input connections -->
          {#each slotsFor(node) as slot (slot)}
            <label class="row">
              <span class="lbl">{slot}</span>
              <select value={node.inputs[slot] ?? ''} disabled={busy}
                onchange={(e) => setConn(node, slot, (e.target as HTMLSelectElement).value)}>
                <option value="">— none —</option>
                {#each comp.nodes.filter((n) => n.id !== node.id) as n (n.id)}
                  <option value={n.id}>{n.name}</option>
                {/each}
              </select>
            </label>
          {/each}

          <!-- Per-kind editable params -->
          {#if v === 'Background'}
            <label class="row"><span class="lbl">color</span>
              <input type="color" value={toHex(s.color)} disabled={busy}
                onchange={(e) => edit(() => (s.color = fromHex((e.target as HTMLInputElement).value, s.color[3])))} />
            </label>
            <label class="row"><span class="lbl">gradient</span>
              <input type="checkbox" checked={!!s.gradient_to} disabled={busy}
                onchange={(e) => edit(() => (s.gradient_to = (e.target as HTMLInputElement).checked ? [0, 0, 0, 1] : null))} />
              {#if s.gradient_to}
                <input type="color" value={toHex(s.gradient_to)} disabled={busy}
                  onchange={(e) => edit(() => (s.gradient_to = fromHex((e.target as HTMLInputElement).value, 1)))} />
              {/if}
            </label>
          {:else if v === 'Text'}
            <label class="row"><span class="lbl">text</span>
              <input type="text" value={s.text} disabled={busy}
                onchange={(e) => edit(() => (s.text = (e.target as HTMLInputElement).value))} /></label>
            <label class="row"><span class="lbl">font</span>
              <input type="text" value={s.font} disabled={busy}
                onchange={(e) => edit(() => (s.font = (e.target as HTMLInputElement).value))} /></label>
            <label class="row"><span class="lbl">size</span>
              <input type="number" min="1" value={animNum(s.size_px, 96)} disabled={busy || !isConst(s.size_px)}
                onchange={(e) => edit(() => (s.size_px = { Const: parseFloat((e.target as HTMLInputElement).value) }))} /></label>
            <label class="row"><span class="lbl">color</span>
              <input type="color" value={toHex(s.color)} disabled={busy}
                onchange={(e) => edit(() => (s.color = fromHex((e.target as HTMLInputElement).value, s.color[3])))} /></label>
            <label class="row"><span class="lbl">align</span>
              <select value={s.align ?? 'Left'} disabled={busy}
                onchange={(e) => edit(() => (s.align = (e.target as HTMLSelectElement).value))}>
                <option>Left</option><option>Center</option><option>Right</option>
              </select></label>
          {:else if v === 'Rectangle'}
            <label class="row"><span class="lbl">w × h</span>
              <input class="sm" type="number" value={animNum(s.width)} disabled={busy}
                onchange={(e) => edit(() => (s.width = { Const: parseFloat((e.target as HTMLInputElement).value) }))} />
              <input class="sm" type="number" value={animNum(s.height)} disabled={busy}
                onchange={(e) => edit(() => (s.height = { Const: parseFloat((e.target as HTMLInputElement).value) }))} /></label>
            <label class="row"><span class="lbl">radius</span>
              <input type="number" min="0" value={animNum(s.radius)} disabled={busy}
                onchange={(e) => edit(() => (s.radius = { Const: parseFloat((e.target as HTMLInputElement).value) }))} /></label>
            <label class="row"><span class="lbl">color</span>
              <input type="color" value={toHex(s.color)} disabled={busy}
                onchange={(e) => edit(() => (s.color = fromHex((e.target as HTMLInputElement).value, s.color[3])))} /></label>
          {:else if v === 'MediaIn'}
            <label class="row"><span class="lbl">name</span>
              <input type="text" value={s.name} disabled={busy}
                onchange={(e) => edit(() => (s.name = (e.target as HTMLInputElement).value))} /></label>
            <label class="row"><span class="lbl">fit</span>
              <select value={s.fit ?? 'Contain'} disabled={busy}
                onchange={(e) => edit(() => (s.fit = (e.target as HTMLSelectElement).value))}>
                <option>None</option><option>Stretch</option><option>Contain</option><option>Cover</option>
              </select></label>
          {:else if v === 'Transform'}
            <label class="row"><span class="lbl">pos x,y</span>
              <input class="sm" type="number" value={animNum(s.position[0])} disabled={busy}
                onchange={(e) => edit(() => (s.position[0] = { Const: parseFloat((e.target as HTMLInputElement).value) }))} />
              <input class="sm" type="number" value={animNum(s.position[1])} disabled={busy}
                onchange={(e) => edit(() => (s.position[1] = { Const: parseFloat((e.target as HTMLInputElement).value) }))} /></label>
            <label class="row"><span class="lbl">scale</span>
              <input class="sm" type="number" step="0.01" value={animNum(s.scale[0], 1)} disabled={busy}
                onchange={(e) => edit(() => (s.scale[0] = { Const: parseFloat((e.target as HTMLInputElement).value) }))} />
              <input class="sm" type="number" step="0.01" value={animNum(s.scale[1], 1)} disabled={busy}
                onchange={(e) => edit(() => (s.scale[1] = { Const: parseFloat((e.target as HTMLInputElement).value) }))} /></label>
            <label class="row"><span class="lbl">rotate</span>
              <input type="number" step="1" value={animNum(s.rotation_deg)} disabled={busy}
                onchange={(e) => edit(() => (s.rotation_deg = { Const: parseFloat((e.target as HTMLInputElement).value) }))} /></label>
            <label class="row"><span class="lbl">opacity</span>
              <input type="range" min="0" max="1" step="0.01" value={animNum(s.opacity, 1)} disabled={busy}
                onchange={(e) => edit(() => (s.opacity = { Const: parseFloat((e.target as HTMLInputElement).value) }))} /></label>
          {:else if v === 'Blur'}
            <label class="row"><span class="lbl">radius</span>
              <input type="number" min="0" value={animNum(s.radius, 8)} disabled={busy}
                onchange={(e) => edit(() => (s.radius = { Const: parseFloat((e.target as HTMLInputElement).value) }))} /></label>
            <label class="row"><span class="lbl">mode</span>
              <select value={s.mode ?? 'Gaussian'} disabled={busy}
                onchange={(e) => edit(() => (s.mode = (e.target as HTMLSelectElement).value))}>
                <option>Box</option><option>Gaussian</option>
              </select></label>
          {:else if v === 'ColorCorrect'}
            <label class="row"><span class="lbl">gamma</span>
              <input type="number" min="0.1" step="0.05" value={animNum(s.gamma, 1)} disabled={busy}
                onchange={(e) => edit(() => (s.gamma = { Const: parseFloat((e.target as HTMLInputElement).value) }))} /></label>
            <label class="row"><span class="lbl">saturation</span>
              <input type="range" min="0" max="3" step="0.05" value={animNum(s.saturation, 1)} disabled={busy}
                onchange={(e) => edit(() => (s.saturation = { Const: parseFloat((e.target as HTMLInputElement).value) }))} /></label>
          {:else if v === 'Crop'}
            <label class="row"><span class="lbl">L,T,R,B</span>
              {#each ['left', 'top', 'right', 'bottom'] as edge (edge)}
                <input class="sm" type="number" min="0" value={animNum(s[edge])} disabled={busy}
                  onchange={(e) => edit(() => (s[edge] = { Const: parseFloat((e.target as HTMLInputElement).value) }))} />
              {/each}</label>
          {:else if v === 'Merge'}
            <label class="row"><span class="lbl">mode</span>
              <select value={s.mode ?? 'Over'} disabled={busy}
                onchange={(e) => edit(() => (s.mode = (e.target as HTMLSelectElement).value))}>
                <option>Over</option><option>Add</option><option>Multiply</option><option>Screen</option>
              </select></label>
            <label class="row"><span class="lbl">opacity</span>
              <input type="range" min="0" max="1" step="0.01" value={animNum(s.opacity, 1)} disabled={busy}
                onchange={(e) => edit(() => (s.opacity = { Const: parseFloat((e.target as HTMLInputElement).value) }))} /></label>
          {:else if v === 'Mask'}
            <label class="row"><span class="lbl">channel</span>
              <select value={s.channel ?? 'Alpha'} disabled={busy}
                onchange={(e) => edit(() => (s.channel = (e.target as HTMLSelectElement).value))}>
                <option>Alpha</option><option>Luminance</option>
              </select></label>
            <label class="row"><span class="lbl">invert</span>
              <input type="checkbox" checked={!!s.invert} disabled={busy}
                onchange={(e) => edit(() => (s.invert = (e.target as HTMLInputElement).checked))} /></label>
          {/if}
        </div>
      {/each}
    </div>
    <div class="hint">Drag the comp from the Browser onto the timeline. The graph animates as the playhead crosses it.</div>
  {/if}
</div>

<style>
  .panel {
    display: flex;
    flex-direction: column;
    font-size: 12px;
    color: var(--fg, #ddd);
    gap: 6px;
    padding: 6px;
  }
  .head {
    font-weight: 600;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 4px;
  }
  .create,
  .edit-pick,
  .add,
  .graphbar {
    display: flex;
    gap: 6px;
    align-items: center;
    flex-wrap: wrap;
  }
  .create label {
    display: flex;
    align-items: center;
    gap: 3px;
    opacity: 0.85;
  }
  .create input {
    width: 56px;
  }
  .edit-pick select,
  .add select {
    flex: 1;
  }
  .graphbar .dim {
    opacity: 0.6;
  }
  .graphbar .out {
    margin-left: auto;
    display: flex;
    align-items: center;
    gap: 4px;
  }
  .nodes {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .node {
    border: 1px solid var(--border, #2a2a2a);
    border-radius: 4px;
    padding: 5px 6px;
    background: var(--panel-bg, #1b1b1b);
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .node.isout {
    border-color: #7ec699;
  }
  .nhead {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .badge {
    font-size: 10px;
    background: #333;
    border-radius: 3px;
    padding: 1px 5px;
    opacity: 0.9;
  }
  .nname {
    flex: 1;
    background: transparent;
    border: none;
    color: var(--fg, #ddd);
    font-weight: 600;
  }
  .del {
    background: transparent;
    border: none;
    color: #ff6b6b;
    cursor: pointer;
    opacity: 0.7;
  }
  .del:hover {
    opacity: 1;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .lbl {
    width: 66px;
    opacity: 0.7;
    font-size: 11px;
  }
  .row input[type='text'],
  .row input[type='range'],
  .row select {
    flex: 1;
    min-width: 0;
  }
  .row input.sm {
    width: 60px;
    flex: none;
  }
  .hint {
    opacity: 0.5;
    font-size: 10px;
    margin-top: 4px;
  }
  .err {
    color: #ff6b6b;
    font-size: 11px;
  }
</style>
