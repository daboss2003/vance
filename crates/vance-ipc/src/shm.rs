//! Shared-memory frame regions (ADR 0006).
//!
//! The parent creates a region of equal-size slots, sends its
//! [`ShmRegionDesc`] to the child over the pipe, and unlinks the OS name
//! once the child has attached (or failed to). The producer side fills a
//! slot through a [`WriteSlot`] and publishes it as a [`SlotRef`] inside a
//! pipe message; the consumer adopts it into a [`Lease`] (or copies it out
//! when over its lease budget) and hands it back through a
//! [`ReleaseQueue`], which the next pipe message drains. No control data
//! lives in shared memory: every ownership transfer is a pipe message, so
//! ordering comes from the pipe.
//!
//! Regions only grow: a bigger frame size means a new region with a higher
//! `epoch`, and references to an older epoch are stale.
//!
//! This build has no OS layer yet: [`ShmRegion::create`] and
//! [`ShmRegion::open`] return [`Error::ShmUnsupported`], so every caller
//! takes the inline-blob fallback. The slot bookkeeping below is complete.

use crate::{Error, Result};
use parking_lot::{Condvar, Mutex};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Everything a child needs to attach to a parent's region. Sent over the
/// pipe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShmRegionDesc {
    /// Region generation. Strictly increases each time the parent replaces
    /// its region, so references to an older region can be told apart.
    pub epoch: u32,
    /// OS object name, from [`object_name`].
    pub os_name: String,
    /// Number of slots.
    pub slot_count: u32,
    /// Bytes per slot, a multiple of [`SLOT_ALIGN`].
    pub slot_size: u64,
}

/// A published slot: which region, which slot, which use of that slot, and
/// how many bytes of it are valid. Sent over the pipe in place of the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SlotRef {
    /// Epoch of the region the slot lives in.
    pub epoch: u32,
    /// Slot index within the region.
    pub slot: u32,
    /// Bumped every time the slot is published, so a late or duplicate
    /// release of an earlier use is rejected.
    pub generation: u32,
    /// Valid bytes, from the start of the slot.
    pub len: u64,
}

/// Slot sizes are rounded up to this. At least the 16 KiB page size of
/// Apple Silicon, and equal to the Windows allocation granularity.
pub const SLOT_ALIGN: u64 = 64 * 1024;

/// How a process maps a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Consumer side (the parent maps decoder regions read-only).
    ReadOnly,
    /// Producer side.
    ReadWrite,
}

/// THE name of an OS shared-memory object. Every region name comes from
/// here, so a platform prefix (such as a macOS App Sandbox app-group
/// identifier) is a change in one place.
///
/// `nonce` should be random per region so a stale object left by a crashed
/// process with a recycled pid can't collide.
pub fn object_name(pid: u32, epoch: u32, nonce: u32) -> String {
    // Intentional: fixed-width hex with no separators. macOS caps POSIX shm
    // names at 31 bytes (PSHMNAMLEN), and "/vance" + 3 × 8 hex digits is 30
    // for every input.
    let unique = format!("vance{pid:08x}{epoch:08x}{nonce:08x}");
    if cfg!(windows) {
        // Per-session namespace: no privilege needed, invisible to other
        // logon sessions.
        format!("Local\\{unique}")
    } else {
        format!("/{unique}")
    }
}

static LIVE_REGIONS: AtomicUsize = AtomicUsize::new(0);

/// Regions currently mapped by this process (for leak tests).
pub fn live_region_count() -> usize {
    LIVE_REGIONS.load(Ordering::SeqCst)
}

/// One mapped shared-memory region: `slot_count` slots of `slot_size` bytes.
pub struct ShmRegion {
    ptr: *mut u8,
    len: u64,
    desc: ShmRegionDesc,
    /// Test-only regions are plain heap memory, freed on drop.
    heap: bool,
}

// SAFETY: the mapping is plain memory that stays valid until drop, and the
// region itself never reads or writes it. Access goes through `WriteSlot`
// (exclusive ownership of one slot, handed out by `SlotPool`) or `Lease`
// (a published slot the producer won't touch again until it's released).
unsafe impl Send for ShmRegion {}
// SAFETY: as above; shared references only hand out disjoint slot views.
unsafe impl Sync for ShmRegion {}

