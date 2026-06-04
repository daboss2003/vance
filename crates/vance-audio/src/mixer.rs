//! Multi-source audio mixer with per-track and per-clip mute / solo / gain.
//!
//! Phase 7.3. Sits alongside [`crate::AudioPlayer`] (which stays the
//! single-file fast path used by the peaks pipeline and any sidecar
//! tooling that needs a one-shot stream). The mixer adds:
//!
//! - A vector of [`AudioSource`]s, each backed by its own
//!   [`crate::AudioClient`] + puller thread + lock-free ring.
//! - A dedicated **fold thread** that reads one stereo frame from each
//!   per-source ring per output frame, applies the mixer snapshot's
//!   effective gain (clip × track × master, gated by mute + solo + the
//!   sequence-wide `solo_anywhere` flag), accumulates into a stereo sum,
//!   and pushes the result into the master ring.
//! - The cpal callback (unchanged shape from `AudioPlayer`) pops from
//!   the master ring and silences underruns.
//! - A lock-free snapshot bus via `ArcSwap<MixerSnapshot>` so the engine
//!   (running on the edit-log thread) can publish new mixer state when
//!   an `EditOp::SetTrack* / SetClip*` lands, with zero contention on
//!   the fold thread.
//!
//! Identifiers crossing the vance-audio boundary are plain
//! [`uuid::Uuid`]s — vance-audio deliberately does not depend on
//! vance-timeline. The engine translates `TrackId::as_uuid()` /
//! `ClipId::as_uuid()` at the call boundary.
//!
//! ## Test surface
//!
//! [`fold_frame`] is the per-frame mix math, factored out of the fold
//! thread so it can be exercised without a real cpal device. The unit
//! tests below construct synthetic input frames, apply a snapshot, and
//! assert the output amplitude matches the documented mute / solo /
//! gain semantics exactly.

#![allow(missing_docs)]

use crate::client::{AudioClient, AudioClientError};
use arc_swap::ArcSwap;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use thiserror::Error;
use uuid::Uuid;

/// Convert decibels to linear amplitude gain. NaN/Infinity inputs return
/// 0.0 (silence) so a bad snapshot value can never amplify the bus.
#[inline]
pub fn db_to_linear(db: f32) -> f32 {
    if db.is_finite() {
        10f32.powf(db / 20.0)
    } else {
        0.0
    }
}

/// Shape of a clip fade curve (Phase 7.4 M3). Mirrors
/// `vance-timeline::FadeCurve` so vance-audio stays decoupled from the
/// timeline crate. Only [`FadeCurve::Linear`] is implemented in 7.4 —
/// the other variants exist so 7.5 can ship the missing shapes without
/// growing the snapshot ABI. Until then they fall back to linear
/// (chosen over silence so a 7.5-authored project remains audible
/// when opened on a 7.4 build).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FadeCurve {
    /// Linear amplitude ramp. Default for forward-compat.
    #[default]
    Linear,
    /// −3 dB equal-power shape. Reserved for 7.5; treated as `Linear` today.
    EqualPower,
    /// Audio-taper / logarithmic shape. Reserved for 7.5; treated as `Linear` today.
    Logarithmic,
}

/// Evaluate the multiplicative fade-in gain at clip-relative time
/// `t_clip_seconds`. Returns 1.0 once `t >= duration_seconds`.
/// `duration_seconds <= 0` returns 1.0 (the "no fade" case). Linear
/// shape only in 7.4; other curves fall back to linear so they don't
/// silently mute. Defense-in-depth duplicate of
/// `vance-timeline::eval_fade_in` — both sides ship the same fallback
/// behaviour and the audio crate stays decoupled from the timeline
/// crate's symbol.
#[inline]
pub fn eval_fade_in_audio(duration_seconds: f64, _curve: FadeCurve, t_clip_seconds: f64) -> f32 {
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return 1.0;
    }
    if t_clip_seconds <= 0.0 {
        return 0.0;
    }
    if t_clip_seconds >= duration_seconds {
        return 1.0;
    }
    (t_clip_seconds / duration_seconds) as f32
}

/// Evaluate the multiplicative fade-out gain at clip-relative time
/// `t_clip_seconds`. `clip_duration_seconds` is the clip's total
/// length. Returns 1.0 before the fade region. Linear shape only in
/// 7.4.
#[inline]
pub fn eval_fade_out_audio(
    duration_seconds: f64,
    _curve: FadeCurve,
    clip_duration_seconds: f64,
    t_clip_seconds: f64,
) -> f32 {
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return 1.0;
    }
    let fade_start = clip_duration_seconds - duration_seconds;
    if t_clip_seconds <= fade_start {
        return 1.0;
    }
    if t_clip_seconds >= clip_duration_seconds {
        return 0.0;
    }
    ((clip_duration_seconds - t_clip_seconds) / duration_seconds) as f32
}

/// Outgoing interpolation between consecutive [`GainKeyframe`]s.
/// Mirrors `vance-timeline::KeyframeInterp` so the audio crate stays
/// decoupled from the timeline crate.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum GainInterp {
    /// Straight line between this keyframe and the next.
    #[default]
    Linear,
    /// Smoothstep cubic (3t² − 2t³) easing.
    Bezier,
    /// Snap: hold this keyframe's value until the next.
    Hold,
}

/// One gain-automation control point. `gain_linear` is precomputed
/// from the vance-timeline dB keyframe at snapshot build time so the
/// fold thread never invokes `powf`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GainKeyframe {
    /// Time in seconds. For [`TrackMix.gain_lane`] this is
    /// timeline-seconds (sequence origin). For [`ClipMix.gain_lane`]
    /// it's CLIP-RELATIVE (`t = 0` is the clip's `timeline_in`).
    pub time_seconds: f64,
    /// Pre-converted linear gain. The vance-timeline keyframe carried
    /// dB; the snapshot build pass does `db_to_linear` once per
    /// keyframe so the audio hot path stays free of `powf`.
    pub gain_linear: f32,
    /// Outgoing interpolation toward the next keyframe.
    pub interp: GainInterp,
}

