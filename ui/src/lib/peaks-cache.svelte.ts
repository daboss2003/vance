// Audio peak (waveform) cache — UI side. Phase 7.1 M4.
//
// Sits between the Tauri `peaks_for_clip` command and the timeline
// waveform renderer. Responsibilities:
//
// 1. Per-media content-hash prefix registry (so the renderer can ask
//    by `mediaId` without knowing the on-disk slot name).
// 2. Per-`(mediaId, level, bucketStart, bucketCount)` cache of
//    decoded lane views (Int16Array / Int16Array / Uint16Array).
// 3. Single-in-flight Promise coalescing — concurrent draw passes
//    asking for the same range share one IPC round-trip.
// 4. LRU byte budget (~16 MB by default) PLUS hard entry-count cap to
//    bound sort cost.
// 5. `peaks://progress` event subscription — on `done` we invalidate
//    cached entries for that media so the next draw fetches the
//    freshly-extracted data instead of stale stub bytes.
// 6. A monotonic `generation` epoch counter bumped on every
//    `invalidate()` and `dispose()`. In-flight fetches that started
//    before the bump are unconditionally discarded on resolve, so
//    `invalidate` is authoritative regardless of whether the prefix
//    changed (closes the "same-prefix invalidate, stale write-back"
//    race in the 'done' event handler).
// 7. A reactive `version` counter so consumers can re-render on any
//    cache mutation without subscribing to every key individually.
// 8. Failure backoff: a fetch that throws (e.g. backend rejected the
//    level) records a short cooldown for its key so a busy redraw
//    loop doesn't spam the IPC retrying the same broken request.
//
// Clip trim/split/move do NOT invalidate this cache. Bucket indices
// are source-frame-keyed, so timeline edits leave the cache valid.
//
// ## Reactivity contract
//
// Consumers MUST read `peaksCache.version` AND call `peek`/`request`
// inside the same `$effect` (or `$derived`) scope. Reading `version`
// outside the same scope as the lookup will NOT cause a re-run when
// the cache mutates.

import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { peaksForClip, peaksMeta, type PeaksMetaResponse, type PeaksProgressPayload } from './ipc';

/** L0 samples-per-bucket — must match `vance-peaks::L0_SAMPLES_PER_BUCKET`. */
const L0_SAMPLES_PER_BUCKET = 256;
/** Pyramid decimation ratio between adjacent levels. */
const LEVEL_RATIO = 4;
/** Maximum number of levels the format reserves. Re-exported so the
 *  caller can pre-budget bucket-range buffers. */
export const PEAKS_LEVEL_COUNT_MAX = 8;

/** Compute samples-per-bucket for a given pyramid level (matches the
 *  `vance-peaks::samples_per_bucket` formula `256 * 4^level`). */
export function samplesPerBucket(level: number): number {
  return L0_SAMPLES_PER_BUCKET * Math.pow(LEVEL_RATIO, level);
}

/** Pick the smallest level whose buckets-per-pixel falls in `[1, 4]`,
 *  matching the Rust `choose_level` exactly. Returns the densest
 *  (level 0) when the user is zoomed in past L0, and the coarsest
 *  (levelCount - 1) when zoomed out past the last level. NaN /
 *  negative / zero inputs return the coarsest level (cheapest fallback). */
export function chooseLevel(
  sampleRate: number,
  pixelsPerSecond: number,
  levelCount: number
): number {
  if (levelCount <= 0) return 0;
  const last = levelCount - 1;
  if (!Number.isFinite(pixelsPerSecond) || pixelsPerSecond <= 0 || sampleRate <= 0) {
    return last;
  }
  for (let i = 0; i < levelCount; i++) {
    const spb = samplesPerBucket(i);
    const bps = (spb / sampleRate) * pixelsPerSecond;
    if (bps >= 1.0 && bps <= 4.0) return i;
  }
  const l0Density = (L0_SAMPLES_PER_BUCKET / sampleRate) * pixelsPerSecond;
  return l0Density > 4.0 ? 0 : last;
}

