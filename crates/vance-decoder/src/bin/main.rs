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
    /// Built on the first decoded frame whose format isn't `info.pixel_format`.
    converter: Option<FrameConverter>,
}

/// Cached swscale pass from a decoded frame's non-wire pixel format (4:2:2,
/// 4:4:4, RGB, 12-bit, ...) to the wire format chosen at open. Rebuilt only
/// when the source format or size changes.
struct FrameConverter {
    sws: SwsContext,
    /// (format, width, height) of the source frames `sws` was built for.
    src: (ffmpeg::format::Pixel, u32, u32),
    /// Reused destination frame, allocated once in the wire format.
    out: ffmpeg::frame::Video,
}

// SAFETY: the SwsContext is owned exclusively by this converter, which lives
// in an `OpenContext` and is only used by the one thread that currently owns
// that context (the main thread, or the decode thread during a stream).
// swscale's own slice-worker threads only run inside `sws_scale_frame`.
unsafe impl Send for FrameConverter {}

/// An initialised `SwsContext`, freed on drop. Not ffmpeg-the-third's
/// `scaling::Context`: that one can't set the `threads` option before init,
/// and its `run()` calls the legacy `sws_scale()`, which ignores slice
/// threads (only `sws_scale_frame()` uses them).
struct SwsContext(*mut ffmpeg::sys::SwsContext);

impl Drop for SwsContext {
    fn drop(&mut self) {
        // SAFETY: we own the context; sws_freeContext accepts NULL too.
        unsafe { ffmpeg::sys::sws_freeContext(self.0) }
    }
}

/// swscale slice threads per converter: one per core, capped at 8 (each
/// converter keeps its own worker pool). Single-threaded, a 4K 10-bit
/// 4:2:2 -> 4:2:0 pass took ~30 ms on an M1, a whole real-time frame budget;
/// 8 threads brought it to ~9 ms.
fn conversion_threads() -> usize {
    thread::available_parallelism().map_or(1, |n| n.get().min(8))
}

impl FrameConverter {
    fn new(src: (ffmpeg::format::Pixel, u32, u32), target: PixelFormat) -> Result<Self, String> {
        Self::with_threads(src, target, conversion_threads())
    }

    fn with_threads(
        src: (ffmpeg::format::Pixel, u32, u32),
        target: PixelFormat,
        threads: usize,
    ) -> Result<Self, String> {
        use ffmpeg::software::scaling::Flags;
        let (format, width, height) = src;
        let dst = wire_to_ffmpeg(target);
        let fail = |what: &str, code: i32| {
            format!(
                "swscale {format:?} -> {dst:?}: {what}: {}",
                ffmpeg::Error::from(code)
            )
        };
        // FULL_CHR_H_INP: without it, swscale's RGB input path pairs pixels
        // for chroma and, on odd widths, averages the last column with one
        // pixel past the edge (a tinted right-hand column). It has no effect
        // on YUV input.
        let flags = Flags::BILINEAR | Flags::FULL_CHR_H_INP;
        let pix = |p: ffmpeg::format::Pixel| ffmpeg::sys::AVPixelFormat::from(p) as i64;
        // SAFETY: a NULL return is checked below; dropping SwsContext frees it.
        let sws = SwsContext(unsafe { ffmpeg::sys::sws_alloc_context() });
        if sws.0.is_null() {
            return Err(format!("swscale {format:?} -> {dst:?}: out of memory"));
        }
        let options: [(&std::ffi::CStr, i64); 8] = [
            (c"srcw", i64::from(width)),
            (c"srch", i64::from(height)),
            (c"src_format", pix(format)),
            (c"dstw", i64::from(width)),
            (c"dsth", i64::from(height)),
            (c"dst_format", pix(dst)),
            (c"sws_flags", i64::from(flags.bits())),
            (c"threads", threads as i64),
        ];
        for (name, value) in options {
            // SAFETY: `sws.0` is a live, not yet initialised SwsContext (an
            // AVClass-enabled struct) and `name` is NUL-terminated.
            let ret = unsafe { ffmpeg::sys::av_opt_set_int(sws.0.cast(), name.as_ptr(), value, 0) };
            if ret < 0 {
                return Err(fail(&format!("set {name:?}"), ret));
            }
        }
        // SAFETY: as above; NULL filters mean "no extra filtering".
        let ret = unsafe {
            ffmpeg::sys::sws_init_context(sws.0, std::ptr::null_mut(), std::ptr::null_mut())
        };
        if ret < 0 {
            return Err(fail("init", ret));
        }
        // Intentional: the same table and the same range flag on both sides,
        // whatever the source's tags say. Between YUV formats swscale then
        // changes only chroma subsampling and bit depth: the source's matrix
        // and range (incl. full-range yuvj*) pass through untouched, exactly
        // as they do for the natively carried formats. For RGB sources (PNG,
        // ...) the destination side makes it a standard Rec.709 limited-range
        // conversion, which is what the renderer assumes for YUV input. The
        // return value is ignored: swscale returns -1 for a YUV->YUV context
        // even after it has applied the settings. (With slice threads it
        // forwards the settings to every slice context.)
        unsafe {
            let bt709 = ffmpeg::sys::sws_getCoefficients(ffmpeg::sys::SWS_CS_ITU709);
            ffmpeg::sys::sws_setColorspaceDetails(sws.0, bt709, 0, bt709, 0, 0, 1 << 16, 1 << 16);
        }
        eprintln!(
            "[decoder] converting {format:?} {width}x{height} -> {dst:?} (swscale, {threads} threads)"
        );
        Ok(Self {
            sws,
            src,
            out: ffmpeg::frame::Video::new(dst, width, height),
        })
    }
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
        let src_pix_fmt = ffmpeg::format::Pixel::from(unsafe { (*codec_ctx.as_ptr()).pix_fmt });
        let high_bit_source = is_high_bit_source(src_pix_fmt);
        // HDR transfer hint from the stream's color VUI (for auto-tagging the
        // clip's input color space on import).
        let color_trc: i32 = unsafe { (*stream.parameters().as_ptr()).color_trc as i32 };
        let color_transfer = color_transfer_hint(color_trc);

