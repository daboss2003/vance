//! Decoder wire protocol.
//!
//! `DecoderRequest` flows parent → sidecar; `DecoderResponse` flows back.
//! Versioned by the `PROTOCOL_VERSION` constant. Bump on any breaking
//! change; older sidecars must reject newer requests cleanly.

use serde::{Deserialize, Serialize};

/// Wire protocol version. Bump on breaking changes.
///
/// History:
/// - v1: Phase 1 — Hello/Open/Seek/DecodeAt/Close/Shutdown.
/// - v2: Phase 2.1 — adds streaming (StartStream/NextFrame/StopStream)
///   and the `EndOfStream` response.
/// - v3: frames arrive as `Frame { meta, payload }`; inline pixels follow the
///   reply as one raw blob instead of a bincode byte array. Adds
///   `AttachShm` / `Release` for shared-memory frames (ADR 0006).
pub const PROTOCOL_VERSION: u32 = 3;

pub use crate::frame::FrameData;
pub use vance_ipc::shm::{ShmRegionDesc, SlotRef};

/// One request from parent to sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DecoderRequest {
    /// Handshake. Sidecar replies with [`DecoderResponse::Hello`].
    Hello {
        /// Parent's protocol version. Sidecar refuses on mismatch.
        protocol_version: u32,
    },

    /// Open a media file. Sidecar replies with [`DecoderResponse::Opened`]
    /// (containing stream info) or [`DecoderResponse::OpenFailed`].
    Open {
        /// Absolute or working-directory-relative path to the media file.
        path: String,
    },

    /// Seek the currently-open stream so the next decode starts at or
    /// before this PTS in the stream's time base. Returns [`DecoderResponse::Seeked`].
    Seek {
        /// Target PTS in the stream's time base units.
        target_pts: i64,
    },

    /// Decode the first frame whose PTS is at or after `target_pts`, in the
    /// stream's time base. The sidecar seeks to the keyframe before it and
    /// decodes forward; the response carries the actual PTS delivered.
    DecodeAt {
        /// Target PTS in the stream's time base.
        target_pts: i64,
    },

    /// Begin streaming decoded frames in PTS order starting at or after
    /// `from_pts`. The sidecar spawns a background decode thread that
    /// pre-fills a ring of up to `buffer_frames` frames; the parent then
    /// pulls them one at a time with [`DecoderRequest::NextFrame`].
    ///
    /// Replaces any in-progress stream. Reply: [`DecoderResponse::Streaming`].
    StartStream {
        /// Stream-time-base PTS to start streaming from.
        from_pts: i64,
        /// How many frames the sidecar may pre-decode ahead of the
        /// parent's last-pulled position. Higher = smoother playback,
        /// more sidecar memory. Phase 2.1 default: 8.
        buffer_frames: u32,
    },

    /// Pull the next decoded frame in PTS order. Blocks (on the sidecar
    /// side) until a frame is ready or end-of-stream is reached.
    ///
    /// Reply: [`DecoderResponse::Frame`], [`DecoderResponse::EndOfStream`],
    /// or [`DecoderResponse::DecodeFailed`].
    NextFrame,

    /// Stop the in-progress stream and tear down its background decode
    /// thread. Reply: [`DecoderResponse::Stopped`].
    StopStream,

    /// Close the currently-open file (if any). Sidecar stays alive,
    /// ready for another `Open`.
    Close,

    /// Graceful shutdown request. Equivalent to closing the parent's stdin
    /// from the sidecar's point of view, but lets the parent get a clean
    /// `Goodbye` response before EOF.
    Shutdown,

    /// Offer a shared-memory region for frame payloads. Implies
    /// `StopStream`. Reply: [`DecoderResponse::ShmAttached`] (frames may now
    /// arrive as [`FramePayload::Shared`]) or [`DecoderResponse::ShmUnavailable`]
    /// (they stay inline).
    AttachShm(ShmRegionDesc),

    /// Hand shared-memory slots back to the sidecar once the parent is done
    /// with them. Reply: [`DecoderResponse::Released`].
    Release {
        /// Slots to return. References to another region epoch are stale.
        slots: Vec<SlotRef>,
    },
}

