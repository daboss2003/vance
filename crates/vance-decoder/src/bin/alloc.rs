//! Where packed frame bytes go.
//!
//! `pack` asks the [`Allocator`] for a buffer of exactly a frame's size and
//! appends the frame's rows to it; `main` turns the filled buffer into the
//! reply's [`FramePayload`] when it answers the request. Every buffer is a
//! heap `Vec` today, sent inline as a raw blob after the reply. Shared-memory
//! slots (ADR 0006) are the other kind of buffer planned here: a slot
//! travels unpublished through the stream channel and is published only in
//! `into_payload`, so dropping an unsent buffer gives its slot back.

use std::sync::atomic::{AtomicBool, Ordering};
use vance_decoder::protocol::{FramePayload, ShmRegionDesc, SlotRef};

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

/// Hands out frame buffers. `Default` is heap-only. Cheap to clone: the
/// stream decode thread gets its own clone.
#[derive(Clone, Default)]
pub struct Allocator {}

impl Allocator {
    /// A buffer for exactly `len` bytes. `None` only when `wait` is
    /// `UntilStopped` and the stop flag is set.
    pub fn alloc(&self, len: usize, wait: Wait<'_>) -> Option<FrameBuf> {
        if let Wait::UntilStopped(stop) = wait {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
        }
        Some(FrameBuf::Heap(Vec::with_capacity(len)))
    }

    /// Back future buffers with the parent's shared-memory region. Not
    /// supported by this sidecar yet: the caller replies `ShmUnavailable`
    /// and frames stay inline.
    pub fn attach(&mut self, desc: &ShmRegionDesc) -> vance_ipc::Result<()> {
        let _ = desc;
        Err(vance_ipc::Error::ShmUnsupported)
    }

    /// Take back slots the parent is done with; returns how many were stale.
    /// With no region attached, every slot is.
    pub fn release(&self, slots: &[SlotRef]) -> u32 {
        u32::try_from(slots.len()).unwrap_or(u32::MAX)
    }
}

/// A frame buffer: filled by `pack`, sent by `main`.
pub enum FrameBuf {
    /// Heap bytes, sent as an inline blob after the reply.
    Heap(Vec<u8>),
}

impl FrameBuf {
    /// Append `bytes` (one row of a plane).
    pub fn push(&mut self, bytes: &[u8]) {
        match self {
            FrameBuf::Heap(v) => v.extend_from_slice(bytes),
        }
    }

    /// The reply payload, plus the blob that must follow the reply for an
    /// inline payload.
    pub fn into_payload(self) -> (FramePayload, Option<Vec<u8>>) {
        match self {
            FrameBuf::Heap(v) => (
                FramePayload::Inline {
                    len: v.len() as u64,
                },
                Some(v),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            slot_size: vance_ipc::shm::SLOT_ALIGN,
        };
        assert!(alloc.attach(&desc).is_err());
        let slot = SlotRef {
            epoch: 1,
            slot: 0,
            generation: 1,
            len: 1,
        };
        assert_eq!(alloc.release(&[slot, slot]), 2);
    }
}