/// Evaluate a gain lane at time `t_seconds`. **Caller MUST handle the
/// empty case OUTSIDE this function.** An empty slice returns the
/// neutral 1.0 (mirrors the "no automation" intent) — different from
/// `vance-timeline::eval_keyframes(&[]) = 0.0`. This is the
/// load-bearing footgun the Phase 7.4 spike flagged: a sanitizer that
/// silently re-runs evaluation against an empty lane would silence the
/// bus. We keep the safe default here AND check `is_empty()` at the
/// snapshot build site (defense in depth).
pub fn eval_gain_lane(lane: &[GainKeyframe], t_seconds: f64) -> f32 {
    if lane.is_empty() {
        return 1.0;
    }
    if lane.len() == 1 || t_seconds <= lane[0].time_seconds {
        return lane[0].gain_linear;
    }
    let last = &lane[lane.len() - 1];
    if t_seconds >= last.time_seconds {
        return last.gain_linear;
    }
    // Binary search for the bracketing segment.
    let mut lo = 0usize;
    let mut hi = lane.len() - 1;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if lane[mid].time_seconds <= t_seconds {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let a = &lane[lo];
    let b = &lane[hi];
    let span = b.time_seconds - a.time_seconds;
    if span <= 0.0 {
        return a.gain_linear;
    }
    let frac = ((t_seconds - a.time_seconds) / span) as f32;
    match a.interp {
        GainInterp::Linear => a.gain_linear + (b.gain_linear - a.gain_linear) * frac,
        GainInterp::Hold => a.gain_linear,
        GainInterp::Bezier => {
            let s = frac * frac * (3.0 - 2.0 * frac); // smoothstep
            a.gain_linear + (b.gain_linear - a.gain_linear) * s
        }
    }
}

/// Per-track mixer state inside a [`MixerSnapshot`]. `gain_linear` is
/// precomputed via [`db_to_linear`] at snapshot build time so the fold
/// thread never touches a `powf` in the audio hot path. When
/// `gain_lane` is non-empty it takes precedence over `gain_linear`
/// (Phase 7.4 automation).
#[derive(Clone, Debug, PartialEq)]
pub struct TrackMix {
    /// Multiplicative track gain, linear. Used when `gain_lane` is
    /// empty (the "no automation" case).
    pub gain_linear: f32,
    /// Phase 7.4: time-varying gain lane in timeline-seconds. Empty =
    /// use `gain_linear`. Non-empty entries are precomputed (dB →
    /// linear) at snapshot build time.
    pub gain_lane: Vec<GainKeyframe>,
    /// Mixer mute. Silences this track's contribution regardless of
    /// solo state.
    pub mute: bool,
    /// Solo flag. If ANY track in the snapshot has `solo == true`,
    /// only soloed tracks contribute.
    pub solo: bool,
}

/// Per-clip mixer state inside a [`MixerSnapshot`]. The fold thread
/// keys lookups by clip uuid; `track_id` is the parent track so the
/// effective-gain composition can dereference both in one pass.
/// `timeline_in_seconds` lets the fold thread map timeline-time → clip-
/// relative-time for the per-clip gain lane.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipMix {
    /// Multiplicative per-clip gain, linear. Used when `gain_lane` is
    /// empty.
    pub gain_linear: f32,
    /// Phase 7.4: time-varying gain lane in CLIP-RELATIVE seconds.
    /// Empty = use `gain_linear`.
    pub gain_lane: Vec<GainKeyframe>,
    /// Per-clip mute (`!Clip.enabled`). False = audible.
    pub enabled: bool,
    /// Parent track uuid for the fold-time track lookup.
    pub track_id: Uuid,
    /// Clip's `timeline_in` in seconds. The fold thread subtracts this
    /// from `t_timeline` to get clip-relative time for the gain lane.
    pub timeline_in_seconds: f64,
    /// Clip's total duration in seconds. Used by the fade-out evaluator
    /// to locate the fade region; populated even when no fade is set
    /// so a future fade EditOp can land without re-publishing.
    pub clip_duration_seconds: f64,
    /// Phase 7.4 M3: fade-in duration in CLIP-RELATIVE seconds.
    /// `0.0` = no fade-in (the fade is skipped at evaluation time).
    pub fade_in_duration_seconds: f64,
    /// Fade-in curve shape. Only `Linear` is honoured in 7.4; the other
    /// variants are present so 7.5 can ship `EqualPower` / `Logarithmic`
    /// without changing the snapshot ABI.
    pub fade_in_curve: FadeCurve,
    /// Phase 7.4 M3: fade-out duration in CLIP-RELATIVE seconds. The
    /// fade region runs from `clip_duration_seconds - fade_out_duration`
    /// to the clip's end. `0.0` = no fade-out.
    pub fade_out_duration_seconds: f64,
    /// Fade-out curve shape. See [`Self::fade_in_curve`].
    pub fade_out_curve: FadeCurve,
}

/// Snapshot of the entire mixer state. Published by the engine to the
/// fold thread via `ArcSwap`. Cheap to clone (every field is `Copy`
/// inside `HashMap`/`bool`/`f32`).
#[derive(Clone, Debug, Default)]
pub struct MixerSnapshot {
    /// Per-track mix params, keyed by track uuid.
    pub tracks: HashMap<Uuid, TrackMix>,
    /// Per-clip mix params, keyed by clip uuid.
    pub clips: HashMap<Uuid, ClipMix>,
    /// True iff at least one track has `solo == true`. Precomputed at
    /// snapshot build time so the fold thread doesn't sweep the tracks
    /// map per frame.
    pub solo_anywhere: bool,
    /// Master bus gain (linear). 1.0 = unity. Phase 7.4 derives this
    /// from `Sequence.master_gain_db` at snapshot build time (see
    /// `vance_app::engine::build_mixer_snapshot`); the master fader UI
    /// drives it via `EditOp::SetMasterGainDb`.
    pub master_gain_linear: f32,
}

impl MixerSnapshot {
    /// Construct an empty snapshot. All sources fold to silence.
    pub fn empty() -> Self {
        Self {
            tracks: HashMap::new(),
            clips: HashMap::new(),
            solo_anywhere: false,
            master_gain_linear: 1.0,
        }
    }

    /// Effective linear gain for `clip_id`'s contribution to the output
    /// sum. Returns 0.0 when:
    ///
    /// - the clip isn't in the snapshot (orphan source),
    /// - the clip's `enabled` is false (per-clip mute),
    /// - the parent track isn't in the snapshot,
    /// - the parent track's `mute` is true, OR
    /// - `solo_anywhere` is true and the parent track's `solo` is false.
    ///
    /// Otherwise: `clip.gain_linear × track.gain_linear × master_gain_linear`.
    ///
    /// Phase 7.4 callers should prefer [`Self::effective_gain_at`]
    /// which honours per-track / per-clip automation lanes. This
    /// constant-only path stays for callers that genuinely don't have
    /// a timeline time (e.g. legacy tests).
    #[inline]
    pub fn effective_gain(&self, clip_id: &Uuid) -> f32 {
        self.effective_gain_at(clip_id, f64::NEG_INFINITY)
    }

    /// Effective linear gain for `clip_id` at `t_timeline_seconds`.
    /// Same gating rules as [`Self::effective_gain`]; additionally,
    /// non-empty per-clip / per-track lanes are evaluated via
    /// [`eval_gain_lane`] in their respective time-frames:
    ///
    /// - The track lane lives in timeline-seconds (its keyframes are
    ///   sequence-relative).
    /// - The clip lane lives in CLIP-RELATIVE seconds, computed as
    ///   `t_timeline_seconds - clip.timeline_in_seconds`.
    ///
    /// Empty lanes fall back to the constant `gain_linear` — that's
    /// the load-bearing invariant the spike's "eval_curve(&[]) = 0.0"
    /// warning was about, structurally avoided by the `is_empty()`
    /// branch.
    pub fn effective_gain_at(&self, clip_id: &Uuid, t_timeline_seconds: f64) -> f32 {
        let Some(clip) = self.clips.get(clip_id) else {
            return 0.0;
        };
        if !clip.enabled {
            return 0.0;
        }
        let Some(track) = self.tracks.get(&clip.track_id) else {
            return 0.0;
        };
        if track.mute {
            return 0.0;
        }
        if self.solo_anywhere && !track.solo {
            return 0.0;
        }
        let track_g = if track.gain_lane.is_empty() {
            track.gain_linear
        } else {
            eval_gain_lane(&track.gain_lane, t_timeline_seconds)
        };
        let t_clip = t_timeline_seconds - clip.timeline_in_seconds;
        let clip_g = if clip.gain_lane.is_empty() {
            clip.gain_linear
        } else {
            eval_gain_lane(&clip.gain_lane, t_clip)
        };
        // Phase 7.4 M3: compose fade ramps multiplicatively. Fade is
        // independent of the keyframe lane by design — setting a fade
        // never erases automation, and editing the lane never disturbs
        // the fade region. Both eval functions return 1.0 when their
        // duration is 0.0 so unset fades cost a single compare.
        let fade_in_g = eval_fade_in_audio(
            clip.fade_in_duration_seconds,
            clip.fade_in_curve,
            t_clip,
        );
        let fade_out_g = eval_fade_out_audio(
            clip.fade_out_duration_seconds,
            clip.fade_out_curve,
            clip.clip_duration_seconds,
            t_clip,
        );
        track_g * clip_g * fade_in_g * fade_out_g * self.master_gain_linear
    }
}

