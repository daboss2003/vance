//! `vance-encoder` sidecar binary.
//!
//! Spawns the `ffmpeg` CLI binary as a child process and pipes raw
//! pixel data + audio samples through it. Going via the CLI keeps the
//! sidecar small and codec-agnostic — every preset ffmpeg supports is
//! reachable by varying argv. Trade-off: requires `ffmpeg` on PATH.
//!
//! Audio is buffered to a side WAV file during the session and muxed
//! into the final container at `Finish`. This avoids needing multiple
//! stdin pipes (the CLI only takes one stdin) at the cost of one extra
//! mux pass per export.

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use vance_encoder::protocol::{
    AudioCodec, AudioConfig, EncoderError, EncoderPreset, EncoderRequest, EncoderResponse,
    HdrMetadata, VideoConfig, PROTOCOL_VERSION,
};
use vance_ipc::BlobReader;

fn main() -> vance_ipc::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::info!("encoder sidecar starting");

    let mut state: State = State::default();
    vance_ipc::serve_with_blobs::<EncoderRequest, EncoderResponse, _>(|req, blobs| {
        state.handle(req, blobs).into()
    })
}

#[derive(Default)]
struct State {
    open: Option<OpenSession>,
    /// Set when a pushed blob couldn't be read. The request stream is then out
    /// of sync, so nothing that follows can be trusted to be a real request.
    stream_error: Option<String>,
}

struct OpenSession {
    final_path: String,
    video: Option<VideoPipe>,
    audio: Option<AudioPipe>,
}

struct VideoPipe {
    child: Child,
    stdin: ChildStdin,
    /// ffmpeg encoder name, for error messages.
    encoder: &'static str,
    width: u32,
    height: u32,
    /// Bytes per pixel of the raw frames the parent pushes (4 for 8-bit
    /// RGBA/BGRA, 8 for the 10-bit `rgba64le` HDR wire format).
    bytes_per_pixel: usize,
    out_path: PathBuf,
    frames: u64,
}

struct AudioPipe {
    file: std::fs::File,
    out_path: PathBuf,
    sample_rate: u32,
    channels: u8,
    codec: AudioCodec,
    samples_written: u64,
    bytes_written: u64,
}

impl State {
    fn handle(&mut self, req: EncoderRequest, blobs: &mut BlobReader<'_>) -> EncoderResponse {
        if let Some(reason) = &self.stream_error {
            // Intentional: refuse every request, even Hello or Shutdown. After
            // a failed blob read, "requests" may be leftover pixel bytes that
            // happen to decode, e.g. as an Open with a garbage path.
            return EncoderResponse::EncodeFailed(EncoderError::new(format!(
                "encoder request stream is out of sync after an earlier error: {reason}"
            )));
        }
        match req {
            EncoderRequest::Hello { .. } => EncoderResponse::Hello {
                protocol_version: PROTOCOL_VERSION,
                ffmpeg_version: probe_ffmpeg_version(),
                available_presets: available_presets(),
            },
            EncoderRequest::Open { path, video, audio } => match self.open(path, video, audio) {
                Ok(()) => EncoderResponse::Opened,
                Err(e) => EncoderResponse::OpenFailed(EncoderError::new(e)),
            },
            EncoderRequest::PushVideoFrame { pts: _, len } => {
                self.push_blob(blobs, len, Self::push_video)
            }
            EncoderRequest::PushAudioFrame { pts: _, len } => {
                self.push_blob(blobs, len, Self::push_audio)
            }
            EncoderRequest::Finish => match self.finish() {
                Ok((v, a)) => EncoderResponse::Finished {
                    video_frames: v,
                    audio_samples: a,
                },
                Err(e) => EncoderResponse::EncodeFailed(EncoderError::new(e)),
            },
            EncoderRequest::Shutdown => {
                tracing::info!("encoder sidecar shutting down");
                EncoderResponse::Goodbye
            }
        }
    }

    fn open(
        &mut self,
        path: String,
        video: Option<VideoConfig>,
        audio: Option<AudioConfig>,
    ) -> Result<(), String> {
        if self.open.is_some() {
            return Err("encoder already open".into());
        }
        if video.is_none() && audio.is_none() {
            return Err("Open requires at least one of video or audio config".into());
        }
        if audio.as_ref().is_some_and(|a| a.channels == 0) {
            return Err("audio config has no channels".into());
        }

        let temp_dir = std::env::temp_dir();
        let pid = std::process::id();

        // Video goes either DIRECTLY to the final container (when
        // there's no audio) or to a temp file we mux at Finish.
        let video_only = audio.is_none();
        let v_pipe = video
            .map(|cfg| {
                let video_out = if video_only {
                    PathBuf::from(&path)
                } else {
                    temp_dir.join(format!("vance-enc-{pid}-video.mov"))
                };
                spawn_video_ffmpeg(&cfg, &video_out)
            })
            .transpose()?;

        let a_pipe = audio
            .map(|cfg| {
                let audio_out = temp_dir.join(format!("vance-enc-{pid}-audio.wav"));
                open_wav_writer(&cfg, &audio_out)
            })
            .transpose()?;

        self.open = Some(OpenSession {
            final_path: path,
            video: v_pipe,
            audio: a_pipe,
        });
        Ok(())
    }

    /// Reads the `len`-byte blob a push request announced and hands it to
    /// `push`, which checks its length against the open session.
    fn push_blob(
        &mut self,
        blobs: &mut BlobReader<'_>,
        len: u64,
        push: fn(&mut Self, &[u8]) -> Result<(), String>,
    ) -> EncoderResponse {
        // Intentional: the blob is read before anything is validated, even
        // when the push will be refused (wrong size, nothing open), so the
        // next request starts where the parent wrote it. `read_blob` bounds
        // the allocation at `MAX_BLOB_BYTES`.
        let bytes = match blobs.read_blob(len) {
            Ok(bytes) => bytes,
            Err(e) => {
                let reason = format!("reading a {len}-byte push: {e}");
                tracing::error!("{reason}");
                self.stream_error = Some(reason.clone());
                return EncoderResponse::EncodeFailed(EncoderError::new(reason));
            }
        };
        match push(self, &bytes) {
            Ok(()) => EncoderResponse::FramePushed,
            Err(e) => EncoderResponse::EncodeFailed(EncoderError::new(e)),
        }
    }

    fn push_video(&mut self, pixels: &[u8]) -> Result<(), String> {
        let s = self.open.as_mut().ok_or("encoder not open")?;
        let v = s.video.as_mut().ok_or("no video stream configured")?;
        let expected = (v.width as usize) * (v.height as usize) * v.bytes_per_pixel;
        if pixels.len() != expected {
            return Err(format!(
                "video frame size mismatch: got {} bytes, expected {} ({}x{} at {} bytes per pixel)",
                pixels.len(),
                expected,
                v.width,
                v.height,
                v.bytes_per_pixel
            ));
        }
        v.stdin
            .write_all(pixels)
            .map_err(|e| format!("write to ffmpeg stdin: {e}"))?;
        v.frames += 1;
        Ok(())
    }