impl ShmRegion {
    /// Parent side: create and map a new region named by [`object_name`].
    pub fn create(epoch: u32, slot_count: u32, slot_size: u64, access: Access) -> Result<Self> {
        let _ = (epoch, slot_count, slot_size, access);
        Err(Error::ShmUnsupported)
    }

    /// Child side: map the region `desc` describes. Validates that the OS
    /// object is at least `slot_count * slot_size` bytes.
    pub fn open(desc: &ShmRegionDesc, access: Access) -> Result<Self> {
        let _ = (desc, access);
        Err(Error::ShmUnsupported)
    }

    /// Remove the OS name so nothing else can attach and the object is freed
    /// once both mappings are gone. Idempotent; a no-op on Windows, where
    /// the section dies with its last handle.
    pub fn unlink_name(&mut self) -> Result<()> {
        Ok(())
    }

    /// The descriptor the child attaches with.
    pub fn desc(&self) -> &ShmRegionDesc {
        &self.desc
    }

    /// Mapped size in bytes (for the engine's memory budget).
    pub fn byte_len(&self) -> u64 {
        self.len
    }

    /// Start of `slot`. Callers check `slot < slot_count`, which keeps the
    /// whole slot inside the mapping (`len == slot_count * slot_size`).
    fn slot_ptr(&self, slot: u32) -> *mut u8 {
        self.ptr
            .wrapping_add((u64::from(slot) * self.desc.slot_size) as usize)
    }

    /// Heap-backed stand-in for a mapping, so the bookkeeping is testable
    /// without an OS layer.
    #[cfg(test)]
    fn heap_for_tests(epoch: u32, slot_count: u32, slot_size: u64) -> Self {
        let len = u64::from(slot_count) * slot_size;
        let mem = vec![0u8; len as usize].into_boxed_slice();
        LIVE_REGIONS.fetch_add(1, Ordering::SeqCst);
        Self {
            ptr: Box::into_raw(mem).cast::<u8>(),
            len,
            desc: ShmRegionDesc {
                epoch,
                os_name: object_name(std::process::id(), epoch, 0),
                slot_count,
                slot_size,
            },
            heap: true,
        }
    }
}

impl Drop for ShmRegion {
    fn drop(&mut self) {
        if self.heap {
            let mem = std::ptr::slice_from_raw_parts_mut(self.ptr, self.len as usize);
            // SAFETY: `ptr`/`len` came from `Box::into_raw` of exactly this
            // slice in `heap_for_tests`, and nothing borrows the region any
            // more (we are its last owner).
            drop(unsafe { Box::from_raw(mem) });
        }
        LIVE_REGIONS.fetch_sub(1, Ordering::SeqCst);
    }
}

impl fmt::Debug for ShmRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShmRegion")
            .field("desc", &self.desc)
            .field("byte_len", &self.len)
            .finish()
    }
}

/// A release that doesn't match a slot currently out: wrong epoch, slot out
/// of range, already released, or an older generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("stale shared-memory slot release: {0:?}")]
pub struct StaleSlot(pub SlotRef);

/// Producer side: hands out free slots of one region, LIFO (the most
/// recently freed slot is the one most likely still in cache).
pub struct SlotPool {
    region: Arc<ShmRegion>,
    state: Mutex<PoolState>,
    freed: Condvar,
}

struct PoolState {
    /// Free slot indices; the last one is handed out next.
    free: Vec<u32>,
    /// Per slot: generation of its latest publish.
    generation: Vec<u32>,
    /// Per slot: published and not yet released.
    out: Vec<bool>,
}

impl SlotPool {
    /// A pool with every slot of `region` free.
    pub fn new(region: Arc<ShmRegion>) -> Arc<Self> {
        let n = region.desc.slot_count as usize;
        Arc::new(Self {
            region,
            state: Mutex::new(PoolState {
                free: (0..n as u32).rev().collect(),
                generation: vec![0; n],
                out: vec![false; n],
            }),
            freed: Condvar::new(),
        })
    }

    /// A free slot, or `None` if all are in use. Never blocks: the only call
    /// a thread that also receives releases may make.
    pub fn try_acquire(self: &Arc<Self>) -> Option<WriteSlot> {
        let slot = self.state.lock().free.pop()?;
        Some(self.write_slot(slot))
    }

