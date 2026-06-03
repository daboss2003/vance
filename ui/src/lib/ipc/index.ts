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

export const setViewportBounds = (x: number, y: number, w: number, h: number): Promise<void> =>
  invoke('set_viewport_bounds', { x, y, w, h });

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
export const insertFromSource = (trackId: string, overwrite: boolean): Promise<string> =>
  invoke('insert_from_source', { trackId, overwrite });

export const openFile = (path: string): Promise<StreamInfo> => invoke('open_file', { path });

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
  { value: 'acescct', label: 'ACEScct' }
];

export type OcioViewInfo = {
  config_path: string | null;
  src: string;
  dst: string;
  size: number;
};

export type ProjectInfo = {
  name: string;
  sequences: SequenceInfo[];
  current_sequence_id: string | null;
  working_color_space: ColorSpaceLabel;
  /** Active display LUT (Phase 6.5). `null` for unfiltered preview. */
  view_lut_path: string | null;
  /** Active OpenColorIO display transform (Phase 6.1b), or `null`. */
  ocio_view: OcioViewInfo | null;
  /** Whether this build links OpenColorIO. */
  ocio_available: boolean;
};

/** Load a `.vance/` bundle and make it the engine's current project. */
export const loadProject = (path: string): Promise<ProjectInfo> => invoke('load_project', { path });

/** Snapshot of the current project, or null if none is loaded. */
export const projectInfo = (): Promise<ProjectInfo | null> => invoke('project_info');

/** Switch the active sequence within the loaded project. */
export const setCurrentSequence = (sequenceId: string): Promise<void> =>
  invoke('set_current_sequence', { sequenceId });

/** Replace the current project with a fresh empty one. */
export const newProject = (name: string): Promise<ProjectInfo> => invoke('new_project', { name });

/** Save the current project. Pass `path` to override the remembered
 *  bundle location (Save As). Pass `null` to write back to the
 *  loaded path; errors if there isn't one yet. */
export const saveProject = (path: string | null): Promise<void> => invoke('save_project', { path });

/** Path the current project was loaded from / last saved to. */
export const currentProjectPath = (): Promise<string | null> => invoke('current_project_path');

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
      (typeof v[0] === 'object' && v[0] !== null && 'time_seconds' in v[0] && 'value' in v[0]))
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
      const a = kfs[i],
        b = kfs[i + 1];
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
  /** Phase 7.3: per-clip mixer gain in dB. 0 = unity. */
  gain_db: number;
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
  /** Phase 7.3: mixer mute. Distinct from `enabled` (project on/off). */
  mute: boolean;
  /** Phase 7.3: solo flag. If any track in the sequence has solo=true,
   *  only soloed tracks contribute to the audio mix. */
  solo: boolean;
  /** Phase 7.3: mixer gain in dB. 0 = unity. */
  gain_db: number;
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
export const diagLog = (message: string): Promise<void> => invoke('diag_log', { message });

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
  /** 24-char lowercase hex of the first 12 bytes of the media's
   *  content hash. Stable per `(path, mtime, length)` even before
   *  BLAKE3 is computed, so the UI can name `.peaks` slots on the
   *  client side without an extra round-trip. */
  content_hash_prefix_hex: string;
  /** Whether the source carries an audio stream (Phase 7.2).
   *  Always `true` for `kind: 'audio'`; for `kind: 'video'` set by
   *  the import-time audio probe. UI uses this to decide whether
   *  to enqueue peak extraction and whether to draw a waveform on
   *  a video clip's body. */
  has_audio_track: boolean;
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
}): Promise<MediaItemInfo> => invoke('create_title_media', args);

/** Override (or clear, by passing `null`) the input color space tag
 *  for a media library item. Phase 6.2. */
export const setMediaInputColorSpace = (
  mediaId: string,
  colorSpace: ColorSpaceLabel | null
): Promise<void> => invoke('set_media_input_color_space', { mediaId, colorSpace });

