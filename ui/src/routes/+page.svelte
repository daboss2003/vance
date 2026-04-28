<script lang="ts">
  import { onMount } from 'svelte';
  import { open } from '@tauri-apps/plugin-dialog';
  import {
    backendVersion,
    decodeAndRenderAt,
    openFile,
    setViewportBounds,
    type StreamInfo
  } from '$lib/ipc';

  let backend = $state<string>('checking…');
  let info = $state<StreamInfo | null>(null);
  let openError = $state<string | null>(null);
  let renderError = $state<string | null>(null);
  let lastPts = $state<number | null>(null);
  let timecodeInput = $state<string>('00:00:00:00');

  let viewportEl: HTMLDivElement | null = $state(null);

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
    reportBounds();
    return () => {
      observer.disconnect();
      window.removeEventListener('resize', reportBounds);
      window.removeEventListener('scroll', reportBounds, true);
    };
  });

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

  async function render(pts: number) {
    renderError = null;
    try {
      lastPts = await decodeAndRenderAt(pts);
    } catch (e) {
      renderError = String(e);
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

  function handleTimecodeSeek(e: SubmitEvent) {
    e.preventDefault();
    const pts = timecodeToPts(timecodeInput);
    if (pts == null) {
      renderError = 'invalid timecode (expected HH:MM:SS:FF)';
      return;
    }
    render(pts);
  }
</script>

<div class="flex h-screen flex-col">
  <!-- Header -->
  <header
    class="border-vance-border bg-vance-panel flex h-11 flex-shrink-0 items-center gap-3 border-b px-4"
  >
    <h1 class="text-sm font-semibold">Vance</h1>
    <span class="text-vance-muted text-xs">Phase 1 · M1.5</span>
    <button
      class="ml-auto rounded bg-[var(--color-vance-accent)] px-3 py-1 text-xs font-semibold text-[#003040] hover:brightness-110"
      onclick={handleOpen}
    >
      Open File…
    </button>
  </header>

  <!-- Main 3-pane area -->
  <div class="flex flex-1 min-h-0">
    <!-- Browser -->
    <aside
      class="border-vance-border bg-vance-panel w-60 flex-shrink-0 overflow-y-auto border-r p-4"
    >
      <h2 class="text-vance-muted mb-3 text-xs uppercase tracking-wider">Browser</h2>
      <p class="text-vance-muted text-xs">
        Open a file from the toolbar. Browser bin lands in Phase 4.
      </p>
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

  <!-- Footer -->
  <footer
    class="border-vance-border bg-vance-panel flex h-7 flex-shrink-0 items-center gap-4 border-t px-4 text-[11px]"
  >
    <span>backend: <span class="font-mono">{backend}</span></span>
  </footer>
</div>
