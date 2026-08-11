<script lang="ts">
  /**
   * Phase 11 — first-run onboarding, as a RESUMABLE setup flow. Instead of
   * showing once, it reappears on every launch until the user actually
   * finishes it with all required components present, resuming at the step
   * they left off (`onboarding_step`). "Finish later" closes it for the
   * session but it returns next launch. The editor is fully usable throughout.
   */
  import {
    detectCapabilities,
    getAppSettings,
    setAppSettings,
    type Capability,
    type AppSettings
  } from '$lib/ipc';
  import AiToolsPanel from '$lib/AiToolsPanel.svelte';

  let { onDone }: { onDone: () => void } = $props();

  const steps = ['Welcome', 'What Vance found', 'Privacy', 'You’re set'];
  const CAPS_STEP = 1;
  let step = $state(0);
  let loaded = $state(false);

  let caps = $state<Capability[]>([]);
  let settings = $state<AppSettings>({
    first_run_complete: false,
    onboarding_step: 0,
    crash_reports_opt_in: false,
    telemetry_opt_in: false,
    ai_provider: null
  });
  let busy = $state(false);
  let error = $state<string | null>(null);

  $effect(() => {
    void detectCapabilities()
      .then((c) => (caps = c))
      .catch((e) => (error = String(e)));
    void getAppSettings()
      .then((s) => {
        settings = s;
        // Resume where setup was left off (once, on load; clamped to range).
        if (!loaded) {
          step = Math.min(Math.max(s.onboarding_step ?? 0, 0), steps.length - 1);
          loaded = true;
        }
      })
      .catch(() => {});
  });

  let requiredCaps = $derived(caps.filter((c) => c.required));
  let requiredMissing = $derived(requiredCaps.filter((c) => !c.present));
  let coreReady = $derived(caps.length > 0 && requiredMissing.length === 0);

  /** Remember the current step so a hard quit still resumes here next launch. */
  async function persistStep(s: number) {
    settings = { ...settings, onboarding_step: s };
    try {
      await setAppSettings(settings);
    } catch {
      /* non-fatal: resume just won't advance */
    }
  }

  function next() {
    if (step < steps.length - 1) {
      step += 1;
      void persistStep(step);
    }
  }
  function back() {
    if (step > 0) {
      step -= 1;
      void persistStep(step);
    }
  }

  /** Re-run capability detection (after the user installs a missing component). */
  function recheck() {
    error = null;
    void detectCapabilities()
      .then((c) => (caps = c))
      .catch((e) => (error = String(e)));
  }

  /** Finish for good — only allowed once required components are present, so an
   *  incomplete setup keeps bringing the wizard back at the components step. */
  async function finish() {
    if (!coreReady) {
      error = 'A required component is still missing. Install it and press “Re-check”, then finish.';
      step = CAPS_STEP;
      await persistStep(step);
      return;
    }
    busy = true;
    error = null;
    try {
      await setAppSettings({ ...settings, first_run_complete: true, onboarding_step: steps.length - 1 });
      onDone();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  /** Close for this session WITHOUT completing — it returns next launch,
   *  resuming at the step we're on now. */
  async function finishLater() {
    busy = true;
    error = null;
    try {
      await persistStep(step);
      onDone();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="scrim" role="dialog" aria-modal="true" aria-label="Welcome to Vance">
  <div class="card">
    <div class="steps">
      {#each steps as s, i (s)}
        <span class="dot" class:on={i === step} class:done={i < step}></span>
      {/each}
    </div>

    {#if error}<div class="err">{error}</div>{/if}

    {#if step === 0}
      <h1>Welcome to Vance</h1>
      <p class="lead">A fast, cross-platform video editor. This quick setup takes about a minute — or skip it; the editor works right away.</p>
      <ul class="feat">
        <li>Edit, grade, mix, and caption on a magnetic or track timeline.</li>
        <li>Node-graph compositing (Fusion), motion-graphics titles, and AI-assisted tools.</li>
        <li>Export to ProRes/DNxHR/H.264/HEVC/AV1, or publish straight to YouTube.</li>
      </ul>
    {:else if step === 1}
      <h1>What Vance found</h1>
      <p class="lead">
        {#if coreReady}
          Core editing and export are ready. Add any optional AI tools below — Vance installs them for you, right here.
        {:else}
          A required component is missing — install it (see the hint below), then press <strong>Re-check</strong>. Setup can’t finish until it’s present.
        {/if}
      </p>
      <div class="caps">
        {#each requiredCaps as c (c.id)}
          <div class="cap" class:missing={!c.present}>
            <span class="mark">{c.present ? '✓' : '✕'}</span>
            <div class="cbody">
              <div class="cname">
                {c.name}
                <span class="tag">required</span>
              </div>
              {#if !c.present}<div class="chint">{c.hint}</div>{/if}
            </div>
          </div>
        {/each}
      </div>
      <button class="ghost recheck" onclick={recheck} disabled={busy}>↻ Re-check</button>

      <h2 class="aihdr">Optional AI tools</h2>
      <p class="lead sub">Captions, voice isolation, subject mask, and generative images. Click <strong>Install</strong> on the ones you want — they download in the background while you keep setting up. All optional; add or remove them any time later.</p>
      <AiToolsPanel embedded />
    {:else if step === 2}
      <h1>Privacy</h1>
      <p class="lead">Both are off by default and you can change them any time. Vance never collects personal data.</p>
      <label class="opt">
        <input type="checkbox" bind:checked={settings.crash_reports_opt_in} disabled={busy} />
        <span>
          <strong>Send crash reports</strong>
          <em>Crash logs are always saved locally; this only enables sharing them to help fix bugs.</em>
        </span>
      </label>
      <label class="opt">
        <input type="checkbox" bind:checked={settings.telemetry_opt_in} disabled={busy} />
        <span>
          <strong>Anonymous usage stats</strong>
          <em>Aggregate, no personal data, documented schema. Helps prioritize features.</em>
        </span>
      </label>
    {:else}
      <h1>You’re set</h1>
      <p class="lead">
        Open a file or create media from the panels below. The
        <strong>AI setup</strong> (choose a provider, paste a key) lives in Settings when you want it — it’s never required.
      </p>
      <p class="lead">Have fun. 🎬</p>
    {/if}

    <div class="actions">
      <button class="ghost" onclick={finishLater} disabled={busy} title="Closes setup for now; it returns next launch where you left off">Finish later</button>
      <span class="spacer"></span>
      {#if step > 0}<button class="ghost" onclick={back} disabled={busy}>Back</button>{/if}
      {#if step < steps.length - 1}
        <button onclick={next} disabled={busy}>Next</button>
      {:else}
        <button onclick={finish} disabled={busy}>{coreReady ? 'Get started' : 'Finish setup'}</button>
      {/if}
    </div>
  </div>
</div>

<style>
  .scrim {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.6);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
  }
  .card {
    width: 560px;
    max-width: 92vw;
    max-height: 88vh;
    overflow: auto;
    background: var(--panel-bg, #1c1d21);
    color: var(--fg, #e6e6e6);
    border: 1px solid var(--border, #33343a);
    border-radius: 10px;
    padding: 22px 26px;
    box-shadow: 0 20px 60px rgba(0, 0, 0, 0.5);
  }
  .steps {
    display: flex;
    gap: 8px;
    margin-bottom: 14px;
  }
  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #3a3b42;
  }
  .dot.on {
    background: #6ea8fe;
  }
  .dot.done {
    background: #7ec699;
  }
  h1 {
    font-size: 20px;
    font-weight: 700;
    margin: 0 0 8px;
  }
  .lead {
    opacity: 0.85;
    line-height: 1.45;
    margin: 0 0 12px;
  }
  .feat {
    margin: 0 0 8px;
    padding-left: 18px;
    line-height: 1.6;
    opacity: 0.85;
  }
  .caps {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .recheck {
    margin-top: 10px;
  }
  .aihdr {
    font-size: 15px;
    font-weight: 700;
    margin: 18px 0 6px;
    padding-top: 16px;
    border-top: 1px solid var(--border, #2c2d33);
  }
  .lead.sub {
    font-size: 12px;
    opacity: 0.72;
  }
  .cap {
    display: flex;
    gap: 10px;
    align-items: flex-start;
    padding: 6px 8px;
    border: 1px solid var(--border, #2c2d33);
    border-radius: 6px;
  }
  .cap.missing {
    border-color: #6b4a2a;
  }
  .mark {
    font-weight: 700;
    width: 16px;
    text-align: center;
  }
  .cap:not(.missing) .mark {
    color: #7ec699;
  }
  .cap.missing .mark {
    color: #e0a458;
  }
  .cname {
    font-weight: 600;
  }
  .tag {
    font-size: 10px;
    opacity: 0.55;
    margin-left: 6px;
  }
  .chint {
    font-size: 11px;
    opacity: 0.7;
    margin-top: 2px;
    line-height: 1.4;
  }
  .opt {
    display: flex;
    gap: 10px;
    align-items: flex-start;
    padding: 8px 0;
  }
  .opt strong {
    display: block;
  }
  .opt em {
    display: block;
    font-style: normal;
    opacity: 0.65;
    font-size: 12px;
    line-height: 1.4;
  }
  .actions {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 18px;
  }
  .spacer {
    flex: 1;
  }
  button {
    cursor: pointer;
    padding: 6px 14px;
    border-radius: 6px;
    border: 1px solid var(--border, #3a3b42);
    background: #6ea8fe;
    color: #0b0b0d;
    font-weight: 600;
  }
  button.ghost {
    background: transparent;
    color: var(--fg, #e6e6e6);
  }
  button:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .err {
    color: #ff6b6b;
    font-size: 12px;
    margin-bottom: 8px;
  }
</style>
