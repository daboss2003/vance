<script lang="ts">
  /** Renders the app's modal dialog stack (see dialog.svelte.ts). Mounted once
   *  at the root so any choice/explainer surfaces as a centered dialog. */
  import { dialogs, closeDialog } from '$lib/dialog.svelte';
</script>

{#each dialogs as d (d.id)}
  <div
    class="scrim"
    role="dialog"
    aria-modal="true"
    aria-label={d.title}
    onclick={(e) => {
      if (e.target === e.currentTarget) closeDialog(d.id, null);
    }}
  >
    <div class="card">
      <h2>{d.title}</h2>
      {#if d.body}<p class="body">{d.body}</p>{/if}
      {#if d.bullets && d.bullets.length}
        <ul class="bullets">
          {#each d.bullets as b}<li>{b}</li>{/each}
        </ul>
      {/if}
      <div class="actions">
        {#each d.actions as a}
          <button
            class="btn {a.kind ?? 'default'}"
            onclick={() => closeDialog(d.id, a.value)}
          >
            {a.label}
          </button>
        {/each}
      </div>
    </div>
  </div>
{/each}

<style>
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
    width: 460px;
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
    margin: 0 0 8px;
  }
  .body {
    margin: 0 0 10px;
    font-size: 13px;
    line-height: 1.5;
    color: var(--fg, #dfe3e9);
    white-space: pre-line;
    opacity: 0.92;
  }
  .bullets {
    margin: 0 0 12px;
    padding-left: 18px;
    font-size: 13px;
    line-height: 1.55;
    color: var(--muted, #949bab);
  }
  .bullets li {
    margin-bottom: 3px;
  }
  .actions {
    display: flex;
    flex-wrap: wrap;
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
  .btn:hover {
    border-color: var(--accent, #4c9dff);
  }
  .btn.primary {
    background: var(--accent, #4c9dff);
    border-color: var(--accent, #4c9dff);
    color: #071522;
    font-weight: 600;
  }
  .btn.danger {
    border-color: var(--err, #f2686f);
    color: var(--err, #f2686f);
  }
  .btn:focus-visible {
    outline: 2px solid var(--accent, #4c9dff);
    outline-offset: 2px;
  }
</style>