/** Replace the project's working color space (Phase 6.2). */
export const setProjectWorkingColorSpace = (colorSpace: ColorSpaceLabel): Promise<void> =>
  invoke('set_project_working_color_space', { colorSpace });

/** Set the project's display LUT (Phase 6.5). Pass `null` to clear. */
export const setProjectViewLut = (path: string | null): Promise<void> =>
  invoke('set_project_view_lut', { path });

/** Set the project's OCIO display transform (Phase 6.1b). `config` is an
 *  `.ocio` path, or `null` for the `$OCIO` env config. */
export const setOcioViewTransform = (
  config: string | null,
  src: string,
  dst: string,
  size: number
): Promise<void> => invoke('set_ocio_view_transform', { config, src, dst, size });

/** Clear the project's OCIO display transform (Phase 6.1b). */
export const clearOcioViewTransform = (): Promise<void> => invoke('clear_ocio_view_transform');

/** List colorspace names in an OCIO config for UI pickers (Phase 6.1b).
 *  `config` is an `.ocio` path, or `null` for the `$OCIO` env config. */
export const ocioListColorspaces = (config: string | null): Promise<string[]> =>
  invoke('ocio_list_colorspaces', { config });

// ---- Scopes (Phase 6.12) -------------------------------------------------

export type ScopeIntensityGrid = { cols: number; levels: number; bins: Uint32Array };
export type ScopeHistogram = {
  r: Uint32Array;
  g: Uint32Array;
  b: Uint32Array;
  luma: Uint32Array;
};
export type ScopeVectorscope = {
  size: number;
  bins: Uint32Array;
  /** Six 75%-bar (vx, vy) grid coords, flat [x0,y0,x1,y1,...]. */
  targets: Float32Array;
  skinLineDeg: number;
};
export type ScopeData = {
  sourceW: number;
  sourceH: number;
  histogram: ScopeHistogram;
  waveformLuma: ScopeIntensityGrid;
  parade: { r: ScopeIntensityGrid; g: ScopeIntensityGrid; b: ScopeIntensityGrid };
  vectorscope: ScopeVectorscope;
};

const SCOPE_BLOB_MAGIC = 0x53435031;

/** Parse the raw little-endian scope blob (mirror of
 *  `ScopeData::to_bytes` in crates/vance-scopes/src/lib.rs). The header
 *  is 9×4 bytes (8 u32 + 1 f32), keeping every following u32/f32 array
 *  4-byte aligned so we can view them zero-copy over the buffer. */
function parseScopeBlob(buf: ArrayBuffer): ScopeData | null {
  if (buf.byteLength < 36) return null;
  const dv = new DataView(buf);
  let o = 0;
  const u32 = () => {
    const v = dv.getUint32(o, true);
    o += 4;
    return v;
  };
  const f32 = () => {
    const v = dv.getFloat32(o, true);
    o += 4;
    return v;
  };
  if (u32() !== SCOPE_BLOB_MAGIC) return null;
  const sourceW = u32();
  const sourceH = u32();
  const histBins = u32();
  const wfCols = u32();
  const wfLevels = u32();
  const vecSize = u32();
  const targetsCount = u32();
  const skinLineDeg = f32();
  const takeU32 = (n: number) => {
    const a = new Uint32Array(buf, o, n);
    o += n * 4;
    return a;
  };
  const histogram: ScopeHistogram = {
    r: takeU32(histBins),
    g: takeU32(histBins),
    b: takeU32(histBins),
    luma: takeU32(histBins)
  };
  const wfN = wfCols * wfLevels;
  const grid = (bins: Uint32Array): ScopeIntensityGrid => ({
    cols: wfCols,
    levels: wfLevels,
    bins
  });
  const waveformLuma = grid(takeU32(wfN));
  const parade = { r: grid(takeU32(wfN)), g: grid(takeU32(wfN)), b: grid(takeU32(wfN)) };
  const vbins = takeU32(vecSize * vecSize);
  const targets = new Float32Array(buf, o, targetsCount * 2);
  return {
    sourceW,
    sourceH,
    histogram,
    waveformLuma,
    parade,
    vectorscope: { size: vecSize, bins: vbins, targets, skinLineDeg }
  };
}

