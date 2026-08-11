// Effects marketplace / manager state. Backs EffectsMarketplace.svelte and the
// window drop routing: while the marketplace is open, a file dropped anywhere
// on the window imports as an effect (a DOM/window drop — no canvas needed).

import { addWasmPlugin, listWasmPlugins, removeWasmPlugin, type WasmPlugin } from '$lib/ipc';
import { toast } from '$lib/toast.svelte';

interface MarketState {
  open: boolean;
  busy: boolean;
  installed: WasmPlugin[];
  loaded: boolean;
  /** True while a file is being dragged over the window with the modal open. */
  dragOver: boolean;
}

export const market = $state<MarketState>({
  open: false,
  busy: false,
  installed: [],
  loaded: false,
  dragOver: false
});

export async function refreshInstalled(): Promise<void> {
  try {
    market.installed = await listWasmPlugins();
  } catch {
    /* leave the last list */
  } finally {
    market.loaded = true;
  }
}

export async function openMarket(): Promise<void> {
  market.open = true;
  await refreshInstalled();
}

export function closeMarket(): void {
  market.open = false;
}

/** Import one or more dropped/picked files as add-on effects. */
export async function importEffectPaths(paths: string[]): Promise<void> {
  const wasm = paths.filter((p) => p.toLowerCase().endsWith('.wasm'));
  if (wasm.length === 0) {
    toast.error('Choose an effect file (.wasm).');
    return;
  }
  market.busy = true;
  try {
    for (const path of wasm) {
      const name = path.split('/').pop() ?? path;
      try {
        const p = await addWasmPlugin(path);
        toast.success(`Added the “${p.name}” effect.`);
      } catch (e) {
        toast.error(`“${name}” couldn’t be added.\n${String(e)}`);
      }
    }
    await refreshInstalled();
  } finally {
    market.busy = false;
  }
}

/** Uninstall an effect by id. */
export async function uninstallEffect(id: string, name: string): Promise<void> {
  market.busy = true;
  try {
    await removeWasmPlugin(id);
    toast.info(`Removed “${name}”.`);
    await refreshInstalled();
  } catch (e) {
    toast.error(`Couldn’t remove “${name}”.\n${String(e)}`);
  } finally {
    market.busy = false;
  }
}
