//! Phase 9.2 — cache-fronted decoder helper.
//!
//! Wraps `DecoderClient::decode_at` with a [`vance_cache::FrameCache`]
//! lookup. On a hit we skip the IPC + FFmpeg seek/decode round-trip
//! entirely; on a miss we decode, write the frame to the cache, and
//! return it.
//!
//! Used by scrub paths (`decode_sequence_at` and friends) where the
//! same PTS gets hit repeatedly during left-right jog. Streaming
//! playback bypasses this — its own pre-decode ring already keeps the
//! pipeline warm.
//!
//! ## On-wire frame format
//!
//! The cache stores a small header followed by raw pixel bytes:
//!
//! ```text
//!  0    u8       version = 2
//!  1    u8       pixel_format (0 = Yuv420P, 1 = Nv12, 2 = Yuv420P10Le,
//!                3 = Rgba8)
//!  2    u32 LE   width
//!  6    u32 LE   height
//! 10    i64 LE   actual PTS (may differ from the requested target)
//! 18    u8       colour range (0 = unknown, 1 = limited, 2 = full)
//! 19    u8       colour matrix (0 = unknown, 1 = BT.601, 2 = BT.709,
//!                3 = BT.2020 NCL)
//! 20    u32 LE   sample aspect numerator
//! 24    u32 LE   sample aspect denominator
//! 28    u8       stride_count
//! 29    [u32]*n  stride per plane
//! 29+4n …       pixel bytes
//! ```
//!
//! Version 1 entries (no colour hints) are ignored, so they re-decode.

use vance_cache::{CacheKey, FrameCache};
use vance_decoder::protocol::{ColorMatrixHint, ColorRangeHint, FrameData, PixelFormat};
use vance_decoder::{DecoderClient, FrameColor};

const CACHE_FORMAT_VERSION: u8 = 2;
/// Bytes before the stride table.
const HEADER_LEN: usize = 29;

fn range_code(r: ColorRangeHint) -> u8 {
    match r {
        ColorRangeHint::Unknown => 0,
        ColorRangeHint::Limited => 1,
        ColorRangeHint::Full => 2,
    }
}

fn range_from_code(c: u8) -> Option<ColorRangeHint> {
    Some(match c {
        0 => ColorRangeHint::Unknown,
        1 => ColorRangeHint::Limited,
        2 => ColorRangeHint::Full,
        _ => return None,
    })
}

fn matrix_code(m: ColorMatrixHint) -> u8 {
    match m {
        ColorMatrixHint::Unknown => 0,
        ColorMatrixHint::Bt601 => 1,
        ColorMatrixHint::Bt709 => 2,
        ColorMatrixHint::Bt2020Ncl => 3,
    }
}

fn matrix_from_code(c: u8) -> Option<ColorMatrixHint> {
    Some(match c {
        0 => ColorMatrixHint::Unknown,
        1 => ColorMatrixHint::Bt601,
        2 => ColorMatrixHint::Bt709,
        3 => ColorMatrixHint::Bt2020Ncl,
        _ => return None,
    })
}

