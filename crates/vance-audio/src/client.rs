//! Parent-side client for the `vance-audio-decoder` sidecar.

use crate::protocol::{AudioRequest, AudioResponse, AudioStreamInfo, PcmChunk, PROTOCOL_VERSION};
use std::path::Path;
use thiserror::Error;
use vance_ipc::Sidecar;

/// Audio client errors.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum AudioClientError {
    /// IPC failure.
    #[error("audio ipc: {0}")]
    Ipc(#[from] vance_ipc::Error),

    /// Sidecar speaks a different protocol version.
    #[error("audio protocol mismatch: parent={parent} sidecar={sidecar}")]
    ProtocolMismatch {
        /// Parent's version.
        parent: u32,
        /// Sidecar's version.
        sidecar: u32,
    },

    /// Sidecar replied with an unexpected variant.
    #[error("audio protocol violation: expected {expected}, got {got:?}")]
    UnexpectedResponse {
        /// What we expected.
        expected: &'static str,
        /// What we got.
        got: AudioResponse,
    },

    /// Open failed.
    #[error("audio open failed: {0}")]
    Open(String),

    /// Decode failed mid-stream.
    #[error("audio decode failed: {0}")]
    Decode(String),
}

/// Parent-side audio decoder client. Owns one sidecar process.
pub struct AudioClient {
    sidecar: Sidecar<AudioRequest, AudioResponse>,
    decoder_label: String,
}

impl AudioClient {
    /// Spawn the sidecar binary and complete the protocol handshake.
    pub fn spawn(binary: impl AsRef<Path>) -> Result<Self, AudioClientError> {
        let mut sidecar: Sidecar<AudioRequest, AudioResponse> =
            Sidecar::spawn(binary, std::iter::empty::<&str>())?;
        match sidecar.request(&AudioRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        })? {
            AudioResponse::Hello {
                protocol_version,
                decoder,
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(AudioClientError::ProtocolMismatch {
                        parent: PROTOCOL_VERSION,
                        sidecar: protocol_version,
                    });
                }
                Ok(Self {
                    sidecar,
                    decoder_label: decoder,
                })
            }
            other => Err(AudioClientError::UnexpectedResponse {
                expected: "Hello",
                got: other,
            }),
        }
    }

    /// Decoder label reported by the sidecar (e.g. "symphonia 0.5").
    pub fn decoder_label(&self) -> &str {
        &self.decoder_label
    }

    /// Open a media file's audio stream.
    pub fn open(&mut self, path: impl Into<String>) -> Result<AudioStreamInfo, AudioClientError> {
        match self
            .sidecar
            .request(&AudioRequest::Open { path: path.into() })?
        {
            AudioResponse::Opened(info) => Ok(info),
            AudioResponse::OpenFailed(e) => Err(AudioClientError::Open(e)),
            other => Err(AudioClientError::UnexpectedResponse {
                expected: "Opened",
                got: other,
            }),
        }
    }

    /// Begin streaming PCM from `from_pts`.
    pub fn start_stream(
        &mut self,
        from_pts: i64,
        chunk_frames: u32,
    ) -> Result<i64, AudioClientError> {
        match self.sidecar.request(&AudioRequest::StartStream {
            from_pts,
            chunk_frames,
        })? {
            AudioResponse::Streaming { first_pts } => Ok(first_pts),
            AudioResponse::DecodeFailed { reason } => Err(AudioClientError::Decode(reason)),
            other => Err(AudioClientError::UnexpectedResponse {
                expected: "Streaming",
                got: other,
            }),
        }
    }

    /// Pull the next chunk. `Ok(None)` = clean end-of-stream.
    pub fn next_chunk(&mut self) -> Result<Option<PcmChunk>, AudioClientError> {
        match self.sidecar.request(&AudioRequest::NextChunk)? {
            AudioResponse::Chunk(c) => Ok(Some(c)),
            AudioResponse::EndOfStream => Ok(None),
            AudioResponse::DecodeFailed { reason } => Err(AudioClientError::Decode(reason)),
            other => Err(AudioClientError::UnexpectedResponse {
                expected: "Chunk|EndOfStream",
                got: other,
            }),
        }
    }

    /// Stop streaming. Sidecar stays alive.
    pub fn stop_stream(&mut self) -> Result<(), AudioClientError> {
        match self.sidecar.request(&AudioRequest::StopStream)? {
            AudioResponse::Stopped => Ok(()),
            other => Err(AudioClientError::UnexpectedResponse {
                expected: "Stopped",
                got: other,
            }),
        }
    }

    /// Close the open file.
    pub fn close(&mut self) -> Result<(), AudioClientError> {
        match self.sidecar.request(&AudioRequest::Close)? {
            AudioResponse::Closed => Ok(()),
            other => Err(AudioClientError::UnexpectedResponse {
                expected: "Closed",
                got: other,
            }),
        }
    }

    /// Graceful shutdown.
    pub fn shutdown(mut self) -> Result<(), AudioClientError> {
        let _ = self.sidecar.request(&AudioRequest::Shutdown);
        let _ = self.sidecar.shutdown();
        Ok(())
    }
}
