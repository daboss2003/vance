//! Where packed frame bytes go.
//!
//! `pack` asks the [`Allocator`] for a buffer of exactly a frame's size and
//! appends the frame's rows to it; `main` turns the filled buffer into the
//! reply's [`FramePayload`] when it answers the request.
//!
//! Once the parent has attached a shared-memory region (ADR 0006), buffers
//! are slots of that region, so the rows are written straight into memory
//! the parent maps. A slot travels unpublished through the stream channel
//! and is published only in [`FrameBuf::into_payload`], so dropping an
//! unsent buffer (as the stream's drain on stop does) gives its slot back.
//! Without a region, or when a frame doesn't fit a slot, or when the main
//! thread finds no slot free, a buffer is a heap `Vec`, sent inline as a raw
//! blob after the reply.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use vance_decoder::protocol::{FramePayload, ShmRegionDesc, SlotRef};
use vance_ipc::shm::{Access, ShmRegion, SlotPool, WriteSlot};

/// How long the decode thread waits for a free slot before it looks at its
/// stop flag again.
const SLOT_WAIT: Duration = Duration::from_millis(50);

/// How long [`Allocator::alloc`] may wait for a free buffer.
pub enum Wait<'a> {
    /// Never block. For the sidecar main thread: it is also the thread that
    /// receives `Release`, so waiting there for a buffer to come back would
    /// deadlock.
    Never,
    /// For the stream decode thread: may wait for a buffer, but gives up
    /// (returns `None`) once `stop` is set.
    UntilStopped(&'a AtomicBool),
}

/// Hands out frame buffers. `Default` is heap-only until
/// [`Allocator::attach`]. Cheap to clone; clones share the attached region
/// (the stream decode thread gets its own clone).
#[derive(Clone, Default)]
pub struct Allocator {
    slots: Arc<Mutex<Slots>>,
}

#[derive(Default)]
struct Slots {
    /// The region new buffers come from.
    current: Option<Attached>,
    /// Epoch of the newest region ever attached.
    newest_epoch: Option<u32>,
}

struct Attached {
    pool: Arc<SlotPool>,
    slot_size: u64,
}

impl Allocator {
    /// A buffer for exactly `len` bytes. `None` only when `wait` is
    /// `UntilStopped` and the stop flag is set.
    pub fn alloc(&self, len: usize, wait: Wait<'_>) -> Option<FrameBuf> {
        if let Wait::UntilStopped(stop) = wait {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
        }
        let pool = self
            .lock()
            .current
            .as_ref()
            .filter(|a| len as u64 <= a.slot_size)
            .map(|a| Arc::clone(&a.pool));
        let slot = match (pool, wait) {
            (None, _) => None,
            (Some(pool), Wait::Never) => pool.try_acquire(),
            // Every slot is with the parent or queued in the stream channel;
            // one comes back with the parent's next `Release`, or the
            // channel's drain on stop.
            (Some(pool), Wait::UntilStopped(stop)) => loop {
                if let Some(slot) = pool.acquire_timeout(SLOT_WAIT) {
                    break Some(slot);
                }
                if stop.load(Ordering::Relaxed) {
                    return None;
                }
            },
        };
        Some(match slot {
            Some(slot) => FrameBuf::Slot { slot, len: 0 },
            None => FrameBuf::Heap(Vec::with_capacity(len)),
        })
    }

    /// Back future buffers with the parent's shared-memory region `desc`,
    /// mapped read-write. Replaces any earlier region: the slots it still
    /// has out stay valid until they drop, and releases for them are stale.
    /// Fails (and leaves every frame inline) unless `desc` is newer than
    /// every region attached before and maps as described.
    pub fn attach(&mut self, desc: &ShmRegionDesc) -> vance_ipc::Result<()> {
        let mut slots = self.lock();
        // Intentional: the old region is dropped even when this attach
        // fails. The parent hears `ShmUnavailable`, so from then on every
        // frame must be inline, whichever region it had before.
        slots.current = None;
        if let Some(newest) = slots.newest_epoch.filter(|&e| desc.epoch <= e) {
            // Generations restart at a new region, so a reused epoch would
            // let a release for the old region free a slot of the new one.
            return Err(vance_ipc::Error::Shm(format!(
                "region epoch {} is not newer than attached epoch {newest}",
                desc.epoch
            )));
        }
        let region = ShmRegion::open(desc, Access::ReadWrite)?;
        slots.current = Some(Attached {
            pool: SlotPool::new(Arc::new(region)),
            slot_size: desc.slot_size,
        });
        slots.newest_epoch = Some(desc.epoch);
        Ok(())
    }

    /// Take back slots the parent is done with; returns how many were stale
    /// (another region's epoch, already released, or an older generation).
    /// With no region attached, every slot is.
    pub fn release(&self, slots: &[SlotRef]) -> u32 {
        let pool = self.lock().current.as_ref().map(|a| Arc::clone(&a.pool));
        let stale = slots
            .iter()
            .filter(|&&r| pool.as_ref().is_none_or(|p| p.release(r).is_err()))
            .count();
        u32::try_from(stale).unwrap_or(u32::MAX)
    }

