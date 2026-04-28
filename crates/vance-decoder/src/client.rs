//! Parent-side client over the [`crate::protocol`].
//!
//! Wraps `vance_ipc::Sidecar` with decoder-specific handshake and ergonomic
//! per-call methods that return `Result<T, ClientError>` instead of bare
//! protocol enums.

use crate::protocol::{
    DecoderRequest, DecoderResponse, FrameData, OpenError, StreamInfo, PROTOCOL_VERSION,
};
use std::path::Path;
use thiserror::Error;
use vance_ipc::Sidecar;

/// Client-side errors.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum ClientError {
    /// Underlying IPC failed (sidecar died, framing broken, etc.).
    #[error("decoder ipc: {0}")]
    Ipc(#[from] vance_ipc::Error),

    /// Sidecar speaks a different protocol version.
    #[error("decoder protocol mismatch: parent={parent} sidecar={sidecar}")]
    ProtocolMismatch {
        /// Version the parent (this code) speaks.
        parent: u32,
        /// Version the sidecar speaks.
        sidecar: u32,
    },

    /// Sidecar replied with a response that doesn't match the request.
    #[error("decoder protocol violation: expected {expected}, got {got:?}")]
    UnexpectedResponse {
        /// Human-readable name of the response we expected.
        expected: &'static str,
        /// What we actually received.
        got: DecoderResponse,
    },

    /// Open failed for a domain reason (file not found, codec unsupported, ...).
    #[error("decoder open failed: {0:?}")]
    Open(OpenError),

    /// Decode call failed (bad PTS, decoder error, ...).
    #[error("decoder decode failed: {0}")]
    Decode(String),
}

/// Parent-side decoder client. Owns one sidecar process.
pub struct DecoderClient {
    sidecar: Sidecar<DecoderRequest, DecoderResponse>,
    ffmpeg_version: String,
}

impl DecoderClient {
    /// Spawn the sidecar binary and complete the protocol handshake.
    pub fn spawn(binary: impl AsRef<Path>) -> Result<Self, ClientError> {
        let mut sidecar: Sidecar<DecoderRequest, DecoderResponse> =
            Sidecar::spawn(binary, std::iter::empty::<&str>())?;
        let resp = sidecar.request(&DecoderRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        })?;
        match resp {
            DecoderResponse::Hello {
                protocol_version,
                ffmpeg_version,
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(ClientError::ProtocolMismatch {
                        parent: PROTOCOL_VERSION,
                        sidecar: protocol_version,
                    });
                }
                Ok(Self {
                    sidecar,
                    ffmpeg_version,
                })
            }
            other => Err(ClientError::UnexpectedResponse {
                expected: "Hello",
                got: other,
            }),
        }
    }

    /// FFmpeg version string reported by the sidecar.
    pub fn ffmpeg_version(&self) -> &str {
        &self.ffmpeg_version
    }

    /// Open a media file. Returns the chosen video stream's info.
    pub fn open(&mut self, path: impl Into<String>) -> Result<StreamInfo, ClientError> {
        match self
            .sidecar
            .request(&DecoderRequest::Open { path: path.into() })?
        {
            DecoderResponse::Opened(info) => Ok(info),
            DecoderResponse::OpenFailed(e) => Err(ClientError::Open(e)),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Opened",
                got: other,
            }),
        }
    }

    /// Seek the open stream so the next decode lands at or before `target_pts`.
    pub fn seek(&mut self, target_pts: i64) -> Result<i64, ClientError> {
        match self.sidecar.request(&DecoderRequest::Seek { target_pts })? {
            DecoderResponse::Seeked { pts } => Ok(pts),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Seeked",
                got: other,
            }),
        }
    }

    /// Decode the frame at or before `target_pts` and return its pixel data.
    pub fn decode_at(&mut self, target_pts: i64) -> Result<FrameData, ClientError> {
        match self
            .sidecar
            .request(&DecoderRequest::DecodeAt { target_pts })?
        {
            DecoderResponse::Frame(f) => Ok(f),
            DecoderResponse::DecodeFailed { reason } => Err(ClientError::Decode(reason)),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Frame",
                got: other,
            }),
        }
    }

    /// Begin streaming decoded frames in PTS order from `from_pts`.
    /// `buffer_frames` is the sidecar's pre-decode-ahead window
    /// (capped at 64). Returns the first PTS the sidecar will deliver.
    pub fn start_stream(&mut self, from_pts: i64, buffer_frames: u32) -> Result<i64, ClientError> {
        match self.sidecar.request(&DecoderRequest::StartStream {
            from_pts,
            buffer_frames,
        })? {
            DecoderResponse::Streaming { first_pts } => Ok(first_pts),
            DecoderResponse::DecodeFailed { reason } => Err(ClientError::Decode(reason)),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Streaming",
                got: other,
            }),
        }
    }

    /// Pull the next streamed frame. Returns `Ok(None)` on clean
    /// end-of-stream, `Err` on decode failure / sidecar death.
    pub fn next_frame(&mut self) -> Result<Option<FrameData>, ClientError> {
        match self.sidecar.request(&DecoderRequest::NextFrame)? {
            DecoderResponse::Frame(f) => Ok(Some(f)),
            DecoderResponse::EndOfStream => Ok(None),
            DecoderResponse::DecodeFailed { reason } => Err(ClientError::Decode(reason)),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Frame|EndOfStream",
                got: other,
            }),
        }
    }

    /// Tear down the in-progress stream. Sidecar stays alive.
    pub fn stop_stream(&mut self) -> Result<(), ClientError> {
        match self.sidecar.request(&DecoderRequest::StopStream)? {
            DecoderResponse::Stopped => Ok(()),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Stopped",
                got: other,
            }),
        }
    }

    /// Close the currently-open file. The sidecar stays alive.
    pub fn close(&mut self) -> Result<(), ClientError> {
        match self.sidecar.request(&DecoderRequest::Close)? {
            DecoderResponse::Closed => Ok(()),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Closed",
                got: other,
            }),
        }
    }

    /// Send `Shutdown`, wait for `Goodbye`, then wait for the child to exit.
    pub fn shutdown(mut self) -> Result<(), ClientError> {
        let _ = self.sidecar.request(&DecoderRequest::Shutdown);
        // Sidecar may have already exited after replying; that's fine.
        let _ = self.sidecar.shutdown();
        Ok(())
    }

    /// Force-kill the sidecar process. Test/diagnostic only — production
    /// code should use [`DecoderClient::shutdown`] for graceful exit.
    /// Use to validate the crash-isolation contract (PLAN.md §3 principle 4).
    pub fn kill(&mut self) -> Result<(), ClientError> {
        Ok(self.sidecar.kill()?)
    }
}
