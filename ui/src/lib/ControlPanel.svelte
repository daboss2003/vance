<script lang="ts">
  /**
   * GOAT-10 — control surfaces. Enable OSC (any phone OSC app over WiFi —
   * send to /vance/button/N, /vance/fader/0, /vance/jog) and/or a MIDI
   * controller. A poll loop drains mapped actions and dispatches them:
   * mixer/undo/redo directly via IPC, transport (play/jog/marks) via a
   * `vance:control` window event that the page's transport handler owns.
   */
  import {
    controlStatus,
    startOscControl,
    stopOscControl,
    midiPorts,
    startMidiControl,
    stopMidiControl,
    pollControlActions,
    setMasterGainDb,
    stopPlayback,
    undo as ipcUndo,
    redo as ipcRedo,
    type ControlAction,
    type ControlStatus
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';

  let status = $state<ControlStatus>({
    osc_running: false,
    osc_port: null,
    midi_running: false,
    midi_name: null
  });
  let oscPort = $state(9000);
  let ports = $state<string[]>([]);
  let midiIndex = $state(0);
  let error = $state<string | null>(null);

  async function refresh() {
    try {
      status = await controlStatus();
    } catch (e) {
      error = String(e);
    }
  }

  $effect(() => {
    void refresh();
    void midiPorts()
      .then((p) => (ports = p))
      .catch(() => {});
  });

  // Poll + dispatch while any surface is live.
  $effect(() => {
    if (!status.osc_running && !status.midi_running) return;
    const timer = setInterval(async () => {
      let actions: ControlAction[] = [];
      try {
        actions = await pollControlActions();
      } catch {
        return;
      }
      for (const a of actions) await dispatch(a);
    }, 60);
    return () => clearInterval(timer);
  });

  async function dispatch(a: ControlAction) {
    try {
      if (typeof a === 'object') {
        if ('SetMasterGainDb' in a) {
          await setMasterGainDb(a.SetMasterGainDb);
          await projectState.refresh();
        } else if ('JogFrames' in a) {
          window.dispatchEvent(new CustomEvent('vance:control', { detail: { JogFrames: a.JogFrames } }));
        }
        return;
      }
      switch (a) {
        case 'Stop':
          await stopPlayback();
          window.dispatchEvent(new CustomEvent('vance:control', { detail: 'Stop' }));
          break;
        case 'Undo':
          await ipcUndo();
          await projectState.refresh();
          break;
        case 'Redo':
          await ipcRedo();
          await projectState.refresh();
          break;
        default:
          // PlayPause / GoToStart / GoToEnd / AddMarker / Split → transport.
          window.dispatchEvent(new CustomEvent('vance:control', { detail: a }));
      }
    } catch (e) {
      error = String(e);
    }
  }

  async function toggleOsc() {
    error = null;
    try {
      if (status.osc_running) await stopOscControl();
      else await startOscControl(oscPort);
      await refresh();
    } catch (e) {
      error = String(e);
    }
  }
  async function toggleMidi() {
    error = null;
    try {
      if (status.midi_running) await stopMidiControl();
      else await startMidiControl(midiIndex);
      await refresh();
    } catch (e) {
      error = String(e);
    }
  }
</script>

<div class="panel">
  <div class="head">Control Surfaces</div>
  {#if error}<div class="err">{error}</div>{/if}

  <div class="row">
    <span>OSC</span>
    <input type="number" min="1024" max="65535" bind:value={oscPort} disabled={status.osc_running} />
    <button onclick={toggleOsc}>{status.osc_running ? 'Stop' : 'Listen'}</button>
    {#if status.osc_running}<span class="on">● {status.osc_port}</span>{/if}
  </div>

  <div class="row">
    <span>MIDI</span>
    <select bind:value={midiIndex} disabled={status.midi_running || ports.length === 0}>
      {#if ports.length === 0}<option value={0}>— no ports —</option>{/if}
      {#each ports as p, i (i)}
        <option value={i}>{p}</option>
      {/each}
    </select>
    <button disabled={ports.length === 0} onclick={toggleMidi}>
      {status.midi_running ? 'Stop' : 'Open'}
    </button>
  </div>
  {#if status.midi_running && status.midi_name}
    <div class="on">● {status.midi_name}</div>
  {/if}

  <div class="hint">
    OSC: send to <code>/vance/button/0..7</code>, <code>/vance/fader/0</code>, <code>/vance/jog</code>.
    Buttons: play, stop, start, end, marker, split, undo, redo.
  </div>
</div>

<style>
  .panel {
    display: flex;
    flex-direction: column;
    gap: 5px;
    padding: 6px;
    font-size: 12px;
    color: var(--fg, #ddd);
  }
  .head {
    font-weight: 600;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 4px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 5px;
  }
  .row > span:first-child {
    width: 34px;
    opacity: 0.7;
  }
  .row input[type='number'] {
    width: 64px;
  }
  .row select {
    flex: 1;
    min-width: 0;
  }
  button {
    cursor: pointer;
  }
  .on {
    color: #7ec699;
    font-size: 11px;
  }
  code {
    background: rgba(255, 255, 255, 0.08);
    padding: 0 3px;
    border-radius: 2px;
  }
  .hint {
    opacity: 0.5;
    font-size: 10px;
  }
  .err {
    color: #ff6b6b;
    font-size: 11px;
  }
</style>
