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
//!
//! Both decode paths time frames with a [`FrameClock`], which also gives a
//! timestamp to frames of a container that stores none (AVI).

use crate::alloc::{Allocator, Wait};
use crate::pack::{self, FrameConverter, PackedFrame, SwsColor};
use std::collections::VecDeque;
use std::ffi::c_int;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use vance_decoder::hwaccel::{self, HwDeviceCtx};
use vance_decoder::protocol::{
    ColorMatrixHint, ColorRangeHint, ColorTransferHint, OpenError, PixelFormat, StreamInfo,
};
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
    /// How the stream's YUV is encoded, for `converter`.
    color: SwsColor,
    /// The keyframe packet [`seek`] stopped on, read but not decoded yet:
    /// the next decode sends it before reading on.
    pending: Option<ffmpeg::Packet>,
    /// The video stream's first pts in microseconds (0 when unknown): where
    /// [`seek`]'s search for an earlier keyframe ends.
    start_us: i64,
    /// The container stores dts only (AVI): see [`FrameClock`].
    dts_only: bool,
    /// `dts_only`, and the codec reorders frames: decodes check the
    /// keyframe `seek` stops on (see [`SeekStart`]).
    check_seek_start: bool,
    /// Times the one-shot decoder's frames since its last flush.
    clock: FrameClock,
    /// The one-shot decoder has been sent the end of the stream since its
    /// last flush.
    eof_sent: bool,
    /// Where a `decode_at` can decode on from without seeking.
    forward: Forward,
    /// How the `decode_at`s went, logged when the file closes.
    counts: DecodeCounts,
}

/// The one-shot decoder's position, for [`decode_forward`].
struct Forward {
    /// pts of the frame the last `decode_at` returned, while the one-shot
    /// decoder is still right after it. `None` after a seek, a stream or a
    /// failed decode.
    last: Option<i64>,
    /// Packets [`read_ahead`] read past what the decoder has been sent.
    ahead: VecDeque<ffmpeg::Packet>,
    /// How far past `last` a target may be: [`FORWARD_WINDOW_SECS`] in the
    /// stream's time base.
    window: i64,
    /// Every frame is a keyframe (an intra-only codec), so a seek decodes
    /// exactly the target: decoding on would only read the frames between.
    never: bool,
}

impl Forward {
    fn reaches(&self, target_pts: i64) -> bool {
        !self.never
            && self
                .last
                .is_some_and(|last| target_pts > last && target_pts - last <= self.window)
    }

    fn reset(&mut self) {
        self.last = None;
        self.ahead.clear();
    }
}

/// How the `decode_at`s of one open file went.
#[derive(Default)]
struct DecodeCounts {
    /// Decoded on from the last frame returned.
    forward: u32,
    /// Of `forward`: restarted at a keyframe read on the way.
    restarted: u32,
    /// Decoded after a seek.
    seeked: u32,
}

impl Drop for OpenContext {
    /// Log the [`DecodeCounts`] (the decoder's tests read this line).
    fn drop(&mut self) {
        let c = &self.counts;
        if c.forward + c.seeked > 0 {
            let line = format!(
                "[decoder] decode_at: {} decoded forward ({} restarted at a keyframe), {} after a seek\n",
                c.forward, c.restarted, c.seeked
            );
            // One write, so the line stays whole next to FFmpeg's output.
            let _ = std::io::stderr().write_all(line.as_bytes());
        }
    }
}

