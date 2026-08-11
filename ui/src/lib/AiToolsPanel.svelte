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
  import { listen, type UnlistenFn } from '@tauri-apps/api/event';

  let {
    /** Tighter chrome when embedded in the onboarding card. */
    embedded = false,
    /** Notified after any successful install (so a host can refresh). */
    onChange = () => {}
  }: { embedded?: boolean; onChange?: () => void } = $props();

  let tools = $state<AiToolStatus[]>([]);
  let pythonOk = $state(true);
  let installing = $state<string | null>(null);
  let queue = $state<string[]>([]);
  let errors = $state<Record<string, string>>({});
  let loaded = $state(false);

  // ---- live install progress (streamed from the backend) ----
  interface Progress {
    id: string;
    phase: string;
    detail: string;
    pct: number | null;
  }
  let progress = $state<Progress | null>(null);
  let elapsed = $state(0);

  // Listen for progress events; only reflect the tool THIS panel is installing
  // (events are global, but only the initiating panel shows the dialog).
  $effect(() => {
    let unlisten: UnlistenFn | null = null;
    let alive = true;
    void listen('ai-tool://progress', (e) => {
      const p = e.payload as Progress & { done?: boolean };
      if (!p || p.done) return;
      if (installing && p.id === installing) {
        progress = { id: p.id, phase: p.phase, detail: p.detail, pct: p.pct ?? null };
      }
    }).then((u) => {
      if (alive) unlisten = u;
      else u();
    });
    return () => {
      alive = false;
      if (unlisten) unlisten();
    };
  });

  // Elapsed timer + reset, driven by which tool is installing.
  $effect(() => {
    if (!installing) return;
    const started = Date.now();
    elapsed = 0;
    progress = null;
    const t = setInterval(() => (elapsed = Math.floor((Date.now() - started) / 1000)), 1000);
    return () => clearInterval(t);
  });

  const PHASE_LABEL: Record<string, string> = {
    setup: 'Setting up',
    pip: 'Installing packages',
    download: 'Downloading',
    done: 'Finishing'
  };
  function currentLabel(): string {
    return tools.find((t) => t.id === installing)?.label ?? 'AI tool';
  }
  function fmtElapsed(s: number): string {
    const m = Math.floor(s / 60);
    return `${m}:${String(s % 60).padStart(2, '0')}`;
  }

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

  /** Queue a tool for install. Multiple can be queued and install one at a
   *  time — the pip tools share one managed venv, so serial is the safe order. */
  function enqueue(t: AiToolStatus) {
    if (installing === t.id || queue.includes(t.id)) return;
    queue = [...queue, t.id];
    void processQueue();
  }

  async function processQueue() {
    if (installing) return; // a run is already draining the queue
    while (queue.length) {
      const id = queue[0];
      installing = id;
      errors = { ...errors, [id]: '' };
      try {
        await installAiTool(id);
        await refresh();
        onChange();
      } catch (e) {
        errors = { ...errors, [id]: String(e) };
      }
      queue = queue.slice(1);
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
          {:else if queue.includes(t.id)}
            <button disabled>Queued…</button>
          {:else}
            <button onclick={() => enqueue(t)} disabled={!t.installable}>
              {t.installed && t.id === 'sd' ? 'Get model' : 'Install'}
            </button>
          {/if}
        </div>
      </div>
    {/each}
  </div>

  <div class="hint foot">
    Click Install on as many as you want — they queue and install one at a time in
    the background (several minutes each; models are large). You can keep going
    meanwhile, and installs continue even after you close setup.
  </div>
</div>

{#if installing}
  <div class="prog-scrim" role="dialog" aria-modal="true" aria-label="Installing AI tool">
    <div class="prog-card">
      <div class="prog-title">Installing {currentLabel()}…</div>
      <div class="prog-phase">
        {progress ? (PHASE_LABEL[progress.phase] ?? progress.phase) : 'Starting…'}
      </div>
      <div class="prog-bar" class:indet={!progress || progress.pct == null}>
        <i style={progress && progress.pct != null ? `width:${progress.pct}%` : ''}></i>
      </div>
      <div class="prog-detail">{progress?.detail ?? 'Preparing…'}</div>
      <div class="prog-foot">
        <span class="prog-elapsed">{fmtElapsed(elapsed)} elapsed</span>
        {#if queue.length > 0}<span> · {queue.length} more queued</span>{/if}
      </div>
      <div class="prog-note">
        Large downloads — this can take several minutes. You can leave this open, or
        close setup and it keeps going in the background.
      </div>
    </div>
  </div>
{/if}

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

  /* ---- live install progress dialog (sits above the onboarding modal) ---- */
  .prog-scrim {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.66);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 2500;
  }
  .prog-card {
    width: 460px;
    max-width: 92vw;
    background: var(--panel-bg, #1c1d21);
    color: var(--fg, #e6e6e6);
    border: 1px solid var(--border, #33343a);
    border-radius: 12px;
    padding: 20px 22px;
    box-shadow: 0 20px 60px rgba(0, 0, 0, 0.55);
  }
  .prog-title {
    font-size: 15px;
    font-weight: 700;
    margin-bottom: 3px;
  }
  .prog-phase {
    font-size: 12px;
    opacity: 0.7;
    margin-bottom: 12px;
  }
  .prog-bar {
    height: 8px;
    border-radius: 999px;
    background: #2a2b31;
    overflow: hidden;
    position: relative;
  }
  .prog-bar > i {
    display: block;
    height: 100%;
    background: #6ea8fe;
    border-radius: 999px;
    transition: width 0.3s ease;
  }
  .prog-bar.indet > i {
    width: 34%;
    position: absolute;
    left: 0;
    animation: prog-slide 1.25s ease-in-out infinite;
  }
  @keyframes prog-slide {
    0% {
      left: -34%;
    }
    100% {
      left: 100%;
    }
  }
  .prog-detail {
    margin-top: 11px;
    font-family: ui-monospace, 'SF Mono', Menlo, monospace;
    font-size: 11px;
    line-height: 1.4;
    color: #c7ccd6;
    background: #15161a;
    border: 1px solid var(--border, #2c2d33);
    border-radius: 7px;
    padding: 8px 10px;
    max-height: 44px;
    overflow: hidden;
    white-space: nowrap;
    text-overflow: ellipsis;
  }
  .prog-foot {
    display: flex;
    gap: 4px;
    margin-top: 10px;
    font-size: 12px;
    opacity: 0.8;
  }
  .prog-elapsed {
    font-variant-numeric: tabular-nums;
    font-weight: 600;
  }
  .prog-note {
    margin-top: 8px;
    font-size: 10.5px;
    opacity: 0.55;
    line-height: 1.4;
  }
  @media (prefers-reduced-motion: reduce) {
    .prog-bar.indet > i {
      animation: none;
      width: 100%;
      position: static;
    }
  }
</style>
