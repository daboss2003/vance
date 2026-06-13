<script lang="ts">
  /**
   * Phase 7.7 — per-track audio effect chain editor.
   *
   * Renders a track's `audio_effects` list with add / remove / move
   * buttons + per-effect param sliders. Every committed value goes
   * through `projectState.updateAudioEffect` (full spec replace) so
   * the EditLog undo stays trivial.
   */

  import {
    defaultCompressor,
    defaultEq6,
    defaultLimiter,
    defaultPlugin,
    listAudioPlugins,
    type AudioEffectSpec,
    type AudioPluginDescriptor,
    type CompressorSpec,
    type EqBandSpec,
    type LimiterSpec,
    type PluginParamInfo
  } from '$lib/ipc';
  import { ensurePluginParams, pluginParamState } from '$lib/pluginParams.svelte';
  import { projectState } from '$lib/project.svelte';

  let {
    trackId,
    effects
  }: {
    trackId: string;
    effects: AudioEffectSpec[];
  } = $props();

  function commit(index: number, next: AudioEffectSpec) {
    void projectState.updateAudioEffect(trackId, index, next);
  }

  // Fetch parameter metadata for every plugin in the chain (deduped in
  // the module-level store). Done in an effect — never as a render-time
  // side effect — so a re-render can't trigger a fetch storm.
  $effect(() => {
    for (const e of effects) {
      if (e.kind === 'Plugin') {
        void ensurePluginParams(e.descriptor.format, e.descriptor.id);
      }
    }
  });

  /** Current value of a plugin param: the override in the effect spec, or
   *  the param's reported default. */
  function pluginParamValue(effect: AudioEffectSpec, info: PluginParamInfo): number {
    if (effect.kind !== 'Plugin') return info.default;
    const found = effect.params.find((p) => p.param_id === info.id);
    return found ? found.value : info.default;
  }

  function setPluginParam(
    index: number,
    effect: AudioEffectSpec,
    paramId: number,
    value: number
  ) {
    if (effect.kind !== 'Plugin') return;
    const params = effect.params.filter((p) => p.param_id !== paramId);
    params.push({ param_id: paramId, value });
    commit(index, { ...effect, params });
  }

  function toggleEnabled(index: number, effect: AudioEffectSpec) {
    const next: AudioEffectSpec = { ...effect, enabled: !effect.enabled };
    commit(index, next);
  }

  function updateEqBand(
    index: number,
    effect: AudioEffectSpec,
    bandIdx: number,
    patch: Partial<EqBandSpec>
  ) {
    if (effect.kind !== 'Eq6') return;
    const bands = effect.eq.bands.map((b, i) => (i === bandIdx ? { ...b, ...patch } : b));
    commit(index, { ...effect, eq: { bands } });
  }

  function updateCompressor(
    index: number,
    effect: AudioEffectSpec,
    patch: Partial<CompressorSpec>
  ) {
    if (effect.kind !== 'Compressor') return;
    const next: AudioEffectSpec = { ...effect, spec: { ...effect.spec, ...patch } };
    commit(index, next);
  }

  function updateLimiter(index: number, effect: AudioEffectSpec, patch: Partial<LimiterSpec>) {
    if (effect.kind !== 'Limiter') return;
    const next: AudioEffectSpec = { ...effect, spec: { ...effect.spec, ...patch } };
    commit(index, next);
  }

  const eqBandKinds = ['LowShelf', 'Peak', 'HighShelf', 'LowCut', 'HighCut'] as const;

  // Phase 7.10 / 7.11 — plugin browser. `null` = not loaded yet;
  // `[]` = sidecar present but no plugins installed; populated array
  // = clickable plugin list.
  let availablePlugins = $state<AudioPluginDescriptor[] | null>(null);
  let pluginBrowserOpen = $state(false);

  async function togglePluginBrowser() {
    pluginBrowserOpen = !pluginBrowserOpen;
    if (pluginBrowserOpen && availablePlugins === null) {
      try {
        availablePlugins = await listAudioPlugins();
      } catch {
        availablePlugins = [];
      }
    }
  }

  function addPlugin(d: AudioPluginDescriptor) {
    void projectState.addAudioEffect(trackId, defaultPlugin(d));
    pluginBrowserOpen = false;
  }
</script>

