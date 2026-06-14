//! On-disk LRU file store.
//!
//! Layout under `base_dir`:
//! - `entries/` — one file per cached blob, named by the
//!   [`CacheKey::disk_key`] hex digest.
//! - `index.bin` — append-only log of (key, byte_len) events. Replayed
//!   at startup to rebuild the in-memory LRU order. Compacted on
//!   open when the log grows past a threshold.
//!
//! Eviction policy: classic LRU. `get()` promotes; `put()` evicts the
//! least-recently-used until total bytes fit the budget.
//!
//! Crash-safety: writes are `write + rename` so a partial file never
//! ends up under a final name. The index log replay tolerates a
//! truncated tail record (treats it as if the entry didn't exist).

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// LRU on-disk store.
pub struct DiskLru {
    base_dir: PathBuf,
    entries_dir: PathBuf,
    index_path: PathBuf,
    /// `disk_key → entry`.
    entries: HashMap<String, DiskEntry>,
    /// Ordered list of keys, oldest first. Mutated by promote-on-get
    /// and append-on-put. We accept the O(n) remove cost for clarity;
    /// the hot path is `get()` which only walks `entries`.
    lru_order: Vec<String>,
    bytes: u64,
    byte_budget: u64,
    /// Open index file handle for appends. Re-opened on compaction.
    index_log: File,
    /// Number of records written to the current index log (used to
    /// trigger compaction).
    log_records: usize,
}

#[derive(Clone, Debug)]
struct DiskEntry {
    bytes: u64,
}

/// Snapshot of [`DiskLru`] occupancy.
#[derive(Clone, Copy, Debug)]
pub struct DiskLruStats {
    /// Number of entries currently on disk.
    pub entries: usize,
    /// Bytes occupied by entry blobs.
    pub bytes: u64,
}

/// Errors surfaced by the on-disk store.
#[non_exhaustive]
#[derive(Debug, Error)]
pub enum DiskLruError {
    /// Filesystem I/O failure.
    #[error("disk cache io: {0}")]
    Io(#[from] std::io::Error),
    /// Corrupt index entry (bad UTF-8 in the recorded key).
    #[error("disk cache index corrupt: {0}")]
    IndexCorrupt(String),
}

/// Compact the index when this many records have been written.
const INDEX_COMPACT_THRESHOLD: usize = 8_192;

impl DiskLru {
    /// Open (or create) a store rooted at `base_dir` with a total
    /// byte budget. Replays the on-disk index to rebuild the LRU.
    pub fn open(base_dir: PathBuf, byte_budget: u64) -> Result<Self, DiskLruError> {
        let entries_dir = base_dir.join("entries");
        let index_path = base_dir.join("index.bin");
        fs::create_dir_all(&entries_dir)?;

        // Replay.
        let (entries, lru_order, bytes) = replay_index(&index_path, &entries_dir)?;

        let index_log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_path)?;

        let mut s = Self {
            base_dir,
            entries_dir,
            index_path,
            entries,
            lru_order,
            bytes,
            byte_budget,
            index_log,
            log_records: 0,
        };
        // Trim down to budget in case the budget shrank since last
        // open.
        s.evict_to_budget()?;
        Ok(s)
    }