/// One source feeding the mixer: an audio sidecar instance opened
/// against a single media file. The engine spawns one [`AudioSource`]
/// per active audio clip in the current playback window.
#[derive(Clone, Debug)]
pub struct AudioSource {
    /// Clip uuid the engine assigns. Used by the snapshot to look up
    /// per-clip gain and the parent track's mix state.
    pub clip_id: Uuid,
    /// Absolute path of the media file.
    pub path: PathBuf,
    /// Playhead-relative start offset in seconds. Forwarded to the
    /// sidecar's `start_stream` so the puller delivers samples
    /// starting at the right source-time position.
    pub from_seconds: f64,
}

/// Pure per-frame mixer fold at a specific timeline time. Given each
/// source's current stereo frame as `(clip_uuid, [L, R])` and the
/// playhead's timeline-seconds, applies the snapshot's
/// keyframe-aware effective gain to each contribution and returns the
/// stereo sum.
///
/// Extracted from the fold thread so it can be unit-tested without a
/// real cpal device. The fold thread loops this function once per
/// output frame, advancing `t_timeline_seconds` by `1 / sample_rate`
/// per call.
#[inline]
pub fn fold_frame_at(
    snapshot: &MixerSnapshot,
    source_samples: &[(Uuid, [f32; 2])],
    t_timeline_seconds: f64,
) -> [f32; 2] {
    let mut out = [0.0f32; 2];
    for (clip_id, [l, r]) in source_samples {
        let g = snapshot.effective_gain_at(clip_id, t_timeline_seconds);
        if g == 0.0 {
            continue;
        }
        out[0] += l * g;
        out[1] += r * g;
    }
    out
}

/// Back-compat shim for the Phase 7.3 fold call. Equivalent to
/// `fold_frame_at(snapshot, source_samples, f64::NEG_INFINITY)`.
///
/// IMPORTANT (Phase 7.4 M3): with `t = -∞`, the clip-relative time
/// `t_clip = -∞ - timeline_in_seconds = -∞`, which makes
/// `eval_fade_in_audio` return `0.0` for ANY clip with a non-zero
/// `fade_in_duration_seconds`. So this shim silently zeroes faded
/// clips — it only stays correct on clips without a fade-in. Used by
/// the legacy mute/solo/gain unit tests (which pre-date fades); new
/// tests should call `fold_frame_at` with a finite time.
#[inline]
pub fn fold_frame(snapshot: &MixerSnapshot, source_samples: &[(Uuid, [f32; 2])]) -> [f32; 2] {
    fold_frame_at(snapshot, source_samples, f64::NEG_INFINITY)
}

/// Mixer errors. Variants surface from sidecar spawn failure, cpal
/// device misbehavior, and the puller / fold thread spawn paths.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum AudioMixerError {
    /// No default audio output device.
    #[error("no default audio output device")]
    NoDevice,
    /// cpal couldn't build the output stream.
    #[error("cpal build_output_stream: {0}")]
    Build(String),
    /// cpal couldn't start the stream.
    #[error("cpal play: {0}")]
    Play(String),
    /// Sidecar / IPC error against one of the source clients.
    #[error("audio client: {0}")]
    Client(#[from] AudioClientError),
    /// Couldn't spawn a worker thread.
    #[error("spawn thread {kind}: {err}")]
    Spawn {
        /// Which thread failed (puller / fold).
        kind: &'static str,
        /// OS error.
        err: String,
    },
}

/// Multi-source audio playback. Owns the cpal output stream, one
/// puller thread per source, the fold thread, and the live mixer
/// snapshot bus.
///
/// Drop stops the cpal stream and joins every worker thread.
pub struct AudioMixer {
    /// Inner state behind a Mutex<Option<…>> so Drop can `.take()`
    /// + signal shutdown idempotently.
    inner: Mutex<Option<Inner>>,
    /// Master output PTS authority (Phase 7.3 clock-authority swap in
    /// M3 retargets `AudioSync.current_pts` here). Counts in
    /// device-sample frames since stream start.
    master_frame: Arc<AtomicI64>,
    /// Cpal output sample rate (Hz).
    device_sample_rate: u32,
    /// Snapshot bus. Engine publishes new state via `store()`; the
    /// fold thread loads it via `load_full()` once per buffer (NOT once
    /// per frame — re-loading per frame is wasteful and would defeat
    /// ArcSwap's whole point).
    snapshot: Arc<ArcSwap<MixerSnapshot>>,
}

struct Inner {
    _stream: cpal::Stream,
    stop: Arc<AtomicBool>,
    fold_handle: Option<JoinHandle<()>>,
}

impl AudioMixer {
    /// Spawn the cpal output stream and the fold thread. Returns once
    /// cpal is playing.
    ///
    /// Phase 7.4 M4 contract:
    /// - `initial_sources` is the set of **pre-spawned** SourceStreams
    ///   the caller wants the fold thread to start mixing from. The
    ///   caller is expected to have already done the synchronous
    ///   sidecar handshake via [`spawn_source`] — `AudioMixer::start`
    ///   does no IPC of its own — so cpal starts producing immediately.
    /// - `command_rx` is the scheduler's channel — `Add(SourceStream)`
    ///   delivers a pre-spawned source to the fold thread, `Remove`
    ///   takes one out. Both are O(1) HashMap ops with no IPC.
    ///
    /// `initial_snapshot` seeds the fold thread; the engine subsequently
    /// calls `publish_snapshot` after every mixer-affecting `EditOp`.
    ///
    /// Observability ([`MixerStats`]) lives on the engine side because
    /// every counter is written from the **scheduler** thread (or the
    /// caller of [`spawn_source`] for the initial seed) — never inside
    /// the mixer itself.
    pub fn start(
        initial_sources: Vec<SourceStream>,
        command_rx: mpsc::Receiver<SourceCommand>,
        initial_snapshot: MixerSnapshot,
        playback_start_timeline_seconds: f64,
    ) -> Result<Self, AudioMixerError> {
        // 1. Cpal output device — same shape as AudioPlayer.
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(AudioMixerError::NoDevice)?;
        let config = device
            .default_output_config()
            .map_err(|e| AudioMixerError::Build(e.to_string()))?;
        let sample_rate = config.sample_rate().0;
        let channels = config.channels() as usize;
        tracing::info!(
            sample_rate,
            channels,
            n_initial = initial_sources.len(),
            "mixer: opened cpal"
        );

        // 2. Master ring sized for ~250 ms at device rate.
        let master_capacity = (sample_rate as usize) * channels / 4;
        let master_rb = HeapRb::<f32>::new(master_capacity);
        let (master_prod, mut master_cons) = master_rb.split();

        // 3. Snapshot bus + master frame counter.
        let snapshot = Arc::new(ArcSwap::from_pointee(initial_snapshot));
        let master_frame = Arc::new(AtomicI64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let source_streams = initial_sources;

        // 5. Cpal output stream — identical to AudioPlayer's pattern
        //    (pop_slice + silence fill). The mixer fold thread is the
        //    sole writer; cpal callback is the sole reader.
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => device
                .build_output_stream(
                    &config.into(),
                    move |out: &mut [f32], _info| {
                        let pulled = master_cons.pop_slice(out);
                        for s in &mut out[pulled..] {
                            *s = 0.0;
                        }
                        if pulled < out.len() {
                            tracing::trace!(
                                requested = out.len(),
                                got = pulled,
                                "mixer underrun"
                            );
                        }
                    },
                    |err| tracing::error!(?err, "mixer cpal stream error"),
                    None,
                )
                .map_err(|e| AudioMixerError::Build(e.to_string()))?,
            other => {
                return Err(AudioMixerError::Build(format!(
                    "unsupported sample format {other:?} (expected f32)"
                )));
            }
        };
        stream
            .play()
            .map_err(|e| AudioMixerError::Play(e.to_string()))?;

        // 6. Fold thread — owns the source HashMap and the command
        //    receiver. Every state-change is a HashMap op, never IPC.
        let fold_stop = stop.clone();
        let fold_snapshot = snapshot.clone();
        let fold_master_frame = master_frame.clone();
        let fold_handle = thread::Builder::new()
            .name("vance-audio.mixer.fold".into())
            .spawn(move || {
                fold_loop(
                    source_streams,
                    command_rx,
                    master_prod,
                    fold_snapshot,
                    fold_master_frame,
                    fold_stop,
                    channels,
                    sample_rate,
                    playback_start_timeline_seconds,
                );
            })
            .map_err(|e| AudioMixerError::Spawn {
                kind: "fold",
                err: e.to_string(),
            })?;

        Ok(Self {
            inner: Mutex::new(Some(Inner {
                _stream: stream,
                stop,
                fold_handle: Some(fold_handle),
            })),
            master_frame,
            device_sample_rate: sample_rate,
            snapshot,
        })
    }