/** Enable/disable scope computation in the engine (Phase 6.12). */
export const setScopesEnabled = (on: boolean): Promise<void> =>
  invoke('set_scopes_enabled', { on });

/** Solo a grade node's HSL-qualifier matte to the viewport, or clear it
 *  (pass `effectId: null`). Transient — not persisted. Phase 6.9. */
export const setMatteNode = (clipId: string, effectId: string | null): Promise<void> =>
  invoke('set_matte_node', { clipId, effectId });

/** Summary returned by `trackWindow`. Phase 6.11. */
export type TrackSummary = {
  frames_tracked: number;
  mean_score: number;
  /** Clip-relative seconds of the first/last keyframes written — the
   *  bounds of the destructive "replaces keyframes in range" effect. */
  start_clip_seconds: number;
  end_clip_seconds: number;
};

/** Track a power-window center across `nFrames` frames forward by ZNCC
 *  on the Y plane, writing the result as keyframes on the effect's
 *  win_cx / win_cy (one undoable edit). Synchronous v1 — the IPC call
 *  blocks while the engine decodes and tracks. Phase 6.11. */
// ---- Typed grading commands (Phase 6.13, AI-assistant hook) -------------
// `GradingCommand` and `GradingCommandOutcome` use schemars-derived JSON
// schemas in the Rust side — fetch the live schema via grading_command_schema()
// at prompt-build time rather than mirroring the variants by hand here.

/** Apply ONE typed grading command (one undo step). */
export const applyGradingCommand = (cmd: unknown): Promise<unknown> =>
  invoke('apply_grading_command', { cmd });

/** Apply a BATCH of typed grading commands as one labeled compound undo
 *  step (sequence ops collapse into `EditOp::Compound`; tracker /
 *  project-level commands run inline and add their own entries). */
export const applyGradingCommands = (cmds: unknown[], label: string | null): Promise<unknown[]> =>
  invoke('apply_grading_commands', { cmds, label });

/** JSON Schema for `GradingCommand` — single source of truth for the
 *  AI's tool-use spec. */
export const gradingCommandSchema = (): Promise<unknown> => invoke('grading_command_schema');

export const trackWindow = (
  trackId: string,
  clipId: string,
  effectId: string,
  startSourceSeconds: number,
  nFrames: number,
  patchHalf: number,
  searchRadius: number
): Promise<TrackSummary> =>
  invoke('track_window', {
    trackId,
    clipId,
    effectId,
    startSourceSeconds,
    nFrames,
    patchHalf,
    searchRadius
  });

/** Compute the current frame's scope data. Returns `null` when there's
 *  no update (scopes disabled, playing, or no cached frame). */
export async function computeScopes(
  width: number,
  height: number,
  preViewLut: boolean
): Promise<ScopeData | null> {
  const resp = await invoke<ArrayBuffer | Uint8Array>('compute_scopes', {
    width,
    height,
    preViewLut
  });
  // Tauri raw responses arrive as ArrayBuffer; normalize to a clean,
  // 0-offset ArrayBuffer so typed-array views are 4-byte aligned.
  let buf: ArrayBuffer;
  if (resp instanceof ArrayBuffer) {
    buf = resp;
  } else if (resp instanceof Uint8Array) {
    buf = resp.buffer.slice(resp.byteOffset, resp.byteOffset + resp.byteLength) as ArrayBuffer;
  } else {
    return null;
  }
  if (buf.byteLength === 0) return null;
  return parseScopeBlob(buf);
}

/** All media items in the current project's library. */
export const mediaList = (): Promise<MediaItemInfo[]> => invoke('media_list');

/** Drop a clip referencing `mediaId` (full visible range) onto
 *  `trackId` at `timelineSeconds`. Returns the new clip's UUID. */
