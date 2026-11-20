//! FFmpeg side of the sidecar: open a file, seek, decode one frame at a PTS,
//! and the background stream decode loop.
//!
//! During streaming a background **decode thread** owns the [`OpenContext`]
//! and pushes packed frames into a bounded channel; the main thread pulls
//! them when a `NextFrame` request arrives. [`Stream::stop`] signals the
//! thread, joins it, and hands the `OpenContext` back for further one-shot
//! `Seek` / `DecodeAt` operations.
//!
//! Software decode is threaded (ADR 0006). The one-shot decoder uses slice
//! threads only, which add no output delay, so a seek still yields the
//! target frame without decoding past it. A stream decodes on its own
//! decoder with frame threads as well (several frames at once), which is
//! where most of the speed comes from but costs up to n − 1 frames of
//! output delay and n frames in flight.

use crate::alloc::{Allocator, Wait};
use crate::pack::{self, FrameConverter, PackedFrame};
use std::ffi::c_int;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use vance_decoder::hwaccel::HwDeviceCtx;
use vance_decoder::protocol::{OpenError, PixelFormat, StreamInfo};
use vance_ffmpeg_sys::ffmpeg;

pub struct OpenContext {
    input: ffmpeg::format::context::Input,
    /// The one-shot (`Seek` / `DecodeAt`) decoder: slice threads only.
    decoder: ffmpeg::decoder::Video,
    stream_index: usize,
    /// Threads for a stream's own decoder. 1 (VideoToolbox, or
    /// `VANCE_DECODE_THREADS=1`) streams on `decoder` instead.
    stream_threads: usize,
    pub info: StreamInfo,
    /// Holds the VideoToolbox device context (if any) for the lifetime
    /// of this open file. `None` means software decode.
    _hw_device: Option<HwDeviceCtx>,
    /// Reusable destination frame for `av_hwframe_transfer_data`.
    /// `None` when not using HW decode.
    sw_dest_frame: Option<ffmpeg::frame::Video>,
    /// Built on the first decoded frame whose format isn't `info.pixel_format`.
    converter: Option<FrameConverter>,
    /// The keyframe packet [`seek`] stopped on, read but not decoded yet:
    /// the next decode sends it before reading on.
    pending: Option<ffmpeg::Packet>,
    /// The video stream's first pts in microseconds (0 when unknown): where
    /// [`seek`]'s search for an earlier keyframe ends.
    start_us: i64,
}

pub fn open(path: &str) -> Result<OpenContext, OpenError> {
    let p = Path::new(path);
    if !p.exists() {
        return Err(OpenError::NotFound(path.to_string()));
    }

    let mut input = ffmpeg::format::input(p).map_err(|e| OpenError::FfmpegError(e.to_string()))?;

    let stream_index = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .ok_or(OpenError::NoVideoStream)?
        .index();
    if let Some(format) = raw_stream_without_timestamps(&mut input, stream_index) {
        return Err(OpenError::NeedsContainer { format });
    }
    let stream = input.stream(stream_index).ok_or(OpenError::NoVideoStream)?;
    let time_base = stream.time_base();
    let avg_rate = stream.avg_frame_rate();
    let duration = stream.duration();
    let start_time = stream.start_time();
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
    // VideoToolbox does the decoding itself; threads only help software.
    let threads = if using_hw { 1 } else { decode_threads() };
    if threads > 1 {
        set_threads(&mut codec_ctx, threads, ffmpeg::sys::FF_THREAD_SLICE);
    }

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
            "VideoToolbox HW".to_string()
        } else {
            format!("software, {threads} threads")
        }
    );

    let time_base = (time_base.numerator() as i64, time_base.denominator() as i64);
    let start_us = match start_time {
        ffmpeg::sys::AV_NOPTS_VALUE => 0,
        pts => pts_to_us(pts, time_base),
    };
    let info = StreamInfo {
        width: decoder.width(),
        height: decoder.height(),
        pixel_format,
        frame_rate: (avg_rate.numerator() as i64, avg_rate.denominator() as i64),
        time_base,
        duration_pts: if duration > 0 { Some(duration) } else { None },
        codec_name,
        stream_index: stream_index as u32,
        color_transfer,
    };

    Ok(OpenContext {
        input,
        decoder,
        stream_index,
        stream_threads: threads,
        info,
        _hw_device: hw_device,
        sw_dest_frame,
        converter: None,
        pending: None,
        start_us,
    })
}

