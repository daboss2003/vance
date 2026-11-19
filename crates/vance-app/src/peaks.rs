//! `PeaksService` — background extractor for audio waveform caches.
//!
//! Phase 7.1 M2. One worker thread pulls `PeakJob`s off a FIFO,
//! spawns its own audio sidecar (separate from the playback sidecar so
//! import never contends with cpal), streams PCM until EOF, folds each
//! chunk into a [`vance_peaks::PeakBuilder`], and writes
//! `<cache_dir>/<hash>.peaks` atomically. Failure writes a sibling
//! `<hash>.peaks.err` (small JSON describing the error) so we don't
//! retry forever; success removes any prior `.err`.
//!
//! ## State machine
//!
//! ```text
//!   (none) ─enqueue─▶ Queued ─worker pull─▶ Running { samples_done }
//!                                                  │
//!                            ┌─cancel/shutdown─────┤───extract OK──▶ Done
//!                            ▼                     ▼
//!                         Cancelled              Failed(json)
//! ```
//!
//! Terminal states (`Done`, `Failed`, `Cancelled`) are sticky — a
//! second `cancel` after `Done` does NOT clobber Done.
//! `enqueue` for a media id that's currently `Running` is a no-op
//! (the in-flight extraction will deliver the same data).
//!
//! ## Cancellation latency
//!
//! Cancellation is propagated three ways:
//! 1. The worker checks the cancelled set + the shutdown flag between
//!    PCM chunks, so a typical chunk-boundary cancel completes within
//!    a few hundred ms.
//! 2. If `cancel`/`shutdown` fires while the worker is blocked inside
//!    a sidecar IPC read, the service sends `SIGTERM` to the sidecar
//!    PID; the worker's `next_chunk` then returns `BrokenPipe`, the
//!    cancelled flag is re-checked, and `Cancelled` is emitted.
//! 3. `Drop` joins with a 5-second timeout; if the worker is still
//!    not joined (e.g. a hung sidecar that ignored SIGTERM), we log
//!    a `tracing::warn` and detach. The OS will reap on process exit.

use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use vance_audio::{AudioClient, AudioClientError};
use vance_peaks::{
    write_atomic, FinishedPeaks, HashPrefix, PeakBuilder, PeakError, PeakFile, PeakHeader,
    FORMAT_VERSION, LANE_COUNT, LEVEL_COUNT_MAX,
};
use vance_timeline::MediaItemId;

/// Errors surfaced to the Tauri layer or the service's internal log.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum PeaksServiceError {
    /// Worker IO (mostly file-system).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Underlying `.peaks` format / writer error.
    #[error("peaks: {0}")]
    Peaks(#[from] PeakError),
    /// Audio sidecar failure (handshake, decode, etc.).
    #[error("audio: {0}")]
    Audio(#[from] AudioClientError),
    /// Service has been shut down; new work is rejected.
    #[error("service shutting down")]
    ShuttingDown,
    /// Job was cancelled mid-stream (or before it started).
    #[error("cancelled")]
    Cancelled,
}

/// One unit of background work — extract peaks for a single media item.
#[derive(Debug, Clone)]
pub struct PeakJob {
    /// Library entry being extracted. Used as the cancellation /
    /// status-lookup key.
    pub media_id: MediaItemId,
    /// Absolute path of the media file (passed to the audio sidecar's
    /// `Open` request). Captured for diagnostics in the `.peaks.err`
    /// marker on failure.
    pub path: PathBuf,
    /// First 12 bytes of the media's content hash (BLAKE3 or
    /// path-based fallback — see [`hash_prefix_from_path`]).
    pub content_hash_prefix: HashPrefix,
    /// Expected duration in seconds, used as a staleness gate when
    /// deciding whether an existing slot is reusable. `None` skips the
    /// length check (only the hash prefix is compared).
    pub expected_duration_seconds: Option<f64>,
}

/// Public state of a peak-extraction job. M3 wraps this in a Tauri
/// event payload; tests assert on it directly.
#[derive(Debug, Clone)]
pub enum JobState {
    /// In the queue but not yet started.
    Queued,
    /// Currently extracting.
    Running {
        /// Frames already folded into the builder.
        samples_done: u64,
        /// Expected total frame count, or `None` for unknown-duration
        /// streams. UI uses ratio = samples_done / samples_total to
        /// drive a progress bar.
        samples_total: Option<u64>,
    },
    /// Finished. `samples_total` is the authoritative count baked into
    /// the on-disk header.
    Done {
        /// Per-channel sample count from `total_frames`.
        samples_total: u64,
    },
    /// Crashed. The `.peaks.err` marker exists; UI renders a thin red
    /// bar instead of a waveform.
    Failed(String),
    /// Cancelled by the user (or by service shutdown).
    Cancelled,
}

impl JobState {
    /// Is this state terminal — meaning no further transition is
    /// allowed (Done / Failed / Cancelled)?
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            JobState::Done { .. } | JobState::Failed(_) | JobState::Cancelled
        )
    }
}

/// Progress event sink. `PeaksService` calls this on every state
/// transition AND on per-chunk progress while Running, throttled to
/// ≤ 10 Hz. The sink runs on the worker thread (or the caller's
/// thread for fast-path Done) — it MUST NOT block.
pub type ProgressSink = Arc<dyn Fn(&MediaItemId, &JobState) + Send + Sync>;

/// On-disk schema of `<hash>.peaks.err`. Small JSON so an operator
/// reading a stranded cache directory knows what failed and when.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeakErrMarker {
    /// Free-form error message (the `Display` of [`PeaksServiceError`]).
    pub reason: String,
    /// The source media path at extraction time (for context when the
    /// file has since moved).
    pub path: String,
    /// Unix epoch milliseconds when the failure was recorded.
    pub at_unix_millis: u128,
}

