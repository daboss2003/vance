<script lang="ts">
  /**
   * Sequence frame size (M1, ADR 0007): a header button showing the current
   * canvas size that opens a dialog with presets and a custom size. Clips
   * whose shape differs are fitted inside the frame without stretching.
   *
   * `open` is bindable so the page can hide the native viewports while the
   * dialog is up (they would otherwise paint over it).
   */

  import { projectState } from '$lib/project.svelte';
  import { dialogFocus } from '$lib/dialog-focus';
  import { exportRun } from '$lib/export-run.svelte';
  import { EXPORT_BUSY_HINT } from '$lib/export-busy';

  let { open = $bindable(false) }: { open?: boolean } = $props();

  const uid = $props.id();
  const MIN_EDGE = 16;
  const MAX_EDGE = 16384;

  const PRESETS: { id: string; label: string; w: number; h: number }[] = [
    { id: '1920x1080', label: 'HD 1080p', w: 1920, h: 1080 },
    { id: '3840x2160', label: '4K UHD', w: 3840, h: 2160 },
    { id: '1080x1920', label: 'Vertical', w: 1080, h: 1920 },
    { id: '1080x1080', label: 'Square', w: 1080, h: 1080 },
    { id: '1280x720', label: 'HD 720p', w: 1280, h: 720 }
  ];

  const size = $derived(projectState.sequence?.frame_size ?? null);
  const buttonLabel = $derived(size ? `${size[0]} × ${size[1]}` : 'Auto');

  let choice = $state<string>(PRESETS[0].id);
  let customW = $state<number>(1920);
  let customH = $state<number>(1080);
  let saving = $state(false);

  const target = $derived.by<[number, number]>(() => {
    if (choice === 'custom') return [customW, customH];
    const p = PRESETS.find((x) => x.id === choice) ?? PRESETS[0];
    return [p.w, p.h];
  });
  const validEdge = (v: number) => Number.isInteger(v) && v >= MIN_EDGE && v <= MAX_EDGE;
  const customError = $derived(
    choice === 'custom' && !(validEdge(customW) && validEdge(customH))
      ? `Enter whole numbers from ${MIN_EDGE} to ${MAX_EDGE}.`
      : null
  );
  // The engine rounds odd edges down to even (`w & !1`), so compare what it
  // would store: 1921 × 1081 over 1920 × 1080 is no change.
  const evenDown = (v: number) => v - (v % 2);
  const unchanged = $derived(
    !!size && size[0] === evenDown(target[0]) && size[1] === evenDown(target[1])
  );

  function show() {
    if (exportRun.refusesEdit()) return;
    const match = size ? PRESETS.find((p) => p.w === size[0] && p.h === size[1]) : PRESETS[0];
    choice = match ? match.id : 'custom';
    customW = size?.[0] ?? 1920;
    customH = size?.[1] ?? 1080;
    open = true;
  }

  // Close if the sequence goes away under the dialog (new / opened project),
  // so the page doesn't keep its video viewports hidden.
  $effect(() => {
    if (open && !projectState.sequence) open = false;
  });

  async function apply(e: SubmitEvent) {
    e.preventDefault();
    if (customError || saving || unchanged) return;
    saving = true;
    const ok = await projectState.setFrameSize(target[0], target[1]);
    saving = false;
    if (ok) open = false;
  }
</script>

