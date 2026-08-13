// Effect Studio (M6). A no-code creator: edit one WGSL `effect(color, uv)`
// function + declare sliders, live-preview it, and save it as a shareable
// `.vsfx` add-on effect.

import { shaderPreview, shaderSave, shaderTemplate, type StudioParam } from '$lib/ipc';
import { toast } from '$lib/toast.svelte';
import { refreshInstalled } from '$lib/effectsMarket.svelte';

interface EditableParam extends StudioParam {
  /** Current value used in the live preview. */
  value: number;
}

interface StudioState {
  open: boolean;
  id: string;
  name: string;
  author: string;
  body: string;
  params: EditableParam[];
  preview: { width: number; height: number; rgba: string } | null;
  error: string | null;
  busy: boolean;
  loaded: boolean;
}

export const studio = $state<StudioState>({
  open: false,
  id: '',
  name: '',
  author: '',
  body: '',
  params: [],
  preview: null,
  error: null,
  busy: false,
  loaded: false
});

let previewTimer: ReturnType<typeof setTimeout> | null = null;

export async function openStudio(): Promise<void> {
  studio.open = true;
  if (!studio.loaded) {
    try {
      const t = await shaderTemplate();
      studio.id = t.id;
      studio.name = t.name;
      studio.body = t.body;
      studio.params = t.params.map((p) => ({ ...p, value: p.default }));
      studio.loaded = true;
    } catch (e) {
      toast.error(String(e));
    }
  }
  void preview();
}

export function closeStudio(): void {
  studio.open = false;
}

/** Debounced live preview (recompiles + renders on a sample image). */
export function schedulePreview(): void {
  if (previewTimer) clearTimeout(previewTimer);
  previewTimer = setTimeout(() => void preview(), 350);
}

export async function preview(): Promise<void> {
  studio.busy = true;
  try {
    const values = studio.params.map((p) => p.value);
    studio.preview = await shaderPreview(studio.body, values);
    studio.error = null;
  } catch (e) {
    // Compile / render error — show it inline, keep the last good preview.
    studio.error = String(e);
  } finally {
    studio.busy = false;
  }
}

export function addParam(): void {
  const n = studio.params.length;
  studio.params = [
    ...studio.params,
    { key: `param${n}`, label: `Param ${n + 1}`, min: 0, max: 1, default: 0.5, value: 0.5 }
  ];
  schedulePreview();
}

export function removeParam(i: number): void {
  studio.params = studio.params.filter((_, idx) => idx !== i);
  schedulePreview();
}

export async function save(): Promise<void> {
  studio.busy = true;
  try {
    const params: StudioParam[] = studio.params.map((p) => ({
      key: p.key,
      label: p.label,
      min: p.min,
      max: p.max,
      default: p.default
    }));
    await shaderSave({
      id: studio.id,
      name: studio.name,
      author: studio.author,
      body: studio.body,
      params
    });
    toast.success(`Saved “${studio.name}” — find it under add-on effects.`);
    await refreshInstalled();
    studio.open = false;
  } catch (e) {
    studio.error = String(e);
    toast.error(`Couldn’t save.\n${String(e)}`);
  } finally {
    studio.busy = false;
  }
}
