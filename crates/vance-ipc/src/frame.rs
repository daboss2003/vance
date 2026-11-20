//! Length-prefixed bincode framing.
//!
//! Wire format:
//!
//! ```text
//! +-----------------+-----------------------+
//! | u32 LE length N | bincode payload (N B) |
//! +-----------------+-----------------------+
//! ```
//!
//! Reads and writes are blocking. The transport is just a `Read`/`Write`,
//! so this works on stdin/stdout pipes, sockets, named pipes, anything.
//!
//! Bulk bytes (video frames) don't go through bincode. A message that
//! announces one is followed by a raw blob (ADR 0006):
//!
//! ```text
//! +-----------------+--------------------+
//! | u64 LE length N | N raw bytes        |
//! +-----------------+--------------------+
//! ```

use crate::{Error, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::io::{Read, Write};

/// Hard cap on a single frame's payload size. Protects against corrupt
/// length headers driving multi-GB allocations.
///
/// Pixel data never travels inside a bincode message any more: decoded frames
/// and encoder pushes both go as raw blobs after a small message
/// ([`MAX_BLOB_BYTES`]). Messages are now only metadata, so 48 MiB is pure
/// headroom; it stays as the bound on a corrupt-header allocation.
pub const MAX_FRAME_BYTES: u32 = 48 * 1024 * 1024;

/// Hard cap on a single raw blob. Covers an 8K 16-bit RGBA frame (~265 MB)
/// with room to spare while still bounding a corrupt-header allocation.
pub const MAX_BLOB_BYTES: u64 = 512 * 1024 * 1024;

const BINCODE_CONFIG: bincode::config::Configuration = bincode::config::standard();

/// Write a single message as a length-prefixed bincode frame.
///
/// Does not flush; the caller decides flush cadence. The supervisor flushes
/// after each request to keep latency low.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let bytes = bincode::serde::encode_to_vec(msg, BINCODE_CONFIG)?;
    if bytes.len() as u64 > MAX_FRAME_BYTES as u64 {
        return Err(Error::FrameTooLarge {
            size: bytes.len() as u32,
        });
    }
    let len = bytes.len() as u32;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&bytes)?;
    Ok(())
}

/// Read a single length-prefixed bincode frame and decode it.
///
/// Returns `Err(Error::UnexpectedEof)` if the stream closes between frames or
/// mid-frame; this is the natural way to detect a sidecar that has exited.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> Result<T> {
    let mut len_buf = [0u8; 4];
    read_exact_or_eof(r, &mut len_buf)?;
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(Error::FrameTooLarge { size: len });
    }
    let mut payload = vec![0u8; len as usize];
    read_exact_or_eof(r, &mut payload)?;
    let (val, _consumed) = bincode::serde::decode_from_slice(&payload, BINCODE_CONFIG)?;
    Ok(val)
}

/// Write `bytes` as a raw blob: `[u64 LE length][bytes]`.
///
/// Sent right after the message that announces it. Does not flush.
pub fn write_blob<W: Write>(w: &mut W, bytes: &[u8]) -> Result<()> {
    let len = bytes.len() as u64;
    if len > MAX_BLOB_BYTES {
        return Err(Error::BlobTooLarge);
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(bytes)?;
    Ok(())
}

/// Read one raw blob written by [`write_blob`].
///
/// `expected_len` is the length the preceding message announced. A header
/// that disagrees is [`Error::BlobLengthMismatch`] (the stream is out of
/// sync); one over [`MAX_BLOB_BYTES`] is [`Error::BlobTooLarge`], checked
/// before anything is allocated.
pub fn read_blob<R: Read>(r: &mut R, expected_len: u64) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 8];
    read_exact_or_eof(r, &mut len_buf)?;
    let len = u64::from_le_bytes(len_buf);
    if len > MAX_BLOB_BYTES {
        return Err(Error::BlobTooLarge);
    }
    if len != expected_len {
        return Err(Error::BlobLengthMismatch);
    }
    // Intentional: `take` + `read_to_end` into an exactly sized Vec instead
    // of `vec![0; len]` + `read_exact`. std reads straight into the Vec's
    // uninitialised capacity (pipes, files and BufReader all support it), so
    // a 25 MB frame isn't zero-filled first only to be overwritten.
    let mut bytes = Vec::with_capacity(len as usize);
    r.by_ref().take(len).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != len {
        return Err(Error::UnexpectedEof);
    }
    Ok(bytes)
}

