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

  import { tick } from 'svelte';
  import type { EncoderPresetKey } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { exportRun } from '$lib/export-run.svelte';
  import {
    exportEdge,
    exportSizeBasis,
    sequenceSize,
    sizeEdit,
    type SizeEdit
  } from '$lib/export-size';

  let outputPath = $state('vance-export.mp4');
  let preset = $state<EncoderPresetKey>('H264');
  // The size follows the sequence frame size (1920×1080 until the first clip
  // sets it) until the user types a different one (rules in export-size.ts).
  const seqWidth = $derived(sequenceSize(projectState.sequence)[0]);
  const seqHeight = $derived(sequenceSize(projectState.sequence)[1]);
  const sizeBasis = $derived(exportSizeBasis(projectState.projectEpoch, projectState.sequence));
  // Intentional: writable $derived. Typing assigns a value; any change of the
  // basis (project opened or created, sequence switched, frame size changed)
  // re-derives it to `undefined`, so the export follows the sequence again.
  let widthEdit = $derived.by<SizeEdit>(() => {
    void sizeBasis;
    return undefined;
  });
  let heightEdit = $derived.by<SizeEdit>(() => {
    void sizeBasis;
    return undefined;
  });
  const width = $derived(exportEdge(widthEdit, seqWidth));
  const height = $derived(exportEdge(heightEdit, seqHeight));
  let startSeconds = $state<number | null>(null);
  let endSeconds = $state<number | null>(null);
  let crf = $state<number | null>(23);
  let hdr = $state(false);

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

  // One export at a time, app-wide (export-run.svelte.ts): the run outlives
  // this panel, which unmounts when the dock switches.
  const busy = $derived(exportRun.phase !== 'idle');
  let renderBtn = $state<HTMLButtonElement | null>(null);
  let cancelBtn = $state<HTMLButtonElement | null>(null);

  function doRender() {
    void exportRun.start({
      output_path: outputPath,
      preset,
      crf: needsCrf ? crf : null,
      width,
      height,
      start_seconds: startSeconds,
      end_seconds: endSeconds,
      hdr
    });
  }

  async function doCancel() {
    const hadFocus = document.activeElement === cancelBtn;
    await exportRun.cancel();
    const run = exportRun.current;
    if (run) await run;
    // The Cancel button goes away with the export; don't strand its focus.
    await tick();
    if (hadFocus && renderBtn?.isConnected) renderBtn.focus();
  }
</script>

<div class="border-vance-border rounded border bg-vance-bg p-2 text-[11px]">
  <div class="text-vance-muted mb-1 uppercase tracking-wider">Export</div>

  <!-- Settings are fixed while an export runs (disabled fieldset). -->
  <fieldset class="min-w-0" disabled={busy}>
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
        bind:value={
          () => (widthEdit === undefined ? seqWidth : widthEdit),
          (v) => (widthEdit = sizeEdit(v, seqWidth))
        }
        min="2"
        class="border-vance-border w-16 rounded border bg-vance-panel px-1 font-mono text-[10px]"
        placeholder={String(seqWidth)}
        aria-label="Export width in pixels"
        title="Defaults to the sequence frame size"
      />
      <span class="text-vance-muted">×</span>
      <input
        type="number"
        bind:value={
          () => (heightEdit === undefined ? seqHeight : heightEdit),
          (v) => (heightEdit = sizeEdit(v, seqHeight))
        }
        min="2"
        class="border-vance-border w-16 rounded border bg-vance-panel px-1 font-mono text-[10px]"
        placeholder={String(seqHeight)}
        aria-label="Export height in pixels"
        title="Defaults to the sequence frame size"
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
  </fieldset>

  <div class="flex gap-1">
    <button
      bind:this={renderBtn}
      type="button"
      class="bg-vance-accent text-vance-bg flex-1 rounded px-2 py-1 text-[11px] font-medium disabled:opacity-50"
      onclick={doRender}
      disabled={busy || !outputPath}
    >
      {busy ? 'Rendering…' : 'Render'}
    </button>
    {#if busy}
      <!-- aria-disabled, not disabled, while cancelling: a focused button
           that turns disabled drops keyboard focus. -->
      <button
        bind:this={cancelBtn}
        type="button"
        class="border-vance-border rounded border bg-vance-panel px-2 py-1 text-[11px] hover:brightness-125 aria-disabled:cursor-default aria-disabled:opacity-50 aria-disabled:hover:brightness-100"
        aria-disabled={exportRun.phase === 'cancelling'}
        onclick={doCancel}
      >
        {exportRun.phase === 'cancelling' ? 'Cancelling…' : 'Cancel'}
      </button>
    {/if}
  </div>

  <div role="status" class="text-vance-muted mt-1 text-[10px]">
    {#if busy}
      Exporting. Cancel stops it without writing a file.
    {:else if exportRun.lastCancelled}
      Export cancelled. No file was written.
    {:else if exportRun.lastEndedUnseen}
      The export that was running has ended.
    {:else if exportRun.lastOutcome}
      {@const o = exportRun.lastOutcome}
      <div class="font-mono">
        <div>→ {o.output_path}</div>
        <div>
          {o.video_frames} frames · {o.audio_samples} samples ·
          {o.elapsed_seconds.toFixed(2)} s
        </div>
      </div>
    {/if}
  </div>
</div>
