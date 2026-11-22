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
//! Ended leases queue their slots, and every request carries the queued
//! slots back in its [`DecoderEnvelope`]. Without a region (shared memory
//! off, or any shared-memory failure) frames arrive inline, as a raw blob
//! after the reply.
//!
//! Two environment variables, read from the sidecar's environment (the
//! `env` given to [`DecoderClient::spawn_with_env`], else this process's):
//! `VANCE_IPC_SHM=0` keeps every frame inline; `VANCE_IPC_SHM_SLOT_BYTES=n`
//! (tests only) caps the slot size at `n` bytes, so frames bigger than that
//! go inline.
//!
//! ## Threads
//!
//! [`ClientOptions::threads`] sets the sidecar's decode and conversion
//! thread counts (`VANCE_DECODE_THREADS`, `VANCE_CONVERT_THREADS`);
//! [`thread_budget`] picks them for a number of concurrent decoders.

use crate::frame::{FrameBytes, FrameData};
use crate::protocol::{
    DecoderEnvelope, DecoderRequest, DecoderResponse, FrameMeta, FramePayload, OpenError, SlotRef,
    StreamInfo, PROTOCOL_VERSION,
};
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use thiserror::Error;
use vance_ipc::shm::{self, Access, Adopted, LeaseTable, ReleaseQueue, ShmRegion, SLOT_ALIGN};
use vance_ipc::Sidecar;

/// `VANCE_IPC_SHM=0` keeps every frame inline.
const SHM_ENV: &str = "VANCE_IPC_SHM";
/// Test-only cap on the slot size, in bytes.
const SLOT_BYTES_ENV: &str = "VANCE_IPC_SHM_SLOT_BYTES";
/// The sidecar's software decode thread count.
const DECODE_THREADS_ENV: &str = "VANCE_DECODE_THREADS";
/// The sidecar's pixel-conversion (swscale) thread count.
const CONVERT_THREADS_ENV: &str = "VANCE_CONVERT_THREADS";

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
    /// The sidecar's decode and conversion thread counts, passed in its
    /// environment at spawn. A count already set in the `env` given to
    /// [`DecoderClient::spawn_with_options`], or in this process's
    /// environment, wins over the budget. `None` (the default) leaves the
    /// sidecar's own defaults.
    pub threads: Option<ThreadBudget>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            shm: true,
            max_leases: 2,
            ring_slots: 4,
            threads: None,
        }
    }
}

/// Threads one decoder sidecar may use (see [`thread_budget`]). The sidecar
/// ignores a count of 0 and keeps its default for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadBudget {
    /// FFmpeg software decode threads (`VANCE_DECODE_THREADS`).
    pub decode: u32,
    /// swscale pixel-conversion threads (`VANCE_CONVERT_THREADS`).
    pub convert: u32,
}

/// Split the machine's cores across `active_layers` concurrent decoders so the
/// total decode + conversion threads don't oversubscribe the CPU. Every layer
/// gets at least 1 of each.
///
/// With `cores` = [`std::thread::available_parallelism`] (1 if unknown) and
/// `layers` = `max(active_layers, 1)`:
///
/// - `share = max(2, cores / layers)` (integer division) is what one layer
///   may use. The floor of 2 (one thread of each kind) only applies past
///   `cores / 2` layers, and only then does the total exceed `cores`.
/// - `decode = min(8, ceil(share / 2))` and `convert = min(8, floor(share / 2))`:
///   an even split, the odd thread going to decode. A 4K 10-bit 4:2:2 frame
///   costs about as much to convert as to decode (~30 and ~35 ms on one M1
///   core), and the two run at the same time: frame threads decode ahead
///   while the stream thread converts. 8 is the sidecar's own default cap.
///
/// So 1 layer on 8 cores gets 4 + 4, 2 layers get 2 + 2 each, and 4 or more
/// layers get 1 + 1 each.
pub fn thread_budget(active_layers: usize) -> ThreadBudget {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    thread_budget_for(cores, active_layers)
}

/// [`thread_budget`] for a machine with `cores` cores.
fn thread_budget_for(cores: usize, active_layers: usize) -> ThreadBudget {
    let share = (cores / active_layers.max(1)).max(2);
    let count = |n: usize| u32::try_from(n.min(8)).unwrap_or(8);
    ThreadBudget {
        decode: count(share.div_ceil(2)),
        convert: count(share / 2),
    }
}

