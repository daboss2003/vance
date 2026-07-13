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
//! [`fold_frame`] / [`fold_frame_at`] are the per-frame mix math
//! kept as TEST-ONLY helpers since Phase 7.5 M1 — the live fold loop
//! inlines an equivalent computation that ALSO splits out the
//! per-track intermediate so the metering accumulators see each
//! track's pre-master sum. The unit tests below construct synthetic
//! input frames, apply a snapshot, and assert the output amplitude
//! matches the documented mute / solo / gain semantics exactly.

#![allow(missing_docs)]

use crate::client::{AudioClient, AudioClientError};
use crate::remote_plugin::RemotePluginHost;
use arc_swap::ArcSwap;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use parking_lot::Mutex;
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::HeapRb;
use std::collections::{HashMap, VecDeque};
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

/// Phase 7.6: equal-power stereo pan conversion.
///
/// Takes a pan position in `[-1, +1]` (`-1` full left, `0` center, `+1`
/// full right) and returns the per-channel multipliers `[L_gain,
/// R_gain]` such that `L² + R² = 1` for every pan value. Center
/// returns `[√½, √½]` (≈ `[0.7071, 0.7071]`) — the equal-power center
/// shape every DAW uses. Non-finite or out-of-range inputs are
/// clamped, never panic.
///
/// Engine call site: `vance-app::engine::build_mixer_snapshot` invokes
/// this once per clip with `track.pan + clip.pan` (clamped) so the
/// fold thread never touches `sin`/`cos`. Per-clip pan_linear is
/// stored on [`ClipMix`].
#[inline]
pub fn pan_to_equal_power(pan: f32) -> [f32; 2] {
    let p = if pan.is_finite() {
        pan.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let angle = (p + 1.0) * std::f32::consts::FRAC_PI_4;
    [angle.cos(), angle.sin()]
}

/// Shape of a clip fade curve. Mirrors `vance-timeline::FadeCurve` so
/// vance-audio stays decoupled from the timeline crate. All three
/// variants ship real implementations since Phase 7.5 M2.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FadeCurve {
    /// Linear amplitude ramp — `gain(t) = t/d`. Default for forward-compat.
    #[default]
    Linear,
    /// −3 dB equal-power shape — `gain(t) = sin(t·π/2)` for fade-in,
    /// `cos((d-t)·π/2)` for fade-out. Constant total power across a
    /// crossfade pair (sin² + cos² = 1).
    EqualPower,
    /// Quadratic / audio-taper shape — `gain(t) = (t/d)²`. The "audio
    /// fade" most NLEs ship: slow at silence, fast near unity, which
    /// feels perceptually linear to listeners. The name "Logarithmic"
    /// is the conventional NLE label even though the math is a power
    /// curve, not a true logarithm — kept for cross-tool consistency.
    Logarithmic,
}

/// Shape a normalized [0, 1] fade-in fraction by the requested curve.
/// `frac == 0` is silence, `frac == 1` is unity.
///
/// Phase 7.5 M2: EqualPower and Logarithmic ship real implementations.
/// Both monotonically rise from 0 to 1 and are equal to `frac` at the
/// endpoints, so existing fade-in tests that assert endpoints continue
/// to pass — only midpoint values change.
#[inline]
fn shape_fade_in_frac(frac: f32, curve: FadeCurve) -> f32 {
    match curve {
        FadeCurve::Linear => frac,
        // EqualPower: gain rises as sin(frac·π/2). At midpoint =
        // sin(π/4) ≈ 0.7071. Pairing fade-in² + fade-out² = 1 means
        // a crossfade keeps constant total power.
        FadeCurve::EqualPower => (frac * std::f32::consts::FRAC_PI_2).sin(),
        // Logarithmic / audio-taper: gain rises as frac². Quadratic
        // shape stays at the floor longer, giving the "fades up from
        // silence smoothly" perceptual feel.
        FadeCurve::Logarithmic => frac * frac,
    }
}

/// Shape a normalized [0, 1] fade-out fraction by the requested curve.
/// `frac` here is the REMAINING fraction inside the fade-out region:
/// `frac == 1` at the fade-out start (unity), `frac == 0` at the clip
/// end (silence) — matches the math used in [`eval_fade_out_audio`].
#[inline]
fn shape_fade_out_frac(frac: f32, curve: FadeCurve) -> f32 {
    match curve {
        FadeCurve::Linear => frac,
        // EqualPower: sin(frac·π/2). At `frac == 1` returns 1.0 (unity)
        // and at `frac == 0` returns 0.0 (silence). Pairs with the
        // fade-in `sin()` so a fade-out + fade-in crossfade keeps
        // constant total power: sin² + cos² = 1.
        FadeCurve::EqualPower => (frac * std::f32::consts::FRAC_PI_2).sin(),
        // Mirror of the fade-in logarithmic shape so a paired
        // fade-out + fade-in produces the same per-side curvature.
        FadeCurve::Logarithmic => frac * frac,
    }
}

/// Evaluate the multiplicative fade-in gain at clip-relative time
/// `t_clip_seconds`. Returns 1.0 once `t >= duration_seconds`.
/// `duration_seconds <= 0` returns 1.0 (the "no fade" case).
///
/// Phase 7.5 M2: honours `curve` — `Linear`, `EqualPower`, and
/// `Logarithmic` all ship real implementations. Defense-in-depth
/// duplicate of `vance-timeline::eval_fade_in` — both sides agree on
/// the shape family.
#[inline]
pub fn eval_fade_in_audio(duration_seconds: f64, curve: FadeCurve, t_clip_seconds: f64) -> f32 {
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return 1.0;
    }
    if t_clip_seconds <= 0.0 {
        return 0.0;
    }
    if t_clip_seconds >= duration_seconds {
        return 1.0;
    }
    shape_fade_in_frac((t_clip_seconds / duration_seconds) as f32, curve)
}

/// Evaluate the multiplicative fade-out gain at clip-relative time
/// `t_clip_seconds`. `clip_duration_seconds` is the clip's total
/// length. Returns 1.0 before the fade region. Phase 7.5 M2 honours
/// `curve`.
#[inline]
pub fn eval_fade_out_audio(
    duration_seconds: f64,
    curve: FadeCurve,
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
    let frac = ((clip_duration_seconds - t_clip_seconds) / duration_seconds) as f32;
    shape_fade_out_frac(frac, curve)
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

/// One entry in a track's audio-effect insert chain in
/// [`MixerSnapshot`]. Mirrors `vance_timeline::AudioEffectSpec` so the
/// audio crate stays decoupled from the timeline crate. The fold
/// thread keys its DSP state by the chain's POSITION + the variant
/// shape so a snapshot that only updates parameters keeps the same
/// DSP unit (preserving filter delay-line state and compressor
/// envelopes); a snapshot that changes the chain length or reorders
/// effects rebuilds the affected units.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioEffectKind {
    /// 6-band parametric EQ. `enabled == false` bypasses the band
    /// chain entirely on the fold thread.
    Eq6 {
        /// Whether this effect contributes to the output.
        enabled: bool,
        /// The 6 bands, precomputed at snapshot build.
        bands: [EqBandParams; 6],
    },
    /// Single-band stereo compressor — RMS detector + soft knee +
    /// decoupled envelope smoother.
    Compressor {
        /// Whether this effect contributes to the output.
        enabled: bool,
        /// Compressor configuration.
        params: CompressorParams,
    },
    /// Look-ahead peak limiter.
    Limiter {
        /// Whether this effect contributes to the output.
        enabled: bool,
        /// Limiter configuration.
        params: LimiterParams,
    },
    /// GOAT-8 — downward noise gate / expander.
    NoiseGate {
        /// Whether this effect contributes to the output.
        enabled: bool,
        /// Gate configuration.
        params: NoiseGateParams,
    },
    /// GOAT-8 — mains-hum remover (harmonic notch comb).
    DeHum {
        /// Whether this effect contributes to the output.
        enabled: bool,
        /// De-hum configuration.
        params: DeHumParams,
    },
    /// Phase 7.10 / 7.11 — third-party (or built-in) audio effect
    /// plugin. The fold thread instantiates a real in-process
    /// [`crate::audio_plugin::BlockProcessor`] for this entry (built-in
    /// gain, CLAP, or LV2 per `format_str`) and runs the audio through
    /// it — no longer a passthrough.
    Plugin {
        /// Whether this plugin contributes to the output.
        enabled: bool,
        /// Plugin format: `"builtin"`, `"clap"`, or `"lv2"`.
        format_str: String,
        /// Format-specific identifier (built-in id, LV2 URI, or
        /// `bundle:id` for CLAP).
        id: String,
        /// Display name.
        name: String,
        /// Parameter values as `(param_id, value)`, precomputed at
        /// snapshot build time.
        params: Vec<(u32, f32)>,
    },
}

/// One band's worth of parametric-EQ params, precomputed at snapshot
/// build time. `shape` matches the [`crate::dsp::BiquadShape`] mirror.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EqBandParams {
    pub enabled: bool,
    pub shape: crate::dsp::BiquadShape,
    pub freq_hz: f32,
    pub gain_db: f32,
    pub q: f32,
}

/// Single-band compressor params. Precomputed sanitised values; the
/// fold thread feeds these directly into [`crate::dsp::Compressor::set`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompressorParams {
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub knee_db: f32,
    pub makeup_db: f32,
}

/// Look-ahead limiter params.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimiterParams {
    pub ceiling_db: f32,
    pub release_ms: f32,
}

/// GOAT-8 — noise gate params (feeds [`crate::dsp::NoiseGate::set`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoiseGateParams {
    /// Open threshold in dBFS.
    pub threshold_db: f32,
    /// Closed attenuation in dB (e.g. -60 ≈ silence).
    pub range_db: f32,
    /// Attack (open) time, ms.
    pub attack_ms: f32,
    /// Release (close) time, ms.
    pub release_ms: f32,
}

/// GOAT-8 — de-hum params: a notch at `base_hz` plus its harmonics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeHumParams {
    /// Mains fundamental — 50 or 60 Hz.
    pub base_hz: f32,
    /// Number of notches (fundamental + harmonics), clamped 1..=10.
    pub harmonics: u8,
    /// Notch Q (narrowness); higher = tighter/deeper.
    pub q: f32,
}

/// Per-track DSP state — lives on the fold thread, NOT in the snapshot.
/// One `TrackDspState` per audio track, keyed by track uuid in a
/// HashMap owned by [`fold_loop`]. The structure caches the DSP units
/// the chain currently uses; `reconcile_with` is the function that
/// reuses-or-rebuilds them when the snapshot changes.
pub struct TrackDspState {
    chain: Vec<EffectUnit>,
}

impl TrackDspState {
    pub fn new() -> Self {
        Self { chain: Vec::new() }
    }

