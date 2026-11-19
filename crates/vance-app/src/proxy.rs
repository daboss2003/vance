//! Phase 9.4 — background proxy generation worker.
//!
//! Transcodes user-supplied source media to a low-bandwidth
//! easy-to-decode format (ProRes 422 Proxy by default, or DNxHR LB)
//! so smooth scrubbing + playback work on big 6K/10-bit sources
//! without a hardware decoder.
//!
//! Architecture:
//! - A long-running worker thread holds a job queue.
//! - Each job: open source via a dedicated decoder sidecar, walk
//!   frames in stream order, push every decoded frame to a fresh
//!   encoder sidecar, close.
//! - Status (queued / running / done / error) is exposed via a
//!   shared snapshot the UI can poll.
//! - When playback or the timeline asks for proxy frames, it uses
//!   the proxy file's path in place of the original source URI.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::Mutex;
use vance_decoder::protocol::PixelFormat;
use vance_decoder::DecoderClient;
use vance_encoder::{
    AudioCodec, AudioConfig, AudioSampleFormat, EncoderClient, EncoderPreset, VideoConfig,
    VideoPixelFormat,
};

/// Codec for generated proxies. Both are fast-to-decode intra-frame
/// intermediates that make heavy 6K/10-bit sources play back smoothly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ProxyCodec {
    /// Apple ProRes 422 — broad NLE compatibility (default).
    #[default]
    ProRes422,
    /// Avid DNxHR LB — low-bandwidth; the smallest proxy files.
    DnxhrLb,
}

impl ProxyCodec {
    /// The encoder preset this codec maps to.
    pub fn preset(self) -> EncoderPreset {
        match self {
            ProxyCodec::ProRes422 => EncoderPreset::ProRes422,
            ProxyCodec::DnxhrLb => EncoderPreset::DnxhrLb,
        }
    }

    /// Parse a UI/IPC string (`"prores"`, `"dnxhr_lb"`); unknown → default.
    pub fn from_ui_str(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "dnxhr_lb" | "dnxhr" | "dnxhrlb" | "dnxhr-lb" => ProxyCodec::DnxhrLb,
            _ => ProxyCodec::ProRes422,
        }
    }
}

/// DNxHR's `dnxhd` encoder has a hard 256x120 floor — it accepts the
/// geometry at spawn but rejects it once frames arrive, so a sub-256x120
/// source would fail the proxy mid-stream with a broken pipe. ProRes has
/// no such floor, so fall back to ProRes 422 for tiny/vertical sources.
/// Returns the preset to actually encode with.
pub fn resolve_proxy_preset(requested: EncoderPreset, width: u32, height: u32) -> EncoderPreset {
    match requested {
        EncoderPreset::DnxhrLb | EncoderPreset::DnxhrSq | EncoderPreset::DnxhrHq
            if width < 256 || height < 120 =>
        {
            EncoderPreset::ProRes422
        }
        other => other,
    }
}

/// One proxy-generation job.
#[derive(Clone, Debug)]
pub struct ProxyJob {
    /// Stable id (UUID hex) used to correlate UI requests with status.
    pub id: String,
    /// Absolute path to the source media file.
    pub source_path: String,
    /// Absolute path to write the proxy to (typically next to the
    /// source with a `.proxy.mov` suffix).
    pub output_path: String,
    /// Encoder preset for the proxy. ProRes 422 Proxy is the default.
    pub preset: EncoderPreset,
}

/// Runtime status of one job.
#[derive(Clone, Debug, PartialEq)]
pub enum ProxyJobStatus {
    /// Sitting in the worker queue.
    Queued,
    /// In flight — `frames_done` reflects per-frame progress.
    Running { frames_done: u64 },
    /// Finished successfully.
    Done { frames_done: u64 },
    /// Failed with a human-readable error.
    Error(String),
}

/// Snapshot view returned by [`ProxyWorker::status_snapshot`].
#[derive(Clone, Debug)]
pub struct ProxyJobSnapshot {
    /// Job id.
    pub id: String,
    /// Source path.
    pub source_path: String,
    /// Proxy output path.
    pub output_path: String,
    /// Current state.
    pub status: ProxyJobStatus,
}

