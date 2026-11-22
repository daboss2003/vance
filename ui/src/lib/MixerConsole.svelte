<script lang="ts">
  /**
   * GOAT-4 — Fairlight-class mixer console (bus channel strips).
   *
   * Lists the sequence's submix buses with fader/pan/mute + output
   * routing, and lets the user add/remove buses. Track→bus routing and
   * aux sends are driven from the timeline rows (routeTrackToBus /
   * addTrackSend). NOTE: routing + gains are stored and undoable now; the
   * audio-fold summing through buses is the tracked remaining engine step
   * (until then a bus's fader changes the stored mix, not live playback).
   */

  import {
    buses,
    addBus,
    removeBus,
    setBusGainDb,
    setBusPan,
    setBusMute,
    setBusOutput,
    addBusInsert,
    removeBusInsert,
    addBusSend,
    removeBusSend,
    defaultEq6,
    defaultCompressor,
    defaultLimiter,
    type AudioBus,
    type BusOutput,
    type AudioEffectSpec,
    type Send
  } from '$lib/ipc';
  import { reportableError } from '$lib/toast.svelte';
  import { projectState } from '$lib/project.svelte';
  import { lockedTrackUsingBus } from '$lib/track-lock';
  import { exportRun } from '$lib/export-run.svelte';
  import { EXPORT_BUSY_HINT, refreshError } from '$lib/export-busy';

  const uid = $props.id();
  let strips = $state<AudioBus[]>([]);
  let busy = $state(false);
  let error = $state<string | null>(null);

  async function refresh() {
    try {
      strips = await buses();
    } catch (e) {
      error = refreshError(e);
    }
  }

  // Re-pull when the project's edit log advances (an undo/route lands), and
  // after an export (the engine turns the list away while one runs).
  $effect(() => {
    void projectState.editTick;
    if (!exportRun.busy) void refresh();
  });

  async function act(fn: () => Promise<unknown>) {
    if (exportRun.refusesEdit()) return;
    busy = true;
    error = null;
    try {
      await fn();
      await refresh();
    } catch (e) {
      error = reportableError(e);
    } finally {
      busy = false;
    }
  }

  function outputLabel(o: BusOutput): string {
    if (o === 'Master') return 'Master';
    const t = strips.find((b) => b.id === o.Bus);
    return t ? t.name : 'Master';
  }

  function onOutputChange(bus: AudioBus, value: string) {
    const output: BusOutput = value === 'master' ? 'Master' : { Bus: value };
    void act(() => setBusOutput(bus.id, output));
  }

  function insertLabel(e: AudioEffectSpec): string {
    switch (e.kind) {
      case 'Eq6':
        return 'EQ';
      case 'Compressor':
        return 'Comp';
      case 'Limiter':
        return 'Limit';
      case 'NoiseGate':
        return 'Gate';
      case 'DeHum':
        return 'DeHum';
      case 'Plugin':
        return e.descriptor.name || 'Plugin';
    }
  }

  function addInsert(bus: AudioBus, kind: 'eq' | 'comp' | 'limit') {
    const spec: AudioEffectSpec =
      kind === 'eq' ? defaultEq6() : kind === 'comp' ? defaultCompressor() : defaultLimiter();
    void act(() => addBusInsert(bus.id, spec));
  }

  function addSend(bus: AudioBus, targetId: string) {
    if (!targetId) return;
    const send: Send = { target: targetId, level_db: 0, pre_fader: false };
    void act(() => addBusSend(bus.id, send));
  }

  function sendLabel(s: Send): string {
    const t = strips.find((b) => b.id === s.target);
    return `${t ? t.name : '?'} ${s.pre_fader ? '(pre)' : ''}`.trim();
  }
</script>

