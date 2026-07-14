//! `vance-decoder` sidecar binary.
//!
//! Wraps FFmpeg behind the protocol in `vance_decoder::protocol`. Software
//! decode only in Phase 2.1; hardware decode (VideoToolbox / NVDEC /
//! VAAPI / D3D11VA) lands in Phase 2.2.
//!
//! ## Threading
//!
//! - The main thread runs `vance_ipc::serve()` and processes one request
//!   at a time.
//! - During streaming, a background **decode thread** owns the FFmpeg
//!   `Input` / `Video` decoder and pushes decoded frames into a bounded
//!   channel. The main thread `recv()`s from that channel when a
//!   `NextFrame` request arrives.
//! - `StopStream` (or `Close` / `Shutdown` during a stream) signals the
//!   decode thread, joins it, and reclaims the `OpenContext` for further
//!   single-shot `Seek` / `DecodeAt` operations.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use vance_decoder::hwaccel::{self, HwDeviceCtx};
use vance_decoder::protocol::{
    DecoderRequest, DecoderResponse, FrameData, OpenError, PixelFormat, StreamInfo,
    PROTOCOL_VERSION,
};
use vance_ffmpeg_sys::ffmpeg;

fn main() -> vance_ipc::Result<()> {
    vance_ffmpeg_sys::init_once();
    let mut state = State::default();
    vance_ipc::serve::<DecoderRequest, DecoderResponse, _>(|req| state.handle(req))
}

#[derive(Default)]
struct State {
    /// Owned exclusively by the main thread while no stream is active.
    /// Moved INTO the decode thread for the duration of a stream, then
    /// reclaimed via the join handle when the stream stops.
    open: Option<OpenContext>,
    /// Set whenever a stream is in progress.
    streaming: Option<StreamingState>,
}

struct OpenContext {
    input: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    stream_index: usize,
    info: StreamInfo,
    /// Holds the VideoToolbox device context (if any) for the lifetime
    /// of this open file. `None` means software decode.
    _hw_device: Option<HwDeviceCtx>,
    /// Reusable destination frame for `av_hwframe_transfer_data`.
    /// `None` when not using HW decode.
    sw_dest_frame: Option<ffmpeg::frame::Video>,
}

/// Decode-thread bookkeeping held by the main thread for the lifetime of a
/// stream. The `JoinHandle` returns the `OpenContext` so we can resume
/// single-shot decoding after the stream stops.
struct StreamingState {
    /// Receives frames in PTS order. `None` = end-of-stream sentinel.
    rx: Receiver<Option<DecodedItem>>,
    /// Tells the decode thread to exit at its next chance.
    stop: Arc<AtomicBool>,
    /// Joins to recover the OpenContext (or an error string).
    handle: JoinHandle<Result<OpenContext, String>>,
}

enum DecodedItem {
    Frame(FrameData),
    /// Decoder error mid-stream; thread will exit after sending this.
    Failure(String),
}

impl State {
    fn handle(&mut self, req: DecoderRequest) -> DecoderResponse {
        match req {
            DecoderRequest::Hello {
                protocol_version: _,
            } => DecoderResponse::Hello {
                protocol_version: PROTOCOL_VERSION,
                ffmpeg_version: ffmpeg_version_string(),
            },
            DecoderRequest::Open { path } => {
                self.stop_stream_if_any();
                match self.open(&path) {
                    Ok(info) => DecoderResponse::Opened(info),
                    Err(e) => DecoderResponse::OpenFailed(e),
                }
            }
            DecoderRequest::Seek { target_pts } => {
                self.stop_stream_if_any();
                match self.seek(target_pts) {
                    Ok(pts) => DecoderResponse::Seeked { pts },
                    Err(e) => DecoderResponse::DecodeFailed { reason: e },
                }
            }
            DecoderRequest::DecodeAt { target_pts } => {
                self.stop_stream_if_any();
                match self.decode_at(target_pts) {
                    Ok(f) => DecoderResponse::Frame(f),
                    Err(e) => DecoderResponse::DecodeFailed { reason: e },
                }
            }
            DecoderRequest::StartStream {
                from_pts,
                buffer_frames,
            } => {
                self.stop_stream_if_any();
                match self.start_stream(from_pts, buffer_frames) {
                    Ok(first_pts) => DecoderResponse::Streaming { first_pts },
                    Err(e) => DecoderResponse::DecodeFailed { reason: e },
                }
            }
            DecoderRequest::NextFrame => self.next_frame(),
            DecoderRequest::StopStream => {
                self.stop_stream_if_any();
                DecoderResponse::Stopped
            }
            DecoderRequest::Close => {
                self.stop_stream_if_any();
                self.open = None;
                DecoderResponse::Closed
            }
            DecoderRequest::Shutdown => {
                self.stop_stream_if_any();
                self.open = None;
                DecoderResponse::Goodbye
            }
        }
    }