    fn lock(&self) -> MutexGuard<'_, Slots> {
        // Nothing panics while holding the lock, and each field is only
        // ever replaced whole, so a poisoned lock's data is still sound.
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A frame buffer: filled by `pack`, sent by `main`.
pub enum FrameBuf {
    /// Heap bytes, sent as an inline blob after the reply.
    Heap(Vec<u8>),
    /// An unpublished shared-memory slot and how many bytes of it are filled.
    /// Dropped unpublished, it goes back to its pool.
    Slot { slot: WriteSlot, len: usize },
}

impl FrameBuf {
    /// Append `bytes` (one row of a plane).
    pub fn push(&mut self, bytes: &[u8]) {
        match self {
            FrameBuf::Heap(v) => v.extend_from_slice(bytes),
            FrameBuf::Slot { slot, len } => {
                let end = *len + bytes.len();
                slot.as_mut_slice()[*len..end].copy_from_slice(bytes);
                *len = end;
            }
        }
    }

    /// The reply payload, plus the blob that must follow the reply for an
    /// inline payload. Publishing hands a slot to the parent until it
    /// releases it.
    pub fn into_payload(self) -> (FramePayload, Option<Vec<u8>>) {
        match self {
            FrameBuf::Heap(v) => (
                FramePayload::Inline {
                    len: v.len() as u64,
                },
                Some(v),
            ),
            FrameBuf::Slot { slot, len } => (FramePayload::Shared(slot.publish(len)), None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vance_ipc::shm::{Adopted, LeaseTable, ReleaseQueue, SLOT_ALIGN};

    const SLOT: usize = SLOT_ALIGN as usize;

    /// The parent's side of a region: `slots` slots of one `SLOT_ALIGN`.
    fn region(epoch: u32, slots: u32) -> Arc<ShmRegion> {
        Arc::new(ShmRegion::create(epoch, slots, SLOT_ALIGN, Access::ReadWrite).unwrap())
    }

    fn attached(region: &ShmRegion) -> Allocator {
        let mut alloc = Allocator::default();
        alloc.attach(region.desc()).unwrap();
        alloc
    }

    fn shared(buf: FrameBuf) -> SlotRef {
        match buf.into_payload() {
            (FramePayload::Shared(r), None) => r,
            (other, blob) => panic!("expected a published slot, got {other:?} / {blob:?}"),
        }
    }

    fn is_heap(buf: &FrameBuf) -> bool {
        matches!(buf, FrameBuf::Heap(_))
    }

    /// What the parent reads at `r`.
    fn parent_bytes(region: &Arc<ShmRegion>, r: SlotRef) -> Vec<u8> {
        let table = LeaseTable::new(Arc::clone(region), Arc::new(ReleaseQueue::default()), 1);
        match table.adopt(r).unwrap() {
            Adopted::Leased(lease) => lease.to_vec(),
            Adopted::Spilled(bytes) => bytes,
        }
    }

    #[test]
    fn heap_buffers_go_inline_with_exactly_the_pushed_bytes() {
        let mut buf = Allocator::default().alloc(5, Wait::Never).unwrap();
        buf.push(&[1, 2]);
        buf.push(&[3, 4, 5]);
        let (payload, blob) = buf.into_payload();
        assert_eq!(payload, FramePayload::Inline { len: 5 });
        assert_eq!(blob.unwrap(), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_stopped_stream_gets_no_buffer() {
        let stop = AtomicBool::new(false);
        let alloc = Allocator::default();
        assert!(alloc.alloc(1, Wait::UntilStopped(&stop)).is_some());
        stop.store(true, Ordering::Relaxed);
        assert!(alloc.alloc(1, Wait::UntilStopped(&stop)).is_none());
        assert!(alloc.alloc(1, Wait::Never).is_some());
    }

    #[test]
    fn without_a_region_shm_is_unavailable_and_every_release_is_stale() {
        let mut alloc = Allocator::default();
        let desc = ShmRegionDesc {
            epoch: 1,
            os_name: vance_ipc::shm::object_name(1, 1, 1),
            slot_count: 3,
            slot_size: SLOT_ALIGN,
        };
        assert!(alloc.attach(&desc).is_err());
        assert!(is_heap(&alloc.alloc(1, Wait::Never).unwrap()));
        let slot = SlotRef {
            epoch: 1,
            slot: 0,
            generation: 1,
            len: 1,
        };
        assert_eq!(alloc.release(&[slot, slot]), 2);
    }

    #[test]
    fn attached_frames_are_written_into_a_slot_and_published() {
        let region = region(1, 2);
        let alloc = attached(&region);
        let mut buf = alloc.alloc(5, Wait::Never).unwrap();
        assert!(!is_heap(&buf), "an attached region backs frames");
        buf.push(&[1, 2]);
        buf.push(&[3, 4, 5]);
        let r = shared(buf);
        assert_eq!((r.epoch, r.slot, r.len), (1, 0, 5));
        assert_eq!(parent_bytes(&region, r), [1, 2, 3, 4, 5]);

        // A slot-sized frame fits; a bigger one goes inline.
        let full = alloc.alloc(SLOT, Wait::Never).unwrap();
        assert!(!is_heap(&full));
        drop(full);
        let stop = AtomicBool::new(false);
        let big = alloc.alloc(SLOT + 1, Wait::UntilStopped(&stop)).unwrap();
        assert!(is_heap(&big), "a frame larger than a slot goes inline");
    }

    #[test]
    fn clones_share_a_region_attached_later() {
        let region = region(1, 1);
        let mut alloc = Allocator::default();
        let clone = alloc.clone();
        alloc.attach(region.desc()).unwrap();
        assert!(!is_heap(&clone.alloc(1, Wait::Never).unwrap()));
    }

    #[test]
    fn the_main_thread_goes_inline_when_no_slot_is_free() {
        let region = region(1, 1);
        let alloc = attached(&region);
        let held = alloc.alloc(1, Wait::Never).unwrap();
        assert!(!is_heap(&held));
        assert!(is_heap(&alloc.alloc(1, Wait::Never).unwrap()));
        // An unsent (unpublished) buffer gives its slot back when dropped.
        drop(held);
        assert!(!is_heap(&alloc.alloc(1, Wait::Never).unwrap()));
    }

    #[test]
    fn the_decode_thread_waits_for_a_release_or_its_stop_flag() {
        let region = region(1, 1);
        let alloc = attached(&region);
        let r = shared(alloc.alloc(1, Wait::Never).unwrap());

        let stop = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (alloc, stop) = (alloc.clone(), Arc::clone(&stop));
            std::thread::spawn(move || {
                alloc
                    .alloc(1, Wait::UntilStopped(&stop))
                    .map(|buf| !is_heap(&buf))
            })
        };
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(alloc.release(&[r]), 0);
        assert_eq!(waiter.join().unwrap(), Some(true), "woken by the release");

        // Hold the only slot again: now only the stop flag ends the wait.
        let _held = shared(alloc.alloc(1, Wait::Never).unwrap());
        let waiter = {
            let (alloc, stop) = (alloc.clone(), Arc::clone(&stop));
            std::thread::spawn(move || alloc.alloc(1, Wait::UntilStopped(&stop)).is_none())
        };
        std::thread::sleep(Duration::from_millis(120));
        stop.store(true, Ordering::Relaxed);
        assert!(waiter.join().unwrap(), "a stopped wait gets no buffer");
    }

    #[test]
    fn stale_releases_are_counted() {
        let region = region(4, 1);
        let alloc = attached(&region);
        let r1 = shared(alloc.alloc(1, Wait::Never).unwrap());
        assert_eq!(alloc.release(&[r1]), 0);
        assert_eq!(alloc.release(&[r1]), 1, "double release");
        let r2 = shared(alloc.alloc(1, Wait::Never).unwrap());
        assert_eq!(r2.slot, r1.slot);
        assert_eq!(r2.generation, r1.generation + 1);
        let other_epoch = SlotRef { epoch: 3, ..r2 };
        assert_eq!(alloc.release(&[r1, other_epoch, r2]), 2);
    }

    #[test]
    fn a_newer_region_replaces_the_old_one_while_its_slots_are_out() {
        let old = region(1, 2);
        let mut alloc = attached(&old);
        let published = shared(alloc.alloc(3, Wait::Never).unwrap());
        let mut unsent = alloc.alloc(3, Wait::Never).unwrap();

        let new = region(2, 1);
        alloc.attach(new.desc()).unwrap();
        let r = shared(alloc.alloc(3, Wait::Never).unwrap());
        assert_eq!(r.epoch, 2);

        // The old region's buffer is still writable and publishable; its
        // pool lives as long as the buffer does.
        unsent.push(&[7, 8, 9]);
        let late = shared(unsent);
        assert_eq!(late.epoch, 1);
        assert_eq!(parent_bytes(&old, late), [7, 8, 9]);
        // Releases for the old region are stale; the new one's are not.
        assert_eq!(alloc.release(&[published, late, r]), 2);

        // Regions only grow: an epoch that isn't newer is refused, and the
        // refusal leaves frames inline.
        let again = region(2, 1);
        assert!(alloc.attach(again.desc()).is_err());
        assert!(is_heap(&alloc.alloc(3, Wait::Never).unwrap()));
        assert_eq!(alloc.release(&[r]), 1);
    }

    #[test]
    fn a_failed_attach_leaves_frames_inline() {
        let region = region(1, 1);
        let mut alloc = attached(&region);
        let missing = ShmRegionDesc {
            epoch: 2,
            os_name: vance_ipc::shm::object_name(std::process::id(), 2, 0x5eed_0001),
            slot_count: 1,
            slot_size: SLOT_ALIGN,
        };
        assert!(alloc.attach(&missing).is_err());
        assert!(is_heap(&alloc.alloc(1, Wait::Never).unwrap()));
    }
}