    /// Bring the cached DSP chain in line with the snapshot. Units at
    /// the same position with the same variant tag are KEPT (only
    /// their params are pushed in via `set`, preserving state). Any
    /// other change rebuilds the affected slots.
    pub fn reconcile_with(
        &mut self,
        spec: &[AudioEffectKind],
        sample_rate: f32,
        mut host: Option<&mut RemotePluginHost>,
    ) {
        // Recover from a sidecar crash BEFORE computing the generation:
        // poll_recovery respawns a dead sidecar (bumping the generation),
        // which makes the remote slots below detect staleness and
        // re-instantiate. Without this a crashed plugin would bypass
        // forever (nothing else advances the generation).
        if let Some(h) = host.as_deref_mut() {
            h.poll_recovery();
        }
        // Shrinking the chain drops tail slots — destroy any out-of-process
        // instances they hold so the sidecar doesn't leak them.
        if self.chain.len() > spec.len() {
            for old in self.chain.drain(spec.len()..) {
                Self::destroy_remote(old, host.as_deref_mut());
            }
        }
        if self.chain.len() != spec.len() {
            self.chain.resize_with(spec.len(), || EffectUnit::Passthrough);
        }
        // The host's current generation (Copy) — used to detect a sidecar
        // respawn so out-of-process slots re-instantiate.
        let host_gen = host.as_deref().map(|h| h.generation());
        for (i, entry) in spec.iter().enumerate() {
            let needs_rebuild = match (&self.chain[i], entry) {
                (EffectUnit::Eq6(_), AudioEffectKind::Eq6 { .. }) => false,
                (EffectUnit::Compressor(_), AudioEffectKind::Compressor { .. }) => false,
                (EffectUnit::Limiter(_), AudioEffectKind::Limiter { .. }) => false,
                (EffectUnit::NoiseGate(_), AudioEffectKind::NoiseGate { .. }) => false,
                (EffectUnit::DeHum(_), AudioEffectKind::DeHum { .. }) => false,
                // Rebuild a plugin on identity change or (for remote) a
                // sidecar respawn. A bare enable/disable toggle keeps the
                // warm instance. Param changes are NOT a rebuild for
                // remote slots — re-spawning would re-`dlopen` the plugin
                // and leak the old instance on every knob tick; params are
                // pushed live via `set_param` below. Built-ins, which read
                // params at construction and rebuild cheaply in-process,
                // DO rebuild on a param change.
                (
                    EffectUnit::Plugin(slot),
                    AudioEffectKind::Plugin {
                        format_str,
                        id,
                        params,
                        ..
                    },
                ) => {
                    let identity_changed = slot.format_str != *format_str || slot.id != *id;
                    let stale = slot.remote_stale(host_gen);
                    match &slot.kind {
                        SlotKind::Local(_) => {
                            identity_changed || slot.params != *params || stale
                        }
                        SlotKind::Remote(_) | SlotKind::Bypass => identity_changed || stale,
                    }
                }
                _ => true,
            };
            if needs_rebuild {
                // Tear down a replaced remote instance first.
                let old = std::mem::replace(&mut self.chain[i], EffectUnit::Passthrough);
                Self::destroy_remote(old, host.as_deref_mut());
                self.chain[i] = EffectUnit::new(entry, sample_rate, host.as_deref_mut());
            }
            // Push the latest params; this updates without resetting
            // the DSP state.
            match (&mut self.chain[i], entry) {
                (EffectUnit::Eq6(bands), AudioEffectKind::Eq6 { bands: bp, .. }) => {
                    for (b, p) in bands.iter_mut().zip(bp.iter()) {
                        b.set(p.shape, sample_rate, p.freq_hz, p.gain_db, p.q);
                    }
                }
                (EffectUnit::Compressor(c), AudioEffectKind::Compressor { params, .. }) => {
                    c.set(
                        params.threshold_db,
                        params.ratio,
                        params.attack_ms,
                        params.release_ms,
                        params.knee_db,
                        params.makeup_db,
                        sample_rate,
                    );
                }
                (EffectUnit::Limiter(lim), AudioEffectKind::Limiter { params, .. }) => {
                    lim.set(params.ceiling_db, params.release_ms, sample_rate);
                }
                (EffectUnit::NoiseGate(g), AudioEffectKind::NoiseGate { params, .. }) => {
                    g.set(
                        params.threshold_db,
                        params.range_db,
                        params.attack_ms,
                        params.release_ms,
                        sample_rate,
                    );
                }
                (EffectUnit::DeHum(notches), AudioEffectKind::DeHum { params, .. }) => {
                    configure_dehum(notches, params, sample_rate);
                }
                (EffectUnit::Plugin(slot), AudioEffectKind::Plugin { params, .. }) => {
                    // Remote slots apply a param change live via set_param
                    // (no rebuild). Built-ins already absorbed it via the
                    // rebuild above; nothing to push.
                    if slot.params != *params {
                        if let SlotKind::Remote(rs) = &slot.kind {
                            if let (Some(h), Some((inst, gen))) =
                                (host.as_deref_mut(), rs.instance)
                            {
                                for (pid, val) in params.iter() {
                                    h.set_param(inst, gen, *pid, *val);
                                }
                            }
                        }
                        slot.params = params.clone();
                    }
                }
                _ => {}
            }
        }
    }

    /// Destroy the out-of-process instance an [`EffectUnit`] holds (if
    /// any), so dropping a slot doesn't leak it in the sidecar.
    fn destroy_remote(unit: EffectUnit, host: Option<&mut RemotePluginHost>) {
        if let EffectUnit::Plugin(slot) = unit {
            if let SlotKind::Remote(rs) = slot.kind {
                if let (Some(h), Some((inst, gen))) = (host, rs.instance) {
                    h.destroy(inst, gen);
                }
            }
        }
    }

    /// Destroy every out-of-process instance in this chain. The fold loop
    /// calls this before dropping a track's DSP state (track removed /
    /// effects cleared / shutdown) so the sidecar reclaims them promptly.
    pub fn destroy_all_remote(&mut self, mut host: Option<&mut RemotePluginHost>) {
        for unit in self.chain.drain(..) {
            Self::destroy_remote(unit, host.as_deref_mut());
        }
    }

    /// Apply the chain to one stereo sample. Disabled entries bypass.
    /// `host` is the out-of-process plugin connection (CLAP/LV2 slots run
    /// through it); built-in/DSP slots ignore it.
    ///
    /// Real-time note: a remote slot does a synchronous IPC round-trip to
    /// the sidecar once per [`crate::audio_plugin::DEFAULT_BLOCK`] samples.
    /// This runs on the fold thread — a BUFFERED producer feeding a ~250 ms
    /// master ring, not the cpal callback — so the per-block trip (sub-ms
    /// normally) is well within budget for a handful of plugins, and a hung
    /// plugin is bypassed within ~135 ms (process timeout + watchdog poll),
    /// comfortably under the ring depth. Pathologically many plugins with a
    /// slow sidecar could still erode that headroom; the future refinement
    /// is to move remote processing onto its own thread + ring so the fold
    /// thread never blocks on IPC. See [`RemoteSlot`].
    #[inline]
    pub fn tick_stereo(
        &mut self,
        spec: &[AudioEffectKind],
        mut l: f32,
        mut r: f32,
        mut host: Option<&mut RemotePluginHost>,
    ) -> (f32, f32) {
        for (unit, entry) in self.chain.iter_mut().zip(spec.iter()) {
            let enabled = match entry {
                AudioEffectKind::Eq6 { enabled, .. }
                | AudioEffectKind::Compressor { enabled, .. }
                | AudioEffectKind::Limiter { enabled, .. }
                | AudioEffectKind::NoiseGate { enabled, .. }
                | AudioEffectKind::DeHum { enabled, .. }
                | AudioEffectKind::Plugin { enabled, .. } => *enabled,
            };
            if !enabled {
                // A disabled plugin must not retain a partial block or
                // stale output across the gap — flush its accumulator so a
                // re-enable starts clean.
                if let EffectUnit::Plugin(slot) = unit {
                    slot.reset();
                }
                continue;
            }
            match (unit, entry) {
                (EffectUnit::Passthrough, _) => {}
                (EffectUnit::Eq6(bands), AudioEffectKind::Eq6 { bands: band_specs, .. }) => {
                    for (b, p) in bands.iter_mut().zip(band_specs.iter()) {
                        if p.enabled {
                            let (nl, nr) = b.tick_stereo(l, r);
                            l = nl;
                            r = nr;
                        }
                    }
                }
                (EffectUnit::Compressor(c), AudioEffectKind::Compressor { .. }) => {
                    let (nl, nr) = c.tick_stereo(l, r);
                    l = nl;
                    r = nr;
                }
                (EffectUnit::Limiter(lim), AudioEffectKind::Limiter { .. }) => {
                    let (nl, nr) = lim.tick_stereo(l, r);
                    l = nl;
                    r = nr;
                }
                (EffectUnit::NoiseGate(g), AudioEffectKind::NoiseGate { .. }) => {
                    let (nl, nr) = g.tick_stereo(l, r);
                    l = nl;
                    r = nr;
                }
                (EffectUnit::DeHum(notches), AudioEffectKind::DeHum { .. }) => {
                    for b in notches.iter_mut() {
                        let (nl, nr) = b.tick_stereo(l, r);
                        l = nl;
                        r = nr;
                    }
                }
                (EffectUnit::Plugin(slot), AudioEffectKind::Plugin { .. }) => {
                    match &mut slot.kind {
                        SlotKind::Local(Some(proc)) => {
                            let (nl, nr) = proc.tick(l, r);
                            l = nl;
                            r = nr;
                        }
                        SlotKind::Remote(rs) => {
                            let (nl, nr) = rs.tick(l, r, host.as_deref_mut());
                            l = nl;
                            r = nr;
                        }
                        // Local(None) / Bypass → unknown plugin / no
                        // sidecar → pass through unchanged.
                        SlotKind::Local(None) | SlotKind::Bypass => {}
                    }
                }
                // Mismatched variant (e.g. spec changed but reconcile
                // not yet called). Silently bypass — the next reconcile
                // tick will rebuild correctly.
                _ => {}
            }
        }
        (l, r)
    }
}

impl Default for TrackDspState {
    fn default() -> Self {
        Self::new()
    }
}

/// A fold-thread audio plugin slot: the processor plus the inputs it was
/// built from (for cheap rebuild detection on reconcile). Built-in
/// effects run in-process; CLAP runs out-of-process in the sandbox
/// sidecar (a crash there bypasses the slot instead of killing the
/// editor).
struct PluginSlot {
    format_str: String,
    id: String,
    params: Vec<(u32, f32)>,
    kind: SlotKind,
}

/// How a [`PluginSlot`] processes audio.
enum SlotKind {
    /// In-process built-in effect (gain / identity). `None` = unknown id
    /// → the slot bypasses.
    Local(Option<crate::audio_plugin::BlockProcessor>),
    /// Out-of-process CLAP (or LV2) via the sandbox sidecar.
    Remote(RemoteSlot),
    /// No processor available (no sidecar / unknown format) → bypass.
    Bypass,
}

/// Out-of-process plugin slot: a block accumulator that ships each full
/// block to the sidecar via [`RemotePluginHost`] and queues the result.
/// Introduces block latency (parity with the in-process
/// [`crate::audio_plugin::BlockProcessor`]); a sidecar crash / hang makes
/// `process` return `None` and the block passes through unchanged.
struct RemoteSlot {
    /// `(instance_id, generation)` from the host; `None` if instantiate
    /// failed (the slot then bypasses).
    instance: Option<(Uuid, u64)>,
    /// Host generation at which instantiate was last attempted, so we
    /// only retry on a respawn (avoids hammering a failing bundle).
    attempted_gen: Option<u64>,
    block: usize,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    fill: usize,
    /// Interleaved scratch reused across blocks (no per-block alloc here).
    scratch: Vec<f32>,
    out_l: VecDeque<f32>,
    out_r: VecDeque<f32>,
}

impl RemoteSlot {
    fn new(block: usize, instance: Option<(Uuid, u64)>, attempted_gen: Option<u64>) -> Self {
        Self {
            instance,
            attempted_gen,
            block,
            in_l: vec![0.0; block],
            in_r: vec![0.0; block],
            fill: 0,
            scratch: vec![0.0; block * 2],
            out_l: VecDeque::with_capacity(block * 2),
            out_r: VecDeque::with_capacity(block * 2),
        }
    }

    /// Process one stereo sample (block-buffered). `host` is the live
    /// sidecar connection; `None` (or a dead/failed instance) → the slot
    /// passes audio through unchanged.
    #[inline]
    fn tick(&mut self, l: f32, r: f32, host: Option<&mut RemotePluginHost>) -> (f32, f32) {
        self.in_l[self.fill] = l;
        self.in_r[self.fill] = r;
        self.fill += 1;
        if self.fill >= self.block {
            self.flush(host);
            self.fill = 0;
        }
        match (self.out_l.pop_front(), self.out_r.pop_front()) {
            (Some(a), Some(b)) => (a, b),
            _ => (0.0, 0.0), // priming silence until the first block lands
        }
    }

    /// Discard buffered input/output so a re-enable starts on a clean
    /// block boundary (no stale pre-disable samples, no mid-block resume).
    fn reset(&mut self) {
        self.fill = 0;
        self.out_l.clear();
        self.out_r.clear();
    }