    /// Wait up to `t` for a free slot. Worker threads only: the thread that
    /// receives releases must use [`SlotPool::try_acquire`].
    pub fn acquire_timeout(self: &Arc<Self>, t: Duration) -> Option<WriteSlot> {
        let deadline = Instant::now() + t;
        let mut state = self.state.lock();
        loop {
            if let Some(slot) = state.free.pop() {
                drop(state);
                return Some(self.write_slot(slot));
            }
            if self.freed.wait_until(&mut state, deadline).timed_out() {
                let slot = state.free.pop()?;
                drop(state);
                return Some(self.write_slot(slot));
            }
        }
    }

    /// The consumer is done with a published slot: it becomes free again.
    /// Rejects anything that isn't the slot's current, unreleased publish.
    pub fn release(&self, r: SlotRef) -> std::result::Result<(), StaleSlot> {
        let mut state = self.state.lock();
        let i = r.slot as usize;
        let current = r.epoch == self.region.desc.epoch
            && i < state.out.len()
            && state.out[i]
            && state.generation[i] == r.generation;
        if !current {
            return Err(StaleSlot(r));
        }
        state.out[i] = false;
        state.free.push(r.slot);
        drop(state);
        self.freed.notify_one();
        Ok(())
    }

    fn write_slot(self: &Arc<Self>, slot: u32) -> WriteSlot {
        WriteSlot {
            pool: Arc::clone(self),
            slot,
            published: false,
        }
    }

    /// An acquired slot comes back without being published.
    fn put_back(&self, slot: u32) {
        self.state.lock().free.push(slot);
        self.freed.notify_one();
    }
}

/// Exclusive write access to one slot. Dropping it unpublished returns the
/// slot to the pool.
pub struct WriteSlot {
    pool: Arc<SlotPool>,
    slot: u32,
    /// Set by `publish`: the slot then belongs to the consumer, not to Drop.
    published: bool,
}

impl WriteSlot {
    /// The whole slot (`slot_size` bytes).
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        let region = &self.pool.region;
        // SAFETY: the pool hands each free slot to exactly one WriteSlot, the
        // slot lies inside the mapping (index from the pool's own range), and
        // nobody reads it until it is published. Only producers (read-write
        // mappings) create pools.
        unsafe {
            std::slice::from_raw_parts_mut(
                region.slot_ptr(self.slot),
                region.desc.slot_size as usize,
            )
        }
    }

    /// Hand the first `len` bytes to the consumer. The slot stays out of the
    /// pool until [`SlotPool::release`] gets this reference back.
    pub fn publish(mut self, len: usize) -> SlotRef {
        let slot_size = self.pool.region.desc.slot_size;
        debug_assert!(
            len as u64 <= slot_size,
            "published {len} > slot {slot_size}"
        );
        let mut state = self.pool.state.lock();
        let i = self.slot as usize;
        state.generation[i] = state.generation[i].wrapping_add(1);
        state.out[i] = true;
        let r = SlotRef {
            epoch: self.pool.region.desc.epoch,
            slot: self.slot,
            generation: state.generation[i],
            len: (len as u64).min(slot_size),
        };
        drop(state);
        self.published = true;
        r
    }
}

impl Drop for WriteSlot {
    fn drop(&mut self) {
        if !self.published {
            self.pool.put_back(self.slot);
        }
    }
}

/// Consumer side: slots whose leases ended, waiting to go back to the
/// producer with the next pipe message.
#[derive(Debug, Default)]
pub struct ReleaseQueue {
    pending: Mutex<Vec<SlotRef>>,
}

impl ReleaseQueue {
    /// Take everything queued so far.
    pub fn drain(&self) -> Vec<SlotRef> {
        std::mem::take(&mut *self.pending.lock())
    }

    fn push(&self, r: SlotRef) {
        self.pending.lock().push(r);
    }
}

/// What [`LeaseTable::adopt`] made of a published slot.
#[derive(Debug)]
pub enum Adopted {
    /// Zero-copy view, valid until dropped.
    Leased(Lease),
    /// Over the lease budget: the bytes were copied out and the slot queued
    /// for release straight away.
    Spilled(Vec<u8>),
}

/// Consumer side: turns published slots of one region into leases, at most
/// `max_leases` at a time. Past the budget it copies frames out instead, so
/// the producer can never run out of slots because of the consumer.
pub struct LeaseTable {
    region: Arc<ShmRegion>,
    queue: Arc<ReleaseQueue>,
    outstanding: AtomicUsize,
    max_leases: usize,
}