<div class="mt-3">
  <div class="mb-1 flex items-center justify-between text-[11px] uppercase tracking-wider">
    <span class="text-vance-muted">Audio FX</span>
    <div class="flex gap-1">
      <button
        type="button"
        class="border-vance-border rounded border bg-vance-bg px-1.5 py-0.5 text-[10px] hover:brightness-125"
        onclick={() => projectState.addAudioEffect(trackId, defaultEq6())}
        title="Add a 6-band parametric EQ"
      >
        + EQ
      </button>
      <button
        type="button"
        class="border-vance-border rounded border bg-vance-bg px-1.5 py-0.5 text-[10px] hover:brightness-125"
        onclick={() => projectState.addAudioEffect(trackId, defaultCompressor())}
        title="Add a single-band compressor"
      >
        + Comp
      </button>
      <button
        type="button"
        class="border-vance-border rounded border bg-vance-bg px-1.5 py-0.5 text-[10px] hover:brightness-125"
        onclick={() => projectState.addAudioEffect(trackId, defaultLimiter())}
        title="Add a look-ahead peak limiter"
      >
        + Limit
      </button>
      <button
        type="button"
        class="border-vance-border rounded border bg-vance-bg px-1.5 py-0.5 text-[10px] hover:brightness-125"
        onclick={togglePluginBrowser}
        title="Browse LV2 / CLAP plugins from the sandboxed host"
      >
        + Plugin
      </button>
    </div>
  </div>

  {#if pluginBrowserOpen}
    <div
      class="border-vance-border mb-2 max-h-48 overflow-y-auto rounded border bg-vance-bg p-2 text-[10px]"
    >
      <div class="text-vance-muted mb-1">
        Sandboxed plugins ({availablePlugins?.length ?? '…'})
      </div>
      {#if availablePlugins === null}
        <div class="italic">Querying sandbox…</div>
      {:else if availablePlugins.length === 0}
        <div class="italic">
          No plugins found. (The sandbox sidecar isn't installed/built with plugin support, or your
          CLAP / LV2 plugin folders are empty.)
        </div>
      {:else}
        {#each availablePlugins as p}
          <button
            type="button"
            class="hover:bg-vance-panel flex w-full items-center gap-2 rounded px-1 py-0.5 text-left"
            onclick={() => addPlugin(p)}
            disabled={!p.runnable}
            title={p.runnable ? `${p.format.toUpperCase()} ${p.id}` : 'Plugin host cannot run this'}
          >
            <span class="text-vance-muted w-8 font-mono uppercase">{p.format}</span>
            <span class="flex-1">{p.name}</span>
            {#if p.vendor}<span class="text-vance-muted">{p.vendor}</span>{/if}
            {#if !p.runnable}<span class="text-red-400">(unavailable)</span>{/if}
          </button>
        {/each}
      {/if}
    </div>
  {/if}

  {#if effects.length === 0}
    <div class="text-[10px] italic text-vance-muted">No effects on this track.</div>
  {/if}

  {#each effects as effect, idx}
    <div class="border-vance-border mb-2 rounded border bg-vance-bg p-2">
      <div class="mb-1 flex items-center gap-2 text-[11px]">
        <button
          type="button"
          class="rounded px-1 font-mono text-[10px]"
          class:bg-vance-accent={effect.enabled}
          class:text-vance-bg={effect.enabled}
          class:bg-vance-panel={!effect.enabled}
          onclick={() => toggleEnabled(idx, effect)}
          title="Enable / disable this effect"
        >
          {effect.enabled ? 'ON' : 'OFF'}
        </button>
        <span class="flex-1 font-medium">
          {effect.kind === 'Eq6'
            ? 'Parametric EQ'
            : effect.kind === 'Compressor'
              ? 'Compressor'
              : effect.kind === 'Limiter'
                ? 'Limiter'
                : `Plugin · ${effect.descriptor.name}`}
        </span>
        <button
          type="button"
          class="text-[10px] text-vance-muted hover:text-vance-fg"
          onclick={() => projectState.moveAudioEffect(trackId, idx, idx - 1)}
          disabled={idx === 0}
          title="Move up in chain"
        >
          ▲
        </button>
        <button
          type="button"
          class="text-[10px] text-vance-muted hover:text-vance-fg"
          onclick={() => projectState.moveAudioEffect(trackId, idx, idx + 1)}
          disabled={idx === effects.length - 1}
          title="Move down in chain"
        >
          ▼
        </button>
        <button
          type="button"
          class="text-[10px] text-red-400 hover:text-red-300"
          onclick={() => projectState.removeAudioEffect(trackId, idx)}
          title="Remove this effect"
        >
          ✕
        </button>
      </div>

      {#if effect.kind === 'Eq6'}
        {#each effect.eq.bands as band, bIdx}
          <div
            class="border-vance-border mb-1 flex flex-wrap items-center gap-1 rounded border-l-2 py-0.5 pl-2 text-[10px] font-mono"
          >
            <span class="w-6 text-vance-muted">B{bIdx + 1}</span>
            <select
              class="border-vance-border rounded border bg-vance-panel px-1 text-[10px]"
              value={band.kind}
              onchange={(e) =>
                updateEqBand(idx, effect, bIdx, {
                  kind: (e.target as HTMLSelectElement).value as EqBandSpec['kind']
                })}
            >
              {#each eqBandKinds as k}
                <option value={k}>{k}</option>
              {/each}
            </select>
            <label class="flex items-center gap-1">
              <span class="text-vance-muted">f</span>
              <input
                type="number"
                step="1"
                min="10"
                max="22000"
                value={band.freq_hz}
                class="border-vance-border w-16 rounded border bg-vance-panel px-1 text-[10px]"
                oninput={(e) =>
                  updateEqBand(idx, effect, bIdx, {
                    freq_hz: parseFloat((e.target as HTMLInputElement).value)
                  })}
              />
              <span class="text-vance-muted">Hz</span>
            </label>
            <label class="flex items-center gap-1">
              <span class="text-vance-muted">g</span>
              <input
                type="number"
                step="0.5"
                min="-24"
                max="24"
                value={band.gain_db}
                class="border-vance-border w-12 rounded border bg-vance-panel px-1 text-[10px]"
                oninput={(e) =>
                  updateEqBand(idx, effect, bIdx, {
                    gain_db: parseFloat((e.target as HTMLInputElement).value)
                  })}
              />
              <span class="text-vance-muted">dB</span>
            </label>
            <label class="flex items-center gap-1">
              <span class="text-vance-muted">Q</span>
              <input
                type="number"
                step="0.1"
                min="0.1"
                max="18"
                value={band.q}
                class="border-vance-border w-10 rounded border bg-vance-panel px-1 text-[10px]"
                oninput={(e) =>
                  updateEqBand(idx, effect, bIdx, {
                    q: parseFloat((e.target as HTMLInputElement).value)
                  })}
              />
            </label>
            <button
              type="button"
              class="text-[10px] text-vance-muted hover:text-vance-fg"
              onclick={() => updateEqBand(idx, effect, bIdx, { enabled: !band.enabled })}
            >
              {band.enabled ? 'on' : 'off'}
            </button>
          </div>
        {/each}
      {:else if effect.kind === 'Compressor'}
        <div class="grid grid-cols-2 gap-1 text-[10px] font-mono">
          <label class="flex items-center gap-1">
            <span class="text-vance-muted">thresh</span>
            <input
              type="number"
              step="0.5"
              min="-60"
              max="0"
              value={effect.spec.threshold_db}
              class="border-vance-border w-14 rounded border bg-vance-panel px-1"
              oninput={(e) =>
                updateCompressor(idx, effect, {
                  threshold_db: parseFloat((e.target as HTMLInputElement).value)
                })}
            />
            <span class="text-vance-muted">dB</span>
          </label>
          <label class="flex items-center gap-1">
            <span class="text-vance-muted">ratio</span>
            <input
              type="number"
              step="0.1"
              min="1"
              max="20"
              value={effect.spec.ratio}
              class="border-vance-border w-12 rounded border bg-vance-panel px-1"
              oninput={(e) =>
                updateCompressor(idx, effect, {
                  ratio: parseFloat((e.target as HTMLInputElement).value)
                })}
            />
          </label>
          <label class="flex items-center gap-1">
            <span class="text-vance-muted">atk</span>
            <input
              type="number"
              step="0.1"
              min="0.1"
              max="1000"
              value={effect.spec.attack_ms}
              class="border-vance-border w-12 rounded border bg-vance-panel px-1"
              oninput={(e) =>
                updateCompressor(idx, effect, {
                  attack_ms: parseFloat((e.target as HTMLInputElement).value)
                })}
            />
            <span class="text-vance-muted">ms</span>
          </label>
          <label class="flex items-center gap-1">
            <span class="text-vance-muted">rel</span>
            <input
              type="number"
              step="1"
              min="0.1"
              max="5000"
              value={effect.spec.release_ms}
              class="border-vance-border w-14 rounded border bg-vance-panel px-1"
              oninput={(e) =>
                updateCompressor(idx, effect, {
                  release_ms: parseFloat((e.target as HTMLInputElement).value)
                })}
            />
            <span class="text-vance-muted">ms</span>
          </label>
          <label class="flex items-center gap-1">
            <span class="text-vance-muted">knee</span>
            <input
              type="number"
              step="0.5"
              min="0"
              max="18"
              value={effect.spec.knee_db}
              class="border-vance-border w-12 rounded border bg-vance-panel px-1"
              oninput={(e) =>
                updateCompressor(idx, effect, {
                  knee_db: parseFloat((e.target as HTMLInputElement).value)
                })}
            />
            <span class="text-vance-muted">dB</span>
          </label>
          <label class="flex items-center gap-1">
            <span class="text-vance-muted">makeup</span>
            <input
              type="number"
              step="0.5"
              min="-12"
              max="24"
              value={effect.spec.makeup_db}
              class="border-vance-border w-12 rounded border bg-vance-panel px-1"
              oninput={(e) =>
                updateCompressor(idx, effect, {
                  makeup_db: parseFloat((e.target as HTMLInputElement).value)
                })}
            />
            <span class="text-vance-muted">dB</span>
          </label>
        </div>
      {:else if effect.kind === 'Limiter'}
        <div class="flex gap-2 text-[10px] font-mono">
          <label class="flex items-center gap-1">
            <span class="text-vance-muted">ceiling</span>
            <input
              type="number"
              step="0.1"
              min="-30"
              max="0"
              value={effect.spec.ceiling_db}
              class="border-vance-border w-14 rounded border bg-vance-panel px-1"
              oninput={(e) =>
                updateLimiter(idx, effect, {
                  ceiling_db: parseFloat((e.target as HTMLInputElement).value)
                })}
            />
            <span class="text-vance-muted">dB</span>
          </label>
          <label class="flex items-center gap-1">
            <span class="text-vance-muted">release</span>
            <input
              type="number"
              step="1"
              min="1"
              max="5000"
              value={effect.spec.release_ms}
              class="border-vance-border w-14 rounded border bg-vance-panel px-1"
              oninput={(e) =>
                updateLimiter(idx, effect, {
                  release_ms: parseFloat((e.target as HTMLInputElement).value)
                })}
            />
            <span class="text-vance-muted">ms</span>
          </label>
        </div>
      {:else if effect.kind === 'Plugin'}
        {@const pstate = pluginParamState(effect.descriptor.id)}
        <div class="text-vance-muted text-[10px] font-mono">
          <div class="break-all">
            <span class="text-vance-muted">{effect.descriptor.format}</span>
            <span class="text-vance-fg ml-1">{effect.descriptor.id}</span>
          </div>
        </div>
        {#if !pstate || pstate.status === 'loading'}
          <div class="text-[10px] italic text-vance-muted">Loading parameters…</div>
        {:else if pstate.status === 'error'}
          <div class="text-[10px] italic text-red-400" title={pstate.message}>
            Couldn't load this plugin to read its parameters.
          </div>
        {:else if pstate.params.length === 0}
          <div class="text-[10px] italic text-vance-muted">This plugin exposes no parameters.</div>
        {:else}
          {#each pstate.params as param}
            <label
              class="border-vance-border mb-1 flex items-center gap-2 rounded border-l-2 py-0.5 pl-2 text-[10px] font-mono"
            >
              <span class="w-24 truncate text-vance-muted" title={param.name}>{param.name}</span>
              <input
                type="range"
                min={param.min}
                max={param.max}
                step={(param.max - param.min) / 1000 || 0.001}
                value={pluginParamValue(effect, param)}
                class="flex-1"
                oninput={(e) =>
                  setPluginParam(
                    idx,
                    effect,
                    param.id,
                    parseFloat((e.target as HTMLInputElement).value)
                  )}
              />
              <span class="w-12 text-right text-vance-fg">
                {pluginParamValue(effect, param).toFixed(3)}
              </span>
            </label>
          {/each}
        {/if}
      {/if}
    </div>
  {/each}
</div>