        // Try VideoToolbox before opening the decoder (8-bit sources of codecs
        // VT decodes only). On success, attach hw_device_ctx so FFmpeg's
        // get_format chooses AV_PIX_FMT_VIDEOTOOLBOX and decoded frames come
        // back as opaque GPU surfaces (which we then transfer to NV12 system
        // memory).
        let hw_device = if high_bit_source || !videotoolbox_decodes(codec_id) {
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
        // renderer picks the right path. Otherwise, report the SW decoder's
        // format if the wire carries it, else the format it's converted to.
        let pixel_format = if using_hw {
            PixelFormat::Nv12
        } else {
            software_wire_format(decoder.format()).map_err(OpenError::UnsupportedPixelFormat)?
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
            color_transfer,
        };

        self.open = Some(OpenContext {
            input,
            decoder,
            stream_index,
            info: info.clone(),
            _hw_device: hw_device,
            sw_dest_frame,
            converter: None,
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
                        &mut ctx.converter,
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
                        &mut ctx.converter,
                        ctx.info.pixel_format,
                    );
                }
            }
            return Err(format!("no frame at or after pts {target_pts}"));
        }
    }
}

/// Convert a decoded `Video` frame into wire `FrameData`, transferring
/// hardware surfaces to system memory and converting any frame whose format
/// isn't `pixel_format` (the wire format chosen at open) via `converter`.
///
/// Takes split borrows (sw_dest_frame, converter, pixel_format) instead of
/// `&mut ctx` so callers can hold a mutable borrow on `ctx.input` (the
/// packets iterator) at the same time.
fn materialize_frame(
    decoded: &ffmpeg::frame::Video,
    sw_dest_frame: Option<&mut ffmpeg::frame::Video>,
    converter: &mut Option<FrameConverter>,
    pixel_format: PixelFormat,
) -> Result<FrameData, String> {
    let format_i32 = unsafe { (*decoded.as_ptr()).format };
    let sw_frame: &ffmpeg::frame::Video = if hwaccel::is_hardware_pixel_format(format_i32) {
        let dest = sw_dest_frame.ok_or("got HW frame but no sw_dest_frame allocated")?;
        unsafe {
            hwaccel::transfer_to_sw(
                decoded.as_ptr() as *const std::ffi::c_void,
                dest.as_mut_ptr() as *mut std::ffi::c_void,
            )?;
            let pts = (*decoded.as_ptr()).pts;
            (*dest.as_mut_ptr()).pts = pts;
        }
        dest
    } else {
        decoded
    };
    if map_pixel_format(sw_frame.format()).ok() == Some(pixel_format) {
        frame_to_payload(sw_frame, pixel_format)
    } else {
        let converted = convert_frame(converter, sw_frame, pixel_format)?;
        frame_to_payload(converted, pixel_format)
    }
}

