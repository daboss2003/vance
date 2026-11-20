import type { MediaItemInfo } from './ipc';

/** Result of `projectState.importMedia`, which reports failure instead of throwing. */
export type ImportOutcome = { ok: true; item: MediaItemInfo } | { ok: false; error: string };

/** The toast a media import ends with. The backend's error text is already
 *  written for the user, so it is shown as-is under the file name. */
export function importToast(
  name: string,
  outcome: ImportOutcome
): { kind: 'success' | 'error'; message: string } {
  return outcome.ok
    ? { kind: 'success', message: `Added “${name}” to the media pool.` }
    : { kind: 'error', message: `Couldn’t import “${name}”.\n${outcome.error}` };
}
