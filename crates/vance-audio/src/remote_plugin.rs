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

/// Per-call hang budget for a `process` block. Must stay well under the
/// master ring depth (~250 ms) so a hung plugin is bypassed before the
/// ring drains and cpal underruns. A real block is sub-millisecond.
const PROCESS_TIMEOUT_MS: u64 = 120;

/// Per-call hang budget for `instantiate` — generous because it runs the
/// plugin's `dlopen` + activate inside the sidecar, which can legitimately
/// take a while for a heavy plugin.
const INSTANTIATE_TIMEOUT_MS: u64 = 5_000;

/// Per-call hang budget for cheap control calls (set_param / destroy).
const CONTROL_TIMEOUT_MS: u64 = 500;

/// Watchdog poll interval. Kept small so the effective stall on a hang is
/// close to the configured per-call budget rather than budget + 100 ms.
const WATCHDOG_POLL_MS: u64 = 15;

/// After this many consecutive respawn attempts without a single
/// successful `process`, give up on the sidecar (a plugin that crashes the
/// host on every load must not spin a respawn loop forever).
const MAX_CONSECUTIVE_RESPAWNS: u32 = 5;

/// State shared between the host (fold thread) and its watchdog thread.
struct Watch {
    /// Monotonic epoch the timestamps below are measured against.
    epoch: Instant,
    /// `epoch`-relative ms deadline of the current in-flight IPC call, or
    /// 0 when idle. The watchdog kills the sidecar once `now >= deadline`.
    in_flight_deadline: AtomicU64,
    /// Sequence number of the current in-flight call, bumped on each call.
    /// The watchdog captures it with the deadline and re-validates it just
    /// before killing, so a call that completes at the timeout boundary is
    /// never killed (and a different, later call can't be mistaken for the
    /// hung one).
    in_flight_seq: AtomicU64,
    /// PID of the live sidecar (0 = none / claimed). Killed only via an
    /// atomic swap-to-0 so the watchdog and the reaper can never both act.
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
    /// Extra env vars to set on each spawned sidecar (e.g. `LV2_PATH`).
    /// Empty in normal use; set by tests so they needn't mutate the
    /// process-global environment.
    spawn_env: Vec<(String, String)>,
    client: Option<PluginClient>,
    generation: u64,
    /// Count of successful instantiate calls (diagnostics/tests — a param
    /// change must NOT bump this, only an add/swap/respawn does).
    instantiations: u64,
    /// `true` once a spawn has failed (binary missing) or we've given up
    /// after too many crash-respawn cycles — we then stop retrying.
    spawn_failed: bool,
    /// Set when the sidecar dies mid-use; [`poll_recovery`] respawns it.
    /// Distinct from "never spawned" (lazy) so we only respawn after a
    /// genuine crash, not before first use.
    dead: bool,
    /// Consecutive respawns without a successful `process`. Bounds a
    /// crash-on-load loop.
    consecutive_respawns: u32,
    watch: Arc<Watch>,
    watchdog: Option<JoinHandle<()>>,
}

impl RemotePluginHost {
    /// Create a host that will lazily spawn `exe` on first use. The
    /// sidecar is configured for `sample_rate` and a maximum process
    /// block of `max_block` frames.
    pub fn new(exe: PathBuf, sample_rate: f32, max_block: u32) -> Self {
        Self::new_with_env(exe, sample_rate, max_block, Vec::new())
    }

