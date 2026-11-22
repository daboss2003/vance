// One ordered queue for engine commands that change state. Engine commands
// run concurrently on background threads, so two fire-and-forget edits could
// otherwise land out of order (a pan slider's second-to-last value applied
// last; undo history reordered). Each queued command starts only after the
// previous one settled.
//
// Slider streams coalesce: a command pushed with the same key as the item at
// the END of the queue (not yet started) replaces it, so only the newest
// value of a burst is sent; everyone who pushed it gets that one result.
// Only the tail coalesces, so a value never jumps ahead of a different edit
// queued after it.

type Waiter = { resolve: (v: unknown) => void; reject: (e: unknown) => void };
type Item = { key: string | null; run: () => Promise<unknown>; waiters: Waiter[] };

export type EditQueue = {
  /** Queue `run`; `key` (or null) groups a slider stream for coalescing. */
  push<T>(key: string | null, run: () => Promise<T>): Promise<T>;
  /** Commands queued but not started yet. */
  readonly pending: number;
};

export function createEditQueue(): EditQueue {
  const items: Item[] = [];
  let running = false;

  async function pump() {
    if (running) return;
    running = true;
    while (items.length > 0) {
      const item = items.shift()!;
      try {
        const value = await item.run();
        for (const w of item.waiters) w.resolve(value);
      } catch (e) {
        // A failed command fails only its own callers; the queue moves on.
        for (const w of item.waiters) w.reject(e);
      }
    }
    running = false;
  }

  return {
    push<T>(key: string | null, run: () => Promise<T>): Promise<T> {
      return new Promise<T>((resolve, reject) => {
        const waiter = { resolve: resolve as (v: unknown) => void, reject };
        const tail = items[items.length - 1];
        if (key !== null && tail !== undefined && tail.key === key) {
          tail.run = run;
          tail.waiters.push(waiter);
        } else {
          items.push({ key, run, waiters: [waiter] });
        }
        void pump();
      });
    },
    get pending() {
      return items.length;
    }
  };
}