/// FFmpeg's demuxers for bare video elementary streams: the codec's own
/// bitstream with no container around it.
const RAW_VIDEO_DEMUXERS: &[&str] = &[
    "av1",
    "avs2",
    "avs3",
    "cavsvideo",
    "dirac",
    "dnxhd",
    "evc",
    "h261",
    "h263",
    "h264",
    "hevc",
    "ingenient",
    "ipu",
    "m4v",
    "mjpeg",
    "mjpeg_2000",
    "mpegvideo",
    "obu",
    "vc1",
    "vvc",
];

/// The demuxer's name if `input` is a raw elementary stream (one of
/// [`RAW_VIDEO_DEMUXERS`]) whose first video packet has no pts. Such a
/// stream has nothing to seek by or to place frames in time with: bare
/// H.264 / HEVC carry no timestamps at all (every seek fails with EPERM),
/// and raw MPEG-1/2 video only gets dts made up from the frame rate, so its
/// frames come out without a pts. Raw streams whose parser does supply a pts
/// (MPEG-4 Part 2, H.263, DNxHD) seek and decode, so they still open. Only
/// these demuxers are checked: a container with dts but no pts (AVI) opens
/// as before, and so do image pipes (a PNG opens as `png_pipe`).
fn raw_stream_without_timestamps(
    input: &mut ffmpeg::format::context::Input,
    stream_index: usize,
) -> Option<String> {
    let name = input.format().name();
    if !RAW_VIDEO_DEMUXERS.contains(&name) {
        return None;
    }
    // Comes from the packets stream probing buffered; every later decode
    // seeks before it reads, so taking one here costs nothing.
    match read_video_packet(input, stream_index) {
        Ok(Some(packet)) if packet.pts().is_some() => None,
        _ => Some(name.to_string()),
    }
}

/// A pts in `time_base` as microseconds (`AV_TIME_BASE`), what `seek` takes.
fn pts_to_us(pts: i64, (tb_num, tb_den): (i64, i64)) -> i64 {
    if tb_den != 0 {
        pts.saturating_mul(tb_num).saturating_mul(1_000_000) / tb_den
    } else {
        pts
    }
}

/// The furthest [`seek_to_keyframe`] steps back from a target before it
/// tries the stream start.
const MAX_SEEK_BACK_US: i64 = 64_000_000;

/// How far before the stream start [`seek_to_start`] aims.
const START_MARGIN_US: i64 = 10_000_000;

/// Video packets one seek attempt reads while looking for a keyframe. Bounds
/// the search in a file whose packets carry no keyframe flags.
const KEYFRAME_SCAN_LIMIT: usize = 2048;

/// Seek so the next decode starts at a keyframe at or before `target_pts`
/// (or at the stream start), and flush the one-shot decoder.
pub fn seek(ctx: &mut OpenContext, target_pts: i64) -> Result<(), String> {
    // Cleared first so a failed seek leaves no packet from the last one.
    ctx.pending = None;
    ctx.pending = seek_to_keyframe(ctx, target_pts)?;
    ctx.decoder.flush();
    // Intentional: frames are pulled and dropped right after the flush.
    // FFmpeg 7.1's HEVC decoder keeps its output queue across
    // `avcodec_flush_buffers`, so a backward seek returned a frame from
    // before it: `DecodeAt` frame 22 then frame 0 gave frame 23, and so did
    // the first `DecodeAt` after a stopped stream. Other decoders have
    // nothing left and return EAGAIN at once.
    let mut stale = ffmpeg::frame::Video::empty();
    while ctx.decoder.receive_frame(&mut stale).is_ok() {}
    Ok(())
}