    // ---- one-shot path (Phase 1 retained) ---------------------------------

    fn open(&mut self, path: &str) -> Result<StreamInfo, OpenError> {
        let p = Path::new(path);
        if !p.exists() {
            return Err(OpenError::NotFound(path.to_string()));
        }

        let input = ffmpeg::format::input(p).map_err(|e| OpenError::FfmpegError(e.to_string()))?;

        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .ok_or(OpenError::NoVideoStream)?;
        let stream_index = stream.index();
        let time_base = stream.time_base();
        let avg_rate = stream.avg_frame_rate();
        let duration = stream.duration();
        let codec_id = stream.parameters().id();
        let codec_name = format!("{codec_id:?}").to_lowercase();

        let mut codec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| OpenError::FfmpegError(e.to_string()))?;

        // Detect the source bit depth from the stream's declared pixel
        // format BEFORE choosing a decode path. A >8-bit source forces
        // software decode (the HW/NV12 path can't carry 10-bit correctly).
        let src_pix_fmt: i32 = unsafe { (*stream.parameters().as_ptr()).format };
        let high_bit_source = is_high_bit_source(src_pix_fmt);

        // Try VideoToolbox before opening the decoder (8-bit only). On
        // success, attach hw_device_ctx so FFmpeg's get_format chooses
        // AV_PIX_FMT_VIDEOTOOLBOX and decoded frames come back as opaque GPU
        // surfaces (which we then transfer to NV12 system memory).
        let hw_device = if high_bit_source {
            None
        } else {
            HwDeviceCtx::try_videotoolbox()
        };
        let using_hw = if let Some(ref hw) = hw_device {
            unsafe {
                hw.attach_to(codec_ctx.as_mut_ptr() as *mut std::ffi::c_void);
            }
            true
        } else {
            false
        };

        let decoder = codec_ctx
            .decoder()
            .video()
            .map_err(|e| OpenError::UnsupportedCodec(format!("{codec_name}: {e}")))?;

        // If HW decode is in play, decoded frames are AV_PIX_FMT_VIDEOTOOLBOX
        // and we transfer them to NV12. Report NV12 in StreamInfo so the
        // renderer picks the right path. Otherwise, report whatever the
        // SW decoder outputs.
        let pixel_format = if using_hw {
            PixelFormat::Nv12
        } else {
            map_pixel_format(decoder.format()).map_err(OpenError::UnsupportedPixelFormat)?
        };
        let sw_dest_frame = if using_hw {
            Some(ffmpeg::frame::Video::empty())
        } else {
            None
        };

        eprintln!(
            "[decoder] open ok: {}x{} {} ({})",
            decoder.width(),
            decoder.height(),
            codec_name,
            if using_hw {
                "VideoToolbox HW"
            } else {
                "software"
            }
        );

        let info = StreamInfo {
            width: decoder.width(),
            height: decoder.height(),
            pixel_format,
            frame_rate: (avg_rate.numerator() as i64, avg_rate.denominator() as i64),
            time_base: (time_base.numerator() as i64, time_base.denominator() as i64),
            duration_pts: if duration > 0 { Some(duration) } else { None },
            codec_name,
            stream_index: stream_index as u32,
        };

