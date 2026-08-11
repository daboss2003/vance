<script lang="ts">
  /**
   * GOAT-9 — Smart Reframe: track the subject in the selected clip and
   * write a keyframed transform that keeps it centered for a target aspect
   * (e.g. 16:9 → 9:16 vertical). Export at the target resolution to get the
   * reframed video. Uses the ZNCC tracker, so the clip must be open in the
   * viewer (play/scrub it once) before running.
   */
  import {
    smartReframe,
    magicMask,
    magicMaskAvailable,
    type TrackSummary
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { selectionState } from '$lib/selection.svelte';

  const PRESETS: Record<string, [number, number]> = {
    '9:16 vertical': [1080, 1920],
    '1:1 square': [1080, 1080],
    '4:5 portrait': [1080, 1350],
    '16:9 wide': [1920, 1080]
  };

  let preset = $state('9:16 vertical');
  let zoom = $state(1.0);
  let frames = $state(120);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let status = $state<string | null>(null);

  let maskAvail = $state(false);
  let maskMediaId = $state('');
  let mediaChoices = $derived(
    projectState.media.filter((m) => m.kind === 'video')
  );
  $effect(() => {
    void magicMaskAvailable().then((v) => (maskAvail = v));
  });

  async function runMask() {
    if (!maskMediaId) return;
    busy = true;
    error = null;
    status = 'Isolating subject… (this can take a while)';
    try {
      const info = await magicMask(maskMediaId);
      await projectState.refresh();
      status = `Imported isolated subject: ${info.name}. Key out the background to composite.`;
    } catch (e) {
      error = String(e);
      status = null;
    } finally {
      busy = false;
    }
  }

  /** Resolve the first selected clip to (trackId, clipId, sourceInSeconds). */
  function selectedClip(): { trackId: string; clipId: string; sourceIn: number } | null {
    const id = selectionState.ids[0];
    if (!id) return null;
    for (const t of projectState.sequence?.tracks ?? []) {
      const c = t.clips.find((c) => c.id === id);
      if (c) return { trackId: t.id, clipId: c.id, sourceIn: c.source_in_seconds };
    }
    return null;
  }

  async function run() {
    const sel = selectedClip();
    if (!sel) {
      error = 'Select a clip on the timeline first (and play it once so the decoder is open).';
      return;
    }
    const [w, h] = PRESETS[preset];
    busy = true;
    error = null;
    status = 'Tracking + reframing…';
    try {
      const s: TrackSummary = await smartReframe({
        trackId: sel.trackId,
        clipId: sel.clipId,
        targetW: w,
        targetH: h,
        zoom,
        startSourceSeconds: sel.sourceIn,
        nFrames: Math.max(2, Math.min(240, Math.round(frames))),
        seedCx: 0.5,
        seedCy: 0.5,
        patchHalf: 24,
        searchRadius: 48
      });
      await projectState.refresh();
      status = `Reframed ${s.frames_tracked} frames (mean match ${(s.mean_score * 100).toFixed(0)}%). Export at ${w}×${h}.`;
    } catch (e) {
      error = String(e);
      status = null;
    } finally {
      busy = false;
    }
  }
</script>

<div class="panel">
  <div class="head">Smart Reframe</div>
  {#if error}<div class="err">{error}</div>{/if}
  {#if status}<div class="status">{status}</div>{/if}
  <div class="row">
    <span>Target</span>
    <select bind:value={preset} disabled={busy}>
      {#each Object.keys(PRESETS) as p (p)}
        <option value={p}>{p}</option>
      {/each}
    </select>
  </div>
  <div class="row">
    <span>Zoom</span>
    <input type="number" step="0.05" min="1" max="4" bind:value={zoom} disabled={busy} />
    <span>Frames</span>
    <input type="number" step="10" min="2" max="240" bind:value={frames} disabled={busy} />
  </div>
  <button disabled={busy} onclick={run}>Reframe selected clip</button>
  <div class="hint">Tracks the subject (center of the clip) and pans to keep it framed.</div>

  <div class="mask">
    <div class="head">Magic Mask</div>
    {#if !maskAvail}
      <div class="hint">Install to enable: <code>pip install -U backgroundremover</code></div>
    {/if}
    <div class="row">
      <select bind:value={maskMediaId} disabled={busy || !maskAvail}>
        <option value="">— media —</option>
        {#each mediaChoices as m (m.id)}
          <option value={m.id}>{m.name}</option>
        {/each}
      </select>
      <button disabled={busy || !maskAvail || !maskMediaId} onclick={runMask}>Isolate</button>
    </div>
    <div class="hint">AI-removes the background; import the matted subject and key it over other tracks.</div>
  </div>
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
  .row {
    display: flex;
    align-items: center;
    gap: 5px;
  }
  .row select {
    flex: 1;
  }
  .row input[type='number'] {
    width: 56px;
  }
  button {
    cursor: pointer;
  }
  .mask {
    border-top: 1px solid var(--border, #2a2a2a);
    padding-top: 4px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .mask .head {
    border: none;
    padding: 0;
  }
  .mask select {
    flex: 1;
  }
  code {
    background: rgba(255, 255, 255, 0.08);
    padding: 0 3px;
    border-radius: 2px;
  }
  .hint {
    opacity: 0.5;
    font-size: 10px;
  }
  .err {
    color: #ff6b6b;
    font-size: 11px;
  }
  .status {
    color: var(--ok);
    font-size: 11px;
  }
</style>