/// Position `ctx.input` on a keyframe at or before `target_pts` and return
/// that keyframe's packet, already read. `None`: no keyframe was found even
/// from the stream start (packets without keyframe flags), and decoding
/// starts at the stream's first packet.
///
/// An indexed container (MP4, MOV, MKV) seeks straight to such a keyframe,
/// so this reads only the packet the decode would read first anyway. MPEG-TS
/// has no keyframe index: FFmpeg seeks it by timestamp and can stop on any
/// frame, and decoding from there gave H.264's next GOP instead of the
/// target and HEVC frames without their references.
///
/// Intentional: when the seek lands past the keyframe the target needs, it
/// seeks again 1 s, 2 s, 4 s ... (doubling, at most 64 s) before the target,
/// then from the stream start. The step is a guess at the GOP length; the
/// doubling bounds the attempts for any length, and the cap bounds how much
/// a file without keyframe flags is read before decoding from the start.
fn seek_to_keyframe(
    ctx: &mut OpenContext,
    target_pts: i64,
) -> Result<Option<ffmpeg::Packet>, String> {
    let target_us = pts_to_us(target_pts, ctx.info.time_base);
    let mut back_us = 0;
    while back_us <= MAX_SEEK_BACK_US {
        let from_us = target_us.saturating_sub(back_us);
        if from_us <= ctx.start_us {
            break;
        }
        if let Err(e) = seek_input(&mut ctx.input, from_us) {
            if back_us == 0 {
                return Err(e);
            }
            break;
        }
        if let Some(key) = scan_for_keyframe(ctx, target_pts, false)? {
            return Ok(Some(key));
        }
        back_us = if back_us == 0 { 1_000_000 } else { back_us * 2 };
    }
    seek_to_start(ctx)?;
    if let Some(key) = scan_for_keyframe(ctx, target_pts, true)? {
        return Ok(Some(key));
    }
    seek_to_start(ctx)?;
    Ok(None)
}

/// Read video packets up to the first keyframe and return it if decoding
/// can start there: it is at or before `target_pts`, or `from_start` (the
/// stream's first keyframe is the earliest start there is). `None` if it is
/// past the target, or a packet before it already is (a later keyframe is
/// later still), or at end of file, or after [`KEYFRAME_SCAN_LIMIT`]
/// packets. The packets before the keyframe are dropped: they belong to the
/// GOP before it.
fn scan_for_keyframe(
    ctx: &mut OpenContext,
    target_pts: i64,
    from_start: bool,
) -> Result<Option<ffmpeg::Packet>, String> {
    for _ in 0..KEYFRAME_SCAN_LIMIT {
        let Some(packet) = read_video_packet(&mut ctx.input, ctx.stream_index)? else {
            return Ok(None);
        };
        // dts where there is no pts (AVI).
        let past_target = packet
            .pts()
            .or(packet.dts())
            .is_some_and(|ts| ts > target_pts);
        if packet.is_key() {
            return Ok((from_start || !past_target).then_some(packet));
        }
        if past_target && !from_start {
            return Ok(None);
        }
    }
    Ok(None)
}

/// Seek the demuxer to `ts_us` or the nearest point before it.
fn seek_input(input: &mut ffmpeg::format::context::Input, ts_us: i64) -> Result<(), String> {
    input
        .seek(ts_us, i64::MIN..=ts_us)
        .map_err(|e| format!("seek: {e}"))
}

/// Seek to the video stream's first packet.
fn seek_to_start(ctx: &mut OpenContext) -> Result<(), String> {
    // Intentional: aimed well before the start pts. MPEG-TS seeks by dts and
    // a B-frame stream's first dts precedes its first pts, so aiming at the
    // start itself stops a packet or two in. Some demuxers (AVI, image and
    // raw streams) refuse a time before their first entry; then the start
    // itself.
    seek_input(&mut ctx.input, ctx.start_us.saturating_sub(START_MARGIN_US))
        .or_else(|_| seek_input(&mut ctx.input, ctx.start_us))
}

/// The next packet of the video stream, or `None` at end of file.
fn read_video_packet(
    input: &mut ffmpeg::format::context::Input,
    stream_index: usize,
) -> Result<Option<ffmpeg::Packet>, String> {
    loop {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(input) {
            Ok(()) if packet.stream() == stream_index => return Ok(Some(packet)),
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => return Ok(None),
            Err(e) => return Err(format!("read packet: {e}")),
        }
    }
}

/// The packet [`seek`] left pending, else the next one from the file.
fn next_video_packet(ctx: &mut OpenContext) -> Result<Option<ffmpeg::Packet>, String> {
    match ctx.pending.take() {
        Some(packet) => Ok(Some(packet)),
        None => read_video_packet(&mut ctx.input, ctx.stream_index),
    }
}

/// Seek, then decode the first frame at or after `target_pts`.
pub fn decode_at(
    ctx: &mut OpenContext,
    target_pts: i64,
    alloc: &Allocator,
) -> Result<PackedFrame, String> {
    seek(ctx, target_pts)?;
    decode_one_at_or_after(ctx, target_pts, alloc)
}

