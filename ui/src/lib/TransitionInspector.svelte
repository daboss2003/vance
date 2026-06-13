<script lang="ts">
  /**
   * Transition inspector — shown for the selected clip. Surfaces any
   * transition at the clip's FOLLOWING cut and, for a WASM transition,
   * lets the user edit its plugin parameters.
   *
   * The `mix`/`progress` parameter is driven by the timeline (the
   * transition's position in its window), so it's hidden here; everything
   * else (e.g. a wipe's `softness`) is editable and written back via
   * `set_transition_param`.
   */

  import {
    listWasmPlugins,
    setTransitionParam,
    type TrackContents,
    type ClipContents,
    type TransitionInfo,
    type WasmPlugin
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';

  let { track, clip }: { track: TrackContents; clip: ClipContents } = $props();

  let plugins = $state<WasmPlugin[] | null>(null);
  $effect(() => {
    if (plugins === null) void listWasmPlugins().then((p) => (plugins = p)).catch(() => (plugins = []));
  });

  // The transition (if any) at the cut immediately after this clip.
  const transition = $derived.by<TransitionInfo | null>(() => {
    const cut = clip.timeline_in_seconds + clip.duration_seconds;
    return (track.transitions ?? []).find((t) => Math.abs(t.position_seconds - cut) < 1e-3) ?? null;
  });

  // The WASM plugin id for a `wasm:<id>` transition, else null.
  const wasmPluginId = $derived(
    transition && transition.kind.startsWith('wasm:') ? transition.kind.slice(5) : null
  );

  const pluginParams = $derived.by(() => {
    if (!wasmPluginId || !plugins) return [];
    const p = plugins.find((pl) => pl.id === wasmPluginId);
    if (!p) return [];
    // The engine supplies mix/progress from the timeline; hide it.
    return p.params.filter((s) => {
      const k = s.key.toLowerCase();
      return k !== 'mix' && k !== 'progress';
    });
  });

  function currentValue(key: string, fallback: number): number {
    const v = transition?.params?.[key];
    return typeof v === 'number' ? v : fallback;
  }

  async function setParam(key: string, value: number) {
    if (!transition) return;
    try {
      await setTransitionParam(track.id, transition.id, key, value);
      await projectState.refresh();
    } catch (e) {
      console.error('setTransitionParam', e);
    }
  }
</script>

{#if transition}
  <div class="border-vance-border mt-2 rounded border bg-vance-bg p-2">
    <div class="mb-1 flex items-center justify-between text-[11px] uppercase tracking-wider">
      <span class="text-vance-muted">Transition</span>
      <span class="text-vance-fg font-mono text-[10px]">
        {transition.kind === 'dissolve' ? 'Dissolve' : (transition.kind.slice(5) ?? transition.kind)}
        · {transition.duration_seconds.toFixed(2)}s
      </span>
    </div>

    {#if wasmPluginId}
      {#if plugins === null}
        <div class="text-vance-muted text-[10px] italic">Loading parameters…</div>
      {:else if pluginParams.length === 0}
        <div class="text-vance-muted text-[10px] italic">
          This transition has no editable parameters.
        </div>
      {:else}
        {#each pluginParams as param}
          <label
            class="border-vance-border mb-1 flex items-center gap-2 rounded border-l-2 py-0.5 pl-2 text-[10px] font-mono"
          >
            <span class="text-vance-muted w-24 truncate" title={param.label}>{param.label}</span>
            <input
              type="range"
              min={param.min}
              max={param.max}
              step={(param.max - param.min) / 1000 || 0.001}
              value={currentValue(param.key, param.default)}
              class="flex-1"
              onchange={(e) => setParam(param.key, parseFloat((e.target as HTMLInputElement).value))}
            />
            <span class="text-vance-fg w-12 text-right">
              {currentValue(param.key, param.default).toFixed(3)}
            </span>
          </label>
        {/each}
      {/if}
    {:else}
      <div class="text-vance-muted text-[10px] italic">
        Built-in cross-dissolve (no parameters).
      </div>
    {/if}
  </div>
{/if}
