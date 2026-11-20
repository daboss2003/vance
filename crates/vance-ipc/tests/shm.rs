//! Shared-memory regions over the real OS layer (ADR 0006), including a
//! second process (`examples/shm_peer.rs`).
//!
//! Intentional: every test here holds `serial()`. `live_region_count` is
//! process-wide, so a test that measures it must not overlap another test's
//! regions; serializing them all keeps that true as tests are added.

use std::io::{BufRead, BufReader};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use vance_ipc::shm::{
    live_region_count, shm_space_available, Access, Adopted, Lease, LeaseTable, ReleaseQueue,
    ShmRegion, ShmRegionDesc, SlotPool, SlotRef, SLOT_ALIGN,
};
use vance_ipc::Error;

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// What `shm-peer` writes for `seed` (mirrors the example).
fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u64 * 31 + u64::from(seed)) % 251) as u8)
        .collect()
}

fn leased(table: &Arc<LeaseTable>, r: SlotRef) -> Lease {
    match table.adopt(r).unwrap() {
        Adopted::Leased(l) => l,
        Adopted::Spilled(_) => panic!("expected a lease for {r:?}"),
    }
}

/// The decoder layout: the parent creates read-only, the child opens
/// read-write, and the parent then unlinks the name.
fn attached_pair(epoch: u32, slots: u32, slot_size: u64) -> (ShmRegion, ShmRegion) {
    let mut parent = ShmRegion::create(epoch, slots, slot_size, Access::ReadOnly).unwrap();
    let child = ShmRegion::open(parent.desc(), Access::ReadWrite).unwrap();
    parent.unlink_name().unwrap();
    (parent, child)
}

#[test]
fn two_mappings_of_one_region_see_each_others_writes() {
    let _serial = serial();
    let (parent, child) = attached_pair(1, 2, SLOT_ALIGN);
    let pool = SlotPool::new(Arc::new(child));
    let table = LeaseTable::new(Arc::new(parent), Arc::default(), 2);
    let mut w = pool.try_acquire().unwrap();
    w.as_mut_slice().fill(0x5A);
    let r = w.publish(SLOT_ALIGN as usize);
    assert!(leased(&table, r).iter().all(|&b| b == 0x5A));

    // The other way round: the creator writes, the opener reads.
    let mut writer = ShmRegion::create(2, 1, SLOT_ALIGN, Access::ReadWrite).unwrap();
    let reader = ShmRegion::open(writer.desc(), Access::ReadOnly).unwrap();
    writer.unlink_name().unwrap();
    let pool = SlotPool::new(Arc::new(writer));
    let table = LeaseTable::new(Arc::new(reader), Arc::default(), 1);
    let mut w = pool.try_acquire().unwrap();
    w.as_mut_slice()[..3].copy_from_slice(b"abc");
    let r = w.publish(3);
    assert_eq!(&leased(&table, r)[..], b"abc");
}

/// Read-only mappings are PROT_READ / FILE_MAP_READ (pinned in the platform
/// modules' tests); here the API refuses to hand out a writable slot of one.
#[test]
fn a_read_only_region_cannot_back_a_slot_pool() {
    let _serial = serial();
    let (parent, _child) = attached_pair(3, 1, SLOT_ALIGN);
    let parent = Arc::new(parent);
    let r = std::panic::catch_unwind(AssertUnwindSafe(|| SlotPool::new(Arc::clone(&parent))));
    assert!(
        r.is_err(),
        "a read-only mapping must not hand out WriteSlots"
    );
}

#[test]
fn real_names_are_short_and_prefixed() {
    let _serial = serial();
    let region = ShmRegion::create(u32::MAX, 1, SLOT_ALIGN, Access::ReadOnly).unwrap();
    let name = &region.desc().os_name;
    if cfg!(windows) {
        assert!(name.starts_with("Local\\vance"), "{name}");
    } else {
        assert!(name.starts_with("/vance"), "{name}");
        assert!(name.len() <= 31, "{name} is past macOS's 31-byte limit");
    }
    assert_eq!(region.byte_len(), SLOT_ALIGN);
}

