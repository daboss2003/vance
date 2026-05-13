// Typed wrappers around the Tauri commands `vance-app` exposes. Keeping
// these in one place makes it easy to update when the Rust side changes.

import { invoke } from '@tauri-apps/api/core';

export type StreamInfo = {
  width: number;
  height: number;
  pixel_format: string;
  codec_name: string;
  frame_rate_num: number;
  frame_rate_den: number;
  time_base_num: number;
  time_base_den: number;
  duration_pts: number | null;
};

export const backendVersion = (): Promise<string> => invoke('backend_version');

export const setViewportBounds = (
  x: number,
  y: number,
  w: number,
  h: number
): Promise<void> => invoke('set_viewport_bounds', { x, y, w, h });

/** Bounds for the Source monitor's native overlay window. */
export const setSourceViewportBounds = (
  x: number,
  y: number,
  w: number,
  h: number
): Promise<void> => invoke('set_source_viewport_bounds', { x, y, w, h });

/** Open a media item in the Source monitor's decoder and render
 *  its first frame to the Source viewport. */
export const loadSource = (mediaId: string): Promise<StreamInfo> =>
  invoke('load_source', { mediaId });

/** Decode + render the Source media at the given source PTS. */
export const decodeSourceAt = (targetPts: number): Promise<number> =>
  invoke('decode_source_at', { targetPts });

export type SourceState = {
  width: number;
  height: number;
  codec_name: string;
  time_base_num: number;
  time_base_den: number;
  duration_pts: number | null;
  last_pts: number | null;
  mark_in_pts: number | null;
  mark_out_pts: number | null;
};

/** Snapshot of the Source monitor's stream + playhead, or null if
 *  nothing has been loaded into Source. */
export const sourceState = (): Promise<SourceState | null> => invoke('source_state');

/** Park the Source mark-in at the current playhead. */
export const setSourceMarkIn = (): Promise<void> => invoke('set_source_mark_in');

/** Park the Source mark-out at the current playhead. */
export const setSourceMarkOut = (): Promise<void> => invoke('set_source_mark_out');

/** Clear both Source marks. */
export const clearSourceMarks = (): Promise<void> => invoke('clear_source_marks');

/** Insert (or overwrite) a clip on the given track, sourced from
 *  the Source media's [mark_in, mark_out] range, at the Record
 *  playhead. Returns the new clip's UUID. */
export const insertFromSource = (
  trackId: string,
  overwrite: boolean
): Promise<string> => invoke('insert_from_source', { trackId, overwrite });

export const openFile = (path: string): Promise<StreamInfo> =>
  invoke('open_file', { path });

export const decodeAndRenderAt = (targetPts: number): Promise<number> =>
  invoke('decode_and_render_at', { targetPts });

export const startPlayback = (fromPts: number): Promise<void> =>
  invoke('start_playback', { fromPts });

/** Returns the last-presented PTS so the UI can resume from there. */
export const stopPlayback = (): Promise<number> => invoke('stop_playback');

export const isPlaying = (): Promise<boolean> => invoke('is_playing');

/** PTS of the last presented frame from the most recent finished playback
 *  session, or null if no session has finished since the last start. */
export const lastResumePts = (): Promise<number | null> => invoke('last_resume_pts');

// ---- Phase 4: project & sequence state ------------------------------------

/** Lightweight view of the loaded project for the UI. */
export type SequenceInfo = {
  id: string;
  name: string;
  clip_count: number;
};

/** Stringified color-space labels — match `color_space_to_str` in
 *  engine.rs. Extend here when the engine adds a new variant. */
export type ColorSpaceLabel =
  | 'rec709-limited'
  | 'rec709-full'
  | 'srgb'
  | 'linear-rec709'
  | 'linear-rec2020'
  | 'rec2020-limited'
  | 'rec2020-pq'
  | 'rec2020-hlg'
  | 'display-p3'
  | 'acescg'
  | 'acescct';