    /// `bytes`: interleaved little-endian f32 samples.
    fn push_audio(&mut self, bytes: &[u8]) -> Result<(), String> {
        let s = self.open.as_mut().ok_or("encoder not open")?;
        let a = s.audio.as_mut().ok_or("no audio stream configured")?;
        let frame_bytes = 4 * usize::from(a.channels);
        if !bytes.len().is_multiple_of(frame_bytes) {
            return Err(format!(
                "audio push of {} bytes is not a whole number of {}-channel f32 sample frames",
                bytes.len(),
                a.channels
            ));
        }
        let mut pcm = Vec::with_capacity(bytes.len() / 4 * 3);
        for chunk in bytes.chunks_exact(4) {
            let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let clamped = sample.clamp(-1.0, 1.0);
            let v = (clamped * 8_388_607.0) as i32; // 2^23 - 1
            pcm.extend_from_slice(&v.to_le_bytes()[0..3]);
        }
        a.file
            .write_all(&pcm)
            .map_err(|e| format!("wav write: {e}"))?;
        a.bytes_written += pcm.len() as u64;
        a.samples_written += (bytes.len() / frame_bytes) as u64;
        Ok(())
    }

    fn finish(&mut self) -> Result<(u64, u64), String> {
        let mut s = self.open.take().ok_or("encoder not open")?;

        // 1. Close the video ffmpeg child and wait.
        let (video_frames, video_out, had_video) = if let Some(mut v) = s.video.take() {
            drop(v.stdin); // signal EOF
            let status = v.child.wait().map_err(|e| format!("wait ffmpeg: {e}"))?;
            if !status.success() {
                return Err(format!(
                    "ffmpeg video ({}) exited with {status:?}",
                    v.encoder
                ));
            }
            (v.frames, Some(v.out_path), true)
        } else {
            (0, None, false)
        };

        // 2. Finalise WAV header + capture target codec for the mux.
        let (audio_samples, audio_out, audio_codec) = if let Some(a) = s.audio.take() {
            finalize_wav_header(&a)?;
            (a.samples_written, Some(a.out_path), Some(a.codec))
        } else {
            (0, None, None)
        };

        // 3. Mux. Re-encode audio to the requested codec at this step
        // so the final container carries the user-chosen format
        // (AAC / FLAC / PCM) instead of the side WAV's PCM_S24.
        match (video_out, audio_out) {
            (Some(v), Some(a)) => {
                run_mux(&v, &a, &s.final_path, audio_codec.unwrap())?;
                let _ = std::fs::remove_file(&v);
                let _ = std::fs::remove_file(&a);
            }
            (None, Some(a)) => {
                run_audio_only_export(&a, &s.final_path, audio_codec.unwrap())?;
                let _ = std::fs::remove_file(&a);
            }
            (Some(_v), None) if had_video => {
                // Video was written directly to final_path. Nothing to do.
            }
            _ => return Err("nothing was encoded".into()),
        }

        Ok((video_frames, audio_samples))
    }
}

fn spawn_video_ffmpeg(cfg: &VideoConfig, out_path: &Path) -> Result<VideoPipe, String> {
    // Resolved before spawning so "no usable H.264 / HEVC encoder" fails Open
    // with a clear message instead of a broken pipe on the first frame.
    let choice = video_encoder_choice(cfg, selected_encoder)?;

    let mut child = Command::new("ffmpeg")
        .args(video_ffmpeg_args(cfg, &choice, out_path))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("failed to spawn ffmpeg (is it on PATH?): {e}"))?;
    let stdin = child.stdin.take().ok_or("ffmpeg stdin unavailable")?;

    Ok(VideoPipe {
        child,
        stdin,
        encoder: choice.name,
        width: cfg.width,
        height: cfg.height,
        bytes_per_pixel: cfg.pixel_format.bytes_per_pixel(),
        out_path: out_path.to_path_buf(),
        frames: 0,
    })
}

/// Full ffmpeg argv for a video encode: raw frames on stdin, encoded into
/// `out_path`. Hardware device setup (`pre_input`) has to precede `-i`.
fn video_ffmpeg_args(
    cfg: &VideoConfig,
    choice: &VideoEncoderChoice,
    out_path: &Path,
) -> Vec<OsString> {
    let size = format!("{}x{}", cfg.width, cfg.height);
    let frame_rate = format!("{}/{}", cfg.frame_rate_num, cfg.frame_rate_den);
    let mut args: Vec<OsString> = ["-hide_banner", "-loglevel", "warning", "-y"]
        .into_iter()
        .map(OsString::from)
        .collect();
    args.extend(choice.pre_input.iter().map(OsString::from));
    for a in [
        "-f",
        "rawvideo",
        "-pix_fmt",
        cfg.pixel_format.ffmpeg_rawvideo(),
        "-s",
        size.as_str(),
        "-framerate",
        frame_rate.as_str(),
        "-i",
        "pipe:0",
    ] {
        args.push(a.into());
    }
    args.extend(choice.codec_args.iter().map(OsString::from));
    args.push(out_path.into());
    args
}

/// The encoder for `cfg`. HDR10 overrides the preset with a software HEVC
/// Main10 path so the mastering metadata + PQ signaling are emitted reliably
/// (hardware encoders don't accept x265-params); otherwise the preset decides.
/// `select` picks an H.264 / HEVC encoder that works at `cfg`'s frame format.
fn video_encoder_choice(
    cfg: &VideoConfig,
    select: impl FnOnce(VideoCodec, FrameFormat) -> Result<Candidate, String>,
) -> Result<VideoEncoderChoice, String> {
    match &cfg.hdr {
        Some(hdr) => Ok(VideoEncoderChoice {
            name: "libx265",
            pre_input: Vec::new(),
            codec_args: hdr_encode_args(hdr, &cfg.preset),
        }),
        None => preset_encoder_choice(&cfg.preset, |codec| select(codec, FrameFormat::of(cfg))),
    }
}

/// FFmpeg args for an HDR10 encode: software HEVC Main10 (`libx265` +
/// `yuv420p10le`) with the full HDR10 x265 params (color signaling +
/// mastering display + content light + `hdr10=1`) and the stream-level color
/// signaling flags. Replaces the preset entirely so no incompatible option
/// (e.g. a ProRes profile) leaks in.
fn hdr_encode_args(hdr: &HdrMetadata, preset: &EncoderPreset) -> Vec<String> {
    let v = |s: &str| s.to_string();
    let crf = preset_crf(preset).unwrap_or(18);
    let mut args = vec![
        v("-c:v"),
        v("libx265"),
        v("-pix_fmt"),
        v("yuv420p10le"),
        v("-profile:v"),
        v("main10"),
        v("-x265-params"),
        hdr.x265_params(crf),
    ];
    args.extend(hdr.signaling.ffmpeg_args());
    args
}

/// Extract a CRF from a quality-based preset (H.264 / HEVC / AV1), else None.
fn preset_crf(preset: &EncoderPreset) -> Option<u8> {
    match preset {
        EncoderPreset::H264 { crf } | EncoderPreset::Hevc { crf } | EncoderPreset::Av1 { crf } => {
            Some(*crf)
        }
        _ => None,
    }
}

