<script lang="ts">
  /**
   * Add-on effects marketplace / manager. A modal with:
   *  - an Import area (drag a .wasm in, or Select a file) — the drop is a
   *    window-level Tauri drop routed here while this modal is open,
   *  - the list of installed effects with a Remove action,
   *  - guidance on getting / creating effects.
   *
   * A hosted "browse" catalog is a future step (needs a catalog server); the
   * Import flow works today.
   */
  import { market, closeMarket, importEffectPaths, uninstallEffect } from '$lib/effectsMarket.svelte';
  import { pluginsFolder } from '$lib/ipc';
  import { open as openFileDialog } from '@tauri-apps/plugin-dialog';

  let folder = $state('');
  $effect(() => {
    if (market.open && !folder) void pluginsFolder().then((f) => (folder = f)).catch(() => {});
  });

  async function selectFile() {
    const picked = await openFileDialog({
      multiple: true,
      filters: [{ name: 'Vance effect', extensions: ['wasm'] }]
    });
    if (!picked) return;
    await importEffectPaths(Array.isArray(picked) ? picked : [picked]);
  }
</script>

{#if market.open}
  <div
    class="scrim"
    role="dialog"
    aria-modal="true"
    aria-label="Add-on effects"
    onclick={(e) => {
      if (e.target === e.currentTarget) closeMarket();
    }}
  >
    <div class="card">
      <div class="head">
        <h2>Add-on effects</h2>
        <button class="x" onclick={closeMarket} aria-label="Close">✕</button>
      </div>

      <!-- Import area -->
      <div class="drop" class:hot={market.dragOver}>
        <div class="drop-title">Add an effect</div>
        <div class="drop-sub">Drag an effect file (.wasm) here, or</div>
        <button class="select" onclick={selectFile} disabled={market.busy}>
          {market.busy ? 'Adding…' : 'Select a file…'}
        </button>
      </div>

      <!-- Installed -->
      <div class="sec-title">Installed</div>
      {#if !market.loaded}
        <div class="muted">Loading…</div>
      {:else if market.installed.length === 0}
        <div class="muted">Nothing yet — add an effect above.</div>
      {:else}
        <ul class="list">
          {#each market.installed as p (p.id)}
            <li>
              <div class="pinfo">
                <div class="pname">{p.name}</div>
                <div class="pmeta">{p.kind} · {p.author} · v{p.version}</div>
              </div>
              <button class="rm" onclick={() => uninstallEffect(p.id, p.name)} disabled={market.busy}>
                Remove
              </button>
            </li>
          {/each}
        </ul>
      {/if}

      <!-- Guidance -->
      <div class="sec-title">Where to get effects</div>
      <ul class="tips">
        <li><b>Download one</b> shared by a creator (an <span class="mono">.wasm</span> file), then add it above.</li>
        <li><b>Make one</b> with the Vance effect SDK and export a <span class="mono">.wasm</span> to share or sell.</li>
        <li>A built-in effects marketplace to browse &amp; install is coming.</li>
      </ul>
      {#if folder}
        <div class="folder">Installed effects are kept in <span class="mono">{folder}</span></div>
      {/if}
    </div>
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 3400;
    background: rgba(0, 0, 0, 0.62);
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 20px;
  }
  .card {
    width: 500px;
    max-width: 94vw;
    max-height: 88vh;
    overflow: auto;
    background: var(--panel-bg, #1e2127);
    color: var(--fg, #dfe3e9);
    border: 1px solid var(--border, #30353d);
    border-radius: 12px;
    padding: 18px 20px;
    box-shadow: 0 24px 64px rgba(0, 0, 0, 0.55);
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: 14px;
  }
  h2 {
    font-size: 16px;
    font-weight: 700;
    margin: 0;
  }
  .x {
    background: none;
    border: none;
    color: var(--muted, #949bab);
    cursor: pointer;
    font-size: 14px;
  }
  .x:hover {
    color: var(--fg, #dfe3e9);
  }
  .drop {
    border: 2px dashed var(--border-strong, #3a4150);
    border-radius: 10px;
    padding: 22px 16px;
    text-align: center;
    transition: border-color 0.15s, background 0.15s;
  }
  .drop.hot {
    border-color: var(--accent, #4c9dff);
    background: color-mix(in srgb, var(--accent, #4c9dff) 10%, transparent);
  }
  .drop-title {
    font-weight: 600;
    font-size: 14px;
  }
  .drop-sub {
    font-size: 12px;
    color: var(--muted, #949bab);
    margin: 4px 0 10px;
  }
  .select {
    cursor: pointer;
    padding: 7px 16px;
    border-radius: 7px;
    border: none;
    background: var(--accent, #4c9dff);
    color: #071522;
    font-weight: 600;
    font-size: 13px;
  }
  .select:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .sec-title {
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--muted, #949bab);
    font-weight: 600;
    margin: 18px 0 7px;
  }
  .muted {
    font-size: 12.5px;
    color: var(--muted, #949bab);
    font-style: italic;
  }
  .list {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin: 0;
    padding: 0;
    list-style: none;
  }
  .list li {
    display: flex;
    align-items: center;
    gap: 10px;
    border: 1px solid var(--border, #30353d);
    border-radius: 8px;
    padding: 8px 10px;
  }
  .pinfo {
    flex: 1;
    min-width: 0;
  }
  .pname {
    font-weight: 600;
    font-size: 13px;
  }
  .pmeta {
    font-size: 11px;
    color: var(--muted, #949bab);
  }
  .rm {
    cursor: pointer;
    padding: 4px 10px;
    border-radius: 6px;
    border: 1px solid var(--border-strong, #3a4150);
    background: transparent;
    color: var(--muted, #949bab);
    font-size: 12px;
  }
  .rm:hover {
    color: var(--err, #f2686f);
    border-color: var(--err, #f2686f);
  }
  .tips {
    margin: 0;
    padding-left: 18px;
    font-size: 12.5px;
    line-height: 1.55;
    color: var(--fg, #dfe3e9);
  }
  .tips li {
    margin-bottom: 3px;
  }
  .folder {
    margin-top: 10px;
    font-size: 11px;
    color: var(--muted, #949bab);
  }
  .mono {
    font-family: ui-monospace, 'SF Mono', Menlo, monospace;
    font-size: 0.92em;
    background: var(--panel-2, #262a31);
    padding: 0 4px;
    border-radius: 4px;
  }
</style>