    /// Publish a new mixer snapshot. Lock-free, cheap.
    pub fn publish_snapshot(&self, snapshot: MixerSnapshot) {
        self.snapshot.store(Arc::new(snapshot));
    }

    /// Latest snapshot. Mostly for diagnostics / tests.
    pub fn current_snapshot(&self) -> Arc<MixerSnapshot> {
        self.snapshot.load_full()
    }

    /// Send + Sync handle to the snapshot bus. `AudioMixer` itself
    /// is `!Send` because it owns the `cpal::Stream`, so the engine
    /// keeps it behind an owner thread; this handle is what the
    /// edit-log thread uses to publish updates without crossing the
    /// owner thread boundary.
    pub fn snapshot_handle(&self) -> Arc<ArcSwap<MixerSnapshot>> {
        self.snapshot.clone()
    }

    /// Output device sample rate (Hz).
    pub fn device_sample_rate(&self) -> u32 {
        self.device_sample_rate
    }

    /// Master frame counter handle. Counts device-sample frames since
    /// stream start. Used by the M3 A/V sync swap to derive the
    /// master playhead from the output clock instead of one source's
    /// per-stream PTS.
    pub fn master_frame_handle(&self) -> Arc<AtomicI64> {
        self.master_frame.clone()
    }
}

impl Drop for AudioMixer {
    fn drop(&mut self) {
        if let Some(mut inner) = self.inner.lock().take() {
            inner.stop.store(true, Ordering::Release);
            // Fold thread owns the live source set + per-source stop
            // flags. Joining it cascades into shutting each source down
            // and joining its puller. We rely on that cascade rather
            // than fanning per-source joins out of `Inner`.
            if let Some(h) = inner.fold_handle.take() {
                let _ = h.join();
            }
            // _stream drops here, stopping cpal output.
        }
    }
}

/// One source-side ring read by the fold thread. Constructed on the
/// scheduler thread by [`spawn_source`] and handed to the fold thread
/// via [`SourceCommand::Add`].
pub struct SourceStream {
    clip_id: Uuid,
    rx: ringbuf::HeapCons<f32>,
    /// Per-source stop flag. Setting this requests the puller exit on
    /// its next iteration (i.e. when its current `next_chunk()` returns).
    /// The puller checks the flag between IPC reads, so the worst-case
    /// wakeup is one decoder chunk period (~21ms at 48 kHz / 1024 frames).
    stop: Arc<AtomicBool>,
    /// Puller join handle. Phase 7.4 M4 deliberately DETACHES this on
    /// `shutdown()` — see the method's doc-comment for the rationale.
    handle: Option<JoinHandle<()>>,
}

impl SourceStream {
    /// Clip uuid this source belongs to.
    pub fn clip_id(&self) -> Uuid {
        self.clip_id
    }

    /// Request the per-source puller exit. Called when the scheduler
    /// removes this clip from the live window or when the fold thread
    /// tears the mixer down.
    ///
    /// Phase 7.4 M4 — DETACHES (does not `join`) the puller:
    /// - The puller only checks `stop` between calls to
    ///   [`AudioClient::next_chunk`], a synchronous blocking IPC read.
    ///   `join`-ing here would block the caller for the worst-case
    ///   sidecar response time (a wedged sidecar would hang
    ///   `AudioMixer::drop` indefinitely — the adversarial review
    ///   flagged this as a deadlock-class bug).
    /// - Detaching is sound: setting the stop flag is observed within
    ///   one chunk period (~21ms); the puller owns the `AudioClient`,
    ///   so its loop-exit drops the sidecar's `Sidecar`, whose `Drop`
    ///   kills any still-alive child. Worst case is a ~21ms-late
    ///   reaper after the source leaves the live set.
    /// - Future milestone: add a force-kill-on-timeout escalation using
    ///   the sidecar's PID for the wedged-sidecar case.
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Detach: drop the handle without joining. See above.
        let _ = self.handle.take();
    }
}

/// A command sent from the scheduler thread (vance-app side) to the
/// fold thread to grow or shrink the live source set without restarting
/// the mixer. Drained at the top of every fold-loop iteration.
///
/// Phase 7.4 M4 invariant: every `Add` must be matched by either a
/// subsequent `Remove` (clip slid out of the preload window) or the
/// mixer shutdown. The fold thread does NOT garbage-collect sources
/// after EOS — the scheduler is the single source of truth for which
/// clips should be live.
///
/// IMPORTANT: the heavy lift (sidecar spawn, IPC handshake, decoder
/// start) happens on the SCHEDULER thread via [`spawn_source`] BEFORE
/// the command is sent. The fold thread then only does a HashMap
/// insert/remove — never a blocking IPC roundtrip — so cpal never
/// underruns just because a scheduler tick decided to add a clip.
pub enum SourceCommand {
    /// Insert a pre-spawned, already-streaming source into the live set.
    /// The scheduler called [`spawn_source`] before issuing the command
    /// so the fold thread can take ownership in a single move. If `Add`
    /// arrives for a clip already live, the prior source is shut down
    /// and replaced (defensive — should not happen under normal
    /// scheduling).
    Add(SourceStream),
    /// Remove the clip with this uuid from the live set. The fold
    /// thread takes ownership and shuts down the source. No-op if the
    /// clip isn't currently live.
    Remove(Uuid),
}

/// Live-process counters for the mixer. Shared by the scheduler
/// (writes) and observability (reads). Atomics give a stable lock-free
/// readout for the UI / logs without touching the fold thread.
///
/// Phase 7.4 M4 ships three signals:
/// - `sources_spawned_total` — monotonically rising count of every
///   `SourceCommand::Add` the fold thread successfully spawned. Useful
///   for spotting scheduling thrash.
/// - `scheduler_cap_hits_total` — count of times the scheduler wanted
///   to spawn a source but `MAX_CONCURRENT_SOURCES` was already reached.
///   A non-zero rate indicates the cap is being hit — either lower
///   PRELOAD or raise the cap.
/// - `last_open_to_first_sample_ms` — gauge of the most recent
///   sidecar-open → first-PCM-byte latency, in milliseconds. The
///   spike asked for an HDR histogram; a single gauge is the v1
///   shape and 7.5 can upgrade to histograms without breaking the ABI.
#[derive(Debug, Default)]
pub struct MixerStats {
    /// Monotonic count of successful source spawns over this mixer's lifetime.
    pub sources_spawned_total: AtomicU64,
    /// Monotonic count of scheduler-tick decisions that dropped a desired
    /// spawn because the per-mixer concurrent-source cap was full.
    pub scheduler_cap_hits_total: AtomicU64,
    /// Most-recent sidecar-open → first-sample latency, in milliseconds.
    pub last_open_to_first_sample_ms: AtomicU64,
}

