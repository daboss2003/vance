//! File checks that can't hang the caller.
//!
//! On a dead SMB share or a hard-mounted NFS export even `stat` blocks, and
//! no decoder exists yet to kill. These run the check on a helper thread
//! and wait at most `timeout`:
//!
//! - an answer within the timeout is returned as is (and not cached, so a
//!   file that appears or changes is seen at once);
//! - a check that doesn't answer is [`TimedOut`] ("offline or unknown" for
//!   the caller), and that path isn't stat'ed again for [`HUNG_FOR`]: calls
//!   meanwhile return `TimedOut` at once;
//! - at most one helper thread is in flight per path: a call while one runs
//!   waits for it instead of starting another, so a dead path costs one
//!   stuck thread, not one per frame.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use parking_lot::{Condvar, Mutex};

/// A timeout for checks on the playback, scrub and export paths: long
/// enough for a slow but working network share, short enough that a dead
/// one costs a frame or so (and then nothing for [`HUNG_FOR`]).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(250);

/// How long a path that didn't answer is treated as unknown without a new
/// check.
pub const HUNG_FOR: Duration = Duration::from_secs(10);

/// Most paths remembered; finished ones are forgotten past this.
const MAX_ENTRIES: usize = 4096;

/// The file system didn't answer within the timeout (or the path didn't
/// answer recently).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedOut;

impl fmt::Display for TimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the file system didn't answer in time")
    }
}

impl std::error::Error for TimedOut {}

/// Whether `path` exists, if the file system answers within `timeout`.
pub fn exists_within(path: &Path, timeout: Duration) -> Result<bool, TimedOut> {
    stat_within(path, timeout).map(|s| s.exists)
}

/// `path`'s modification time (`None`: no such file, or no mtime), if the
/// file system answers within `timeout`.
pub fn mtime_within(path: &Path, timeout: Duration) -> Result<Option<SystemTime>, TimedOut> {
    stat_within(path, timeout).map(|s| s.mtime)
}

/// `path`'s `(modification time, length in bytes)` (`Ok(None)`: no such
/// file), if the file system answers within `timeout`.
pub fn metadata_within(
    path: &Path,
    timeout: Duration,
) -> Result<Option<(Option<SystemTime>, u64)>, TimedOut> {
    stat_within(path, timeout).map(|s| s.exists.then_some((s.mtime, s.len)))
}

#[derive(Debug, Clone, Copy)]
struct Stat {
    exists: bool,
    mtime: Option<SystemTime>,
    len: u64,
}

#[derive(Default)]
struct Entry {
    /// A helper thread is checking the path now.
    running: bool,
    /// Checks started and finished on this path (generations).
    started: u64,
    finished: u64,
    /// The answer of check `finished`.
    last: Option<Stat>,
    /// Didn't answer in time: unknown until then, without a new check.
    hung_until: Option<Instant>,
}

struct Probes {
    entries: Mutex<HashMap<PathBuf, Entry>>,
    /// Notified whenever a check finishes.
    done: Condvar,
}

fn probes() -> &'static Probes {
    static PROBES: OnceLock<Probes> = OnceLock::new();
    PROBES.get_or_init(|| Probes {
        entries: Mutex::new(HashMap::new()),
        done: Condvar::new(),
    })
}

fn stat_within(path: &Path, timeout: Duration) -> Result<Stat, TimedOut> {
    let probes = probes();
    let mut entries = probes.entries.lock();
    let now = Instant::now();
    if entries.len() >= MAX_ENTRIES {
        entries.retain(|_, e| e.running || e.hung_until.is_some_and(|u| u > now));
    }
    let entry = entries.entry(path.to_owned()).or_default();
    if entry.hung_until.is_some_and(|until| now < until) {
        return Err(TimedOut);
    }
    let wanted = if entry.running {
        // One in flight per path: wait for the running check.
        entry.started
    } else {
        entry.running = true;
        entry.started += 1;
        let generation = entry.started;
        let owned = path.to_owned();
        let spawned = std::thread::Builder::new()
            .name("vance-fs-probe".into())
            .spawn(move || finish(owned.clone(), generation, check(&owned)));
        if spawned.is_err() {
            // No thread to spare: check here, as the code did before.
            drop(entries);
            let stat = check(path);
            finish(path.to_owned(), generation, stat);
            return Ok(stat);
        }
        generation
    };
    let deadline = now + timeout;
    loop {
        // Only forgotten (`MAX_ENTRIES`) once finished, and then answered.
        let Some(entry) = entries.get_mut(path) else {
            return stat_within(path, timeout);
        };
        if entry.finished >= wanted {
            if let Some(stat) = entry.last {
                return Ok(stat);
            }
        }
        if Instant::now() >= deadline {
            entry.hung_until = Some(Instant::now() + HUNG_FOR);
            tracing::warn!(
                path = %path.display(),
                "file check didn't answer in time; treating the file as unknown for a while"
            );
            return Err(TimedOut);
        }
        probes.done.wait_until(&mut entries, deadline);
    }
}

