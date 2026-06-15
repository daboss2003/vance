<script lang="ts">
  /**
   * GOAT-7 — captions / subtitles editor + auto-transcription.
   * Edit timed cues, import/export .srt/.vtt, and (if a local Whisper CLI
   * is installed) auto-transcribe a media item into cues.
   */
  import {
    captions as ipcCaptions,
    addCaption,
    removeCaption,
    setCaptionText,
    setCaptionTiming,
    splitCaption,
    mergeCaptionWithNext,
    importCaptions,
    exportCaptions,
    transcriptionAvailable,
    transcribeMedia,
    type Cue
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';

  let cues = $state<Cue[]>([]);
  let busy = $state(false);
  let error = $state<string | null>(null);
  let status = $state<string | null>(null);

  let filePath = $state('captions.srt');
  let whisperOk = $state(false);
  let transcribeMediaId = $state('');
  let model = $state('base');
  let language = $state('');

  let mediaChoices = $derived(
    projectState.media.filter((m) => m.kind === 'video' || m.kind === 'audio')
  );

  $effect(() => {
    void projectState.editTick;
    void refresh();
  });
  $effect(() => {
    void transcriptionAvailable().then((v) => (whisperOk = v));
  });

  async function refresh() {
    try {
      cues = await ipcCaptions();
    } catch (e) {
      error = String(e);
    }
  }

  async function act(fn: () => Promise<unknown>, ok?: string) {
    busy = true;
    error = null;
    status = null;
    try {
      await fn();
      await projectState.refresh();
      await refresh();
      if (ok) status = ok;
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  function fmt(ms: number): string {
    const s = Math.max(0, ms) / 1000;
    return s.toFixed(2);
  }
  function parseSec(v: string): number {
    return Math.round(parseFloat(v || '0') * 1000);
  }

  function addAtEnd() {
    const last = cues.length ? cues[cues.length - 1].end_ms : 0;
    void act(() => addCaption(last, last + 2000, 'New caption'));
  }

  async function runTranscribe() {
    if (!transcribeMediaId) return;
    busy = true;
    error = null;
    status = 'Transcribing… (this can take a while)';
    try {
      const n = await transcribeMedia(transcribeMediaId, model, language || null);
      await projectState.refresh();
      await refresh();
      status = `Transcribed ${n} cues.`;
    } catch (e) {
      error = String(e);
      status = null;
    } finally {
      busy = false;
    }
  }
</script>

<div class="panel">
  <div class="head">
    <span>Captions</span>
    <button onclick={addAtEnd} disabled={busy}>＋ Cue</button>
  </div>
  {#if error}<div class="err">{error}</div>{/if}
  {#if status}<div class="status">{status}</div>{/if}

  <div class="cues">
    {#if cues.length === 0}
      <div class="hint">No captions. Add a cue, import a file, or transcribe below.</div>
    {:else}
      {#each cues as c (c.id)}
        <div class="cue">
          <div class="times">
            <input
              type="number" step="0.01" min="0" value={fmt(c.start_ms)} disabled={busy}
              onchange={(e) =>
                act(() => setCaptionTiming(c.id, parseSec((e.target as HTMLInputElement).value), c.end_ms))}
            />
            <span>→</span>
            <input
              type="number" step="0.01" min="0" value={fmt(c.end_ms)} disabled={busy}
              onchange={(e) =>
                act(() => setCaptionTiming(c.id, c.start_ms, parseSec((e.target as HTMLInputElement).value)))}
            />
          </div>
          <input
            class="text" type="text" value={c.text} disabled={busy}
            onchange={(e) => act(() => setCaptionText(c.id, (e.target as HTMLInputElement).value))}
          />
          <div class="cue-actions">
            <button title="Split at mid-point" disabled={busy}
              onclick={() => act(() => splitCaption(c.id, Math.round((c.start_ms + c.end_ms) / 2)))}>⊟</button>
            <button title="Merge with next" disabled={busy}
              onclick={() => act(() => mergeCaptionWithNext(c.id))}>⊕</button>
            <button class="rm" title="Remove" disabled={busy}
              onclick={() => act(() => removeCaption(c.id))}>✕</button>
          </div>
        </div>
      {/each}
    {/if}
  </div>

  <div class="io">
    <input type="text" bind:value={filePath} placeholder="captions.srt or .vtt" disabled={busy} />
    <button disabled={busy} onclick={() => act(() => importCaptions(filePath), 'Imported.')}>Import</button>
    <button disabled={busy}
      onclick={() => act(() => exportCaptions(filePath, filePath.toLowerCase().endsWith('.vtt')), 'Exported.')}>Export</button>
  </div>

  <div class="transcribe">
    <div class="t-head">Auto-transcribe (Whisper)</div>
    {#if !whisperOk}
      <div class="hint">
        Install a Whisper CLI to enable: <code>pip install -U openai-whisper</code> (or whisper.cpp).
      </div>
    {/if}
    <div class="t-row">
      <select bind:value={transcribeMediaId} disabled={busy || !whisperOk}>
        <option value="">— media —</option>
        {#each mediaChoices as m (m.id)}
          <option value={m.id}>{m.name}</option>
        {/each}
      </select>
      <select bind:value={model} disabled={busy || !whisperOk} title="Whisper model">
        <option value="tiny">tiny</option>
        <option value="base">base</option>
        <option value="small">small</option>
        <option value="medium">medium</option>
        <option value="large-v3">large-v3</option>
      </select>
    </div>
    <div class="t-row">
      <input type="text" bind:value={language} placeholder="lang (auto)" disabled={busy || !whisperOk} />
      <button disabled={busy || !whisperOk || !transcribeMediaId} onclick={runTranscribe}>Transcribe</button>
    </div>
  </div>
</div>

<style>
  .panel {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 6px;
    font-size: 12px;
    color: var(--fg, #ddd);
  }
  .head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-weight: 600;
    border-bottom: 1px solid var(--border, #333);
    padding-bottom: 4px;
  }
  .cues {
    display: flex;
    flex-direction: column;
    gap: 4px;
    max-height: 180px;
    overflow-y: auto;
  }
  .cue {
    display: flex;
    flex-direction: column;
    gap: 2px;
    border: 1px solid var(--border, #2a2a2a);
    border-radius: 3px;
    padding: 3px;
  }
  .times {
    display: flex;
    align-items: center;
    gap: 4px;
  }
  .times input {
    width: 64px;
  }
  .text {
    width: 100%;
  }
  .cue-actions {
    display: flex;
    justify-content: flex-end;
    gap: 4px;
  }
  .cue-actions button {
    cursor: pointer;
    font-size: 11px;
  }
  .rm {
    color: #ff6b6b;
  }
  .io {
    display: flex;
    gap: 4px;
    border-top: 1px solid var(--border, #2a2a2a);
    padding-top: 4px;
  }
  .io input {
    flex: 1;
  }
  .transcribe {
    border-top: 1px solid var(--border, #2a2a2a);
    padding-top: 4px;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .t-head {
    font-weight: 600;
    opacity: 0.8;
  }
  .t-row {
    display: flex;
    gap: 4px;
  }
  .t-row select,
  .t-row input {
    flex: 1;
  }
  code {
    background: rgba(255, 255, 255, 0.08);
    padding: 0 3px;
    border-radius: 2px;
  }
  .hint {
    opacity: 0.55;
    font-size: 10px;
  }
  .err {
    color: #ff6b6b;
    font-size: 11px;
  }
  .status {
    color: #7ec699;
    font-size: 11px;
  }
</style>
