//! Decoded frame → wire frame: hardware-surface transfer, pixel-format
//! conversion (swscale) and packing the planes tightly into a frame buffer
//! from the [`Allocator`].

use crate::alloc::{Allocator, FrameBuf, Wait};
use crate::media;
use std::ffi::c_int;
use vance_decoder::hwaccel;
use vance_decoder::protocol::{FrameMeta, PixelFormat};
use vance_ffmpeg_sys::ffmpeg;

/// A packed frame waiting to be sent: its description plus its bytes.
pub struct PackedFrame {
    pub meta: FrameMeta,
    pub buf: FrameBuf,
}

/// How a stream's YUV samples are encoded, as the converter reads them: the
/// sample range and the YUV → RGB matrix (an `SWS_CS_*` value), from the
/// stream's tags (see `media::sws_color`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwsColor {
    pub full_range: bool,
    pub matrix: c_int,
    /// The stream's tags named the range / the matrix. Where they didn't, a
    /// frame's own tags (a ProRes frame header, say) are used instead.
    pub range_tagged: bool,
    pub matrix_tagged: bool,
}

impl Default for SwsColor {
    /// Untagged: limited-range BT.709, what untagged video is read as.
    fn default() -> Self {
        Self {
            full_range: false,
            matrix: ffmpeg::sys::SWS_CS_ITU709,
            range_tagged: false,
            matrix_tagged: false,
        }
    }
}

impl SwsColor {
    /// These details for `frame`: its own range and matrix tags fill in
    /// what the stream's tags left out.
    fn for_frame(self, frame: &ffmpeg::frame::Video) -> Self {
        use ffmpeg::sys::AVColorRange::{AVCOL_RANGE_JPEG, AVCOL_RANGE_MPEG};
        // SAFETY: a decoded frame; FFmpeg keeps only valid enum values.
        let (range, space) = unsafe {
            let f = &*frame.as_ptr();
            (f.color_range, f.colorspace)
        };
        let mut color = self;
        if !self.range_tagged {
            match range {
                AVCOL_RANGE_JPEG => color.full_range = true,
                AVCOL_RANGE_MPEG => color.full_range = false,
                _ => {}
            }
        }
        if let (false, Some(matrix)) = (self.matrix_tagged, sws_matrix(space)) {
            color.matrix = matrix;
        }
        color
    }
}

/// The swscale matrix (`SWS_CS_*`) for a tagged YUV colour space, among
/// all swscale has; `None` for an untagged (or RGB) one.
pub fn sws_matrix(space: ffmpeg::sys::AVColorSpace) -> Option<c_int> {
    use ffmpeg::sys::AVColorSpace::*;
    use ffmpeg::sys::{SWS_CS_BT2020, SWS_CS_FCC, SWS_CS_ITU601, SWS_CS_ITU709, SWS_CS_SMPTE240M};
    Some(match space {
        AVCOL_SPC_BT709 => SWS_CS_ITU709,
        AVCOL_SPC_FCC => SWS_CS_FCC,
        AVCOL_SPC_BT470BG | AVCOL_SPC_SMPTE170M => SWS_CS_ITU601,
        AVCOL_SPC_SMPTE240M => SWS_CS_SMPTE240M,
        AVCOL_SPC_BT2020_NCL | AVCOL_SPC_BT2020_CL => SWS_CS_BT2020,
        _ => return None,
    })
}

/// The `AV_PIX_FMT_FLAG_*` bits of `pf` (0 for an unknown format).
fn pixel_flags(pf: ffmpeg::format::Pixel) -> u64 {
    // SAFETY: `descriptor()` only returns non-null pointers into libavutil's
    // static descriptor table.
    pf.descriptor()
        .map_or(0, |d| unsafe { (*d.as_ptr()).flags })
}

/// Whether `pf` has an alpha channel (yuva*, rgba / bgra / argb, rgba64,
/// ya8, gbrap, pal8 ...): such sources go out as [`PixelFormat::Rgba8`].
pub fn has_alpha(pf: ffmpeg::format::Pixel) -> bool {
    pixel_flags(pf) & ffmpeg::sys::AV_PIX_FMT_FLAG_ALPHA as u64 != 0
}