/** All built-in color-space options in display order. */
export const COLOR_SPACE_OPTIONS: { value: ColorSpaceLabel; label: string }[] = [
  { value: 'rec709-limited', label: 'Rec.709 (limited)' },
  { value: 'rec709-full', label: 'Rec.709 (full)' },
  { value: 'srgb', label: 'sRGB' },
  { value: 'linear-rec709', label: 'Linear Rec.709' },
  { value: 'linear-rec2020', label: 'Linear Rec.2020' },
  { value: 'rec2020-limited', label: 'Rec.2020 (limited)' },
  { value: 'rec2020-pq', label: 'Rec.2020 PQ (HDR)' },
  { value: 'rec2020-hlg', label: 'Rec.2020 HLG (HDR)' },
  { value: 'display-p3', label: 'Display P3' },
  { value: 'acescg', label: 'ACEScg' },
  { value: 'acescct', label: 'ACEScct' },
];

export type ProjectInfo = {
  name: string;
  sequences: SequenceInfo[];
  current_sequence_id: string | null;
  working_color_space: ColorSpaceLabel;
};

/** Load a `.vance/` bundle and make it the engine's current project. */
export const loadProject = (path: string): Promise<ProjectInfo> =>
  invoke('load_project', { path });

/** Snapshot of the current project, or null if none is loaded. */
export const projectInfo = (): Promise<ProjectInfo | null> => invoke('project_info');

/** Switch the active sequence within the loaded project. */
export const setCurrentSequence = (sequenceId: string): Promise<void> =>
  invoke('set_current_sequence', { sequenceId });

/** Replace the current project with a fresh empty one. */
export const newProject = (name: string): Promise<ProjectInfo> =>
  invoke('new_project', { name });

/** Save the current project. Pass `path` to override the remembered
 *  bundle location (Save As). Pass `null` to write back to the
 *  loaded path; errors if there isn't one yet. */
export const saveProject = (path: string | null): Promise<void> =>
  invoke('save_project', { path });

/** Path the current project was loaded from / last saved to. */
export const currentProjectPath = (): Promise<string | null> =>
  invoke('current_project_path');

// ---- Sequence contents (for the timeline canvas) ---------------------------

/** How the engine interpolates outward from this keyframe to the
 *  next one in the list. `Linear` is the default (and the v1
 *  shape); `Bezier` is smoothstep ease-in/ease-out; `Hold` snaps. */
export type KeyframeInterp = 'Linear' | 'Bezier' | 'Hold';

/** Animated numeric value — list of {time_seconds, value, interp}
 *  pinned at clip-relative times, interpolated by the engine. */
export type Keyframe = {
  time_seconds: number;
  value: number;
  interp?: KeyframeInterp;
};

/** Either a plain scalar or an animated keyframes array. The
 *  engine's `EffectValue` is `#[serde(untagged)]`, so a number
 *  serializes as `1`, a bool as `true`, a string as `"x"`, and a
 *  keyframes track as the array itself: `[{time_seconds, value}, …]`. */
export type EffectValue = number | boolean | string | Keyframe[];

/** True when `v` is an animated keyframes array. */
export function isKeyframes(v: EffectValue): v is Keyframe[] {
  return (
    Array.isArray(v) &&
    (v.length === 0 ||
      (typeof v[0] === 'object' &&
        v[0] !== null &&
        'time_seconds' in v[0] &&
        'value' in v[0]))
  );
}

/** Evaluate an effect param at clip-relative `t` seconds. Mirrors
 *  the engine's `EffectValue::eval_number` — including per-keyframe
 *  interpolation modes — so UI live readouts match what's about
 *  to render without an IPC round-trip. */
