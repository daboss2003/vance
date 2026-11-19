// Reactive project state for the editor chrome.
//
// Single shared store using Svelte 5 runes. Components read `state.project`
// and call the helpers below; everything is kept in sync with the engine
// via the IPC commands in `$lib/ipc`.

import {
  canRedo as ipcCanRedo,
  canUndo as ipcCanUndo,
  currentProjectPath,
  currentSequenceContents,
  decodeSourceAt,
  importMediaFile,
  createTitleMedia,
  setMediaInputColorSpace,
  setProjectWorkingColorSpace,
  setProjectViewLut,
  setOcioViewTransform,
  clearOcioViewTransform,
  ocioListColorspaces,
  computeScopes,
  setScopesEnabled as ipcSetScopesEnabled,
  setMatteNode as ipcSetMatteNode,
  trackWindow as ipcTrackWindow,
  type TrackSummary,
  insertClipAt,
  loadProject,
  addEffectToClip as ipcAddEffect,
  addTransitionAtCut as ipcAddTransition,
  removeTransition as ipcRemoveTransition,
  clearSourceMarks as ipcClearSourceMarks,
  insertFromSource as ipcInsertFromSource,
  removeEffectFromClip as ipcRemoveEffect,
  updateEffectParam as ipcUpdateEffectParam,
  moveEffectOnClip as ipcMoveEffect,
  setEffectEnabled as ipcSetEffectEnabled,
  setTrackMute as ipcSetTrackMute,
  setTrackSolo as ipcSetTrackSolo,
  setTrackGainDb as ipcSetTrackGainDb,
  setClipMute as ipcSetClipMute,
  setClipGainDb as ipcSetClipGainDb,
  setMasterGainDb as ipcSetMasterGainDb,
  setTrackGainKeyframes as ipcSetTrackGainKeyframes,
  setClipGainKeyframes as ipcSetClipGainKeyframes,
  setClipFadeIn as ipcSetClipFadeIn,
  setClipFadeOut as ipcSetClipFadeOut,
  setTrackPan as ipcSetTrackPan,
  setClipPan as ipcSetClipPan,
  addAudioEffect as ipcAddAudioEffect,
  removeAudioEffect as ipcRemoveAudioEffect,
  moveAudioEffect as ipcMoveAudioEffect,
  updateAudioEffect as ipcUpdateAudioEffect,
  type AudioEffectSpec,
  type GainKeyframe,
  type FadeCurveName,
  updateEffectParams as ipcUpdateEffectParams,
  previewGradeParams as ipcPreviewGradeParams,
  loadSource as ipcLoadSource,
  mediaList,
  moveClipTo,
  isMagnetic,
  setMagnetic as ipcSetMagnetic,
  connectClip as ipcConnectClip,
  disconnectClip as ipcDisconnectClip,
  magneticMoveClip,
  appendClip as ipcAppendClip,
  magneticRippleDelete,
  removeClip as ipcRemoveClip,
  newProject as ipcNew,
  projectInfo,
  redo as ipcRedo,
  saveProject as ipcSave,
  setCurrentSequence as ipcSetSeq,
  setSourceMarkIn as ipcSetSourceMarkIn,
  setSourceMarkOut as ipcSetSourceMarkOut,
  sourceState as ipcSourceState,
  trimClipIn,
  trimClipOut,
  undo as ipcUndo,
  type ColorSpaceLabel,
  type EffectValue,
  type MediaItemInfo,
  type ProjectInfo,
  type ScopeData,
  type SequenceContents,
  type SourceState,
  type StreamInfo,
  peaksEnqueue,
  peaksSetCacheDir,
  peaksServiceStatus,
  peaksUseUntitledCacheDir,
  peaksBackfillVideoAudio
} from './ipc';
import { peaksCache } from './peaks-cache.svelte';

