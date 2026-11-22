<script lang="ts">
  /**
   * Keyer inspector (M1, ADR 0007): controls for one `keyer` effect on the
   * selected clip, rendered inside its Effects-list entry. Every numeric
   * param is keyframeable like the other effect sliders. "Show matte" solos
   * this clip's matte in the Record viewer (transient viewer state).
   */

  import { onDestroy, tick, untrack } from 'svelte';
  import {
    evalNumber,
    isKeyframes,
    type ClipContents,
    type EffectInfo,
    type Keyframe,
    type TrackContents
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import * as KK from '$lib/keyer-keys';
  import { canEditTrackContents } from '$lib/track-lock';
  import { exportRun } from '$lib/export-run.svelte';
  import { orderedClipValue } from '$lib/keyer-order';

  let { track, clip, effect }: { track: TrackContents; clip: ClipContents; effect: EffectInfo } =
    $props();

  const uid = $props.id();
  // Locked track or an export in progress: every control that edits the
  // keyer is disabled ("Show matte" is viewer state, not an edit, so it stays
  // available).
  const locked = $derived(!canEditTrackContents(track) || exportRun.busy);

  type Row = {
    key: string;
    label: string;
    hint: string;
    min: number;
    max: number;
    step: number;
    precision: number;
  };

  const MATTE_ROWS: Row[] = [
    {
      key: KK.KEY_BALANCE,
      label: 'Balance',
      hint: 'How the two other color channels are weighed against the key color.',
      min: 0,
      max: 1,
      step: 0.01,
      precision: 2
    },
    {
      key: KK.KEY_STRENGTH,
      label: 'Strength',
      hint: 'How hard the key color is pulled out. Raise it if the background still shows.',
      min: 0,
      max: 4,
      step: 0.01,
      precision: 2
    },
    {
      key: KK.KEY_CLIP_BLACK,
      label: 'Clip black',
      hint: 'Matte values at or below this become fully transparent. Cleans up the background.',
      min: 0,
      max: 1,
      step: 0.01,
      precision: 2
    },
    {
      key: KK.KEY_CLIP_WHITE,
      label: 'Clip white',
      hint: 'Matte values at or above this become fully solid. Fills holes in the subject.',
      min: 0,
      max: 1,
      step: 0.01,
      precision: 2
    }
  ];
  const EDGE_ROWS: Row[] = [
    {
      key: KK.KEY_CHOKE_PX,
      label: 'Shrink',
      hint: 'Shrinks the matte edge (positive) or grows it (negative), in source pixels.',
      min: -20,
      max: 20,
      step: 0.5,
      precision: 1
    },
    {
      key: KK.KEY_SOFTEN_PX,
      label: 'Soften',
      hint: 'Blurs the matte edge, in source pixels.',
      min: 0,
      max: 20,
      step: 0.5,
      precision: 1
    }
  ];
  const SPILL_ROWS: Row[] = [
    {
      key: KK.KEY_SPILL,
      label: 'Amount',
      hint: 'Removes the key color that reflects onto the subject.',
      min: 0,
      max: 1,
      step: 0.01,
      precision: 2
    },
    {
      key: KK.KEY_SPILL_BIAS,
      label: 'Bias',
      hint: 'Lower uses the average of the other channels as the reference, higher uses their maximum.',
      min: 0,
      max: 1,
      step: 0.01,
      precision: 2
    }
  ];

  const GROUPS = [
    { id: 'matte', label: 'Matte', rows: MATTE_ROWS },
    { id: 'edges', label: 'Edges', rows: EDGE_ROWS },
    { id: 'spill', label: 'Spill', rows: SPILL_ROWS }
  ];

  function raw(key: string) {
    return effect.params[key];
  }
  /** Value at clip time `t`. */
  function valueAt(key: string, t: number): number {
    const r = raw(key);
    return r !== undefined ? evalNumber(r, t, KK.keyerDefault(key)) : KK.keyerDefault(key);
  }
  function value(key: string): number {
    // Readout at the clip start, like the other effect sliders.
    return valueAt(key, 0);
  }
  function keyframes(key: string): Keyframe[] {
    const r = raw(key);
    return r !== undefined && isKeyframes(r) ? r : [];
  }

  // Live readouts while a slider is dragged; a key is dropped on commit.
  let drafts = $state<Record<string, number>>({});

  async function commit(key: string, input: HTMLInputElement) {
    let v = parseFloat(input.value);
    let at: number | undefined;
    // Keep the clip range ordered so the matte never inverts by accident, at
    // every frame of the clip (keyer-order.ts). A keyframed param gets its
    // keyframe at the playhead.
    if (key === KK.KEY_CLIP_BLACK || key === KK.KEY_CLIP_WHITE) {
      try {
        at = await projectState.clipTimeAtPlayhead(clip);
      } catch {
        at = undefined; // order against the clip start; the write reports any failure
      }
      const seq = projectState.sequence;
      const fps = seq && seq.frame_rate_den > 0 ? seq.frame_rate_num / seq.frame_rate_den : 0;
      v = orderedClipValue(
        key,
        v,
        raw(KK.KEY_CLIP_BLACK),
        raw(KK.KEY_CLIP_WHITE),
        at ?? 0,
        clip.duration_seconds,
        fps
      );
    }
    await projectState.setEffectNumbers(track.id, clip, effect, [[key, v]], at);
    delete drafts[key];
    // Resync the thumb: a clamped or rejected value can leave the stored
    // value unchanged, and then nothing re-renders the input.
    await tick();
    input.value = String(value(key));
  }

  // ---- Key color ------------------------------------------------------------

  const keyRgb = $derived<[number, number, number]>([
    value(KK.KEY_R),
    value(KK.KEY_G),
    value(KK.KEY_B)
  ]);
  const keyHex = $derived(toHex(keyRgb));

  function toHex(rgb: [number, number, number]): string {
    return (
      '#' +
      rgb
        .map((c) =>
          Math.round(Math.max(0, Math.min(1, c)) * 255)
            .toString(16)
            .padStart(2, '0')
        )
        .join('')
    );
  }
  function fromHex(hex: string): [number, number, number] {
    const n = parseInt(hex.slice(1), 16);
    return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255];
  }

  async function setKeyColor(rgb: [number, number, number], picker?: HTMLInputElement) {
    await projectState.setEffectNumbers(track.id, clip, effect, [
      [KK.KEY_R, rgb[0]],
      [KK.KEY_G, rgb[1]],
      [KK.KEY_B, rgb[2]]
    ]);
    if (picker) {
      await tick();
      picker.value = keyHex;
    }
  }

  const KEY_PRESETS: { label: string; rgb: [number, number, number] }[] = [
    { label: 'Green', rgb: [0, 1, 0] },
    { label: 'Blue', rgb: [0, 0, 1] }
  ];

  // ---- Invert + matte view ----------------------------------------------------

  const inverted = $derived(value(KK.KEY_INVERT) > 0.5);

  async function setInverted(box: HTMLInputElement) {
    await projectState.setEffectNumbers(track.id, clip, effect, [
      [KK.KEY_INVERT, box.checked ? 1 : 0]
    ]);
    await tick();
    box.checked = inverted;
  }
  const matteOn = $derived(projectState.matteViewClipId === clip.id);
  const matteTitle = $derived(
    matteOn
      ? 'Show the composited picture again'
      : 'Show this clip’s matte in the Record viewer: white is kept, black is keyed out'
  );

  // The matte solo belongs to this keyer; drop it when the inspector goes
  // away (clip deselected, effect removed, page or tab switched).
  onDestroy(() => {
    untrack(() => {
      if (projectState.matteViewClipId === clip.id) void projectState.setMatteView(null);
    });
  });

  function interpLabel(i: Keyframe['interp']): string {
    if (i === 'Bezier') return 'B';
    if (i === 'Hold') return 'H';
    return 'L';
  }
