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

use crate::{Error, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::io::{Read, Write};

/// Hard cap on a single frame's payload size. Protects against corrupt
/// length headers driving multi-GB allocations.
///
/// Decoded video frames do **not** flow through this channel — they go via
/// shared memory. Anything we send here is a small command, response, or
/// status. 16 MiB is generous for that.
pub const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

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
}