<div class="console" title={exportRun.busy ? EXPORT_BUSY_HINT : undefined}>
  <div class="head">
    <span>Mixer — Groups</span>
    <button
      onclick={() => {
        const name = window.prompt('Bus name (e.g. "Dialogue", "Music", "Reverb")');
        if (name) void act(() => addBus(name));
      }}
      disabled={busy || exportRun.busy}>＋ Group</button
    >
  </div>
  {#if error}<div class="err">{error}</div>{/if}
  {#if strips.length === 0}
    <div class="empty">No groups yet — every track goes straight to the Master mix.</div>
  {:else}
    <div class="strips">
      {#each strips as b (b.id)}
        {@const lockedUser = lockedTrackUsingBus(projectState.sequence?.tracks ?? [], b.id)}
        <div class="strip" class:muted={b.mute}>
          <div class="name" title={b.id}>{b.name}</div>
          <label class="row">
            <span>Volume</span>
            <input
              type="range"
              min="-60"
              max="12"
              step="0.5"
              value={b.gain_db}
              disabled={busy || exportRun.busy}
              onchange={(e) =>
                act(() => setBusGainDb(b.id, parseFloat((e.target as HTMLInputElement).value)))}
            />
            <span class="val">{b.gain_db.toFixed(1)} dB</span>
          </label>
          <label class="row">
            <span>Pan</span>
            <input
              type="range"
              min="-1"
              max="1"
              step="0.05"
              value={b.pan}
              disabled={busy || exportRun.busy}
              onchange={(e) =>
                act(() => setBusPan(b.id, parseFloat((e.target as HTMLInputElement).value)))}
            />
            <span class="val">{b.pan.toFixed(2)}</span>
          </label>
          <div class="row">
            <span>Out</span>
            <select
              value={b.output === 'Master' ? 'master' : b.output.Bus}
              disabled={busy || exportRun.busy}
              onchange={(e) => onOutputChange(b, (e.target as HTMLSelectElement).value)}
            >
              <option value="master">Master</option>
              {#each strips.filter((x) => x.id !== b.id) as t (t.id)}
                <option value={t.id}>{t.name}</option>
              {/each}
            </select>
          </div>
          <div class="section">
            <div class="sec-head">
              <span>Effects</span>
              <span class="add-group">
                <button
                  disabled={busy || exportRun.busy}
                  title="Add EQ"
                  onclick={() => addInsert(b, 'eq')}>＋EQ</button
                >
                <button
                  disabled={busy || exportRun.busy}
                  title="Add Compressor"
                  onclick={() => addInsert(b, 'comp')}>＋ Compressor</button
                >
                <button
                  disabled={busy || exportRun.busy}
                  title="Add Limiter"
                  onclick={() => addInsert(b, 'limit')}>＋ Limiter</button
                >
              </span>
            </div>
            {#if b.inserts.length === 0}
              <div class="hint">— none —</div>
            {:else}
              {#each b.inserts as ins, i (i)}
                <div class="chip">
                  <span>{insertLabel(ins)}</span>
                  <button
                    class="x"
                    disabled={busy || exportRun.busy}
                    onclick={() => act(() => removeBusInsert(b.id, i))}>✕</button
                  >
                </div>
              {/each}
            {/if}
          </div>
          <div class="section">
            <div class="sec-head">
              <span>Aux / FX</span>
              <select
                disabled={busy || exportRun.busy}
                value=""
                onchange={(e) => {
                  const v = (e.target as HTMLSelectElement).value;
                  (e.target as HTMLSelectElement).value = '';
                  addSend(b, v);
                }}
              >
                <option value="">＋ Send…</option>
                {#each strips.filter((x) => x.id !== b.id) as t (t.id)}
                  <option value={t.id}>{t.name}</option>
                {/each}
              </select>
            </div>
            {#if b.sends.length === 0}
              <div class="hint">— none —</div>
            {:else}
              {#each b.sends as s, i (i)}
                <div class="chip">
                  <span>→ {sendLabel(s)}</span>
                  <button
                    class="x"
                    disabled={busy || exportRun.busy}
                    onclick={() => act(() => removeBusSend(b.id, i))}>✕</button
                  >
                </div>
              {/each}
            {/if}
          </div>
          <div class="row actions">
            <button
              class:on={b.mute}
              disabled={busy || exportRun.busy}
              onclick={() => act(() => setBusMute(b.id, !b.mute))}
            >
              {b.mute ? 'Muted' : 'Mute'}
            </button>
            <button
              class="rm"
              disabled={busy || exportRun.busy || !!lockedUser}
              aria-describedby={lockedUser ? `${uid}-${b.id}-locked` : undefined}
              onclick={() => act(() => removeBus(b.id))}>Remove</button
            >
          </div>
          {#if lockedUser}
            <div class="locked" id="{uid}-{b.id}-locked">
              Track {lockedUser.name} is locked and uses this group. Unlock it to remove the group.
            </div>
          {/if}
          <div class="route">→ {outputLabel(b.output)}</div>
        </div>
      {/each}
    </div>
  {/if}
</div>

<style>
  .console {
    display: flex;
    flex-direction: column;
    font-size: 12px;
    color: var(--fg, #ddd);
  }
  .head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 4px 6px;
    font-weight: 600;
    border-bottom: 1px solid var(--border, #333);
  }
  .head button {
    font-size: 11px;
    cursor: pointer;
  }
  .strips {
    display: flex;
    gap: 8px;
    overflow-x: auto;
    padding: 6px;
  }
  .strip {
    min-width: 150px;
    border: 1px solid var(--border, #333);
    border-radius: 4px;
    padding: 6px;
    background: rgba(255, 255, 255, 0.02);
  }
  .strip.muted {
    opacity: 0.55;
  }
  .name {
    font-weight: 600;
    margin-bottom: 4px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 4px;
    margin: 3px 0;
  }
  .row > span:first-child {
    width: 28px;
    opacity: 0.7;
  }
  .row input[type='range'] {
    flex: 1;
  }
  .val {
    width: 48px;
    text-align: right;
    font-variant-numeric: tabular-nums;
    opacity: 0.85;
  }
  .actions {
    justify-content: space-between;
  }
  .actions button.on {
    color: #ff6b6b;
  }
  .actions .rm {
    color: var(--muted, #999);
  }
  .locked {
    margin-top: 2px;
    font-size: 10px;
    line-height: 1.35;
    color: var(--muted, #949bab);
  }
  .route {
    margin-top: 4px;
    opacity: 0.6;
    font-size: 10px;
  }
  .section {
    margin-top: 5px;
    border-top: 1px solid var(--border, #2a2a2a);
    padding-top: 4px;
  }
  .sec-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    opacity: 0.7;
    font-size: 10px;
    margin-bottom: 2px;
  }
  .sec-head button {
    font-size: 9px;
    cursor: pointer;
    padding: 0 3px;
  }
  .sec-head select {
    font-size: 10px;
    max-width: 84px;
  }
  .add-group {
    display: flex;
    gap: 2px;
  }
  .chip {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 10px;
    background: rgba(255, 255, 255, 0.05);
    border-radius: 3px;
    padding: 1px 4px;
    margin: 2px 0;
  }
  .chip .x {
    cursor: pointer;
    color: var(--muted, #999);
    background: none;
    border: none;
    font-size: 10px;
  }
  .hint {
    font-size: 10px;
    opacity: 0.4;
  }
  .empty,
  .err {
    padding: 8px;
    opacity: 0.7;
  }
  .err {
    color: #ff6b6b;
  }
</style>
