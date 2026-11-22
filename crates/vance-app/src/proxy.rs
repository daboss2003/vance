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
//!
//! A proxy is half the source's DISPLAY size (anamorphic pixels made
//! square), even-rounded (user decision 5). Its pixels are the source's
//! R'G'B' as the renderer decodes them (the stream's range and matrix, with
//! the renderer's defaults when unsignalled), so it previews in the same
//! colours; 10-bit sources go to the encoder at 16 bits, and a source with
//! alpha gets a ProRes 4444 proxy that keeps it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use parking_lot::Mutex;
use vance_decoder::protocol::{ColorMatrixHint, ColorRangeHint, FrameData, PixelFormat};
use vance_decoder::DecoderClient;
use vance_encoder::{
    AudioCodec, AudioConfig, AudioSampleFormat, EncoderClient, EncoderPreset, VideoConfig,
    VideoPixelFormat,
};

/// The proxy format this build writes: half the display size, the source's
/// colours decoded as the renderer decodes them (with the media's input
/// space for what the stream doesn't signal), tagged with what they are
/// (BT.709 SDR, or the source's PQ / HLG BT.2020). Proxies made before
/// (version 1: full size, BT.601-converted and untagged, washed out;
/// version 2: HDR ones tagged as SDR; version 3: no record of the input
/// space used; version 4: HDR ones in HEVC) aren't current, so the engine
/// remakes them.
pub const PROXY_FORMAT_VERSION: u32 = 5;

/// The input space a proxy of media tagged `tag` decodes its source with:
/// the renderer's (`compose::input_space`): the tag, else Rec.709 limited,
/// and Rec.709 limited for a space the built-in pipeline can't read.
pub fn effective_input_space(tag: Option<vance_color::ColorSpace>) -> vance_color::ColorSpace {
    let cs = tag.unwrap_or(vance_color::ColorSpace::Rec709Limited);
    if cs.is_builtin_input_supported() {
        cs
    } else {
        vance_color::ColorSpace::Rec709Limited
    }
}

/// Whether `proxy` is current AND was made for media tagged `tag` (its
/// colours depend on the input space; a re-tagged item needs a new one).
pub fn matches_media(proxy: &Path, tag: Option<vance_color::ColorSpace>) -> bool {
    current_marker(proxy).is_some_and(|m| m.input_space == Some(effective_input_space(tag)))
}

/// What `<proxy>.vance-proxy` records about a proxy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProxyMarker {
    /// The format that made it ([`PROXY_FORMAT_VERSION`] when current).
    pub version: u32,
    /// The codec it was made with: a remake uses it again.
    pub codec: ProxyCodec,
    /// The ORIGINAL's stored frame size, when recorded: what the engine
    /// scales source-pixel parameters by while previewing the proxy.
    pub original_size: Option<(u32, u32)>,
    /// The original's sample aspect ratio, `(1, 1)` for square pixels.
    pub original_sar: (u32, u32),
    /// The input space the source was decoded with (see
    /// [`effective_input_space`]), when recorded.
    pub input_space: Option<vance_color::ColorSpace>,
    /// The proxy file's length when it was marked, so a file replaced by
    /// another one isn't taken for current.
    pub bytes: u64,
}

impl ProxyMarker {
    fn to_text(self) -> String {
        let (w, h) = self.original_size.unwrap_or((0, 0));
        let (n, d) = self.original_sar;
        let space = self
            .input_space
            .map_or_else(|| "unknown".to_string(), crate::engine::color_space_to_str);
        format!(
            "vance-proxy-format={} codec={} width={w} height={h} sar={n}:{d} space={space} \
             bytes={}\n",
            self.version,
            self.codec.as_ui_str(),
            self.bytes
        )
    }

    /// Reads any version's marker (fields an older one lacks get defaults).
    fn parse(text: &str) -> Option<Self> {
        let mut m = ProxyMarker {
            version: 0,
            codec: ProxyCodec::default(),
            original_size: None,
            original_sar: (1, 1),
            input_space: None,
            bytes: 0,
        };
        let (mut w, mut h) = (0u32, 0u32);
        for field in text.split_whitespace() {
            let (key, value) = field.split_once('=')?;
            match key {
                "vance-proxy-format" => m.version = value.parse().ok()?,
                "codec" => m.codec = ProxyCodec::from_ui_str(value),
                "width" => w = value.parse().ok()?,
                "height" => h = value.parse().ok()?,
                "sar" => {
                    let (n, d) = value.split_once(':')?;
                    m.original_sar = (n.parse().ok()?, d.parse().ok()?);
                }
                "space" => m.input_space = crate::engine::color_space_from_str(value),
                "bytes" => m.bytes = value.parse().ok()?,
                _ => {}
            }
        }
        m.original_size = (w > 0 && h > 0).then_some((w, h));
        (m.version > 0).then_some(m)
    }
}