/// Whether `pf` holds RGB (or palette / XYZ) samples rather than YUV or
/// gray, so no YUV matrix or range applies to it.
pub fn is_rgb_like(pf: ffmpeg::format::Pixel) -> bool {
    use ffmpeg::sys::{AV_PIX_FMT_FLAG_PAL, AV_PIX_FMT_FLAG_RGB, AV_PIX_FMT_FLAG_XYZ};
    let rgb = (AV_PIX_FMT_FLAG_RGB | AV_PIX_FMT_FLAG_PAL | AV_PIX_FMT_FLAG_XYZ) as u64;
    pixel_flags(pf) & rgb != 0
}

/// Whether every alpha sample of `frame` (a format with alpha) is 0. Read
/// from its RGBA form, so every alpha layout (planar, packed, palette) is
/// one check; `color` is how the stream's YUV is read on the way.
pub fn alpha_is_zero(frame: &ffmpeg::frame::Video, color: SwsColor) -> Result<bool, String> {
    let mut slot = None;
    let rgba = if frame.format() == ffmpeg::format::Pixel::RGBA {
        frame
    } else {
        convert_frame(&mut slot, frame, PixelFormat::Rgba8, color)?
    };
    let (row_bytes, stride) = (rgba.width() as usize * 4, rgba.stride(0));
    let data = rgba.data(0);
    Ok((0..rgba.height() as usize).all(|row| {
        let start = row * stride;
        data[start..start + row_bytes]
            .iter()
            .skip(3)
            .step_by(4)
            .all(|&a| a == 0)
    }))
}

/// Cached swscale pass from a decoded frame's non-wire pixel format (4:2:2,
/// 4:4:4, RGB, 12-bit, alpha ...) to the wire format chosen at open.
/// Rebuilt only when the source format or size changes.
pub struct FrameConverter {
    sws: SwsContext,
    /// (format, width, height) of the source frames `sws` was built for.
    src: (ffmpeg::format::Pixel, u32, u32),
    /// The colour details it reads them with.
    color: SwsColor,
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

/// swscale slice threads per converter: `VANCE_CONVERT_THREADS` if set, else
/// one per core, capped at 8 (each converter keeps its own worker pool).
/// Single-threaded, a 4K 10-bit 4:2:2 -> 4:2:0 pass took ~30 ms on an M1, a
/// whole real-time frame budget; 8 threads brought it to ~9 ms.
fn conversion_threads() -> usize {
    media::env_threads("VANCE_CONVERT_THREADS")
}

impl FrameConverter {
    fn new(
        src: (ffmpeg::format::Pixel, u32, u32),
        target: PixelFormat,
        color: SwsColor,
    ) -> Result<Self, String> {
        Self::with_threads(src, target, color, conversion_threads())
    }