/// One response from sidecar to parent. Always sent in reply to a request,
/// in the same order — this is strict request/response, not push.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DecoderResponse {
    /// Reply to [`DecoderRequest::Hello`].
    Hello {
        /// Sidecar's protocol version (must equal the parent's).
        protocol_version: u32,
        /// FFmpeg `av_version_info()` string, e.g. "7.1.3".
        ffmpeg_version: String,
    },

    /// File opened successfully. Carries the chosen video stream's info.
    Opened(StreamInfo),

    /// File open failed.
    OpenFailed(OpenError),

    /// Seek completed. The sidecar's internal state now points at or before `pts`.
    Seeked {
        /// PTS of the seek target as recorded by the sidecar.
        pts: i64,
    },

    /// A decoded frame. With an `Inline` payload exactly one raw blob of
    /// `len` bytes follows this reply on the stream.
    Frame {
        /// Everything about the frame except its pixels.
        meta: FrameMeta,
        /// Where the pixels are.
        payload: FramePayload,
    },

    /// `DecodeAt` failed (no frame at or near the requested PTS, decoder
    /// error, etc.).
    DecodeFailed {
        /// Human-readable reason; the sidecar logs more detail to stderr.
        reason: String,
    },

    /// `StartStream` acknowledged.
    Streaming {
        /// First PTS the sidecar will deliver via `NextFrame`. May be
        /// at or after the requested `from_pts` (the nearest keyframe
        /// available at that point).
        first_pts: i64,
    },

    /// The stream has been fully delivered; no more frames will arrive.
    /// The sidecar is still alive and ready for `StopStream` / `Seek` /
    /// new `StartStream` / `Close`.
    EndOfStream,

    /// `StopStream` acknowledged.
    Stopped,

    /// `Close` acknowledged. The sidecar has no open file.
    Closed,

    /// Graceful shutdown acknowledged; sidecar will exit immediately.
    Goodbye,

    /// `AttachShm` succeeded: the sidecar mapped the region of this epoch.
    ShmAttached {
        /// Epoch of the attached region.
        epoch: u32,
    },

    /// `AttachShm` failed; frames stay inline.
    ShmUnavailable {
        /// Why, for the log.
        reason: String,
    },

    /// `Release` processed.
    Released {
        /// How many of the slots were stale (unknown epoch, already
        /// released, or an older generation) and were ignored.
        stale: u32,
    },
}

/// Information about the chosen video stream after open. Returned in
/// [`DecoderResponse::Opened`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamInfo {
    /// Width of the video frame in pixels.
    pub width: u32,
    /// Height of the video frame in pixels.
    pub height: u32,
    /// Pixel format the decoder will deliver frames in.
    pub pixel_format: PixelFormat,
    /// Frame rate as `(num, den)`. May be the container's average rate.
    pub frame_rate: (i64, i64),
    /// Stream time base as `(num, den)`. PTS values are in this unit.
    pub time_base: (i64, i64),
    /// Duration in stream time-base units, if known.
    pub duration_pts: Option<i64>,
    /// Codec name, e.g. "h264", "hevc", "prores".
    pub codec_name: String,
    /// 0-based index of this stream within the container.
    pub stream_index: u32,
    /// Detected HDR transfer from the stream's color VUI, so the engine can
    /// tag an HDR source's input color space automatically. `#[serde(default)]`
    /// → `Unknown` for older sidecars.
    #[serde(default)]
    pub color_transfer: ColorTransferHint,
}

/// A coarse transfer-characteristic hint read from the stream's color VUI,
/// used to auto-tag a clip's input color space on import.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorTransferHint {
    /// Not signaled / a standard SDR transfer (Rec.709 / sRGB).
    #[default]
    Unknown,
    /// SMPTE ST 2084 (PQ) — HDR10.
    Pq,
    /// ARIB STD-B67 (HLG).
    Hlg,
}