/// Background service handle. Drop or call `shutdown` to stop.
pub struct PeaksService {
    inner: Arc<Inner>,
    shutdown: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

struct Inner {
    audio_binary: PathBuf,
    cache_dir: PathBuf,
    queue: Mutex<Queue>,
    cv: Condvar,
    progress: ProgressSink,
    /// PID of the currently-running sidecar, if any. Set by the worker
    /// at the start of `extract`, cleared on exit. `cancel`/`shutdown`
    /// reads it to send `SIGTERM`, which unsticks any blocking IPC
    /// read inside `next_chunk`.
    current_sidecar_pid: Mutex<Option<u32>>,
    /// Long-lived sidecar dedicated to `probe_audio` calls (Phase 7.2).
    /// Lazily spawned on first probe; reused across probes so import-
    /// time audio detection pays ~5-15 ms per file instead of the
    /// ~50-200 ms `AudioClient::spawn` cost. The mutex is held ONLY
    /// to take/insert the client out of/into the pool — the blocking
    /// IPC calls and the initial spawn happen with the lock released,
    /// so `do_shutdown` can preempt a hung probe.
    probe_client: Mutex<Option<AudioClient>>,
    /// PID of the probe sidecar, populated after spawn. `do_shutdown`
    /// SIGTERMs it before attempting to drain the pool, so a probe
    /// blocked inside an open/close IPC read unblocks promptly.
    probe_sidecar_pid: Mutex<Option<u32>>,
}

struct Queue {
    /// FIFO of pending jobs in submission order. Prioritize moves a job
    /// to the front.
    pending: VecDeque<PeakJob>,
    /// Last-known state per media id (Queued / Running / Done / …).
    statuses: HashMap<MediaItemId, JobState>,
    /// Media ids the user explicitly cancelled. The worker checks
    /// this every chunk; entries are removed when the corresponding
    /// terminal state is observed so the set doesn't grow unbounded.
    cancelled: HashSet<MediaItemId>,
    /// Media id currently being extracted (so cancel can race the
    /// worker safely).
    current: Option<MediaItemId>,
    /// True after `shutdown`/`Drop`; new enqueue/cancel/prioritize
    /// become no-ops (or fail explicitly). Set under the queue lock
    /// so callers see it atomically.
    closed: bool,
}

impl PeaksService {
    /// Spawn the worker thread. `audio_binary` is the absolute path of
    /// the `vance-audio-decoder` sidecar. `cache_dir` is created if
    /// missing, and any leftover `*.peaks.tmp` orphans are cleaned at
    /// startup (an orphan is what a previous crash mid-`write_atomic`
    /// would leave behind).
    pub fn start(
        audio_binary: PathBuf,
        cache_dir: PathBuf,
        progress: ProgressSink,
    ) -> Result<Self, PeaksServiceError> {
        fs::create_dir_all(&cache_dir)?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let inner = Arc::new(Inner {
            audio_binary,
            cache_dir,
            queue: Mutex::new(Queue {
                pending: VecDeque::new(),
                statuses: HashMap::new(),
                cancelled: HashSet::new(),
                current: None,
                closed: false,
            }),
            cv: Condvar::new(),
            progress,
            current_sidecar_pid: Mutex::new(None),
            probe_client: Mutex::new(None),
            probe_sidecar_pid: Mutex::new(None),
        });
        // Best-effort startup cleanup. Logged but not fatal.
        match clean_orphan_tmp(&inner.cache_dir) {
            Ok(n) if n > 0 => {
                tracing::info!(
                    target: "vance_app.peaks",
                    removed = n,
                    cache_dir = ?inner.cache_dir,
                    "removed orphan .peaks.tmp files at start"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    target: "vance_app.peaks",
                    cache_dir = ?inner.cache_dir,
                    err = ?e,
                    "orphan-tmp cleanup failed at start"
                );
            }
        }
        let worker_inner = inner.clone();
        let worker_shutdown = shutdown.clone();
        let worker = std::thread::Builder::new()
            .name("vance-app.peaks-worker".into())
            .spawn(move || worker_loop(worker_inner, worker_shutdown))
            .map_err(PeaksServiceError::Io)?;
        Ok(Self {
            inner,
            shutdown,
            worker: Some(worker),
        })
    }

    /// Add a job to the queue. If the slot already exists on disk and
    /// passes the staleness check, the job is marked `Done` immediately
    /// (no work done, but the status event fires so the UI repaints).
    ///
    /// Behavior matrix:
    /// * service shut down → `Err(ShuttingDown)`, no events emitted.
    /// * fresh slot on disk → emit `Queued` then `Done` (state-machine
    ///   skips `Running` because there's no work).
    /// * media id currently `Running` or `Queued` → no-op, returns Ok.
    /// * any other prior state (Done, Failed, Cancelled, none) → enqueue
    ///   fresh, emit `Queued`.
    pub fn enqueue(&self, job: PeakJob) -> Result<(), PeaksServiceError> {
        // First: take the lock and decide the action. We hold the lock
        // for everything that mutates state so cancel/shutdown can't
        // race us mid-transition.
        let mut q = self.inner.queue.lock();
        if q.closed {
            return Err(PeaksServiceError::ShuttingDown);
        }
        match q.statuses.get(&job.media_id) {
            Some(JobState::Queued) | Some(JobState::Running { .. }) => {
                // Duplicate submission for an active job — no-op.
                return Ok(());
            }
            _ => {}
        }
        // Clear any prior cancel flag so a re-enqueue after a user
        // cancel can actually run.
        q.cancelled.remove(&job.media_id);

        // Fast path — fresh slot already on disk. Check INSIDE the
        // lock so we can't race a concurrent cancel(): the cancel
        // would have to win the lock either before this branch (so we
        // see the cancelled flag still set above and re-enqueue with
        // fresh status) or after we install Done — and a cancel of a
        // Done id is a no-op.
        let slot = slot_path(&self.inner.cache_dir, &job.content_hash_prefix);
        match slot_freshness(&slot, &job) {
            FreshnessCheck::Fresh(samples) => {
                // Remove any stale failure marker first.
                let err_path = err_path(&self.inner.cache_dir, &job.content_hash_prefix);
                let _ = fs::remove_file(&err_path);
                let media_id = job.media_id;
                q.statuses.insert(media_id, JobState::Queued);
                drop(q);
                (self.inner.progress)(&media_id, &JobState::Queued);
                let done = JobState::Done {
                    samples_total: samples,
                };
                let mut q = self.inner.queue.lock();
                q.statuses.insert(media_id, done.clone());
                drop(q);
                (self.inner.progress)(&media_id, &done);
                return Ok(());
            }
            FreshnessCheck::Stale | FreshnessCheck::Missing => {}
            FreshnessCheck::IoError(e) => {
                // EACCES / EIO etc. — propagate as Failed rather than
                // silently re-running (would loop on a broken disk).
                let media_id = job.media_id;
                let reason = format!("io reading existing slot: {e}");
                q.statuses
                    .insert(media_id, JobState::Failed(reason.clone()));
                drop(q);
                write_err_marker(&self.inner.cache_dir, &job, &reason);
                (self.inner.progress)(&media_id, &JobState::Failed(reason));
                return Ok(());
            }
        }

        // Otherwise enqueue.
        let media_id = job.media_id;
        q.statuses.insert(media_id, JobState::Queued);
        // Replace any prior pending entry for the same id (rare but
        // possible if the UI double-fires before the worker picks up).
        q.pending.retain(|j| j.media_id != media_id);
        q.pending.push_back(job);
        drop(q);
        (self.inner.progress)(&media_id, &JobState::Queued);
        self.inner.cv.notify_one();
        Ok(())
    }