/// Drain packets + decoder until we get a frame at or after `target_pts`.
/// Used by one-shot `decode_at` (NOT by the streaming loop, which decodes
/// every frame it produces).
fn decode_one_at_or_after(
    ctx: &mut OpenContext,
    target_pts: i64,
    alloc: &Allocator,
) -> Result<PackedFrame, String> {
    let mut decoded = ffmpeg::frame::Video::empty();
    loop {
        let packet = next_video_packet(ctx)?;
        match &packet {
            Some(packet) => ctx
                .decoder
                .send_packet(packet)
                .map_err(|e| format!("send_packet: {e}"))?,
            None => ctx
                .decoder
                .send_eof()
                .map_err(|e| format!("send_eof: {e}"))?,
        }
        while ctx.decoder.receive_frame(&mut decoded).is_ok() {
            let pts = decoded.pts().unwrap_or(0);
            if pts >= target_pts {
                return pack_one_shot(
                    &decoded,
                    ctx.sw_dest_frame.as_mut(),
                    &mut ctx.converter,
                    ctx.info.pixel_format,
                    alloc,
                );
            }
        }
        if packet.is_none() {
            return Err(format!("no frame at or after pts {target_pts}"));
        }
    }
}

/// `materialize_frame` on the main thread, which never waits for a buffer.
fn pack_one_shot(
    decoded: &ffmpeg::frame::Video,
    sw_dest_frame: Option<&mut ffmpeg::frame::Video>,
    converter: &mut Option<FrameConverter>,
    pixel_format: PixelFormat,
    alloc: &Allocator,
) -> Result<PackedFrame, String> {
    pack::materialize_frame(
        decoded,
        sw_dest_frame,
        converter,
        pixel_format,
        alloc,
        Wait::Never,
    )?
    // `Wait::Never` always yields a buffer; this is only a guard.
    .ok_or_else(|| "no frame buffer available".to_string())
}

/// What the decode thread sends the main thread.
pub enum DecodedItem {
    Frame(PackedFrame),
    /// Decoder error mid-stream; thread will exit after sending this.
    Failure(String),
}

/// Decode-thread bookkeeping held by the main thread for the lifetime of a
/// stream. The `JoinHandle` returns the `OpenContext` so we can resume
/// single-shot decoding after the stream stops.
pub struct Stream {
    /// Receives frames in PTS order. `None` = end-of-stream sentinel.
    rx: Receiver<Option<DecodedItem>>,
    /// Tells the decode thread to exit at its next chance.
    stop: Arc<AtomicBool>,
    /// Joins to recover the OpenContext (or an error string).
    handle: JoinHandle<Result<OpenContext, String>>,
}

/// Seek to `from_pts` and start the decode thread. It pre-decodes up to
/// `buffer_frames` (capped at 64) frames ahead of the main thread. On
/// failure `ctx` is dropped (the file is closed).
pub fn start_stream(
    mut ctx: OpenContext,
    from_pts: i64,
    buffer_frames: u32,
    alloc: Allocator,
) -> Result<Stream, String> {
    // Also flushes the one-shot decoder, so it holds no reference frames
    // while the stream's own decoder runs.
    seek(&mut ctx, from_pts)?;

    let cap = (buffer_frames.max(1) as usize).min(64);
    let (tx, rx) = sync_channel::<Option<DecodedItem>>(cap);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();

    let handle = thread::Builder::new()
        .name("vance-decoder.stream".into())
        .spawn(move || decode_stream_loop(ctx, tx, stop_clone, from_pts, alloc))
        .map_err(|e| format!("spawn decode thread: {e}"))?;

    Ok(Stream { rx, stop, handle })
}

impl Stream {
    /// The next item, waiting at most `timeout`. `Ok(None)` is end-of-stream.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<Option<DecodedItem>, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }

    /// Stop and join the decode thread. Returns the `OpenContext`, or `None`
    /// if the thread failed or panicked (the file is then closed).
    pub fn stop(self) -> Option<OpenContext> {
        self.stop.store(true, Ordering::Relaxed);
        // Drain the channel so the producer can unblock and exit.
        while self.rx.try_recv().is_ok() {}
        match self.handle.join() {
            Ok(Ok(ctx)) => Some(ctx),
            Ok(Err(e)) => {
                eprintln!("[stream] decode thread returned error: {e}");
                None
            }
            Err(_) => {
                eprintln!("[stream] decode thread panicked");
                None
            }
        }
    }
}

