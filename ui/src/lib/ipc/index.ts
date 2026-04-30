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