class ProjectState {
  /** Current project as the engine sees it; `null` until something is loaded. */
  info = $state<ProjectInfo | null>(null);
  /** Filesystem path of the loaded bundle, or `null` if the project is unsaved. */
  path = $state<string | null>(null);
  /** Last error string from a project IPC call; `null` when clean. */
  error = $state<string | null>(null);
  /** Latest computed scope data (Phase 6.12); `null` until first refresh. */
  scopeData = $state<ScopeData | null>(null);
  /** Media items in the project's library, for the Browser panel. */
  media = $state<MediaItemInfo[]>([]);
  /** Counter bumped after every edit (clip insert, etc.) so the
   *  Timeline canvas can refresh its sequence contents. */
  editTick = $state<number>(0);
  /** Live state of the current sequence's edit log, refreshed
   *  after every edit op and after undo / redo. Drives toolbar
   *  button enable-disable. */
  canUndo = $state<boolean>(false);
  canRedo = $state<boolean>(false);
  /** Full contents of the engine's current sequence — clips by
   *  track + frame rate + duration. Re-fetched after every edit
   *  via `editTick`. Single source of truth for the Timeline and
   *  Inspector. */
  sequence = $state<SequenceContents | null>(null);
  /** Media item currently loaded into the Source monitor (its
   *  StreamInfo as returned by the engine). `null` when nothing
   *  has been loaded yet. */
  sourceInfo = $state<StreamInfo | null>(null);
  /** Display name of the source media, for the monitor label. */
  sourceName = $state<string>('');
  /** Live source state — stream info + playhead in source PTS. */
  source = $state<SourceState | null>(null);

  /** Last advisory message from the peaks subsystem (sidecar missing,
   *  cache dir un-writable, etc.). UI shows this once as a banner so
   *  the user knows waveforms won't appear; clears on next success. */
  peaksWarning = $state<string | null>(null);

  /** Per-project ephemeral cache-dir identifier. For an unsaved
   *  project we use a stable name derived from a counter so peaks
   *  never spill into the previous bundle. Bumped on every newProject. */
  private untitledCounter = 0;
  private untitledCacheDir: string | null = null;

  private async refreshUndoState() {
    try {
      this.canUndo = await ipcCanUndo();
      this.canRedo = await ipcCanRedo();
    } catch {
      this.canUndo = false;
      this.canRedo = false;
    }
  }

  /** Pull the engine's current sequence contents into the store. */
  async refreshSequence() {
    try {
      this.sequence = await currentSequenceContents();
    } catch {
      this.sequence = null;
    }
  }

  /** Bump editTick + re-fetch sequence contents + refresh undo state.
   *  Call from every mutator (and after undo / redo) so dependents
   *  redraw with the latest state. */
  private async commitEdit() {
    this.editTick++;
    await this.refreshSequence();
    await this.refreshUndoState();
    await this.refreshMagnetic();
  }