        self.open = Some(OpenContext {
            input,
            decoder,
            stream_index,
            info: info.clone(),
            _hw_device: hw_device,
            sw_dest_frame,
        });
        Ok(info)
    }

    fn seek(&mut self, target_pts: i64) -> Result<i64, String> {
        let ctx = self.open.as_mut().ok_or("no file open")?;
        seek_ctx(ctx, target_pts)?;
        Ok(target_pts)
    }

    fn decode_at(&mut self, target_pts: i64) -> Result<FrameData, String> {
        let ctx = self.open.as_mut().ok_or("no file open")?;
        seek_ctx(ctx, target_pts)?;
        decode_one_at_or_after(ctx, target_pts)
    }

    // ---- streaming path (Phase 2.1) ---------------------------------------

    fn start_stream(&mut self, from_pts: i64, buffer_frames: u32) -> Result<i64, String> {
        // Take ownership of the open context and move it into the decode thread.
        let mut ctx = self.open.take().ok_or("no file open")?;
        seek_ctx(&mut ctx, from_pts)?;

        let cap = (buffer_frames.max(1) as usize).min(64);
        let (tx, rx) = sync_channel::<Option<DecodedItem>>(cap);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_clone = stop.clone();

        let handle = thread::Builder::new()
            .name("vance-decoder.stream".into())
            .spawn(move || decode_stream_loop(ctx, tx, stop_clone, from_pts))
            .map_err(|e| format!("spawn decode thread: {e}"))?;

        self.streaming = Some(StreamingState { rx, stop, handle });
        // We return the requested `from_pts` as the first PTS upper-bound;
        // the actual first frame's PTS will arrive on the channel and the
        // caller will see it via `NextFrame`.
        Ok(from_pts)
    }

    fn next_frame(&mut self) -> DecoderResponse {
        let Some(s) = self.streaming.as_ref() else {
            return DecoderResponse::DecodeFailed {
                reason: "no stream in progress".into(),
            };
        };
        // Long-ish timeout so wedged decoders surface as errors rather than
        // hanging the parent forever. Real playback should always have a
        // frame within ~1s; if not, something is wrong.
        match s.rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Some(DecodedItem::Frame(f))) => DecoderResponse::Frame(f),
            Ok(Some(DecodedItem::Failure(reason))) => {
                self.stop_stream_if_any();
                DecoderResponse::DecodeFailed { reason }
            }
            Ok(None) => {
                self.stop_stream_if_any();
                DecoderResponse::EndOfStream
            }
            Err(RecvTimeoutError::Timeout) => DecoderResponse::DecodeFailed {
                reason: "decode thread starved (5s timeout)".into(),
            },
            Err(RecvTimeoutError::Disconnected) => {
                self.stop_stream_if_any();
                DecoderResponse::DecodeFailed {
                    reason: "decode thread exited".into(),
                }
            }
        }
    }

    fn stop_stream_if_any(&mut self) {
        if let Some(s) = self.streaming.take() {
            s.stop.store(true, Ordering::Relaxed);
            // Drain the channel so the producer can unblock and exit.
            while s.rx.try_recv().is_ok() {}
            match s.handle.join() {
                Ok(Ok(ctx)) => {
                    self.open = Some(ctx);
                }
                Ok(Err(e)) => {
                    eprintln!("[stream] decode thread returned error: {e}");
                }
                Err(_) => {
                    eprintln!("[stream] decode thread panicked");
                }
            }
        }
    }
}

// ---- shared decode helpers (used by both one-shot and streaming) -----------

fn seek_ctx(ctx: &mut OpenContext, target_pts: i64) -> Result<(), String> {
    let (tb_num, tb_den) = ctx.info.time_base;
    let target_us = if tb_den != 0 {
        target_pts.saturating_mul(tb_num).saturating_mul(1_000_000) / tb_den
    } else {
        target_pts
    };
    ctx.input
        .seek(target_us, i64::MIN..=target_us)
        .map_err(|e| format!("seek: {e}"))?;
    ctx.decoder.flush();
    Ok(())
}