/// The environment entries that pass `budget` to the sidecar: one per count
/// that `is_set` (the caller's env, or this process's) doesn't already set.
fn thread_budget_env(
    budget: ThreadBudget,
    is_set: impl Fn(&str) -> bool,
) -> Vec<(OsString, OsString)> {
    [
        (DECODE_THREADS_ENV, budget.decode),
        (CONVERT_THREADS_ENV, budget.convert),
    ]
    .into_iter()
    .filter(|&(key, _)| !is_set(key))
    .map(|(key, n)| (key.into(), n.to_string().into()))
    .collect()
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

/// Kills a decoder's sidecar from any thread, e.g. one stuck in a read on a
/// hung network share while its client is blocked waiting for it. Get one
/// with [`DecoderClient::kill_handle`].
#[derive(Clone)]
pub struct SidecarKill(Arc<KillState>);

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
    /// A signal has been sent (whether or not it found the process).
    sent: bool,
}

impl SidecarKill {
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
        // Intentional: signalled by pid, without the lock, but while
        // `in_flight` keeps the client from reaping the process (it waits
        // in `mark_reaped`): until reaped, the process keeps its pid, even as
        // a zombie. One reap the client can only record afterwards: vance-ipc
        // checks the exit status when a read fails, which happens only once
        // the process has died. Died through this handle, `killed` stops a
        // second signal; died on its own, a kill() in the instant before the
        // client's record would need the OS to hand the pid out again in that
        // instant. (A `Child` shared by vance-ipc's `Sidecar` would close it.)
        let sent = force_kill(pid);
        let mut flags = self.flags();
        flags.in_flight = false;
        // A failed attempt may be retried.
        flags.killed = sent;
        drop(flags);
        self.0.idle.notify_all();
    }

    /// Whether a kill through this handle (or a clone) has sent the sidecar
    /// a signal. `false` after kills that came too late: the client had
    /// reaped the process, whose pid may be another's by then.
    pub fn signalled(&self) -> bool {
        self.flags().sent
    }

    fn flags(&self) -> std::sync::MutexGuard<'_, KillFlags> {
        self.0.flags.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Record that the client may reap the process now; first waits for a
    /// kill in flight, which signals it by pid.
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

/// Kill process `pid` without the `libc` / `windows-sys` this crate doesn't
/// depend on: `kill -KILL`, or `taskkill /F` (without a console window).
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
struct ReapGuard(SidecarKill);

impl Drop for ReapGuard {
    fn drop(&mut self) {
        self.0.mark_reaped();
    }
}

/// Parent-side decoder client. Owns one sidecar process.
pub struct DecoderClient {
    /// Declared first, so it drops before `sidecar`.
    reap: ReapGuard,
    sidecar: Sidecar<DecoderEnvelope, DecoderResponse>,
    ffmpeg_version: String,
    options: ClientOptions,
    stats: TransportStats,
    shm: ShmState,
    /// Encoding hints of the open stream, stamped on every frame.
    color: crate::frame::FrameColor,
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
    /// Slots of ended leases and spills, handed back with the next request.
    releases: Arc<ReleaseQueue>,
}