/** Lane views over a single fetched bucket range. The underlying
 *  ArrayBuffer is owned by the cache; views are zero-copy and become
 *  STALE if the entry is evicted (the ArrayBuffer is still valid JS,
 *  but the cache no longer guarantees that its contents match the
 *  on-disk slot). Consumers MUST re-read views from the cache on every
 *  draw — never store a `LaneViews` across draws. */
export type LaneViews = {
  /** Per-bucket signed envelope minima (i16). Read-only by contract;
   *  writing through this view mutates cache-owned memory and is
   *  undefined behavior at the data-coherence level. */
  min: Int16Array;
  /** Per-bucket signed envelope maxima (i16). See `min` re mutation. */
  max: Int16Array;
  /** Per-bucket RMS (u16, normalized 0..65535). See `min` re mutation. */
  rms: Uint16Array;
  /** Bytes of the underlying ArrayBuffer for accounting. */
  byteLength: number;
};

type Entry = {
  views: LaneViews;
  /** Monotone access stamp for LRU eviction. Bumped on every `request`
   *  that hits this entry. `peek` does NOT bump (per its contract). */
  lastUsed: number;
  /** Cache key (so we can find ourselves in the map during eviction). */
  key: string;
};

/** Bytes per bucket on disk: i16 min + i16 max + u16 rms = 6. */
const BYTES_PER_BUCKET = 6;

/** Default memory budget: 16 MB. Enough to keep ~10 hours of L2 cache
 *  at 48 kHz across multiple media items in working memory. */
const DEFAULT_BUDGET_BYTES = 16 * 1024 * 1024;

/** Hard cap on entry count. Bounds the cost of the sort-by-recency
 *  inside `evictIfOverBudget` even if the per-entry size is tiny
 *  (e.g. 1-bucket-per-pixel L0 ranges). At 2048 entries the sort is
 *  ~22000 comparisons — negligible per fetch. */
const MAX_ENTRY_COUNT = 2048;

/** Cooldown applied to a key whose last fetch threw. Prevents a
 *  draw-loop that re-requests every frame from hammering the IPC
 *  with the same persistently-failing request. */
const FAILURE_COOLDOWN_MS = 1000;

/** Mark a mediaId is a per-process delimiter. UUIDs use `-` so this
 *  byte never appears in a real id, but we still validate to defend
 *  against caller typos sneaking past the type system. */
const KEY_SEPARATOR = '|';

class PeaksCache {
  /** mediaId → content_hash_prefix_hex (24 lowercase hex chars). M6
   *  wires this from media-import; M5 reads it via `getPrefix`. */
  private prefixes = new Map<string, string>();

  /** Cache entries, keyed by `${mediaId}|${level}|${bucketStart}|${bucketCount}`. */
  private entries = new Map<string, Entry>();

  /** In-flight requests by the same key. Concurrent `request` calls
   *  share one IPC round-trip. */
  private inflight = new Map<string, Promise<LaneViews | null>>();

  /** Per-key cooldown end-times (epoch ms). A request whose previous
   *  fetch threw is ignored until `Date.now() >= cooldownUntil[key]`. */
  private cooldownUntil = new Map<string, number>();

  /** Per-media slot header + level directory. Populated lazily on
   *  first `requestMeta` and refreshed on `invalidate` (a 'done'
   *  event implies the slot's headers may have changed sample-rate
   *  / total-samples). */
  private meta = new Map<string, PeaksMetaResponse>();

  /** In-flight `peaksMeta` requests by mediaId. */
  private metaInflight = new Map<string, Promise<PeaksMetaResponse | null>>();

  /** Per-mediaId meta-fetch cooldown end (epoch ms). A null return
   *  (slot not on disk yet) or a thrown error parks the mediaId on a
   *  short cooldown so a busy draw loop doesn't spam peaks_meta every
   *  frame. The peaks://progress 'done' event clears the cooldown via
   *  `invalidate`. */
  private metaCooldownUntil = new Map<string, number>();