/// How far ahead of the last frame `decode_at` returned a target is decoded
/// to without a seek. Decoding on gives the same frames a seek would
/// (restarting at any keyframe on the way, as a seek would start there), and
/// saves both the seek and decoding again from the keyframe to the last
/// frame: the frame step that cost 70–180 ms per layer. The bound is on how
/// much is read on the way: beyond a GOP or two (typically 0.5–2 s) a seek
/// skips the packets in between instead of reading them.
const FORWARD_WINDOW_SECS: i64 = 2;

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
    // FFmpeg's one AVI demuxer, OpenDML (> 1 GB) files included.
    let dts_only = input.format().name() == "avi";
    let stream = input.stream(stream_index).ok_or(OpenError::NoVideoStream)?;
    let time_base = stream.time_base();
    // SAFETY: the live stream of `input`, its index as the demuxer read it.
    let avi_step = dts_only.then(|| unsafe { avi_frame_step(stream.as_ptr().cast_mut()) });
    let frame_rate = displayed_frame_rate(
        stream.avg_frame_rate(),
        stream.rate(),
        avi_step.flatten().map(|step| (step, time_base)),
        dts_only,
    );
    let duration = stream.duration();
    let start_time = stream.start_time();
    let codec_id = stream.parameters().id();
    let codec_name = format!("{codec_id:?}").to_lowercase();
    // SAFETY: the live stream of `input` and its codec parameters. FFmpeg
    // keeps only valid values in the colour fields read as enums here, and
    // av_guess_sample_aspect_ratio only reads the stream (its format
    // argument is unused).
    let (tags, sample_aspect) = unsafe {
        let st = stream.as_ptr();
        let par = &*(*st).codecpar;
        let tags = StreamTags {
            range: par.color_range,
            space: par.color_space,
            primaries: par.color_primaries,
            transfer: color_transfer_hint(par.color_trc as i32),
        };
        let sar = ffmpeg::sys::av_guess_sample_aspect_ratio(
            input.as_ptr().cast_mut(),
            st.cast_mut(),
            std::ptr::null_mut(),
        );
        (tags, reduced_aspect(sar.num, sar.den))
    };

    let codec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
        .map_err(|e| OpenError::FfmpegError(e.to_string()))?;

    // Detect the source bit depth from the stream's declared pixel
    // format BEFORE choosing a decode path. A >8-bit source forces
    // software decode (the HW/NV12 path can't carry 10-bit correctly), and
    // so does alpha (NV12 has none).
    let src_pix_fmt = ffmpeg::format::Pixel::from(unsafe { (*codec_ctx.as_ptr()).pix_fmt });
    let high_bit_source = is_high_bit_source(src_pix_fmt);
    let alpha_source = pack::has_alpha(src_pix_fmt);
    // Formats whose 4th byte is often padding rather than alpha. Every
    // other codec with alpha (ProRes 4444, FFV1, PNG, VP8/VP9, ...) means
    // it, even all zero: that is real transparency.
    let padding_prone = matches!(
        codec_id,
        ffmpeg::codec::Id::RAWVIDEO | ffmpeg::codec::Id::BMP | ffmpeg::codec::Id::QTRLE
    );

    // Try VideoToolbox before opening the decoder (8-bit sources of codecs
    // VT decodes only). On success, attach hw_device_ctx so FFmpeg's
    // get_format chooses AV_PIX_FMT_VIDEOTOOLBOX and decoded frames come
    // back as opaque GPU surfaces (which we then transfer to NV12 system
    // memory).
    let mut hw_device = if high_bit_source || alpha_source || !videotoolbox_decodes(codec_id) {
        None
    } else {
        HwDeviceCtx::try_videotoolbox()
    };
    let mut decoder = open_decoder(codec_ctx, hw_device.as_ref(), &codec_name)?;
    if hw_device.is_some() && !decodes_on_hardware(&mut input, stream_index, &mut decoder) {
        // Without this, StreamInfo said NV12 and every software frame paid
        // a conversion to it (MPEG-4 Part 2, H.264 4:2:2 on Apple Silicon),
        // on a single thread.
        eprintln!("[decoder] VideoToolbox can't decode this {codec_name} stream; software decode");
        hw_device = None;
        let stream = input.stream(stream_index).ok_or(OpenError::NoVideoStream)?;
        let codec_ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| OpenError::FfmpegError(e.to_string()))?;
        decoder = open_decoder(codec_ctx, None, &codec_name)?;
    }
    let using_hw = hw_device.is_some();
    // VideoToolbox does the decoding itself; threads only help software.
    let threads = if using_hw { 1 } else { decode_threads() };

    // If HW decode is in play, decoded frames are AV_PIX_FMT_VIDEOTOOLBOX
    // and we transfer them to NV12. Report NV12 in StreamInfo so the
    // renderer picks the right path. Otherwise, report the SW decoder's
    // format if the wire carries it, else the format it's converted to.
    let pixel_format = if using_hw {
        PixelFormat::Nv12
    } else {
        software_wire_format(decoder.format(), alpha_source)
            .map_err(OpenError::UnsupportedPixelFormat)?
    };
    let signalled = (range_hint(tags.range, src_pix_fmt), matrix_hint(tags.space));
    let (color_range, color_matrix) = wire_hints(src_pix_fmt, pixel_format, signalled);
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

    // Only a codec that reorders frames (B-frames) can show a frame before a
    // keyframe that is decoded after it.
    let codec_props = decoder
        .codec()
        .and_then(|codec| codec.descriptor())
        .map(|d| d.props());
    let check_seek_start = dts_only
        && codec_props.is_some_and(|p| p.contains(ffmpeg::codec::CodecProperties::REORDER));
    let intra_only =
        codec_props.is_some_and(|p| p.contains(ffmpeg::codec::CodecProperties::INTRA_ONLY));
    let time_base = (time_base.numerator() as i64, time_base.denominator() as i64);
    let start_us = match start_time {
        ffmpeg::sys::AV_NOPTS_VALUE => 0,
        pts => pts_to_us(pts, time_base),
    };
    let info = StreamInfo {
        width: decoder.width(),
        height: decoder.height(),
        pixel_format,
        frame_rate,
        time_base,
        duration_pts: if duration > 0 { Some(duration) } else { None },
        codec_name,
        stream_index: stream_index as u32,
        color_transfer: tags.transfer,
        color_range,
        color_matrix,
        sample_aspect,
    };

    let mut ctx = OpenContext {
        input,
        decoder,
        stream_index,
        stream_threads: threads,
        info,
        _hw_device: hw_device,
        sw_dest_frame,
        converter: None,
        color: sws_color(&tags, signalled.0),
        pending: None,
        start_us,
        dts_only,
        check_seek_start,
        clock: FrameClock::new(dts_only),
        eof_sent: false,
        forward: Forward {
            last: None,
            ahead: VecDeque::new(),
            window: match time_base {
                (num, den) if num > 0 && den > 0 => FORWARD_WINDOW_SECS.saturating_mul(den) / num,
                _ => 0,
            },
            never: intra_only,
        },
        counts: DecodeCounts::default(),
    };
    if ctx.info.pixel_format.is_rgba()
        && padding_prone
        && alpha_is_all_zero(&mut ctx, duration, start_time)
    {
        // Decided at open: the stream's format is fixed from here on.
        eprintln!(
            "[decoder] {} alpha is all zero: treated as opaque",
            ctx.info.codec_name
        );
        let wire = software_wire_format(ctx.decoder.format(), false)
            .map_err(OpenError::UnsupportedPixelFormat)?;
        let (range, matrix) = wire_hints(src_pix_fmt, wire, signalled);
        ctx.info.pixel_format = wire;
        ctx.info.color_range = range;
        ctx.info.color_matrix = matrix;
    }
    Ok(ctx)
}

/// Whether the alpha of the stream `ctx` sends as RGBA is all zero, and so
/// means nothing: in uncompressed RGB, BMP and QuickTime Animation the 4th
/// byte is often padding (screen captures), which FFmpeg decodes as alpha,
/// making the clip fully transparent. Only asked for those formats. An
/// all-zero first frame alone doesn't decide it, since an animation can
/// start fully transparent: a frame from the middle and one from the end
/// must be all zero too. `false` when a frame can't be decoded. Leaves the
/// one-shot decoder to be sought before its next decode.
fn alpha_is_all_zero(ctx: &mut OpenContext, duration: i64, start_time: i64) -> bool {
    let start = if start_time == ffmpeg::sys::AV_NOPTS_VALUE {
        0
    } else {
        start_time
    };
    let (tb_num, tb_den) = ctx.info.time_base;
    let to_pts = |us: i64| {
        if tb_num > 0 {
            i128::from(us) * i128::from(tb_den) / (i128::from(tb_num) * 1_000_000)
        } else {
            0
        }
    };
    // Many MKV / NUT streams don't know their own duration: the file's.
    let duration = if duration > 0 {
        duration
    } else {
        // SAFETY: an open input context; `duration` is a plain field.
        let us = unsafe { (*ctx.input.as_ptr()).duration };
        if us > 0 {
            i64::try_from(to_pts(us)).unwrap_or(0)
        } else {
            0
        }
    };
    let mut decoded = ffmpeg::frame::Video::empty();
    let zero = |ctx: &OpenContext, decoded: &ffmpeg::frame::Video| {
        pack::alpha_is_zero(decoded, ctx.color).unwrap_or(false)
    };
    let mut all_zero = decode_after_seek(ctx, &mut decoded, start).is_ok() && zero(ctx, &decoded);
    if all_zero && duration > 0 {
        all_zero = decode_after_seek(ctx, &mut decoded, start + duration / 2).is_ok()
            && zero(ctx, &decoded);
        // The last frame decoded near the end, not one at a computed time:
        // in a variable frame rate or truncated file no frame need start
        // where the frame rate says the last one does.
        all_zero = all_zero
            && [0, 1, 4, 16].into_iter().any(|back_secs| {
                let back = i64::try_from(to_pts(back_secs * 1_000_000)).unwrap_or(0);
                let target = (start + duration - back.max(1)).max(start);
                decode_last_frame(ctx, &mut decoded, target).is_ok()
            })
            && zero(ctx, &decoded);
    }
    ctx.forward.reset();
    all_zero
}

/// The last frame the one-shot decoder gets to from a seek to `target_pts`,
/// in `decoded`: the stream's last frame for a target near its end. A read
/// error ends the stream like its end (a truncated file). `Err` when no
/// frame decodes from there at all.
fn decode_last_frame(
    ctx: &mut OpenContext,
    decoded: &mut ffmpeg::frame::Video,
    target_pts: i64,
) -> Result<(), String> {
    seek(ctx, target_pts)?;
    let mut last = None;
    loop {
        while ctx.decoder.receive_frame(decoded).is_ok() {
            last = Some(decoded.clone());
        }
        if ctx.eof_sent {
            break;
        }
        match next_video_packet(ctx) {
            // A packet the decoder refuses is skipped: only frames count.
            Ok(Some(packet)) => {
                let _ = ctx.decoder.send_packet(&packet);
            }
            Ok(None) | Err(_) => {
                let _ = ctx.decoder.send_eof();
                ctx.eof_sent = true;
            }
        }
    }
    *decoded = last.ok_or_else(|| format!("no frame from pts {target_pts} on"))?;
    Ok(())
}