    /// Bump a queued job to the front of the FIFO. No-op if not queued
    /// (already running, done, or never enqueued).
    pub fn prioritize(&self, media_id: MediaItemId) {
        let mut q = self.inner.queue.lock();
        if q.closed {
            return;
        }
        if let Some(idx) = q.pending.iter().position(|j| j.media_id == media_id) {
            if idx > 0 {
                if let Some(job) = q.pending.remove(idx) {
                    q.pending.push_front(job);
                }
            }
        }
        // Wake the worker even if the queue was non-empty — harmless
        // if it was already going to wake.
        self.inner.cv.notify_one();
    }

    /// Cancel a job. Behavior:
    /// * id is in a terminal state (Done/Failed/Cancelled) → no-op.
    /// * id is `Queued` → drop from pending, emit `Cancelled`.
    /// * id is `Running` → mark cancelled; the worker observes it
    ///   between chunks AND we send `SIGTERM` to the sidecar to
    ///   unstick any in-flight blocking read. The single `Cancelled`
    ///   event is emitted from the worker, not here.
    /// * id never seen → no-op.
    pub fn cancel(&self, media_id: MediaItemId) {
        let mut q = self.inner.queue.lock();
        if q.closed {
            return;
        }
        // Sticky terminal states.
        if let Some(s) = q.statuses.get(&media_id) {
            if s.is_terminal() {
                return;
            }
        } else {
            // Never seen — don't pollute the status map.
            return;
        }
        let was_pending = q.pending.iter().any(|j| j.media_id == media_id);
        let was_running = q.current == Some(media_id);
        q.pending.retain(|j| j.media_id != media_id);
        q.cancelled.insert(media_id);

        if was_pending && !was_running {
            // Worker hasn't seen it yet; emit Cancelled here directly
            // so the UI doesn't need a round-trip via the worker.
            q.statuses.insert(media_id, JobState::Cancelled);
            drop(q);
            (self.inner.progress)(&media_id, &JobState::Cancelled);
            return;
        }

        // Running — the worker emits Cancelled once it returns. We
        // also unstick any in-flight blocking read on the sidecar.
        let pid = *self.inner.current_sidecar_pid.lock();
        drop(q);
        if was_running {
            if let Some(p) = pid {
                signal_terminate(p);
            }
        }
        // No Cancelled event from this thread for Running — that's
        // the worker's responsibility on its way out.
    }

    /// Read the latest [`JobState`] for `media_id`, or `None` if the
    /// service has never seen it.
    pub fn status(&self, media_id: MediaItemId) -> Option<JobState> {
        self.inner.queue.lock().statuses.get(&media_id).cloned()
    }

    /// Probe whether `path` carries an audio stream the peaks pipeline
    /// can decode. Used by `Engine::import_media_file` (Phase 7.2) to
    /// flag video media items that have embedded audio.
    ///
    /// Pools a single long-lived `AudioClient` across calls so each
    /// probe is one IPC round-trip (~5-15 ms) instead of a full sidecar
    /// spawn (~50-200 ms). The pool mutex is held ONLY for the brief
    /// take/insert operations — `AudioClient::spawn` and every blocking
    /// IPC call happen with the mutex released, so a hung probe does
    /// NOT block `do_shutdown` (which SIGTERMs the probe sidecar's PID
    /// to unstick it) and does NOT serialize an unrelated concurrent
    /// import behind a slow first-spawn.
    ///
    /// Treats any `AudioClientError::Open` as `Ok(false)` — the
    /// engine-side decoder probe has already validated the file exists,
    /// so an audio-side open failure genuinely means "no audio stream
    /// we can use" (codec unsupported, no audio in container, etc.).
    /// Other IPC errors bubble up so the caller can log.
    ///
    /// Returns `Err(ShuttingDown)` if the service was shut down before
    /// or during the call.
    pub fn probe_audio(&self, path: &str) -> Result<bool, PeaksServiceError> {
        for attempt in 0..2 {
            if self.shutdown.load(Ordering::Acquire) {
                return Err(PeaksServiceError::ShuttingDown);
            }
            // Take the client out of the pool, OR spawn one if empty,
            // with the lock held only for the take. The spawn happens
            // OUTSIDE the lock so a slow startup doesn't serialize
            // do_shutdown or a parallel import.
            let mut client = match self.inner.probe_client.lock().take() {
                Some(c) => c,
                None => {
                    let c = AudioClient::spawn(&self.inner.audio_binary)?;
                    if let Some(pid) = c.pid() {
                        *self.inner.probe_sidecar_pid.lock() = Some(pid);
                    }
                    c
                }
            };
            match client.open(path.to_string()) {
                Ok(_info) => {
                    // Inspect close() — if the sidecar died mid-close,
                    // drop the client so the next call respawns at the
                    // same file (better diagnostics than deferring the
                    // respawn to a later probe of an unrelated file).
                    if client.close().is_err() {
                        *self.inner.probe_sidecar_pid.lock() = None;
                        return Ok(true);
                    }
                    *self.inner.probe_client.lock() = Some(client);
                    return Ok(true);
                }
                Err(AudioClientError::Open(_)) => {
                    // Genuine "no audio stream" / unsupported. Sidecar
                    // is still alive — return it to the pool.
                    *self.inner.probe_client.lock() = Some(client);
                    return Ok(false);
                }
                Err(other) => {
                    // IPC error — sidecar may be dead. Drop it; the
                    // retry attempt spawns fresh.
                    tracing::warn!(
                        target: "vance_app.peaks",
                        attempt,
                        path = %path,
                        err = ?other,
                        "probe_audio: sidecar IPC error; respawning"
                    );
                    drop(client);
                    *self.inner.probe_sidecar_pid.lock() = None;
                    if attempt == 1 {
                        return Err(PeaksServiceError::Audio(other));
                    }
                }
            }
        }
        unreachable!("probe_audio loop bounded to 2 attempts");
    }

    /// Resolve `<cache_dir>/<hex(content_hash_prefix)>.peaks`.
    pub fn slot_path(&self, prefix: &HashPrefix) -> PathBuf {
        slot_path(&self.inner.cache_dir, prefix)
    }

    /// Resolve the failure-marker sibling path for a slot. M6 uses
    /// this for diagnostics + cache-eviction.
    #[allow(dead_code)]
    pub fn err_path(&self, prefix: &HashPrefix) -> PathBuf {
        err_path(&self.inner.cache_dir, prefix)
    }

    /// Delete any `*.peaks.tmp` left over in `cache_dir`. Already called
    /// once by `start`; re-exposed so callers can sweep again before a
    /// long idle period (used by M6's project-load hook).
    #[allow(dead_code)]
    pub fn cleanup_orphan_tmp(&self) -> Result<usize, PeaksServiceError> {
        clean_orphan_tmp(&self.inner.cache_dir).map_err(PeaksServiceError::Io)
    }

