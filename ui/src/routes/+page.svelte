<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { open } from '@tauri-apps/plugin-dialog';
  import {
    backendVersion,
    decodeAndRenderAt,
    diagLog,
    isPlaying,
    lastResumePts,
    openFile,
    setViewportBounds,
    startPlayback,
    stopPlayback,
    type StreamInfo
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { dragState } from '$lib/drag.svelte';
  import Timeline from '$lib/timeline/Timeline.svelte';

  let backend = $state<string>('checking…');
  let info = $state<StreamInfo | null>(null);
  let openError = $state<string | null>(null);
  let renderError = $state<string | null>(null);
  let lastPts = $state<number | null>(null);
  let timecodeInput = $state<string>('00:00:00:00');
  let playing = $state<boolean>(false);
  let playError = $state<string | null>(null);

  let viewportEl: HTMLDivElement | null = $state(null);

  // Bumped whenever something in the project changes that the Timeline
  // canvas should re-render in response to. We track both the project
  // info (sequence/project changes) and the edit tick (clip inserts).
  //
  // NOTE: the `++` would read+write the same rune inside the effect,
  // which Svelte 5 trips as `effect_update_depth_exceeded`. We wrap
  // the mutation in `untrack` so the read isn't tracked as a dep.
  let timelineRefreshTick = $state<number>(0);
  $effect(() => {
    void projectState.info;
    void projectState.editTick;
    untrack(() => {
      timelineRefreshTick = timelineRefreshTick + 1;
    });
  });

  onMount(() => {
    backendVersion()
      .then((v) => {
        backend = v;
      })
      .catch((e) => {
        backend = `error: ${e}`;
      });

    if (!viewportEl) return;
    const observer = new ResizeObserver(reportBounds);
    observer.observe(viewportEl);
    window.addEventListener('resize', reportBounds);
    window.addEventListener('scroll', reportBounds, true);
    window.addEventListener('keydown', handleKeydown);
    // Poll the engine for playback state — covers natural EOS / errors.
    // On a true→false transition we also fetch the engine's last resume
    // PTS so the next Play can detect end-of-file and rewind to 0.
    // We also re-broadcast viewport bounds here: layout reflows that
    // don't change the viewport el's *size* (header content updates,
    // font loading, async chrome shifts) won't trip ResizeObserver, so
    // a 250ms heartbeat keeps the native overlay aligned with what the
    // user sees.
    const pollId = window.setInterval(async () => {
      try {
        const next = await isPlaying();
        if (playing && !next) {
          const resume = await lastResumePts();
          if (resume != null) lastPts = resume;
        }
        playing = next;
      } catch {
        // ignore transient errors
      }
      reportBounds();
    }, 250);
    reportBounds();

    // Manual drag tracking: move the ghost with the cursor and clear
    // the drag if mouseup happens anywhere outside a drop zone. The
    // clear is deferred to a microtask so each drop zone's mouseup
    // handler runs first and can claim the drop before we wipe state.
    const onMouseMoveDrag = (e: MouseEvent) => dragState.move(e.clientX, e.clientY);
    const onMouseUpDrag = () => {
      if (!dragState.mediaId) return;
      queueMicrotask(() => {
        if (dragState.mediaId) dragState.end();
      });
    };
    window.addEventListener('mousemove', onMouseMoveDrag);
    window.addEventListener('mouseup', onMouseUpDrag);

    return () => {
      observer.disconnect();
      window.removeEventListener('resize', reportBounds);
      window.removeEventListener('scroll', reportBounds, true);
      window.removeEventListener('keydown', handleKeydown);
      window.removeEventListener('mousemove', onMouseMoveDrag);
      window.removeEventListener('mouseup', onMouseUpDrag);
      window.clearInterval(pollId);
    };
  });

  /** PTS units per video frame: time_base_den / (frame_rate_num/den). */
  function ptsPerFrame(): number | null {
    if (!info) return null;
    if (info.frame_rate_num === 0 || info.frame_rate_den === 0) return null;
    if (info.time_base_num === 0 || info.time_base_den === 0) return null;
    // one frame in seconds = den / num; in stream PTS = sec * (tb_den / tb_num)
    return Math.round(
      (info.frame_rate_den / info.frame_rate_num) *
        (info.time_base_den / info.time_base_num)
    );
  }

  /** PTS for one second in the stream's time base. */
  function ptsPerSecond(): number {
    if (!info) return 1;
    return info.time_base_num !== 0
      ? Math.round(info.time_base_den / info.time_base_num)
      : 1;
  }

  /** Clamp a PTS into the valid seekable range for the open stream. */
  function clampPts(target: number, step: number): number {
    let t = Math.max(0, target);
    if (info && info.duration_pts != null) {
      const last = Math.max(0, info.duration_pts - step);
      if (t > last) t = last;
    }
    return t;
  }

  async function stepFrame(direction: 1 | -1) {
    if (!info) return;
    if (playing) await togglePlayback();
    const step = ptsPerFrame();
    if (step == null) return;
    const target = clampPts((lastPts ?? 0) + direction * step, step);
    await render(target);
  }

  async function jumpSeconds(seconds: number) {
    if (!info) return;
    // Preserve play state across the jump: J/L while playing should jump
    // and KEEP PLAYING from the new position, like every NLE.
    const wasPlaying = playing;
    if (wasPlaying) {
      try {
        const resumePts = await stopPlayback();
        lastPts = resumePts;
        playing = false;
      } catch {
        /* ignore */
      }
    }
    const step = ptsPerFrame() ?? 1;
    const target = clampPts((lastPts ?? 0) + seconds * ptsPerSecond(), step);
    await render(target);
    if (wasPlaying) {
      try {
        await startPlayback(lastPts ?? target);
        playing = true;
      } catch (e) {
        playError = String(e);
      }
    }
  }

  function handleKeydown(e: KeyboardEvent) {
    // Don't steal keys when the user is typing in an input.
    const target = e.target as HTMLElement | null;
    if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA')) {
      return;
    }
    switch (e.key) {
      case ' ':
        e.preventDefault();
        togglePlayback();
        return;
      case 'k':
      case 'K':
        e.preventDefault();
        if (playing) togglePlayback();
        return;
      case 'l':
      case 'L':
        e.preventDefault();
        if (!playing) togglePlayback();
        return;
      case 'j':
      case 'J':
        e.preventDefault();
        jumpSeconds(-1);
        return;
      case 'ArrowLeft':
        e.preventDefault();
        if (e.shiftKey) jumpSeconds(-1);
        else stepFrame(-1);
        return;
      case 'ArrowRight':
        e.preventDefault();
        if (e.shiftKey) jumpSeconds(1);
        else stepFrame(1);
        return;
    }
  }

  function reportBounds() {
    if (!viewportEl) return;
    const r = viewportEl.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    const x = Math.max(0, Math.round(r.left * dpr));
    const y = Math.max(0, Math.round(r.top * dpr));
    const w = Math.max(0, Math.round(r.width * dpr));
    const h = Math.max(0, Math.round(r.height * dpr));
    setViewportBounds(x, y, w, h).catch((e) => console.error('set_viewport_bounds', e));
  }

  // Re-broadcast bounds whenever the chrome around the viewport changes
  // shape (project loaded/cleared, sequence selection, save path appears).
  // ResizeObserver only fires on size changes of the viewport el itself
  // and misses the case where its *position* changes because a sibling
  // reflowed.
  $effect(() => {
    // Touch the runes we care about so the effect re-runs on change.
    void projectState.info;
    void projectState.path;
    // Wait for the next frame so DOM layout has settled.
    requestAnimationFrame(reportBounds);
  });

  async function handleOpen() {
    openError = null;
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [
        {
          name: 'Video',
          extensions: ['mp4', 'mov', 'mkv', 'm4v', 'webm', 'avi']
        },
        { name: 'All files', extensions: ['*'] }
      ]
    });
    if (typeof selected !== 'string') return;
    try {
      info = await openFile(selected);
      // Auto-render the first frame.
      await render(0);
    } catch (e) {
      openError = String(e);
    }
  }

  // ---- Project toolbar handlers --------------------------------------------

  async function handleNewProject() {
    // Tauri's WKWebView suppresses window.prompt(), so we just create
    // an "Untitled" project here. Renaming via the Inspector lands in
    // 4.10; for now the user can Save As to give it a real path/name.
    await projectState.newProject('Untitled');
  }

  async function handleOpenProject() {
    const selected = await open({
      multiple: false,
      directory: true,
      title: 'Open .vance project bundle'
    });
    if (typeof selected !== 'string') return;
    await projectState.open(selected);
  }

  async function handleSaveProject() {
    if (!projectState.info) return;
    if (projectState.path) {
      await projectState.save(null);
      return;
    }
    // No remembered path → Save As: pick a location, append .vance.
    const selected = await open({
      multiple: false,
      directory: true,
      title: 'Pick a folder to save the project bundle in'
    });
    if (typeof selected !== 'string') return;
    const name = projectState.info.name || 'Untitled';
    const bundle = `${selected}/${name}.vance`;
    await projectState.save(bundle);
  }

  async function handleImportMedia() {
    if (!projectState.info) return;
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [
        {
          name: 'Video',
          extensions: ['mp4', 'mov', 'mkv', 'm4v', 'webm', 'avi']
        },
        { name: 'All files', extensions: ['*'] }
      ]
    });
    if (typeof selected !== 'string') return;
    await projectState.importMedia(selected);
  }

  function formatDuration(seconds: number): string {
    const s = Math.max(0, Math.floor(seconds));
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const ss = s % 60;
    if (h > 0) return `${h}:${String(m).padStart(2, '0')}:${String(ss).padStart(2, '0')}`;
    return `${m}:${String(ss).padStart(2, '0')}`;
  }

  async function render(pts: number) {
    renderError = null;
    try {
      lastPts = await decodeAndRenderAt(pts);
    } catch (e) {
      const msg = String(e);
      // "no frame at or after pts X" is the decoder telling us we asked
      // for a frame past the last decodable one. Treat as a no-op (stay
      // on the current frame) instead of surfacing an error banner.
      if (msg.includes('no frame at or after')) return;
      renderError = msg;
    }
  }

  // Parse HH:MM:SS:FF into a stream-time-base PTS, given current info.
  function timecodeToPts(tc: string): number | null {
    if (!info) return null;
    const m = tc.match(/^(\d+):(\d+):(\d+)[:;](\d+)$/);
    if (!m) return null;
    const [_, hh, mm, ss, ff] = m;
    const fps =
      info.frame_rate_den !== 0 ? info.frame_rate_num / info.frame_rate_den : 24;
    const totalFrames =
      Number(hh) * 3600 * fps + Number(mm) * 60 * fps + Number(ss) * fps + Number(ff);
    const seconds = totalFrames / fps;
    // PTS in stream time base = seconds * (time_base_den / time_base_num)
    return Math.round(seconds * (info.time_base_den / info.time_base_num));
  }

  async function handleTimecodeSeek(e: SubmitEvent) {
    e.preventDefault();
    const pts = timecodeToPts(timecodeInput);
    if (pts == null) {
      renderError = 'invalid timecode (expected HH:MM:SS:FF)';
      return;
    }
    // If we're playing, the engine refuses single-shot decode_at because
    // the decoder is held by the scheduler. Stop, seek, then resume from
    // the requested PTS so playback continues from the new position.
    const wasPlaying = playing;
    if (wasPlaying) {
      try {
        await stopPlayback();
        playing = false;
      } catch {
        /* ignore */
      }
    }
    await render(pts);
    if (wasPlaying) {
      try {
        await startPlayback(lastPts ?? pts);
        playing = true;
      } catch (e) {
        playError = String(e);
      }
    }
  }

  async function togglePlayback() {
    playError = null;
    if (playing) {
      try {
        // Engine returns the PTS at which playback was stopped so the
        // next Play resumes from there (not from the original anchor).
        const resumePts = await stopPlayback();
        lastPts = resumePts;
        playing = false;
      } catch (e) {
        playError = String(e);
      }
    } else {
      try {
        // If the playhead is on/past the last frame of the file, rewind
        // to 0 so Play means "play the file again" (matches every NLE).
        // The last presented frame is one step short of duration_pts, so
        // compare against (duration_pts - step) rather than duration_pts.
        let from = lastPts ?? 0;
        const step = ptsPerFrame() ?? 1;
        if (
          info &&
          info.duration_pts != null &&
          from >= info.duration_pts - step
        ) {
          from = 0;
          lastPts = 0;
        }
        await startPlayback(from);
        playing = true;
      } catch (e) {
        playError = String(e);
      }
    }
  }