  /** Running total of `entries[*].views.byteLength` for LRU budgeting. */
  private totalBytes = 0;

  /** Monotone counter for LRU recency. */
  private accessCounter = 0;

  /** Epoch counter. Bumped by `invalidate` and `dispose`. Captured by
   *  each `doFetch` at start; the post-await coherence check rejects
   *  any result whose captured epoch is stale, so cleared/invalidated
   *  entries can't be silently re-populated by stragglers. */
  private generation = 0;

  /** Reactive bump consumed by Svelte components: any draw that depends
   *  on cache contents should read `cache.version` so it re-runs after
   *  a fetch lands or an invalidate fires. */
  private versionState = $state(0);

  /** True after `ensureListener` has been called AND either succeeded
   *  or scheduled a pending install. Set BEFORE the await to gate
   *  reentry; only flipped back to false if the install threw or
   *  `dispose()` ran. */
  private listenerInstalled = false;
  /** Resolves once the in-flight install finishes (success or fail).
   *  `dispose()` awaits this before clearing so the resolved
   *  UnlistenFn doesn't escape the dispose. */
  private listenerInstallPromise: Promise<void> | null = null;
  private unlistenFn: UnlistenFn | null = null;

  /** Memory budget in bytes. */
  readonly budgetBytes: number;

  constructor(budgetBytes: number = DEFAULT_BUDGET_BYTES) {
    this.budgetBytes = budgetBytes;
  }

  /** Bump-counter exposed to Svelte effects. Read this AND call
   *  `peek`/`request` inside the same `$effect` / `$derived` scope:
   *
   *  ```ts
   *  $effect(() => {
   *    const _ = peaksCache.version;      // subscribe to mutations
   *    const v = peaksCache.request(...); // read in the same scope
   *    if (v) draw(v);
   *  });
   *  ```
   *
   *  Reading `version` outside the scope of the lookup will NOT
   *  cause re-runs when the cache changes. */
  get version(): number {
    return this.versionState;
  }

  /** Idempotent, race-safe lazy installer for the
   *  `peaks://progress` listener. Multiple concurrent callers share
   *  the same install promise; `dispose()` awaits it so the resolved
   *  UnlistenFn can be safely cancelled. */
  private async ensureListener(): Promise<void> {
    if (this.listenerInstallPromise) return this.listenerInstallPromise;
    if (this.listenerInstalled) return;
    this.listenerInstalled = true;
    this.listenerInstallPromise = (async () => {
      try {
        this.unlistenFn = await listen<PeaksProgressPayload>('peaks://progress', (e) => {
          const p = e.payload;
          if (p.state === 'done') {
            // Fresh extraction finished — drop any cached ranges for
            // this media so the next draw pass picks up real data.
            this.invalidate(p.media_id);
          }
        });
      } catch (err) {
        // Most likely we're outside a Tauri context (svelte-check,
        // unit host). Treat as best-effort — the cache still works for
        // direct calls, just without auto-invalidation on 'done'.
        this.listenerInstalled = false;
        this.unlistenFn = null;
        // eslint-disable-next-line no-console
        console.warn('peaks://progress listen failed:', err);
      } finally {
        this.listenerInstallPromise = null;
      }
    })();
    return this.listenerInstallPromise;
  }

  /** Register a media item's content-hash prefix. Required before
   *  `request()`. The prefix is the 24-char lowercase hex that names
   *  the on-disk slot (see `vance-app::peaks::slot_path`). */
  registerMedia(mediaId: string, contentHashPrefixHex: string): void {
    if (mediaId.includes(KEY_SEPARATOR)) {
      throw new Error(`mediaId cannot contain ${KEY_SEPARATOR}: ${mediaId}`);
    }
    const prev = this.prefixes.get(mediaId);
    if (prev === contentHashPrefixHex) return;
    if (prev !== undefined) {
      // Re-registered with a new prefix → invalidate the old slot's
      // cached entries so we don't read against the wrong file.
      this.invalidate(mediaId);
    }
    this.prefixes.set(mediaId, contentHashPrefixHex);
  }

