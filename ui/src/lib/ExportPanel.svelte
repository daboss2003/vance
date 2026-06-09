<script lang="ts">
  /**
   * Phase 8 — sequence export panel.
   *
   * Renders a small form: output path + preset dropdown + range +
   * Render button. Calls the backend's `render_sequence` Tauri
   * command and shows the resulting frame / sample / wall-clock
   * totals.
   *
   * v1 ships the synthetic-test-pattern path on the backend so the
   * file content is a moving stripe; format, duration, audio
   * channels + sample rate are all correct. Renderer-to-encoder
   * offscreen wgpu readback for real sequence content lands in 8.5.
   */

  import { renderSequence, type EncoderPresetKey, type RenderOutcome } from '$lib/ipc';

  let outputPath = $state('/tmp/vance-export.mov');
  let preset = $state<EncoderPresetKey>('ProRes422HQ');
  let width = $state<number | null>(1920);
  let height = $state<number | null>(1080);
  let startSeconds = $state<number | null>(null);
  let endSeconds = $state<number | null>(null);
  let crf = $state<number | null>(23);

  let rendering = $state(false);
  let lastOutcome = $state<RenderOutcome | null>(null);
  let error = $state<string | null>(null);

  /** Pretty label for each preset key. */
  const PRESETS: { value: EncoderPresetKey; label: string }[] = [
    { value: 'ProRes422HQ', label: 'ProRes 422 HQ' },
    { value: 'ProRes422', label: 'ProRes 422' },
    { value: 'ProRes4444', label: 'ProRes 4444 (alpha)' },
    { value: 'DnxhrHq', label: 'DNxHR HQ' },
    { value: 'DnxhrSq', label: 'DNxHR SQ' },
    { value: 'DnxhrLb', label: 'DNxHR LB' },
    { value: 'H264', label: 'H.264 (hardware)' },
    { value: 'Hevc', label: 'HEVC / H.265 (hardware)' },
    { value: 'Av1', label: 'AV1 (svt-av1)' }
  ];

  const needsCrf = $derived(preset === 'H264' || preset === 'Hevc' || preset === 'Av1');

  async function doRender() {
    error = null;
    rendering = true;
    lastOutcome = null;
    try {
      lastOutcome = await renderSequence({
        output_path: outputPath,
        preset,
        crf: needsCrf ? crf : null,
        width,
        height,
        start_seconds: startSeconds,
        end_seconds: endSeconds
      });
    } catch (e) {
      error = String(e);
    } finally {
      rendering = false;
    }
  }
</script>

<div class="border-vance-border rounded border bg-vance-bg p-2 text-[11px]">
  <div class="text-vance-muted mb-1 uppercase tracking-wider">Export</div>

  <label class="mb-1 flex items-center gap-1">
    <span class="text-vance-muted w-12">Out</span>
    <input
      type="text"
      bind:value={outputPath}
      class="border-vance-border flex-1 rounded border bg-vance-panel px-1 font-mono text-[10px]"
      placeholder="/path/to/out.mov"
    />
  </label>

  <label class="mb-1 flex items-center gap-1">
    <span class="text-vance-muted w-12">Preset</span>
    <select
      bind:value={preset}
      class="border-vance-border flex-1 rounded border bg-vance-panel px-1 text-[10px]"
    >
      {#each PRESETS as p}
        <option value={p.value}>{p.label}</option>
      {/each}
    </select>
  </label>

  {#if needsCrf}
    <label class="mb-1 flex items-center gap-1">
      <span class="text-vance-muted w-12">CRF</span>
      <input
        type="number"
        bind:value={crf}
        min="0"
        max="63"
        class="border-vance-border w-16 rounded border bg-vance-panel px-1 font-mono text-[10px]"
      />
    </label>
  {/if}

  <div class="mb-1 flex items-center gap-1">
    <span class="text-vance-muted w-12">Size</span>
    <input
      type="number"
      bind:value={width}
      min="2"
      class="border-vance-border w-16 rounded border bg-vance-panel px-1 font-mono text-[10px]"
      placeholder="W"
    />
    <span class="text-vance-muted">×</span>
    <input
      type="number"
      bind:value={height}
      min="2"
      class="border-vance-border w-16 rounded border bg-vance-panel px-1 font-mono text-[10px]"
      placeholder="H"
    />
  </div>

  <div class="mb-1 flex items-center gap-1">
    <span class="text-vance-muted w-12">Range</span>
    <input
      type="number"
      bind:value={startSeconds}
      step="0.1"
      min="0"
      class="border-vance-border w-16 rounded border bg-vance-panel px-1 font-mono text-[10px]"
      placeholder="start s"
    />
    <span class="text-vance-muted">→</span>
    <input
      type="number"
      bind:value={endSeconds}
      step="0.1"
      min="0"
      class="border-vance-border w-16 rounded border bg-vance-panel px-1 font-mono text-[10px]"
      placeholder="end s"
    />
  </div>

  <button
    type="button"
    class="bg-vance-accent text-vance-bg w-full rounded px-2 py-1 text-[11px] font-medium disabled:opacity-50"
    onclick={doRender}
    disabled={rendering || !outputPath}
  >
    {rendering ? 'Rendering…' : 'Render'}
  </button>

  {#if error}
    <div class="mt-1 text-[10px] text-red-400">{error}</div>
  {/if}

  {#if lastOutcome}
    <div class="text-vance-muted mt-1 font-mono text-[10px]">
      <div>→ {lastOutcome.output_path}</div>
      <div>
        {lastOutcome.video_frames} frames · {lastOutcome.audio_samples} samples ·
        {lastOutcome.elapsed_seconds.toFixed(2)} s
      </div>
    </div>
  {/if}
</div>