/// Background decode loop: decodes from where `start_stream` sought to,
/// sends frames to `tx`, then `None` at a clean end of stream or
/// `Some(Failure(...))` on a decode error. Returns the `OpenContext` so the
/// main thread can reclaim it.
fn decode_stream_loop(
    mut ctx: OpenContext,
    tx: SyncSender<Option<DecodedItem>>,
    stop: Arc<AtomicBool>,
    from_pts: i64,
    alloc: Allocator,
) -> Result<OpenContext, String> {
    let end = match open_stream_decoder(&ctx) {
        Err(e) => StreamEnd::Failed(e),
        Ok(mut own) => {
            let mut sink = FrameSink {
                decoded: ffmpeg::frame::Video::empty(),
                sw_dest_frame: ctx.sw_dest_frame.as_mut(),
                converter: &mut ctx.converter,
                pixel_format: ctx.info.pixel_format,
                alloc: &alloc,
                tx: &tx,
                stop: &stop,
                from_pts,
            };
            let decoder = own.as_mut().unwrap_or(&mut ctx.decoder);
            let pending = ctx.pending.take();
            let end = feed_stream(
                &mut ctx.input,
                ctx.stream_index,
                pending,
                decoder,
                &mut sink,
            );
            drop(sink);
            // Joins the frame threads (each finishes the frame it is on) and
            // frees their frames before the sentinel below waits for room.
            drop(own);
            end
        }
    };
    match end {
        StreamEnd::Eos => {
            let _ = tx.send(None);
        }
        StreamEnd::Failed(reason) => {
            let _ = tx.send(Some(DecodedItem::Failure(reason)));
        }
        StreamEnd::Stopped => {}
    }
    Ok(ctx)
}

/// A stream's own decoder, opened from the stream's codec parameters with
/// frame and slice threads, or `None` to stream on the one-shot decoder
/// (`stream_threads` is 1). The caller drops it when the stream ends.
///
/// Intentional: a second decoder for the same file. Frame threading delays
/// output by up to n − 1 frames and decodes that far past any target; on
/// the one-shot decoder it made a 4K ProRes `DecodeAt` slower than with no
/// threads at all (78 vs 48 ms on an M1). Kept separate, the one-shot
/// decoder stays on slice threads, which add no delay (~30 ms there).
fn open_stream_decoder(ctx: &OpenContext) -> Result<Option<ffmpeg::decoder::Video>, String> {
    if ctx.stream_threads <= 1 {
        return Ok(None);
    }
    let stream = ctx
        .input
        .stream(ctx.stream_index)
        .ok_or("stream decoder: video stream missing")?;
    let mut codec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|e| format!("stream decoder: {e}"))?;
    // FFmpeg picks frame threads when the codec has them, else slice threads.
    set_threads(
        &mut codec_ctx,
        ctx.stream_threads,
        ffmpeg::sys::FF_THREAD_FRAME | ffmpeg::sys::FF_THREAD_SLICE,
    );
    let decoder = codec_ctx
        .decoder()
        .video()
        .map_err(|e| format!("open stream decoder: {e}"))?;
    // SAFETY: an opened decoder; `active_thread_type` is set by the open.
    let active = unsafe { (*decoder.as_ptr()).active_thread_type };
    eprintln!(
        "[stream] decoder: {} threads ({})",
        ctx.stream_threads,
        if active & ffmpeg::sys::FF_THREAD_FRAME != 0 {
            "frame"
        } else if active & ffmpeg::sys::FF_THREAD_SLICE != 0 {
            "slice"
        } else {
            "codec has none"
        }
    );
    Ok(Some(decoder))
}

/// Why a stream's decode loop ended.
enum StreamEnd {
    /// Every frame was decoded and sent.
    Eos,
    /// The stop flag was set, or the receiver went away.
    Stopped,
    /// Decoding failed; the main thread gets this as the stream's last item.
    Failed(String),
}

/// What the stream needs to turn decoded frames into sent ones. Borrows
/// fields of the `OpenContext` apart from `input`, which `feed_stream`
/// reads at the same time.
struct FrameSink<'a> {
    /// Reused for every `receive_frame`.
    decoded: ffmpeg::frame::Video,
    sw_dest_frame: Option<&'a mut ffmpeg::frame::Video>,
    converter: &'a mut Option<FrameConverter>,
    pixel_format: PixelFormat,
    alloc: &'a Allocator,
    tx: &'a SyncSender<Option<DecodedItem>>,
    stop: &'a AtomicBool,
    from_pts: i64,
}

