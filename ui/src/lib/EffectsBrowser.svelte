<script lang="ts">
  /**
   * Phase 10.6 — WASM effects browser.
   *
   * Lists discovered third-party `.wasm` plugins (from
   * `~/.vance/plugins/` + the project-local `plugins/` folder) and
   * lets the user apply one to the selected clip. Applying adds a
   * `wasm:<id>` effect seeded with the plugin's declared param
   * defaults; the engine runs it over the composited RGBA at render
   * time.
   */

  import {
    listWasmPlugins,
    applyWasmPlugin,
    listOfxPlugins,
    addWasmPlugin,
    pluginsFolder,
    type WasmPlugin,
    type OfxPlugin
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { openDialog } from '$lib/dialog.svelte';
  import { toast } from '$lib/toast.svelte';
  import { open as openFileDialog } from '@tauri-apps/plugin-dialog';

  let {
    trackId,
    clipId
  }: {
    /** Selected clip's track id, or null when no single clip is selected. */
    trackId: string | null;
    /** Selected clip id, or null. */
    clipId: string | null;
  } = $props();

  let plugins = $state<WasmPlugin[] | null>(null);
  let ofxPlugins = $state<OfxPlugin[] | null>(null);
  let loading = $state(false);
  let error = $state<string | null>(null);
  let lastApplied = $state<string | null>(null);

  async function refresh() {
    loading = true;
    error = null;
    try {
      plugins = await listWasmPlugins();
    } catch (e) {
      error = String(e);
      plugins = [];
    }
    // OFX is best-effort + read-only; failures here don't block WASM.
    try {
      const cat = await listOfxPlugins();
      ofxPlugins = cat.plugins;
    } catch {
      ofxPlugins = [];
    } finally {
      loading = false;
    }
  }

  /** Install an add-on effect from a chosen .wasm file, then refresh the list. */
  async function importEffectFile() {
    const picked = await openFileDialog({
      multiple: false,
      filters: [{ name: 'Vance add-on effect', extensions: ['wasm'] }]
    });
    if (typeof picked !== 'string') return;
    try {
      const p = await addWasmPlugin(picked);
      await refresh();
      toast.success(`Added the “${p.name}” effect.`);
    } catch (e) {
      toast.error("That file couldn't be added as an effect.\n" + String(e));
    }
  }

  /** Explainer dialog: what add-on effects are, how to get/create one, + import. */
  async function showEffectsHelp() {
    let folder = '~/.vance/plugins';
    try {
      folder = await pluginsFolder();
    } catch {
      /* keep the default hint */
    }
    const choice = await openDialog({
      title: 'Add-on effects',
      body:
        'Add-on effects are extra video effects you can install into Vance. ' +
        'The easiest way to add one: just drag an effect file (.wasm) anywhere ' +
        'onto Vance — it’s installed automatically and shows up here when a clip ' +
        'is selected. You can also import one below.',
      bullets: [
        'Get one — download a Vance effect (.wasm) shared by a creator, then drag it in.',
        'Create one — build a small effect with the Vance plugin SDK and export a .wasm to share.',
        'Installed effects live in: ' + folder
      ],
      actions: [
        { label: 'Import a file…', value: 'import', kind: 'primary' },
        { label: 'Close', value: 'close' }
      ]
    });
    if (choice === 'import') await importEffectFile();
  }

  async function apply(plugin: WasmPlugin) {
    if (!trackId || !clipId) return;
    error = null;
    try {
      if (plugin.kind === 'transition') {
        // A transition is applied at the cut AFTER the selected clip.
        await projectState.addTransitionAtCut(trackId, clipId, `wasm:${plugin.id}`, 1);
      } else {
        await applyWasmPlugin(trackId, clipId, plugin);
      }
      lastApplied = plugin.name;
      // Refresh the project so the new effect / transition shows.
      await projectState.refresh();
    } catch (e) {
      error = String(e);
    }
  }

  // Load once on mount.
  $effect(() => {
    if (plugins === null && !loading) void refresh();
  });

  const canApply = $derived(trackId !== null && clipId !== null);
</script>

<div class="border-vance-border rounded border bg-vance-bg p-2 text-[11px]">
  <div class="mb-1 flex items-center justify-between">
    <span class="text-vance-muted uppercase tracking-wider">Add-on effects</span>
    <div class="flex items-center gap-1">
      <button
        type="button"
        class="border-vance-border rounded border bg-vance-panel px-1.5 py-0.5 text-[10px] hover:brightness-125"
        onclick={showEffectsHelp}
        title="What are add-on effects? Get or import one"
      >
        + Get / Import
      </button>
      <button
        type="button"
        class="border-vance-border rounded border bg-vance-panel px-1.5 py-0.5 text-[10px] hover:brightness-125"
        onclick={refresh}
        disabled={loading}
        title="Check for installed add-on effects"
      >
        {loading ? '…' : 'Refresh'}
      </button>
    </div>
  </div>

  {#if error}
    <div class="mb-1 text-[10px] text-red-400">{error}</div>
  {/if}

  {#if !canApply}
    <div class="text-vance-muted mb-1 text-[10px] italic">Select a clip to apply an effect.</div>
  {/if}

  {#if plugins === null}
    <div class="text-vance-muted text-[10px] italic">Loading…</div>
  {:else if plugins.length === 0}
    <button
      type="button"
      class="text-vance-muted hover:text-vance-fg text-left text-[10px] italic"
      onclick={showEffectsHelp}
    >
      None installed — drag an effect file in, or click to get one.
    </button>
  {:else}
    <ul class="flex flex-col gap-1">
      {#each plugins as p (p.id)}
        <li
          class="border-vance-border flex items-center gap-2 rounded border bg-vance-panel px-1.5 py-1"
        >
          <div class="min-w-0 flex-1">
            <div class="truncate font-medium">{p.name}</div>
            <div class="text-vance-muted truncate text-[10px]">
              {p.kind} · {p.author} · v{p.version}
            </div>
          </div>
          <button
            type="button"
            class="bg-vance-accent text-vance-bg rounded px-1.5 py-0.5 text-[10px] disabled:opacity-40"
            onclick={() => apply(p)}
            disabled={!canApply}
            title={p.kind === 'effect'
              ? 'Apply this effect to the selected clip'
              : 'Add this transition at the cut after the selected clip'}
          >
            {p.kind === 'transition' ? 'Add' : 'Apply'}
          </button>
        </li>
      {/each}
    </ul>
  {/if}

  {#if lastApplied}
    <div class="text-vance-muted mt-1 text-[10px]">Applied: {lastApplied}</div>
  {/if}

  <!-- Phase 10.7 — read-only OFX catalog. Installed OFX plugins are
       listed for transparency; running them through the pipeline is
       post-v1, so there's no Apply button yet. -->
  {#if ofxPlugins && ofxPlugins.length > 0}
    <div class="border-vance-border mt-2 border-t pt-2">
      <div class="text-vance-muted mb-1 uppercase tracking-wider">OFX (read-only)</div>
      <ul class="flex flex-col gap-1">
        {#each ofxPlugins as p (p.identifier)}
          <li class="text-vance-muted truncate text-[10px]" title={p.identifier}>
            {p.bundle} · {p.identifier} · v{p.version_major}.{p.version_minor}
          </li>
        {/each}
      </ul>
      <div class="text-vance-muted mt-1 text-[10px] italic">
        OFX plugins are catalogued; running them lands post-v1.
      </div>
    </div>
  {/if}
</div>
