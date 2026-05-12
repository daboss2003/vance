<script lang="ts">
  // Read-only timeline canvas for Phase 4.3.
  //
  // Renders the active sequence's tracks and clips onto a 2D canvas.
  // Pan: shift+wheel or click-drag. Zoom: ctrl/cmd + wheel.
  // No editing yet — that's 4.4+.
  //
  // We use a plain 2D context here, not WebGL2; the read-only render
  // for hundreds of clips is not GPU-bound. WebGL2 with batched
  // rectangles will land alongside 4.x perf work for 1000+ clips.

  import { onMount } from 'svelte';
  import {
    currentSequenceContents,
    currentTimelineSeconds,
    decodeSequenceAt,
    diagLog,
    isPlaying,
    stopPlayback,
    type SequenceContents,
    type TrackContents,
    type ClipContents
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { dragState } from '$lib/drag.svelte';

  let { refreshTick = 0 }: { refreshTick?: number } = $props();

  let canvas = $state<HTMLCanvasElement | null>(null);
  let wrap = $state<HTMLDivElement | null>(null);
  let sequence = $state<SequenceContents | null>(null);

  // Viewport: pixelsPerSecond is the zoom; scrollSeconds is how many
  // seconds are clipped off the left edge of the visible area.
  let pixelsPerSecond = $state<number>(60);
  let scrollSeconds = $state<number>(0);

  // Engine's current playhead, fetched from `current_timeline_seconds`.
  // Updated by the playback poll set up in onMount.
  let playheadSeconds = $state<number>(0);

  // Layout constants.
  const RULER_HEIGHT = 22; // px
  const TRACK_HEIGHT = 36; // px per track row
  const TRACK_HEADER_W = 56; // px reserved on the left for V1/A1 labels

  // Track rows are alternating shaded.
  const VIDEO_FILL = '#3a4252';
  const VIDEO_STROKE = '#88c0d0';
  const AUDIO_FILL = '#3a4a3a';
  const AUDIO_STROKE = '#a3be8c';
  const DISABLED_FILL = '#2a2a2a';
  const ROW_BG = ['#1f2228', '#23272f'];
  const RULER_BG = '#16181c';
  const RULER_TEXT = '#9099a8';
  const PLAYHEAD = '#ffd166';
  const TRACK_HEADER_BG = '#16181c';
  const TRACK_HEADER_TEXT = '#9099a8';

  // Subscribe to the engine's sequence — refresh whenever the parent
  // bumps `refreshTick`. (4.4+ will switch to a proper edit-log
  // subscription; this is enough for read-only display.)
  $effect(() => {
    void refreshTick;
    currentSequenceContents()
      .then((s) => {
        sequence = s;
        scheduleDraw();
      })
      .catch((e) => console.error('currentSequenceContents', e));
  });

  // Re-draw the canvas. Idempotent; draws into the canvas at its
  // current physical-pixel size with the chrome's devicePixelRatio.
  let drawScheduled = false;
  function scheduleDraw() {
    if (drawScheduled) return;
    drawScheduled = true;
    requestAnimationFrame(() => {
      drawScheduled = false;
      draw();
    });
  }

  function resizeCanvas() {
    if (!canvas || !wrap) return;
    const dpr = window.devicePixelRatio || 1;
    const w = wrap.clientWidth;
    const h = wrap.clientHeight;
    canvas.width = Math.max(1, Math.floor(w * dpr));
    canvas.height = Math.max(1, Math.floor(h * dpr));
    canvas.style.width = w + 'px';
    canvas.style.height = h + 'px';
    scheduleDraw();
  }

  function draw() {
    if (!canvas) return;
    const dpr = window.devicePixelRatio || 1;
    const ctx = canvas.getContext('2d');
    if (!ctx) return;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);

    const cssW = canvas.width / dpr;
    const cssH = canvas.height / dpr;

    // Background.
    ctx.fillStyle = '#0d0f12';
    ctx.fillRect(0, 0, cssW, cssH);

    if (!sequence || sequence.tracks.length === 0) {
      ctx.fillStyle = '#5a6172';
      ctx.font = '12px ui-sans-serif, system-ui, sans-serif';
      ctx.textBaseline = 'middle';
      ctx.fillText(
        sequence ? 'No tracks on this sequence yet.' : 'No sequence loaded.',
        TRACK_HEADER_W + 12,
        cssH / 2
      );
      return;
    }

    drawRuler(ctx, cssW);
    drawTracks(ctx, cssW, cssH);
    drawPlayhead(ctx, cssH);
    drawTrackHeaders(ctx, cssH);
  }

  // ---- Ruler --------------------------------------------------------------

  /** Pick a "nice" tick interval (in seconds) given the current zoom. */
  function chooseTickInterval(): number {
    // Aim for a tick every ~80px.
    const target = 80 / pixelsPerSecond;
    const candidates = [
      0.04, 0.1, 0.2, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600
    ];
    for (const c of candidates) {
      if (c >= target) return c;
    }
    return candidates[candidates.length - 1];
  }

  function formatSeconds(t: number): string {
    if (t < 0) t = 0;
    const total = Math.floor(t);
    const ms = Math.round((t - total) * 1000);
    const h = Math.floor(total / 3600);
    const m = Math.floor((total % 3600) / 60);
    const s = total % 60;
    if (h > 0) {
      return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
    }
    if (ms === 0) {
      return `${m}:${String(s).padStart(2, '0')}`;
    }
    return `${m}:${String(s).padStart(2, '0')}.${String(ms).padStart(3, '0')}`;
  }

  function timeToX(t: number): number {
    return TRACK_HEADER_W + (t - scrollSeconds) * pixelsPerSecond;
  }

  function drawRuler(ctx: CanvasRenderingContext2D, cssW: number) {
    ctx.fillStyle = RULER_BG;
    ctx.fillRect(0, 0, cssW, RULER_HEIGHT);

    const interval = chooseTickInterval();
    const left_t = scrollSeconds - 1;
    const right_t = scrollSeconds + (cssW - TRACK_HEADER_W) / pixelsPerSecond + 1;

    ctx.strokeStyle = '#2a2e36';
    ctx.fillStyle = RULER_TEXT;
    ctx.font = '10px ui-monospace, monospace';
    ctx.textBaseline = 'middle';
    ctx.lineWidth = 1;

    const first = Math.ceil(left_t / interval) * interval;
    for (let t = first; t <= right_t; t += interval) {
      const x = timeToX(t);
      if (x < TRACK_HEADER_W) continue;
      ctx.beginPath();
      ctx.moveTo(Math.round(x) + 0.5, RULER_HEIGHT - 6);
      ctx.lineTo(Math.round(x) + 0.5, RULER_HEIGHT);
      ctx.stroke();
      ctx.fillText(formatSeconds(t), x + 3, RULER_HEIGHT / 2);
    }

    // Bottom border of ruler.
    ctx.strokeStyle = '#2a2e36';
    ctx.beginPath();
    ctx.moveTo(0, RULER_HEIGHT + 0.5);
    ctx.lineTo(cssW, RULER_HEIGHT + 0.5);
    ctx.stroke();
  }

  // ---- Tracks + clips -----------------------------------------------------

  function trackY(rowIdx: number): number {
    return RULER_HEIGHT + rowIdx * TRACK_HEIGHT;
  }

  function drawTracks(ctx: CanvasRenderingContext2D, cssW: number, cssH: number) {
    if (!sequence) return;
    const visibleLeft = scrollSeconds;
    const visibleRight = scrollSeconds + (cssW - TRACK_HEADER_W) / pixelsPerSecond;

    sequence.tracks.forEach((track, rowIdx) => {
      const y = trackY(rowIdx);
      // Row background — alternating, slightly muted when locked.
      ctx.fillStyle = ROW_BG[rowIdx % 2];
      ctx.fillRect(TRACK_HEADER_W, y, cssW - TRACK_HEADER_W, TRACK_HEIGHT);

      drawClips(ctx, track, y, visibleLeft, visibleRight);

      // Row separator.
      ctx.strokeStyle = '#2a2e36';
      ctx.beginPath();
      ctx.moveTo(TRACK_HEADER_W, y + TRACK_HEIGHT - 0.5);
      ctx.lineTo(cssW, y + TRACK_HEIGHT - 0.5);
      ctx.stroke();
    });

    void cssH;
  }

  function drawClips(
    ctx: CanvasRenderingContext2D,
    track: TrackContents,
    y: number,
    visibleLeft: number,
    visibleRight: number
  ) {
    const fill = track.kind === 'video' ? VIDEO_FILL : AUDIO_FILL;
    const stroke = track.kind === 'video' ? VIDEO_STROKE : AUDIO_STROKE;

    for (const c of track.clips) {
      const t_in = c.timeline_in_seconds;
      const t_out = t_in + c.duration_seconds;
      // Cull clips entirely off-screen.
      if (t_out < visibleLeft || t_in > visibleRight) continue;

      const x = timeToX(t_in);
      const w = c.duration_seconds * pixelsPerSecond;
      const cy = y + 4;
      const ch = TRACK_HEIGHT - 8;

      // Clip body.
      ctx.fillStyle = c.enabled && track.enabled ? fill : DISABLED_FILL;
      roundRect(ctx, x, cy, Math.max(2, w), ch, 3);
      ctx.fill();

      // Border.
      ctx.strokeStyle = stroke;
      ctx.lineWidth = 1;
      roundRect(ctx, x + 0.5, cy + 0.5, Math.max(1, w - 1), ch - 1, 3);
      ctx.stroke();

      // Label, if there's room.
      if (w > 24) {
        ctx.fillStyle = '#e6ebf5';
        ctx.font = '11px ui-sans-serif, system-ui, sans-serif';
        ctx.textBaseline = 'middle';
        const text = formatClipLabel(c, w - 8);
        ctx.save();
        ctx.beginPath();
        ctx.rect(x + 4, cy + 2, w - 8, ch - 4);
        ctx.clip();
        ctx.fillText(text, x + 4, cy + ch / 2);
        ctx.restore();
      }
    }
  }

  function formatClipLabel(c: ClipContents, _maxPxWidth: number): string {
    return c.name && c.name.length > 0 ? c.name : c.id.slice(0, 8);
  }

  function roundRect(
    ctx: CanvasRenderingContext2D,
    x: number,
    y: number,
    w: number,
    h: number,
    r: number
  ) {
    const rr = Math.min(r, w / 2, h / 2);
    ctx.beginPath();
    ctx.moveTo(x + rr, y);
    ctx.arcTo(x + w, y, x + w, y + h, rr);
    ctx.arcTo(x + w, y + h, x, y + h, rr);
    ctx.arcTo(x, y + h, x, y, rr);
    ctx.arcTo(x, y, x + w, y, rr);
    ctx.closePath();
  }

  // ---- Track headers ------------------------------------------------------

  function drawTrackHeaders(ctx: CanvasRenderingContext2D, cssH: number) {
    if (!sequence) return;
    ctx.fillStyle = TRACK_HEADER_BG;
    ctx.fillRect(0, RULER_HEIGHT, TRACK_HEADER_W, cssH - RULER_HEIGHT);
    ctx.strokeStyle = '#2a2e36';
    ctx.beginPath();
    ctx.moveTo(TRACK_HEADER_W + 0.5, RULER_HEIGHT);
    ctx.lineTo(TRACK_HEADER_W + 0.5, cssH);
    ctx.stroke();

    ctx.fillStyle = TRACK_HEADER_TEXT;
    ctx.font = '11px ui-sans-serif, system-ui, sans-serif';
    ctx.textBaseline = 'middle';
    sequence.tracks.forEach((track, rowIdx) => {
      const y = trackY(rowIdx);
      ctx.fillText(track.name, 6, y + TRACK_HEIGHT / 2);
    });
  }

  // ---- Playhead -----------------------------------------------------------

  function drawPlayhead(ctx: CanvasRenderingContext2D, cssH: number) {
    const x = timeToX(playheadSeconds);
    if (x < TRACK_HEADER_W) return;
    ctx.strokeStyle = PLAYHEAD;
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(Math.round(x) + 0.5, 0);
    ctx.lineTo(Math.round(x) + 0.5, cssH);
    ctx.stroke();
    // Small filled triangle on the ruler so the playhead is easy to grab.
    ctx.fillStyle = PLAYHEAD;
    ctx.beginPath();
    ctx.moveTo(x - 5, 0);
    ctx.lineTo(x + 5, 0);
    ctx.lineTo(x, 8);
    ctx.closePath();
    ctx.fill();
  }

  // ---- Wheel / drag interaction ------------------------------------------

  function onWheel(e: WheelEvent) {
    if (e.ctrlKey || e.metaKey) {
      // Zoom around the cursor's time position.
      e.preventDefault();
      const rect = (canvas as HTMLCanvasElement).getBoundingClientRect();
      const cursorX = e.clientX - rect.left;
      const tAtCursor = scrollSeconds + (cursorX - TRACK_HEADER_W) / pixelsPerSecond;
      const factor = Math.exp(-e.deltaY * 0.0015);
      const next = Math.max(2, Math.min(2000, pixelsPerSecond * factor));
      pixelsPerSecond = next;
      scrollSeconds = Math.max(0, tAtCursor - (cursorX - TRACK_HEADER_W) / pixelsPerSecond);
      scheduleDraw();
      return;
    }
    // Horizontal pan: shift+wheel uses deltaY, plain wheel respects deltaX.
    e.preventDefault();
    const dx = (e.shiftKey ? e.deltaY : e.deltaX) || e.deltaY;
    scrollSeconds = Math.max(0, scrollSeconds + dx / pixelsPerSecond);
    scheduleDraw();
  }

  let dragStart: { x: number; clientX: number; clientY: number; scroll: number } | null = null;
  function onMouseDown(e: MouseEvent) {
    if (e.button !== 0) return;
    dragStart = {
      x: e.clientX,
      clientX: e.clientX,
      clientY: e.clientY,
      scroll: scrollSeconds
    };
  }
  function onMouseMove(e: MouseEvent) {
    if (!dragStart) return;
    const dx = e.clientX - dragStart.x;
    if (Math.abs(dx) < 3) return; // ignore micro-jitter so clicks survive
    scrollSeconds = Math.max(0, dragStart.scroll - dx / pixelsPerSecond);
    scheduleDraw();
  }
  async function onMouseUp(e: MouseEvent) {
    if (!dragStart) return;
    const moved =
      Math.abs(e.clientX - dragStart.clientX) +
      Math.abs(e.clientY - dragStart.clientY);
    dragStart = null;
    // Click (≤ 3 px) seeks the sequence to that timeline position.
    if (moved > 3 || !canvas || !sequence) return;
    const rect = canvas.getBoundingClientRect();
    const localX = e.clientX - rect.left;
    if (localX < TRACK_HEADER_W) return;
    const t = Math.max(0, scrollSeconds + (localX - TRACK_HEADER_W) / pixelsPerSecond);
    try {
      // `decode_sequence_at` refuses while playback is in flight;
      // stop first so the click always lands.
      if (await isPlaying()) {
        try {
          await stopPlayback();
        } catch {
          /* ignore */
        }
      }
      await decodeSequenceAt(t);
      playheadSeconds = t;
      scheduleDraw();
    } catch (err) {
      await diagLog(`timeline: click-seek failed at t=${t.toFixed(3)}: ${String(err)}`);
    }
  }

  // ---- Media drag-drop from Browser --------------------------------------

  /** Map a (canvas-local) cursor position to (timeline_seconds, trackIdx). */
  function dropTargetAt(localX: number, localY: number): { t: number; trackIdx: number } | null {
    if (!sequence) return null;
    if (localX < TRACK_HEADER_W) return null;
    const t = scrollSeconds + (localX - TRACK_HEADER_W) / pixelsPerSecond;
    if (localY < RULER_HEIGHT) return null;
    const trackIdx = Math.floor((localY - RULER_HEIGHT) / TRACK_HEIGHT);
    if (trackIdx < 0 || trackIdx >= sequence.tracks.length) return null;
    return { t: Math.max(0, t), trackIdx };
  }

  /** Global mouseup handler: if we're currently drag-carrying a
   *  media id and the release lands inside this canvas, insert the
   *  clip and clear the drag state. */
  async function onWindowMouseUp(e: MouseEvent) {
    const mediaId = dragState.mediaId;
    if (!mediaId) return;
    if (!canvas || !sequence) return;
    const rect = canvas.getBoundingClientRect();
    if (
      e.clientX < rect.left ||
      e.clientX > rect.right ||
      e.clientY < rect.top ||
      e.clientY > rect.bottom
    ) {
      return;
    }
    // We claim this drop — clear dragState before any await so the
    // global mouseup handler in the parent doesn't double-clear.
    dragState.end();
    const localX = e.clientX - rect.left;
    const localY = e.clientY - rect.top;
    const target = dropTargetAt(localX, localY);
    await diagLog(
      `timeline: drop at localX=${localX.toFixed(0)} localY=${localY.toFixed(0)} target=${
        target ? `t=${target.t.toFixed(2)} trackIdx=${target.trackIdx}` : 'null'
      }`
    );
    if (!target) return;
    const track = sequence.tracks[target.trackIdx];
    await diagLog(`timeline: inserting on track ${track.id} (${track.name})`);
    await projectState.insertClip(track.id, mediaId, target.t);
    await diagLog('timeline: insertClip returned');
  }

  // ---- Mount --------------------------------------------------------------

  onMount(() => {
    if (!wrap) return;
    const ro = new ResizeObserver(resizeCanvas);
    ro.observe(wrap);
    resizeCanvas();
    window.addEventListener('mousemove', onMouseMove);
    window.addEventListener('mouseup', onMouseUp);
    // Drag-from-Browser-to-canvas uses our own mouse-based drag (Tauri
    // disables HTML5 native drag-and-drop). On mouseup we check
    // dragState and consume it if the cursor is inside our canvas.
    window.addEventListener('mouseup', onWindowMouseUp);

    // Playhead poll: fetch the engine's running playhead ~10×/sec so
    // the canvas reflects live playback position.
    const playheadPoll = window.setInterval(async () => {
      try {
        const t = await currentTimelineSeconds();
        if (t != null && Math.abs(t - playheadSeconds) > 1e-3) {
          playheadSeconds = t;
          scheduleDraw();
        }
      } catch {
        // ignore transient errors
      }
    }, 100);

    return () => {
      ro.disconnect();
      window.removeEventListener('mousemove', onMouseMove);
      window.removeEventListener('mouseup', onMouseUp);
      window.removeEventListener('mouseup', onWindowMouseUp);
      window.clearInterval(playheadPoll);
    };
  });
</script>

<div class="flex h-full w-full flex-col" bind:this={wrap}>
  <canvas
    bind:this={canvas}
    class="block h-full w-full cursor-grab active:cursor-grabbing"
    onwheel={onWheel}
    onmousedown={onMouseDown}
  ></canvas>
</div>