/// Pixel formats the decoder emits over the wire.
///
/// Phase 1 ships planar 8-bit YUV (`Yuv420P`, the most common decoder
/// output) and semi-planar `Nv12` (what VideoToolbox returns from hardware
/// decode). Other formats get added as they're encountered; downstream
/// code (renderer, etc.) must handle every variant or fail loudly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    /// I420: Y plane, then half-resolution U and V planes (4:2:0).
    Yuv420P,
    /// NV12: Y plane, then interleaved UV plane at half resolution.
    Nv12,
    /// I420 with 10-bit samples stored as little-endian `u16` (2 bytes/
    /// sample, 10 valid low bits) — the wire format for a 10-bit source
    /// (HEVC Main10 / `yuv420p10le`, incl. HDR PQ/HLG). Same plane layout as
    /// `Yuv420P`, twice the bytes. Carried 10-bit end to end so HDR
    /// highlights survive to the render pipeline.
    Yuv420P10Le,
}

impl PixelFormat {
    /// Number of bytes for a frame of this format at `width` × `height`.
    pub fn frame_size(self, width: u32, height: u32) -> usize {
        // 4:2:0: a full-size luma plane plus two chroma planes (planar) or one
        // interleaved UV plane (NV12) of ceil(w/2) x ceil(h/2) samples each,
        // so odd sizes round chroma UP (w*h*3/2 would come out short).
        let luma = width as usize * height as usize;
        let chroma = width.div_ceil(2) as usize * height.div_ceil(2) as usize;
        (luma + 2 * chroma) * self.bytes_per_sample()
    }

    /// Bytes per sample on the wire (1 for 8-bit, 2 for 10/16-bit).
    pub fn bytes_per_sample(self) -> usize {
        match self {
            PixelFormat::Yuv420P | PixelFormat::Nv12 => 1,
            PixelFormat::Yuv420P10Le => 2,
        }
    }
}

/// A decoded frame's description, sent with [`DecoderResponse::Frame`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameMeta {
    /// PTS in the stream's time base.
    pub pts: i64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format (matches the open's `StreamInfo.pixel_format`).
    pub pixel_format: PixelFormat,
    /// Per-plane row lengths in samples (NV12: [Y, UV]; YUV420P: [Y, U, V]).
    pub strides: Vec<u32>,
}

/// Where a frame's pixel bytes (planes packed back to back, Y first) are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FramePayload {
    /// Exactly one raw blob of `len` bytes follows the reply.
    Inline {
        /// Byte count of the blob.
        len: u64,
    },
    /// In a shared-memory slot, owned by the parent until it releases it.
    Shared(SlotRef),
}

