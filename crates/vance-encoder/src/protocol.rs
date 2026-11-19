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
    /// Packed RGBA, 16 bits per channel, little-endian (`rgba64le`) — 8
    /// bytes/pixel. The wire format for a 10-bit HDR readback (10-bit render
    /// data lives in the high bits). Sidecar converts to `yuv420p10le`.
    Rgba64Le,
}

impl VideoPixelFormat {
    /// Bytes per pixel on the IPC boundary.
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            VideoPixelFormat::Rgba8 | VideoPixelFormat::Bgra8 => 4,
            VideoPixelFormat::Rgba64Le => 8,
        }
    }

    /// The FFmpeg rawvideo `-pix_fmt` name for this input format.
    pub fn ffmpeg_rawvideo(self) -> &'static str {
        match self {
            VideoPixelFormat::Rgba8 => "rgba",
            VideoPixelFormat::Bgra8 => "bgra",
            VideoPixelFormat::Rgba64Le => "rgba64le",
        }
    }
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
    /// HDR10 signaling + mastering metadata. When `Some`, the sidecar encodes
    /// a 10-bit HDR10 stream (libx265 Main10) tagged with these values.
    /// `#[serde(default)]` so existing SDR configs deserialize as `None`.
    #[serde(default)]
    pub hdr: Option<HdrMetadata>,
}

// ---------------------------------------------------------------------
// HDR10 signaling + mastering metadata
// ---------------------------------------------------------------------

/// Color primaries (container/stream signaling). Maps to FFmpeg + x265 names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorPrimaries {
    /// Rec.709 / sRGB gamut.
    Bt709,
    /// Rec.2020 (wide gamut, HDR).
    Bt2020,
    /// Display P3 (D65).
    DisplayP3,
}

impl ColorPrimaries {
    /// FFmpeg `-color_primaries` value.
    pub fn ffmpeg(self) -> &'static str {
        match self {
            ColorPrimaries::Bt709 => "bt709",
            ColorPrimaries::Bt2020 => "bt2020",
            ColorPrimaries::DisplayP3 => "smpte432",
        }
    }
}

/// Transfer characteristics (the EOTF/OETF the samples are encoded with).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferCharacteristics {
    /// Rec.709.
    Bt709,
    /// SMPTE ST 2084 (PQ) — HDR10.
    Pq,
    /// Hybrid Log-Gamma (ARIB STD-B67).
    Hlg,
}

impl TransferCharacteristics {
    /// FFmpeg `-color_trc` value.
    pub fn ffmpeg(self) -> &'static str {
        match self {
            TransferCharacteristics::Bt709 => "bt709",
            TransferCharacteristics::Pq => "smpte2084",
            TransferCharacteristics::Hlg => "arib-std-b67",
        }
    }
}

/// Matrix coefficients (RGB↔YCbCr).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatrixCoefficients {
    /// Rec.709.
    Bt709,
    /// Rec.2020 non-constant luminance.
    Bt2020Ncl,
}

impl MatrixCoefficients {
    /// FFmpeg `-colorspace` value.
    pub fn ffmpeg(self) -> &'static str {
        match self {
            MatrixCoefficients::Bt709 => "bt709",
            MatrixCoefficients::Bt2020Ncl => "bt2020nc",
        }
    }
}

/// Signal quantization range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignalRange {
    /// Limited / "TV" (16–235 at 8-bit).
    Limited,
    /// Full / "PC".
    Full,
}

impl SignalRange {
    /// FFmpeg `-color_range` value.
    pub fn ffmpeg(self) -> &'static str {
        match self {
            SignalRange::Limited => "tv",
            SignalRange::Full => "pc",
        }
    }
}

/// The four color-signaling codes written into the stream + container.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorSignaling {
    /// Color primaries.
    pub primaries: ColorPrimaries,
    /// Transfer characteristics.
    pub transfer: TransferCharacteristics,
    /// Matrix coefficients.
    pub matrix: MatrixCoefficients,
    /// Signal range.
    pub range: SignalRange,
}

