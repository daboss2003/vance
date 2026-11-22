//! A pool of decoder sidecars shared by every layer (ADR 0007).
//!
//! An LRU of about six [`DecoderClient`]s keyed by path replaces the single
//! playback and transition decoders. Clients spawn with
//! `ClientOptions { threads: Some(thread_budget(active_layers)), .. }` (ADR
//! 0006) so decode and conversion threads share the cores, and the total
//! shared-memory frame memory is capped at about 25% of RAM.
//!
//! ## Memory budget (user decision 5)
//!
//! [`DecoderPool::shm_bytes`] counts every live decoder's region, idle or
//! checked out. Idle decoders are evicted (least recently used first) to
//! make room for a new region. When the decoders already checked out plus
//! the new region would pass [`MemoryBudget::cap_bytes`], `take`:
//!
//! - opens the proxy registered with [`DecoderPool::set_proxy_paths`]
//!   instead, so [`PooledDecoder::path`] is the proxy's path; or
//! - with no proxy registered, opens the original anyway and reports its
//!   path once through [`DecoderPool::take_proxy_requests`], so the engine
//!   can generate a proxy and show its notice.
//!
//! A proxy that won't open (missing, corrupt, or crashing the sidecar)
//! falls back to its original, is reported once through
//! [`DecoderPool::take_broken_proxies`] (and its original through
//! `take_proxy_requests`, so it is regenerated), and isn't tried again
//! until the file changes. That holds for a proxy the engine names itself
//! too ("use proxies"), when it is registered with `set_proxy_paths`.
//!
//! A region's size is only known once a file is open. The first `take` of a
//! path that lands over the cap with a proxy available therefore opens the
//! original, drops that decoder and opens the proxy; later takes go to the
//! proxy directly.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use parking_lot::Mutex;
use vance_decoder::client::ClientError;
use vance_decoder::{
    thread_budget, ClientOptions, DecoderClient, OpenError, SidecarKill, StreamInfo, ThreadBudget,
};

use crate::engine::EngineError;

// The timed file checks; a separate file, reachable as
// `crate::decoder_pool::fs_probe` (the engine uses them too).
#[path = "fs_probe.rs"]
pub mod fs_probe;

use fs_probe::{exists_within, mtime_within, TimedOut, DEFAULT_TIMEOUT};

/// Source seconds → stream PTS. The ONE rounding rule every frame provider
/// uses, so scrub, playback and export pick the same frame. Matches the
/// engine's existing `decode_at` targets: round to nearest; 0 for a
/// degenerate time base.
pub fn source_seconds_to_pts(sec: f64, time_base: (i64, i64)) -> i64 {
    let (num, den) = time_base;
    if num == 0 {
        return 0;
    }
    (sec * den as f64 / num as f64).round() as i64
}

/// A decoder checked out of the pool, with the file it has open.
///
/// Dropping it instead of handing it back with [`DecoderPool::put`] kills
/// the sidecar and still updates the pool's accounting; do that with a
/// decoder whose sidecar failed. Don't re-open another file on a
/// checked-out decoder: the pool accounts it by the file it opened.
pub struct PooledDecoder {
    /// The sidecar client.
    pub client: DecoderClient,
    /// Path currently open in `client`: the requested path, or the proxy
    /// the pool opened in its place (see the module docs).
    pub path: String,
    /// Stream info from opening `path`.
    pub info: StreamInfo,
    checkout: Checkout,
}

/// A checked-out decoder's share of the pool's accounting, given back when
/// it drops (on `put`, or wherever the decoder is dropped).
struct Checkout {
    counters: Arc<OutCounters>,
    /// Region bytes counted while the decoder is out.
    bytes: u64,
    /// The thread budget the sidecar was spawned with.
    threads: ThreadBudget,
    /// Its entry among the pool's busy sidecars, removed when this drops.
    busy: Busy,
}