/// Reasons an `Open` can fail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OpenError {
    /// Path does not exist or is not readable.
    NotFound(String),
    /// Path opened but no video stream was found.
    NoVideoStream,
    /// Video codec is recognized but not supported by this build.
    UnsupportedCodec(String),
    /// Pixel format is recognized but not supported by this build.
    UnsupportedPixelFormat(String),
    /// FFmpeg returned an error during open / probe.
    FfmpegError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_format_size_1080p() {
        // 1920x1080 @ 4:2:0 = 1920*1080*1.5 = 3_110_400 bytes.
        assert_eq!(PixelFormat::Yuv420P.frame_size(1920, 1080), 3_110_400);
        assert_eq!(PixelFormat::Nv12.frame_size(1920, 1080), 3_110_400);
    }

    #[test]
    fn pixel_format_size_4k() {
        // 3840x2160 @ 4:2:0 = 12_441_600 bytes ~ 11.9 MiB.
        // Inline-payload-OK for now (vance-ipc cap is 16 MiB) but tight.
        assert_eq!(PixelFormat::Yuv420P.frame_size(3840, 2160), 12_441_600);
    }

    #[test]
    fn pixel_format_size_10bit() {
        // 10-bit is 2 bytes/sample → double the 8-bit size.
        assert_eq!(PixelFormat::Yuv420P10Le.frame_size(1920, 1080), 6_220_800);
        assert_eq!(PixelFormat::Yuv420P10Le.bytes_per_sample(), 2);
        assert_eq!(PixelFormat::Yuv420P.bytes_per_sample(), 1);
        // 4K 10-bit ≈ 23.7 MiB — exceeds the old 16 MiB IPC cap (raised).
        assert_eq!(PixelFormat::Yuv420P10Le.frame_size(3840, 2160), 24_883_200);
    }

    #[test]
    fn pixel_format_size_odd_dimensions() {
        // Chroma is ceil(w/2) x ceil(h/2): 161x91 → Y 14_651 + 2 × (81 × 46).
        assert_eq!(
            PixelFormat::Yuv420P.frame_size(161, 91),
            14_651 + 2 * 81 * 46
        );
        assert_eq!(PixelFormat::Nv12.frame_size(161, 91), 14_651 + 2 * 81 * 46);
        assert_eq!(
            PixelFormat::Yuv420P10Le.frame_size(161, 91),
            2 * (14_651 + 2 * 81 * 46)
        );
    }

    #[test]
    fn protocol_round_trips() {
        // Round-trip every variant through bincode to catch serde slip-ups
        // before they bite us across a process boundary.
        let cases = vec![
            DecoderRequest::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
            DecoderRequest::Open {
                path: "/tmp/x.mov".into(),
            },
            DecoderRequest::Seek { target_pts: 12345 },
            DecoderRequest::DecodeAt { target_pts: 12345 },
            DecoderRequest::StartStream {
                from_pts: 0,
                buffer_frames: 8,
            },
            DecoderRequest::NextFrame,
            DecoderRequest::StopStream,
            DecoderRequest::Close,
            DecoderRequest::Shutdown,
            DecoderRequest::AttachShm(ShmRegionDesc {
                epoch: 2,
                os_name: vance_ipc::shm::object_name(1, 2, 3),
                slot_count: 7,
                slot_size: 25 * 1024 * 1024,
            }),
            DecoderRequest::Release {
                slots: vec![SLOT, SlotRef { slot: 4, ..SLOT }],
            },
        ];
        for c in cases {
            let cfg = bincode::config::standard();
            let bytes = bincode::serde::encode_to_vec(&c, cfg).unwrap();
            let (back, _): (DecoderRequest, _) =
                bincode::serde::decode_from_slice(&bytes, cfg).unwrap();
            assert_eq!(format!("{c:?}"), format!("{back:?}"));
        }
    }

    const SLOT: SlotRef = SlotRef {
        epoch: 2,
        slot: 3,
        generation: 9,
        len: 1234,
    };

    #[test]
    fn response_round_trips() {
        let meta = FrameMeta {
            pts: -5,
            width: 161,
            height: 91,
            pixel_format: PixelFormat::Yuv420P10Le,
            strides: vec![161, 81, 81],
        };
        let cases = vec![
            DecoderResponse::Hello {
                protocol_version: PROTOCOL_VERSION,
                ffmpeg_version: "7.1".into(),
            },
            DecoderResponse::OpenFailed(OpenError::NoVideoStream),
            DecoderResponse::Seeked { pts: 7 },
            DecoderResponse::Frame {
                meta: meta.clone(),
                payload: FramePayload::Inline { len: 40_000 },
            },
            DecoderResponse::Frame {
                meta,
                payload: FramePayload::Shared(SLOT),
            },
            DecoderResponse::DecodeFailed { reason: "x".into() },
            DecoderResponse::Streaming { first_pts: 3 },
            DecoderResponse::EndOfStream,
            DecoderResponse::Stopped,
            DecoderResponse::Closed,
            DecoderResponse::Goodbye,
            DecoderResponse::ShmAttached { epoch: 2 },
            DecoderResponse::ShmUnavailable { reason: "y".into() },
            DecoderResponse::Released { stale: 1 },
        ];
        for c in cases {
            let cfg = bincode::config::standard();
            let bytes = bincode::serde::encode_to_vec(&c, cfg).unwrap();
            let (back, _): (DecoderResponse, _) =
                bincode::serde::decode_from_slice(&bytes, cfg).unwrap();
            assert_eq!(format!("{c:?}"), format!("{back:?}"));
        }
    }
}
