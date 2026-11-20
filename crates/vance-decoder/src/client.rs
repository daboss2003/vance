//! Parent-side client over the [`crate::protocol`].
//!
//! Wraps `vance_ipc::Sidecar` with decoder-specific handshake and ergonomic
//! per-call methods that return `Result<T, ClientError>` instead of bare
//! protocol enums.
//!
//! ## Frame transport (ADR 0006)
//!
//! After each `Open` the client offers the sidecar a shared-memory region
//! sized for the stream's frames, unless the attached one is already big
//! enough (regions only grow). Frames then arrive as slots of that region
//! and are handed out as zero-copy leases, at most
//! [`ClientOptions::max_leases`] at a time; past that they are copied out.
//! Ended leases queue their slots, and every request first hands the queued
//! slots back with a `Release`. Without a region (shared memory off, or any
//! shared-memory failure) frames arrive inline, as a raw blob after the
//! reply.
//!
//! Two environment variables, read from the sidecar's environment (the
//! `env` given to [`DecoderClient::spawn_with_env`], else this process's):
//! `VANCE_IPC_SHM=0` keeps every frame inline; `VANCE_IPC_SHM_SLOT_BYTES=n`
//! (tests only) caps the slot size at `n` bytes, so frames bigger than that
//! go inline.

use crate::frame::{FrameBytes, FrameData};
use crate::protocol::{
    DecoderRequest, DecoderResponse, FrameMeta, FramePayload, OpenError, StreamInfo,
    PROTOCOL_VERSION,
};
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;
use vance_ipc::shm::{self, Access, Adopted, LeaseTable, ReleaseQueue, ShmRegion, SLOT_ALIGN};
use vance_ipc::Sidecar;

/// `VANCE_IPC_SHM=0` keeps every frame inline.
const SHM_ENV: &str = "VANCE_IPC_SHM";
/// Test-only cap on the slot size, in bytes.
const SLOT_BYTES_ENV: &str = "VANCE_IPC_SHM_SLOT_BYTES";

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
    /// Byte length of the shared-memory region in use (0 without one). A
    /// replaced region stays mapped only while leases on it are alive.
    pub shm_bytes: u64,
    /// Frames received through shared memory as zero-copy leases.
    pub shared: u64,
    /// Frames received inline, as a raw blob after the reply.
    pub inline: u64,
    /// Shared-memory frames copied out because the lease budget was full
    /// (not counted in `shared`).
    pub spilled: u64,
}

/// Parent-side decoder client. Owns one sidecar process.
pub struct DecoderClient {
    sidecar: Sidecar<DecoderRequest, DecoderResponse>,
    ffmpeg_version: String,
    options: ClientOptions,
    stats: TransportStats,
    shm: ShmState,
}

/// The client's side of the shared-memory transport.
struct ShmState {
    /// Whether to offer the sidecar a region when an open needs one. Off
    /// when shared memory is disabled, and after any shared-memory failure
    /// (which is logged once; frames then stay inline).
    enabled: bool,
    /// `VANCE_IPC_SHM_SLOT_BYTES`.
    slot_bytes_cap: Option<u64>,
    /// The region the sidecar has attached; mirrors the sidecar's own state.
    attached: Option<Attached>,
    /// Epoch of the newest region offered, attached or not. Each new offer
    /// is newer: the sidecar refuses an epoch it has seen.
    newest_epoch: u32,
    /// Slots of ended leases and spills, handed back before each request.
    releases: Arc<ReleaseQueue>,
}

struct Attached {
    epoch: u32,
    slot_size: u64,
    table: Arc<LeaseTable>,
}