/// Feed `pending` (the keyframe `seek` stopped on), then the rest of the
/// video stream's packets to `decoder`, sending every frame it produces,
/// then drain it to the end.
fn feed_stream(
    input: &mut ffmpeg::format::context::Input,
    stream_index: usize,
    mut pending: Option<ffmpeg::Packet>,
    decoder: &mut ffmpeg::decoder::Video,
    sink: &mut FrameSink<'_>,
) -> StreamEnd {
    loop {
        let packet = match pending.take() {
            Some(packet) => packet,
            None => match read_video_packet(input, stream_index) {
                Ok(Some(packet)) => packet,
                Ok(None) => break,
                Err(e) => return StreamEnd::Failed(e),
            },
        };
        // Checked after the read, as late as it can be: `send_packet` may
        // decode the whole frame, which nothing interrupts.
        if sink.stopped() {
            return StreamEnd::Stopped;
        }
        let mut retried = false;
        loop {
            match decoder.send_packet(&packet) {
                Ok(()) => break,
                // EAGAIN: the decoder wants its output read first, after
                // which it accepts the same packet (FFmpeg's send/receive
                // contract). Only once: a second EAGAIN would be a bug.
                Err(e) if is_eagain(&e) && !retried => {
                    retried = true;
                    if let Some(end) = sink.drain(decoder) {
                        return end;
                    }
                }
                Err(e) => return StreamEnd::Failed(format!("send_packet: {e}")),
            }
        }
        // Frame threads return nothing for the first n − 1 packets.
        if let Some(end) = sink.drain(decoder) {
            return end;
        }
    }
    if let Err(e) = decoder.send_eof() {
        return StreamEnd::Failed(format!("send_eof: {e}"));
    }
    // Draining: frame threads still hold up to n − 1 frames.
    sink.drain(decoder)
        .unwrap_or_else(|| StreamEnd::Failed("decoder wants input after end of stream".to_string()))
}

impl FrameSink<'_> {
    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Receive and send every frame `decoder` has ready. `None` means it
    /// wants another packet; otherwise the stream is over (`Eos` once a
    /// drain after `send_eof` is complete).
    fn drain(&mut self, decoder: &mut ffmpeg::decoder::Video) -> Option<StreamEnd> {
        loop {
            if self.stopped() {
                return Some(StreamEnd::Stopped);
            }
            match decoder.receive_frame(&mut self.decoded) {
                Ok(()) => {
                    if let Err(end) = self.send_decoded() {
                        return Some(end);
                    }
                }
                Err(e) if is_eagain(&e) => return None,
                Err(ffmpeg::Error::Eof) => return Some(StreamEnd::Eos),
                // Intentional: logged and skipped, not fatal. Before frame
                // threading these errors silently ended a drain, and the
                // engine stops playback on a stream failure; ffmpeg's CLI
                // also keeps going. FFmpeg itself ends a drain that only
                // errors, so this can't spin.
                Err(e) => eprintln!("[stream] frame skipped, decode error: {e}"),
            }
        }
    }

    /// Pack and send the frame just received, unless it is before `from_pts`.
    fn send_decoded(&mut self) -> Result<(), StreamEnd> {
        // Typical when the seek snapped to an earlier keyframe.
        if self.decoded.pts().unwrap_or(0) < self.from_pts {
            return Ok(());
        }
        let frame = match pack::materialize_frame(
            &self.decoded,
            self.sw_dest_frame.as_deref_mut(),
            self.converter,
            self.pixel_format,
            self.alloc,
            Wait::UntilStopped(self.stop),
        ) {
            Ok(Some(f)) => f,
            // Stopped while waiting for a frame buffer.
            Ok(None) => return Err(StreamEnd::Stopped),
            Err(e) => return Err(StreamEnd::Failed(e)),
        };
        // Fails only once the receiver is gone: the stream was stopped.
        self.tx
            .send(Some(DecodedItem::Frame(frame)))
            .map_err(|_| StreamEnd::Stopped)
    }
}

/// Software decode threads: `VANCE_DECODE_THREADS` if set, else the default
/// (see [`env_threads`]).
fn decode_threads() -> usize {
    env_threads("VANCE_DECODE_THREADS")
}

