<script lang="ts">
  /**
   * GOAT-8 — AI audio: one-click dialogue leveling (BS.1770 loudness) for
   * the selected clip(s), and voice isolation (Demucs) for a media item.
   */
  import { levelClip, voiceIsolationAvailable, isolateVoice } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { selectionState } from '$lib/selection.svelte';

  let targetLufs = $state(-16);
  let busy = $state(false);
  let status = $state<string | null>(null);
  let error = $state<string | null>(null);

  let isolateAvail = $state(false);
  let isolateMediaId = $state('');

  let mediaChoices = $derived(
    projectState.media.filter((m) => m.kind === 'video' || m.kind === 'audio')
  );

  $effect(() => {
    void voiceIsolationAvailable().then((v) => (isolateAvail = v));
  });

  /** Resolve a selected clip id to its (trackId, clipId). */
  function trackForClip(clipId: string): string | null {
    const tracks = projectState.sequence?.tracks ?? [];
    for (const t of tracks) {
      if (t.clips.some((c) => c.id === clipId)) return t.id;
    }
    return null;
  }

  async function levelSelected() {
    const ids = selectionState.ids;
    if (ids.length === 0) {
      error = 'Select one or more clips on the timeline first.';
      return;
    }
    busy = true;
    error = null;
    status = `Analyzing loudness of ${ids.length} clip(s)…`;
    let done = 0;
    try {
      for (const clipId of ids) {
        const trackId = trackForClip(clipId);
        if (!trackId) continue;
        try {
          await levelClip(trackId, clipId, targetLufs);
          done++;
        } catch {
          // Clip without audio → skip.
        }
      }
      await projectState.refresh();
      status = `Leveled ${done} clip(s) to ${targetLufs} LUFS.`;
    } catch (e) {
      error = String(e);
      status = null;
    } finally {
      busy = false;
    }
  }

  async function runIsolate() {
    if (!isolateMediaId) return;
    busy = true;
    error = null;
    status = 'Isolating vocals… (this can take a while)';
    try {
      const info = await isolateVoice(isolateMediaId);
      await projectState.refresh();
      status = `Imported isolated stem: ${info.name}`;
    } catch (e) {
      error = String(e);
      status = null;
    } finally {
      busy = false;
    }
  }
</script>

<div class="panel">
  <div class="head">AI Audio</div>
  {#if error}<div class="err">{error}</div>{/if}
  {#if status}<div class="status">{status}</div>{/if}

  <div class="section">
    <div class="lbl">Dialogue leveling</div>
    <div class="row">
      <span>Target</span>
      <input type="number" step="1" min="-40" max="0" bind:value={targetLufs} disabled={busy} />
      <span>LUFS</span>
      <button disabled={busy} onclick={levelSelected}>Level selected</button>
    </div>
    <div class="hint">Normalizes each selected clip's perceived loudness to the target.</div>
  </div>

  <div class="section">
    <div class="lbl">Voice isolation</div>
    {#if !isolateAvail}
      <div class="hint">Install Demucs to enable: <code>pip install -U demucs</code></div>
    {/if}
    <div class="row">
      <select bind:value={isolateMediaId} disabled={busy || !isolateAvail}>
        <option value="">— media —</option>
        {#each mediaChoices as m (m.id)}
          <option value={m.id}>{m.name}</option>
        {/each}
      </select>
      <button disabled={busy || !isolateAvail || !isolateMediaId} onclick={runIsolate}>Isolate</button>
    </div>
    <div class="hint">Separates vocals from background; imports the vocal stem.</div>
  </div>
</div>

<style>
  .panel {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 6px;
    font-size: 12px;
    color: var(--fg, #ddd);
  }
  .head {
    font-weight: 600;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 4px;
  }
  .section {
    border-top: 1px solid var(--border, #2a2a2a);
    padding-top: 4px;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .lbl {
    font-weight: 600;
    opacity: 0.85;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 5px;
  }
  .row input[type='number'] {
    width: 52px;
  }
  .row select {
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
    color: #7ec699;
    font-size: 11px;
  }
</style>
