<script lang="ts">
  /**
   * GOAT-6 — magnetic timeline + clip connections (Cut-page-style).
   * Toggle magnetic mode (clip drags ripple + carry connected clips), and
   * connect / disconnect / ripple-delete using the timeline selection.
   */
  import { projectState } from '$lib/project.svelte';
  import { selectionState } from '$lib/selection.svelte';
  import { toast } from '$lib/toast.svelte';
  import { lockedTrackMessage, splitByLock } from '$lib/track-lock';

  let busy = $state(false);

  const selCount = $derived(selectionState.count);

  async function act(fn: () => Promise<unknown>) {
    busy = true;
    try {
      await fn();
    } finally {
      busy = false;
    }
  }

  function toggleMagnetic(on: boolean) {
    void act(() => projectState.setMagnetic(on));
  }

  // Connect: first selected = anchor, second = connected.
  function connect() {
    const ids = selectionState.ids;
    if (ids.length !== 2) return;
    void act(() => projectState.connectClip(ids[0], ids[1]));
  }

  function disconnectSelected() {
    const ids = [...selectionState.ids];
    void act(async () => {
      for (const id of ids) await projectState.disconnectClip(id);
    });
  }

  function rippleDeleteSelected() {
    // Clips on locked tracks are skipped (the engine refuses them).
    const { editable, locked } = splitByLock(projectState.sequence?.tracks ?? [], [
      ...selectionState.ids
    ]);
    if (locked.length > 0) toast.info(lockedTrackMessage(locked[0].track.name));
    void act(async () => {
      for (const { trackId, clipId } of editable)
        await projectState.rippleDeleteClip(trackId, clipId);
    });
  }
</script>

<div class="panel">
  <div class="head">
    <span>Magnetic Timeline</span>
    <label class="toggle">
      <input
        type="checkbox"
        checked={projectState.magnetic}
        disabled={busy}
        onchange={(e) => toggleMagnetic((e.target as HTMLInputElement).checked)}
      />
      <span>{projectState.magnetic ? 'On' : 'Off'}</span>
    </label>
  </div>

  <div class="row">
    <button
      onclick={connect}
      disabled={busy || selCount !== 2}
      title="Select an anchor clip then a clip to attach"
    >
      Connect (2)
    </button>
    <button onclick={disconnectSelected} disabled={busy || selCount === 0}>Disconnect</button>
  </div>
  <div class="row">
    <button onclick={rippleDeleteSelected} disabled={busy || selCount === 0}>Ripple delete</button>
  </div>

  <div class="hint">
    {#if projectState.magnetic}
      Dragging a clip ripples the timeline and carries its connected clips.
    {:else}
      Off: classic per-track moves. Connections still apply under ripple ops.
    {/if}
    <br />
    Connect: select the <em>anchor</em> first, then the clip to attach.
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
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-weight: 600;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 4px;
  }
  .toggle {
    display: flex;
    align-items: center;
    gap: 4px;
    font-weight: 400;
    cursor: pointer;
  }
  .row {
    display: flex;
    gap: 6px;
  }
  .row button {
    flex: 1;
    cursor: pointer;
    font-size: 11px;
  }
  .hint {
    opacity: 0.55;
    font-size: 10px;
    line-height: 1.4;
  }
</style>