/// Spawn one sidecar + puller for `source`. The puller writes into a
/// fresh per-source ring; the returned `SourceStream` owns the
/// consumer half and the per-source stop flag the fold thread uses to
/// tear it down. `MixerStats` is bumped on each successful spawn and
/// the open→first-sample latency is sampled.
///
/// Phase 7.4 M4: this is invoked from the **scheduler** thread (and the
/// caller of [`AudioMixer::start`] for the initial seed) so the
/// blocking IPC handshake never lands on the fold thread that feeds
/// cpal. The fold thread only takes ownership of the ready
/// [`SourceStream`] via [`SourceCommand::Add`].
pub fn spawn_source(
    sidecar_binary: &Path,
    source: &AudioSource,
    capacity: usize,
    stats: &Arc<MixerStats>,
) -> Result<SourceStream, AudioMixerError> {
    let mut client = AudioClient::spawn(sidecar_binary)?;
    let open_t0 = Instant::now();
    let info = client.open(source.path.to_string_lossy().into_owned())?;
    let (atb_n, atb_d) = info.time_base;
    let from_pts = if atb_n > 0 {
        (source.from_seconds.max(0.0) * (atb_d as f64) / (atb_n as f64)).round() as i64
    } else {
        0
    };
    let _ = client.start_stream(from_pts, 1024)?;
    let open_to_first_sample_ms = open_t0.elapsed().as_millis() as u64;
    stats
        .last_open_to_first_sample_ms
        .store(open_to_first_sample_ms, Ordering::Release);
    stats
        .sources_spawned_total
        .fetch_add(1, Ordering::Release);

    let rb = HeapRb::<f32>::new(capacity);
    let (prod, cons) = rb.split();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let handle = thread::Builder::new()
        .name(format!("vance-audio.mixer.puller.{}", source.clip_id))
        .spawn(move || source_puller_loop(client, prod, stop_clone))
        .map_err(|e| AudioMixerError::Spawn {
            kind: "puller",
            err: e.to_string(),
        })?;
    Ok(SourceStream {
        clip_id: source.clip_id,
        rx: cons,
        stop,
        handle: Some(handle),
    })
}

/// Phase 7.4 M4 per-source timeline gate. Returns `true` when the
/// fold thread should pop a sample for the clip at `t_frame`. The gate
/// is symmetric: it fires `false` BOTH before the clip starts (so
/// preloaded samples accumulate in the ring without being mixed early)
/// AND after the clip ends (so the scheduler's grace window cannot
/// leak post-out source audio that the editor explicitly trimmed off).
///
/// Inputs come from the live [`MixerSnapshot`]; pure function so the
/// boundary cases are unit-testable.
#[inline]
pub fn source_audible_at(timeline_in_seconds: f64, clip_duration_seconds: f64, t_frame: f64) -> bool {
    t_frame >= timeline_in_seconds && t_frame < timeline_in_seconds + clip_duration_seconds
}

/// Per-source ring capacity at `sample_rate` — stereo, ~125 ms.
/// Exposed so the scheduler can pre-compute it once and pass it to
/// [`spawn_source`] without depending on `AudioMixer`'s internals.
pub fn per_source_ring_capacity(sample_rate: u32) -> usize {
    (sample_rate as usize) * 2 / 8
}

/// Probe the default cpal output device's sample rate without opening
/// the stream. Used by the engine so initial sources can be spawned on
/// the caller's thread before [`AudioMixer::start`] is invoked — that
/// way the structurally-required seek-onto-edge pre-spawn doesn't
/// depend on circular setup (mixer needs sources, sources need rate,
/// rate is known only after mixer opens the device).
pub fn probe_default_sample_rate() -> Result<u32, AudioMixerError> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or(AudioMixerError::NoDevice)?;
    let config = device
        .default_output_config()
        .map_err(|e| AudioMixerError::Build(e.to_string()))?;
    Ok(config.sample_rate().0)
}

fn source_puller_loop(
    mut client: AudioClient,
    mut producer: ringbuf::HeapProd<f32>,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Acquire) {
        if producer.vacant_len() < producer.capacity().get() / 2 {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        match client.next_chunk() {
            Ok(Some(chunk)) => {
                let pushed = producer.push_slice(&chunk.samples);
                if pushed < chunk.samples.len() {
                    tracing::warn!(
                        chunk_len = chunk.samples.len(),
                        pushed,
                        "mixer: per-source ring full, dropping tail"
                    );
                }
            }
            Ok(None) => {
                tracing::info!("mixer: source EOS");
                break;
            }
            Err(e) => {
                tracing::error!(?e, "mixer: source puller error, exiting");
                break;
            }
        }
    }
    let _ = client.stop_stream();
    let _ = client.shutdown();
}

