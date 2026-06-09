//! Parent-side encoder client.

use crate::protocol::{
    AudioConfig, EncoderError, EncoderPreset, EncoderRequest, EncoderResponse, VideoConfig,
    PROTOCOL_VERSION,
};
use std::path::Path;
use thiserror::Error;
use vance_ipc::Sidecar;

/// Client-side errors.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum EncoderClientError {
    /// Underlying IPC failed.
    #[error("encoder ipc: {0}")]
    Ipc(#[from] vance_ipc::Error),

    /// Sidecar speaks a different protocol version.
    #[error("encoder protocol mismatch: parent={parent} sidecar={sidecar}")]
    ProtocolMismatch {
        /// Parent's version.
        parent: u32,
        /// Sidecar's version.
        sidecar: u32,
    },

    /// Sidecar replied with an unexpected variant.
    #[error("encoder protocol violation: expected {expected}, got {got:?}")]
    UnexpectedResponse {
        /// What we expected.
        expected: &'static str,
        /// What we got.
        got: EncoderResponse,
    },

    /// Open or encode failed domain-side.
    #[error("encoder failed: {0}")]
    Encoder(EncoderError),
}

/// Parent-side encoder client. Owns one sidecar process.
pub struct EncoderClient {
    sidecar: Sidecar<EncoderRequest, EncoderResponse>,
    ffmpeg_version: String,
    available_presets: Vec<EncoderPreset>,
}

impl EncoderClient {
    /// Spawn the sidecar binary and complete the handshake.
    pub fn spawn(binary: impl AsRef<Path>) -> Result<Self, EncoderClientError> {
        let mut sidecar: Sidecar<EncoderRequest, EncoderResponse> =
            Sidecar::spawn(binary, std::iter::empty::<&str>())?;
        match sidecar.request(&EncoderRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        })? {
            EncoderResponse::Hello {
                protocol_version,
                ffmpeg_version,
                available_presets,
            } => {
                if protocol_version != PROTOCOL_VERSION {
                    return Err(EncoderClientError::ProtocolMismatch {
                        parent: PROTOCOL_VERSION,
                        sidecar: protocol_version,
                    });
                }
                Ok(Self {
                    sidecar,
                    ffmpeg_version,
                    available_presets,
                })
            }
            other => Err(EncoderClientError::UnexpectedResponse {
                expected: "Hello",
                got: other,
            }),
        }
    }

    /// FFmpeg version string the sidecar reported on connect.
    pub fn ffmpeg_version(&self) -> &str {
        &self.ffmpeg_version
    }

    /// Which presets the sidecar can actually run on this system.
    pub fn available_presets(&self) -> &[EncoderPreset] {
        &self.available_presets
    }

    /// Open the output file + configure the encoder streams. Must be
    /// followed by `push_video` / `push_audio` calls + a single
    /// `finish`.
    pub fn open(
        &mut self,
        path: impl Into<String>,
        video: Option<VideoConfig>,
        audio: Option<AudioConfig>,
    ) -> Result<(), EncoderClientError> {
        match self.sidecar.request(&EncoderRequest::Open {
            path: path.into(),
            video,
            audio,
        })? {
            EncoderResponse::Opened => Ok(()),
            EncoderResponse::OpenFailed(e) => Err(EncoderClientError::Encoder(e)),
            other => Err(EncoderClientError::UnexpectedResponse {
                expected: "Opened",
                got: other,
            }),
        }
    }

    /// Push one decoded video frame. `pixels` length must equal
    /// `width * height * 4` (RGBA/BGRA).
    pub fn push_video(&mut self, pts: i64, pixels: Vec<u8>) -> Result<(), EncoderClientError> {
        match self
            .sidecar
            .request(&EncoderRequest::PushVideoFrame { pts, pixels })?
        {
            EncoderResponse::FramePushed => Ok(()),
            EncoderResponse::EncodeFailed(e) => Err(EncoderClientError::Encoder(e)),
            other => Err(EncoderClientError::UnexpectedResponse {
                expected: "FramePushed",
                got: other,
            }),
        }
    }

    /// Push one chunk of interleaved stereo f32 audio.
    pub fn push_audio(&mut self, pts: i64, samples: Vec<f32>) -> Result<(), EncoderClientError> {
        match self
            .sidecar
            .request(&EncoderRequest::PushAudioFrame { pts, samples })?
        {
            EncoderResponse::FramePushed => Ok(()),
            EncoderResponse::EncodeFailed(e) => Err(EncoderClientError::Encoder(e)),
            other => Err(EncoderClientError::UnexpectedResponse {
                expected: "FramePushed",
                got: other,
            }),
        }
    }

    /// Finish + flush. Returns `(video_frames, audio_samples)` totals.
    pub fn finish(&mut self) -> Result<(u64, u64), EncoderClientError> {
        match self.sidecar.request(&EncoderRequest::Finish)? {
            EncoderResponse::Finished {
                video_frames,
                audio_samples,
            } => Ok((video_frames, audio_samples)),
            EncoderResponse::EncodeFailed(e) => Err(EncoderClientError::Encoder(e)),
            other => Err(EncoderClientError::UnexpectedResponse {
                expected: "Finished",
                got: other,
            }),
        }
    }

    /// Graceful shutdown of the entire sidecar.
    pub fn shutdown(mut self) -> Result<(), EncoderClientError> {
        let _ = self.sidecar.request(&EncoderRequest::Shutdown);
        let _ = self.sidecar.shutdown();
        Ok(())
    }
}