/// The encoder for a non-HDR preset. H.264 / HEVC have no single encoder that
/// works everywhere, so they go through `select`; the rest are fixed.
fn preset_encoder_choice(
    preset: &EncoderPreset,
    select: impl FnOnce(VideoCodec) -> Result<Candidate, String>,
) -> Result<VideoEncoderChoice, String> {
    let fixed = |name: &'static str, rest: &[&str]| -> Result<VideoEncoderChoice, String> {
        let mut codec_args = vec!["-c:v".to_string(), name.to_string()];
        codec_args.extend(rest.iter().map(|s| s.to_string()));
        Ok(VideoEncoderChoice {
            name,
            pre_input: Vec::new(),
            codec_args,
        })
    };
    match preset {
        EncoderPreset::ProRes422HQ => {
            fixed("prores_ks", &["-profile:v", "3", "-pix_fmt", "yuv422p10le"])
        }
        EncoderPreset::ProRes422 => {
            fixed("prores_ks", &["-profile:v", "2", "-pix_fmt", "yuv422p10le"])
        }
        EncoderPreset::ProRes4444 => fixed(
            "prores_ks",
            &["-profile:v", "4", "-pix_fmt", "yuva444p10le"],
        ),
        EncoderPreset::DnxhrHq => {
            fixed("dnxhd", &["-profile:v", "dnxhr_hq", "-pix_fmt", "yuv422p"])
        }
        EncoderPreset::DnxhrSq => {
            fixed("dnxhd", &["-profile:v", "dnxhr_sq", "-pix_fmt", "yuv422p"])
        }
        EncoderPreset::DnxhrLb => {
            fixed("dnxhd", &["-profile:v", "dnxhr_lb", "-pix_fmt", "yuv422p"])
        }
        EncoderPreset::H264 { crf } => Ok(select(VideoCodec::H264)?.choice(*crf)),
        EncoderPreset::Hevc { crf } => Ok(select(VideoCodec::Hevc)?.choice(*crf)),
        EncoderPreset::Av1 { crf } => fixed(
            "libsvtav1",
            &["-crf", &crf.to_string(), "-pix_fmt", "yuv420p"],
        ),
    }
}

// ---------------------------------------------------------------------
// Runtime H.264 / HEVC encoder selection
// ---------------------------------------------------------------------

/// The encoder one export runs: `pre_input` goes before `-i` (hardware
/// device setup), `codec_args` after it (filters, codec, quality, pix_fmt).
#[derive(Clone, Debug, PartialEq, Eq)]
struct VideoEncoderChoice {
    name: &'static str,
    pre_input: Vec<String>,
    codec_args: Vec<String>,
}

/// Codecs whose encoder is picked at runtime from a candidate list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum VideoCodec {
    H264,
    Hevc,
}

impl VideoCodec {
    fn label(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::Hevc => "HEVC",
        }
    }

    fn software_encoder(self) -> &'static str {
        match self {
            VideoCodec::H264 => "libx264",
            VideoCodec::Hevc => "libx265",
        }
    }
}

/// The frame size and rate an export encodes at. Hardware encoders have size
/// limits (an M1's h264_videotoolbox refuses frames wider than 4096), so a
/// candidate is probed, and its pick cached, at the export's own format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct FrameFormat {
    width: u32,
    height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
}

impl FrameFormat {
    fn of(cfg: &VideoConfig) -> Self {
        FrameFormat {
            width: cfg.width,
            height: cfg.height,
            frame_rate_num: cfg.frame_rate_num,
            frame_rate_den: cfg.frame_rate_den,
        }
    }
}

impl fmt::Display for FrameFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{} at {}/{} fps",
            self.width, self.height, self.frame_rate_num, self.frame_rate_den
        )
    }
}

/// How an encoder takes its quality and pixel-format options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EncoderFamily {
    VideoToolbox,
    Nvenc,
    Qsv,
    Vaapi,
    Amf,
    MediaFoundation,
    /// libx264 / libx265.
    X26x,
    OpenH264,
}

/// One encoder ffmpeg might provide for a codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Candidate {
    codec: VideoCodec,
    name: &'static str,
    family: EncoderFamily,
}