    /// Is `key` present in the index? Presence-only check — no blob read
    /// and no LRU promotion (so it can't perturb eviction order).
    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Read an entry, promoting it to the most-recently-used position.
    pub fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, DiskLruError> {
        if !self.entries.contains_key(key) {
            return Ok(None);
        }
        let path = self.entry_path(key);
        let mut buf = Vec::new();
        match File::open(&path) {
            Ok(mut f) => {
                f.read_to_end(&mut buf)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Entry vanished under us (manual deletion?). Treat as miss.
                self.evict_entry_inner(key)?;
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
        self.promote(key);
        self.append_log("T", key, 0)?;
        Ok(Some(buf))
    }

    /// Insert (or overwrite) an entry. Evicts LRU as needed.
    pub fn put(&mut self, key: &str, bytes: &[u8]) -> Result<(), DiskLruError> {
        // Remove any prior entry's byte accounting + file.
        if let Some(prev) = self.entries.remove(key) {
            self.bytes = self.bytes.saturating_sub(prev.bytes);
            let _ = fs::remove_file(self.entry_path(key));
            // Will re-add below.
            self.lru_order.retain(|k| k != key);
        }
        // Write the file atomically.
        let tmp_path = self.entries_dir.join(format!("{key}.tmp"));
        {
            let mut f = File::create(&tmp_path)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        fs::rename(&tmp_path, self.entry_path(key))?;

        let len = bytes.len() as u64;
        self.entries.insert(key.to_string(), DiskEntry { bytes: len });
        self.lru_order.push(key.to_string());
        self.bytes += len;
        self.append_log("P", key, len)?;
        self.evict_to_budget()?;
        if self.log_records >= INDEX_COMPACT_THRESHOLD {
            self.compact_log()?;
        }
        Ok(())
    }

    /// Stats snapshot.
    pub fn stats(&self) -> DiskLruStats {
        DiskLruStats {
            entries: self.entries.len(),
            bytes: self.bytes,
        }
    }

    fn entry_path(&self, key: &str) -> PathBuf {
        self.entries_dir.join(key)
    }

    fn promote(&mut self, key: &str) {
        if let Some(idx) = self.lru_order.iter().position(|k| k == key) {
            let removed = self.lru_order.remove(idx);
            self.lru_order.push(removed);
        }
    }

    fn evict_to_budget(&mut self) -> Result<(), DiskLruError> {
        while self.bytes > self.byte_budget {
            let Some(victim) = self.lru_order.first().cloned() else {
                break;
            };
            self.evict_entry_inner(&victim)?;
        }
        Ok(())
    }

    fn evict_entry_inner(&mut self, key: &str) -> Result<(), DiskLruError> {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes = self.bytes.saturating_sub(entry.bytes);
            let _ = fs::remove_file(self.entry_path(key));
        }
        self.lru_order.retain(|k| k != key);
        self.append_log("E", key, 0)?;
        Ok(())
    }

    fn append_log(&mut self, op: &str, key: &str, bytes: u64) -> Result<(), DiskLruError> {
        writeln!(self.index_log, "{op}\t{key}\t{bytes}")?;
        self.log_records += 1;
        Ok(())
    }

    /// Rewrite the index log as a snapshot of the current state. Keeps
    /// the file from growing unbounded with promotion records.
    fn compact_log(&mut self) -> Result<(), DiskLruError> {
        let snap_path = self.base_dir.join("index.snapshot.bin");
        {
            let mut snap = File::create(&snap_path)?;
            // P records in LRU order so a replay rebuilds order + sizes.
            for key in &self.lru_order {
                if let Some(entry) = self.entries.get(key) {
                    writeln!(snap, "P\t{key}\t{}", entry.bytes)?;
                }
            }
            snap.sync_all()?;
        }
        // Close the open append handle before swapping.
        self.index_log = File::create(self.base_dir.join("index.compact-marker"))?;
        fs::rename(&snap_path, &self.index_path)?;
        let _ = fs::remove_file(self.base_dir.join("index.compact-marker"));
        self.index_log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.index_path)?;
        self.log_records = self.lru_order.len();
        Ok(())
    }
}

/// Replay the on-disk log to rebuild the in-memory state.
fn replay_index(
    index_path: &Path,
    entries_dir: &Path,
) -> Result<(HashMap<String, DiskEntry>, Vec<String>, u64), DiskLruError> {
    let mut entries: HashMap<String, DiskEntry> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut bytes: u64 = 0;

    if !index_path.exists() {
        return Ok((entries, order, bytes));
    }

    let mut f = File::open(index_path)?;
    // Make sure we don't read past the end of a truncated last line.
    let file_len = f.seek(SeekFrom::End(0))?;
    f.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::new(f.take(file_len));
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        // Truncated tail record? Read returned bytes but no
        // terminating newline — last record may be partial. Bail.
        if !line.ends_with('\n') {
            break;
        }
        let s = line.trim_end_matches('\n');
        let mut parts = s.split('\t');
        let (Some(op), Some(key), Some(bytes_s)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if op.is_empty() || key.is_empty() {
            continue;
        }
        match op {
            "P" => {
                let Ok(len) = bytes_s.parse::<u64>() else {
                    // Malformed bytes field — skip this record rather
                    // than aborting the whole replay.
                    continue;
                };
                if let Some(prev) = entries.insert(key.to_string(), DiskEntry { bytes: len }) {
                    bytes = bytes.saturating_sub(prev.bytes);
                }
                bytes += len;
                order.retain(|k| k != key);
                order.push(key.to_string());
            }
            "E" => {
                if let Some(prev) = entries.remove(key) {
                    bytes = bytes.saturating_sub(prev.bytes);
                }
                order.retain(|k| k != key);
            }
            "T" => {
                // Touch: move to MRU. Only act if the entry exists.
                if entries.contains_key(key) {
                    order.retain(|k| k != key);
                    order.push(key.to_string());
                }
            }
            _ => {
                // Unknown op — treat as no-op so the cache survives
                // a forward-compat write from a future vance version.
            }
        }
    }

    // Drop entries whose files are missing on disk. Handles
    // mid-write crashes where the index recorded a put but the
    // file got cleaned up.
    let mut bad: Vec<String> = Vec::new();
    for (k, entry) in entries.iter() {
        let p = entries_dir.join(k);
        if !p.exists() {
            bad.push(k.clone());
            bytes = bytes.saturating_sub(entry.bytes);
        }
    }
    for k in bad {
        entries.remove(&k);
        order.retain(|x| x != &k);
    }

    Ok((entries, order, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn put_get_round_trip() {
        let dir = tempdir().unwrap();
        let mut lru = DiskLru::open(dir.path().to_path_buf(), 1024 * 1024).unwrap();
        lru.put("abc", b"hello world").unwrap();
        let hit = lru.get("abc").unwrap().unwrap();
        assert_eq!(hit, b"hello world");
        let s = lru.stats();
        assert_eq!(s.entries, 1);
        assert_eq!(s.bytes, 11);
    }

    #[test]
    fn budget_evicts_lru() {
        let dir = tempdir().unwrap();
        // 10-byte budget; insert four 4-byte entries.
        let mut lru = DiskLru::open(dir.path().to_path_buf(), 10).unwrap();
        for k in ["a", "b", "c", "d"] {
            lru.put(k, b"1234").unwrap();
        }
        // 16 > 10, so at least one is evicted; LRU rule kills 'a' first.
        assert!(lru.get("a").unwrap().is_none(), "a should be evicted");
        // Newest two ('c' + 'd') should be live.
        assert!(lru.get("c").unwrap().is_some());
        assert!(lru.get("d").unwrap().is_some());
    }

    #[test]
    fn promote_on_get_protects_from_eviction() {
        let dir = tempdir().unwrap();
        let mut lru = DiskLru::open(dir.path().to_path_buf(), 8).unwrap();
        lru.put("a", b"1234").unwrap();
        lru.put("b", b"1234").unwrap();
        // Touch 'a' → make 'b' the LRU.
        let _ = lru.get("a").unwrap();
        // Push 'c' in, forcing eviction. 'b' goes, 'a' stays.
        lru.put("c", b"1234").unwrap();
        assert!(lru.get("a").unwrap().is_some(), "a stays (promoted)");
        assert!(lru.get("b").unwrap().is_none(), "b evicted");
        assert!(lru.get("c").unwrap().is_some());
    }

    #[test]
    fn replay_restores_state_across_reopen() {
        let dir = tempdir().unwrap();
        {
            let mut lru = DiskLru::open(dir.path().to_path_buf(), 1024).unwrap();
            lru.put("k1", b"abc").unwrap();
            lru.put("k2", b"defg").unwrap();
        }
        let mut reopened = DiskLru::open(dir.path().to_path_buf(), 1024).unwrap();
        assert_eq!(reopened.get("k1").unwrap().unwrap(), b"abc");
        assert_eq!(reopened.get("k2").unwrap().unwrap(), b"defg");
    }

    #[test]
    fn truncated_index_tail_is_tolerated() {
        let dir = tempdir().unwrap();
        {
            let mut lru = DiskLru::open(dir.path().to_path_buf(), 1024).unwrap();
            lru.put("k1", b"abc").unwrap();
        }
        // Truncate the index file mid-record.
        let idx = dir.path().join("index.bin");
        let mut data = fs::read(&idx).unwrap();
        data.truncate(data.len().saturating_sub(2));
        fs::write(&idx, data).unwrap();
        // Reopen still works; the entry may or may not have survived
        // depending on whether the truncation cut the original P
        // record or a later T.
        let mut reopened = DiskLru::open(dir.path().to_path_buf(), 1024).unwrap();
        // Whether k1 survives depends on the truncation, but reopen
        // must not error.
        let _ = reopened.get("k1");
    }

    #[test]
    fn missing_blob_file_is_purged_on_replay() {
        let dir = tempdir().unwrap();
        {
            let mut lru = DiskLru::open(dir.path().to_path_buf(), 1024).unwrap();
            lru.put("k1", b"abc").unwrap();
        }
        fs::remove_file(dir.path().join("entries/k1")).unwrap();
        let mut reopened = DiskLru::open(dir.path().to_path_buf(), 1024).unwrap();
        assert!(reopened.get("k1").unwrap().is_none());
    }

    #[test]
    fn overwrite_replaces_bytes_in_place() {
        let dir = tempdir().unwrap();
        let mut lru = DiskLru::open(dir.path().to_path_buf(), 1024).unwrap();
        lru.put("k1", b"first").unwrap();
        lru.put("k1", b"second-larger").unwrap();
        assert_eq!(lru.get("k1").unwrap().unwrap(), b"second-larger");
        assert_eq!(lru.stats().entries, 1);
    }
}
