//! `vance-cache` — frame + render cache for the video pipeline.
//!
//! Two layers:
//! 1. **In-memory ring per source** — small, bounded buffer of the
//!    most-recently-decoded frames for one source URI. Reads are
//!    O(log n) (BTreeMap by PTS); writes evict the oldest insertion
//!    when over the byte budget. Per Phase 9 spec, "ring per source"
//!    means independent buffers keyed by source URI so scrubs within
//!    one clip don't evict another clip's hot frames.
//! 2. **On-disk LRU** — content-addressed file store under a base
//!    directory. Survives restarts. Used by both the decoded-frame
//!    cache (key = URI + PTS + transform_hash) and the render cache
//!    (key = input_frame_hash + effect_chain_hash).
//!
//! The hash function is hand-rolled FNV-1a (64-bit) so file names are
//! deterministic across Rust / std versions — important for an
//! on-disk store that has to survive vance upgrades. There's no
//! cryptographic adversary here; the only property we need is
//! collision-resistance among legitimate inputs.

#![warn(missing_docs)]

pub mod disk;
pub mod fnv;
pub mod key;
pub mod ring;

pub use disk::{DiskLru, DiskLruError, DiskLruStats};
pub use fnv::Fnv1a64;
pub use key::CacheKey;
pub use ring::FrameCacheRing;

use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::PathBuf;
use thiserror::Error;

/// Combined two-tier cache. In-memory ring per source URI, backed by
/// a single on-disk LRU shared across all sources.
pub struct FrameCache {
    rings: Mutex<HashMap<String, FrameCacheRing>>,
    disk: Mutex<DiskLru>,
    ring_budget_bytes: usize,
}

/// Combined errors from the cache.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum FrameCacheError {
    /// On-disk LRU failure (filesystem, permissions, corruption).
    #[error(transparent)]
    Disk(#[from] DiskLruError),
}

impl FrameCache {
    /// Open or create a cache at `base_dir`.
    ///
    /// - `ring_budget_bytes` — per-source in-memory ring size.
    /// - `disk_budget_bytes` — on-disk LRU total size.
    pub fn new(
        base_dir: PathBuf,
        ring_budget_bytes: usize,
        disk_budget_bytes: u64,
    ) -> Result<Self, FrameCacheError> {
        let disk = DiskLru::open(base_dir, disk_budget_bytes)?;
        Ok(Self {
            rings: Mutex::new(HashMap::new()),
            disk: Mutex::new(disk),
            ring_budget_bytes,
        })
    }

    /// Look up a frame. Checks the in-memory ring first (fast path),
    /// then the on-disk LRU. On a disk hit, the value is promoted
    /// back into the in-memory ring.
    pub fn get(&self, key: &CacheKey) -> Option<Vec<u8>> {
        if let Some(ring) = self.rings.lock().get_mut(&key.source_uri) {
            if let Some(bytes) = ring.get(key.pts, key.transform_hash) {
                tracing::trace!(uri = %key.source_uri, pts = key.pts, "frame cache ring hit");
                return Some(bytes);
            }
        }
        let disk_hit = self.disk.lock().get(&key.disk_key()).ok().flatten();
        if let Some(bytes) = disk_hit {
            tracing::trace!(uri = %key.source_uri, pts = key.pts, "frame cache disk hit");
            let mut rings = self.rings.lock();
            let ring = rings
                .entry(key.source_uri.clone())
                .or_insert_with(|| FrameCacheRing::new(self.ring_budget_bytes));
            ring.put(key.pts, key.transform_hash, bytes.clone());
            return Some(bytes);
        }
        None
    }

    /// Insert a frame into both tiers.
    pub fn put(&self, key: &CacheKey, bytes: &[u8]) -> Result<(), FrameCacheError> {
        {
            let mut rings = self.rings.lock();
            let ring = rings
                .entry(key.source_uri.clone())
                .or_insert_with(|| FrameCacheRing::new(self.ring_budget_bytes));
            ring.put(key.pts, key.transform_hash, bytes.to_vec());
        }
        self.disk.lock().put(&key.disk_key(), bytes)?;
        Ok(())
    }

