/**
 * Phase 7.5 M1 — reactive audio meters store.
 *
 * Polls the Rust mixer's metering snapshot at ~30Hz (cheap ArcSwap
 * load on the backend) and applies peak-hold + decay client-side so
 * the visualisation stays smooth between snapshots.
 *
 * The decay math lives here (NOT in Rust) so the hold time and the
 * decay rate can be tuned without bumping the backend.
 */

import { meterSnapshot, type MeterSnapshot, type TrackMeter } from './ipc';

/** Hold the peak indicator at its observed value for this many ms
 *  before it begins to decay. 1500ms is the industry-standard "peak
 *  hold" duration. */
const PEAK_HOLD_MS = 1500;

/** Decay rate once the hold expires, in dB per second. 20dB/s is the
 *  EBU R128 reference rate for digital meters. */
const PEAK_DECAY_DB_PER_SEC = 20;

/** RMS smoothing time-constant in ms — the moving exponential window
 *  applied to incoming RMS values to keep the bar from jittering. */
const RMS_SMOOTH_MS = 80;

/** Clip indicator latches as soon as a peak >= 1.0 (0 dBFS) is
 *  observed and clears after this many ms with no further clipping. */
const CLIP_LATCH_MS = 2000;

/** Poll period — keeps the bus warm without overshooting the backend's
 *  ~30Hz publish rate. 33ms ≈ 30 polls/sec. */
const POLL_MS = 33;

/** Linear amplitude in [0, ~1+] → dB float for display.
 *  Returns -Infinity for 0 input; the meter draw code clamps to its
 *  display floor. */
export function linToDb(amp: number): number {
  if (amp <= 0) return -Infinity;
  return 20 * Math.log10(amp);
}

/** Display dB → fractional fill [0, 1] inside the meter bar.
 *  Range is configurable per meter; defaults match the Vance audio
 *  display range [-60, +6] dB. */
export function dbToFill(db: number, dbMin = -60, dbMax = 6): number {
  if (!isFinite(db)) return 0;
  if (db <= dbMin) return 0;
  if (db >= dbMax) return 1;
  return (db - dbMin) / (dbMax - dbMin);
}

/** Per-channel display state — what the UI draws right now. The
 *  underlying instantaneous `peak` lives in linear amplitude; `hold_db`
 *  is the held-then-decayed peak in dB ready for display. */
type ChannelState = {
  /** Smoothed RMS in linear amplitude. */
  rms: number;
  /** Peak in dB after hold + decay. -Infinity == silent / no signal. */
  peak_db: number;
  /** Wall-clock ms (performance.now) at which the held peak began. */
  peak_hold_started_ms: number;
  /** Whether the clip LED is currently latched. */
  clipping: boolean;
  /** Wall-clock ms at which the most recent clip event was observed. */
  clip_latched_ms: number;
};

const emptyChannel = (): ChannelState => ({
  rms: 0,
  peak_db: -Infinity,
  peak_hold_started_ms: 0,
  clipping: false,
  clip_latched_ms: 0
});

/** Per-meter (track or master) display state — left + right channels. */
export type MeterDisplay = {
  l: ChannelState;
  r: ChannelState;
};

const emptyMeter = (): MeterDisplay => ({ l: emptyChannel(), r: emptyChannel() });

class MetersStore {
  /** Per-track display state keyed by track UUID. */
  tracks = $state<Record<string, MeterDisplay>>({});
  /** Master bus display state. */
  master = $state<MeterDisplay>(emptyMeter());
  /** Last seen publish generation. Stalled fold thread = generation
   *  stops advancing; the decay continues regardless. */
  lastGeneration = $state<number>(0);
  /** Phase 7.8 — BS.1770 loudness + true-peak readout for the master
   *  bus. -Infinity / 0 = "no measurement yet"; rendered as `--.- LU`. */
  loudness = $state<{
    lufs_m: number;
    lufs_s: number;
    lufs_i: number;
    true_peak_l_db: number;
    true_peak_r_db: number;
  }>({
    lufs_m: -Infinity,
    lufs_s: -Infinity,
    lufs_i: -Infinity,
    true_peak_l_db: -Infinity,
    true_peak_r_db: -Infinity
  });