#[test]
fn layouts_are_validated_and_open_checks_the_object_size() {
    let _serial = serial();
    for (count, size) in [(0, SLOT_ALIGN), (1, SLOT_ALIGN + 1), (1, 0)] {
        let r = ShmRegion::create(4, count, size, Access::ReadOnly);
        assert!(matches!(r, Err(Error::Shm(_))), "{count} x {size}: {r:?}");
    }

    let mut region = ShmRegion::create(4, 3, 2 * SLOT_ALIGN, Access::ReadOnly).unwrap();
    let good = region.desc().clone();
    for bad in [
        ShmRegionDesc {
            slot_count: 2,
            ..good.clone()
        },
        ShmRegionDesc {
            slot_count: 4,
            ..good.clone()
        },
        ShmRegionDesc {
            slot_size: SLOT_ALIGN,
            ..good.clone()
        },
        ShmRegionDesc {
            slot_size: 3 * SLOT_ALIGN,
            ..good.clone()
        },
        ShmRegionDesc {
            slot_size: 2 * SLOT_ALIGN + 1,
            ..good.clone()
        },
        ShmRegionDesc {
            slot_count: 0,
            ..good.clone()
        },
    ] {
        let r = ShmRegion::open(&bad, Access::ReadWrite);
        assert!(matches!(r, Err(Error::Shm(_))), "{bad:?}: {r:?}");
    }
    assert!(ShmRegion::open(&good, Access::ReadWrite).is_ok());
    region.unlink_name().unwrap();
}

#[test]
fn unlinking_is_idempotent_and_ends_attaching() {
    let _serial = serial();
    let mut region = ShmRegion::create(5, 1, SLOT_ALIGN, Access::ReadOnly).unwrap();
    let desc = region.desc().clone();
    region.unlink_name().unwrap();
    region.unlink_name().unwrap();
    let r = ShmRegion::open(&desc, Access::ReadWrite);
    if cfg!(windows) {
        // A section has no name to remove; it dies with its last handle.
        assert!(r.is_ok(), "{r:?}");
    } else {
        assert!(matches!(r, Err(Error::Shm(_))), "{r:?}");
    }
}

#[test]
fn dropping_the_creator_removes_the_name_but_dropping_an_opener_does_not() {
    let _serial = serial();
    let region = ShmRegion::create(6, 1, SLOT_ALIGN, Access::ReadOnly).unwrap();
    let desc = region.desc().clone();
    drop(ShmRegion::open(&desc, Access::ReadWrite).unwrap());
    assert!(ShmRegion::open(&desc, Access::ReadWrite).is_ok());
    drop(region);
    let r = ShmRegion::open(&desc, Access::ReadWrite);
    assert!(matches!(r, Err(Error::Shm(_))), "{r:?}");
}

#[test]
fn live_region_count_tracks_mappings() {
    let _serial = serial();
    let start = live_region_count();
    let (parent, child) = attached_pair(7, 2, SLOT_ALIGN);
    assert_eq!(live_region_count(), start + 2);
    let wrong = ShmRegionDesc {
        slot_count: 3,
        ..parent.desc().clone()
    };
    assert!(ShmRegion::open(&wrong, Access::ReadOnly).is_err());
    assert!(ShmRegion::create(7, 0, SLOT_ALIGN, Access::ReadOnly).is_err());
    assert_eq!(live_region_count(), start + 2, "failures count nothing");

    let pool = SlotPool::new(Arc::new(child));
    let table = LeaseTable::new(Arc::new(parent), Arc::default(), 1);
    let lease = leased(&table, pool.try_acquire().unwrap().publish(1));
    drop(pool);
    drop(table);
    assert_eq!(
        live_region_count(),
        start + 1,
        "the lease keeps the parent's region mapped"
    );
    drop(lease);
    assert_eq!(live_region_count(), start);
}

