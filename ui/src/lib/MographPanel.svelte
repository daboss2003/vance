<script lang="ts">
  /**
   * GOAT-5 — Motion Graphics Template panel. Create a title/graphic from a
   * built-in .cmgt template, then edit its exposed controls. Created items
   * land in the Browser; drag one onto the timeline to use it.
   */
  import {
    mographCatalog,
    createMographMedia,
    mographInstance,
    setMographValue,
    type MographInstance,
    type MographControl,
    type MographValue
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';

  let catalog = $state<[string, string][]>([]);
  let selectedKey = $state<string>('');
  let editId = $state<string | null>(null);
  let instance = $state<MographInstance | null>(null);
  let busy = $state(false);
  let error = $state<string | null>(null);

  // Existing mograph items in the project, for re-editing.
  let mographItems = $derived(projectState.media.filter((m) => m.kind === 'mograph'));

  $effect(() => {
    void mographCatalog()
      .then((c) => {
        catalog = c;
        if (!selectedKey && c.length) selectedKey = c[0][0];
      })
      .catch((e) => (error = String(e)));
  });

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
    const info = await act(() => createMographMedia(selectedKey));
    if (!info) return;
    await projectState.refresh();
    editId = info.id;
    await loadInstance();
  }

  async function loadInstance() {
    if (!editId) {
      instance = null;
      return;
    }
    instance = (await act(() => mographInstance(editId!))) ?? null;
  }

  async function onEditSelect(id: string) {
    editId = id || null;
    await loadInstance();
  }

  function current(ctrl: MographControl): MographValue {
    const v = instance?.values[ctrl.id];
    if (v) return v;
    // Fall back to the control's default.
    const k = ctrl.kind;
    if ('Text' in k) return { Text: k.Text.default };
    if ('Number' in k) return { Number: k.Number.default };
    if ('Color' in k) return { Color: k.Color.default };
    if ('Choice' in k) return { Choice: k.Choice.default };
    if ('Bool' in k) return { Bool: k.Bool.default };
    if ('Font' in k) return { Font: k.Font.default };
    return { Point: k.Point.default };
  }

  async function commit(ctrl: MographControl, value: MographValue) {
    if (!editId) return;
    await act(() => setMographValue(editId!, ctrl.id, value));
    await loadInstance();
  }

  // Color helpers: [r,g,b,a] in 0..1 <-> #rrggbb hex.
  function toHex(c: [number, number, number, number]): string {
    const h = (x: number) =>
      Math.round(Math.min(1, Math.max(0, x)) * 255)
        .toString(16)
        .padStart(2, '0');
    return `#${h(c[0])}${h(c[1])}${h(c[2])}`;
  }
  function fromHex(hex: string, alpha: number): [number, number, number, number] {
    const n = parseInt(hex.slice(1), 16);
    return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255, alpha];
  }
</script>

<div class="panel">
  <div class="head">
    <span>Titles &amp; Motion Graphics</span>
  </div>
  {#if error}<div class="err">{error}</div>{/if}

  <div class="create">
    <select bind:value={selectedKey} disabled={busy}>
      {#each catalog as [key, name] (key)}
        <option value={key}>{name}</option>
      {/each}
    </select>
    <button onclick={create} disabled={busy || !selectedKey}>＋ Create</button>
  </div>

  {#if mographItems.length > 0}
    <div class="edit-pick">
      <span>Edit:</span>
      <select
        value={editId ?? ''}
        onchange={(e) => onEditSelect((e.target as HTMLSelectElement).value)}
        disabled={busy}
      >
        <option value="">— select —</option>
        {#each mographItems as m (m.id)}
          <option value={m.id}>{m.name}</option>
        {/each}
      </select>
    </div>
  {/if}

  {#if instance}
    <div class="controls">
      {#each instance.template.controls as ctrl (ctrl.id)}
        {@const v = current(ctrl)}
        <label class="ctrl">
          <span class="lbl">{ctrl.label}</span>
          {#if 'Text' in ctrl.kind && 'Text' in v}
            <input
              type="text"
              value={v.Text}
              disabled={busy}
              onchange={(e) => commit(ctrl, { Text: (e.target as HTMLInputElement).value })}
            />
          {:else if 'Font' in ctrl.kind && 'Font' in v}
            <input
              type="text"
              value={v.Font}
              disabled={busy}
              onchange={(e) => commit(ctrl, { Font: (e.target as HTMLInputElement).value })}
            />
          {:else if 'Number' in ctrl.kind && 'Number' in v}
            <input
              type="range"
              min={ctrl.kind.Number.min}
              max={ctrl.kind.Number.max}
              step="0.5"
              value={v.Number}
              disabled={busy}
              onchange={(e) =>
                commit(ctrl, { Number: parseFloat((e.target as HTMLInputElement).value) })}
            />
            <span class="val">{v.Number.toFixed(1)}</span>
          {:else if 'Color' in ctrl.kind && 'Color' in v}
            <input
              type="color"
              value={toHex(v.Color)}
              disabled={busy}
              onchange={(e) =>
                commit(ctrl, { Color: fromHex((e.target as HTMLInputElement).value, v.Color[3]) })}
            />
          {:else if 'Bool' in ctrl.kind && 'Bool' in v}
            <input
              type="checkbox"
              checked={v.Bool}
              disabled={busy}
              onchange={(e) => commit(ctrl, { Bool: (e.target as HTMLInputElement).checked })}
            />
          {:else if 'Choice' in ctrl.kind && 'Choice' in v}
            <select
              value={v.Choice}
              disabled={busy}
              onchange={(e) =>
                commit(ctrl, { Choice: parseInt((e.target as HTMLSelectElement).value, 10) })}
            >
              {#each ctrl.kind.Choice.options as opt, i (i)}
                <option value={i}>{opt}</option>
              {/each}
            </select>
          {:else if 'Point' in ctrl.kind && 'Point' in v}
            <span class="point">
              <input
                type="number"
                step="0.01"
                min="0"
                max="1"
                value={v.Point[0]}
                disabled={busy}
                onchange={(e) =>
                  commit(ctrl, {
                    Point: [parseFloat((e.target as HTMLInputElement).value), v.Point[1]]
                  })}
              />
              <input
                type="number"
                step="0.01"
                min="0"
                max="1"
                value={v.Point[1]}
                disabled={busy}
                onchange={(e) =>
                  commit(ctrl, {
                    Point: [v.Point[0], parseFloat((e.target as HTMLInputElement).value)]
                  })}
              />
            </span>
          {/if}
        </label>
      {/each}
      <div class="hint">
        Drag the item from the Browser onto the timeline. It animates as the playhead crosses it.
      </div>
    </div>
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
  .edit-pick {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .create select,
  .edit-pick select {
    flex: 1;
  }
  .controls {
    display: flex;
    flex-direction: column;
    gap: 5px;
    border-top: 1px solid var(--border, #2a2a2a);
    padding-top: 6px;
  }
  .ctrl {
    display: flex;
    align-items: center;
    gap: 6px;
  }
  .lbl {
    width: 84px;
    opacity: 0.8;
  }
  .ctrl input[type='text'],
  .ctrl input[type='range'] {
    flex: 1;
  }
  .val {
    width: 44px;
    text-align: right;
    font-variant-numeric: tabular-nums;
    opacity: 0.85;
  }
  .point {
    display: flex;
    gap: 4px;
    flex: 1;
  }
  .point input {
    width: 56px;
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
