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

export type ProjectInfo = {
  name: string;
  sequences: SequenceInfo[];
  current_sequence_id: string | null;
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

export type ClipContents = {
  id: string;
  media_id: string;
  name: string;
  timeline_in_seconds: number;
  duration_seconds: number;
  source_in_seconds: number;
  enabled: boolean;
};

export type TrackContents = {
  id: string;
  kind: 'video' | 'audio';
  name: string;
  enabled: boolean;
  locked: boolean;
  clips: ClipContents[];
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
  kind: 'video' | 'audio' | 'image';
  duration_seconds: number;
  frame_rate_num: number | null;
  frame_rate_den: number | null;
};

/** Pull metadata from a media file via the decoder and register it
 *  in the current project's media library. */
export const importMediaFile = (path: string): Promise<MediaItemInfo> =>
  invoke('import_media_file', { path });

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
