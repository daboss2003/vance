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
    currentTimelineSeconds,
    decodeSequenceAt,
    diagLog,
    isPlaying,
    stopPlayback,
    type TrackContents,
    type ClipContents,
    type GainKeyframe,
    type FadeCurveName
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { dragState } from '$lib/drag.svelte';
  import { selectionState } from '$lib/selection.svelte';
  import { peaksCache, chooseLevel } from '$lib/peaks-cache.svelte';
  import { metersStore, dbToFill, type MeterDisplay } from '$lib/meters.svelte';
  import {
    yForDb,
    dbForY,
    clampDb,
    nextFadeCurve,
    normalizeLane,
    fadeCurveLetter
  } from './automation-geom';
  import {
    drawGainLane,
    drawFade,
    drawChip,
    drawDragTag,
    drawZeroBaseline,
    fadeDragTag,
    keyframeDragTag,
    CLIP_GAIN_COLOR,
    TRACK_GAIN_COLOR
  } from './automation-draw';
  import {
    hitFadeGrip,
    hitFadeChip,
    hitClipKeyframe,
    hitTrackKeyframe,
    fadeChipGeomFor,
    CHIP_W,
    CHIP_H,
    KF_HITBOX,
    type AutomationHit
  } from './automation-hit';

  let { refreshTick = 0 }: { refreshTick?: number } = $props();

  let canvas = $state<HTMLCanvasElement | null>(null);
  let wrap = $state<HTMLDivElement | null>(null);
  // The sequence lives in projectState now (single source of truth
  // shared with the Inspector). Aliased here for readability.
  const sequence = $derived(projectState.sequence);

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
  // Phase 7.5 M1 — header widened by 18 px so a stereo meter strip
  // (2× 6 px bars + 2 px gap + 4 px margin = ~18 px) can sit between
  // the M/S buttons and the timeline edge without overlapping either.
  const TRACK_HEADER_W = 106; // px reserved on the left
  // Mixer button geometry inside the track header.
  const MIXER_BTN_SIZE = 16; // square button (px)
  const MIXER_BTN_GAP = 3; // gap between M and S
  const MIXER_BTN_X = 44; // first button's x (after label area)
  // Phase 7.5 M1 stereo meter strip geometry.
  const METER_BAR_W = 6;
  const METER_BAR_GAP = 2;
  const METER_STRIP_W = METER_BAR_W * 2 + METER_BAR_GAP;
  const METER_STRIP_X = TRACK_HEADER_W - METER_STRIP_W - 4;

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

  // Redraw whenever the shared sequence rune updates. projectState
  // refreshes that rune after every successful edit op (and on
  // project load), so the canvas stays in sync without its own fetch.
  $effect(() => {
    void projectState.sequence;
    void refreshTick;
    scheduleDraw();
  });

  // Redraw whenever the selection changes so external mutations
  // (e.g. Inspector later, keyboard shortcuts) update the canvas.
  $effect(() => {
    void selectionState.ids;
    scheduleDraw();
  });

  // Redraw whenever the peaks cache mutates (a fetch landed or an
  // invalidate fired). Reading .version inside this $effect is what
  // makes the redraw reactive — the cache's request() calls inside
  // drawWaveform read but don't subscribe Svelte effects.
  $effect(() => {
    void peaksCache.version;
    scheduleDraw();
  });

  // Redraw when the media library mutates (e.g. Phase 7.2 backfill
  // flips has_audio_track on legacy videos). drawClips reads
  // projectState.media.find(...) inside drawWaveform's gating
  // expression; that read is in the draw function, not in a $effect,
  // so we need an explicit subscription here.
  $effect(() => {
    void projectState.media;
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
    const candidates = [0.04, 0.1, 0.2, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600];
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

      // Phase 7.4 M5 — track-wide gain lane underlay. Drawn BEFORE clips
      // so clip bodies overpaint it inside their rects, then drawClips
      // re-applies the amber overlay at reduced alpha on top.
      drawTrackGainLane(ctx, track, y, visibleLeft, visibleRight);

      drawClips(ctx, track, y, visibleLeft, visibleRight);
      drawTransitions(ctx, track, y, visibleLeft, visibleRight);

      // Row separator.
      ctx.strokeStyle = '#2a2e36';
      ctx.beginPath();
      ctx.moveTo(TRACK_HEADER_W, y + TRACK_HEIGHT - 0.5);
      ctx.lineTo(cssW, y + TRACK_HEIGHT - 0.5);
      ctx.stroke();
    });

    // Phase 7.4 M5 — drag readout floats above everything.
    if (cursorCss && (fadeDrag || kfDrag)) {
      const tagText = dragReadoutText();
      if (tagText) drawDragTag(ctx, cursorCss.x, cursorCss.y, tagText);
    }

    void cssH;
  }

  /** Drag readout text builder, kept inline so both fade and keyframe
   *  drags route through one call site. */
  function dragReadoutText(): string | null {
    if (fadeDrag) {
      return fadeDragTag(fadeDrag.side, fadeDrag.candidateDurationSeconds, fadeDrag.curve);
    }
    if (kfDrag) {
      const k = kfDrag.candidateLane[kfDrag.index];
      if (!k) return null;
      return keyframeDragTag(k.time_seconds, k.value, k.interp);
    }
    return null;
  }

  /** Phase 7.4 M5 — render a track-wide gain automation lane across the
   *  full row, including over clip bodies. Skipped for non-audio tracks
   *  to keep the visual quiet. */
  function drawTrackGainLane(
    ctx: CanvasRenderingContext2D,
    track: TrackContents,
    y: number,
    visibleLeft: number,
    visibleRight: number
  ) {
    if (track.kind !== 'audio') return;
    const laneX = TRACK_HEADER_W;
    const laneW = (ctx.canvas as HTMLCanvasElement).clientWidth - TRACK_HEADER_W;
    if (laneW <= 0) return;
    const { top, height } = trackLaneRect(y);
    // Always show the 0 dB baseline so empty tracks telegraph "drop a
    // keyframe here." Reinforce alpha when the lane has zero keyframes.
    drawZeroBaseline(ctx, laneX, top, laneW, height, track.gain_keyframes.length === 0);
    // Use the local kfDrag preview lane when the user is moving a track
    // keyframe so the polyline tracks the cursor live.
    const lane =
      kfDrag && kfDrag.scope === 'track' && kfDrag.trackId === track.id
        ? kfDrag.candidateLane
        : track.gain_keyframes;
    if (lane.length === 0) return;
    const selectedIdx =
      selectedKf && selectedKf.scope === 'track' && selectedKf.trackId === track.id
        ? selectedKf.index
        : null;
    drawGainLane(
      ctx,
      lane,
      laneX,
      top,
      laneW,
      height,
      (t) => timeToX(t),
      { start: Math.max(0, visibleLeft - 0.5), end: visibleRight + 0.5 },
      TRACK_GAIN_COLOR,
      selectedIdx,
      0.85
    );
  }

  // Waveform sub-pass styling. Translucent RMS fill underneath the
  // signed min/max envelope, both clipped to the clip's rounded body.
  const WAVE_RMS_FILL = 'rgba(255, 255, 255, 0.18)';
  const WAVE_ENV_STROKE = 'rgba(230, 235, 245, 0.85)';

  function drawClips(
    ctx: CanvasRenderingContext2D,
    track: TrackContents,
    y: number,
    visibleLeft: number,
    visibleRight: number
  ) {
    const baseFill = track.kind === 'video' ? VIDEO_FILL : AUDIO_FILL;
    const baseStroke = track.kind === 'video' ? VIDEO_STROKE : AUDIO_STROKE;
    // Audio/Video are the only kinds that can carry a waveform; skip
    // the per-clip media lookup entirely for image/title-only tracks.
    // Build a small per-track Map<id, MediaItemInfo> so the per-clip
    // gate is O(1) instead of O(N×M) per draw.
    const trackCanCarryAudio = track.kind === 'audio' || track.kind === 'video';
    const mediaIndex = trackCanCarryAudio
      ? new Map(projectState.media.map((m) => [m.id, m]))
      : null;

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

      // Audio waveform overlay (Phase 7.1 M5 / 7.2). Renders the
      // cached peaks for this clip's source range on top of the clip
      // body and underneath the border. Visually no-op while the
      // cache is still fetching — the underlying clip fill shows
      // through. Phase 7.2 extends this to Video clips whose backing
      // media has an embedded audio track (looked up via mediaIndex).
      if (c.enabled && track.enabled && mediaIndex) {
        const media = mediaIndex.get(c.media_id);
        const clipHasAudio =
          !!media && (media.kind === 'audio' || (media.kind === 'video' && media.has_audio_track));
        if (clipHasAudio) {
          const t_in_eff = trimming || moving ? t_in : c.timeline_in_seconds;
          const source_in_eff = trimming ? trimDrag!.candidateSourceIn : c.source_in_seconds;
          drawWaveform(ctx, c, x, cy, w, ch, t_in_eff, source_in_eff, dur);
        }
      }

      // Border — thicker accent when selected, lighter when hovered.
      ctx.lineWidth = isSelected ? 2 : 1;
      ctx.strokeStyle = isSelected ? SELECTED_STROKE : isHovered ? HOVER_STROKE : baseStroke;
      const inset = isSelected ? 1 : 0.5;
      roundRect(ctx, x + inset, cy + inset, Math.max(1, w - 2 * inset), ch - 2 * inset, 3);
      ctx.stroke();

      // Phase 7.4 M5 — fade wedges + grips, drawn inside the clip body
      // so they clip against the rounded edge. Reads liveFade so the
      // user sees the wedge follow the cursor during a fade drag.
      if (track.kind === 'audio' && w > 4) {
        const liveFadeIn =
          fadeDrag && fadeDrag.clipId === c.id && fadeDrag.side === 'in'
            ? fadeDrag.candidateDurationSeconds
            : (c.fade_in?.duration_seconds ?? 0);
        const liveFadeOut =
          fadeDrag && fadeDrag.clipId === c.id && fadeDrag.side === 'out'
            ? fadeDrag.candidateDurationSeconds
            : (c.fade_out?.duration_seconds ?? 0);
        const fadeInPx = Math.min(w - 2, liveFadeIn * pixelsPerSecond);
        const fadeOutPx = Math.min(w - 2, liveFadeOut * pixelsPerSecond);
        if (fadeInPx > 0.5) drawFade(ctx, x, cy, w, ch, 'in', fadeInPx, true);
        if (fadeOutPx > 0.5) drawFade(ctx, x, cy, w, ch, 'out', fadeOutPx, true);
      }

      // Phase 7.4 M5 — clip-scope gain keyframe lane in the bottom 70%
      // of the clip body. The amber track lane already painted across
      // the whole row; we'll re-paint cyan on top here so the local
      // edit reads strongest where the clip is.
      const lane = clipLaneRect(cy, ch);
      if (lane && track.kind === 'audio' && w > 8) {
        const liveLane =
          kfDrag && kfDrag.scope === 'clip' && kfDrag.clipId === c.id
            ? kfDrag.candidateLane
            : c.gain_keyframes;
        const selectedIdx =
          selectedKf && selectedKf.scope === 'clip' && selectedKf.clipId === c.id
            ? selectedKf.index
            : null;
        // Re-stamp the amber track lane CLIPPED to this clip rect at
        // reduced alpha so the underlying track automation remains
        // visible through the cyan clip lane.
        const trackLane =
          kfDrag && kfDrag.scope === 'track' && kfDrag.trackId === track.id
            ? kfDrag.candidateLane
            : track.gain_keyframes;
        if (trackLane.length > 0) {
          drawGainLane(
            ctx,
            trackLane,
            x,
            lane.top,
            w,
            lane.height,
            (t) => timeToX(t),
            { start: t_in, end: t_in + dur },
            TRACK_GAIN_COLOR,
            null,
            0.55
          );
        }
        if (liveLane.length > 0) {
          drawGainLane(
            ctx,
            liveLane,
            x,
            lane.top,
            w,
            lane.height,
            // Clip lane stores CLIP-RELATIVE times; convert to canvas X
            // via timeToX(t_in + tClip).
            (tClip) => timeToX(t_in + tClip),
            { start: 0, end: dur },
            CLIP_GAIN_COLOR,
            selectedIdx,
            0.85
          );
        }
      }

      // Phase 7.4 M5 — fade curve chip in the bottom corner of the wedge.
      // Geometry comes from the shared fadeChipGeomFor so the hit pass
      // (hitFadeChip) sees the same chipInVisible / chipOutVisible flags
      // and can never drift from this draw pass.
      if (track.kind === 'audio') {
        const geom = fadeChipGeomFor(
          c.fade_in?.duration_seconds ?? 0,
          c.fade_out?.duration_seconds ?? 0,
          w,
          pixelsPerSecond
        );
        const chipY = cy + ch - CHIP_H - 2;
        if (geom.chipInVisible) {
          drawChip(ctx, x + 3, chipY, CHIP_W, CHIP_H, fadeCurveLetter(c.fade_in?.curve), '#9099a8');
        }
        if (geom.chipOutVisible) {
          drawChip(
            ctx,
            x + w - CHIP_W - 3,
            chipY,
            CHIP_W,
            CHIP_H,
            fadeCurveLetter(c.fade_out?.curve),
            '#9099a8'
          );
        }
      }

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

  // Inverse-of-quantizer constants. Hoisting these out of the inner
  // loops eliminates ~3-6 FP divides per pixel column.
  const INV_I16 = 1 / 32767;
  const INV_U16 = 1 / 65535;

  // Round bucket-range start down to a multiple of WAVE_KEY_STRIDE so
  // a 1-pixel scroll doesn't change the cache key and re-fetch on
  // every frame. 1024 buckets = ~5.5 s @ 48 kHz at L0; at L3 it's
  // ~5.8 min. Bigger stride = less thrash + more memory per fetch;
  // 1024 stays well under the LRU budget.
  const WAVE_KEY_STRIDE = 1024;

  /** Draw the audio waveform overlay for one clip, clipped to its
   *  rounded body. Two sub-passes:
   *  1. Translucent RMS band (single Path2D fill) whose height follows
   *     the normalized RMS at each screen-pixel column.
   *  2. Signed min/max envelope (single beginPath / lineTo / stroke
   *     per clip) drawn as one vertical line per screen-pixel column.
   *
   *  Bucket indices are derived from `source_in_seconds` (NOT the
   *  timeline position), so clip trims/splits/moves leave the cache
   *  valid. Level selection follows `chooseLevel` exactly.
   *
   *  At extreme zoom-out (more buckets per pixel column than 1) the
   *  per-column envelope folds across the [colStartBucket,
   *  colEndBucket) range so peaks aren't dropped.
   *
   *  Columns past the source media's EOF (when a clip's duration runs
   *  past `total_samples / sample_rate`) are skipped — the underlying
   *  clip-body fill shows through, signalling silence instead of
   *  smearing the last real bucket across the tail.
   *
   *  Renders nothing — without throwing — when the cache hasn't yet
   *  populated meta or peaks for this media. A subsequent draw, after
   *  the `peaksCache.version` $effect fires, will paint the waveform. */
  function drawWaveform(
    ctx: CanvasRenderingContext2D,
    c: ClipContents,
    x: number,
    cy: number,
    w: number,
    ch: number,
    timelineInSeconds: number,
    sourceInSeconds: number,
    durationSeconds: number
  ) {
    const meta = peaksCache.requestMeta(c.media_id);
    if (!meta) return;
    const sampleRate = meta.sample_rate;
    if (sampleRate <= 0) return;

    const level = chooseLevel(sampleRate, pixelsPerSecond, meta.level_count);
    const dir = meta.levels[level];
    if (!dir || dir.bucket_count === 0) return;
    const spb = dir.samples_per_bucket;

    // Compute the source-frame range for this clip's visible portion,
    // intersected with the screen and clamped to the source's actual
    // EOF so trailing samples aren't smeared by the last bucket clamp.
    const cssW = ctx.canvas.width / (window.devicePixelRatio || 1);
    const sourceDurationSec = meta.total_samples / sampleRate;
    const clipSourceRightSec = Math.min(
      sourceInSeconds + durationSeconds,
      sourceInSeconds + Math.max(0, sourceDurationSec - sourceInSeconds)
    );
    const effectiveTimelineRightSec = timelineInSeconds + (clipSourceRightSec - sourceInSeconds);
    const visibleClipLeftSec = Math.max(timelineInSeconds, scrollSeconds);
    const visibleClipRightSec = Math.min(
      effectiveTimelineRightSec,
      scrollSeconds + (cssW - TRACK_HEADER_W) / pixelsPerSecond
    );
    if (visibleClipRightSec <= visibleClipLeftSec) return;

    const srcStartSec = sourceInSeconds + (visibleClipLeftSec - timelineInSeconds);
    const srcEndSec = sourceInSeconds + (visibleClipRightSec - timelineInSeconds);

    // Snap the cache key to a stride boundary so scrolling doesn't
    // thrash the cache. The visible bucket range fits inside
    // [fetchStartBucket, fetchStartBucket + fetchCount).
    const visStart = Math.max(0, Math.floor((srcStartSec * sampleRate) / spb));
    const visEnd = Math.min(dir.bucket_count, Math.ceil((srcEndSec * sampleRate) / spb));
    if (visEnd <= visStart) return;
    const fetchStart = Math.max(0, Math.floor(visStart / WAVE_KEY_STRIDE) * WAVE_KEY_STRIDE);
    const fetchEnd = Math.min(
      dir.bucket_count,
      Math.ceil(visEnd / WAVE_KEY_STRIDE) * WAVE_KEY_STRIDE
    );
    const fetchCount = fetchEnd - fetchStart;
    const views = peaksCache.request(c.media_id, level, fetchStart, fetchCount);
    if (!views) return;

    // Map src→screen for each visible pixel column. We sample per
    // column rather than per bucket so the loop count is bounded by
    // the clip's visible CSS width (typically <2000), independent of
    // how many buckets back the range.
    const xStart = Math.max(timeToX(visibleClipLeftSec), x);
    const xEnd = Math.min(timeToX(visibleClipRightSec), x + w);
    const colW = Math.floor(xEnd - xStart);
    if (colW < 1) return;

    const mid = cy + ch / 2;
    const half = (ch - 2) / 2;

    // Hoist loop invariants and precompute per-column bucket spans
    // so the inner loops don't redo the same arithmetic three times.
    const clipOriginX = timeToX(timelineInSeconds);
    const invPps = 1 / pixelsPerSecond;
    const fetchCountInt = fetchCount | 0;
    const srcToBucketScale = sampleRate / spb;
    // bIdxStart[i] = inclusive start bucket index (offset within views).
    // bIdxEnd[i]   = exclusive end bucket index. For dense zoom the
    // start..end-1 range is a single bucket; when bps < 1 (zoomed out
    // past coarsest level) multiple buckets fall in one column and we
    // fold their min/max/rms.
    const bIdxStart = new Int32Array(colW);
    const bIdxEnd = new Int32Array(colW);
    for (let i = 0; i < colW; i++) {
      const cx = xStart + i;
      const colStartSec = sourceInSeconds + (cx - clipOriginX) * invPps;
      const colEndSec = colStartSec + invPps;
      let s = Math.floor(colStartSec * srcToBucketScale) - fetchStart;
      let e = Math.ceil(colEndSec * srcToBucketScale) - fetchStart;
      if (s < 0) s = 0;
      if (e > fetchCountInt) e = fetchCountInt;
      if (e <= s) {
        // Past-EOF guard — leave start==end so the per-column passes
        // skip drawing for this column (RMS poly emits mid, envelope
        // skipped).
        s = e = -1;
      }
      bIdxStart[i] = s;
      bIdxEnd[i] = e;
    }

    ctx.save();
    // Clip to the clip's rounded body so the waveform doesn't bleed
    // over the border.
    ctx.beginPath();
    roundRect(ctx, x, cy, Math.max(2, w), ch, 3);
    ctx.clip();

    // Pass 1 — RMS band (filled translucent strip). One Path2D poly:
    // top edge L→R via mid - rmsHalf, bottom edge R→L via mid + rmsHalf.
    ctx.beginPath();
    let started = false;
    for (let i = 0; i < colW; i++) {
      const s = bIdxStart[i];
      const e = bIdxEnd[i];
      let rmsMax = 0;
      if (s >= 0) {
        for (let b = s; b < e; b++) {
          const r = views.rms[b];
          if (r > rmsMax) rmsMax = r;
        }
      }
      const rmsHalf = rmsMax * INV_U16 * half;
      const cx = xStart + i + 0.5;
      if (!started) {
        ctx.moveTo(cx, mid - rmsHalf);
        started = true;
      } else {
        ctx.lineTo(cx, mid - rmsHalf);
      }
    }
    for (let i = colW - 1; i >= 0; i--) {
      const s = bIdxStart[i];
      const e = bIdxEnd[i];
      let rmsMax = 0;
      if (s >= 0) {
        for (let b = s; b < e; b++) {
          const r = views.rms[b];
          if (r > rmsMax) rmsMax = r;
        }
      }
      const rmsHalf = rmsMax * INV_U16 * half;
      ctx.lineTo(xStart + i + 0.5, mid + rmsHalf);
    }
    ctx.closePath();
    ctx.fillStyle = WAVE_RMS_FILL;
    ctx.fill();

    // Pass 2 — signed min/max envelope, one stroke. Skips columns
    // whose span is past EOF (s < 0) — the clip body fill shows
    // through, signalling silence rather than smeared last-bucket.
    ctx.beginPath();
    ctx.strokeStyle = WAVE_ENV_STROKE;
    ctx.lineWidth = 1;
    for (let i = 0; i < colW; i++) {
      const s = bIdxStart[i];
      if (s < 0) continue;
      const e = bIdxEnd[i];
      let minV = views.min[s];
      let maxV = views.max[s];
      for (let b = s + 1; b < e; b++) {
        const mn = views.min[b];
        if (mn < minV) minV = mn;
        const mx = views.max[b];
        if (mx > maxV) maxV = mx;
      }
      // i16 → [-1, 1] normalized. Negative min → -min*half > 0 →
      // below mid (drawn at y > mid). Positive max → above mid.
      const yMin = mid - minV * INV_I16 * half;
      const yMax = mid - maxV * INV_I16 * half;
      const cx = xStart + i + 0.5;
      ctx.moveTo(cx, yMax);
      ctx.lineTo(cx, yMin);
    }
    ctx.stroke();

    ctx.restore();
  }

  /** Draw cross-dissolve transitions on a track. Each transition
   *  renders as a translucent overlay spanning its window, with a
   *  diagonal stripe to signal "this is a blend, not a clip". */
  function drawTransitions(
    ctx: CanvasRenderingContext2D,
    track: TrackContents,
    y: number,
    visibleLeft: number,
    visibleRight: number
  ) {
    if (!track.transitions || track.transitions.length === 0) return;
    for (const tr of track.transitions) {
      const t_in = tr.position_seconds - tr.duration_seconds / 2;
      const t_out = tr.position_seconds + tr.duration_seconds / 2;
      if (t_out < visibleLeft || t_in > visibleRight) continue;
      const x = timeToX(t_in);
      const w = Math.max(2, (t_out - t_in) * pixelsPerSecond);
      const ty = y + 4;
      const th = TRACK_HEIGHT - 8;
      ctx.save();
      // Translucent body
      ctx.fillStyle = 'rgba(120, 200, 255, 0.32)';
      roundRect(ctx, x, ty, w, th, 3);
      ctx.fill();
      // Diagonal stripe — cheap "this is a blend" cue.
      ctx.beginPath();
      ctx.moveTo(x, ty + th);
      ctx.lineTo(x + w, ty);
      ctx.strokeStyle = 'rgba(120, 200, 255, 0.85)';
      ctx.lineWidth = 1.5;
      ctx.stroke();
      // Border
      ctx.strokeStyle = 'rgba(120, 200, 255, 0.65)';
      ctx.lineWidth = 1;
      roundRect(ctx, x + 0.5, ty + 0.5, Math.max(1, w - 1), th - 1, 3);
      ctx.stroke();
      ctx.restore();
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
  function clipAt(
    localX: number,
    localY: number
  ): { clip: ClipContents; track: TrackContents } | null {
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

    ctx.font = '11px ui-sans-serif, system-ui, sans-serif';
    ctx.textBaseline = 'middle';
    sequence.tracks.forEach((track, rowIdx) => {
      const y = trackY(rowIdx);
      // Track name label (V1, A1, ...).
      ctx.fillStyle = TRACK_HEADER_TEXT;
      ctx.fillText(track.name, 6, y + TRACK_HEIGHT / 2);
      // Phase 7.3 — M (mute) and S (solo) toggle buttons. Drawn for
      // every track for visual consistency; the engine ignores mixer
      // state on Video tracks during the audio fold.
      const btnY = y + (TRACK_HEIGHT - MIXER_BTN_SIZE) / 2;
      drawMixerToggle(ctx, MIXER_BTN_X, btnY, 'M', track.mute, '#cf4444');
      drawMixerToggle(
        ctx,
        MIXER_BTN_X + MIXER_BTN_SIZE + MIXER_BTN_GAP,
        btnY,
        'S',
        track.solo,
        '#e6c52c'
      );
      // Phase 7.5 M1 — stereo meter strip for audio tracks. Video
      // tracks reserve the slot (so the row layout stays uniform) but
      // skip the live draw — the fold thread never produces a meter
      // entry for them.
      if (track.kind === 'audio') {
        const display = metersStore.tracks[track.id];
        if (display) drawMeterStrip(ctx, METER_STRIP_X, y + 4, TRACK_HEIGHT - 8, display);
      }
    });
  }

  /** Phase 7.5 M1 — stereo vertical meter strip. RMS fills from the
   *  bottom; held peak is a thin bright line; clip indicator is a
   *  small red square at the top. */
  function drawMeterStrip(
    ctx: CanvasRenderingContext2D,
    x: number,
    y: number,
    h: number,
    display: MeterDisplay
  ) {
    if (h < 8) return;
    const barH = h - 2; // 1 px reserved top + bottom for the clip LED.
    // Background slot.
    ctx.fillStyle = '#1c1f25';
    ctx.fillRect(x, y, METER_STRIP_W, h);
    drawMeterBar(ctx, x, y + 1, METER_BAR_W, barH, display.l);
    drawMeterBar(ctx, x + METER_BAR_W + METER_BAR_GAP, y + 1, METER_BAR_W, barH, display.r);
  }

  function drawMeterBar(
    ctx: CanvasRenderingContext2D,
    x: number,
    y: number,
    w: number,
    h: number,
    ch: { rms: number; peak_db: number; clipping: boolean }
  ) {
    // RMS fill (green → amber → red gradient by amplitude).
    const rmsDb = ch.rms > 0 ? 20 * Math.log10(ch.rms) : -Infinity;
    const fill = Math.max(0, Math.min(1, dbToFill(rmsDb)));
    const fillPx = Math.round(fill * h);
    if (fillPx > 0) {
      ctx.fillStyle = '#3ab66c';
      ctx.fillRect(x, y + h - fillPx, w, fillPx);
      // Yellow band above -6 dB.
      const sixDbFill = Math.round(dbToFill(-6) * h);
      if (fillPx > sixDbFill) {
        ctx.fillStyle = '#e6c52c';
        const yellowTop = y + h - fillPx;
        const yellowBot = y + h - sixDbFill;
        ctx.fillRect(x, yellowTop, w, Math.max(0, yellowBot - yellowTop));
      }
      // Red band above 0 dBFS — only triggered if rms exceeds unity,
      // which is rare in practice but defensive.
      const zeroDbFill = Math.round(dbToFill(0) * h);
      if (fillPx > zeroDbFill) {
        ctx.fillStyle = '#e44b4b';
        const redTop = y + h - fillPx;
        const redBot = y + h - zeroDbFill;
        ctx.fillRect(x, redTop, w, Math.max(0, redBot - redTop));
      }
    }
    // Held-peak line.
    if (isFinite(ch.peak_db)) {
      const peakFill = Math.max(0, Math.min(1, dbToFill(ch.peak_db)));
      const peakPx = Math.round(peakFill * h);
      const peakY = y + h - peakPx;
      ctx.fillStyle = '#e6ebf5';
      ctx.fillRect(x, peakY, w, 1);
    }
    // Clip LED.
    if (ch.clipping) {
      ctx.fillStyle = '#ff3030';
      ctx.fillRect(x, y - 1, w, 2);
    }
  }

  function drawMixerToggle(
    ctx: CanvasRenderingContext2D,
    x: number,
    y: number,
    label: string,
    active: boolean,
    activeColor: string
  ) {
    ctx.save();
    ctx.fillStyle = active ? activeColor : '#2c2f37';
    ctx.strokeStyle = active ? activeColor : '#4a4e58';
    ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.roundRect(x + 0.5, y + 0.5, MIXER_BTN_SIZE - 1, MIXER_BTN_SIZE - 1, 3);
    ctx.fill();
    ctx.stroke();
    ctx.fillStyle = active ? '#0d0f12' : '#9099a8';
    ctx.textAlign = 'center';
    ctx.font = 'bold 10px ui-sans-serif, system-ui, sans-serif';
    ctx.fillText(label, x + MIXER_BTN_SIZE / 2, y + MIXER_BTN_SIZE / 2 + 0.5);
    ctx.textAlign = 'left';
    ctx.restore();
  }

  /** Hit-test the mixer toggles in a track header. Returns
   *  `{ trackId, kind }` if the click is inside an M or S button. */
  function hitTestMixerButton(
    cssX: number,
    cssY: number
  ): { trackId: string; kind: 'mute' | 'solo' } | null {
    if (!sequence || cssX > TRACK_HEADER_W) return null;
    if (cssY < RULER_HEIGHT) return null;
    const rowIdx = Math.floor((cssY - RULER_HEIGHT) / TRACK_HEIGHT);
    if (rowIdx < 0 || rowIdx >= sequence.tracks.length) return null;
    const track = sequence.tracks[rowIdx];
    const y = trackY(rowIdx);
    const btnY = y + (TRACK_HEIGHT - MIXER_BTN_SIZE) / 2;
    if (cssY < btnY || cssY > btnY + MIXER_BTN_SIZE) return null;
    const mX = MIXER_BTN_X;
    const sX = MIXER_BTN_X + MIXER_BTN_SIZE + MIXER_BTN_GAP;
    if (cssX >= mX && cssX < mX + MIXER_BTN_SIZE) {
      return { trackId: track.id, kind: 'mute' };
    }
    if (cssX >= sX && cssX < sX + MIXER_BTN_SIZE) {
      return { trackId: track.id, kind: 'solo' };
    }
    return null;
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

  type DragKind =
    | 'pan'
    | 'marquee'
    | 'move'
    | 'trim-left'
    | 'trim-right'
    // Phase 7.4 M5 — automation overlay drag kinds.
    | 'fade-in'
    | 'fade-out'
    | 'kf-clip-move'
    | 'kf-track-move';

  // --- Phase 7.4 M5 automation overlay state ----------------------------

  /** Selected keyframe (across both clip and track scopes). null = nothing
   *  selected. Single-select only in v1 — marquee selection over keyframes
   *  is a documented future enhancement. */
  type SelectedKf =
    | { scope: 'clip'; trackId: string; clipId: string; index: number }
    | { scope: 'track'; trackId: string; index: number }
    | null;
  let selectedKf = $state<SelectedKf>(null);

  // Phase 7.4 M5 — re-resolve selectedKf against the live sequence
  // after every edit. When the underlying clip/track/keyframe goes
  // away (clip deleted, lane reset, kf-index shifted by another edit),
  // null the selection so subsequent Delete / interp-set actions can't
  // operate on a phantom target. Adversarial review finding #4.
  $effect(() => {
    const sel = selectedKf;
    if (!sel || !sequence) return;
    const track = sequence.tracks.find((t) => t.id === sel.trackId);
    if (!track) {
      selectedKf = null;
      return;
    }
    if (sel.scope === 'clip') {
      const clip = track.clips.find((c) => c.id === sel.clipId);
      if (!clip || sel.index < 0 || sel.index >= clip.gain_keyframes.length) {
        selectedKf = null;
      }
    } else {
      if (sel.index < 0 || sel.index >= track.gain_keyframes.length) {
        selectedKf = null;
      }
    }
  });

  /** Live fade-edit preview. The wedge follows the cursor; the IPC
   *  commit fires on mouseup. */
  type FadeDrag = {
    trackId: string;
    clipId: string;
    side: 'in' | 'out';
    originalDurationSeconds: number;
    candidateDurationSeconds: number;
    curve: FadeCurveName;
  };
  let fadeDrag = $state<FadeDrag | null>(null);

  /** Live keyframe-move preview. The polyline + keyframe symbol render
   *  against `candidateLane` until mouseup. */
  type KfDrag = {
    scope: 'clip' | 'track';
    trackId: string;
    clipId?: string;
    index: number;
    originalLane: GainKeyframe[];
    candidateLane: GainKeyframe[];
    axisLock: 'time' | 'value' | null;
    fineScale: number;
  };
  let kfDrag = $state<KfDrag | null>(null);

  /** Cursor position for the optional drag tag overlay. */
  let cursorCss = $state<{ x: number; y: number } | null>(null);

  /** dB lane geometry inside a clip body. Returns null when the clip
   *  is too short vertically to host a useful lane. */
  function clipLaneRect(cy: number, ch: number): { top: number; height: number } | null {
    // Top 30% of the clip body remains for waveform + label; bottom 70%
    // hosts the gain lane.
    const top = cy + ch * 0.3;
    const height = ch - (top - cy);
    if (height < 8) return null;
    return { top, height };
  }

  /** dB lane geometry for a track-wide automation lane. Renders across
   *  the full track row body. */
  function trackLaneRect(y: number): { top: number; height: number } {
    return { top: y + 4, height: TRACK_HEIGHT - 8 };
  }
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

  let dragStart: {
    kind: DragKind;
    clientX: number;
    clientY: number;
    localX: number;
    localY: number;
    scroll: number;
    modKeys: { shift: boolean; cmd: boolean };
    downOnClipId: string | null;
  } | null = null;

  /** Phase 7.4 M5 — resolve every automation hit at (cssX, cssY).
   *  Walks tracks/clips in the same order draw uses; first hit wins.
   *  Returns null when the cursor is not on any automation control. */
  function hitAutomation(cssX: number, cssY: number): AutomationHit | null {
    if (!sequence || cssX < TRACK_HEADER_W || cssY < RULER_HEIGHT) return null;
    const rowIdx = Math.floor((cssY - RULER_HEIGHT) / TRACK_HEIGHT);
    if (rowIdx < 0 || rowIdx >= sequence.tracks.length) return null;
    const track = sequence.tracks[rowIdx];
    const y = trackY(rowIdx);
    // Search per-clip targets first (highest priority). Shared
    // FadeChipGeom ensures the hit pass sees exactly what the draw pass
    // painted — no draw/hit drift, no zero-fade grip stealing trim.
    if (track.kind === 'audio') {
      for (const c of track.clips) {
        const t_in = c.timeline_in_seconds;
        const dur = c.duration_seconds;
        const x = timeToX(t_in);
        const w = dur * pixelsPerSecond;
        const cy = y + 4;
        const ch = TRACK_HEIGHT - 8;
        if (cssX < x - 8 || cssX > x + w + 8) continue;
        const geom = fadeChipGeomFor(
          c.fade_in?.duration_seconds ?? 0,
          c.fade_out?.duration_seconds ?? 0,
          w,
          pixelsPerSecond
        );
        const chip = hitFadeChip(cssX, cssY, track.id, c.id, x, cy, w, ch, geom);
        if (chip) return chip;
        const grip = hitFadeGrip(cssX, cssY, track.id, c.id, x, cy, w, geom);
        if (grip) return grip;
        const lane = clipLaneRect(cy, ch);
        if (lane && cssX >= x && cssX <= x + w) {
          // Gate the keyframe hit-test on the lane rect so a click
          // ABOVE the lane (e.g. on the clip label) can never grab a
          // keyframe symbol that happens to sit at the lane's top edge.
          if (cssY >= lane.top - KF_HITBOX / 2 && cssY <= lane.top + lane.height + KF_HITBOX / 2) {
            const kf = hitClipKeyframe(
              cssX,
              cssY,
              track.id,
              c.id,
              c.gain_keyframes,
              (tClip) => timeToX(t_in + tClip),
              (db) => yForDb(db, lane.top, lane.height)
            );
            if (kf) return kf;
          }
        }
      }
    }
    // Track keyframe — checked AFTER per-clip targets so a kf landing
    // on the same pixel as a clip keyframe loses to the clip one. Y is
    // gated to the lane rect for the same reason as above.
    if (track.kind === 'audio') {
      const lane = trackLaneRect(y);
      if (cssY >= lane.top - KF_HITBOX / 2 && cssY <= lane.top + lane.height + KF_HITBOX / 2) {
        const trackKf = hitTrackKeyframe(
          cssX,
          cssY,
          track.id,
          track.gain_keyframes,
          (t) => timeToX(t),
          (db) => yForDb(db, lane.top, lane.height)
        );
        if (trackKf) return trackKf;
      }
    }
    return null;
  }

  function onMouseDown(e: MouseEvent) {
    if (e.button !== 0) return;
    if (!canvas) return;
    const rect = canvas.getBoundingClientRect();
    const localX = e.clientX - rect.left;
    const localY = e.clientY - rect.top;
    // A. Phase 7.3 — track-header mixer toggle hit-test runs BEFORE clip
    // hit-testing because the buttons live inside TRACK_HEADER_W where
    // hitAt never returns clips anyway. Fire-and-forget so the canvas
    // stays responsive; commitEdit refreshes the sequence on resolve.
    const mixHit = hitTestMixerButton(localX, localY);
    if (mixHit && sequence) {
      const track = sequence.tracks.find((t) => t.id === mixHit.trackId);
      if (track) {
        if (mixHit.kind === 'mute') {
          void projectState.setTrackMute(track.id, !track.mute);
        } else {
          void projectState.setTrackSolo(track.id, !track.solo);
        }
      }
      return;
    }
    // Phase 7.4 M5 — automation hit-test runs BEFORE trim/move/marquee
    // so fade grips and keyframe handles win over the underlying clip
    // body interactions. Priority resolved inside hitAutomation per the
    // spike's documented order.
    const autoHit = hitAutomation(localX, localY);
    if (autoHit) {
      handleAutomationMouseDown(e, autoHit, localX, localY);
      return;
    }
    const hit = hitAt(localX, localY);
    // Phase 7.4 M5 — Alt-click priority resolution:
    //   1. Inside an audio clip body → create a CLIP-scope keyframe.
    //   2. On empty audio-track strip (no clip under cursor) → create
    //      a TRACK-scope keyframe.
    // The previous ordering ran the track-strip branch unconditionally
    // on any Alt-click in an audio row, leaving the clip-body branch
    // unreachable (adversarial review finding #2).
    if (e.altKey && hit?.kind === 'body' && sequence) {
      const track = sequence.tracks.find((t) => t.clips.some((c) => c.id === hit.clip.id));
      if (track && track.kind === 'audio') {
        const cy = trackY(sequence.tracks.indexOf(track)) + 4;
        const ch = TRACK_HEIGHT - 8;
        const lane = clipLaneRect(cy, ch);
        if (lane) {
          const tSec = scrollSeconds + (localX - TRACK_HEADER_W) / pixelsPerSecond;
          const tClip = Math.max(0, tSec - hit.clip.timeline_in_seconds);
          const db = clampDb(dbForY(localY, lane.top, lane.height));
          void createClipKeyframe(track.id, hit.clip.id, tClip, db);
          return;
        }
      }
    }
    if (e.altKey && sequence && !hit && localX >= TRACK_HEADER_W && localY >= RULER_HEIGHT) {
      const rowIdx = Math.floor((localY - RULER_HEIGHT) / TRACK_HEIGHT);
      if (rowIdx >= 0 && rowIdx < sequence.tracks.length) {
        const track = sequence.tracks[rowIdx];
        if (track.kind === 'audio') {
          const lane = trackLaneRect(trackY(rowIdx));
          const tSec = scrollSeconds + (localX - TRACK_HEADER_W) / pixelsPerSecond;
          const db = clampDb(dbForY(localY, lane.top, lane.height));
          void createTrackKeyframe(track.id, tSec, db);
          return;
        }
      }
    }
    const kind: DragKind =
      hit?.kind === 'left'
        ? 'trim-left'
        : hit?.kind === 'right'
          ? 'trim-right'
          : hit?.kind === 'body'
            ? 'move'
            : 'marquee';
    // Clicking empty space clears keyframe selection.
    if (!hit) selectedKf = null;
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

  /** Phase 7.4 M5 — start the appropriate drag (or commit a click) for
   *  an automation hit. Mutates dragStart + the per-drag preview state. */
  function handleAutomationMouseDown(
    e: MouseEvent,
    hit: AutomationHit,
    localX: number,
    localY: number
  ) {
    if (!sequence) return;
    switch (hit.kind) {
      case 'fade-chip': {
        // Single click cycles the curve through linear → equal_power → log.
        const track = sequence.tracks.find((t) => t.id === hit.trackId);
        const clip = track?.clips.find((c) => c.id === hit.clipId);
        if (!clip) return;
        const fade = hit.side === 'in' ? clip.fade_in : clip.fade_out;
        const dur = fade?.duration_seconds ?? 0;
        const curve = nextFadeCurve(fade?.curve);
        if (hit.side === 'in') {
          void projectState.setClipFadeIn(track!.id, clip.id, dur, curve);
        } else {
          void projectState.setClipFadeOut(track!.id, clip.id, dur, curve);
        }
        return;
      }
      case 'fade-grip': {
        const track = sequence.tracks.find((t) => t.id === hit.trackId);
        const clip = track?.clips.find((c) => c.id === hit.clipId);
        if (!clip) return;
        const fade = hit.side === 'in' ? clip.fade_in : clip.fade_out;
        fadeDrag = {
          trackId: hit.trackId,
          clipId: hit.clipId,
          side: hit.side,
          originalDurationSeconds: fade?.duration_seconds ?? 0,
          candidateDurationSeconds: fade?.duration_seconds ?? 0,
          curve: fade?.curve ?? 'linear'
        };
        dragStart = {
          kind: hit.side === 'in' ? 'fade-in' : 'fade-out',
          clientX: e.clientX,
          clientY: e.clientY,
          localX,
          localY,
          scroll: scrollSeconds,
          modKeys: { shift: e.shiftKey, cmd: e.metaKey || e.ctrlKey },
          downOnClipId: hit.clipId
        };
        return;
      }
      case 'kf-clip': {
        const track = sequence.tracks.find((t) => t.id === hit.trackId);
        const clip = track?.clips.find((c) => c.id === hit.clipId);
        if (!clip) return;
        selectedKf = {
          scope: 'clip',
          trackId: hit.trackId,
          clipId: hit.clipId,
          index: hit.index
        };
        kfDrag = {
          scope: 'clip',
          trackId: hit.trackId,
          clipId: hit.clipId,
          index: hit.index,
          originalLane: clip.gain_keyframes,
          candidateLane: clip.gain_keyframes.map((k) => ({ ...k })),
          axisLock: e.shiftKey ? 'time' : e.altKey ? 'value' : null,
          fineScale: e.metaKey || e.ctrlKey ? 0.1 : 1
        };
        dragStart = {
          kind: 'kf-clip-move',
          clientX: e.clientX,
          clientY: e.clientY,
          localX,
          localY,
          scroll: scrollSeconds,
          modKeys: { shift: e.shiftKey, cmd: e.metaKey || e.ctrlKey },
          downOnClipId: hit.clipId
        };
        return;
      }
      case 'kf-track': {
        const track = sequence.tracks.find((t) => t.id === hit.trackId);
        if (!track) return;
        selectedKf = { scope: 'track', trackId: hit.trackId, index: hit.index };
        kfDrag = {
          scope: 'track',
          trackId: hit.trackId,
          index: hit.index,
          originalLane: track.gain_keyframes,
          candidateLane: track.gain_keyframes.map((k) => ({ ...k })),
          axisLock: e.shiftKey ? 'time' : e.altKey ? 'value' : null,
          fineScale: e.metaKey || e.ctrlKey ? 0.1 : 1
        };
        dragStart = {
          kind: 'kf-track-move',
          clientX: e.clientX,
          clientY: e.clientY,
          localX,
          localY,
          scroll: scrollSeconds,
          modKeys: { shift: e.shiftKey, cmd: e.metaKey || e.ctrlKey },
          downOnClipId: null
        };
        return;
      }
    }
  }

  /** Append a new keyframe to a clip's lane and commit via IPC. */
  async function createClipKeyframe(trackId: string, clipId: string, tClip: number, db: number) {
    const track = sequence?.tracks.find((t) => t.id === trackId);
    const clip = track?.clips.find((c) => c.id === clipId);
    if (!clip) return;
    const next = normalizeLane([
      ...clip.gain_keyframes,
      { time_seconds: tClip, value: db, interp: 'linear' }
    ]);
    selectedKf = {
      scope: 'clip',
      trackId,
      clipId,
      index: next.findIndex(
        (k) => Math.abs(k.time_seconds - tClip) < 1e-6 && Math.abs(k.value - db) < 1e-6
      )
    };
    await projectState.setClipGainKeyframes(trackId, clipId, next);
  }

  /** Append a new keyframe to a track's lane and commit via IPC. */
  async function createTrackKeyframe(trackId: string, tSec: number, db: number) {
    const track = sequence?.tracks.find((t) => t.id === trackId);
    if (!track) return;
    const next = normalizeLane([
      ...track.gain_keyframes,
      { time_seconds: tSec, value: db, interp: 'linear' }
    ]);
    selectedKf = {
      scope: 'track',
      trackId,
      index: next.findIndex(
        (k) => Math.abs(k.time_seconds - tSec) < 1e-6 && Math.abs(k.value - db) < 1e-6
      )
    };
    await projectState.setTrackGainKeyframes(trackId, next);
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
        const cursorT = scrollSeconds + (e.clientX - rect.left - TRACK_HEADER_W) / pixelsPerSecond;
        applyTrimCandidate(trimDrag, cursorT, e.shiftKey);
        scheduleDraw();
      }
    } else if (dragStart.kind === 'fade-in' || dragStart.kind === 'fade-out') {
      const fd = fadeDrag;
      if (!fd || !canvas || !sequence) return;
      // Phase 7.4 M5 — fade drag: cursor X → fade duration.
      const rect = canvas.getBoundingClientRect();
      cursorCss = { x: e.clientX - rect.left, y: e.clientY - rect.top };
      const track = sequence.tracks.find((t) => t.id === fd.trackId);
      const clip = track?.clips.find((c) => c.id === fd.clipId);
      if (clip) {
        const cursorT = scrollSeconds + (e.clientX - rect.left - TRACK_HEADER_W) / pixelsPerSecond;
        // For fade-in: duration = cursorT - clip.timeline_in. For fade-out: clip end - cursorT.
        const raw =
          fd.side === 'in'
            ? cursorT - clip.timeline_in_seconds
            : clip.timeline_in_seconds + clip.duration_seconds - cursorT;
        // Clamp so the two fades together can't exceed clip duration.
        const otherFade =
          fd.side === 'in'
            ? (clip.fade_out?.duration_seconds ?? 0)
            : (clip.fade_in?.duration_seconds ?? 0);
        const maxDur = Math.max(0, clip.duration_seconds - otherFade - 0.001);
        const fine = e.metaKey || e.ctrlKey ? 0.1 : 1;
        const original = fd.originalDurationSeconds;
        const candidate = Math.max(0, Math.min(maxDur, original + (raw - original) * fine));
        fd.candidateDurationSeconds = candidate;
        scheduleDraw();
      }
    } else if (dragStart.kind === 'kf-clip-move' || dragStart.kind === 'kf-track-move') {
      if (!kfDrag || !canvas) return;
      // Phase 7.4 M5 — keyframe drag: cursor → (time, dB).
      const rect = canvas.getBoundingClientRect();
      cursorCss = { x: e.clientX - rect.left, y: e.clientY - rect.top };
      applyKeyframeDrag(e, cursorCss.x, cursorCss.y);
      scheduleDraw();
    }
  }

  /** Apply the live cursor position to the kfDrag preview lane. Respects
   *  axis-lock (Shift = time-only, Alt = value-only) and fine-adjust
   *  (Cmd/Ctrl scales delta by 0.1). */
  function applyKeyframeDrag(e: MouseEvent, cssX: number, cssY: number) {
    const kd = kfDrag;
    if (!kd || !sequence) return;
    // Recompute axis-lock + fine each frame so the user can toggle modifiers
    // mid-drag.
    kd.axisLock = e.shiftKey ? 'time' : e.altKey ? 'value' : null;
    kd.fineScale = e.metaKey || e.ctrlKey ? 0.1 : 1;
    const original = kd.originalLane[kd.index];
    if (!original) return;
    let newTime = original.time_seconds;
    let newValue = original.value;
    if (kd.scope === 'track') {
      const lane = trackLaneRect(trackY(sequence.tracks.findIndex((t) => t.id === kd.trackId)));
      const targetTime = scrollSeconds + (cssX - TRACK_HEADER_W) / pixelsPerSecond;
      const targetDb = dbForY(cssY, lane.top, lane.height);
      if (kd.axisLock !== 'value') {
        newTime = Math.max(
          0,
          original.time_seconds + (targetTime - original.time_seconds) * kd.fineScale
        );
      }
      if (kd.axisLock !== 'time') {
        newValue = clampDb(original.value + (targetDb - original.value) * kd.fineScale);
      }
    } else {
      const track = sequence.tracks.find((t) => t.id === kd.trackId);
      const clip = track?.clips.find((c) => c.id === kd.clipId);
      if (!clip || !track) return;
      const trackIdx = sequence.tracks.indexOf(track);
      const cy = trackY(trackIdx) + 4;
      const ch = TRACK_HEIGHT - 8;
      const lane = clipLaneRect(cy, ch);
      if (!lane) return;
      const targetTime =
        scrollSeconds + (cssX - TRACK_HEADER_W) / pixelsPerSecond - clip.timeline_in_seconds;
      const targetDb = dbForY(cssY, lane.top, lane.height);
      if (kd.axisLock !== 'value') {
        newTime = Math.max(
          0,
          Math.min(
            clip.duration_seconds,
            original.time_seconds + (targetTime - original.time_seconds) * kd.fineScale
          )
        );
      }
      if (kd.axisLock !== 'time') {
        newValue = clampDb(original.value + (targetDb - original.value) * kd.fineScale);
      }
    }
    const next = kd.candidateLane.map((k, i) =>
      i === kd.index ? { ...k, time_seconds: newTime, value: newValue } : k
    );
    kd.candidateLane = next;
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
      const cursorT = scrollSeconds + (ds.clientX - rect.left - TRACK_HEADER_W) / pixelsPerSecond;
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
    const moved = Math.abs(e.clientX - dragStart.clientX) + Math.abs(e.clientY - dragStart.clientY);
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

    // ---- Phase 7.4 M5 — Fade-drag release: commit via engine -------------
    if ((ds.kind === 'fade-in' || ds.kind === 'fade-out') && fadeDrag) {
      const fd = fadeDrag;
      fadeDrag = null;
      cursorCss = null;
      if (Math.abs(fd.candidateDurationSeconds - fd.originalDurationSeconds) > 1e-3) {
        if (fd.side === 'in') {
          await projectState.setClipFadeIn(
            fd.trackId,
            fd.clipId,
            fd.candidateDurationSeconds,
            fd.curve
          );
        } else {
          await projectState.setClipFadeOut(
            fd.trackId,
            fd.clipId,
            fd.candidateDurationSeconds,
            fd.curve
          );
        }
        return;
      }
      // No-op drag falls through (selection logic below).
    }

    // ---- Phase 7.4 M5 — Keyframe-drag release: commit via engine ---------
    if ((ds.kind === 'kf-clip-move' || ds.kind === 'kf-track-move') && kfDrag) {
      const kd = kfDrag;
      kfDrag = null;
      cursorCss = null;
      const original = kd.originalLane[kd.index];
      const candidate = kd.candidateLane[kd.index];
      if (
        original &&
        candidate &&
        (Math.abs(candidate.time_seconds - original.time_seconds) > 1e-4 ||
          Math.abs(candidate.value - original.value) > 1e-4)
      ) {
        const normalized = normalizeLane(kd.candidateLane);
        if (kd.scope === 'clip' && kd.clipId) {
          await projectState.setClipGainKeyframes(kd.trackId, kd.clipId, normalized);
        } else {
          await projectState.setTrackGainKeyframes(kd.trackId, normalized);
        }
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
        const t = sequence.tracks.find((tr) => tr.clips.some((c) => c.id === ds.downOnClipId));
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

  /** Phase 7.4 M5 keyboard shortcuts:
   *  Esc — cancel an in-flight fade or keyframe drag (no IPC commit).
   *  Delete / Backspace — delete the selected keyframe.
   *  L / B / H — set interp on the selected clip-gain or track-gain
   *              keyframe (Linear / Bezier / Hold).
   */
  function onKeyDown(e: KeyboardEvent) {
    // Don't steal keys while typing into an input.
    const target = e.target as HTMLElement | null;
    if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA')) return;
    if (e.key === 'Escape') {
      // Cancel any in-flight drag — M5 automation drags AND the
      // existing move/trim drags. Without clearing all of these the
      // dragStart record can survive the cancel and re-fire on the
      // next mousemove (finding #7).
      if (fadeDrag || kfDrag || moveDrag || trimDrag || dragStart) {
        fadeDrag = null;
        kfDrag = null;
        moveDrag = null;
        trimDrag = null;
        cursorCss = null;
        dragStart = null;
        marquee = null;
        scheduleDraw();
        e.preventDefault();
      }
      return;
    }
    if (selectedKf && (e.key === 'Delete' || e.key === 'Backspace')) {
      e.preventDefault();
      void deleteSelectedKeyframe();
      return;
    }
    if (
      selectedKf &&
      (e.key === 'l' ||
        e.key === 'L' ||
        e.key === 'b' ||
        e.key === 'B' ||
        e.key === 'h' ||
        e.key === 'H')
    ) {
      const interp: 'linear' | 'bezier' | 'hold' =
        e.key === 'l' || e.key === 'L'
          ? 'linear'
          : e.key === 'b' || e.key === 'B'
            ? 'bezier'
            : 'hold';
      e.preventDefault();
      void setSelectedKeyframeInterp(interp);
    }
  }

  /** Single click on the selected-keyframe interp chip cycles through
   *  Linear → Bezier → Hold. (Currently only invoked from the keyboard
   *  shortcuts above; the chip itself is reserved for v1.1 since the
   *  visible chip would compete with the M/S toggle for header pixels.) */
  async function setSelectedKeyframeInterp(interp: 'linear' | 'bezier' | 'hold') {
    const sel = selectedKf;
    if (!sel || !sequence) return;
    if (sel.scope === 'clip') {
      const track = sequence.tracks.find((t) => t.id === sel.trackId);
      const clip = track?.clips.find((c) => c.id === sel.clipId);
      if (!track || !clip || sel.index >= clip.gain_keyframes.length) return;
      const next = clip.gain_keyframes.map((k, i) => (i === sel.index ? { ...k, interp } : k));
      await projectState.setClipGainKeyframes(track.id, clip.id, next);
    } else {
      const track = sequence.tracks.find((t) => t.id === sel.trackId);
      if (!track || sel.index >= track.gain_keyframes.length) return;
      const next = track.gain_keyframes.map((k, i) => (i === sel.index ? { ...k, interp } : k));
      await projectState.setTrackGainKeyframes(track.id, next);
    }
  }

  async function deleteSelectedKeyframe() {
    const sel = selectedKf;
    if (!sel || !sequence) return;
    if (sel.scope === 'clip') {
      const track = sequence.tracks.find((t) => t.id === sel.trackId);
      const clip = track?.clips.find((c) => c.id === sel.clipId);
      if (!track || !clip) return;
      const next = clip.gain_keyframes.filter((_, i) => i !== sel.index);
      selectedKf = null;
      await projectState.setClipGainKeyframes(track.id, clip.id, next);
    } else {
      const track = sequence.tracks.find((t) => t.id === sel.trackId);
      if (!track) return;
      const next = track.gain_keyframes.filter((_, i) => i !== sel.index);
      selectedKf = null;
      await projectState.setTrackGainKeyframes(track.id, next);
    }
  }

  /** Any drag in progress? Used to gate one-shot handlers (contextmenu,
   *  dblclick) so they can't mutate the underlying lane while a drag
   *  preview is still holding a captured snapshot — that race would
   *  silently re-commit the pre-edit state on mouseup. Adversarial
   *  review finding #3. */
  function dragInFlight(): boolean {
    return !!(dragStart || fadeDrag || kfDrag || moveDrag || trimDrag);
  }

  /** Right-click on a fade wedge or keyframe removes it. */
  function onContextMenu(e: MouseEvent) {
    if (!canvas || !sequence) return;
    if (dragInFlight()) return;
    const rect = canvas.getBoundingClientRect();
    const localX = e.clientX - rect.left;
    const localY = e.clientY - rect.top;
    const hit = hitAutomation(localX, localY);
    if (!hit) return;
    if (hit.kind === 'fade-grip' || hit.kind === 'fade-chip') {
      e.preventDefault();
      const sideKey = hit.side;
      const trackId = hit.trackId;
      const clipId = hit.clipId;
      const setter = sideKey === 'in' ? projectState.setClipFadeIn : projectState.setClipFadeOut;
      void setter.call(projectState, trackId, clipId, 0, 'linear');
    } else if (hit.kind === 'kf-clip' || hit.kind === 'kf-track') {
      e.preventDefault();
      selectedKf =
        hit.kind === 'kf-clip'
          ? { scope: 'clip', trackId: hit.trackId, clipId: hit.clipId, index: hit.index }
          : { scope: 'track', trackId: hit.trackId, index: hit.index };
      void deleteSelectedKeyframe();
    }
  }

  /** Double-click on a fade grip or keyframe removes it. */
  function onDoubleClick(e: MouseEvent) {
    if (!canvas || !sequence) return;
    if (dragInFlight()) return;
    const rect = canvas.getBoundingClientRect();
    const localX = e.clientX - rect.left;
    const localY = e.clientY - rect.top;
    const hit = hitAutomation(localX, localY);
    if (!hit) return;
    if (hit.kind === 'fade-grip' || hit.kind === 'fade-chip') {
      e.preventDefault();
      const setter = hit.side === 'in' ? projectState.setClipFadeIn : projectState.setClipFadeOut;
      void setter.call(projectState, hit.trackId, hit.clipId, 0, 'linear');
    } else if (hit.kind === 'kf-clip' || hit.kind === 'kf-track') {
      e.preventDefault();
      selectedKf =
        hit.kind === 'kf-clip'
          ? { scope: 'clip', trackId: hit.trackId, clipId: hit.clipId, index: hit.index }
          : { scope: 'track', trackId: hit.trackId, index: hit.index };
      void deleteSelectedKeyframe();
    }
  }

  onMount(() => {
    if (!wrap) return;
    const ro = new ResizeObserver(resizeCanvas);
    ro.observe(wrap);
    resizeCanvas();
    window.addEventListener('mousemove', onMouseMove);
    window.addEventListener('mouseup', onMouseUp);
    window.addEventListener('keydown', onKeyDown);
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

    // Phase 7.5 M1 — start the metering poll. Owned by the timeline
    // (a singleton in this component tree) so the lifecycle is one
    // start / one stop. A redraw pump fires at the meter cadence so
    // the bars animate even when nothing else changes on the canvas.
    metersStore.start();
    const meterRedraw = window.setInterval(() => scheduleDraw(), 33);

    return () => {
      ro.disconnect();
      window.removeEventListener('mousemove', onMouseMove);
      window.removeEventListener('mouseup', onMouseUp);
      window.removeEventListener('mouseup', onWindowMouseUp);
      window.removeEventListener('keydown', onKeyDown);
      window.clearInterval(playheadPoll);
      window.clearInterval(meterRedraw);
      metersStore.stop();
    };
  });
</script>

<div class="flex h-full w-full flex-col" bind:this={wrap}>
  <canvas
    bind:this={canvas}
    class="block h-full w-full cursor-grab active:cursor-grabbing"
    onwheel={onWheel}
    onmousedown={onMouseDown}
    oncontextmenu={onContextMenu}
    ondblclick={onDoubleClick}
    onmouseleave={() => {
      if (hoverClipId !== null) {
        hoverClipId = null;
        scheduleDraw();
      }
    }}
  ></canvas>
</div>