/// Drain packets + decoder until we get a frame at or after `target_pts`.
/// Used by one-shot `decode_at` (NOT by the streaming loop, which decodes
/// every frame it produces).
fn decode_one_at_or_after(ctx: &mut OpenContext, target_pts: i64) -> Result<FrameData, String> {
    let mut decoded = ffmpeg::frame::Video::empty();
    let stream_index = ctx.stream_index;
    loop {
        let mut got_packet = false;
        for item in ctx.input.packets() {
            let (stream, packet) = item.map_err(|e| format!("read packet: {e}"))?;
            if stream.index() != stream_index {
                continue;
            }
            got_packet = true;
            ctx.decoder
                .send_packet(&packet)
                .map_err(|e| format!("send_packet: {e}"))?;
            while ctx.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(0);
                if pts >= target_pts {
                    return materialize_frame(
                        &decoded,
                        ctx.sw_dest_frame.as_mut(),
                        ctx.info.pixel_format,
                    );
                }
            }
            break;
        }
        if !got_packet {
            ctx.decoder
                .send_eof()
                .map_err(|e| format!("send_eof: {e}"))?;
            while ctx.decoder.receive_frame(&mut decoded).is_ok() {
                let pts = decoded.pts().unwrap_or(0);
                if pts >= target_pts {
                    return materialize_frame(
                        &decoded,
                        ctx.sw_dest_frame.as_mut(),
                        ctx.info.pixel_format,
                    );
                }
            }
            return Err(format!("no frame at or after pts {target_pts}"));
        }
    }
}

/// Convert a decoded `Video` frame into wire `FrameData`, transferring
/// hardware surfaces to NV12 system memory if needed.
///
/// Takes split borrows (sw_dest_frame, pixel_format) instead of `&mut ctx`
/// so callers can hold a mutable borrow on `ctx.input` (the packets
/// iterator) at the same time.
fn materialize_frame(
    decoded: &ffmpeg::frame::Video,
    sw_dest_frame: Option<&mut ffmpeg::frame::Video>,
    pixel_format: PixelFormat,
) -> Result<FrameData, String> {
    let format_i32 = unsafe { (*decoded.as_ptr()).format };
    if hwaccel::is_hardware_pixel_format(format_i32) {
        let dest = sw_dest_frame.ok_or("got HW frame but no sw_dest_frame allocated")?;
        unsafe {
            hwaccel::transfer_to_sw(
                decoded.as_ptr() as *const std::ffi::c_void,
                dest.as_mut_ptr() as *mut std::ffi::c_void,
            )?;
            let pts = (*decoded.as_ptr()).pts;
            (*dest.as_mut_ptr()).pts = pts;
        }
        frame_to_payload(dest, pixel_format)
    } else {
        frame_to_payload(decoded, pixel_format)
    }
}

/// Background decode loop. Reads packets, sends decoded frames to `tx`.
/// Sends `None` on clean EOS, `Some(Failure(...))` on decode error.
/// Returns the `OpenContext` so the main thread can reclaim it.
fn decode_stream_loop(
    mut ctx: OpenContext,
    tx: SyncSender<Option<DecodedItem>>,
    stop: Arc<AtomicBool>,
    from_pts: i64,
) -> Result<OpenContext, String> {
    let mut decoded = ffmpeg::frame::Video::empty();
    let stream_index = ctx.stream_index;

    'outer: loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let mut got_packet = false;
        for item in ctx.input.packets() {
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            let (stream, packet) = match item {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(Some(DecodedItem::Failure(format!("read packet: {e}"))));
                    return Ok(ctx);
                }
            };
            if stream.index() != stream_index {
                continue;
            }
            got_packet = true;
            if let Err(e) = ctx.decoder.send_packet(&packet) {
                let _ = tx.send(Some(DecodedItem::Failure(format!("send_packet: {e}"))));
                return Ok(ctx);
            }
            while ctx.decoder.receive_frame(&mut decoded).is_ok() {
                if stop.load(Ordering::Relaxed) {
                    break 'outer;
                }
                let pts = decoded.pts().unwrap_or(0);
                // Skip frames before from_pts (typical when seek snapped to
                // an earlier keyframe).
                if pts < from_pts {
                    continue;
                }
                let payload = match materialize_frame(
                    &decoded,
                    ctx.sw_dest_frame.as_mut(),
                    ctx.info.pixel_format,
                ) {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx.send(Some(DecodedItem::Failure(e)));
                        return Ok(ctx);
                    }
                };
                if tx.send(Some(DecodedItem::Frame(payload))).is_err() {
                    // Receiver dropped → main thread told us to stop.
                    break 'outer;
                }
            }
            // Check stop & continue with next packet.
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
        }

        if !got_packet {
            // EOF in the demuxer; flush the decoder.
            if let Err(e) = ctx.decoder.send_eof() {
                let _ = tx.send(Some(DecodedItem::Failure(format!("send_eof: {e}"))));
                return Ok(ctx);
            }
            while ctx.decoder.receive_frame(&mut decoded).is_ok() {
                if stop.load(Ordering::Relaxed) {
                    break 'outer;
                }
                let pts = decoded.pts().unwrap_or(0);
                if pts < from_pts {
                    continue;
                }
                match materialize_frame(&decoded, ctx.sw_dest_frame.as_mut(), ctx.info.pixel_format)
                {
                    Ok(p) => {
                        if tx.send(Some(DecodedItem::Frame(p))).is_err() {
                            break 'outer;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Some(DecodedItem::Failure(e)));
                        return Ok(ctx);
                    }
                }
            }
            // Clean EOS sentinel.
            let _ = tx.send(None);
            break 'outer;
        }
    }
    Ok(ctx)
}