impl ColorSignaling {
    /// The standard HDR10 signaling: Rec.2020 primaries, PQ transfer,
    /// Rec.2020 non-constant-luminance matrix, limited range.
    pub fn hdr10() -> Self {
        ColorSignaling {
            primaries: ColorPrimaries::Bt2020,
            transfer: TransferCharacteristics::Pq,
            matrix: MatrixCoefficients::Bt2020Ncl,
            range: SignalRange::Limited,
        }
    }

    /// FFmpeg color-signaling flags (`-color_primaries … -color_trc … …`).
    pub fn ffmpeg_args(&self) -> Vec<String> {
        vec![
            "-color_primaries".into(),
            self.primaries.ffmpeg().into(),
            "-color_trc".into(),
            self.transfer.ffmpeg().into(),
            "-colorspace".into(),
            self.matrix.ffmpeg().into(),
            "-color_range".into(),
            self.range.ffmpeg().into(),
        ]
    }
}

/// SMPTE ST 2086 mastering-display color volume. Chromaticities are CIE xy
/// in `[0,1]`; luminance is cd/m² (nits).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct MasteringDisplay {
    /// Red primary `[x, y]`.
    pub red: [f64; 2],
    /// Green primary `[x, y]`.
    pub green: [f64; 2],
    /// Blue primary `[x, y]`.
    pub blue: [f64; 2],
    /// White point `[x, y]`.
    pub white: [f64; 2],
    /// Peak luminance in nits.
    pub max_luminance: f64,
    /// Minimum luminance in nits.
    pub min_luminance: f64,
}

impl MasteringDisplay {
    /// A Rec.2020-primaries, D65 mastering display at the given peak/black.
    pub fn rec2020(max_nits: f64, min_nits: f64) -> Self {
        MasteringDisplay {
            red: [0.708, 0.292],
            green: [0.170, 0.797],
            blue: [0.131, 0.046],
            white: [0.3127, 0.3290],
            max_luminance: max_nits,
            min_luminance: min_nits,
        }
    }

    /// A P3-D65-primaries mastering display (a common grading monitor volume).
    pub fn p3_d65(max_nits: f64, min_nits: f64) -> Self {
        MasteringDisplay {
            red: [0.680, 0.320],
            green: [0.265, 0.690],
            blue: [0.150, 0.060],
            white: [0.3127, 0.3290],
            max_luminance: max_nits,
            min_luminance: min_nits,
        }
    }

    /// The x265 `master-display=G(...)B(...)R(...)WP(...)L(max,min)` string.
    /// Chromaticities are in units of `0.00002` (×50000); luminance in units
    /// of `0.0001` cd/m² (×10000) — the ST 2086 / x265 fixed-point form.
    pub fn x265_master_display(&self) -> String {
        let c = |v: f64| (v * 50000.0).round() as i64;
        let l = |v: f64| (v * 10000.0).round() as i64;
        format!(
            "G({},{})B({},{})R({},{})WP({},{})L({},{})",
            c(self.green[0]),
            c(self.green[1]),
            c(self.blue[0]),
            c(self.blue[1]),
            c(self.red[0]),
            c(self.red[1]),
            c(self.white[0]),
            c(self.white[1]),
            l(self.max_luminance),
            l(self.min_luminance),
        )
    }
}

/// HDR10 metadata: color signaling + mastering-display volume + content
/// light levels (MaxCLL / MaxFALL, in nits).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct HdrMetadata {
    /// Stream/container color signaling.
    pub signaling: ColorSignaling,
    /// Mastering display (ST 2086).
    pub mastering: MasteringDisplay,
    /// Maximum content light level (nits).
    pub max_cll: u16,
    /// Maximum frame-average light level (nits).
    pub max_fall: u16,
}

impl HdrMetadata {
    /// Standard HDR10: Rec.2020/PQ signaling, a Rec.2020 mastering display at
    /// `max_nits`/`min_nits`, and the given content light levels.
    pub fn hdr10(max_nits: f64, min_nits: f64, max_cll: u16, max_fall: u16) -> Self {
        HdrMetadata {
            signaling: ColorSignaling::hdr10(),
            mastering: MasteringDisplay::rec2020(max_nits, min_nits),
            max_cll,
            max_fall,
        }
    }