/// A thread count from the environment variable `name`: its value when that
/// is a positive integer, else one per core, capped at 8. Any other value is
/// logged and ignored. The client's thread budget sets these at spawn.
pub fn env_threads(name: &str) -> usize {
    threads_from_env(name, std::env::var(name).ok().as_deref())
}

fn threads_from_env(name: &str, value: Option<&str>) -> usize {
    if let Some(v) = value {
        match v.trim().parse::<usize>() {
            Ok(n) if n > 0 => return n,
            _ => eprintln!("[decoder] ignoring {name}={v:?}: not a positive integer"),
        }
    }
    thread::available_parallelism().map_or(1, |n| n.get().min(8))
}

/// Set an unopened codec context's thread count and allowed thread types
/// (`FF_THREAD_*` bits). Raw fields: ffmpeg-the-third's `set_threading`
/// takes a single thread type, and the stream decoder needs FRAME | SLICE.
fn set_threads(codec_ctx: &mut ffmpeg::codec::context::Context, count: usize, types: c_int) {
    // SAFETY: a live AVCodecContext that isn't open yet; FFmpeg reads both
    // fields when it opens.
    unsafe {
        let raw = codec_ctx.as_mut_ptr();
        (*raw).thread_count = c_int::try_from(count).unwrap_or(c_int::MAX);
        (*raw).thread_type = types;
    }
}

/// Whether `e` is `AVERROR(EAGAIN)`: "read output first" from
/// `send_packet`, "send more input" from `receive_frame`.
fn is_eagain(e: &ffmpeg::Error) -> bool {
    match *e {
        ffmpeg::Error::Other { errno } => is_eagain_errno(errno),
        _ => false,
    }
}

#[cfg(unix)]
fn is_eagain_errno(errno: c_int) -> bool {
    std::io::Error::from_raw_os_error(errno).kind() == std::io::ErrorKind::WouldBlock
}

/// std's raw OS errors on Windows are Win32 codes, not C `errno` values, so
/// compare with `EAGAIN` from the CRT's `errno.h` (MSVC and MinGW), which
/// FFmpeg's `AVERROR(EAGAIN)` is built from.
#[cfg(windows)]
fn is_eagain_errno(errno: c_int) -> bool {
    errno == 11
}

/// The wire format a software decoder outputting `decoded` delivers: the
/// format itself when the wire carries it natively, else the 4:2:0 format
/// it's converted to (10-bit for a >8-bit source so ProRes / DNxHR / 4:2:2
/// HEVC stay 10-bit, else 8-bit). Fails only for formats swscale can't read.
fn software_wire_format(decoded: ffmpeg::format::Pixel) -> Result<PixelFormat, String> {
    if let Ok(native) = pack::map_pixel_format(decoded) {
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

    #[test]
    fn thread_overrides_must_be_positive_integers() {
        let default = thread::available_parallelism().map_or(1, |n| n.get().min(8));
        for name in ["VANCE_DECODE_THREADS", "VANCE_CONVERT_THREADS"] {
            assert_eq!(threads_from_env(name, None), default, "{name} unset");
            assert_eq!(threads_from_env(name, Some("1")), 1, "{name}");
            assert_eq!(threads_from_env(name, Some(" 12 ")), 12, "{name}");
            for bad in ["0", "-2", "eight", "", "2.5"] {
                assert_eq!(threads_from_env(name, Some(bad)), default, "{name}={bad:?}");
            }
        }
    }

    /// `is_eagain` must recognise the EAGAIN FFmpeg really returns: a
    /// decoder that has had no input asks for some.
    #[test]
    fn eagain_is_recognised_from_a_real_decoder() {
        let codec = ffmpeg::codec::decoder::find(ffmpeg::codec::Id::MJPEG).expect("mjpeg decoder");
        let mut decoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .decoder()
            .video()
            .expect("open mjpeg decoder");
        let mut frame = ffmpeg::frame::Video::empty();
        let err = decoder.receive_frame(&mut frame).unwrap_err();
        assert!(is_eagain(&err), "{err:?}");
        for other in [
            ffmpeg::Error::Eof,
            ffmpeg::Error::InvalidData,
            ffmpeg::Error::from(ffmpeg::sys::AVERROR(12)),
        ] {
            assert!(!is_eagain(&other), "{other:?}");
        }
    }
}
