// The export in progress, app-wide. `render_sequence` is async on the engine
// side, so the UI stays responsive during an export; this store is the one
// place an export starts from, and it refuses a second one while the first
// runs. It outlives the Export panel, which unmounts when the dock switches.

import {
  cancelExport,
  EXPORT_CANCELLED,
  exportStatus,
  renderSequence,
  type RenderOutcome,
  type RenderRequest
} from './ipc';
import { toast } from './toast.svelte';

export type ExportPhase = 'idle' | 'running' | 'cancelling';

/** How an export ended. */
export type ExportEnd = 'done' | 'cancelled' | 'failed';

/** Classify a finished export: a cancel is a neutral outcome, not an error. */
export function exportEnd(error: unknown): ExportEnd {
  if (error === undefined) return 'done';
  return String(error).trim() === EXPORT_CANCELLED ? 'cancelled' : 'failed';
}

class ExportRun {
  phase = $state<ExportPhase>('idle');
  /** An export runs: the engine turns other commands away until it ends, so
   *  editing entry points are disabled (`EXPORT_BUSY_HINT` as their tooltip). */
  busy = $derived(this.phase !== 'idle');
  /** The last export's outcome: the render totals when it finished. */
  lastOutcome = $state<RenderOutcome | null>(null);
  /** The last export was cancelled (nothing was written). */
  lastCancelled = $state(false);
  /** The last export was one this page adopted after a reload: it ended, but
   *  how (finished, failed) isn't known here. */
  lastEndedUnseen = $state(false);
  /** Settles when the export in progress ends; null when idle. */
  current: Promise<void> | null = null;

  /** Start an export. Resolves to false, doing nothing, while one runs. */
  start(req: RenderRequest): Promise<boolean> {
    if (this.phase !== 'idle') return Promise.resolve(false);
    this.phase = 'running';
    this.lastOutcome = null;
    this.lastCancelled = false;
    this.lastEndedUnseen = false;
    const run = (async () => {
      try {
        this.lastOutcome = await renderSequence(req);
        toast.success('Export finished. Saved to ' + this.lastOutcome.output_path);
      } catch (e) {
        if (exportEnd(e) === 'cancelled') this.lastCancelled = true;
        else toast.error('Export failed.\n' + String(e));
      } finally {
        this.phase = 'idle';
        this.current = null;
      }
    })();
    this.current = run;
    return run.then(() => true);
  }

  /** After a webview reload: if the engine still runs an export (started by
   *  the page before the reload), show it as running, with a working Cancel,
   *  and poll `export_status` every `pollMs` until it ends. */
  async adopt(pollMs = 500): Promise<void> {
    if (this.phase !== 'idle') return;
    let running = false;
    try {
      running = await exportStatus();
    } catch {
      return;
    }
    if (!running || this.phase !== 'idle') return;
    this.phase = 'running';
    this.lastOutcome = null;
    this.lastCancelled = false;
    this.lastEndedUnseen = false;
    const watch = (async () => {
      for (;;) {
        await new Promise((r) => setTimeout(r, pollMs));
        try {
          if (!(await exportStatus())) break;
        } catch {
          // Intentional: the status call is lock-free; if it fails anyway,
          // stop watching rather than show "Exporting" forever.
          break;
        }
      }
      if (this.phase === 'cancelling') this.lastCancelled = true;
      else this.lastEndedUnseen = true;
      this.phase = 'idle';
      this.current = null;
    })();
    this.current = watch;
    await watch;
  }

  /** For an edit about to start: true (after showing the one "exporting"
   *  hint) while an export runs, so the caller doesn't start it. */
  refusesEdit(): boolean {
    if (!this.busy) return false;
    toast.exportBusy();
    return true;
  }

  /** Ask the engine to stop the export in progress. */
  async cancel(): Promise<void> {
    if (this.phase !== 'running') return;
    this.phase = 'cancelling';
    try {
      await cancelExport();
    } catch (e) {
      // The export goes on; offer Cancel again unless it ended meanwhile.
      if (this.phase === 'cancelling') this.phase = 'running';
      toast.error("Couldn't cancel the export.\n" + String(e));
    }
  }
}

/** Singleton: the Export panel binds to it. */
export const exportRun = new ExportRun();