{#if projectState.sequence}
  <button
    type="button"
    class="bg-vance-bg border-vance-border shrink-0 rounded border px-1.5 py-0.5 font-mono text-[10px] hover:brightness-125 disabled:opacity-50"
    onclick={show}
    disabled={exportRun.busy}
    title={exportRun.busy ? EXPORT_BUSY_HINT : 'Frame size of this sequence. Click to change.'}
    aria-label="Frame size: {size ? `${size[0]} by ${size[1]}` : 'auto'}. Change frame size"
  >
    {buttonLabel}
  </button>
{/if}

{#if open && projectState.sequence}
  <div
    class="scrim"
    role="dialog"
    aria-modal="true"
    aria-labelledby="{uid}-title"
    tabindex="-1"
    use:dialogFocus
    onclick={(e) => {
      if (e.target === e.currentTarget && !saving) open = false;
    }}
    onkeydown={(e) => {
      // The dialog owns the keyboard: the app's global shortcuts (Esc clears
      // the selection, Backspace deletes clips, Space plays) must not fire
      // behind it.
      e.stopPropagation();
      if (e.key === 'Escape' && !saving) open = false;
    }}
  >
    <!-- novalidate: odd sizes are allowed (the engine rounds them down), so
         the inputs' step must not block the submit. -->
    <form class="card" novalidate onsubmit={apply}>
      <h2 id="{uid}-title">Frame size</h2>
      <p class="current">
        Current: <span class="font-mono"
          >{size ? `${size[0]} × ${size[1]}` : 'Auto (set by the first clip)'}</span
        >
      </p>

      <fieldset>
        <legend class="sr-only">Choose a frame size</legend>
        {#each PRESETS as p (p.id)}
          <label class="option">
            <input type="radio" name="{uid}-size" value={p.id} bind:group={choice} />
            <span class="name">{p.label}</span>
            <span class="dims font-mono">{p.w} × {p.h}</span>
          </label>
        {/each}
        <label class="option">
          <input type="radio" name="{uid}-size" value="custom" bind:group={choice} />
          <span class="name">Custom</span>
        </label>
        {#if choice === 'custom'}
          <div class="custom">
            <label>
              <span class="sr-only">Width</span>
              <input
                type="number"
                min={MIN_EDGE}
                max={MAX_EDGE}
                step="2"
                bind:value={customW}
                aria-describedby="{uid}-note"
                aria-invalid={!!customError}
              />
            </label>
            <span class="times" aria-hidden="true">×</span>
            <label>
              <span class="sr-only">Height</span>
              <input
                type="number"
                min={MIN_EDGE}
                max={MAX_EDGE}
                step="2"
                bind:value={customH}
                aria-describedby="{uid}-note"
                aria-invalid={!!customError}
              />
            </label>
            <span class="unit">px</span>
          </div>
          {#if customError}
            <p class="error" role="alert">{customError}</p>
          {/if}
        {/if}
      </fieldset>

      <p id="{uid}-note" class="note">
        Clips with a different shape are fitted inside the frame without stretching. Odd sizes round
        down to the nearest even number.
      </p>

      <div class="actions">
        <button type="button" class="btn" onclick={() => (open = false)} disabled={saving}>
          Cancel
        </button>
        <button
          type="submit"
          class="btn primary"
          disabled={saving || !!customError || unchanged}
          title={unchanged ? 'This is already the frame size.' : undefined}
        >
          {saving ? 'Applying…' : 'Apply'}
        </button>
      </div>
    </form>
  </div>
{/if}

<style>
  /* Same scrim + card as the app's dialogs (Dialogs.svelte). */
  .scrim:focus {
    outline: none;
  }
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 3500;
    background: rgba(0, 0, 0, 0.62);
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 20px;
  }
  .card {
    width: 380px;
    max-width: 92vw;
    max-height: 86vh;
    overflow: auto;
    background: var(--panel-bg, #1e2127);
    color: var(--fg, #dfe3e9);
    border: 1px solid var(--border, #30353d);
    border-radius: 12px;
    padding: 20px 22px;
    box-shadow: 0 24px 64px rgba(0, 0, 0, 0.55);
  }
  h2 {
    font-size: 16px;
    font-weight: 700;
    margin: 0 0 4px;
  }
  .current {
    margin: 0 0 12px;
    font-size: 12px;
    color: var(--muted, #949bab);
  }
  .current span {
    color: var(--fg, #dfe3e9);
  }
  fieldset {
    border: 0;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .option {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 6px 8px;
    border-radius: 6px;
    font-size: 13px;
    cursor: pointer;
  }
  .option:hover {
    background: var(--panel-2, #262a31);
  }
  .option input {
    accent-color: var(--accent, #4c9dff);
  }
  .name {
    flex: 1;
  }
  .dims {
    font-size: 12px;
    color: var(--muted, #949bab);
  }
  .custom {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 8px 0 32px;
  }
  .custom input {
    width: 84px;
    padding: 4px 6px;
    border-radius: 6px;
    border: 1px solid var(--border, #30353d);
    background: var(--bg, #17191d);
    color: var(--fg, #dfe3e9);
    font-size: 13px;
    font-family: ui-monospace, monospace;
  }
  .custom input[aria-invalid='true'] {
    border-color: var(--err, #f2686f);
  }
  .times,
  .unit {
    color: var(--muted, #949bab);
    font-size: 12px;
  }
  .error {
    margin: 6px 0 0 32px;
    font-size: 12px;
    color: var(--err, #f2686f);
  }
  .note {
    margin: 12px 0 0;
    font-size: 12px;
    line-height: 1.5;
    color: var(--muted, #949bab);
  }
  .actions {
    display: flex;
    gap: 8px;
    justify-content: flex-end;
    margin-top: 16px;
  }
  .btn {
    cursor: pointer;
    padding: 7px 14px;
    border-radius: 7px;
    border: 1px solid var(--border-strong, #3a4150);
    background: var(--panel-2, #262a31);
    color: var(--fg, #dfe3e9);
    font-size: 13px;
    font-weight: 500;
  }
  .btn:hover:not(:disabled) {
    border-color: var(--accent, #4c9dff);
  }
  .btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .btn.primary {
    background: var(--accent, #4c9dff);
    border-color: var(--accent, #4c9dff);
    color: #071522;
    font-weight: 600;
  }
  .btn:focus-visible {
    outline: 2px solid var(--accent, #4c9dff);
    outline-offset: 2px;
  }
</style>
