/**
 * Phase 7.9 — UI-side audio scrub session manager.
 *
 * Handles the rhythm of "fire a burst → schedule a stop" so the
 * Timeline component doesn't have to track timers across drag events.
 * Throttles rapid drag events to the device-frame rate (one burst per
 * frame at most) — anything faster overwhelms the sidecar startup
 * cost without an audible benefit.
 *
 * Cold-start latency on the FIRST burst is bounded by the audio
 * sidecar spawn (~100 ms). Subsequent bursts within the M4 scheduler's
 * PRELOAD window (2 s) reuse warm sidecars.
 */

import { scrubBurst, stopPlayback } from './ipc';

/** Audio device sample rate the backend assumes for `scrub_burst`'s
 *  `from_pts`. The engine's mixer uses `(1, sample_rate)` as its time
 *  base; we mirror 48 kHz here as a stable convention. */
const DEVICE_SAMPLE_RATE = 48_000;

/** How long each burst plays before the UI fires the auto-stop. */
const BURST_DURATION_MS = 120;

/** Minimum gap between consecutive bursts. Below this we coalesce
 *  drag events. */
const MIN_BURST_INTERVAL_MS = 32;

class ScrubSession {
  private active = false;
  private lastBurstAt = 0;
  private pendingStopId: number | null = null;
  private pendingBurstAt: number | null = null;
  private pendingBurstTimer: number | null = null;

  /** Mark the session as active. Subsequent `at(timelineSeconds)`
   *  calls will emit bursts. `end()` clears it. */
  begin() {
    this.active = true;
  }

  /** Burst-at-time. Coalesces if called more than once per
   *  `MIN_BURST_INTERVAL_MS`. */
  async at(timelineSeconds: number) {
    if (!this.active) return;
    const now = performance.now();
    if (now - this.lastBurstAt < MIN_BURST_INTERVAL_MS) {
      // Coalesce: remember the latest position and let the trailing
      // timer fire one burst.
      this.pendingBurstAt = timelineSeconds;
      if (this.pendingBurstTimer == null) {
        const delay = MIN_BURST_INTERVAL_MS - (now - this.lastBurstAt);
        this.pendingBurstTimer = window.setTimeout(() => {
          this.pendingBurstTimer = null;
          const t = this.pendingBurstAt;
          this.pendingBurstAt = null;
          if (t != null) void this.fire(t);
        }, delay);
      }
      return;
    }
    await this.fire(timelineSeconds);
  }

  private async fire(timelineSeconds: number) {
    this.lastBurstAt = performance.now();
    const fromPts = Math.round(timelineSeconds * DEVICE_SAMPLE_RATE);
    // Cancel any pending auto-stop from the prior burst; the engine's
    // `scrub_burst` already stops the previous one server-side, but
    // we still want to cancel OUR pending stop so it doesn't fire
    // after the new burst has been scheduled.
    if (this.pendingStopId != null) {
      window.clearTimeout(this.pendingStopId);
      this.pendingStopId = null;
    }
    try {
      await scrubBurst(fromPts);
    } catch {
      // The engine returns errors when no sequence is loaded; swallow.
      return;
    }
    this.pendingStopId = window.setTimeout(() => {
      this.pendingStopId = null;
      void stopPlayback().catch(() => {});
    }, BURST_DURATION_MS);
  }

  /** End the session. Stops any in-flight burst and clears pending
   *  timers. */
  async end() {
    this.active = false;
    if (this.pendingStopId != null) {
      window.clearTimeout(this.pendingStopId);
      this.pendingStopId = null;
    }
    if (this.pendingBurstTimer != null) {
      window.clearTimeout(this.pendingBurstTimer);
      this.pendingBurstTimer = null;
    }
    this.pendingBurstAt = null;
    try {
      await stopPlayback();
    } catch {
      /* swallow */
    }
  }
}

export const scrubSession = new ScrubSession();