  /** Forget that we ever knew about `mediaId` — drops both the prefix
   *  registration and any cached/in-flight data. Called by M6 from
   *  media-removal hooks so the prefixes map doesn't grow unboundedly
   *  across a long session. */
  unregisterMedia(mediaId: string): void {
    this.invalidate(mediaId);
    this.prefixes.delete(mediaId);
  }

  /** Read-only access to a media item's registered prefix, or `null`
   *  if M6 hasn't populated it yet. */
  getPrefix(mediaId: string): string | null {
    return this.prefixes.get(mediaId) ?? null;
  }

  /** Drop every cached entry for `mediaId`, clear pending failure
   *  cooldowns and in-flight references, bump the generation counter
   *  so any straggler `doFetch` resolution is discarded, and bump the
   *  reactive version. The prefix is NOT removed (use `unregisterMedia`
   *  for that). */
  invalidate(mediaId: string): void {
    this.generation++;
    let changed = false;
    const prefix = `${mediaId}${KEY_SEPARATOR}`;
    for (const [k, entry] of this.entries) {
      if (k.startsWith(prefix)) {
        this.totalBytes -= entry.views.byteLength;
        this.entries.delete(k);
        changed = true;
      }
    }
    for (const k of this.inflight.keys()) {
      if (k.startsWith(prefix)) this.inflight.delete(k);
    }
    for (const k of this.cooldownUntil.keys()) {
      if (k.startsWith(prefix)) this.cooldownUntil.delete(k);
    }
    if (this.meta.delete(mediaId)) changed = true;
    this.metaInflight.delete(mediaId);
    this.metaCooldownUntil.delete(mediaId);
    if (changed) this.bumpVersion();
  }

  /** Cached header + level directory for `mediaId`, or `null` if it
   *  hasn't been fetched yet (or the slot wasn't on disk last time we
   *  tried). Synchronous; does not trigger a fetch — use `requestMeta`
   *  for the lazy fetch behavior. */
  peekMeta(mediaId: string): PeaksMetaResponse | null {
    return this.meta.get(mediaId) ?? null;
  }

  /** Return cached metadata immediately if available; otherwise
   *  trigger a coalesced fetch and return `null`. Once the fetch
   *  lands, the cache is populated and the reactive version is bumped
   *  so the caller's `$effect` re-runs and sees the result. The
   *  fetch is a no-op if the media isn't registered. */
  requestMeta(mediaId: string): PeaksMetaResponse | null {
    const cached = this.meta.get(mediaId);
    if (cached) return cached;
    const prefix = this.prefixes.get(mediaId);
    if (!prefix) return null;
    if (this.metaInflight.has(mediaId)) return null;
    const coolUntil = this.metaCooldownUntil.get(mediaId);
    if (coolUntil !== undefined && Date.now() < coolUntil) return null;
    const startGen = this.generation;
    const p = (async (): Promise<PeaksMetaResponse | null> => {
      try {
        const m = await peaksMeta(prefix);
        if (this.generation !== startGen) return null;
        if (this.prefixes.get(mediaId) !== prefix) return null;
        if (m) {
          this.meta.set(mediaId, m);
          this.metaCooldownUntil.delete(mediaId);
          this.bumpVersion();
        } else {
          // Slot not on disk yet (or hash-mismatched). Park on
          // cooldown so a draw loop doesn't spam peaks_meta every
          // frame; the peaks://progress 'done' event invalidates
          // and clears the cooldown to retry immediately.
          this.metaCooldownUntil.set(mediaId, Date.now() + FAILURE_COOLDOWN_MS);
        }
        return m;
      } catch (err) {
        this.metaCooldownUntil.set(mediaId, Date.now() + FAILURE_COOLDOWN_MS);
        // eslint-disable-next-line no-console
        console.warn('peaks_meta failed:', err);
        return null;
      }
    })();
    this.metaInflight.set(mediaId, p);
    void p.finally(() => {
      if (this.metaInflight.get(mediaId) === p) {
        this.metaInflight.delete(mediaId);
      }
    });
    return null;
  }

