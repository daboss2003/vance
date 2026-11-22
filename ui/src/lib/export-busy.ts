// While an export holds the engine, every other engine command is turned
// away at once with `EXPORT_IN_PROGRESS` and does nothing. The UI shows one
// gentle hint for that instead of an error, and leaves its state as it was.

import { EXPORT_IN_PROGRESS } from './ipc';

/** The hint shown instead of an error, and the tooltip of editing controls
 *  that are paused during an export. */
export const EXPORT_BUSY_HINT = 'Exporting. This will be available when the export finishes.';

/** Whether `error` (a rejected engine call, or a message built around one,
 *  like "Couldn't add the effect.\nExport in progress") is the engine turning
 *  a command away because an export runs. Matches whole lines only, so a
 *  message that merely mentions an export doesn't count. */
export function isExportBusy(error: unknown): boolean {
  if (error === null || error === undefined) return false;
  const text = error instanceof Error ? error.message : String(error);
  return text.split('\n').some((line) => line.trim() === EXPORT_IN_PROGRESS);
}

/** The error text a REFRESH (a poll, a panel loading its list) should show,
 *  or null when an export turned it away: the panel keeps what it had,
 *  quietly, and loads again when the export ends. (A user action shows the
 *  hint instead: `reportableError` in toast.svelte.ts.) */
export function refreshError(error: unknown): string | null {
  return isExportBusy(error) ? null : String(error);
}

/** Lets a hint through at most once per `windowMs` (the hint's own display
 *  time), so a burst of refused actions shows it once. */
export function createHintGate(windowMs: number) {
  let lastShown: number | null = null;
  return {
    allow(now: number): boolean {
      if (lastShown !== null && now - lastShown < windowMs) return false;
      lastShown = now;
      return true;
    }
  };
}
