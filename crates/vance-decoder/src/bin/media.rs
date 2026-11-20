//! FFmpeg side of the sidecar: open a file, seek, decode one frame at a PTS,
//! and the background stream decode loop.
//!
//! During streaming a background **decode thread** owns the [`OpenContext`]
//! and pushes packed frames into a bounded channel; the main thread pulls
//! them when a `NextFrame` request arrives. [`Stream::stop`] signals the
//! thread, joins it, and hands the `OpenContext` back for further one-shot
//! `Seek` / `DecodeAt` operations.

use crate::alloc::{Allocator, Wait};
use crate::pack::{self, FrameConverter, PackedFrame};
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
    decoder: ffmpeg::decoder::Video,
    stream_index: usize,
    pub info: StreamInfo,
    /// Holds the VideoToolbox device context (if any) for the lifetime
    /// of this open file. `None` means software decode.
    _hw_device: Option<HwDeviceCtx>,
    /// Reusable destination frame for `av_hwframe_transfer_data`.
    /// `None` when not using HW decode.
    sw_dest_frame: Option<ffmpeg::frame::Video>,
    /// Built on the first decoded frame whose format isn't `info.pixel_format`.
    converter: Option<FrameConverter>,
}

pub fn open(path: &str) -> Result<OpenContext, OpenError> {
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

    Ok(OpenContext {
        input,
        decoder,
        stream_index,
        info,
        _hw_device: hw_device,
        sw_dest_frame,
        converter: None,
    })
}

pub fn seek(ctx: &mut OpenContext, target_pts: i64) -> Result<(), String> {
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
                    return pack_one_shot(
                        &decoded,
                        ctx.sw_dest_frame.as_mut(),
                        &mut ctx.converter,
                        ctx.info.pixel_format,
                        alloc,
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
                    return pack_one_shot(
                        &decoded,
                        ctx.sw_dest_frame.as_mut(),
                        &mut ctx.converter,
                        ctx.info.pixel_format,
                        alloc,
                    );
                }
            }
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

/// Background decode loop. Reads packets, sends decoded frames to `tx`.
/// Sends `None` on clean EOS, `Some(Failure(...))` on decode error.
/// Returns the `OpenContext` so the main thread can reclaim it.
fn decode_stream_loop(
    mut ctx: OpenContext,
    tx: SyncSender<Option<DecodedItem>>,
    stop: Arc<AtomicBool>,
    from_pts: i64,
    alloc: Allocator,
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
                let payload = match pack::materialize_frame(
                    &decoded,
                    ctx.sw_dest_frame.as_mut(),
                    &mut ctx.converter,
                    ctx.info.pixel_format,
                    &alloc,
                    Wait::UntilStopped(&stop),
                ) {
                    Ok(Some(p)) => p,
                    // Stopped while waiting for a frame buffer.
                    Ok(None) => break 'outer,
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
                match pack::materialize_frame(
                    &decoded,
                    ctx.sw_dest_frame.as_mut(),
                    &mut ctx.converter,
                    ctx.info.pixel_format,
                    &alloc,
                    Wait::UntilStopped(&stop),
                ) {
                    Ok(Some(p)) => {
                        if tx.send(Some(DecodedItem::Frame(p))).is_err() {
                            break 'outer;
                        }
                    }
                    Ok(None) => break 'outer,
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
}