export const insertClipAt = (
  trackId: string,
  mediaId: string,
  timelineSeconds: number
): Promise<string> => invoke('insert_clip_at', { trackId, mediaId, timelineSeconds });

/** Move an existing clip to a new timeline position on the same
 *  track. Rejects if the new position would overlap a neighbor. */
export const moveClipTo = (
  trackId: string,
  clipId: string,
  timelineSeconds: number
): Promise<void> => invoke('move_clip_to', { trackId, clipId, timelineSeconds });

/** Remove a clip from its track (leaves a gap). */
export const removeClip = (trackId: string, clipId: string): Promise<void> =>
  invoke('remove_clip', { trackId, clipId });

/** Attach an effect to a clip. Returns the new effect's UUID. */
export const addEffectToClip = (
  trackId: string,
  clipId: string,
  kind: string,
  params: Record<string, EffectValue>
): Promise<string> => invoke('add_effect_to_clip', { trackId, clipId, kind, params });

/** Remove an effect from a clip by ID. */
export const removeEffectFromClip = (
  trackId: string,
  clipId: string,
  effectId: string
): Promise<void> => invoke('remove_effect_from_clip', { trackId, clipId, effectId });

/** Move an effect to a new index in a clip's chain (color-page node
 *  reorder). Undoable. */
export const moveEffectOnClip = (
  trackId: string,
  clipId: string,
  effectId: string,
  newIndex: number
): Promise<void> => invoke('move_effect_on_clip', { trackId, clipId, effectId, newIndex });

/** Toggle an effect's enabled (bypass) flag. Undoable. */
export const setEffectEnabled = (
  trackId: string,
  clipId: string,
  effectId: string,
  enabled: boolean
): Promise<void> => invoke('set_effect_enabled', { trackId, clipId, effectId, enabled });

/** Update several effect params in one undoable step. The color-page
 *  wheel commit writes R/G/B/master together so a drag is one undo. */
export const updateEffectParams = (
  trackId: string,
  clipId: string,
  effectId: string,
  params: [string, EffectValue][]
): Promise<void> => invoke('update_effect_params', { trackId, clipId, effectId, params });

/** Live, NON-undoable preview of several grade params mid-drag.
 *  Repaints the Record viewport with the overrides applied together;
 *  the caller commits once via `updateEffectParams` on pointer-up. */
export const previewGradeParams = (
  clipId: string,
  effectId: string,
  params: [string, EffectValue][]
): Promise<void> => invoke('preview_grade_params', { clipId, effectId, params });

/** Update one parameter on an effect attached to a clip. */
export const updateEffectParam = (
  trackId: string,
  clipId: string,
  effectId: string,
  key: string,
  value: EffectValue
): Promise<void> => invoke('update_effect_param', { trackId, clipId, effectId, key, value });

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
    durationSeconds
  });

/** Remove a transition by ID. Undoable. */
export const removeTransition = (trackId: string, transitionId: string): Promise<void> =>
  invoke('remove_transition', { trackId, transitionId });

/** Trim a clip's in-point. `newSourceInSeconds` is the new visible
 *  start inside the source media; the clip's timeline_in shifts by
 *  the same delta so the right edge stays put on the timeline. */
export const trimClipIn = (
  trackId: string,
  clipId: string,
  newSourceInSeconds: number
): Promise<void> => invoke('trim_clip_in', { trackId, clipId, newSourceInSeconds });

/** Trim a clip's out-point. `timeline_in` is unchanged; the clip's
 *  timeline_out slides to match the new length. */
export const trimClipOut = (
  trackId: string,
  clipId: string,
  newSourceOutSeconds: number
): Promise<void> => invoke('trim_clip_out', { trackId, clipId, newSourceOutSeconds });

// ---- Phase 7.3 mixer controls ------------------------------------
//
// Mute / solo / gain mutators flow through the same EditLog as every
// other edit op (so undo/redo work). The engine treats them as
// non-pause-for-edit because the whole point of mixer controls is
// live response while playback is running; the AudioMixer republishes
// its snapshot within one buffer of the IPC returning.