fn ffmpeg_version_string() -> String {
    use std::ffi::CStr;
    unsafe {
        let p = vance_ffmpeg_sys::ffmpeg::sys::av_version_info();
        if p.is_null() {
            "unknown".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

fn map_pixel_format(pf: ffmpeg::format::Pixel) -> Result<PixelFormat, String> {
    use ffmpeg::format::Pixel as P;
    match pf {
        P::YUV420P | P::YUVJ420P => Ok(PixelFormat::Yuv420P),
        P::NV12 => Ok(PixelFormat::Nv12),
        // 10-bit 4:2:0 (HEVC Main10 / HDR PQ-HLG) → carried 10-bit.
        P::YUV420P10LE => Ok(PixelFormat::Yuv420P10Le),
        other => Err(format!("{other:?}")),
    }
}

/// Whether a source pixel format is >8-bit. We force SOFTWARE decode for
/// these: VideoToolbox returns a P010 surface for a Main10 source but the HW
/// path unconditionally reports/handles NV12, which would corrupt the frame.
/// Software decode yields planar `yuv420p10le` we carry losslessly.
fn is_high_bit_source(src_pix_fmt: i32) -> bool {
    use ffmpeg::sys::AVPixelFormat::*;
    src_pix_fmt == AV_PIX_FMT_YUV420P10LE as i32
        || src_pix_fmt == AV_PIX_FMT_YUV420P10BE as i32
        || src_pix_fmt == AV_PIX_FMT_P010LE as i32
        || src_pix_fmt == AV_PIX_FMT_YUV420P12LE as i32
}

fn frame_to_payload(
    frame: &ffmpeg::frame::Video,
    expected_pf: PixelFormat,
) -> Result<FrameData, String> {
    let pts = frame.pts().unwrap_or(0);
    let width = frame.width();
    let height = frame.height();

    let actual = map_pixel_format(frame.format()).map_err(|s| format!("pixel format: {s}"))?;
    if actual != expected_pf {
        return Err(format!(
            "pixel format changed mid-stream: expected {expected_pf:?}, got {actual:?}"
        ));
    }

    let plane_count = match expected_pf {
        PixelFormat::Yuv420P | PixelFormat::Yuv420P10Le => 3,
        PixelFormat::Nv12 => 2,
    };
    // 1 for 8-bit, 2 for 10-bit (samples are little-endian u16 on both the
    // ffmpeg plane and our wire — copied through verbatim).
    let bytes_per_sample = expected_pf.bytes_per_sample();

    let mut strides = Vec::with_capacity(plane_count);
    let mut bytes = Vec::new();
    for plane in 0..plane_count {
        let stride = frame.stride(plane) as u32;
        strides.push(stride);
        let plane_height = if plane == 0 {
            height
        } else {
            height.div_ceil(2)
        };
        // Width in SAMPLES for this plane.
        let plane_width = if plane == 0 || expected_pf == PixelFormat::Nv12 && plane == 1 {
            width
        } else {
            width.div_ceil(2)
        };
        // Row length in BYTES (samples × bytes-per-sample).
        let row_bytes = plane_width as usize * bytes_per_sample;
        let data = frame.data(plane);
        for row in 0..plane_height {
            let start = (row as usize) * (stride as usize);
            let end = start + row_bytes;
            bytes.extend_from_slice(&data[start..end]);
        }
        // Emitted stride is the tight per-row SAMPLE count (padding stripped).
        strides[plane] = plane_width;
    }

    Ok(FrameData {
        pts,
        width,
        height,
        pixel_format: expected_pf,
        strides,
        bytes,
    })
}