impl Drop for Checkout {
    fn drop(&mut self) {
        self.counters.count.fetch_sub(1, Ordering::Relaxed);
        self.counters.bytes.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

/// The kill handles of a pool's BUSY sidecars (opening a file, or checked
/// out), reachable from any thread. A thread stopping a playback session
/// kills them when the session's thread is blocked on one (an open or a
/// read on a hung network share), so stopping never hangs on a sidecar.
/// Idle decoders aren't in it: nothing waits on them.
#[derive(Clone, Default)]
pub struct SidecarRegistry(Arc<Mutex<Registry>>);

#[derive(Default)]
struct Registry {
    next: u64,
    busy: HashMap<u64, Entry>,
    /// Set by [`SidecarRegistry::kill_busy`] until
    /// [`SidecarRegistry::resume`]: a sidecar registered meanwhile is killed
    /// at once, so a thread working through several opens can't block on
    /// the next one.
    stopping: bool,
}

struct Entry {
    kill: SidecarKill,
    /// Killed by `kill_busy` (not by its own fault). Kept until the entry
    /// goes, past `resume`: the decoder is dead whenever it is handed back.
    killed: bool,
}

impl SidecarRegistry {
    /// Kill every busy sidecar now, and any that becomes busy before
    /// [`Self::resume`]. A request a client is blocked in then returns
    /// with an IPC error. Returns how many were killed.
    pub fn kill_busy(&self) -> usize {
        let mut r = self.0.lock();
        r.stopping = true;
        for e in r.busy.values_mut() {
            e.kill.kill();
            e.killed = true;
        }
        r.busy.len()
    }

    /// Back to normal after [`Self::kill_busy`]: sidecars registered from
    /// now on live.
    pub fn resume(&self) {
        self.0.lock().stopping = false;
    }

    fn register(&self, kill: SidecarKill) -> Busy {
        let mut r = self.0.lock();
        r.next += 1;
        let id = r.next;
        let killed = r.stopping;
        if killed {
            kill.kill();
        }
        r.busy.insert(id, Entry { kill, killed });
        Busy {
            registry: self.clone(),
            id,
        }
    }
}

/// A busy sidecar's registry entry; dropping it unregisters the sidecar.
struct Busy {
    registry: SidecarRegistry,
    id: u64,
}

impl Busy {
    /// Whether [`SidecarRegistry::kill_busy`] killed this sidecar.
    fn killed_by_stop(&self) -> bool {
        self.registry
            .0
            .lock()
            .busy
            .get(&self.id)
            .is_some_and(|e| e.killed)
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        self.registry.0.lock().busy.remove(&self.id);
    }
}

/// Checked-out decoders: how many, and their region bytes.
#[derive(Default)]
struct OutCounters {
    count: AtomicUsize,
    bytes: AtomicU64,
}

/// An idle decoder, file still open.
struct Idle {
    client: DecoderClient,
    path: String,
    info: StreamInfo,
    threads: ThreadBudget,
}

/// Cap on the pool's total shared-memory frame bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryBudget {
    /// Byte cap (about 25% of RAM).
    pub cap_bytes: u64,
}

/// Physical RAM in bytes, or `None` where it can't be read.
///
/// macOS: `sysctl hw.memsize`. Linux: `MemTotal` in `/proc/meminfo`.
/// Windows: `GlobalMemoryStatusEx`'s `ullTotalPhys`.
pub fn total_ram_bytes() -> Option<u64> {
    total_ram_bytes_os()
}

#[cfg(target_os = "macos")]
fn total_ram_bytes_os() -> Option<u64> {
    let mut bytes: u64 = 0;
    let mut len = std::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` is a u64 sysctl; `bytes`/`len` are a valid
    // out-buffer of that size, and no new value is written (null, 0).
    let rc = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut bytes as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && len == std::mem::size_of::<u64>() && bytes > 0).then_some(bytes)
}

#[cfg(target_os = "linux")]
fn total_ram_bytes_os() -> Option<u64> {
    parse_meminfo_total(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

#[cfg(windows)]
fn total_ram_bytes_os() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: MEMORYSTATUSEX is plain integers, so all-zero is a valid value.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    // The API requires the struct size in `dwLength` before the call.
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: `status` is a valid, writable MEMORYSTATUSEX with its length set.
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    (ok != 0 && status.ullTotalPhys > 0).then_some(status.ullTotalPhys)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn total_ram_bytes_os() -> Option<u64> {
    None
}

/// `MemTotal` from `/proc/meminfo` text, in bytes (the file reports kB).
// Intentional: compiled on every OS so its unit test runs everywhere; only
// the Linux build calls it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_meminfo_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let mut parts = line["MemTotal:".len()..].split_whitespace();
    let value: u64 = parts.next()?.parse().ok()?;
    let scale = match parts.next() {
        Some(unit) if unit.eq_ignore_ascii_case("kB") => 1024,
        None => 1,
        Some(_) => return None,
    };
    value.checked_mul(scale).filter(|&b| b > 0)
}

/// What a new region of `needed` bytes does to the budget, given the
/// region bytes already checked out. Idle decoders don't count: they are
/// evicted to make room.
#[derive(Debug, PartialEq, Eq)]
enum BudgetChoice {
    /// It fits: open the requested file.
    Fits,
    /// Over the cap: open this proxy instead.
    Proxy(String),
    /// Over the cap and no proxy: open the original, request a proxy.
    NeedsProxy,
}

fn budget_choice(out_bytes: u64, needed: u64, cap: u64, proxy: Option<&str>) -> BudgetChoice {
    if out_bytes.saturating_add(needed) <= cap {
        BudgetChoice::Fits
    } else {
        proxy.map_or(BudgetChoice::NeedsProxy, |p| {
            BudgetChoice::Proxy(p.to_owned())
        })
    }
}

/// Whether a sidecar has at least the threads `wanted` asks for. One
/// spawned while many layers were active has fewer than a lone layer
/// needs; one with more just oversubscribes a little.
fn has_threads(spawned: ThreadBudget, wanted: ThreadBudget) -> bool {
    spawned.decode >= wanted.decode && spawned.convert >= wanted.convert
}

/// Whether `e` leaves the sidecar unusable (dead, or the protocol out of
/// step). A decode failure leaves it fine for the next request.
pub(crate) fn is_fatal(e: &ClientError) -> bool {
    !matches!(e, ClientError::Decode(_))
}

/// An equivalent error, for reporting one failure more than once
/// (`ClientError` isn't `Clone`: its IPC errors wrap `io::Error`).
pub(crate) fn duplicate_error(e: &ClientError) -> ClientError {
    match e {
        ClientError::Ipc(inner) => {
            ClientError::Ipc(vance_ipc::Error::SidecarExited(inner.to_string()))
        }
        ClientError::ProtocolMismatch { parent, sidecar } => ClientError::ProtocolMismatch {
            parent: *parent,
            sidecar: *sidecar,
        },
        ClientError::UnexpectedResponse { expected, got } => ClientError::UnexpectedResponse {
            expected,
            got: got.clone(),
        },
        ClientError::Open(o) => ClientError::Open(o.clone()),
        ClientError::Decode(reason) => ClientError::Decode(reason.clone()),
        // A future variant: reported as a dead sidecar (the cautious reading).
        other => ClientError::Ipc(vance_ipc::Error::SidecarExited(other.to_string())),
    }
}

/// Whether a failed `take` is the file's fault (missing, won't open,
/// crashes the sidecar), as opposed to the sidecar not starting at all.
fn file_fault(e: &EngineError) -> bool {
    matches!(e, EngineError::DecoderClient(_))
}

/// `path`'s mtime, through the timed check: a file on a dead share must
/// not hang the caller (see [`fs_probe`]).
fn file_mtime(path: &str) -> Result<Option<SystemTime>, TimedOut> {
    mtime_within(Path::new(path), DEFAULT_TIMEOUT)
}

/// Whether `path`'s mtime is still `mtime`; a file that doesn't answer
/// counts as unchanged (nothing is retried on a dead share).
fn unchanged(path: &str, mtime: Option<SystemTime>) -> bool {
    match file_mtime(path) {
        Ok(now) => now == mtime,
        Err(TimedOut) => true,
    }
}

/// How long an open that crashed the sidecar stays cached: long enough
/// that scrubbing doesn't respawn a sidecar per frame, short enough that a
/// one-off crash (the OS killing the process, say) doesn't hide the file
/// for the whole session.
const CRASHED_OPEN_RETRY_AFTER: Duration = Duration::from_secs(10);

/// A file that failed to open: not retried until it changes, or (a crash)
/// until [`CRASHED_OPEN_RETRY_AFTER`] has passed.
struct FailedOpen {
    /// The file's mtime at the failure.
    mtime: Option<SystemTime>,
    at: Instant,
    why: OpenFailure,
}

enum OpenFailure {
    /// The sidecar said why.
    Open(OpenError),
    /// The sidecar died (or broke the protocol) opening it.
    Crashed(String),
}

impl FailedOpen {
    fn still_valid(&self, path: &str) -> bool {
        let fresh = match self.why {
            OpenFailure::Open(_) => true,
            OpenFailure::Crashed(_) => self.at.elapsed() < CRASHED_OPEN_RETRY_AFTER,
        };
        // A file that doesn't answer is unknown: keep the failure rather
        // than spawn a sidecar on it.
        fresh && unchanged(path, self.mtime)
    }

    fn error(&self) -> ClientError {
        match &self.why {
            OpenFailure::Open(e) => ClientError::Open(e.clone()),
            OpenFailure::Crashed(msg) => {
                ClientError::Ipc(vance_ipc::Error::SidecarExited(format!(
                    "{msg} (while opening this file; retried when it changes or after {} s)",
                    CRASHED_OPEN_RETRY_AFTER.as_secs()
                )))
            }
        }
    }
}

/// A proxy that wouldn't open.
struct BrokenProxy {
    /// The proxy's mtime then: a regenerated proxy is tried again.
    mtime: Option<SystemTime>,
    /// The original it stands in for.
    original: String,
}

/// LRU pool of decoder sidecars keyed by path.
pub struct DecoderPool {
    bin: PathBuf,
    cap: usize,
    budget: MemoryBudget,
    /// Idle decoders, least recently used first.
    idle: Vec<Idle>,
    out: Arc<OutCounters>,
    /// Region bytes a freshly spawned decoder needed for each path opened.
    region_bytes: HashMap<String, u64>,
    /// Original path → proxy file, for layers over the budget.
    proxies: HashMap<String, String>,
    /// Over-budget originals without a proxy, not yet reported.
    proxy_requests: Vec<String>,
    proxy_requested: HashSet<String>,
    /// Files that failed to open, by their mtime then: not retried until
    /// the file changes (or, for a crash, [`CRASHED_OPEN_RETRY_AFTER`]), so
    /// a broken file doesn't spawn a sidecar per frame.
    failed_opens: HashMap<String, FailedOpen>,
    /// Proxies that wouldn't open, by proxy path.
    broken_proxies: HashMap<String, BrokenProxy>,
    /// Broken proxies not yet reported by `take_broken_proxies`.
    broken_reports: Vec<String>,
    /// Latest failure per requested path since `clear_failures`.
    failures: HashMap<String, String>,
    /// Last failure logged per path, so a failure retried every frame logs
    /// once.
    logged: HashMap<String, String>,
    /// Requested path → the file its last successful `take` opened.
    opened: HashMap<String, String>,
    /// PTS of each file's last frame, once a target past it was seen.
    last_frame_pts: HashMap<String, i64>,
    /// Stream info of every file opened so far.
    infos: HashMap<String, StreamInfo>,
    /// First decoder error since the last `take_error`.
    error: Option<EngineError>,
    /// Busy sidecars' kill handles (see [`Self::sidecars`]).
    sidecars: SidecarRegistry,
    spawns: u64,
}

impl DecoderPool {
    /// A pool spawning `bin`, keeping at most `cap` decoders alive when it
    /// can (idle ones are shut down, least recently used first), within
    /// `budget`. `take` still spawns past `cap` when every decoder is
    /// checked out.
    pub fn new(bin: PathBuf, cap: usize, budget: MemoryBudget) -> Self {
        Self {
            bin,
            cap,
            budget,
            idle: Vec::new(),
            out: Arc::default(),
            region_bytes: HashMap::new(),
            proxies: HashMap::new(),
            proxy_requests: Vec::new(),
            proxy_requested: HashSet::new(),
            failed_opens: HashMap::new(),
            broken_proxies: HashMap::new(),
            broken_reports: Vec::new(),
            failures: HashMap::new(),
            logged: HashMap::new(),
            opened: HashMap::new(),
            last_frame_pts: HashMap::new(),
            infos: HashMap::new(),
            error: None,
            sidecars: SidecarRegistry::default(),
            spawns: 0,
        }
    }

    /// An empty pool like this one (same sidecar, cap, budget and proxy
    /// map), with no decoders and its own registry: what an abandoned
    /// playback session hands back instead of its pool.
    pub fn fresh(&self) -> DecoderPool {
        let mut pool = DecoderPool::new(self.bin.clone(), self.cap, self.budget);
        pool.proxies = self.proxies.clone();
        pool
    }

    /// The registry of this pool's busy sidecars, to kill them from another
    /// thread (see [`SidecarRegistry`]).
    pub fn sidecars(&self) -> SidecarRegistry {
        self.sidecars.clone()
    }

    /// Check out a decoder with `path` open: an idle one already on that
    /// path, else a fresh spawn using `thread_budget(active_layers)`. Over
    /// the memory budget it may open `path`'s proxy instead, and a proxy
    /// that won't open falls back to its original (see the module docs);
    /// [`PooledDecoder::path`] says which file is open.
    pub fn take(&mut self, path: &str, active_layers: usize) -> Result<PooledDecoder, EngineError> {
        let threads = thread_budget(active_layers);
        let taken = match self.broken_original(path) {
            // A proxy named directly ("use proxies") that is known not to
            // open.
            Some(original) => self.take_budgeted(&original, threads),
            None => match self.take_budgeted(path, threads) {
                Err(e) if file_fault(&e) => match self.original_of_proxy(path) {
                    Some(original) => {
                        self.mark_broken_proxy(path, &original, &e);
                        self.take_budgeted(&original, threads)
                    }
                    None => Err(e),
                },
                result => result,
            },
        };
        if let Ok(d) = &taken {
            self.opened.insert(path.to_owned(), d.path.clone());
        }
        taken
    }

    /// The file the last successful [`Self::take`] of `requested` opened:
    /// its proxy when the budget swapped one in, its original when a proxy
    /// wouldn't open, else `requested` itself. `None` before any.
    pub fn opened_path(&self, requested: &str) -> Option<&str> {
        self.opened.get(requested).map(String::as_str)
    }

    /// `take` for `path` itself: the budget may swap in its proxy.
    fn take_budgeted(
        &mut self,
        path: &str,
        threads: ThreadBudget,
    ) -> Result<PooledDecoder, EngineError> {
        if let Some(d) = self.reuse_idle(path, threads) {
            return Ok(d);
        }
        let proxy = self.proxies.get(path).filter(|p| *p != path).cloned();
        if let Some(&needed) = self.region_bytes.get(path) {
            match budget_choice(
                self.out_bytes(),
                needed,
                self.budget.cap_bytes,
                proxy.as_deref(),
            ) {
                BudgetChoice::Proxy(p) => match self.take_exact(&p, threads) {
                    Ok(d) => return Ok(d),
                    // Fall through to the original, over the budget.
                    Err(e) => self.proxy_failed(&p, path, &e),
                },
                BudgetChoice::NeedsProxy => self.request_proxy(path),
                BudgetChoice::Fits => {}
            }
            return self.take_exact(path, threads);
        }
        // First open of `path`: its region size is known once it is open.
        let out_before = self.out_bytes();
        let d = self.take_exact(path, threads)?;
        match budget_choice(
            out_before,
            d.checkout.bytes,
            self.budget.cap_bytes,
            proxy.as_deref(),
        ) {
            // The original stays open until the proxy is: a proxy that won't
            // open leaves the layer on its original instead of nothing.
            BudgetChoice::Proxy(p) => match self.take_exact(&p, threads) {
                Ok(pd) => {
                    // Dropped, not put back: the point is to free its region.
                    drop(d);
                    Ok(pd)
                }
                Err(e) => {
                    self.proxy_failed(&p, path, &e);
                    Ok(d)
                }
            },
            BudgetChoice::NeedsProxy => {
                self.request_proxy(path);
                Ok(d)
            }
            BudgetChoice::Fits => Ok(d),
        }
    }

    /// Opening `proxy` (for `original`) failed: remember it if it was the
    /// file's fault, so the layer stays on its original.
    fn proxy_failed(&mut self, proxy: &str, original: &str, e: &EngineError) {
        if file_fault(e) {
            self.mark_broken_proxy(proxy, original, e);
        } else {
            tracing::warn!(proxy, error = %e, "proxy decoder didn't start; using the original");
        }
    }

    fn mark_broken_proxy(&mut self, proxy: &str, original: &str, e: &EngineError) {
        tracing::warn!(
            proxy,
            original,
            error = %e,
            "proxy won't open; decoding the original and requesting a new proxy"
        );
        self.broken_proxies.insert(
            proxy.to_owned(),
            BrokenProxy {
                mtime: file_mtime(proxy).unwrap_or(None),
                original: original.to_owned(),
            },
        );
        self.proxies.retain(|_, p| p != proxy);
        self.broken_reports.push(proxy.to_owned());
        // Reported again even if it was requested before: that proxy is
        // the one that's broken.
        self.proxy_requested.remove(original);
        self.request_proxy(original);
    }

    /// The original of `path` when `path` is a proxy known not to open (and
    /// unchanged since).
    fn broken_original(&mut self, path: &str) -> Option<String> {
        let b = self.broken_proxies.get(path)?;
        // Unknown (no answer) counts as unchanged: still broken.
        if unchanged(path, b.mtime) {
            return Some(b.original.clone());
        }
        // Regenerated (or replaced): worth trying again.
        self.broken_proxies.remove(path);
        None
    }

    /// The original `path` is the registered proxy of, if any.
    fn original_of_proxy(&self, path: &str) -> Option<String> {
        self.proxies
            .iter()
            .find(|(original, proxy)| proxy.as_str() == path && original.as_str() != path)
            .map(|(original, _)| original.clone())
    }

    /// Return a decoder to the pool, shutting down the least recently used
    /// idle decoders while more than `cap` are alive.
    pub fn put(&mut self, d: PooledDecoder) {
        let PooledDecoder {
            client,
            path,
            info,
            checkout,
        } = d;
        if checkout.busy.killed_by_stop() {
            // Its sidecar is dead: a worker whose stream had already ended
            // sends nothing on stop, so it can't tell, and would hand a dead
            // decoder back as healthy.
            tracing::debug!(%path, "decoder pool: dropping a decoder killed by stop");
            return;
        }
        let threads = checkout.threads;
        drop(checkout);
        self.idle.push(Idle {
            client,
            path,
            info,
            threads,
        });
        // Intentional: only the count cap applies here. Idle regions are
        // reclaimed when a new region needs the room (`take`); evicting them
        // now would respawn a decoder every frame while one frame's layers
        // alone are over the cap.
        self.evict_idle(0, None);
    }

    /// Total shared-memory frame bytes of the pooled decoders, idle and
    /// checked out (sum of `TransportStats.shm_bytes`).
    pub fn shm_bytes(&self) -> u64 {
        let idle: u64 = self
            .idle
            .iter()
            .map(|d| d.client.transport_stats().shm_bytes)
            .sum();
        idle.saturating_add(self.out_bytes())
    }

    /// Would adding `extra_bytes` of shared memory exceed the budget?
    pub fn over_budget_with(&self, extra_bytes: u64) -> bool {
        self.shm_bytes().saturating_add(extra_bytes) > self.budget.cap_bytes
    }

    /// Register the proxy files layers may switch to over the budget:
    /// original path → proxy path (files that exist). Replaces the previous
    /// map, leaving out proxies known not to open (until they change).
    /// Decoders already open keep their file.
    pub fn set_proxy_paths(&mut self, mut proxies: HashMap<String, String>) {
        proxies.retain(|_, proxy| self.broken_original(proxy).is_none());
        self.proxies = proxies;
    }

    /// Originals that went over the budget with no usable proxy, each
    /// reported once per pool (again when their proxy turns out broken, see
    /// [`Self::take_broken_proxies`]): the engine should generate their
    /// proxies and show its one-time notice. They decode at full resolution
    /// meanwhile.
    pub fn take_proxy_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.proxy_requests)
    }

    /// Proxy files that wouldn't open since the last call (missing, corrupt
    /// or crashing the sidecar). Their originals are in
    /// [`Self::take_proxy_requests`]; the engine should regenerate these
    /// even though the files exist.
    pub fn take_broken_proxies(&mut self) -> Vec<String> {
        std::mem::take(&mut self.broken_reports)
    }

    /// The first decoder error since the last call. Providers return `None`
    /// for a frame they couldn't get and keep the reason here (later ones
    /// are logged), so export can fail instead of dropping a layer.
    pub fn take_error(&mut self) -> Option<EngineError> {
        self.error.take()
    }

    /// Keep `e` as `path`'s failure (see [`Self::take_error`] and
    /// [`Self::failure`]). A failure repeated every frame logs once.
    pub(crate) fn record_error(&mut self, path: &str, e: EngineError) {
        let msg = e.to_string();
        if self.logged.get(path) != Some(&msg) {
            tracing::warn!(path, error = %msg, "layer decode failed");
            self.logged.insert(path.to_owned(), msg.clone());
        }
        self.failures.insert(path.to_owned(), msg);
        if self.error.is_none() {
            self.error = Some(e);
        }
    }

    /// Why the last frame asked of `path` (the requested path) failed,
    /// since [`Self::clear_failures`].
    pub(crate) fn failure(&self, path: &str) -> Option<&str> {
        self.failures.get(path).map(String::as_str)
    }

    /// Forget per-path failures (each provider `frames` call starts here).
    pub(crate) fn clear_failures(&mut self) {
        self.failures.clear();
    }

    /// Stream info of `path`, if the pool has opened it.
    pub fn stream_info(&self, path: &str) -> Option<&StreamInfo> {
        self.infos.get(path)
    }

    /// PTS of `path`'s last frame, if a target past it has been seen.
    pub(crate) fn last_frame_pts(&self, path: &str) -> Option<i64> {
        self.last_frame_pts.get(path).copied()
    }

    pub(crate) fn note_last_frame_pts(&mut self, path: &str, pts: i64) {
        self.last_frame_pts.insert(path.to_owned(), pts);
    }

    /// Sidecars spawned so far.
    #[cfg(test)]
    pub(crate) fn spawn_count(&self) -> u64 {
        self.spawns
    }

    /// Transport counters of the idle decoders.
    #[cfg(test)]
    pub(crate) fn idle_transport_stats(&self) -> Vec<vance_decoder::TransportStats> {
        self.idle
            .iter()
            .map(|d| d.client.transport_stats())
            .collect()
    }

    fn out_bytes(&self) -> u64 {
        self.out.bytes.load(Ordering::Relaxed)
    }

    fn out_count(&self) -> usize {
        self.out.count.load(Ordering::Relaxed)
    }

    fn request_proxy(&mut self, path: &str) {
        if self.proxy_requested.insert(path.to_owned()) {
            tracing::info!(
                path,
                "layer over the decoder memory budget; proxy requested"
            );
            self.proxy_requests.push(path.to_owned());
        }
    }

    /// Check out an idle decoder on `path`: the most recently used one with
    /// at least `threads`, else the most recently used one.
    fn reuse_idle(&mut self, path: &str, threads: ThreadBudget) -> Option<PooledDecoder> {
        // Intentional: a decoder with fewer threads than the budget now
        // allows is kept, not respawned. The budget per layer goes UP when
        // layers end (a picture-in-picture finishing), and a respawn there
        // is a 50–100 ms hitch for a few more threads.
        let on_path = |d: &Idle| d.path == path;
        let i = self
            .idle
            .iter()
            .rposition(|d| on_path(d) && has_threads(d.threads, threads))
            .or_else(|| self.idle.iter().rposition(on_path))?;
        let d = self.idle.remove(i);
        let busy = self.sidecars.register(d.client.kill_handle());
        Some(self.check_out(d.client, d.path, d.info, d.threads, busy))
    }

    /// `path` exactly, no budget decision: reuse an idle decoder, else
    /// make room and spawn one.
    fn take_exact(
        &mut self,
        path: &str,
        threads: ThreadBudget,
    ) -> Result<PooledDecoder, EngineError> {
        if let Some(d) = self.reuse_idle(path, threads) {
            return Ok(d);
        }
        let needed = self.region_bytes.get(path).copied();
        self.evict_idle(1, Some(needed.unwrap_or(0)));
        let d = self.spawn_open(path, threads)?;
        if needed.is_none() {
            self.evict_idle(0, Some(0));
        }
        Ok(d)
    }

    /// Shut down idle decoders, least recently used first, while more than
    /// `cap` would be alive with `extra` more, or (with `extra_bytes`) while
    /// the total region bytes plus `extra_bytes` pass the budget.
    fn evict_idle(&mut self, extra: usize, extra_bytes: Option<u64>) {
        while !self.idle.is_empty() {
            let alive = self.idle.len() + self.out_count() + extra;
            let over_bytes = extra_bytes.is_some_and(|b| self.over_budget_with(b));
            if alive <= self.cap && !over_bytes {
                break;
            }
            let victim = self.idle.remove(0);
            tracing::debug!(path = %victim.path, "decoder pool: evicting idle decoder");
            let _ = victim.client.shutdown();
        }
    }

    fn spawn_open(
        &mut self,
        path: &str,
        threads: ThreadBudget,
    ) -> Result<PooledDecoder, EngineError> {
        // Checked here, not left to the sidecar: no process for a file
        // that isn't there (offline media).
        match exists_within(Path::new(path), DEFAULT_TIMEOUT) {
            Ok(true) => {}
            Ok(false) => return Err(ClientError::Open(OpenError::NotFound(path.to_owned())).into()),
            // A dead share: offline for this take (and, cached by the probe,
            // for the next few seconds' too).
            Err(TimedOut) => {
                let why = format!("{path} (its storage isn't answering)");
                return Err(ClientError::Open(OpenError::NotFound(why)).into());
            }
        }
        if let Some(failed) = self.failed_opens.get(path) {
            if failed.still_valid(path) {
                return Err(failed.error().into());
            }
            self.failed_opens.remove(path);
        }
        let options = ClientOptions {
            threads: Some(threads),
            ..Default::default()
        };
        // Registered as soon as the process exists, BEFORE its handshake
        // and the open: a sidecar that hangs starting up (or opening a file
        // on a hung network share) never returns, and stopping a session
        // kills it through this.
        let sidecars = self.sidecars.clone();
        let mut busy = None;
        let spawned = DecoderClient::spawn_with_kill_handle(
            &self.bin,
            std::iter::empty::<(&str, &str)>(),
            options,
            |kill| busy = Some(sidecars.register(kill)),
        );
        let mut client = match spawned {
            Ok(client) => client,
            Err(e) => {
                let stopped = busy.as_ref().is_some_and(Busy::killed_by_stop);
                let why = if stopped {
                    format!("stopped while starting a decoder for {path} ({e})")
                } else {
                    e.to_string()
                };
                return Err(EngineError::DecoderSpawn(why));
            }
        };
        self.spawns += 1;
        let busy = busy.unwrap_or_else(|| self.sidecars.register(client.kill_handle()));
        match client.open(path) {
            Ok(info) => {
                self.region_bytes
                    .insert(path.to_owned(), client.transport_stats().shm_bytes);
                self.infos.insert(path.to_owned(), info.clone());
                self.logged.remove(path);
                Ok(self.check_out(client, path.to_owned(), info, threads, busy))
            }
            // Killed by a stopping session, not the file's fault: nothing to
            // remember about the file, and NOT a file fault (`file_fault`),
            // so a good proxy isn't marked broken and remade.
            Err(e) if busy.killed_by_stop() => {
                drop(client);
                Err(EngineError::DecoderSpawn(format!(
                    "stopped while opening {path} ({e})"
                )))
            }
            Err(ClientError::Open(e)) => {
                self.remember_failed_open(path, OpenFailure::Open(e.clone()));
                let _ = client.shutdown();
                Err(ClientError::Open(e).into())
            }
            Err(e) => {
                // Intentional: an open that kills the sidecar (or breaks the
                // protocol) is cached like an `OpenError`, or every scrub
                // frame would spawn a sidecar for the same crash.
                let msg = match &e {
                    ClientError::Ipc(inner) => inner.to_string(),
                    other => other.to_string(),
                };
                self.remember_failed_open(path, OpenFailure::Crashed(msg));
                // Dropping the client kills whatever is left of the sidecar.
                drop(client);
                Err(e.into())
            }
        }
    }

    fn remember_failed_open(&mut self, path: &str, why: OpenFailure) {
        self.failed_opens.insert(
            path.to_owned(),
            FailedOpen {
                mtime: file_mtime(path).unwrap_or(None),
                at: Instant::now(),
                why,
            },
        );
    }

    fn check_out(
        &self,
        client: DecoderClient,
        path: String,
        info: StreamInfo,
        threads: ThreadBudget,
        busy: Busy,
    ) -> PooledDecoder {
        let bytes = client.transport_stats().shm_bytes;
        self.out.count.fetch_add(1, Ordering::Relaxed);
        self.out.bytes.fetch_add(bytes, Ordering::Relaxed);
        PooledDecoder {
            client,
            path,
            info,
            checkout: Checkout {
                counters: Arc::clone(&self.out),
                bytes,
                threads,
                busy,
            },
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The debug sidecar, if built.
    pub(crate) fn decoder_bin() -> Option<PathBuf> {
        let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug")
            .join(if cfg!(windows) {
                "vance-decoder.exe"
            } else {
                "vance-decoder"
            });
        if bin.exists() {
            return Some(bin);
        }
        if std::env::var_os("VANCE_REQUIRE_SIDECARS").is_some() {
            panic!(
                "VANCE_REQUIRE_SIDECARS set but {} is missing",
                bin.display()
            );
        }
        eprintln!("decoder sidecar missing — skipping");
        None
    }

    pub(crate) fn fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../vance-decoder/tests/fixtures/testsrc_320x240_5s.mp4")
    }

    /// `n` copies of the fixture under distinct names (distinct pool keys).
    fn copies(dir: &Path, n: usize) -> Vec<String> {
        (0..n)
            .map(|i| {
                let p = dir.join(format!("clip{i}.mp4"));
                std::fs::copy(fixture(), &p).expect("copy fixture");
                p.to_string_lossy().into_owned()
            })
            .collect()
    }

    fn big_budget() -> MemoryBudget {
        MemoryBudget {
            cap_bytes: u64::MAX,
        }
    }

    /// Frames this decoder has received: 0 for a fresh spawn.
    fn frames_seen(d: &PooledDecoder) -> u64 {
        let s = d.client.transport_stats();
        s.shared + s.inline + s.spilled
    }

    fn decode_one(d: &mut PooledDecoder) {
        d.client.decode_at(0).expect("decode");
    }

    #[test]
    fn seconds_to_pts_rounds_to_nearest() {
        assert_eq!(source_seconds_to_pts(1.0, (1, 90_000)), 90_000);
        assert_eq!(source_seconds_to_pts(0.5, (1001, 30_000)), 15);
        // 0.0166… s at 1/60 → 0.99… rounds to 1.
        assert_eq!(source_seconds_to_pts(1.0 / 60.0 - 1e-9, (1, 60)), 1);
        assert_eq!(source_seconds_to_pts(2.0, (0, 1)), 0, "degenerate tb");
    }

    #[test]
    fn meminfo_total_parses_kb() {
        let text = "MemTotal:       16318128 kB\nMemFree:         1234 kB\n";
        assert_eq!(parse_meminfo_total(text), Some(16_318_128 * 1024));
        assert_eq!(parse_meminfo_total("MemFree: 1 kB\n"), None);
        assert_eq!(parse_meminfo_total("MemTotal: lots kB\n"), None);
        assert_eq!(parse_meminfo_total("MemTotal: 4 MB\n"), None);
        assert_eq!(parse_meminfo_total("MemTotal: 0 kB\n"), None);
    }

    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    #[test]
    fn total_ram_is_reported_on_this_os() {
        let bytes = total_ram_bytes().expect("RAM size on macOS / Linux / Windows");
        assert!(bytes >= 512 * 1024 * 1024, "{bytes} bytes");
    }

    #[test]
    fn empty_pool_has_no_shm_and_respects_budget() {
        let pool = DecoderPool::new(PathBuf::from("unused"), 6, MemoryBudget { cap_bytes: 100 });
        assert_eq!(pool.shm_bytes(), 0);
        assert!(!pool.over_budget_with(100));
        assert!(pool.over_budget_with(101));
    }

    #[test]
    fn budget_choice_counts_checked_out_bytes_and_prefers_a_proxy() {
        assert_eq!(budget_choice(0, 100, 100, None), BudgetChoice::Fits);
        assert_eq!(budget_choice(60, 40, 100, Some("p")), BudgetChoice::Fits);
        assert_eq!(budget_choice(60, 41, 100, None), BudgetChoice::NeedsProxy);
        assert_eq!(
            budget_choice(60, 41, 100, Some("p")),
            BudgetChoice::Proxy("p".into())
        );
        assert_eq!(
            budget_choice(u64::MAX, 1, u64::MAX - 1, None),
            BudgetChoice::NeedsProxy,
            "saturates instead of overflowing"
        );
    }

    #[test]
    fn a_reused_sidecar_needs_at_least_the_wanted_threads() {
        let b = |decode, convert| ThreadBudget { decode, convert };
        assert!(has_threads(b(4, 4), b(1, 1)));
        assert!(has_threads(b(2, 2), b(2, 2)));
        assert!(!has_threads(b(1, 1), b(4, 4)));
        assert!(!has_threads(b(4, 1), b(2, 2)));
    }

    #[test]
    fn a_missing_file_fails_without_spawning() {
        let mut pool = DecoderPool::new(PathBuf::from("unused"), 6, big_budget());
        let err = pool
            .take("/no/such/vance-clip.mp4", 1)
            .err()
            .expect("error");
        assert!(
            matches!(
                err,
                EngineError::DecoderClient(ClientError::Open(OpenError::NotFound(_)))
            ),
            "{err}"
        );
        assert_eq!(pool.spawns, 0);
    }

    #[test]
    fn idle_decoders_are_reused_by_path_most_recent_first() {
        let Some(bin) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let paths = copies(dir.path(), 2);
        let mut pool = DecoderPool::new(bin, 6, big_budget());

        let mut a = pool.take(&paths[0], 1).expect("take a");
        assert_eq!(a.path, paths[0]);
        let kept = pool.stream_info(&paths[0]).expect("info kept");
        assert_eq!(
            (kept.width, kept.time_base),
            (a.info.width, a.info.time_base)
        );
        assert!(pool.stream_info(&paths[1]).is_none());
        assert_eq!(frames_seen(&a), 0, "fresh spawn");
        decode_one(&mut a);
        pool.put(a);
        let b = pool.take(&paths[1], 1).expect("take b");
        assert_eq!(frames_seen(&b), 0, "another path never reuses a's sidecar");
        pool.put(b);

        let a = pool.take(&paths[0], 1).expect("take a again");
        assert_eq!(frames_seen(&a), 1, "a's idle sidecar was reused");
        // Two layers on one file need two decoders.
        let a2 = pool.take(&paths[0], 2).expect("second decoder on a");
        assert_eq!(frames_seen(&a2), 0);
        assert_eq!(pool.spawns, 3);
        pool.put(a);
        pool.put(a2);
        assert_eq!(pool.idle.len(), 3);
    }

    #[test]
    fn a_decoder_is_kept_when_the_layer_count_drops() {
        let Some(bin) = decoder_bin() else { return };
        if has_threads(thread_budget(16), thread_budget(1)) {
            eprintln!("one core budget fits every layer count here — skipping");
            return;
        }
        let path = fixture().to_string_lossy().into_owned();
        let mut pool = DecoderPool::new(bin, 6, big_budget());
        let mut many = pool.take(&path, 16).expect("take with 16 layers");
        decode_one(&mut many);
        pool.put(many);
        // The picture-in-picture ended: one layer may use more threads now.
        let mut lone = pool.take(&path, 1).expect("take with 1 layer");
        assert_eq!(frames_seen(&lone), 1, "the running sidecar was kept");
        assert_eq!(pool.spawns, 1, "no respawn for more threads");
        assert_eq!(lone.checkout.threads, thread_budget(16), "with its threads");
        decode_one(&mut lone);
        // With a better-provisioned one idle too, that one is preferred.
        let fresh = pool.take(&path, 1).expect("a second decoder");
        assert_eq!(fresh.checkout.threads, thread_budget(1));
        pool.put(fresh);
        pool.put(lone);
        let best = pool.take(&path, 1).expect("again");
        assert_eq!(
            best.checkout.threads,
            thread_budget(1),
            "enough threads wins"
        );
        assert_eq!(pool.spawns, 2);
        drop(best);
    }

    #[test]
    fn the_cap_evicts_the_least_recently_used_idle_decoder() {
        let Some(bin) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let paths = copies(dir.path(), 3);
        let mut pool = DecoderPool::new(bin, 2, big_budget());

        let mut ds: Vec<_> = paths
            .iter()
            .map(|p| pool.take(p, 3).expect("take"))
            .collect();
        assert_eq!(pool.spawns, 3, "take spawns past the cap when all are out");
        for d in &mut ds {
            decode_one(d);
        }
        for d in ds {
            pool.put(d);
        }
        let idle: Vec<_> = pool.idle.iter().map(|d| d.path.clone()).collect();
        assert_eq!(idle, paths[1..], "the first returned was evicted");

        // Taking a new path at the cap evicts the LRU idle one first.
        let a = pool.take(&paths[0], 1).expect("respawn a");
        assert_eq!(frames_seen(&a), 0);
        let idle: Vec<_> = pool.idle.iter().map(|d| d.path.clone()).collect();
        assert_eq!(idle, paths[2..]);
        drop(a);
    }

    #[test]
    fn shm_bytes_count_idle_and_checked_out_decoders() {
        let Some(bin) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let paths = copies(dir.path(), 2);
        let mut pool = DecoderPool::new(bin, 6, big_budget());

        let a = pool.take(&paths[0], 2).expect("take a");
        let region = a.client.transport_stats().shm_bytes;
        if region == 0 {
            eprintln!("shared memory off (VANCE_IPC_SHM=0?) — skipping");
            return;
        }
        assert_eq!(pool.shm_bytes(), region, "checked out counts");
        let b = pool.take(&paths[1], 2).expect("take b");
        assert_eq!(pool.shm_bytes(), 2 * region);
        pool.put(a);
        assert_eq!(pool.shm_bytes(), 2 * region, "idle + checked out");
        // A decoder dropped while out (say its sidecar died) stops counting.
        drop(b);
        assert_eq!(pool.shm_bytes(), region);
        assert_eq!(pool.out_count(), 0);
    }

    #[test]
    fn over_the_budget_a_layer_opens_its_proxy_or_requests_one() {
        let Some(bin) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let paths = copies(dir.path(), 4);
        let (a, b, c, proxy) = (&paths[0], &paths[1], &paths[2], &paths[3]);

        // Measure one region, then allow 1.5 of them.
        let mut probe = DecoderPool::new(bin.clone(), 6, big_budget());
        let region = probe.take(a, 1).expect("probe").checkout.bytes;
        if region == 0 {
            eprintln!("shared memory off (VANCE_IPC_SHM=0?) — skipping");
            return;
        }
        let mut pool = DecoderPool::new(
            bin,
            6,
            MemoryBudget {
                cap_bytes: region * 3 / 2,
            },
        );
        pool.set_proxy_paths(HashMap::from([(b.clone(), proxy.clone())]));

        let da = pool.take(a, 2).expect("a fits");
        assert_eq!(da.path, *a);
        // b's size is unknown until opened: opened, found over, swapped.
        let db = pool.take(b, 2).expect("b");
        assert_eq!(db.path, *proxy, "b falls back to its proxy");
        assert_eq!(pool.spawns, 3, "a, b (dropped), proxy");
        assert_eq!(pool.shm_bytes(), 2 * region, "b's region was freed");
        pool.put(db);
        // Now b's size is known: straight to the (idle) proxy.
        let db = pool.take(b, 2).expect("b again");
        assert_eq!(db.path, *proxy);
        assert_eq!(pool.spawns, 3, "the idle proxy decoder was reused");

        // c has no proxy: full resolution, reported once.
        let dc = pool.take(c, 3).expect("c");
        assert_eq!(dc.path, *c);
        assert_eq!(pool.take_proxy_requests(), std::slice::from_ref(c));
        drop(dc);
        let dc = pool.take(c, 3).expect("c again");
        assert_eq!(dc.path, *c);
        assert!(pool.take_proxy_requests().is_empty(), "reported once");

        // With room again (a and the proxy returned), b opens itself.
        drop(dc);
        pool.put(da);
        pool.put(db);
        let db = pool.take(b, 1).expect("b with room");
        assert_eq!(db.path, *b, "under the budget the original opens");
        assert!(pool.shm_bytes() <= region * 3 / 2, "idle regions evicted");
    }

    #[test]
    fn a_file_that_fails_to_open_is_not_retried_until_it_changes() {
        let Some(bin) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.mp4");
        std::fs::write(&junk, b"not a video").unwrap();
        let junk = junk.to_string_lossy().into_owned();
        let mut pool = DecoderPool::new(bin, 6, big_budget());

        assert!(pool.take(&junk, 1).is_err());
        assert_eq!(pool.spawns, 1);
        assert!(pool.take(&junk, 1).is_err());
        assert_eq!(pool.spawns, 1, "cached failure");
        assert_eq!(pool.out_count(), 0);

        std::fs::copy(fixture(), &junk).unwrap();
        touch_later(&junk);
        let d = pool.take(&junk, 1).expect("a fixed file opens");
        assert_eq!(pool.spawns, 2);
        drop(d);
    }

    /// A small cap that fits one decoder region but not two, with `a`
    /// already checked out; `None` when shared memory is off.
    fn tight_pool(bin: &Path, a: &str) -> Option<(DecoderPool, PooledDecoder, u64)> {
        let mut probe = DecoderPool::new(bin.to_path_buf(), 6, big_budget());
        let region = probe.take(a, 1).expect("probe").checkout.bytes;
        if region == 0 {
            eprintln!("shared memory off (VANCE_IPC_SHM=0?) — skipping");
            return None;
        }
        let mut pool = DecoderPool::new(
            bin.to_path_buf(),
            6,
            MemoryBudget {
                cap_bytes: region * 3 / 2,
            },
        );
        let da = pool.take(a, 2).expect("a fits");
        Some((pool, da, region))
    }

    #[test]
    fn a_proxy_that_wont_open_falls_back_to_the_original() {
        let Some(bin) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let paths = copies(dir.path(), 2);
        let (a, b) = (&paths[0], &paths[1]);
        let proxy = dir.path().join("b.proxy.mov");
        std::fs::write(&proxy, b"not a video").unwrap();
        let proxy = proxy.to_string_lossy().into_owned();
        let Some((mut pool, da, _)) = tight_pool(&bin, a) else {
            return;
        };
        pool.set_proxy_paths(HashMap::from([(b.clone(), proxy.clone())]));

        // First open (b's size unknown): over the budget, the proxy fails.
        let db = pool.take(b, 2).expect("b still opens");
        assert_eq!(db.path, *b, "the original, not nothing");
        assert_eq!(pool.opened_path(b), Some(b.as_str()));
        assert_eq!(pool.take_broken_proxies(), std::slice::from_ref(&proxy));
        assert_eq!(pool.take_proxy_requests(), std::slice::from_ref(b));
        let spawns = pool.spawns;
        drop(db);
        // Known size now: straight to the original, the proxy isn't retried,
        // even after the engine registers it again.
        pool.set_proxy_paths(HashMap::from([(b.clone(), proxy.clone())]));
        let db = pool.take(b, 2).expect("b again");
        assert_eq!(db.path, *b);
        assert_eq!(pool.spawns, spawns + 1, "only the original respawned");
        assert!(pool.take_broken_proxies().is_empty(), "reported once");
        assert!(pool.take_proxy_requests().is_empty());
        drop(db);

        // A regenerated proxy (new contents and mtime) is used again.
        std::fs::copy(fixture(), &proxy).unwrap();
        touch_later(&proxy);
        pool.set_proxy_paths(HashMap::from([(b.clone(), proxy.clone())]));
        let db = pool.take(b, 2).expect("b with a good proxy");
        assert_eq!(db.path, proxy);
        assert_eq!(pool.opened_path(b), Some(proxy.as_str()), "the swap");
        assert_eq!(pool.opened_path(a), Some(a.as_str()));
        drop((da, db));
    }

    #[test]
    fn a_broken_proxy_with_a_known_size_falls_back_too() {
        let Some(bin) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let paths = copies(dir.path(), 2);
        let (a, b) = (&paths[0], &paths[1]);
        let Some((mut pool, da, _)) = tight_pool(&bin, a) else {
            return;
        };
        // b's region size is learned while it fits.
        pool.put(da);
        drop(pool.take(b, 2).expect("b alone"));
        let da = pool.take(a, 2).expect("a");
        let gone = dir.path().join("deleted.proxy.mov");
        let gone = gone.to_string_lossy().into_owned();
        pool.set_proxy_paths(HashMap::from([(b.clone(), gone.clone())]));
        let db = pool.take(b, 2).expect("b falls back");
        assert_eq!(db.path, *b);
        assert_eq!(pool.take_broken_proxies(), [gone]);
        assert_eq!(pool.take_proxy_requests(), std::slice::from_ref(b));
        drop((da, db));
    }

    #[test]
    fn a_broken_proxy_named_directly_decodes_its_original() {
        let Some(bin) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let original = copies(dir.path(), 1).remove(0);
        let proxy = dir.path().join("o.proxy.mov");
        std::fs::write(&proxy, b"truncated").unwrap();
        let proxy = proxy.to_string_lossy().into_owned();
        let mut pool = DecoderPool::new(bin, 6, big_budget());
        pool.set_proxy_paths(HashMap::from([(original.clone(), proxy.clone())]));
        // "Use proxies": the engine asks for the proxy file itself.
        let d = pool.take(&proxy, 1).expect("the original instead");
        assert_eq!(d.path, original);
        assert_eq!(pool.opened_path(&proxy), Some(original.as_str()));
        assert_eq!(pool.take_broken_proxies(), std::slice::from_ref(&proxy));
        assert_eq!(pool.take_proxy_requests(), std::slice::from_ref(&original));
        pool.put(d);
        let d = pool.take(&proxy, 1).expect("again");
        assert_eq!(d.path, original);
        assert_eq!(pool.spawns, 2, "the broken proxy isn't reopened");
        drop(d);
    }

    /// A decoder that answers the `Hello`, then exits before the `Open`:
    /// opening anything through it crashes the sidecar.
    #[cfg(unix)]
    fn crashing_decoder(dir: &Path, real: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let hello = hello_bytes();
        let script = dir.join("crashing-decoder.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nhead -c {} | exec '{}'\n",
                hello.len(),
                real.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// The bytes of the client's `Hello` request.
    #[cfg(unix)]
    fn hello_bytes() -> Vec<u8> {
        use vance_decoder::protocol::{DecoderEnvelope, DecoderRequest, PROTOCOL_VERSION};
        let mut hello = Vec::new();
        vance_ipc::write_frame(
            &mut hello,
            &DecoderEnvelope {
                releases: Vec::new(),
                request: DecoderRequest::Hello {
                    protocol_version: PROTOCOL_VERSION,
                },
            },
        )
        .unwrap();
        hello
    }

    /// A decoder that answers the `Hello`, then never hears another
    /// request: an open through it blocks like one on a hung network share.
    /// Returns the script and the files holding the decoder's pid and the
    /// pid of the feeder process holding its input open.
    #[cfg(unix)]
    pub(crate) fn stuck_open_decoder(dir: &Path, real: &Path) -> (PathBuf, PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let (pid, feeder) = (dir.join("decoder.pid"), dir.join("feeder.pid"));
        let fifo = dir.join("requests.fifo");
        let script = dir.join("stuck-open-decoder.sh");
        // fd 3 keeps the real stdin for the background feeder (whose own
        // stdin is /dev/null): it passes the Hello on, then holds the fifo
        // open without writing, so the decoder waits for the Open forever.
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{pid}'\nmkfifo '{fifo}'\nexec 3<&0\n\
                 ( head -c {n} <&3; exec sleep 60 ) > '{fifo}' &\necho $! > '{feeder}'\n\
                 exec '{real}' < '{fifo}' 3<&-\n",
                pid = pid.display(),
                fifo = fifo.display(),
                n = hello_bytes().len(),
                feeder = feeder.display(),
                real = real.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, pid, feeder)
    }

    /// A decoder that writes its pid to the returned file (then is the real
    /// one), so a test can stop or kill it.
    #[cfg(unix)]
    pub(crate) fn pid_recording_decoder(dir: &Path, real: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let pid = dir.join("decoder.pid");
        let script = dir.join("pid-decoder.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho $$ > '{}'\nexec '{}'\n",
                pid.display(),
                real.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, pid)
    }

    /// A "decoder" that starts and never answers the `Hello`, like one
    /// stuck starting up. Returns the script and its pid file.
    #[cfg(unix)]
    pub(crate) fn silent_decoder(dir: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let pid = dir.join("silent.pid");
        let script = dir.join("silent-decoder.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n", pid.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, pid)
    }

    /// The pid a [`pid_recording_decoder`] wrote.
    #[cfg(unix)]
    pub(crate) fn read_pid(file: &Path) -> i32 {
        let text = std::fs::read_to_string(file).expect("the decoder wrote its pid");
        text.trim().parse().expect("a pid")
    }

    #[cfg(unix)]
    #[test]
    fn an_open_that_crashes_the_sidecar_is_not_retried_every_frame() {
        let Some(real) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let bin = crashing_decoder(dir.path(), &real);
        let clip = copies(dir.path(), 1).remove(0);
        let mut pool = DecoderPool::new(bin, 6, big_budget());
        let err = pool.take(&clip, 1).err().expect("the open crashes");
        assert!(
            matches!(err, EngineError::DecoderClient(ClientError::Ipc(_))),
            "an IPC error, not an OpenError: {err}"
        );
        assert_eq!(pool.spawns, 1);
        for _ in 0..3 {
            let again = pool.take(&clip, 1).err().expect("cached failure");
            assert!(matches!(again, EngineError::DecoderClient(_)), "{again}");
        }
        assert_eq!(pool.spawns, 1, "no sidecar per frame for the same crash");
        assert_eq!(pool.out_count(), 0);
        touch_later(&clip);
        assert!(pool.take(&clip, 1).is_err());
        assert_eq!(pool.spawns, 2, "a changed file is tried again");
    }

    #[test]
    fn a_killed_open_is_neither_a_broken_proxy_nor_a_cached_crash() {
        let Some(bin) = decoder_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let original = copies(dir.path(), 1).remove(0);
        let proxy = dir.path().join("good.proxy.mp4");
        std::fs::copy(fixture(), &proxy).unwrap();
        let proxy = proxy.to_string_lossy().into_owned();
        let mut pool = DecoderPool::new(bin, 6, big_budget());
        pool.set_proxy_paths(HashMap::from([(original.clone(), proxy.clone())]));
        // A session is stopping: every sidecar that starts now is killed.
        let sidecars = pool.sidecars();
        assert_eq!(sidecars.kill_busy(), 0);
        let err = pool.take(&proxy, 1).err().expect("killed while opening");
        assert!(!file_fault(&err), "a kill is not the file's fault: {err}");
        assert!(pool.take_broken_proxies().is_empty(), "the proxy is fine");
        assert!(pool.take_proxy_requests().is_empty(), "nothing to remake");
        assert!(pool.take(&original, 1).is_err(), "still stopping");
        sidecars.resume();
        // After `resume` sidecars live again, the proxy wasn't marked
        // broken, and neither file was cached as crashing.
        let d = pool.take(&proxy, 1).expect("the proxy opens");
        assert_eq!(d.path, proxy);
        let o = pool.take(&original, 1).expect("the original opens");
        assert_eq!(o.path, original);
        drop((d, o));
    }

    #[cfg(unix)]
    #[test]
    fn a_decoder_stuck_in_its_handshake_is_killed_by_stop() {
        let dir = tempfile::tempdir().unwrap();
        let (bin, pid_file) = silent_decoder(dir.path());
        let original = copies(dir.path(), 1).remove(0);
        let proxy = dir.path().join("p.proxy.mp4");
        std::fs::copy(fixture(), &proxy).unwrap();
        let proxy = proxy.to_string_lossy().into_owned();
        let mut pool = DecoderPool::new(bin, 6, big_budget());
        pool.set_proxy_paths(HashMap::from([(original.clone(), proxy.clone())]));
        let sidecars = pool.sidecars();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let taken = pool.take(&proxy, 1).map(|d| d.path.clone());
            let _ = tx.send((taken, pool));
        });
        // Let the spawn reach its handshake, then stop.
        let started = std::time::Instant::now();
        while !pid_file.exists() && started.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(100));
        sidecars.kill_busy();
        let got = rx.recv_timeout(Duration::from_secs(10));
        if got.is_err() && pid_file.exists() {
            // SAFETY: frees the test's own helper process.
            unsafe { libc::kill(read_pid(&pid_file), libc::SIGKILL) };
        }
        let (taken, mut pool) = got.expect("the handshake wasn't killable");
        let err = taken.expect_err("killed");
        assert!(!file_fault(&err), "not the file's fault: {err}");
        assert!(err.to_string().contains("stopped"), "{err}");
        assert!(pool.take_broken_proxies().is_empty(), "the proxy is fine");
        assert!(pool.take_proxy_requests().is_empty());
        assert!(pool.failed_opens.is_empty(), "not cached as a crash");
    }

    #[test]
    fn a_decoder_killed_by_stop_is_not_put_back() {
        let Some(bin) = decoder_bin() else { return };
        let path = fixture().to_string_lossy().into_owned();
        let mut pool = DecoderPool::new(bin, 6, big_budget());
        let d = pool.take(&path, 1).expect("take");
        let sidecars = pool.sidecars();
        assert_eq!(sidecars.kill_busy(), 1);
        sidecars.resume();
        // Handed back as if healthy (its stream had ended, so its worker
        // never noticed): the pool must not keep it.
        pool.put(d);
        assert_eq!(pool.idle.len(), 0, "a dead decoder isn't kept");
        let mut d = pool.take(&path, 1).expect("a fresh decoder");
        assert_eq!(pool.spawns, 2);
        decode_one(&mut d);
    }

    #[test]
    fn a_file_on_a_dead_share_is_offline_without_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("on-a-dead-share.mp4");
        std::fs::write(&clip, b"x").unwrap();
        fs_probe::tests::block(&clip);
        let path = clip.to_string_lossy().into_owned();
        let mut pool = DecoderPool::new(PathBuf::from("unused"), 6, big_budget());
        let started = std::time::Instant::now();
        let err = pool.take(&path, 1).err().expect("offline");
        assert!(
            started.elapsed() < DEFAULT_TIMEOUT + Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert!(
            matches!(
                err,
                EngineError::DecoderClient(ClientError::Open(OpenError::NotFound(_)))
            ),
            "offline for this take: {err}"
        );
        // The dead path isn't stat'ed again for a while.
        let again = std::time::Instant::now();
        assert!(pool.take(&path, 1).is_err());
        assert!(again.elapsed() < Duration::from_millis(50), "cached");
        assert_eq!(fs_probe::tests::checks(&clip), 1);
        assert_eq!(pool.spawns, 0, "no sidecar for a file that doesn't answer");
        fs_probe::tests::release(&clip);
    }

    #[test]
    fn failures_are_kept_per_path_until_cleared() {
        let mut pool = DecoderPool::new(PathBuf::from("unused"), 6, big_budget());
        let missing = "/no/such/vance-clip.mp4";
        let e = pool.take(missing, 1).err().expect("missing");
        assert_eq!(pool.opened_path(missing), None, "nothing opened");
        pool.record_error(missing, e);
        assert!(pool
            .failure(missing)
            .is_some_and(|m| m.contains("NotFound")));
        assert!(pool.failure("other.mp4").is_none());
        assert!(pool.take_error().is_some());
        pool.clear_failures();
        assert!(pool.failure(missing).is_none());
    }

    #[test]
    fn duplicated_errors_read_the_same() {
        let open = ClientError::Open(OpenError::NoVideoStream);
        assert_eq!(duplicate_error(&open).to_string(), open.to_string());
        let decode = ClientError::Decode("bad pts".into());
        assert!(matches!(duplicate_error(&decode), ClientError::Decode(r) if r == "bad pts"));
        let ipc = ClientError::Ipc(vance_ipc::Error::UnexpectedEof);
        let dup = duplicate_error(&ipc);
        assert!(is_fatal(&dup));
        assert!(dup.to_string().contains("unexpected eof"), "{dup}");
    }

    /// Move `path`'s mtime clearly forward (some filesystems keep a coarse
    /// mtime).
    fn touch_later(path: &str) {
        let later = SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(later)
            .unwrap();
    }

    #[test]
    fn pool_and_decoders_move_across_threads() {
        fn send<T: Send>() {}
        send::<DecoderPool>();
        send::<PooledDecoder>();
    }
}
