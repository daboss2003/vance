//! Per-source in-memory frame ring.
//!
//! One ring per source URI. Keyed by `(pts, transform_hash)`. Reads
//! are O(log n) via a BTreeMap. Writes evict the oldest insertion
//! when the running byte budget is exceeded (FIFO eviction — the
//! "ring" semantics).
//!
//! FIFO instead of LRU because the dominant access pattern is "scrub
//! a contiguous range," not "hop randomly across PTSes." A small
//! LRU on top would help random-access scrubs but adds a per-frame
//! touch — the in-memory ring is already small enough that FIFO is
//! fast and simple.

use std::collections::{BTreeMap, VecDeque};

/// One entry stored in the ring.
#[derive(Clone, Debug)]
struct Entry {
    pts: i64,
    transform_hash: u64,
    bytes: Vec<u8>,
}

/// Bounded in-memory frame buffer for ONE source URI.
pub struct FrameCacheRing {
    /// (pts, transform_hash) → ordinal index into `order`.
    index: BTreeMap<(i64, u64), usize>,
    /// Insertion order, with `Option` slots so removed entries don't
    /// invalidate ordinals in `index`. Compacted lazily.
    order: VecDeque<Option<Entry>>,
    /// Total bytes currently held by live entries.
    bytes_used: usize,
    /// Maximum bytes we'll hold before evicting the oldest insertion.
    byte_budget: usize,
}

impl FrameCacheRing {
    /// New ring with the given byte budget.
    pub fn new(byte_budget: usize) -> Self {
        Self {
            index: BTreeMap::new(),
            order: VecDeque::new(),
            bytes_used: 0,
            byte_budget,
        }
    }

    /// Bytes currently held.
    pub fn bytes_used(&self) -> usize {
        self.bytes_used
    }

    /// Number of live entries.
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Whether the ring holds any entries.
    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Get a frame's bytes by (pts, transform_hash). `None` on miss.
    pub fn get(&self, pts: i64, transform_hash: u64) -> Option<Vec<u8>> {
        let ord = self.index.get(&(pts, transform_hash))?;
        self.order
            .get(*ord)
            .and_then(|slot| slot.as_ref())
            .map(|e| e.bytes.clone())
    }

    /// Insert (or replace) a frame's bytes. Evicts the oldest live
    /// entry until the budget is satisfied.
    pub fn put(&mut self, pts: i64, transform_hash: u64, bytes: Vec<u8>) {
        // Replace path — remove the existing entry first so the
        // budget accounting stays correct.
        if let Some(&existing_ord) = self.index.get(&(pts, transform_hash)) {
            if let Some(slot) = self.order.get_mut(existing_ord) {
                if let Some(old) = slot.take() {
                    self.bytes_used = self.bytes_used.saturating_sub(old.bytes.len());
                }
            }
            self.index.remove(&(pts, transform_hash));
        }
        let new_bytes = bytes.len();
        self.bytes_used += new_bytes;
        let entry = Entry {
            pts,
            transform_hash,
            bytes,
        };
        let ord = self.order.len();
        self.order.push_back(Some(entry));
        self.index.insert((pts, transform_hash), ord);
        self.evict_to_budget();
    }

    fn evict_to_budget(&mut self) {
        while self.bytes_used > self.byte_budget && !self.order.is_empty() {
            // Pop from the front until we hit a live entry to evict.
            // Empty slots are dead weight; compact them as we go.
            while let Some(front) = self.order.front() {
                if front.is_none() {
                    self.order.pop_front();
                    self.rebase_index_after_front_pop();
                } else {
                    break;
                }
            }
            if let Some(front) = self.order.pop_front() {
                self.rebase_index_after_front_pop();
                if let Some(entry) = front {
                    self.bytes_used = self.bytes_used.saturating_sub(entry.bytes.len());
                    self.index.remove(&(entry.pts, entry.transform_hash));
                }
            } else {
                break;
            }
        }
    }

    /// After popping the front of `order`, every stored ordinal in
    /// `index` is off by one. We walk the index once and decrement.
    fn rebase_index_after_front_pop(&mut self) {
        for ord in self.index.values_mut() {
            *ord = ord.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_after_put_returns_bytes() {
        let mut r = FrameCacheRing::new(1024);
        r.put(1, 0, b"hello".to_vec());
        assert_eq!(r.get(1, 0).unwrap(), b"hello");
        assert_eq!(r.bytes_used(), 5);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn miss_returns_none() {
        let r = FrameCacheRing::new(1024);
        assert!(r.get(99, 0).is_none());
    }

    #[test]
    fn budget_evicts_fifo() {
        // 3-byte budget; insert 4 entries of 1 byte each.
        let mut r = FrameCacheRing::new(3);
        for i in 1..=4i64 {
            r.put(i, 0, vec![i as u8]);
        }
        assert!(r.bytes_used() <= 3);
        // Oldest one (pts=1) must have been evicted.
        assert!(r.get(1, 0).is_none());
        // pts=4 must still be present.
        assert_eq!(r.get(4, 0).unwrap(), vec![4u8]);
    }

    #[test]
    fn replace_same_key_does_not_double_count() {
        let mut r = FrameCacheRing::new(1024);
        r.put(1, 0, vec![0u8; 100]);
        r.put(1, 0, vec![0u8; 50]);
        // Budget accounting: 50 bytes, not 150.
        assert_eq!(r.bytes_used(), 50);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn transform_hash_distinguishes_variants() {
        let mut r = FrameCacheRing::new(1024);
        r.put(1, 0, b"raw".to_vec());
        r.put(1, 0xdeadbeef, b"transformed".to_vec());
        assert_eq!(r.get(1, 0).unwrap(), b"raw");
        assert_eq!(r.get(1, 0xdeadbeef).unwrap(), b"transformed");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn zero_budget_evicts_immediately() {
        let mut r = FrameCacheRing::new(0);
        r.put(1, 0, b"x".to_vec());
        // Over budget instantly — evict the entry we just inserted.
        assert!(r.is_empty());
        assert_eq!(r.bytes_used(), 0);
    }
}
