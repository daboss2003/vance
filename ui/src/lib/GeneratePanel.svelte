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

  let available = $state(false);
  let prompt = $state('');
  let negative = $state('');
  let width = $state(1024);
  let height = $state(576);
  let steps = $state(20);
  let seed = $state('');
  let busy = $state(false);
  let error = $state<string | null>(null);
  let status = $state<string | null>(null);

  $effect(() => {
    void imageGenAvailable()
      .then((v) => (available = v))
      .catch(() => {});
  });

  async function generate() {
    if (!prompt.trim()) {
      error = 'Enter a prompt.';
      return;
    }
    busy = true;
    error = null;
    status = 'Generating… (model inference can take a while)';
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
      status = `Generated “${info.name}”. It’s in the Browser — drag it onto the timeline.`;
    } catch (e) {
      error = String(e);
      status = null;
    } finally {
      busy = false;
    }
  }
</script>

<div class="panel">
  <div class="head">Generate (AI Image)</div>
  {#if error}<div class="err">{error}</div>{/if}
  {#if status}<div class="status">{status}</div>{/if}

  {#if !available}
    <div class="hint">
      No image generator configured. Set <code>VANCE_IMAGEGEN_CMD</code> to a command
      template using <code>{'{prompt}'} {'{width}'} {'{height}'} {'{out}'}</code>, or install
      <code>sd</code> (stable-diffusion.cpp) and set <code>VANCE_SD_MODEL</code>. Vance runs it
      and imports the result.
    </div>
  {/if}

  <textarea placeholder="Prompt" rows="2" bind:value={prompt} disabled={busy}></textarea>
  <textarea placeholder="Negative prompt (optional)" rows="1" bind:value={negative} disabled={busy}></textarea>

  <div class="row">
    <label>W<input type="number" min="64" max="4096" step="8" bind:value={width} disabled={busy} /></label>
    <label>H<input type="number" min="64" max="4096" step="8" bind:value={height} disabled={busy} /></label>
    <label>steps<input type="number" min="1" max="150" bind:value={steps} disabled={busy} /></label>
  </div>
  <div class="row">
    <label class="seed">seed<input type="text" placeholder="random" bind:value={seed} disabled={busy} /></label>
    <button onclick={generate} disabled={busy || !available}>✨ Generate</button>
  </div>
  <div class="hint">img2img / inpaint from a clip frame is a planned follow-up.</div>
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
    color: #7ec699;
    font-size: 11px;
  }
</style>