/// The value of `key` in the sidecar's environment: the last `extra` entry
/// for it (as `Command::envs` applies them), else this process's.
fn sidecar_env(extra: &[(OsString, OsString)], key: &str) -> Option<OsString> {
    extra
        .iter()
        .rev()
        .find(|(k, _)| k.as_os_str() == OsStr::new(key))
        .map(|(_, v)| v.clone())
        .or_else(|| std::env::var_os(key))
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
    ///
    /// `VANCE_IPC_SHM=0` in `env` (or in this process's environment)
    /// overrides `options.shm`.
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
        let env: Vec<(OsString, OsString)> = env
            .into_iter()
            .map(|(k, v)| (k.as_ref().to_owned(), v.as_ref().to_owned()))
            .collect();
        let shm = ShmState {
            enabled: options.shm && sidecar_env(&env, SHM_ENV).is_none_or(|v| v != "0"),
            slot_bytes_cap: sidecar_env(&env, SLOT_BYTES_ENV)
                .and_then(|v| v.to_str()?.trim().parse::<u64>().ok())
                .filter(|&n| n > 0),
            attached: None,
            newest_epoch: 0,
            releases: Arc::default(),
        };
        let mut sidecar: Sidecar<DecoderRequest, DecoderResponse> = Sidecar::spawn_with_env(
            binary,
            std::iter::empty::<&str>(),
            env.iter().map(|(k, v)| (k, v)),
        )?;
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
                    shm,
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
        match self.request(&DecoderRequest::Open { path: path.into() })? {
            DecoderResponse::Opened(info) => {
                self.offer_region(&info)?;
                Ok(info)
            }
            DecoderResponse::OpenFailed(e) => Err(ClientError::Open(e)),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Opened",
                got: other,
            }),
        }
    }

    /// Send one request and wait for its reply, handing queued slots back
    /// first. Every request after `Hello` goes through here.
    fn request(&mut self, req: &DecoderRequest) -> Result<DecoderResponse, ClientError> {
        self.send_releases()?;
        Ok(self.sidecar.request(req)?)
    }

    /// Hand the slots of ended leases and spills back to the sidecar.
    fn send_releases(&mut self) -> Result<(), ClientError> {
        let mut slots = self.shm.releases.drain();
        let current = self.shm.attached.as_ref().map(|a| a.epoch);
        // Intentional: slots of any other region are dropped, not sent. The
        // sidecar let go of that region when it attached a newer one (or
        // answered `ShmUnavailable`), so it would only reject them as stale.
        slots.retain(|r| Some(r.epoch) == current);
        if slots.is_empty() {
            return Ok(());
        }
        match self.sidecar.request(&DecoderRequest::Release { slots })? {
            DecoderResponse::Released { stale } => {
                if stale > 0 {
                    tracing::warn!(
                        stale,
                        "decoder sidecar rejected shared-memory slot releases"
                    );
                }
                Ok(())
            }
            other => Err(ClientError::UnexpectedResponse {
                expected: "Released",
                got: other,
            }),
        }
    }

    /// After an open: offer the sidecar a region whose slots fit `info`'s
    /// frames, unless the attached one already does (regions only grow).
    /// Shared-memory failures are logged and leave frames inline; only a
    /// broken pipe or a protocol violation is an error.
    fn offer_region(&mut self, info: &StreamInfo) -> Result<(), ClientError> {
        if !self.shm.enabled {
            return Ok(());
        }
        let frame_bytes = info.pixel_format.frame_size(info.width, info.height) as u64;
        let wanted = self
            .shm
            .slot_bytes_cap
            .map_or(frame_bytes, |cap| frame_bytes.min(cap));
        if wanted == 0 {
            return Ok(());
        }
        let Some(slot_size) = wanted.checked_next_multiple_of(SLOT_ALIGN) else {
            self.shm_failed(&format!("a {wanted}-byte frame is too large for a slot"));
            return Ok(());
        };
        if self
            .shm
            .attached
            .as_ref()
            .is_some_and(|a| a.slot_size >= slot_size)
        {
            return Ok(());
        }
        let slot_count = u32::try_from(self.options.max_leases)
            .ok()
            .and_then(|leases| self.options.ring_slots.checked_add(leases))
            .and_then(|n| n.checked_add(1));
        let Some(slot_count) = slot_count else {
            self.shm_failed("ring_slots + max_leases + 1 slots overflow u32");
            return Ok(());
        };
        if !shm::shm_space_available(u64::from(slot_count).saturating_mul(slot_size)) {
            self.shm_failed(&format!(
                "no room for {slot_count} slots of {slot_size} bytes"
            ));
            return Ok(());
        }
        let Some(epoch) = self.shm.newest_epoch.checked_add(1) else {
            self.shm_failed("region epochs exhausted");
            return Ok(());
        };
        self.shm.newest_epoch = epoch;
        // Read-only: the sidecar is the only writer of a decoder region, so
        // the app can't scribble on a slot the sidecar owns. The creator
        // needs no write access to size the object.
        let mut region = match ShmRegion::create(epoch, slot_count, slot_size, Access::ReadOnly) {
            Ok(region) => region,
            Err(e) => {
                self.shm_failed(&e.to_string());
                return Ok(());
            }
        };
        let reply = self.request(&DecoderRequest::AttachShm(region.desc().clone()));
        // Intentional: unlinked whatever the reply, even an error. The
        // sidecar has mapped the region by now or never will, and the name
        // must not outlive the handshake (on Windows this is a no-op; the
        // section dies with its last handle).
        if let Err(e) = region.unlink_name() {
            tracing::warn!(error = %e, "could not unlink a decoder shared-memory region name");
        }
        match reply? {
            DecoderResponse::ShmAttached { epoch: attached } if attached == epoch => {
                let region = Arc::new(region);
                self.stats.shm_epoch = Some(epoch);
                self.stats.shm_bytes = region.byte_len();
                self.shm.attached = Some(Attached {
                    epoch,
                    slot_size,
                    table: LeaseTable::new(
                        region,
                        Arc::clone(&self.shm.releases),
                        self.options.max_leases,
                    ),
                });
                tracing::debug!(
                    epoch,
                    slot_count,
                    slot_size,
                    "decoder shared-memory region attached"
                );
                Ok(())
            }
            DecoderResponse::ShmUnavailable { reason } => {
                // The sidecar has dropped any region it had: every frame is
                // inline from now on, and old leases stay readable on their
                // own (they keep our mapping alive).
                self.shm.attached = None;
                self.stats.shm_epoch = None;
                self.stats.shm_bytes = 0;
                self.shm_failed(&reason);
                Ok(())
            }
            other => Err(ClientError::UnexpectedResponse {
                expected: "ShmAttached for the offered epoch, or ShmUnavailable",
                got: other,
            }),
        }
    }

    /// Stop offering regions to this sidecar, logging why once. An attached
    /// region keeps serving the frames that fit it.
    fn shm_failed(&mut self, reason: &str) {
        self.shm.enabled = false;
        tracing::warn!(
            reason,
            "decoder shared memory unavailable; frames that don't fit an attached region go inline"
        );
    }

    /// Seek the open stream so the next decode lands at or before `target_pts`.
    pub fn seek(&mut self, target_pts: i64) -> Result<i64, ClientError> {
        match self.request(&DecoderRequest::Seek { target_pts })? {
            DecoderResponse::Seeked { pts } => Ok(pts),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Seeked",
                got: other,
            }),
        }
    }

    /// Decode the first frame at or after `target_pts` and return it.
    pub fn decode_at(&mut self, target_pts: i64) -> Result<FrameData, ClientError> {
        match self.request(&DecoderRequest::DecodeAt { target_pts })? {
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
        match self.request(&DecoderRequest::StartStream {
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
    ///
    /// Slots released since the last request go back first, so the
    /// sidecar's decode-ahead can reuse them while the stream runs.
    pub fn next_frame(&mut self) -> Result<Option<FrameData>, ClientError> {
        match self.request(&DecoderRequest::NextFrame)? {
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
            FramePayload::Shared(slot) => {
                // `adopt` checks the epoch, slot index and length against the
                // attached region. The sidecar only publishes into its newest
                // region, which is the attached one, so a mismatch is a
                // protocol violation. No blob follows a shared payload, so
                // the stream stays in sync after rejecting it.
                let adopted = match &self.shm.attached {
                    Some(a) => a.table.adopt(slot),
                    None => Err(vance_ipc::Error::Shm(
                        "no shared-memory region is attached".into(),
                    )),
                };
                match adopted {
                    Ok(Adopted::Leased(lease)) => {
                        self.stats.shared += 1;
                        FrameBytes::from(lease)
                    }
                    Ok(Adopted::Spilled(bytes)) => {
                        self.stats.spilled += 1;
                        bytes.into()
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "decoder sent an unusable shared-memory slot");
                        return Err(ClientError::UnexpectedResponse {
                            expected: "Frame in the attached shared-memory region",
                            got: DecoderResponse::Frame { meta, payload },
                        });
                    }
                }
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
        match self.request(&DecoderRequest::StopStream)? {
            DecoderResponse::Stopped => Ok(()),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Stopped",
                got: other,
            }),
        }
    }

    /// Close the currently-open file. The sidecar stays alive.
    pub fn close(&mut self) -> Result<(), ClientError> {
        match self.request(&DecoderRequest::Close)? {
            DecoderResponse::Closed => Ok(()),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Closed",
                got: other,
            }),
        }
    }

    /// Send `Shutdown`, wait for `Goodbye`, then wait for the child to exit.
    pub fn shutdown(mut self) -> Result<(), ClientError> {
        let _ = self.request(&DecoderRequest::Shutdown);
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