/// DRM render node the VAAPI candidates open (the first GPU).
const VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// Encoders to try for `codec` on `os` (a `std::env::consts::OS` value), in
/// order: the platform's hardware encoders, then software.
fn candidates_for(os: &str, codec: VideoCodec) -> Vec<Candidate> {
    use EncoderFamily as F;
    let hardware: &[(&'static str, EncoderFamily)] = match (os, codec) {
        ("macos", VideoCodec::H264) => &[("h264_videotoolbox", F::VideoToolbox)],
        ("macos", VideoCodec::Hevc) => &[("hevc_videotoolbox", F::VideoToolbox)],
        ("linux", VideoCodec::H264) => &[
            ("h264_nvenc", F::Nvenc),
            ("h264_qsv", F::Qsv),
            ("h264_vaapi", F::Vaapi),
        ],
        ("linux", VideoCodec::Hevc) => &[
            ("hevc_nvenc", F::Nvenc),
            ("hevc_qsv", F::Qsv),
            ("hevc_vaapi", F::Vaapi),
        ],
        ("windows", VideoCodec::H264) => &[
            ("h264_nvenc", F::Nvenc),
            ("h264_qsv", F::Qsv),
            ("h264_amf", F::Amf),
            ("h264_mf", F::MediaFoundation),
        ],
        ("windows", VideoCodec::Hevc) => &[
            ("hevc_nvenc", F::Nvenc),
            ("hevc_qsv", F::Qsv),
            ("hevc_amf", F::Amf),
            ("hevc_mf", F::MediaFoundation),
        ],
        _ => &[],
    };
    // Intentional: libx264 / libx265 are GPL. Vance doesn't bundle or link
    // them; it runs the user's installed `ffmpeg` as a separate process, so
    // they're used only when that ffmpeg build already provides them.
    let software: &[(&'static str, EncoderFamily)] = match codec {
        VideoCodec::H264 => &[("libx264", F::X26x), ("libopenh264", F::OpenH264)],
        VideoCodec::Hevc => &[("libx265", F::X26x)],
    };
    hardware
        .iter()
        .chain(software)
        .map(|&(name, family)| Candidate {
            codec,
            name,
            family,
        })
        .collect()
}

impl Candidate {
    /// ffmpeg args for this encoder at a preset CRF (0 best – 51 worst).
    /// Every family outputs 8-bit 4:2:0 (`yuv420p`, or `nv12` where that's
    /// the encoder's native input).
    fn choice(self, crf: u8) -> VideoEncoderChoice {
        use EncoderFamily as F;
        let crf_arg = crf.to_string();
        // QP-style knobs treat 0 as "unset / automatic", so CRF 0 maps to 1.
        let qp = crf.clamp(1, 51).to_string();
        // VideoToolbox and Media Foundation take a 1..=100 quality (higher is
        // better) instead of a CRF.
        let quality = (100 - i32::from(crf) * 2).clamp(1, 100).to_string();
        let bitrate = format!("{}k", openh264_bitrate_kbps(crf));
        let mut pre_input: Vec<&str> = Vec::new();
        let rest: Vec<&str> = match self.family {
            F::VideoToolbox => vec!["-q:v", &quality, "-pix_fmt", "yuv420p"],
            F::Nvenc => vec!["-rc", "vbr", "-cq", &qp, "-b:v", "0", "-pix_fmt", "yuv420p"],
            F::Qsv => vec!["-global_quality", &qp, "-pix_fmt", "nv12"],
            F::Vaapi => {
                // VAAPI encodes GPU surfaces: open the device before `-i`, then
                // convert + upload each frame. No `-pix_fmt` after the upload.
                pre_input.extend(["-vaapi_device", VAAPI_DEVICE]);
                vec!["-vf", "format=nv12,hwupload", "-qp", &qp]
            }
            F::Amf => {
                let mut a = vec!["-rc", "cqp", "-qp_i", &qp, "-qp_p", &qp];
                // hevc_amf has no B-frame QP option.
                if self.codec == VideoCodec::H264 {
                    a.extend(["-qp_b", &qp]);
                }
                a.extend(["-pix_fmt", "nv12"]);
                a
            }
            F::MediaFoundation => vec![
                "-rate_control",
                "quality",
                "-quality",
                &quality,
                "-pix_fmt",
                "nv12",
            ],
            F::X26x => vec!["-preset", "medium", "-crf", &crf_arg, "-pix_fmt", "yuv420p"],
            F::OpenH264 => vec!["-b:v", &bitrate, "-pix_fmt", "yuv420p"],
        };
        let mut codec_args = vec!["-c:v".to_string(), self.name.to_string()];
        codec_args.extend(rest.into_iter().map(String::from));
        VideoEncoderChoice {
            name: self.name,
            pre_input: pre_input.into_iter().map(String::from).collect(),
            codec_args,
        }
    }
}

/// libopenh264 has no CRF / constant-quality mode, so the CRF becomes a
/// target bitrate: 8 Mb/s at CRF 23 (a sensible 1080p rate), doubling every 6
/// CRF steps toward higher quality, as with x264's CRF scale.
fn openh264_bitrate_kbps(crf: u8) -> u32 {
    let kbps = 8000.0 * 2f64.powf((23.0 - f64::from(crf.min(51))) / 6.0);
    (kbps.round() as u32).clamp(500, 50_000)
}

/// The first candidate that this ffmpeg lists (`available`) AND that passes
/// `probe` (a real test encode at `format`): an encoder can be compiled into
/// ffmpeg yet unusable on this machine (no GPU, driver or device node) or at
/// this frame size.
fn select_encoder(
    codec: VideoCodec,
    format: FrameFormat,
    candidates: &[Candidate],
    available: &HashSet<String>,
    mut probe: impl FnMut(&Candidate, FrameFormat) -> bool,
) -> Result<Candidate, String> {
    let mut tried = Vec::with_capacity(candidates.len());
    for c in candidates {
        if !available.contains(c.name) {
            tried.push(format!("{} (not in this ffmpeg build)", c.name));
        } else if probe(c, format) {
            return Ok(*c);
        } else {
            tried.push(format!("{} (test encode failed)", c.name));
        }
    }
    Err(format!(
        "No working {} encoder found for {format} (tried: {}). Install an ffmpeg build with \
         {} or a supported hardware encoder.",
        codec.label(),
        tried.join(", "),
        codec.software_encoder()
    ))
}

/// CRF the probe encodes at. Any in-range value works: the probe checks that
/// the encoder opens and encodes a frame, not its quality.
const PROBE_CRF: u8 = 23;
/// A wedged hardware driver must not hang the export's Open forever. This is
/// the limit for frames up to 4K UHD; see [`probe_timeout`].
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Encoder picks per (codec, frame format). A pick is reused only for the
/// format it was probed at: hardware that handles 1080p can refuse 8K.
struct SelectionCache(Mutex<BTreeMap<(VideoCodec, FrameFormat), Result<Candidate, String>>>);

impl SelectionCache {
    const fn new() -> Self {
        SelectionCache(Mutex::new(BTreeMap::new()))
    }

    /// The cached pick for `codec` at `format`, running `select` on a miss.
    /// Failures are cached too: probing again would fail the same way.
    fn get_or_select(
        &self,
        codec: VideoCodec,
        format: FrameFormat,
        select: impl FnOnce() -> Result<Candidate, String>,
    ) -> Result<Candidate, String> {
        // Intentional: the lock is held across `select` (seconds of probe
        // encodes). The sidecar handles one request at a time, and a second
        // caller for the same format should wait for the pick, not re-probe.
        let mut picks = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        picks.entry((codec, format)).or_insert_with(select).clone()
    }
}

/// The encoder this sidecar uses for `codec` at `format`, chosen at most once
/// per process and format (listing + probe encodes cost real time) and logged
/// once.
fn selected_encoder(codec: VideoCodec, format: FrameFormat) -> Result<Candidate, String> {
    static PICKS: SelectionCache = SelectionCache::new();
    PICKS.get_or_select(codec, format, || {
        let picked = list_ffmpeg_encoders().and_then(|available| {
            let candidates = candidates_for(std::env::consts::OS, codec);
            select_encoder(codec, format, &candidates, &available, probe_encoder)
        });
        match &picked {
            Ok(c) => eprintln!(
                "[encoder] {} encoder for {format}: {}",
                codec.label(),
                c.name
            ),
            Err(e) => eprintln!("[encoder] {e}"),
        }
        picked
    })
}

/// Encoder names compiled into the `ffmpeg` on PATH.
fn list_ffmpeg_encoders() -> Result<HashSet<String>, String> {
    let out = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-encoders"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("failed to run ffmpeg (is it on PATH?): {e}"))?;
    if !out.status.success() {
        return Err(format!("`ffmpeg -encoders` exited with {}", out.status));
    }
    Ok(parse_encoder_list(&String::from_utf8_lossy(&out.stdout)))
}

/// Parse `ffmpeg -encoders`: a flags legend, a ` ------` rule, then one
/// ` V....D name  description` row per encoder.
fn parse_encoder_list(listing: &str) -> HashSet<String> {
    listing
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("---"))
        .skip(1)
        .filter_map(|l| l.split_whitespace().nth(1))
        .map(str::to_owned)
        .collect()
}

fn probe_encoder(candidate: &Candidate, format: FrameFormat) -> bool {
    let args = probe_args(&candidate.choice(PROBE_CRF), format);
    match run_probe(&args, probe_timeout(format)) {
        Ok(()) => true,
        Err(reason) => {
            eprintln!(
                "[encoder] {} unusable at {format}: {reason}",
                candidate.name
            );
            false
        }
    }
}

/// A one-frame test encode with exactly the codec args, frame size and rate a
/// real export would use.
fn probe_args(choice: &VideoEncoderChoice, format: FrameFormat) -> Vec<String> {
    let s = |a: &[&str]| a.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let source = format!(
        "color=c=black:s={}x{}:r={}/{}",
        format.width, format.height, format.frame_rate_num, format.frame_rate_den
    );
    let mut args = s(&["-hide_banner", "-nostdin", "-loglevel", "error"]);
    args.extend(choice.pre_input.iter().cloned());
    args.extend(s(&["-f", "lavfi", "-i", &source, "-frames:v", "1"]));
    args.extend(choice.codec_args.iter().cloned());
    args.extend(s(&["-f", "null", "-"]));
    args
}

/// [`PROBE_TIMEOUT`] scaled by frame area above 4K UHD. A software probe's
/// time grows with the frame (on an M1, one libx265 frame takes ~1.7 s at 8K
/// and ~7 s at 16K), so a fixed limit would fail a slower CPU's legitimate
/// probe and refuse a big export for being big.
fn probe_timeout(format: FrameFormat) -> Duration {
    const UHD_AREA: u128 = 3840 * 2160;
    let area = u128::from(format.width) * u128::from(format.height);
    let ms = PROBE_TIMEOUT.as_millis() * area / UHD_AREA;
    Duration::from_millis(u64::try_from(ms).unwrap_or(u64::MAX)).max(PROBE_TIMEOUT)
}

fn run_probe(args: &[String], timeout: Duration) -> Result<(), String> {
    // stdin / stdout carry this sidecar's IPC frames; the probe must not touch them.
    let mut child = Command::new("ffmpeg")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn ffmpeg: {e}"))?;
    let mut stderr = child.stderr.take();
    // Drained on a thread so a chatty failure can't fill the pipe and stall
    // the child while we poll it.
    let drain = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_string(&mut text);
        }
        text
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "test encode timed out after {}s",
                    timeout.as_secs()
                ));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("waiting for test encode: {e}"));
            }
        }
    };
    let stderr = drain.join().unwrap_or_default();
    let status = status?;
    if status.success() {
        return Ok(());
    }
    let detail = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("no error output");
    Err(format!("test encode exited with {status}: {detail}"))
}

