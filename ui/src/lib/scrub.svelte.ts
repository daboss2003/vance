/**
 * UI-side audio scrub session manager.
 *
 * Sample-accurate scrubbing (Phase 7): on drag-start we open a persistent
 * scrub session (warm decoder + output stream); each drag move renders a
 * short grain of the mixed audio AT the playhead and plays it, replacing
 * the previous grain so the audio follows the cursor. On release we tear
 * the session down.
 *
 * This replaces the old "burst playback" approach, which restarted the
 * whole mixer session per ~120 ms burst (heavy, not sample-accurate, and
 * played forward instead of at the playhead).
 *
 * Rapid drag events are coalesced to at most one grain per
 * `MIN_GRAIN_INTERVAL_MS` — grains are ~80 ms, so firing faster than that
 * just wastes work without an audible benefit.
 */

import { scrubBegin, scrubGrain, scrubEnd } from './ipc';

/** Minimum gap between consecutive grains. Below this we coalesce drag
 *  events and let a trailing timer fire one grain at the latest position. */
const MIN_GRAIN_INTERVAL_MS = 40;

class ScrubSession {
  private active = false;
  private lastGrainAt = 0;
  private pendingAt: number | null = null;
  private pendingTimer: number | null = null;

  /** Begin a session (warms the decoder + output). Subsequent
   *  `at(timelineSeconds)` calls emit grains; `end()` clears it. */
  begin() {
    if (this.active) return;
    this.active = true;
    // Fire-and-forget: warming can happen while the first grains queue.
    void scrubBegin().catch(() => {});
  }

  /** Play a grain at `timelineSeconds`. Coalesces if called more than
   *  once per `MIN_GRAIN_INTERVAL_MS`. */
  async at(timelineSeconds: number) {
    if (!this.active) return;
    const now = performance.now();
    if (now - this.lastGrainAt < MIN_GRAIN_INTERVAL_MS) {
      this.pendingAt = timelineSeconds;
      if (this.pendingTimer == null) {
        const delay = MIN_GRAIN_INTERVAL_MS - (now - this.lastGrainAt);
        this.pendingTimer = window.setTimeout(() => {
          this.pendingTimer = null;
          const t = this.pendingAt;
          this.pendingAt = null;
          if (t != null) void this.fire(t);
        }, delay);
      }
      return;
    }
    await this.fire(timelineSeconds);
  }

  private async fire(timelineSeconds: number) {
    this.lastGrainAt = performance.now();
    try {
      await scrubGrain(Math.max(0, timelineSeconds));
    } catch {
      // No sequence loaded / device busy — swallow.
    }
  }

  /** End the session: cancel pending grains and tear down the backend. */
  async end() {
    if (!this.active && this.pendingTimer == null) return;
    this.active = false;
    if (this.pendingTimer != null) {
      window.clearTimeout(this.pendingTimer);
      this.pendingTimer = null;
    }
    this.pendingAt = null;
    try {
      await scrubEnd();
    } catch {
      /* swallow */
    }
  }
}

export const scrubSession = new ScrubSession();