    /// Remove a source's ring (called when the source is no longer
    /// active in the timeline). On-disk entries are NOT purged.
    pub fn drop_source(&self, source_uri: &str) {
        self.rings.lock().remove(source_uri);
    }

    /// Statistics snapshot for telemetry / the UI / soak test.
    pub fn stats(&self) -> CacheStats {
        let rings = self.rings.lock();
        let total_ring_bytes: usize = rings.values().map(|r| r.bytes_used()).sum();
        let ring_count = rings.len();
        drop(rings);
        let disk = self.disk.lock().stats();
        CacheStats {
            ring_count,
            ring_total_bytes: total_ring_bytes,
            disk_entries: disk.entries,
            disk_bytes: disk.bytes,
        }
    }
}

/// Snapshot of cache occupancy.
#[derive(Clone, Copy, Debug)]
pub struct CacheStats {
    /// Number of active per-source rings.
    pub ring_count: usize,
    /// Total bytes held across all rings.
    pub ring_total_bytes: usize,
    /// Number of entries in the on-disk LRU.
    pub disk_entries: usize,
    /// Total bytes on disk.
    pub disk_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn two_tier_get_put_round_trip() {
        let dir = tempdir().unwrap();
        let cache = FrameCache::new(dir.path().to_path_buf(), 1024 * 1024, 16 * 1024 * 1024)
            .unwrap();
        let key = CacheKey::for_source_frame("file:///x.mp4", 100, 0);
        assert!(cache.get(&key).is_none(), "miss before write");
        cache.put(&key, b"hello").unwrap();
        let hit = cache.get(&key).expect("hit after write");
        assert_eq!(hit, b"hello");
    }

    #[test]
    fn ring_isolation_per_source() {
        let dir = tempdir().unwrap();
        let cache = FrameCache::new(dir.path().to_path_buf(), 1024 * 1024, 16 * 1024 * 1024)
            .unwrap();
        let k_a = CacheKey::for_source_frame("file:///a.mp4", 1, 0);
        let k_b = CacheKey::for_source_frame("file:///b.mp4", 1, 0);
        cache.put(&k_a, b"a-frame").unwrap();
        cache.put(&k_b, b"b-frame").unwrap();
        cache.drop_source("file:///a.mp4");
        // a's ring is gone but b's stays + a's disk row is still there.
        assert_eq!(cache.get(&k_b).unwrap(), b"b-frame");
        // a still resolves via disk promotion.
        assert_eq!(cache.get(&k_a).unwrap(), b"a-frame");
    }

    #[test]
    fn transform_hash_differentiates_keys() {
        let dir = tempdir().unwrap();
        let cache = FrameCache::new(dir.path().to_path_buf(), 1024 * 1024, 16 * 1024 * 1024)
            .unwrap();
        let raw = CacheKey::for_source_frame("file:///x.mp4", 100, 0);
        let transformed = CacheKey::for_source_frame("file:///x.mp4", 100, 0xdeadbeef);
        cache.put(&raw, b"raw").unwrap();
        cache.put(&transformed, b"transformed").unwrap();
        assert_eq!(cache.get(&raw).unwrap(), b"raw");
        assert_eq!(cache.get(&transformed).unwrap(), b"transformed");
    }

    #[test]
    fn stats_reflect_writes() {
        let dir = tempdir().unwrap();
        let cache = FrameCache::new(dir.path().to_path_buf(), 1024 * 1024, 16 * 1024 * 1024)
            .unwrap();
        cache
            .put(&CacheKey::for_source_frame("file:///x.mp4", 1, 0), &vec![0u8; 100])
            .unwrap();
        let s = cache.stats();
        assert_eq!(s.ring_count, 1);
        assert!(s.ring_total_bytes >= 100);
        assert_eq!(s.disk_entries, 1);
        assert!(s.disk_bytes >= 100);
    }
}