#[test]
fn pool_and_leases_round_trip_over_a_real_region() {
    let _serial = serial();
    let (parent, child) = attached_pair(8, 3, SLOT_ALIGN);
    let pool = SlotPool::new(Arc::new(child));
    let queue = Arc::new(ReleaseQueue::default());
    let table = LeaseTable::new(Arc::new(parent), Arc::clone(&queue), 1);

    // Fill the last slot completely: its end is the end of the mapping.
    let w0 = pool.try_acquire().unwrap();
    let w1 = pool.try_acquire().unwrap();
    let mut w2 = pool.try_acquire().unwrap();
    let full = pattern(9, SLOT_ALIGN as usize);
    w2.as_mut_slice().copy_from_slice(&full);
    let r = w2.publish(full.len());
    assert_eq!(r.slot, 2);
    drop((w0, w1));
    let lease = leased(&table, r);
    assert_eq!(&lease[..], &full[..]);

    // Over the budget of one: the next frame is copied out and its slot
    // queued for release at once.
    let mut w = pool.try_acquire().unwrap();
    w.as_mut_slice()[..2].copy_from_slice(b"hi");
    let spilled = w.publish(2);
    match table.adopt(spilled).unwrap() {
        Adopted::Spilled(bytes) => assert_eq!(bytes, b"hi"),
        other => panic!("expected a spill, got {other:?}"),
    }

    drop(lease);
    let released = queue.drain();
    assert_eq!(released, [spilled, r]);
    for x in released {
        pool.release(x).unwrap();
    }
    assert!(pool.release(r).is_err(), "double release");
    let again = pool.try_acquire().unwrap().publish(0);
    assert_eq!(again.slot, r.slot, "last released, first reused");
    assert_eq!(again.generation, r.generation.wrapping_add(1));
    assert!(pool.release(r).is_err(), "the earlier use is stale");
    pool.release(again).unwrap();
}

/// `target/<profile>/examples/shm-peer`, next to this test's `deps/` dir.
/// `cargo test` builds examples; `cargo xtask test` builds them first too.
fn peer_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // deps/
    p.pop(); // <profile>/
    p.push("examples");
    p.push(if cfg!(windows) {
        "shm-peer.exe"
    } else {
        "shm-peer"
    });
    assert!(
        p.exists(),
        "shm-peer example not built at {}; run `cargo build --example shm-peer -p vance-ipc` first",
        p.display()
    );
    p
}

fn parse_published(line: &str) -> SlotRef {
    let fields: Vec<&str> = line.split_whitespace().collect();
    let ["published", epoch, slot, generation, len] = fields.as_slice() else {
        panic!("unexpected peer output {line:?}");
    };
    SlotRef {
        epoch: epoch.parse().unwrap(),
        slot: slot.parse().unwrap(),
        generation: generation.parse().unwrap(),
        len: len.parse().unwrap(),
    }
}

#[test]
fn a_peer_process_writes_a_slot_and_the_lease_outlives_it() {
    let _serial = serial();
    let mut region = ShmRegion::create(9, 3, 2 * SLOT_ALIGN, Access::ReadOnly).unwrap();
    let desc = region.desc().clone();
    // Not page-aligned, and past the first page of the slot.
    let (slot, len, seed) = (1u32, SLOT_ALIGN as usize + 123, 42u8);
    let mut peer = vance_ipc::process::spawn(
        Command::new(peer_path())
            .args([
                desc.os_name.clone(),
                desc.epoch.to_string(),
                desc.slot_count.to_string(),
                desc.slot_size.to_string(),
                slot.to_string(),
                len.to_string(),
                seed.to_string(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped()),
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(peer.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    // The child has attached (or failed to): unlink, as the client does.
    region.unlink_name().unwrap();
    let r = parse_published(&line);
    assert_eq!((r.epoch, r.slot, r.len), (desc.epoch, slot, len as u64));

    let table = LeaseTable::new(Arc::new(region), Arc::default(), 2);
    let lease = leased(&table, r);
    let expected = pattern(seed, len);
    assert_eq!(&lease[..], &expected[..]);

    peer.kill().unwrap();
    peer.wait().unwrap();
    assert_eq!(
        &lease[..],
        &expected[..],
        "a lease stays readable after its producer died"
    );
}

#[test]
fn space_check_accepts_small_regions() {
    let _serial = serial();
    assert!(shm_space_available(0));
    assert!(shm_space_available(SLOT_ALIGN));
    if cfg!(target_os = "linux") {
        assert!(!shm_space_available(u64::MAX));
    }
}
