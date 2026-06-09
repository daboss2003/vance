//! Encoder wire protocol.
//!
//! Parent → sidecar: [`EncoderRequest`]. Sidecar → parent:
//! [`EncoderResponse`]. Versioned via [`PROTOCOL_VERSION`].
//!
//! Architecture mirrors `vance-decoder`: ONE encoder sidecar process
//! handles ONE in-progress encode at a time, sequential frames pushed
//! by the parent. The parent can spawn multiple sidecars to run the
//! render queue in parallel.

use serde::{Deserialize, Serialize};

/// Wire protocol version. Bump on breaking changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Video pixel format the encoder accepts on the IPC boundary. The
/// sidecar converts to the codec's required format internally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoPixelFormat {
    /// Packed RGBA, 8 bits per channel. Convenient for the renderer's
    /// composited output. Sidecar converts to YUV before encode.
    Rgba8,
    /// Packed BGRA, 8 bits per channel. Same shape as Rgba8 with
    /// channel swap — matches macOS view-output convention.
    Bgra8,
}

/// Audio sample format on the IPC boundary. Sidecar converts to the
/// codec's required format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioSampleFormat {
    /// 32-bit float, interleaved stereo `(L, R, L, R, …)`. Matches the
    /// mixer's master output.
    F32Interleaved,
}

/// Phase 8 — built-in encoder presets. Each maps onto a specific
/// FFmpeg codec + container + parameter set the sidecar applies.
///
/// The optional GPL Encoder Pack (PLAN.md §8.4) adds `libx264` /
/// `libx265` software variants; without it the H.264 / HEVC variants
/// listed here are hardware-only.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EncoderPreset {
    /// Apple ProRes 422 HQ. Deterministic; the exit-gate target for
    /// Phase 8.
    ProRes422HQ,
    /// Apple ProRes 422 (standard).
    ProRes422,
    /// Apple ProRes 4444 (with alpha).
    ProRes4444,
    /// Avid DNxHR HQ.
    DnxhrHq,
    /// Avid DNxHR SQ.
    DnxhrSq,
    /// Avid DNxHR LB (low-bandwidth proxy).
    DnxhrLb,
    /// H.264 — hardware encoder per platform (videotoolbox on macOS,
    /// nvenc / vaapi on Linux, mediafoundation on Windows). CPU
    /// fallback (libx264) requires the GPL Encoder Pack.
    H264 {
        /// CRF-equivalent quality, 0 (best) — 51 (worst). 23 is the
        /// libx264 default; hardware encoders map this to their own
        /// quality knob.
        crf: u8,
    },
    /// HEVC / H.265 — hardware first, libx265 with the GPL pack.
    Hevc {
        /// CRF-equivalent quality (see [`EncoderPreset::H264::crf`]).
        crf: u8,
    },
    /// AV1 via svt-av1 (hardware where available).
    Av1 {
        /// CRF quality 0 (best) — 63 (worst). svt-av1's typical
        /// default is 30.
        crf: u8,
    },
}

/// Audio codec the encoder writes alongside video.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum AudioCodec {
    /// 16-bit signed PCM (no compression). For WAV / MOV containers.
    PcmS16,
    /// 24-bit signed PCM.
    PcmS24,
    /// AAC LC (the typical broadcast / web profile).
    Aac,
    /// FLAC lossless.
    Flac,
}

/// One request from the parent to the encoder sidecar.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EncoderRequest {
    /// Handshake — must be first.
    Hello {
        /// Parent's protocol version.
        protocol_version: u32,
    },
    /// Open the output file + configure the encode. Must be followed
    /// by some number of [`EncoderRequest::PushVideoFrame`] /
    /// [`EncoderRequest::PushAudioFrame`] then exactly one
    /// [`EncoderRequest::Finish`].
    Open {
        /// Filesystem path to write to.
        path: String,
        /// Video stream config. `None` = audio-only output.
        video: Option<VideoConfig>,
        /// Audio stream config. `None` = video-only output.
        audio: Option<AudioConfig>,
    },
    /// Push one decoded video frame, in display order. The sidecar
    /// owns timing — the parent supplies frames at the sequence's
    /// nominal rate.
    PushVideoFrame {
        /// Frame PTS in `(time_base_num, time_base_den)` units (set
        /// at [`EncoderRequest::Open`] time).
        pts: i64,
        /// Pixel buffer. Length must equal `width * height * 4`.
        pixels: Vec<u8>,
    },
    /// Push one audio chunk in display order. Interleaved stereo f32.
    PushAudioFrame {
        /// First sample's PTS in `(audio_time_base_num,
        /// audio_time_base_den)` units (usually `(1, sample_rate)`).
        pts: i64,
        /// Interleaved samples `(L, R, L, R, …)`.
        samples: Vec<f32>,
    },
    /// Signal end-of-stream and flush all encoders. Sidecar replies
    /// with [`EncoderResponse::Finished`] when the file is fully
    /// written.
    Finish,
    /// Graceful shutdown.
    Shutdown,
}

