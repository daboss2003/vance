//! Out-of-process plugin host — the fold thread's connection to the
//! sandboxed `vance-plugin-host` sidecar.
//!
//! Third-party CLAP plugins run IN THE SIDECAR, not in-process, so a
//! plugin crash (panic / abort / segfault) kills only the sidecar; the
//! editor survives. This type owns one long-lived sidecar connection and
//! exposes lazy-spawn + instantiate + process to the fold thread, with
//! two failure modes handled:
//!
//! - **Crash:** an IPC transport error (broken pipe) means the sidecar
//!   died. The client is dropped; the next call lazily respawns and bumps
//!   a generation counter so callers re-instantiate their plugins.
//! - **Hang:** a malicious/buggy plugin that never returns from `process`
//!   would block the fold thread forever. A watchdog thread tracks the
//!   in-flight call and, past [`HANG_TIMEOUT_MS`], kills the sidecar by
//!   PID — breaking the pipe so the blocked call returns an error and the
//!   slot bypasses (then respawns).
//!
//! The host itself is `!Send`-friendly to own on the fold thread; only
//! the small [`Watch`] block is shared with the watchdog via `Arc`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use uuid::Uuid;
use vance_audio_plugins::{PluginClient, PluginClientError, PluginDescriptor};

/// How long one `instantiate` / `process` IPC call may block before the
/// watchdog assumes the plugin hung and kills the sidecar. Generous —
/// a real block is sub-millisecond; this only fires on a true hang.
const HANG_TIMEOUT_MS: u64 = 1_000;

/// State shared between the host (fold thread) and its watchdog thread.
struct Watch {
    /// Monotonic epoch the timestamps below are measured against.
    epoch: Instant,
    /// `epoch`-relative ms (+1) when the current IPC call started, or 0
    /// when idle. The `+1` keeps a call started at t=0 distinct from idle.
    in_flight_since: AtomicU64,
    /// PID of the live sidecar (0 = none / already reaped).
    pid: AtomicU32,
    /// Set on host drop so the watchdog exits.
    stop: AtomicBool,
    /// Count of hung sidecars the watchdog has killed (diagnostics/tests).
    kills: AtomicU64,
}

/// Owns one sandboxed plugin sidecar for the fold thread.
pub struct RemotePluginHost {
    exe: PathBuf,
    sample_rate: f32,
    max_block: u32,
    client: Option<PluginClient>,
    generation: u64,
    /// `true` once a spawn has failed, so we don't fork-bomb on every
    /// block when the binary is missing.
    spawn_failed: bool,
    watch: Arc<Watch>,
    watchdog: Option<JoinHandle<()>>,
}

impl RemotePluginHost {
    /// Create a host that will lazily spawn `exe` on first use. The
    /// sidecar is configured for `sample_rate` and a maximum process
    /// block of `max_block` frames.
    pub fn new(exe: PathBuf, sample_rate: f32, max_block: u32) -> Self {
        let watch = Arc::new(Watch {
            epoch: Instant::now(),
            in_flight_since: AtomicU64::new(0),
            pid: AtomicU32::new(0),
            stop: AtomicBool::new(false),
            kills: AtomicU64::new(0),
        });
        let watchdog = {
            let watch = watch.clone();
            std::thread::Builder::new()
                .name("vance-audio.plugin-watchdog".into())
                .spawn(move || watchdog_loop(watch))
                .ok()
        };
        Self {
            exe,
            sample_rate,
            max_block: max_block.max(1),
            client: None,
            generation: 0,
            spawn_failed: false,
            watch,
            watchdog,
        }
    }

    /// Current sidecar generation. Bumps on every (re)spawn; callers
    /// store the generation an instance was created under and discard the
    /// instance when it no longer matches (the sidecar was respawned).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Number of hung sidecars the watchdog has killed so far.
    pub fn kills(&self) -> u64 {
        self.watch.kills.load(Ordering::Acquire)
    }

    /// Whether a live sidecar connection currently exists.
    pub fn is_connected(&self) -> bool {
        self.client.is_some()
    }

    /// PID of the live sidecar (0 if none). Diagnostics + tests.
    pub fn sidecar_pid(&self) -> u32 {
        self.watch.pid.load(Ordering::Acquire)
    }

    /// Lazily (re)spawn the sidecar. Returns whether a client is now live.
    fn ensure_client(&mut self) -> bool {
        if self.client.is_some() {
            return true;
        }
        if self.spawn_failed {
            return false;
        }
        match PluginClient::spawn(&self.exe) {
            Ok(client) => {
                self.watch
                    .pid
                    .store(client.pid().unwrap_or(0), Ordering::Release);
                self.client = Some(client);
                self.generation += 1;
                true
            }
            Err(e) => {
                tracing::warn!(?e, exe = %self.exe.display(), "plugin sidecar spawn failed — CLAP plugins will bypass");
                self.spawn_failed = true;
                false
            }
        }
    }