impl ShmState {
    /// The queued slots the sidecar should take back now.
    fn take_releases(&self) -> Vec<SlotRef> {
        let mut slots = self.releases.drain();
        let current = self.attached.as_ref().map(|a| a.epoch);
        // Intentional: slots of any other region are dropped, not sent. The
        // sidecar let go of that region when it attached a newer one (or
        // answered `ShmUnavailable`), so it would only reject them as stale.
        slots.retain(|r| Some(r.epoch) == current);
        slots
    }
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
    /// options. `spawn` / `spawn_with_env` use the defaults.
    ///
    /// `VANCE_IPC_SHM=0` in `env` (or in this process's environment)
    /// overrides `options.shm`, and `VANCE_DECODE_THREADS` /
    /// `VANCE_CONVERT_THREADS` there override `options.threads`.
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
        Self::spawn_with_kill_handle(binary, env, options, |_| {})
    }

    /// [`DecoderClient::spawn_with_options`], handing `on_spawn` the
    /// sidecar's [`SidecarKill`] as soon as the process has started, before
    /// the `Hello` handshake: a sidecar that hangs during startup can then be
    /// killed from another thread, and this returns an error promptly.
    /// `on_spawn` runs on the calling thread, once, unless the process can't
    /// be started at all. It is the same handle as
    /// [`DecoderClient::kill_handle`]'s.
    pub fn spawn_with_kill_handle<P, I, K, V, F>(
        binary: P,
        env: I,
        options: ClientOptions,
        on_spawn: F,
    ) -> Result<Self, ClientError>
    where
        P: AsRef<Path>,
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
        F: FnOnce(SidecarKill),
    {
        let mut env: Vec<(OsString, OsString)> = env
            .into_iter()
            .map(|(k, v)| (k.as_ref().to_owned(), v.as_ref().to_owned()))
            .collect();
        if let Some(budget) = options.threads {
            let budget_env = thread_budget_env(budget, |key| sidecar_env(&env, key).is_some());
            env.extend(budget_env);
        }
        let shm = ShmState {
            enabled: options.shm && sidecar_env(&env, SHM_ENV).is_none_or(|v| v != "0"),
            slot_bytes_cap: sidecar_env(&env, SLOT_BYTES_ENV)
                .and_then(|v| v.to_str()?.trim().parse::<u64>().ok())
                .filter(|&n| n > 0),
            attached: None,
            newest_epoch: 0,
            releases: Arc::default(),
        };
        let mut sidecar: Sidecar<DecoderEnvelope, DecoderResponse> = Sidecar::spawn_with_env(
            binary,
            std::iter::empty::<&str>(),
            env.iter().map(|(k, v)| (k, v)),
        )?;
        let kill = SidecarKill(Arc::new(KillState {
            pid: sidecar.pid(),
            flags: Mutex::default(),
            idle: Condvar::new(),
        }));
        // Declared after `sidecar`, so on an early return it drops first:
        // the process is marked reaped before the `Sidecar` reaps it.
        let reap = ReapGuard(kill.clone());
        on_spawn(kill);
        // No releases: a sidecar on another protocol version must still
        // read this as its `Hello` (see `PROTOCOL_VERSION`).
        let resp = sidecar.request(&DecoderEnvelope {
            releases: Vec::new(),
            request: DecoderRequest::Hello {
                protocol_version: PROTOCOL_VERSION,
            },
        });
        if resp.is_err() {
            reap.0.mark_reaped();
        }
        let resp = resp?;
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
                    reap,
                    sidecar,
                    ffmpeg_version,
                    options,
                    stats: TransportStats::default(),
                    shm,
                    color: Default::default(),
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
        match self.request(DecoderRequest::Open { path: path.into() })? {
            DecoderResponse::Opened(info) => {
                self.offer_region(&info)?;
                self.color = crate::frame::FrameColor::from_stream(&info);
                Ok(info)
            }
            DecoderResponse::OpenFailed(e) => Err(ClientError::Open(e)),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Opened",
                got: other,
            }),
        }
    }

    /// Send one request, with every queued slot riding along to be
    /// released first, and wait for its reply. Every request after `Hello`
    /// goes through here.
    fn request(&mut self, request: DecoderRequest) -> Result<DecoderResponse, ClientError> {
        let envelope = DecoderEnvelope {
            releases: self.shm.take_releases(),
            request,
        };
        let reply = self.sidecar.request(&envelope);
        self.reaped_after(&reply);
        Ok(reply?)
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
        let reply = self.request(DecoderRequest::AttachShm(region.desc().clone()));
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
        match self.request(DecoderRequest::Seek { target_pts })? {
            DecoderResponse::Seeked { pts } => Ok(pts),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Seeked",
                got: other,
            }),
        }
    }

    /// Decode the first frame at or after `target_pts` and return it.
    pub fn decode_at(&mut self, target_pts: i64) -> Result<FrameData, ClientError> {
        match self.request(DecoderRequest::DecodeAt { target_pts })? {
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
        match self.request(DecoderRequest::StartStream {
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
    /// Slots released since the last request ride along and go back
    /// first, so the sidecar's decode-ahead can reuse them while the stream
    /// runs.
    pub fn next_frame(&mut self) -> Result<Option<FrameData>, ClientError> {
        match self.request(DecoderRequest::NextFrame)? {
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
                let bytes = self.sidecar.recv_blob(len);
                self.reaped_after(&bytes);
                let bytes = bytes?;
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
            color: self.color,
        })
    }

    /// Tear down the in-progress stream. Sidecar stays alive.
    pub fn stop_stream(&mut self) -> Result<(), ClientError> {
        match self.request(DecoderRequest::StopStream)? {
            DecoderResponse::Stopped => Ok(()),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Stopped",
                got: other,
            }),
        }
    }

    /// Close the currently-open file. The sidecar stays alive.
    pub fn close(&mut self) -> Result<(), ClientError> {
        match self.request(DecoderRequest::Close)? {
            DecoderResponse::Closed => Ok(()),
            other => Err(ClientError::UnexpectedResponse {
                expected: "Closed",
                got: other,
            }),
        }
    }

    /// Send `Shutdown`, wait for `Goodbye`, then wait for the child to exit.
    pub fn shutdown(mut self) -> Result<(), ClientError> {
        let _ = self.request(DecoderRequest::Shutdown);
        // The shutdown below waits for (reaps) the process.
        self.reap.0.mark_reaped();
        // Sidecar may have already exited after replying; that's fine.
        let _ = self.sidecar.shutdown();
        Ok(())
    }

    /// A handle that kills this client's sidecar from any thread (see
    /// [`SidecarKill`]).
    pub fn kill_handle(&self) -> SidecarKill {
        self.reap.0.clone()
    }

    /// The sidecar's OS process id (`None` once it has been shut down).
    pub fn pid(&self) -> Option<u32> {
        self.sidecar.pid()
    }

    /// After a failed exchange with the sidecar: vance-ipc then checks the
    /// child's exit status, which reaps a process that has exited, so its
    /// pid is no longer safe to signal.
    fn reaped_after<T>(&self, result: &vance_ipc::Result<T>) {
        if result.is_err() {
            self.reap.0.mark_reaped();
        }
    }

    /// Force-kill the sidecar process. Test/diagnostic only — production
    /// code should use [`DecoderClient::shutdown`] for graceful exit.
    /// Use to validate the crash-isolation contract (PLAN.md §3 principle 4).
    pub fn kill(&mut self) -> Result<(), ClientError> {
        Ok(self.sidecar.kill()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(decode, convert)` for 1, 2, 4, 6 and 16 layers.
    fn budgets(cores: usize) -> Vec<(u32, u32)> {
        [1, 2, 4, 6, 16]
            .into_iter()
            .map(|layers| {
                let b = thread_budget_for(cores, layers);
                (b.decode, b.convert)
            })
            .collect()
    }

    #[test]
    fn thread_budget_splits_the_cores_across_layers() {
        assert_eq!(budgets(4), [(2, 2), (1, 1), (1, 1), (1, 1), (1, 1)]);
        assert_eq!(budgets(8), [(4, 4), (2, 2), (1, 1), (1, 1), (1, 1)]);
        assert_eq!(budgets(16), [(8, 8), (4, 4), (2, 2), (1, 1), (1, 1)]);
        // Odd shares give decode the extra thread; each kind stays <= 8.
        assert_eq!(
            thread_budget_for(6, 2),
            ThreadBudget {
                decode: 2,
                convert: 1
            }
        );
        assert_eq!(
            thread_budget_for(64, 1),
            ThreadBudget {
                decode: 8,
                convert: 8
            }
        );
        // No layers counts as one; an unknown core count as one core.
        assert_eq!(thread_budget_for(8, 0), thread_budget_for(8, 1));
        assert_eq!(
            thread_budget_for(1, 1),
            ThreadBudget {
                decode: 1,
                convert: 1
            }
        );
    }

    #[test]
    fn thread_budget_never_oversubscribes_a_layers_share() {
        for cores in 1..=64 {
            for layers in 1..=32 {
                let b = thread_budget_for(cores, layers);
                let what = format!("{cores} cores, {layers} layers: {b:?}");
                assert!((1..=8).contains(&b.decode), "{what}");
                assert!((1..=8).contains(&b.convert), "{what}");
                let share = (cores / layers).max(2) as u32;
                assert!(b.decode + b.convert <= share, "{what}");
            }
        }
    }

    #[test]
    fn thread_budget_uses_this_machines_cores() {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        assert_eq!(thread_budget(3), thread_budget_for(cores, 3));
    }

    #[test]
    fn a_thread_count_set_explicitly_wins_over_the_budget() {
        let budget = ThreadBudget {
            decode: 3,
            convert: 5,
        };
        let entries = |set: &[&str]| -> Vec<(String, String)> {
            thread_budget_env(budget, |key| set.contains(&key))
                .into_iter()
                .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
                .collect()
        };
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        assert_eq!(
            entries(&[]),
            [
                pair("VANCE_DECODE_THREADS", "3"),
                pair("VANCE_CONVERT_THREADS", "5")
            ]
        );
        assert_eq!(
            entries(&["VANCE_DECODE_THREADS"]),
            [pair("VANCE_CONVERT_THREADS", "5")]
        );
        assert!(entries(&["VANCE_DECODE_THREADS", "VANCE_CONVERT_THREADS"]).is_empty());
    }

    #[test]
    fn the_sidecar_env_is_the_last_caller_entry_else_this_process() {
        let env = |k: &str, v: &str| (OsString::from(k), OsString::from(v));
        let extra = [
            env("VANCE_TEST_SIDECAR_ENV", "1"),
            env("VANCE_TEST_SIDECAR_ENV", "2"),
        ];
        assert_eq!(
            sidecar_env(&extra, "VANCE_TEST_SIDECAR_ENV"),
            Some("2".into())
        );
        assert_eq!(sidecar_env(&[], "VANCE_TEST_SIDECAR_ENV_UNSET"), None);
        assert_eq!(sidecar_env(&[], "PATH"), std::env::var_os("PATH"));
    }
}