export const setTrackMute = (trackId: string, mute: boolean): Promise<void> =>
  invoke('set_track_mute', { trackId, mute });
export const setTrackSolo = (trackId: string, solo: boolean): Promise<void> =>
  invoke('set_track_solo', { trackId, solo });
export const setTrackGainDb = (trackId: string, gainDb: number): Promise<void> =>
  invoke('set_track_gain_db', { trackId, gainDb });
export const setClipMute = (
  trackId: string,
  clipId: string,
  mute: boolean
): Promise<void> => invoke('set_clip_mute', { trackId, clipId, mute });
export const setClipGainDb = (
  trackId: string,
  clipId: string,
  gainDb: number
): Promise<void> => invoke('set_clip_gain_db', { trackId, clipId, gainDb });

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

// ---- Peaks (audio waveform cache) ----------------------------------------
//
// Phase 7.1. Tauri-side service does the work (vance-app/src/peaks.rs);
// the JS side only needs to (a) tell it which media to extract, (b) read
// status, (c) pull bucket ranges for rendering. See the
// `peaksCache.svelte.ts` rune for the actual buffer-management layer
// that builds on these.

/** Schedule a media item for peak extraction. Idempotent — duplicate
 *  calls for the same `mediaId` while it's already queued or running
 *  are no-ops. Resolves once the worker has acknowledged the request
 *  (not when extraction finishes). The `peaks://progress` event fires
 *  for each state transition. */
export const peaksEnqueue = (
  mediaId: string,
  path: string,
  contentHashPrefixHex: string,
  expectedDurationSeconds: number | null
): Promise<void> =>
  invoke('peaks_enqueue', {
    mediaId,
    path,
    contentHashPrefixHex,
    expectedDurationSeconds
  });

export type PeaksStatusResponse = {
  /** lowercase tag. */
  state: 'queued' | 'running' | 'done' | 'failed' | 'cancelled' | 'unknown';
  samples_done: number | null;
  samples_total: number | null;
  reason: string | null;
};

export const peaksStatus = (mediaId: string): Promise<PeaksStatusResponse> =>
  invoke('peaks_status', { mediaId });

/** Bump a queued job to the front of the extraction queue. No-op if
 *  not currently queued. Called when a clip scrolls into the visible
 *  viewport so the user-visible work jumps ahead of background imports. */
export const peaksPrioritize = (mediaId: string): Promise<void> =>
  invoke('peaks_prioritize', { mediaId });

export const peaksCancel = (mediaId: string): Promise<void> => invoke('peaks_cancel', { mediaId });

/** Swap the on-disk peaks cache directory. M6 calls this from the
 *  project-load flow so the cache lives in `<bundle>/cache/peaks/`. */
export const peaksSetCacheDir = (cacheDir: string): Promise<void> =>
  invoke('peaks_set_cache_dir', { cacheDir });

/** Swap the backend cache dir to a per-untitled-project OS-temp slot.
 *  Returns the absolute path of the chosen directory so the UI can
 *  surface it if needed. */
export const peaksUseUntitledCacheDir = (label: string): Promise<string> =>
  invoke('peaks_use_untitled_cache_dir', { label });

export type PeaksBackfillUpdate = {
  media_id: string;
  has_audio_track: boolean;
};

export type PeaksBackfillResult = {
  probed: number;
  flipped_to_has_audio: number;
  updates: PeaksBackfillUpdate[];
};

/** Phase 7.2 — run the audio-track probe against every Video media
 *  item whose `has_audio_track` is still the post-deserialize
 *  default (`false`). Called by ProjectState.open after a legacy
 *  `.vance/` bundle loads. Sequential ~5-15ms per item via the
 *  pooled probe sidecar. */
export const peaksBackfillVideoAudio = (): Promise<PeaksBackfillResult> =>
  invoke('peaks_backfill_video_audio');

export type PeaksServiceAvailability = {
  available: boolean;
  reason: string | null;
};