/// Run `frame` through the cached converter (building or rebuilding it if
/// the source format/size differs) and return the converted frame.
fn convert_frame<'c>(
    slot: &'c mut Option<FrameConverter>,
    frame: &ffmpeg::frame::Video,
    target: PixelFormat,
) -> Result<&'c ffmpeg::frame::Video, String> {
    let src = (frame.format(), frame.width(), frame.height());
    let conv = match slot.take() {
        Some(c) if c.src == src => slot.insert(c),
        _ => slot.insert(FrameConverter::new(src, target)?),
    };
    // SAFETY: `frame` matches the context's source (format, size), checked
    // above, and `conv.out` was allocated in its destination format and size.
    let ret =
        unsafe { ffmpeg::sys::sws_scale_frame(conv.sws.0, conv.out.as_mut_ptr(), frame.as_ptr()) };
    if ret < 0 {
        return Err(format!("swscale: {}", ffmpeg::Error::from(ret)));
    }
    conv.out.set_pts(frame.pts());
    Ok(&conv.out)
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
                    &mut ctx.converter,
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
                match materialize_frame(
                    &decoded,
                    ctx.sw_dest_frame.as_mut(),
                    &mut ctx.converter,
                    ctx.info.pixel_format,
                ) {
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

/// The wire format for a decoded format the wire carries as-is (no
/// conversion pass). Every other format goes through `FrameConverter`.
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

/// The wire format a software decoder outputting `decoded` delivers: the
/// format itself when the wire carries it natively, else the 4:2:0 format
/// it's converted to (10-bit for a >8-bit source so ProRes / DNxHR / 4:2:2
/// HEVC stay 10-bit, else 8-bit). Fails only for formats swscale can't read.
fn software_wire_format(decoded: ffmpeg::format::Pixel) -> Result<PixelFormat, String> {
    if let Ok(native) = map_pixel_format(decoded) {
        return Ok(native);
    }
    if !ffmpeg::software::scaling::support::input(decoded) {
        return Err(format!("{decoded:?}"));
    }
    Ok(if is_high_bit_source(decoded) {
        PixelFormat::Yuv420P10Le
    } else {
        PixelFormat::Yuv420P
    })
}

fn wire_to_ffmpeg(pf: PixelFormat) -> ffmpeg::format::Pixel {
    use ffmpeg::format::Pixel as P;
    match pf {
        PixelFormat::Yuv420P => P::YUV420P,
        PixelFormat::Nv12 => P::NV12,
        PixelFormat::Yuv420P10Le => P::YUV420P10LE,
    }
}

/// Map an ffmpeg `AVColorTransferCharacteristic` value to a coarse HDR hint.
fn color_transfer_hint(trc: i32) -> vance_decoder::protocol::ColorTransferHint {
    use ffmpeg::sys::AVColorTransferCharacteristic::*;
    use vance_decoder::protocol::ColorTransferHint as H;
    if trc == AVCOL_TRC_SMPTE2084 as i32 {
        H::Pq
    } else if trc == AVCOL_TRC_ARIB_STD_B67 as i32 {
        H::Hlg
    } else {
        H::Unknown
    }
}

/// Codecs we attach VideoToolbox to. Attaching it to anything else (PNG,
/// FFV1, MJPEG, ...) still decodes, because FFmpeg silently falls back to
/// software, but `open()` has then reported `Nv12` and every frame pays a
/// pointless swscale pass to NV12. VP9 / AV1 are left off on purpose: VT
/// decodes them only on some Macs, so they stay on the software path.
fn videotoolbox_decodes(codec: ffmpeg::codec::Id) -> bool {
    use ffmpeg::codec::Id;
    matches!(
        codec,
        Id::H264 | Id::HEVC | Id::MPEG1VIDEO | Id::MPEG2VIDEO | Id::MPEG4 | Id::PRORES
    )
}

/// Whether a source pixel format has more than 8 bits per component (10-bit
/// HEVC, 10-bit 4:2:2 ProRes / DNxHR, 12-bit 4:4:4, P010, 16-bit RGB, ...).
/// We force SOFTWARE decode for these: VideoToolbox returns a >8-bit surface
/// but the HW path unconditionally reports/handles NV12, which would corrupt
/// the frame or drop it to 8 bits. Software decode keeps the depth, carried
/// as `yuv420p10le` (natively or via `FrameConverter`).
fn is_high_bit_source(pf: ffmpeg::format::Pixel) -> bool {
    // SAFETY: `descriptor()` only returns non-null pointers into libavutil's
    // static descriptor table.
    pf.descriptor()
        .is_some_and(|d| unsafe { (*d.as_ptr()).comp[0].depth } > 8)
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
        // Width in SAMPLES for this plane. NV12's UV plane interleaves one U
        // and one V per chroma column, so it's 2 * ceil(w/2) — one more than
        // `width` when the width is odd.
        let plane_width = if plane == 0 {
            width
        } else if expected_pf == PixelFormat::Nv12 {
            width.div_ceil(2) * 2
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

#[cfg(test)]
mod tests {
    use super::*;
    use ffmpeg::format::Pixel as P;

    #[test]
    fn high_bit_detection_uses_component_depth() {
        for pf in [P::YUV422P10LE, P::YUV444P12LE, P::P010LE, P::YUV420P10LE] {
            assert!(is_high_bit_source(pf), "{pf:?} should be high-bit");
        }
        for pf in [P::YUV420P, P::NV12, P::YUV444P, P::RGB24, P::None] {
            assert!(!is_high_bit_source(pf), "{pf:?} should be 8-bit");
        }
    }

    #[test]
    fn videotoolbox_is_attached_only_to_allowlisted_codecs() {
        use ffmpeg::codec::Id;
        for id in [
            Id::H264,
            Id::HEVC,
            Id::MPEG1VIDEO,
            Id::MPEG2VIDEO,
            Id::MPEG4,
            Id::PRORES,
        ] {
            assert!(videotoolbox_decodes(id), "{id:?} should use VideoToolbox");
        }
        for id in [
            Id::FFV1,
            Id::PNG,
            Id::MJPEG,
            Id::VP9,
            Id::AV1,
            Id::DNXHD,
            Id::RAWVIDEO,
            Id::None,
        ] {
            assert!(
                !videotoolbox_decodes(id),
                "{id:?} should decode in software"
            );
        }
    }

    /// Slice threading must not change a single output sample. Odd size and
    /// pseudo-random content, tall enough for every thread to get a slice.
    #[test]
    fn threaded_conversion_matches_single_threaded() {
        let (w, h) = (641, 361);
        let cases = [
            (P::YUV422P10LE, PixelFormat::Yuv420P10Le),
            (P::YUV444P12LE, PixelFormat::Yuv420P10Le),
            (P::RGB24, PixelFormat::Yuv420P),
            (P::YUVJ422P, PixelFormat::Yuv420P),
            (P::YUV420P, PixelFormat::Nv12),
        ];
        for (format, target) in cases {
            let mut src = ffmpeg::frame::Video::new(format, w, h);
            let mut seed = 0x9e37_79b9_u32;
            let mut next = move || {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed
            };
            // SAFETY: as in `is_high_bit_source`.
            let depth = format
                .descriptor()
                .map_or(8, |d| unsafe { (*d.as_ptr()).comp[0].depth });
            for plane in 0..src.planes() {
                let data = src.data_mut(plane);
                if depth > 8 {
                    for s in data.chunks_exact_mut(2) {
                        let v = (next() % (1 << depth)) as u16;
                        s.copy_from_slice(&v.to_le_bytes());
                    }
                } else {
                    data.iter_mut().for_each(|b| *b = next() as u8);
                }
            }
            let run = |threads| {
                let conv = FrameConverter::with_threads((format, w, h), target, threads).unwrap();
                let mut slot = Some(conv);
                let out = convert_frame(&mut slot, &src, target).unwrap();
                frame_to_payload(out, target).unwrap().bytes
            };
            assert!(
                run(1) == run(8),
                "{format:?} -> {target:?} differs when threaded"
            );
        }
    }

    #[test]
    fn software_wire_format_keeps_native_and_converts_the_rest() {
        let cases = [
            (P::YUV420P, PixelFormat::Yuv420P),
            (P::YUVJ420P, PixelFormat::Yuv420P),
            (P::NV12, PixelFormat::Nv12),
            (P::YUV420P10LE, PixelFormat::Yuv420P10Le),
            (P::YUV422P10LE, PixelFormat::Yuv420P10Le),
            (P::YUV444P12LE, PixelFormat::Yuv420P10Le),
            (P::YUV444P, PixelFormat::Yuv420P),
            (P::YUVJ422P, PixelFormat::Yuv420P),
            (P::RGB24, PixelFormat::Yuv420P),
        ];
        for (src, wire) in cases {
            assert_eq!(software_wire_format(src), Ok(wire), "{src:?}");
        }
        assert!(software_wire_format(P::None).is_err());
    }
}