/// The stream's colour tags, from its codec parameters.
struct StreamTags {
    range: ffmpeg::sys::AVColorRange,
    space: ffmpeg::sys::AVColorSpace,
    primaries: ffmpeg::sys::AVColorPrimaries,
    transfer: ColorTransferHint,
}

/// Open the one-shot decoder from `codec_ctx` (a stream's parameters): on
/// VideoToolbox when `hw` is given, else in software with slice threads.
fn open_decoder(
    mut codec_ctx: ffmpeg::codec::context::Context,
    hw: Option<&HwDeviceCtx>,
    codec_name: &str,
) -> Result<ffmpeg::decoder::Video, OpenError> {
    if let Some(hw) = hw {
        // SAFETY: a live codec context that isn't open yet.
        unsafe { hw.attach_to(codec_ctx.as_mut_ptr() as *mut std::ffi::c_void) };
    } else {
        let threads = decode_threads();
        if threads > 1 {
            set_threads(&mut codec_ctx, threads, ffmpeg::sys::FF_THREAD_SLICE);
        }
    }
    codec_ctx
        .decoder()
        .video()
        .map_err(|e| OpenError::UnsupportedCodec(format!("{codec_name}: {e}")))
}

/// Video packets [`decodes_on_hardware`] sends before it gives up waiting
/// for a frame.
const HW_PROBE_PACKETS: usize = 64;

/// Whether `decoder`, opened with VideoToolbox attached, really decodes on
/// it: FFmpeg falls back to software by itself when VideoToolbox can't
/// decode the stream (MPEG-4 Part 2 on Apple Silicon, H.264 4:2:2), which
/// only the first decoded frame shows. `true` when no frame comes: the
/// decode then reports its own error. Leaves the decoder and `input` where
/// the probe stopped; every decode seeks first.
fn decodes_on_hardware(
    input: &mut ffmpeg::format::context::Input,
    stream_index: usize,
    decoder: &mut ffmpeg::decoder::Video,
) -> bool {
    let mut frame = ffmpeg::frame::Video::empty();
    for _ in 0..HW_PROBE_PACKETS {
        let end = match read_video_packet(input, stream_index) {
            Ok(Some(packet)) => {
                // A packet the decoder refuses is skipped: only a frame tells.
                let _ = decoder.send_packet(&packet);
                false
            }
            _ => {
                let _ = decoder.send_eof();
                true
            }
        };
        if decoder.receive_frame(&mut frame).is_ok() {
            // SAFETY: a frame the decoder just returned.
            return hwaccel::is_hardware_pixel_format(unsafe { (*frame.as_ptr()).format });
        }
        if end {
            break;
        }
    }
    true
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
    // Cleared first so a failed seek leaves no packet from the last one,
    // and no position to decode on from.
    ctx.pending = None;
    ctx.forward.reset();
    ctx.pending = seek_to_keyframe(ctx, target_pts)?;
    restart_decoder(ctx);
    Ok(())
}

