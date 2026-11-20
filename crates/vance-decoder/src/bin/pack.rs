//! Decoded frame → wire frame: hardware-surface transfer, pixel-format
//! conversion (swscale) and packing the planes tightly into a frame buffer
//! from the [`Allocator`].

use crate::alloc::{Allocator, FrameBuf, Wait};
use std::thread;
use vance_decoder::hwaccel;
use vance_decoder::protocol::{FrameMeta, PixelFormat};
use vance_ffmpeg_sys::ffmpeg;

/// A packed frame waiting to be sent: its description plus its bytes.
pub struct PackedFrame {
    pub meta: FrameMeta,
    pub buf: FrameBuf,
}

/// Cached swscale pass from a decoded frame's non-wire pixel format (4:2:2,
/// 4:4:4, RGB, 12-bit, ...) to the wire format chosen at open. Rebuilt only
/// when the source format or size changes.
pub struct FrameConverter {
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

/// Convert a decoded `Video` frame into a packed wire frame, transferring
/// hardware surfaces to system memory and converting any frame whose format
/// isn't `pixel_format` (the wire format chosen at open) via `converter`.
/// `Ok(None)` means `wait` gave up on getting a buffer (the stream stopped).
///
/// Takes split borrows (sw_dest_frame, converter, pixel_format) instead of
/// `&mut ctx` so callers can hold a mutable borrow on `ctx.input` (the
/// packets iterator) at the same time.
pub fn materialize_frame(
    decoded: &ffmpeg::frame::Video,
    sw_dest_frame: Option<&mut ffmpeg::frame::Video>,
    converter: &mut Option<FrameConverter>,
    pixel_format: PixelFormat,
    alloc: &Allocator,
    wait: Wait<'_>,
) -> Result<Option<PackedFrame>, String> {
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
        frame_to_payload(sw_frame, pixel_format, alloc, wait)
    } else {
        let converted = convert_frame(converter, sw_frame, pixel_format)?;
        frame_to_payload(converted, pixel_format, alloc, wait)
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

/// The wire format for a decoded format the wire carries as-is (no
/// conversion pass). Every other format goes through `FrameConverter`.
pub fn map_pixel_format(pf: ffmpeg::format::Pixel) -> Result<PixelFormat, String> {
    use ffmpeg::format::Pixel as P;
    match pf {
        P::YUV420P | P::YUVJ420P => Ok(PixelFormat::Yuv420P),
        P::NV12 => Ok(PixelFormat::Nv12),
        // 10-bit 4:2:0 (HEVC Main10 / HDR PQ-HLG) → carried 10-bit.
        P::YUV420P10LE => Ok(PixelFormat::Yuv420P10Le),
        other => Err(format!("{other:?}")),
    }
}

fn wire_to_ffmpeg(pf: PixelFormat) -> ffmpeg::format::Pixel {
    use ffmpeg::format::Pixel as P;
    match pf {
        PixelFormat::Yuv420P => P::YUV420P,
        PixelFormat::Nv12 => P::NV12,
        PixelFormat::Yuv420P10Le => P::YUV420P10LE,
    }
}

/// Pack `frame`'s planes back to back (padding stripped) into a buffer of
/// exactly the frame's size. `Ok(None)` when `wait` gave up on a buffer.
fn frame_to_payload(
    frame: &ffmpeg::frame::Video,
    expected_pf: PixelFormat,
    alloc: &Allocator,
    wait: Wait<'_>,
) -> Result<Option<PackedFrame>, String> {
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

    // (rows, width in samples) per plane.
    let planes: Vec<(u32, u32)> = (0..plane_count)
        .map(|plane| {
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
            (plane_height, plane_width)
        })
        .collect();
    let total: usize = planes
        .iter()
        .map(|&(rows, samples)| rows as usize * samples as usize * bytes_per_sample)
        .sum();
    let Some(mut buf) = alloc.alloc(total, wait) else {
        return Ok(None);
    };

    for (plane, &(plane_height, plane_width)) in planes.iter().enumerate() {
        let stride = frame.stride(plane);
        // Row length in BYTES (samples × bytes-per-sample).
        let row_bytes = plane_width as usize * bytes_per_sample;
        let data = frame.data(plane);
        for row in 0..plane_height as usize {
            let start = row * stride;
            buf.push(&data[start..start + row_bytes]);
        }
    }

    Ok(Some(PackedFrame {
        meta: FrameMeta {
            pts,
            width,
            height,
            pixel_format: expected_pf,
            // Emitted stride is the tight per-row SAMPLE count (padding stripped).
            strides: planes.iter().map(|&(_, samples)| samples).collect(),
        },
        buf,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffmpeg::format::Pixel as P;

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
            // SAFETY: `descriptor()` only returns non-null pointers into
            // libavutil's static descriptor table.
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
                packed_bytes(out, target)
            };
            assert!(
                run(1) == run(8),
                "{format:?} -> {target:?} differs when threaded"
            );
        }
    }

    /// The packed size is exactly the wire frame size, odd dimensions
    /// included, and the strides are the tight sample counts.
    #[test]
    fn packing_is_tight_and_exactly_frame_sized() {
        let (w, h) = (161, 91);
        for (format, wire, strides) in [
            (P::YUV420P, PixelFormat::Yuv420P, vec![161, 81, 81]),
            (P::NV12, PixelFormat::Nv12, vec![161, 162]),
            (P::YUV420P10LE, PixelFormat::Yuv420P10Le, vec![161, 81, 81]),
        ] {
            let frame = ffmpeg::frame::Video::new(format, w, h);
            let packed = frame_to_payload(&frame, wire, &Allocator::default(), Wait::Never)
                .unwrap()
                .unwrap();
            assert_eq!(packed.meta.strides, strides, "{format:?}");
            assert_eq!((packed.meta.width, packed.meta.height), (w, h));
            let (_, blob) = packed.buf.into_payload();
            assert_eq!(blob.unwrap().len(), wire.frame_size(w, h), "{format:?}");
        }
    }

    fn packed_bytes(frame: &ffmpeg::frame::Video, target: PixelFormat) -> Vec<u8> {
        let packed = frame_to_payload(frame, target, &Allocator::default(), Wait::Never)
            .unwrap()
            .unwrap();
        packed.buf.into_payload().1.unwrap()
    }
}