export function evalNumber(v: EffectValue, t: number, fallback = 0): number {
  if (typeof v === 'number') return v;
  if (isKeyframes(v)) {
    const kfs = v;
    if (kfs.length === 0) return fallback;
    if (kfs.length === 1 || t <= kfs[0].time_seconds) return kfs[0].value;
    const last = kfs[kfs.length - 1];
    if (t >= last.time_seconds) return last.value;
    for (let i = 0; i < kfs.length - 1; i++) {
      const a = kfs[i], b = kfs[i + 1];
      if (t >= a.time_seconds && t <= b.time_seconds) {
        const span = b.time_seconds - a.time_seconds;
        if (span <= 0) return a.value;
        const frac = (t - a.time_seconds) / span;
        const interp: KeyframeInterp = a.interp ?? 'Linear';
        if (interp === 'Hold') return a.value;
        if (interp === 'Bezier') {
          const s = frac * frac * (3 - 2 * frac);
          return a.value + (b.value - a.value) * s;
        }
        return a.value + (b.value - a.value) * frac;
      }
    }
    return last.value;
  }
  return fallback;
}

export type EffectInfo = {
  id: string;
  kind: string;
  enabled: boolean;
  params: Record<string, EffectValue>;
};

export type ClipContents = {
  id: string;
  media_id: string;
  name: string;
  timeline_in_seconds: number;
  duration_seconds: number;
  source_in_seconds: number;
  enabled: boolean;
  effects: EffectInfo[];
};

export type TransitionInfo = {
  id: string;
  kind: 'dissolve';
  position_seconds: number;
  duration_seconds: number;
};

export type TrackContents = {
  id: string;
  kind: 'video' | 'audio';
  name: string;
  enabled: boolean;
  locked: boolean;
  clips: ClipContents[];
  transitions: TransitionInfo[];
};

export type SequenceContents = {
  id: string;
  name: string;
  frame_rate_num: number;
  frame_rate_den: number;
  duration_seconds: number;
  tracks: TrackContents[];
};

/** Full contents of the engine's current sequence, ready to render on
 *  the timeline canvas. `null` when no project / no active sequence. */
export const currentSequenceContents = (): Promise<SequenceContents | null> =>
  invoke('current_sequence_contents');

/** Send a debug line into the engine's tracing log. */
export const diagLog = (message: string): Promise<void> =>
  invoke('diag_log', { message });

// ---- Media library + edit ops --------------------------------------------

export type MediaItemInfo = {
  id: string;
  name: string;
  path: string;
  kind: 'video' | 'audio' | 'image' | 'title';
  duration_seconds: number;
  frame_rate_num: number | null;
  frame_rate_den: number | null;
  /** User-overridable input color space (Phase 6.2). `null` means
   *  "use engine default" (Rec.709 limited today). */
  input_color_space: ColorSpaceLabel | null;
};

/** Pull metadata from a media file via the decoder and register it
 *  in the current project's media library. */
export const importMediaFile = (path: string): Promise<MediaItemInfo> =>
  invoke('import_media_file', { path });

/** Create a synthetic title media item. No file backing — the engine
 *  rasterizes frames from these params each time the clip plays. */
export const createTitleMedia = (args: {
  displayName: string;
  durationSeconds: number;
  text: string;
  sizePx: number;
  fgRgb: [number, number, number];
  bgRgb: [number, number, number];
  width: number;
  height: number;
}): Promise<MediaItemInfo> =>
  invoke('create_title_media', args);

/** Override (or clear, by passing `null`) the input color space tag
 *  for a media library item. Phase 6.2. */
export const setMediaInputColorSpace = (
  mediaId: string,
  colorSpace: ColorSpaceLabel | null
): Promise<void> =>
  invoke('set_media_input_color_space', { mediaId, colorSpace });

/** Replace the project's working color space (Phase 6.2). */
export const setProjectWorkingColorSpace = (
  colorSpace: ColorSpaceLabel
): Promise<void> =>
  invoke('set_project_working_color_space', { colorSpace });

/** All media items in the current project's library. */
export const mediaList = (): Promise<MediaItemInfo[]> => invoke('media_list');

/** Drop a clip referencing `mediaId` (full visible range) onto
 *  `trackId` at `timelineSeconds`. Returns the new clip's UUID. */