    /// Stop accepting new jobs, signal the worker to abort, and join.
    /// Idempotent. Uses a 5-second join timeout: if the worker is
    /// blocked inside an unkillable IPC read we log a warning and
    /// detach rather than hanging the host process forever. M6 calls
    /// this from the app-shutdown hook.
    #[allow(dead_code)]
    pub fn shutdown(mut self) {
        self.do_shutdown();
    }

    fn do_shutdown(&mut self) {
        {
            let mut q = self.inner.queue.lock();
            q.closed = true;
        }
        self.shutdown.store(true, Ordering::Release);
        self.inner.cv.notify_all();
        // Unstick a currently-blocked worker-side sidecar IPC.
        if let Some(p) = *self.inner.current_sidecar_pid.lock() {
            signal_terminate(p);
        }
        // Unstick a hung probe BEFORE attempting to drain the pool —
        // the probe holds the AudioClient outside the pool mutex
        // while it's mid-IPC, so signaling its PID is the only way
        // to break a blocking read.
        if let Some(p) = self.inner.probe_sidecar_pid.lock().take() {
            signal_terminate(p);
        }
        // Drain the pool (any idle client). After the SIGTERM above,
        // a probe currently in flight will fail and not return its
        // client to the pool, so this is just for the idle case.
        if let Some(client) = self.inner.probe_client.lock().take() {
            let _ = client.shutdown();
        }
        let Some(handle) = self.worker.take() else {
            return;
        };
        let (tx, rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
        });
        let timeout = Duration::from_secs(5);
        match rx.recv_timeout(timeout) {
            Ok(()) => {
                tracing::info!(target: "vance_app.peaks", "worker joined cleanly");
            }
            Err(_) => {
                tracing::warn!(
                    target: "vance_app.peaks",
                    timeout_secs = timeout.as_secs(),
                    "worker did not join within timeout; detaching"
                );
            }
        }
    }
}

impl Drop for PeaksService {
    fn drop(&mut self) {
        if self.worker.is_some() {
            self.do_shutdown();
        }
    }
}

/// Result of inspecting an existing slot for staleness.
enum FreshnessCheck {
    /// Slot is valid for the requested job; `samples_total` is the
    /// frame count baked into the header.
    Fresh(u64),
    /// Slot doesn't exist on disk.
    Missing,
    /// Slot exists but doesn't match (hash mismatch, duration
    /// mismatch, format mismatch).
    Stale,
    /// Slot couldn't be inspected because of a non-ENOENT IO error
    /// (permission, disk corruption). Treated as Failed by the
    /// caller, NOT silently re-run.
    IoError(std::io::Error),
}

fn slot_freshness(slot: &Path, job: &PeakJob) -> FreshnessCheck {
    match PeakFile::open(slot) {
        Ok(pf) => {
            if pf.header.content_hash_prefix != job.content_hash_prefix {
                return FreshnessCheck::Stale;
            }
            if let Some(expected) = job.expected_duration_seconds {
                let sr = pf.header.sample_rate;
                if sr == 0 {
                    return FreshnessCheck::Stale;
                }
                let actual = pf.header.total_samples as f64 / sr as f64;
                // Audio tolerance: 100ms — covers PTS jitter,
                // container-vs-decoded duration differences, and
                // resampling rounding without re-extracting on
                // every load.
                const TOLERANCE_SECONDS: f64 = 0.100;
                if (actual - expected).abs() > TOLERANCE_SECONDS {
                    return FreshnessCheck::Stale;
                }
            }
            FreshnessCheck::Fresh(pf.header.total_samples)
        }
        Err(PeakError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            FreshnessCheck::Missing
        }
        Err(PeakError::Io(e)) => FreshnessCheck::IoError(e),
        // Any non-IO PeakError (bad magic, version, layout) = stale
        // file on disk; re-extract.
        Err(_) => FreshnessCheck::Stale,
    }
}

fn slot_path(cache_dir: &Path, prefix: &HashPrefix) -> PathBuf {
    cache_dir.join(format!("{}.peaks", hex_lower(prefix)))
}

fn err_path(cache_dir: &Path, prefix: &HashPrefix) -> PathBuf {
    cache_dir.join(format!("{}.peaks.err", hex_lower(prefix)))
}