    fn with_threads(
        src: (ffmpeg::format::Pixel, u32, u32),
        target: PixelFormat,
        color: SwsColor,
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
        // on YUV input. FULL_CHR_H_INT is the same for RGB output: each
        // pixel gets its own interpolated chroma instead of its pair's.
        let mut flags = Flags::BILINEAR | Flags::FULL_CHR_H_INP;
        if target.is_rgba() {
            flags |= Flags::FULL_CHR_H_INT | Flags::ACCURATE_RND;
        }
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
        // Set explicitly, never left to swscale's defaults (BT.601, and a
        // range from the pixel format: a yuvj* source would be squeezed to
        // limited range). (src matrix, src full range, dst matrix, dst full).
        let bt709 = ffmpeg::sys::SWS_CS_ITU709;
        let (src_matrix, src_full, dst_matrix, dst_full) = if target.is_rgba() {
            // YUV / gray -> RGB with the stream's matrix and range (RGB
            // output is always full range; RGB -> RGB uses neither).
            (color.matrix, color.full_range, color.matrix, true)
        } else if is_rgb_like(format) {
            // RGB -> YUV: Rec.709 limited range, what the renderer assumes
            // for YUV input and what `media::wire_hints` reports.
            (bt709, false, bt709, false)
        } else {
            // Intentional: YUV -> YUV with the same matrix and range on both
            // sides, so swscale changes only chroma subsampling and bit
            // depth: the samples keep the source's matrix and range (incl.
            // full-range yuvj*), as natively carried formats do, and the
            // stream's hints still describe them.
            (
                color.matrix,
                color.full_range,
                color.matrix,
                color.full_range,
            )
        };
        // SAFETY: an initialised context; sws_getCoefficients returns a
        // static table for any value. The return value is ignored: swscale
        // returns -1 for a YUV->YUV context even after it has applied the
        // settings. (With slice threads it forwards them to every slice
        // context.)
        unsafe {
            ffmpeg::sys::sws_setColorspaceDetails(
                sws.0,
                ffmpeg::sys::sws_getCoefficients(src_matrix),
                c_int::from(src_full),
                ffmpeg::sys::sws_getCoefficients(dst_matrix),
                c_int::from(dst_full),
                0,
                1 << 16,
                1 << 16,
            );
        }
        eprintln!(
            "[decoder] converting {format:?} {width}x{height} -> {dst:?} (swscale, {threads} threads)"
        );
        Ok(Self {
            sws,
            src,
            color,
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
/// packets iterator) at the same time. `color`: how the stream's YUV is
/// encoded, for the converter.
pub fn materialize_frame(
    decoded: &ffmpeg::frame::Video,
    sw_dest_frame: Option<&mut ffmpeg::frame::Video>,
    converter: &mut Option<FrameConverter>,
    pixel_format: PixelFormat,
    color: SwsColor,
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
        let converted = convert_frame(converter, sw_frame, pixel_format, color)?;
        frame_to_payload(converted, pixel_format, alloc, wait)
    }
}

/// Run `frame` through the cached converter (building or rebuilding it if
/// the source format/size differs) and return the converted frame.
fn convert_frame<'c>(
    slot: &'c mut Option<FrameConverter>,
    frame: &ffmpeg::frame::Video,
    target: PixelFormat,
    color: SwsColor,
) -> Result<&'c ffmpeg::frame::Video, String> {
    let src = (frame.format(), frame.width(), frame.height());
    let color = color.for_frame(frame);
    let conv = match slot.take() {
        Some(c) if c.src == src && c.color == color => slot.insert(c),
        _ => slot.insert(FrameConverter::new(src, target, color)?),
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
        // Straight alpha, as every FFmpeg format is (PNG, ...).
        P::RGBA => Ok(PixelFormat::Rgba8),
        other => Err(format!("{other:?}")),
    }
}

fn wire_to_ffmpeg(pf: PixelFormat) -> ffmpeg::format::Pixel {
    use ffmpeg::format::Pixel as P;
    match pf {
        PixelFormat::Yuv420P => P::YUV420P,
        PixelFormat::Nv12 => P::NV12,
        PixelFormat::Yuv420P10Le => P::YUV420P10LE,
        PixelFormat::Rgba8 => P::RGBA,
    }
}

/// Pack `frame`'s planes back to back (padding stripped) into a buffer of
/// exactly the frame's size. `Ok(None)` when `wait` gave up on a buffer.
///
/// Rows are copied straight into the buffer, which is a shared-memory slot
/// whenever one is available: this is the frame's only copy on its way to
/// the parent.
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
        PixelFormat::Rgba8 => 1,
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
            let plane_width = if expected_pf == PixelFormat::Rgba8 {
                // One packed plane: 4 samples (R, G, B, A) per pixel.
                width * 4
            } else if plane == 0 {
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
            (P::YUVA444P12LE, PixelFormat::Rgba8),
            (P::YUVA420P, PixelFormat::Rgba8),
            (P::ARGB, PixelFormat::Rgba8),
            (P::RGBA64LE, PixelFormat::Rgba8),
            (P::YA8, PixelFormat::Rgba8),
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
                let color = SwsColor::default();
                let conv =
                    FrameConverter::with_threads((format, w, h), target, color, threads).unwrap();
                let mut slot = Some(conv);
                let out = convert_frame(&mut slot, &src, target, color).unwrap();
                packed_bytes(out, target)
            };
            assert!(
                run(1) == run(8),
                "{format:?} -> {target:?} differs when threaded"
            );
        }
    }

    /// One 2x1 frame of `format` with `planes` (one row each), converted.
    fn convert_one(format: P, planes: &[&[u8]], target: PixelFormat, color: SwsColor) -> Vec<u8> {
        let mut src = ffmpeg::frame::Video::new(format, 2, 1);
        for (plane, row) in planes.iter().enumerate() {
            src.data_mut(plane)[..row.len()].copy_from_slice(row);
        }
        let mut slot = None;
        let out = convert_frame(&mut slot, &src, target, color).unwrap();
        packed_bytes(out, target)
    }

    /// YUV -> RGBA reads the samples with the stream's matrix and range;
    /// YUV -> YUV keeps them as they are, full range included; RGB -> YUV is
    /// Rec.709 limited range whatever the stream's tags.
    #[test]
    fn conversions_use_the_streams_matrix_and_range() {
        use ffmpeg::sys::{SWS_CS_ITU601, SWS_CS_ITU709};
        let color = |full_range, matrix| SwsColor {
            full_range,
            matrix,
            range_tagged: true,
            matrix_tagged: true,
        };
        // BT.709 limited pure green (Y 173, Cb 42, Cr 26), opaque.
        let green = [&[173u8, 173][..], &[42, 42], &[26, 26], &[255, 255]];
        let rgba = convert_one(
            P::YUVA444P,
            &green,
            PixelFormat::Rgba8,
            color(false, SWS_CS_ITU709),
        );
        for px in rgba.chunks_exact(4) {
            assert!(
                px[0] <= 2 && px[1] >= 253 && px[2] <= 2,
                "BT.709 green: {px:?}"
            );
            assert_eq!(px[3], 255);
        }
        // Read as BT.601 the same samples give no green at full strength.
        let rgba = convert_one(
            P::YUVA444P,
            &green,
            PixelFormat::Rgba8,
            color(false, SWS_CS_ITU601),
        );
        assert!(
            rgba[0] >= 15,
            "BT.601 reading of BT.709 green: {:?}",
            &rgba[..4]
        );
        // Full range: Y 200 is white-ish 200, not stretched to 214.
        let grey = [&[200u8, 200][..], &[128, 128], &[128, 128], &[77, 77]];
        let rgba = convert_one(
            P::YUVA444P,
            &grey,
            PixelFormat::Rgba8,
            color(true, SWS_CS_ITU709),
        );
        assert!(
            rgba[..3].iter().all(|c| c.abs_diff(200) <= 1),
            "full range: {rgba:?}"
        );
        assert_eq!(rgba[3], 77, "alpha");
        let rgba = convert_one(
            P::YUVA444P,
            &grey,
            PixelFormat::Rgba8,
            color(false, SWS_CS_ITU709),
        );
        assert!(
            rgba[0].abs_diff(214) <= 1,
            "limited range: {:?}",
            &rgba[..4]
        );

        // A full-range 4:4:4 source keeps Y 250 in 4:2:0.
        let yuv = [&[250u8, 250][..], &[90, 90], &[200, 200]];
        let out = convert_one(
            P::YUVJ444P,
            &yuv,
            PixelFormat::Yuv420P,
            color(true, SWS_CS_ITU709),
        );
        assert_eq!(out, [250, 250, 90, 200]);
        let out = convert_one(
            P::YUV444P,
            &yuv,
            PixelFormat::Yuv420P,
            color(true, SWS_CS_ITU601),
        );
        assert_eq!(out, [250, 250, 90, 200]);

        // Pure red RGB -> BT.709 limited (Y 63, Cb 102, Cr 240) even for a
        // stream tagged full range / BT.601.
        let red = [&[255u8, 0, 0, 255, 0, 0][..]];
        let out = convert_one(
            P::RGB24,
            &red,
            PixelFormat::Yuv420P,
            color(true, SWS_CS_ITU601),
        );
        assert!(
            out[0].abs_diff(63) <= 1 && out[2].abs_diff(102) <= 1,
            "{out:?}"
        );
        assert!(out[3].abs_diff(240) <= 1, "{out:?}");
    }

    /// All-zero alpha is found in any alpha layout; one non-zero sample (the
    /// last of an odd-sized frame here) is enough to count as alpha.
    #[test]
    fn all_zero_alpha_is_detected_in_every_layout() {
        let (w, h) = (5, 3);
        for format in [P::RGBA, P::BGRA, P::ARGB, P::YUVA444P, P::YUVA420P, P::YA8] {
            let mut frame = ffmpeg::frame::Video::new(format, w, h);
            for plane in 0..frame.planes() {
                frame.data_mut(plane).fill(0);
            }
            let color = SwsColor::default();
            assert_eq!(
                alpha_is_zero(&frame, color),
                Ok(true),
                "{format:?} all zero"
            );
            // The alpha of the last pixel: its own plane, or its byte.
            let (plane, offset) = match format {
                P::RGBA | P::BGRA => (
                    0,
                    (h - 1) as usize * frame.stride(0) + (w - 1) as usize * 4 + 3,
                ),
                P::ARGB => (0, (h - 1) as usize * frame.stride(0) + (w - 1) as usize * 4),
                P::YA8 => (
                    0,
                    (h - 1) as usize * frame.stride(0) + (w - 1) as usize * 2 + 1,
                ),
                _ => (3, (h - 1) as usize * frame.stride(3) + (w - 1) as usize),
            };
            frame.data_mut(plane)[offset] = 1;
            assert_eq!(
                alpha_is_zero(&frame, color),
                Ok(false),
                "{format:?} one alpha sample"
            );
        }
    }

    /// Where the stream's tags say nothing, a frame's own count; where they
    /// say something, they win.
    #[test]
    fn a_frames_tags_fill_in_for_an_untagged_stream() {
        use ffmpeg::sys::AVColorRange::*;
        use ffmpeg::sys::AVColorSpace::*;
        use ffmpeg::sys::{SWS_CS_ITU601, SWS_CS_ITU709};
        let mut frame = ffmpeg::frame::Video::new(P::YUVA444P, 2, 1);
        // SAFETY: a frame we own.
        unsafe {
            (*frame.as_mut_ptr()).colorspace = AVCOL_SPC_SMPTE170M;
            (*frame.as_mut_ptr()).color_range = AVCOL_RANGE_JPEG;
        }
        let untagged = SwsColor::default().for_frame(&frame);
        assert_eq!(
            (untagged.matrix, untagged.full_range),
            (SWS_CS_ITU601, true)
        );
        let tagged = SwsColor {
            full_range: false,
            matrix: SWS_CS_ITU709,
            range_tagged: true,
            matrix_tagged: true,
        };
        assert_eq!(tagged.for_frame(&frame), tagged);
        let plain = ffmpeg::frame::Video::new(P::YUVA444P, 2, 1);
        assert_eq!(SwsColor::default().for_frame(&plain), SwsColor::default());
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
            (P::RGBA, PixelFormat::Rgba8, vec![161 * 4]),
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

    /// Packing into a shared-memory slot writes exactly the bytes the heap
    /// path sends inline, padding stripped.
    #[test]
    fn packing_into_a_slot_matches_packing_on_the_heap() {
        use std::sync::Arc;
        use vance_decoder::protocol::FramePayload;
        use vance_ipc::shm::{Access, Adopted, LeaseTable, ShmRegion, SLOT_ALIGN};

        let (w, h) = (161, 91);
        let largest = PixelFormat::Yuv420P10Le.frame_size(w, h) as u64;
        let slot_size = largest.div_ceil(SLOT_ALIGN) * SLOT_ALIGN;
        let region = Arc::new(ShmRegion::create(1, 1, slot_size, Access::ReadWrite).unwrap());
        let mut alloc = Allocator::default();
        alloc.attach(region.desc()).unwrap();
        let table = LeaseTable::new(Arc::clone(&region), Arc::default(), 1);

        for (format, wire) in [
            (P::YUV420P, PixelFormat::Yuv420P),
            (P::NV12, PixelFormat::Nv12),
            (P::YUV420P10LE, PixelFormat::Yuv420P10Le),
        ] {
            let mut frame = ffmpeg::frame::Video::new(format, w, h);
            for plane in 0..frame.planes() {
                let data = frame.data_mut(plane);
                for (i, b) in data.iter_mut().enumerate() {
                    *b = (i * 7 + plane * 31) as u8;
                }
            }
            let packed = frame_to_payload(&frame, wire, &alloc, Wait::Never)
                .unwrap()
                .unwrap();
            let r = match packed.buf.into_payload() {
                (FramePayload::Shared(r), None) => r,
                (other, _) => panic!("{format:?}: expected a slot, got {other:?}"),
            };
            assert_eq!(r.len as usize, wire.frame_size(w, h), "{format:?}");
            let lease = match table.adopt(r).unwrap() {
                Adopted::Leased(lease) => lease,
                other => panic!("{other:?}"),
            };
            assert!(
                lease[..] == packed_bytes(&frame, wire)[..],
                "{format:?}: slot bytes differ from the inline bytes"
            );
            drop(lease);
            assert_eq!(alloc.release(&[r]), 0);
        }
    }

    fn packed_bytes(frame: &ffmpeg::frame::Video, target: PixelFormat) -> Vec<u8> {
        let packed = frame_to_payload(frame, target, &Allocator::default(), Wait::Never)
            .unwrap()
            .unwrap();
        packed.buf.into_payload().1.unwrap()
    }
}