/// The marker file beside `proxy` that says which format made it.
fn marker_path(proxy: &Path) -> PathBuf {
    let mut name = proxy.as_os_str().to_owned();
    name.push(".vance-proxy");
    PathBuf::from(name)
}

/// What [`mark_current`] records about the source.
#[derive(Clone, Copy, Debug)]
pub struct ProxySource {
    /// The codec the proxy was made with.
    pub codec: ProxyCodec,
    /// The original's stored frame size.
    pub size: (u32, u32),
    /// The original's sample aspect ratio.
    pub sar: (u32, u32),
    /// The input space its frames were decoded with.
    pub input_space: vance_color::ColorSpace,
}

/// Mark the finished `proxy` as made in the current format from `source`.
pub fn mark_current(proxy: &Path, source: ProxySource) -> std::io::Result<()> {
    let marker = ProxyMarker {
        version: PROXY_FORMAT_VERSION,
        codec: source.codec,
        original_size: Some(source.size),
        original_sar: source.sar,
        input_space: Some(source.input_space),
        bytes: std::fs::metadata(proxy)?.len(),
    };
    std::fs::write(marker_path(proxy), marker.to_text())
}

/// `proxy`'s marker, any version, or `None` without one.
pub fn read_marker(proxy: &Path) -> Option<ProxyMarker> {
    ProxyMarker::parse(&std::fs::read_to_string(marker_path(proxy)).ok()?)
}