  /** Return cached views for `(mediaId, level, [bucketStart, bucketStart + bucketCount))`,
   *  or `null` if not cached. Does NOT trigger a fetch and does NOT
   *  update LRU recency — that's `request`'s job. Safe to call from
   *  diagnostic / inspector paths without perturbing eviction order. */
  peek(mediaId: string, level: number, bucketStart: number, bucketCount: number): LaneViews | null {
    const key = this.makeKey(mediaId, level, bucketStart, bucketCount);
    const entry = this.entries.get(key);
    return entry ? entry.views : null;
  }

  /** Return cached views immediately if available; otherwise trigger
   *  a coalesced fetch and return `null`. Updates LRU recency on a
   *  hit. Once the fetch lands, the cache is populated and the
   *  reactive version is bumped so a re-run of the caller's
   *  `$effect` / `$derived` will see the result. */
  request(
    mediaId: string,
    level: number,
    bucketStart: number,
    bucketCount: number
  ): LaneViews | null {
    if (bucketCount <= 0) return null;
    const key = this.makeKey(mediaId, level, bucketStart, bucketCount);
    const entry = this.entries.get(key);
    if (entry) {
      entry.lastUsed = ++this.accessCounter;
      return entry.views;
    }

    const prefix = this.prefixes.get(mediaId);
    if (!prefix) {
      // Media isn't registered yet; M6 will call registerMedia.
      return null;
    }

    // Failure cooldown: a recent fetch on this exact key threw. Wait
    // before retrying so the draw loop doesn't spam the IPC.
    const coolUntil = this.cooldownUntil.get(key);
    if (coolUntil !== undefined && Date.now() < coolUntil) return null;

    if (!this.inflight.has(key)) {
      void this.ensureListener();
      const startGen = this.generation;
      const p = this.doFetch(mediaId, prefix, level, bucketStart, bucketCount, key, startGen);
      this.inflight.set(key, p);
      void p.finally(() => {
        // Only delete if still tracking THIS promise (a re-issued
        // fetch with the same key may have replaced it).
        if (this.inflight.get(key) === p) this.inflight.delete(key);
      });
    }
    return null;
  }

  private async doFetch(
    mediaId: string,
    prefix: string,
    level: number,
    bucketStart: number,
    bucketCount: number,
    key: string,
    startGen: number
  ): Promise<LaneViews | null> {
    try {
      const buf = await peaksForClip(prefix, level, bucketStart, bucketCount);
      // Generation guard: invalidate()/dispose() bumps generation; if
      // ours is now stale, the IPC result is for a slot we no longer
      // trust. Discard without caching.
      if (this.generation !== startGen) return null;
      if (buf === null) {
        // Not ready (4-byte sentinel). No entry stored; caller sees
        // null and re-requests on the next draw. Eventually the
        // peaks://progress 'done' event triggers invalidate-then-draw.
        return null;
      }
      // Prefix guard: a registerMedia() with a different prefix could
      // have raced in. Discard rather than alias the new slot's key
      // to old-slot bytes.
      if (this.prefixes.get(mediaId) !== prefix) return null;
      if (buf.byteLength !== bucketCount * BYTES_PER_BUCKET) {
        // The IPC binding enforces this; defensive guard. Treat as a
        // protocol violation, not a transient miss.
        this.markFailure(key);
        return null;
      }
      const views: LaneViews = {
        min: new Int16Array(buf, 0, bucketCount),
        max: new Int16Array(buf, bucketCount * 2, bucketCount),
        rms: new Uint16Array(buf, bucketCount * 4, bucketCount),
        byteLength: buf.byteLength
      };
      this.insertEntry(key, views);
      this.bumpVersion();
      return views;
    } catch (err) {
      // Backend rejected the request (e.g. invalid level or service
      // not initialized). Cooldown the key so a draw loop doesn't
      // burn CPU re-requesting the same broken combination every
      // frame, but don't bump version (no new data for consumers).
      this.markFailure(key);
      // eslint-disable-next-line no-console
      console.warn('peaks_for_clip failed:', err);
      return null;
    }
  }