  /** Replace the current project with a freshly created empty one. */
  async newProject(name: string) {
    this.error = null;
    try {
      this.info = await ipcNew(name);
      this.path = null;
      this.media = [];
      // Dispose the UI cache AND retarget the backend service to a
      // per-Untitled-N OS-temp slot so peaks for the new project
      // never bleed into whatever bundle was previously loaded.
      await peaksCache.dispose();
      this.untitledCounter++;
      this.untitledCacheDir = await this.retargetCacheDir(
        () => peaksUseUntitledCacheDir(`untitled-${this.untitledCounter}`),
        `untitled-${this.untitledCounter}`
      );
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Load a `.vance/` bundle from disk. */
  async open(path: string) {
    this.error = null;
    try {
      this.info = await loadProject(path);
      this.path = path;
      this.media = await mediaList();
      // Reset the UI peaks cache (drops listener + entries) before
      // pointing the backend service at the new bundle's cache dir.
      // syncPeaksLifecycle then registers every media item + enqueues
      // audio extractions so the timeline can paint waveforms as soon
      // as the user scrolls to them.
      await peaksCache.dispose();
      this.untitledCacheDir = null;
      await this.syncPeaksLifecycle();
      await this.commitEdit();
      // Phase 7.2 — legacy projects (pre-7.2 bundles) deserialize
      // with `has_audio_track: false` for every video. Probe them
      // now so the timeline can render waveforms on Video clips that
      // carry embedded audio. Runs AFTER commitEdit so the chrome is
      // already responsive; results merge into `this.media` as they
      // land and the cache picks them up via subsequent enqueueAllAudio.
      void this.backfillVideoAudioProbes();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Fire the backfill probe for legacy `has_audio_track: false`
   *  videos in the just-loaded project. Async + fire-and-forget so it
   *  doesn't block `open` returning. Merges results into `this.media`
   *  and enqueues extraction only for the items that flipped to
   *  `true` — re-enqueuing the whole audio library would double the
   *  initial extraction queue. */
  private async backfillVideoAudioProbes() {
    // Capture the path the backfill is associated with so a project
    // swap mid-backfill (user opens a different bundle) doesn't apply
    // updates meant for the prior project.
    const issuedAtPath = this.path;
    try {
      const result = await peaksBackfillVideoAudio();
      if (result.probed === 0) return;
      // Drop the result if the project changed while we were probing.
      if (this.path !== issuedAtPath) return;
      if (result.flipped_to_has_audio === 0) return;
      // Patch the local media array with the new flags.
      const flippedToTrue = result.updates.filter((u) => u.has_audio_track);
      const flippedSet = new Set(flippedToTrue.map((u) => u.media_id));
      this.media = this.media.map((m) =>
        flippedSet.has(m.id) ? { ...m, has_audio_track: true } : m
      );
      // Enqueue extraction ONLY for the items that flipped.
      const newlyExtractable = this.media.filter(
        (m) => flippedSet.has(m.id) && isExtractableAudio(m)
      );
      if (newlyExtractable.length === 0) return;
      const results = await Promise.allSettled(
        newlyExtractable.map((m) =>
          peaksEnqueue(
            m.id,
            m.path,
            m.content_hash_prefix_hex,
            Number.isFinite(m.duration_seconds) ? m.duration_seconds : null
          )
        )
      );
      const failed = results
        .map((r, i) => (r.status === 'rejected' ? newlyExtractable[i].id : null))
        .filter((x): x is string => x !== null);
      if (failed.length > 0) {
        console.warn(`peaksEnqueue (backfill) failed for ${failed.length} media items:`, failed);
      }
    } catch (e) {
      console.warn('peaksBackfillVideoAudio failed:', e);
    }
  }

  /** Centralized backend cache-dir swap. Captures the attempted path
   *  in any error message so the warning is operator-actionable. */
  private async retargetCacheDir(
    swap: () => Promise<string | void>,
    attemptedLabel: string
  ): Promise<string | null> {
    try {
      const result = await swap();
      this.peaksWarning = null;
      return typeof result === 'string' ? result : null;
    } catch (e) {
      const msg = `peaksSetCacheDir(${attemptedLabel}) failed: ${e}`;

      console.warn(msg);
      this.peaksWarning = msg;
      return null;
    }
  }

  /** Point the backend PeaksService at `<bundle>/cache/peaks/`, then
   *  register every media item with the UI cache and enqueue audio
   *  items for background extraction. Idempotent — safe to call on
   *  re-mount, project refresh, etc.
   *
   *  Registration is unconditional: even if the backend swap fails,
   *  the UI cache still learns about every media so a later
   *  peaks://progress 'done' (e.g. from a sidecar that recovers) can
   *  surface fresh waveforms without a project reload. Enqueues are
   *  parallelized via Promise.allSettled so a 50-clip project doesn't
   *  block project-open on 50 sequential IPC round-trips. */
  private async syncPeaksLifecycle() {
    if (this.path) {
      await this.retargetCacheDir(
        () => peaksSetCacheDir(`${this.path}/cache/peaks`),
        `${this.path}/cache/peaks`
      );
    }
    // Always register, regardless of cache-dir-swap outcome.
    for (const m of this.media) {
      peaksCache.registerMedia(m.id, m.content_hash_prefix_hex);
    }
    // Parallel enqueue — fire-and-forget per item; aggregate via
    // Promise.allSettled so individual failures don't reject the lot.
    const audio = this.media.filter((m) => isExtractableAudio(m));
    if (audio.length === 0) return;
    const results = await Promise.allSettled(
      audio.map((m) =>
        peaksEnqueue(
          m.id,
          m.path,
          m.content_hash_prefix_hex,
          Number.isFinite(m.duration_seconds) ? m.duration_seconds : null
        )
      )
    );
    const failed = results
      .map((r, i) => (r.status === 'rejected' ? audio[i].id : null))
      .filter((x): x is string => x !== null);
    if (failed.length > 0) {
      console.warn(`peaksEnqueue failed for ${failed.length} media items:`, failed);
    }
  }

  /** One-shot probe of the backend peaks subsystem. Sets
   *  `peaksWarning` so the chrome can surface a banner. Call once at
   *  app startup. */
  async checkPeaksService(): Promise<void> {
    try {
      const s = await peaksServiceStatus();
      if (!s.available) {
        this.peaksWarning = s.reason ?? 'peaks service unavailable (no reason reported)';
      }
    } catch (e) {
      this.peaksWarning = `peaks_service_status failed: ${e}`;
    }
  }

  /** Save the current project. Pass a path to "Save As"; otherwise the
   *  engine writes back to the loaded location. */
  async save(path: string | null = null) {
    this.error = null;
    try {
      const previousPath = this.path;
      await ipcSave(path);
      // Keep the local `path` mirror in sync for the title bar.
      if (path) this.path = path;
      else this.path = await currentProjectPath();
      // Save As / first-time save: retarget the backend peaks cache
      // dir to the new bundle so subsequent extractions land in the
      // right place. Any peaks already extracted into the untitled
      // temp slot remain there until evicted; the next request will
      // see the slot missing and re-enqueue.
      if (this.path && this.path !== previousPath) {
        await this.retargetCacheDir(
          () => peaksSetCacheDir(`${this.path}/cache/peaks`),
          `${this.path}/cache/peaks`
        );
        this.untitledCacheDir = null;
        // Re-enqueue any audio media so the new bundle gets its own
        // .peaks slots (untitled-temp slots stay in /tmp but aren't
        // authoritative for the saved project any more).
        await this.enqueueAllAudio();
      }
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Re-enqueue extraction for every audio item in `this.media`.
   *  Parallel via Promise.allSettled — see `syncPeaksLifecycle`. */
  private async enqueueAllAudio() {
    const audio = this.media.filter((m) => isExtractableAudio(m));
    if (audio.length === 0) return;
    const results = await Promise.allSettled(
      audio.map((m) =>
        peaksEnqueue(
          m.id,
          m.path,
          m.content_hash_prefix_hex,
          Number.isFinite(m.duration_seconds) ? m.duration_seconds : null
        )
      )
    );
    const failed = results
      .map((r, i) => (r.status === 'rejected' ? audio[i].id : null))
      .filter((x): x is string => x !== null);
    if (failed.length > 0) {
      console.warn(`peaksEnqueue failed for ${failed.length} media items:`, failed);
    }
  }

  /** Switch the active sequence inside the loaded project. */
  async selectSequence(sequenceId: string) {
    this.error = null;
    try {
      await ipcSetSeq(sequenceId);
      // Refresh so `info.current_sequence_id` reflects the change.
      this.info = await projectInfo();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Re-fetch the engine's view of the project. Useful after edit-log
   *  operations once Phase 4.x writes them. */
  async refresh() {
    this.info = await projectInfo();
    this.path = await currentProjectPath();
    this.media = await mediaList();
  }

  /** Load a media item into the Source monitor. Double-click in
   *  the Browser triggers this. */
  async loadSource(mediaId: string) {
    this.error = null;
    try {
      this.sourceInfo = await ipcLoadSource(mediaId);
      const m = this.media.find((x) => x.id === mediaId);
      this.sourceName = m?.name ?? '';
      this.source = await ipcSourceState();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Seek Source to `seconds`. Single-shot decode + render. */
  async seekSource(seconds: number) {
    if (!this.source) return;
    const { time_base_num, time_base_den } = this.source;
    if (time_base_num === 0) return;
    const pts = Math.max(0, Math.round((seconds * time_base_den) / time_base_num));
    try {
      await decodeSourceAt(pts);
      this.source = await ipcSourceState();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Park the Source mark-in at the current source playhead. */
  async setSourceMarkIn() {
    try {
      await ipcSetSourceMarkIn();
      this.source = await ipcSourceState();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Park the Source mark-out at the current source playhead. */
  async setSourceMarkOut() {
    try {
      await ipcSetSourceMarkOut();
      this.source = await ipcSourceState();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Clear both Source marks. */
  async clearSourceMarks() {
    try {
      await ipcClearSourceMarks();
      this.source = await ipcSourceState();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Three-point edit: drop a clip on `trackId` sourced from the
   *  Source media's marks, at the Record playhead. */
  async insertFromSource(trackId: string, overwrite: boolean) {
    this.error = null;
    try {
      await ipcInsertFromSource(trackId, overwrite);
      this.info = await projectInfo();
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Import a media file by path and append it to the library. */
  async importMedia(path: string) {
    this.error = null;
    try {
      const item = await importMediaFile(path);
      this.media = [...this.media, item];
      peaksCache.registerMedia(item.id, item.content_hash_prefix_hex);
      if (isExtractableAudio(item)) {
        // Fire-and-forget — don't block the user's next interaction on
        // an IPC round-trip. Failure logs to console; the peaks://
        // progress event will surface Failed states when M7+ wires
        // them to the chrome.
        void peaksEnqueue(
          item.id,
          item.path,
          item.content_hash_prefix_hex,
          Number.isFinite(item.duration_seconds) ? item.duration_seconds : null
        ).catch((e) => {
          console.warn('peaksEnqueue failed for', item.id, e);
        });
      }
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Create a synthetic title clip in the media library. Returns the
   *  new item's ID so the caller can immediately insert it on the
   *  timeline (or `null` on failure). */
  async createTitle(args: {
    displayName: string;
    durationSeconds: number;
    text: string;
    sizePx?: number;
    fgRgb?: [number, number, number];
    bgRgb?: [number, number, number];
    width?: number;
    height?: number;
  }): Promise<string | null> {
    this.error = null;
    try {
      const item = await createTitleMedia({
        displayName: args.displayName,
        durationSeconds: args.durationSeconds,
        text: args.text,
        sizePx: args.sizePx ?? 96,
        fgRgb: args.fgRgb ?? [1, 1, 1],
        bgRgb: args.bgRgb ?? [0, 0, 0],
        width: args.width ?? 1280,
        height: args.height ?? 720
      });
      this.media = [...this.media, item];
      return item.id;
    } catch (e) {
      this.error = String(e);
      return null;
    }
  }

  /** Drop a clip on the timeline. */
  /** Override the input color space tag on a media library item.
   *  `null` clears the override (engine falls back to default). */
  async setMediaInputColorSpace(mediaId: string, space: ColorSpaceLabel | null) {
    this.error = null;
    try {
      await setMediaInputColorSpace(mediaId, space);
      // Refresh the local copy of media so the dropdown reflects
      // the new value immediately.
      this.media = this.media.map((m) =>
        m.id === mediaId ? { ...m, input_color_space: space } : m
      );
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Replace the project's working color space. */
  async setWorkingColorSpace(space: ColorSpaceLabel) {
    this.error = null;
    try {
      await setProjectWorkingColorSpace(space);
      if (this.info) {
        this.info = { ...this.info, working_color_space: space };
      }
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Set or clear the project's display LUT (Phase 6.5). */
  async setViewLut(path: string | null) {
    this.error = null;
    try {
      await setProjectViewLut(path);
      if (this.info) {
        this.info = { ...this.info, view_lut_path: path };
      }
    } catch (e) {
      this.error = String(e);
    }
  }

  /** List colorspaces in an OCIO config (Phase 6.1b). Returns [] and sets
   *  `error` on failure (e.g. OCIO not enabled, bad config). */
  async ocioListColorspaces(config: string | null): Promise<string[]> {
    this.error = null;
    try {
      return await ocioListColorspaces(config);
    } catch (e) {
      this.error = String(e);
      return [];
    }
  }

  /** Apply an OCIO display transform (Phase 6.1b). On success updates the
   *  cached project info so the UI reflects it without a refetch. */
  async setOcioViewTransform(config: string | null, src: string, dst: string, size: number) {
    this.error = null;
    try {
      await setOcioViewTransform(config, src, dst, size);
      if (this.info) {
        this.info = {
          ...this.info,
          ocio_view: { config_path: config, src, dst, size }
        };
      }
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Clear the OCIO display transform (Phase 6.1b). */
  async clearOcioViewTransform() {
    this.error = null;
    try {
      await clearOcioViewTransform();
      if (this.info) {
        this.info = { ...this.info, ocio_view: null };
      }
    } catch (e) {
      this.error = String(e);
    }
  }

  // ---- Scopes (Phase 6.12) -------------------------------------------------

  #scopeBusy = false;
  #scopeAgain = false;
  #scopeArgs = { w: 256, h: 144, pre: false };

  /** Tell the engine whether to compute scopes (skips the GPU readback
   *  entirely when the panel is hidden). Best-effort. */
  async setScopesEnabled(on: boolean) {
    if (!on) this.scopeData = null;
    try {
      await ipcSetScopesEnabled(on);
    } catch (e) {
      console.debug('setScopesEnabled', e);
    }
  }

  /** Track a power window's center forward N frames via ZNCC and write
   *  the result as keyframes on win_cx/win_cy (one undoable edit).
   *  Synchronous: blocks while the engine decodes and tracks. Phase 6.11. */
  async trackWindow(
    trackId: string,
    clipId: string,
    effectId: string,
    startSourceSeconds: number,
    nFrames: number,
    patchHalf: number,
    searchRadius: number
  ): Promise<TrackSummary | null> {
    this.error = null;
    try {
      return await ipcTrackWindow(
        trackId,
        clipId,
        effectId,
        startSourceSeconds,
        nFrames,
        patchHalf,
        searchRadius
      );
    } catch (e) {
      this.error = String(e);
      return null;
    }
  }

  /** Solo a grade node's qualifier matte to the viewport, or clear it
   *  (`effectId: null`). Transient viewer state (Phase 6.9). */
  async setMatteNode(clipId: string, effectId: string | null) {
    this.error = null;
    try {
      await ipcSetMatteNode(clipId, effectId);
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Recompute scopes for the current frame, coalescing bursts: if a
   *  refresh is already in flight, the latest request's params are kept
   *  and run once the current one settles (single-in-flight). Scopes are
   *  best-effort — failures don't surface as a project error. */
  async refreshScopes(w = 256, h = 144, pre = false) {
    this.#scopeArgs = { w, h, pre };
    if (this.#scopeBusy) {
      this.#scopeAgain = true;
      return;
    }
    this.#scopeBusy = true;
    try {
      do {
        this.#scopeAgain = false;
        const { w: cw, h: ch, pre: cpre } = this.#scopeArgs;
        const d = await computeScopes(cw, ch, cpre);
        if (d) this.scopeData = d;
      } while (this.#scopeAgain);
    } catch (e) {
      console.debug('refreshScopes', e);
    } finally {
      this.#scopeBusy = false;
    }
  }

  async insertClip(trackId: string, mediaId: string, timelineSeconds: number) {
    this.error = null;
    try {
      await insertClipAt(trackId, mediaId, timelineSeconds);
      this.info = await projectInfo();
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** GOAT-6: magnetic-timeline mode (clips ripple + connected clips move
   *  with their anchor). Mirrors the engine's per-sequence flag. */
  magnetic = $state<boolean>(false);

  /** Move an existing clip. In magnetic mode the drag also carries the
   *  clip's connected subtree; otherwise it's a plain single-clip slide. */
  async moveClip(trackId: string, clipId: string, timelineSeconds: number) {
    this.error = null;
    try {
      if (this.magnetic) {
        await magneticMoveClip(trackId, clipId, timelineSeconds);
      } else {
        await moveClipTo(trackId, clipId, timelineSeconds);
      }
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Toggle magnetic-timeline mode (persisted on the sequence, undoable). */
  async setMagnetic(on: boolean) {
    this.error = null;
    try {
      await ipcSetMagnetic(on);
      this.magnetic = on;
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Refresh the magnetic flag from the engine (after open/undo). */
  async refreshMagnetic() {
    try {
      this.magnetic = await isMagnetic();
    } catch {
      this.magnetic = false;
    }
  }

  /** Connect `connectedId` to `anchorId` (FCPX-style attachment). */
  async connectClip(anchorId: string, connectedId: string) {
    this.error = null;
    try {
      await ipcConnectClip(anchorId, connectedId);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Detach a connected clip. */
  async disconnectClip(connectedId: string) {
    this.error = null;
    try {
      await ipcDisconnectClip(connectedId);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Cut-page append: add a media item at a track's gapless end. */
  async appendClip(trackId: string, mediaId: string) {
    this.error = null;
    try {
      await ipcAppendClip(trackId, mediaId);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Ripple delete: remove a clip + close the gap across all tracks. */
  async rippleDeleteClip(trackId: string, clipId: string) {
    this.error = null;
    try {
      await magneticRippleDelete(trackId, clipId);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** The track id that owns `clipId` in the current sequence, or null. */
  trackOfClip(clipId: string): string | null {
    for (const tr of this.sequence?.tracks ?? []) {
      if (tr.clips.some((c) => c.id === clipId)) return tr.id;
    }
    return null;
  }

  /** Remove a clip from its track. */
  async removeClip(trackId: string, clipId: string) {
    this.error = null;
    try {
      await ipcRemoveClip(trackId, clipId);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Attach an effect to a clip. */
  async addEffect(
    trackId: string,
    clipId: string,
    kind: string,
    params: Record<string, EffectValue> = {}
  ) {
    this.error = null;
    try {
      await ipcAddEffect(trackId, clipId, kind, params);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Remove an effect from a clip by ID. */
  async removeEffect(trackId: string, clipId: string, effectId: string) {
    this.error = null;
    try {
      await ipcRemoveEffect(trackId, clipId, effectId);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Update one parameter on an effect (e.g. opacity's `value`). */
  async updateEffectParam(
    trackId: string,
    clipId: string,
    effectId: string,
    key: string,
    value: EffectValue
  ) {
    this.error = null;
    try {
      await ipcUpdateEffectParam(trackId, clipId, effectId, key, value);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Reorder a grade node (effect) within a clip's chain. */
  async moveGradeNode(trackId: string, clipId: string, effectId: string, newIndex: number) {
    this.error = null;
    try {
      await ipcMoveEffect(trackId, clipId, effectId, newIndex);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Toggle a grade node's (effect's) enabled/bypass flag. */
  async setNodeEnabled(trackId: string, clipId: string, effectId: string, enabled: boolean) {
    this.error = null;
    try {
      await ipcSetEffectEnabled(trackId, clipId, effectId, enabled);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  // ---- Phase 7.3 mixer controls --------------------------------
  //
  // Mute / solo / gain mutators do NOT pause playback — the AudioMixer
  // republishes its snapshot on the next buffer, so the user hears
  // the change within ~5-20 ms.

  async setTrackMute(trackId: string, mute: boolean) {
    this.error = null;
    try {
      await ipcSetTrackMute(trackId, mute);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  async setTrackSolo(trackId: string, solo: boolean) {
    this.error = null;
    try {
      await ipcSetTrackSolo(trackId, solo);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  async setTrackGainDb(trackId: string, gainDb: number) {
    this.error = null;
    try {
      await ipcSetTrackGainDb(trackId, gainDb);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  async setClipMute(trackId: string, clipId: string, mute: boolean) {
    this.error = null;
    try {
      await ipcSetClipMute(trackId, clipId, mute);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  async setClipGainDb(trackId: string, clipId: string, gainDb: number) {
    this.error = null;
    try {
      await ipcSetClipGainDb(trackId, clipId, gainDb);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Phase 7.6: track stereo pan. `pan ∈ [-1, +1]`. Live during playback —
   *  the snapshot republish recomputes every clip's pan_linear. */
  async setTrackPan(trackId: string, pan: number) {
    this.error = null;
    try {
      await ipcSetTrackPan(trackId, pan);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Phase 7.6: clip stereo pan. Composes with the parent track's pan
   *  at snapshot build time. Live during playback. */
  async setClipPan(trackId: string, clipId: string, pan: number) {
    this.error = null;
    try {
      await ipcSetClipPan(trackId, clipId, pan);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Phase 7.7 — append an audio effect to a track's chain. */
  async addAudioEffect(trackId: string, effect: AudioEffectSpec) {
    this.error = null;
    try {
      await ipcAddAudioEffect(trackId, effect);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Phase 7.7 — remove the effect at `index`. */
  async removeAudioEffect(trackId: string, index: number) {
    this.error = null;
    try {
      await ipcRemoveAudioEffect(trackId, index);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Phase 7.7 — reorder. */
  async moveAudioEffect(trackId: string, fromIndex: number, toIndex: number) {
    this.error = null;
    try {
      await ipcMoveAudioEffect(trackId, fromIndex, toIndex);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Phase 7.7 — replace one effect's full spec (knob commit). */
  async updateAudioEffect(trackId: string, index: number, effect: AudioEffectSpec) {
    this.error = null;
    try {
      await ipcUpdateAudioEffect(trackId, index, effect);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Phase 7.4: master bus gain in dB. Sequence-level — affects every
   *  audio source after track/clip composition. Live during playback. */
  async setMasterGainDb(gainDb: number) {
    this.error = null;
    try {
      await ipcSetMasterGainDb(gainDb);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Phase 7.4: replace a track's gain automation lane. Empty list =
   *  revert to the static gain_db. Times are timeline-seconds. */
  async setTrackGainKeyframes(trackId: string, keyframes: GainKeyframe[]) {
    this.error = null;
    try {
      await ipcSetTrackGainKeyframes(trackId, keyframes);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Phase 7.4: replace a clip's gain automation lane. Times are
   *  CLIP-relative seconds. Empty list = revert to static gain_db. */
  async setClipGainKeyframes(trackId: string, clipId: string, keyframes: GainKeyframe[]) {
    this.error = null;
    try {
      await ipcSetClipGainKeyframes(trackId, clipId, keyframes);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Phase 7.4 M3: set the clip's fade-in. `durationSeconds == 0`
   *  clears it. Fades compose multiplicatively with the keyframe lane
   *  — setting a fade never erases manual automation. */
  async setClipFadeIn(
    trackId: string,
    clipId: string,
    durationSeconds: number,
    curve: FadeCurveName = 'linear'
  ) {
    this.error = null;
    try {
      await ipcSetClipFadeIn(trackId, clipId, durationSeconds, curve);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Phase 7.4 M3: set the clip's fade-out. Mirror of
   *  [`setClipFadeIn`]. */
  async setClipFadeOut(
    trackId: string,
    clipId: string,
    durationSeconds: number,
    curve: FadeCurveName = 'linear'
  ) {
    this.error = null;
    try {
      await ipcSetClipFadeOut(trackId, clipId, durationSeconds, curve);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Commit several effect params as one undoable step (color-page
   *  wheel release writes R/G/B/master together → single undo). */
  async updateEffectParams(
    trackId: string,
    clipId: string,
    effectId: string,
    params: [string, EffectValue][]
  ) {
    this.error = null;
    try {
      await ipcUpdateEffectParams(trackId, clipId, effectId, params);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Live, non-undoable preview of several grade params during a wheel
   *  drag. Does NOT commitEdit — the caller commits once via
   *  `updateEffectParams` on pointer-up so the undo history gets a
   *  single step. */
  async previewGradeParams(clipId: string, effectId: string, params: [string, EffectValue][]) {
    try {
      await ipcPreviewGradeParams(clipId, effectId, params);
    } catch (e) {
      // Preview is best-effort; don't surface transient drag errors.
      console.debug('previewGradeParams', e);
    }
  }

  /** Drop a transition at the cut after `afterClipId`. `kind` is
   *  'dissolve' (built-in GPU) or 'wasm:<plugin-id>' (a WASM transition
   *  plugin). Default duration: 1 second. */
  async addTransitionAtCut(
    trackId: string,
    afterClipId: string,
    kind = 'dissolve',
    durationSeconds = 1
  ) {
    this.error = null;
    try {
      await ipcAddTransition(trackId, afterClipId, kind, durationSeconds);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Convenience: drop the built-in cross-dissolve. */
  async addDissolveAtCut(trackId: string, afterClipId: string, durationSeconds = 1) {
    await this.addTransitionAtCut(trackId, afterClipId, 'dissolve', durationSeconds);
  }

  /** Remove a transition from a track by ID. */
  async removeTransition(trackId: string, transitionId: string) {
    this.error = null;
    try {
      await ipcRemoveTransition(trackId, transitionId);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Trim a clip's in-point. */
  async trimIn(trackId: string, clipId: string, newSourceInSeconds: number) {
    this.error = null;
    try {
      await trimClipIn(trackId, clipId, newSourceInSeconds);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Trim a clip's out-point. */
  async trimOut(trackId: string, clipId: string, newSourceOutSeconds: number) {
    this.error = null;
    try {
      await trimClipOut(trackId, clipId, newSourceOutSeconds);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Undo the last edit on the active sequence. */
  async undo() {
    this.error = null;
    try {
      const ok = await ipcUndo();
      if (ok) {
        this.info = await projectInfo();
        await this.commitEdit();
      } else {
        await this.refreshUndoState();
      }
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Redo the next edit on the active sequence. */
  async redo() {
    this.error = null;
    try {
      const ok = await ipcRedo();
      if (ok) {
        this.info = await projectInfo();
        await this.commitEdit();
      } else {
        await this.refreshUndoState();
      }
    } catch (e) {
      this.error = String(e);
    }
  }
}

/** Does this media item warrant peak extraction? `audio` items
 *  always do; `video` items that carry an embedded audio track
 *  (detected at import via the audio sidecar's probe, surfaced as
 *  `has_audio_track`) do too. Zero-duration media are skipped — the
 *  freshness check would always fail (the cache compares duration
 *  ±100ms), causing a re-extract on every project open. */
function isExtractableAudio(m: MediaItemInfo): boolean {
  if (m.duration_seconds <= 0) return false;
  if (m.kind === 'audio') return true;
  if (m.kind === 'video') return m.has_audio_track;
  return false;
}

/** Singleton — modules import `projectState` and bind to its runes. */
export const projectState = new ProjectState();