/// Worker thread that processes proxy jobs sequentially.
pub struct ProxyWorker {
    queue: Arc<Mutex<VecDeque<ProxyJob>>>,
    status: Arc<Mutex<Vec<ProxyJobSnapshot>>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ProxyWorker {
    /// Spawn the worker. Takes decoder + encoder binary paths so it
    /// can lazily start sidecars per job.
    pub fn spawn(decoder_binary: PathBuf, encoder_binary: PathBuf) -> Self {
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let status = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let q = queue.clone();
        let s = status.clone();
        let sd = shutdown.clone();
        let handle = thread::Builder::new()
            .name("vance.proxy".into())
            .spawn(move || worker_loop(q, s, sd, decoder_binary, encoder_binary))
            .expect("spawn proxy worker");
        Self {
            queue,
            status,
            shutdown,
            handle: Some(handle),
        }
    }

    /// Enqueue a job. Returns the assigned id.
    pub fn enqueue(&self, mut job: ProxyJob) -> String {
        if job.id.is_empty() {
            job.id = format!("job-{}", std::process::id());
        }
        let snap = ProxyJobSnapshot {
            id: job.id.clone(),
            source_path: job.source_path.clone(),
            output_path: job.output_path.clone(),
            status: ProxyJobStatus::Queued,
        };
        self.status.lock().push(snap);
        let id = job.id.clone();
        self.queue.lock().push_back(job);
        id
    }

    /// Snapshot of all known jobs (cleared on `shutdown`).
    pub fn status_snapshot(&self) -> Vec<ProxyJobSnapshot> {
        self.status.lock().clone()
    }
}

impl Drop for ProxyWorker {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn worker_loop(
    queue: Arc<Mutex<VecDeque<ProxyJob>>>,
    status: Arc<Mutex<Vec<ProxyJobSnapshot>>>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    decoder_binary: PathBuf,
    encoder_binary: PathBuf,
) {
    loop {
        if shutdown.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        let next = queue.lock().pop_front();
        let Some(job) = next else {
            thread::sleep(Duration::from_millis(100));
            continue;
        };
        set_status(&status, &job.id, ProxyJobStatus::Running { frames_done: 0 });
        let result = run_one(&job, &decoder_binary, &encoder_binary, &status);
        let final_status = match result {
            Ok(n) => ProxyJobStatus::Done { frames_done: n },
            Err(e) => ProxyJobStatus::Error(e),
        };
        set_status(&status, &job.id, final_status);
    }
}

fn run_one(
    job: &ProxyJob,
    decoder_binary: &PathBuf,
    encoder_binary: &PathBuf,
    status: &Arc<Mutex<Vec<ProxyJobSnapshot>>>,
) -> Result<u64, String> {
    let mut decoder =
        DecoderClient::spawn(decoder_binary).map_err(|e| format!("decoder spawn: {e}"))?;
    let stream = decoder
        .open(job.source_path.clone())
        .map_err(|e| format!("decoder open: {e}"))?;
    if stream.frame_rate.1 == 0 {
        return Err("source has no frame rate metadata; cannot generate proxy".into());
    }

    // DNxHR has a hard 256x120 minimum (see `resolve_proxy_preset`); fall
    // back to ProRes for tiny/vertical sources so the proxy still produces
    // a usable file instead of failing mid-stream.
    let preset = resolve_proxy_preset(job.preset.clone(), stream.width, stream.height);
    if preset != job.preset {
        tracing::warn!(
            w = stream.width,
            h = stream.height,
            requested = ?job.preset,
            "DNxHR requires at least 256x120; falling back to ProRes 422 for this proxy"
        );
    }

    let mut encoder =
        EncoderClient::spawn(encoder_binary).map_err(|e| format!("encoder spawn: {e}"))?;
    // Proxy is video-only — audio passes through untouched at use
    // time (Phase 9.5 toggle decides which file feeds audio).
    encoder
        .open(
            job.output_path.clone(),
            Some(VideoConfig {
                width: stream.width,
                height: stream.height,
                frame_rate_num: stream.frame_rate.0 as u32,
                frame_rate_den: stream.frame_rate.1 as u32,
                pixel_format: VideoPixelFormat::Rgba8,
                preset,
                time_base_num: stream.frame_rate.1 as u32,
                time_base_den: stream.frame_rate.0 as u32,
                hdr: None, // proxies are SDR
            }),
            // Audio-side stub: silent track at 48 kHz stereo. ProRes
            // muxers expect at least one audio stream for the typical
            // QuickTime flow; users get a true silent track on the
            // proxy and the original audio when scrubbing.
            Some(AudioConfig {
                sample_rate: 48_000,
                channels: 2,
                sample_format: AudioSampleFormat::F32Interleaved,
                codec: AudioCodec::PcmS24,
                time_base_num: 1,
                time_base_den: 48_000,
            }),
        )
        .map_err(|e| format!("encoder open: {e}"))?;

    let pixel_count = (stream.width as usize) * (stream.height as usize) * 4;
    let mut frame_index: u64 = 0;
    let step_pts = if stream.frame_rate.0 > 0 && stream.frame_rate.1 > 0 {
        let (tb_n, tb_d) = stream.time_base;
        if tb_n <= 0 {
            return Err("invalid stream time_base".into());
        }
        let num = tb_d * stream.frame_rate.1;
        let den = tb_n * stream.frame_rate.0;
        if den == 0 {
            return Err("step_pts division by zero".into());
        }
        num / den
    } else {
        1
    };
    let mut next_pts: i64 = 0;
    let frames_total = stream
        .duration_pts
        .map(|d| {
            if step_pts > 0 {
                (d / step_pts) as u64
            } else {
                0
            }
        })
        .unwrap_or(0);
    // Decode at next_pts. If we ran past the duration_pts, the
    // decoder will return Err — treat that as natural EOS.
    while let Ok(frame) = decoder.decode_at(next_pts) {
        // Convert YUV (or NV12) → RGBA via a simple in-Rust BT.709
        // pass. This is the SLOW path; future work routes through
        // the GPU renderer's readback. For v1 we want correctness
        // over throughput.
        let rgba = yuv_or_nv12_to_rgba(&frame, pixel_count);
        encoder
            .push_video(frame_index as i64, rgba)
            .map_err(|e| format!("push_video: {e}"))?;
        // Push a frame's worth of silence so the audio track
        // exists alongside the video.
        let samples_per_frame =
            (48_000u64 * stream.frame_rate.1 as u64 / stream.frame_rate.0 as u64) as usize;
        encoder
            .push_audio(0, vec![0f32; samples_per_frame * 2])
            .map_err(|e| format!("push_audio: {e}"))?;
        frame_index += 1;
        next_pts += step_pts;

        // Surface progress to the UI every 12 frames.
        if frame_index.is_multiple_of(12) {
            set_status(
                status,
                &job.id,
                ProxyJobStatus::Running {
                    frames_done: frame_index,
                },
            );
        }
        // Bail when we've covered the declared duration.
        if frames_total > 0 && frame_index >= frames_total {
            break;
        }
    }

    encoder.finish().map_err(|e| format!("finish: {e}"))?;
    let _ = encoder.shutdown();
    Ok(frame_index)
}

fn set_status(status: &Arc<Mutex<Vec<ProxyJobSnapshot>>>, id: &str, new_status: ProxyJobStatus) {
    let mut s = status.lock();
    if let Some(slot) = s.iter_mut().find(|j| j.id == id) {
        slot.status = new_status;
    }
}

/// YUV-or-NV12 → RGBA. Uses BT.709 limited-range coefficients —
/// matches the engine's default render pipeline. For 4:2:0 sources
/// the chroma is upsampled nearest-neighbour (cheap; acceptable for
/// a proxy file that the user will scrub but rarely export from).
fn yuv_or_nv12_to_rgba(
    frame: &vance_decoder::protocol::FrameData,
    pixel_count_rgba: usize,
) -> Vec<u8> {
    let w = frame.width as usize;
    let h = frame.height as usize;
    let mut out = vec![0u8; pixel_count_rgba];
    match frame.pixel_format {
        PixelFormat::Yuv420P => {
            let stride_y = frame.strides[0] as usize;
            let stride_u = frame.strides[1] as usize;
            let stride_v = frame.strides[2] as usize;
            let y_plane = &frame.bytes[..stride_y * h];
            let u_off = stride_y * h;
            let u_plane = &frame.bytes[u_off..u_off + stride_u * (h / 2)];
            let v_off = u_off + stride_u * (h / 2);
            let v_plane = &frame.bytes[v_off..v_off + stride_v * (h / 2)];
            for y in 0..h {
                for x in 0..w {
                    let y_b = y_plane[y * stride_y + x] as f32;
                    let u_b = u_plane[(y / 2) * stride_u + (x / 2)] as f32;
                    let v_b = v_plane[(y / 2) * stride_v + (x / 2)] as f32;
                    let (r, g, b) = bt709_full_range(y_b, u_b, v_b);
                    let i = (y * w + x) * 4;
                    out[i] = r;
                    out[i + 1] = g;
                    out[i + 2] = b;
                    out[i + 3] = 255;
                }
            }
        }
        PixelFormat::Yuv420P10Le => {
            // 10-bit planar: samples are little-endian u16; strides are in
            // samples. Downshift 10→8 bit for the (8-bit, scrub-only) proxy.
            let sy = frame.strides[0] as usize;
            let su = frame.strides[1] as usize;
            let sv = frame.strides[2] as usize;
            let y_bytes = sy * 2 * h;
            let u_bytes = su * 2 * (h / 2);
            let y_plane = &frame.bytes[..y_bytes];
            let u_plane = &frame.bytes[y_bytes..y_bytes + u_bytes];
            let v_plane = &frame.bytes[y_bytes + u_bytes..y_bytes + u_bytes + sv * 2 * (h / 2)];
            let s16 = |plane: &[u8], idx: usize| -> f32 {
                let b = idx * 2;
                (u16::from_le_bytes([plane[b], plane[b + 1]]) >> 2) as f32
            };
            for y in 0..h {
                for x in 0..w {
                    let y_b = s16(y_plane, y * sy + x);
                    let u_b = s16(u_plane, (y / 2) * su + (x / 2));
                    let v_b = s16(v_plane, (y / 2) * sv + (x / 2));
                    let (r, g, b) = bt709_full_range(y_b, u_b, v_b);
                    let i = (y * w + x) * 4;
                    out[i] = r;
                    out[i + 1] = g;
                    out[i + 2] = b;
                    out[i + 3] = 255;
                }
            }
        }
        PixelFormat::Nv12 => {
            let stride_y = frame.strides[0] as usize;
            let stride_uv = frame.strides[1] as usize;
            let y_plane = &frame.bytes[..stride_y * h];
            let uv_off = stride_y * h;
            let uv_plane = &frame.bytes[uv_off..uv_off + stride_uv * (h / 2)];
            for y in 0..h {
                for x in 0..w {
                    let y_b = y_plane[y * stride_y + x] as f32;
                    let uv_row = (y / 2) * stride_uv;
                    let uv_col = (x / 2) * 2;
                    let u_b = uv_plane[uv_row + uv_col] as f32;
                    let v_b = uv_plane[uv_row + uv_col + 1] as f32;
                    let (r, g, b) = bt709_full_range(y_b, u_b, v_b);
                    let i = (y * w + x) * 4;
                    out[i] = r;
                    out[i + 1] = g;
                    out[i + 2] = b;
                    out[i + 3] = 255;
                }
            }
        }
    }
    out
}

#[inline]
fn bt709_full_range(y: f32, u: f32, v: f32) -> (u8, u8, u8) {
    // BT.709 full-range conversion (assumes input is already
    // limited→full normalised at the encoder). For limited-range
    // sources the colours will be slightly low-contrast; acceptable
    // for a scrub proxy.
    let y_n = y;
    let u_n = u - 128.0;
    let v_n = v - 128.0;
    let r = (y_n + 1.5748 * v_n).clamp(0.0, 255.0) as u8;
    let g = (y_n - 0.1873 * u_n - 0.4681 * v_n).clamp(0.0, 255.0) as u8;
    let b = (y_n + 1.8556 * u_n).clamp(0.0, 255.0) as u8;
    (r, g, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dnxhr_falls_back_to_prores_below_256x120() {
        use EncoderPreset::*;
        // DNxHR below the floor → ProRes 422.
        assert_eq!(resolve_proxy_preset(DnxhrLb, 200, 200), ProRes422);
        assert_eq!(resolve_proxy_preset(DnxhrLb, 1920, 100), ProRes422); // height < 120
        assert_eq!(resolve_proxy_preset(DnxhrLb, 255, 1080), ProRes422); // width < 256
        assert_eq!(resolve_proxy_preset(DnxhrHq, 64, 64), ProRes422);
        // DNxHR at/above the floor → unchanged.
        assert_eq!(resolve_proxy_preset(DnxhrLb, 256, 120), DnxhrLb);
        assert_eq!(resolve_proxy_preset(DnxhrLb, 1920, 1080), DnxhrLb);
        // ProRes is never floored (no minimum).
        assert_eq!(resolve_proxy_preset(ProRes422, 64, 64), ProRes422);
        assert_eq!(resolve_proxy_preset(ProRes422HQ, 16, 16), ProRes422HQ);
    }

    #[test]
    fn proxy_codec_maps_and_parses() {
        assert_eq!(ProxyCodec::default(), ProxyCodec::ProRes422);
        assert_eq!(ProxyCodec::ProRes422.preset(), EncoderPreset::ProRes422);
        assert_eq!(ProxyCodec::DnxhrLb.preset(), EncoderPreset::DnxhrLb);
        assert_eq!(ProxyCodec::from_ui_str("dnxhr_lb"), ProxyCodec::DnxhrLb);
        assert_eq!(ProxyCodec::from_ui_str("DNxHR"), ProxyCodec::DnxhrLb);
        assert_eq!(ProxyCodec::from_ui_str("prores"), ProxyCodec::ProRes422);
        assert_eq!(ProxyCodec::from_ui_str("nonsense"), ProxyCodec::ProRes422);
    }

    #[test]
    fn bt709_grey_round_trips() {
        let (r, g, b) = bt709_full_range(128.0, 128.0, 128.0);
        assert_eq!(r, 128);
        assert_eq!(g, 128);
        assert_eq!(b, 128);
    }

    #[test]
    fn bt709_black_is_black() {
        let (r, g, b) = bt709_full_range(0.0, 128.0, 128.0);
        assert_eq!(r, 0);
        assert_eq!(g, 0);
        assert_eq!(b, 0);
    }

    #[test]
    fn bt709_white_is_white() {
        let (r, g, b) = bt709_full_range(255.0, 128.0, 128.0);
        assert_eq!(r, 255);
        assert_eq!(g, 255);
        assert_eq!(b, 255);
    }

    #[test]
    fn yuv420p_passthrough_buffer_size() {
        let w = 16u32;
        let h = 16u32;
        let frame = vance_decoder::protocol::FrameData {
            pts: 0,
            width: w,
            height: h,
            pixel_format: PixelFormat::Yuv420P,
            strides: vec![w, w / 2, w / 2],
            bytes: vec![128u8; (w * h + 2 * (w / 2) * (h / 2)) as usize],
        };
        let out = yuv_or_nv12_to_rgba(&frame, (w * h * 4) as usize);
        assert_eq!(out.len(), (w * h * 4) as usize);
        // All-grey input → all 128 RGB + 255 A.
        for chunk in out.chunks_exact(4) {
            assert_eq!(chunk[0], 128);
            assert_eq!(chunk[1], 128);
            assert_eq!(chunk[2], 128);
            assert_eq!(chunk[3], 255);
        }
    }
}
