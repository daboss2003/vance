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
  import { toast } from '$lib/toast.svelte';

  let outputPath = $state('vance-export.mp4');
  let preset = $state<EncoderPresetKey>('H264');
  let width = $state<number | null>(1920);
  let height = $state<number | null>(1080);
  let startSeconds = $state<number | null>(null);
  let endSeconds = $state<number | null>(null);
  let crf = $state<number | null>(23);
  let hdr = $state(false);

  let rendering = $state(false);
  let lastOutcome = $state<RenderOutcome | null>(null);

  /** Pretty label for each preset key. */
  const PRESETS: { value: EncoderPresetKey; label: string }[] = [
    { value: 'H264', label: 'Web & social — H.264 (fast)' },
    { value: 'Hevc', label: 'Smaller file — HEVC / H.265 (fast)' },
    { value: 'Av1', label: 'Smallest file — AV1' },
    { value: 'ProRes422HQ', label: 'High-quality master — ProRes 422 HQ' },
    { value: 'ProRes422', label: 'Master — ProRes 422' },
    { value: 'ProRes4444', label: 'Master with transparency — ProRes 4444' },
    { value: 'DnxhrHq', label: 'Editing / archival — DNxHR HQ' },
    { value: 'DnxhrSq', label: 'Editing — DNxHR SQ' },
    { value: 'DnxhrLb', label: 'Editing (light) — DNxHR LB' }
  ];

  const needsCrf = $derived(preset === 'H264' || preset === 'Hevc' || preset === 'Av1');

  async function doRender() {
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
        end_seconds: endSeconds,
        hdr
      });
      toast.success('Export finished — saved to ' + lastOutcome.output_path);
    } catch (e) {
      toast.error('Export failed.\n' + String(e));
    } finally {
      rendering = false;
    }
  }
</script>

<div class="border-vance-border rounded border bg-vance-bg p-2 text-[11px]">
  <div class="text-vance-muted mb-1 uppercase tracking-wider">Export</div>

  <label class="mb-1 flex items-center gap-1">
    <span class="text-vance-muted w-12">Save to</span>
    <input
      type="text"
      bind:value={outputPath}
      class="border-vance-border flex-1 rounded border bg-vance-panel px-1 font-mono text-[10px]"
      placeholder="my-video.mp4"
    />
  </label>

  <label class="mb-1 flex items-center gap-1">
    <span class="text-vance-muted w-12">Format</span>
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
    <label
      class="mb-1 flex items-center gap-1"
      title="Lower number = higher quality (and larger file)"
    >
      <span class="text-vance-muted w-12">Quality</span>
      <input
        type="number"
        bind:value={crf}
        min="0"
        max="63"
        class="border-vance-border w-16 rounded border bg-vance-panel px-1 font-mono text-[10px]"
      />
      <span class="text-vance-muted text-[10px]">lower = better</span>
    </label>
  {/if}

  <label
    class="mb-1 flex items-center gap-1"
    title="Export in HDR — brighter highlights and richer color (HEVC 10-bit)."
  >
    <span class="text-vance-muted w-12">HDR</span>
    <input type="checkbox" bind:checked={hdr} />
  </label>

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