  private markFailure(key: string): void {
    this.cooldownUntil.set(key, Date.now() + FAILURE_COOLDOWN_MS);
  }

  private insertEntry(key: string, views: LaneViews): void {
    // Replace any prior entry on the same key.
    const prev = this.entries.get(key);
    if (prev) this.totalBytes -= prev.views.byteLength;
    this.entries.set(key, {
      views,
      lastUsed: ++this.accessCounter,
      key
    });
    this.totalBytes += views.byteLength;
    // Clear the cooldown — we got real bytes for this key.
    this.cooldownUntil.delete(key);
    this.evictIfOverBudget();
  }

  private evictIfOverBudget(): void {
    if (this.totalBytes <= this.budgetBytes && this.entries.size <= MAX_ENTRY_COUNT) {
      return;
    }
    // Sort once, then evict from the front. Entry-count is hard-capped
    // (MAX_ENTRY_COUNT) so the sort cost is bounded.
    const sorted = Array.from(this.entries.values()).sort((a, b) => a.lastUsed - b.lastUsed);
    for (const e of sorted) {
      if (this.totalBytes <= this.budgetBytes && this.entries.size <= MAX_ENTRY_COUNT) break;
      this.totalBytes -= e.views.byteLength;
      this.entries.delete(e.key);
    }
  }

  /** Total bytes currently held in cached entries. */
  get bytesUsed(): number {
    return this.totalBytes;
  }

  /** Number of cached entries. */
  get entryCount(): number {
    return this.entries.size;
  }

  /** Number of media items currently registered. */
  get mediaCount(): number {
    return this.prefixes.size;
  }

  /** Drop ALL state (entries, prefixes, cooldowns, in-flight). Bumps
   *  the generation so any straggler in-flight fetches are discarded.
   *  Awaits any in-flight listener install before clearing so the
   *  resolved UnlistenFn is cancelled deterministically. Called by
   *  the project-bundle swap (M6) and on Vite HMR dispose so re-
   *  evaluation doesn't leak the old subscription. */
  async dispose(): Promise<void> {
    this.generation++;
    this.entries.clear();
    this.prefixes.clear();
    this.inflight.clear();
    this.cooldownUntil.clear();
    this.meta.clear();
    this.metaInflight.clear();
    this.metaCooldownUntil.clear();
    this.totalBytes = 0;
    if (this.listenerInstallPromise) {
      // ensureListener catches its own errors, so the awaited promise
      // never rejects — wrapping in try/catch was paranoia.
      await this.listenerInstallPromise;
    }
    if (this.unlistenFn) {
      try {
        await this.unlistenFn();
      } catch {
        // best-effort
      }
      this.unlistenFn = null;
    }
    this.listenerInstalled = false;
    this.bumpVersion();
  }

  private makeKey(
    mediaId: string,
    level: number,
    bucketStart: number,
    bucketCount: number
  ): string {
    return `${mediaId}${KEY_SEPARATOR}${level}${KEY_SEPARATOR}${bucketStart}${KEY_SEPARATOR}${bucketCount}`;
  }

  private bumpVersion(): void {
    // u32 wrap is fine — Svelte's reactivity only compares for
    // inequality, and a wraparound counter still changes.
    this.versionState = (this.versionState + 1) >>> 0;
  }
}

/** Singleton instance. Importing modules share state — this matches
 *  the rest of the codebase's rune pattern (`selection.svelte.ts` etc.). */
export const peaksCache = new PeaksCache();

// Vite HMR: dispose the old singleton's listener + entries before a
// fresh module instance takes over. Without this the old listener
// keeps firing into a detached cache instance after every save.
if (import.meta.hot) {
  import.meta.hot.dispose(() => {
    void peaksCache.dispose();
  });
}
