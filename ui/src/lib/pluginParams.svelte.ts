/**
 * Module-level cache of plugin parameter metadata, shared across all
 * `AudioEffectChain` instances. Params are a property of the plugin
 * CLASS (keyed by plugin id), so caching globally means we enumerate a
 * given plugin at most once per session — surviving inspector remounts
 * and track/clip switches (which would otherwise re-spawn a sidecar +
 * dlopen the plugin on every FX-panel expand).
 */

import { listPluginParams, type PluginParamInfo } from '$lib/ipc';

export type ParamState =
  | { status: 'loading' }
  | { status: 'ready'; params: PluginParamInfo[] }
  | { status: 'error'; message: string };

const cache = $state<Record<string, ParamState>>({});

/** Current cached state for a plugin id, or `undefined` if never fetched. */
export function pluginParamState(id: string): ParamState | undefined {
  return cache[id];
}

/** Fetch a plugin's parameters once (deduped by id). Distinguishes a
 *  genuine empty param list from a load failure so the UI can render
 *  them differently. */
export async function ensurePluginParams(format: string, id: string): Promise<void> {
  if (id in cache) return; // already loading / loaded / errored
  cache[id] = { status: 'loading' };
  try {
    const params = await listPluginParams(format, id);
    cache[id] = { status: 'ready', params };
  } catch (e) {
    cache[id] = { status: 'error', message: String(e) };
  }
}