  private pollHandle: number | null = null;
  private lastTickMs = 0;
  /** A snapshot request is pending: the 33 ms timer skips its tick rather
   *  than stack requests on a busy engine. */
  private inFlight = false;

  /** Begin polling. Idempotent — calling twice doesn't start two timers. */
  start() {
    if (this.pollHandle != null) return;
    this.lastTickMs = performance.now();
    this.pollHandle = window.setInterval(() => {
      if (this.inFlight) return;
      this.inFlight = true;
      void this.tick().finally(() => (this.inFlight = false));
    }, POLL_MS);
  }

  stop() {
    if (this.pollHandle != null) {
      window.clearInterval(this.pollHandle);
      this.pollHandle = null;
    }
    // Drain to silence so the bars fall to zero immediately when
    // playback stops. The decay path would eventually do this anyway
    // but an explicit reset avoids "frozen" bars in the no-playback
    // case.
    this.master = emptyMeter();
    this.tracks = {};
    this.lastGeneration = 0;
    this.loudness = {
      lufs_m: -Infinity,
      lufs_s: -Infinity,
      lufs_i: -Infinity,
      true_peak_l_db: -Infinity,
      true_peak_r_db: -Infinity
    };
  }

  private async tick() {
    const now = performance.now();
    const dtSec = Math.max(0, (now - this.lastTickMs) / 1000);
    this.lastTickMs = now;
    let snap: MeterSnapshot | null = null;
    try {
      snap = await meterSnapshot();
    } catch {
      snap = null;
    }
    if (snap == null) {
      // No mixer running — decay everything toward zero so the bars
      // fall instead of freezing at their last observed value.
      this.decayOnly(now, dtSec);
      return;
    }
    this.lastGeneration = snap.generation;
    // Phase 7.8 — copy the loudness readout verbatim. Sanitise NaN
    // (which Rust f32 NaN would serialise as `null` on the wire — but
    // the Rust side currently emits f32 numbers, including the
    // sentinel `-Infinity` for unmeasured values).
    if (snap.loudness) {
      this.loudness = {
        lufs_m: Number.isFinite(snap.loudness.lufs_m) ? snap.loudness.lufs_m : -Infinity,
        lufs_s: Number.isFinite(snap.loudness.lufs_s) ? snap.loudness.lufs_s : -Infinity,
        lufs_i: Number.isFinite(snap.loudness.lufs_i) ? snap.loudness.lufs_i : -Infinity,
        true_peak_l_db: Number.isFinite(snap.loudness.true_peak_l_db)
          ? snap.loudness.true_peak_l_db
          : -Infinity,
        true_peak_r_db: Number.isFinite(snap.loudness.true_peak_r_db)
          ? snap.loudness.true_peak_r_db
          : -Infinity
      };
    }
    this.ingest(snap, now, dtSec);
  }

  private ingest(snap: MeterSnapshot, now: number, dtSec: number) {
    this.master = applyFrame(this.master, snap.master, now, dtSec);
    // Phase 7.5 M1 adversarial review findings #6 + #16:
    // The Rust fold thread only inserts a track entry when at least
    // one audible clip contributed in the publish window. So
    // `snap.tracks` is naturally sparse — silent tracks vanish for
    // ~33 ms gaps. Replacing `this.tracks` from the snapshot alone
    // would snap an inert track's bar to zero and erase its held
    // peak + clip-latch state on every silent window.
    //
    // Carry forward tracks not in the snapshot through the same
    // decayChannel path the "no snapshot at all" branch uses, so the
    // visualisation stays smooth and held peaks decay rather than
    // snap.
    const fresh: Record<string, MeterDisplay> = {};
    for (const [id, prev] of Object.entries(this.tracks)) {
      const incoming = snap.tracks[id];
      fresh[id] = incoming ? applyFrame(prev, incoming, now, dtSec) : decayMeter(prev, now, dtSec);
    }
    // Pick up tracks that JUST appeared in this snapshot.
    for (const [id, m] of Object.entries(snap.tracks)) {
      if (!(id in fresh)) {
        fresh[id] = applyFrame(emptyMeter(), m, now, dtSec);
      }
    }
    this.tracks = fresh;
  }

