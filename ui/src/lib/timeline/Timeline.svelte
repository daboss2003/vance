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
  import { selectionState } from '$lib/selection.svelte';

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

  // Hover + marquee state for selection feedback.
  let hoverClipId = $state<string | null>(null);
  let marquee = $state<{ x0: number; y0: number; x1: number; y1: number } | null>(null);

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
  const SELECTED_STROKE = '#5ec5f5';
  const HOVER_STROKE = '#a8d8f0';
  const MARQUEE_FILL = 'rgba(94, 197, 245, 0.15)';
  const MARQUEE_STROKE = '#5ec5f5';

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

  // Redraw whenever the selection changes so external mutations
  // (e.g. Inspector later, keyboard shortcuts) update the canvas.
  $effect(() => {
    void selectionState.ids;
    scheduleDraw();
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
    drawMarquee(ctx);
    drawMoveDragOverlay(ctx, cssH);
    drawTrimSnapLine(ctx, cssH);
  }

  function drawTrimSnapLine(ctx: CanvasRenderingContext2D, cssH: number) {
    if (!trimDrag || trimDrag.snapAt == null) return;
    const sx = timeToX(trimDrag.snapAt);
    ctx.save();
    ctx.strokeStyle = SELECTED_STROKE;
    ctx.lineWidth = 1;
    ctx.setLineDash([3, 3]);
    ctx.beginPath();
    ctx.moveTo(Math.round(sx) + 0.5, 0);
    ctx.lineTo(Math.round(sx) + 0.5, cssH);
    ctx.stroke();
    ctx.restore();
  }

  /** Ghost outline at the clip's *original* position, plus a vertical
   *  snap line when a snap target is active. Only renders while a
   *  move-drag is in flight. */
  function drawMoveDragOverlay(ctx: CanvasRenderingContext2D, cssH: number) {
    if (!moveDrag) return;
    const y = trackY(moveDrag.trackIdx);
    const x = timeToX(moveDrag.originalTimelineIn);
    const w = moveDrag.durationSeconds * pixelsPerSecond;
    // Dashed grey outline at the source position.
    ctx.save();
    ctx.strokeStyle = '#5a6172';
    ctx.lineWidth = 1;
    ctx.setLineDash([4, 3]);
    ctx.strokeRect(x + 0.5, y + 4.5, Math.max(1, w - 1), TRACK_HEIGHT - 9);
    ctx.restore();

    if (moveDrag.snapAt != null) {
      const sx = timeToX(moveDrag.snapAt);
      ctx.save();
      ctx.strokeStyle = SELECTED_STROKE;
      ctx.lineWidth = 1;
      ctx.setLineDash([3, 3]);
      ctx.beginPath();
      ctx.moveTo(Math.round(sx) + 0.5, 0);
      ctx.lineTo(Math.round(sx) + 0.5, cssH);
      ctx.stroke();
      ctx.restore();
    }
  }

  function drawMarquee(ctx: CanvasRenderingContext2D) {
    if (!marquee) return;
    const x = Math.min(marquee.x0, marquee.x1);
    const y = Math.min(marquee.y0, marquee.y1);
    const w = Math.abs(marquee.x1 - marquee.x0);
    const h = Math.abs(marquee.y1 - marquee.y0);
    ctx.fillStyle = MARQUEE_FILL;
    ctx.fillRect(x, y, w, h);
    ctx.strokeStyle = MARQUEE_STROKE;
    ctx.lineWidth = 1;
    ctx.strokeRect(x + 0.5, y + 0.5, w, h);
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
    const baseFill = track.kind === 'video' ? VIDEO_FILL : AUDIO_FILL;
    const baseStroke = track.kind === 'video' ? VIDEO_STROKE : AUDIO_STROKE;

    for (const c of track.clips) {
      // While moving / trimming this clip, draw it at candidate bounds.
      const moving = moveDrag && moveDrag.clipId === c.id;
      const trimming = trimDrag && trimDrag.clipId === c.id;
      let t_in: number;
      let dur: number;
      if (trimming) {
        t_in = trimDrag!.candidateTimelineIn;
        dur = trimDrag!.candidateSourceOut - trimDrag!.candidateSourceIn;
      } else if (moving) {
        t_in = moveDrag!.candidateTimelineIn;
        dur = c.duration_seconds;
      } else {
        t_in = c.timeline_in_seconds;
        dur = c.duration_seconds;
      }
      const t_out = t_in + dur;
      // Cull clips entirely off-screen.
      if (t_out < visibleLeft || t_in > visibleRight) continue;

      const x = timeToX(t_in);
      const w = dur * pixelsPerSecond;
      const cy = y + 4;
      const ch = TRACK_HEIGHT - 8;

      const isSelected = selectionState.has(c.id);
      const isHovered = hoverClipId === c.id;

      // Clip body.
      ctx.fillStyle = c.enabled && track.enabled ? baseFill : DISABLED_FILL;
      roundRect(ctx, x, cy, Math.max(2, w), ch, 3);
      ctx.fill();

      // Border — thicker accent when selected, lighter when hovered.
      ctx.lineWidth = isSelected ? 2 : 1;
      ctx.strokeStyle = isSelected
        ? SELECTED_STROKE
        : isHovered
          ? HOVER_STROKE
          : baseStroke;
      const inset = isSelected ? 1 : 0.5;
      roundRect(
        ctx,
        x + inset,
        cy + inset,
        Math.max(1, w - 2 * inset),
        ch - 2 * inset,
        3
      );
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

      // Edge-handle hint: a brighter vertical bar on the side the
      // cursor is hovering, so the user can see the trim is grabable.
      if (hoverClipId === c.id && hoverEdge) {
        ctx.fillStyle = SELECTED_STROKE;
        if (hoverEdge === 'left') {
          ctx.fillRect(Math.round(x) - 1, cy, 3, ch);
        } else {
          ctx.fillRect(Math.round(x + w) - 2, cy, 3, ch);
        }
      }
    }
  }

  type HitKind = 'body' | 'left' | 'right';
  type Hit = {
    kind: HitKind;
    clip: ClipContents;
    track: TrackContents;
    trackIdx: number;
  };

  /** Hit-test: where on a clip (if any) is (localX, localY)? Returns
   *  `null` for empty area / ruler / header. Edge handles take a
   *  `HANDLE_PX`-wide region around each clip's left and right edges. */
  function hitAt(localX: number, localY: number): Hit | null {
    if (!sequence) return null;
    if (localX < TRACK_HEADER_W) return null;
    if (localY < RULER_HEIGHT) return null;
    const trackIdx = Math.floor((localY - RULER_HEIGHT) / TRACK_HEIGHT);
    if (trackIdx < 0 || trackIdx >= sequence.tracks.length) return null;
    const track = sequence.tracks[trackIdx];
    for (const c of track.clips) {
      const xLeft = timeToX(c.timeline_in_seconds);
      const xRight = xLeft + c.duration_seconds * pixelsPerSecond;
      // Test edges first so handles win even if the clip is short.
      if (Math.abs(localX - xLeft) <= HANDLE_PX) {
        return { kind: 'left', clip: c, track, trackIdx };
      }
      if (Math.abs(localX - xRight) <= HANDLE_PX) {
        return { kind: 'right', clip: c, track, trackIdx };
      }
      if (localX >= xLeft && localX <= xRight) {
        return { kind: 'body', clip: c, track, trackIdx };
      }
    }
    return null;
  }

  /** Kept as a thin shim for code that just needs the clip itself. */
  function clipAt(localX: number, localY: number): { clip: ClipContents; track: TrackContents } | null {
    const h = hitAt(localX, localY);
    return h ? { clip: h.clip, track: h.track } : null;
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

  type DragKind = 'pan' | 'marquee' | 'move' | 'trim-left' | 'trim-right';
  /** Width of the edge-grab region in CSS pixels. Cursor within this
   *  many pixels of a clip's left/right edge triggers a trim handle. */
  const HANDLE_PX = 6;

  /** While a move-drag is live, this carries the in-flight clip plus
   *  its anchor and current (snapped) candidate position. Cleared on
   *  mouseup. The canvas redraws the clip at `candidateTimelineIn`
   *  while this is set, ignoring the persisted `timeline_in`. */
  type MoveDrag = {
    trackId: string;
    trackIdx: number;
    clipId: string;
    originalTimelineIn: number;
    durationSeconds: number;
    /** Horizontal offset between the cursor and the clip's left edge
     *  at the moment the drag started, so the clip doesn't "jump" to
     *  align its left edge with the cursor. */
    grabOffsetSeconds: number;
    candidateTimelineIn: number;
    /** Snap line drawn on the canvas, if any. */
    snapAt: number | null;
  };
  let moveDrag = $state<MoveDrag | null>(null);

  /** While a trim-drag is live, we know which edge and the live
   *  candidate values to render the clip at. Committed on mouseup. */
  type TrimDrag = {
    trackId: string;
    trackIdx: number;
    clipId: string;
    edge: 'left' | 'right';
    /** Original anchors so we can compute the delta and constrain. */
    originalTimelineIn: number;
    originalSourceIn: number;
    originalSourceOut: number;
    /** Live candidates. */
    candidateTimelineIn: number;
    candidateSourceIn: number;
    candidateSourceOut: number;
    snapAt: number | null;
  };
  let trimDrag = $state<TrimDrag | null>(null);

  /** Set on every mousemove; drives the cursor style + handle hint. */
  let hoverEdge = $state<'left' | 'right' | null>(null);

  let dragStart:
    | {
        kind: DragKind;
        clientX: number;
        clientY: number;
        localX: number;
        localY: number;
        scroll: number;
        modKeys: { shift: boolean; cmd: boolean };
        downOnClipId: string | null;
      }
    | null = null;

  function onMouseDown(e: MouseEvent) {
    if (e.button !== 0) return;
    if (!canvas) return;
    const rect = canvas.getBoundingClientRect();
    const localX = e.clientX - rect.left;
    const localY = e.clientY - rect.top;
    const hit = hitAt(localX, localY);
    const kind: DragKind =
      hit?.kind === 'left'
        ? 'trim-left'
        : hit?.kind === 'right'
          ? 'trim-right'
          : hit?.kind === 'body'
            ? 'move'
            : 'marquee';
    dragStart = {
      kind,
      clientX: e.clientX,
      clientY: e.clientY,
      localX,
      localY,
      scroll: scrollSeconds,
      modKeys: { shift: e.shiftKey, cmd: e.metaKey || e.ctrlKey },
      downOnClipId: hit ? hit.clip.id : null
    };
  }

  function onMouseMove(e: MouseEvent) {
    // Track hover so clips light up under the cursor regardless of drag,
    // and the cursor flips to col-resize over an edge handle.
    if (canvas && !dragStart) {
      const rect = canvas.getBoundingClientRect();
      const hit = hitAt(e.clientX - rect.left, e.clientY - rect.top);
      const id = hit ? hit.clip.id : null;
      const edge = hit?.kind === 'left' || hit?.kind === 'right' ? hit.kind : null;
      if (id !== hoverClipId) {
        hoverClipId = id;
        scheduleDraw();
      }
      if (edge !== hoverEdge) {
        hoverEdge = edge;
        canvas.style.cursor = edge ? 'ew-resize' : '';
      }
    }

    if (!dragStart) return;
    const dx = e.clientX - dragStart.clientX;
    const dy = e.clientY - dragStart.clientY;
    if (Math.abs(dx) + Math.abs(dy) < 3) return; // ignore micro-jitter

    if (dragStart.kind === 'pan') {
      scrollSeconds = Math.max(0, dragStart.scroll - dx / pixelsPerSecond);
      scheduleDraw();
    } else if (dragStart.kind === 'marquee' && canvas) {
      const rect = canvas.getBoundingClientRect();
      marquee = {
        x0: dragStart.localX,
        y0: dragStart.localY,
        x1: e.clientX - rect.left,
        y1: e.clientY - rect.top
      };
      scheduleDraw();
    } else if (dragStart.kind === 'move' && dragStart.downOnClipId && sequence) {
      ensureMoveDrag(dragStart);
      if (moveDrag && canvas) {
        const rect = canvas.getBoundingClientRect();
        const cursorT = scrollSeconds + (e.clientX - rect.left - TRACK_HEADER_W) / pixelsPerSecond;
        let candidate = Math.max(0, cursorT - moveDrag.grabOffsetSeconds);
        const snap = e.shiftKey ? null : snapTimeline(candidate, moveDrag);
        if (snap != null) {
          candidate = snap;
          moveDrag.snapAt = snap;
        } else {
          moveDrag.snapAt = null;
        }
        moveDrag.candidateTimelineIn = candidate;
        scheduleDraw();
      }
    } else if (
      (dragStart.kind === 'trim-left' || dragStart.kind === 'trim-right') &&
      dragStart.downOnClipId &&
      sequence
    ) {
      ensureTrimDrag(dragStart);
      if (trimDrag && canvas) {
        const rect = canvas.getBoundingClientRect();
        const cursorT =
          scrollSeconds + (e.clientX - rect.left - TRACK_HEADER_W) / pixelsPerSecond;
        applyTrimCandidate(trimDrag, cursorT, e.shiftKey);
        scheduleDraw();
      }
    }
  }

  function ensureTrimDrag(ds: NonNullable<typeof dragStart>) {
    if (trimDrag || !sequence || !ds.downOnClipId) return;
    for (let i = 0; i < sequence.tracks.length; i++) {
      const track = sequence.tracks[i];
      const clip = track.clips.find((c) => c.id === ds.downOnClipId);
      if (!clip) continue;
      trimDrag = {
        trackId: track.id,
        trackIdx: i,
        clipId: clip.id,
        edge: ds.kind === 'trim-left' ? 'left' : 'right',
        originalTimelineIn: clip.timeline_in_seconds,
        originalSourceIn: clip.source_in_seconds,
        originalSourceOut: clip.source_in_seconds + clip.duration_seconds,
        candidateTimelineIn: clip.timeline_in_seconds,
        candidateSourceIn: clip.source_in_seconds,
        candidateSourceOut: clip.source_in_seconds + clip.duration_seconds,
        snapAt: null
      };
      return;
    }
  }

  /** Map the cursor's timeline position to new source_in / source_out
   *  (and timeline_in for left-edge trim), clamping to legal ranges
   *  and snapping to nearby clip edges / playhead unless `noSnap`. */
  function applyTrimCandidate(td: TrimDrag, cursorT: number, noSnap: boolean) {
    if (td.edge === 'left') {
      // Drag left edge to cursor. timeline_out stays fixed at
      // (originalTimelineIn + duration). new_timeline_in = cursorT,
      // new_source_in = originalSourceIn + (cursorT - originalTimelineIn).
      const timelineOut = td.originalTimelineIn + (td.originalSourceOut - td.originalSourceIn);
      // Don't let the user drag past timeline_out (zero-length).
      const maxTimelineIn = timelineOut - 1 / Math.max(1, pixelsPerSecond);
      let candTimelineIn = Math.min(Math.max(0, cursorT), maxTimelineIn);
      const snap = noSnap ? null : snapEdge(candTimelineIn, td.trackIdx, td.clipId);
      if (snap != null) {
        candTimelineIn = snap;
        td.snapAt = snap;
      } else {
        td.snapAt = null;
      }
      const delta = candTimelineIn - td.originalTimelineIn;
      const candSourceIn = Math.max(0, td.originalSourceIn + delta);
      td.candidateTimelineIn = candTimelineIn;
      td.candidateSourceIn = candSourceIn;
      td.candidateSourceOut = td.originalSourceOut;
    } else {
      // Drag right edge to cursor. timeline_in stays fixed; new
      // timeline_out = cursorT; new_source_out = originalSourceIn +
      // (cursorT - originalTimelineIn).
      const minTimelineOut = td.originalTimelineIn + 1 / Math.max(1, pixelsPerSecond);
      let candTimelineOut = Math.max(minTimelineOut, cursorT);
      const snap = noSnap ? null : snapEdge(candTimelineOut, td.trackIdx, td.clipId);
      if (snap != null) {
        candTimelineOut = snap;
        td.snapAt = snap;
      } else {
        td.snapAt = null;
      }
      const dur = candTimelineOut - td.originalTimelineIn;
      td.candidateTimelineIn = td.originalTimelineIn;
      td.candidateSourceIn = td.originalSourceIn;
      td.candidateSourceOut = td.originalSourceIn + dur;
    }
  }

  /** Snap a single timeline value to the closest of {playhead, t=0,
   *  every neighbor clip's edges on the same track} within
   *  `HANDLE_PX` pixels' worth of seconds. */
  function snapEdge(candidate: number, trackIdx: number, skipClipId: string): number | null {
    if (!sequence) return null;
    const tolSec = HANDLE_PX / pixelsPerSecond;
    const candidates: number[] = [0, playheadSeconds];
    const track = sequence.tracks[trackIdx];
    if (track) {
      for (const c of track.clips) {
        if (c.id === skipClipId) continue;
        candidates.push(c.timeline_in_seconds);
        candidates.push(c.timeline_in_seconds + c.duration_seconds);
      }
    }
    let bestDiff = tolSec + 1;
    let best: number | null = null;
    for (const t of candidates) {
      const d = Math.abs(candidate - t);
      if (d < bestDiff) {
        bestDiff = d;
        best = t;
      }
    }
    return best;
  }

  /** Initialize moveDrag the first time the move-drag exceeds the
   *  jitter threshold. Looks up the clip in the current sequence so
   *  we have its original anchor + duration to snap against. */
  function ensureMoveDrag(ds: NonNullable<typeof dragStart>) {
    if (moveDrag || !sequence || !ds.downOnClipId || !canvas) return;
    for (let i = 0; i < sequence.tracks.length; i++) {
      const track = sequence.tracks[i];
      const clip = track.clips.find((c) => c.id === ds.downOnClipId);
      if (!clip) continue;
      const rect = canvas.getBoundingClientRect();
      const cursorT =
        scrollSeconds + (ds.clientX - rect.left - TRACK_HEADER_W) / pixelsPerSecond;
      moveDrag = {
        trackId: track.id,
        trackIdx: i,
        clipId: clip.id,
        originalTimelineIn: clip.timeline_in_seconds,
        durationSeconds: clip.duration_seconds,
        grabOffsetSeconds: cursorT - clip.timeline_in_seconds,
        candidateTimelineIn: clip.timeline_in_seconds,
        snapAt: null
      };
      return;
    }
  }

  /** Find a snap target within 8 px of `candidateTimelineIn`. Tries:
   *  playhead, t=0, edges of *other* clips on the same track. Returns
   *  the snapped timeline_in seconds, or null for no snap. */
  function snapTimeline(candidate: number, m: MoveDrag): number | null {
    if (!sequence) return null;
    const tolPx = 8;
    const tolSec = tolPx / pixelsPerSecond;
    const candidates: number[] = [0, playheadSeconds];

    const myTrack = sequence.tracks[m.trackIdx];
    if (myTrack) {
      for (const c of myTrack.clips) {
        if (c.id === m.clipId) continue;
        candidates.push(c.timeline_in_seconds);
        candidates.push(c.timeline_in_seconds + c.duration_seconds);
      }
    }

    // Snap can be the clip's *left* edge OR its *right* edge to a
    // candidate, whichever is closer.
    let bestDiff = tolSec + 1;
    let bestTarget: number | null = null;
    for (const t of candidates) {
      const leftDiff = Math.abs(candidate - t);
      const rightDiff = Math.abs(candidate + m.durationSeconds - t);
      if (leftDiff < bestDiff) {
        bestDiff = leftDiff;
        bestTarget = t;
      }
      if (rightDiff < bestDiff) {
        bestDiff = rightDiff;
        bestTarget = t - m.durationSeconds;
      }
    }
    if (bestTarget == null) return null;
    return Math.max(0, bestTarget);
  }

  async function onMouseUp(e: MouseEvent) {
    if (!dragStart) return;
    const moved =
      Math.abs(e.clientX - dragStart.clientX) +
      Math.abs(e.clientY - dragStart.clientY);
    const ds = dragStart;
    dragStart = null;
    const wasMarquee = ds.kind === 'marquee' && marquee != null;
    const m = marquee;
    marquee = null;

    // ---- Move-drag release: commit via engine -----------------------------
    if (ds.kind === 'move' && moveDrag) {
      const md = moveDrag;
      moveDrag = null;
      const delta = md.candidateTimelineIn - md.originalTimelineIn;
      if (Math.abs(delta) < 1e-3) {
        // Effectively no move — fall through to the click path below.
      } else {
        await projectState.moveClip(md.trackId, md.clipId, md.candidateTimelineIn);
        // The Timeline's $effect on refreshTick will re-fetch the
        // sequence and redraw the clip at its new position.
        return;
      }
    }

    // ---- Trim-drag release: commit via engine ----------------------------
    if ((ds.kind === 'trim-left' || ds.kind === 'trim-right') && trimDrag) {
      const td = trimDrag;
      trimDrag = null;
      if (td.edge === 'left') {
        const delta = td.candidateSourceIn - td.originalSourceIn;
        if (Math.abs(delta) > 1e-3) {
          await projectState.trimIn(td.trackId, td.clipId, td.candidateSourceIn);
          return;
        }
      } else {
        const delta = td.candidateSourceOut - td.originalSourceOut;
        if (Math.abs(delta) > 1e-3) {
          await projectState.trimOut(td.trackId, td.clipId, td.candidateSourceOut);
          return;
        }
      }
      // Effectively no trim — fall through to click handling (selects).
    }

    // ---- Marquee release: select intersecting clips, no seek ------------
    if (wasMarquee && moved > 3 && m && sequence) {
      const ids = clipsIntersecting(m);
      if (ds.modKeys.shift || ds.modKeys.cmd) {
        // additive — keep existing selection, add intersected
        ids.forEach((id) => selectionState.add(id));
      } else {
        selectionState.replace(ids);
      }
      scheduleDraw();
      return;
    }

    // ---- Click (≤ 3 px) ---------------------------------------------------
    if (moved > 3 || !canvas || !sequence) return;

    if (ds.downOnClipId) {
      // Clip-click selection behavior (Premiere defaults).
      if (ds.modKeys.cmd) {
        selectionState.toggle(ds.downOnClipId);
      } else if (ds.modKeys.shift) {
        // Range-extend: select all clips on the same track between the
        // earliest selected and this one (inclusive). With no prior
        // selection this just selects the clicked clip.
        const t = sequence.tracks.find((tr) =>
          tr.clips.some((c) => c.id === ds.downOnClipId)
        );
        if (t) {
          const selectedOnTrack = t.clips.filter((c) => selectionState.has(c.id));
          if (selectedOnTrack.length === 0) {
            selectionState.selectOnly(ds.downOnClipId);
          } else {
            const target = t.clips.find((c) => c.id === ds.downOnClipId)!;
            const lo = Math.min(
              target.timeline_in_seconds,
              ...selectedOnTrack.map((c) => c.timeline_in_seconds)
            );
            const hi = Math.max(
              target.timeline_in_seconds,
              ...selectedOnTrack.map((c) => c.timeline_in_seconds)
            );
            const inRange = t.clips
              .filter((c) => c.timeline_in_seconds >= lo && c.timeline_in_seconds <= hi)
              .map((c) => c.id);
            selectionState.replace(inRange);
          }
        }
      } else {
        selectionState.selectOnly(ds.downOnClipId);
      }
      scheduleDraw();
      // Also seek the playhead to the click position — feels natural in
      // every NLE. The seek runs in the background; we don't await it.
      const localX = e.clientX - canvas.getBoundingClientRect().left;
      const t = Math.max(0, scrollSeconds + (localX - TRACK_HEADER_W) / pixelsPerSecond);
      void seekToTimeline(t);
      return;
    }

    // ---- Click in empty area or ruler ----------------------------------
    if (!ds.modKeys.shift && !ds.modKeys.cmd && selectionState.count > 0) {
      selectionState.clear();
      scheduleDraw();
    }
    const rect = canvas.getBoundingClientRect();
    const localX = e.clientX - rect.left;
    if (localX < TRACK_HEADER_W) return;
    const t = Math.max(0, scrollSeconds + (localX - TRACK_HEADER_W) / pixelsPerSecond);
    await seekToTimeline(t);
  }

  /** Seek the engine to `t` and park the playhead. Used by both
   *  click-on-clip (background, no await) and click-empty. */
  async function seekToTimeline(t: number) {
    try {
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
      await diagLog(`timeline: seek failed at t=${t.toFixed(3)}: ${String(err)}`);
    }
  }

  /** Which clip IDs intersect a marquee rect (in canvas-local px)? */
  function clipsIntersecting(rect: { x0: number; y0: number; x1: number; y1: number }): string[] {
    if (!sequence) return [];
    const xLo = Math.min(rect.x0, rect.x1);
    const xHi = Math.max(rect.x0, rect.x1);
    const yLo = Math.min(rect.y0, rect.y1);
    const yHi = Math.max(rect.y0, rect.y1);
    const ids: string[] = [];
    sequence.tracks.forEach((track, rowIdx) => {
      const y = trackY(rowIdx) + 4;
      const yh = TRACK_HEIGHT - 8;
      if (y + yh < yLo || y > yHi) return;
      for (const c of track.clips) {
        const cx = timeToX(c.timeline_in_seconds);
        const cw = c.duration_seconds * pixelsPerSecond;
        if (cx + cw < xLo || cx > xHi) continue;
        ids.push(c.id);
      }
    });
    return ids;
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
    onmouseleave={() => {
      if (hoverClipId !== null) {
        hoverClipId = null;
        scheduleDraw();
      }
    }}
  ></canvas>
</div>
