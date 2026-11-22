//! A pool of decoder sidecars shared by every layer (ADR 0007).
//!
//! An LRU of about six [`DecoderClient`]s keyed by path replaces the single
//! playback and transition decoders. Clients spawn with
//! `ClientOptions { threads: Some(thread_budget(active_layers)), .. }` (ADR
//! 0006) so decode and conversion threads share the cores, and the total
//! shared-memory frame memory is capped at about 25% of RAM.

use std::path::PathBuf;

use vance_decoder::{DecoderClient, StreamInfo};

use crate::engine::EngineError;

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
pub struct PooledDecoder {
    /// The sidecar client.
    pub client: DecoderClient,
    /// Path currently open in `client`.
    pub path: String,
    /// Stream info from opening `path`.
    pub info: StreamInfo,
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
/// Windows: `None` for now — vance-app has no `windows-sys` dependency for
/// `GlobalMemoryStatusEx`; callers fall back to a fixed budget.
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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

/// LRU pool of decoder sidecars keyed by path.
pub struct DecoderPool {
    // Intentional: read by `take` once the sources seam (S) lands.
    #[allow(dead_code)]
    bin: PathBuf,
    cap: usize,
    budget: MemoryBudget,
    /// Idle decoders, least recently used first.
    idle: Vec<PooledDecoder>,
}

impl DecoderPool {
    /// A pool spawning `bin`, keeping at most `cap` idle decoders, within
    /// `budget`.
    pub fn new(bin: PathBuf, cap: usize, budget: MemoryBudget) -> Self {
        Self {
            bin,
            cap,
            budget,
            idle: Vec::new(),
        }
    }

    /// Check out a decoder with `path` open: an idle one already on that
    /// path, else a fresh spawn using `thread_budget(active_layers)`.
    // Intentional: Unimplemented until the sources seam (S) lands the pool
    // logic.
    pub fn take(
        &mut self,
        _path: &str,
        _active_layers: usize,
    ) -> Result<PooledDecoder, EngineError> {
        Err(EngineError::Unimplemented("DecoderPool::take"))
    }

    /// Return a decoder to the pool, dropping the least recently used
    /// idle decoder past `cap`.
    pub fn put(&mut self, d: PooledDecoder) {
        self.idle.push(d);
        if self.idle.len() > self.cap {
            let excess = self.idle.len() - self.cap;
            self.idle.drain(..excess);
        }
    }

    /// Total shared-memory frame bytes of the pooled decoders (sum of
    /// `TransportStats.shm_bytes`).
    pub fn shm_bytes(&self) -> u64 {
        self.idle
            .iter()
            .map(|d| d.client.transport_stats().shm_bytes)
            .sum()
    }

    /// Would adding `extra_bytes` of shared memory exceed the budget?
    pub fn over_budget_with(&self, extra_bytes: u64) -> bool {
        self.shm_bytes().saturating_add(extra_bytes) > self.budget.cap_bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn total_ram_is_reported_on_this_os() {
        let bytes = total_ram_bytes().expect("RAM size on macOS / Linux");
        assert!(bytes >= 512 * 1024 * 1024, "{bytes} bytes");
    }

    #[test]
    fn empty_pool_has_no_shm_and_respects_budget() {
        let pool = DecoderPool::new(PathBuf::from("unused"), 6, MemoryBudget { cap_bytes: 100 });
        assert_eq!(pool.shm_bytes(), 0);
        assert!(!pool.over_budget_with(100));
        assert!(pool.over_budget_with(101));
    }
}