  private decayOnly(now: number, dtSec: number) {
    this.master = decayMeter(this.master, now, dtSec);
    const next: Record<string, MeterDisplay> = {};
    for (const [id, m] of Object.entries(this.tracks)) {
      next[id] = decayMeter(m, now, dtSec);
    }
    this.tracks = next;
  }
}

/** Sanitise an incoming raw amplitude. NaN / -Inf / Inf would
 *  permanently poison the RMS / peak state otherwise (finding #8). */
function safeAmp(v: number): number {
  return Number.isFinite(v) ? Math.max(0, v) : 0;
}

/** Apply one snapshot frame to a channel: rises immediately to the new
 *  peak, otherwise decays toward zero; RMS is exponentially smoothed. */
function applyChannel(
  prev: ChannelState,
  newPeak: number,
  newRms: number,
  now: number,
  dtSec: number
): ChannelState {
  const sanePeak = safeAmp(newPeak);
  const saneRms = safeAmp(newRms);
  const newPeakDb = linToDb(sanePeak);
  // Rise immediately on a louder peak; otherwise hold then decay.
  let peak_db = prev.peak_db;
  let peak_hold_started_ms = prev.peak_hold_started_ms;
  if (newPeakDb > peak_db) {
    peak_db = newPeakDb;
    peak_hold_started_ms = now;
  } else if (now - peak_hold_started_ms > PEAK_HOLD_MS) {
    peak_db -= PEAK_DECAY_DB_PER_SEC * dtSec;
    if (peak_db < -120) peak_db = -Infinity;
  }
  // RMS smoothing: y = (1 - α) * y_prev + α * y_new with α tied to dt.
  const alpha = Math.min(1, dtSec / (RMS_SMOOTH_MS / 1000));
  const rms = prev.rms * (1 - alpha) + saneRms * alpha;
  // Clip latch: any sample >= 0 dBFS (== 1.0 amplitude) trips the LED.
  let clipping = prev.clipping;
  let clip_latched_ms = prev.clip_latched_ms;
  if (sanePeak >= 1.0) {
    clipping = true;
    clip_latched_ms = now;
  } else if (clipping && now - clip_latched_ms > CLIP_LATCH_MS) {
    clipping = false;
  }
  return { rms, peak_db, peak_hold_started_ms, clipping, clip_latched_ms };
}

function applyFrame(
  prev: MeterDisplay,
  next: TrackMeter,
  now: number,
  dtSec: number
): MeterDisplay {
  return {
    l: applyChannel(prev.l, next.peak_l, next.rms_l, now, dtSec),
    r: applyChannel(prev.r, next.peak_r, next.rms_r, now, dtSec)
  };
}

function decayChannel(prev: ChannelState, now: number, dtSec: number): ChannelState {
  let peak_db = prev.peak_db;
  if (now - prev.peak_hold_started_ms > PEAK_HOLD_MS) {
    peak_db -= PEAK_DECAY_DB_PER_SEC * dtSec;
    if (peak_db < -120) peak_db = -Infinity;
  }
  const alpha = Math.min(1, dtSec / (RMS_SMOOTH_MS / 1000));
  const rms = prev.rms * (1 - alpha);
  let clipping = prev.clipping;
  if (clipping && now - prev.clip_latched_ms > CLIP_LATCH_MS) clipping = false;
  return { ...prev, rms, peak_db, clipping };
}

function decayMeter(prev: MeterDisplay, now: number, dtSec: number): MeterDisplay {
  return {
    l: decayChannel(prev.l, now, dtSec),
    r: decayChannel(prev.r, now, dtSec)
  };
}

export const metersStore = new MetersStore();
