<script lang="ts">
  /**
   * Shown above controls that edit a locked track's clip (inspector, Color
   * page): says why they're disabled and offers the unlock. The engine
   * refuses those edits on a locked track (lock policy in track-lock.ts).
   */

  import { tick } from 'svelte';
  import type { TrackContents } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { lastInputWasKeyboard } from '$lib/keys';
  import { exportRun } from '$lib/export-run.svelte';
  import { EXPORT_BUSY_HINT } from '$lib/export-busy';
  import { focusAfterUnlock, lockActionLabel, LOCKED_EDIT_HINT } from '$lib/track-lock';

  let { track }: { track: TrackContents } = $props();

  let unlocking = $state(false);

  async function unlock() {
    if (unlocking) return;
    const viaKeyboard = lastInputWasKeyboard();
    const trackId = track.id;
    unlocking = true;
    const ok = await projectState.setTrackLocked(trackId, false);
    unlocking = false;
    if (!ok) return;
    // This notice goes away with the lock (and with it a keyboard user's
    // focus); see focusAfterUnlock for where focus lands.
    await tick();
    const lockButton = document.querySelector<HTMLElement>(
      `[data-track-lock="${CSS.escape(trackId)}"]`
    );
    focusAfterUnlock(viaKeyboard, lockButton)?.focus();
  }
</script>

<div
  class="border-vance-border mb-3 flex items-start gap-2 rounded border bg-vance-bg px-2 py-1.5 text-[11px]"
>
  <svg
    class="mt-0.5 h-3 w-3 shrink-0 text-[var(--color-vance-accent)]"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2.5"
    stroke-linecap="round"
    aria-hidden="true"
  >
    <rect x="4" y="11" width="16" height="10" rx="2" fill="currentColor" />
    <path d="M7.5 11V7a4.5 4.5 0 0 1 9 0v4" />
  </svg>
  <p class="text-vance-muted min-w-0 flex-1 leading-snug">
    Track <span class="text-vance-fg">{track.name}</span> is locked. {LOCKED_EDIT_HINT}
  </p>
  <button
    type="button"
    class="rounded bg-vance-panel border-vance-border shrink-0 border px-1.5 py-0.5 text-[10px] hover:brightness-125 disabled:opacity-50"
    disabled={unlocking || exportRun.busy}
    title={exportRun.busy ? EXPORT_BUSY_HINT : undefined}
    onclick={unlock}
  >
    {lockActionLabel(true)}
  </button>
</div>