/// Record check `generation` of `path`, and wake the callers waiting on it.
fn finish(path: PathBuf, generation: u64, stat: Stat) {
    let probes = probes();
    let mut entries = probes.entries.lock();
    let entry = entries.entry(path).or_default();
    entry.running = false;
    entry.finished = entry.finished.max(generation);
    entry.last = Some(stat);
    // It answered: the path works again.
    entry.hung_until = None;
    drop(entries);
    probes.done.notify_all();
}

/// The check itself (on the helper thread).
fn check(path: &Path) -> Stat {
    #[cfg(test)]
    tests::hook(path);
    match std::fs::metadata(path) {
        Ok(meta) => Stat {
            exists: true,
            mtime: meta.modified().ok(),
            len: meta.len(),
        },
        Err(_) => Stat {
            exists: false,
            mtime: None,
            len: 0,
        },
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Paths whose check blocks (a dead share) until released, with how
    /// many checks of each have started.
    static BLOCKED: Mutex<Option<HashMap<PathBuf, (bool, usize)>>> = Mutex::new(None);
    static RELEASED: Condvar = Condvar::new();

    pub(super) fn hook(path: &Path) {
        let mut blocked = BLOCKED.lock();
        let Some((_, checks)) = blocked.as_mut().and_then(|b| b.get_mut(path)) else {
            return;
        };
        *checks += 1;
        while blocked
            .as_ref()
            .and_then(|b| b.get(path))
            .is_some_and(|(on, _)| *on)
        {
            RELEASED.wait(&mut blocked);
        }
    }

    /// Make checks of `path` block like a dead share's, until [`release`].
    pub(crate) fn block(path: &Path) {
        BLOCKED
            .lock()
            .get_or_insert_with(HashMap::new)
            .insert(path.to_owned(), (true, 0));
    }

    /// Let `path`'s blocked checks finish.
    pub(crate) fn release(path: &Path) {
        if let Some((on, _)) = BLOCKED.lock().as_mut().and_then(|b| b.get_mut(path)) {
            *on = false;
        }
        RELEASED.notify_all();
    }

    /// Checks of `path` started since [`block`].
    pub(crate) fn checks(path: &Path) -> usize {
        BLOCKED
            .lock()
            .as_ref()
            .and_then(|b| b.get(path))
            .map_or(0, |(_, n)| *n)
    }

    #[test]
    fn answers_are_returned_and_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.mp4");
        assert_eq!(exists_within(&file, DEFAULT_TIMEOUT), Ok(false));
        assert_eq!(mtime_within(&file, DEFAULT_TIMEOUT), Ok(None));
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            exists_within(&file, DEFAULT_TIMEOUT),
            Ok(true),
            "seen at once"
        );
        let want = std::fs::metadata(&file).unwrap().modified().ok();
        assert_eq!(mtime_within(&file, DEFAULT_TIMEOUT), Ok(want));
        assert_eq!(metadata_within(&file, DEFAULT_TIMEOUT), Ok(Some((want, 1))));
        let gone = dir.path().join("gone.mp4");
        assert_eq!(metadata_within(&gone, DEFAULT_TIMEOUT), Ok(None));
    }

    #[test]
    fn a_hung_path_times_out_is_cached_and_costs_one_thread() {
        let dir = tempfile::tempdir().unwrap();
        let dead = dir.path().join("on-a-dead-share.mov");
        std::fs::write(&dead, b"x").unwrap();
        block(&dead);
        let timeout = Duration::from_millis(100);
        let started = Instant::now();
        assert_eq!(exists_within(&dead, timeout), Err(TimedOut));
        let took = started.elapsed();
        assert!(took >= timeout, "waited for the answer: {took:?}");
        assert!(took < timeout + Duration::from_secs(2), "{took:?}");
        // Cached: answered at once, with no new check.
        let again = Instant::now();
        assert_eq!(mtime_within(&dead, timeout), Err(TimedOut));
        assert_eq!(metadata_within(&dead, timeout), Err(TimedOut));
        assert_eq!(exists_within(&dead, Duration::from_secs(5)), Err(TimedOut));
        assert!(again.elapsed() < Duration::from_millis(50), "cached");
        assert_eq!(checks(&dead), 1, "one stuck check, not one per call");
        // The share comes back: its stuck check answers, and the path works.
        release(&dead);
        let back = Instant::now();
        loop {
            if exists_within(&dead, timeout) == Ok(true) {
                break;
            }
            assert!(back.elapsed() < Duration::from_secs(5), "never recovered");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn concurrent_calls_share_one_check() {
        let dir = tempfile::tempdir().unwrap();
        let dead = dir.path().join("dead.mov");
        block(&dead);
        // Every caller, early or late, waits on (or is answered from) the
        // one stuck check: none starts another.
        let callers: Vec<_> = (0..8)
            .map(|_| {
                let dead = dead.clone();
                std::thread::spawn(move || exists_within(&dead, Duration::from_millis(300)))
            })
            .collect();
        for c in callers {
            assert_eq!(c.join().unwrap(), Err(TimedOut));
        }
        assert_eq!(checks(&dead), 1);
        release(&dead);
    }
}
