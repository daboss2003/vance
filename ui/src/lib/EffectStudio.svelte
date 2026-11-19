<script lang="ts">
  /**
   * Effect Studio (M6) — no-code effect creation. Edit one WGSL `effect()`
   * function, declare sliders, live-preview on a sample image, and save it as a
   * shareable `.vsfx` add-on effect.
   */
  import {
    studio,
    closeStudio,
    schedulePreview,
    addParam,
    removeParam,
    save
  } from '$lib/studio.svelte';
  import NodeGraphEditor from '$lib/NodeGraphEditor.svelte';
  import { dialogFocus } from '$lib/dialog-focus';

  let canvasEl = $state<HTMLCanvasElement | null>(null);
  let mode = $state<'code' | 'nodes'>('code');

  // Draw the preview RGBA whenever it changes.
  $effect(() => {
    const p = studio.preview;
    if (!canvasEl || !p) return;
    const bin = atob(p.rgba);
    const bytes = new Uint8ClampedArray(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    canvasEl.width = p.width;
    canvasEl.height = p.height;
    canvasEl.getContext('2d')?.putImageData(new ImageData(bytes, p.width, p.height), 0, 0);
  });
</script>

{#if studio.open}
  <div
    class="scrim"
    role="dialog"
    use:dialogFocus
    aria-modal="true"
    aria-label="Effect Studio"
    tabindex="-1"
    onclick={(e) => {
      if (e.target === e.currentTarget) closeStudio();
    }}
    onkeydown={(e) => {
      // Esc closes this dialog only (not the app's global Esc = clear selection).
      if (e.key === 'Escape') {
        e.stopPropagation();
        closeStudio();
      }
    }}
  >
    <div class="card">
      <div class="head">
        <h2>Effect Studio</h2>
        <button class="x" onclick={closeStudio} aria-label="Close">✕</button>
      </div>

      <div class="cols">
        <!-- Left: editor + controls -->
        <div class="left">
          <div class="ids">
            <input class="in" placeholder="Name" bind:value={studio.name} />
            <input class="in" placeholder="Effect id (com.you.name)" bind:value={studio.id} />
          </div>
          <input class="in" placeholder="Your name (author)" bind:value={studio.author} />

          <div class="modes">
            <button class:on={mode === 'code'} onclick={() => (mode = 'code')}>Code</button>
            <button class:on={mode === 'nodes'} onclick={() => (mode = 'nodes')}>Nodes</button>
          </div>

          {#if mode === 'code'}
            <div class="sec">Effect code (WGSL)</div>
            <textarea
              class="code"
              spellcheck="false"
              bind:value={studio.body}
              oninput={schedulePreview}
            ></textarea>
            <div class="hint">
              Write <span class="mono">fn effect(color, uv) -&gt; vec4&lt;f32&gt;</span>. Read your
              sliders with <span class="mono">param(0)</span>, <span class="mono">param(1)</span>…
            </div>

            <div class="sec row">
              <span>Controls (sliders)</span>
              <button class="mini" onclick={addParam}>+ Add</button>
            </div>
            {#each studio.params as p, i (i)}
              <div class="param">
                <input class="in tiny" placeholder="Label" bind:value={p.label} />
                <input class="in num" type="number" step="any" bind:value={p.min} title="min" />
                <input class="in num" type="number" step="any" bind:value={p.max} title="max" />
                <input
                  class="in num"
                  type="number"
                  step="any"
                  bind:value={p.default}
                  title="default"
                  oninput={schedulePreview}
                />
                <button class="rm" onclick={() => removeParam(i)} aria-label="Remove">✕</button>
              </div>
            {/each}
          {:else}
            <NodeGraphEditor />
          {/if}
        </div>

        <!-- Right: preview + save -->
        <div class="right">
          <div class="sec">Live preview</div>
          <div class="preview">
            <canvas bind:this={canvasEl}></canvas>
            {#if studio.busy}<div class="spin">rendering…</div>{/if}
          </div>

          {#if mode === 'code'}
            {#each studio.params as p (p.key)}
              <label class="slider">
                <span>{p.label}</span>
                <input
                  type="range"
                  min={p.min}
                  max={p.max}
                  step="any"
                  bind:value={p.value}
                  oninput={schedulePreview}
                />
                <span class="val">{p.value.toFixed(2)}</span>
              </label>
            {/each}
          {/if}

          {#if studio.error}
            <pre class="err">{studio.error}</pre>
          {/if}

          <button class="save" disabled={studio.busy || !studio.name || !studio.id} onclick={save}>
            Save effect
          </button>
          <div class="hint">
            Saves to your add-on effects so you can use it on clips — or share/sell the
            <span class="mono">.vsfx</span> file.
          </div>
        </div>
      </div>
    </div>
  </div>
{/if}

<style>
  .scrim:focus {
    outline: none;
  }
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 3500;
    background: rgba(0, 0, 0, 0.62);
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 20px;
  }
  .card {
    width: 900px;
    max-width: 96vw;
    max-height: 90vh;
    overflow: auto;
    background: var(--panel-bg, #1e2127);
    color: var(--fg, #dfe3e9);
    border: 1px solid var(--border, #30353d);
    border-radius: 12px;
    padding: 18px 20px;
    box-shadow: 0 24px 64px rgba(0, 0, 0, 0.55);
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 14px;
  }
  h2 {
    font-size: 16px;
    font-weight: 700;
    margin: 0;
  }
  .x {
    background: none;
    border: none;
    color: var(--muted, #949bab);
    cursor: pointer;
    font-size: 14px;
  }
  .cols {
    display: grid;
    grid-template-columns: 1fr 360px;
    gap: 18px;
  }
  /* Grid items default to min-width:auto, so wide content (the code box, or the
     preview canvas's intrinsic buffer size) can blow a column past its track and
     spill over the card. min-width:0 lets them shrink to their track instead. */
  .left,
  .right {
    min-width: 0;
  }
  @media (max-width: 720px) {
    .cols {
      grid-template-columns: 1fr;
    }
  }
  .ids {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }
  .in {
    width: 100%;
    box-sizing: border-box;
    padding: 7px 9px;
    margin-bottom: 8px;
    border-radius: 7px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--vance-bg, #17191d);
    color: var(--fg, #dfe3e9);
    font-size: 13px;
  }
  .sec {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted, #949bab);
    font-weight: 600;
    margin: 12px 0 6px;
  }
  .sec.row {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .modes {
    display: inline-flex;
    gap: 2px;
    background: var(--vance-bg, #17191d);
    border: 1px solid var(--border-strong, #3a4150);
    border-radius: 8px;
    padding: 2px;
    margin: 4px 0 4px;
  }
  .modes button {
    border: none;
    background: none;
    color: var(--muted, #949bab);
    font-size: 12px;
    padding: 4px 14px;
    border-radius: 6px;
    cursor: pointer;
  }
  .modes button.on {
    background: var(--accent, #4c9dff);
    color: #071522;
    font-weight: 600;
  }
  .code {
    width: 100%;
    box-sizing: border-box;
    min-height: 240px;
    resize: vertical;
    border-radius: 8px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--vance-bg, #17191d);
    color: #d7e0ea;
    font-family: ui-monospace, 'SF Mono', Menlo, monospace;
    font-size: 12.5px;
    line-height: 1.5;
    padding: 10px;
    tab-size: 2;
  }
  .hint {
    font-size: 11px;
    color: var(--muted, #949bab);
    margin-top: 6px;
    line-height: 1.5;
  }
  .mono {
    font-family: ui-monospace, 'SF Mono', Menlo, monospace;
    background: var(--panel-2, #262a31);
    padding: 0 4px;
    border-radius: 4px;
  }
  .mini {
    background: var(--panel-2, #262a31);
    border: 1px solid var(--border-strong, #3a4150);
    color: var(--fg, #dfe3e9);
    border-radius: 6px;
    font-size: 11px;
    padding: 2px 8px;
    cursor: pointer;
  }
  .param {
    display: grid;
    grid-template-columns: 1fr 64px 64px 64px auto;
    gap: 5px;
    margin-bottom: 5px;
    align-items: center;
  }
  .in.tiny,
  .in.num {
    margin: 0;
    padding: 5px 6px;
    font-size: 12px;
  }
  .rm {
    background: none;
    border: none;
    color: var(--muted, #949bab);
    cursor: pointer;
    font-size: 12px;
  }
  .rm:hover {
    color: var(--err, #f2686f);
  }
  .preview {
    position: relative;
    border: 1px solid var(--border, #30353d);
    border-radius: 8px;
    overflow: hidden;
    background: #000;
    aspect-ratio: 16 / 9;
    display: flex;
    align-items: center;
    justify-content: center;
  }
  .preview canvas {
    display: block;
    width: 100%;
    height: 100%;
    object-fit: contain;
    image-rendering: auto;
  }
  .spin {
    position: absolute;
    bottom: 6px;
    right: 8px;
    font-size: 10px;
    color: var(--muted, #949bab);
    background: rgba(0, 0, 0, 0.5);
    padding: 1px 6px;
    border-radius: 4px;
  }
  .slider {
    display: grid;
    grid-template-columns: 90px 1fr 42px;
    gap: 8px;
    align-items: center;
    font-size: 12px;
    margin-top: 8px;
  }
  .slider .val {
    text-align: right;
    color: var(--muted, #949bab);
    font-variant-numeric: tabular-nums;
  }
  .slider input[type='range'] {
    accent-color: var(--accent, #4c9dff);
  }
  .err {
    margin-top: 10px;
    white-space: pre-wrap;
    word-break: break-word;
    font-size: 11px;
    color: var(--err, #f2686f);
    background: color-mix(in srgb, var(--err, #f2686f) 8%, transparent);
    border: 1px solid var(--err, #f2686f);
    border-radius: 8px;
    padding: 8px;
    max-height: 140px;
    overflow: auto;
  }
  .save {
    margin-top: 14px;
    width: 100%;
    padding: 9px;
    border-radius: 8px;
    border: none;
    background: var(--accent, #4c9dff);
    color: #071522;
    font-weight: 700;
    font-size: 13px;
    cursor: pointer;
  }
  .save:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