export const insertClipAt = (
  trackId: string,
  mediaId: string,
  timelineSeconds: number
): Promise<string> =>
  invoke('insert_clip_at', { trackId, mediaId, timelineSeconds });

/** Move an existing clip to a new timeline position on the same
 *  track. Rejects if the new position would overlap a neighbor. */
export const moveClipTo = (
  trackId: string,
  clipId: string,
  timelineSeconds: number
): Promise<void> =>
  invoke('move_clip_to', { trackId, clipId, timelineSeconds });

/** Remove a clip from its track (leaves a gap). */
export const removeClip = (trackId: string, clipId: string): Promise<void> =>
  invoke('remove_clip', { trackId, clipId });

/** Attach an effect to a clip. Returns the new effect's UUID. */
export const addEffectToClip = (
  trackId: string,
  clipId: string,
  kind: string,
  params: Record<string, EffectValue>
): Promise<string> =>
  invoke('add_effect_to_clip', { trackId, clipId, kind, params });

/** Remove an effect from a clip by ID. */
export const removeEffectFromClip = (
  trackId: string,
  clipId: string,
  effectId: string
): Promise<void> =>
  invoke('remove_effect_from_clip', { trackId, clipId, effectId });

/** Update one parameter on an effect attached to a clip. */
export const updateEffectParam = (
  trackId: string,
  clipId: string,
  effectId: string,
  key: string,
  value: EffectValue
): Promise<void> =>
  invoke('update_effect_param', { trackId, clipId, effectId, key, value });

/** Add a cross-dissolve transition at the cut following `afterClipId`.
 *  `kind` is currently always `"dissolve"`. Returns the new transition's UUID. */
export const addTransitionAtCut = (
  trackId: string,
  afterClipId: string,
  kind: 'dissolve',
  durationSeconds: number
): Promise<string> =>
  invoke('add_transition_at_cut', {
    trackId,
    afterClipId,
    kind,
    durationSeconds,
  });

/** Remove a transition by ID. Undoable. */
export const removeTransition = (
  trackId: string,
  transitionId: string
): Promise<void> =>
  invoke('remove_transition', { trackId, transitionId });

/** Trim a clip's in-point. `newSourceInSeconds` is the new visible
 *  start inside the source media; the clip's timeline_in shifts by
 *  the same delta so the right edge stays put on the timeline. */
export const trimClipIn = (
  trackId: string,
  clipId: string,
  newSourceInSeconds: number
): Promise<void> =>
  invoke('trim_clip_in', { trackId, clipId, newSourceInSeconds });

/** Trim a clip's out-point. `timeline_in` is unchanged; the clip's
 *  timeline_out slides to match the new length. */
export const trimClipOut = (
  trackId: string,
  clipId: string,
  newSourceOutSeconds: number
): Promise<void> =>
  invoke('trim_clip_out', { trackId, clipId, newSourceOutSeconds });

/** Start playback of the active sequence from `timelineSeconds`,
 *  using scene_plan to pick the active clip and swapping the
 *  decoder onto its media. */
export const playSequence = (timelineSeconds: number): Promise<void> =>
  invoke('play_sequence', { timelineSeconds });

/** Single-shot decode + render of the active sequence at
 *  `timelineSeconds`. Returns the actual presented PTS. */
export const decodeSequenceAt = (timelineSeconds: number): Promise<number> =>
  invoke('decode_sequence_at', { timelineSeconds });

/** Engine's current sequence playhead in seconds. `null` when no
 *  sequence playback has started since the last project load. */
export const currentTimelineSeconds = (): Promise<number | null> =>
  invoke('current_timeline_seconds');

// ---- Undo / redo ----------------------------------------------------------

export const undo = (): Promise<boolean> => invoke('undo');
export const redo = (): Promise<boolean> => invoke('redo');
export const canUndo = (): Promise<boolean> => invoke('can_undo');
export const canRedo = (): Promise<boolean> => invoke('can_redo');
