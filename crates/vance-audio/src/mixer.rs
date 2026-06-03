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
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
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

/// Per-track mixer state inside a [`MixerSnapshot`]. `gain_linear` is
/// precomputed via [`db_to_linear`] at snapshot build time so the fold
/// thread never touches a `powf` in the audio hot path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackMix {
    /// Multiplicative track gain, linear.
    pub gain_linear: f32,
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipMix {
    /// Multiplicative per-clip gain, linear.
    pub gain_linear: f32,
    /// Per-clip mute (`!Clip.enabled`). False = audible.
    pub enabled: bool,
    /// Parent track uuid for the fold-time track lookup.
    pub track_id: Uuid,
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
    /// Master bus gain (linear). 1.0 = unity. Phase 7.3 ships with
    /// 1.0 hard-coded; a future master fader would publish a non-unity
    /// value here.
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
    #[inline]
    pub fn effective_gain(&self, clip_id: &Uuid) -> f32 {
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
        clip.gain_linear * track.gain_linear * self.master_gain_linear
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

/// Pure per-frame mixer fold. Given each source's current stereo
/// frame as `(clip_uuid, [L, R])`, applies the snapshot's effective
/// gain to each contribution and returns the stereo sum.
///
/// Extracted from the fold thread so it can be unit-tested without a
/// real cpal device. The fold thread loops this function with one
/// `f32` pair pulled from each source's ring per call.
#[inline]
pub fn fold_frame(snapshot: &MixerSnapshot, source_samples: &[(Uuid, [f32; 2])]) -> [f32; 2] {
    let mut out = [0.0f32; 2];
    for (clip_id, [l, r]) in source_samples {
        let g = snapshot.effective_gain(clip_id);
        if g == 0.0 {
            continue;
        }
        out[0] += l * g;
        out[1] += r * g;
    }
    out
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
    puller_handles: Vec<JoinHandle<()>>,
}

impl AudioMixer {
    /// Spawn the cpal output stream, one puller per source, and the
    /// fold thread. Returns once cpal is playing — extraction
    /// continues asynchronously in the worker threads.
    ///
    /// `sidecar_binary` is the path of the `vance-audio-decoder`
    /// binary (each source gets its own sidecar process so they pull
    /// in parallel). `initial_snapshot` seeds the fold thread; the
    /// engine subsequently calls `publish_snapshot` after every
    /// mixer-affecting `EditOp`.
    pub fn start(
        sidecar_binary: impl AsRef<Path>,
        sources: Vec<AudioSource>,
        initial_snapshot: MixerSnapshot,
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
        tracing::info!(sample_rate, channels, n_sources = sources.len(), "mixer: opened cpal");

        // 2. Master ring sized for ~250 ms at device rate. Same budget
        //    as AudioPlayer; the fold thread is the sole producer.
        let master_capacity = (sample_rate as usize) * channels / 4;
        let master_rb = HeapRb::<f32>::new(master_capacity);
        let (master_prod, mut master_cons) = master_rb.split();

        // 3. Spawn one sidecar + puller per source, each writing into
        //    its own ring. Per-source ring is small (~125 ms) — the
        //    pullers are bandwidth-bounded by the sidecar IPC, not by
        //    the device, so excess buffering wastes memory.
        let per_source_capacity = (sample_rate as usize) * 2 / 8; // stereo, ~125 ms
        let mut puller_handles = Vec::with_capacity(sources.len());
        let mut source_streams = Vec::with_capacity(sources.len());
        let stop = Arc::new(AtomicBool::new(false));
        for source in &sources {
            let client = AudioClient::spawn(sidecar_binary.as_ref())?;
            let stream =
                spawn_source(client, source, per_source_capacity, stop.clone(), &mut puller_handles)?;
            source_streams.push(stream);
        }

        // 4. Snapshot bus + master frame counter.
        let snapshot = Arc::new(ArcSwap::from_pointee(initial_snapshot));
        let master_frame = Arc::new(AtomicI64::new(0));

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

        // 6. Fold thread.
        let fold_stop = stop.clone();
        let fold_snapshot = snapshot.clone();
        let fold_master_frame = master_frame.clone();
        let fold_handle = thread::Builder::new()
            .name("vance-audio.mixer.fold".into())
            .spawn(move || {
                fold_loop(
                    source_streams,
                    master_prod,
                    fold_snapshot,
                    fold_master_frame,
                    fold_stop,
                    channels,
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
                puller_handles,
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
            // Join puller threads first; they'll exit on the next
            // backpressure check or sidecar EOS / error.
            for handle in inner.puller_handles.drain(..) {
                let _ = handle.join();
            }
            if let Some(h) = inner.fold_handle.take() {
                let _ = h.join();
            }
            // _stream drops here, stopping cpal output.
        }
    }
}

/// One source-side ring read by the fold thread.
struct SourceStream {
    clip_id: Uuid,
    rx: ringbuf::HeapCons<f32>,
}

fn spawn_source(
    mut client: AudioClient,
    source: &AudioSource,
    capacity: usize,
    stop: Arc<AtomicBool>,
    puller_handles: &mut Vec<JoinHandle<()>>,
) -> Result<SourceStream, AudioMixerError> {
    let info = client.open(source.path.to_string_lossy().into_owned())?;
    let (atb_n, atb_d) = info.time_base;
    let from_pts = if atb_n > 0 {
        (source.from_seconds.max(0.0) * (atb_d as f64) / (atb_n as f64)).round() as i64
    } else {
        0
    };
    let _ = client.start_stream(from_pts, 1024)?;

    let rb = HeapRb::<f32>::new(capacity);
    let (prod, cons) = rb.split();
    let stop_clone = stop.clone();
    let handle = thread::Builder::new()
        .name(format!("vance-audio.mixer.puller.{}", source.clip_id))
        .spawn(move || source_puller_loop(client, prod, stop_clone))
        .map_err(|e| AudioMixerError::Spawn {
            kind: "puller",
            err: e.to_string(),
        })?;
    puller_handles.push(handle);
    Ok(SourceStream {
        clip_id: source.clip_id,
        rx: cons,
    })
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

fn fold_loop(
    mut sources: Vec<SourceStream>,
    mut master_prod: ringbuf::HeapProd<f32>,
    snapshot: Arc<ArcSwap<MixerSnapshot>>,
    master_frame: Arc<AtomicI64>,
    stop: Arc<AtomicBool>,
    channels: usize,
) {
    if channels != 2 {
        tracing::error!(
            channels,
            "mixer: only stereo output supported in v1; fold thread exiting"
        );
        return;
    }
    // Working slice the fold thread reuses each iteration.
    let mut frame_samples: Vec<(Uuid, [f32; 2])> = Vec::with_capacity(sources.len());
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }
        // Backpressure: if the master ring is more than half full,
        // sleep briefly so cpal can drain.
        if master_prod.vacant_len() < master_prod.capacity().get() / 2 {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        // Snapshot bus: one load per buffer (not per frame). Cheap.
        let snap = snapshot.load_full();
        // Fold as many frames as we can without blocking. Each frame
        // pops 2 f32s from each source ring (interleaved stereo); if
        // any source is empty, that source contributes silence this
        // frame.
        let frames_this_batch = master_prod.vacant_len() / 2;
        let frames_this_batch = frames_this_batch.min(1024); // cap per-batch latency
        for _ in 0..frames_this_batch {
            frame_samples.clear();
            for src in sources.iter_mut() {
                // Try to pop one stereo frame. If only one half is
                // available, treat as no data this frame.
                let mut buf = [0.0f32; 2];
                let got = src.rx.pop_slice(&mut buf);
                if got == 2 {
                    frame_samples.push((src.clip_id, buf));
                }
                // Otherwise: source has nothing right now; skip.
            }
            let out = fold_frame(&snap, &frame_samples);
            // Push interleaved L, R. If the ring is full we'd hit
            // backpressure on the next iteration; this push should
            // always succeed because we sized frames_this_batch by
            // vacant_len.
            let _ = master_prod.push_slice(&out);
        }
        master_frame.fetch_add(frames_this_batch as i64, Ordering::Release);
        // If we did nothing this iteration (no headroom AND no
        // sources ready) yield briefly to avoid burning CPU.
        if frames_this_batch == 0 {
            thread::sleep(Duration::from_millis(2));
        }
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
            s.tracks.insert(*id, *m);
        }
        for (id, m) in clips {
            s.clips.insert(*id, *m);
        }
        s.solo_anywhere = s.tracks.values().any(|t| t.solo);
        s
    }

    fn unity_track() -> TrackMix {
        TrackMix {
            gain_linear: 1.0,
            mute: false,
            solo: false,
        }
    }

    fn unity_clip(track_id: Uuid) -> ClipMix {
        ClipMix {
            gain_linear: 1.0,
            enabled: true,
            track_id,
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
}