    /// The full x265 `-x265-params` value for HDR10 at the given CRF: color
    /// signaling + mastering display + content light levels + `hdr10=1`.
    pub fn x265_params(&self, crf: u8) -> String {
        format!(
            "crf={crf}:colorprim={}:transfer={}:colormatrix={}:master-display={}:max-cll={},{}:hdr10=1:repeat-headers=1:range={}",
            self.signaling.primaries.x265(),
            self.signaling.transfer.x265(),
            self.signaling.matrix.x265(),
            self.mastering.x265_master_display(),
            self.max_cll,
            self.max_fall,
            match self.signaling.range {
                SignalRange::Limited => "limited",
                SignalRange::Full => "full",
            },
        )
    }
}

impl ColorPrimaries {
    /// x265 `colorprim` value.
    pub fn x265(self) -> &'static str {
        match self {
            ColorPrimaries::Bt709 => "bt709",
            ColorPrimaries::Bt2020 => "bt2020",
            ColorPrimaries::DisplayP3 => "smpte432",
        }
    }
}
impl TransferCharacteristics {
    /// x265 `transfer` value.
    pub fn x265(self) -> &'static str {
        match self {
            TransferCharacteristics::Bt709 => "bt709",
            TransferCharacteristics::Pq => "smpte2084",
            TransferCharacteristics::Hlg => "arib-std-b67",
        }
    }
}
impl MatrixCoefficients {
    /// x265 `colormatrix` value.
    pub fn x265(self) -> &'static str {
        match self {
            MatrixCoefficients::Bt709 => "bt709",
            MatrixCoefficients::Bt2020Ncl => "bt2020nc",
        }
    }
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
    fn hdr10_signaling_ffmpeg_args() {
        let sig = ColorSignaling::hdr10();
        assert_eq!(
            sig.ffmpeg_args(),
            vec![
                "-color_primaries",
                "bt2020",
                "-color_trc",
                "smpte2084",
                "-colorspace",
                "bt2020nc",
                "-color_range",
                "tv",
            ]
        );
    }

    #[test]
    fn mastering_display_st2086_units() {
        // Rec.2020 red (0.708, 0.292) → ×50000 = (35400, 14600); max 1000 nits
        // → ×10000 = 10_000_000; min 0.0001 → 1.
        let md = MasteringDisplay::rec2020(1000.0, 0.0001);
        let s = md.x265_master_display();
        assert!(s.contains("R(35400,14600)"), "red primary units: {s}");
        assert!(s.contains("G(8500,39850)"), "green primary units: {s}");
        assert!(s.contains("B(6550,2300)"), "blue primary units: {s}");
        assert!(s.contains("WP(15635,16450)"), "D65 white point units: {s}");
        assert!(s.ends_with("L(10000000,1)"), "luminance units: {s}");
    }

    #[test]
    fn hdr10_x265_params_are_complete() {
        let hdr = HdrMetadata::hdr10(1000.0, 0.0001, 1000, 400);
        let p = hdr.x265_params(20);
        assert!(p.contains("crf=20"));
        assert!(p.contains("colorprim=bt2020"));
        assert!(p.contains("transfer=smpte2084"));
        assert!(p.contains("colormatrix=bt2020nc"));
        assert!(p.contains("master-display=G("));
        assert!(p.contains("max-cll=1000,400"));
        assert!(p.contains("hdr10=1"));
        assert!(p.contains("range=limited"));
    }

    #[test]
    fn pixel_format_bytes_and_rawvideo() {
        assert_eq!(VideoPixelFormat::Rgba8.bytes_per_pixel(), 4);
        assert_eq!(VideoPixelFormat::Bgra8.bytes_per_pixel(), 4);
        assert_eq!(VideoPixelFormat::Rgba64Le.bytes_per_pixel(), 8);
        assert_eq!(VideoPixelFormat::Rgba8.ffmpeg_rawvideo(), "rgba");
        assert_eq!(VideoPixelFormat::Rgba64Le.ffmpeg_rawvideo(), "rgba64le");
    }

    #[test]
    fn hdr_metadata_round_trips() {
        let hdr = HdrMetadata::hdr10(4000.0, 0.005, 4000, 1000);
        let s = serde_json::to_string(&hdr).unwrap();
        let back: HdrMetadata = serde_json::from_str(&s).unwrap();
        assert_eq!(hdr, back);
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
                hdr: None,
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
