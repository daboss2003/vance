//! Decoded frames as the client hands them out.
//!
//! Not a wire type: the sidecar sends a [`crate::protocol::FrameMeta`] plus
//! the pixels as a raw blob or a shared-memory slot, and the client
//! assembles a [`FrameData`].

use crate::protocol::{ColorMatrixHint, ColorRangeHint, PixelFormat, StreamInfo};
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use vance_ipc::shm::Lease;

/// A frame's pixel bytes: owned, or a zero-copy lease on a shared-memory
/// slot. Reads go through `Deref<Target = [u8]>` either way.
pub struct FrameBytes(Repr);

enum Repr {
    Owned(Vec<u8>),
    Leased(Lease),
    /// Handed out by [`FrameBytes::share`]: one buffer, several owners.
    Shared(Arc<FrameBytes>),
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
            Repr::Shared(inner) => inner,
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
        match &self.0 {
            Repr::Owned(_) => false,
            Repr::Leased(_) => true,
            Repr::Shared(inner) => inner.is_shared(),
        }
    }

    /// The bytes as an owned `Vec`: free for owned bytes, a copy for a lease
    /// (which then ends) or for bytes with other owners.
    pub fn into_vec(self) -> Vec<u8> {
        match self.0 {
            Repr::Owned(bytes) => bytes,
            Repr::Leased(lease) => lease.to_vec(),
            Repr::Shared(inner) => match Arc::try_unwrap(inner) {
                Ok(only) => only.into_vec(),
                Err(inner) => inner.to_vec(),
            },
        }
    }

    /// A second owner of the same bytes, without copying them: this value's
    /// storage becomes shared in place. Unlike [`Clone`] (always a copy), a
    /// share of a lease keeps the shared-memory slot out until EVERY owner is
    /// dropped, so a holder must drop its shares before the decoder can run
    /// out of slots (e.g. drop a frame's shares once it is drawn).
    pub fn share(&mut self) -> FrameBytes {
        if let Repr::Shared(inner) = &self.0 {
            return FrameBytes(Repr::Shared(inner.clone()));
        }
        let own = std::mem::replace(&mut self.0, Repr::Owned(Vec::new()));
        let inner = Arc::new(FrameBytes(own));
        self.0 = Repr::Shared(inner.clone());
        FrameBytes(Repr::Shared(inner))
    }
}

/// How a frame's samples are encoded, as its stream signals (protocol v6):
/// the client copies these from the stream's [`StreamInfo`] onto each frame,
/// so every consumer (incl. a frame cache) sees them with the pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameColor {
    /// YUV sample range (`Unknown` = not signalled).
    pub range: ColorRangeHint,
    /// YUV → RGB matrix (`Unknown` = not signalled).
    pub matrix: ColorMatrixHint,
    /// Sample aspect ratio `(num, den)`; `(1, 1)` = square pixels.
    pub sample_aspect: (u32, u32),
}

impl Default for FrameColor {
    fn default() -> Self {
        Self {
            range: ColorRangeHint::Unknown,
            matrix: ColorMatrixHint::Unknown,
            sample_aspect: (1, 1),
        }
    }
}

impl FrameColor {
    /// The hints `info` carries.
    pub fn from_stream(info: &StreamInfo) -> Self {
        Self {
            range: info.color_range,
            matrix: info.color_matrix,
            sample_aspect: info.sample_aspect,
        }
    }