fn open_wav_writer(cfg: &AudioConfig, out_path: &PathBuf) -> Result<AudioPipe, String> {
    use std::fs::OpenOptions;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(out_path)
        .map_err(|e| format!("open wav: {e}"))?;
    let mut a = AudioPipe {
        file,
        out_path: out_path.clone(),
        sample_rate: cfg.sample_rate,
        channels: cfg.channels,
        codec: cfg.codec,
        samples_written: 0,
        bytes_written: 0,
    };
    write_wav_header_placeholder(&mut a)?;
    Ok(a)
}

fn write_wav_header_placeholder(a: &mut AudioPipe) -> Result<(), String> {
    let mut hdr = Vec::with_capacity(44);
    hdr.extend_from_slice(b"RIFF");
    hdr.extend_from_slice(&0u32.to_le_bytes());
    hdr.extend_from_slice(b"WAVE");
    hdr.extend_from_slice(b"fmt ");
    hdr.extend_from_slice(&16u32.to_le_bytes());
    hdr.extend_from_slice(&1u16.to_le_bytes()); // PCM
    hdr.extend_from_slice(&(a.channels as u16).to_le_bytes());
    hdr.extend_from_slice(&a.sample_rate.to_le_bytes());
    let byte_rate = a.sample_rate * a.channels as u32 * 3;
    hdr.extend_from_slice(&byte_rate.to_le_bytes());
    let block_align = a.channels as u16 * 3;
    hdr.extend_from_slice(&block_align.to_le_bytes());
    hdr.extend_from_slice(&24u16.to_le_bytes()); // bits per sample
    hdr.extend_from_slice(b"data");
    hdr.extend_from_slice(&0u32.to_le_bytes());
    a.file
        .write_all(&hdr)
        .map_err(|e| format!("wav header: {e}"))
}

fn finalize_wav_header(a: &AudioPipe) -> Result<(), String> {
    use std::io::{Seek, SeekFrom};
    let data_size = a.bytes_written as u32;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&a.out_path)
        .map_err(|e| format!("open wav for patch: {e}"))?;
    let riff_size = 36 + data_size;
    file.seek(SeekFrom::Start(4))
        .map_err(|e| format!("seek riff: {e}"))?;
    file.write_all(&riff_size.to_le_bytes())
        .map_err(|e| format!("patch riff: {e}"))?;
    file.seek(SeekFrom::Start(40))
        .map_err(|e| format!("seek data: {e}"))?;
    file.write_all(&data_size.to_le_bytes())
        .map_err(|e| format!("patch data: {e}"))?;
    Ok(())
}

fn audio_codec_args(codec: AudioCodec) -> Vec<String> {
    match codec {
        AudioCodec::PcmS16 => vec!["-c:a".into(), "pcm_s16le".into()],
        AudioCodec::PcmS24 => vec!["-c:a".into(), "pcm_s24le".into()],
        AudioCodec::Aac => vec![
            "-c:a".into(),
            "aac".into(),
            // Use a constant-quality VBR mode so output bitrate
            // matches source loudness rather than being padded with
            // silence at a fixed CBR.
            "-vbr".into(),
            "4".into(),
        ],
        AudioCodec::Flac => vec![
            "-c:a".into(),
            "flac".into(),
            "-compression_level".into(),
            "5".into(),
        ],
    }
}

fn run_mux(
    video_path: &PathBuf,
    audio_path: &PathBuf,
    final_path: &str,
    audio_codec: AudioCodec,
) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-y")
        .arg("-i")
        .arg(video_path)
        .arg("-i")
        .arg(audio_path)
        .arg("-c:v")
        .arg("copy");
    for a in audio_codec_args(audio_codec) {
        cmd.arg(a);
    }
    cmd.arg("-map")
        .arg("0:v:0")
        .arg("-map")
        .arg("1:a:0")
        .arg(final_path);
    let status = cmd.status().map_err(|e| format!("mux spawn: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg mux exited with {status:?}"));
    }
    Ok(())
}

fn run_audio_only_export(
    audio_path: &PathBuf,
    final_path: &str,
    audio_codec: AudioCodec,
) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-y")
        .arg("-i")
        .arg(audio_path);
    for a in audio_codec_args(audio_codec) {
        cmd.arg(a);
    }
    cmd.arg(final_path);
    let status = cmd
        .status()
        .map_err(|e| format!("audio export spawn: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg audio export exited with {status:?}"));
    }
    Ok(())
}

fn probe_ffmpeg_version() -> String {
    match Command::new("ffmpeg").arg("-version").output() {
        Ok(out) => {
            let s = String::from_utf8_lossy(&out.stdout);
            s.lines()
                .next()
                .unwrap_or("ffmpeg present, version unknown")
                .to_string()
        }
        Err(_) => "ffmpeg NOT FOUND on PATH".to_string(),
    }
}

