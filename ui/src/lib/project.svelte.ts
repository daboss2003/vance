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
  updateEffectParams as ipcUpdateEffectParams,
  previewGradeParams as ipcPreviewGradeParams,
  loadSource as ipcLoadSource,
  mediaList,
  moveClipTo,
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
  type StreamInfo
} from './ipc';

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
  }

  /** Replace the current project with a freshly created empty one. */
  async newProject(name: string) {
    this.error = null;
    try {
      this.info = await ipcNew(name);
      this.path = null;
      this.media = [];
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
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
  }

  /** Save the current project. Pass a path to "Save As"; otherwise the
   *  engine writes back to the loaded location. */
  async save(path: string | null = null) {
    this.error = null;
    try {
      await ipcSave(path);
      // Keep the local `path` mirror in sync for the title bar.
      if (path) this.path = path;
      else this.path = await currentProjectPath();
    } catch (e) {
      this.error = String(e);
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
        height: args.height ?? 720,
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
  async setMediaInputColorSpace(
    mediaId: string,
    space: ColorSpaceLabel | null
  ) {
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

  /** Move an existing clip. */
  async moveClip(trackId: string, clipId: string, timelineSeconds: number) {
    this.error = null;
    try {
      await moveClipTo(trackId, clipId, timelineSeconds);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
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
  async previewGradeParams(
    clipId: string,
    effectId: string,
    params: [string, EffectValue][]
  ) {
    try {
      await ipcPreviewGradeParams(clipId, effectId, params);
    } catch (e) {
      // Preview is best-effort; don't surface transient drag errors.
      console.debug('previewGradeParams', e);
    }
  }

  /** Drop a cross-dissolve at the cut after `afterClipId`. Default
   *  duration: 1 second. */
  async addDissolveAtCut(
    trackId: string,
    afterClipId: string,
    durationSeconds = 1
  ) {
    this.error = null;
    try {
      await ipcAddTransition(trackId, afterClipId, 'dissolve', durationSeconds);
      await this.commitEdit();
    } catch (e) {
      this.error = String(e);
    }
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

/** Singleton — modules import `projectState` and bind to its runes. */
export const projectState = new ProjectState();
