<script lang="ts">
  import { onMount, untrack } from 'svelte';
  import { open } from '@tauri-apps/plugin-dialog';
  import {
    backendVersion,
    currentTimelineSeconds,
    evalNumber,
    isKeyframes,
    COLOR_SPACE_OPTIONS,
    type ColorSpaceLabel,
    type ClipContents,
    type EffectInfo,
    type EffectValue,
    type Keyframe,
    type KeyframeInterp,
    type TrackContents,
    decodeAndRenderAt,
    decodeSequenceAt,
    diagLog,
    isPlaying,
    lastResumePts,
    openFile,
    playSequence,
    setSourceViewportBounds,
    setViewportBounds,
    startPlayback,
    stopPlayback,
    type StreamInfo
  } from '$lib/ipc';
  import { projectState } from '$lib/project.svelte';
  import { dragState } from '$lib/drag.svelte';
  import { selectionState } from '$lib/selection.svelte';
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
  let sourceViewportEl: HTMLDivElement | null = $state(null);

  // Bumped whenever something in the project changes that the Timeline
  // canvas should re-render in response to. We track both the project
  // info (sequence/project changes) and the edit tick (clip inserts).
  //
  // NOTE: the `++` would read+write the same rune inside the effect,
  // which Svelte 5 trips as `effect_update_depth_exceeded`. We wrap
  // the mutation in `untrack` so the read isn't tracked as a dep.
  let timelineRefreshTick = $state<number>(0);

  // + Title modal state. Text + duration + style are edited inline;
  // on submit we call projectState.createTitle which adds the
  // synthetic media to the library. The user then drags it onto the
  // timeline like any other clip.
  let titleModalOpen = $state<boolean>(false);
  let titleDraftText = $state<string>('Title');
  let titleDraftDuration = $state<number>(3);
  let titleDraftSize = $state<number>(96);
  let titleSubmitting = $state<boolean>(false);
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
    if (sourceViewportEl) observer.observe(sourceViewportEl);
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
      case 'Escape':
        if (selectionState.count > 0) {
          e.preventDefault();
          selectionState.clear();
        }
        return;
      case 'Delete':
      case 'Backspace':
        if (selectionState.count > 0 && projectState.sequence) {
          e.preventDefault();
          deleteSelectedClips();
        }
        return;
      case 'i':
      case 'I':
        if (projectState.source) {
          e.preventDefault();
          projectState.setSourceMarkIn();
        }
        return;
      case 'o':
      case 'O':
        if (projectState.source) {
          e.preventDefault();
          projectState.setSourceMarkOut();
        }
        return;
      case 'x':
      case 'X':
        if (projectState.source?.mark_in_pts != null || projectState.source?.mark_out_pts != null) {
          e.preventDefault();
          projectState.clearSourceMarks();
        }
        return;
      case ',':
        // Insert: ripple-place a source clip at the Record playhead.
        if (projectState.source && projectState.sequence) {
          e.preventDefault();
          const tid = firstVideoTrackId();
          if (tid) projectState.insertFromSource(tid, false);
        }
        return;
      case '.':
        // Overwrite at Record playhead.
        if (projectState.source && projectState.sequence) {
          e.preventDefault();
          const tid = firstVideoTrackId();
          if (tid) projectState.insertFromSource(tid, true);
        }
        return;
      case 't':
      case 'T':
        // Drop a 1s cross-dissolve at the cut after the selected clip.
        if (selectionState.count === 1 && projectState.sequence) {
          e.preventDefault();
          addDissolveForSelection();
        }
        return;
      case 'z':
      case 'Z':
        if (e.metaKey || e.ctrlKey) {
          e.preventDefault();
          if (e.shiftKey) projectState.redo();
          else projectState.undo();
        }
        return;
      case 'y':
      case 'Y':
        // Windows-style redo, also accepted on Mac as an alt.
        if (e.metaKey || e.ctrlKey) {
          e.preventDefault();
          projectState.redo();
        }
        return;
    }
  }

  function reportBoundsFor(el: HTMLDivElement, setter: (x: number, y: number, w: number, h: number) => Promise<void>, label: string) {
    const r = el.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    const x = Math.max(0, Math.round(r.left * dpr));
    const y = Math.max(0, Math.round(r.top * dpr));
    const w = Math.max(0, Math.round(r.width * dpr));
    const h = Math.max(0, Math.round(r.height * dpr));
    setter(x, y, w, h).catch((e) => console.error(label, e));
  }

  function reportBounds() {
    if (viewportEl) reportBoundsFor(viewportEl, setViewportBounds, 'set_viewport_bounds');
    if (sourceViewportEl)
      reportBoundsFor(sourceViewportEl, setSourceViewportBounds, 'set_source_viewport_bounds');
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

  /** Clip-relative time of the current playhead, clamped to >= 0. */
  async function clipRelativeTime(c: ClipContents): Promise<number> {
    const t = (await currentTimelineSeconds()) ?? c.timeline_in_seconds;
    return Math.max(0, t - c.timeline_in_seconds);
  }

  /** Set or update a numeric param value. If the param is already
   *  keyframed, this writes a keyframe at the current playhead (or
   *  replaces the existing one at that time). Otherwise it writes a
   *  plain `Number`. Lets the slider DTRT in both modes. */
  async function setOrKeyframe(
    track: TrackContents,
    clip: ClipContents,
    effect: EffectInfo,
    key: string,
    value: number
  ) {
    const existing = effect.params[key];
    if (existing !== undefined && isKeyframes(existing)) {
      const clipT = await clipRelativeTime(clip);
      // Preserve the outgoing interp of any keyframe we're
      // overwriting so sliding the value at an existing kf doesn't
      // silently flip its easing back to Linear.
      const prev = existing.find(
        (k) => Math.abs(k.time_seconds - clipT) <= 1e-3
      );
      const kfs: Keyframe[] = existing.filter(
        (k) => Math.abs(k.time_seconds - clipT) > 1e-3
      );
      kfs.push({
        time_seconds: clipT,
        value,
        interp: prev?.interp ?? 'Linear',
      });
      kfs.sort((a, b) => a.time_seconds - b.time_seconds);
      await projectState.updateEffectParam(track.id, clip.id, effect.id, key, kfs);
    } else {
      await projectState.updateEffectParam(track.id, clip.id, effect.id, key, value);
    }
  }

  /** Pin a keyframe at the current playhead with the param's
   *  currently-evaluated value. First click converts a plain number
   *  into a keyframes list. */
  async function addKeyframeAtPlayhead(
    track: TrackContents,
    clip: ClipContents,
    effect: EffectInfo,
    key: string
  ) {
    const clipT = await clipRelativeTime(clip);
    const existing = effect.params[key];
    const currentVal =
      existing !== undefined ? evalNumber(existing, clipT, 0) : 0;
    let kfs: Keyframe[];
    let prevInterp: KeyframeInterp = 'Linear';
    if (existing !== undefined && isKeyframes(existing)) {
      const prev = existing.find(
        (k) => Math.abs(k.time_seconds - clipT) <= 1e-3
      );
      if (prev?.interp) prevInterp = prev.interp;
      kfs = existing.filter((k) => Math.abs(k.time_seconds - clipT) > 1e-3);
    } else {
      kfs = [];
    }
    kfs.push({ time_seconds: clipT, value: currentVal, interp: prevInterp });
    kfs.sort((a, b) => a.time_seconds - b.time_seconds);
    await projectState.updateEffectParam(track.id, clip.id, effect.id, key, kfs);
  }

  /** Remove a keyframe by time. If the resulting list is empty,
   *  collapses the param back to a plain `Number(0)`. */
  async function removeKeyframe(
    track: TrackContents,
    clip: ClipContents,
    effect: EffectInfo,
    key: string,
    time_seconds: number
  ) {
    const existing = effect.params[key];
    if (existing === undefined || !isKeyframes(existing)) return;
    const kfs = existing.filter(
      (k) => Math.abs(k.time_seconds - time_seconds) > 1e-3
    );
    if (kfs.length === 0) {
      await projectState.updateEffectParam(track.id, clip.id, effect.id, key, 0);
    } else {
      await projectState.updateEffectParam(track.id, clip.id, effect.id, key, kfs);
    }
  }

  /** Cycle a keyframe's outgoing interpolation: Linear → Bezier →
   *  Hold → Linear. Affects the segment from this keyframe to the
   *  next one in the list. */
  async function cycleKeyframeInterp(
    track: TrackContents,
    clip: ClipContents,
    effect: EffectInfo,
    key: string,
    time_seconds: number
  ) {
    const existing = effect.params[key];
    if (existing === undefined || !isKeyframes(existing)) return;
    const next = (cur: Keyframe['interp']): KeyframeInterp => {
      const c = cur ?? 'Linear';
      if (c === 'Linear') return 'Bezier';
      if (c === 'Bezier') return 'Hold';
      return 'Linear';
    };
    const kfs = existing.map((k) =>
      Math.abs(k.time_seconds - time_seconds) <= 1e-3
        ? { ...k, interp: next(k.interp) }
        : k
    );
    await projectState.updateEffectParam(track.id, clip.id, effect.id, key, kfs);
  }

  /** One-character label for a keyframe's interp mode. */
  function interpLabel(i: Keyframe['interp']): string {
    const c = i ?? 'Linear';
    if (c === 'Bezier') return 'B';
    if (c === 'Hold') return 'H';
    return 'L';
  }

  /** First enabled video track on the active sequence, or null. */
  function firstVideoTrackId(): string | null {
    const seq = projectState.sequence;
    if (!seq) return null;
    return seq.tracks.find((t) => t.kind === 'video' && t.enabled)?.id ?? null;
  }

  /** Toggle a 1s cross-dissolve at the cut following the single
   *  selected clip. Adds one if absent, removes the existing one if
   *  present, no-op if there's no clip immediately after the
   *  selection on the same track. */
  async function addDissolveForSelection() {
    const seq = projectState.sequence;
    if (!seq) return;
    if (selectionState.count !== 1) return;
    const id = [...selectionState.ids][0];
    for (const t of seq.tracks) {
      const idx = t.clips.findIndex((c) => c.id === id);
      if (idx < 0) continue;
      if (idx + 1 >= t.clips.length) return;
      const clip = t.clips[idx];
      const cutAt = clip.timeline_in_seconds + clip.duration_seconds;
      const existing = (t.transitions ?? []).find(
        (tr) => Math.abs(tr.position_seconds - cutAt) < 1e-3
      );
      if (existing) {
        await projectState.removeTransition(t.id, existing.id);
      } else {
        await projectState.addDissolveAtCut(t.id, id, 1);
      }
      return;
    }
  }

  /** Delete every currently-selected clip from the active sequence,
   *  one engine call per clip. Each becomes its own undo step. */
  async function deleteSelectedClips() {
    const seq = projectState.sequence;
    if (!seq) return;
    const ids = [...selectionState.ids];
    // Build (trackId, clipId) pairs first, then issue removes. We can't
    // rely on `projectState.sequence` after the first await because it
    // refreshes mid-loop.
    const targets: { trackId: string; clipId: string }[] = [];
    for (const id of ids) {
      for (const t of seq.tracks) {
        if (t.clips.some((c) => c.id === id)) {
          targets.push({ trackId: t.id, clipId: id });
          break;
        }
      }
    }
    selectionState.clear();
    for (const { trackId, clipId } of targets) {
      await projectState.removeClip(trackId, clipId);
    }
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

  /** HH:MM:SS.mmm formatting for the Inspector. */
  function formatPrecise(seconds: number): string {
    const s = Math.max(0, seconds);
    const total = Math.floor(s);
    const ms = Math.round((s - total) * 1000);
    const h = Math.floor(total / 3600);
    const m = Math.floor((total % 3600) / 60);
    const ss = total % 60;
    const head =
      h > 0
        ? `${h}:${String(m).padStart(2, '0')}:${String(ss).padStart(2, '0')}`
        : `${m}:${String(ss).padStart(2, '0')}`;
    return `${head}.${String(ms).padStart(3, '0')}`;
  }

  /** Find the selected clip (when exactly one is selected) in the
   *  current sequence so the Inspector can render its details. */
  const selectedClipDetails = $derived.by(() => {
    const ids = selectionState.ids;
    if (ids.length !== 1) return null;
    const seq = projectState.sequence;
    if (!seq) return null;
    for (const t of seq.tracks) {
      const c = t.clips.find((c) => c.id === ids[0]);
      if (c) return { clip: c, track: t };
    }
    return null;
  });

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

  /** True when we should treat Play as "play the active sequence"
   *  (project + sequence + at least one clip somewhere). When false
   *  Play falls back to the Phase-2 single-file path. */
  function isSequencePlayable(): boolean {
    const p = projectState.info;
    if (!p) return false;
    return p.sequences.some((s) => s.clip_count > 0);
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
    } else if (isSequencePlayable()) {
      // Phase 4.7.5: play the active sequence from the current
      // playhead (so Stop → Play resumes where it left off rather
      // than restarting from the first clip). Phase 4.7.6: when the
      // playhead sits at/past the end of the sequence, rewind to 0
      // so Play means "play again" instead of "play nothing".
      try {
        let from = (await currentTimelineSeconds()) ?? 0;
        const seq = projectState.sequence;
        if (seq && from >= seq.duration_seconds - 0.05) {
          from = 0;
        }
        await playSequence(from);
        playing = true;
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
      <span class="ml-3 text-xs text-vance-muted flex items-center gap-1">
        Project: <span class="text-vance-fg font-medium">{projectState.info.name}</span>
        {#if projectState.path}
          <span class="text-vance-muted">·</span>
          <span class="text-vance-muted">{projectState.path}</span>
        {:else}
          <span class="text-vance-muted">·</span>
          <span class="text-yellow-400">unsaved</span>
        {/if}
        <span class="text-vance-muted ml-2">·</span>
        <span class="text-vance-muted">Working space:</span>
        <select
          class="bg-vance-bg border-vance-border rounded border px-1 py-0.5 text-[10px]"
          value={projectState.info.working_color_space}
          onchange={(ev) => {
            const v = (ev.currentTarget as HTMLSelectElement).value as ColorSpaceLabel;
            projectState.setWorkingColorSpace(v);
          }}
          title="Project working color space (Phase 6.2)"
        >
          {#each COLOR_SPACE_OPTIONS as opt (opt.value)}
            <option value={opt.value}>{opt.label}</option>
          {/each}
        </select>
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
        class="rounded bg-vance-bg border-vance-border border px-2 py-1 text-xs hover:brightness-110 disabled:opacity-50"
        onclick={() => projectState.undo()}
        disabled={!projectState.canUndo}
        title="Undo (⌘Z)"
      >
        Undo
      </button>
      <button
        class="rounded bg-vance-bg border-vance-border border px-2 py-1 text-xs hover:brightness-110 disabled:opacity-50"
        onclick={() => projectState.redo()}
        disabled={!projectState.canRedo}
        title="Redo (⌘⇧Z)"
      >
        Redo
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
          <div class="flex gap-1">
            <button
              class="rounded bg-vance-bg border-vance-border border px-2 py-0.5 text-[10px] hover:brightness-125"
              onclick={() => (titleModalOpen = true)}
            >
              + Title
            </button>
            <button
              class="rounded bg-vance-bg border-vance-border border px-2 py-0.5 text-[10px] hover:brightness-125"
              onclick={handleImportMedia}
            >
              + Import
            </button>
          </div>
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
                  ondblclick={() => projectState.loadSource(m.id)}
                  title="Drag onto the timeline · Double-click to preview in Source"
                >
                  <div class="font-medium truncate">{m.name}</div>
                  <div class="text-[10px] text-vance-muted">
                    {m.kind} · {formatDuration(m.duration_seconds)}
                  </div>
                  <div
                    class="mt-1 text-[10px] flex items-center gap-1"
                    onmousedown={(e) => e.stopPropagation()}
                    role="presentation"
                  >
                    <span class="text-vance-muted">space:</span>
                    <select
                      class="bg-vance-panel border-vance-border rounded border px-1 py-0.5 text-[10px]"
                      value={m.input_color_space ?? ''}
                      onchange={(ev) => {
                        const v = (ev.currentTarget as HTMLSelectElement).value;
                        projectState.setMediaInputColorSpace(
                          m.id,
                          v === '' ? null : (v as ColorSpaceLabel)
                        );
                      }}
                      title="Override the input color space tag for this item"
                    >
                      <option value="">(default)</option>
                      {#each COLOR_SPACE_OPTIONS as opt (opt.value)}
                        <option value={opt.value}>{opt.label}</option>
                      {/each}
                    </select>
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

    <!-- Center: Source + Record viewports + transport -->
    <main class="flex flex-1 flex-col min-w-0">
      <div class="flex flex-1 min-w-0 min-h-0">
        <!-- Source monitor (left) — previews a single MediaItem. -->
        <div class="flex flex-1 flex-col min-w-0 m-4 mr-2">
          <div class="mb-1 flex items-center gap-2 text-[10px] uppercase tracking-wider text-vance-muted">
            <span>Source</span>
            {#if projectState.sourceName}
              <span class="truncate normal-case text-vance-fg">{projectState.sourceName}</span>
            {/if}
          </div>
          <div
            class="flex-1 rounded border border-dashed border-[#333] bg-black"
            bind:this={sourceViewportEl}
          ></div>
          <!-- Source scrub bar + time display. Click to seek;
               I / O mark in/out at playhead; X clears marks;
               , inserts, . overwrites at the Record playhead. -->
          {#if projectState.source}
            {@const s = projectState.source}
            {@const tb = s.time_base_num / s.time_base_den}
            {@const cur = (s.last_pts ?? 0) * tb}
            {@const total = (s.duration_pts ?? 0) * tb}
            {@const frac = total > 0 ? Math.min(1, Math.max(0, cur / total)) : 0}
            {@const inSec = s.mark_in_pts != null ? s.mark_in_pts * tb : null}
            {@const outSec = s.mark_out_pts != null ? s.mark_out_pts * tb : null}
            <div class="mt-1 flex items-center gap-2 text-[10px] text-vance-muted">
              <span class="font-mono">{formatPrecise(cur)}</span>
              <div
                class="relative h-2 flex-1 cursor-pointer rounded bg-[#1a1d22]"
                role="slider"
                tabindex="0"
                aria-valuemin="0"
                aria-valuemax={total}
                aria-valuenow={cur}
                onclick={(e) => {
                  if (total <= 0) return;
                  const target = e.currentTarget as HTMLDivElement;
                  const r = target.getBoundingClientRect();
                  const x = (e.clientX - r.left) / r.width;
                  projectState.seekSource(Math.max(0, Math.min(1, x)) * total);
                }}
              >
                <!-- Highlight between marks. -->
                {#if total > 0 && inSec != null && outSec != null}
                  <div
                    class="absolute top-0 h-full bg-[var(--color-vance-accent)]/25"
                    style:left="{(inSec / total) * 100}%"
                    style:width="{Math.max(0, ((outSec - inSec) / total) * 100)}%"
                  ></div>
                {/if}
                <!-- Progress fill. -->
                <div
                  class="absolute top-0 h-full bg-[var(--color-vance-accent)]/70"
                  style:width="{frac * 100}%"
                ></div>
                <!-- Mark-in / mark-out vertical lines. -->
                {#if total > 0 && inSec != null}
                  <div
                    class="absolute top-[-2px] h-[12px] w-[2px] bg-[#f4a261]"
                    style:left="{(inSec / total) * 100}%"
                  ></div>
                {/if}
                {#if total > 0 && outSec != null}
                  <div
                    class="absolute top-[-2px] h-[12px] w-[2px] bg-[#f4a261]"
                    style:left="{(outSec / total) * 100}%"
                  ></div>
                {/if}
              </div>
              <span class="font-mono">{formatPrecise(total)}</span>
            </div>
            <div class="mt-1 flex items-center gap-1 text-[10px]">
              <button
                class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 hover:brightness-125"
                onclick={() => projectState.setSourceMarkIn()}
                title="I: mark in at playhead"
              >
                Mark In
              </button>
              <button
                class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 hover:brightness-125"
                onclick={() => projectState.setSourceMarkOut()}
                title="O: mark out at playhead"
              >
                Mark Out
              </button>
              <button
                class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 hover:brightness-125 disabled:opacity-50"
                onclick={() => projectState.clearSourceMarks()}
                disabled={s.mark_in_pts == null && s.mark_out_pts == null}
                title="X: clear marks"
              >
                Clear
              </button>
              <span class="mx-1 text-vance-muted">·</span>
              <button
                class="rounded bg-[var(--color-vance-accent)] px-2 py-0.5 font-semibold text-[#003040] hover:brightness-110 disabled:opacity-50"
                onclick={() => {
                  const tid = firstVideoTrackId();
                  if (tid) projectState.insertFromSource(tid, false);
                }}
                disabled={!projectState.sequence}
                title=", : insert at Record playhead"
              >
                Insert
              </button>
              <button
                class="rounded bg-vance-bg border-vance-border border px-2 py-0.5 hover:brightness-125 disabled:opacity-50"
                onclick={() => {
                  const tid = firstVideoTrackId();
                  if (tid) projectState.insertFromSource(tid, true);
                }}
                disabled={!projectState.sequence}
                title=". : overwrite at Record playhead"
              >
                Overwrite
              </button>
            </div>
          {/if}
        </div>
        <!-- Record monitor (right) — previews the active sequence. -->
        <div class="flex flex-1 flex-col min-w-0 m-4 ml-2">
          <div class="mb-1 text-[10px] uppercase tracking-wider text-vance-muted">
            Record
          </div>
          <div
            class="flex-1 rounded border border-dashed border-[#333] bg-black"
            bind:this={viewportEl}
          >
            <!-- The native wgpu surface paints over this div. -->
          </div>
        </div>
      </div>
      <div
        class="border-vance-border bg-vance-panel flex h-12 flex-shrink-0 items-center gap-3 border-t px-4 text-xs"
      >
        <div class="flex items-center gap-1">
          <button
            class="rounded bg-vance-bg border-vance-border border px-2 py-1 disabled:opacity-50"
            onclick={() => jumpSeconds(-1)}
            disabled={!info && !isSequencePlayable()}
            title="J: jump back 1s"
          >
            J
          </button>
          <button
            class="rounded bg-vance-bg border-vance-border border px-2 py-1 disabled:opacity-50"
            onclick={() => stepFrame(-1)}
            disabled={!info && !isSequencePlayable()}
            title="←: step back 1 frame"
          >
            ‹
          </button>
          <button
            class="rounded bg-[var(--color-vance-accent)] px-3 py-1 font-semibold text-[#003040] disabled:opacity-50"
            onclick={togglePlayback}
            disabled={!info && !isSequencePlayable()}
            title="Space / L: play, K: pause"
          >
            {playing ? 'Stop' : 'Play'}
          </button>
          <button
            class="rounded bg-vance-bg border-vance-border border px-2 py-1 disabled:opacity-50"
            onclick={() => stepFrame(1)}
            disabled={!info && !isSequencePlayable()}
            title="→: step forward 1 frame"
          >
            ›
          </button>
          <button
            class="rounded bg-vance-bg border-vance-border border px-2 py-1 disabled:opacity-50"
            onclick={() => jumpSeconds(1)}
            disabled={!info && !isSequencePlayable()}
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
            disabled={!info && !isSequencePlayable()}
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

    {#if titleModalOpen}
      <div
        class="fixed inset-0 z-50 flex items-center justify-center bg-black/60"
        onclick={(e) => {
          if (e.target === e.currentTarget) titleModalOpen = false;
        }}
        role="presentation"
      >
        <div class="bg-vance-panel border-vance-border w-[28rem] rounded border p-4 shadow-xl">
          <h3 class="text-vance-fg mb-3 text-sm font-semibold">New Title</h3>
          <label class="mb-2 block text-xs">
            <span class="text-vance-muted mb-1 block">Text</span>
            <input
              type="text"
              class="bg-vance-bg border-vance-border w-full rounded border px-2 py-1 text-sm"
              bind:value={titleDraftText}
              placeholder="Title text"
            />
          </label>
          <div class="mb-3 grid grid-cols-2 gap-3">
            <label class="block text-xs">
              <span class="text-vance-muted mb-1 block">Duration (s)</span>
              <input
                type="number"
                min="0.1"
                step="0.1"
                class="bg-vance-bg border-vance-border w-full rounded border px-2 py-1 text-sm"
                bind:value={titleDraftDuration}
              />
            </label>
            <label class="block text-xs">
              <span class="text-vance-muted mb-1 block">Font size (px)</span>
              <input
                type="number"
                min="8"
                step="4"
                class="bg-vance-bg border-vance-border w-full rounded border px-2 py-1 text-sm"
                bind:value={titleDraftSize}
              />
            </label>
          </div>
          <div class="flex justify-end gap-2">
            <button
              type="button"
              class="rounded bg-vance-bg border-vance-border border px-3 py-1 text-xs"
              onclick={() => (titleModalOpen = false)}
              disabled={titleSubmitting}
            >
              Cancel
            </button>
            <button
              type="button"
              class="rounded bg-[var(--color-vance-accent)] px-3 py-1 text-xs font-semibold text-[#003040]"
              onclick={async () => {
                titleSubmitting = true;
                const id = await projectState.createTitle({
                  displayName: titleDraftText.trim() || 'Title',
                  durationSeconds: titleDraftDuration,
                  text: titleDraftText,
                  sizePx: titleDraftSize,
                });
                titleSubmitting = false;
                if (id) titleModalOpen = false;
              }}
              disabled={titleSubmitting}
            >
              {titleSubmitting ? 'Creating…' : 'Add to library'}
            </button>
          </div>
        </div>
      </div>
    {/if}

    <!-- Inspector -->
    <aside
      class="border-vance-border bg-vance-panel w-72 flex-shrink-0 overflow-y-auto overflow-x-hidden border-l p-4"
    >
      <h2 class="text-vance-muted mb-3 text-xs uppercase tracking-wider">Inspector</h2>

      {#if selectedClipDetails}
        {@const c = selectedClipDetails.clip}
        {@const t = selectedClipDetails.track}
        <div class="mb-3">
          <div class="text-xs uppercase tracking-wider text-vance-muted">Clip</div>
          <div class="mt-1 break-all text-sm font-medium">{c.name}</div>
        </div>
        <dl class="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 font-mono text-xs">
          <dt class="text-vance-muted">track</dt>
          <dd>{t.name} ({t.kind})</dd>
          <dt class="text-vance-muted">tl in</dt>
          <dd>{formatPrecise(c.timeline_in_seconds)}</dd>
          <dt class="text-vance-muted">tl out</dt>
          <dd>{formatPrecise(c.timeline_in_seconds + c.duration_seconds)}</dd>
          <dt class="text-vance-muted">duration</dt>
          <dd>{formatPrecise(c.duration_seconds)}</dd>
          <dt class="text-vance-muted">src in</dt>
          <dd>{formatPrecise(c.source_in_seconds)}</dd>
          <dt class="text-vance-muted">src out</dt>
          <dd>{formatPrecise(c.source_in_seconds + c.duration_seconds)}</dd>
          <dt class="text-vance-muted">enabled</dt>
          <dd>{c.enabled ? 'yes' : 'muted'}</dd>
        </dl>

        <!-- Effects: list each attached effect with its kind + a
             remove button. 5.2+ adds parameter editing (opacity
             slider, transform widgets, etc.). -->
        <div class="mt-4">
          <div class="mb-1 flex items-center justify-between text-[11px] uppercase tracking-wider">
            <span class="text-vance-muted">Effects</span>
            <div class="flex gap-1">
              <button
                class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 text-[10px] hover:brightness-125"
                onclick={() => projectState.addEffect(t.id, c.id, 'opacity', { value: 1 })}
                title="Attach an opacity effect"
              >
                + Opacity
              </button>
              <button
                class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 text-[10px] hover:brightness-125"
                onclick={() =>
                  projectState.addEffect(t.id, c.id, 'transform', {
                    translate_x: 0,
                    translate_y: 0,
                    scale_x: 1,
                    scale_y: 1,
                    rotation: 0
                  })}
                title="Attach a transform effect (position/scale/rotate)"
              >
                + Transform
              </button>
              <button
                class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 text-[10px] hover:brightness-125"
                onclick={() =>
                  projectState.addEffect(t.id, c.id, 'crop', {
                    left: 0,
                    right: 0,
                    top: 0,
                    bottom: 0
                  })}
                title="Attach a crop effect"
              >
                + Crop
              </button>
              <button
                class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 text-[10px] hover:brightness-125"
                onclick={() =>
                  projectState.addEffect(t.id, c.id, 'color', {
                    brightness: 0,
                    contrast: 1
                  })}
                title="Attach a brightness/contrast effect"
              >
                + Color
              </button>
              <button
                class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 text-[10px] hover:brightness-125"
                onclick={() =>
                  projectState.addEffect(t.id, c.id, 'chroma_key', {
                    r: 0,
                    g: 1,
                    b: 0,
                    tolerance: 0.2,
                    softness: 0.08
                  })}
                title="Attach a chroma key (green-screen) effect"
              >
                + Chroma
              </button>
              <button
                class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 text-[10px] hover:brightness-125"
                onclick={() =>
                  projectState.addEffect(t.id, c.id, 'luma_key', {
                    threshold: 0.5,
                    direction: 1,
                    softness: 0.05
                  })}
                title="Attach a luma key (brightness mask) effect"
              >
                + Luma
              </button>
            </div>
          </div>
          {#if c.effects.length === 0}
            <p class="text-vance-muted text-xs italic">No effects.</p>
          {:else}
            <ul class="flex flex-col gap-2 text-xs">
              {#each c.effects as e (e.id)}
                <li class="rounded border border-vance-border bg-vance-bg px-2 py-1.5">
                  <div class="flex items-center justify-between">
                    <span class="font-mono">{e.kind}</span>
                    <button
                      class="text-vance-muted hover:text-red-400"
                      onclick={() => projectState.removeEffect(t.id, c.id, e.id)}
                      title="Remove effect"
                    >
                      ✕
                    </button>
                  </div>
                  {#snippet numericParam(
                    key: string,
                    label: string,
                    min: number,
                    max: number,
                    step: number,
                    defaultVal: number,
                    precision: number
                  )}
                    {@const raw = e.params[key]}
                    {@const animated = raw !== undefined && isKeyframes(raw)}
                    {@const kfs: Keyframe[] =
                      animated && raw !== undefined && isKeyframes(raw) ? raw : []}
                    {@const cur =
                      raw !== undefined ? evalNumber(raw, 0, defaultVal) : defaultVal}
                    <div class="mt-1 grid grid-cols-[3rem_minmax(0,1fr)_2.25rem_1rem] items-center gap-x-2">
                      <span class="text-vance-muted text-[10px]">{label}</span>
                      <input
                        type="range"
                        {min}
                        {max}
                        {step}
                        value={cur}
                        class="w-full min-w-0"
                        onchange={(ev) => {
                          const v = parseFloat((ev.currentTarget as HTMLInputElement).value);
                          setOrKeyframe(t, c, e, key, v);
                        }}
                      />
                      <span class="font-mono text-right text-[10px]">{cur.toFixed(precision)}</span>
                      <button
                        class="text-[12px] leading-none {animated ? 'text-[var(--color-vance-accent)]' : 'text-vance-muted'} hover:brightness-125"
                        onclick={() => addKeyframeAtPlayhead(t, c, e, key)}
                        title="Add keyframe at playhead"
                      >
                        ◆
                      </button>
                    </div>
                    {#if animated && kfs.length > 0}
                      <ul class="mt-1 flex flex-col gap-0.5 pl-12 text-[10px] text-vance-muted">
                        {#each kfs as k (k.time_seconds)}
                          <li class="flex items-center justify-between gap-2">
                            <span class="font-mono">{k.time_seconds.toFixed(2)}s</span>
                            <span class="font-mono flex-1 text-right">{k.value.toFixed(precision)}</span>
                            <button
                              class="rounded border border-vance-border px-1 font-mono hover:brightness-125"
                              onclick={() => cycleKeyframeInterp(t, c, e, key, k.time_seconds)}
                              title="Outgoing interpolation: click to cycle Linear → Bezier → Hold"
                            >
                              {interpLabel(k.interp)}
                            </button>
                            <button
                              class="hover:text-red-400"
                              onclick={() => removeKeyframe(t, c, e, key, k.time_seconds)}
                              title="Remove keyframe"
                            >
                              ✕
                            </button>
                          </li>
                        {/each}
                      </ul>
                    {/if}
                  {/snippet}
                  {#if e.kind === 'opacity'}
                    {@const raw = e.params.value}
                    {@const animated = raw !== undefined && isKeyframes(raw)}
                    {@const kfs: Keyframe[] =
                      animated && raw !== undefined && isKeyframes(raw) ? raw : []}
                    {@const cur =
                      raw !== undefined ? evalNumber(raw, 0, 1) : 1}
                    <div class="mt-1 grid grid-cols-[3rem_minmax(0,1fr)_2.25rem_1rem] items-center gap-x-2">
                      <span class="text-vance-muted text-[10px]">value</span>
                      <input
                        type="range"
                        min="0"
                        max="1"
                        step="0.01"
                        value={cur}
                        class="w-full min-w-0"
                        onchange={(ev) => {
                          const v = parseFloat((ev.currentTarget as HTMLInputElement).value);
                          setOrKeyframe(t, c, e, 'value', v);
                        }}
                      />
                      <span class="font-mono text-right text-[10px]">{cur.toFixed(2)}</span>
                      <button
                        class="text-[12px] leading-none {animated ? 'text-[var(--color-vance-accent)]' : 'text-vance-muted'} hover:brightness-125"
                        onclick={() => addKeyframeAtPlayhead(t, c, e, 'value')}
                        title="Add keyframe at playhead"
                      >
                        ◆
                      </button>
                    </div>
                    {#if animated && kfs.length > 0}
                      <ul class="mt-1 flex flex-col gap-0.5 pl-12 text-[10px] text-vance-muted">
                        {#each kfs as k (k.time_seconds)}
                          <li class="flex items-center justify-between gap-2">
                            <span class="font-mono">{k.time_seconds.toFixed(2)}s</span>
                            <span class="font-mono">{k.value.toFixed(2)}</span>
                            <button
                              class="hover:text-red-400"
                              onclick={() => removeKeyframe(t, c, e, 'value', k.time_seconds)}
                              title="Remove keyframe"
                            >
                              ✕
                            </button>
                          </li>
                        {/each}
                      </ul>
                    {/if}
                  {:else if e.kind === 'transform'}
                    {@const tx = typeof e.params.translate_x === 'number' ? e.params.translate_x : 0}
                    {@const ty = typeof e.params.translate_y === 'number' ? e.params.translate_y : 0}
                    {@const sx = typeof e.params.scale_x === 'number' ? e.params.scale_x : 1}
                    {@const sy = typeof e.params.scale_y === 'number' ? e.params.scale_y : 1}
                    {@const rot = typeof e.params.rotation === 'number' ? e.params.rotation : 0}
                    <div class="mt-1 grid grid-cols-[3rem_minmax(0,1fr)_2.25rem] items-center gap-x-2 gap-y-1">
                      <span class="text-vance-muted text-[10px]">x</span>
                      <input
                        type="range"
                        min="-1"
                        max="1"
                        step="0.01"
                        value={tx}
                        class="w-full min-w-0"
                        onchange={(ev) => {
                          const v = parseFloat((ev.currentTarget as HTMLInputElement).value);
                          projectState.updateEffectParam(t.id, c.id, e.id, 'translate_x', v);
                        }}
                      />
                      <span class="font-mono text-right text-[10px]">{tx.toFixed(2)}</span>

                      <span class="text-vance-muted text-[10px]">y</span>
                      <input
                        type="range"
                        min="-1"
                        max="1"
                        step="0.01"
                        value={ty}
                        class="flex-1"
                        onchange={(ev) => {
                          const v = parseFloat((ev.currentTarget as HTMLInputElement).value);
                          projectState.updateEffectParam(t.id, c.id, e.id, 'translate_y', v);
                        }}
                      />
                      <span class="font-mono text-right text-[10px]">{ty.toFixed(2)}</span>

                      <span class="text-vance-muted text-[10px]">scale x</span>
                      <input
                        type="range"
                        min="0.1"
                        max="3"
                        step="0.01"
                        value={sx}
                        class="w-full min-w-0"
                        onchange={(ev) => {
                          const v = parseFloat((ev.currentTarget as HTMLInputElement).value);
                          projectState.updateEffectParam(t.id, c.id, e.id, 'scale_x', v);
                        }}
                      />
                      <span class="font-mono text-right text-[10px]">{sx.toFixed(2)}</span>

                      <span class="text-vance-muted text-[10px]">scale y</span>
                      <input
                        type="range"
                        min="0.1"
                        max="3"
                        step="0.01"
                        value={sy}
                        class="flex-1"
                        onchange={(ev) => {
                          const v = parseFloat((ev.currentTarget as HTMLInputElement).value);
                          projectState.updateEffectParam(t.id, c.id, e.id, 'scale_y', v);
                        }}
                      />
                      <span class="font-mono text-right text-[10px]">{sy.toFixed(2)}</span>

                      <span class="text-vance-muted text-[10px]">rot°</span>
                      <input
                        type="range"
                        min="-180"
                        max="180"
                        step="1"
                        value={rot}
                        class="w-full min-w-0"
                        onchange={(ev) => {
                          const v = parseFloat((ev.currentTarget as HTMLInputElement).value);
                          projectState.updateEffectParam(t.id, c.id, e.id, 'rotation', v);
                        }}
                      />
                      <span class="font-mono text-right text-[10px]">{rot.toFixed(0)}°</span>
                    </div>
                  {:else if e.kind === 'crop'}
                    {@render numericParam('left', 'left', 0, 0.95, 0.01, 0, 2)}
                    {@render numericParam('right', 'right', 0, 0.95, 0.01, 0, 2)}
                    {@render numericParam('top', 'top', 0, 0.95, 0.01, 0, 2)}
                    {@render numericParam('bottom', 'bot', 0, 0.95, 0.01, 0, 2)}
                  {:else if e.kind === 'color'}
                    {@render numericParam('brightness', 'bright', -1, 1, 0.01, 0, 2)}
                    {@render numericParam('contrast', 'contr', 0, 3, 0.01, 1, 2)}
                  {:else if e.kind === 'chroma_key'}
                    {@render numericParam('r', 'R', 0, 1, 0.01, 0, 2)}
                    {@render numericParam('g', 'G', 0, 1, 0.01, 1, 2)}
                    {@render numericParam('b', 'B', 0, 1, 0.01, 0, 2)}
                    {@render numericParam('tolerance', 'tol', 0, 1, 0.01, 0.2, 2)}
                    {@render numericParam('softness', 'soft', 0, 0.5, 0.01, 0.08, 2)}
                  {:else if e.kind === 'luma_key'}
                    {@render numericParam('threshold', 'thr', 0, 1, 0.01, 0.5, 2)}
                    {@render numericParam('direction', 'dir', -1, 1, 1, 1, 0)}
                    {@render numericParam('softness', 'soft', 0, 0.5, 0.01, 0.05, 2)}
                  {/if}
                </li>
              {/each}
            </ul>
          {/if}
        </div>
      {:else if selectionState.count > 1}
        <p class="text-vance-muted text-xs">
          {selectionState.count} clips selected. Single-selection details
          show here.
        </p>
      {:else if info}
        <div class="mb-2 text-xs uppercase tracking-wider text-vance-muted">File</div>
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
        <p class="text-vance-muted text-xs">
          Select a clip on the timeline to inspect its properties.
        </p>
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
