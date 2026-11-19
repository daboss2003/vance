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

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use vance_encoder::protocol::{
    AudioCodec, AudioConfig, EncoderError, EncoderPreset, EncoderRequest, EncoderResponse,
    HdrMetadata, VideoConfig, PROTOCOL_VERSION,
};

fn main() -> vance_ipc::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    tracing::info!("encoder sidecar starting");

    let mut state: State = State::default();
    vance_ipc::serve::<EncoderRequest, EncoderResponse, _>(|req| state.handle(req))
}

#[derive(Default)]
struct State {
    open: Option<OpenSession>,
}

struct OpenSession {
    final_path: String,
    video: Option<VideoPipe>,
    audio: Option<AudioPipe>,
}

struct VideoPipe {
    child: Child,
    stdin: ChildStdin,
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
    fn handle(&mut self, req: EncoderRequest) -> EncoderResponse {
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
            EncoderRequest::PushVideoFrame { pts: _, pixels } => match self.push_video(&pixels) {
                Ok(()) => EncoderResponse::FramePushed,
                Err(e) => EncoderResponse::EncodeFailed(EncoderError::new(e)),
            },
            EncoderRequest::PushAudioFrame { pts: _, samples } => match self.push_audio(&samples) {
                Ok(()) => EncoderResponse::FramePushed,
                Err(e) => EncoderResponse::EncodeFailed(EncoderError::new(e)),
            },
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

    fn push_video(&mut self, pixels: &[u8]) -> Result<(), String> {
        let s = self.open.as_mut().ok_or("encoder not open")?;
        let v = s.video.as_mut().ok_or("no video stream configured")?;
        let expected = (v.width as usize) * (v.height as usize) * v.bytes_per_pixel;
        if pixels.len() != expected {
            return Err(format!(
                "video frame size mismatch: got {} bytes, expected {}",
                pixels.len(),
                expected
            ));
        }
        v.stdin
            .write_all(pixels)
            .map_err(|e| format!("write to ffmpeg stdin: {e}"))?;
        v.frames += 1;
        Ok(())
    }

    fn push_audio(&mut self, samples: &[f32]) -> Result<(), String> {
        let s = self.open.as_mut().ok_or("encoder not open")?;
        let a = s.audio.as_mut().ok_or("no audio stream configured")?;
        for &sample in samples {
            let clamped = sample.clamp(-1.0, 1.0);
            let v = (clamped * 8_388_607.0) as i32; // 2^23 - 1
            let bytes = v.to_le_bytes();
            a.file
                .write_all(&bytes[0..3])
                .map_err(|e| format!("wav write: {e}"))?;
            a.bytes_written += 3;
        }
        a.samples_written += (samples.len() / a.channels as usize) as u64;
        Ok(())
    }

    fn finish(&mut self) -> Result<(u64, u64), String> {
        let mut s = self.open.take().ok_or("encoder not open")?;

        // 1. Close the video ffmpeg child and wait.
        let (video_frames, video_out, had_video) = if let Some(mut v) = s.video.take() {
            drop(v.stdin); // signal EOF
            let status = v.child.wait().map_err(|e| format!("wait ffmpeg: {e}"))?;
            if !status.success() {
                return Err(format!("ffmpeg video exited with {status:?}"));
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

fn spawn_video_ffmpeg(cfg: &VideoConfig, out_path: &PathBuf) -> Result<VideoPipe, String> {
    let pix_fmt = cfg.pixel_format.ffmpeg_rawvideo();
    // HDR10 export overrides the preset with a software HEVC Main10 path so
    // the mastering metadata + PQ signaling are emitted reliably (hardware
    // encoders don't accept x265-params); otherwise use the chosen preset.
    let video_codec_args = match &cfg.hdr {
        Some(hdr) => hdr_encode_args(hdr, &cfg.preset),
        None => preset_to_ffmpeg_args(&cfg.preset),
    };

    let frame_rate = format!("{}/{}", cfg.frame_rate_num, cfg.frame_rate_den);
    let size = format!("{}x{}", cfg.width, cfg.height);

    let mut cmd = Command::new("ffmpeg");
    cmd.arg("-hide_banner")
        .arg("-loglevel")
        .arg("warning")
        .arg("-y")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg(pix_fmt)
        .arg("-s")
        .arg(&size)
        .arg("-framerate")
        .arg(&frame_rate)
        .arg("-i")
        .arg("pipe:0");
    for a in video_codec_args {
        cmd.arg(a);
    }
    cmd.arg(out_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn ffmpeg (is it on PATH?): {e}"))?;
    let stdin = child.stdin.take().ok_or("ffmpeg stdin unavailable")?;

    Ok(VideoPipe {
        child,
        stdin,
        width: cfg.width,
        height: cfg.height,
        bytes_per_pixel: cfg.pixel_format.bytes_per_pixel(),
        out_path: out_path.clone(),
        frames: 0,
    })
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

fn preset_to_ffmpeg_args(preset: &EncoderPreset) -> Vec<String> {
    let v = |s: &str| s.to_string();
    match preset {
        EncoderPreset::ProRes422HQ => vec![
            v("-c:v"),
            v("prores_ks"),
            v("-profile:v"),
            v("3"),
            v("-pix_fmt"),
            v("yuv422p10le"),
        ],
        EncoderPreset::ProRes422 => vec![
            v("-c:v"),
            v("prores_ks"),
            v("-profile:v"),
            v("2"),
            v("-pix_fmt"),
            v("yuv422p10le"),
        ],
        EncoderPreset::ProRes4444 => vec![
            v("-c:v"),
            v("prores_ks"),
            v("-profile:v"),
            v("4"),
            v("-pix_fmt"),
            v("yuva444p10le"),
        ],
        EncoderPreset::DnxhrHq => vec![
            v("-c:v"),
            v("dnxhd"),
            v("-profile:v"),
            v("dnxhr_hq"),
            v("-pix_fmt"),
            v("yuv422p"),
        ],
        EncoderPreset::DnxhrSq => vec![
            v("-c:v"),
            v("dnxhd"),
            v("-profile:v"),
            v("dnxhr_sq"),
            v("-pix_fmt"),
            v("yuv422p"),
        ],
        EncoderPreset::DnxhrLb => vec![
            v("-c:v"),
            v("dnxhd"),
            v("-profile:v"),
            v("dnxhr_lb"),
            v("-pix_fmt"),
            v("yuv422p"),
        ],
        EncoderPreset::H264 { crf } => {
            let q = (100 - (*crf as i32) * 2).clamp(1, 100);
            #[cfg(target_os = "macos")]
            {
                vec![
                    v("-c:v"),
                    v("h264_videotoolbox"),
                    v("-q:v"),
                    q.to_string(),
                    v("-pix_fmt"),
                    v("yuv420p"),
                ]
            }
            #[cfg(target_os = "linux")]
            {
                vec![
                    v("-c:v"),
                    v("h264_vaapi"),
                    v("-q:v"),
                    q.to_string(),
                    v("-pix_fmt"),
                    v("yuv420p"),
                ]
            }
            #[cfg(target_os = "windows")]
            {
                vec![v("-c:v"), v("h264_mf"), v("-pix_fmt"), v("yuv420p")]
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
            {
                vec![
                    v("-c:v"),
                    v("libx264"),
                    v("-crf"),
                    crf.to_string(),
                    v("-pix_fmt"),
                    v("yuv420p"),
                ]
            }
        }
        EncoderPreset::Hevc { crf } => {
            let q = (100 - (*crf as i32) * 2).clamp(1, 100);
            #[cfg(target_os = "macos")]
            {
                vec![
                    v("-c:v"),
                    v("hevc_videotoolbox"),
                    v("-q:v"),
                    q.to_string(),
                    v("-pix_fmt"),
                    v("yuv420p"),
                ]
            }
            #[cfg(not(target_os = "macos"))]
            {
                vec![
                    v("-c:v"),
                    v("libx265"),
                    v("-crf"),
                    crf.to_string(),
                    v("-pix_fmt"),
                    v("yuv420p"),
                ]
            }
        }
        EncoderPreset::Av1 { crf } => vec![
            v("-c:v"),
            v("libsvtav1"),
            v("-crf"),
            crf.to_string(),
            v("-pix_fmt"),
            v("yuv420p"),
        ],
    }
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
