# ADR 0006 — Shared-Memory Frame Transport

- **Status:** Proposed
- **Date:** 2026-11-20
- **Deciders:** Vance core team
- **Supersedes:** — (amends ADR 0004 rule 5; implements its rule 3)

## Context

Decoded frames went from the decoder sidecar to the app inline in a bincode message
(`FrameData { bytes: Vec<u8> }`). A 4K 10-bit 4:2:0 frame is ~25 MB. On an Apple M1 the hop
cost ~77 ms per frame: `bytes` was serialized byte by byte (no `serde_bytes`), the buffers were
reallocated and zero-filled, and each frame was copied about six times. Streaming 4K ProRes 422
HQ ran at 10–11 fps (88–98 ms/frame, `prores_4k_decode_throughput`). FFmpeg decode itself was
single-threaded (~35 ms for a 4K ProRes frame; ~19 ms with FFmpeg's automatic threading).

The opposite direction has the same root problem. 4K HDR export fails: an `rgba64le` 3840×2160
frame is 66.4 MB, larger than vance-ipc's `MAX_FRAME_BYTES` (48 MiB). Only a 320×240 HDR test
existed, so nothing caught it.

ADR 0004 already says per-frame data belongs in shared-memory rings and the control plane in
bincode. This ADR fixes how.

## Decision

**Pixels never go through bincode. They travel in shared-memory slots the parent owns, with a
raw blob after the message as the fallback everywhere.**

1. **Raw blobs (the fallback, all platforms).** A message that announces bulk bytes is followed
   on the same stream by `[u64 LE length][bytes]` (`vance_ipc::{write_blob, read_blob}`,
   `MAX_BLOB_BYTES` = 512 MiB). The reader allocates exactly once and reads straight into the
   buffer: no per-byte serde, no zero-fill. Parent side: `Sidecar::{send, recv, recv_blob,
   send_blob}` (`request` = `send` + `recv`). Child side: `serve_with_blobs`, whose handler reads
   request blobs through a `BlobReader` and returns a `Reply { msg, blob }`. `serve` is unchanged
   for sidecars that never move bulk data. Blobs alone took 4K ProRes streaming from ~95 to
   ~42 ms/frame on the M1.
2. **Shared memory.** POSIX `shm_open` + `mmap` on macOS and Linux; a named, pagefile-backed
   section (`CreateFileMappingW` / `MapViewOfFile`) on Windows. Both through the existing `libc`
   and `windows-sys` 0.59 (features `Win32_System_Memory`, `Win32_Foundation`). No new crates.
   The API is `vance_ipc::shm`.
3. **Region layout.** One region per decoder client: `slot_count` slots of `slot_size` bytes,
   where `slot_size` is the frame size rounded up to `SLOT_ALIGN` (64 KiB: at least the 16 KiB
   Apple Silicon page, equal to the Windows allocation granularity) and
   `slot_count = ring_slots + max_leases + 1` (default 4 + 2 + 1 = 7, ~175 MB for a 4K 10-bit
   layer).
4. **Ownership.** The parent creates every region, maps decoder regions read-only, and unlinks
   the OS name as soon as the child has attached (or failed to). The child maps read-write. No
   control data lives in shared memory: every ownership transfer is a pipe message, so the pipe
   orders them. `Frame { meta, payload: Shared(SlotRef) }` hands a slot to the parent;
   `Release { slots }` hands slots back (drained from a `ReleaseQueue` before each request).
   `SlotRef`s carry an epoch and a per-slot generation, so late or duplicate releases are
   rejected.
5. **Epochs.** Regions only grow. A bigger frame size means a new region with a higher epoch;
   releases for an older epoch are dropped.
6. **Leases and spill.** The client holds at most `max_leases` frames as zero-copy `Lease`s. A
   frame past that budget is copied out ("spilled") and its slot released at once, so a slow
   consumer can never deadlock the producer. `FrameBytes::clone()` always makes an owned copy: a
   clone never extends a lease.
7. **Sidecar threading.** The sidecar main thread only `try_acquire`s slots, because it is also
   the thread that receives `Release`; blocking there would deadlock. The decode thread uses
   `acquire_timeout(50 ms)` in a loop that also checks its stop flag. Unpublished slots travel
   through the stream channel and are published only when the main thread replies, so draining
   the channel on stop returns them to the pool.
8. **Fallback triggers.** Inline blobs are used when shm is disabled (`VANCE_IPC_SHM=0`), when
   creating or attaching a region fails, when Linux `/dev/shm` lacks the free space (`statvfs`
   check before creating), and for any frame larger than its slot.
9. **Naming.** Every OS object name comes from one function, `shm::object_name(pid, epoch,
   nonce)`: `/vance` plus three fixed-width hex fields, 30 bytes, within macOS's 31-byte POSIX
   limit. A random nonce keeps a recycled pid from colliding with a crashed process's leftover.
10. **Protocol.** The decoder protocol moves to v3: `Frame { meta: FrameMeta, payload:
    FramePayload }` with `Inline { len }` (exactly one blob follows) or `Shared(SlotRef)`, plus
    `AttachShm` / `ShmAttached` / `ShmUnavailable` and `Release` / `Released { stale }`.
    `FrameData` is no longer a wire type; its `bytes` is a `FrameBytes` (owned or leased, read
    through `Deref<Target = [u8]>`).
11. **Frame-threaded decode** ships in the same milestone. The thread count is
    `VANCE_DECODE_THREADS`, else `available_parallelism().min(8)`, software decode only.
    One-shot decodes (`Seek` / `DecodeAt`) use slice threads only, so "first frame at or after
    the target" keeps working with no output delay. Streams use a second decoder with frame and
    slice threading.

### User decisions

- **Mechanism:** POSIX shm + Windows named sections on the existing `libc` / `windows-sys`
  dependencies, with raw-blob fallback everywhere.
- **Windows:** shared memory is on by default there too. Only CI verifies it, and any shm error
  falls back to blobs automatically.
- **Naming:** one naming function, so a macOS App Sandbox app-group prefix is a one-line change
  later.
- **Memory budget:** the engine will cap total shm frame memory at ~25% of RAM and switch layers
  past the cap to proxies. That is enforced in the compositing step; this ADR only provides the
  inputs: `ClientOptions` sizing, `TransportStats`, `ShmRegion::byte_len`.
- **Encoder direction:** app → encoder frames get blob framing in this milestone, which fixes 4K
  HDR export. The failure is reproduced first with a real 4K HDR export test of a few frames.

### ADR 0004, revisited

- **Rule 3** (shared-memory rings for per-frame data): implemented here. The control plane stays
  bincode over the sidecar's stdin/stdout pipes; only pixels move to shared memory.
- **Rule 2** (crash isolation): unchanged. The parent's mapping outlives a crashed sidecar, so
  leased frames stay readable, and the next request reports the crash as before.
- **Rule 5** (decoder sidecars: no filesystem writes) is **amended**. A decoder may open (never
  create) the one shared-memory object its parent created for it, and map it read-write. On
  macOS and Windows that object is a kernel object, not a file. On Linux it is a file in the
  `/dev/shm` tmpfs, so a future decoder sandbox must allow opening `/dev/shm/vance*`
  read-write (and, on macOS, `ipc-posix-shm-read-data` / `ipc-posix-shm-write-data` for the
  `/vance` prefix). That is the only write it gains.

## Consequences

**Easier:**
- 4K playback gets frames without copying them through the pipe. The fallback alone already
  doubles throughput, and every platform keeps a working path when shared memory isn't
  available.
- Bulk data of any size (4K HDR export frames, future 8K) fits through one mechanism, without
  raising `MAX_FRAME_BYTES`.

**Harder:**
- The decoder protocol is v3: app and sidecar must ship together (they already do).
- Slot ownership is a protocol invariant both sides must keep. The pool and lease bookkeeping
  is generation-checked and covered by tests, but a bug there corrupts frames rather than
  failing loudly.

**Risks:**
- The parent hands out `&[u8]` over memory the sidecar can write. That is sound only because
  the sidecar is first-party and writes only the slots it owns. Lengths come from the pipe and
  are bounds-checked, and the renderer re-checks frame sizes, so a misbehaving decoder can
  corrupt pixels but can't make the app read out of bounds.
- Linux tmpfs: `ftruncate` past the tmpfs limit succeeds, and touching those pages later raises
  `SIGBUS`. Only the writer (the sidecar) touches the pages, so a crash stays isolated, and the
  `statvfs` check avoids it. If the app ever writes a region (the encoder direction), it must
  `posix_fallocate` first. This milestone's encoder direction uses blobs, not shm.
- Windows commits the whole section up front, so memory accounting there sees the full region
  size at once.
- A crash between creating a region and unlinking it leaves a stale name (a `/dev/shm` file on
  Linux). The window is one round trip, and the nonce prevents collisions.
- A macOS App Sandbox app-group prefix (team ID + group name + `/`) uses up most of the 31-byte
  limit. Adopting it will also mean shortening the unique part of the name.

**Follow-up:**
- The engine's memory budget and automatic proxies (compositing step).
- IOSurface-backed slots on macOS could later feed GPU upload directly. That would be a new
  ADR.

## Alternatives Considered

- **Keep bincode, add `serde_bytes`:** removes the per-byte serde but still copies each frame
  through bincode's buffer. The raw blob is simpler and strictly cheaper.
- **Blobs only, no shared memory:** measured at ~42 ms/frame for one 4K ProRes stream. That is
  at the edge of 24 fps, and it still makes two kernel copies per frame, which doesn't scale to
  several 4K layers.
- **A crate (`shared_memory`, `ipc-channel`, `iceoryx2`, `memmap2` over temp files):** each is a
  new dependency for what `libc` / `windows-sys` already give us. Temp files would also hit the
  disk and break the decoder's no-filesystem-write rule.
- **Anonymous memory passed by handle (`memfd_create` + `SCM_RIGHTS`, `DuplicateHandle`):**
  needs a different mechanism on each OS (`memfd` doesn't exist on macOS) and a socket next to
  the stdio pipes. Named objects work the same way everywhere.
- **Control data in shared memory (a lock-free ring):** saves one message per frame, but brings
  cross-process atomics, ordering and crash recovery into shared state. The pipe already orders
  messages, and we send one per frame anyway.
- **Sidecar-created regions:** the app would map memory named and sized by a process that parses
  untrusted media. Parent-owned regions keep creation, naming, unlinking and the memory budget in
  the app.

## Notes

- Code: `crates/vance-ipc/src/{frame.rs, sidecar.rs, shm.rs}`, `crates/vance-decoder/src/{protocol.rs, frame.rs, client.rs}`,
  the sidecar in `crates/vance-decoder/src/bin/` (`alloc.rs` is where slots replace heap buffers).
- Benchmark: `cargo test -p vance-decoder --test it -- --ignored --nocapture prores_4k`.
  Target: ≥ 24 fps (≤ 41.7 ms/frame) on an M1 dev build with shared memory and threaded decode.
- Related: ADR 0004 (process model).
