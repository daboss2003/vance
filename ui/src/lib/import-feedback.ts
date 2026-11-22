import type { MediaItemInfo } from './ipc';

/** Result of `projectState.importMedia`, which reports failure instead of
 *  throwing. `cancelled`: the user stopped an import that was waiting on its
 *  file (a neutral outcome, not an error). */
export type ImportOutcome =
  { ok: true; item: MediaItemInfo } | { ok: false; error: string; cancelled?: boolean };

/** The engine's error text for an import stopped by `cancel_import`, if it
 *  reports one (a user-requested cancel is recognised without it too). */
export const IMPORT_CANCELLED = 'Import cancelled';

/** The toast a media import ends with. The backend's error text is already
 *  written for the user, so it is shown as-is under the file name. */
export function importToast(
  name: string,
  outcome: ImportOutcome
): { kind: 'success' | 'error' | 'info'; message: string } {
  if (outcome.ok) return { kind: 'success', message: `Added “${name}” to the media pool.` };
  if (outcome.cancelled)
    return { kind: 'info', message: `Import cancelled.\n“${name}” wasn’t added.` };
  return { kind: 'error', message: `Couldn’t import “${name}”.\n${outcome.error}` };
}

/** The "Waiting for <file>…" notices (`engine://import-waiting` /
 *  `engine://import-settled`), one per file name, and which waits the user
 *  cancelled. `show` / `hide` / `isShown` are the notice UI (toasts). */
export function createImportWaits<Id>(ui: {
  show: (fileName: string) => Id;
  hide: (id: Id) => void;
  isShown: (id: Id) => boolean;
}) {
  const shown = new Map<string, Id>();
  const cancelled = new Set<string>();
  return {
    /** An import started waiting on `fileName`: show its notice once. */
    waiting(fileName: string) {
      const id = shown.get(fileName);
      if (id !== undefined && ui.isShown(id)) return;
      shown.set(fileName, ui.show(fileName));
    },
    /** That wait ended (the file arrived, or the import was stopped). */
    settled(fileName: string) {
      const id = shown.get(fileName);
      if (id === undefined) return;
      ui.hide(id);
      shown.delete(fileName);
    },
    /** The user pressed Cancel on `fileName`'s notice (or that failed). */
    markCancelled(fileName: string, on = true) {
      if (on) cancelled.add(fileName);
      else cancelled.delete(fileName);
    },
    /** Whether a failed import of `fileName` was cancelled: by the user from
     *  its notice, or by the engine's own text. Consumes the user's mark. */
    wasCancelled(fileName: string, error: string): boolean {
      const byUser = cancelled.delete(fileName);
      return byUser || error.trim() === IMPORT_CANCELLED;
    }
  };
}