    fn flush(&mut self, host: Option<&mut RemotePluginHost>) {
        let n = self.block;
        for i in 0..n {
            self.scratch[2 * i] = self.in_l[i];
            self.scratch[2 * i + 1] = self.in_r[i];
        }
        let processed = match (host, self.instance) {
            (Some(h), Some((id, gen))) => h.process(id, gen, &self.scratch),
            _ => None,
        };
        match processed {
            Some(out) if out.len() == n * 2 => {
                for i in 0..n {
                    self.out_l.push_back(out[2 * i]);
                    self.out_r.push_back(out[2 * i + 1]);
                }
            }
            // No host / crash / wrong length → passthrough unchanged.
            _ => {
                for i in 0..n {
                    self.out_l.push_back(self.in_l[i]);
                    self.out_r.push_back(self.in_r[i]);
                }
            }
        }
    }
}

enum EffectUnit {
    Passthrough,
    Eq6(Box<[crate::dsp::Biquad; 6]>),
    Compressor(Box<crate::dsp::Compressor>),
    Limiter(Box<crate::dsp::Limiter>),
    /// GOAT-8 — noise gate.
    NoiseGate(Box<crate::dsp::NoiseGate>),
    /// GOAT-8 — de-hum notch comb (one biquad per harmonic).
    DeHum(Vec<crate::dsp::Biquad>),
    /// Phase 7.10 / 7.11 — real in-process audio plugin (built-in /
    /// CLAP / LV2), block-processed via
    /// [`crate::audio_plugin::BlockProcessor`].
    Plugin(Box<PluginSlot>),
}

impl PluginSlot {
    /// Build a slot for a plugin spec. Built-in formats host in-process;
    /// CLAP/LV2 go out-of-process through `host` (or bypass when no
    /// sidecar is available).
    fn new(
        format_str: &str,
        id: &str,
        params: &[(u32, f32)],
        sample_rate: f32,
        host: Option<&mut RemotePluginHost>,
    ) -> Self {
        let block = crate::audio_plugin::DEFAULT_BLOCK;
        let kind = match format_str {
            "builtin" => SlotKind::Local(crate::audio_plugin::instantiate(
                "builtin",
                id,
                params,
                sample_rate,
                block,
            )),
            // Everything third-party hosts out-of-process so a plugin
            // crash can't take down the editor.
            "clap" | "lv2" => match host {
                Some(h) => {
                    let descriptor = vance_audio_plugins::PluginDescriptor {
                        format: if format_str == "clap" {
                            vance_audio_plugins::PluginFormat::Clap
                        } else {
                            vance_audio_plugins::PluginFormat::Lv2
                        },
                        id: id.to_string(),
                        name: String::new(),
                        vendor: String::new(),
                        runnable: true,
                    };
                    let inst = h.instantiate(descriptor);
                    // The generation we attempted under: the one the
                    // instance was created at, or (on failure) the host's
                    // current generation — so we only retry on a respawn.
                    let attempted = Some(inst.map(|(_, g)| g).unwrap_or_else(|| h.generation()));
                    // Apply the INITIAL parameter values now — reconcile's
                    // diff-push only fires on later changes, so a plugin
                    // added with preset params (or loaded from a project)
                    // would otherwise keep its defaults.
                    if let Some((id_uuid, gen)) = inst {
                        for (pid, val) in params {
                            h.set_param(id_uuid, gen, *pid, *val);
                        }
                    }
                    SlotKind::Remote(RemoteSlot::new(block, inst, attempted))
                }
                None => {
                    tracing::warn!(
                        format = format_str,
                        id,
                        "no plugin sidecar — CLAP/LV2 effect bypassed"
                    );
                    SlotKind::Bypass
                }
            },
            other => {
                tracing::warn!(format = other, id, "unknown plugin format — bypassed");
                SlotKind::Bypass
            }
        };
        PluginSlot {
            format_str: format_str.to_string(),
            id: id.to_string(),
            params: params.to_vec(),
            kind,
        }
    }

    /// Whether an out-of-process slot needs re-instantiation because the
    /// sidecar generation changed (a respawn after a crash/hang, or a
    /// host appearing where there was none). Local/bypass slots never go
    /// stale this way.
    fn remote_stale(&self, host_gen: Option<u64>) -> bool {
        match &self.kind {
            SlotKind::Remote(rs) => rs.attempted_gen != host_gen,
            _ => false,
        }
    }

    /// Flush the slot's block accumulator (called when the effect is
    /// disabled) so a later re-enable can't emit stale buffered samples or
    /// resume mid-block.
    fn reset(&mut self) {
        match &mut self.kind {
            SlotKind::Local(Some(proc)) => proc.reset(),
            SlotKind::Remote(rs) => rs.reset(),
            SlotKind::Local(None) | SlotKind::Bypass => {}
        }
    }
}

/// GOAT-8 — (re)configure a de-hum notch comb: one notch biquad per
/// harmonic at `base_hz × k`, dropping any notch at/above Nyquist.
/// Resizes the vec in place; existing biquads keep their filter state so
/// a knob drag doesn't click.
fn configure_dehum(notches: &mut Vec<crate::dsp::Biquad>, p: &DeHumParams, sample_rate: f32) {
    let n = (p.harmonics.clamp(1, 10)) as usize;
    let q = p.q.clamp(0.5, 40.0);
    if notches.len() != n {
        notches.resize(n, crate::dsp::Biquad::identity());
    }
    for (i, b) in notches.iter_mut().enumerate() {
        let f = p.base_hz * (i as f32 + 1.0);
        // set() falls back to identity (pass-through) when f >= Nyquist.
        b.set(crate::dsp::BiquadShape::Notch, sample_rate, f, 0.0, q);
    }
}

impl EffectUnit {
    fn new(
        spec: &AudioEffectKind,
        sample_rate: f32,
        host: Option<&mut RemotePluginHost>,
    ) -> Self {
        match spec {
            AudioEffectKind::Eq6 { bands, .. } => {
                let mut units = Box::new([crate::dsp::Biquad::identity(); 6]);
                for (b, p) in units.iter_mut().zip(bands.iter()) {
                    b.set(p.shape, sample_rate, p.freq_hz, p.gain_db, p.q);
                }
                EffectUnit::Eq6(units)
            }
            AudioEffectKind::Compressor { params, .. } => {
                let mut c = Box::new(crate::dsp::Compressor::passthrough());
                c.set(
                    params.threshold_db,
                    params.ratio,
                    params.attack_ms,
                    params.release_ms,
                    params.knee_db,
                    params.makeup_db,
                    sample_rate,
                );
                EffectUnit::Compressor(c)
            }
            AudioEffectKind::Limiter { params, .. } => {
                let mut lim = Box::new(crate::dsp::Limiter::new(sample_rate, 3.0));
                lim.set(params.ceiling_db, params.release_ms, sample_rate);
                EffectUnit::Limiter(lim)
            }
            AudioEffectKind::NoiseGate { params, .. } => {
                let mut g = Box::new(crate::dsp::NoiseGate::passthrough());
                g.set(
                    params.threshold_db,
                    params.range_db,
                    params.attack_ms,
                    params.release_ms,
                    sample_rate,
                );
                EffectUnit::NoiseGate(g)
            }
            AudioEffectKind::DeHum { params, .. } => {
                let mut notches = Vec::new();
                configure_dehum(&mut notches, params, sample_rate);
                EffectUnit::DeHum(notches)
            }
            AudioEffectKind::Plugin {
                format_str,
                id,
                params,
                ..
            } => EffectUnit::Plugin(Box::new(PluginSlot::new(
                format_str,
                id,
                params,
                sample_rate,
                host,
            ))),
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
    /// Phase 7.7: ordered post-clip-fold insert chain — parametric EQ,
    /// compressor, limiter. The fold thread maps these specs onto
    /// stateful DSP units stored in its own [`TrackDspState`]; the
    /// state survives snapshot updates so the user can tweak a knob
    /// mid-signal without resetting the compressor envelope or
    /// flushing the biquad delay lines.
    pub audio_effects: Vec<AudioEffectKind>,
    /// GOAT-4: the submix bus this track's main output routes to, or
    /// `None` for the master. Applied as a gain chain AFTER the track's
    /// own insert DSP (so it composes correctly with a nonlinear
    /// compressor/limiter on the track).
    pub output_bus: Option<Uuid>,
    /// GOAT-4: aux sends — extra taps to other buses, IN ADDITION to the
    /// main output. A `pre_fader` send taps the pre-fader sum (before the
    /// track gain); otherwise it taps the post-fader, post-insert signal.
    /// See [`SendMix`]. Sends to a missing bus are dropped at fold time.
    pub sends: Vec<SendMix>,
}

/// GOAT-4 — one aux send inside a [`MixerSnapshot`], precomputed from a
/// `vance_timeline::Send` at snapshot-build time (`level_db` → linear).
#[derive(Clone, Debug, PartialEq)]
pub struct SendMix {
    /// Destination bus uuid.
    pub target: Uuid,
    /// Send level, linear (precomputed from dB at build time).
    pub level_linear: f32,
    /// Tap point: `true` = before the source's fader (and inserts, for a
    /// track; after inserts, before the fader, for a bus); `false` =
    /// post-fader. See [`fold_bus_graph`] for the exact bus tap point.
    pub pre_fader: bool,
}

/// GOAT-4 — per-bus mix state inside a [`MixerSnapshot`]. v1 models the
/// bus FADER (gain), mute, and main-output routing; bus pan + bus insert
/// DSP are a follow-up.
#[derive(Clone, Debug, PartialEq)]
pub struct BusMix {
    /// Bus fader gain, linear (precomputed from dB at build time).
    pub gain_linear: f32,
    /// Per-channel BALANCE multipliers `[L, R]` precomputed from the bus
    /// pan via [`bus_balance`]. Center = `[1, 1]` (unity — adding a
    /// centered bus never changes level), full-pan zeroes the opposite
    /// channel. (A stereo submix uses balance, not the equal-power pan law
    /// clips use.)
    pub pan_linear: [f32; 2],
    /// Bus mute — silences everything routed through it.
    pub mute: bool,
    /// Where the bus's main output goes: another bus, or `None` = master.
    pub output_bus: Option<Uuid>,
    /// GOAT-4: ordered insert DSP chain for this bus (EQ / comp / limiter
    /// / plugins) — same units as a track's. Applied to the summed bus
    /// input BEFORE the bus fader + pan, with stateful DSP owned by the
    /// fold thread (a knob drag doesn't reset the bus compressor).
    pub audio_effects: Vec<AudioEffectKind>,
    /// GOAT-4: aux sends from this bus to other buses, in addition to the
    /// main output. A `pre_fader` send taps post-insert / pre-fader; a
    /// post-fader send taps after the bus fader + pan. See [`SendMix`].
    pub sends: Vec<SendMix>,
}

/// Stereo BALANCE multipliers `[L, R]` for a bus pan in `[-1, 1]`. Center
/// (`0`) is unity `[1, 1]`; panning right attenuates the left channel
/// linearly (and vice-versa). Distinct from the equal-power pan law used
/// for clips — a submix bus shouldn't lose 3 dB just for existing.
pub fn bus_balance(pan: f32) -> [f32; 2] {
    let p = pan.clamp(-1.0, 1.0);
    let l = if p > 0.0 { 1.0 - p } else { 1.0 };
    let r = if p < 0.0 { 1.0 + p } else { 1.0 };
    [l, r]
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
    /// Fade-in curve shape. All three variants ship real
    /// implementations since 7.5 M2 — see [`FadeCurve`].
    pub fade_in_curve: FadeCurve,
    /// Phase 7.4 M3: fade-out duration in CLIP-RELATIVE seconds. The
    /// fade region runs from `clip_duration_seconds - fade_out_duration`
    /// to the clip's end. `0.0` = no fade-out.
    pub fade_out_duration_seconds: f64,
    /// Fade-out curve shape. See [`Self::fade_in_curve`].
    pub fade_out_curve: FadeCurve,
    /// Phase 7.6: equal-power pan multipliers `[L_gain, R_gain]`
    /// PRECOMPOSED at snapshot build time from the track's and clip's
    /// pan values: `effective_pan = clamp(track.pan + clip.pan, -1, +1)`,
    /// then `angle = (effective_pan + 1) · π/4`, then
    /// `[cos(angle), sin(angle)]`. The fold thread applies these as a
    /// per-channel BALANCE (`out_L = sample_L · pan_linear[0]`,
    /// `out_R = sample_R · pan_linear[1]`) so the stereo image is
    /// preserved at center (`[√½, √½]`, the equal-power center) and
    /// fully panned positions zero out the opposite channel.
    pub pan_linear: [f32; 2],
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
    /// GOAT-4: per-bus mix params, keyed by bus uuid. Empty = no buses
    /// (every track straight to master, the Phase-7 topology).
    pub buses: HashMap<Uuid, BusMix>,
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
            buses: HashMap::new(),
            solo_anywhere: false,
            master_gain_linear: 1.0,
        }
    }

