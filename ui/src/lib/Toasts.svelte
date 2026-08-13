<script lang="ts">
  /** Corner overlay that renders app-wide toasts (see toast.svelte.ts). Mounted
   *  once at the app root so any message/error surfaces in a readable card
   *  instead of being squeezed into a panel. */
  import { toasts, dismissToast } from '$lib/toast.svelte';

  const ICON: Record<string, string> = { info: 'ℹ', success: '✓', error: '⚠' };
</script>

<div class="wrap" aria-live="polite">
  {#each toasts as t (t.id)}
    <div class="toast {t.kind}" role={t.kind === 'error' ? 'alert' : 'status'}>
      <span class="ico">{ICON[t.kind]}</span>
      <span class="msg">{t.message}</span>
      <button class="x" onclick={() => dismissToast(t.id)} aria-label="Dismiss">✕</button>
    </div>
  {/each}
</div>

<style>
  .wrap {
    position: fixed;
    right: 16px;
    bottom: 16px;
    z-index: 3000;
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-width: min(420px, 92vw);
    max-height: calc(100vh - 32px);
    overflow-y: auto;
    pointer-events: none;
  }
  .toast {
    pointer-events: auto;
    display: flex;
    align-items: flex-start;
    gap: 9px;
    background: var(--panel-2, #262a31);
    color: var(--fg, #dfe3e9);
    border: 1px solid var(--border, #30353d);
    border-left: 3px solid var(--accent, #4c9dff);
    border-radius: 9px;
    padding: 11px 12px;
    font-size: 13px;
    line-height: 1.45;
    box-shadow: 0 10px 30px rgba(0, 0, 0, 0.45);
    animation: toast-in 0.18s ease-out;
  }
  .toast.error {
    border-left-color: var(--err, #f2686f);
  }
  .toast.success {
    border-left-color: var(--ok, #5fbf8f);
  }
  .ico {
    flex-shrink: 0;
    font-size: 13px;
    line-height: 1.4;
    color: var(--accent, #4c9dff);
  }
  .toast.error .ico {
    color: var(--err, #f2686f);
  }
  .toast.success .ico {
    color: var(--ok, #5fbf8f);
  }
  .msg {
    flex: 1;
    min-width: 0;
    white-space: pre-line;
    word-break: break-word;
  }
  .x {
    flex-shrink: 0;
    background: none;
    border: none;
    color: var(--muted, #949bab);
    cursor: pointer;
    font-size: 12px;
    padding: 0 2px;
    line-height: 1.2;
  }
  .x:hover {
    color: var(--fg, #dfe3e9);
  }
  @keyframes toast-in {
    from {
      opacity: 0;
      transform: translateY(8px);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .toast {
      animation: none;
    }
  }
</style>
