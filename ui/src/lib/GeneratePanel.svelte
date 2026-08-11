<script lang="ts">
  /**
   * GOAT-gated — generative image synthesis. Drives the user's own,
   * externally-configured model (VANCE_IMAGEGEN_CMD, or `sd` +
   * VANCE_SD_MODEL) to synthesize an image from a prompt. The result lands
   * in the Browser as a still Image; drag it onto the timeline or wire it
   * into a Fusion MediaIn. Vance never bundles model weights — the model is
   * the user's to provide.
   */
  import { imageGenAvailable, generateImage } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { listen } from '@tauri-apps/api/event';
  import { toast } from '$lib/toast.svelte';

  let available = $state(false);
  let prompt = $state('');
  let negative = $state('');
  // SD-1.5's native size; larger is much slower + heavier on memory.
  let width = $state(512);
  let height = $state(512);
  let steps = $state(20);
  let seed = $state('');
  let showAdvanced = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let status = $state<string | null>(null);

  // Live generation progress (streamed from the backend on gen://progress).
  let phase = $state('');
  let pct = $state<number | null>(null);
  let elapsed = $state(0);

  $effect(() => {
    void imageGenAvailable()
      .then((v) => (available = v))
      .catch(() => {});
  });

  $effect(() => {
    let un: (() => void) | null = null;
    void listen<{ step: number; total: number; pct: number | null; detail: string; done: boolean }>(
      'gen://progress',
      (e) => {
        const p = e.payload;
        if (!p || p.done) return;
        pct = p.pct;
        phase = p.total > 0 ? `Step ${p.step} / ${p.total}` : p.detail.replace('[vance-imagegen] ', '');
      }
    ).then((u) => (un = u));
    return () => un?.();
  });

  $effect(() => {
    if (!busy) return;
    const started = Date.now();
    elapsed = 0;
    const t = setInterval(() => (elapsed = Math.floor((Date.now() - started) / 1000)), 1000);
    return () => clearInterval(t);
  });

  function fmt(s: number): string {
    const m = Math.floor(s / 60);
    return `${m}:${String(s % 60).padStart(2, '0')}`;
  }

  async function generate() {
    if (!projectState.info) {
      toast.error('Create or open a project first (New in the toolbar), then generate.');
      return;
    }
    if (!prompt.trim()) {
      toast.error('Type a description of the image you want first.');
      return;
    }
    busy = true;
    error = null;
    phase = 'Getting ready…';
    pct = null;
    status = null;
    try {
      const seedNum = seed.trim() === '' ? null : parseInt(seed, 10);
      const info = await generateImage({
        prompt,
        negative: negative || null,
        width,
        height,
        steps,
        seed: Number.isFinite(seedNum as number) ? seedNum : null
      });
      await projectState.refresh();
      toast.success(`Created “${info.name}”. It’s in the Browser — drag it onto the timeline.`);
      status = null;
    } catch (e) {
      toast.error("Couldn't create the image.\n" + String(e));
      status = null;
    } finally {
      busy = false;
    }
  }
</script>

<div class="panel">
  <div class="head">Generate Image</div>

  {#if !available}
    <div class="hint">
      Not set up yet. Add <strong>Image generation</strong> from
      <strong>Settings → AI add-ons</strong> — Vance sets it up for you.
    </div>
  {/if}

  {#if busy}
    <div class="prog">
      <div class="prog-bar" class:indet={pct == null}>
        <i style={pct != null ? `width:${pct}%` : ''}></i>
      </div>
      <div class="prog-row">
        <span>{phase || 'Working…'}</span>
        <span class="mono">{fmt(elapsed)}</span>
      </div>
    </div>
  {/if}

  <textarea placeholder="Describe the image you want…" rows="2" bind:value={prompt} disabled={busy}></textarea>
  <textarea placeholder="Things to leave out (optional)" rows="1" bind:value={negative} disabled={busy}></textarea>

  <div class="row">
    <label>Width<input type="number" min="64" max="4096" step="8" bind:value={width} disabled={busy} /></label>
    <label>Height<input type="number" min="64" max="4096" step="8" bind:value={height} disabled={busy} /></label>
  </div>

  <button class="adv-toggle" onclick={() => (showAdvanced = !showAdvanced)}>
    {showAdvanced ? '▾' : '▸'} Advanced
  </button>
  {#if showAdvanced}
    <div class="row">
      <label title="Higher = more detail, but slower">
        Detail<input type="number" min="1" max="150" bind:value={steps} disabled={busy} /></label>
      <label class="seed" title="Leave blank for a new image each time">
        Variation<input type="text" placeholder="random" bind:value={seed} disabled={busy} /></label>
    </div>
  {/if}

  <button class="gen" onclick={generate} disabled={busy || !available}>✨ Generate</button>
</div>

<style>
  .panel {
    display: flex;
    flex-direction: column;
    gap: 5px;
    padding: 6px;
    font-size: 12px;
    color: var(--fg, #ddd);
  }
  .adv-toggle {
    align-self: flex-start;
    background: none;
    border: none;
    color: var(--muted, #8a8a92);
    font-size: 11px;
    cursor: pointer;
    padding: 2px 0;
  }
  .adv-toggle:hover {
    color: var(--fg, #ddd);
  }
  button.gen {
    margin-top: 2px;
    padding: 6px 12px;
    border-radius: 6px;
    border: none;
    background: var(--accent, #4c9dff);
    color: #071522;
    font-weight: 600;
    cursor: pointer;
  }
  button.gen:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .head {
    font-weight: 600;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 4px;
  }
  textarea {
    resize: vertical;
    width: 100%;
    box-sizing: border-box;
    background: var(--panel-bg, #1b1b1b);
    color: var(--fg, #ddd);
    border: 1px solid var(--border, #333);
    border-radius: 3px;
    font: inherit;
  }
  .row {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .row label {
    display: flex;
    align-items: center;
    gap: 3px;
    opacity: 0.85;
  }
  .row input[type='number'] {
    width: 60px;
  }
  .seed {
    flex: 1;
  }
  .seed input {
    flex: 1;
    min-width: 0;
  }
  button {
    cursor: pointer;
    margin-left: auto;
  }
  .hint {
    opacity: 0.55;
    font-size: 10px;
  }
  code {
    background: #2a2a2a;
    padding: 0 3px;
    border-radius: 2px;
  }
  .err {
    color: #ff6b6b;
    font-size: 11px;
  }
  .status {
    color: var(--ok);
    font-size: 11px;
  }
  .prog {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .prog-bar {
    height: 6px;
    border-radius: 999px;
    background: #2a2b31;
    overflow: hidden;
    position: relative;
  }
  .prog-bar > i {
    display: block;
    height: 100%;
    background: var(--accent);
    border-radius: 999px;
    transition: width 0.3s ease;
  }
  .prog-bar.indet > i {
    width: 34%;
    position: absolute;
    left: 0;
    animation: gen-slide 1.25s ease-in-out infinite;
  }
  @keyframes gen-slide {
    0% {
      left: -34%;
    }
    100% {
      left: 100%;
    }
  }
  .prog-row {
    display: flex;
    justify-content: space-between;
    font-size: 10px;
    opacity: 0.8;
  }
  .mono {
    font-family: ui-monospace, 'SF Mono', Menlo, monospace;
    font-variant-numeric: tabular-nums;
  }
  @media (prefers-reduced-motion: reduce) {
    .prog-bar.indet > i {
      animation: none;
      width: 100%;
      position: static;
    }
  }
</style>
