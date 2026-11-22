//! Parent-side client for the `vance-audio-decoder` sidecar.

use crate::protocol::{AudioRequest, AudioResponse, AudioStreamInfo, PcmChunk, PROTOCOL_VERSION};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
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

/// Kills an audio decoder's sidecar from any thread, e.g. one stuck opening
/// a file on a hung network share while its client is blocked waiting for
/// it. Get one with [`AudioClient::kill_handle`]. The same design as the
/// video decoder's `SidecarKill` (vance-decoder): signalled by pid, never
/// after the client may have reaped the process.
#[derive(Clone)]
pub struct AudioKill(Arc<KillState>);

struct KillState {
    pid: Option<u32>,
    flags: Mutex<KillFlags>,
    /// Signalled when a kill in flight finishes.
    idle: Condvar,
}

#[derive(Default)]
struct KillFlags {
    /// The client may have reaped the process: its pid may be reused.
    reaped: bool,
    /// Killed through a handle (or a kill is under way).
    killed: bool,
    /// A kill is sending its signal now, without the lock.
    in_flight: bool,
    /// A kill through a handle has tried to signal the process.
    sent: bool,
}

impl AudioKill {
    /// Kill the sidecar now (SIGKILL / `TerminateProcess`): a request its
    /// client is blocked in returns promptly with an IPC error. Idempotent,
    /// and a no-op once the client has (or may have) reaped the process.
    pub fn kill(&self) {
        let pid = {
            let mut flags = self.flags();
            let Some(pid) = self.0.pid.filter(|_| !flags.reaped && !flags.killed) else {
                return;
            };
            flags.killed = true;
            flags.in_flight = true;
            flags.sent = true;
            pid
        };
        // Intentional: signalled without the lock, while `in_flight` keeps
        // the client from reaping the process (see `mark_reaped`), so the
        // pid can't have been handed to another process yet.
        let sent = force_kill(pid);
        let mut flags = self.flags();
        flags.in_flight = false;
        // A failed attempt may be retried.
        flags.killed = sent;
        drop(flags);
        self.0.idle.notify_all();
    }

    /// Whether a kill through this handle (or a clone) has signalled the
    /// sidecar. `false` after kills that came too late: the client had
    /// (or may have) reaped the process, whose pid may be another's by then.
    pub fn signalled(&self) -> bool {
        self.flags().sent
    }

    fn flags(&self) -> std::sync::MutexGuard<'_, KillFlags> {
        self.0.flags.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record that the client may reap the process now; first waits for a
    /// kill in flight.
    fn mark_reaped(&self) {
        let mut flags = self.flags();
        flags.reaped = true;
        while flags.in_flight {
            flags = self
                .0
                .idle
                .wait(flags)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

/// Kill process `pid`: `kill -KILL`, or `taskkill /F` (no console window).
/// Whether that worked.
fn force_kill(pid: u32) -> bool {
    let pid = pid.to_string();
    let mut cmd = if cfg!(windows) {
        let mut cmd = Command::new("taskkill");
        cmd.args(["/F", "/PID", &pid]);
        cmd
    } else {
        let mut cmd = Command::new("kill");
        cmd.args(["-KILL", &pid]);
        cmd
    };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    vance_ipc::process::status(&mut cmd).is_ok_and(|status| status.success())
}

/// Marks the sidecar reaped when the client drops, before its `Sidecar`
/// field (which kills and waits for the process) does.
struct ReapGuard(AudioKill);

impl Drop for ReapGuard {
    fn drop(&mut self) {
        self.0.mark_reaped();
    }
}

/// Parent-side audio decoder client. Owns one sidecar process.
pub struct AudioClient {
    /// Declared first, so it drops before `sidecar`.
    reap: ReapGuard,
    sidecar: Sidecar<AudioRequest, AudioResponse>,
    decoder_label: String,
}

impl AudioClient {
    /// Spawn the sidecar binary and complete the protocol handshake.
    pub fn spawn(binary: impl AsRef<Path>) -> Result<Self, AudioClientError> {
        let mut sidecar: Sidecar<AudioRequest, AudioResponse> =
            Sidecar::spawn(binary, std::iter::empty::<&str>())?;
        let reap = ReapGuard(AudioKill(Arc::new(KillState {
            pid: sidecar.pid(),
            flags: Mutex::default(),
            idle: Condvar::new(),
        })));
        let hello = sidecar.request(&AudioRequest::Hello {
            protocol_version: PROTOCOL_VERSION,
        });
        if hello.is_err() {
            // The sidecar is reaped as it drops below.
            reap.0.mark_reaped();
        }
        match hello? {
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
                    reap,
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

    /// A handle that kills this client's sidecar from any thread (see
    /// [`AudioKill`]).
    pub fn kill_handle(&self) -> AudioKill {
        self.reap.0.clone()
    }

    /// Decoder label reported by the sidecar (e.g. "symphonia 0.5").
    pub fn decoder_label(&self) -> &str {
        &self.decoder_label
    }

    /// OS PID of the underlying sidecar process. Use this when you
    /// need to signal the sidecar from another thread (the
    /// [`PeaksService`] cancel path uses it to unstick a worker
    /// blocked inside `next_chunk`).
    ///
    /// [`PeaksService`]: ../../vance-app/peaks/struct.PeaksService.html
    pub fn pid(&self) -> Option<u32> {
        self.sidecar.pid()
    }

    /// Send one request and wait for its reply. Every request after `Hello`
    /// goes through here.
    fn request(&mut self, request: &AudioRequest) -> Result<AudioResponse, AudioClientError> {
        let reply = self.sidecar.request(request);
        // After a failed exchange vance-ipc checks the child's exit status,
        // which reaps a process that has exited: its pid is no longer safe
        // to signal.
        if reply.is_err() {
            self.reap.0.mark_reaped();
        }
        Ok(reply?)
    }

    /// Open a media file's audio stream.
    pub fn open(&mut self, path: impl Into<String>) -> Result<AudioStreamInfo, AudioClientError> {
        match self.request(&AudioRequest::Open { path: path.into() })? {
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
        match self.request(&AudioRequest::StartStream {
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
        match self.request(&AudioRequest::NextChunk)? {
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
        match self.request(&AudioRequest::StopStream)? {
            AudioResponse::Stopped => Ok(()),
            other => Err(AudioClientError::UnexpectedResponse {
                expected: "Stopped",
                got: other,
            }),
        }
    }

    /// Close the open file.
    pub fn close(&mut self) -> Result<(), AudioClientError> {
        match self.request(&AudioRequest::Close)? {
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
        // The shutdown below waits for (reaps) the process.
        self.reap.0.mark_reaped();
        let _ = self.sidecar.shutdown();
        Ok(())
    }
}
