<script lang="ts">
  /**
   * GOAT-8 — AI audio: one-click dialogue leveling (BS.1770 loudness) for
   * the selected clip(s), and voice isolation (Demucs) for a media item.
   */
  import { levelClip, voiceIsolationAvailable, isolateVoice } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { selectionState } from '$lib/selection.svelte';
  import { toast } from '$lib/toast.svelte';
  import { lockedTrackMessage, splitByLock } from '$lib/track-lock';

  let targetLufs = $state(-16);
  let busy = $state(false);

  let isolateAvail = $state(false);
  let isolateMediaId = $state('');

  let mediaChoices = $derived(
    projectState.media.filter((m) => m.kind === 'video' || m.kind === 'audio')
  );

  $effect(() => {
    void voiceIsolationAvailable().then((v) => (isolateAvail = v));
  });

  async function levelSelected() {
    const ids = selectionState.ids;
    if (ids.length === 0) {
      toast.error('Select one or more clips on the timeline first.');
      return;
    }
    // Leveling sets clip gain, which a locked track refuses: skip those.
    const { editable, locked } = splitByLock(projectState.sequence?.tracks ?? [], ids);
    if (locked.length > 0) toast.info(lockedTrackMessage(locked[0].track.name));
    if (editable.length === 0) return;
    busy = true;
    let done = 0;
    try {
      for (const { trackId, clipId } of editable) {
        try {
          await levelClip(trackId, clipId, targetLufs);
          done++;
        } catch {
          // Clip without audio → skip.
        }
      }
      await projectState.refresh();
      toast.success(`Balanced the loudness of ${done} clip(s).`);
    } catch (e) {
      toast.error('Loudness balancing failed.\n' + String(e));
    } finally {
      busy = false;
    }
  }

  async function runIsolate() {
    if (!isolateMediaId) return;
    busy = true;
    toast.info('Isolating the voice… this can take a while.');
    try {
      const info = await isolateVoice(isolateMediaId);
      await projectState.refresh();
      toast.success(`Added the isolated vocal track: ${info.name}`);
    } catch (e) {
      toast.error('Voice isolation failed.\n' + String(e));
    } finally {
      busy = false;
    }
  }
</script>

<div class="panel">
  <div class="head">Voice & Levels</div>

  <div class="section">
    <div class="lbl">Balance loudness</div>
    <div class="row">
      <span>Target</span>
      <input type="number" step="1" min="-40" max="0" bind:value={targetLufs} disabled={busy} />
      <span title="Loudness units. −16 is a good target for YouTube.">LUFS</span>
      <button disabled={busy} onclick={levelSelected}>Balance selected</button>
    </div>
    <div class="hint">Evens out the loudness of the selected clips. −16 LUFS suits YouTube.</div>
  </div>

  <div class="section">
    <div class="lbl">Isolate voice</div>
    {#if !isolateAvail}
      <div class="hint">Needs the voice-isolation add-on. Set it up in Settings → AI add-ons.</div>
    {/if}
    <div class="row">
      <select bind:value={isolateMediaId} disabled={busy || !isolateAvail}>
        <option value="">Choose a clip…</option>
        {#each mediaChoices as m (m.id)}
          <option value={m.id}>{m.name}</option>
        {/each}
      </select>
      <button disabled={busy || !isolateAvail || !isolateMediaId} onclick={runIsolate}
        >Isolate</button
      >
    </div>
    <div class="hint">Separates the voice from background audio and adds it as a new track.</div>
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
  .hint {
    opacity: 0.5;
    font-size: 10px;
  }
</style>