/// Configuration for one video output stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VideoConfig {
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Frame rate numerator (e.g. `30_000` for 29.97 fps).
    pub frame_rate_num: u32,
    /// Frame rate denominator.
    pub frame_rate_den: u32,
    /// Pixel format of the bytes the parent will push.
    pub pixel_format: VideoPixelFormat,
    /// Encoder preset + quality.
    pub preset: EncoderPreset,
    /// Time-base for `pts` values on `PushVideoFrame`. Typically
    /// `(frame_rate_den, frame_rate_num)` so each frame's PTS is its
    /// integer frame index.
    pub time_base_num: u32,
    /// Time-base denominator.
    pub time_base_den: u32,
}

/// Configuration for one audio output stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Number of channels (1 = mono, 2 = stereo).
    pub channels: u8,
    /// Sample format of the bytes the parent will push.
    pub sample_format: AudioSampleFormat,
    /// Output codec.
    pub codec: AudioCodec,
    /// Time-base for audio `pts`. Typically `(1, sample_rate)` so
    /// `pts` is the cumulative sample index.
    pub time_base_num: u32,
    /// Time-base denominator.
    pub time_base_den: u32,
}

/// One response from sidecar to parent.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum EncoderResponse {
    /// Reply to [`EncoderRequest::Hello`].
    Hello {
        /// Sidecar's protocol version.
        protocol_version: u32,
        /// FFmpeg version string from the linked binary.
        ffmpeg_version: String,
        /// Which presets the sidecar can actually run. Software-only
        /// codecs requiring the GPL Encoder Pack are absent unless
        /// the pack is installed.
        available_presets: Vec<EncoderPreset>,
    },
    /// File opened successfully — encoders are wired and the sidecar
    /// is ready to accept frame pushes.
    Opened,
    /// File open failed.
    OpenFailed(EncoderError),
    /// Acknowledgement of a frame push. Cheap so the parent can keep
    /// the wire busy.
    FramePushed,
    /// Encode error mid-stream.
    EncodeFailed(EncoderError),
    /// Reply to [`EncoderRequest::Finish`] — file is closed, all data
    /// is on disk.
    Finished {
        /// Number of video frames written.
        video_frames: u64,
        /// Number of audio samples written.
        audio_samples: u64,
    },
    /// Reply to [`EncoderRequest::Shutdown`].
    Goodbye,
}

/// Structured encoder error. Display-only for now; future work can
/// extend with codes that the parent UI maps to remediation tips.
#[derive(Clone, Debug, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct EncoderError {
    /// Human-readable cause.
    pub message: String,
}

impl EncoderError {
    /// Build from any displayable error.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_constants() {
        assert!(PROTOCOL_VERSION >= 1);
    }

    #[test]
    fn open_request_serde_round_trip() {
        let req = EncoderRequest::Open {
            path: "/tmp/out.mov".into(),
            video: Some(VideoConfig {
                width: 1920,
                height: 1080,
                frame_rate_num: 30_000,
                frame_rate_den: 1001,
                pixel_format: VideoPixelFormat::Rgba8,
                preset: EncoderPreset::ProRes422HQ,
                time_base_num: 1001,
                time_base_den: 30_000,
            }),
            audio: Some(AudioConfig {
                sample_rate: 48_000,
                channels: 2,
                sample_format: AudioSampleFormat::F32Interleaved,
                codec: AudioCodec::PcmS24,
                time_base_num: 1,
                time_base_den: 48_000,
            }),
        };
        let s = serde_json::to_string(&req).unwrap();
        let _back: EncoderRequest = serde_json::from_str(&s).unwrap();
    }

    #[test]
    fn every_preset_round_trips() {
        for preset in [
            EncoderPreset::ProRes422HQ,
            EncoderPreset::ProRes422,
            EncoderPreset::ProRes4444,
            EncoderPreset::DnxhrHq,
            EncoderPreset::DnxhrSq,
            EncoderPreset::DnxhrLb,
            EncoderPreset::H264 { crf: 23 },
            EncoderPreset::Hevc { crf: 28 },
            EncoderPreset::Av1 { crf: 30 },
        ] {
            let s = serde_json::to_string(&preset).unwrap();
            let back: EncoderPreset = serde_json::from_str(&s).unwrap();
            assert_eq!(back, preset);
        }
    }
}