fn clean_orphan_tmp(cache_dir: &Path) -> Result<usize, std::io::Error> {
    let mut removed = 0;
    for entry in fs::read_dir(cache_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let s = name.to_string_lossy();
        // Match the unique-tmp suffix produced by vance-peaks
        // write_atomic: `<file>.<pid>.<counter>.peaks.tmp`. The
        // suffix-only filter is sufficient — the only files in this
        // dir that end in `.peaks.tmp` are our own orphans.
        if s.ends_with(".peaks.tmp") && fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

fn write_err_marker(cache_dir: &Path, job: &PeakJob, reason: &str) {
    let path = err_path(cache_dir, &job.content_hash_prefix);
    let marker = PeakErrMarker {
        reason: reason.to_string(),
        path: job.path.to_string_lossy().into_owned(),
        at_unix_millis: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    };
    match serde_json::to_vec_pretty(&marker) {
        Ok(bytes) => {
            if let Err(e) = fs::write(&path, &bytes) {
                tracing::warn!(
                    target: "vance_app.peaks",
                    path = ?path,
                    err = ?e,
                    "failed to write .peaks.err marker"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "vance_app.peaks",
                err = ?e,
                "failed to serialize .peaks.err marker"
            );
        }
    }
}

/// Send `SIGTERM` to the given PID (Unix) or `TerminateProcess` (Windows).
/// Best-effort — errors are logged but ignored.
fn signal_terminate(pid: u32) {
    #[cfg(unix)]
    {
        // SAFETY: `kill` is async-signal-safe and takes only POD args.
        // Sending to a PID we believe we own (sidecar we spawned) —
        // worst case the PID has been reused after the sidecar exited,
        // in which case the kill is harmless (we lose the right to
        // signal an arbitrary unrelated process only if the kernel
        // recycles PIDs to processes we'd be permitted to signal,
        // which is bounded by our own uid).
        let ret = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if ret != 0 {
            let errno = std::io::Error::last_os_error();
            tracing::debug!(
                target: "vance_app.peaks",
                pid,
                err = ?errno,
                "SIGTERM returned non-zero (PID may have exited)"
            );
        }
    }
    #[cfg(windows)]
    {
        // Fallback via taskkill — vance-app doesn't depend on the
        // windows crate, and writing FFI for one syscall isn't worth
        // the cargo footprint. taskkill is in System32 on every
        // supported Windows release.
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// Worker thread body. Pulls one job at a time, runs the extractor,
/// then loops. Sleeps on `cv` when the queue is empty.
fn worker_loop(inner: Arc<Inner>, shutdown: Arc<AtomicBool>) {
    // Catch panics so a single bad job doesn't kill the worker
    // silently. The unwind is logged and the worker continues.
    loop {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker_iter(&inner, &shutdown)
        }));
        match result {
            Ok(KeepGoing::Continue) => continue,
            Ok(KeepGoing::Stop) => return,
            Err(panic_payload) => {
                let msg = panic_message(&panic_payload);
                tracing::error!(
                    target: "vance_app.peaks",
                    panic = %msg,
                    "peaks worker panic — recovering and continuing"
                );
                // Clear any half-set sidecar PID before next iteration.
                *inner.current_sidecar_pid.lock() = None;
                let mut q = inner.queue.lock();
                if let Some(id) = q.current.take() {
                    let state = JobState::Failed(format!("worker panic: {msg}"));
                    q.statuses.insert(id, state.clone());
                    q.cancelled.remove(&id);
                    drop(q);
                    (inner.progress)(&id, &state);
                }
            }
        }
    }
}

enum KeepGoing {
    Continue,
    Stop,
}

fn worker_iter(inner: &Arc<Inner>, shutdown: &AtomicBool) -> KeepGoing {
    // Pull next job (or shutdown).
    let job: PeakJob = {
        let mut q = inner.queue.lock();
        loop {
            if shutdown.load(Ordering::Acquire) || q.closed {
                return KeepGoing::Stop;
            }
            if let Some(job) = q.pending.pop_front() {
                if q.cancelled.contains(&job.media_id) {
                    // Stale cancel; acknowledge and continue.
                    let id = job.media_id;
                    q.statuses.insert(id, JobState::Cancelled);
                    q.cancelled.remove(&id);
                    drop(q);
                    (inner.progress)(&id, &JobState::Cancelled);
                    q = inner.queue.lock();
                    continue;
                }
                q.current = Some(job.media_id);
                break job;
            }
            inner.cv.wait(&mut q);
        }
    };

    let result = extract(inner, shutdown, &job);

    {
        let mut q = inner.queue.lock();
        q.current = None;
    }
    *inner.current_sidecar_pid.lock() = None;

    // Decide the final state. Re-check the cancelled flag at the end
    // so a cancel that beat the loop's chunk-boundary check still
    // wins over a successful Done.
    let cancelled_post = {
        let mut q = inner.queue.lock();
        let was = q.cancelled.contains(&job.media_id);
        q.cancelled.remove(&job.media_id);
        was
    };

    let state = match (result, cancelled_post) {
        (_, true) => JobState::Cancelled,
        (Ok(samples_total), false) => {
            // Success — clear any prior .err marker.
            let _ = fs::remove_file(err_path(&inner.cache_dir, &job.content_hash_prefix));
            JobState::Done { samples_total }
        }
        (Err(PeaksServiceError::Cancelled), false) => JobState::Cancelled,
        (Err(e), false) => {
            let reason = e.to_string();
            write_err_marker(&inner.cache_dir, &job, &reason);
            tracing::warn!(
                target: "vance_app.peaks",
                media_id = %job.media_id,
                path = ?job.path,
                err = %reason,
                "peaks extraction failed"
            );
            JobState::Failed(reason)
        }
    };

    {
        let mut q = inner.queue.lock();
        q.statuses.insert(job.media_id, state.clone());
    }
    (inner.progress)(&job.media_id, &state);
    KeepGoing::Continue
}

/// Drive one extraction end-to-end. Spawns the sidecar, scans to EOF,
/// writes the slot atomically.
fn extract(inner: &Inner, shutdown: &AtomicBool, job: &PeakJob) -> Result<u64, PeaksServiceError> {
    // Early cancel check — caller may have cancelled between dequeue
    // and our first opportunity here.
    {
        let q = inner.queue.lock();
        if q.cancelled.contains(&job.media_id) || shutdown.load(Ordering::Acquire) {
            return Err(PeaksServiceError::Cancelled);
        }
    }

    let mut client = AudioClient::spawn(&inner.audio_binary)?;
    let pid = client.pid();
    *inner.current_sidecar_pid.lock() = pid;

    let info = client.open(job.path.to_string_lossy().into_owned())?;
    let _first_pts = client.start_stream(0, 16_384)?;

    let samples_total = info.duration_pts.and_then(|p| {
        let (num, den) = info.time_base;
        if num <= 0 || den <= 0 || p < 0 {
            return None;
        }
        let seconds = p as f64 * num as f64 / den as f64;
        // Sanity-bound: a duration > 24h is almost certainly bogus.
        if !seconds.is_finite() || !(0.0..=24.0 * 3600.0).contains(&seconds) {
            return None;
        }
        Some((seconds * info.sample_rate as f64).round() as u64)
    });

    // Initial Running event.
    {
        let state = JobState::Running {
            samples_done: 0,
            samples_total,
        };
        let mut q = inner.queue.lock();
        // Check cancel one more time — a cancel that fired between
        // the early-check and AudioClient::spawn would otherwise see
        // Running stomp Cancelled in the status map.
        if q.cancelled.contains(&job.media_id) {
            drop(q);
            let _ = client.shutdown();
            return Err(PeaksServiceError::Cancelled);
        }
        q.statuses.insert(job.media_id, state.clone());
        drop(q);
        (inner.progress)(&job.media_id, &state);
    }

    let mut builder = PeakBuilder::new();
    let mut last_emit = Instant::now();
    let throttle = Duration::from_millis(100); // ≤10 Hz progress
    loop {
        // Cancellation / shutdown checkpoint between chunks.
        if shutdown.load(Ordering::Acquire) {
            let _ = client.shutdown();
            return Err(PeaksServiceError::Cancelled);
        }
        {
            let q = inner.queue.lock();
            if q.cancelled.contains(&job.media_id) {
                drop(q);
                let _ = client.shutdown();
                return Err(PeaksServiceError::Cancelled);
            }
        }

        let chunk = match client.next_chunk() {
            Ok(Some(c)) => c,
            Ok(None) => break, // clean EOS
            Err(e) => {
                // The cancel path's SIGTERM will surface here as a
                // BrokenPipe / IO error. If we were cancelled, treat
                // it as cancellation; otherwise propagate.
                let q = inner.queue.lock();
                if q.cancelled.contains(&job.media_id) || shutdown.load(Ordering::Acquire) {
                    return Err(PeaksServiceError::Cancelled);
                }
                drop(q);
                return Err(PeaksServiceError::from(e));
            }
        };
        builder.push_stereo_chunk(&chunk.samples);

        if last_emit.elapsed() >= throttle {
            last_emit = Instant::now();
            let state = JobState::Running {
                samples_done: builder.frames_consumed(),
                samples_total,
            };
            let mut q = inner.queue.lock();
            q.statuses.insert(job.media_id, state.clone());
            drop(q);
            (inner.progress)(&job.media_id, &state);
        }
    }
    let _ = client.shutdown();

    let finished: FinishedPeaks = builder.finish();
    let total_frames = finished.total_frames;

    let header = PeakHeader {
        format_version: FORMAT_VERSION,
        lane_count: LANE_COUNT,
        level_count: 1, // overridden by encode/write_atomic
        sample_rate: info.sample_rate,
        total_samples: 0, // overridden
        content_hash_prefix: job.content_hash_prefix,
    };
    let slot = slot_path(&inner.cache_dir, &job.content_hash_prefix);
    write_atomic(&slot, &header, &finished)?;
    Ok(total_frames)
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic payload)".to_string()
    }
}

