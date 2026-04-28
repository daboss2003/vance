//! Audio decoder wire protocol (separate from `vance-decoder`'s video protocol).

use serde::{Deserialize, Serialize};

/// Wire protocol version. Bump on breaking changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Parent → sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioRequest {
    /// Handshake.
    Hello {
        /// Parent's protocol version.
        protocol_version: u32,
    },
    /// Open a media file's audio stream.
    Open {
        /// Absolute or relative path to the media file.
        path: String,
    },
    /// Begin streaming PCM in PTS order from `from_pts` (in stream time base).
    /// `chunk_frames` controls how many audio frames (samples per channel)
    /// each `PcmChunk` contains.
    StartStream {
        /// PTS in audio stream's time base to start at.
        from_pts: i64,
        /// Frames-per-chunk hint. The sidecar may return fewer at
        /// chunk boundaries.
        chunk_frames: u32,
    },
    /// Pull the next PCM chunk. Blocks until ready / EOS.
    NextChunk,
    /// Stop streaming. Sidecar stays alive.
    StopStream,
    /// Close the file. Sidecar stays alive.
    Close,
    /// Graceful shutdown.
    Shutdown,
}

/// Sidecar → parent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AudioResponse {
    /// Reply to Hello.
    Hello {
        /// Sidecar's protocol version (must equal parent's).
        protocol_version: u32,
        /// "symphonia 0.5" or similar.
        decoder: String,
    },
    /// File opened OK.
    Opened(AudioStreamInfo),
    /// Open failed.
    OpenFailed(String),
    /// StartStream acknowledged.
    Streaming {
        /// First PTS the sidecar will deliver.
        first_pts: i64,
    },
    /// One PCM chunk in interleaved-stereo f32 at the stream's native rate.
    Chunk(PcmChunk),
    /// Stream exhausted.
    EndOfStream,
    /// Decode error mid-stream.
    DecodeFailed {
        /// Human-readable reason.
        reason: String,
    },
    /// StopStream acknowledged.
    Stopped,
    /// Close acknowledged.
    Closed,
    /// Shutdown acknowledged.
    Goodbye,
}

/// Information about the chosen audio stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioStreamInfo {
    /// Sample rate in Hz (e.g. 48000).
    pub sample_rate: u32,
    /// Channel count (we always *output* stereo; mono inputs are duplicated).
    pub channels: u16,
    /// Stream time base as `(num, den)`. PTS values are in this unit.
    pub time_base: (i64, i64),
    /// Codec name, e.g. "aac", "mp3".
    pub codec_name: String,
    /// Duration in stream time base, if known.
    pub duration_pts: Option<i64>,
}

/// One chunk of decoded interleaved-stereo f32 PCM at the stream's native rate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PcmChunk {
    /// PTS of the first sample in the chunk (stream time base).
    pub pts: i64,
    /// Interleaved stereo samples (`[L0, R0, L1, R1, ...]`). Length is
    /// always even.
    pub samples: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_via_bincode() {
        let cases = vec![
            AudioRequest::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
            AudioRequest::Open {
                path: "/tmp/x.mp3".into(),
            },
            AudioRequest::StartStream {
                from_pts: 0,
                chunk_frames: 1024,
            },
            AudioRequest::NextChunk,
            AudioRequest::StopStream,
            AudioRequest::Close,
            AudioRequest::Shutdown,
        ];
        let cfg = bincode::config::standard();
        for c in cases {
            let bytes = bincode::serde::encode_to_vec(&c, cfg).unwrap();
            let (back, _): (AudioRequest, _) =
                bincode::serde::decode_from_slice(&bytes, cfg).unwrap();
            assert_eq!(format!("{c:?}"), format!("{back:?}"));
        }
    }
}