impl LeaseTable {
    /// A table for `region` whose ended leases go to `q`.
    pub fn new(region: Arc<ShmRegion>, q: Arc<ReleaseQueue>, max_leases: usize) -> Arc<Self> {
        Arc::new(Self {
            region,
            queue: q,
            outstanding: AtomicUsize::new(0),
            max_leases,
        })
    }

    /// Take ownership of a published slot. Rejects a reference that doesn't
    /// fit this region (other epoch, slot out of range, `len` past the slot).
    pub fn adopt(self: &Arc<Self>, r: SlotRef) -> Result<Adopted> {
        let desc = &self.region.desc;
        if r.epoch != desc.epoch || r.slot >= desc.slot_count || r.len > desc.slot_size {
            return Err(Error::Shm(format!(
                "slot reference {r:?} doesn't fit region epoch {} ({} slots of {} bytes)",
                desc.epoch, desc.slot_count, desc.slot_size
            )));
        }
        let under_budget = self
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.max_leases).then_some(n + 1)
            })
            .is_ok();
        if under_budget {
            return Ok(Adopted::Leased(Lease {
                table: Arc::clone(self),
                slot: r,
            }));
        }
        let bytes = self.bytes(r).to_vec();
        self.queue.push(r);
        Ok(Adopted::Spilled(bytes))
    }

    fn bytes(&self, r: SlotRef) -> &[u8] {
        // SAFETY: `adopt` bounds-checked `r` against this region, and the
        // producer handed the slot over (published it) and won't write it
        // again until the release we queue when done with these bytes.
        unsafe { std::slice::from_raw_parts(self.region.slot_ptr(r.slot), r.len as usize) }
    }
}

/// A zero-copy view of one published slot. Dropping it queues the slot's
/// release.
pub struct Lease {
    table: Arc<LeaseTable>,
    slot: SlotRef,
}

impl Deref for Lease {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.table.bytes(self.slot)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.table.outstanding.fetch_sub(1, Ordering::AcqRel);
        self.table.queue.push(self.slot);
    }
}

impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease").field("slot", &self.slot).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOT: u64 = SLOT_ALIGN;

    fn pool(slots: u32) -> Arc<SlotPool> {
        SlotPool::new(Arc::new(ShmRegion::heap_for_tests(1, slots, SLOT)))
    }

    #[test]
    fn object_names_fit_macos_limit_and_differ() {
        let names = [
            object_name(0, 0, 0),
            object_name(u32::MAX, u32::MAX, u32::MAX),
            object_name(1, 2, 3),
            object_name(1, 2, 4),
            object_name(1, 3, 3),
            object_name(2, 2, 3),
        ];
        for n in &names {
            assert!(
                n.len() <= 31 || cfg!(windows),
                "{n} is longer than 31 bytes"
            );
            let unique_part = n.trim_start_matches("Local\\").trim_start_matches('/');
            assert!(!unique_part.contains(['/', '\\']), "{n}");
        }
        let distinct: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(distinct.len(), names.len());
    }

    #[test]
    fn os_layer_is_unsupported_for_now() {
        let r = ShmRegion::create(1, 4, SLOT, Access::ReadWrite);
        assert!(matches!(r, Err(Error::ShmUnsupported)), "{r:?}");
        let desc = ShmRegionDesc {
            epoch: 1,
            os_name: object_name(1, 1, 1),
            slot_count: 4,
            slot_size: SLOT,
        };
        let r = ShmRegion::open(&desc, Access::ReadOnly);
        assert!(matches!(r, Err(Error::ShmUnsupported)), "{r:?}");
    }

    #[test]
    fn slots_are_reused_lifo_and_unpublished_drop_returns_them() {
        let pool = pool(3);
        let a = pool.try_acquire().unwrap();
        let b = pool.try_acquire().unwrap();
        let c = pool.try_acquire().unwrap();
        assert_eq!((a.slot, b.slot, c.slot), (0, 1, 2));
        assert!(pool.try_acquire().is_none());
        drop(b);
        assert_eq!(pool.try_acquire().unwrap().slot, 1, "dropped slot is next");
        let ra = a.publish(10);
        let rc = c.publish(10);
        pool.release(ra).unwrap();
        pool.release(rc).unwrap();
        assert_eq!(pool.try_acquire().unwrap().slot, 2, "last released first");
    }

    #[test]
    fn releases_are_generation_checked() {
        let pool = pool(1);
        let r1 = pool.try_acquire().unwrap().publish(5);
        assert_eq!(r1.len, 5);
        pool.release(r1).unwrap();
        assert_eq!(pool.release(r1), Err(StaleSlot(r1)), "double release");
        let r2 = pool.try_acquire().unwrap().publish(5);
        assert_eq!(r2.slot, r1.slot);
        assert_ne!(r2.generation, r1.generation);
        assert!(pool.release(r1).is_err(), "earlier use of the same slot");
        let other_epoch = SlotRef { epoch: 9, ..r2 };
        assert!(pool.release(other_epoch).is_err());
        let out_of_range = SlotRef { slot: 7, ..r2 };
        assert!(pool.release(out_of_range).is_err());
        pool.release(r2).unwrap();
    }

    #[test]
    fn acquire_timeout_waits_for_a_release() {
        let pool = pool(1);
        let r = pool.try_acquire().unwrap().publish(1);
        assert!(pool.acquire_timeout(Duration::from_millis(20)).is_none());
        let releaser = {
            let pool = Arc::clone(&pool);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(30));
                pool.release(r).unwrap();
            })
        };
        assert!(pool.acquire_timeout(Duration::from_secs(5)).is_some());
        releaser.join().unwrap();
    }

    #[test]
    fn leases_read_published_bytes_and_spill_past_the_budget() {
        let region = Arc::new(ShmRegion::heap_for_tests(3, 4, SLOT));
        let pool = SlotPool::new(Arc::clone(&region));
        let queue = Arc::new(ReleaseQueue::default());
        let table = LeaseTable::new(Arc::clone(&region), Arc::clone(&queue), 2);

        let mut refs = Vec::new();
        for i in 0..3u8 {
            let mut w = pool.try_acquire().unwrap();
            w.as_mut_slice()[..4].copy_from_slice(&[i; 4]);
            refs.push(w.publish(4));
        }
        let l0 = match table.adopt(refs[0]).unwrap() {
            Adopted::Leased(l) => l,
            other => panic!("{other:?}"),
        };
        let l1 = match table.adopt(refs[1]).unwrap() {
            Adopted::Leased(l) => l,
            other => panic!("{other:?}"),
        };
        assert_eq!(&l0[..], [0; 4]);
        assert_eq!(&l1[..], [1; 4]);
        match table.adopt(refs[2]).unwrap() {
            Adopted::Spilled(bytes) => assert_eq!(bytes, [2; 4]),
            other => panic!("third frame should spill, got {other:?}"),
        }
        assert_eq!(queue.drain(), [refs[2]], "spilled slot released at once");
        drop(l1);
        assert_eq!(queue.drain(), [refs[1]]);
        assert!(queue.drain().is_empty());
        // Budget freed: the next frame is leased again.
        assert!(matches!(table.adopt(refs[1]).unwrap(), Adopted::Leased(_)));
        drop(l0);
    }

    #[test]
    fn adopt_rejects_references_that_do_not_fit() {
        let region = Arc::new(ShmRegion::heap_for_tests(5, 2, SLOT));
        let table = LeaseTable::new(region, Arc::default(), 2);
        let ok = SlotRef {
            epoch: 5,
            slot: 1,
            generation: 1,
            len: SLOT,
        };
        assert!(table.adopt(ok).is_ok());
        for bad in [
            SlotRef { epoch: 4, ..ok },
            SlotRef { slot: 2, ..ok },
            SlotRef {
                len: SLOT + 1,
                ..ok
            },
        ] {
            assert!(matches!(table.adopt(bad), Err(Error::Shm(_))), "{bad:?}");
        }
    }

    #[test]
    fn a_published_slot_does_not_keep_its_pool_alive() {
        let region = Arc::new(ShmRegion::heap_for_tests(1, 1, SLOT));
        let pool = SlotPool::new(Arc::clone(&region));
        let _ = pool.try_acquire().unwrap().publish(1);
        drop(pool);
        assert_eq!(Arc::strong_count(&region), 1, "pool leaked");
    }

    #[test]
    fn leases_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Lease>();
        assert_send_sync::<WriteSlot>();
        assert_send_sync::<Arc<SlotPool>>();
    }
}