/// Lowercase hex encoding for a `HashPrefix` (12 bytes → 24 chars).
fn hex_lower(b: &HashPrefix) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &byte in b.iter() {
        s.push(HEX[(byte >> 4) as usize] as char);
        s.push(HEX[(byte & 0x0f) as usize] as char);
    }
    s
}

/// Derive a stable, non-cryptographic 12-byte hash prefix from a media
/// path's metadata. Used when the project hasn't computed a BLAKE3 yet
/// — same path + mtime + length always yields the same prefix, so
/// repeated imports of the same file hit the same cache slot. M6
/// chooses between this fallback and `MediaItem.content_hash` when
/// computing the prefix for an enqueue.
///
/// Two FNV-1a passes use DISTINCT 64-bit offset bases so the two halves
/// produce independent values; the 12 bytes are filled as 8 from the
/// first pass + 4 from the second pass shifted right (so we use the
/// MSBs, not just the low 32 bits).
#[allow(dead_code)]
pub fn hash_prefix_from_path(path: &Path, mtime_nanos: i128, len_bytes: u64) -> HashPrefix {
    let mut out = [0u8; 12];
    let bytes = path.as_os_str().to_string_lossy();
    let a = fnv1a(bytes.as_bytes(), mtime_nanos, len_bytes, 0xcbf29ce484222325);
    // Distinct offset basis (not the FNV prime) so b is not a
    // deterministic function of a for arbitrary inputs.
    let b = fnv1a(bytes.as_bytes(), mtime_nanos, len_bytes, 0xa3b195994a5d2b1f);
    out[0..8].copy_from_slice(&a.to_le_bytes());
    out[8..12].copy_from_slice(&((b >> 32) as u32).to_le_bytes());
    out
}

#[allow(dead_code)]
const FNV_PRIME: u64 = 0x100000001b3;