/// Pack a [`FrameData`] into the cache's on-wire byte format.
pub fn pack_frame(frame: &FrameData) -> Vec<u8> {
    let stride_count = frame.strides.len();
    let header_len = HEADER_LEN + 4 * stride_count;
    let mut out = Vec::with_capacity(header_len + frame.bytes.len());
    out.push(CACHE_FORMAT_VERSION);
    out.push(match frame.pixel_format {
        PixelFormat::Yuv420P => 0,
        PixelFormat::Nv12 => 1,
        PixelFormat::Yuv420P10Le => 2,
        PixelFormat::Rgba8 => 3,
    });
    out.extend_from_slice(&frame.width.to_le_bytes());
    out.extend_from_slice(&frame.height.to_le_bytes());
    out.extend_from_slice(&frame.pts.to_le_bytes());
    out.push(range_code(frame.color.range));
    out.push(matrix_code(frame.color.matrix));
    out.extend_from_slice(&frame.color.sample_aspect.0.to_le_bytes());
    out.extend_from_slice(&frame.color.sample_aspect.1.to_le_bytes());
    out.push(stride_count as u8);
    for s in &frame.strides {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out.extend_from_slice(&frame.bytes);
    out
}

/// Reverse of [`pack_frame`]. Returns `None` on any header mismatch
/// so a corrupt cache entry just falls back to a fresh decode.
pub fn unpack_frame(buf: &[u8]) -> Option<FrameData> {
    if buf.len() < HEADER_LEN {
        return None;
    }
    if buf[0] != CACHE_FORMAT_VERSION {
        return None;
    }
    let pixel_format = match buf[1] {
        0 => PixelFormat::Yuv420P,
        1 => PixelFormat::Nv12,
        2 => PixelFormat::Yuv420P10Le,
        3 => PixelFormat::Rgba8,
        _ => return None,
    };
    let u32_at = |off: usize| buf[off..off + 4].try_into().ok().map(u32::from_le_bytes);
    let width = u32_at(2)?;
    let height = u32_at(6)?;
    let pts = i64::from_le_bytes(buf[10..18].try_into().ok()?);
    let color = FrameColor {
        range: range_from_code(buf[18])?,
        matrix: matrix_from_code(buf[19])?,
        sample_aspect: (u32_at(20)?, u32_at(24)?),
    };
    let stride_count = buf[28] as usize;
    let stride_table_end = HEADER_LEN + 4 * stride_count;
    if buf.len() < stride_table_end {
        return None;
    }
    let mut strides = Vec::with_capacity(stride_count);
    for i in 0..stride_count {
        strides.push(u32_at(HEADER_LEN + i * 4)?);
    }
    let bytes = buf[stride_table_end..].to_vec().into();
    Some(FrameData {
        pts,
        width,
        height,
        pixel_format,
        strides,
        bytes,
        color,
    })
}

/// Cache-fronted single-frame decode. Returns the decoded frame +
/// whether the cache served it.
#[tracing::instrument(skip(decoder, cache), fields(target_pts, hit))]
pub fn cached_decode_at(
    decoder: &mut DecoderClient,
    cache: &FrameCache,
    source_uri: &str,
    target_pts: i64,
) -> Result<(FrameData, bool), vance_decoder::client::ClientError> {
    let key = CacheKey::for_source_frame(source_uri, target_pts, 0);
    if let Some(buf) = cache.get(&key) {
        if let Some(frame) = unpack_frame(&buf) {
            return Ok((frame, true));
        }
    }
    let frame = decoder.decode_at(target_pts)?;
    let packed = pack_frame(&frame);
    let _ = cache.put(&key, &packed);
    Ok((frame, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame() -> FrameData {
        FrameData {
            pts: 12345,
            width: 16,
            height: 16,
            pixel_format: PixelFormat::Yuv420P,
            strides: vec![16, 8, 8],
            bytes: (0u8..=255)
                .cycle()
                .take(16 * 16 * 3 / 2)
                .collect::<Vec<u8>>()
                .into(),
            color: Default::default(),
        }
    }

    #[test]
    fn pack_unpack_round_trip_yuv420p() {
        let f = sample_frame();
        let packed = pack_frame(&f);
        let back = unpack_frame(&packed).expect("unpack");
        assert_eq!(back.pts, f.pts);
        assert_eq!(back.width, f.width);
        assert_eq!(back.height, f.height);
        assert_eq!(back.pixel_format, f.pixel_format);
        assert_eq!(back.strides, f.strides);
        assert_eq!(back.bytes, f.bytes);
    }

    #[test]
    fn pack_unpack_round_trip_nv12() {
        let mut f = sample_frame();
        f.pixel_format = PixelFormat::Nv12;
        f.strides = vec![16, 16];
        let packed = pack_frame(&f);
        let back = unpack_frame(&packed).expect("unpack");
        assert_eq!(back.pixel_format, f.pixel_format);
        assert_eq!(back.strides, f.strides);
    }

    #[test]
    fn colour_hints_round_trip() {
        let mut f = sample_frame();
        f.pixel_format = PixelFormat::Yuv420P10Le;
        f.color = FrameColor {
            range: ColorRangeHint::Full,
            matrix: ColorMatrixHint::Bt2020Ncl,
            sample_aspect: (4, 3),
        };
        let back = unpack_frame(&pack_frame(&f)).expect("unpack");
        assert_eq!(back.color, f.color);
        assert_eq!(back.pixel_format, f.pixel_format);
        assert_eq!(back.bytes, f.bytes);
        for (range, matrix) in [
            (ColorRangeHint::Unknown, ColorMatrixHint::Unknown),
            (ColorRangeHint::Limited, ColorMatrixHint::Bt601),
            (ColorRangeHint::Full, ColorMatrixHint::Bt709),
        ] {
            f.color.range = range;
            f.color.matrix = matrix;
            assert_eq!(unpack_frame(&pack_frame(&f)).unwrap().color, f.color);
        }
    }

    #[test]
    fn version_1_entries_are_ignored() {
        // A pre-hint entry (19-byte header, no colour fields) whose bytes
        // would also parse in the new layout: only the version tells them
        // apart.
        let f = sample_frame();
        let mut v1 = vec![1u8, 0];
        v1.extend_from_slice(&f.width.to_le_bytes());
        v1.extend_from_slice(&f.height.to_le_bytes());
        v1.extend_from_slice(&f.pts.to_le_bytes());
        v1.push(0); // no stride table
        v1.extend_from_slice(&[0u8; 64]);
        assert!(unpack_frame(&v1).is_none());
        v1[0] = CACHE_FORMAT_VERSION;
        assert!(unpack_frame(&v1).is_some(), "the layouts overlap");
    }

    #[test]
    fn unpack_rejects_unknown_colour_codes() {
        let mut packed = pack_frame(&sample_frame());
        packed[18] = 9;
        assert!(unpack_frame(&packed).is_none());
        let mut packed = pack_frame(&sample_frame());
        packed[19] = 9;
        assert!(unpack_frame(&packed).is_none());
    }

    #[test]
    fn unpack_rejects_wrong_version() {
        let mut packed = pack_frame(&sample_frame());
        packed[0] = 99; // bogus version
        assert!(unpack_frame(&packed).is_none());
    }

    #[test]
    fn unpack_rejects_short_header() {
        assert!(unpack_frame(&[1u8; 5]).is_none());
    }

    #[test]
    fn unpack_rejects_unknown_pixel_format() {
        let mut packed = pack_frame(&sample_frame());
        packed[1] = 77;
        assert!(unpack_frame(&packed).is_none());
    }
}