</script>

{#snippet numberRow(row: Row)}
  {@const kfs = keyframes(row.key)}
  {@const cur = value(row.key)}
  {@const shown = drafts[row.key] ?? cur}
  <div
    class="mt-1 grid grid-cols-[4.5rem_minmax(0,1fr)_2.25rem_1rem] items-center gap-x-2"
    title={row.hint}
  >
    <label for="{uid}-{row.key}" class="text-vance-muted text-[10px]">{row.label}</label>
    <input
      id="{uid}-{row.key}"
      type="range"
      min={row.min}
      max={row.max}
      step={row.step}
      value={cur}
      disabled={locked}
      class="w-full min-w-0 accent-[var(--color-vance-accent)] disabled:opacity-50"
      aria-valuetext={shown.toFixed(row.precision)}
      oninput={(e) => (drafts[row.key] = parseFloat(e.currentTarget.value))}
      onchange={(e) => commit(row.key, e.currentTarget)}
    />
    <span class="font-mono text-right text-[10px]" aria-hidden="true"
      >{shown.toFixed(row.precision)}</span
    >
    <button
      type="button"
      class="text-[12px] leading-none {kfs.length > 0
        ? 'text-[var(--color-vance-accent)]'
        : 'text-vance-muted'} hover:brightness-125 disabled:opacity-50"
      disabled={locked}
      onclick={() =>
        projectState.addEffectKeyframe(track.id, clip, effect, row.key, KK.keyerDefault(row.key))}
      title="Add {row.label.toLowerCase()} keyframe at playhead"
      aria-label="Add {row.label.toLowerCase()} keyframe at playhead"
    >
      ◆
    </button>
  </div>
  {#if kfs.length > 0}
    <ul class="mt-1 flex flex-col gap-0.5 pl-[5rem] text-[10px] text-vance-muted">
      {#each kfs as k (k.time_seconds)}
        <li class="flex items-center justify-between gap-2">
          <span class="font-mono">{k.time_seconds.toFixed(2)}s</span>
          <span class="font-mono flex-1 text-right">{k.value.toFixed(row.precision)}</span>
          <button
            type="button"
            class="rounded border border-vance-border px-1 font-mono hover:brightness-125 disabled:opacity-50"
            disabled={locked}
            onclick={() =>
              projectState.cycleEffectKeyframeInterp(
                track.id,
                clip,
                effect,
                row.key,
                k.time_seconds
              )}
            title="Easing to the next keyframe: click to cycle Linear, Bezier, Hold"
          >
            {interpLabel(k.interp)}
          </button>
          <button
            type="button"
            class="hover:text-red-400 disabled:opacity-50 disabled:hover:text-inherit"
            disabled={locked}
            onclick={() =>
              projectState.removeEffectKeyframe(track.id, clip, effect, row.key, k.time_seconds)}
            title="Remove keyframe"
            aria-label="Remove keyframe at {k.time_seconds.toFixed(2)} seconds"
          >
            ✕
          </button>
        </li>
      {/each}
    </ul>
  {/if}
{/snippet}

<div class="mt-1 text-xs">
  <div class="grid grid-cols-[4.5rem_minmax(0,1fr)] items-center gap-x-2">
    <label for="{uid}-color" class="text-vance-muted text-[10px]">Key color</label>
    <div class="flex min-w-0 items-center gap-1">
      <input
        id="{uid}-color"
        type="color"
        value={keyHex}
        disabled={locked}
        class="border-vance-border h-5 w-8 shrink-0 cursor-pointer rounded border bg-transparent p-0 disabled:cursor-default disabled:opacity-50"
        title="The background color to key out"
        onchange={(e) => setKeyColor(fromHex(e.currentTarget.value), e.currentTarget)}
      />
      <span class="font-mono text-vance-muted truncate text-[10px]">{keyHex}</span>
      <span class="ml-auto flex gap-1">
        {#each KEY_PRESETS as p (p.label)}
          <button
            type="button"
            class="rounded bg-vance-panel border-vance-border border px-1.5 py-0.5 text-[10px] hover:brightness-125 disabled:opacity-50"
            disabled={locked}
            onclick={() => setKeyColor(p.rgb)}
            title="Key out a {p.label.toLowerCase()} screen"
          >
            {p.label}
          </button>
        {/each}
      </span>
    </div>
  </div>

  {#each GROUPS as group (group.id)}
    <div role="group" aria-labelledby="{uid}-{group.id}">
      <div id="{uid}-{group.id}" class="text-vance-muted mt-2 text-[10px] uppercase tracking-wider">
        {group.label}
      </div>
      {#each group.rows as row (row.key)}
        {@render numberRow(row)}
      {/each}
    </div>
  {/each}

  <div class="mt-2 flex items-center justify-between gap-2">
    <label class="flex items-center gap-1.5 text-[11px]">
      <input
        type="checkbox"
        checked={inverted}
        disabled={locked}
        onchange={(e) => setInverted(e.currentTarget)}
      />
      Invert matte
    </label>
    <button
      type="button"
      title={matteTitle}
      class="rounded border px-1.5 py-0.5 text-[10px] {matteOn
        ? 'border-yellow-400 bg-[#3a3320] text-yellow-200'
        : 'bg-vance-panel border-vance-border hover:brightness-125'}"
      aria-pressed={matteOn}
      onclick={() => projectState.setMatteView(matteOn ? null : clip.id)}
    >
      Show matte
    </button>
  </div>
</div>
