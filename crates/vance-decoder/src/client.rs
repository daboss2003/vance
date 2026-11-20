//! Parent-side client over the [`crate::protocol`].
//!
//! Wraps `vance_ipc::Sidecar` with decoder-specific handshake and ergonomic
//! per-call methods that return `Result<T, ClientError>` instead of bare
//! protocol enums.

use crate::frame::FrameData;
use crate::protocol::{
    DecoderRequest, DecoderResponse, FrameMeta, FramePayload, OpenError, StreamInfo,
    PROTOCOL_VERSION,
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

/// How a [`DecoderClient`] moves frames (see ADR 0006).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientOptions {
    /// Use shared memory for frames where possible. Frames arrive inline
    /// (a raw blob after each reply) when this is off and whenever shared
    /// memory can't be set up. Default `true`.
    pub shm: bool,
    /// Frames the caller may hold as zero-copy leases at once; frames past
    /// that are copied out. Default 2.
    pub max_leases: usize,
    /// Slots for the sidecar's decode-ahead ring. A region holds
    /// `ring_slots + max_leases + 1` slots. Default 4.
    pub ring_slots: u32,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            shm: true,
            max_leases: 2,
            ring_slots: 4,
        }
    }
}

/// Frame-transport counters (for tests, diagnostics and the engine's
/// memory budget).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransportStats {
    /// Epoch of the shared-memory region in use, if any.
    pub shm_epoch: Option<u32>,
    /// Bytes of shared memory mapped for this client's frames.
    pub shm_bytes: u64,
    /// Frames received through shared memory.
    pub shared: u64,
    /// Frames received inline, as a raw blob after the reply.
    pub inline: u64,
    /// Shared-memory frames copied out because the lease budget was full.
    pub spilled: u64,
}

/// Parent-side decoder client. Owns one sidecar process.
pub struct DecoderClient {
    sidecar: Sidecar<DecoderRequest, DecoderResponse>,
    ffmpeg_version: String,
    #[expect(
        dead_code,
        reason = "read once the shared-memory handshake exists (ADR 0006); frames are inline-only until then"
    )]
    options: ClientOptions,
    stats: TransportStats,
}

impl DecoderClient {
    /// Spawn the sidecar binary and complete the protocol handshake.
    pub fn spawn(binary: impl AsRef<Path>) -> Result<Self, ClientError> {
        Self::spawn_with_env(binary, std::iter::empty::<(&str, &str)>())
    }

    /// Spawn the sidecar with extra environment variables (e.g. `VANCE_FORCE_SW=1`
    /// to disable hardware decode for testing). Otherwise identical to `spawn`.
    pub fn spawn_with_env<P, I, K, V>(binary: P, env: I) -> Result<Self, ClientError>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        Self::spawn_with_options(binary, env, ClientOptions::default())
    }

    /// Spawn the sidecar with extra environment variables and explicit
    /// transport options. `spawn` / `spawn_with_env` use the defaults.
    pub fn spawn_with_options<P, I, K, V>(
        binary: P,
        env: I,
        options: ClientOptions,
    ) -> Result<Self, ClientError>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        let mut sidecar: Sidecar<DecoderRequest, DecoderResponse> =
            Sidecar::spawn_with_env(binary, std::iter::empty::<&str>(), env)?;
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
                    options,
                    stats: TransportStats::default(),
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

    /// Frame-transport counters since spawn.
    pub fn transport_stats(&self) -> TransportStats {
        self.stats
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

    /// Decode the first frame at or after `target_pts` and return it.
    pub fn decode_at(&mut self, target_pts: i64) -> Result<FrameData, ClientError> {
        match self
            .sidecar
            .request(&DecoderRequest::DecodeAt { target_pts })?
        {
            DecoderResponse::Frame { meta, payload } => self.receive_frame(meta, payload),
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
            DecoderResponse::Frame { meta, payload } => self.receive_frame(meta, payload).map(Some),
            DecoderResponse::EndOfStream => Ok(None),
            DecoderResponse::DecodeFailed { reason } => Err(ClientError::Decode(reason)),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Frame|EndOfStream",
                got: other,
            }),
        }
    }

    /// Collect the pixels of a `Frame` reply.
    fn receive_frame(
        &mut self,
        meta: FrameMeta,
        payload: FramePayload,
    ) -> Result<FrameData, ClientError> {
        let bytes = match payload {
            FramePayload::Inline { len } => {
                let bytes = self.sidecar.recv_blob(len)?;
                self.stats.inline += 1;
                bytes.into()
            }
            // No region was ever offered, so a shared payload is a protocol
            // violation (and carries no blob, so the stream stays in sync).
            FramePayload::Shared(_) => {
                return Err(ClientError::UnexpectedResponse {
                    expected: "Frame with an inline payload",
                    got: DecoderResponse::Frame { meta, payload },
                })
            }
        };
        Ok(FrameData {
            pts: meta.pts,
            width: meta.width,
            height: meta.height,
            pixel_format: meta.pixel_format,
            strides: meta.strides,
            bytes,
        })
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