</script>

<div class="flex h-screen flex-col">
  <!-- Header -->
  <header
    class="border-vance-border bg-vance-panel flex h-11 flex-shrink-0 items-center gap-3 border-b px-4"
  >
    <h1 class="text-sm font-semibold">Vance</h1>
    <span class="text-vance-muted text-xs">Phase 4 · M4.2</span>

    {#if projectState.info}
      <span class="ml-3 text-xs text-vance-muted">
        Project: <span class="text-vance-fg font-medium">{projectState.info.name}</span>
        {#if projectState.path}
          <span class="text-vance-muted">·</span>
          <span class="text-vance-muted">{projectState.path}</span>
        {:else}
          <span class="text-vance-muted">·</span>
          <span class="text-yellow-400">unsaved</span>
        {/if}
      </span>
    {/if}

    <div class="ml-auto flex items-center gap-2">
      <button
        class="rounded bg-vance-bg border-vance-border border px-2 py-1 text-xs hover:brightness-110"
        onclick={handleNewProject}
        title="Create a new empty project"
      >
        New
      </button>
      <button
        class="rounded bg-vance-bg border-vance-border border px-2 py-1 text-xs hover:brightness-110"
        onclick={handleOpenProject}
        title="Open a .vance/ bundle"
      >
        Open Project
      </button>
      <button
        class="rounded bg-vance-bg border-vance-border border px-2 py-1 text-xs hover:brightness-110 disabled:opacity-50"
        onclick={handleSaveProject}
        disabled={!projectState.info}
        title="Save the current project"
      >
        Save
      </button>
      <button
        class="rounded bg-[var(--color-vance-accent)] px-3 py-1 text-xs font-semibold text-[#003040] hover:brightness-110"
        onclick={handleOpen}
      >
        Open File…
      </button>
    </div>
  </header>

  <!-- Main 3-pane area -->
  <div class="flex flex-1 min-h-0">
    <!-- Browser -->
    <aside
      class="border-vance-border bg-vance-panel w-60 flex-shrink-0 overflow-y-auto border-r p-4"
    >
      <h2 class="text-vance-muted mb-3 text-xs uppercase tracking-wider">Browser</h2>
      {#if projectState.info}
        <div class="mb-4">
          <h3 class="text-vance-muted mb-1 text-[11px] uppercase">Sequences</h3>
          {#if projectState.info.sequences.length === 0}
            <p class="text-vance-muted text-xs italic">No sequences yet.</p>
          {:else}
            <ul class="flex flex-col gap-1 text-xs">
              {#each projectState.info.sequences as seq (seq.id)}
                {@const active = seq.id === projectState.info?.current_sequence_id}
                <li>
                  <button
                    class="w-full rounded border px-2 py-1 text-left
                      {active
                        ? 'border-[var(--color-vance-accent)] bg-[#003040] text-[var(--color-vance-accent)]'
                        : 'bg-vance-bg border-vance-border hover:brightness-125'}"
                    onclick={() => projectState.selectSequence(seq.id)}
                  >
                    <div class="font-medium">{seq.name}</div>
                    <div class="text-[10px] text-vance-muted">{seq.clip_count} clips</div>
                  </button>
                </li>
              {/each}
            </ul>
          {/if}
        </div>

        <div class="mb-3 flex items-center justify-between">
          <h3 class="text-vance-muted text-[11px] uppercase">Media</h3>
          <button
            class="rounded bg-vance-bg border-vance-border border px-2 py-0.5 text-[10px] hover:brightness-125"
            onclick={handleImportMedia}
          >
            + Import
          </button>
        </div>
        {#if projectState.media.length === 0}
          <p class="text-vance-muted text-xs italic">
            Click + Import to add a video. Drag it onto the timeline below.
          </p>
        {:else}
          <ul class="flex flex-col gap-1 text-xs">
            {#each projectState.media as m (m.id)}
              <li>
                <div
                  class="cursor-grab rounded border border-vance-border bg-vance-bg px-2 py-1 active:cursor-grabbing hover:brightness-125"
                  role="button"
                  tabindex="0"
                  onmousedown={(e) => {
                    if (e.button !== 0) return;
                    e.preventDefault();
                    dragState.begin(m.id, m.name, e.clientX, e.clientY);
                    diagLog(`browser: mousedown media=${m.id}`);
                  }}
                  title="Drag onto the timeline"
                >
                  <div class="font-medium truncate">{m.name}</div>
                  <div class="text-[10px] text-vance-muted">
                    {m.kind} · {formatDuration(m.duration_seconds)}
                  </div>
                </div>
              </li>
            {/each}
          </ul>
        {/if}
      {:else}
        <p class="text-vance-muted text-xs">
          Create or open a project from the toolbar.
        </p>
      {/if}
      {#if projectState.error}
        <p class="mt-3 rounded border border-red-700 bg-red-950 p-2 text-xs text-red-200">
          {projectState.error}
        </p>
      {/if}
    </aside>

    <!-- Center: viewport + transport -->
    <main class="flex flex-1 flex-col min-w-0">
      <div
        class="m-4 flex-1 rounded border border-dashed border-[#333] bg-black"
        bind:this={viewportEl}
      >
        <!-- The native wgpu surface paints over this div. Anything inside
             will be visually covered (per ADR 0005). -->
      </div>
      <div
        class="border-vance-border bg-vance-panel flex h-12 flex-shrink-0 items-center gap-3 border-t px-4 text-xs"
      >
        <div class="flex items-center gap-1">
          <button
            class="rounded bg-vance-bg border-vance-border border px-2 py-1 disabled:opacity-50"
            onclick={() => jumpSeconds(-1)}
            disabled={!info}
            title="J: jump back 1s"
          >
            J
          </button>
          <button
            class="rounded bg-vance-bg border-vance-border border px-2 py-1 disabled:opacity-50"
            onclick={() => stepFrame(-1)}
            disabled={!info}
            title="←: step back 1 frame"
          >
            ‹
          </button>
          <button
            class="rounded bg-[var(--color-vance-accent)] px-3 py-1 font-semibold text-[#003040] disabled:opacity-50"
            onclick={togglePlayback}
            disabled={!info}
            title="Space / L: play, K: pause"
          >
            {playing ? 'Stop' : 'Play'}
          </button>
          <button
            class="rounded bg-vance-bg border-vance-border border px-2 py-1 disabled:opacity-50"
            onclick={() => stepFrame(1)}
            disabled={!info}
            title="→: step forward 1 frame"
          >
            ›
          </button>
          <button
            class="rounded bg-vance-bg border-vance-border border px-2 py-1 disabled:opacity-50"
            onclick={() => jumpSeconds(1)}
            disabled={!info}
            title="Shift+→: jump forward 1s"
          >
            L
          </button>
        </div>
        <form onsubmit={handleTimecodeSeek} class="flex items-center gap-2">
          <label class="text-vance-muted">
            Seek to TC
            <input
              class="bg-vance-bg border-vance-border ml-2 w-32 rounded border px-2 py-1 font-mono"
              bind:value={timecodeInput}
              placeholder="HH:MM:SS:FF"
            />
          </label>
          <button
            type="submit"
            class="rounded bg-[var(--color-vance-accent)] px-2 py-1 text-[#003040]"
            disabled={!info}
          >
            Go
          </button>
        </form>
        <span class="text-vance-muted ml-auto">
          last PTS: <span class="text-vance-fg font-mono">{lastPts ?? '—'}</span>
          {#if playError}
            <span class="ml-3 text-red-400">{playError}</span>
          {/if}
        </span>
      </div>
    </main>

    <!-- Inspector -->
    <aside
      class="border-vance-border bg-vance-panel w-72 flex-shrink-0 overflow-y-auto border-l p-4"
    >
      <h2 class="text-vance-muted mb-3 text-xs uppercase tracking-wider">Inspector</h2>
      {#if info}
        <dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 font-mono text-xs">
          <dt class="text-vance-muted">codec</dt>
          <dd>{info.codec_name}</dd>
          <dt class="text-vance-muted">size</dt>
          <dd>{info.width} × {info.height}</dd>
          <dt class="text-vance-muted">format</dt>
          <dd>{info.pixel_format}</dd>
          <dt class="text-vance-muted">rate</dt>
          <dd>{info.frame_rate_num}/{info.frame_rate_den}</dd>
          <dt class="text-vance-muted">tb</dt>
          <dd>{info.time_base_num}/{info.time_base_den}</dd>
          {#if info.duration_pts}
            <dt class="text-vance-muted">dur (pts)</dt>
            <dd>{info.duration_pts}</dd>
          {/if}
        </dl>
      {:else}
        <p class="text-vance-muted text-xs">No file open.</p>
      {/if}
      {#if openError}
        <p class="mt-3 rounded border border-red-700 bg-red-950 p-2 text-xs text-red-200">
          {openError}
        </p>
      {/if}
      {#if renderError}
        <p class="mt-3 rounded border border-red-700 bg-red-950 p-2 text-xs text-red-200">
          {renderError}
        </p>
      {/if}
    </aside>
  </div>

  <!-- Timeline panel -->
  <div
    class="border-vance-border bg-vance-panel h-56 flex-shrink-0 border-t"
    role="region"
    aria-label="Timeline"
  >
    <Timeline refreshTick={timelineRefreshTick} />
  </div>

  <!-- Footer -->
  <footer
    class="border-vance-border bg-vance-panel flex h-7 flex-shrink-0 items-center gap-4 border-t px-4 text-[11px]"
  >
    <span>backend: <span class="font-mono">{backend}</span></span>
  </footer>
</div>

{#if dragState.mediaId}
  <div
    class="pointer-events-none fixed z-50 rounded border border-[var(--color-vance-accent)] bg-[#003040] px-2 py-1 text-xs font-medium text-[var(--color-vance-accent)] shadow-lg"
    style:left="{dragState.cursorX + 8}px"
    style:top="{dragState.cursorY + 8}px"
  >
    {dragState.mediaName}
  </div>
{/if}
