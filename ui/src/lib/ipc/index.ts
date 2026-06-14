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
  /** Phase 7.4: gain automation lane in CLIP-relative seconds. Empty
   *  = use static `gain_db`. */
  gain_keyframes: GainKeyframe[];
  /** Phase 7.4 M3: optional fade-in. `null` = no fade-in. */
  fade_in: Fade | null;
  /** Phase 7.4 M3: optional fade-out. `null` = no fade-out. */
  fade_out: Fade | null;
  /** Phase 7.6: per-clip stereo pan position, range `[-1, +1]`. */
  pan: number;
};

export type FadeCurveName = 'linear' | 'equal_power' | 'logarithmic';

export type Fade = {
  duration_seconds: number;
  curve: FadeCurveName;
};

export type TransitionInfo = {
  id: string;
  /** "dissolve" or "wasm:<plugin-id>". */
  kind: string;
  position_seconds: number;
  duration_seconds: number;
  /** Plugin param overrides (WASM transitions); empty for dissolve. */
  params: Record<string, EffectValue>;
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
  /** Phase 7.4: gain automation lane in timeline-seconds. Empty = use
   *  static `gain_db`. */
  gain_keyframes: GainKeyframe[];
  /** Phase 7.6: stereo pan position, range `[-1, +1]`. */
  pan: number;
  /** Phase 7.7: built-in audio effect chain (EQ + comp + limiter). */
  audio_effects: AudioEffectSpec[];
};

/** Phase 7.7 / 7.10 — tagged-enum spec for one entry in a track's audio
 *  effect chain. Mirrors `vance_timeline::AudioEffectSpec`. */
export type AudioEffectSpec =
  | { kind: 'Eq6'; enabled: boolean; eq: Eq6Spec }
  | { kind: 'Compressor'; enabled: boolean; spec: CompressorSpec }
  | { kind: 'Limiter'; enabled: boolean; spec: LimiterSpec }
  | {
      kind: 'Plugin';
      enabled: boolean;
      descriptor: { format: string; id: string; name: string; vendor: string };
      params: { param_id: number; value: number }[];
    };

/** Phase 7.10 — default for "Add Plugin" with a captured descriptor. */
export function defaultPlugin(d: {
  format: string;
  id: string;
  name: string;
  vendor?: string;
}): AudioEffectSpec {
  return {
    kind: 'Plugin',
    enabled: true,
    descriptor: { format: d.format, id: d.id, name: d.name, vendor: d.vendor ?? '' },
    params: []
  };
}

export type Eq6Spec = { bands: EqBandSpec[] };

export type EqBandKind = 'LowShelf' | 'Peak' | 'HighShelf' | 'LowCut' | 'HighCut';

export type EqBandSpec = {
  kind: EqBandKind;
  enabled: boolean;
  freq_hz: number;
  gain_db: number;
  q: number;
};

export type CompressorSpec = {
  threshold_db: number;
  ratio: number;
  attack_ms: number;
  release_ms: number;
  knee_db: number;
  makeup_db: number;
};

export type LimiterSpec = {
  ceiling_db: number;
  release_ms: number;
};

/** Phase 7.7 — default specs matching the Rust crate constants. Used
 *  by the "Add EQ / Compressor / Limiter" buttons. */
export function defaultEq6(): AudioEffectSpec {
  return {
    kind: 'Eq6',
    enabled: true,
    eq: {
      bands: [
        { kind: 'LowShelf', enabled: true, freq_hz: 80, gain_db: 0, q: 0.707 },
        { kind: 'Peak', enabled: true, freq_hz: 250, gain_db: 0, q: 1.0 },
        { kind: 'Peak', enabled: true, freq_hz: 800, gain_db: 0, q: 1.0 },
        { kind: 'Peak', enabled: true, freq_hz: 2500, gain_db: 0, q: 1.0 },
        { kind: 'Peak', enabled: true, freq_hz: 6000, gain_db: 0, q: 1.0 },
        { kind: 'HighShelf', enabled: true, freq_hz: 12000, gain_db: 0, q: 0.707 }
      ]
    }
  };
}

export function defaultCompressor(): AudioEffectSpec {
  return {
    kind: 'Compressor',
    enabled: true,
    spec: {
      threshold_db: -18,
      ratio: 3,
      attack_ms: 5,
      release_ms: 100,
      knee_db: 6,
      makeup_db: 0
    }
  };
}

export function defaultLimiter(): AudioEffectSpec {
  return {
    kind: 'Limiter',
    enabled: true,
    spec: {
      ceiling_db: -0.3,
      release_ms: 50
    }
  };
}