    /// Drop the current (dead) client so the next call respawns. Reaps
    /// the child via `PluginClient`/`Sidecar` Drop.
    fn drop_client(&mut self) {
        self.watch.pid.store(0, Ordering::Release);
        self.client = None;
    }

    /// Run one IPC call bracketed by the watchdog in-flight markers.
    fn guarded<T>(
        &mut self,
        f: impl FnOnce(&mut PluginClient) -> Result<T, PluginClientError>,
    ) -> Option<Result<T, PluginClientError>> {
        let watch = &self.watch;
        let client = self.client.as_mut()?;
        let stamp = watch.epoch.elapsed().as_millis() as u64 + 1;
        watch.in_flight_since.store(stamp, Ordering::Release);
        let r = f(client);
        watch.in_flight_since.store(0, Ordering::Release);
        Some(r)
    }

    /// Classify an IPC error and drop the client if the sidecar is dead.
    /// Returns nothing useful — callers always bypass on error.
    fn note_error(&mut self, e: PluginClientError) {
        match e {
            // Transport failure → the sidecar process is gone.
            PluginClientError::Ipc(_) => {
                tracing::warn!("plugin sidecar died — respawning on next use");
                self.drop_client();
            }
            // The sidecar replied with an error (e.g. unknown plugin):
            // it's alive; just this op failed.
            other => {
                tracing::debug!(?other, "plugin op failed (sidecar alive)");
            }
        }
    }

    /// Instantiate a plugin in the sidecar. Returns `(instance, generation)`
    /// on success, or `None` (caller bypasses) on any failure.
    pub fn instantiate(&mut self, descriptor: PluginDescriptor) -> Option<(Uuid, u64)> {
        if !self.ensure_client() {
            return None;
        }
        let (sr, mb, gen) = (self.sample_rate, self.max_block, self.generation);
        match self.guarded(move |c| c.instantiate(descriptor, sr, mb)) {
            Some(Ok(id)) => Some((id, gen)),
            Some(Err(e)) => {
                self.note_error(e);
                None
            }
            None => None,
        }
    }

    /// Process one interleaved-stereo block through `instance`. `gen` is
    /// the generation the instance was created under; a mismatch means
    /// the sidecar was respawned and the instance is stale → `None`.
    pub fn process(&mut self, instance: Uuid, gen: u64, frames_in: &[f32]) -> Option<Vec<f32>> {
        if gen != self.generation || self.client.is_none() {
            return None;
        }
        match self.guarded(move |c| c.process(instance, frames_in)) {
            Some(Ok(out)) => Some(out),
            Some(Err(e)) => {
                self.note_error(e);
                None
            }
            None => None,
        }
    }

    /// Best-effort destroy of an instance (ignored if the sidecar is gone).
    pub fn destroy(&mut self, instance: Uuid, gen: u64) {
        if gen != self.generation {
            return;
        }
        if let Some(Err(e)) = self.guarded(move |c| c.destroy(instance)) {
            self.note_error(e);
        }
    }
}

impl Drop for RemotePluginHost {
    fn drop(&mut self) {
        self.watch.stop.store(true, Ordering::Release);
        // Drop the client first (kills + reaps the sidecar), then join
        // the watchdog so it can't outlive us.
        self.client = None;
        if let Some(h) = self.watchdog.take() {
            let _ = h.join();
        }
    }
}

fn watchdog_loop(watch: Arc<Watch>) {
    loop {
        if watch.stop.load(Ordering::Acquire) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
        let started = watch.in_flight_since.load(Ordering::Acquire);
        if started == 0 {
            continue; // idle
        }
        let now = watch.epoch.elapsed().as_millis() as u64;
        if now.saturating_sub(started - 1) >= HANG_TIMEOUT_MS {
            let pid = watch.pid.load(Ordering::Acquire);
            if pid != 0 {
                tracing::warn!(pid, "plugin sidecar hung — killing");
                kill_pid(pid);
                watch.kills.fetch_add(1, Ordering::AcqRel);
            }
            // Don't re-fire on the same stuck call; the fold thread will
            // also clear this once its IPC call returns the broken-pipe
            // error.
            watch.in_flight_since.store(0, Ordering::Release);
        }
    }
}

/// Kill a process by PID. std exposes no portable kill-by-pid, and the
/// fold thread (which owns the killable `Child`) is blocked in the hung
/// IPC call — so the watchdog must reach for the platform primitive.
fn kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        // SIGKILL: the sidecar is a sandbox; we don't need it to clean up.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !handle.is_null() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid; // no portable kill — hang protection unavailable
    }
}