    /// Width / height of one pixel (1.0 for square or invalid ratios).
    pub fn pixel_aspect(&self) -> f32 {
        match self.sample_aspect {
            (n, d) if n > 0 && d > 0 => n as f32 / d as f32,
            _ => 1.0,
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
    /// Range / matrix / pixel aspect the stream signals.
    pub color: FrameColor,
}

impl FrameData {
    /// A second owner of this frame without copying its pixels (see
    /// [`FrameBytes::share`]).
    pub fn share(&mut self) -> FrameData {
        FrameData {
            pts: self.pts,
            width: self.width,
            height: self.height,
            pixel_format: self.pixel_format,
            strides: self.strides.clone(),
            bytes: self.bytes.share(),
            color: self.color,
        }
    }
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

    /// One region with `slots` slots, its pool and a lease table that
    /// queues ended leases.
    struct Leases {
        pool: std::sync::Arc<vance_ipc::shm::SlotPool>,
        table: std::sync::Arc<vance_ipc::shm::LeaseTable>,
        queue: std::sync::Arc<vance_ipc::shm::ReleaseQueue>,
    }

    impl Leases {
        fn new(slots: u32) -> Self {
            use std::sync::Arc;
            use vance_ipc::shm::{Access, LeaseTable, ReleaseQueue, ShmRegion, SlotPool};
            let mut region =
                ShmRegion::create(1, slots, vance_ipc::shm::SLOT_ALIGN, Access::ReadWrite).unwrap();
            region.unlink_name().unwrap();
            let region = Arc::new(region);
            let queue = Arc::new(ReleaseQueue::default());
            Leases {
                pool: SlotPool::new(Arc::clone(&region)),
                table: LeaseTable::new(region, Arc::clone(&queue), slots as usize),
                queue,
            }
        }

        /// A lease on a fresh slot holding `fill` x 8.
        fn lease(&self, fill: u8) -> (FrameBytes, vance_ipc::shm::SlotRef) {
            use vance_ipc::shm::Adopted;
            let mut w = self.pool.try_acquire().unwrap();
            w.as_mut_slice()[..8].fill(fill);
            let r = w.publish(8);
            match self.table.adopt(r).unwrap() {
                Adopted::Leased(l) => (FrameBytes::from(l), r),
                Adopted::Spilled(_) => panic!("under budget, expected a lease"),
            }
        }
    }

    /// Shares of a lease read the same slot, zero-copy; the slot goes back
    /// only when the last owner drops, whichever that is.
    #[test]
    fn a_shared_lease_is_released_by_its_last_owner() {
        let leases = Leases::new(2);
        let (mut original, r) = leases.lease(5);
        let first = original.share();
        let mut second = original.share();
        let third = second.share();
        for b in [&original, &first, &second, &third] {
            assert!(b.is_shared());
            assert_eq!(&b[..], [5; 8]);
            assert_eq!(b.as_ptr(), original.as_ptr(), "a share is not a copy");
        }
        drop(original);
        drop(second);
        drop(first);
        assert!(leases.queue.drain().is_empty(), "an owner is left");
        drop(third);
        assert_eq!(leases.queue.drain(), [r], "the last owner releases it");

        // A clone of a share is an owned copy: it never holds the lease.
        let (mut b, r) = leases.lease(6);
        let shared = b.share();
        let copy = shared.clone();
        assert!(!copy.is_shared());
        drop((b, shared));
        assert_eq!(leases.queue.drain(), [r]);
        assert_eq!(&copy[..], [6; 8]);
    }

    /// `into_vec` on a shared lease copies the bytes; the other owners keep
    /// the lease until they go. The last owner's `into_vec` ends it.
    #[test]
    fn into_vec_on_a_shared_lease_copies_and_leaves_the_others() {
        let leases = Leases::new(2);
        let (mut b, r) = leases.lease(7);
        let other = b.share();
        assert_eq!(b.into_vec(), [7; 8]);
        assert!(leases.queue.drain().is_empty(), "`other` still holds it");
        assert_eq!(&other[..], [7; 8]);
        assert_eq!(other.into_vec(), [7; 8]);
        assert_eq!(leases.queue.drain(), [r]);
    }

    /// Shares of owned bytes are one buffer: not leased, the same address,
    /// and the last owner's `into_vec` gets the original `Vec` back.
    #[test]
    fn owned_shares_are_one_buffer() {
        let bytes = vec![3u8; 64];
        let address = bytes.as_ptr();
        let mut b = FrameBytes::from(bytes);
        let share = b.share();
        assert!(!b.is_shared() && !share.is_shared());
        assert_eq!(share.as_ptr(), address);
        assert_eq!(
            format!("{share:?}"),
            r#"FrameBytes { kind: "owned", len: 64 }"#
        );
        drop(b);
        let back = share.into_vec();
        assert_eq!(back.as_ptr(), address, "the only owner's into_vec moves");
        assert_eq!(back, [3; 64]);
    }

    /// `FrameData::share` keeps every field and shares the pixels; a clone
    /// of it is an owned copy.
    #[test]
    fn frame_data_shares_keep_every_field() {
        let leases = Leases::new(2);
        let (bytes, r) = leases.lease(9);
        let mut frame = FrameData {
            pts: 42,
            width: 2,
            height: 1,
            pixel_format: PixelFormat::Rgba8,
            strides: vec![8],
            bytes,
            color: FrameColor {
                range: ColorRangeHint::Full,
                matrix: ColorMatrixHint::Unknown,
                sample_aspect: (4, 3),
            },
        };
        let share = frame.share();
        assert_eq!(
            (share.pts, share.width, share.height, share.pixel_format),
            (42, 2, 1, PixelFormat::Rgba8)
        );
        assert_eq!((&share.strides, share.color), (&frame.strides, frame.color));
        assert_eq!(share.bytes.as_ptr(), frame.bytes.as_ptr());
        assert!(share.bytes.is_shared());
        let copy = share.clone();
        assert!(!copy.bytes.is_shared());
        drop(frame);
        assert!(leases.queue.drain().is_empty());
        drop(share);
        assert_eq!(leases.queue.drain(), [r]);
        assert_eq!(&copy.bytes[..], [9; 8]);
    }
}
