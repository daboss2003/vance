<script lang="ts">
  /**
   * Phase 11 — first-run onboarding. Shown once (until `first_run_complete`),
   * as a dismissible modal overlay. Welcomes the user, shows what's detected
   * vs. what to install, offers privacy opt-ins (default OFF), and gets out
   * of the way. The editor is fully usable whether or not this is completed.
   */
  import {
    detectCapabilities,
    getAppSettings,
    setAppSettings,
    type Capability,
    type AppSettings
  } from '$lib/ipc';

  let { onDone }: { onDone: () => void } = $props();

  let step = $state(0);
  const steps = ['Welcome', 'What Vance found', 'Privacy', 'You’re set'];

  let caps = $state<Capability[]>([]);
  let settings = $state<AppSettings>({
    first_run_complete: false,
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
      .then((s) => (settings = s))
      .catch(() => {});
  });

  let requiredMissing = $derived(caps.filter((c) => c.required && !c.present));
  let coreReady = $derived(caps.length > 0 && requiredMissing.length === 0);

  function next() {
    if (step < steps.length - 1) step += 1;
  }
  function back() {
    if (step > 0) step -= 1;
  }

  async function finish() {
    busy = true;
    error = null;
    try {
      await setAppSettings({ ...settings, first_run_complete: true });
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
          Core editing and export are ready. Optional AI tools light up when their helpers are installed.
        {:else}
          A required component is missing — install it below, then reopen Vance.
        {/if}
      </p>
      <div class="caps">
        {#each caps as c (c.id)}
          <div class="cap" class:missing={!c.present}>
            <span class="mark">{c.present ? '✓' : c.required ? '✕' : '○'}</span>
            <div class="cbody">
              <div class="cname">
                {c.name}
                <span class="tag">{c.required ? 'required' : 'optional'}</span>
              </div>
              {#if !c.present}<div class="chint">{c.hint}</div>{/if}
            </div>
          </div>
        {/each}
      </div>
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
      <button class="ghost" onclick={finish} disabled={busy}>Skip setup</button>
      <span class="spacer"></span>
      {#if step > 0}<button class="ghost" onclick={back} disabled={busy}>Back</button>{/if}
      {#if step < steps.length - 1}
        <button onclick={next} disabled={busy}>Next</button>
      {:else}
        <button onclick={finish} disabled={busy}>Get started</button>
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
