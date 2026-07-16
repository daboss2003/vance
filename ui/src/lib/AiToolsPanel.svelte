<script lang="ts">
  /**
   * Optional AI tools — install/manage the heavy AI helpers (captions,
   * voice isolation, subject mask, generative images) from inside Vance.
   *
   * The three Python tools install via `pip` into Vance's own managed venv
   * (no hosting needed — PyPI serves them; the app just needs a system
   * Python 3). Stable-Diffusion is a native binary + model download that
   * comes from a manifest URL (upstream is fine — it need not be our server).
   * Either way the tool lands in Vance's managed tools dir and "just works"
   * with zero PATH setup.
   *
   * Used both in first-run onboarding and as an anytime Settings panel, so a
   * user who declines at setup can add them later.
   */
  import {
    aiToolStatus,
    aiPythonAvailable,
    installAiTool,
    type AiToolStatus
  } from '$lib/ipc';

  let {
    /** Tighter chrome when embedded in the onboarding card. */
    embedded = false,
    /** Notified after any successful install (so a host can refresh). */
    onChange = () => {}
  }: { embedded?: boolean; onChange?: () => void } = $props();

  let tools = $state<AiToolStatus[]>([]);
  let pythonOk = $state(true);
  let installing = $state<string | null>(null);
  let errors = $state<Record<string, string>>({});
  let loaded = $state(false);

  async function refresh() {
    try {
      [tools, pythonOk] = await Promise.all([aiToolStatus(), aiPythonAvailable()]);
    } catch (e) {
      errors = { ...errors, _load: String(e) };
    } finally {
      loaded = true;
    }
  }

  $effect(() => {
    void refresh();
  });

  function ready(t: AiToolStatus): boolean {
    // For SD, "ready" also needs the model file present.
    return t.installed && (t.id !== 'sd' || t.model_ready);
  }

  function stateLabel(t: AiToolStatus): string {
    if (!t.installed) return 'Not installed';
    if (t.id === 'sd' && !t.model_ready) return 'Runner installed — model missing';
    return t.managed ? 'Installed (managed by Vance)' : 'Found on system PATH';
  }

  function blockedReason(t: AiToolStatus): string | null {
    if (t.installable) return null;
    return t.method === 'pip'
      ? 'Needs Python 3 — install it, then reopen this panel.'
      : 'Needs a download source (set VANCE_TOOLS_MANIFEST_URL).';
  }

  async function install(t: AiToolStatus) {
    installing = t.id;
    errors = { ...errors, [t.id]: '' };
    try {
      await installAiTool(t.id);
      await refresh();
      onChange();
    } catch (e) {
      errors = { ...errors, [t.id]: String(e) };
    } finally {
      installing = null;
    }
  }
</script>

<div class="panel" class:embedded>
  {#if !embedded}<div class="head">AI tools</div>{/if}

  <p class="lead">
    Optional AI helpers. Add the ones you want — Vance installs them into its own
    managed folder, so nothing else on your system changes and they need no setup.
    {#if !pythonOk && loaded}
      <br /><strong class="warn">Python 3 not found</strong> — needed for the pip
      tools below. Install it from python.org (or your package manager), then reopen this panel.
    {/if}
  </p>

  {#if errors._load}<div class="err">{errors._load}</div>{/if}

  {#if loaded && tools.length === 0}
    <div class="hint">No tools reported.</div>
  {/if}

  <div class="rows">
    {#each tools as t (t.id)}
      <div class="tool" class:ok={ready(t)}>
        <span class="mark">{ready(t) ? '✓' : t.installed ? '◐' : '○'}</span>
        <div class="body">
          <div class="name">
            {t.label}
            <span class="method">{t.method === 'pip' ? 'pip' : 'download'}</span>
          </div>
          <div class="sub">{stateLabel(t)}</div>
          {#if errors[t.id]}<div class="err small">{errors[t.id]}</div>{/if}
          {#if blockedReason(t) && !ready(t)}
            <div class="hint">{blockedReason(t)}</div>
          {/if}
        </div>
        <div class="act">
          {#if ready(t)}
            <span class="done">Ready</span>
          {:else if installing === t.id}
            <button disabled>Installing…</button>
          {:else}
            <button onclick={() => install(t)} disabled={!t.installable || installing !== null}>
              {t.installed && t.id === 'sd' ? 'Get model' : 'Install'}
            </button>
          {/if}
        </div>
      </div>
    {/each}
  </div>

  <div class="hint foot">
    Installs run in the background and can take several minutes (models are large).
    You can keep editing while they download.
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
  .panel.embedded {
    padding: 0;
  }
  .head {
    font-weight: 600;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 4px;
  }
  .lead {
    margin: 0;
    opacity: 0.8;
    line-height: 1.45;
  }
  .warn {
    color: #e0a458;
  }
  .rows {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .tool {
    display: flex;
    gap: 8px;
    align-items: flex-start;
    padding: 6px 8px;
    border: 1px solid var(--border, #2c2d33);
    border-radius: 6px;
  }
  .tool.ok {
    border-color: #315a3f;
  }
  .mark {
    font-weight: 700;
    width: 14px;
    text-align: center;
  }
  .tool.ok .mark {
    color: #7ec699;
  }
  .body {
    flex: 1;
    min-width: 0;
  }
  .name {
    font-weight: 600;
  }
  .method {
    font-size: 10px;
    opacity: 0.5;
    margin-left: 6px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .sub {
    opacity: 0.7;
    font-size: 11px;
    margin-top: 1px;
  }
  .act {
    display: flex;
    align-items: center;
  }
  .done {
    color: #7ec699;
    font-size: 11px;
    font-weight: 600;
  }
  button {
    cursor: pointer;
    padding: 4px 10px;
    border-radius: 5px;
    border: 1px solid var(--border, #3a3b42);
    background: #6ea8fe;
    color: #0b0b0d;
    font-weight: 600;
    font-size: 11px;
  }
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .hint {
    opacity: 0.55;
    font-size: 10px;
    line-height: 1.4;
  }
  .foot {
    margin-top: 2px;
  }
  .err {
    color: #ff6b6b;
    font-size: 11px;
  }
  .err.small {
    font-size: 10px;
    margin-top: 2px;
  }
</style>