/// Like `Read::read_exact`, but maps `UnexpectedEof` → our `Error::UnexpectedEof`.
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<()> {
    match r.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(Error::UnexpectedEof),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    enum Msg {
        Ping,
        Echo(String),
        Numbers(Vec<i64>),
    }

    #[test]
    fn round_trip_in_memory() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Msg::Ping).unwrap();
        write_frame(&mut buf, &Msg::Echo("hello".into())).unwrap();
        write_frame(&mut buf, &Msg::Numbers(vec![1, 2, 3, 4, 5])).unwrap();

        let mut r = std::io::Cursor::new(buf);
        let m1: Msg = read_frame(&mut r).unwrap();
        let m2: Msg = read_frame(&mut r).unwrap();
        let m3: Msg = read_frame(&mut r).unwrap();
        assert_eq!(m1, Msg::Ping);
        assert_eq!(m2, Msg::Echo("hello".into()));
        assert_eq!(m3, Msg::Numbers(vec![1, 2, 3, 4, 5]));
    }

    #[test]
    fn eof_between_frames_is_unexpected_eof() {
        let buf: Vec<u8> = Vec::new();
        let mut r = std::io::Cursor::new(buf);
        let result: Result<Msg> = read_frame(&mut r);
        assert!(matches!(result, Err(Error::UnexpectedEof)));
    }

    #[test]
    fn truncated_payload_is_unexpected_eof() {
        // Write a length header that promises 100 bytes; supply only 4.
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(&[0u8; 4]);
        let mut r = std::io::Cursor::new(buf);
        let result: Result<Msg> = read_frame(&mut r);
        assert!(matches!(result, Err(Error::UnexpectedEof)));
    }

    #[test]
    fn frame_too_large_rejected_on_read() {
        // Header claims a frame larger than MAX_FRAME_BYTES.
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_BYTES + 1).to_le_bytes());
        let mut r = std::io::Cursor::new(buf);
        let result: Result<Msg> = read_frame(&mut r);
        assert!(matches!(result, Err(Error::FrameTooLarge { .. })));
    }

    #[test]
    fn blob_wire_format_is_u64_le_length_then_raw_bytes() {
        let mut buf = Vec::new();
        write_blob(&mut buf, &[7, 8, 9]).unwrap();
        assert_eq!(buf, [3, 0, 0, 0, 0, 0, 0, 0, 7, 8, 9]);
    }

    #[test]
    fn messages_and_blobs_interleave() {
        // Bigger than any internal read chunk, with every byte value.
        let big: Vec<u8> = (0..3_000_017u32).map(|i| (i * 31 % 251) as u8).collect();
        let mut buf = Vec::new();
        write_frame(&mut buf, &Msg::Echo("frame".into())).unwrap();
        write_blob(&mut buf, &big).unwrap();
        write_blob(&mut buf, &[]).unwrap();
        write_frame(&mut buf, &Msg::Ping).unwrap();

        let mut r = std::io::BufReader::new(std::io::Cursor::new(buf));
        let m: Msg = read_frame(&mut r).unwrap();
        assert_eq!(m, Msg::Echo("frame".into()));
        let blob = read_blob(&mut r, big.len() as u64).unwrap();
        assert!(blob == big, "blob bytes differ");
        assert_eq!(blob.capacity(), big.len(), "blob buffer is exactly sized");
        assert!(read_blob(&mut r, 0).unwrap().is_empty());
        let m: Msg = read_frame(&mut r).unwrap();
        assert_eq!(m, Msg::Ping);
    }

    #[test]
    fn blob_length_must_match_the_announced_length() {
        let mut buf = Vec::new();
        write_blob(&mut buf, &[1, 2, 3, 4]).unwrap();
        let result = read_blob(&mut std::io::Cursor::new(buf), 5);
        assert!(
            matches!(result, Err(Error::BlobLengthMismatch)),
            "{result:?}"
        );
    }

    #[test]
    fn blob_header_over_the_cap_is_rejected_before_allocating() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_BLOB_BYTES + 1).to_le_bytes());
        // Even when the caller expects that length.
        let result = read_blob(&mut std::io::Cursor::new(buf), MAX_BLOB_BYTES + 1);
        assert!(matches!(result, Err(Error::BlobTooLarge)), "{result:?}");
    }

    #[test]
    fn truncated_blob_is_unexpected_eof() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u64.to_le_bytes());
        buf.extend_from_slice(&[0u8; 40]);
        let result = read_blob(&mut std::io::Cursor::new(buf), 100);
        assert!(matches!(result, Err(Error::UnexpectedEof)), "{result:?}");
        // EOF inside the length header too.
        let result = read_blob(&mut std::io::Cursor::new(vec![1u8, 0]), 1);
        assert!(matches!(result, Err(Error::UnexpectedEof)), "{result:?}");
    }
}
