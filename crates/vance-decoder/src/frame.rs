//! Decoded frames as the client hands them out.
//!
//! Not a wire type: the sidecar sends a [`crate::protocol::FrameMeta`] plus
//! the pixels as a raw blob or a shared-memory slot, and the client
//! assembles a [`FrameData`].

use crate::protocol::PixelFormat;
use std::fmt;
use std::ops::Deref;
use vance_ipc::shm::Lease;

/// A frame's pixel bytes: owned, or a zero-copy lease on a shared-memory
/// slot. Reads go through `Deref<Target = [u8]>` either way.
pub struct FrameBytes(Repr);

enum Repr {
    Owned(Vec<u8>),
    Leased(Lease),
}

impl From<Vec<u8>> for FrameBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(Repr::Owned(bytes))
    }
}

impl From<Lease> for FrameBytes {
    fn from(lease: Lease) -> Self {
        Self(Repr::Leased(lease))
    }
}

impl Deref for FrameBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match &self.0 {
            Repr::Owned(bytes) => bytes,
            Repr::Leased(lease) => lease,
        }
    }
}

impl Clone for FrameBytes {
    /// Always an owned copy: a clone never extends a lease, so holding
    /// clones can't starve the sidecar of slots.
    fn clone(&self) -> Self {
        Self(Repr::Owned(self.to_vec()))
    }
}

impl PartialEq for FrameBytes {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl fmt::Debug for FrameBytes {
    /// Kind and length only: a 4K frame is 25 MB.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = if self.is_shared() { "leased" } else { "owned" };
        f.debug_struct("FrameBytes")
            .field("kind", &kind)
            .field("len", &self.len())
            .finish()
    }
}

impl FrameBytes {
    /// Whether these bytes are a lease on shared memory (not an owned copy).
    pub fn is_shared(&self) -> bool {
        matches!(self.0, Repr::Leased(_))
    }

    /// The bytes as an owned `Vec`: free for owned bytes, a copy for a lease
    /// (which then ends).
    pub fn into_vec(self) -> Vec<u8> {
        match self.0 {
            Repr::Owned(bytes) => bytes,
            Repr::Leased(lease) => lease.to_vec(),
        }
    }
}

/// One decoded frame.
#[derive(Debug, Clone)]
pub struct FrameData {
    /// PTS in the stream's time base.
    pub pts: i64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel format (matches the open's `StreamInfo.pixel_format`).
    pub pixel_format: PixelFormat,
    /// Per-plane row lengths in samples (NV12: [Y, UV]; YUV420P: [Y, U, V]).
    pub strides: Vec<u32>,
    /// Raw pixel bytes, planes packed back to back (Y first).
    pub bytes: FrameBytes,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_bytes_read_through_deref() {
        let b = FrameBytes::from(vec![1u8, 2, 3, 4]);
        assert_eq!(b.len(), 4);
        assert_eq!(&b[1..3], [2, 3]);
        assert!(!b.is_shared());
        assert_eq!(b.into_vec(), [1, 2, 3, 4]);
    }

    #[test]
    fn clones_are_owned_and_equal() {
        let b = FrameBytes::from(vec![9u8; 16]);
        let c = b.clone();
        assert!(!c.is_shared());
        assert_eq!(b, c);
        assert_ne!(b, FrameBytes::from(vec![9u8; 15]));
    }

    #[test]
    fn debug_shows_kind_and_length_not_bytes() {
        let b = FrameBytes::from(vec![0xABu8; 1000]);
        assert_eq!(
            format!("{b:?}"),
            r#"FrameBytes { kind: "owned", len: 1000 }"#
        );
    }

    /// Over a real region: a clone of leased bytes is an owned copy, and the
    /// lease ends (its slot is queued for release) when the original goes,
    /// however long the clone lives. `into_vec` copies and ends the lease.
    #[test]
    fn leased_bytes_clone_to_owned_and_release_on_drop() {
        use std::sync::Arc;
        use vance_ipc::shm::{Access, Adopted, LeaseTable, ReleaseQueue, ShmRegion, SlotPool};

        let mut region =
            ShmRegion::create(1, 2, vance_ipc::shm::SLOT_ALIGN, Access::ReadWrite).unwrap();
        region.unlink_name().unwrap();
        let region = Arc::new(region);
        let pool = SlotPool::new(Arc::clone(&region));
        let queue = Arc::new(ReleaseQueue::default());
        let table = LeaseTable::new(region, Arc::clone(&queue), 2);
        let lease = |fill: u8| {
            let mut w = pool.try_acquire().unwrap();
            w.as_mut_slice()[..8].fill(fill);
            let r = w.publish(8);
            match table.adopt(r).unwrap() {
                Adopted::Leased(l) => (FrameBytes::from(l), r),
                Adopted::Spilled(_) => panic!("under budget, expected a lease"),
            }
        };

        let (b, r) = lease(7);
        assert!(b.is_shared());
        assert_eq!(&b[..], [7; 8]);
        assert_eq!(format!("{b:?}"), r#"FrameBytes { kind: "leased", len: 8 }"#);
        let c = b.clone();
        assert!(!c.is_shared());
        assert_eq!(b, c);
        drop(b);
        assert_eq!(queue.drain(), [r], "the clone must not keep the lease");
        assert_eq!(&c[..], [7; 8]);

        let (b, r) = lease(9);
        assert_eq!(b.into_vec(), [9; 8]);
        assert_eq!(queue.drain(), [r]);
    }
}