/// Flush the one-shot decoder, which then decodes afresh from the next
/// packet it is sent, as after a seek.
fn restart_decoder(ctx: &mut OpenContext) {
    ctx.decoder.flush();
    // Intentional: frames are pulled and dropped right after the flush.
    // FFmpeg 7.1's HEVC decoder keeps its output queue across
    // `avcodec_flush_buffers`, so a backward seek returned a frame from
    // before it: `DecodeAt` frame 22 then frame 0 gave frame 23, and so did
    // the first `DecodeAt` after a stopped stream. Other decoders have
    // nothing left and return EAGAIN at once.
    let mut stale = ffmpeg::frame::Video::empty();
    while ctx.decoder.receive_frame(&mut stale).is_ok() {}
    ctx.clock = FrameClock::new(ctx.dts_only);
    ctx.eof_sent = false;
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
        if let Err(e) = seek_input(&mut ctx.input, from_us, false) {
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
    seek_to_start(ctx, false)?;
    if let Some(key) = scan_for_keyframe(ctx, target_pts, true)? {
        return Ok(Some(key));
    }
    // Intentional: to any frame, not a keyframe. After an ordinary seek
    // FFmpeg's Matroska demuxer drops packets until a keyframe, and a stream
    // without keyframe flags has none, so its decode got no packet at all.
    seek_to_start(ctx, true)?;
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
        // dts where there is no pts (AVI). A keyframe's dts there is the
        // timestamp `FrameClock` gives its frame: in a closed GOP every frame
        // shown before the keyframe is decoded before it.
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

/// Seek the demuxer to `ts_us` or the nearest point before it: a keyframe
/// there, or with `any_frame` any frame.
fn seek_input(
    input: &mut ffmpeg::format::context::Input,
    ts_us: i64,
    any_frame: bool,
) -> Result<(), String> {
    if !any_frame {
        return input
            .seek(ts_us, i64::MIN..=ts_us)
            .map_err(|e| format!("seek: {e}"));
    }
    // SAFETY: an open input context; what `Input::seek` calls, with a flag.
    let ret = unsafe {
        ffmpeg::sys::avformat_seek_file(
            input.as_mut_ptr(),
            -1,
            i64::MIN,
            ts_us,
            ts_us,
            ffmpeg::sys::AVSEEK_FLAG_ANY,
        )
    };
    if ret < 0 {
        return Err(format!("seek: {}", ffmpeg::Error::from(ret)));
    }
    Ok(())
}

/// Seek to the video stream's first packet (see [`seek_input`]).
fn seek_to_start(ctx: &mut OpenContext, any_frame: bool) -> Result<(), String> {
    // Intentional: aimed well before the start pts. MPEG-TS seeks by dts and
    // a B-frame stream's first dts precedes its first pts, so aiming at the
    // start itself stops a packet or two in. Some demuxers (AVI, image and
    // raw streams) refuse a time before their first entry; then the start
    // itself.
    let before = ctx.start_us.saturating_sub(START_MARGIN_US);
    seek_input(&mut ctx.input, before, any_frame)
        .or_else(|_| seek_input(&mut ctx.input, ctx.start_us, any_frame))
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

/// The packet [`seek`] left pending, else the next one [`read_ahead`]
/// read, else the next one from the file.
fn next_video_packet(ctx: &mut OpenContext) -> Result<Option<ffmpeg::Packet>, String> {
    if let Some(packet) = ctx.pending.take().or_else(|| ctx.forward.ahead.pop_front()) {
        return Ok(Some(packet));
    }
    read_video_packet(&mut ctx.input, ctx.stream_index)
}

/// The first frame at or after `target_pts`: decoded on from the last one
/// returned when the target is a little ahead of it ([`Forward::reaches`]),
/// else after a seek.
pub fn decode_at(
    ctx: &mut OpenContext,
    target_pts: i64,
    alloc: &Allocator,
) -> Result<PackedFrame, String> {
    let mut decoded = ffmpeg::frame::Video::empty();
    // A forward decode that fails (a target past the end, a decode error)
    // is retried after a seek, which gives the answer the client gets.
    if ctx.forward.reaches(target_pts) && decode_forward(ctx, &mut decoded, target_pts).is_ok() {
        ctx.counts.forward += 1;
    } else {
        ctx.counts.seeked += 1;
        decode_after_seek(ctx, &mut decoded, target_pts)?;
    }
    // `decode_to` stamped it.
    ctx.forward.last = decoded.pts();
    pack_one_shot(
        &decoded,
        ctx.sw_dest_frame.as_mut(),
        &mut ctx.converter,
        ctx.info.pixel_format,
        ctx.color,
        alloc,
    )
}

/// Seek, then decode the first frame at or after `target_pts` into `decoded`.
fn decode_after_seek(
    ctx: &mut OpenContext,
    decoded: &mut ffmpeg::frame::Video,
    target_pts: i64,
) -> Result<(), String> {
    let mut start = SeekStart::new(target_pts);
    loop {
        let check = start.seek(ctx)?;
        if decode_to(ctx, decoded, target_pts, check)? {
            return Ok(());
        }
        start.step_back();
    }
}

/// Decode on from where the last `decode_at` left the one-shot decoder to
/// the first frame at or after `target_pts`, without a seek. Frames come
/// out in pts order, so this is that frame, as after a seek.
///
/// The packets up to the target are read first ([`read_ahead`]). When one of
/// them is a keyframe at or before the target the decoder restarts there,
/// as after a seek to it, and the packets before it are never decoded: the
/// decode then costs what a seek's would, minus the seek.
fn decode_forward(
    ctx: &mut OpenContext,
    decoded: &mut ffmpeg::frame::Video,
    target_pts: i64,
) -> Result<(), String> {
    // The decoder may already hold the target (a reordered frame).
    while ctx.decoder.receive_frame(decoded).is_ok() {
        let delay = reorder_delay(&ctx.decoder);
        if ctx.clock.stamp(decoded, delay) >= target_pts {
            return Ok(());
        }
    }
    if !ctx.eof_sent && read_ahead(ctx, target_pts)? {
        restart_decoder(ctx);
        ctx.counts.restarted += 1;
    }
    decode_to(ctx, decoded, target_pts, false).map(|_| ())
}

/// Read video packets into `ctx.forward.ahead` (after any left there) up to
/// the first one at or after `target_pts` (by pts, else dts), the end of the
/// file, or [`KEYFRAME_SCAN_LIMIT`] packets. On a keyframe at or before the
/// target, drop the packets before it and return `true`: decoding should
/// restart at it.
///
/// Not in AVI with a codec that reorders frames: a keyframe's dts there is
/// earlier than its frame (see [`SeekStart`]), so a target among the
/// B-frames shown before it would restart past them.
fn read_ahead(ctx: &mut OpenContext, target_pts: i64) -> Result<bool, String> {
    if let Some(packet) = ctx.pending.take() {
        ctx.forward.ahead.push_front(packet);
    }
    let ahead = &mut ctx.forward.ahead;
    let mut restart = false;
    let mut i = 0;
    loop {
        if i == ahead.len() {
            if i >= KEYFRAME_SCAN_LIMIT {
                break;
            }
            match read_video_packet(&mut ctx.input, ctx.stream_index)? {
                Some(packet) => ahead.push_back(packet),
                None => break,
            }
        }
        let packet = &ahead[i];
        let ts = packet.pts().or(packet.dts());
        if packet.is_key() && !ctx.check_seek_start && ts.is_some_and(|ts| ts <= target_pts) {
            ahead.drain(..i);
            i = 0;
            restart = true;
        }
        if ts.is_some_and(|ts| ts >= target_pts) {
            break;
        }
        i += 1;
    }
    Ok(restart)
}

/// Where a decode for a target starts: [`seek`] for the target, then, in AVI
/// with a codec that reorders frames, a keyframe earlier each time the
/// decode's first frame is past the target.
///
/// AVI's index seeks by dts, and a keyframe's timestamp there is only known
/// once it is decoded ([`FrameClock`]). An open GOP's keyframe is decoded
/// before the B-frames shown just ahead of it, so its dts is earlier than
/// its frame, and a target among those B-frames seeks to a keyframe whose
/// frame comes after the target (and whose B-frames can't be decoded
/// without the GOP before): `DecodeAt` frame 11 of an MPEG-4 Part 2 file
/// with 12-frame open GOPs gave frame 12. Other containers seek by the
/// keyframe's own pts, which `seek` checks against the target before.
struct SeekStart {
    /// What to `seek` for: the target, then just before the last keyframe.
    from: i64,
    /// The keyframe the last `seek` stopped on, when its decode checks it.
    keyframe: Option<i64>,
}

impl SeekStart {
    fn new(target_pts: i64) -> Self {
        Self {
            from: target_pts,
            keyframe: None,
        }
    }

    /// Seek. `true`: the decode from here has to check that its first frame
    /// is not past the target (with [`decode_to`]).
    fn seek(&mut self, ctx: &mut OpenContext) -> Result<bool, String> {
        seek(ctx, self.from)?;
        let keyframe = ctx
            .pending
            .as_ref()
            .and_then(ffmpeg::Packet::dts)
            .filter(|_| ctx.check_seek_start);
        // Only a keyframe before the last one can start earlier.
        let check = match (keyframe, self.keyframe) {
            (Some(k), Some(last)) => k < last,
            (k, _) => k.is_some(),
        };
        self.keyframe = keyframe;
        Ok(check)
    }

    /// Start before the keyframe the last `seek` stopped on.
    fn step_back(&mut self) {
        if let Some(k) = self.keyframe {
            self.from = k.saturating_sub(1);
        }
    }
}

/// Decode on the one-shot decoder from where [`seek`] (or the last
/// `decode_at`) left it to the first frame at or after `target_pts`, which
/// is left in `decoded`. `Ok(false)`: `check_first`, and the first frame
/// decoded was already past the target. Not used by streams, which send
/// every frame they decode.
fn decode_to(
    ctx: &mut OpenContext,
    decoded: &mut ffmpeg::frame::Video,
    target_pts: i64,
    check_first: bool,
) -> Result<bool, String> {
    let mut first = true;
    loop {
        // Frames the decoder holds come first: decoding on, it may have
        // decoded past the last frame it returned. (None after a flush.)
        while ctx.decoder.receive_frame(decoded).is_ok() {
            let delay = reorder_delay(&ctx.decoder);
            let pts = ctx.clock.stamp(decoded, delay);
            if pts >= target_pts {
                return Ok(!(first && check_first && pts > target_pts));
            }
            first = false;
        }
        if ctx.eof_sent {
            return Err(format!("no frame at or after pts {target_pts}"));
        }
        match next_video_packet(ctx)? {
            Some(packet) => {
                ctx.decoder
                    .send_packet(&packet)
                    .map_err(|e| format!("send_packet: {e}"))?;
                ctx.clock.sent(&packet);
            }
            None => {
                ctx.decoder
                    .send_eof()
                    .map_err(|e| format!("send_eof: {e}"))?;
                ctx.eof_sent = true;
            }
        }
    }
}

/// [`seek`] for a stream from `from_pts`, the keyframe checked as for
/// [`decode_at`], by decoding up to its first frame on the one-shot decoder.
fn seek_for_stream(ctx: &mut OpenContext, from_pts: i64) -> Result<(), String> {
    let mut start = SeekStart::new(from_pts);
    let mut decoded = ffmpeg::frame::Video::empty();
    while start.seek(ctx)? {
        // A decode error here is left for the stream to report.
        let past = matches!(decode_to(ctx, &mut decoded, i64::MIN, false), Ok(true))
            && decoded.pts().is_some_and(|pts| pts > from_pts);
        if !past {
            // Back to the keyframe the check decoded from.
            start.seek(ctx)?;
            break;
        }
        start.step_back();
    }
    Ok(())
}

/// Times the frames of one decoder from a flush (or its opening) on: told
/// of every packet the decoder accepts ([`FrameClock::sent`]), it gives each
/// frame the decoder returns its presentation timestamp
/// ([`FrameClock::stamp`]).
///
/// A container with pts (MP4, MOV, MKV, MPEG-TS, ...): a frame's own pts,
/// else FFmpeg's best-effort timestamp. AVI stores no pts. Its index gives
/// each chunk a dts, in decode order, and FFmpeg's best-effort timestamp
/// for a frame there is the dts of the packet whose sending released it
/// (the pts it guesses for some packets is a dts too). Without B-frames
/// that is the frame's own packet. With them the decoder holds frames back
/// by its reorder delay, so the timestamp comes that many frames late (the
/// same content in MOV shows frame `k` at `k / fps`, and the AVI's audio
/// starts at 0), and the last frames, released by the end-of-stream drain,
/// have none.
///
/// Intentional: so in AVI a frame takes the dts of the packet `delay`
/// packets before the one that released it, and a frame the drain releases
/// the next dts left. Frames come out in display order, so the n-th frame
/// takes the n-th dts: its own slot. Looked up by value, not counted, so a
/// packet the decoder returns nothing for (an MPEG-4 N-VOP placeholder for
/// a dropped frame) costs one frame a late timestamp instead of shifting
/// every later one, and dts that step by more than 1 (a `-c copy` remux
/// pads with empty chunks, which the demuxer skips) are kept as they are.
struct FrameClock {
    dts_only: bool,
    /// AVI: dts of the packets sent that no frame has taken, in send order.
    unclaimed: VecDeque<i64>,
    /// AVI: the last timestamp given and the step to it from the one before.
    last: Option<(i64, i64)>,
}

impl FrameClock {
    fn new(dts_only: bool) -> Self {
        Self {
            dts_only,
            unclaimed: VecDeque::new(),
            last: None,
        }
    }

    /// Note `packet`, which the decoder has just accepted.
    fn sent(&mut self, packet: &ffmpeg::Packet) {
        if let (true, Some(dts)) = (self.dts_only, packet.dts()) {
            self.unclaimed.push_back(dts);
        }
    }

    /// `frame`'s presentation timestamp, also set as its pts (what is sent
    /// to the client). `delay`: the decoder's [`reorder_delay`].
    fn stamp(&mut self, frame: &mut ffmpeg::frame::Video, delay: usize) -> i64 {
        let ts = if self.dts_only {
            self.claim(frame.timestamp(), delay)
        } else if let Some(pts) = frame.pts() {
            return pts;
        } else {
            frame.timestamp().unwrap_or(0)
        };
        frame.set_pts(Some(ts));
        ts
    }

    /// AVI: the dts `delay` packets before the one with the frame's
    /// best-effort timestamp, else the oldest unclaimed one.
    fn claim(&mut self, best_effort: Option<i64>, delay: usize) -> i64 {
        let released_by =
            best_effort.and_then(|ts| self.unclaimed.iter().position(|&dts| dts == ts));
        let slot = released_by.map_or(0, |i| i.saturating_sub(delay));
        let ts = match self.unclaimed.get(slot) {
            Some(&dts) => {
                // Older ones belong to packets that gave no frame.
                self.unclaimed.drain(..=slot);
                dts
            }
            // More frames than packets: one step on.
            None => match self.last {
                Some((last, step)) => last + step,
                None => best_effort.unwrap_or(0),
            },
        };
        let step = match self.last {
            Some((last, _)) if ts > last => ts - last,
            Some((_, step)) => step,
            None => 1,
        };
        self.last = Some((ts, step));
        ts
    }
}

/// How many packets `decoder` holds a frame back to reorder frames
/// (`has_b_frames`): 0 without B-frames.
fn reorder_delay(decoder: &ffmpeg::decoder::Video) -> usize {
    // SAFETY: an open decoder's context. With frame threads FFmpeg copies
    // the field from the worker before it returns that worker's frame.
    usize::try_from(unsafe { (*decoder.as_ptr()).has_b_frames }).unwrap_or(0)
}

/// `materialize_frame` on the main thread, which never waits for a buffer.
fn pack_one_shot(
    decoded: &ffmpeg::frame::Video,
    sw_dest_frame: Option<&mut ffmpeg::frame::Video>,
    converter: &mut Option<FrameConverter>,
    pixel_format: PixelFormat,
    color: SwsColor,
    alloc: &Allocator,
) -> Result<PackedFrame, String> {
    pack::materialize_frame(
        decoded,
        sw_dest_frame,
        converter,
        pixel_format,
        color,
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
    seek_for_stream(&mut ctx, from_pts)?;

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
                color: ctx.color,
                alloc: &alloc,
                tx: &tx,
                stop: &stop,
                from_pts,
                // Either decoder is fresh: its own, or the one `seek` flushed.
                clock: FrameClock::new(ctx.dts_only),
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
    color: SwsColor,
    alloc: &'a Allocator,
    tx: &'a SyncSender<Option<DecodedItem>>,
    stop: &'a AtomicBool,
    from_pts: i64,
    clock: FrameClock,
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
        sink.clock.sent(&packet);
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
                    if let Err(end) = self.send_decoded(reorder_delay(decoder)) {
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
    /// `delay`: the decoder's [`reorder_delay`].
    fn send_decoded(&mut self, delay: usize) -> Result<(), StreamEnd> {
        // Typical when the seek snapped to an earlier keyframe.
        if self.clock.stamp(&mut self.decoded, delay) < self.from_pts {
            return Ok(());
        }
        let frame = match pack::materialize_frame(
            &self.decoded,
            self.sw_dest_frame.as_deref_mut(),
            self.converter,
            self.pixel_format,
            self.color,
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
/// format itself when the wire carries it natively, else what it's converted
/// to: RGBA for a format with alpha (unless `keep_alpha` is false: its
/// alpha means nothing), else 4:2:0 (10-bit for a >8-bit source
/// so ProRes / DNxHR / 4:2:2 HEVC stay 10-bit, else 8-bit). Fails only for
/// formats swscale can't read.
fn software_wire_format(
    decoded: ffmpeg::format::Pixel,
    keep_alpha: bool,
) -> Result<PixelFormat, String> {
    match pack::map_pixel_format(decoded) {
        Ok(native) if keep_alpha || !native.is_rgba() => return Ok(native),
        _ => {}
    }
    if !ffmpeg::software::scaling::support::input(decoded) {
        return Err(format!("{decoded:?}"));
    }
    Ok(if keep_alpha && pack::has_alpha(decoded) {
        PixelFormat::Rgba8
    } else if is_high_bit_source(decoded) {
        PixelFormat::Yuv420P10Le
    } else {
        PixelFormat::Yuv420P
    })
}

/// Map an ffmpeg `AVColorTransferCharacteristic` value to a coarse HDR hint.
fn color_transfer_hint(trc: i32) -> ColorTransferHint {
    use ffmpeg::sys::AVColorTransferCharacteristic::*;
    if trc == AVCOL_TRC_SMPTE2084 as i32 {
        ColorTransferHint::Pq
    } else if trc == AVCOL_TRC_ARIB_STD_B67 as i32 {
        ColorTransferHint::Hlg
    } else {
        ColorTransferHint::Unknown
    }
}

/// The sample range a stream signals. A `yuvj*` format is full range
/// whatever the tags say (that is what the `j` means).
fn range_hint(range: ffmpeg::sys::AVColorRange, format: ffmpeg::format::Pixel) -> ColorRangeHint {
    use ffmpeg::format::Pixel as P;
    use ffmpeg::sys::AVColorRange::*;
    let yuvj = matches!(
        format,
        P::YUVJ420P | P::YUVJ422P | P::YUVJ444P | P::YUVJ440P | P::YUVJ411P
    );
    match range {
        _ if yuvj => ColorRangeHint::Full,
        AVCOL_RANGE_JPEG => ColorRangeHint::Full,
        AVCOL_RANGE_MPEG => ColorRangeHint::Limited,
        _ => ColorRangeHint::Unknown,
    }
}

/// The YUV matrix a stream signals, among those the renderer implements.
fn matrix_hint(space: ffmpeg::sys::AVColorSpace) -> ColorMatrixHint {
    use ffmpeg::sys::AVColorSpace::*;
    match space {
        AVCOL_SPC_BT709 => ColorMatrixHint::Bt709,
        AVCOL_SPC_BT470BG | AVCOL_SPC_SMPTE170M => ColorMatrixHint::Bt601,
        AVCOL_SPC_BT2020_NCL => ColorMatrixHint::Bt2020Ncl,
        _ => ColorMatrixHint::Unknown,
    }
}

/// The range and matrix hints for frames sent as `wire` from a `source`
/// whose tags give `signalled`: they describe the bytes as sent.
/// - RGBA: full-range RGB, no YUV matrix.
/// - An RGB (or palette) source in a YUV wire format: what the converter
///   made of it, Rec.709 limited range.
/// - YUV or gray, as is or converted (which keeps range and matrix, and is
///   what VideoToolbox does too): what the stream signals.
fn wire_hints(
    source: ffmpeg::format::Pixel,
    wire: PixelFormat,
    signalled: (ColorRangeHint, ColorMatrixHint),
) -> (ColorRangeHint, ColorMatrixHint) {
    if wire.is_rgba() {
        (ColorRangeHint::Full, ColorMatrixHint::Unknown)
    } else if pack::is_rgb_like(source) {
        (ColorRangeHint::Limited, ColorMatrixHint::Bt709)
    } else {
        signalled
    }
}

/// How the converter reads the stream's YUV (for RGBA output, the only
/// conversion where it changes a sample): its `range`, and the matrix its
/// tags name, among all swscale has. Untagged, a frame's own tags count
/// (see [`SwsColor`]), else the matrix is BT.709, or BT.2020 for BT.2020
/// primaries or an HDR transfer (the renderer's defaults for untagged YUV),
/// and the range is limited.
fn sws_color(tags: &StreamTags, range: ColorRangeHint) -> SwsColor {
    let tagged = pack::sws_matrix(tags.space);
    let bt2020 = tags.primaries == ffmpeg::sys::AVColorPrimaries::AVCOL_PRI_BT2020
        || tags.transfer != ColorTransferHint::Unknown;
    let untagged = if bt2020 {
        ffmpeg::sys::SWS_CS_BT2020
    } else {
        ffmpeg::sys::SWS_CS_ITU709
    };
    SwsColor {
        full_range: range == ColorRangeHint::Full,
        matrix: tagged.unwrap_or(untagged),
        range_tagged: range != ColorRangeHint::Unknown,
        matrix_tagged: tagged.is_some(),
    }
}

/// A sample aspect ratio, reduced; `(1, 1)` for square pixels and for an
/// unknown or invalid ratio (`0/1`, negative).
fn reduced_aspect(num: c_int, den: c_int) -> (u32, u32) {
    let (Ok(num), Ok(den)) = (u32::try_from(num), u32::try_from(den)) else {
        return (1, 1);
    };
    if num == 0 || den == 0 {
        return (1, 1);
    }
    let (mut a, mut b) = (num, den);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    (num / a, den / a)
}

/// The rate frames are shown at, `(num, den)`. The stream's average frame
/// rate, except in AVI, whose rate is its chunk rate: a `-c copy` remux pads
/// every frame with empty chunks, how many depending on the FFmpeg version
/// (23 of 24 from a 1/600 MOV with FFmpeg 7 and 8), and FFmpeg's own
/// estimate (`real`) isn't the frame rate either (6.1 gives 50 for a 25 fps
/// MJPEG remux). There `avi_step` is the frames' measured step in ticks of
/// the time base ([`avi_frame_step`]); without one (no index), `real` when
/// it is the lower.
fn displayed_frame_rate(
    avg: ffmpeg::Rational,
    real: ffmpeg::Rational,
    avi_step: Option<(i64, ffmpeg::Rational)>,
    dts_only: bool,
) -> (i64, i64) {
    let pair = |r: ffmpeg::Rational| (i64::from(r.numerator()), i64::from(r.denominator()));
    let (avg, real) = (pair(avg), pair(real));
    let valid = |(n, d): (i64, i64)| n > 0 && d > 0;
    if let Some((step, time_base)) = avi_step {
        let (tb_num, tb_den) = pair(time_base);
        if step > 0 && valid((tb_num, tb_den)) {
            // frames per second = 1 / (step * time base)
            let (num, den) = (tb_den, tb_num.saturating_mul(step));
            let (mut a, mut b) = (num, den);
            while b != 0 {
                (a, b) = (b, a % b);
            }
            return (num / a, den / a);
        }
    }
    if dts_only && valid(real) && (!valid(avg) || avg.0 * real.1 > real.0 * avg.1) {
        real
    } else {
        avg
    }
}

/// Index entries [`avi_frame_step`] looks at: the start of the file says
/// enough.
const FRAME_STEP_ENTRIES: c_int = 256;

/// AVI: the step between the stream's frames, in ticks of its time base:
/// the most common gap between consecutive entries of its index, which
/// lists only chunks that hold a frame (an empty padding chunk still takes
/// its tick). `None` without an index.
///
/// # Safety
/// `stream` is a live stream of an open input, not touched meanwhile.
unsafe fn avi_frame_step(stream: *mut ffmpeg::sys::AVStream) -> Option<i64> {
    // SAFETY: as the caller promises; each entry is read before the next
    // call (the pointers stay valid until the context is used).
    let timestamps: Vec<i64> = unsafe {
        let n = ffmpeg::sys::avformat_index_get_entries_count(stream).clamp(0, FRAME_STEP_ENTRIES);
        (0..n)
            .filter_map(|i| {
                let entry = ffmpeg::sys::avformat_index_get_entry(stream, i);
                (!entry.is_null()).then(|| (*entry).timestamp)
            })
            .collect()
    };
    most_common_step(&timestamps)
}

/// The most common positive gap between consecutive `timestamps` (the
/// smaller one on a tie: a stream with dropped frames keeps its base rate).
fn most_common_step(timestamps: &[i64]) -> Option<i64> {
    let mut counts = std::collections::BTreeMap::new();
    for pair in timestamps.windows(2) {
        let step = pair[1] - pair[0];
        if step > 0 {
            *counts.entry(step).or_insert(0u32) += 1;
        }
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(step, _)| step)
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
            (P::RGBZ, PixelFormat::Yuv420P),
            (P::RGBA, PixelFormat::Rgba8),
            (P::BGRA, PixelFormat::Rgba8),
            (P::ARGB, PixelFormat::Rgba8),
            (P::RGBA64BE, PixelFormat::Rgba8),
            (P::YUVA444P12LE, PixelFormat::Rgba8),
            (P::YUVA420P, PixelFormat::Rgba8),
            (P::YA8, PixelFormat::Rgba8),
            (P::GBRAP, PixelFormat::Rgba8),
            (P::PAL8, PixelFormat::Rgba8),
        ];
        for (src, wire) in cases {
            assert_eq!(software_wire_format(src, true), Ok(wire), "{src:?}");
        }
        assert!(software_wire_format(P::None, true).is_err());
        // An alpha channel that means nothing (all zero): the opaque path.
        for (src, wire) in [
            (P::RGBA, PixelFormat::Yuv420P),
            (P::BGRA, PixelFormat::Yuv420P),
            (P::YUVA444P12LE, PixelFormat::Yuv420P10Le),
            (P::YUV420P, PixelFormat::Yuv420P),
        ] {
            assert_eq!(
                software_wire_format(src, false),
                Ok(wire),
                "{src:?} without alpha"
            );
        }
    }

    #[test]
    fn range_and_matrix_hints_follow_the_tags() {
        use ffmpeg::sys::AVColorRange::*;
        use ffmpeg::sys::AVColorSpace::*;
        use ColorMatrixHint as M;
        use ColorRangeHint as R;
        for (range, format, want) in [
            (AVCOL_RANGE_MPEG, P::YUV420P, R::Limited),
            (AVCOL_RANGE_JPEG, P::YUV420P, R::Full),
            (AVCOL_RANGE_UNSPECIFIED, P::YUV420P, R::Unknown),
            (AVCOL_RANGE_UNSPECIFIED, P::YUVJ420P, R::Full),
            (AVCOL_RANGE_MPEG, P::YUVJ422P, R::Full),
        ] {
            assert_eq!(range_hint(range, format), want, "{range:?} {format:?}");
        }
        for (space, want) in [
            (AVCOL_SPC_BT709, M::Bt709),
            (AVCOL_SPC_BT470BG, M::Bt601),
            (AVCOL_SPC_SMPTE170M, M::Bt601),
            (AVCOL_SPC_BT2020_NCL, M::Bt2020Ncl),
            (AVCOL_SPC_BT2020_CL, M::Unknown),
            (AVCOL_SPC_SMPTE240M, M::Unknown),
            (AVCOL_SPC_RGB, M::Unknown),
            (AVCOL_SPC_UNSPECIFIED, M::Unknown),
        ] {
            assert_eq!(matrix_hint(space), want, "{space:?}");
        }
        // The hints describe what is sent: an RGB source goes out as BT.709
        // limited YUV, an alpha source as RGBA.
        let tagged = (R::Full, M::Bt601);
        assert_eq!(wire_hints(P::YUV422P, PixelFormat::Yuv420P, tagged), tagged);
        assert_eq!(wire_hints(P::GRAY8, PixelFormat::Yuv420P, tagged), tagged);
        assert_eq!(wire_hints(P::NV12, PixelFormat::Nv12, tagged), tagged);
        assert_eq!(
            wire_hints(P::RGB24, PixelFormat::Yuv420P, tagged),
            (R::Limited, M::Bt709)
        );
        assert_eq!(
            wire_hints(P::GBRP10LE, PixelFormat::Yuv420P10Le, tagged),
            (R::Limited, M::Bt709)
        );
        for source in [P::YUVA444P12LE, P::RGBA] {
            assert_eq!(
                wire_hints(source, PixelFormat::Rgba8, tagged),
                (R::Full, M::Unknown),
                "{source:?}"
            );
        }
    }

    #[test]
    fn untagged_matrices_read_as_the_renderer_reads_them() {
        use ffmpeg::sys::AVColorPrimaries::*;
        use ffmpeg::sys::AVColorRange::AVCOL_RANGE_UNSPECIFIED;
        use ffmpeg::sys::AVColorSpace::*;
        use ffmpeg::sys::{SWS_CS_BT2020, SWS_CS_ITU601, SWS_CS_ITU709, SWS_CS_SMPTE240M};
        let tags = |space, primaries, transfer| StreamTags {
            range: AVCOL_RANGE_UNSPECIFIED,
            space,
            primaries,
            transfer,
        };
        let sdr = ColorTransferHint::Unknown;
        for (t, want) in [
            (tags(AVCOL_SPC_BT709, AVCOL_PRI_BT2020, sdr), SWS_CS_ITU709),
            (
                tags(AVCOL_SPC_SMPTE170M, AVCOL_PRI_UNSPECIFIED, sdr),
                SWS_CS_ITU601,
            ),
            (
                tags(AVCOL_SPC_SMPTE240M, AVCOL_PRI_UNSPECIFIED, sdr),
                SWS_CS_SMPTE240M,
            ),
            (
                tags(AVCOL_SPC_BT2020_CL, AVCOL_PRI_UNSPECIFIED, sdr),
                SWS_CS_BT2020,
            ),
            (
                tags(AVCOL_SPC_UNSPECIFIED, AVCOL_PRI_UNSPECIFIED, sdr),
                SWS_CS_ITU709,
            ),
            (
                tags(AVCOL_SPC_UNSPECIFIED, AVCOL_PRI_BT2020, sdr),
                SWS_CS_BT2020,
            ),
            (
                tags(
                    AVCOL_SPC_UNSPECIFIED,
                    AVCOL_PRI_UNSPECIFIED,
                    ColorTransferHint::Pq,
                ),
                SWS_CS_BT2020,
            ),
        ] {
            assert_eq!(
                sws_color(&t, ColorRangeHint::Unknown).matrix,
                want,
                "{:?}",
                t.space
            );
        }
        let t = tags(AVCOL_SPC_BT709, AVCOL_PRI_BT709, sdr);
        assert!(sws_color(&t, ColorRangeHint::Full).full_range);
        assert!(!sws_color(&t, ColorRangeHint::Limited).full_range);
        assert!(!sws_color(&t, ColorRangeHint::Unknown).full_range);
    }

    #[test]
    fn sample_aspect_ratios_are_reduced_and_default_to_square() {
        assert_eq!(reduced_aspect(4, 3), (4, 3));
        assert_eq!(reduced_aspect(128, 90), (64, 45));
        assert_eq!(reduced_aspect(10, 10), (1, 1));
        for (n, d) in [(0, 1), (0, 0), (1, 0), (-4, 3), (4, -3)] {
            assert_eq!(reduced_aspect(n, d), (1, 1), "{n}/{d}");
        }
    }

    /// AVI's chunk rate gives way to the timestamps' rate when that is
    /// lower; other containers keep their average rate.
    #[test]
    fn avi_frame_rates_come_from_the_timestamps() {
        let r = ffmpeg::Rational::new;
        // Measured: 24 ticks of 1/600, 2 of 1/50 (whatever FFmpeg's own
        // estimate says), 1 of 1001/30000.
        let step = |s, tb| Some((s, tb));
        assert_eq!(
            displayed_frame_rate(r(600, 1), r(600, 1), step(24, r(1, 600)), true),
            (25, 1)
        );
        assert_eq!(
            displayed_frame_rate(r(50, 1), r(50, 1), step(2, r(1, 50)), true),
            (25, 1)
        );
        assert_eq!(
            displayed_frame_rate(r(30000, 1001), r(30, 1), step(1, r(1001, 30000)), true),
            (30000, 1001)
        );
        // No index: FFmpeg's estimate when it is the lower.
        assert_eq!(
            displayed_frame_rate(r(600, 1), r(25, 1), None, true),
            (25, 1)
        );
        assert_eq!(
            displayed_frame_rate(r(25, 1), r(25, 1), None, true),
            (25, 1)
        );
        assert_eq!(displayed_frame_rate(r(0, 0), r(25, 1), None, true), (25, 1));
        assert_eq!(
            displayed_frame_rate(r(600, 1), r(0, 0), None, true),
            (600, 1)
        );
        // Not AVI: the average rate.
        assert_eq!(
            displayed_frame_rate(r(600, 1), r(25, 1), None, false),
            (600, 1)
        );
    }

    #[test]
    fn the_frame_step_is_the_most_common_gap() {
        let ticks = |step: i64, n: i64| (0..n).map(|k| k * step).collect::<Vec<_>>();
        assert_eq!(most_common_step(&ticks(24, 10)), Some(24));
        assert_eq!(most_common_step(&ticks(2, 10)), Some(2));
        assert_eq!(most_common_step(&ticks(1, 10)), Some(1));
        // A dropped frame here and there doesn't change the rate.
        assert_eq!(most_common_step(&[0, 1, 2, 4, 5, 6, 8, 9]), Some(1));
        // A tie: the smaller step.
        assert_eq!(most_common_step(&[0, 1, 2, 4, 6]), Some(1));
        assert_eq!(most_common_step(&[0]), None);
        assert_eq!(most_common_step(&[]), None);
        assert_eq!(most_common_step(&[5, 5, 5]), None);
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

    /// An AVI clock told of packets with `dts`, in send order.
    fn avi_clock(dts: &[i64]) -> FrameClock {
        let mut clock = FrameClock::new(true);
        for &d in dts {
            let mut packet = ffmpeg::Packet::empty();
            packet.set_dts(Some(d));
            clock.sent(&packet);
        }
        clock
    }

    /// A decoded frame as FFmpeg returns it: its pts and best-effort
    /// timestamp.
    fn frame(pts: Option<i64>, best_effort: Option<i64>) -> ffmpeg::frame::Video {
        let mut f = ffmpeg::frame::Video::empty();
        f.set_pts(pts);
        // SAFETY: a live frame we own.
        unsafe {
            (*f.as_mut_ptr()).best_effort_timestamp =
                best_effort.unwrap_or(ffmpeg::sys::AV_NOPTS_VALUE);
        }
        f
    }

    /// A container with pts keeps every frame's pts untouched; a frame
    /// without one gets FFmpeg's best-effort timestamp, else 0 as before.
    #[test]
    fn frames_with_pts_keep_them() {
        let mut clock = FrameClock::new(false);
        for (pts, best_effort, want) in [
            (Some(7), Some(9), 7),
            (Some(-3), None, -3),
            (None, Some(11), 11),
            (None, None, 0),
        ] {
            let mut f = frame(pts, best_effort);
            assert_eq!(clock.stamp(&mut f, 2), want, "{pts:?} {best_effort:?}");
            assert_eq!(f.pts(), Some(want), "{pts:?} {best_effort:?}: frame's pts");
        }
    }

    /// The decoder releases frame k with packet k + delay; the frames left
    /// at the end come out with no timestamp. Each takes its own slot, with
    /// dts as they step (24 a frame here, as in a `-c copy` remux).
    #[test]
    fn avi_frames_take_their_slot_back_through_the_reorder_delay() {
        let dts: Vec<i64> = (0..8).map(|k| k * 24).collect();
        for delay in 0..3 {
            let mut clock = avi_clock(&dts);
            let got: Vec<i64> = (0..8)
                .map(|k| {
                    let released_by = dts.get(k + delay).copied();
                    let mut f = frame(None, released_by);
                    let ts = clock.stamp(&mut f, delay);
                    assert_eq!(f.pts(), Some(ts), "delay {delay}: frame {k}'s pts");
                    ts
                })
                .collect();
            assert_eq!(got, dts, "delay {delay}");
        }
    }

    /// FFmpeg's guessed pts on some AVI frames (MPEG-4 Part 2 B-frames) is a
    /// dts like the best-effort timestamp; only the latter is used.
    #[test]
    fn avi_frames_ignore_the_guessed_pts() {
        let mut clock = avi_clock(&[0, 1, 2, 3]);
        let mut got = Vec::new();
        for (pts, best_effort) in [(None, Some(1)), (Some(2), Some(2)), (Some(3), Some(3))] {
            got.push(clock.stamp(&mut frame(pts, best_effort), 1));
        }
        got.push(clock.stamp(&mut frame(None, None), 1));
        assert_eq!(got, [0, 1, 2, 3]);
    }

    /// A packet the decoder returns no frame for (an N-VOP placeholder,
    /// packet 3 here) costs the frame released next one late timestamp; the
    /// frames after are on time again.
    #[test]
    fn a_packet_without_a_frame_does_not_shift_later_frames() {
        let mut clock = avi_clock(&[0, 1, 2, 3, 4, 5, 6]);
        let got: Vec<i64> = [Some(1), Some(2), Some(4), Some(5), Some(6), None]
            .into_iter()
            .map(|best_effort| clock.stamp(&mut frame(None, best_effort), 1))
            .collect();
        assert_eq!(got, [0, 1, 3, 4, 5, 6]);
    }

    /// More frames than packets, or a best-effort timestamp that is no
    /// packet's dts: still strictly increasing.
    #[test]
    fn avi_timestamps_go_on_increasing_past_the_packets() {
        let mut clock = avi_clock(&[0, 24, 48]);
        let got: Vec<i64> = [None, Some(999), None, None, None]
            .into_iter()
            .map(|best_effort| clock.stamp(&mut frame(None, best_effort), 0))
            .collect();
        assert_eq!(got, [0, 24, 48, 72, 96]);
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