#[allow(clippy::too_many_arguments)]
fn fold_loop(
    initial_sources: Vec<SourceStream>,
    command_rx: mpsc::Receiver<SourceCommand>,
    mut master_prod: ringbuf::HeapProd<f32>,
    snapshot: Arc<ArcSwap<MixerSnapshot>>,
    master_frame: Arc<AtomicI64>,
    stop: Arc<AtomicBool>,
    channels: usize,
    sample_rate: u32,
    playback_start_timeline_seconds: f64,
) {
    if channels != 2 {
        tracing::error!(
            channels,
            "mixer: only stereo output supported in v1; fold thread exiting"
        );
        return;
    }
    if sample_rate == 0 {
        // Defensive: cpal should never report 0 Hz (it errors first),
        // but if it did `t_frame` would never advance, freezing every
        // keyframe lane / fade at `playback_start_timeline_seconds`.
        // Fail closed rather than silently produce time-invariant audio.
        tracing::error!("mixer: sample_rate=0; fold thread exiting");
        return;
    }
    // Live source set, keyed by clip uuid so Remove and Add lookups
    // stay O(1). The scheduler thread does the slow sidecar spawn and
    // hands READY SourceStreams to us via SourceCommand::Add — fold
    // never blocks on IPC.
    let mut sources: HashMap<Uuid, SourceStream> = HashMap::with_capacity(initial_sources.len());
    for s in initial_sources {
        sources.insert(s.clip_id, s);
    }
    let mut frame_samples: Vec<(Uuid, [f32; 2])> = Vec::with_capacity(sources.len());
    let inv_sample_rate = 1.0 / sample_rate as f64;
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        // Phase 7.4 M4: drain pending scheduler commands BEFORE folding
        // this batch. Adds come pre-spawned (no IPC on the fold thread);
        // Removes detach the puller — see SourceStream::shutdown.
        loop {
            // Honour the global stop even mid-drain so a panic/fast
            // shutdown path doesn't grow `sources` after stop=true.
            if stop.load(Ordering::Acquire) {
                break;
            }
            match command_rx.try_recv() {
                Ok(SourceCommand::Add(stream)) => {
                    let clip_id = stream.clip_id;
                    if let Some(mut existing) = sources.remove(&clip_id) {
                        // Defensive: scheduler asked for a re-spawn.
                        existing.shutdown();
                    }
                    sources.insert(clip_id, stream);
                }
                Ok(SourceCommand::Remove(clip_id)) => {
                    if let Some(mut s) = sources.remove(&clip_id) {
                        s.shutdown();
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Scheduler dropped its Sender. The mixer itself
                    // owns the stop flag for shutdown, so we keep
                    // folding live sources to drain them; we just
                    // won't receive new commands.
                    break;
                }
            }
        }

        // Backpressure: if the master ring is more than half full,
        // sleep briefly so cpal can drain.
        if master_prod.vacant_len() < master_prod.capacity().get() / 2 {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        let snap = snapshot.load_full();
        let batch_start_frame = master_frame.load(Ordering::Acquire);
        let t_batch_start =
            playback_start_timeline_seconds + batch_start_frame as f64 * inv_sample_rate;
        let frames_this_batch = master_prod.vacant_len() / 2;
        let frames_this_batch = frames_this_batch.min(1024);
        for i in 0..frames_this_batch {
            let t_frame = t_batch_start + (i as f64) * inv_sample_rate;
            frame_samples.clear();
            for src in sources.values_mut() {
                // Phase 7.4 M4: symmetric per-source timeline gate via
                // pure helper [`source_audible_at`]. Skipping pop on
                // either side keeps preloaded samples queued until
                // `t_frame` enters the clip's live region AND prevents
                // post-out source audio from leaking during the
                // scheduler's removal grace window.
                let (timeline_in, clip_dur) = snap
                    .clips
                    .get(&src.clip_id)
                    .map(|c| (c.timeline_in_seconds, c.clip_duration_seconds))
                    .unwrap_or((f64::NEG_INFINITY, f64::INFINITY));
                if !source_audible_at(timeline_in, clip_dur, t_frame) {
                    continue;
                }
                let mut buf = [0.0f32; 2];
                let got = src.rx.pop_slice(&mut buf);
                if got == 2 {
                    frame_samples.push((src.clip_id, buf));
                }
            }
            let out = fold_frame_at(&snap, &frame_samples, t_frame);
            let _ = master_prod.push_slice(&out);
        }
        master_frame.fetch_add(frames_this_batch as i64, Ordering::Release);
        if frames_this_batch == 0 {
            thread::sleep(Duration::from_millis(2));
        }
    }
    // Detach every live source — sets per-source stop flags. Pullers
    // self-clean within one chunk period when their next IPC read
    // returns.
    for (_, mut s) in sources.drain() {
        s.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with(
        tracks: &[(Uuid, TrackMix)],
        clips: &[(Uuid, ClipMix)],
    ) -> MixerSnapshot {
        let mut s = MixerSnapshot::empty();
        for (id, m) in tracks {
            s.tracks.insert(*id, m.clone());
        }
        for (id, m) in clips {
            s.clips.insert(*id, m.clone());
        }
        s.solo_anywhere = s.tracks.values().any(|t| t.solo);
        s
    }

    fn unity_track() -> TrackMix {
        TrackMix {
            gain_linear: 1.0,
            gain_lane: Vec::new(),
            mute: false,
            solo: false,
        }
    }

    fn unity_clip(track_id: Uuid) -> ClipMix {
        ClipMix {
            gain_linear: 1.0,
            gain_lane: Vec::new(),
            enabled: true,
            track_id,
            timeline_in_seconds: 0.0,
            clip_duration_seconds: 10.0,
            fade_in_duration_seconds: 0.0,
            fade_in_curve: FadeCurve::Linear,
            fade_out_duration_seconds: 0.0,
            fade_out_curve: FadeCurve::Linear,
        }
    }

    #[test]
    fn db_to_linear_round_numbers() {
        assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6);
        // -6 dB ≈ 0.5012
        assert!((db_to_linear(-6.0) - 0.5012).abs() < 1e-3);
        // +6 dB ≈ 1.9953
        assert!((db_to_linear(6.0) - 1.9953).abs() < 1e-3);
        // -inf-ish (very negative dB) → near zero
        assert!(db_to_linear(-200.0) < 1e-9);
    }

    #[test]
    fn db_to_linear_rejects_non_finite() {
        assert_eq!(db_to_linear(f32::NAN), 0.0);
        assert_eq!(db_to_linear(f32::INFINITY), 0.0);
        assert_eq!(db_to_linear(f32::NEG_INFINITY), 0.0);
    }

    #[test]
    fn fold_frame_sums_unity_sources() {
        // Two clips on two tracks, all unity. fold = (gA + gB) per channel.
        let ta = Uuid::new_v4();
        let tb = Uuid::new_v4();
        let ca = Uuid::new_v4();
        let cb = Uuid::new_v4();
        let snap = snapshot_with(
            &[(ta, unity_track()), (tb, unity_track())],
            &[(ca, unity_clip(ta)), (cb, unity_clip(tb))],
        );
        let out = fold_frame(&snap, &[(ca, [0.4, -0.4]), (cb, [0.3, -0.3])]);
        assert!((out[0] - 0.7).abs() < 1e-6);
        assert!((out[1] - (-0.7)).abs() < 1e-6);
    }

    #[test]
    fn fold_frame_track_gain_applies_multiplicatively_with_clip_gain() {
        // Track at -6 dB (~0.5), clip at -6 dB (~0.5) → effective ~0.25.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let track_mix = TrackMix {
            gain_linear: db_to_linear(-6.0),
            ..unity_track()
        };
        let clip_mix = ClipMix {
            gain_linear: db_to_linear(-6.0),
            ..unity_clip(t)
        };
        let snap = snapshot_with(&[(t, track_mix)], &[(c, clip_mix)]);
        let out = fold_frame(&snap, &[(c, [1.0, 1.0])]);
        let expected = db_to_linear(-6.0) * db_to_linear(-6.0);
        assert!((out[0] - expected).abs() < 1e-4);
        assert!((out[1] - expected).abs() < 1e-4);
    }

    #[test]
    fn fold_frame_track_mute_silences_clip() {
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let track_mix = TrackMix {
            mute: true,
            ..unity_track()
        };
        let snap = snapshot_with(&[(t, track_mix)], &[(c, unity_clip(t))]);
        let out = fold_frame(&snap, &[(c, [0.5, -0.5])]);
        assert_eq!(out, [0.0, 0.0]);
    }

    #[test]
    fn fold_frame_clip_disabled_silences_clip() {
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let clip_mix = ClipMix {
            enabled: false,
            ..unity_clip(t)
        };
        let snap = snapshot_with(&[(t, unity_track())], &[(c, clip_mix)]);
        let out = fold_frame(&snap, &[(c, [0.5, -0.5])]);
        assert_eq!(out, [0.0, 0.0]);
    }

    #[test]
    fn fold_frame_solo_silences_non_soloed_tracks() {
        // ta unsoloed, tb soloed → only tb's clip contributes.
        let ta = Uuid::new_v4();
        let tb = Uuid::new_v4();
        let ca = Uuid::new_v4();
        let cb = Uuid::new_v4();
        let snap = snapshot_with(
            &[
                (ta, unity_track()),
                (
                    tb,
                    TrackMix {
                        solo: true,
                        ..unity_track()
                    },
                ),
            ],
            &[(ca, unity_clip(ta)), (cb, unity_clip(tb))],
        );
        assert!(snap.solo_anywhere);
        let out = fold_frame(&snap, &[(ca, [0.4, 0.4]), (cb, [0.3, 0.3])]);
        assert!((out[0] - 0.3).abs() < 1e-6);
        assert!((out[1] - 0.3).abs() < 1e-6);
    }

    #[test]
    fn fold_frame_solo_overrides_mute_consistently() {
        // A track that's both soloed AND muted: mute wins (silent).
        // This matches Resolve / Premiere behavior — solo enables
        // audibility, mute forces silence.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let track_mix = TrackMix {
            solo: true,
            mute: true,
            ..unity_track()
        };
        let snap = snapshot_with(&[(t, track_mix)], &[(c, unity_clip(t))]);
        assert!(snap.solo_anywhere);
        let out = fold_frame(&snap, &[(c, [0.7, 0.7])]);
        assert_eq!(out, [0.0, 0.0]);
    }

    #[test]
    fn fold_frame_orphan_clip_silences() {
        // Clip whose parent track isn't in the snapshot — defensive
        // zero, never amplifies.
        let t_real = Uuid::new_v4();
        let t_ghost = Uuid::new_v4();
        let c = Uuid::new_v4();
        let snap = snapshot_with(
            &[(t_real, unity_track())],
            &[(c, unity_clip(t_ghost))], // parent ghost not in tracks map
        );
        let out = fold_frame(&snap, &[(c, [1.0, 1.0])]);
        assert_eq!(out, [0.0, 0.0]);
    }

    #[test]
    fn fold_frame_unknown_clip_id_silences() {
        let snap = MixerSnapshot::empty();
        let mystery = Uuid::new_v4();
        let out = fold_frame(&snap, &[(mystery, [1.0, 1.0])]);
        assert_eq!(out, [0.0, 0.0]);
    }

    #[test]
    fn fold_frame_master_gain_scales_master_bus() {
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut snap = snapshot_with(&[(t, unity_track())], &[(c, unity_clip(t))]);
        snap.master_gain_linear = 0.5;
        let out = fold_frame(&snap, &[(c, [1.0, -1.0])]);
        assert!((out[0] - 0.5).abs() < 1e-6);
        assert!((out[1] - (-0.5)).abs() < 1e-6);
    }

    #[test]
    fn fold_frame_empty_sources_returns_silence() {
        let snap = MixerSnapshot::empty();
        assert_eq!(fold_frame(&snap, &[]), [0.0, 0.0]);
    }

    #[test]
    fn snapshot_empty_is_silent_for_any_clip() {
        let snap = MixerSnapshot::empty();
        assert_eq!(snap.effective_gain(&Uuid::new_v4()), 0.0);
    }

    #[test]
    fn snapshot_solo_anywhere_does_not_affect_soloed_tracks() {
        let ta = Uuid::new_v4();
        let ca = Uuid::new_v4();
        let snap = snapshot_with(
            &[(
                ta,
                TrackMix {
                    solo: true,
                    ..unity_track()
                },
            )],
            &[(ca, unity_clip(ta))],
        );
        assert!(snap.solo_anywhere);
        assert!((snap.effective_gain(&ca) - 1.0).abs() < 1e-6);
    }

    // ---- Phase 7.4 keyframe lane tests --------------------------------

    fn kf(t: f64, g: f32, interp: GainInterp) -> GainKeyframe {
        GainKeyframe {
            time_seconds: t,
            gain_linear: g,
            interp,
        }
    }

    #[test]
    fn eval_gain_lane_empty_returns_unity_not_zero() {
        // THE footgun: vance-timeline's eval_keyframes(&[]) = 0.0.
        // The mixer's lane evaluator MUST return 1.0 on empty.
        assert_eq!(eval_gain_lane(&[], 0.0), 1.0);
        assert_eq!(eval_gain_lane(&[], 100.0), 1.0);
    }

    #[test]
    fn eval_gain_lane_clamps_before_first_and_after_last() {
        let lane = vec![
            kf(1.0, 0.5, GainInterp::Linear),
            kf(2.0, 0.8, GainInterp::Linear),
        ];
        // Before first → first value.
        assert!((eval_gain_lane(&lane, 0.0) - 0.5).abs() < 1e-6);
        // After last → last value.
        assert!((eval_gain_lane(&lane, 5.0) - 0.8).abs() < 1e-6);
    }

    #[test]
    fn eval_gain_lane_linear_interpolates() {
        let lane = vec![
            kf(0.0, 0.0, GainInterp::Linear),
            kf(1.0, 1.0, GainInterp::Linear),
        ];
        assert!((eval_gain_lane(&lane, 0.25) - 0.25).abs() < 1e-6);
        assert!((eval_gain_lane(&lane, 0.5) - 0.5).abs() < 1e-6);
        assert!((eval_gain_lane(&lane, 0.75) - 0.75).abs() < 1e-6);
    }

    #[test]
    fn eval_gain_lane_hold_snaps_to_previous() {
        let lane = vec![
            kf(0.0, 0.2, GainInterp::Hold),
            kf(1.0, 0.8, GainInterp::Linear),
        ];
        // Any t in [0, 1) holds at 0.2.
        assert!((eval_gain_lane(&lane, 0.0) - 0.2).abs() < 1e-6);
        assert!((eval_gain_lane(&lane, 0.5) - 0.2).abs() < 1e-6);
        assert!((eval_gain_lane(&lane, 0.999) - 0.2).abs() < 1e-6);
    }

    #[test]
    fn eval_gain_lane_bezier_smoothsteps() {
        // Smoothstep: y = 3t² − 2t³ produces 0 at t=0, 0.5 at t=0.5, 1 at t=1.
        let lane = vec![
            kf(0.0, 0.0, GainInterp::Bezier),
            kf(1.0, 1.0, GainInterp::Linear),
        ];
        let mid = eval_gain_lane(&lane, 0.5);
        assert!((mid - 0.5).abs() < 1e-6, "midpoint should be 0.5 by symmetry");
        // At t=0.25 → 3(0.25)² − 2(0.25)³ = 0.1875 − 0.03125 = 0.15625.
        let quarter = eval_gain_lane(&lane, 0.25);
        assert!((quarter - 0.15625).abs() < 1e-6);
    }

    #[test]
    fn effective_gain_at_uses_track_lane_when_non_empty() {
        // Track lane has a 0→1 ramp over [0, 1] timeline-seconds; clip
        // lane empty (uses static gain 1.0).
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut track = unity_track();
        track.gain_lane = vec![
            kf(0.0, 0.0, GainInterp::Linear),
            kf(1.0, 1.0, GainInterp::Linear),
        ];
        let snap = snapshot_with(&[(t, track)], &[(c, unity_clip(t))]);
        assert!((snap.effective_gain_at(&c, 0.0) - 0.0).abs() < 1e-6);
        assert!((snap.effective_gain_at(&c, 0.5) - 0.5).abs() < 1e-6);
        assert!((snap.effective_gain_at(&c, 1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn effective_gain_at_clip_lane_is_clip_relative() {
        // Clip starts at timeline t=10 seconds. Lane goes 0→1 over
        // CLIP-RELATIVE [0, 2] seconds. So at timeline t=11 the
        // clip-relative t is 1 → midpoint → 0.5.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut clip = unity_clip(t);
        clip.timeline_in_seconds = 10.0;
        clip.gain_lane = vec![
            kf(0.0, 0.0, GainInterp::Linear),
            kf(2.0, 1.0, GainInterp::Linear),
        ];
        let snap = snapshot_with(&[(t, unity_track())], &[(c, clip)]);
        assert!((snap.effective_gain_at(&c, 10.0) - 0.0).abs() < 1e-6);
        assert!((snap.effective_gain_at(&c, 11.0) - 0.5).abs() < 1e-6);
        assert!((snap.effective_gain_at(&c, 12.0) - 1.0).abs() < 1e-6);
        // Before the clip (timeline t < 10) → clamp to first → 0.0.
        assert!((snap.effective_gain_at(&c, 5.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn effective_gain_at_falls_back_to_static_when_lane_empty() {
        // CRITICAL invariant: empty lane → use the static gain, NOT 0.
        // This is the structural antidote to the eval_curve(&[]) = 0.0
        // footgun the spike warned about.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        // Track at unity static, clip at unity static, both lanes empty.
        let snap = snapshot_with(&[(t, unity_track())], &[(c, unity_clip(t))]);
        // No matter the time, the gain is 1.0 (unity track × unity clip × unity master).
        assert!((snap.effective_gain_at(&c, 0.0) - 1.0).abs() < 1e-6);
        assert!((snap.effective_gain_at(&c, 1000.0) - 1.0).abs() < 1e-6);
        assert!((snap.effective_gain_at(&c, f64::NEG_INFINITY) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn effective_gain_at_mute_overrides_lane() {
        // Track mute still wins over a non-trivial lane.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut track = TrackMix {
            mute: true,
            ..unity_track()
        };
        track.gain_lane = vec![
            kf(0.0, 0.5, GainInterp::Linear),
            kf(1.0, 1.0, GainInterp::Linear),
        ];
        let snap = snapshot_with(&[(t, track)], &[(c, unity_clip(t))]);
        assert_eq!(snap.effective_gain_at(&c, 0.5), 0.0);
    }

    #[test]
    fn effective_gain_at_track_and_clip_lanes_compose_multiplicatively() {
        // Track lane at 0.5 throughout; clip lane at 0.4 throughout.
        // Expected: 0.5 × 0.4 = 0.2.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut track = unity_track();
        track.gain_lane = vec![kf(0.0, 0.5, GainInterp::Linear)];
        let mut clip = unity_clip(t);
        clip.gain_lane = vec![kf(0.0, 0.4, GainInterp::Linear)];
        let snap = snapshot_with(&[(t, track)], &[(c, clip)]);
        let g = snap.effective_gain_at(&c, 5.0);
        assert!((g - 0.2).abs() < 1e-6, "composed gain (got {})", g);
    }

    #[test]
    fn fold_frame_at_advances_with_t() {
        // Track lane: 0 dB → -inf dB (here 0.0) over [0, 1] timeline.
        // Frame 0 at t=0 → full level. Frame at t=1 → silence.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut track = unity_track();
        track.gain_lane = vec![
            kf(0.0, 1.0, GainInterp::Linear),
            kf(1.0, 0.0, GainInterp::Linear),
        ];
        let snap = snapshot_with(&[(t, track)], &[(c, unity_clip(t))]);
        let frame_in = [(c, [0.6f32, -0.6f32])];
        let at_zero = fold_frame_at(&snap, &frame_in, 0.0);
        let at_half = fold_frame_at(&snap, &frame_in, 0.5);
        let at_end = fold_frame_at(&snap, &frame_in, 1.0);
        assert!((at_zero[0] - 0.6).abs() < 1e-6);
        assert!((at_half[0] - 0.3).abs() < 1e-6);
        assert!(at_end[0].abs() < 1e-6);
    }

    #[test]
    fn fade_in_zero_duration_is_unity() {
        // No fade → 1.0 everywhere, regardless of curve.
        for c in [FadeCurve::Linear, FadeCurve::EqualPower, FadeCurve::Logarithmic] {
            assert_eq!(eval_fade_in_audio(0.0, c, 0.0), 1.0);
            assert_eq!(eval_fade_in_audio(0.0, c, 5.0), 1.0);
        }
    }

    #[test]
    fn fade_in_linear_ramps_from_zero_to_unity() {
        assert_eq!(eval_fade_in_audio(1.0, FadeCurve::Linear, -0.5), 0.0);
        assert_eq!(eval_fade_in_audio(1.0, FadeCurve::Linear, 0.0), 0.0);
        assert!((eval_fade_in_audio(1.0, FadeCurve::Linear, 0.5) - 0.5).abs() < 1e-6);
        assert_eq!(eval_fade_in_audio(1.0, FadeCurve::Linear, 1.0), 1.0);
        assert_eq!(eval_fade_in_audio(1.0, FadeCurve::Linear, 2.0), 1.0);
    }

    #[test]
    fn fade_out_linear_ramps_to_silence_at_clip_end() {
        // 5s clip, 1s fade-out: unity at t<=4, half at 4.5, zero at >=5.
        assert_eq!(eval_fade_out_audio(1.0, FadeCurve::Linear, 5.0, 3.0), 1.0);
        assert_eq!(eval_fade_out_audio(1.0, FadeCurve::Linear, 5.0, 4.0), 1.0);
        assert!((eval_fade_out_audio(1.0, FadeCurve::Linear, 5.0, 4.5) - 0.5).abs() < 1e-6);
        assert_eq!(eval_fade_out_audio(1.0, FadeCurve::Linear, 5.0, 5.0), 0.0);
        assert_eq!(eval_fade_out_audio(1.0, FadeCurve::Linear, 5.0, 6.0), 0.0);
    }

    #[test]
    fn fade_reserved_curves_fall_back_to_linear() {
        // 7.5 will implement EqualPower/Logarithmic; until then they must
        // not silently mute audio. Match linear at the midpoint.
        for c in [FadeCurve::EqualPower, FadeCurve::Logarithmic] {
            let g = eval_fade_in_audio(2.0, c, 1.0);
            assert!((g - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn effective_gain_at_composes_fade_with_keyframes_and_static() {
        // Clip at unity static, lane empty, fade-in 1s, at t_clip=0.25 →
        // fade-in returns 0.25. clip_g = 1.0; track_g = 1.0; result = 0.25.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut clip = unity_clip(t);
        clip.fade_in_duration_seconds = 1.0;
        clip.clip_duration_seconds = 5.0;
        let snap = snapshot_with(&[(t, unity_track())], &[(c, clip)]);
        let g = snap.effective_gain_at(&c, 0.25);
        assert!((g - 0.25).abs() < 1e-6, "got {g}");
    }

    #[test]
    fn effective_gain_at_fade_in_and_keyframe_lane_compose_independently() {
        // Lane holds the clip at 0.5 throughout; fade-in 1s at t_clip=0.5
        // returns 0.5. Composed: 0.5 (lane) × 0.5 (fade) = 0.25.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut clip = unity_clip(t);
        clip.gain_lane = vec![kf(0.0, 0.5, GainInterp::Linear)];
        clip.fade_in_duration_seconds = 1.0;
        clip.clip_duration_seconds = 5.0;
        let snap = snapshot_with(&[(t, unity_track())], &[(c, clip)]);
        let g = snap.effective_gain_at(&c, 0.5);
        assert!((g - 0.25).abs() < 1e-6, "got {g}");
    }

    #[test]
    fn effective_gain_at_fade_out_composes_with_clip_end_time() {
        // 5s clip starting at timeline t=10. fade-out 1s. At timeline
        // t=14.5 → t_clip=4.5 → fade-out = 0.5.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut clip = unity_clip(t);
        clip.timeline_in_seconds = 10.0;
        clip.clip_duration_seconds = 5.0;
        clip.fade_out_duration_seconds = 1.0;
        let snap = snapshot_with(&[(t, unity_track())], &[(c, clip)]);
        assert!((snap.effective_gain_at(&c, 14.0) - 1.0).abs() < 1e-6);
        assert!((snap.effective_gain_at(&c, 14.5) - 0.5).abs() < 1e-6);
        assert!((snap.effective_gain_at(&c, 15.0) - 0.0).abs() < 1e-6);
    }

    #[test]
    fn effective_gain_at_mute_still_overrides_fade() {
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let mut clip = unity_clip(t);
        clip.fade_in_duration_seconds = 1.0;
        clip.clip_duration_seconds = 5.0;
        let track = TrackMix {
            mute: true,
            ..unity_track()
        };
        let snap = snapshot_with(&[(t, track)], &[(c, clip)]);
        assert_eq!(snap.effective_gain_at(&c, 0.5), 0.0);
    }

    // ============================================
    // Phase 7.4 M4 fold-gate boundary tests
    // ============================================

    #[test]
    fn source_audible_at_inside_clip_returns_true() {
        // Clip [0..5]: audible at 0.0, 2.5, 4.999.
        assert!(source_audible_at(0.0, 5.0, 0.0));
        assert!(source_audible_at(0.0, 5.0, 2.5));
        assert!(source_audible_at(0.0, 5.0, 4.999));
    }

    #[test]
    fn source_audible_at_before_start_returns_false() {
        // Before timeline_in: not yet audible (preloaded samples sit
        // in the ring waiting for the playhead to arrive).
        assert!(!source_audible_at(5.0, 5.0, 4.999));
        assert!(!source_audible_at(5.0, 5.0, 0.0));
        assert!(!source_audible_at(5.0, 5.0, -1.0));
    }

    #[test]
    fn source_audible_at_at_or_past_end_returns_false() {
        // PRIMARY M4 fix: the symmetric past-end gate. Without this,
        // clips with no fade-out would mix at full level for up to
        // grace_seconds (50ms) past their timeline-out, leaking audio
        // the editor explicitly trimmed.
        assert!(!source_audible_at(0.0, 5.0, 5.0)); // exact end
        assert!(!source_audible_at(0.0, 5.0, 5.001));
        assert!(!source_audible_at(0.0, 5.0, 100.0));
    }

    #[test]
    fn source_audible_at_zero_duration_clip_never_audible() {
        // Defensive: zero-duration clip is never audible at any t.
        assert!(!source_audible_at(5.0, 0.0, 5.0));
        assert!(!source_audible_at(5.0, 0.0, 4.0));
    }
}
