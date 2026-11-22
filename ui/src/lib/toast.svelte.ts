// App-wide notifications. Instead of squeezing messages/errors into cramped
// panel corners, code calls `toast.info/success/error(msg)` and they surface as
// dismissible cards in a corner overlay (see Toasts.svelte, mounted at the root).

import { createHintGate, EXPORT_BUSY_HINT, isExportBusy } from './export-busy';

export type ToastKind = 'info' | 'success' | 'error';

/** A button on a toast (e.g. Cancel on "Waiting for clip.mov…"). */
export interface ToastAction {
  label: string;
  /** Label while `run` is in flight (the button is unavailable then). */
  busyLabel: string;
  busy: boolean;
  run: () => unknown;
}

export interface Toast {
  id: number;
  message: string;
  kind: ToastKind;
  action?: ToastAction;
}

let nextId = 0;

/** Never let the overlay grow past this — a misbehaving caller (e.g. a retry
 *  loop) can't bury the app under a stack of cards. Oldest are dropped first. */
const MAX_TOASTS = 4;

/** The live toast queue (rendered by Toasts.svelte). */
export const toasts = $state<Toast[]>([]);

/** Push a toast; returns its id. `ms = 0` keeps it until dismissed.
 *  Identical (same message + kind) toasts are coalesced instead of stacking. */
export function pushToast(
  message: string,
  kind: ToastKind = 'info',
  ms = 5000,
  action?: Omit<ToastAction, 'busy'>
): number {
  const msg = String(message);
  // Coalesce: if the same message is already showing, don't add a duplicate.
  const existing = toasts.find((t) => t.message === msg && t.kind === kind);
  if (existing) return existing.id;

  const id = ++nextId;
  toasts.push({ id, message: msg, kind, action: action ? { ...action, busy: false } : undefined });
  // Cap the queue: drop the oldest until we're within the limit. Toasts with
  // an action go last: they're the only way to reach it.
  while (toasts.length > MAX_TOASTS) {
    const i = toasts.findIndex((t) => !t.action);
    toasts.splice(i >= 0 ? i : 0, 1);
  }
  if (ms > 0) setTimeout(() => dismissToast(id), ms);
  return id;
}

export function dismissToast(id: number): void {
  const i = toasts.findIndex((t) => t.id === id);
  if (i >= 0) toasts.splice(i, 1);
}

const BUSY_HINT_MS = 5000;
const busyGate = createHintGate(BUSY_HINT_MS);

/** The "exporting" hint (export-busy.ts), at most once per its display time. */
function exportBusyHint(): void {
  if (busyGate.allow(Date.now())) pushToast(EXPORT_BUSY_HINT, 'info', BUSY_HINT_MS);
}

/** Convenience API. Errors persist until dismissed; info/success auto-hide.
 *  Intentional: an error that is the engine turning a command away during an
 *  export becomes the one gentle "exporting" hint, not an error toast, for
 *  every caller at once. */
export const toast = {
  info: (m: string, ms = 5000) => pushToast(m, 'info', ms),
  success: (m: string, ms = 4000) => pushToast(m, 'success', ms),
  error: (m: string) => {
    if (isExportBusy(m)) {
      exportBusyHint();
      return -1;
    }
    return pushToast(m, 'error', 0);
  },
  /** Show the "exporting" hint (de-duplicated). */
  exportBusy: exportBusyHint
};

/** The text a panel should show for a failed engine call, or null when the
 *  engine only turned it away during an export (the hint is shown instead
 *  and the panel keeps its state). */
export function reportableError(e: unknown): string | null {
  if (isExportBusy(e)) {
    exportBusyHint();
    return null;
  }
  return String(e);
}