export type SequenceContents = {
  id: string;
  name: string;
  frame_rate_num: number;
  frame_rate_den: number;
  duration_seconds: number;
  tracks: TrackContents[];
  /** Phase 7.4: master bus gain in dB. UI's master fader binds to
   *  `set_master_gain_db`. */
  master_gain_db: number;
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
  // 'dissolve' (built-in GPU) or 'wasm:<plugin-id>' (a WASM transition).
  kind: string,
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

/** Set one parameter on a transition (WASM transitions). Undoable. The
 *  engine drives the `mix`/`progress` param from the timeline; this sets
 *  the others (e.g. a wipe's `softness`). */
export const setTransitionParam = (
  trackId: string,
  transitionId: string,
  key: string,
  value: EffectValue
): Promise<void> => invoke('set_transition_param', { trackId, transitionId, key, value });

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
export const setClipMute = (trackId: string, clipId: string, mute: boolean): Promise<void> =>
  invoke('set_clip_mute', { trackId, clipId, mute });
export const setClipGainDb = (trackId: string, clipId: string, gainDb: number): Promise<void> =>
  invoke('set_clip_gain_db', { trackId, clipId, gainDb });

/** Phase 7.6: set a track's stereo pan. `pan ∈ [-1, +1]`. Live during playback. */
export const setTrackPan = (trackId: string, pan: number): Promise<void> =>
  invoke('set_track_pan', { trackId, pan });
/** Phase 7.6: set a clip's stereo pan. Composes additively with the parent
 *  track's pan at mixer snapshot build time. Live during playback. */
export const setClipPan = (trackId: string, clipId: string, pan: number): Promise<void> =>
  invoke('set_clip_pan', { trackId, clipId, pan });

/** Phase 7.7 — append a built-in audio effect to a track's chain. */
export const addAudioEffect = (trackId: string, effect: AudioEffectSpec): Promise<void> =>
  invoke('add_audio_effect', { trackId, effect });
/** Phase 7.7 — remove the audio effect at `index` from the chain. */
export const removeAudioEffect = (trackId: string, index: number): Promise<void> =>
  invoke('remove_audio_effect', { trackId, index });
/** Phase 7.7 — reorder one effect within the chain. */
export const moveAudioEffect = (
  trackId: string,
  fromIndex: number,
  toIndex: number
): Promise<void> => invoke('move_audio_effect', { trackId, fromIndex, toIndex });
/** Phase 7.7 — replace one effect's spec (used for every knob commit). */
export const updateAudioEffect = (
  trackId: string,
  index: number,
  effect: AudioEffectSpec
): Promise<void> => invoke('update_audio_effect', { trackId, index, effect });

/** Phase 7.4: set the master bus gain in dB. Sequence-level. */
export const setMasterGainDb = (gainDb: number): Promise<void> =>
  invoke('set_master_gain_db', { gainDb });

/** Phase 7.4: keyframe for a gain automation lane. `time_seconds` is
 *  timeline-relative for tracks and CLIP-relative for clips. `value`
 *  is in dB. `interp` defaults to `linear` if omitted. */
export type GainKeyframe = {
  time_seconds: number;
  value: number;
  interp?: 'linear' | 'bezier' | 'hold';
};

/** Phase 7.4: replace a track's gain automation lane. Empty list =
 *  revert to the static `gain_db`. Live during playback. */
export const setTrackGainKeyframes = (trackId: string, keyframes: GainKeyframe[]): Promise<void> =>
  invoke('set_track_gain_keyframes', { trackId, keyframes });

/** Phase 7.4: replace a clip's gain automation lane (CLIP-relative
 *  time). Live during playback. */
export const setClipGainKeyframes = (
  trackId: string,
  clipId: string,
  keyframes: GainKeyframe[]
): Promise<void> => invoke('set_clip_gain_keyframes', { trackId, clipId, keyframes });

/** Phase 7.4 M3: set the clip's fade-in. `durationSeconds == 0` clears
 *  the fade. Only `'linear'` is honoured in 7.4; the other curve names
 *  are accepted and persisted for 7.5 forward-compat. Fades compose
 *  multiplicatively with the keyframe lane — setting a fade never
 *  erases manual automation. */
export const setClipFadeIn = (
  trackId: string,
  clipId: string,
  durationSeconds: number,
  curve: FadeCurveName = 'linear'
): Promise<void> => invoke('set_clip_fade_in', { trackId, clipId, durationSeconds, curve });

/** Phase 7.4 M3: set the clip's fade-out. Mirror of [`setClipFadeIn`]. */
export const setClipFadeOut = (
  trackId: string,
  clipId: string,
  durationSeconds: number,
  curve: FadeCurveName = 'linear'
): Promise<void> => invoke('set_clip_fade_out', { trackId, clipId, durationSeconds, curve });

/** Phase 7.5 M1: per-track and master meter values (linear amplitude,
 *  NOT dB — the UI converts). */
export type TrackMeter = {
  peak_l: number;
  peak_r: number;
  rms_l: number;
  rms_r: number;
};

/** Phase 7.8 — ITU-R BS.1770-4 loudness + true-peak readout. */
export type LoudnessReadout = {
  /** Momentary loudness — latest 400 ms block, in LUFS. */
  lufs_m: number;
  /** Short-term loudness — sliding 3 s window, in LUFS. */
  lufs_s: number;
  /** Integrated loudness — gated across the playback session. */
  lufs_i: number;
  /** True-peak dBFS, including inter-sample peaks (4× oversampled). */
  true_peak_l_db: number;
  true_peak_r_db: number;
};

/** Phase 7.5 M1: full meter snapshot from the audio mixer's fold
 *  thread, published ~30Hz via ArcSwap. `null` when no mixer backend
 *  is currently running (no playback or single-source path). */
export type MeterSnapshot = {
  master: TrackMeter;
  /** Per-track meters keyed by track UUID (matches `TrackContents.id`). */
  tracks: Record<string, TrackMeter>;
  /** Monotonic publish counter — wraps on overflow. */
  generation: number;
  /** Phase 7.8 — BS.1770 loudness + true-peak readout on the master bus. */
  loudness: LoudnessReadout;
};

export const meterSnapshot = (): Promise<MeterSnapshot | null> => invoke('meter_snapshot');

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

/** Begin a scrub session — warms the decoder + opens the output stream
 *  so the first grain isn't slow. */
export const scrubBegin = (): Promise<void> => invoke('scrub_begin');

/** Play a sample-accurate grain of mixed audio at `atSeconds`. Replaces
 *  the previous grain, so the audio follows the playhead. */
export const scrubGrain = (atSeconds: number): Promise<void> =>
  invoke('scrub_grain', { atSeconds });

/** End the scrub session (tear down the output + warm decoder). */
export const scrubEnd = (): Promise<void> => invoke('scrub_end');

/** Phase 7.10 / 7.11 — UI descriptor for one enumerated plugin
 *  returned by `list_audio_plugins`. `format` is `"lv2"` or `"clap"`;
 *  `runnable` is false when the sidecar can see the plugin (via the
 *  filesystem walk) but cannot host it (e.g. CLAP support not compiled in). */
export type AudioPluginDescriptor = {
  format: string;
  id: string;
  name: string;
  vendor: string;
  runnable: boolean;
};

/** Phase 7.10 / 7.11 — enumerate plugins via the sandboxed plugin host
 *  sidecar. Returns an empty list when the sidecar binary is missing
 *  so the UI degrades gracefully. */
export const listAudioPlugins = (): Promise<AudioPluginDescriptor[]> =>
  invoke('list_audio_plugins');

/** One enumerated plugin parameter, for the FX panel's knobs. */
export type PluginParamInfo = {
  id: number;
  name: string;
  min: number;
  max: number;
  default: number;
};

/** Enumerate a plugin's parameters (spawns a one-shot sidecar probe).
 *  Returns an empty list when the plugin can't be loaded. */
export const listPluginParams = (
  format: string,
  id: string
): Promise<PluginParamInfo[]> => invoke('list_plugin_params', { format, id });

// ---- Phase 10 — WASM effect plugins ---------------------------------------

/** One tunable parameter on a discovered WASM plugin. */
export type WasmPluginParam = {
  key: string;
  label: string;
  min: number;
  max: number;
  default: number;
};

/** A discovered WASM effect/transition plugin (mirrors the host's
 *  UiWasmPlugin). */
export type WasmPlugin = {
  id: string;
  name: string;
  version: string;
  author: string;
  kind: 'effect' | 'transition';
  params: WasmPluginParam[];
};

/** Phase 10.3 — rescan the plugin folders; returns the count found. */
export const rescanPlugins = (): Promise<number> => invoke('rescan_plugins');

/** Phase 10.5 — list discovered WASM plugins (rescans first so a
 *  freshly-dropped .wasm appears without a restart). */
export const listWasmPlugins = (): Promise<WasmPlugin[]> => invoke('list_wasm_plugins');

/** Apply a discovered WASM plugin to a clip as a `wasm:<id>` effect,
 *  seeding params with the plugin's declared defaults. */
export const applyWasmPlugin = (
  trackId: string,
  clipId: string,
  plugin: WasmPlugin
): Promise<string> => {
  const params: Record<string, EffectValue> = {};
  for (const p of plugin.params) params[p.key] = p.default;
  return addEffectToClip(trackId, clipId, `wasm:${plugin.id}`, params);
};

/** Phase 10.7 — one installed OFX plugin (read-only catalog entry). */
export type OfxPlugin = {
  bundle: string;
  identifier: string;
  api: string;
  api_version: number;
  version_major: number;
  version_minor: number;
};

/** Phase 10.7 — OFX catalog: installed plugins + per-bundle load
 *  errors. */
export type OfxCatalog = {
  plugins: OfxPlugin[];
  errors: string[];
};

/** Phase 10.7 — list installed OFX plugins via the sandboxed
 *  vance-ofx-host sidecar. Read-only: OFX plugins are catalogued but
 *  not yet runnable in the pipeline (post-v1). Empty when the sidecar
 *  binary is absent. */
export const listOfxPlugins = (): Promise<OfxCatalog> => invoke('list_ofx_plugins');

// ---- Phase 8 — Export pipeline -------------------------------------------

/** Phase 8 — encoder preset discriminants (mirrors the Rust enum). */
export type EncoderPresetKey =
  | 'ProRes422HQ'
  | 'ProRes422'
  | 'ProRes4444'
  | 'DnxhrHq'
  | 'DnxhrSq'
  | 'DnxhrLb'
  | 'H264'
  | 'Hevc'
  | 'Av1';

/** Phase 8 — UI render request. */
export type RenderRequest = {
  output_path: string;
  preset: EncoderPresetKey;
  crf?: number | null;
  width?: number | null;
  height?: number | null;
  start_seconds?: number | null;
  end_seconds?: number | null;
};

/** Phase 8 — UI render outcome. */
export type RenderOutcome = {
  output_path: string;
  video_frames: number;
  audio_samples: number;
  elapsed_seconds: number;
};

/** Phase 8 — fire off a sequence render via the encoder sidecar.
 *  Blocking on the backend side until the file is fully written. */
export const renderSequence = (req: RenderRequest): Promise<RenderOutcome> =>
  invoke('render_sequence', { req });

/** Phase 8.7 — render-and-replace request. Renders the range, imports
 *  the result as a new media file, then overwrites the range on
 *  `track_id` with a single clip pointing at the rendered file. */
export type RenderAndReplaceRequest = {
  output_path: string;
  preset: EncoderPresetKey;
  crf?: number | null;
  width?: number | null;
  height?: number | null;
  track_id: string;
  start_seconds: number;
  end_seconds: number;
};

/** Phase 8.7 — render-and-replace outcome. */
export type RenderAndReplaceOutcome = {
  render: RenderOutcome;
  new_media_id: string;
  new_clip_id: string;
};

/** Phase 8.7 — fire a render-and-replace operation. */
export const renderAndReplace = (req: RenderAndReplaceRequest): Promise<RenderAndReplaceOutcome> =>
  invoke('render_and_replace', { req });

// ---- Undo / redo ----------------------------------------------------------

export const undo = (): Promise<boolean> => invoke('undo');
export const redo = (): Promise<boolean> => invoke('redo');
export const canUndo = (): Promise<boolean> => invoke('can_undo');
export const canRedo = (): Promise<boolean> => invoke('can_redo');

// ---- Edit history + branching ("Git for edits") --------------------------

/** One node in the edit-history tree. */
export interface HistoryNode {
  id: number;
  parent: number | null;
  children: number[];
  label: string;
  name: string | null;
  is_current: boolean;
}

/** Full edit-history tree for the current sequence. */
export const editHistory = (): Promise<HistoryNode[]> => invoke('edit_history');
/** Name the current history node as a checkpoint / branch. */
export const checkpointHistory = (name: string): Promise<void> =>
  invoke('checkpoint_history', { name });
/** Jump to a history node by id (switch branch / restore version). */
export const gotoHistory = (nodeId: number): Promise<boolean> =>
  invoke('goto_history', { nodeId });

// ---- Parameter expressions (GOAT expression engine) ----------------------

/** Live compile-check of a parameter expression. Returns the error
 *  message, or null when valid. */
export const validateExpression = (expr: string): Promise<string | null> =>
  invoke('validate_expression', { expr });

/** Drive an effect parameter with an expression (e.g.
 *  "value + 30.0 * noise(time * 2.0)"), evaluated per frame. Pass a number
 *  to updateEffectParam to revert to a static value. */
export const setEffectParamExpr = (
  trackId: string,
  clipId: string,
  effectId: string,
  key: string,
  expr: string
): Promise<void> =>
  invoke('update_effect_param', {
    trackId,
    clipId,
    effectId,
    key,
    value: { expr }
  });

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
