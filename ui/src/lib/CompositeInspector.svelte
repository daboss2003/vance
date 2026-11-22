<script lang="ts">
  /**
   * Composite inspector (M1, ADR 0007): how the selected video clip's layer
   * combines with the tracks below it. Blend mode is a clip field; opacity is
   * the clip's existing `opacity` effect, added on the first change.
   * Position and scale stay in the Effects list's Transform effect.
   */

  import { tick } from 'svelte';
  import {
    evalNumber,
    isKeyframes,
    type BlendMode,
    type ClipContents,
    type EffectInfo,
    type Keyframe,
    type TrackContents
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { canEditTrackContents, LOCKED_EDIT_HINT } from '$lib/track-lock';
  import { exportRun } from '$lib/export-run.svelte';
  import { EXPORT_BUSY_HINT } from '$lib/export-busy';

  let { track, clip }: { track: TrackContents; clip: ClipContents } = $props();

  const BLEND_MODES: { value: BlendMode; label: string; hint: string }[] = [
    { value: 'normal', label: 'Normal', hint: 'Shows this clip on top of the tracks below.' },
    {
      value: 'add',
      label: 'Add',
      hint: 'Adds this clip’s light to the tracks below. Good for glows and flares.'
    },
    {
      value: 'multiply',
      label: 'Multiply',
      hint: 'Darkens: white disappears and dark areas show. Good for shadows and textures.'
    },
    {
      value: 'screen',
      label: 'Screen',
      hint: 'Lightens: black disappears and bright areas show. Good for light leaks.'
    },
    {
      value: 'overlay',
      label: 'Overlay',
      hint: 'Adds contrast to the tracks below. Mid grey disappears.'
    },
    {
      value: 'darken',
      label: 'Darken',
      hint: 'Keeps whichever is darker: this clip or the tracks below.'
    },
    {
      value: 'lighten',
      label: 'Lighten',
      hint: 'Keeps whichever is lighter: this clip or the tracks below.'
    },
    {
      value: 'difference',
      label: 'Difference',
      hint: 'Shows where this clip differs from the tracks below. Matching areas turn black.'
    },
    { value: 'soft_light', label: 'Soft light', hint: 'Gentle contrast, like a soft spotlight.' },
    { value: 'hard_light', label: 'Hard light', hint: 'Strong contrast, like a harsh spotlight.' },
    {
      value: 'color_dodge',
      label: 'Color dodge',
      hint: 'Strongly brightens the tracks below. Highlights can blow out.'
    },
    {
      value: 'color_burn',
      label: 'Color burn',
      hint: 'Strongly darkens the tracks below and deepens their color.'
    }
  ];

  const uid = $props.id();
  const blendMode = $derived<BlendMode>(clip.blend_mode ?? 'normal');
  const blendHint = $derived(BLEND_MODES.find((m) => m.value === blendMode)?.hint ?? '');

  // The engine multiplies every enabled opacity effect; this control drives
  // the first one and says so when there are more.
  const opacityEffects = $derived(clip.effects.filter((e) => e.kind === 'opacity' && e.enabled));
  const opacityEffect = $derived<EffectInfo | null>(opacityEffects[0] ?? null);
  const opacityRaw = $derived(opacityEffect?.params.value);
  const opacityKfs = $derived<Keyframe[]>(
    opacityRaw !== undefined && isKeyframes(opacityRaw) ? opacityRaw : []
  );
  const animated = $derived(opacityRaw !== undefined && isKeyframes(opacityRaw));
  // Readout at the clip start, like the other effect sliders.
  const opacity = $derived(opacityRaw !== undefined ? evalNumber(opacityRaw, 0, 1) : 1);
  // Live readout while the slider is being dragged; cleared on commit.
  let draft = $state<number | null>(null);
  const shown = $derived(draft ?? opacity);

  // Off when the track is locked or an export holds the engine.
  const locked = $derived(!canEditTrackContents(track) || exportRun.busy);

  async function changeBlend(select: HTMLSelectElement) {
    const mode = select.value as BlendMode;
    const ok = await projectState.setClipBlendMode(track.id, clip.id, mode);
    // A rejected change must not leave the dropdown showing a mode the clip
    // doesn't have.
    if (!ok) select.value = blendMode;
  }

  async function commitOpacity(input: HTMLInputElement) {
    const v = Math.max(0, Math.min(1, parseFloat(input.value) / 100));
    if (opacityEffect) {
      await projectState.setEffectNumbers(track.id, clip, opacityEffect, [['value', v]]);
    } else {
      await projectState.addClipEffect(track.id, clip.id, 'opacity', { value: v });
    }
    draft = null;
    // Resync the thumb: a rejected change leaves the stored value as it was,
    // and then nothing re-renders the input.
    await tick();
    input.value = String(Math.round(opacity * 100));
  }

  async function keyframeOpacity() {
    if (opacityEffect) {
      await projectState.addEffectKeyframe(track.id, clip, opacityEffect, 'value', 1);
      return;
    }
    const t = await projectState.clipTimeAtPlayhead(clip);
    await projectState.addClipEffect(track.id, clip.id, 'opacity', {
      value: [{ time_seconds: t, value: 1, interp: 'Linear' }]
    });
  }

  function interpLabel(i: Keyframe['interp']): string {
    if (i === 'Bezier') return 'B';
    if (i === 'Hold') return 'H';
    return 'L';
  }
</script>

<section class="mb-3" aria-labelledby="{uid}-title">
  <h3 id="{uid}-title" class="mb-1 text-[11px] uppercase tracking-wider text-vance-muted">
    Composite
  </h3>
  <div class="border-vance-border rounded border bg-vance-bg p-2 text-xs">
    <div class="grid grid-cols-[4.5rem_minmax(0,1fr)] items-center gap-x-2">
      <label for="{uid}-blend" class="text-vance-muted text-[10px]">Blend mode</label>
      <select
        id="{uid}-blend"
        class="bg-vance-panel border-vance-border w-full min-w-0 rounded border px-1 py-0.5 text-[11px] disabled:opacity-50"
        value={blendMode}
        disabled={locked}
        title={exportRun.busy ? EXPORT_BUSY_HINT : locked ? LOCKED_EDIT_HINT : undefined}
        aria-describedby="{uid}-blend-hint"
        onchange={(e) => changeBlend(e.currentTarget)}
      >
        {#each BLEND_MODES as m (m.value)}
          <option value={m.value}>{m.label}</option>
        {/each}
      </select>
    </div>
    <p id="{uid}-blend-hint" class="text-vance-muted mt-1 text-[10px] leading-snug">
      {blendHint}
    </p>

    <div class="mt-2 grid grid-cols-[4.5rem_minmax(0,1fr)_2.25rem_1rem] items-center gap-x-2">
      <label for="{uid}-opacity" class="text-vance-muted text-[10px]">Opacity</label>
      <input
        id="{uid}-opacity"
        type="range"
        min="0"
        max="100"
        step="1"
        value={Math.round(opacity * 100)}
        disabled={locked}
        class="w-full min-w-0 accent-[var(--color-vance-accent)] disabled:opacity-50"
        aria-valuetext="{Math.round(shown * 100)}%"
        oninput={(e) => (draft = parseFloat(e.currentTarget.value) / 100)}
        onchange={(e) => commitOpacity(e.currentTarget)}
      />
      <span class="font-mono text-right text-[10px]" aria-hidden="true"
        >{Math.round(shown * 100)}%</span
      >
      <button
        type="button"
        class="text-[12px] leading-none {animated
          ? 'text-[var(--color-vance-accent)]'
          : 'text-vance-muted'} hover:brightness-125 disabled:opacity-50"
        disabled={locked}
        onclick={keyframeOpacity}
        title="Add opacity keyframe at playhead"
        aria-label="Add opacity keyframe at playhead"
      >
        ◆
      </button>
    </div>
    {#if opacityKfs.length > 0}
      <ul class="mt-1 flex flex-col gap-0.5 pl-[5rem] text-[10px] text-vance-muted">
        {#each opacityKfs as k (k.time_seconds)}
          <li class="flex items-center justify-between gap-2">
            <span class="font-mono">{k.time_seconds.toFixed(2)}s</span>
            <span class="font-mono flex-1 text-right">{Math.round(k.value * 100)}%</span>
            <button
              type="button"
              class="rounded border border-vance-border px-1 font-mono hover:brightness-125 disabled:opacity-50"
              disabled={locked}
              onclick={() =>
                opacityEffect &&
                projectState.cycleEffectKeyframeInterp(
                  track.id,
                  clip,
                  opacityEffect,
                  'value',
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
                opacityEffect &&
                projectState.removeEffectKeyframe(
                  track.id,
                  clip,
                  opacityEffect,
                  'value',
                  k.time_seconds
                )}
              title="Remove keyframe"
              aria-label="Remove keyframe at {k.time_seconds.toFixed(2)} seconds"
            >
              ✕
            </button>
          </li>
        {/each}
      </ul>
    {/if}
    {#if opacityEffects.length > 1}
      <p class="text-vance-muted mt-1 text-[10px] leading-snug">
        This clip has {opacityEffects.length} opacity effects. They multiply; this slider sets the first.
      </p>
    {/if}
  </div>
</section>
