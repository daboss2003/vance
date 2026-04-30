// Reactive project state for the editor chrome.
//
// Single shared store using Svelte 5 runes. Components read `state.project`
// and call the helpers below; everything is kept in sync with the engine
// via the IPC commands in `$lib/ipc`.

import {
  currentProjectPath,
  loadProject,
  newProject as ipcNew,
  projectInfo,
  saveProject as ipcSave,
  setCurrentSequence as ipcSetSeq,
  type ProjectInfo
} from './ipc';

class ProjectState {
  /** Current project as the engine sees it; `null` until something is loaded. */
  info = $state<ProjectInfo | null>(null);
  /** Filesystem path of the loaded bundle, or `null` if the project is unsaved. */
  path = $state<string | null>(null);
  /** Last error string from a project IPC call; `null` when clean. */
  error = $state<string | null>(null);

  /** Replace the current project with a freshly created empty one. */
  async newProject(name: string) {
    this.error = null;
    try {
      this.info = await ipcNew(name);
      this.path = null;
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
  }
}

/** Singleton — modules import `projectState` and bind to its runes. */
export const projectState = new ProjectState();
