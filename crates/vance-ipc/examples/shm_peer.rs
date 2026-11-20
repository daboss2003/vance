//! Child process for `vance-ipc`'s cross-process shared-memory tests. It
//! plays the sidecar side of a region.
//!
//! ```text
//! shm-peer <os_name> <epoch> <slot_count> <slot_size> <slot> <len> <seed>
//! ```
//!
//! Opens the region read-write, writes `len` bytes of [`pattern`] for `seed`
//! into slot `slot`, publishes it, and prints the `SlotRef` on stdout as
//! `published <epoch> <slot> <generation> <len>`. Then it waits until stdin
//! closes (or it is killed), so the parent controls its lifetime.

use std::io::{Read, Write};
use std::sync::Arc;
use vance_ipc::shm::{Access, ShmRegion, ShmRegionDesc, SlotPool};

/// Byte `i` of the pattern for `seed`. The tests compute the same thing.
fn pattern(seed: u8, i: usize) -> u8 {
    ((i as u64 * 31 + u64::from(seed)) % 251) as u8
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [os_name, epoch, slot_count, slot_size, slot, len, seed] = args.as_slice() else {
        return Err(
            "usage: shm-peer <os_name> <epoch> <slot_count> <slot_size> <slot> <len> <seed>".into(),
        );
    };
    let desc = ShmRegionDesc {
        epoch: epoch.parse()?,
        os_name: os_name.clone(),
        slot_count: slot_count.parse()?,
        slot_size: slot_size.parse()?,
    };
    let (slot, len, seed): (u32, usize, u8) = (slot.parse()?, len.parse()?, seed.parse()?);

    let pool = SlotPool::new(Arc::new(ShmRegion::open(&desc, Access::ReadWrite)?));
    // A fresh pool hands out slots 0, 1, 2, … in order, so the target slot
    // is acquire number `slot + 1`. The others go back unpublished on exit.
    let mut held = Vec::new();
    for _ in 0..=slot {
        held.push(pool.try_acquire().ok_or("slot index past the region")?);
    }
    let mut target = held.pop().expect("acquired at least one slot");
    let bytes = target
        .as_mut_slice()
        .get_mut(..len)
        .ok_or("len past the slot")?;
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = pattern(seed, i);
    }
    let r = target.publish(len);
    assert_eq!(r.slot, slot, "slots weren't handed out in order");

    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "published {} {} {} {}",
        r.epoch, r.slot, r.generation, r.len
    )?;
    out.flush()?;
    drop(out);

    std::io::stdin().read_to_end(&mut Vec::new())?;
    Ok(())
}
