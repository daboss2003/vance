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
  import MasterMeter from '$lib/MasterMeter.svelte';
  import AudioEffectChain from '$lib/AudioEffectChain.svelte';
  import ColorWheel from '$lib/color/ColorWheel.svelte';
  import CurveEditor from '$lib/color/CurveEditor.svelte';
  import ScopeCanvas from '$lib/color/ScopeCanvas.svelte';
  import * as GK from '$lib/grade-keys';

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

  // Edit ↔ Color page toggle (Phase 6.6). The center viewers + Record
  // viewport stay mounted across the switch (only the side panels swap)
  // so the native wgpu surface never rebinds.
  let activePage = $state<'edit' | 'color'>('edit');
  // Which grade node's wheels are shown on the Color page.
  let selectedNodeId = $state<string | null>(null);
  // Guards the color-page auto-seek so we only decode once per
  // (page, clip) change rather than on every editTick refresh.
  let lastColorSeekKey = $state<string>('');
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
      (info.frame_rate_den / info.frame_rate_num) * (info.time_base_den / info.time_base_num)
    );
  }

  /** PTS for one second in the stream's time base. */
  function ptsPerSecond(): number {
    if (!info) return 1;
    return info.time_base_num !== 0 ? Math.round(info.time_base_den / info.time_base_num) : 1;
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
        if (ocioPanelOpen) {
          e.preventDefault();
          ocioPanelOpen = false;
        } else if (selectionState.count > 0) {
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

  function reportBoundsFor(
    el: HTMLDivElement,
    setter: (x: number, y: number, w: number, h: number) => Promise<void>,
    label: string
  ) {
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

  // Color page: when the user switches to Color or selects a different
  // clip, seek the Record viewport into that clip's range so the
  // preview shows the clip being graded (not whatever the timeline
  // playhead last decoded). Guarded by lastColorSeekKey so an editTick
  // refresh that re-derives selectedClipDetails doesn't re-decode.
  $effect(() => {
    const d = selectedClipDetails;
    const key = activePage === 'color' && d ? d.clip.id : '';
    if (key && key !== lastColorSeekKey) {
      const mid = d!.clip.timeline_in_seconds + d!.clip.duration_seconds / 2;
      untrack(() => {
        // Switching clips drops any active matte (it belonged to the
        // previous clip's node).
        if (matteNodeId) clearMatte();
        lastColorSeekKey = key;
        decodeSequenceAt(mid)
          .then(() => projectState.refreshScopes(256, 144, scopePreLut))
          .catch(() => {});
      });
    } else if (!key && lastColorSeekKey) {
      untrack(() => {
        if (matteNodeId) clearMatte();
        lastColorSeekKey = '';
      });
    }
  });

  // ---- Scopes (Phase 6.12) -------------------------------------------------
  let scopeKind = $state<'waveform' | 'parade' | 'vector' | 'histogram'>('waveform');
  let scopeLog = $state(true);
  let scopePreLut = $state(false);

  // Tell the engine to compute scopes only while the Color page is open
  // (it skips the GPU readback otherwise — zero cost).
  $effect(() => {
    const on = activePage === 'color';
    untrack(() => {
      projectState.setScopesEnabled(on);
      if (on) projectState.refreshScopes(256, 144, scopePreLut);
      // Matte solo is a Color-page viewer state — never leak it onto the
      // Edit page's preview.
      else clearMatte();
    });
  });

  // Recompute scopes for the current frame. Single-in-flight coalescing
  // lives in projectState.refreshScopes, so calling this on every grade
  // drag tick is safe.
  function refreshScopesNow() {
    if (activePage === 'color') projectState.refreshScopes(256, 144, scopePreLut);
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
      const prev = existing.find((k) => Math.abs(k.time_seconds - clipT) <= 1e-3);
      const kfs: Keyframe[] = existing.filter((k) => Math.abs(k.time_seconds - clipT) > 1e-3);
      kfs.push({
        time_seconds: clipT,
        value,
        interp: prev?.interp ?? 'Linear'
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
    const currentVal = existing !== undefined ? evalNumber(existing, clipT, 0) : 0;
    let kfs: Keyframe[];
    let prevInterp: KeyframeInterp = 'Linear';
    if (existing !== undefined && isKeyframes(existing)) {
      const prev = existing.find((k) => Math.abs(k.time_seconds - clipT) <= 1e-3);
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
    const kfs = existing.filter((k) => Math.abs(k.time_seconds - time_seconds) > 1e-3);
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
      Math.abs(k.time_seconds - time_seconds) <= 1e-3 ? { ...k, interp: next(k.interp) } : k
    );
    await projectState.updateEffectParam(track.id, clip.id, effect.id, key, kfs);
  }

  /** Open a file dialog for the project-wide view LUT (Phase 6.5).
   *  Wraps `projectState.setViewLut` so the engine + viewport
   *  light up immediately. */
  async function pickViewLut() {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [
        { name: 'LUT', extensions: ['cube', '3dl'] },
        { name: 'All files', extensions: ['*'] }
      ]
    });
    if (typeof selected !== 'string') return;
    await projectState.setViewLut(selected);
  }

  // ---- OCIO display transform (Phase 6.1b) ---------------------------------

  let ocioPanelOpen = $state(false);
  let ocioConfigPath = $state(''); // empty => $OCIO env config
  let ocioColorspaces = $state<string[]>([]);
  let ocioSrc = $state('');
  let ocioDst = $state('');
  let ocioSize = $state(33);
  let ocioLoading = $state(false);

  // Seed the panel fields from the project's persisted OCIO transform
  // whenever it opens, so editing starts from the current state.
  function openOcioPanel() {
    // Clear any stale error from a prior op so the panel's error line
    // only reflects this session.
    projectState.error = null;
    const v = projectState.info?.ocio_view;
    if (v) {
      ocioConfigPath = v.config_path ?? '';
      ocioSrc = v.src;
      ocioDst = v.dst;
      ocioSize = v.size;
    }
    ocioPanelOpen = true;
    void loadOcioColorspaces();
  }

  /** Open a file dialog to pick the `.ocio` config. */
  async function pickOcioConfig() {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [
        { name: 'OCIO config', extensions: ['ocio'] },
        { name: 'All files', extensions: ['*'] }
      ]
    });
    if (typeof selected !== 'string') return;
    ocioConfigPath = selected;
    await loadOcioColorspaces();
  }

  /** Populate the src/dst dropdowns from the chosen config (or $OCIO). */
  async function loadOcioColorspaces() {
    ocioLoading = true;
    try {
      ocioColorspaces = await projectState.ocioListColorspaces(ocioConfigPath || null);
      // Keep current picks if still valid; otherwise default sensibly.
      if (ocioColorspaces.length && !ocioColorspaces.includes(ocioSrc)) {
        ocioSrc = ocioColorspaces[0];
      }
      if (ocioColorspaces.length && !ocioColorspaces.includes(ocioDst)) {
        ocioDst = ocioColorspaces[ocioColorspaces.length - 1];
      }
    } finally {
      ocioLoading = false;
    }
  }

  async function applyOcio() {
    if (!ocioSrc || !ocioDst) return;
    // Clamp + reflect back so the engine never rejects on size and the
    // field shows the value actually used (2..=129 matches the backend).
    ocioSize = Math.max(2, Math.min(129, ocioSize || 33));
    await projectState.setOcioViewTransform(ocioConfigPath || null, ocioSrc, ocioDst, ocioSize);
    if (!projectState.error) ocioPanelOpen = false;
  }

  async function clearOcio() {
    await projectState.clearOcioViewTransform();
    ocioPanelOpen = false;
  }

  /** Open a file dialog filtered to .cube / .3dl and write the
   *  chosen path back to a LUT effect's `path` param. */
  async function pickLutFile(track: TrackContents, clip: ClipContents, effect: EffectInfo) {
    const selected = await open({
      multiple: false,
      directory: false,
      filters: [
        { name: 'LUT', extensions: ['cube', '3dl'] },
        { name: 'All files', extensions: ['*'] }
      ]
    });
    if (typeof selected !== 'string') return;
    await projectState.updateEffectParam(track.id, clip.id, effect.id, 'path', selected);
  }

  /** One-character label for a keyframe's interp mode. */
  function interpLabel(i: Keyframe['interp']): string {
    const c = i ?? 'Linear';
    if (c === 'Bezier') return 'B';
    if (c === 'Hold') return 'H';
    return 'L';
  }

  // ---- Color page (Phase 6.6/6.7) ------------------------------------------

  /** Grade nodes (effects of kind 'grade') on the selected clip, in
   *  chain order. Empty when no single clip is selected. */
  const gradeNodes = $derived.by<EffectInfo[]>(() => {
    const d = selectedClipDetails;
    if (!d) return [];
    return d.clip.effects.filter((e) => e.kind === GK.GRADE_KIND);
  });

  /** The node whose wheels are shown — `selectedNodeId` if it still
   *  exists, else the first node. */
  const activeNode = $derived.by<EffectInfo | null>(() => {
    const nodes = gradeNodes;
    if (nodes.length === 0) return null;
    return nodes.find((n) => n.id === selectedNodeId) ?? nodes[0];
  });

  function nodeLabel(node: EffectInfo, index: number): string {
    const raw = node.params[GK.KEY_LABEL];
    if (typeof raw === 'string' && raw.length > 0) return raw;
    return `Node ${index + 1}`;
  }

  function nodeMode(node: EffectInfo): 'primary' | 'log' | 'curves' {
    const m = node.params[GK.KEY_MODE];
    if (m === GK.MODE_LOG) return 'log';
    if (m === GK.MODE_CURVES) return 'curves';
    return 'primary';
  }

  /** Read a numeric grade param's current value (static or evaluated). */
  function gradeVal(node: EffectInfo, key: string): number {
    const raw = node.params[key];
    if (raw === undefined) return GK.gradeDefault(key);
    return evalNumber(raw, 0, GK.gradeDefault(key));
  }

  /** Append a new grade node (primary / log / curves) to the clip. */
  async function addGradeNode(mode: 'primary' | 'log' | 'curves' = 'primary') {
    const d = selectedClipDetails;
    if (!d) return;
    await projectState.addEffect(d.track.id, d.clip.id, GK.GRADE_KIND, {
      [GK.KEY_MODE]: mode
    });
  }

  // Node-list actions — each captures selectedClipDetails once (instead
  // of repeating a `!` non-null assertion at every call site) so a
  // stale async fire can't dereference null.
  async function toggleNodeEnabled(node: EffectInfo) {
    const d = selectedClipDetails;
    if (!d) return;
    await projectState.setNodeEnabled(d.track.id, d.clip.id, node.id, !node.enabled);
  }
  async function moveNode(node: EffectInfo, newIndex: number) {
    const d = selectedClipDetails;
    if (!d) return;
    await projectState.moveGradeNode(d.track.id, d.clip.id, node.id, newIndex);
  }
  async function deleteNode(node: EffectInfo) {
    const d = selectedClipDetails;
    if (!d) return;
    await projectState.removeEffect(d.track.id, d.clip.id, node.id);
  }

  /** Live-preview a wheel group's three channel values + master during
   *  a drag (non-undoable), then nothing is committed until pointer-up
   *  (`commitWheel`). `group` is e.g. 'lift' | 'gamma' | 'gain' (primary)
   *  or 'offset' | 'shadow' | 'midtone' | 'highlight' (log). */
  function wheelKeys(group: string): { rgb: [string, string, string]; master: string | null } {
    const p = (GK.PRIMARY_KEYS as Record<string, readonly string[]>)[group];
    if (p) return { rgb: [p[0], p[1], p[2]], master: p[3] };
    const l = (GK.LOG_KEYS as Record<string, readonly string[]>)[group];
    return { rgb: [l[0], l[1], l[2]], master: null };
  }

  function readWheel(
    node: EffectInfo,
    group: string
  ): { rgb: [number, number, number]; master: number } {
    const k = wheelKeys(group);
    return {
      rgb: [gradeVal(node, k.rgb[0]), gradeVal(node, k.rgb[1]), gradeVal(node, k.rgb[2])],
      master: k.master ? gradeVal(node, k.master) : 0
    };
  }

  /** All (key, value) pairs a wheel change touches — R/G/B plus master
   *  (primary only). Batched so preview + commit each move as a unit. */
  function wheelParams(
    group: string,
    rgb: [number, number, number],
    master: number
  ): [string, EffectValue][] {
    const k = wheelKeys(group);
    const out: [string, EffectValue][] = [
      [k.rgb[0], rgb[0]],
      [k.rgb[1], rgb[1]],
      [k.rgb[2], rgb[2]]
    ];
    if (k.master) out.push([k.master, master]);
    return out;
  }

  /** Push a wheel change live to the engine preview (no undo step). All
   *  channels go in ONE batched call so the preview shows every channel
   *  at once (a per-key sequence would only show the last). */
  function previewWheel(
    node: EffectInfo,
    group: string,
    rgb: [number, number, number],
    master: number
  ) {
    const d = selectedClipDetails;
    if (!d) return;
    projectState.previewGradeParams(d.clip.id, node.id, wheelParams(group, rgb, master));
    refreshScopesNow();
  }

  /** Commit a wheel change as ONE undoable edit (on pointer-up). */
  async function commitWheel(
    node: EffectInfo,
    group: string,
    rgb: [number, number, number],
    master: number
  ) {
    const d = selectedClipDetails;
    if (!d) return;
    await projectState.updateEffectParams(
      d.track.id,
      d.clip.id,
      node.id,
      wheelParams(group, rgb, master)
    );
    refreshScopesNow();
  }

  // ---- Curves (Phase 6.8) --------------------------------------------------

  type CurveChannel = 'master' | 'r' | 'g' | 'b' | 'luma' | 'hh' | 'hs' | 'hl' | 'ss' | 'll';
  type CurvePt = { x: number; y: number };

  /** Which channel's curve is shown in the editor. */
  let curveChannel = $state<CurveChannel>('master');

  // hh/ss/ll are REMAPS (identity y=x, like tone). hs/hl are MULTIPLIERS
  // (identity y=0.5 — the shader decodes 2*y so 0.5 == no-op scale 1).
  // The editor receives identityY so a fresh tab opens at the correct
  // baseline instead of always at the diagonal.
  const CURVE_CHANNELS: {
    key: CurveChannel;
    label: string;
    color: string;
    identityY?: number;
  }[] = [
    { key: 'master', label: 'Master', color: '#e6ebf5' },
    { key: 'r', label: 'Red', color: '#e0524d' },
    { key: 'g', label: 'Green', color: '#5fb86b' },
    { key: 'b', label: 'Blue', color: '#5b8def' },
    { key: 'luma', label: 'Luma', color: '#d8b24a' },
    { key: 'hh', label: 'Hue/Hue', color: '#c98ad9' },
    { key: 'hs', label: 'Hue/Sat', color: '#c98ad9', identityY: 0.5 },
    { key: 'hl', label: 'Hue/Lum', color: '#c98ad9', identityY: 0.5 },
    { key: 'ss', label: 'Sat/Sat', color: '#9bb4c8' },
    { key: 'll', label: 'Lum/Lum', color: '#d8b24a' }
  ];

  // Maps a CurveChannel to its grade.rs param key (tone vs HSL).
  function curveKeyFor(ch: CurveChannel): string {
    if (ch === 'hh' || ch === 'hs' || ch === 'hl' || ch === 'ss' || ch === 'll') {
      const map = { hh: 'hueHue', hs: 'hueSat', hl: 'hueLum', ss: 'satSat', ll: 'lumLum' } as const;
      return GK.HSL_CURVE_KEYS[map[ch]];
    }
    return GK.CURVE_KEYS[ch];
  }

  /** Read a channel's control points as `{x,y}[]` (empty = identity). */
  function readCurve(node: EffectInfo, ch: CurveChannel): CurvePt[] {
    const raw = node.params[curveKeyFor(ch)];
    if (!isKeyframes(raw)) return [];
    return raw.map((k) => ({ x: k.time_seconds, y: k.value }));
  }

  /** Build the EffectValue (Keyframes) for a channel from its points. An
   *  identity curve is sent as an empty list so the engine bakes the
   *  exact identity ramp (and `is_identity` skips sampling). Linear
   *  interp keeps the editor WYSIWYG with the baked LUT. */
  function curveParam(ch: CurveChannel, points: CurvePt[]): [string, EffectValue][] {
    // REMAP channels (tone, hh, ss, ll) identity = y=x; MULTIPLIER
    // channels (hs, hl) identity = y=0.5. Either way an exact-identity
    // 2-point curve is sent as an EMPTY list so the engine bakes the
    // type-specific identity and the non-identity flag stays off.
    const idY = ch === 'hs' || ch === 'hl' ? 0.5 : null; // null => y=x
    const matchesIdentity = (i: number) => {
      if (idY === null) return Math.abs(points[i].y - points[i].x) < 1e-6;
      return Math.abs(points[i].y - idY) < 1e-6;
    };
    const isIdentity =
      points.length === 2 &&
      Math.abs(points[0].x) < 1e-6 &&
      Math.abs(points[1].x - 1) < 1e-6 &&
      matchesIdentity(0) &&
      matchesIdentity(1);
    const value: EffectValue = isIdentity
      ? []
      : points.map((p) => ({ time_seconds: p.x, value: p.y, interp: 'Linear' as const }));
    return [[curveKeyFor(ch), value]];
  }

  function previewCurve(node: EffectInfo, ch: CurveChannel, points: CurvePt[]) {
    const d = selectedClipDetails;
    if (!d) return;
    projectState.previewGradeParams(d.clip.id, node.id, curveParam(ch, points));
    refreshScopesNow();
  }

  async function commitCurve(node: EffectInfo, ch: CurveChannel, points: CurvePt[]) {
    const d = selectedClipDetails;
    if (!d) return;
    await projectState.updateEffectParams(d.track.id, d.clip.id, node.id, curveParam(ch, points));
    refreshScopesNow();
  }

  // ---- HSL qualifier / secondary key (Phase 6.9) ---------------------------

  /** Which node's qualifier matte is soloed to the viewport, or null.
   *  Transient (engine-side), mutually exclusive by construction. */
  let matteNodeId = $state<string | null>(null);
  let matteClipId: string | null = null;

  function readQual(node: EffectInfo, key: string): number {
    const raw = node.params[key];
    if (raw === undefined) return GK.qualifierDefault(key);
    return evalNumber(raw, 0, GK.qualifierDefault(key));
  }
  function qualOn(node: EffectInfo): boolean {
    return readQual(node, GK.QUALIFIER_KEYS.enable) > 0.5;
  }

  // Power window (6.10) — reuses the generic single-param preview/commit
  // plumbing (previewQual/commitQual write any key, keyframe-animatable).
  function readWin(node: EffectInfo, key: string): number {
    const raw = node.params[key];
    if (raw === undefined) return GK.windowDefault(key);
    return evalNumber(raw, 0, GK.windowDefault(key));
  }
  function winShape(node: EffectInfo): number {
    return Math.round(readWin(node, GK.WINDOW_KEYS.shape));
  }
  function winOn(node: EffectInfo): boolean {
    return winShape(node) >= 1;
  }
  const RAD_PER_DEG = Math.PI / 180;
  const WIN_GROUPS: { label: string; keys: string[]; labels: string[] }[] = [
    { label: 'Center', keys: [GK.WINDOW_KEYS.centerX, GK.WINDOW_KEYS.centerY], labels: ['x', 'y'] },
    { label: 'Size', keys: [GK.WINDOW_KEYS.sizeX, GK.WINDOW_KEYS.sizeY], labels: ['w', 'h'] },
    { label: 'Feather', keys: [GK.WINDOW_KEYS.feather], labels: ['soft'] }
  ];
  /** Live-preview a qualifier param during a slider drag (non-undoable). */
  function previewQual(node: EffectInfo, key: string, value: number) {
    const d = selectedClipDetails;
    if (!d) return;
    projectState.previewGradeParams(d.clip.id, node.id, [[key, value]]);
    refreshScopesNow();
  }
  /** Commit a qualifier param as one undoable edit. */
  async function commitQual(node: EffectInfo, key: string, value: number) {
    const d = selectedClipDetails;
    if (!d) return;
    await projectState.updateEffectParams(d.track.id, d.clip.id, node.id, [[key, value]]);
    refreshScopesNow();
  }
  function toggleQualBool(node: EffectInfo, key: string) {
    commitQual(node, key, readQual(node, key) > 0.5 ? 0 : 1);
  }

  /** Solo / unsolo this node's qualifier matte (engine viewer state). */
  function toggleMatte(node: EffectInfo) {
    const d = selectedClipDetails;
    if (!d) return;
    if (matteNodeId === node.id) {
      clearMatte();
    } else {
      matteNodeId = node.id;
      matteClipId = d.clip.id;
      projectState.setMatteNode(d.clip.id, node.id);
    }
  }
  function clearMatte() {
    if (matteNodeId && matteClipId) projectState.setMatteNode(matteClipId, null);
    matteNodeId = null;
    matteClipId = null;
  }

  // ---- Point tracker (Phase 6.11) ----------------------------------------
  let trackFrames = $state(60);
  let trackPatchHalf = $state(15);
  let trackSearchRadius = $state(16);
  let trackBusy = $state(false);
  let trackSummary = $state<{
    frames_tracked: number;
    mean_score: number;
    start_clip_seconds: number;
    end_clip_seconds: number;
  } | null>(null);

  async function runTracker(node: EffectInfo) {
    const d = selectedClipDetails;
    if (!d) return;
    trackBusy = true;
    trackSummary = null;
    try {
      const r = await projectState.trackWindow(
        d.track.id,
        d.clip.id,
        node.id,
        d.clip.source_in_seconds,
        Math.min(240, Math.max(2, Math.round(trackFrames))),
        Math.max(2, Math.round(trackPatchHalf)),
        Math.max(1, Math.round(trackSearchRadius))
      );
      if (r) trackSummary = r;
    } finally {
      trackBusy = false;
    }
  }

  // If the soloed node is deleted (or otherwise vanishes from the chain),
  // drop the stale matte so the toggle can't stay "active" on a node that
  // no longer exists.
  $effect(() => {
    const nodes = gradeNodes;
    untrack(() => {
      if (matteNodeId && !nodes.some((n) => n.id === matteNodeId)) clearMatte();
    });
  });

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
    const fps = info.frame_rate_den !== 0 ? info.frame_rate_num / info.frame_rate_den : 24;
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
        if (info && info.duration_pts != null && from >= info.duration_pts - step) {
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
    <span class="text-vance-muted text-xs">Phase 6 · Color</span>

    <!-- Edit / Color page toggle (Phase 6.6) -->
    <div class="ml-1 flex items-center rounded border border-vance-border overflow-hidden text-xs">
      <button
        class="px-2 py-0.5 {activePage === 'edit'
          ? 'bg-[var(--color-vance-accent)] text-[#003040] font-semibold'
          : 'bg-vance-bg hover:brightness-125'}"
        onclick={() => (activePage = 'edit')}
      >
        Edit
      </button>
      <button
        class="px-2 py-0.5 {activePage === 'color'
          ? 'bg-[var(--color-vance-accent)] text-[#003040] font-semibold'
          : 'bg-vance-bg hover:brightness-125'}"
        onclick={() => (activePage = 'color')}
      >
        Color
      </button>
    </div>

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
        <span class="text-vance-muted ml-2">·</span>
        <span class="text-vance-muted">View LUT:</span>
        {#if projectState.info.view_lut_path}
          <button
            class="bg-vance-bg border-vance-border rounded border px-1 py-0.5 text-[10px] truncate max-w-[12rem]"
            onclick={pickViewLut}
            title={projectState.info.view_lut_path}
          >
            {projectState.info.view_lut_path.split('/').pop() ?? '(unnamed)'}
          </button>
          <button
            class="text-vance-muted hover:text-red-400 text-[10px]"
            onclick={() => projectState.setViewLut(null)}
            title="Clear the display LUT (unfiltered preview)"
          >
            ✕
          </button>
        {:else}
          <button
            class="bg-vance-bg border-vance-border rounded border px-1 py-0.5 text-[10px]"
            onclick={pickViewLut}
            title="Pick a .cube / .3dl monitoring LUT"
          >
            (none)
          </button>
        {/if}

        <!-- OCIO display transform (Phase 6.1b) -->
        <span class="text-vance-muted ml-2">·</span>
        <span class="text-vance-muted">OCIO:</span>
        <div class="relative inline-block">
          <button
            class="bg-vance-bg border-vance-border rounded border px-1 py-0.5 text-[10px] disabled:opacity-40 max-w-[12rem] truncate"
            onclick={openOcioPanel}
            disabled={!projectState.info.ocio_available}
            title={projectState.info.ocio_available
              ? 'Configure an OpenColorIO display transform'
              : 'OCIO not enabled in this build (rebuild with --features ocio)'}
          >
            {#if projectState.info.ocio_view}
              {projectState.info.ocio_view.src} → {projectState.info.ocio_view.dst}
            {:else}
              (off)
            {/if}
          </button>
          {#if ocioPanelOpen}
            <!-- Click-outside backdrop (mirrors the title modal pattern). -->
            <button
              type="button"
              class="fixed inset-0 z-40 cursor-default"
              aria-label="Dismiss OCIO panel"
              onclick={() => (ocioPanelOpen = false)}
            ></button>
            <div
              class="border-vance-border bg-vance-panel absolute left-0 top-full z-50 mt-1 flex w-72 flex-col gap-2 rounded border p-3 text-[11px] shadow-lg"
            >
              <div class="flex items-center justify-between">
                <span class="text-vance-fg font-medium">OCIO Display Transform</span>
                <button
                  class="text-vance-muted hover:brightness-150"
                  onclick={() => (ocioPanelOpen = false)}>✕</button
                >
              </div>
              <label class="text-vance-muted flex flex-col gap-1">
                Config (.ocio, blank = $OCIO env)
                <div class="flex gap-1">
                  <input
                    class="bg-vance-bg border-vance-border flex-1 truncate rounded border px-1 py-0.5"
                    value={ocioConfigPath}
                    oninput={(e) => (ocioConfigPath = (e.currentTarget as HTMLInputElement).value)}
                    placeholder="$OCIO"
                  />
                  <button
                    class="bg-vance-bg border-vance-border rounded border px-1.5"
                    onclick={pickOcioConfig}>…</button
                  >
                </div>
              </label>
              <button
                class="bg-vance-bg border-vance-border self-start rounded border px-1.5 py-0.5"
                onclick={loadOcioColorspaces}
                disabled={ocioLoading}
              >
                {ocioLoading ? 'Loading…' : 'Load colorspaces'}
              </button>
              <label class="text-vance-muted flex flex-col gap-1">
                Source
                <select
                  class="bg-vance-bg border-vance-border rounded border px-1 py-0.5"
                  value={ocioSrc}
                  onchange={(e) => (ocioSrc = (e.currentTarget as HTMLSelectElement).value)}
                >
                  {#each ocioColorspaces as cs (cs)}
                    <option value={cs}>{cs}</option>
                  {/each}
                </select>
              </label>
              <label class="text-vance-muted flex flex-col gap-1">
                Display
                <select
                  class="bg-vance-bg border-vance-border rounded border px-1 py-0.5"
                  value={ocioDst}
                  onchange={(e) => (ocioDst = (e.currentTarget as HTMLSelectElement).value)}
                >
                  {#each ocioColorspaces as cs (cs)}
                    <option value={cs}>{cs}</option>
                  {/each}
                </select>
              </label>
              <label class="text-vance-muted flex items-center gap-2">
                LUT size
                <input
                  type="number"
                  min="2"
                  max="129"
                  class="bg-vance-bg border-vance-border w-16 rounded border px-1 py-0.5"
                  value={ocioSize}
                  oninput={(e) =>
                    (ocioSize = parseInt((e.currentTarget as HTMLInputElement).value, 10) || 33)}
                />
              </label>
              <div class="flex gap-2">
                <button
                  class="rounded border border-[var(--color-vance-accent)] bg-[#003040] px-2 py-0.5 disabled:opacity-40"
                  onclick={applyOcio}
                  disabled={!ocioSrc || !ocioDst}
                >
                  Apply
                </button>
                <button
                  class="bg-vance-bg border-vance-border rounded border px-2 py-0.5"
                  onclick={clearOcio}
                >
                  Clear
                </button>
              </div>
              <!-- Surface OCIO errors (bad config, unknown colorspace,
                   bake failure) inside the panel where the action was
                   taken, not just the far-away global error line. -->
              {#if projectState.error}
                <p class="rounded border border-red-700 bg-red-950 p-1.5 text-[10px] text-red-200">
                  {projectState.error}
                </p>
              {/if}
            </div>
          {/if}
        </div>
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
    <!-- Left panel: Browser (Edit) or Node list (Color) -->
    {#if activePage === 'color'}
      <aside
        class="border-vance-border bg-vance-panel w-60 flex-shrink-0 overflow-y-auto border-r p-4"
      >
        <div class="mb-3 flex items-center justify-between">
          <h2 class="text-vance-muted text-xs uppercase tracking-wider">Nodes</h2>
          <div class="flex gap-1">
            <button
              class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 text-[10px] hover:brightness-125 disabled:opacity-50"
              onclick={() => addGradeNode('primary')}
              disabled={!selectedClipDetails}
              title="Add a primary (lift/gamma/gain) grade node"
            >
              + Primary
            </button>
            <button
              class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 text-[10px] hover:brightness-125 disabled:opacity-50"
              onclick={() => addGradeNode('log')}
              disabled={!selectedClipDetails}
              title="Add a log (offset/shadow/mid/highlight) grade node"
            >
              + Log
            </button>
            <button
              class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 text-[10px] hover:brightness-125 disabled:opacity-50"
              onclick={() => addGradeNode('curves')}
              disabled={!selectedClipDetails}
              title="Add a curves (per-channel tone curve) grade node"
            >
              + Curves
            </button>
          </div>
        </div>
        {#if !selectedClipDetails}
          <p class="text-vance-muted text-xs italic">Select a clip on the timeline to grade it.</p>
        {:else if gradeNodes.length === 0}
          <p class="text-vance-muted text-xs italic">
            No grade nodes. Add one above — nodes apply top-to-bottom.
          </p>
        {:else}
          <ul class="flex flex-col gap-1 text-xs">
            {#each gradeNodes as node, i (node.id)}
              {@const isSel = activeNode?.id === node.id}
              <li
                class="flex items-center gap-1 rounded border px-2 py-1
                  {isSel
                  ? 'border-[var(--color-vance-accent)] bg-[#003040]'
                  : 'bg-vance-bg border-vance-border'}"
              >
                <input
                  type="checkbox"
                  checked={node.enabled}
                  onchange={() => toggleNodeEnabled(node)}
                  title="Bypass / enable this node"
                />
                <button
                  class="flex-1 truncate text-left {node.enabled ? '' : 'opacity-40 line-through'}"
                  onclick={() => (selectedNodeId = node.id)}
                >
                  {nodeLabel(node, i)} · {nodeMode(node)}
                </button>
                <button
                  class="text-vance-muted hover:brightness-125 disabled:opacity-30"
                  onclick={() => moveNode(node, i - 1)}
                  disabled={i === 0}
                  title="Move up (earlier in chain)"
                >
                  ↑
                </button>
                <button
                  class="text-vance-muted hover:brightness-125 disabled:opacity-30"
                  onclick={() => moveNode(node, i + 1)}
                  disabled={i === gradeNodes.length - 1}
                  title="Move down (later in chain)"
                >
                  ↓
                </button>
                <button
                  class="text-vance-muted hover:text-red-400"
                  onclick={() => deleteNode(node)}
                  title="Delete node"
                >
                  ✕
                </button>
              </li>
            {/each}
          </ul>
          {#if gradeNodes.length >= GK.MAX_GRADE_NODES}
            <p class="mt-2 text-[10px] text-yellow-500">
              Max {GK.MAX_GRADE_NODES} nodes render; extras are ignored.
            </p>
          {/if}
        {/if}
        {#if projectState.error}
          <p class="mt-3 rounded border border-red-700 bg-red-950 p-2 text-xs text-red-200">
            {projectState.error}
          </p>
        {/if}
      </aside>
    {:else}
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
          <p class="text-vance-muted text-xs">Create or open a project from the toolbar.</p>
        {/if}
        {#if projectState.error}
          <p class="mt-3 rounded border border-red-700 bg-red-950 p-2 text-xs text-red-200">
            {projectState.error}
          </p>
        {/if}
      </aside>
    {/if}

    <!-- Center: Source + Record viewports + transport -->
    <main class="flex flex-1 flex-col min-w-0">
      <div class="flex flex-1 min-w-0 min-h-0">
        <!-- Source monitor (left) — previews a single MediaItem. -->
        <div class="flex flex-1 flex-col min-w-0 m-4 mr-2">
          <div
            class="mb-1 flex items-center gap-2 text-[10px] uppercase tracking-wider text-vance-muted"
          >
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
          <div class="mb-1 text-[10px] uppercase tracking-wider text-vance-muted">Record</div>
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
        <!--
          Phase 7.4 — master fader. Sequence-level gain in dB,
          applied at the mixer fold thread after track/clip
          composition. Live during playback. UI inputs to the
          slider fire `setMasterGainDb` on every change (cheap IPC,
          ArcSwap snapshot republish is the audible path).
        -->
        <label
          class="text-vance-muted flex items-center gap-2"
          title="Master bus gain (dB). 0 = unity, -inf = silence."
        >
          Master
          <input
            type="range"
            min="-60"
            max="12"
            step="0.5"
            value={projectState.sequence?.master_gain_db ?? 0}
            oninput={(e) =>
              projectState.setMasterGainDb(parseFloat((e.target as HTMLInputElement).value))}
            disabled={!projectState.sequence}
            class="w-32 accent-[var(--color-vance-accent)]"
          />
          <span class="font-mono text-vance-fg w-12 text-right">
            {(projectState.sequence?.master_gain_db ?? 0).toFixed(1)} dB
          </span>
        </label>
        <!-- Phase 7.5 M1 — master meter. Polls the same metersStore as
             the track meters so the values share their hold + decay
             tuning. -->
        <MasterMeter />

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
                  sizePx: titleDraftSize
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

    <!-- Right panel: Wheels (Color) or Inspector (Edit) -->
    {#if activePage === 'color'}
      <aside
        class="border-vance-border bg-vance-panel w-80 flex-shrink-0 overflow-y-auto overflow-x-hidden border-l p-4"
      >
        <!-- Scopes (Phase 6.12) -->
        <div class="mb-4">
          <div class="mb-2 flex items-center justify-between">
            <h2 class="text-vance-muted text-xs uppercase tracking-wider">Scopes</h2>
            <div class="flex gap-1 text-[10px]">
              <button
                class="rounded border px-1 py-0.5 {scopeLog
                  ? 'border-[var(--color-vance-accent)] bg-[#003040]'
                  : 'bg-vance-bg border-vance-border'}"
                onclick={() => (scopeLog = !scopeLog)}
                title="Toggle log / linear intensity scaling"
              >
                {scopeLog ? 'log' : 'lin'}
              </button>
              <button
                class="rounded border px-1 py-0.5 {scopePreLut
                  ? 'border-[var(--color-vance-accent)] bg-[#003040]'
                  : 'bg-vance-bg border-vance-border'}"
                onclick={() => {
                  scopePreLut = !scopePreLut;
                  refreshScopesNow();
                }}
                title="Analyze the signal before the display/view LUT"
              >
                {scopePreLut ? 'pre-LUT' : 'display'}
              </button>
            </div>
          </div>
          <div class="mb-2 flex gap-1 text-[10px]">
            {#each [['waveform', 'Wave'], ['parade', 'RGB'], ['vector', 'Vector'], ['histogram', 'Hist']] as [k, label] (k)}
              <button
                class="flex-1 rounded border px-1 py-0.5 {scopeKind === k
                  ? 'border-[var(--color-vance-accent)] bg-[#003040]'
                  : 'bg-vance-bg border-vance-border hover:brightness-125'}"
                onclick={() => (scopeKind = k as typeof scopeKind)}
              >
                {label}
              </button>
            {/each}
          </div>
          <ScopeCanvas
            kind={scopeKind}
            data={projectState.scopeData}
            log={scopeLog}
            width={288}
            height={180}
          />
        </div>

        <h2 class="text-vance-muted mb-3 text-xs uppercase tracking-wider">
          {activeNode ? nodeLabel(activeNode, gradeNodes.indexOf(activeNode)) : 'Wheels'}
        </h2>
        {#if !activeNode}
          <p class="text-vance-muted text-xs italic">
            {selectedClipDetails ? 'Add a grade node to start.' : 'Select a clip to grade.'}
          </p>
        {:else}
          {@const node = activeNode}
          {@const mode = nodeMode(node)}

          <!-- HSL qualifier / secondary key (Phase 6.9). Present for ALL
               modes — set the key first, then grade inside it. -->
          {@const qOn = qualOn(node)}
          <div class="mb-4 rounded border border-vance-border p-2">
            <div class="mb-2 flex items-center justify-between">
              <label class="flex items-center gap-1.5 text-[11px]">
                <input
                  type="checkbox"
                  checked={qOn}
                  onchange={() => toggleQualBool(node, GK.QUALIFIER_KEYS.enable)}
                />
                <span class="text-vance-muted uppercase tracking-wider">Qualifier</span>
              </label>
              <div class="flex gap-1 text-[10px]">
                <button
                  class="rounded border px-1 py-0.5 {readQual(node, GK.QUALIFIER_KEYS.invert) > 0.5
                    ? 'border-[var(--color-vance-accent)] bg-[#003040]'
                    : 'bg-vance-bg border-vance-border'} disabled:opacity-40"
                  disabled={!qOn}
                  onclick={() => toggleQualBool(node, GK.QUALIFIER_KEYS.invert)}
                  title="Invert the mask"
                >
                  invert
                </button>
                <button
                  class="rounded border px-1 py-0.5 {matteNodeId === node.id
                    ? 'border-yellow-400 bg-[#3a3320] text-yellow-200'
                    : 'bg-vance-bg border-vance-border'} disabled:opacity-40"
                  disabled={!qOn && !winOn(node)}
                  onclick={() => toggleMatte(node)}
                  title="Solo the combined matte (qualifier ∩ window) to the viewport"
                >
                  ◧ mask
                </button>
              </div>
            </div>
            {#if qOn}
              <div class="flex flex-col gap-2 text-[10px]" class:opacity-40={!qOn}>
                {#each [['Hue', [GK.QUALIFIER_KEYS.hueC, GK.QUALIFIER_KEYS.hueRange, GK.QUALIFIER_KEYS.hueSoft]], ['Sat', [GK.QUALIFIER_KEYS.satLo, GK.QUALIFIER_KEYS.satHi, GK.QUALIFIER_KEYS.satSoft]], ['Luma', [GK.QUALIFIER_KEYS.lumaLo, GK.QUALIFIER_KEYS.lumaHi, GK.QUALIFIER_KEYS.lumaSoft]]] as [label, keys] (label)}
                  {@const labels =
                    label === 'Hue' ? ['center', 'range', 'soft'] : ['lo', 'hi', 'soft']}
                  <div>
                    <div class="text-vance-muted mb-0.5">{label}</div>
                    <div class="grid grid-cols-3 gap-1">
                      {#each keys as k, i (k)}
                        <label class="flex flex-col">
                          <span class="text-vance-muted text-[9px]">{labels[i]}</span>
                          <input
                            type="range"
                            min="0"
                            max={labels[i] === 'soft' ? 0.5 : 1}
                            step="0.001"
                            value={readQual(node, k)}
                            oninput={(e) =>
                              previewQual(
                                node,
                                k,
                                parseFloat((e.currentTarget as HTMLInputElement).value)
                              )}
                            onchange={(e) =>
                              commitQual(
                                node,
                                k,
                                parseFloat((e.currentTarget as HTMLInputElement).value)
                              )}
                          />
                        </label>
                      {/each}
                    </div>
                  </div>
                {/each}
              </div>
            {/if}
          </div>

          <!-- Power window / shape mask (Phase 6.10). Numeric controls;
               on-viewport drag handles arrive in 6.10b. Combined with the
               qualifier via the shared matte toggle above. -->
          {@const wShape = winShape(node)}
          <div class="mb-4 rounded border border-vance-border p-2">
            <div class="mb-2 flex items-center justify-between">
              <span class="text-vance-muted text-[11px] uppercase tracking-wider">Power Window</span
              >
              <select
                class="bg-vance-bg border-vance-border rounded border px-1 py-0.5 text-[10px]"
                value={wShape}
                onchange={(e) =>
                  commitQual(
                    node,
                    GK.WINDOW_KEYS.shape,
                    parseInt((e.currentTarget as HTMLSelectElement).value, 10)
                  )}
              >
                <option value={0}>Off</option>
                <option value={1}>Ellipse</option>
                <option value={2}>Rectangle</option>
              </select>
            </div>
            {#if wShape >= 1}
              <div class="flex flex-col gap-2 text-[10px]">
                {#each WIN_GROUPS as g (g.label)}
                  <div>
                    <div class="text-vance-muted mb-0.5">{g.label}</div>
                    <div class="grid grid-cols-2 gap-1">
                      {#each g.keys as k, i (k)}
                        <label class="flex flex-col">
                          <span class="text-vance-muted text-[9px]">{g.labels[i]}</span>
                          <input
                            type="range"
                            min="0"
                            max="1"
                            step="0.001"
                            value={readWin(node, k)}
                            oninput={(e) =>
                              previewQual(
                                node,
                                k,
                                parseFloat((e.currentTarget as HTMLInputElement).value)
                              )}
                            onchange={(e) =>
                              commitQual(
                                node,
                                k,
                                parseFloat((e.currentTarget as HTMLInputElement).value)
                              )}
                          />
                        </label>
                      {/each}
                    </div>
                  </div>
                {/each}
                <div class="flex items-center justify-between">
                  <label class="flex flex-1 flex-col">
                    <span class="text-vance-muted text-[9px]">rotation</span>
                    <input
                      type="range"
                      min="-180"
                      max="180"
                      step="1"
                      value={readWin(node, GK.WINDOW_KEYS.rotation) / RAD_PER_DEG}
                      oninput={(e) =>
                        previewQual(
                          node,
                          GK.WINDOW_KEYS.rotation,
                          parseFloat((e.currentTarget as HTMLInputElement).value) * RAD_PER_DEG
                        )}
                      onchange={(e) =>
                        commitQual(
                          node,
                          GK.WINDOW_KEYS.rotation,
                          parseFloat((e.currentTarget as HTMLInputElement).value) * RAD_PER_DEG
                        )}
                    />
                  </label>
                  <button
                    class="ml-2 self-end rounded border px-1 py-0.5 {readWin(
                      node,
                      GK.WINDOW_KEYS.invert
                    ) > 0.5
                      ? 'border-[var(--color-vance-accent)] bg-[#003040]'
                      : 'bg-vance-bg border-vance-border'}"
                    onclick={() =>
                      commitQual(
                        node,
                        GK.WINDOW_KEYS.invert,
                        readWin(node, GK.WINDOW_KEYS.invert) > 0.5 ? 0 : 1
                      )}
                  >
                    invert
                  </button>
                </div>
                <p class="text-vance-muted text-[9px] italic">
                  Drag handles on the viewport arrive in 6.10b.
                </p>

                <!-- Point tracker (Phase 6.11). Tracks the window center
                     forward from the clip start by ZNCC on the Y plane;
                     writes win_cx/win_cy keyframes in ONE undoable edit.
                     Synchronous v1 — the UI is blocked while tracking. -->
                <div class="mt-2 border-t border-vance-border pt-2">
                  <div class="text-vance-muted mb-1 text-[10px] uppercase tracking-wider">
                    Tracker
                  </div>
                  <div class="grid grid-cols-3 gap-1 text-[10px]">
                    <label class="flex flex-col">
                      <span class="text-vance-muted text-[9px]">frames</span>
                      <input
                        type="number"
                        min="2"
                        max="240"
                        step="1"
                        class="bg-vance-bg border-vance-border rounded border px-1 py-0.5"
                        bind:value={trackFrames}
                      />
                    </label>
                    <label class="flex flex-col">
                      <span class="text-vance-muted text-[9px]">patch ½</span>
                      <input
                        type="number"
                        min="2"
                        max="64"
                        step="1"
                        class="bg-vance-bg border-vance-border rounded border px-1 py-0.5"
                        bind:value={trackPatchHalf}
                      />
                    </label>
                    <label class="flex flex-col">
                      <span class="text-vance-muted text-[9px]">search ±</span>
                      <input
                        type="number"
                        min="1"
                        max="128"
                        step="1"
                        class="bg-vance-bg border-vance-border rounded border px-1 py-0.5"
                        bind:value={trackSearchRadius}
                      />
                    </label>
                  </div>
                  <button
                    class="mt-1 w-full rounded border border-[var(--color-vance-accent)] bg-[#003040] px-2 py-0.5 text-[11px] disabled:opacity-40"
                    onclick={() => runTracker(node)}
                    disabled={trackBusy}
                  >
                    {trackBusy ? 'Tracking…' : 'Track from clip start →'}
                  </button>
                  {#if trackSummary}
                    <p class="text-vance-muted mt-1 text-[9px]">
                      Tracked {trackSummary.frames_tracked} frames ({trackSummary.start_clip_seconds.toFixed(
                        2
                      )}s →
                      {trackSummary.end_clip_seconds.toFixed(2)}s) · mean score {trackSummary.mean_score.toFixed(
                        2
                      )}
                    </p>
                  {/if}
                  <p class="text-vance-muted text-[9px] italic">
                    Replaces win_cx/win_cy keyframes from the clip start through the tracked range.
                    Decoder must be open on this clip (play/seek it once first).
                  </p>
                </div>
              </div>
            {/if}
          </div>

          {#if mode === 'primary'}
            {#each ['lift', 'gamma', 'gain'] as group (group)}
              {@const w = readWheel(node, group)}
              <div class="mb-4">
                <ColorWheel
                  label={group}
                  rgb={w.rgb}
                  master={w.master}
                  masterMin={group === 'lift' ? -0.5 : 0}
                  masterMax={group === 'lift' ? 0.5 : 2}
                  onInput={(rgb, m) => previewWheel(node, group, rgb, m)}
                  onCommit={(rgb, m) => commitWheel(node, group, rgb, m)}
                />
              </div>
            {/each}
          {:else if mode === 'log'}
            {#each ['offset', 'shadow', 'midtone', 'highlight'] as group (group)}
              {@const w = readWheel(node, group)}
              <div class="mb-4">
                <ColorWheel
                  label={group}
                  rgb={w.rgb}
                  master={w.master}
                  showMaster={false}
                  onInput={(rgb, m) => previewWheel(node, group, rgb, m)}
                  onCommit={(rgb, m) => commitWheel(node, group, rgb, m)}
                />
              </div>
            {/each}
          {:else}
            <!-- Curves mode -->
            {@const chMeta =
              CURVE_CHANNELS.find((c) => c.key === curveChannel) ?? CURVE_CHANNELS[0]}
            <div class="mb-2 flex flex-wrap gap-1">
              {#each CURVE_CHANNELS as ch (ch.key)}
                <button
                  class="rounded border px-1.5 py-0.5 text-[10px] {curveChannel === ch.key
                    ? 'border-[var(--color-vance-accent)] bg-[#003040]'
                    : 'bg-vance-bg border-vance-border hover:brightness-125'}"
                  style={`color:${ch.color}`}
                  onclick={() => (curveChannel = ch.key)}
                >
                  {ch.label}
                </button>
              {/each}
            </div>
            <!-- (HSL hue-wheel tabs are now full members of CURVE_CHANNELS above — 6.8b shipped.) -->
            <CurveEditor
              label={chMeta.label}
              color={chMeta.color}
              identityY={chMeta.identityY ?? null}
              points={readCurve(node, curveChannel)}
              onInput={(pts) => previewCurve(node, curveChannel, pts)}
              onCommit={(pts) => commitCurve(node, curveChannel, pts)}
            />
          {/if}
        {/if}
        {#if projectState.error}
          <p class="mt-3 rounded border border-red-700 bg-red-950 p-2 text-xs text-red-200">
            {projectState.error}
          </p>
        {/if}
      </aside>
    {:else}
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

          <!-- Phase 7.6 — pan controls. Sliders for the parent track's
               pan AND the clip's pan, since they compose additively at
               mix time. Audio tracks only (video tracks accept the
               edit but the mixer ignores it; we still show the
               controls so the user sees the value land). -->
          {#if t.kind === 'audio'}
            <div class="mt-3">
              <div class="mb-1 text-[11px] uppercase tracking-wider text-vance-muted">Pan</div>
              <label class="flex items-center gap-2 font-mono text-xs">
                <span class="w-12 text-vance-muted">track</span>
                <input
                  type="range"
                  min="-1"
                  max="1"
                  step="0.01"
                  value={t.pan}
                  oninput={(e) =>
                    projectState.setTrackPan(
                      t.id,
                      parseFloat((e.target as HTMLInputElement).value)
                    )}
                  class="flex-1 accent-[var(--color-vance-accent)]"
                />
                <span class="w-10 text-right">{t.pan.toFixed(2)}</span>
              </label>
              <label class="mt-1 flex items-center gap-2 font-mono text-xs">
                <span class="w-12 text-vance-muted">clip</span>
                <input
                  type="range"
                  min="-1"
                  max="1"
                  step="0.01"
                  value={c.pan}
                  oninput={(e) =>
                    projectState.setClipPan(
                      t.id,
                      c.id,
                      parseFloat((e.target as HTMLInputElement).value)
                    )}
                  class="flex-1 accent-[var(--color-vance-accent)]"
                />
                <span class="w-10 text-right">{c.pan.toFixed(2)}</span>
              </label>
              <div class="mt-0.5 text-[10px] text-vance-muted">
                composed: {Math.max(-1, Math.min(1, t.pan + c.pan)).toFixed(2)} (clamped)
              </div>
            </div>

            <!-- Phase 7.7 — per-track built-in DSP insert chain
                 (parametric EQ + compressor + limiter). Shown for
                 audio tracks since the engine ignores the chain on
                 video tracks. -->
            <AudioEffectChain trackId={t.id} effects={t.audio_effects} />
          {/if}

          <!-- Effects: list each attached effect with its kind + a
             remove button. 5.2+ adds parameter editing (opacity
             slider, transform widgets, etc.). -->
          <div class="mt-4">
            <div
              class="mb-1 flex items-center justify-between text-[11px] uppercase tracking-wider"
            >
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
                <button
                  class="rounded bg-vance-bg border-vance-border border px-1.5 py-0.5 text-[10px] hover:brightness-125"
                  onclick={() => projectState.addEffect(t.id, c.id, 'lut', { path: '' })}
                  title="Attach a 3D LUT effect (.cube / .3dl)"
                >
                  + LUT
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
                      {@const cur = raw !== undefined ? evalNumber(raw, 0, defaultVal) : defaultVal}
                      <div
                        class="mt-1 grid grid-cols-[3rem_minmax(0,1fr)_2.25rem_1rem] items-center gap-x-2"
                      >
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
                        <span class="font-mono text-right text-[10px]"
                          >{cur.toFixed(precision)}</span
                        >
                        <button
                          class="text-[12px] leading-none {animated
                            ? 'text-[var(--color-vance-accent)]'
                            : 'text-vance-muted'} hover:brightness-125"
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
                              <span class="font-mono flex-1 text-right"
                                >{k.value.toFixed(precision)}</span
                              >
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
                      {@const cur = raw !== undefined ? evalNumber(raw, 0, 1) : 1}
                      <div
                        class="mt-1 grid grid-cols-[3rem_minmax(0,1fr)_2.25rem_1rem] items-center gap-x-2"
                      >
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
                          class="text-[12px] leading-none {animated
                            ? 'text-[var(--color-vance-accent)]'
                            : 'text-vance-muted'} hover:brightness-125"
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
                      {@const tx =
                        typeof e.params.translate_x === 'number' ? e.params.translate_x : 0}
                      {@const ty =
                        typeof e.params.translate_y === 'number' ? e.params.translate_y : 0}
                      {@const sx = typeof e.params.scale_x === 'number' ? e.params.scale_x : 1}
                      {@const sy = typeof e.params.scale_y === 'number' ? e.params.scale_y : 1}
                      {@const rot = typeof e.params.rotation === 'number' ? e.params.rotation : 0}
                      <div
                        class="mt-1 grid grid-cols-[3rem_minmax(0,1fr)_2.25rem] items-center gap-x-2 gap-y-1"
                      >
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
                    {:else if e.kind === 'lut'}
                      {@const rawPath = e.params.path}
                      {@const lutPath: string = typeof rawPath === 'string' ? rawPath : ''}
                      <div class="mt-1 flex items-center gap-2 text-[10px]">
                        <button
                          class="rounded bg-vance-bg border-vance-border border px-2 py-0.5 hover:brightness-125"
                          onclick={() => pickLutFile(t, c, e)}
                          title="Pick a .cube or .3dl LUT file"
                        >
                          Pick file…
                        </button>
                        <span class="text-vance-muted truncate font-mono" title={lutPath}>
                          {lutPath || '(none selected)'}
                        </span>
                      </div>
                    {/if}
                  </li>
                {/each}
              </ul>
            {/if}
          </div>
        {:else if selectionState.count > 1}
          <p class="text-vance-muted text-xs">
            {selectionState.count} clips selected. Single-selection details show here.
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
    {/if}
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
