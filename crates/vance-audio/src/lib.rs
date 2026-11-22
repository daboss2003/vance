//! `vance-audio` — sidecar process + parent-side player for decoded audio.
//!
//! Mirrors the `vance-decoder` split:
//! - `vance-audio-decoder` sidecar (in `src/bin/main.rs`): opens a media
//!   file via Symphonia, decodes packets to f32 stereo PCM at the file's
//!   native sample rate, ships chunks over IPC.
//! - Parent-side `AudioPlayer` (in this lib): owns the cpal output stream,
//!   pulls PCM chunks via the IPC client, and feeds the cpal callback
//!   from a lock-free ring buffer.
//!
//! Phase 2.3 ships:
//! - The wire protocol.
//! - The decoder sidecar (Symphonia).
//! - `AudioClient` parent-side IPC wrapper.
//! - `AudioPlayer` skeleton (cpal output, ring buffer, simple stereo
//!   passthrough; no resample yet — file rate must match device rate).
//!
//! Phase 2.5 will add the audio clock + drift correction for A/V sync.

#![warn(missing_docs)]

pub mod audio_plugin;
#[cfg(feature = "clap")]
pub mod clap_host;
pub mod client;
pub mod dsp;
pub mod loudness;
#[cfg(feature = "lv2")]
pub mod lv2_host;
pub mod mixer;
pub mod player;
pub mod protocol;
pub mod remote_plugin;
pub mod scrub_output;

pub use client::{AudioClient, AudioClientError, AudioKill};
pub use mixer::{
    bus_balance, db_to_linear, eval_fade_in_audio, eval_fade_out_audio, eval_gain_lane,
    fold_bus_graph, fold_frame, fold_frame_at, pan_to_equal_power, per_source_ring_capacity,
    probe_default_sample_rate, source_audible_at, spawn_source, AudioEffectKind, AudioMixer,
    AudioMixerError, AudioSource, BusMix, ClipMix, CompressorParams, DeHumParams, EqBandParams,
    FadeCurve, GainInterp, GainKeyframe, LimiterParams, MeterSnapshot, MixerSnapshot, MixerStats,
    NoiseGateParams, SendMix, SourceCommand, SourceStream, TrackDspState, TrackMeter, TrackMix,
};
pub use player::{AudioPlayer, AudioPlayerError};
pub use protocol::{AudioRequest, AudioResponse, AudioStreamInfo, PcmChunk};
