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
//!  0    u8       version = 1
//!  1    u8       pixel_format (0 = Yuv420P, 1 = Nv12)
//!  2    u32 LE   width
//!  6    u32 LE   height
//! 10    i64 LE   actual PTS (may differ from the requested target)
//! 18    u8       stride_count
//! 19    [u32]*n  stride per plane
//! 19+4n …       pixel bytes
//! ```

use vance_cache::{CacheKey, FrameCache};
use vance_decoder::protocol::{FrameData, PixelFormat};
use vance_decoder::DecoderClient;

const CACHE_FORMAT_VERSION: u8 = 1;

/// Pack a [`FrameData`] into the cache's on-wire byte format.
pub fn pack_frame(frame: &FrameData) -> Vec<u8> {
    let stride_count = frame.strides.len();
    let header_len = 19 + 4 * stride_count;
    let mut out = Vec::with_capacity(header_len + frame.bytes.len());
    out.push(CACHE_FORMAT_VERSION);
    out.push(match frame.pixel_format {
        PixelFormat::Yuv420P => 0,
        PixelFormat::Nv12 => 1,
        PixelFormat::Yuv420P10Le => 2,
    });
    out.extend_from_slice(&frame.width.to_le_bytes());
    out.extend_from_slice(&frame.height.to_le_bytes());
    out.extend_from_slice(&frame.pts.to_le_bytes());
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
    if buf.len() < 19 {
        return None;
    }
    if buf[0] != CACHE_FORMAT_VERSION {
        return None;
    }
    let pixel_format = match buf[1] {
        0 => PixelFormat::Yuv420P,
        1 => PixelFormat::Nv12,
        2 => PixelFormat::Yuv420P10Le,
        _ => return None,
    };
    let width = u32::from_le_bytes(buf[2..6].try_into().ok()?);
    let height = u32::from_le_bytes(buf[6..10].try_into().ok()?);
    let pts = i64::from_le_bytes(buf[10..18].try_into().ok()?);
    let stride_count = buf[18] as usize;
    let stride_table_end = 19 + 4 * stride_count;
    if buf.len() < stride_table_end {
        return None;
    }
    let mut strides = Vec::with_capacity(stride_count);
    for i in 0..stride_count {
        let off = 19 + i * 4;
        strides.push(u32::from_le_bytes(buf[off..off + 4].try_into().ok()?));
    }
    let bytes = buf[stride_table_end..].to_vec().into();
    Some(FrameData {
        pts,
        width,
        height,
        pixel_format,
        strides,
        bytes,
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
    use vance_decoder::protocol::{FrameData, PixelFormat};

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