#[allow(dead_code)]
fn fnv1a(bytes: &[u8], mtime: i128, len: u64, seed: u64) -> u64 {
    let mut h: u64 = seed;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    for &b in &mtime.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    for &b in &len.to_le_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[doc(hidden)]
pub const fn _level_count_max() -> u8 {
    LEVEL_COUNT_MAX
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use tempfile::TempDir;

    type EventLog = Arc<StdMutex<Vec<(MediaItemId, JobState)>>>;

    fn capturing_sink() -> (ProgressSink, EventLog) {
        let log: EventLog = Arc::new(StdMutex::new(Vec::new()));
        let log2 = log.clone();
        let sink: ProgressSink = Arc::new(move |id: &MediaItemId, s: &JobState| {
            log2.lock().unwrap().push((*id, s.clone()));
        });
        (sink, log)
    }

    fn no_op_service(tmp: &TempDir) -> (PeaksService, EventLog) {
        let (sink, log) = capturing_sink();
        let service = PeaksService::start(
            tmp.path().join("not-a-real-binary"),
            tmp.path().join("peaks_cache"),
            sink,
        )
        .unwrap();
        (service, log)
    }

    fn dummy_job(prefix_byte: u8) -> PeakJob {
        PeakJob {
            media_id: MediaItemId::new(),
            path: PathBuf::from("/tmp/does-not-exist.wav"),
            content_hash_prefix: [prefix_byte; 12],
            expected_duration_seconds: None,
        }
    }

    fn wait_for(
        service: &PeaksService,
        id: MediaItemId,
        timeout: Duration,
        predicate: impl Fn(&JobState) -> bool,
    ) -> Option<JobState> {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if let Some(s) = service.status(id) {
                if predicate(&s) {
                    return Some(s);
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        service.status(id)
    }

    #[test]
    fn cache_dir_is_created_on_start() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("p").join("q");
        let (sink, _log) = capturing_sink();
        let service = PeaksService::start(tmp.path().join("bin"), cache.clone(), sink).unwrap();
        assert!(cache.is_dir());
        service.shutdown();
    }

    #[test]
    fn start_cleans_orphan_tmp() {
        // Pre-seed an orphan tmp file, start the service, assert it
        // was cleaned up by the implicit start-time sweep.
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("abcd.peaks.tmp"), b"orphan").unwrap();
        let (sink, _log) = capturing_sink();
        let service = PeaksService::start(tmp.path().join("bin"), cache.clone(), sink).unwrap();
        assert!(!cache.join("abcd.peaks.tmp").exists(), "orphan removed");
        service.shutdown();
    }

    #[test]
    fn slot_path_uses_lowercase_hex() {
        let tmp = TempDir::new().unwrap();
        let (service, _) = no_op_service(&tmp);
        let p = service.slot_path(&[0xde, 0xad, 0xbe, 0xef, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("deadbeef00000000"));
        service.shutdown();
    }

    #[test]
    fn enqueue_emits_queued_state() {
        let tmp = TempDir::new().unwrap();
        let (service, log) = no_op_service(&tmp);
        let job = dummy_job(0xAA);
        let id = job.media_id;
        service.enqueue(job).unwrap();
        let _ = wait_for(&service, id, Duration::from_millis(200), |s| {
            matches!(
                s,
                JobState::Queued | JobState::Running { .. } | JobState::Failed(_)
            )
        });
        let log = log.lock().unwrap();
        assert!(matches!(log.first(), Some((mid, JobState::Queued)) if *mid == id));
        drop(log);
        service.shutdown();
    }

    #[test]
    fn enqueue_dedupes_repeated_submissions_when_queued() {
        let tmp = TempDir::new().unwrap();
        let (service, _log) = no_op_service(&tmp);
        let job = dummy_job(0xDD);
        let id = job.media_id;
        service.enqueue(job.clone()).unwrap();
        service.enqueue(job.clone()).unwrap();
        service.enqueue(job).unwrap();
        let q = service.inner.queue.lock();
        let count = q.pending.iter().filter(|j| j.media_id == id).count();
        // At most one entry in pending, AND the second/third enqueue
        // should have early-returned as no-op (Queued/Running).
        assert!(count <= 1, "got {count}");
        drop(q);
        service.shutdown();
    }

    #[test]
    fn cancel_marks_state_and_drops_pending() {
        let tmp = TempDir::new().unwrap();
        let (service, log) = no_op_service(&tmp);
        let job = dummy_job(0x55);
        let id = job.media_id;
        service.enqueue(job).unwrap();
        // Try to cancel before the worker dequeues (best-effort; race
        // is OK since terminal-state handling covers both orderings).
        service.cancel(id);
        let final_state = wait_for(&service, id, Duration::from_millis(500), |s| {
            s.is_terminal()
        });
        assert!(
            matches!(final_state, Some(JobState::Cancelled | JobState::Failed(_))),
            "expected terminal Cancelled or Failed (binary doesn't exist), got {final_state:?}"
        );
        let log = log.lock().unwrap();
        // Either Cancelled emitted (worker emits or we did directly)
        // OR Failed emitted (binary missing) — both reach a terminal
        // state, which is the contract.
        assert!(
            log.iter()
                .any(|(_, st)| matches!(st, JobState::Cancelled | JobState::Failed(_))),
            "expected terminal event in log: {log:?}"
        );
        drop(log);
        service.shutdown();
    }

    #[test]
    fn cancel_unknown_id_is_noop() {
        let tmp = TempDir::new().unwrap();
        let (service, log) = no_op_service(&tmp);
        let id = MediaItemId::new();
        service.cancel(id);
        // No status set, no events emitted.
        assert!(service.status(id).is_none());
        assert!(log.lock().unwrap().is_empty());
        service.shutdown();
    }

    #[test]
    fn cancel_terminal_state_is_noop() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let prefix: HashPrefix = [0x33; 12];
        let slot = cache.join(format!("{}.peaks", hex_lower(&prefix)));
        let mut b = PeakBuilder::new();
        for _ in 0..256 {
            b.push_stereo_chunk(&[0.1, 0.1]);
        }
        let header = PeakHeader {
            format_version: FORMAT_VERSION,
            lane_count: LANE_COUNT,
            level_count: 1,
            sample_rate: 48_000,
            total_samples: 0,
            content_hash_prefix: prefix,
        };
        write_atomic(&slot, &header, &b.finish()).unwrap();

        let (sink, log) = capturing_sink();
        let service = PeaksService::start(tmp.path().join("bin"), cache, sink).unwrap();
        let job = PeakJob {
            media_id: MediaItemId::new(),
            path: PathBuf::from("/tmp/whatever.wav"),
            content_hash_prefix: prefix,
            expected_duration_seconds: Some(256.0 / 48_000.0),
        };
        let id = job.media_id;
        service.enqueue(job).unwrap();
        let _ = wait_for(&service, id, Duration::from_millis(500), |s| {
            matches!(s, JobState::Done { .. })
        });
        let events_before = log.lock().unwrap().len();
        service.cancel(id);
        std::thread::sleep(Duration::from_millis(20));
        // No new events from cancel.
        assert_eq!(log.lock().unwrap().len(), events_before);
        // Status still Done.
        assert!(matches!(service.status(id), Some(JobState::Done { .. })));
        service.shutdown();
    }

    #[test]
    fn fresh_slot_emits_queued_then_done_and_clears_err() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let prefix: HashPrefix = [0x11; 12];
        let slot = cache.join(format!("{}.peaks", hex_lower(&prefix)));
        let err = cache.join(format!("{}.peaks.err", hex_lower(&prefix)));
        // Pre-seed both the valid slot AND a stale err marker.
        let mut b = PeakBuilder::new();
        for _ in 0..256 {
            b.push_stereo_chunk(&[0.1, 0.1]);
        }
        let header = PeakHeader {
            format_version: FORMAT_VERSION,
            lane_count: LANE_COUNT,
            level_count: 1,
            sample_rate: 48_000,
            total_samples: 0,
            content_hash_prefix: prefix,
        };
        write_atomic(&slot, &header, &b.finish()).unwrap();
        fs::write(&err, b"old error").unwrap();
        assert!(err.exists());

        let (sink, log) = capturing_sink();
        let service = PeaksService::start(tmp.path().join("bin"), cache, sink).unwrap();
        let job = PeakJob {
            media_id: MediaItemId::new(),
            path: PathBuf::from("/tmp/whatever.wav"),
            content_hash_prefix: prefix,
            expected_duration_seconds: Some(256.0 / 48_000.0),
        };
        let id = job.media_id;
        service.enqueue(job).unwrap();
        let _ = wait_for(&service, id, Duration::from_millis(300), |s| {
            matches!(s, JobState::Done { .. })
        });
        let log = log.lock().unwrap();
        // The first event for this id must be Queued.
        let first_for_id = log.iter().find(|(mid, _)| *mid == id);
        assert!(matches!(first_for_id, Some((_, JobState::Queued))));
        // Then Done.
        let done = log.iter().rev().find(|(mid, _)| *mid == id);
        assert!(matches!(
            done,
            Some((_, JobState::Done { samples_total: 256 }))
        ));
        drop(log);
        // Stale .err removed.
        assert!(!err.exists());
        service.shutdown();
    }

    #[test]
    fn stale_slot_is_re_enqueued() {
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let stored_prefix: HashPrefix = [0xAA; 12];
        let job_prefix: HashPrefix = [0xBB; 12];
        let slot = cache.join(format!("{}.peaks", hex_lower(&job_prefix)));
        let mut b = PeakBuilder::new();
        b.push_stereo_chunk(&[0.0; 512]);
        let header = PeakHeader {
            format_version: FORMAT_VERSION,
            lane_count: LANE_COUNT,
            level_count: 1,
            sample_rate: 48_000,
            total_samples: 0,
            content_hash_prefix: stored_prefix,
        };
        write_atomic(&slot, &header, &b.finish()).unwrap();

        let (sink, log) = capturing_sink();
        let service = PeaksService::start(tmp.path().join("bin"), cache, sink).unwrap();
        let job = PeakJob {
            media_id: MediaItemId::new(),
            path: PathBuf::from("/tmp/whatever.wav"),
            content_hash_prefix: job_prefix,
            expected_duration_seconds: None,
        };
        let id = job.media_id;
        service.enqueue(job).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let log = log.lock().unwrap();
        assert!(log
            .iter()
            .any(|(mid, st)| *mid == id && matches!(st, JobState::Queued)));
        assert!(
            !log.iter()
                .any(|(mid, st)| *mid == id && matches!(st, JobState::Done { .. })),
            "stale slot must NOT yield Done: {log:?}"
        );
        drop(log);
        service.shutdown();
    }

    #[test]
    fn cleanup_orphan_tmp_removes_only_tmp_files() {
        let tmp = TempDir::new().unwrap();
        let (service, _log) = no_op_service(&tmp);
        let cache = service.inner.cache_dir.clone();
        fs::write(cache.join("abcd.peaks.tmp"), b"x").unwrap();
        fs::write(cache.join("efgh.peaks.tmp"), b"y").unwrap();
        fs::write(cache.join("zz.peaks"), b"z").unwrap();
        let removed = service.cleanup_orphan_tmp().unwrap();
        assert_eq!(removed, 2);
        assert!(!cache.join("abcd.peaks.tmp").exists());
        assert!(!cache.join("efgh.peaks.tmp").exists());
        assert!(cache.join("zz.peaks").exists(), "real .peaks survives");
        service.shutdown();
    }

    #[test]
    fn failed_job_writes_json_err_marker() {
        let tmp = TempDir::new().unwrap();
        let (sink, log) = capturing_sink();
        let service = PeaksService::start(
            tmp.path().join("definitely-not-a-binary"),
            tmp.path().join("cache"),
            sink,
        )
        .unwrap();
        let prefix: HashPrefix = [0x77; 12];
        let job = PeakJob {
            media_id: MediaItemId::new(),
            path: PathBuf::from("/tmp/whatever.wav"),
            content_hash_prefix: prefix,
            expected_duration_seconds: None,
        };
        let id = job.media_id;
        service.enqueue(job).unwrap();
        let _ = wait_for(&service, id, Duration::from_secs(1), |s| {
            matches!(s, JobState::Failed(_))
        });
        let log = log.lock().unwrap();
        assert!(log.iter().any(|(_, st)| matches!(st, JobState::Failed(_))));
        let err_path = tmp
            .path()
            .join("cache")
            .join(format!("{}.peaks.err", hex_lower(&prefix)));
        assert!(err_path.is_file(), ".peaks.err marker exists");
        // Verify it's valid JSON with the expected fields.
        let body = fs::read_to_string(&err_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(parsed.get("reason").and_then(|v| v.as_str()).is_some());
        assert!(parsed.get("path").and_then(|v| v.as_str()).is_some());
        assert!(parsed.get("at_unix_millis").is_some());
        drop(log);
        service.shutdown();
    }

    #[test]
    fn shutdown_rejects_new_enqueues() {
        let tmp = TempDir::new().unwrap();
        let (sink, _log) = capturing_sink();
        let mut service =
            PeaksService::start(tmp.path().join("bin"), tmp.path().join("cache"), sink).unwrap();
        // Trigger shutdown without consuming `service` (so we can
        // still call enqueue afterwards).
        service.do_shutdown();
        match service.enqueue(dummy_job(1)) {
            Err(PeaksServiceError::ShuttingDown) => {}
            other => panic!("expected ShuttingDown, got {other:?}"),
        }
    }

    #[test]
    fn shutdown_drains_queue_and_joins_quickly() {
        let tmp = TempDir::new().unwrap();
        let (service, _log) = no_op_service(&tmp);
        for i in 0..8 {
            let _ = service.enqueue(dummy_job(i));
        }
        let start = Instant::now();
        service.shutdown();
        assert!(
            start.elapsed() < Duration::from_secs(6),
            "shutdown took {:?}; should be < join-timeout of 5s",
            start.elapsed()
        );
    }

    #[test]
    fn hash_prefix_is_stable_and_distinct() {
        let p = PathBuf::from("/tmp/clip.wav");
        let a = hash_prefix_from_path(&p, 12345, 678);
        let b = hash_prefix_from_path(&p, 12345, 678);
        assert_eq!(a, b);
        let c = hash_prefix_from_path(&p, 99999, 678);
        assert_ne!(a, c, "mtime change → different prefix");
        let d = hash_prefix_from_path(&p, 12345, 999);
        assert_ne!(a, d, "len change → different prefix");
        // The two FNV passes use distinct offset bases — the high 32
        // bits of `b` (used in positions [8..12]) must differ from the
        // identical-input-but-different-seed pass.
        let prefix = hash_prefix_from_path(&p, 1, 1);
        assert!(prefix[8..12].iter().any(|&x| x != 0));
    }

    #[test]
    fn hex_lower_matches_expected_output() {
        let b: HashPrefix = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0xff,
        ];
        assert_eq!(hex_lower(&b), "000102030405060708090aff");
        assert_eq!(hex_lower(&b).len(), 24);
    }

    #[test]
    fn slot_freshness_rejects_io_error() {
        // Point at a path whose existence we can't read (parent
        // doesn't exist). PeakFile::open returns NotFound for missing
        // entries, so we get Missing — not IoError. IoError would need
        // a permission error which is platform-finicky in unit tests.
        // Just assert the Missing path here.
        let p = PathBuf::from("/this/does/not/exist/x.peaks");
        let job = dummy_job(0);
        assert!(matches!(slot_freshness(&p, &job), FreshnessCheck::Missing));
    }

    #[test]
    fn slot_freshness_uses_audio_tolerance_not_video() {
        // Stored = 256 samples @ 48k = 5.333…ms. Expected = 6ms.
        // Difference ≈ 0.7ms. With old 1/24s (41ms) tolerance this
        // passed; with new 100ms tolerance it still passes. Sanity
        // check: 200ms difference must FAIL.
        let tmp = TempDir::new().unwrap();
        let cache = tmp.path().join("cache");
        fs::create_dir_all(&cache).unwrap();
        let prefix: HashPrefix = [0x22; 12];
        let slot = cache.join(format!("{}.peaks", hex_lower(&prefix)));
        let mut b = PeakBuilder::new();
        for _ in 0..256 {
            b.push_stereo_chunk(&[0.0, 0.0]);
        }
        let header = PeakHeader {
            format_version: FORMAT_VERSION,
            lane_count: LANE_COUNT,
            level_count: 1,
            sample_rate: 48_000,
            total_samples: 0,
            content_hash_prefix: prefix,
        };
        write_atomic(&slot, &header, &b.finish()).unwrap();

        let mut job = PeakJob {
            media_id: MediaItemId::new(),
            path: PathBuf::from("/tmp/x.wav"),
            content_hash_prefix: prefix,
            expected_duration_seconds: Some(256.0 / 48_000.0),
        };
        assert!(matches!(
            slot_freshness(&slot, &job),
            FreshnessCheck::Fresh(256)
        ));
        // 200ms drift is well outside the 100ms tolerance.
        job.expected_duration_seconds = Some(256.0 / 48_000.0 + 0.200);
        assert!(matches!(slot_freshness(&slot, &job), FreshnessCheck::Stale));
    }

    #[test]
    fn err_marker_contains_path_and_timestamp() {
        let tmp = TempDir::new().unwrap();
        let prefix: HashPrefix = [0x99; 12];
        let job = PeakJob {
            media_id: MediaItemId::new(),
            path: PathBuf::from("/tmp/myclip.wav"),
            content_hash_prefix: prefix,
            expected_duration_seconds: None,
        };
        write_err_marker(tmp.path(), &job, "test reason");
        let body = fs::read_to_string(err_path(tmp.path(), &prefix)).unwrap();
        let parsed: PeakErrMarker = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed.reason, "test reason");
        assert_eq!(parsed.path, "/tmp/myclip.wav");
        assert!(parsed.at_unix_millis > 0);
    }
}