    /// GOAT-4: product of bus fader gains along a track/bus output chain
    /// to the master. `None` (route to master) returns 1.0. Returns 0.0 if
    /// any bus on the path is muted, missing, or the chain exceeds a sane
    /// depth (defensive cycle guard — the edit layer already rejects
    /// cycles). Applied AFTER track-insert DSP in the fold.
    pub fn bus_chain_gain(&self, mut bus: Option<Uuid>) -> f32 {
        let mut g = 1.0;
        let mut steps = 0usize;
        while let Some(id) = bus {
            steps += 1;
            // A valid (acyclic) output chain visits at most `buses.len()`
            // buses before reaching master; exceeding that means a cycle.
            if steps > self.buses.len() {
                return 0.0;
            }
            let Some(b) = self.buses.get(&id) else {
                return 0.0;
            };
            if b.mute {
                return 0.0;
            }
            g *= b.gain_linear;
            bus = b.output_bus;
        }
        g
    }

    /// GOAT-4: per-channel `[L, R]` multipliers along a track/bus output
    /// chain to master — composes each bus's fader gain AND balance pan.
    /// `None` → `[1, 1]`. `[0, 0]` on mute / missing bus / cycle. Used by
    /// the live fold so the console pan knob is audible.
    pub fn bus_chain_lr(&self, mut bus: Option<Uuid>) -> [f32; 2] {
        let mut lr = [1.0f32, 1.0];
        let mut steps = 0usize;
        while let Some(id) = bus {
            steps += 1;
            // Bounded by the bus count (see `bus_chain_gain`): a longer
            // walk means a cycle, so bail to silence.
            if steps > self.buses.len() {
                return [0.0, 0.0];
            }
            let Some(b) = self.buses.get(&id) else {
                return [0.0, 0.0];
            };
            if b.mute {
                return [0.0, 0.0];
            }
            lr[0] *= b.gain_linear * b.pan_linear[0];
            lr[1] *= b.gain_linear * b.pan_linear[1];
            bus = b.output_bus;
        }
        lr
    }

    /// GOAT-4: bus processing order for [`fold_bus_graph`] — UPSTREAM
    /// buses first, so each bus's full input has accumulated before it
    /// runs its inserts + fader. A bus "feeds" every bus it routes to:
    /// its main `output_bus` AND every aux `send` target. Order is the
    /// longest-path level (`1 + max(level over fed buses)`, master = 0)
    /// descending — so a bus is always processed before any bus it feeds,
    /// including a post-fader send to a same-tier return bus. Uuid breaks
    /// ties for a deterministic frame-to-frame order; a defensive depth
    /// cap keeps a (rejected-but-present) cycle from looping forever.
    pub fn bus_process_order(&self) -> Vec<Uuid> {
        fn level(
            buses: &HashMap<Uuid, BusMix>,
            id: Uuid,
            depth: usize,
            cache: &mut HashMap<Uuid, u32>,
        ) -> u32 {
            // A DAG's longest path is bounded by the node count; deeper
            // recursion means a cycle (the edit layer rejects those, but
            // the snapshot stays defensive). With memoization the live
            // recursion depth never exceeds `buses.len()` for any acyclic
            // graph, so this orders ANY valid depth correctly — no fixed
            // cap that could misorder deep submix chains.
            if depth > buses.len() {
                return 0;
            }
            if let Some(&l) = cache.get(&id) {
                return l;
            }
            let mut max_fed = 0u32;
            if let Some(b) = buses.get(&id) {
                if let Some(out) = b.output_bus {
                    if buses.contains_key(&out) {
                        max_fed = max_fed.max(level(buses, out, depth + 1, cache));
                    }
                }
                for s in &b.sends {
                    if buses.contains_key(&s.target) {
                        max_fed = max_fed.max(level(buses, s.target, depth + 1, cache));
                    }
                }
            }
            let l = 1 + max_fed;
            cache.insert(id, l);
            l
        }
        let mut cache: HashMap<Uuid, u32> = HashMap::new();
        let mut ids: Vec<Uuid> = self.buses.keys().copied().collect();
        for id in &ids {
            level(&self.buses, *id, 0, &mut cache);
        }
        ids.sort_by(|a, b| cache[b].cmp(&cache[a]).then_with(|| a.cmp(b)));
        ids
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
        // GOAT-4: route through the track's output-bus chain (gain/mute)
        // before the master fader. 1.0 when the track goes straight to
        // master (the common case).
        let bus_g = self.bus_chain_gain(track.output_bus);
        track_g * clip_g * fade_in_g * fade_out_g * bus_g * self.master_gain_linear
    }

    /// GOAT-4 — a clip's PRE-FADER gain at `t`: `clip_gain × fade_in ×
    /// fade_out`, with the same gating as [`Self::effective_gain_at`]
    /// (clip enabled, parent track present/un-muted, solo) but WITHOUT the
    /// track fader, bus chain, or master. The offline bounce builds a
    /// pre-fader per-track sum from this so it can mirror the live fold's
    /// fader → insert → send → bus pipeline (pre-fader sends tap here).
    pub fn clip_pre_fader_gain_at(&self, clip_id: &Uuid, t_timeline_seconds: f64) -> f32 {
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
        let t_clip = t_timeline_seconds - clip.timeline_in_seconds;
        let clip_g = if clip.gain_lane.is_empty() {
            clip.gain_linear
        } else {
            eval_gain_lane(&clip.gain_lane, t_clip)
        };
        let fade_in_g =
            eval_fade_in_audio(clip.fade_in_duration_seconds, clip.fade_in_curve, t_clip);
        let fade_out_g = eval_fade_out_audio(
            clip.fade_out_duration_seconds,
            clip.fade_out_curve,
            clip.clip_duration_seconds,
            t_clip,
        );
        clip_g * fade_in_g * fade_out_g
    }