export const peaksServiceStatus = (): Promise<PeaksServiceAvailability> =>
  invoke('peaks_service_status');

/** Sentinel returned by `peaks_for_clip` when the slot isn't on disk
 *  yet (or is hash-mismatched). Four bytes 0xFFFFFFFF — distinct from
 *  any real `N * 6`-byte payload because 4 is not a multiple of 6. */
export const PEAKS_NOT_READY_SENTINEL_BYTES = 4;

/** Raw lane-major byte slice for a bucket range, or `null` when the
 *  peaks file isn't ready yet. Real payloads are exactly
 *  `bucket_count * 6` bytes laid out as `[min × N | max × N | rms × N]`,
 *  each lane 2-byte aligned. */
export async function peaksForClip(
  contentHashPrefixHex: string,
  level: number,
  bucketStart: number,
  bucketCount: number
): Promise<ArrayBuffer | null> {
  if (!Number.isInteger(bucketCount) || bucketCount <= 0) {
    throw new Error(`bucketCount must be a positive integer (got ${bucketCount})`);
  }
  if (!Number.isInteger(level) || level < 0) {
    throw new Error(`level must be a non-negative integer (got ${level})`);
  }
  if (!Number.isInteger(bucketStart) || bucketStart < 0) {
    throw new Error(`bucketStart must be a non-negative integer (got ${bucketStart})`);
  }
  const resp = await invoke<ArrayBuffer | Uint8Array>('peaks_for_clip', {
    contentHashPrefixHex,
    level,
    bucketStart,
    bucketCount
  });
  let buf: ArrayBuffer;
  if (resp instanceof ArrayBuffer) {
    buf = resp;
  } else if (resp instanceof Uint8Array) {
    buf = resp.buffer.slice(resp.byteOffset, resp.byteOffset + resp.byteLength) as ArrayBuffer;
  } else {
    // Tauri returned a JSON-typed value (number/object/string) instead
    // of a raw byte buffer — that's a protocol mismatch, NOT a
    // not-ready signal. Surface it so we don't silently mask a host-
    // side bug as a transient miss.
    throw new Error(
      `peaks_for_clip: unexpected response type ${typeof resp} (expected ArrayBuffer or Uint8Array)`
    );
  }
  // A 4-byte response is uniquely the not-ready sentinel: real
  // payloads are N*6 bytes for N>=1, so the smallest is 6.
  if (buf.byteLength === PEAKS_NOT_READY_SENTINEL_BYTES) {
    const v = new Uint32Array(buf, 0, 1);
    if (v[0] === 0xffffffff) return null;
    throw new Error(
      `peaks_for_clip: 4-byte response with non-sentinel content 0x${v[0].toString(16).padStart(8, '0')}`
    );
  }
  const expected = bucketCount * 6;
  if (buf.byteLength !== expected) {
    throw new Error(
      `peaks_for_clip: unexpected response length ${buf.byteLength} (expected ${expected} bytes for ${bucketCount} buckets, or 4-byte sentinel)`
    );
  }
  return buf;
}

export type PeaksLevelMeta = {
  level_idx: number;
  samples_per_bucket: number;
  bucket_count: number;
};

export type PeaksMetaResponse = {
  sample_rate: number;
  total_samples: number;
  level_count: number;
  levels: PeaksLevelMeta[];
};

/** Header + level directory for the requested media's peaks slot, or
 *  `null` when the slot isn't on disk yet / is hash-mismatched. UI
 *  uses this to pick the right level for the current zoom and to
 *  bound the bucket range it requests via `peaksForClip`. */
export const peaksMeta = (contentHashPrefixHex: string): Promise<PeaksMetaResponse | null> =>
  invoke('peaks_meta', { contentHashPrefixHex });

// ---- Peaks progress events ----------------------------------------------

export type PeaksProgressPayload = {
  media_id: string;
  state: 'queued' | 'running' | 'done' | 'failed' | 'cancelled';
  samples_done: number | null;
  samples_total: number | null;
  reason: string | null;
};