fn available_presets() -> Vec<EncoderPreset> {
    vec![
        EncoderPreset::ProRes422HQ,
        EncoderPreset::ProRes422,
        EncoderPreset::ProRes4444,
        EncoderPreset::DnxhrHq,
        EncoderPreset::DnxhrSq,
        EncoderPreset::DnxhrLb,
        EncoderPreset::H264 { crf: 23 },
        EncoderPreset::Hevc { crf: 28 },
        EncoderPreset::Av1 { crf: 30 },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use vance_encoder::protocol::VideoPixelFormat;
    use VideoCodec::{Hevc, H264};

    fn listed(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// `width`×`height` at 30 fps.
    fn at(width: u32, height: u32) -> FrameFormat {
        FrameFormat {
            width,
            height,
            frame_rate_num: 30,
            frame_rate_den: 1,
        }
    }

    fn config(preset: EncoderPreset) -> VideoConfig {
        VideoConfig {
            width: 320,
            height: 240,
            frame_rate_num: 24,
            frame_rate_den: 1,
            pixel_format: VideoPixelFormat::Rgba8,
            preset,
            time_base_num: 1,
            time_base_den: 24,
            hdr: None,
        }
    }

    fn candidate(os: &str, codec: VideoCodec, name: &str) -> Candidate {
        candidates_for(os, codec)
            .into_iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("{name} is not a {os} {codec:?} candidate"))
    }

    fn argv(cfg: &VideoConfig, choice: &VideoEncoderChoice) -> Vec<String> {
        video_ffmpeg_args(cfg, choice, Path::new("out.mp4"))
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect()
    }

    #[test]
    fn listed_hardware_that_fails_its_probe_falls_back_to_libx264() {
        let candidates = candidates_for("linux", H264);
        let available = listed(&["h264_vaapi", "libx264", "libopenh264"]);
        let mut probed = Vec::new();
        let picked = select_encoder(H264, at(1920, 1080), &candidates, &available, |c, _| {
            probed.push(c.name);
            c.name != "h264_vaapi"
        })
        .unwrap();
        assert_eq!(picked.name, "libx264");
        // Unlisted encoders are never probed; probing stops at the first success.
        assert_eq!(probed, ["h264_vaapi", "libx264"]);
    }

    #[test]
    fn working_hardware_encoder_wins_over_software() {
        let candidates = candidates_for("linux", H264);
        // NVENC isn't in this build, so the first listed hardware encoder wins.
        let available = listed(&["h264_vaapi", "h264_qsv", "libx264"]);
        let picked =
            select_encoder(H264, at(1920, 1080), &candidates, &available, |_, _| true).unwrap();
        assert_eq!(picked.name, "h264_qsv");
    }

    #[test]
    fn probe_runs_at_the_export_size_so_8k_falls_back_while_1080p_keeps_hardware() {
        let candidates = candidates_for("macos", H264);
        let available = listed(&["h264_videotoolbox", "libx264", "libopenh264"]);
        let (hd, uhd8k) = (at(1920, 1080), at(7680, 4320));
        let mut probed = Vec::new();
        let mut pick = |format| {
            select_encoder(H264, format, &candidates, &available, |c, seen| {
                probed.push((c.name, seen));
                // Like an M1's h264_videotoolbox: nothing wider than 4096.
                c.family != EncoderFamily::VideoToolbox || seen.width <= 4096
            })
            .map(|c| c.name)
        };
        assert_eq!(pick(hd), Ok("h264_videotoolbox"));
        assert_eq!(pick(uhd8k), Ok("libx264"));
        assert_eq!(
            probed,
            [
                ("h264_videotoolbox", hd),
                ("h264_videotoolbox", uhd8k),
                ("libx264", uhd8k)
            ]
        );
    }

    #[test]
    fn video_encoder_choice_selects_at_the_configs_size_and_rate() {
        let mut cfg = config(EncoderPreset::Hevc { crf: 28 });
        (cfg.width, cfg.height) = (7680, 4320);
        (cfg.frame_rate_num, cfg.frame_rate_den) = (30_000, 1001);
        let choice = video_encoder_choice(&cfg, |codec, format| {
            assert_eq!(codec, Hevc);
            assert_eq!(
                format,
                FrameFormat {
                    width: 7680,
                    height: 4320,
                    frame_rate_num: 30_000,
                    frame_rate_den: 1001
                }
            );
            Ok(candidate("macos", Hevc, "libx265"))
        })
        .unwrap();
        assert_eq!(choice.name, "libx265");
    }

    #[test]
    fn selection_cache_never_reuses_a_pick_made_for_another_format() {
        let cache = SelectionCache::new();
        let vt = candidate("macos", H264, "h264_videotoolbox");
        let x264 = candidate("macos", H264, "libx264");
        let hd = at(1920, 1080);
        let cached = || -> Result<Candidate, String> { panic!("re-selected a cached format") };

        assert_eq!(cache.get_or_select(H264, hd, || Ok(vt)), Ok(vt));
        assert_eq!(cache.get_or_select(H264, hd, cached), Ok(vt));
        // Each of these differs from `hd` in one field, so each gets its own
        // selection instead of the 1080p pick.
        for other in [
            at(7680, 4320),
            at(1920, 4320),
            at(7680, 1080),
            FrameFormat {
                frame_rate_num: 240,
                ..hd
            },
            FrameFormat {
                frame_rate_num: 30_000,
                frame_rate_den: 1001,
                ..hd
            },
        ] {
            assert_eq!(cache.get_or_select(H264, other, || Ok(x264)), Ok(x264));
            assert_eq!(cache.get_or_select(H264, other, cached), Ok(x264));
        }
        assert_eq!(cache.get_or_select(H264, hd, cached), Ok(vt));

        // The codec is part of the key, and a failed selection is cached too.
        let none = Err("No working HEVC encoder found".to_string());
        assert_eq!(cache.get_or_select(Hevc, hd, || none.clone()), none);
        assert_eq!(cache.get_or_select(Hevc, hd, cached), none);
        assert_eq!(cache.get_or_select(H264, hd, cached), Ok(vt));
    }

    #[test]
    fn probe_encodes_one_frame_at_the_export_size_and_rate() {
        let choice = candidate("macos", H264, "h264_videotoolbox").choice(PROBE_CRF);
        let format = FrameFormat {
            width: 7680,
            height: 4320,
            frame_rate_num: 30_000,
            frame_rate_den: 1001,
        };
        assert_eq!(
            probe_args(&choice, format),
            [
                "-hide_banner",
                "-nostdin",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=black:s=7680x4320:r=30000/1001",
                "-frames:v",
                "1",
                "-c:v",
                "h264_videotoolbox",
                "-q:v",
                "54",
                "-pix_fmt",
                "yuv420p",
                "-f",
                "null",
                "-",
            ]
        );

        // The probe's size and rate are the export command line's `-s` / `-framerate`.
        let cfg = config(EncoderPreset::H264 { crf: 23 });
        let export = argv(&cfg, &choice);
        let value_of = |flag: &str| &export[export.iter().position(|a| a == flag).unwrap() + 1];
        let source = format!(
            "color=c=black:s={}:r={}",
            value_of("-s"),
            value_of("-framerate")
        );
        assert_eq!(source, "color=c=black:s=320x240:r=24/1");
        assert!(probe_args(&choice, FrameFormat::of(&cfg)).contains(&source));
    }

    #[test]
    fn probe_timeout_grows_with_frame_area() {
        let secs = |w, h| probe_timeout(at(w, h)).as_secs();
        assert_eq!(secs(256, 256), 15);
        assert_eq!(secs(1920, 1080), 15);
        assert_eq!(secs(3840, 2160), 15);
        assert_eq!(secs(4096, 2160), 16);
        assert_eq!(secs(7680, 4320), 60);
        assert_eq!(secs(8192, 4320), 64);
        assert_eq!(secs(15_360, 8640), 240);
        assert_eq!(secs(0, 0), 15);
        assert!(secs(u32::MAX, u32::MAX) >= 240);
    }

    #[test]
    fn no_working_encoder_is_a_clear_error_naming_what_was_tried() {
        let candidates = candidates_for("linux", H264);
        let err = select_encoder(
            H264,
            at(7680, 4320),
            &candidates,
            &listed(&["h264_vaapi"]),
            |_, _| false,
        )
        .unwrap_err();
        assert!(
            err.starts_with("No working H.264 encoder found for 7680x4320 at 30/1 fps (tried: "),
            "{err}"
        );
        assert!(err.contains("h264_vaapi (test encode failed)"), "{err}");
        for name in ["h264_nvenc", "h264_qsv", "libx264", "libopenh264"] {
            assert!(
                err.contains(&format!("{name} (not in this ffmpeg build)")),
                "{err}"
            );
        }
        assert!(
            err.ends_with("Install an ffmpeg build with libx264 or a supported hardware encoder."),
            "{err}"
        );

        let err = select_encoder(
            Hevc,
            at(1920, 1080),
            &candidates_for("linux", Hevc),
            &listed(&[]),
            |_, _| true,
        )
        .unwrap_err();
        assert!(
            err.starts_with("No working HEVC encoder found for 1920x1080 at 30/1 fps"),
            "{err}"
        );
        assert!(
            err.contains("Install an ffmpeg build with libx265"),
            "{err}"
        );
    }

    #[test]
    fn selection_error_surfaces_through_the_video_encoder_path() {
        let err = video_encoder_choice(&config(EncoderPreset::Hevc { crf: 28 }), |codec, _| {
            assert_eq!(codec, Hevc);
            Err("No working HEVC encoder found (tried: libx265)".to_string())
        })
        .unwrap_err();
        assert!(err.starts_with("No working HEVC encoder found"), "{err}");
    }

    #[test]
    fn vaapi_opens_its_device_before_the_input_and_uploads_frames() {
        let cfg = config(EncoderPreset::H264 { crf: 23 });
        let vaapi = candidate("linux", H264, "h264_vaapi");
        let choice = video_encoder_choice(&cfg, |_, _| Ok(vaapi)).unwrap();
        for args in [
            argv(&cfg, &choice),
            probe_args(&choice, FrameFormat::of(&cfg)),
        ] {
            let pos = |a: &str| {
                args.iter()
                    .position(|x| x == a)
                    .unwrap_or_else(|| panic!("`{a}` missing from {args:?}"))
            };
            let input = pos("-i");
            let device = pos("-vaapi_device");
            assert!(device < input, "{args:?}");
            assert_eq!(args[device + 1], VAAPI_DEVICE);
            let vf = pos("-vf");
            assert!(vf > input, "{args:?}");
            assert_eq!(args[vf + 1], "format=nv12,hwupload");
            assert_eq!(args[pos("-c:v") + 1], "h264_vaapi");
            assert_eq!(args[pos("-qp") + 1], "23");
            // Frames are VAAPI surfaces after hwupload: no software output pix_fmt.
            assert!(!args[input..].iter().any(|a| a == "-pix_fmt"), "{args:?}");
        }
        assert_eq!(argv(&cfg, &choice).last().unwrap(), "out.mp4");
    }

    #[test]
    fn videotoolbox_args_match_the_previous_hard_coded_ones() {
        for (codec, name) in [(H264, "h264_videotoolbox"), (Hevc, "hevc_videotoolbox")] {
            let vt = candidates_for("macos", codec)[0];
            assert_eq!(vt.name, name);
            for (crf, q) in [
                (0, "100"),
                (18, "64"),
                (23, "54"),
                (28, "44"),
                (51, "1"),
                (255, "1"),
            ] {
                let choice = vt.choice(crf);
                assert!(choice.pre_input.is_empty());
                assert_eq!(
                    choice.codec_args,
                    ["-c:v", name, "-q:v", q, "-pix_fmt", "yuv420p"]
                );
            }
        }
        // And the whole command line keeps its previous layout.
        let cfg = config(EncoderPreset::H264 { crf: 23 });
        let choice =
            video_encoder_choice(&cfg, |_, _| Ok(candidates_for("macos", H264)[0])).unwrap();
        assert_eq!(
            argv(&cfg, &choice),
            [
                "-hide_banner",
                "-loglevel",
                "warning",
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
                "320x240",
                "-framerate",
                "24/1",
                "-i",
                "pipe:0",
                "-c:v",
                "h264_videotoolbox",
                "-q:v",
                "54",
                "-pix_fmt",
                "yuv420p",
                "out.mp4",
            ]
        );
    }

    #[test]
    fn candidate_order_per_platform() {
        let names = |os: &str, codec| -> Vec<&str> {
            candidates_for(os, codec).iter().map(|c| c.name).collect()
        };
        assert_eq!(
            names("macos", H264),
            ["h264_videotoolbox", "libx264", "libopenh264"]
        );
        assert_eq!(
            names("linux", H264),
            [
                "h264_nvenc",
                "h264_qsv",
                "h264_vaapi",
                "libx264",
                "libopenh264"
            ]
        );
        assert_eq!(
            names("windows", H264),
            [
                "h264_nvenc",
                "h264_qsv",
                "h264_amf",
                "h264_mf",
                "libx264",
                "libopenh264"
            ]
        );
        assert_eq!(names("macos", Hevc), ["hevc_videotoolbox", "libx265"]);
        assert_eq!(
            names("linux", Hevc),
            ["hevc_nvenc", "hevc_qsv", "hevc_vaapi", "libx265"]
        );
        assert_eq!(
            names("windows", Hevc),
            ["hevc_nvenc", "hevc_qsv", "hevc_amf", "hevc_mf", "libx265"]
        );
        assert_eq!(names("freebsd", H264), ["libx264", "libopenh264"]);
        assert_eq!(names("freebsd", Hevc), ["libx265"]);
    }

    #[test]
    fn crf_maps_onto_each_encoders_quality_controls() {
        let args = |os, codec, name, crf| candidate(os, codec, name).choice(crf).codec_args;
        assert_eq!(
            args("linux", H264, "h264_nvenc", 23),
            [
                "-c:v",
                "h264_nvenc",
                "-rc",
                "vbr",
                "-cq",
                "23",
                "-b:v",
                "0",
                "-pix_fmt",
                "yuv420p"
            ]
        );
        assert_eq!(
            args("linux", Hevc, "hevc_qsv", 28),
            [
                "-c:v",
                "hevc_qsv",
                "-global_quality",
                "28",
                "-pix_fmt",
                "nv12"
            ]
        );
        assert_eq!(
            args("windows", H264, "h264_amf", 23),
            [
                "-c:v", "h264_amf", "-rc", "cqp", "-qp_i", "23", "-qp_p", "23", "-qp_b", "23",
                "-pix_fmt", "nv12",
            ]
        );
        assert_eq!(
            args("windows", Hevc, "hevc_amf", 23),
            ["-c:v", "hevc_amf", "-rc", "cqp", "-qp_i", "23", "-qp_p", "23", "-pix_fmt", "nv12"]
        );
        assert_eq!(
            args("windows", H264, "h264_mf", 23),
            [
                "-c:v",
                "h264_mf",
                "-rate_control",
                "quality",
                "-quality",
                "54",
                "-pix_fmt",
                "nv12"
            ]
        );
        assert_eq!(
            args("linux", H264, "libx264", 23),
            ["-c:v", "libx264", "-preset", "medium", "-crf", "23", "-pix_fmt", "yuv420p"]
        );
        assert_eq!(
            args("linux", Hevc, "libx265", 28),
            ["-c:v", "libx265", "-preset", "medium", "-crf", "28", "-pix_fmt", "yuv420p"]
        );
        assert_eq!(
            args("linux", H264, "libopenh264", 23),
            [
                "-c:v",
                "libopenh264",
                "-b:v",
                "8000k",
                "-pix_fmt",
                "yuv420p"
            ]
        );
        // CRF 0 must not land on the "0 = unset / automatic" value of QP knobs.
        assert_eq!(args("linux", H264, "h264_nvenc", 0)[5], "1");
        assert_eq!(args("linux", H264, "h264_qsv", 0)[3], "1");
        assert_eq!(args("linux", H264, "h264_vaapi", 0).last().unwrap(), "1");
        // openh264's bitrate stays bounded at the CRF extremes.
        assert_eq!(openh264_bitrate_kbps(0), 50_000);
        assert_eq!(openh264_bitrate_kbps(51), 500);
        assert_eq!(openh264_bitrate_kbps(255), 500);
    }

    #[test]
    fn every_candidate_outputs_8_bit_420() {
        for os in ["macos", "linux", "windows", "freebsd"] {
            for codec in [H264, Hevc] {
                for c in candidates_for(os, codec) {
                    let args = c.choice(23).codec_args;
                    assert_eq!(args[..2], ["-c:v", c.name]);
                    let has = |k: &str, v: &str| args.windows(2).any(|w| w[0] == k && w[1] == v);
                    assert!(
                        has("-pix_fmt", "yuv420p")
                            || has("-pix_fmt", "nv12")
                            || has("-vf", "format=nv12,hwupload"),
                        "{}: {args:?}",
                        c.name
                    );
                }
            }
        }
    }

    #[test]
    fn fixed_presets_keep_their_args_and_never_run_selection() {
        let cases: [(EncoderPreset, &[&str]); 7] = [
            (
                EncoderPreset::ProRes422HQ,
                &[
                    "-c:v",
                    "prores_ks",
                    "-profile:v",
                    "3",
                    "-pix_fmt",
                    "yuv422p10le",
                ],
            ),
            (
                EncoderPreset::ProRes422,
                &[
                    "-c:v",
                    "prores_ks",
                    "-profile:v",
                    "2",
                    "-pix_fmt",
                    "yuv422p10le",
                ],
            ),
            (
                EncoderPreset::ProRes4444,
                &[
                    "-c:v",
                    "prores_ks",
                    "-profile:v",
                    "4",
                    "-pix_fmt",
                    "yuva444p10le",
                ],
            ),
            (
                EncoderPreset::DnxhrHq,
                &[
                    "-c:v",
                    "dnxhd",
                    "-profile:v",
                    "dnxhr_hq",
                    "-pix_fmt",
                    "yuv422p",
                ],
            ),
            (
                EncoderPreset::DnxhrSq,
                &[
                    "-c:v",
                    "dnxhd",
                    "-profile:v",
                    "dnxhr_sq",
                    "-pix_fmt",
                    "yuv422p",
                ],
            ),
            (
                EncoderPreset::DnxhrLb,
                &[
                    "-c:v",
                    "dnxhd",
                    "-profile:v",
                    "dnxhr_lb",
                    "-pix_fmt",
                    "yuv422p",
                ],
            ),
            (
                EncoderPreset::Av1 { crf: 30 },
                &["-c:v", "libsvtav1", "-crf", "30", "-pix_fmt", "yuv420p"],
            ),
        ];
        for (preset, expected) in cases {
            let choice = video_encoder_choice(&config(preset.clone()), |_, _| {
                panic!("{preset:?} must not run encoder selection")
            })
            .unwrap();
            assert!(choice.pre_input.is_empty());
            assert_eq!(choice.codec_args, expected);
        }
    }

    #[test]
    fn hdr_always_uses_software_hevc_main10_without_selection() {
        for preset in [
            EncoderPreset::H264 { crf: 20 },
            EncoderPreset::Hevc { crf: 20 },
        ] {
            let mut cfg = config(preset);
            cfg.hdr = Some(HdrMetadata::hdr10(1000.0, 0.0001, 1000, 400));
            let choice =
                video_encoder_choice(&cfg, |_, _| panic!("HDR must not run encoder selection"))
                    .unwrap();
            assert!(choice.pre_input.is_empty());
            assert_eq!(
                choice.codec_args,
                hdr_encode_args(cfg.hdr.as_ref().unwrap(), &cfg.preset)
            );
            assert_eq!(
                choice.codec_args[..4],
                ["-c:v", "libx265", "-pix_fmt", "yuv420p10le"]
            );
        }
    }

    #[test]
    fn parses_the_ffmpeg_encoder_listing() {
        let listing = "Encoders:\n V..... = Video\n A..... = Audio\n .....D = Supports direct \
                       rendering method 1\n ------\n V....D libx264              libx264 \
                       H.264 / AVC (codec h264)\n V....D h264_videotoolbox    VideoToolbox \
                       H.264 Encoder (codec h264)\n A....D aac                  AAC (Advanced \
                       Audio Coding)\n";
        assert_eq!(
            parse_encoder_list(listing),
            listed(&["libx264", "h264_videotoolbox", "aac"])
        );
    }

    fn ffmpeg_on_path() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok()
    }

    /// Opens `cfg` the way Open does (encoder selection included), pushes
    /// `frames` frames and returns the encoder used + the output size in bytes.
    fn real_encode(cfg: &VideoConfig, frames: u8) -> (&'static str, u64) {
        let out = std::env::temp_dir().join(format!(
            "vance-enc-select-{}-{}x{}.mp4",
            std::process::id(),
            cfg.width,
            cfg.height
        ));
        let _ = std::fs::remove_file(&out);
        let mut pipe = spawn_video_ffmpeg(cfg, &out).expect("spawn ffmpeg");
        let pixels = cfg.width as usize * cfg.height as usize;
        let mut frame = vec![0u8; pixels * cfg.pixel_format.bytes_per_pixel()];
        let mut written = Ok(());
        for f in 0..frames {
            // Bands of flat color: some content, and cheap to fill at 8K.
            for (i, band) in frame.chunks_mut(4096).enumerate() {
                band.fill((i as u8) ^ f.wrapping_mul(37));
            }
            written = pipe.stdin.write_all(&frame);
            if written.is_err() {
                break;
            }
        }
        drop(pipe.stdin);
        let status = pipe.child.wait().expect("wait ffmpeg");
        let len = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
        let _ = std::fs::remove_file(&out);
        assert!(
            written.is_ok() && status.success(),
            "{} encode at {} failed: write {written:?}, exit {status}",
            pipe.encoder,
            FrameFormat::of(cfg)
        );
        assert!(len > 1_000, "{} wrote only {len} bytes", pipe.encoder);
        (pipe.encoder, len)
    }

    /// Real ffmpeg on this machine: selection must find a working H.264
    /// encoder, and a short encode through the export's own argv must succeed.
    #[test]
    fn real_ffmpeg_selects_a_working_h264_encoder_and_encodes() {
        if !ffmpeg_on_path() {
            eprintln!("ffmpeg not on PATH — skipping real encoder selection test");
            return;
        }
        let cfg = config(EncoderPreset::H264 { crf: 23 });
        let format = FrameFormat::of(&cfg);
        let picked = selected_encoder(H264, format).expect("a working H.264 encoder");
        assert!(candidates_for(std::env::consts::OS, H264).contains(&picked));
        // Cached for the process: a second call returns the same pick.
        assert_eq!(selected_encoder(H264, format), Ok(picked));
        assert_eq!(real_encode(&cfg, 8).0, picked.name);
    }

    /// Real ffmpeg at 8K, which many hardware H.264 encoders refuse (an M1's
    /// h264_videotoolbox passes a small probe, then can't open at 8K): the
    /// export must get an encoder that works at its own size.
    #[test]
    fn real_ffmpeg_8k_h264_export_gets_an_encoder_that_handles_8k() {
        if !ffmpeg_on_path() {
            eprintln!("ffmpeg not on PATH — skipping real 8K encoder selection test");
            return;
        }
        let mut cfg = config(EncoderPreset::H264 { crf: 23 });
        (cfg.width, cfg.height) = (7680, 4320);
        let (encoder, len) = real_encode(&cfg, 2);
        eprintln!("8K H.264 export used {encoder} ({len} bytes)");
    }
}
