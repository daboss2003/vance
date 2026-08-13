// App-wide notifications. Instead of squeezing messages/errors into cramped
// panel corners, code calls `toast.info/success/error(msg)` and they surface as
// dismissible cards in a corner overlay (see Toasts.svelte, mounted at the root).

export type ToastKind = 'info' | 'success' | 'error';

export interface Toast {
  id: number;
  message: string;
  kind: ToastKind;
}

let nextId = 0;

/** Never let the overlay grow past this — a misbehaving caller (e.g. a retry
 *  loop) can't bury the app under a stack of cards. Oldest are dropped first. */
const MAX_TOASTS = 4;

/** The live toast queue (rendered by Toasts.svelte). */
export const toasts = $state<Toast[]>([]);

/** Push a toast; returns its id. `ms = 0` keeps it until dismissed.
 *  Identical (same message + kind) toasts are coalesced instead of stacking. */
export function pushToast(message: string, kind: ToastKind = 'info', ms = 5000): number {
  const msg = String(message);
  // Coalesce: if the same message is already showing, don't add a duplicate.
  const existing = toasts.find((t) => t.message === msg && t.kind === kind);
  if (existing) return existing.id;

  const id = ++nextId;
  toasts.push({ id, message: msg, kind });
  // Cap the queue: drop the oldest until we're within the limit.
  while (toasts.length > MAX_TOASTS) toasts.shift();
  if (ms > 0) setTimeout(() => dismissToast(id), ms);
  return id;
}

export function dismissToast(id: number): void {
  const i = toasts.findIndex((t) => t.id === id);
  if (i >= 0) toasts.splice(i, 1);
}

/** Convenience API. Errors persist until dismissed; info/success auto-hide. */
export const toast = {
  info: (m: string, ms = 5000) => pushToast(m, 'info', ms),
  success: (m: string, ms = 4000) => pushToast(m, 'success', ms),
  error: (m: string) => pushToast(m, 'error', 0)
};