    /// Like [`RemotePluginHost::new`] but sets extra environment variables
    /// on every spawned sidecar. Used by tests to pass `LV2_PATH` to the
    /// sandbox without a process-global `set_var` (which races with the
    /// concurrent spawns of sibling tests in the same binary).
    pub fn new_with_env(
        exe: PathBuf,
        sample_rate: f32,
        max_block: u32,
        spawn_env: Vec<(String, String)>,
    ) -> Self {
        let watch = Arc::new(Watch {
            epoch: Instant::now(),
            in_flight_deadline: AtomicU64::new(0),
            in_flight_seq: AtomicU64::new(0),
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
            spawn_env,
            client: None,
            generation: 0,
            instantiations: 0,
            spawn_failed: false,
            dead: false,
            consecutive_respawns: 0,
            watch,
            watchdog,
        }
    }

    /// Number of successful instantiate calls so far (diagnostics/tests).
    pub fn instantiations(&self) -> u64 {
        self.instantiations
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

    /// Recover from a sidecar crash. Called at the top of every reconcile:
    /// if the sidecar died (`dead`), respawn it so the generation advances
    /// — that makes every live remote slot stale, so reconcile rebuilds
    /// (re-instantiates) it against the fresh sidecar. WITHOUT this, a
    /// crashed plugin whose spec never changes would be bypassed forever
    /// (nothing else advances the generation).
    pub fn poll_recovery(&mut self) {
        if !self.dead || self.spawn_failed {
            return;
        }
        self.consecutive_respawns += 1;
        if self.consecutive_respawns > MAX_CONSECUTIVE_RESPAWNS {
            tracing::warn!(
                attempts = self.consecutive_respawns,
                "plugin sidecar crashed repeatedly — giving up (slots bypass)"
            );
            self.spawn_failed = true;
            self.dead = false;
            return;
        }
        // ensure_client respawns + bumps the generation on success.
        self.dead = false;
        let _ = self.ensure_client();
    }

    /// Lazily (re)spawn the sidecar. Returns whether a client is now live.
    fn ensure_client(&mut self) -> bool {
        if self.client.is_some() {
            return true;
        }
        if self.spawn_failed {
            return false;
        }
        let env = self.spawn_env.iter().map(|(k, v)| (k.as_str(), v.as_str()));
        match PluginClient::spawn_with_env(&self.exe, env) {
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
    /// the child via `PluginClient`/`Sidecar` Drop. Claims the PID (swap to
    /// 0) so the watchdog can't also act on it.
    fn drop_client(&mut self) {
        self.watch.pid.swap(0, Ordering::AcqRel);
        self.client = None;
    }

    /// Run one IPC call bracketed by the watchdog in-flight markers, with a
    /// per-call hang `timeout_ms`. The watchdog only kills the sidecar
    /// while a call is genuinely in flight (deadline non-zero), at which
    /// point the fold thread is blocked here and is NOT reaping — so the
    /// kill-by-PID can't race a reap (no PID-reuse window).
    fn guarded<T>(
        &mut self,
        timeout_ms: u64,
        f: impl FnOnce(&mut PluginClient) -> Result<T, PluginClientError>,
    ) -> Option<Result<T, PluginClientError>> {
        let watch = &self.watch;
        let client = self.client.as_mut()?;
        // Publish seq before deadline so the watchdog, which reads deadline
        // first, always sees a seq that belongs to this-or-a-later call.
        watch.in_flight_seq.fetch_add(1, Ordering::AcqRel);
        let deadline = watch.epoch.elapsed().as_millis() as u64 + timeout_ms;
        watch.in_flight_deadline.store(deadline, Ordering::Release);
        let r = f(client);
        watch.in_flight_deadline.store(0, Ordering::Release);
        Some(r)
    }

    /// Classify an IPC error: a transport failure means the sidecar is
    /// gone → mark it dead so `poll_recovery` respawns it.
    fn note_error(&mut self, e: PluginClientError) {
        match e {
            PluginClientError::Ipc(_) => {
                tracing::warn!("plugin sidecar died — will respawn");
                self.drop_client();
                self.dead = true;
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
        match self.guarded(INSTANTIATE_TIMEOUT_MS, move |c| {
            c.instantiate(descriptor, sr, mb)
        }) {
            Some(Ok(id)) => {
                self.instantiations += 1;
                Some((id, gen))
            }
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
        match self.guarded(PROCESS_TIMEOUT_MS, move |c| c.process(instance, frames_in)) {
            Some(Ok(out)) => {
                // A working block clears the crash-loop counter.
                self.consecutive_respawns = 0;
                Some(out)
            }
            Some(Err(e)) => {
                self.note_error(e);
                None
            }
            None => None,
        }
    }

    /// Push one parameter value to a live instance. Best-effort: a dead
    /// sidecar or stale generation is a silent no-op (the caller has
    /// already stored the value and will re-apply on the next instance).
    pub fn set_param(&mut self, instance: Uuid, gen: u64, param_id: u32, value: f32) {
        if gen != self.generation || self.client.is_none() {
            return;
        }
        if let Some(Err(e)) = self.guarded(CONTROL_TIMEOUT_MS, move |c| {
            c.set_param(instance, param_id, value)
        }) {
            self.note_error(e);
        }
    }

    /// Best-effort destroy of an instance (ignored if the sidecar is gone).
    pub fn destroy(&mut self, instance: Uuid, gen: u64) {
        if gen != self.generation || self.client.is_none() {
            return;
        }
        if let Some(Err(e)) = self.guarded(CONTROL_TIMEOUT_MS, move |c| c.destroy(instance)) {
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
        std::thread::sleep(Duration::from_millis(WATCHDOG_POLL_MS));
        // Capture the in-flight call's deadline + seq together.
        let deadline = watch.in_flight_deadline.load(Ordering::Acquire);
        if deadline == 0 {
            continue; // idle
        }
        let seq = watch.in_flight_seq.load(Ordering::Acquire);
        let now = watch.epoch.elapsed().as_millis() as u64;
        if now < deadline {
            continue; // still within budget
        }
        // Re-validate that the SAME call is still in flight right before
        // killing — a call that finished at the boundary (deadline now 0,
        // or seq advanced) must not be killed.
        if watch.in_flight_deadline.load(Ordering::Acquire) != deadline
            || watch.in_flight_seq.load(Ordering::Acquire) != seq
        {
            continue;
        }
        // Claim the PID atomically. If the fold thread (drop_client) or a
        // prior watchdog pass already took it, we get 0 and do nothing —
        // so the kill and the reap can never both fire on the same PID.
        let pid = watch.pid.swap(0, Ordering::AcqRel);
        if pid != 0 {
            tracing::warn!(pid, "plugin sidecar hung — killing");
            kill_pid(pid);
            watch.kills.fetch_add(1, Ordering::AcqRel);
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
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
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