/// `proxy`'s marker when it was made in the current format and is
/// unchanged since.
pub fn current_marker(proxy: &Path) -> Option<ProxyMarker> {
    let len = std::fs::metadata(proxy).ok()?.len();
    read_marker(proxy).filter(|m| m.version == PROXY_FORMAT_VERSION && m.bytes == len)
}

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

    /// The codec a proxy preset belongs to (DNxHR of any profile → DNxHR LB).
    pub fn of_preset(preset: &EncoderPreset) -> Self {
        match preset {
            EncoderPreset::DnxhrLb | EncoderPreset::DnxhrSq | EncoderPreset::DnxhrHq => {
                ProxyCodec::DnxhrLb
            }
            _ => ProxyCodec::ProRes422,
        }
    }

    /// The UI/IPC name [`Self::from_ui_str`] reads back.
    pub fn as_ui_str(self) -> &'static str {
        match self {
            ProxyCodec::ProRes422 => "prores",
            ProxyCodec::DnxhrLb => "dnxhr_lb",
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
    /// Encoder preset for the proxy. ProRes 422 Proxy is the default; a
    /// source with alpha always gets ProRes 4444.
    pub preset: EncoderPreset,
    /// The source's tagged input colour space, if any: its primaries pick
    /// the YUV matrix of a stream that doesn't signal one, as the renderer
    /// does.
    pub input_color_space: Option<vance_color::ColorSpace>,
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

    let (width, height) = proxy_size(stream.width, stream.height, stream.sample_aspect);
    let alpha = stream.pixel_format.is_rgba();
    let input = effective_input_space(job.input_color_space);
    // The proxy holds the source's encoded values, so it is tagged as the
    // source is: an HDR source's proxy says PQ / HLG BT.2020.
    // Intentional: not for alpha sources; ProRes 4444 is the only proxy
    // codec that keeps alpha, and alpha HDR footage is rare.
    let hdr = if alpha {
        None
    } else {
        hdr_signaling(input.decompose().1)
    };
    // Only ProRes 4444 carries alpha. HDR goes to 10-bit ProRes 422 Proxy:
    // intra-frame like every proxy, and ProRes keeps the HDR tags (another
    // codec with the HDR option would be long-GOP HEVC). DNxHR has a hard
    // 256x120 minimum (see `resolve_proxy_preset`); fall back to ProRes for
    // tiny/vertical proxies so they still produce a usable file instead of
    // failing mid-stream.
    let preset = match (alpha, hdr.is_some()) {
        (true, _) => EncoderPreset::ProRes4444,
        (false, true) => EncoderPreset::ProRes422Proxy,
        (false, false) => resolve_proxy_preset(job.preset.clone(), width, height),
    };
    if preset != job.preset {
        tracing::info!(
            width,
            height,
            alpha,
            hdr = hdr.is_some(),
            requested = ?job.preset,
            chosen = ?preset,
            "proxy preset changed for this source"
        );
    }
    // 10-bit sources go to the encoder at 16 bits, so a 10-bit proxy codec
    // (ProRes) keeps their precision; DNxHR LB is 8-bit.
    let deep = stream.pixel_format == PixelFormat::Yuv420P10Le;
    let pixel_format = if deep {
        VideoPixelFormat::Rgba64Le
    } else {
        VideoPixelFormat::Rgba8
    };

    let mut encoder =
        EncoderClient::spawn(encoder_binary).map_err(|e| format!("encoder spawn: {e}"))?;
    // Proxy is video-only — audio passes through untouched at use
    // time (Phase 9.5 toggle decides which file feeds audio).
    encoder
        .open(
            job.output_path.clone(),
            Some(VideoConfig {
                width,
                height,
                frame_rate_num: stream.frame_rate.0 as u32,
                frame_rate_den: stream.frame_rate.1 as u32,
                pixel_format,
                preset,
                time_base_num: stream.frame_rate.1 as u32,
                time_base_den: stream.frame_rate.0 as u32,
                // SDR proxies are tagged BT.709 by the encoder itself.
                hdr,
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
    let mut scaler = Downscaler::new(
        (stream.width as usize, stream.height as usize),
        (width as usize, height as usize),
    );
    // Decode at next_pts. If we ran past the duration_pts, the
    // decoder will return Err — treat that as natural EOS.
    while let Ok(frame) = decoder.decode_at(next_pts) {
        // CPU conversion: a background job, correctness over throughput;
        // rows convert in parallel into the one reused output buffer.
        let small = scaler.run(&frame, input, alpha)?;
        let bytes = if deep {
            pack_rgba64le(small)
        } else {
            pack_rgba8(small)
        };
        encoder
            .push_video(frame_index as i64, bytes)
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
    mark_current(
        Path::new(&job.output_path),
        ProxySource {
            codec: ProxyCodec::of_preset(&job.preset),
            size: (stream.width, stream.height),
            sar: stream.sample_aspect,
            input_space: input,
        },
    )
    .map_err(|e| format!("couldn't mark the proxy current: {e}"))?;
    Ok(frame_index)
}

/// The encoder's HDR option for a source in `transfer`: PQ / HLG BT.2020
/// signalling, which a ProRes preset keeps as ProRes (MOV ProRes carries
/// no mastering display or light levels; those are for HEVC exports);
/// `None` for SDR.
fn hdr_signaling(transfer: vance_color::TransferFunction) -> Option<vance_encoder::HdrMetadata> {
    use vance_encoder::protocol::{
        ColorPrimaries, MatrixCoefficients, SignalRange, TransferCharacteristics,
    };
    let transfer = match transfer {
        vance_color::TransferFunction::Pq => TransferCharacteristics::Pq,
        vance_color::TransferFunction::Hlg => TransferCharacteristics::Hlg,
        _ => return None,
    };
    Some(vance_encoder::HdrMetadata {
        signaling: vance_encoder::ColorSignaling {
            primaries: ColorPrimaries::Bt2020,
            transfer,
            matrix: MatrixCoefficients::Bt2020Ncl,
            range: SignalRange::Limited,
        },
        mastering: vance_encoder::MasteringDisplay::rec2020(1000.0, 0.0001),
        max_cll: 1000,
        max_fall: 400,
    })
}

fn set_status(status: &Arc<Mutex<Vec<ProxyJobSnapshot>>>, id: &str, new_status: ProxyJobStatus) {
    let mut s = status.lock();
    if let Some(slot) = s.iter_mut().find(|j| j.id == id) {
        slot.status = new_status;
    }
}

/// The proxy size of a `width × height` source with sample aspect `sar`:
/// half its display size (square pixels), rounded down to even.
pub fn proxy_size(width: u32, height: u32, sar: (u32, u32)) -> (u32, u32) {
    let display_w = match sar {
        (n, d) if n > 0 && d > 0 => f64::from(width) * f64::from(n) / f64::from(d),
        _ => f64::from(width),
    };
    let even = |v: f64| ((v / 2.0).round() as u32 & !1).max(2);
    (even(display_w), even(f64::from(height)))
}

/// How one frame's YUV codes map to R'G'B' in [0, 1].
#[derive(Clone, Copy, Debug, PartialEq)]
struct YuvDecode {
    y_off: f32,
    y_scale: f32,
    c_off: f32,
    c_scale: f32,
    kr: f32,
    kb: f32,
}

impl YuvDecode {
    /// From the frame's signalled range and matrix, with the renderer's
    /// rule for what it doesn't signal (`compose::frame_coding`): the range
    /// of the media's `input` space (PQ / HLG: limited); BT.601 up to 576
    /// lines (the original's: proxies are made from it), else the matrix of
    /// the input primaries.
    fn for_frame(frame: &FrameData, input: vance_color::ColorSpace) -> Self {
        use vance_color::ColorSpace as Cs;
        let deep = frame.pixel_format == PixelFormat::Yuv420P10Le;
        let s = if deep { 4.0 } else { 1.0 };
        let max = if deep { 1023.0 } else { 255.0 };
        let (primaries, _, input_range) = input.decompose();
        let full = match frame.color.range {
            ColorRangeHint::Full => true,
            ColorRangeHint::Limited => false,
            ColorRangeHint::Unknown => match input {
                Cs::Rec2020Pq | Cs::Rec2020Hlg => false,
                _ => input_range == vance_color::Range::Full,
            },
        };
        let (y_off, y_scale, c_scale) = if full {
            (0.0, max, max)
        } else {
            (16.0 * s, 219.0 * s, 224.0 * s)
        };
        let matrix = match frame.color.matrix {
            ColorMatrixHint::Unknown if frame.height <= 576 => ColorMatrixHint::Bt601,
            ColorMatrixHint::Unknown if primaries == vance_color::Primaries::Bt2020 => {
                ColorMatrixHint::Bt2020Ncl
            }
            ColorMatrixHint::Unknown => ColorMatrixHint::Bt709,
            m => m,
        };
        let (kr, kb) = match matrix {
            ColorMatrixHint::Bt601 => (0.299, 0.114),
            ColorMatrixHint::Bt2020Ncl => (0.2627, 0.0593),
            ColorMatrixHint::Bt709 | ColorMatrixHint::Unknown => (0.2126, 0.0722),
        };
        Self {
            y_off,
            y_scale,
            c_off: 128.0 * s,
            c_scale,
            kr,
            kb,
        }
    }

    fn rgb(&self, y: f32, u: f32, v: f32) -> [f32; 3] {
        let y = (y - self.y_off) / self.y_scale;
        let cb = (u - self.c_off) / self.c_scale;
        let cr = (v - self.c_off) / self.c_scale;
        let r = y + 2.0 * (1.0 - self.kr) * cr;
        let b = y + 2.0 * (1.0 - self.kb) * cb;
        let g = (y - self.kr * r - self.kb * b) / (1.0 - self.kr - self.kb);
        [r, g, b].map(|c| c.clamp(0.0, 1.0))
    }
}

/// A decoded frame's planes, shareable across the row threads.
#[derive(Clone, Copy)]
struct Planes<'a> {
    bytes: &'a [u8],
    format: PixelFormat,
    strides: [usize; 3],
    size: (usize, usize),
}

impl<'a> Planes<'a> {
    fn of(frame: &'a FrameData) -> Result<Self, String> {
        let (w, h) = (frame.width as usize, frame.height as usize);
        let bytes: &[u8] = &frame.bytes;
        if bytes.len() < frame.pixel_format.frame_size(frame.width, frame.height) {
            return Err(format!(
                "frame too short for {w}x{h} {:?}",
                frame.pixel_format
            ));
        }
        let stride = |i: usize| frame.strides.get(i).copied().unwrap_or(0) as usize;
        Ok(Self {
            bytes,
            format: frame.pixel_format,
            strides: [stride(0), stride(1), stride(2)],
            size: (w, h),
        })
    }

    /// Source row `y` as straight-alpha R'G'B'A floats in [0, 1] (4:2:0
    /// chroma nearest-neighbour: the proxy halves the size anyway),
    /// premultiplied when `premultiply`.
    fn row(&self, dec: &YuvDecode, y: usize, out: &mut [f32], premultiply: bool) {
        let (w, h) = self.size;
        let [sy, s1, s2] = self.strides;
        // 4:2:0 chroma has ceil(h/2) rows: an odd height's last row has one.
        let ch = h.div_ceil(2);
        let b = self.bytes;
        let mut put = |x: usize, [r, g, bl]: [f32; 3]| {
            out[x * 4..x * 4 + 4].copy_from_slice(&[r, g, bl, 1.0]);
        };
        match self.format {
            PixelFormat::Yuv420P => {
                let (u, v) = (sy * h + (y / 2) * s1, sy * h + s1 * ch + (y / 2) * s2);
                for x in 0..w {
                    let luma = f32::from(b[y * sy + x]);
                    put(
                        x,
                        dec.rgb(luma, f32::from(b[u + x / 2]), f32::from(b[v + x / 2])),
                    );
                }
            }
            PixelFormat::Nv12 => {
                let uv = sy * h + (y / 2) * s1;
                for x in 0..w {
                    let i = uv + (x / 2) * 2;
                    let luma = f32::from(b[y * sy + x]);
                    put(x, dec.rgb(luma, f32::from(b[i]), f32::from(b[i + 1])));
                }
            }
            PixelFormat::Yuv420P10Le => {
                // Strides are in samples; samples are little-endian u16.
                let at = |i: usize| f32::from(u16::from_le_bytes([b[i * 2], b[i * 2 + 1]]));
                let (u, v) = (sy * h + (y / 2) * s1, sy * h + s1 * ch + (y / 2) * s2);
                for x in 0..w {
                    put(x, dec.rgb(at(y * sy + x), at(u + x / 2), at(v + x / 2)));
                }
            }
            PixelFormat::Rgba8 => {
                let row = &b[y * sy..y * sy + w * 4];
                for (o, &c) in out.iter_mut().zip(row) {
                    *o = f32::from(c) / 255.0;
                }
            }
        }
        if premultiply {
            for px in out[..w * 4].chunks_exact_mut(4) {
                let a = px[3];
                px[..3].iter_mut().for_each(|c| *c *= a);
            }
        }
    }
}

/// For each of `n` output samples spanning `len` input samples, the inputs
/// it covers and their share of it (summing to 1).
fn area_weights(len: usize, n: usize) -> Vec<Vec<(usize, f32)>> {
    let scale = len as f64 / n as f64;
    (0..n)
        .map(|i| {
            let (a, b) = (i as f64 * scale, (i + 1) as f64 * scale);
            let mut taps = Vec::new();
            let mut j = a.floor() as usize;
            while (j as f64) < b && j < len {
                let cover = b.min(j as f64 + 1.0) - a.max(j as f64);
                if cover > 0.0 {
                    taps.push((j, (cover / scale) as f32));
                }
                j += 1;
            }
            taps
        })
        .collect()
}

/// Area-averages decoded frames to the proxy size, one job's frames after
/// another. The output frame is the only frame-sized buffer: source rows are
/// converted (a row at a time, per thread) as the output rows that cover
/// them are filled.
struct Downscaler {
    src: (usize, usize),
    dst: (usize, usize),
    cols: Vec<Vec<(usize, f32)>>,
    rows: Vec<Vec<(usize, f32)>>,
    /// Straight-alpha RGBA floats of the last frame, `dst` sized.
    out: Vec<f32>,
}

impl Downscaler {
    fn new(src: (usize, usize), dst: (usize, usize)) -> Self {
        Self {
            src,
            dst,
            cols: area_weights(src.0, dst.0),
            rows: area_weights(src.1, dst.1),
            out: vec![0.0; dst.0 * dst.1 * 4],
        }
    }

    /// `frame` at the proxy size as straight-alpha R'G'B'A floats in
    /// [0, 1]. With `alpha`, colours average premultiplied, so transparent
    /// pixels don't bleed their (meaningless) colour into edges.
    fn run(
        &mut self,
        frame: &FrameData,
        input: vance_color::ColorSpace,
        alpha: bool,
    ) -> Result<&[f32], String> {
        let planes = Planes::of(frame)?;
        if planes.size != self.src {
            // A frame that isn't the stream's size (rare): new weights.
            *self = Self::new(planes.size, self.dst);
        }
        let dec = YuvDecode::for_frame(frame, input);
        let (w, (dw, dh)) = (self.src.0, self.dst);
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 8)
            .min(dh.max(1));
        let rows_per = dh.div_ceil(threads).max(1);
        let (cols, rows) = (&self.cols, &self.rows);
        std::thread::scope(|s| {
            for (chunk, out) in self.out.chunks_mut(rows_per * dw * 4).enumerate() {
                let dec = &dec;
                s.spawn(move || {
                    let mut src_row = vec![0.0f32; w * 4];
                    for (k, acc) in out.chunks_exact_mut(dw * 4).enumerate() {
                        acc.fill(0.0);
                        for &(sy, wy) in &rows[chunk * rows_per + k] {
                            planes.row(dec, sy, &mut src_row, alpha);
                            for (ox, taps) in cols.iter().enumerate() {
                                let px = &mut acc[ox * 4..ox * 4 + 4];
                                for &(sx, wx) in taps {
                                    let wt = wx * wy;
                                    for c in 0..4 {
                                        px[c] += src_row[sx * 4 + c] * wt;
                                    }
                                }
                            }
                        }
                        if alpha {
                            for px in acc.chunks_exact_mut(4) {
                                let a = px[3];
                                if a > 0.0 {
                                    px[..3].iter_mut().for_each(|c| *c = (*c / a).min(1.0));
                                }
                            }
                        }
                    }
                });
            }
        });
        Ok(&self.out)
    }
}

fn pack_rgba8(px: &[f32]) -> Vec<u8> {
    px.iter()
        .map(|&v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
        .collect()
}

fn pack_rgba64le(px: &[f32]) -> Vec<u8> {
    px.iter()
        .flat_map(|&v| ((v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16).to_le_bytes())
        .collect()
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

    fn frame(
        pixel_format: PixelFormat,
        (w, h): (u32, u32),
        strides: Vec<u32>,
        bytes: Vec<u8>,
        range: ColorRangeHint,
        matrix: ColorMatrixHint,
    ) -> FrameData {
        FrameData {
            pts: 0,
            width: w,
            height: h,
            pixel_format,
            strides,
            bytes: bytes.into(),
            color: vance_decoder::FrameColor {
                range,
                matrix,
                sample_aspect: (1, 1),
            },
        }
    }

    /// A 2×2 8-bit I420 frame of one YUV colour.
    fn yuv8(y: u8, u: u8, v: u8, range: ColorRangeHint, matrix: ColorMatrixHint) -> FrameData {
        frame(
            PixelFormat::Yuv420P,
            (2, 2),
            vec![2, 1, 1],
            vec![y, y, y, y, u, v],
            range,
            matrix,
        )
    }

    /// `f` converted at its own size (every output pixel one input pixel).
    fn convert(f: &FrameData) -> Result<Vec<f32>, String> {
        let size = (f.width as usize, f.height as usize);
        let alpha = f.pixel_format.is_rgba();
        Downscaler::new(size, size)
            .run(f, vance_color::ColorSpace::Rec709Limited, alpha)
            .map(<[f32]>::to_vec)
    }

    fn rgba8(size: (u32, u32), px: &[[u8; 4]]) -> FrameData {
        frame(
            PixelFormat::Rgba8,
            size,
            vec![size.0 * 4],
            px.concat(),
            ColorRangeHint::Unknown,
            ColorMatrixHint::Unknown,
        )
    }

    fn rgb_of(f: &FrameData) -> [f32; 3] {
        let px = convert(f).unwrap();
        [px[0], px[1], px[2]]
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 0.004)
    }

    #[test]
    fn limited_range_expands_to_full_black_and_white() {
        use ColorMatrixHint::Unknown as U;
        // Unsignalled range is limited (as the renderer assumes): 16 is
        // black and 235 white, not the washed-out 16/255 and 235/255.
        for range in [ColorRangeHint::Unknown, ColorRangeHint::Limited] {
            assert!(close(rgb_of(&yuv8(16, 128, 128, range, U)), [0.0; 3]));
            assert!(close(rgb_of(&yuv8(235, 128, 128, range, U)), [1.0; 3]));
        }
        assert!(close(
            rgb_of(&yuv8(0, 128, 128, ColorRangeHint::Full, U)),
            [0.0; 3]
        ));
        assert!(close(
            rgb_of(&yuv8(255, 128, 128, ColorRangeHint::Full, U)),
            [1.0; 3]
        ));
    }

    #[test]
    fn the_matrix_follows_the_stream_then_the_renderer_defaults() {
        use vance_color::ColorSpace as Cs;
        let limited = ColorRangeHint::Limited;
        // BT.709 limited code for sRGB-ish red (1, 0, 0): Y 63, Cb 102, Cr 240.
        let red709 = rgb_of(&yuv8(63, 102, 240, limited, ColorMatrixHint::Bt709));
        assert!(close(red709, [1.0, 0.0, 0.0]), "{red709:?}");
        // The same codes through BT.601 are a different colour.
        let as601 = rgb_of(&yuv8(63, 102, 240, limited, ColorMatrixHint::Bt601));
        assert!(
            as601[0] < 0.95,
            "BT.601 reads 709 red differently: {as601:?}"
        );

        // Unknown: BT.601 up to 576 lines, else by primaries.
        let unknown = |h: u32| {
            let mut f = yuv8(63, 102, 240, limited, ColorMatrixHint::Unknown);
            f.height = h;
            f
        };
        let (sd, hd) = (unknown(576), unknown(577));
        assert_eq!(YuvDecode::for_frame(&sd, Cs::Rec709Limited).kr, 0.299);
        assert_eq!(YuvDecode::for_frame(&hd, Cs::Rec709Limited).kr, 0.2126);
        assert_eq!(YuvDecode::for_frame(&hd, Cs::Rec2020Limited).kr, 0.2627);
        // A signalled matrix wins over the size rule.
        let mut tagged = unknown(480);
        tagged.color.matrix = ColorMatrixHint::Bt709;
        assert_eq!(YuvDecode::for_frame(&tagged, Cs::Rec709Limited).kr, 0.2126);
    }

    #[test]
    fn an_unsignalled_range_follows_the_media_tag_as_compose_does() {
        use vance_color::ColorSpace as Cs;
        let decode = |range, space| {
            let f = yuv8(16, 128, 128, range, ColorMatrixHint::Bt709);
            let size = (2, 2);
            let px = Downscaler::new(size, size).run(&f, space, false).unwrap()[0];
            (px * 255.0).round() as u8
        };
        // Unknown range: the media's tag decides ("Rec.709 (full)" → full:
        // code 16 is a dark grey, not black)...
        assert_eq!(decode(ColorRangeHint::Unknown, Cs::Rec709Limited), 0);
        assert_eq!(decode(ColorRangeHint::Unknown, Cs::Rec709Full), 16);
        // ...except PQ / HLG, which are limited.
        assert_eq!(decode(ColorRangeHint::Unknown, Cs::Rec2020Pq), 0);
        assert_eq!(decode(ColorRangeHint::Unknown, Cs::Rec2020Hlg), 0);
        // A signalled range wins over the tag.
        assert_eq!(decode(ColorRangeHint::Limited, Cs::Rec709Full), 0);
        assert_eq!(decode(ColorRangeHint::Full, Cs::Rec709Limited), 16);
        // What the renderer can't read built in decodes as Rec.709 limited.
        assert_eq!(effective_input_space(None), Cs::Rec709Limited);
        assert_eq!(effective_input_space(Some(Cs::Acescct)), Cs::Rec709Limited);
    }

    #[test]
    fn ten_bit_codes_use_ten_bit_levels() {
        let sample = |v: u16| v.to_le_bytes();
        let ten = |y: u16| {
            let bytes: Vec<u8> = [y, y, y, y, 512, 512]
                .iter()
                .flat_map(|&v| sample(v))
                .collect();
            frame(
                PixelFormat::Yuv420P10Le,
                (2, 2),
                vec![2, 1, 1],
                bytes,
                ColorRangeHint::Unknown,
                ColorMatrixHint::Bt709,
            )
        };
        assert!(close(rgb_of(&ten(64)), [0.0; 3]), "10-bit black is 64");
        assert!(close(rgb_of(&ten(940)), [1.0; 3]), "10-bit white is 940");
        let mid = rgb_of(&ten(502));
        assert!(close(mid, [0.5; 3]), "{mid:?}");
    }

    #[test]
    fn proxies_are_half_the_display_size_even_rounded() {
        assert_eq!(proxy_size(1920, 1080, (1, 1)), (960, 540));
        assert_eq!(proxy_size(3840, 2160, (1, 1)), (1920, 1080));
        // Anamorphic: 1440×1080 at 4:3 displays 1920 wide.
        assert_eq!(proxy_size(1440, 1080, (4, 3)), (960, 540));
        // PAL widescreen 720×576 at 64:45 → 1024 wide.
        assert_eq!(proxy_size(720, 576, (64, 45)), (512, 288));
        // Odd halves round down to even; unknown ratios are square.
        assert_eq!(proxy_size(1366, 770, (0, 1)), (682, 384));
        assert_eq!(proxy_size(2, 2, (1, 1)), (2, 2));
    }

    #[test]
    fn resampling_averages_areas_and_keeps_transparent_colour_out() {
        let scale = |f: &FrameData, dst: (usize, usize), alpha: bool| -> Vec<f32> {
            let src = (f.width as usize, f.height as usize);
            Downscaler::new(src, dst)
                .run(f, vance_color::ColorSpace::Rec709Limited, alpha)
                .unwrap()
                .to_vec()
        };
        // 2×2 → 1×1: the mean of four opaque pixels.
        let quad = rgba8(
            (2, 2),
            &[
                [0, 0, 0, 255],
                [255, 255, 255, 255],
                [51, 51, 51, 255],
                [204, 204, 204, 255],
            ],
        );
        let one = scale(&quad, (1, 1), false);
        assert!(
            close([one[0], one[1], one[2]], [0.5; 3]) && one[3] == 1.0,
            "{one:?}"
        );
        // A transparent pixel's colour doesn't tint the result.
        let edge = rgba8((2, 1), &[[255, 0, 0, 255], [0, 255, 0, 0]]);
        let px = scale(&edge, (1, 1), true);
        assert!(close([px[0], px[1], px[2]], [1.0, 0.0, 0.0]), "{px:?}");
        assert!((px[3] - 0.5).abs() < 1e-6);
        // Anamorphic stretch: 2 → 3 columns covers each input proportionally.
        let row = rgba8((2, 1), &[[0, 0, 0, 255], [255, 255, 255, 255]]);
        let wide = scale(&row, (3, 1), false);
        assert!(
            close([wide[0], wide[4], wide[8]], [0.0, 0.5, 1.0]),
            "{wide:?}"
        );
        // Many rows and output rows (the per-thread chunks) match one pass.
        let big: Vec<[u8; 4]> = (0..64 * 48)
            .map(|i| [(i % 251) as u8, (i % 97) as u8, (i % 13) as u8, 255])
            .collect();
        let big = rgba8((64, 48), &big);
        let small = scale(&big, (32, 24), false);
        for (oy, ox) in [(0, 0), (11, 7), (23, 31)] {
            let src =
                |x: usize, y: usize, c: usize| f32::from(big.bytes[(y * 64 + x) * 4 + c]) / 255.0;
            for c in 0..3 {
                let want = (src(2 * ox, 2 * oy, c)
                    + src(2 * ox + 1, 2 * oy, c)
                    + src(2 * ox, 2 * oy + 1, c)
                    + src(2 * ox + 1, 2 * oy + 1, c))
                    / 4.0;
                let got = small[(oy * 32 + ox) * 4 + c];
                assert!(
                    (got - want).abs() < 1e-5,
                    "({ox},{oy}) c{c}: {got} vs {want}"
                );
            }
        }
    }

    #[test]
    fn rgba_sources_keep_their_alpha() {
        let f = frame(
            PixelFormat::Rgba8,
            (1, 1),
            vec![4],
            vec![255, 128, 0, 64],
            ColorRangeHint::Unknown,
            ColorMatrixHint::Unknown,
        );
        let px = convert(&f).unwrap();
        assert_eq!(pack_rgba8(&px), [255, 128, 0, 64]);
        assert_eq!(&pack_rgba64le(&[1.0])[..], &65535u16.to_le_bytes());
    }

    #[test]
    fn odd_dimension_frames_convert_in_every_format() {
        // 4:2:0 chroma is ceil(w/2) x ceil(h/2): the last (odd) row and column
        // must read the final chroma sample, not run past the plane.
        let (w, h) = (5u32, 3u32);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let grey = |n: u32, bytes_per_sample: usize| -> Vec<u8> {
            if bytes_per_sample == 2 {
                std::iter::repeat_n((128u16 << 2).to_le_bytes(), n as usize)
                    .flatten()
                    .collect()
            } else {
                vec![128u8; n as usize]
            }
        };
        let frames = [
            (
                PixelFormat::Yuv420P,
                vec![w, cw, cw],
                grey(w * h + 2 * cw * ch, 1),
            ),
            (
                PixelFormat::Nv12,
                vec![w, cw * 2],
                grey(w * h + 2 * cw * ch, 1),
            ),
            (
                PixelFormat::Yuv420P10Le,
                vec![w, cw, cw],
                grey(w * h + 2 * cw * ch, 2),
            ),
        ];
        // Limited-range code 128 is (128 − 16) / 219 of white.
        let want = (112.0f32 / 219.0 * 255.0).round() as u8;
        for (pixel_format, strides, bytes) in frames {
            let f = frame(
                pixel_format,
                (w, h),
                strides,
                bytes,
                ColorRangeHint::Unknown,
                ColorMatrixHint::Unknown,
            );
            let out = pack_rgba8(&convert(&f).unwrap());
            assert!(
                out.chunks_exact(4).all(|px| px == [want, want, want, 255]),
                "{pixel_format:?}: odd frame converted wrong: {out:?}"
            );
        }
    }

    #[test]
    fn only_a_marked_unchanged_proxy_is_current() {
        let dir = tempfile::tempdir().unwrap();
        let proxy = dir.path().join("clip.proxy.mov");
        assert!(current_marker(&proxy).is_none(), "no file");
        std::fs::write(&proxy, b"proxy bytes").unwrap();
        assert!(
            current_marker(&proxy).is_none(),
            "an older build's proxy has no marker"
        );
        let source = ProxySource {
            codec: ProxyCodec::DnxhrLb,
            size: (1440, 1080),
            sar: (4, 3),
            input_space: vance_color::ColorSpace::Rec709Full,
        };
        mark_current(&proxy, source).unwrap();
        assert!(current_marker(&proxy).is_some());
        let marker = current_marker(&proxy).unwrap();
        assert_eq!(
            marker.input_space,
            Some(vance_color::ColorSpace::Rec709Full)
        );
        assert!(matches_media(
            &proxy,
            Some(vance_color::ColorSpace::Rec709Full)
        ));
        assert!(
            !matches_media(&proxy, None),
            "re-tagged media (untagged = Rec.709 limited) needs a new proxy"
        );
        assert_eq!(
            marker.codec,
            ProxyCodec::DnxhrLb,
            "remade with the same codec"
        );
        assert_eq!(marker.original_size, Some((1440, 1080)));
        assert_eq!(marker.original_sar, (4, 3));
        std::fs::write(&proxy, b"another proxy").unwrap();
        assert!(
            current_marker(&proxy).is_none(),
            "replaced after it was marked"
        );
        // A version 2 marker (no codec or size) still reads, but isn't current.
        let old = format!("vance-proxy-format={} bytes=13\n", PROXY_FORMAT_VERSION - 1);
        std::fs::write(marker_path(&proxy), old).unwrap();
        assert!(current_marker(&proxy).is_none(), "an older format's marker");
        let old = read_marker(&proxy).unwrap();
        assert_eq!(
            (old.codec, old.original_size),
            (ProxyCodec::ProRes422, None)
        );
        std::fs::write(marker_path(&proxy), "garbage").unwrap();
        assert_eq!(read_marker(&proxy), None);
    }

    #[test]
    fn hdr_sources_get_hdr_signalling() {
        use vance_color::TransferFunction as T;
        use vance_encoder::protocol::TransferCharacteristics as Tc;
        assert_eq!(hdr_signaling(T::Rec709), None);
        assert_eq!(hdr_signaling(T::Srgb), None);
        let pq = hdr_signaling(T::Pq).unwrap().signaling;
        assert_eq!(pq.transfer, Tc::Pq);
        assert_eq!(
            pq.primaries,
            vance_encoder::protocol::ColorPrimaries::Bt2020
        );
        assert_eq!(hdr_signaling(T::Hlg).unwrap().signaling.transfer, Tc::Hlg);
    }

    #[test]
    fn a_short_frame_is_an_error_not_a_panic() {
        let f = frame(
            PixelFormat::Yuv420P,
            (4, 4),
            vec![4, 2, 2],
            vec![0; 5],
            ColorRangeHint::Unknown,
            ColorMatrixHint::Unknown,
        );
        assert!(convert(&f).is_err());
    }
}