    /// GOAT-4 — a track's fader gain at `t` (lane-aware), or `1.0` when the
    /// track isn't in the snapshot. The offline bounce applies this as the
    /// per-track fader stage between the pre-fader sum and the insert DSP.
    pub fn track_gain_at(&self, track_id: &Uuid, t_timeline_seconds: f64) -> f32 {
        self.tracks.get(track_id).map_or(1.0, |t| {
            if t.gain_lane.is_empty() {
                t.gain_linear
            } else {
                eval_gain_lane(&t.gain_lane, t_timeline_seconds)
            }
        })
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
/// Phase 7.5 M1 — TEST-ONLY helper. The live fold thread inlines an
/// equivalent computation so it can also split out the per-track
/// pre-master intermediate the metering accumulators need. Kept here
/// because every effective-gain test in this file asserts against
/// this function; if the fold-loop math drifts from it, the assertion
/// surface catches the regression.
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

/// GOAT-4 — fold the bus graph for one frame. The caller has already
/// summed each track's main output and each track's aux sends into
/// `bus_in` (per-bus input accumulators) and `master` (direct-to-master
/// sum). This walks the buses in `order` (UPSTREAM first; use
/// [`MixerSnapshot::bus_process_order`]) and for each bus:
///
/// 1. runs its insert DSP via `insert(bus_id, input)` — the realtime
///    path threads stateful per-bus DSP through this closure; tests pass
///    identity,
/// 2. fires its PRE-FADER sends (post-insert, independent of the bus
///    fader / mute),
/// 3. applies the bus fader + balance pan (skipped entirely when the bus
///    is muted — mute silences the main output AND post-fader sends),
/// 4. fires its POST-FADER sends, and
/// 5. routes the main output to the bus's own `output_bus` (another bus,
///    accumulated for a later `order` step) or to `master`.
///
/// Sends to a missing bus, or to a bus already processed this frame (an
/// upstream target — only possible via a send cycle the edit layer
/// rejects), are dropped: the re-created accumulator is never drained.
/// On return `bus_in` is empty of processed buses and `master` holds the
/// pre-master-fader stereo sum.
pub fn fold_bus_graph(
    snap: &MixerSnapshot,
    order: &[Uuid],
    bus_in: &mut HashMap<Uuid, [f32; 2]>,
    master: &mut [f32; 2],
    mut insert: impl FnMut(Uuid, [f32; 2]) -> [f32; 2],
) {
    // Add a stereo signal to a send/main destination: a live bus's input
    // accumulator, or `master` for `None`. A missing target is dropped.
    fn route(
        snap: &MixerSnapshot,
        bus_in: &mut HashMap<Uuid, [f32; 2]>,
        master: &mut [f32; 2],
        target: Option<Uuid>,
        sig: [f32; 2],
    ) {
        match target {
            Some(id) if snap.buses.contains_key(&id) => {
                let e = bus_in.entry(id).or_insert([0.0, 0.0]);
                e[0] += sig[0];
                e[1] += sig[1];
            }
            None => {
                master[0] += sig[0];
                master[1] += sig[1];
            }
            // Missing target bus → drop (don't leak to master).
            Some(_) => {}
        }
    }

    for &bus_id in order {
        let Some(bus) = snap.buses.get(&bus_id) else {
            continue;
        };
        let input = bus_in.remove(&bus_id).unwrap_or([0.0, 0.0]);
        let post_insert = insert(bus_id, input);
        // Pre-fader sends tap post-insert, independent of fader + mute.
        for s in &bus.sends {
            if s.pre_fader {
                route(
                    snap,
                    bus_in,
                    master,
                    Some(s.target),
                    [post_insert[0] * s.level_linear, post_insert[1] * s.level_linear],
                );
            }
        }
        if bus.mute {
            continue;
        }
        let out = [
            post_insert[0] * bus.gain_linear * bus.pan_linear[0],
            post_insert[1] * bus.gain_linear * bus.pan_linear[1],
        ];
        // Post-fader sends tap the post-fader output.
        for s in &bus.sends {
            if !s.pre_fader {
                route(
                    snap,
                    bus_in,
                    master,
                    Some(s.target),
                    [out[0] * s.level_linear, out[1] * s.level_linear],
                );
            }
        }
        route(snap, bus_in, master, bus.output_bus, out);
    }
}

/// GOAT-4 — accumulate an aux-send contribution into its target bus's
/// input. A send always targets a specific bus; a send to a bus that
/// isn't in the snapshot is dropped (it never leaks to the master).
#[inline]
fn seed_bus_input(
    snap: &MixerSnapshot,
    bus_in: &mut HashMap<Uuid, [f32; 2]>,
    target: Uuid,
    sig: [f32; 2],
) {
    if snap.buses.contains_key(&target) {
        let e = bus_in.entry(target).or_insert([0.0, 0.0]);
        e[0] += sig[0];
        e[1] += sig[1];
    }
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
    /// Phase 7.5 M1: lock-free metering bus. The fold thread publishes
    /// a new `MeterSnapshot` ~30 times per second; the UI polls it via
    /// a Tauri command. Engine never writes; reads are cheap.
    meter: Arc<ArcSwap<MeterSnapshot>>,
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
        plugin_host_bin: Option<PathBuf>,
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

        // 3. Snapshot bus + master frame counter + metering bus.
        let snapshot = Arc::new(ArcSwap::from_pointee(initial_snapshot));
        let master_frame = Arc::new(AtomicI64::new(0));
        let meter = Arc::new(ArcSwap::from_pointee(MeterSnapshot::default()));
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
        let fold_meter = meter.clone();
        let fold_handle = thread::Builder::new()
            .name("vance-audio.mixer.fold".into())
            .spawn(move || {
                fold_loop(
                    source_streams,
                    command_rx,
                    master_prod,
                    fold_snapshot,
                    fold_master_frame,
                    fold_meter,
                    fold_stop,
                    channels,
                    sample_rate,
                    playback_start_timeline_seconds,
                    plugin_host_bin,
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
            meter,
        })
    }

    /// Phase 7.5 M1: Send + Sync handle to the metering bus. The fold
    /// thread is the sole writer; readers (UI poll path) just load.
    pub fn meter_handle(&self) -> Arc<ArcSwap<MeterSnapshot>> {
        self.meter.clone()
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

/// Per-track and master metering values for one publish frame.
/// Linear amplitude (NOT dB) — the UI converts to dB. Stored in
/// `MeterSnapshot` and published lock-free from the fold thread.
///
/// Phase 7.5 M1.
#[derive(Clone, Copy, Debug, Default)]
pub struct TrackMeter {
    /// Peak abs sample observed on the left channel over the publish window.
    pub peak_l: f32,
    /// Peak abs sample observed on the right channel over the publish window.
    pub peak_r: f32,
    /// RMS over the publish window, left channel.
    pub rms_l: f32,
    /// RMS over the publish window, right channel.
    pub rms_r: f32,
}

/// Snapshot of every metering value the UI needs to draw a meter bar.
/// Published from the fold thread via `ArcSwap` ~30 times per second.
///
/// `generation` is a monotonic counter so the UI can spot dropped
/// frames (e.g. when the fold thread stalls during shutdown).
#[derive(Clone, Debug, Default)]
pub struct MeterSnapshot {
    /// Master bus meter (post-master-gain).
    pub master: TrackMeter,
    /// Per-track meters (post-track-gain, pre-master-gain).
    pub tracks: HashMap<Uuid, TrackMeter>,
    /// Monotonic publication counter. UI uses this to detect "the
    /// numbers haven't changed for a while" without polling at the
    /// fold thread's exact rate.
    pub generation: u64,
    /// Phase 7.8 — ITU-R BS.1770-4 loudness on the master bus.
    /// Default `LoudnessStats::default()` is all `0.0` (NOT `-inf`),
    /// which the UI treats as "no measurement yet" and displays as
    /// "--.- LU". Once playback begins the fold thread overwrites.
    pub loudness: crate::loudness::LoudnessStats,
    /// Phase 7.8 — true-peak (post-master-gain) on the master bus,
    /// linear amplitude. `[L, R]`. UI converts to dBTP.
    pub master_true_peak: [f32; 2],
}

/// Running accumulators the fold thread keeps in-thread (no Arc, no
/// lock). At publish time these collapse into a [`TrackMeter`] and the
/// fold thread resets them for the next window.
#[derive(Clone, Copy, Debug, Default)]
struct MeterAccum {
    peak_l: f32,
    peak_r: f32,
    sum_sq_l: f64,
    sum_sq_r: f64,
    count: u32,
}

impl MeterAccum {
    #[inline]
    fn observe(&mut self, l: f32, r: f32) {
        let al = l.abs();
        let ar = r.abs();
        if al > self.peak_l {
            self.peak_l = al;
        }
        if ar > self.peak_r {
            self.peak_r = ar;
        }
        self.sum_sq_l += (l as f64) * (l as f64);
        self.sum_sq_r += (r as f64) * (r as f64);
        self.count += 1;
    }

    fn into_meter(self) -> TrackMeter {
        if self.count == 0 {
            return TrackMeter::default();
        }
        let n = self.count as f64;
        TrackMeter {
            peak_l: self.peak_l,
            peak_r: self.peak_r,
            rms_l: (self.sum_sq_l / n).sqrt() as f32,
            rms_r: (self.sum_sq_r / n).sqrt() as f32,
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn fold_loop(
    initial_sources: Vec<SourceStream>,
    command_rx: mpsc::Receiver<SourceCommand>,
    mut master_prod: ringbuf::HeapProd<f32>,
    snapshot: Arc<ArcSwap<MixerSnapshot>>,
    master_frame: Arc<AtomicI64>,
    meter_bus: Arc<ArcSwap<MeterSnapshot>>,
    stop: Arc<AtomicBool>,
    channels: usize,
    sample_rate: u32,
    playback_start_timeline_seconds: f64,
    plugin_host_bin: Option<PathBuf>,
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
    // Phase 7.5 M1 — per-frame scratch space for the per-track stereo
    // SUM. Cleared (not reallocated) every frame so the per-track
    // metering accum can `observe` ONCE per output frame per track
    // (instead of once per clip — overlapping clips would otherwise
    // bias peak and inflate the RMS denominator). Adversarial review
    // findings #1 + #15.
    let mut track_frame_sums: HashMap<Uuid, [f32; 2]> = HashMap::new();
    // Phase 7.7 — per-track DSP chain state. One TrackDspState per
    // audio track; reconciled with each snapshot load so a param-only
    // change preserves filter delay-lines + compressor envelopes (a
    // user knob drag must NOT zip the audio). Entries for tracks
    // that have been removed are pruned on the same snapshot load to
    // keep the map bounded.
    let mut track_dsp_states: HashMap<Uuid, TrackDspState> = HashMap::new();
    // GOAT-4 — per-bus input accumulators (reused each frame) + stateful
    // per-bus insert DSP (same lifecycle as `track_dsp_states`). The bus
    // processing order is recomputed once per snapshot load below.
    let mut bus_frame_sums: HashMap<Uuid, [f32; 2]> = HashMap::new();
    let mut bus_dsp_states: HashMap<Uuid, TrackDspState> = HashMap::new();
    let sample_rate_f32 = sample_rate as f32;
    // Out-of-process plugin host (CLAP/LV2). Created here but the sidecar
    // is spawned lazily on the first plugin instantiate, so tracks with no
    // plugins pay nothing. `None` when no sidecar binary is available —
    // CLAP slots then bypass.
    let mut plugin_host: Option<RemotePluginHost> = plugin_host_bin.map(|bin| {
        RemotePluginHost::new(bin, sample_rate_f32, crate::audio_plugin::DEFAULT_BLOCK as u32)
    });
    // Phase 7.8 — master-bus loudness + true-peak. One meter per
    // fold loop instance; the engine's playback start resets this
    // because the AudioMixer is constructed fresh on every play.
    let mut loudness_meter = crate::loudness::LoudnessMeter::new(sample_rate_f32);
    let mut true_peak = crate::loudness::TruePeakDetector::new();
    // Phase 7.5 M1 — per-track + master metering accumulators. Kept on
    // the fold thread (no Arc, no lock); collapsed into a TrackMeter
    // every `meter_publish_period_frames` frames and published via
    // ArcSwap so the UI can poll without touching the fold thread.
    let mut track_accums: HashMap<Uuid, MeterAccum> = HashMap::new();
    let mut master_accum = MeterAccum::default();
    // 30 publishes/sec, capped at >= 1 to avoid div-by-zero on tiny rates.
    let meter_publish_period_frames = (sample_rate / 30).max(1);
    let mut frames_since_publish: u32 = 0;
    let mut meter_generation: u64 = 0;
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
        // Phase 7.7 — reconcile per-track DSP state once per snapshot
        // load, NOT per frame. The hot inner loop just calls
        // `tick_stereo`; spec/state mismatch (added effect, swapped
        // variant) is detected here.
        for (track_id, track) in snap.tracks.iter() {
            if track.audio_effects.is_empty() {
                // Effects cleared → destroy this track's out-of-process
                // instances before dropping its DSP state.
                if let Some(mut st) = track_dsp_states.remove(track_id) {
                    st.destroy_all_remote(plugin_host.as_mut());
                }
            } else {
                let state = track_dsp_states.entry(*track_id).or_default();
                state.reconcile_with(
                    &track.audio_effects,
                    sample_rate_f32,
                    plugin_host.as_mut(),
                );
            }
        }
        // Tracks gone from the snapshot: destroy their remote instances,
        // then drop their DSP state.
        let live_tracks: std::collections::HashSet<Uuid> = snap.tracks.keys().copied().collect();
        let dead_tracks: Vec<Uuid> = track_dsp_states
            .keys()
            .filter(|id| !live_tracks.contains(id))
            .copied()
            .collect();
        for id in dead_tracks {
            if let Some(mut st) = track_dsp_states.remove(&id) {
                st.destroy_all_remote(plugin_host.as_mut());
            }
        }
        // GOAT-4 — reconcile per-bus insert DSP on the same lifecycle as
        // tracks, and recompute the upstream→downstream bus order once
        // per snapshot load (not per frame).
        for (bus_id, bus) in snap.buses.iter() {
            if bus.audio_effects.is_empty() {
                if let Some(mut st) = bus_dsp_states.remove(bus_id) {
                    st.destroy_all_remote(plugin_host.as_mut());
                }
            } else {
                bus_dsp_states.entry(*bus_id).or_default().reconcile_with(
                    &bus.audio_effects,
                    sample_rate_f32,
                    plugin_host.as_mut(),
                );
            }
        }
        let live_buses: std::collections::HashSet<Uuid> = snap.buses.keys().copied().collect();
        let dead_buses: Vec<Uuid> = bus_dsp_states
            .keys()
            .filter(|id| !live_buses.contains(id))
            .copied()
            .collect();
        for id in dead_buses {
            if let Some(mut st) = bus_dsp_states.remove(&id) {
                st.destroy_all_remote(plugin_host.as_mut());
            }
        }
        let bus_order = snap.bus_process_order();
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
            // Phase 7.5 M1: per-track + master fold INLINE so we can
            // accumulate metering data alongside the sum. Mirrors
            // `effective_gain_at` math (track × clip × fade × master)
            // but exposes the per-track intermediate so the meters
            // reflect what the user actually hears at each track row.
            let mut master_pre_master = [0.0f32; 2];
            for (clip_id, samples) in frame_samples.iter() {
                let Some(clip) = snap.clips.get(clip_id) else {
                    continue;
                };
                if !clip.enabled {
                    continue;
                }
                let Some(track) = snap.tracks.get(&clip.track_id) else {
                    continue;
                };
                if track.mute {
                    continue;
                }
                if snap.solo_anywhere && !track.solo {
                    continue;
                }
                // GOAT-4: accumulate the PRE-FADER track sum (clip gain ×
                // fades × pan, but NOT the track fader). The track fader
                // is applied once per track in the stage below — pulling
                // it out is mathematically identical for the main output
                // (Σ clipᵢ·trackG = trackG·Σ clipᵢ) and gives a clean
                // pre-fader tap point for aux sends.
                let t_clip = t_frame - clip.timeline_in_seconds;
                let clip_g = if clip.gain_lane.is_empty() {
                    clip.gain_linear
                } else {
                    eval_gain_lane(&clip.gain_lane, t_clip)
                };
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
                let pre_fader_g = clip_g * fade_in_g * fade_out_g;
                // Phase 7.6: equal-power BALANCE pan. Each output
                // channel reads only its own input channel — preserves
                // stereo image at center (with the standard −3 dB pad)
                // and zeroes the opposite channel at full pan.
                let l = samples[0] * pre_fader_g * clip.pan_linear[0];
                let r = samples[1] * pre_fader_g * clip.pan_linear[1];
                // Accumulate the per-track SUM in the per-frame scratch
                // map — we observe after the inner loop so peak +
                // sum_sq reflect the audible signal, not individual
                // clip contributions.
                let entry = track_frame_sums.entry(clip.track_id).or_insert([0.0, 0.0]);
                entry[0] += l;
                entry[1] += r;
            }
            // Phase 7.7 / GOAT-4: per-track FADER → insert DSP → sends →
            // output routing. The fader (track gain / lane) applies first
            // — preserving the historical "fader then inserts" order — so
            // a pre-fader send taps the raw `track_frame_sums` value and a
            // post-fader send taps the post-insert signal. DSP state lives
            // on the fold thread so a knob drag updates params via
            // `reconcile_with` without resetting filter/compressor state.
            bus_frame_sums.clear();
            for (track_id, pre_fader) in track_frame_sums.drain() {
                let track = snap.tracks.get(&track_id);
                let track_g = track.map_or(1.0, |t| {
                    if t.gain_lane.is_empty() {
                        t.gain_linear
                    } else {
                        eval_gain_lane(&t.gain_lane, t_frame)
                    }
                });
                // Pre-fader sends tap the raw track sum (before the fader).
                if let Some(track) = track {
                    for s in &track.sends {
                        if s.pre_fader {
                            seed_bus_input(
                                &snap,
                                &mut bus_frame_sums,
                                s.target,
                                [pre_fader[0] * s.level_linear, pre_fader[1] * s.level_linear],
                            );
                        }
                    }
                }
                // Fader, then insert DSP.
                let (mut l, mut r) = (pre_fader[0] * track_g, pre_fader[1] * track_g);
                if let Some(track) = track {
                    if !track.audio_effects.is_empty() {
                        let dsp = track_dsp_states.entry(track_id).or_default();
                        let (nl, nr) =
                            dsp.tick_stereo(&track.audio_effects, l, r, plugin_host.as_mut());
                        l = nl;
                        r = nr;
                    }
                }
                // Meter the POST-EFFECT track signal — what the user
                // actually hears at the track row.
                track_accums.entry(track_id).or_default().observe(l, r);
                // Post-fader sends + the main output both tap the post-
                // insert signal; the main output flows into its bus's
                // input accumulator (or master), the buses fold below.
                if let Some(track) = track {
                    for s in &track.sends {
                        if !s.pre_fader {
                            seed_bus_input(
                                &snap,
                                &mut bus_frame_sums,
                                s.target,
                                [l * s.level_linear, r * s.level_linear],
                            );
                        }
                    }
                }
                let out_bus = track.and_then(|t| t.output_bus);
                match out_bus {
                    Some(id) if snap.buses.contains_key(&id) => {
                        let e = bus_frame_sums.entry(id).or_insert([0.0, 0.0]);
                        e[0] += l;
                        e[1] += r;
                    }
                    _ => {
                        master_pre_master[0] += l;
                        master_pre_master[1] += r;
                    }
                }
            }
            // GOAT-4: fold the bus graph — inserts (stateful), fader, pan,
            // sends, and onward routing — into the master sum. The insert
            // closure threads each bus's persistent DSP state.
            fold_bus_graph(
                &snap,
                &bus_order,
                &mut bus_frame_sums,
                &mut master_pre_master,
                |bus_id, sig| {
                    if let Some(bus) = snap.buses.get(&bus_id) {
                        if !bus.audio_effects.is_empty() {
                            let dsp = bus_dsp_states.entry(bus_id).or_default();
                            let (l, r) = dsp.tick_stereo(
                                &bus.audio_effects,
                                sig[0],
                                sig[1],
                                plugin_host.as_mut(),
                            );
                            return [l, r];
                        }
                    }
                    sig
                },
            );
            let master_l = master_pre_master[0] * snap.master_gain_linear;
            let master_r = master_pre_master[1] * snap.master_gain_linear;
            master_accum.observe(master_l, master_r);
            // Phase 7.8 — feed the BS.1770 meter + true-peak detector
            // with the post-master-gain stereo frame. K-weighting and
            // 4× oversampling both add a small constant cost per frame.
            loudness_meter.ingest(master_l, master_r);
            true_peak.ingest(master_l, master_r);
            let _ = master_prod.push_slice(&[master_l, master_r]);

            frames_since_publish += 1;
            if frames_since_publish >= meter_publish_period_frames {
                let mut tracks_out: HashMap<Uuid, TrackMeter> =
                    HashMap::with_capacity(track_accums.len());
                for (id, acc) in track_accums.drain() {
                    tracks_out.insert(id, acc.into_meter());
                }
                meter_generation = meter_generation.wrapping_add(1);
                // Phase 7.8 — pull current BS.1770 stats + drain
                // running true-peak. The integrated value continues
                // accumulating; only the per-publish true-peak resets.
                let loudness = loudness_meter.stats();
                let true_peak_drained = true_peak.drain_peaks();
                meter_bus.store(Arc::new(MeterSnapshot {
                    master: master_accum.into_meter(),
                    tracks: tracks_out,
                    generation: meter_generation,
                    loudness,
                    master_true_peak: true_peak_drained,
                }));
                master_accum = MeterAccum::default();
                frames_since_publish = 0;
            }
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
            audio_effects: Vec::new(),
            output_bus: None,
            sends: Vec::new(),
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
            // Equal-power center: existing tests assert per-clip gain
            // composition WITHOUT pan in mind, so set the pan to
            // unity-pass-through `[1.0, 1.0]` instead of the natural
            // center `[√½, √½]` — that way the existing assertions
            // keep their pre-7.6 numerical truth.
            pan_linear: [1.0, 1.0],
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
    fn fold_frame_routes_through_bus_gain_and_mute() {
        // GOAT-4: a track routed to a -6 dB bus halves its contribution.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let bus = Uuid::new_v4();
        let mut track = unity_track();
        track.output_bus = Some(bus);
        let mut snap = snapshot_with(&[(t, track)], &[(c, unity_clip(t))]);
        snap.buses.insert(
            bus,
            BusMix {
                gain_linear: db_to_linear(-6.0),
                pan_linear: [1.0, 1.0],
                mute: false,
                output_bus: None,
                audio_effects: Vec::new(),
                sends: Vec::new(),
            },
        );
        let out = fold_frame(&snap, &[(c, [1.0, -1.0])]);
        assert!((out[0] - db_to_linear(-6.0)).abs() < 1e-4, "bus gain not applied: {}", out[0]);

        // Muting the bus silences everything routed through it.
        snap.buses.get_mut(&bus).unwrap().mute = true;
        assert_eq!(fold_frame(&snap, &[(c, [1.0, -1.0])]), [0.0, 0.0]);
    }

    #[test]
    fn fold_frame_chains_bus_gains_to_master() {
        // track → busA(-6 dB) → busB(-6 dB) → master ⇒ ×0.25.
        let t = Uuid::new_v4();
        let c = Uuid::new_v4();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut track = unity_track();
        track.output_bus = Some(a);
        let mut snap = snapshot_with(&[(t, track)], &[(c, unity_clip(t))]);
        snap.buses.insert(
            a,
            BusMix { gain_linear: db_to_linear(-6.0), pan_linear: [1.0, 1.0], mute: false, audio_effects: Vec::new(), sends: Vec::new(), output_bus: Some(b) },
        );
        snap.buses.insert(
            b,
            BusMix { gain_linear: db_to_linear(-6.0), pan_linear: [1.0, 1.0], mute: false, audio_effects: Vec::new(), sends: Vec::new(), output_bus: None },
        );
        let expect = db_to_linear(-6.0) * db_to_linear(-6.0);
        let out = fold_frame(&snap, &[(c, [1.0, 0.0])]);
        assert!((out[0] - expect).abs() < 1e-4, "chained bus gain: {} vs {expect}", out[0]);
    }

    #[test]
    fn bus_chain_gain_is_defensive() {
        let mut snap = MixerSnapshot::empty();
        assert_eq!(snap.bus_chain_gain(None), 1.0, "route-to-master is unity");
        assert_eq!(snap.bus_chain_gain(Some(Uuid::new_v4())), 0.0, "missing bus is silent");
        // A→B→A cycle is caught by the depth guard → 0.
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        snap.buses.insert(a, BusMix { gain_linear: 1.0, pan_linear: [1.0, 1.0], mute: false, audio_effects: Vec::new(), sends: Vec::new(), output_bus: Some(b) });
        snap.buses.insert(b, BusMix { gain_linear: 1.0, pan_linear: [1.0, 1.0], mute: false, audio_effects: Vec::new(), sends: Vec::new(), output_bus: Some(a) });
        assert_eq!(snap.bus_chain_gain(Some(a)), 0.0);
    }

    #[test]
    fn bus_balance_and_chain_lr_pan() {
        // Balance law: center unity, full-pan zeroes the opposite channel.
        assert_eq!(bus_balance(0.0), [1.0, 1.0]);
        assert_eq!(bus_balance(1.0), [0.0, 1.0]); // hard right → L=0
        assert_eq!(bus_balance(-1.0), [1.0, 0.0]); // hard left → R=0
        assert_eq!(bus_balance(0.5), [0.5, 1.0]);

        // A hard-left bus passes only the left channel; None is unity.
        let mut snap = MixerSnapshot::empty();
        let bus = Uuid::new_v4();
        snap.buses.insert(
            bus,
            BusMix {
                gain_linear: 1.0,
                pan_linear: bus_balance(-1.0),
                mute: false,
                output_bus: None,
                audio_effects: Vec::new(),
                sends: Vec::new(),
            },
        );
        assert_eq!(snap.bus_chain_lr(Some(bus)), [1.0, 0.0]);
        assert_eq!(snap.bus_chain_lr(None), [1.0, 1.0]);

        // Panned gain composes per channel: -6 dB bus hard-left → [~0.5, 0].
        let g = snap.buses.get_mut(&bus).unwrap();
        g.gain_linear = db_to_linear(-6.0);
        let lr = snap.bus_chain_lr(Some(bus));
        assert!((lr[0] - db_to_linear(-6.0)).abs() < 1e-6);
        assert_eq!(lr[1], 0.0);
    }

    // ---- GOAT-4: bus-graph fold (inserts + sends + topo routing) ----

    fn bmix(gain: f32, output: Option<Uuid>) -> BusMix {
        BusMix {
            gain_linear: gain,
            pan_linear: [1.0, 1.0],
            mute: false,
            output_bus: output,
            audio_effects: Vec::new(),
            sends: Vec::new(),
        }
    }

    #[test]
    fn fold_bus_graph_applies_bus_fader_to_master() {
        let mut snap = MixerSnapshot::empty();
        let bus = Uuid::new_v4();
        snap.buses.insert(bus, bmix(0.5, None));
        let order = snap.bus_process_order();
        // A track main already summed into the bus.
        let mut bus_in = HashMap::from([(bus, [1.0, 1.0])]);
        let mut master = [0.0f32; 2];
        fold_bus_graph(&snap, &order, &mut bus_in, &mut master, |_, s| s);
        assert_eq!(master, [0.5, 0.5]);
    }

    #[test]
    fn fold_bus_graph_chains_sub_into_main_in_topo_order() {
        // sub (×0.5) → main (×0.5) → master: a signal of 1.0 → 0.25.
        let mut snap = MixerSnapshot::empty();
        let (sub, main) = (Uuid::new_v4(), Uuid::new_v4());
        snap.buses.insert(sub, bmix(0.5, Some(main)));
        snap.buses.insert(main, bmix(0.5, None));
        let order = snap.bus_process_order();
        // sub feeds main, so sub must be processed first regardless of uuid.
        assert_eq!(order[0], sub, "upstream bus must precede its output bus");
        let mut bus_in = HashMap::from([(sub, [1.0, 0.0])]);
        let mut master = [0.0f32; 2];
        fold_bus_graph(&snap, &order, &mut bus_in, &mut master, |_, s| s);
        assert!((master[0] - 0.25).abs() < 1e-6, "got {}", master[0]);
        assert_eq!(master[1], 0.0);
    }

    #[test]
    fn fold_bus_graph_post_fader_send_follows_source_fader() {
        // src (fader 0.5 → master) sends post-fader at unity to a reverb
        // return (unity → master). The send must reflect the 0.5 fader.
        let mut snap = MixerSnapshot::empty();
        let (src, rev) = (Uuid::new_v4(), Uuid::new_v4());
        let mut s = bmix(0.5, None);
        s.sends.push(SendMix { target: rev, level_linear: 1.0, pre_fader: false });
        snap.buses.insert(src, s);
        snap.buses.insert(rev, bmix(1.0, None));
        let order = snap.bus_process_order();
        assert_eq!(order[0], src, "send source must precede its target");
        let mut bus_in = HashMap::from([(src, [1.0, 0.0])]);
        let mut master = [0.0f32; 2];
        fold_bus_graph(&snap, &order, &mut bus_in, &mut master, |_, s| s);
        // master = src main (0.5) + reverb (post-fader send 0.5) = 1.0.
        assert!((master[0] - 1.0).abs() < 1e-6, "got {}", master[0]);
    }

    #[test]
    fn fold_bus_graph_pre_fader_send_ignores_source_fader_and_mute() {
        // A pre-fader send taps the post-insert signal BEFORE the fader,
        // and fires even when the source bus is muted.
        let mut snap = MixerSnapshot::empty();
        let (src, cue) = (Uuid::new_v4(), Uuid::new_v4());
        let mut s = bmix(0.0, None); // fader fully down + muted below
        s.mute = true;
        s.sends.push(SendMix { target: cue, level_linear: 1.0, pre_fader: true });
        snap.buses.insert(src, s);
        snap.buses.insert(cue, bmix(1.0, None));
        let order = snap.bus_process_order();
        let mut bus_in = HashMap::from([(src, [1.0, 0.0])]);
        let mut master = [0.0f32; 2];
        fold_bus_graph(&snap, &order, &mut bus_in, &mut master, |_, s| s);
        // src main is muted (0), but the pre-fader cue send passes at unity.
        assert!((master[0] - 1.0).abs() < 1e-6, "pre-fader send lost: {}", master[0]);
    }

    #[test]
    fn fold_bus_graph_mute_silences_main_and_post_fader_send() {
        let mut snap = MixerSnapshot::empty();
        let (src, rev) = (Uuid::new_v4(), Uuid::new_v4());
        let mut s = bmix(1.0, None);
        s.mute = true;
        s.sends.push(SendMix { target: rev, level_linear: 1.0, pre_fader: false });
        snap.buses.insert(src, s);
        snap.buses.insert(rev, bmix(1.0, None));
        let order = snap.bus_process_order();
        let mut bus_in = HashMap::from([(src, [1.0, 1.0])]);
        let mut master = [0.0f32; 2];
        fold_bus_graph(&snap, &order, &mut bus_in, &mut master, |_, s| s);
        assert_eq!(master, [0.0, 0.0], "muted bus must drop main + post-fader send");
    }

    #[test]
    fn fold_bus_graph_runs_insert_closure() {
        // The insert closure halves the signal (stands in for stateful DSP).
        let mut snap = MixerSnapshot::empty();
        let bus = Uuid::new_v4();
        snap.buses.insert(bus, bmix(1.0, None));
        let order = snap.bus_process_order();
        let mut bus_in = HashMap::from([(bus, [1.0, 1.0])]);
        let mut master = [0.0f32; 2];
        fold_bus_graph(&snap, &order, &mut bus_in, &mut master, |_, s| {
            [s[0] * 0.5, s[1] * 0.5]
        });
        assert_eq!(master, [0.5, 0.5]);
    }

    #[test]
    fn fold_bus_graph_drops_send_to_missing_bus() {
        // A send whose target isn't in the snapshot is dropped, never
        // leaking into the master sum.
        let mut snap = MixerSnapshot::empty();
        let src = Uuid::new_v4();
        let mut s = bmix(1.0, None);
        s.sends.push(SendMix {
            target: Uuid::new_v4(),
            level_linear: 1.0,
            pre_fader: false,
        });
        snap.buses.insert(src, s);
        let order = snap.bus_process_order();
        let mut bus_in = HashMap::from([(src, [1.0, 0.0])]);
        let mut master = [0.0f32; 2];
        fold_bus_graph(&snap, &order, &mut bus_in, &mut master, |_, s| s);
        // Only the main output reaches master; the orphan send is gone.
        assert_eq!(master, [1.0, 0.0]);
    }

    #[test]
    fn bus_process_order_handles_deep_chain_past_old_cap() {
        // 70-bus straight chain b0 → b1 → … → b69 → master, deeper than the
        // former fixed depth cap (64). The order must place every bus
        // before the bus it feeds, and the fold must carry signal end to
        // end (the old cap silently zeroed past ~65).
        let mut snap = MixerSnapshot::empty();
        let ids: Vec<Uuid> = (0..70).map(|_| Uuid::new_v4()).collect();
        for i in 0..70 {
            let out = if i + 1 < 70 { Some(ids[i + 1]) } else { None };
            snap.buses.insert(ids[i], bmix(1.0, out));
        }
        let order = snap.bus_process_order();
        let pos: HashMap<Uuid, usize> =
            order.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        for i in 0..69 {
            assert!(pos[&ids[i]] < pos[&ids[i + 1]], "bus {i} must precede its output");
        }
        // bus_chain_gain must also survive the full depth (was capped at 64).
        assert_eq!(snap.bus_chain_gain(Some(ids[0])), 1.0);
        let mut bus_in = HashMap::from([(ids[0], [1.0f32, 0.0])]);
        let mut master = [0.0f32; 2];
        fold_bus_graph(&snap, &order, &mut bus_in, &mut master, |_, s| s);
        assert!((master[0] - 1.0).abs() < 1e-6, "deep chain lost signal: {}", master[0]);
    }

    #[test]
    fn clip_pre_fader_gain_factors_out_track_and_bus() {
        // The offline bounce splits effective_gain_at into pre-fader (clip
        // only) × track fader × bus chain × master. Verify the split
        // recomposes exactly, so the bounce mirrors the live fold.
        let mut snap = MixerSnapshot::empty();
        let track_id = Uuid::new_v4();
        let clip_id = Uuid::new_v4();
        let bus = Uuid::new_v4();
        let mut tm = unity_track();
        tm.gain_linear = db_to_linear(-6.0);
        tm.output_bus = Some(bus);
        snap.tracks.insert(track_id, tm);
        let mut cm = unity_clip(track_id);
        cm.gain_linear = db_to_linear(-6.0);
        snap.clips.insert(clip_id, cm);
        snap.buses.insert(bus, bmix(db_to_linear(-6.0), None));
        snap.master_gain_linear = db_to_linear(-6.0);

        let pre = snap.clip_pre_fader_gain_at(&clip_id, 0.0);
        assert!(
            (pre - db_to_linear(-6.0)).abs() < 1e-6,
            "pre-fader is clip gain only: {pre}"
        );
        assert!((snap.track_gain_at(&track_id, 0.0) - db_to_linear(-6.0)).abs() < 1e-6);
        let composed = pre
            * snap.track_gain_at(&track_id, 0.0)
            * snap.bus_chain_gain(Some(bus))
            * snap.master_gain_linear;
        assert!(
            (composed - snap.effective_gain_at(&clip_id, 0.0)).abs() < 1e-6,
            "factored stages must recompose into effective_gain_at"
        );
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
    fn fade_curves_match_their_documented_shapes() {
        // 7.5 M2: EqualPower fade-in is sin(t/d·π/2); Logarithmic is (t/d)².
        let mid_eq = eval_fade_in_audio(2.0, FadeCurve::EqualPower, 1.0);
        assert!((mid_eq - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5);
        let mid_log = eval_fade_in_audio(2.0, FadeCurve::Logarithmic, 1.0);
        assert!((mid_log - 0.25).abs() < 1e-6);
        // All curves agree at the endpoints.
        for c in [FadeCurve::Linear, FadeCurve::EqualPower, FadeCurve::Logarithmic] {
            assert_eq!(eval_fade_in_audio(2.0, c, 0.0), 0.0);
            assert_eq!(eval_fade_in_audio(2.0, c, 2.0), 1.0);
        }
    }

    #[test]
    fn equal_power_crossfade_keeps_constant_power_in_audio_path() {
        // Two 1s clips A and B fully overlapping with a 1s equal-power
        // crossfade. At any moment `t` within the overlap, the
        // INCOMING clip's fade-in is sampled at `t` and the OUTGOING
        // clip's fade-out is also sampled at `t` (its `t_clip` is the
        // same wall-clock moment, since the fade-out spans the whole
        // clip). For EqualPower, fade_in(t) = sin(t·π/2) and
        // fade_out(t) = cos(t·π/2), so the sum of squares is 1.
        // The interior `0 < t < 1` is what actually exercises the
        // shape function; the endpoints are excluded because the
        // fade-out at exact clip-end returns 0 by the past-end gate.
        for k in 1..8 {
            let t = k as f64 / 8.0;
            let g_in = eval_fade_in_audio(1.0, FadeCurve::EqualPower, t);
            let g_out = eval_fade_out_audio(1.0, FadeCurve::EqualPower, 1.0, t);
            let power = g_in * g_in + g_out * g_out;
            assert!((power - 1.0).abs() < 1e-5, "t={t}, power={power}");
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

    // ============================================================
    // Phase 7.5 M1 metering tests
    // ============================================================

    #[test]
    fn meter_accum_empty_collapses_to_zeros() {
        let m = MeterAccum::default().into_meter();
        assert_eq!(m.peak_l, 0.0);
        assert_eq!(m.peak_r, 0.0);
        assert_eq!(m.rms_l, 0.0);
        assert_eq!(m.rms_r, 0.0);
    }

    #[test]
    fn meter_accum_peak_tracks_max_absolute() {
        let mut a = MeterAccum::default();
        a.observe(0.1, -0.4);
        a.observe(-0.3, 0.2);
        a.observe(0.7, -0.5);
        let m = a.into_meter();
        // L peak: max(|0.1|, |-0.3|, |0.7|) = 0.7
        assert!((m.peak_l - 0.7).abs() < 1e-6);
        // R peak: max(|-0.4|, |0.2|, |-0.5|) = 0.5
        assert!((m.peak_r - 0.5).abs() < 1e-6);
    }

    #[test]
    fn meter_accum_rms_is_sqrt_mean_squares() {
        let mut a = MeterAccum::default();
        a.observe(1.0, 1.0);
        a.observe(-1.0, -1.0);
        a.observe(1.0, 1.0);
        a.observe(-1.0, -1.0);
        let m = a.into_meter();
        // All abs == 1.0 → RMS == 1.0 on both channels.
        assert!((m.rms_l - 1.0).abs() < 1e-6);
        assert!((m.rms_r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn meter_accum_rms_dc_signal_matches_amplitude() {
        let mut a = MeterAccum::default();
        for _ in 0..64 {
            a.observe(0.5, -0.25);
        }
        let m = a.into_meter();
        assert!((m.rms_l - 0.5).abs() < 1e-6);
        assert!((m.rms_r - 0.25).abs() < 1e-6);
    }

    #[test]
    fn meter_snapshot_default_is_empty_and_silent() {
        let s = MeterSnapshot::default();
        assert_eq!(s.master.peak_l, 0.0);
        assert_eq!(s.master.rms_l, 0.0);
        assert!(s.tracks.is_empty());
        assert_eq!(s.generation, 0);
    }

    #[test]
    fn meter_accum_silence_input_produces_zero_meter() {
        let mut a = MeterAccum::default();
        for _ in 0..512 {
            a.observe(0.0, 0.0);
        }
        let m = a.into_meter();
        assert_eq!(m.peak_l, 0.0);
        assert_eq!(m.peak_r, 0.0);
        assert_eq!(m.rms_l, 0.0);
        assert_eq!(m.rms_r, 0.0);
    }

    // ============================================================
    // Phase 7.6 pan tests
    // ============================================================

    // ============================================================
    // Phase 7.7 per-track DSP integration tests
    // ============================================================

    #[test]
    fn track_dsp_state_passthrough_with_empty_chain() {
        let mut state = TrackDspState::new();
        state.reconcile_with(&[], 48_000.0, None);
        let (l, r) = state.tick_stereo(&[], 0.5, -0.3, None);
        assert!((l - 0.5).abs() < 1e-6);
        assert!((r - (-0.3)).abs() < 1e-6);
    }

    #[test]
    fn track_dsp_state_plugin_processes_audio() {
        // A built-in gain plugin at -6.02 dB (≈0.5x) in the chain must
        // actually attenuate the track signal (no longer passthrough).
        let spec = vec![AudioEffectKind::Plugin {
            enabled: true,
            format_str: "builtin".into(),
            id: "vance:gain".into(),
            name: "Gain".into(),
            params: vec![(0, -6.0206)],
        }];
        let mut state = TrackDspState::new();
        state.reconcile_with(&spec, 48_000.0, None);
        // Drive a constant 1.0; after the block-priming latency the
        // output settles to ≈0.5.
        let mut last = 0.0f32;
        for _ in 0..256 {
            last = state.tick_stereo(&spec, 1.0, 1.0, None).0;
        }
        assert!((last - 0.5).abs() < 1e-2, "plugin gain not applied: {last}");
    }

    #[test]
    fn track_dsp_state_disabled_plugin_bypasses() {
        let spec = vec![AudioEffectKind::Plugin {
            enabled: false,
            format_str: "builtin".into(),
            id: "vance:gain".into(),
            name: "Gain".into(),
            params: vec![(0, -60.0)], // would be near-silent if applied
        }];
        let mut state = TrackDspState::new();
        state.reconcile_with(&spec, 48_000.0, None);
        let (l, _) = state.tick_stereo(&spec, 1.0, 1.0, None);
        assert!((l - 1.0).abs() < 1e-6, "disabled plugin must pass through");
    }

    #[test]
    fn track_dsp_state_unknown_plugin_bypasses() {
        let spec = vec![AudioEffectKind::Plugin {
            enabled: true,
            format_str: "clap".into(),
            id: "nonexistent.plugin".into(),
            name: "X".into(),
            params: vec![],
        }];
        let mut state = TrackDspState::new();
        state.reconcile_with(&spec, 48_000.0, None);
        // No CLAP plugin by that id → proc is None → bypass, not a crash.
        let (l, r) = state.tick_stereo(&spec, 0.7, -0.4, None);
        assert!((l - 0.7).abs() < 1e-6 && (r - (-0.4)).abs() < 1e-6);
    }

    // NOTE: CLAP now processes OUT-OF-PROCESS through the sidecar, so the
    // real-plugin path is covered by the `tests/remote_plugin.rs`
    // integration suite (which drives a live sidecar) rather than a
    // unit test here. A `clap` slot with no host bypasses — see
    // `track_dsp_state_clap_without_host_bypasses` below.

    #[test]
    fn track_dsp_state_clap_without_host_bypasses() {
        // With no sidecar (host = None), a CLAP effect must pass audio
        // through unchanged rather than panic or silence the track.
        let spec = vec![AudioEffectKind::Plugin {
            enabled: true,
            format_str: "clap".into(),
            id: "/some/bundle.clap::com.x".into(),
            name: "X".into(),
            params: vec![],
        }];
        let mut state = TrackDspState::new();
        state.reconcile_with(&spec, 48_000.0, None);
        // Run enough samples to cross a block boundary; output equals
        // input (passthrough), never silence.
        let mut last = (0.0, 0.0);
        for _ in 0..256 {
            last = state.tick_stereo(&spec, 0.7, -0.3, None);
        }
        assert!((last.0 - 0.7).abs() < 1e-6, "L not passthrough: {}", last.0);
        assert!((last.1 - (-0.3)).abs() < 1e-6, "R not passthrough: {}", last.1);
    }

    #[test]
    fn track_dsp_state_disabled_effect_bypasses_audio() {
        let spec = vec![AudioEffectKind::Limiter {
            enabled: false,
            params: LimiterParams {
                ceiling_db: -6.0,
                release_ms: 50.0,
            },
        }];
        let mut state = TrackDspState::new();
        state.reconcile_with(&spec, 48_000.0, None);
        // Even though a limiter is in the chain, `enabled = false`
        // must result in pass-through.
        let (l, _) = state.tick_stereo(&spec, 1.0, 1.0, None);
        assert!((l - 1.0).abs() < 1e-6);
    }

    #[test]
    fn track_dsp_state_noise_gate_closes_on_quiet() {
        // GOAT-8: a gate effect on a track closes on a sub-threshold signal.
        let spec = vec![AudioEffectKind::NoiseGate {
            enabled: true,
            params: NoiseGateParams {
                threshold_db: -20.0,
                range_db: -80.0,
                attack_ms: 1.0,
                release_ms: 5.0,
            },
        }];
        let mut state = TrackDspState::new();
        state.reconcile_with(&spec, 48_000.0, None);
        let mut last = 1.0f32;
        for _ in 0..4096 {
            let (l, _) = state.tick_stereo(&spec, 0.01, 0.01, None); // -40 dB
            last = l.abs();
        }
        assert!(last < 0.005, "gate closed on quiet input: {last}");
    }

    #[test]
    fn track_dsp_state_dehum_attenuates_mains_tone() {
        // GOAT-8: a 60 Hz de-hum notch comb crushes a 60 Hz tone but passes
        // a 1 kHz tone.
        let spec = vec![AudioEffectKind::DeHum {
            enabled: true,
            params: DeHumParams {
                base_hz: 60.0,
                harmonics: 3,
                q: 12.0,
            },
        }];
        let sr = 48_000.0;
        let energy = |freq: f32| -> f32 {
            let mut state = TrackDspState::new();
            state.reconcile_with(&spec, sr, None);
            let mut peak = 0.0f32;
            for i in 0..9600 {
                let s = (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin();
                let (l, _) = state.tick_stereo(&spec, s, s, None);
                if i > 4800 {
                    peak = peak.max(l.abs());
                }
            }
            peak
        };
        assert!(energy(60.0) < 0.2, "60Hz hum attenuated");
        assert!(energy(1000.0) > 0.8, "1kHz passes the de-hum comb");
    }

    #[test]
    fn track_dsp_state_limiter_caps_track_signal() {
        let spec = vec![AudioEffectKind::Limiter {
            enabled: true,
            params: LimiterParams {
                ceiling_db: -3.0,
                release_ms: 50.0,
            },
        }];
        let mut state = TrackDspState::new();
        state.reconcile_with(&spec, 48_000.0, None);
        let mut peak = 0.0f32;
        for _ in 0..1024 {
            let (l, _) = state.tick_stereo(&spec, 1.0, 1.0, None);
            peak = peak.max(l.abs());
        }
        let ceiling = 10f32.powf(-3.0 / 20.0);
        assert!(
            peak <= ceiling + 1e-3,
            "limiter ceiling: got {peak}, ceiling {ceiling}"
        );
    }

    #[test]
    fn track_dsp_state_compressor_envelope_persists_across_param_update() {
        // Build a chain, drive it loud, then tweak params via a fresh
        // reconcile — the compressor envelope MUST NOT zero out.
        let mut spec = vec![AudioEffectKind::Compressor {
            enabled: true,
            params: CompressorParams {
                threshold_db: -10.0,
                ratio: 4.0,
                attack_ms: 0.5,
                release_ms: 200.0,
                knee_db: 0.0,
                makeup_db: 0.0,
            },
        }];
        let mut state = TrackDspState::new();
        state.reconcile_with(&spec, 48_000.0, None);
        // Pump signal to build envelope.
        for _ in 0..2048 {
            state.tick_stereo(&spec, 1.0, 1.0, None);
        }
        // Now tweak ratio — must keep variant (no rebuild), only update params.
        if let AudioEffectKind::Compressor { params, .. } = &mut spec[0] {
            params.ratio = 8.0;
        }
        state.reconcile_with(&spec, 48_000.0, None);
        // Tick once more; gain should still be reduced (envelope didn't reset).
        let (l, _) = state.tick_stereo(&spec, 1.0, 1.0, None);
        assert!(
            l.abs() < 0.7,
            "param-update must preserve compressor envelope (got |l|={})",
            l.abs()
        );
    }

    #[test]
    fn track_dsp_state_rebuilds_when_variant_changes() {
        // Start with a compressor; replace with a limiter.
        let mut spec = vec![AudioEffectKind::Compressor {
            enabled: true,
            params: CompressorParams {
                threshold_db: -10.0,
                ratio: 4.0,
                attack_ms: 5.0,
                release_ms: 100.0,
                knee_db: 0.0,
                makeup_db: 0.0,
            },
        }];
        let mut state = TrackDspState::new();
        state.reconcile_with(&spec, 48_000.0, None);
        // Swap variant.
        spec = vec![AudioEffectKind::Limiter {
            enabled: true,
            params: LimiterParams {
                ceiling_db: -1.0,
                release_ms: 50.0,
            },
        }];
        state.reconcile_with(&spec, 48_000.0, None);
        // Tick — must not panic, must produce finite output.
        let (l, r) = state.tick_stereo(&spec, 0.5, 0.5, None);
        assert!(l.is_finite() && r.is_finite());
    }

    #[test]
    fn pan_center_is_equal_power_center() {
        let [l, r] = pan_to_equal_power(0.0);
        let expected = std::f32::consts::FRAC_1_SQRT_2;
        assert!((l - expected).abs() < 1e-6);
        assert!((r - expected).abs() < 1e-6);
        assert!((l * l + r * r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn pan_full_left_zeros_right_channel() {
        let [l, r] = pan_to_equal_power(-1.0);
        assert!((l - 1.0).abs() < 1e-6);
        assert!(r.abs() < 1e-6);
    }

    #[test]
    fn pan_full_right_zeros_left_channel() {
        let [l, r] = pan_to_equal_power(1.0);
        assert!(l.abs() < 1e-6);
        assert!((r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn pan_preserves_total_power_at_every_position() {
        for k in -16..=16 {
            let p = k as f32 / 16.0;
            let [l, r] = pan_to_equal_power(p);
            let power = l * l + r * r;
            assert!((power - 1.0).abs() < 1e-5, "pan={p}, power={power}");
        }
    }

    #[test]
    fn pan_non_finite_falls_back_to_center() {
        for nan_in in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let [l, r] = pan_to_equal_power(nan_in);
            let expected = std::f32::consts::FRAC_1_SQRT_2;
            assert!((l - expected).abs() < 1e-6);
            assert!((r - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn pan_out_of_range_clamps_to_endpoints() {
        let [l_neg, r_neg] = pan_to_equal_power(-5.0);
        assert!((l_neg - 1.0).abs() < 1e-6);
        assert!(r_neg.abs() < 1e-6);
        let [l_pos, r_pos] = pan_to_equal_power(5.0);
        assert!(l_pos.abs() < 1e-6);
        assert!((r_pos - 1.0).abs() < 1e-6);
    }

    #[test]
    fn meter_accum_two_observations_sum_correctly_in_rms() {
        // Sanity check that pinpoints the fix for adversarial review
        // findings #1 + #15: two contributions on the SAME frame
        // accumulate into ONE observe with their sum, not into two
        // observes that count separately.
        //
        // Two contributions on one frame, summed: l = 0.3 + 0.4 = 0.7.
        // RMS_l = sqrt(0.7² / 1) = 0.7.
        let mut summed = MeterAccum::default();
        summed.observe(0.3 + 0.4, 0.0);
        let m = summed.into_meter();
        assert!((m.peak_l - 0.7).abs() < 1e-6);
        assert!((m.rms_l - 0.7).abs() < 1e-6);

        // Two separate observes (the OLD broken path) — would have
        // produced peak_l = 0.4 (max of individual) and
        // RMS_l = sqrt((0.3² + 0.4²) / 2) = sqrt(0.125) ≈ 0.354. We
        // assert these for documentation: this is what the BUG
        // produced before the fix. The fold loop must NOT take this
        // path.
        let mut split = MeterAccum::default();
        split.observe(0.3, 0.0);
        split.observe(0.4, 0.0);
        let m_bad = split.into_meter();
        assert!((m_bad.peak_l - 0.4).abs() < 1e-6);
        assert!((m_bad.rms_l - 0.353_553_4).abs() < 1e-4);
        // Both reported values are STRICTLY LESS than the summed truth.
        assert!(m_bad.peak_l < m.peak_l);
        assert!(m_bad.rms_l < m.rms_l);
    }
}
